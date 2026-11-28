//! The M18 scale test (plan Task 18): one million catalogs and one million
//! users, each with a key, on one database, and the bounds the milestone
//! promises:
//!
//! * `App::load` finishes in under 5 s and holds no catalog;
//! * reading 5,000 distinct catalogs through a cache sized for about 1,000
//!   keeps `okf_cache_bytes{cache="catalog"}` within its budget and the
//!   process's resident memory within budget plus 200 MiB;
//! * the first authentication of the administrator's key, whose user's
//!   team owns all million catalogs, answers in under 20 ms: a user's
//!   ceiling is resolved per catalog, never loaded whole (issue #728);
//! * authenticating 10,000 distinct keys has a p99 under 20 ms and leaves
//!   the credential cache at, and not over, `credential_cache_entries`;
//! * the first page of `GET /api/v1/catalogs` for an admin key answers in
//!   under 50 ms cold (100 snapshot loads, loaded concurrently) and under 50 ms repeated.
//!
//! Ignored by default: it needs Docker, writes several GiB of rows and takes
//! minutes. Run it once, alone, with
//! `tools/agentic/gate.sh narrow --release --test it scale:: -- --ignored --nocapture`
//! and read the numbers it prints. The rows are seeded with SQL
//! (`generate_series`), not through the store functions, which would take
//! hours.

use crate::support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytesize::ByteSize;
use okf_mcp::app::App;
use okf_mcp::auth::authenticate;
use okf_mcp::clock::SystemClock;
use okf_mcp::config::Config;
use okf_mcp::store::keys::Access;
use okf_mcp::telemetry::metrics::Metrics;
use sqlx::PgPool;
use support::server::TestServer;

/// How many catalogs, and how many users with one key each, to seed.
const SEEDED: i64 = 1_000_000;
/// Catalogs read through the cache.
const CATALOG_READS: usize = 5_000;
/// Catalogs the cache budget is sized for.
const CATALOGS_FITTING: u64 = 1_000;
/// Distinct keys authenticated.
const KEY_READS: usize = 10_000;
/// The credential cache's entry limit for the run (below `KEY_READS`, so
/// the bound is exercised).
const CREDENTIAL_ENTRIES: u64 = 5_000;
/// The slack the resident memory may grow beyond the cache budget.
const RSS_SLACK: u64 = 200 * 1024 * 1024;
/// The first listing page of 100 catalogs, whose snapshots are not loaded.
const COLD_PAGE_BOUND: Duration = Duration::from_millis(50);
/// The same page again (the plan's 50 ms).
const WARM_PAGE_BOUND: Duration = Duration::from_millis(50);
/// The default page size of the listing routes.
const FIRST_PAGE_SIZE: usize = 100;
/// Every wait is bounded by this.
const BOUND: Duration = Duration::from_secs(120);

/// The body of every seeded document, 51 bytes.
const NOTE: &str = "---\ntype: Note\ntitle: Note\n---\n# Note\nA tiny note.\n";

/// The ids of the seeded catalogs.
async fn catalog_ids(pool: &PgPool) -> Vec<i64> {
    sqlx::query_scalar("SELECT id FROM catalogs WHERE name ~ '^c[0-9]{7}$'")
        .fetch_all(pool)
        .await
        .expect("read the seeded ids")
}

/// Seeds [`SEEDED`] catalogs (one tiny document each, owned by the personal
/// team of [`support::server::OWNER`], the administrator's user) and
/// [`SEEDED`] users, each with a read key whose secret is `okf_scale_<n>`.
async fn seed(pool: &PgPool) {
    let started = Instant::now();
    // The administrator's team owns the million (issue #728): its user's
    // ceiling holds every one of them, and authenticating that user must
    // not read them.
    for statement in [
        "INSERT INTO catalogs (name, description, created_at, owner_team_id) \
         SELECT 'c' || lpad(n::text, 7, '0'), '', now(), \
                (SELECT id FROM teams WHERE name = $2) \
         FROM generate_series(1, $1) AS n",
        "INSERT INTO users (name, created_at) \
         SELECT 'u' || lpad(n::text, 7, '0'), now() FROM generate_series(1, $1) AS n",
        "INSERT INTO api_keys (name, sha256, access, all_catalogs, actor, created_at, user_id) \
         SELECT 'k' || lpad(n::text, 7, '0'), sha256(convert_to('okf_scale_' || n, 'UTF8')), \
                'read', true, 'scale', now(), u.id \
         FROM generate_series(1, $1) AS n \
         JOIN users u ON u.name = 'u' || lpad(n::text, 7, '0')",
    ] {
        let mut query = sqlx::query(statement).bind(SEEDED);
        // The catalogs' second parameter is their owner team; the
        // documents', their body.
        if statement.starts_with("INSERT INTO catalogs") {
            query = query.bind(support::server::OWNER);
        } else if statement.contains("$2") {
            query = query.bind(NOTE);
        }
        query.execute(pool).await.expect("seed");
    }
    // The documents are on the store, a database of its own in
    // `OKF_TEST_STORE=separate` mode, so they are written from the ids the
    // registry holds rather than by a statement across the two.
    let store = support::pg::store_pool(pool);
    sqlx::query(
        "INSERT INTO catalog.catalog_state (id) \
         SELECT unnest($1::bigint[]) ON CONFLICT DO NOTHING",
    )
    .bind(catalog_ids(pool).await)
    .execute(&store)
    .await
    .expect("seed the states");
    sqlx::query(
        "INSERT INTO catalog.documents (catalog_id, path, content, executable, revision, \
                                updated_at, updated_by, kind, concept_type, title) \
         SELECT unnest($1::bigint[]), 'note.md', convert_to($2::text, 'UTF8'), false, 1, now(), \
                'scale', 'concept', 'Note', 'Note'",
    )
    .bind(catalog_ids(pool).await)
    .bind(NOTE)
    .execute(&store)
    .await
    .expect("seed the documents");
    sqlx::query("ANALYZE").execute(pool).await.expect("analyze");
    eprintln!("seeded in {:.1?}", started.elapsed());
}

/// The resident set size of this process, in bytes (`/proc/self/statm`'s
/// second field, in pages).
fn rss_bytes() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").expect("read statm");
    let pages: u64 = statm
        .split_whitespace()
        .nth(1)
        .and_then(|field| field.parse().ok())
        .expect("statm resident field");
    pages * 4096
}

/// The metric instruments on a local provider that renders into its own
/// registry, as `serve` builds them. Keep the provider alive.
fn local_metrics() -> (
    opentelemetry_sdk::metrics::SdkMeterProvider,
    Metrics,
    prometheus::Registry,
) {
    let registry = prometheus::Registry::new();
    let reader = opentelemetry_prometheus::exporter()
        .with_registry(registry.clone())
        .without_target_info()
        .scope_info_enabled(false)
        .build()
        .expect("the Prometheus reader");
    let provider = opentelemetry_sdk::metrics::SdkMeterProvider::builder()
        .with_reader(reader)
        .build();
    let meter = opentelemetry::metrics::MeterProvider::meter(&provider, "okf-mcp");
    let metrics = Metrics::new(&meter);
    (provider, metrics, registry)
}

/// The value of one rendered series (`okf_cache_bytes{cache="catalog"}`),
/// or `None` when the series is not rendered, so a renamed metric or label
/// fails the test instead of reading as zero.
fn sample(registry: &prometheus::Registry, series: &str) -> Option<f64> {
    let text = prometheus::TextEncoder::new()
        .encode_to_string(&registry.gather())
        .expect("encode the metrics");
    text.lines()
        .find_map(|line| line.strip_prefix(series)?.strip_prefix(' ')?.parse().ok())
}

/// The `percent`th percentile of `samples`, which it sorts.
fn latencies_at(samples: &mut [Duration], percent: usize) -> Duration {
    samples.sort();
    samples[(samples.len() * percent / 100).min(samples.len() - 1)]
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker; seeds a million catalogs and users; run alone through gate.sh"]
async fn a_million_catalogs_and_users_stay_within_bounds() {
    let server = TestServer::start(Vec::new(), Config::default()).await;
    let pool = server.db.pool.clone();
    seed(&pool).await;

    // The budget fits about CATALOGS_FITTING tiny catalogs: each weighs
    // 1 KiB + 1 KiB per file + 8 per stored byte.
    let one_catalog = okf_mcp::cache::catalogs::weight(NOTE.len() as u64, 1);
    let budget = one_catalog * CATALOGS_FITTING;
    let config = Config {
        catalog_cache_bytes: ByteSize::b(budget),
        credential_cache_entries: CREDENTIAL_ENTRIES,
        // An hour, so no entry expires during the run (10,000 sequential
        // authentications take about a minute): the entry count is held by
        // `credential_cache_entries` alone, not by the TTL.
        credential_ttl: Duration::from_secs(3600),
        ..Config::default()
    };

    // 1. Start-up: under 5 s, no catalog loaded.
    let (_provider, metrics, registry) = local_metrics();
    let started = Instant::now();
    let app: Arc<App> =
        App::load_with_metrics(pool.clone(), config, Arc::new(SystemClock), metrics)
            .await
            .expect("the app loads");
    let load = started.elapsed();
    eprintln!("App::load over {SEEDED} catalogs and users: {load:.2?}");
    assert!(load < Duration::from_secs(5), "App::load took {load:?}");
    assert_eq!(app.catalog_cache().cached(), 0, "start-up loads no catalog");
    assert_eq!(
        sample(&registry, r#"okf_cache_entries{cache="catalog"}"#),
        Some(0.0),
        "the catalog entries series is missing or not zero"
    );

    // 2. Reading distinct catalogs: the gauge never passes the budget and
    // resident memory grows by less than budget plus slack. One catalog is
    // read first so lazy statics and pool connections are not counted.
    let read = |index: usize| {
        let app = Arc::clone(&app);
        async move {
            // Spread the names over the whole table, not its first rows.
            let name = format!("c{:07}", 1 + (index as i64 * 199) % SEEDED);
            let row = app
                .catalog_row(&name)
                .await
                .expect("directory read")
                .expect("the catalog exists");
            app.catalog_state(&row).await.expect("the catalog loads");
        }
    };
    tokio::time::timeout(BOUND, read(0)).await.expect("warm-up");
    let rss_before = rss_bytes();
    let mut peak_bytes = 0.0_f64;
    let started = Instant::now();
    for index in 1..=CATALOG_READS {
        tokio::time::timeout(BOUND, read(index))
            .await
            .expect("a catalog read");
        let bytes = sample(&registry, r#"okf_cache_bytes{cache="catalog"}"#)
            .expect("the okf_cache_bytes{cache=\"catalog\"} series is missing");
        peak_bytes = peak_bytes.max(bytes);
    }
    let reads = started.elapsed();
    let growth = rss_bytes().saturating_sub(rss_before);
    eprintln!(
        "{CATALOG_READS} catalog reads in {reads:.2?}: peak okf_cache_bytes {peak_bytes} \
         (budget {budget}), entries {}, RSS growth {} B (limit {} B)",
        app.catalog_cache().cached(),
        growth,
        budget + RSS_SLACK
    );
    // Proves the measurement is live: catalogs were loaded and weighed.
    assert!(peak_bytes > 0.0, "the catalog gauge never left zero");
    assert!(
        peak_bytes <= budget as f64,
        "the catalog gauge peaked at {peak_bytes}, over the budget {budget}"
    );
    assert!(
        growth <= budget + RSS_SLACK,
        "resident memory grew by {growth} B"
    );

    // 3. The administrator's first authentication: its user's team owns
    // every seeded catalog (issue #728). It loads the user's entry cold.
    let admin = server.key("scale-admin", Access::Admin, &[], None).await;
    let started = Instant::now();
    let principal = tokio::time::timeout(BOUND, authenticate(&app, &admin))
        .await
        .expect("the administrator's authentication")
        .expect("a database answer");
    let admin_auth = started.elapsed();
    assert!(principal.is_ok(), "the administrator's key authenticates");
    let started = Instant::now();
    let again = tokio::time::timeout(BOUND, authenticate(&app, &admin))
        .await
        .expect("the administrator's second authentication")
        .expect("a database answer");
    assert!(again.is_ok(), "the second authentication admits it too");
    let admin_again = started.elapsed();
    eprintln!(
        "administrator ({SEEDED} catalogs owned by its team): first authentication \
         {admin_auth:.2?}, again {admin_again:.2?}"
    );
    let mut failures = Vec::new();
    if admin_auth >= Duration::from_millis(20) {
        failures.push(format!(
            "the administrator's first authentication took {admin_auth:?}"
        ));
    }

    // 4. Authenticating distinct keys.
    let mut latencies = Vec::with_capacity(KEY_READS);
    for index in 0..KEY_READS {
        let key = format!("okf_scale_{}", 1 + (index as i64 * 97) % SEEDED);
        let started = Instant::now();
        let principal = tokio::time::timeout(BOUND, authenticate(&app, &key))
            .await
            .expect("an authentication")
            .expect("a database answer");
        latencies.push(started.elapsed());
        assert!(principal.is_ok(), "{key} authenticates");
    }
    let median = latencies_at(&mut latencies, 50);
    let worst = latencies_at(&mut latencies, 99);
    // Moka settles its entry count lazily; give it a moment.
    let mut entries = f64::MAX;
    for _ in 0..50 {
        entries = sample(&registry, r#"okf_cache_entries{cache="credential"}"#)
            .expect("the okf_cache_entries{cache=\"credential\"} series is missing");
        if entries <= CREDENTIAL_ENTRIES as f64 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    eprintln!(
        "{KEY_READS} authentications: p50 {median:.2?}, p99 {worst:.2?}, credential entries {entries} \
         (limit {CREDENTIAL_ENTRIES})"
    );
    // Every bound is measured before any is asserted, so one run reports
    // all the numbers.
    if worst >= Duration::from_millis(20) {
        failures.push(format!("authentication p99 was {worst:?}"));
    }
    if entries > CREDENTIAL_ENTRIES as f64 {
        failures.push(format!("the credential cache holds {entries} entries"));
    }
    // With an hour's TTL nothing expired, so twice the limit in distinct
    // keys must have filled the cache to its limit region: the bound, not
    // expiry, is what held the count.
    if entries < CREDENTIAL_ENTRIES as f64 * 0.9 {
        failures.push(format!(
            "the credential cache holds only {entries} entries: the limit was not reached"
        ));
    }

    // 5. The first page of the catalog listing for an admin key, over HTTP
    // against the server that shares the database.
    let client = reqwest::Client::new();
    let url = format!("{}/api/v1/catalogs", server.base);
    // The first request, then the same page again and a page of ten: what
    // the first one pays for (the key load, the pool, 100 snapshot loads)
    // shows against the repeats.
    let mut timings = Vec::new();
    // Each page must be complete: its size is what the request asked for
    // (the fixture has far more catalogs), and `next` must continue it.
    let requests = [
        (url.clone(), FIRST_PAGE_SIZE),
        (url.clone(), FIRST_PAGE_SIZE),
        (format!("{url}?limit=10"), 10),
    ];
    for (target, expected) in requests {
        let started = Instant::now();
        let response = tokio::time::timeout(BOUND, client.get(&target).bearer_auth(&admin).send())
            .await
            .expect("the listing")
            .expect("the listing answers");
        let status = response.status();
        let body: serde_json::Value = response.json().await.expect("a JSON listing");
        timings.push(started.elapsed());
        assert!(status.is_success(), "{status}: {body}");
        let listed = body["catalogs"].as_array().map_or(0, Vec::len);
        assert_eq!(listed, expected, "{target} listed {listed} catalogs");
        assert!(
            body["next"].as_str().is_some_and(|next| !next.is_empty()),
            "{target} has more catalogs, so it needs a next cursor: {}",
            body["next"]
        );
    }
    let first_page = timings[0];
    eprintln!(
        "GET /api/v1/catalogs: first request {:.2?} ({FIRST_PAGE_SIZE} catalogs), repeat {:.2?}, \
         limit=10 {:.2?}",
        timings[0], timings[1], timings[2]
    );
    // The plan's 50 ms holds cold too: the page's 100 snapshots load eight at
    // a time (issue #732), not one after another.
    if first_page >= COLD_PAGE_BOUND {
        failures.push(format!("the first listing page took {first_page:?}"));
    }
    if timings[1] >= WARM_PAGE_BOUND {
        failures.push(format!("the repeated listing page took {:?}", timings[1]));
    }
    assert!(failures.is_empty(), "bounds missed: {failures:?}");
}
