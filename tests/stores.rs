//! Catalog stores end to end (milestone 21, Task 20): the account database
//! and two stores on one server, team A pinned to the store `acme`, and a
//! catalog for each of two teams. The whole write and read cycle runs over
//! the MCP and HTTP transports, each catalog's rows are checked to sit on
//! its own store only, and then `acme`'s database is taken away: team B
//! carries on and team A is told the store is unavailable.
//!
//! The store `acme` is added through `stores::add` as a `host` store, so
//! this test also covers adding one (plan decision P8, lifted by Task 20).
//! A forwarding proxy ([`support::stores::StoreProxy`]) stands between the
//! server and `acme`'s database, which is how the test stops the database
//! after the server holds connections to it.

// The tests' futures hold whole servers, whose router's middleware stack
// nests deeply enough to overflow the default query depth (128) when their
// layout is computed, as in the library (`src/lib.rs`).

use crate::support;

use std::time::Duration;

use chrono::Utc;
use okf_mcp::config::Config;
use okf_mcp::labels::DisplayName;
use okf_mcp::store::audit::AuditActor;
use okf_mcp::store::catalogs::{self, NewCatalog};
use okf_mcp::store::import::{import, ImportOptions, ImportScope};
use okf_mcp::store::keys::Access;
use okf_mcp::store::stores::{self as store_rows, CredentialKind, StoreLocation, StoreRow};
use okf_mcp::store::teams::{self, NewTeam};
use okf_mcp::stores::{PoolSettings, StoreName, Stores};
use serde_json::{json, Value};
use support::pg::STORE_PASSWORD_VAR;
use support::server::{
    applied, call, files, open_changeset, structured, Client, TestServer, OWNER,
};
use support::stores::StoreProxy;

/// The longest any step may take: a hung call fails the test, naming it.
const STEP: Duration = Duration::from_secs(60);

const ALPHA_NOTE: &str = "---\ntype: Note\ntitle: Alpha\n---\n# Alpha\nThe zebra lives on acme.\n";
const BETA_NOTE: &str = "---\ntype: Note\ntitle: Beta\n---\n# Beta\nThe zebra lives on main.\n";

/// Bounds `step` by [`STEP`].
///
/// # Panics
/// With `what`, if the step does not finish in time.
async fn within<T>(what: &str, step: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(STEP, step)
        .await
        .unwrap_or_else(|_| panic!("timed out: {what}"))
}

/// A server with two stores and a catalog on each.
struct Fixture {
    server: TestServer,
    proxy: StoreProxy,
    acme: StoreName,
    alpha_id: i64,
    beta_id: i64,
    /// A write key reaching every catalog.
    key: String,
}

/// The `Err` text of a tool result that failed, or panics if it did not.
fn failure(result: &rmcp::model::CallToolResult) -> Value {
    assert_eq!(result.is_error, Some(true), "{result:?}");
    result.structured_content.clone().unwrap_or(Value::Null)
}

/// The server's settings: a short connect bound, so the lost store costs the
/// test seconds.
fn quick_config() -> Config {
    Config {
        store_connect_timeout: Duration::from_secs(2),
        ..Config::default()
    }
}

/// Builds the fixture: team `team-a` pinned to `acme`, team `team-b` on the
/// default store, catalog `alpha` for A and `beta` for B, each with one note.
async fn fixture() -> Fixture {
    let server = TestServer::start(vec![], quick_config()).await;
    let pool = &server.db.pool;
    let actor = AuditActor::cli();

    // The database of `acme`, behind a proxy, added as a `host` store.
    let database = server.db.extra_store_database("acme").await;
    let options = pool.connect_options();
    let proxy = StoreProxy::start(format!("{}:{}", options.get_host(), options.get_port())).await;
    let acme = StoreName::parse("acme").unwrap();
    let row = StoreRow {
        name: acme.clone(),
        location: StoreLocation::Host {
            host: "127.0.0.1".to_string(),
            port: proxy.port,
            database,
            credential_kind: CredentialKind::Env,
            credential_ref: STORE_PASSWORD_VAR.to_string(),
        },
    };
    within(
        "adding the store",
        store_rows::add(pool, server.app.stores(), row, &actor, Utc::now()),
    )
    .await
    .expect("add the host store");

    for team in ["team-a", "team-b"] {
        let new = NewTeam {
            name: team,
            display_name: &DisplayName::default(),
            owner: OWNER,
        };
        teams::create(pool, &new, &actor, Utc::now())
            .await
            .expect("create the team");
    }
    teams::set_store(pool, "team-a", Some(&acme), &actor, Utc::now())
        .await
        .expect("pin team A to acme");

    for (name, team, note) in [
        ("alpha", "team-a", ALPHA_NOTE),
        ("beta", "team-b", BETA_NOTE),
    ] {
        let new = NewCatalog {
            name,
            display_name: &DisplayName::default(),
            description: "",
            owner_team: team,
        };
        let created = applied(&server.app, async {
            catalogs::create_in(server.app.stores(), &new, &actor, Utc::now()).await
        })
        .await
        .expect("create the catalog");
        let db = server
            .app
            .stores()
            .get(&created_store(&server, created.id).await)
            .await
            .expect("open the catalog's store");
        let options = ImportOptions {
            replace: false,
            scope: ImportScope::Everything,
            expect_generation: None,
            expect_catalog_id: None,
            actor: "tester",
            audit: &actor,
            now: Utc::now(),
        };
        import(&db, name, files(&[("note.md", note)]), &options)
            .await
            .expect("import the note");
    }
    let alpha_id = catalog_id(&server, "alpha").await;
    let beta_id = catalog_id(&server, "beta").await;
    let key = server.key("writer", Access::Write, &[], None).await;
    Fixture {
        server,
        proxy,
        acme,
        alpha_id,
        beta_id,
        key,
    }
}

/// The store a catalog's registry row names.
async fn created_store(server: &TestServer, id: i64) -> StoreName {
    let name: String = sqlx::query_scalar("SELECT store FROM catalogs WHERE id = $1")
        .bind(id)
        .fetch_one(&server.db.pool)
        .await
        .expect("the catalog's store");
    StoreName::parse(&name).expect("a store name")
}

/// A catalog's registry id.
async fn catalog_id(server: &TestServer, name: &str) -> i64 {
    sqlx::query_scalar("SELECT id FROM catalogs WHERE name = $1")
        .bind(name)
        .fetch_one(&server.db.pool)
        .await
        .expect("the catalog's id")
}

/// How many documents of catalog `id` the store behind `pool` holds.
async fn documents_on(pool: &sqlx::PgPool, id: i64) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM catalog.documents WHERE catalog_id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("count the documents")
}

/// Changes `catalog`'s note through a changeset and publishes it.
async fn edit_and_publish(client: &Client, catalog: &str, from: &str, to: &str, message: &str) {
    let handle = open_changeset(client, Some(catalog)).await;
    structured(
        &call(
            client,
            "edit_document",
            json!({"catalog": catalog, "changeset": handle, "path": "/note.md",
                   "old_str": from, "new_str": to}),
        )
        .await,
    );
    let published = call(
        client,
        "publish",
        json!({"catalog": catalog, "changeset": handle, "message": message}),
    )
    .await;
    assert_eq!(structured(&published)["published"], true, "{published:?}");
}

/// The hits of a search over `catalogs`, as `(catalog, path)` pairs, and the
/// catalogs it could not read.
async fn search(client: &Client, catalogs: &[&str]) -> (Vec<(String, String)>, Vec<String>) {
    let result = call(
        client,
        "search",
        json!({"query": "zebra", "catalogs": catalogs}),
    )
    .await;
    let body = structured(&result);
    let hits = body["hits"]
        .as_array()
        .expect("hits")
        .iter()
        .map(|hit| {
            (
                hit["catalog"].as_str().unwrap_or_default().to_string(),
                hit["path"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    let unavailable = body["unavailable"]
        .as_array()
        .map(|names| {
            names
                .iter()
                .filter_map(|name| name.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    (hits, unavailable)
}

/// The maintenance steps over MCP: claim one document's lease, then submit.
async fn run_maintenance(client: &Client, catalog: &str) {
    let claimed = structured(
        &call(
            client,
            "maintenance_claim",
            json!({"catalog": catalog, "idempotency_key": "claim", "batch_size": 1}),
        )
        .await,
    )
    .clone();
    let lease = &claimed["leases"][0];
    let evidence = json!([{"pin": lease["pin"], "excerpt": "type: Note"}]);
    let submitted = call(
        client,
        "maintenance_submit",
        json!({"catalog": catalog, "work_id": lease["work_id"], "token": lease["token"],
               "idempotency_key": "submit",
               "submission": {"outcome": "completed", "assessment": "supported_current",
                              "summary": "Reported support, not verification.",
                              "evidence": evidence, "findings": [], "reported_tokens": null}}),
    )
    .await;
    assert_eq!(
        structured(&submitted)["outcome"],
        "completed",
        "{submitted:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn two_stores_serve_their_catalogs_and_a_lost_store_costs_only_its_own() {
    let fixture = within("the fixture", fixture()).await;
    let server = &fixture.server;
    let client = within("the client", server.client(&fixture.key, None))
        .await
        .expect("connect");

    // Open, edit, publish: one revision beyond the import on each store.
    within(
        "publishing on alpha",
        edit_and_publish(&client, "alpha", "acme.", "acme, twice.", "Edit alpha"),
    )
    .await;
    within(
        "publishing on beta",
        edit_and_publish(&client, "beta", "main.", "main, twice.", "Edit beta"),
    )
    .await;

    // History: the import and the edit, newest first.
    let history = call(
        &client,
        "history",
        json!({"catalog": "alpha", "path": "/note.md"}),
    )
    .await;
    let revisions = structured(&history)["revisions"]
        .as_array()
        .expect("revisions")
        .clone();
    assert_eq!(revisions.len(), 2, "{revisions:?}");
    let first = revisions.last().expect("the import")["revision"].clone();

    // Revert to the import, and publish it.
    let handle = open_changeset(&client, Some("alpha")).await;
    structured(
        &call(
            &client,
            "revert",
            json!({"catalog": "alpha", "changeset": handle, "path": "/note.md", "revision": first}),
        )
        .await,
    );
    let published = call(
        &client,
        "publish",
        json!({"catalog": "alpha", "changeset": handle, "message": "Revert alpha"}),
    )
    .await;
    assert_eq!(structured(&published)["published"], true, "{published:?}");
    let read = call(
        &client,
        "get_concept",
        json!({"catalog": "alpha", "path": "/note.md", "raw": true}),
    )
    .await;
    assert!(
        structured(&read)["raw"].to_string().contains("acme."),
        "the revert restored the import: {read:?}"
    );

    // Search across both stores finds both catalogs.
    let (hits, unavailable) = within("search", search(&client, &["alpha", "beta"])).await;
    let mut found: Vec<&str> = hits.iter().map(|(catalog, _)| catalog.as_str()).collect();
    found.sort_unstable();
    assert_eq!(found, ["alpha", "beta"], "{hits:?}");
    assert!(unavailable.is_empty(), "{unavailable:?}");

    // Export and maintenance, on the pinned store.
    let exported = reqwest::Client::new()
        .get(format!("{}/catalogs/alpha/export", server.base))
        .bearer_auth(&fixture.key)
        .send()
        .await
        .expect("export");
    assert_eq!(exported.status(), 200);
    let bytes = exported.bytes().await.expect("the tarball");
    let tarball = okf_mcp::archive::read_tarball(&bytes[..], 1 << 20).expect("a tarball");
    assert!(
        tarball.contains_key("note.md"),
        "{:?}",
        tarball.keys().collect::<Vec<_>>()
    );
    let reporter = server.reporter_key("reporter", &["alpha"]).await;
    let reporting = server.client(&reporter, None).await.expect("connect");
    within("maintenance on alpha", run_maintenance(&reporting, "alpha")).await;
    reporting.cancel().await.expect("close the reporter");

    // Each catalog's rows are on its own store and nowhere else.
    let acme_pool = fixture
        .server
        .app
        .stores()
        .get(&fixture.acme)
        .await
        .expect("open acme");
    let main_pool = server.db.store();
    assert!(documents_on(acme_pool.executor(), fixture.alpha_id).await > 0);
    assert_eq!(documents_on(acme_pool.executor(), fixture.beta_id).await, 0);
    assert!(documents_on(&main_pool, fixture.beta_id).await > 0);
    assert_eq!(documents_on(&main_pool, fixture.alpha_id).await, 0);
    for pool in [acme_pool.executor(), &main_pool] {
        let own: i64 =
            sqlx::query_scalar("SELECT count(DISTINCT catalog_id) FROM catalog.revisions")
                .fetch_one(pool)
                .await
                .expect("count the catalogs with revisions");
        assert_eq!(own, 1, "each store holds the revisions of one catalog only");
    }

    // A restart drops what the server holds in memory, so the outage meets a
    // server that has loaded no catalog: what a replica started during the
    // outage, or one whose cache expired, sees. Then stop acme's database.
    client.cancel().await.expect("close the client");
    let Fixture {
        mut server,
        proxy,
        key,
        ..
    } = fixture;
    within("the restart", server.restart(quick_config())).await;
    proxy.stop().await;
    let client = within("the client", server.client(&key, None))
        .await
        .expect("connect");

    // Team B is unaffected: a write and a read on beta both work.
    within(
        "publishing on beta after the outage",
        edit_and_publish(
            &client,
            "beta",
            "main, twice.",
            "main, thrice.",
            "Edit beta again",
        ),
    )
    .await;
    let (hits, unavailable) = within("search on beta", search(&client, &["beta"])).await;
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert!(unavailable.is_empty(), "{unavailable:?}");

    // Team A is told the store is unavailable: a write is refused ...
    let refused = within(
        "opening on alpha",
        call(&client, "open_changeset", json!({"catalog": "alpha"})),
    )
    .await;
    assert_eq!(
        failure(&refused)["error"],
        "StoreUnavailable",
        "{refused:?}"
    );

    // ... and a search that merges reads names alpha as unavailable while it
    // still answers for beta.
    let (hits, unavailable) = within("merged search", search(&client, &["alpha", "beta"])).await;
    let found: Vec<&str> = hits.iter().map(|(catalog, _)| catalog.as_str()).collect();
    assert_eq!(found, ["beta"], "{hits:?}");
    assert_eq!(unavailable, ["alpha"]);
    // The reply names no host, port or database.
    assert!(!refused
        .content
        .iter()
        .any(|part| format!("{part:?}").contains("127.0.0.1")));

    client.cancel().await.expect("close the client");
    drop(server);
}

/// The note the seeded catalogs of the upgrade test hold.
const OLD_NOTE: &str = "---\ntype: Note\ntitle: Old\n---\n# Old\nFrom before the split.\n";

/// Seeds the pre-split schema (account migrations 1 to 35, which are the
/// migrations of `main` before the catalog stores milestone) with two
/// catalogs, their documents and revisions, and one catalog audit entry in
/// the account table, as release 2 left them.
async fn seed_release_two(pool: &sqlx::PgPool) {
    okf_mcp::db::MIGRATOR
        .run_to(35, pool)
        .await
        .expect("apply the migrations of release 2");
    for name in ["old-one", "old-two"] {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO catalogs (name, description, generation, created_at) \
             VALUES ($1, '', 1, now()) RETURNING id",
        )
        .bind(name)
        .fetch_one(pool)
        .await
        .expect("seed a catalog");
        sqlx::query(
            "INSERT INTO documents (catalog_id, path, content, executable, revision, \
             updated_at, updated_by, kind, concept_type, title) \
             VALUES ($1, 'note.md', $2, false, 1, now(), 'seed', 'concept', 'Note', 'Old')",
        )
        .bind(id)
        .bind(OLD_NOTE.as_bytes())
        .execute(pool)
        .await
        .expect("seed a document");
        sqlx::query(
            "INSERT INTO revisions (catalog_id, path, revision, op, content, executable, \
             actor, message, created_at) \
             VALUES ($1, 'note.md', 1, 'create', $2, false, 'seed', 'seed', now())",
        )
        .bind(id)
        .bind(OLD_NOTE.as_bytes())
        .execute(pool)
        .await
        .expect("seed a revision");
    }
    sqlx::query(
        "INSERT INTO audit_entries (at, action, source, actor_label, actor_operator, \
         target_kind, target_name) \
         VALUES (now() - interval '1 hour', 'catalog.published', 'cli', 'cli:seed', true, \
         'catalog', 'old-one')",
    )
    .execute(pool)
    .await
    .expect("seed an audit entry");
}

/// Runs one `okf-mcp` subcommand against the database at `url`, as the
/// operator does.
async fn okf_mcp(url: &str, args: &[&str]) {
    let mut argv = vec!["okf-mcp", "--database-url", url];
    argv.extend_from_slice(args);
    let cli = <okf_mcp::cli::Cli as clap::Parser>::try_parse_from(argv).expect("a valid command");
    within(
        "the subcommand",
        okf_mcp::cli::run(cli, std::sync::Arc::new(okf_mcp::clock::SystemClock)),
    )
    .await
    .expect("the subcommand succeeds");
}

/// The smoke test of release 3's upgrade procedure (operations guide,
/// "Upgrading to the release with separate stores"), on a disposable
/// database built at the schema release 2 has: `okf-mcp migrate`, then
/// `okf-mcp stores migrate`, then a server on it. Not a substitute for a
/// run against a real snapshot, which only the maintainer can do.
#[tokio::test(flavor = "multi_thread")]
async fn the_upgrade_procedure_adopts_a_pre_split_database_and_keeps_its_history() {
    let db = support::pg::TestDb::start().await;
    seed_release_two(&db.pool).await;

    // Steps 3 of the procedure: migrate, then check the stores.
    okf_mcp(&db.url, &["migrate"]).await;
    // `okf-mcp stores migrate` asks the registry to migrate each store, which
    // this does for `main` (the command installs the telemetry subscriber, once
    // per process, so a test can run only one subcommand): the adoption
    // happened in `migrate`, so there is nothing left to apply.
    let registry = Stores::new(
        db.pool.clone(),
        PoolSettings::from_config(&Config::default()),
    );
    let migration = within("stores migrate", registry.migration(&StoreName::main()))
        .await
        .expect("migrate main");
    assert!(!migration.adopted && migration.applied == 0, "{migration}");
    registry.forget_all().await;

    // The adoption: main is the account database's `catalog` schema, its
    // baseline recorded as applied, its identity written, and every old row
    // moved with its catalog.
    let identity: String = sqlx::query_scalar("SELECT name FROM catalog.store_identity")
        .fetch_one(&db.pool)
        .await
        .expect("main's identity row");
    assert_eq!(identity, "main");
    let recorded: i64 = sqlx::query_scalar("SELECT count(*) FROM catalog._sqlx_migrations")
        .fetch_one(&db.pool)
        .await
        .expect("the recorded catalog migrations");
    let embedded = okf_mcp::db::CATALOG_MIGRATOR
        .iter()
        .filter(|migration| !migration.migration_type.is_down_migration())
        .count();
    assert_eq!(recorded as usize, embedded);
    let moved: (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM catalog.catalog_state), \
                (SELECT count(*) FROM catalog.documents), \
                (SELECT count(*) FROM catalog.revisions)",
    )
    .fetch_one(&db.pool)
    .await
    .expect("count the moved rows");
    assert_eq!(moved, (2, 2, 2));
    let left_behind: bool =
        sqlx::query_scalar("SELECT to_regclass('public.documents') IS NOT NULL")
            .fetch_one(&db.pool)
            .await
            .expect("look for the old table");
    assert!(!left_behind, "the catalog tables left `public`");

    // A server on the upgraded database publishes to an old catalog.
    support::server::create_owner(&db.pool).await;
    let server = TestServer::start_on(db, quick_config()).await;
    let key = server.key("admin", Access::Admin, &[], None).await;
    let client = within("the client", server.client(&key, None))
        .await
        .expect("connect");
    within(
        "publishing on an old catalog",
        edit_and_publish(
            &client,
            "old-one",
            "before the split",
            "after the split",
            "Edit",
        ),
    )
    .await;
    let read = call(
        &client,
        "get_concept",
        json!({"catalog": "old-one", "path": "/note.md", "raw": true}),
    )
    .await;
    assert!(structured(&read)["raw"]
        .to_string()
        .contains("after the split"));
    client.cancel().await.expect("close the client");

    // The audit view reads the old entry (account table, no store) and the
    // new one (the store `main`), each once.
    let audit: Value = reqwest::Client::new()
        .get(format!(
            "{}/api/v1/audit?action=catalog.published",
            server.base
        ))
        .bearer_auth(&key)
        .send()
        .await
        .expect("read the audit log")
        .json()
        .await
        .expect("a JSON page");
    let entries = audit["entries"].as_array().expect("entries");
    let stores: Vec<Option<&str>> = entries
        .iter()
        .map(|entry| entry["store"].as_str())
        .collect();
    assert_eq!(stores, [Some("main"), None], "newest first: {audit}");
}
