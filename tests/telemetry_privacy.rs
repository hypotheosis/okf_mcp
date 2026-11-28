//! The mock OTLP collector's own tests and the end-to-end privacy sentinel.
//!
//! The first group checks the exporters' wire behaviour against the
//! collector: configured headers, the Sentry path and header, redirects not
//! followed, 500s counted. The last test is the one that matters for
//! privacy: it plants synthetic secrets in every input channel, drives the
//! server through sign-in, MCP, publish, archives, mail, OAuth and failures,
//! flushes through `Telemetry::shutdown`, proves that spans, logs and metrics
//! really arrived, and only then asserts that none of the secrets is in any
//! exported byte or on `/metrics`. stderr is the documented exception to the
//! rule, so it is not scanned for them (it is read to learn the mail links).
//!
//! The tests share the process's global meter provider and tracing
//! subscriber, so a lock makes them run one at a time.

use crate::support;

use std::collections::{BTreeSet, HashMap};
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use okf_mcp::config::{Config, OAuthConfig};
use okf_mcp::store::keys::Access;
use okf_mcp::telemetry::config::{OtlpConfig, TelemetryConfig};
use okf_mcp::telemetry::otlp::{FailureReason, Signal};
use okf_mcp::telemetry::subscriber::settings;
use okf_mcp::telemetry::{install_with_writer, ProviderOutcome, Telemetry};
use opentelemetry_proto::tonic::common::v1::KeyValue as OtlpAttribute;
use opentelemetry_proto::tonic::metrics::v1::{metric, Metric};
use serde_json::json;
use support::oauth::form;
use support::otlp_collector::{self as otlp, Collector, Mode, Payload, Recorded};
use support::server::{call, files, open_changeset, structured, TestServer};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::fmt::writer::MakeWriter;

/// One test at a time: they set the process-wide meter provider (and the
/// sentinel test the global subscriber).
static SERIAL: AsyncMutex<()> = AsyncMutex::const_new(());

/// The planted secret. It is obviously synthetic and appears in every
/// variant below.
const SENTINEL: &str = "zq-sentinel-7f3a";

/// The variable `headers_env` names in the tests.
const HEADERS_ENV: &str = "OKF_TEST_OTLP_HEADERS";

/// The variable `sentry_dsn_env` names in the tests.
const DSN_ENV: &str = "OKF_TEST_SENTRY_DSN";

/// How long a test waits for something that must happen. Generous: the
/// machine may be loaded, and every wait ends as soon as its condition holds.
const PATIENCE: Duration = Duration::from_secs(60);

/// A writer that keeps what it is given, shared between clones: stderr.
#[derive(Clone, Default)]
struct Stderr(Arc<Mutex<Vec<u8>>>);

impl Stderr {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("stderr lock")).into_owned()
    }
}

struct StderrWriter(Arc<Mutex<Vec<u8>>>);

impl io::Write for StderrWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("stderr lock").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Stderr {
    type Writer = StderrWriter;

    fn make_writer(&'a self) -> Self::Writer {
        StderrWriter(Arc::clone(&self.0))
    }
}

/// `[telemetry]` exporting to `endpoint`, every layer at `trace`, pushing
/// metrics every second.
fn telemetry_config(otlp: OtlpConfig) -> TelemetryConfig {
    TelemetryConfig {
        otlp: Some(OtlpConfig {
            traces_filter: "trace".to_string(),
            logs_filter: "trace".to_string(),
            export_interval: Duration::from_secs(1),
            ..otlp
        }),
        ..TelemetryConfig::default()
    }
}

/// The `[telemetry.otlp]` table of an endpoint whose headers come from
/// [`HEADERS_ENV`].
fn endpoint_with_headers(url: &str) -> OtlpConfig {
    OtlpConfig {
        endpoint: Some(url.to_string()),
        headers_env: Some(HEADERS_ENV.to_string()),
        ..OtlpConfig::default()
    }
}

/// As [`endpoint_with_headers`], with `headers_env = "none"`, the documented
/// no-header form (it reads no variable).
fn endpoint_without_headers(url: &str) -> OtlpConfig {
    OtlpConfig {
        headers_env: Some("none".to_string()),
        ..endpoint_with_headers(url)
    }
}

/// Emits one span through a subscriber that exports to `telemetry`, without
/// installing it globally.
///
/// The target is `okf_mcp::...`: the OTLP layers drop every other target.
fn emit_a_span(telemetry: &Telemetry, config: &TelemetryConfig) {
    let layers = settings(config, None).expect("layer settings");
    // `with_default` scopes the subscriber to this closure on this thread
    // (a Python `with` block), so no other test sees it.
    tracing::subscriber::with_default(telemetry.subscriber(layers), || {
        let span = tracing::info_span!(target: "okf_mcp::probe", "probe");
        drop(span.entered());
    });
}

/// Flushes `telemetry` and asserts every provider that ran flushed.
async fn flush(telemetry: Telemetry) {
    let report = telemetry.shutdown().await;
    assert!(report.all_flushed(), "{report:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn configured_headers_arrive() {
    let _serial = SERIAL.lock().await;
    let collector = Collector::start().await;
    let config = telemetry_config(endpoint_with_headers(&collector.url()));
    let env = |name: &str| {
        (name == HEADERS_ENV).then(|| "authorization=Bearer sentinel-otlp-token".to_string())
    };
    let telemetry = Telemetry::start(&config, &env).expect("telemetry starts");

    emit_a_span(&telemetry, &config);
    flush(telemetry).await;

    let requests = collector
        .wait_for(
            "a trace export",
            |requests| requests.iter().any(|r| r.path == "/v1/traces"),
            PATIENCE,
        )
        .await;
    let traces = requests
        .iter()
        .find(|request| request.path == "/v1/traces")
        .expect("a trace export");
    assert_eq!(
        traces.header("authorization"),
        Some("Bearer sentinel-otlp-token")
    );
    assert!(
        otlp::spans(&requests)
            .iter()
            .any(|span| span.name == "probe"),
        "the probe span arrived decoded"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sentry_dsn_derives_path_and_header() {
    let _serial = SERIAL.lock().await;
    let collector = Collector::start().await;
    let dsn = format!("http://public@{}/42", collector.authority());
    let config = telemetry_config(OtlpConfig {
        sentry_dsn_env: Some(DSN_ENV.to_string()),
        ..OtlpConfig::default()
    });
    let env = move |name: &str| (name == DSN_ENV).then(|| dsn.clone());
    let telemetry = Telemetry::start(&config, &env).expect("telemetry starts");

    emit_a_span(&telemetry, &config);
    flush(telemetry).await;

    let requests = collector
        .wait_for(
            "a trace export on the Sentry path",
            |requests| {
                requests
                    .iter()
                    .any(|r| r.path == "/api/42/integration/otlp/v1/traces")
            },
            PATIENCE,
        )
        .await;
    let traces = requests
        .iter()
        .find(|request| request.path == "/api/42/integration/otlp/v1/traces")
        .expect("a trace export");
    assert_eq!(
        traces.header("x-sentry-auth"),
        Some("sentry sentry_key=public")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_redirect_is_not_followed() {
    let _serial = SERIAL.lock().await;
    let first = Collector::start().await;
    let second = Collector::start().await;
    first.set_mode(Mode::Redirect(second.url()));
    let config = telemetry_config(endpoint_without_headers(&first.url()));
    let telemetry = Telemetry::start(&config, &|_| None).expect("telemetry starts");
    let failures = Arc::clone(telemetry.failures());

    emit_a_span(&telemetry, &config);
    let report = telemetry.shutdown().await;

    first
        .wait_for(
            "the redirecting collector to be asked",
            |requests| requests.iter().any(|r| r.path == "/v1/traces"),
            PATIENCE,
        )
        .await;
    assert!(
        second.requests().is_empty(),
        "the redirect target received {:?}",
        second.requests()
    );
    assert!(
        failures.total(Signal::Traces) >= 1,
        "a 307 is a failed export, not a followed one; shutdown said {report:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_500_counts_as_a_failure() {
    let _serial = SERIAL.lock().await;
    let collector = Collector::start().await;
    collector.set_mode(Mode::Fail500);
    let config = telemetry_config(endpoint_without_headers(&collector.url()));
    let telemetry = Telemetry::start(&config, &|_| None).expect("telemetry starts");
    let failures = Arc::clone(telemetry.failures());

    emit_a_span(&telemetry, &config);
    let report = telemetry.shutdown().await;

    collector
        .wait_for(
            "the failing collector to be asked",
            |requests| requests.iter().any(|r| r.path == "/v1/traces"),
            PATIENCE,
        )
        .await;
    assert!(
        failures.count(Signal::Traces, FailureReason::Rejected) >= 1,
        "a 500 is a rejected export; shutdown said {report:?}"
    );
}

/// Every route the sentinel test calls, as axum's `MatchedPath` names it.
const ROUTES: &[&str] = &[
    "/mcp",
    "/signin",
    "/signin/forgot",
    "/oauth/authorize",
    "/catalogs/{name}/export",
    "/catalogs/{name}/import",
    "/api/v1/whoami",
    "/api/v1/users",
    "/api/v1/users/{name}",
    "/api/v1/users/{name}/email",
    "/api/v1/users/{name}/invite",
    "/api/v1/mail/test",
];

/// A secret that must not leave the process, with a label for the report.
struct Secret {
    label: &'static str,
    value: String,
}

fn secret(label: &'static str, value: impl Into<String>) -> Secret {
    Secret {
        label,
        value: value.into(),
    }
}

/// The value of the cookie `name` that `response` sets.
fn set_cookie(response: &reqwest::Response, name: &str) -> Option<String> {
    response
        .headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(|line| {
            let pair = line.split(';').next()?;
            pair.strip_prefix(&format!("{name}=")).map(str::to_string)
        })
}

/// The token of every `token=` link in `text` (the mail links on stderr).
fn link_tokens(text: &str) -> Vec<String> {
    let pattern = regex::Regex::new(r"token=([A-Za-z0-9_\-]+)").expect("pattern");
    pattern
        .captures_iter(text)
        .map(|capture| capture[1].to_string())
        .collect()
}

/// Names of every table the migrations create.
fn migration_tables() -> BTreeSet<String> {
    let pattern =
        regex::Regex::new(r"(?i)create\s+table\s+(?:if\s+not\s+exists\s+)?(?:\w+\.)?(\w+)")
            .expect("pattern");
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("migrations")
        .join("accounts");
    let mut tables = BTreeSet::new();
    for entry in std::fs::read_dir(dir).expect("migrations directory") {
        let path = entry.expect("entry").path();
        if path.extension().is_some_and(|ext| ext == "sql") {
            let sql = std::fs::read_to_string(&path).expect("migration");
            tables.extend(pattern.captures_iter(&sql).map(|c| c[1].to_string()));
        }
    }
    tables
}

/// The ids of the traces that hold a span for `route`.
fn trace_ids_of_route(requests: &[Recorded], route: &str) -> Vec<Vec<u8>> {
    otlp::spans(requests)
        .into_iter()
        .filter(|span| otlp::attribute(&span.attributes, "http.route").as_deref() == Some(route))
        .map(|span| span.trace_id.clone())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nothing_secret_is_exported_or_scraped() {
    let _serial = SERIAL.lock().await;

    // Secrets the test chooses.
    let otlp_token = format!("{SENTINEL}-otlp-token");
    let password = format!("{SENTINEL}-Passw0rd-long");
    let user_name = format!("{SENTINEL}-user");
    let user_email = format!("{SENTINEL}-user@example.com");
    let invitee = format!("{SENTINEL}-invitee");
    let invitee_email = format!("{SENTINEL}-invitee@example.com");
    let display_name = format!("{SENTINEL}-display");
    let concept_body = format!("{SENTINEL}-concept-body");
    let concept_path = format!("{SENTINEL}-concept");
    let query_value = format!("{SENTINEL}-q");
    let path_segment = format!("{SENTINEL}-path");
    let header_value = format!("{SENTINEL}-header");
    let cookie_value = format!("{SENTINEL}-cookie");
    let tracestate = format!("{SENTINEL}-tracestate");
    let agent = format!("{SENTINEL}-agent");
    let bad_bearer = format!("{SENTINEL}-bad");
    let client_id = format!("{SENTINEL}-client");
    let cimd_name = format!("{SENTINEL}-cimd");
    let catalog_name = format!("{SENTINEL}-cat");
    let db_error = format!("{SENTINEL}-dberr");
    let search_text = format!("{SENTINEL}-search");
    let wrong_password = format!("{SENTINEL}-wrong");

    // The plumbing: a collector, telemetry exporting to it, the process's
    // subscriber, and a `/metrics` listener, as `serve` assembles them.
    let collector = Collector::start().await;
    let config = telemetry_config(endpoint_with_headers(&collector.url()));
    let headers = format!("authorization=Bearer {otlp_token}");
    let env = move |name: &str| (name == HEADERS_ENV).then(|| headers.clone());
    let telemetry = Telemetry::start(&config, &env).expect("telemetry starts");
    let stderr = Stderr::default();
    install_with_writer(&config, Some(&telemetry), None, stderr.clone()).expect("install");
    let scrape_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind /metrics");
    let scrape_url = format!(
        "http://{}/metrics",
        scrape_listener.local_addr().expect("addr")
    );
    let scrape_stop = CancellationToken::new();
    let scraping = tokio::spawn(okf_mcp::telemetry::scrape::serve(
        scrape_listener,
        telemetry.registry(),
        scrape_stop.clone(),
    ));

    // The server, with OAuth on and the mail worker running (the `log`
    // provider: it delivers nothing and logs the message locally).
    let catalogs = vec![
        (
            "pg",
            files(&[
                ("index.md", "# PG\n"),
                (
                    "infra/index.md",
                    "# Infra\n\n* [Time server](time.md) - the GPS clock\n",
                ),
                (
                    "infra/time.md",
                    "---\ntype: System\ntitle: Time server\ndescription: The GPS clock.\n---\n# Time\n",
                ),
                ("log.md", "# Log\n"),
            ]),
        ),
        ("work", files(&[])),
    ];
    let mut server = TestServer::start_serving(
        catalogs,
        |base| Config {
            public_url: Some(base.to_string()),
            oauth: OAuthConfig {
                cimd_allow_private: true,
                ..OAuthConfig::default()
            },
            ..Config::default()
        },
        true,
    )
    .await;
    let base = server.base.clone();
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(60))
        .build()
        .expect("client");

    // Credentials: a writer and an admin key (their secrets are secrets), and
    // a user who signs in with a password.
    let writer_key = server.key("writer", Access::Write, &[], Some("pg")).await;
    let admin_key = server.key("admin", Access::Admin, &[], None).await;
    let user_id = server
        .user_with_password(&user_name, &user_email, &password)
        .await;

    // --- MCP: a search, a read of a missing path, a write and a publish,
    // on a connection that also sends a header carrying a sentinel.
    let client = server
        .client_with(
            &writer_key,
            &[("x-zq-sentinel-7f3a-hdr", header_value.as_str())],
        )
        .await
        .expect("mcp client");
    call(&client, "search", json!({"query": search_text})).await;
    call(
        &client,
        "get_concept",
        json!({"path": format!("/infra/{path_segment}.md")}),
    )
    .await;
    let handle = open_changeset(&client, None).await;
    structured(
        &call(
            &client,
            "create_concept",
            json!({
                "path": format!("/infra/{concept_path}.md"),
                "frontmatter": {"type": "System", "title": &concept_path, "description": &concept_body},
                "body": format!("# {concept_path}\n\n{concept_body}\n"),
                "changeset": handle,
            }),
        )
        .await,
    );
    structured(
        &call(
            &client,
            "publish",
            json!({"message": &concept_body, "changeset": handle}),
        )
        .await,
    );

    // --- REST: a wrong bearer, a query string, a path segment, headers.
    http.get(format!("{base}/api/v1/whoami"))
        .bearer_auth(&bad_bearer)
        .send()
        .await
        .expect("whoami with a bad bearer");
    http.get(format!("{base}/api/v1/whoami?token={query_value}"))
        .bearer_auth(&admin_key)
        .header("x-zq-sentinel-7f3a-hdr", &header_value)
        .header("cookie", format!("{SENTINEL}-name={cookie_value}"))
        .header("user-agent", &agent)
        .header("referer", format!("http://example.com/{SENTINEL}-referer"))
        .header(
            "traceparent",
            "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
        )
        .header("tracestate", &tracestate)
        .send()
        .await
        .expect("whoami");
    let missing_user = http
        .get(format!("{base}/api/v1/users/{path_segment}"))
        .bearer_auth(&admin_key)
        .send()
        .await
        .expect("a user that does not exist");
    assert_eq!(missing_user.status(), 404);

    // --- Console session: a refused sign-in, then a real one, then a call
    // made with the cookie.
    let refused = http
        .post(format!("{base}/signin"))
        .header("sec-fetch-site", "same-origin")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form(&[
            ("email", &user_email),
            ("password", &wrong_password),
        ]))
        .send()
        .await
        .expect("a refused sign-in");
    assert_eq!(refused.status(), 401);
    let signed_in = http
        .post(format!("{base}/signin"))
        .header("sec-fetch-site", "same-origin")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form(&[("email", &user_email), ("password", &password)]))
        .send()
        .await
        .expect("sign-in");
    assert_eq!(signed_in.status(), 303, "sign-in: {signed_in:?}");
    let session = set_cookie(&signed_in, "okf_session").expect("a session cookie");
    let whoami = http
        .get(format!("{base}/api/v1/whoami"))
        .header("cookie", format!("okf_session={session}"))
        .send()
        .await
        .expect("whoami as the session");
    assert_eq!(whoami.status(), 200);

    // --- Mail: an invitation through the admin API, and a password reset
    // through the forgot-password form. The `log` provider writes both
    // messages to stderr only.
    let created = http
        .post(format!("{base}/api/v1/users"))
        .bearer_auth(&admin_key)
        .json(&json!({"name": invitee, "display_name": display_name}))
        .send()
        .await
        .expect("create the invitee");
    assert!(created.status().is_success(), "create: {created:?}");
    let emailed = http
        .put(format!("{base}/api/v1/users/{invitee}/email"))
        .bearer_auth(&admin_key)
        .json(&json!({"email": invitee_email}))
        .send()
        .await
        .expect("set the invitee's email");
    assert!(emailed.status().is_success(), "email: {emailed:?}");
    let invited = http
        .post(format!("{base}/api/v1/users/{invitee}/invite"))
        .bearer_auth(&admin_key)
        .send()
        .await
        .expect("invite");
    assert!(invited.status().is_success(), "invite: {invited:?}");
    let forgot = http
        .post(format!("{base}/signin/forgot"))
        .header("sec-fetch-site", "same-origin")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form(&[("email", &user_email)]))
        .send()
        .await
        .expect("forgot");
    assert!(forgot.status().is_success(), "forgot: {forgot:?}");
    // Wait for the worker to send both (to stderr), then learn their links.
    tokio::time::timeout(PATIENCE, async {
        while stderr.text().matches("not delivered").count() < 2 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the mail worker logged both messages to stderr");
    let link_secrets = link_tokens(&stderr.text());
    assert!(
        link_secrets.len() >= 2,
        "the setup and reset links were read from stderr: {link_secrets:?}"
    );

    // --- OAuth: a refused client id that is not a URL, and one that is a
    // URL nobody serves.
    let refusing = support::refused::Refused::new();
    let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
    for id in [
        client_id.clone(),
        format!("http://{}/{cimd_name}.json", refusing.address()),
    ] {
        let query = form(&[
            ("response_type", "code"),
            ("client_id", &id),
            ("redirect_uri", "http://127.0.0.1:9/callback"),
            ("state", "state"),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
        ]);
        let refused = support::refused::bounded(
            "an authorize that fetches a client document from a refusing address",
            http.get(format!("{base}/oauth/authorize?{query}")).send(),
        )
        .await
        .expect("authorize");
        assert!(refused.status().is_client_error(), "authorize: {refused:?}");
    }

    // --- Archives: a real export and import, and both routes for a catalog
    // that does not exist (the name must reach no span or label).
    let exported = http
        .get(format!("{base}/catalogs/pg/export"))
        .bearer_auth(&admin_key)
        .send()
        .await
        .expect("export");
    assert_eq!(exported.status(), 200);
    let tarball = exported.bytes().await.expect("tarball").to_vec();
    let imported = http
        .post(format!("{base}/catalogs/work/import"))
        .bearer_auth(&admin_key)
        .header("content-type", "application/gzip")
        .body(tarball)
        .send()
        .await
        .expect("import");
    assert!(imported.status().is_success(), "import: {imported:?}");
    let export_missing = http
        .get(format!("{base}/catalogs/{catalog_name}/export"))
        .bearer_auth(&admin_key)
        .send()
        .await
        .expect("export of a missing catalog");
    assert_eq!(export_missing.status(), 404);
    let import_missing = http
        .post(format!("{base}/catalogs/{catalog_name}/import"))
        .bearer_auth(&admin_key)
        .header("content-type", "application/gzip")
        .body(Vec::<u8>::new())
        .send()
        .await
        .expect("import into a missing catalog");
    assert!(
        import_missing.status().is_client_error(),
        "import into a missing catalog: {import_missing:?}"
    );

    // --- Internal errors whose text holds a sentinel: a trigger makes the
    // database refuse the next insert with a message naming it. One HTTP
    // request (a 500) and one MCP tool call hit it.
    let pool = &server.db.pool;
    // The message is a literal (sqlx refuses a built string); it must equal
    // `db_error`, which the scan looks for.
    assert_eq!(db_error, "zq-sentinel-7f3a-dberr");
    sqlx::query(
        "CREATE FUNCTION zq_refuse() RETURNS trigger LANGUAGE plpgsql AS \
         $$ BEGIN RAISE EXCEPTION 'zq-sentinel-7f3a-dberr'; END $$",
    )
    .execute(pool)
    .await
    .expect("create the trigger function");
    sqlx::query(
        "CREATE TRIGGER zq_refuse_mail BEFORE INSERT ON apalis.jobs \
         FOR EACH ROW EXECUTE FUNCTION zq_refuse()",
    )
    .execute(pool)
    .await
    .expect("create the mail trigger");
    // `catalog.changesets` is on the store, a database of its own in
    // `OKF_TEST_STORE=separate` mode; the function goes there too.
    let store = support::pg::store_pool(pool);
    sqlx::query(
        "CREATE OR REPLACE FUNCTION zq_refuse() RETURNS trigger LANGUAGE plpgsql AS \
         $$ BEGIN RAISE EXCEPTION 'zq-sentinel-7f3a-dberr'; END $$",
    )
    .execute(&store)
    .await
    .expect("create the trigger function on the store");
    sqlx::query(
        "CREATE TRIGGER zq_refuse_changesets BEFORE INSERT ON catalog.changesets \
         FOR EACH ROW EXECUTE FUNCTION zq_refuse()",
    )
    .execute(&store)
    .await
    .expect("create the changeset trigger");
    let failed = http
        .post(format!("{base}/api/v1/mail/test"))
        .bearer_auth(&admin_key)
        .json(&json!({"to": "someone@example.com"}))
        .send()
        .await
        .expect("a request that fails inside");
    assert_eq!(failed.status(), 500, "a forced 5xx: {failed:?}");
    let refused_tool = call(&client, "open_changeset", json!({"catalog": null})).await;
    assert_eq!(refused_tool.is_error, Some(true), "{refused_tool:?}");

    // --- Everything is done. Scrape first, then stop the server and flush
    // through `Telemetry::shutdown`, which is what proves the export finished.
    let scraped = http
        .get(&scrape_url)
        .send()
        .await
        .expect("scrape /metrics")
        .text()
        .await
        .expect("metrics text");
    client.cancel().await.expect("close the client");
    server.stop().await;
    scrape_stop.cancel();
    tokio::time::timeout(PATIENCE, scraping)
        .await
        .expect("the /metrics listener stops")
        .expect("its task")
        .expect("it serves without an I/O error");
    let report = telemetry.shutdown().await;
    assert_eq!(
        (&report.traces, &report.logs, &report.metrics),
        (
            &ProviderOutcome::Flushed,
            &ProviderOutcome::Flushed,
            &ProviderOutcome::Flushed
        ),
        "every provider flushed, none abandoned: {report:?}"
    );

    // --- Positive assertions: the records that could carry a leak arrived.
    // A quiet collector would pass every negative check below, so these come
    // first and fail the test as "not exported".
    let requests = collector
        .wait_for(
            "spans, logs and metrics",
            |requests| {
                !otlp::spans(requests).is_empty()
                    && !otlp::logs(requests).is_empty()
                    && !otlp::metrics(requests).is_empty()
            },
            PATIENCE,
        )
        .await;
    for request in &requests {
        assert!(
            !matches!(request.payload, Payload::Undecodable(_)),
            "an export did not decode: {:?}",
            request.payload
        );
    }
    let spans = otlp::spans(&requests);
    let logs = otlp::logs(&requests);
    let span_routes: BTreeSet<String> = spans
        .iter()
        .filter_map(|span| otlp::attribute(&span.attributes, "http.route"))
        .collect();
    let mut quiet_routes = Vec::new();
    for route in ROUTES {
        assert!(
            span_routes.contains(*route),
            "no span for route {route}; routes seen: {span_routes:?}"
        );
        let traces = trace_ids_of_route(&requests, route);
        if !logs.iter().any(|record| traces.contains(&record.trace_id)) {
            quiet_routes.push(*route);
        }
    }
    // The forgot-password form does its work after it has answered, on a
    // task of its own, so its trace holds no log record; its mail job is
    // checked below.
    let expected_quiet: &[&str] = &["/signin/forgot"];
    assert_eq!(
        quiet_routes, expected_quiet,
        "the routes whose trace holds no log record"
    );
    // The exported halves of the events that carry caller text, error text
    // and principal text arrived: a scan that finds nothing in them means
    // something only because they were exported.
    let log_bodies: BTreeSet<String> = logs
        .iter()
        .map(|record| record.body.as_ref().map(otlp::any_text).unwrap_or_default())
        .collect();
    for body in [
        "refused an OAuth client",
        "request failed",
        "tool error",
        "setup email queued",
        "mail queued",
        "mail sent",
        "console session started",
        "sign-in attempt",
        "imported",
    ] {
        assert!(
            log_bodies.contains(body),
            "no log record `{body}`; bodies seen: {log_bodies:?}"
        );
    }
    let span_names: BTreeSet<&str> = spans.iter().map(|span| span.name.as_str()).collect();
    for name in [
        "mcp search",
        "okf.publish",
        "okf.archive.export",
        "okf.archive.import",
        "okf.mail.job",
    ] {
        assert!(
            span_names.contains(name),
            "no `{name}` span; names seen: {span_names:?}"
        );
    }
    let metric_points: HashMap<&str, usize> = otlp::metrics(&requests)
        .into_iter()
        .map(|metric| (metric.name.as_str(), otlp::point_count(metric)))
        .fold(HashMap::new(), |mut totals, (name, points)| {
            *totals.entry(name).or_default() += points;
            totals
        });
    for name in [
        "http.server.request.duration",
        "okf.mcp.tool.duration",
        "okf.publish.duration",
    ] {
        assert!(
            metric_points.get(name).copied().unwrap_or(0) > 0,
            "no data point for {name}; metrics seen: {metric_points:?}"
        );
    }
    assert!(
        scraped.contains("http_server_request_duration") && scraped.contains("okf_build_info"),
        "/metrics served the families: {scraped}"
    );
    // The console session is the case the plan names: its user name is never
    // a key id, and the user's id is the end user.
    let session_span = spans
        .iter()
        .find(|span| {
            otlp::attribute(&span.attributes, "http.route").as_deref() == Some("/api/v1/whoami")
                && otlp::attribute(&span.attributes, "okf.key.id").as_deref() == Some("session")
        })
        .expect("a whoami span for the console session, with okf.key.id = session");
    assert_eq!(
        otlp::attribute(&session_span.attributes, "enduser.id").as_deref(),
        Some(user_id.as_str())
    );
    // The mail provider's own output reached stderr, which is how the links
    // were learned: proof the mail path ran, and that stderr is where it stays.
    assert!(stderr.text().contains("not delivered"));

    // --- No dependency records, no request-shaped attributes, no SQL.
    // A dependency's log record would carry its own wording or SQL keys; no
    // span or record carries a URL, a query, an address or a header.
    for body in &log_bodies {
        for crate_name in [
            "sqlx",
            "apalis",
            "rmcp",
            "tower_http",
            "hyper",
            "reqwest",
            "rustls",
        ] {
            assert!(
                !body.contains(crate_name),
                "a log record from a dependency? `{body}`"
            );
        }
    }
    const FORBIDDEN_KEYS: &[&str] = &[
        "url.full",
        "url.path",
        "url.query",
        "http.url",
        "http.target",
        "user_agent.original",
        "client.address",
        "db.statement",
        "db.query.text",
        "error.message",
        "exception.message",
    ];
    let attribute_keys = spans
        .iter()
        .flat_map(|span| span.attributes.iter().map(|pair| pair.key.as_str()))
        .chain(
            logs.iter()
                .flat_map(|record| record.attributes.iter().map(|pair| pair.key.as_str())),
        );
    for key in attribute_keys {
        assert!(
            !FORBIDDEN_KEYS.contains(&key) && !key.starts_with("http.request.header."),
            "an exported attribute named {key}"
        );
    }
    // A dependency's span would carry its crate in the namespace attribute.
    for span in &spans {
        if let Some(namespace) = otlp::attribute(&span.attributes, "code.namespace") {
            assert!(
                namespace == "okf_mcp" || namespace.starts_with("okf_mcp::"),
                "span {} comes from {namespace}",
                span.name
            );
        }
    }
    let tables: Vec<String> = migration_tables().into_iter().collect();
    let sql = regex::Regex::new(&format!(
        r#"(?i)\b(select\b[^\x00]{{0,400}}?\bfrom|insert\s+into|update|delete\s+from)\s+"?(?:\w+\.)?({})\b"#,
        tables.join("|")
    ))
    .expect("the SQL pattern");

    // --- Only now: nothing planted, and nothing real, is in any exported
    // byte or on /metrics.
    let mut secrets = vec![
        secret("the sentinel itself", SENTINEL),
        secret("the OTLP header token", &otlp_token),
        secret("the user's name", &user_name),
        secret("the user's email", &user_email),
        secret("the user's password", &password),
        secret("a wrong password", &wrong_password),
        secret("the invitee's name", &invitee),
        secret("the invitee's email", &invitee_email),
        secret("a display name", &display_name),
        secret("a concept body", &concept_body),
        secret("a concept path", &concept_path),
        secret("a search text", &search_text),
        secret("a query value", &query_value),
        secret("a path segment", &path_segment),
        secret("a header value", &header_value),
        secret("a cookie value", &cookie_value),
        secret("a tracestate", &tracestate),
        secret("a user agent", &agent),
        secret("a wrong bearer", &bad_bearer),
        secret("an OAuth client id", &client_id),
        secret("a CIMD client id", &cimd_name),
        secret("a catalog name", &catalog_name),
        secret("a database error", &db_error),
        secret("the writer key's secret", &writer_key),
        secret("the admin key's secret", &admin_key),
        secret("the session cookie", &session),
    ];
    for token in &link_secrets {
        secrets.push(secret("a token from an emailed link", token));
    }
    let mut leaks = Vec::new();
    for request in &requests {
        let text = String::from_utf8_lossy(&request.body);
        for planted in &secrets {
            if text.contains(planted.value.as_str()) {
                leaks.push(format!(
                    "{} ({}) in a {} export",
                    planted.label, planted.value, request.path
                ));
            }
        }
        if let Some(found) = sql.find(&text) {
            leaks.push(format!(
                "SQL text `{}` in a {} export",
                found.as_str(),
                request.path
            ));
        }
        // Request headers the collector saw are the exporter's own; the
        // body is what carries telemetry.
    }
    for planted in &secrets {
        if scraped.contains(planted.value.as_str()) {
            leaks.push(format!("{} ({}) on /metrics", planted.label, planted.value));
        }
    }
    assert!(
        leaks.is_empty(),
        "secrets reached an exporter or /metrics:\n{}",
        leaks.join("\n")
    );
}

/// The attributes of every data point of `metric`, whatever its kind.
fn point_attributes(metric: &Metric) -> Vec<&[OtlpAttribute]> {
    match &metric.data {
        Some(metric::Data::Gauge(gauge)) => gauge
            .data_points
            .iter()
            .map(|point| point.attributes.as_slice())
            .collect(),
        Some(metric::Data::Sum(sum)) => sum
            .data_points
            .iter()
            .map(|point| point.attributes.as_slice())
            .collect(),
        Some(metric::Data::Histogram(histogram)) => histogram
            .data_points
            .iter()
            .map(|point| point.attributes.as_slice())
            .collect(),
        Some(metric::Data::ExponentialHistogram(histogram)) => histogram
            .data_points
            .iter()
            .map(|point| point.attributes.as_slice())
            .collect(),
        Some(metric::Data::Summary(summary)) => summary
            .data_points
            .iter()
            .map(|point| point.attributes.as_slice())
            .collect(),
        None => Vec::new(),
    }
}

/// Issue #1157: no catalog name is a metric attribute, on `/metrics` or in an
/// OTLP export, and the same operation on two catalogs lands in one series.
///
/// It exercises an MCP call, a successful and a failed publish (which waits
/// for the lock and, failing, is refused by the lint), and an archive export,
/// each on two catalogs with recognizable synthetic names. The trace and log
/// policy is not under test here; only metrics are scanned.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_catalog_name_is_a_metric_attribute() {
    let _serial = SERIAL.lock().await;
    let names = [
        format!("{SENTINEL}-alpha-cat"),
        format!("{SENTINEL}-beta-cat"),
    ];

    // Telemetry exporting metrics to the collector and a `/metrics` listener.
    // The global tracing subscriber is not installed (the sentinel test owns
    // it); the instruments need only the meter provider `Telemetry::start`
    // sets.
    let collector = Collector::start().await;
    let config = telemetry_config(endpoint_without_headers(&collector.url()));
    let telemetry = Telemetry::start(&config, &|_| None).expect("telemetry starts");
    let scrape_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind /metrics");
    let scrape_url = format!(
        "http://{}/metrics",
        scrape_listener.local_addr().expect("addr")
    );
    let scrape_stop = CancellationToken::new();
    let scraping = tokio::spawn(okf_mcp::telemetry::scrape::serve(
        scrape_listener,
        telemetry.registry(),
        scrape_stop.clone(),
    ));

    let tree = || {
        files(&[
            ("index.md", "# Home\n"),
            (
                "infra/index.md",
                "# Infra\n\n* [Time server](time.md) - the GPS clock\n",
            ),
            (
                "infra/time.md",
                "---\ntype: System\ntitle: Time server\ndescription: The GPS clock.\n---\n# Time\n",
            ),
            ("log.md", "# Log\n"),
        ])
    };
    let mut server = TestServer::start(
        names.iter().map(|name| (name.as_str(), tree())).collect(),
        Config::default(),
    )
    .await;
    let base = server.base.clone();
    let key = server.key("writer", Access::Admin, &[], None).await;
    let client = server.client(&key, None).await.expect("mcp client");

    for name in &names {
        // An MCP call on the catalog.
        let read = call(
            &client,
            "get_concept",
            json!({"path": "/infra/time.md", "catalog": name}),
        )
        .await;
        structured(&read);
        // A successful publish.
        let handle = open_changeset(&client, Some(name)).await;
        structured(
            &call(
                &client,
                "create_concept",
                json!({
                    "path": "/infra/ok.md",
                    "frontmatter": {"type": "System", "title": "Ok", "description": "Fine."},
                    "body": "# Ok\n\nFine.\n",
                    "changeset": handle,
                }),
            )
            .await,
        );
        structured(
            &call(
                &client,
                "publish",
                json!({"message": "add ok", "changeset": handle}),
            )
            .await,
        );
        // A failed publish: a broken link is a lint error.
        let handle = open_changeset(&client, Some(name)).await;
        structured(
            &call(
                &client,
                "create_concept",
                json!({
                    "path": "/infra/broken.md",
                    "frontmatter": {"type": "System", "title": "Broken", "description": "Bad."},
                    "body": "# Broken\n\nSee [gone](/gone.md).\n",
                    "changeset": handle,
                }),
            )
            .await,
        );
        let refused = call(
            &client,
            "publish",
            json!({"message": "add broken", "changeset": handle}),
        )
        .await;
        assert_eq!(refused.is_error, Some(true), "{refused:?}");
    }

    // A successful archive export of each catalog.
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .expect("client");
    for name in &names {
        let exported = http
            .get(format!("{base}/catalogs/{name}/export"))
            .bearer_auth(&key)
            .send()
            .await
            .expect("export");
        assert_eq!(exported.status(), 200, "export of {name}");
        exported.bytes().await.expect("tarball");
    }

    let scraped = http
        .get(&scrape_url)
        .send()
        .await
        .expect("scrape /metrics")
        .text()
        .await
        .expect("metrics text");
    client.cancel().await.expect("close the client");
    server.stop().await;
    scrape_stop.cancel();
    tokio::time::timeout(PATIENCE, scraping)
        .await
        .expect("the /metrics listener stops")
        .expect("its task")
        .expect("it serves without an I/O error");
    let report = telemetry.shutdown().await;
    assert_eq!(report.metrics, ProviderOutcome::Flushed, "{report:?}");
    let requests = collector
        .wait_for(
            "metrics",
            |requests| !otlp::metrics(requests).is_empty(),
            PATIENCE,
        )
        .await;

    // The operations happened (so the checks below are not vacuous), and the
    // two catalogs share one series each: two publishes ok, two refused, two
    // lock waits per outcome counted together, two tool calls, two exports.
    let series = |prefix: &str| -> Vec<&str> {
        scraped
            .lines()
            .filter(|line| line.starts_with(prefix))
            .collect()
    };
    assert_eq!(
        series("okf_publish_duration_seconds_count"),
        [
            r#"okf_publish_duration_seconds_count{outcome="error"} 2"#,
            r#"okf_publish_duration_seconds_count{outcome="ok"} 2"#,
        ],
        "{scraped}"
    );
    assert_eq!(
        series("okf_publish_lock_wait_seconds_count"),
        ["okf_publish_lock_wait_seconds_count 4"],
        "{scraped}"
    );
    let rejections = series("okf_lint_rejections_total");
    assert_eq!(rejections.len(), 1, "{scraped}");
    assert!(
        rejections[0].starts_with(r#"okf_lint_rejections_total{rule="broken-link"} "#),
        "{rejections:?}"
    );
    let archives = series("okf_archive_size_bytes_total");
    assert_eq!(archives.len(), 1, "{scraped}");
    assert!(
        archives[0].starts_with(r#"okf_archive_size_bytes_total{direction="export"} "#),
        "{archives:?}"
    );
    assert_eq!(
        series(r#"okf_archive_duration_seconds_count{direction="export",outcome="ok"}"#),
        [r#"okf_archive_duration_seconds_count{direction="export",outcome="ok"} 2"#],
        "{scraped}"
    );
    let reads: Vec<&str> = series("okf_mcp_tool_duration_seconds_count")
        .into_iter()
        .filter(|line| line.contains(r#"tool="get_concept""#))
        .collect();
    assert_eq!(
        reads,
        [r#"okf_mcp_tool_duration_seconds_count{outcome="ok",tool="get_concept"} 2"#],
        "{scraped}"
    );

    // No catalog name, and no `catalog` label, on /metrics or in any OTLP
    // metric attribute.
    for name in &names {
        assert!(!scraped.contains(name.as_str()), "{name} on /metrics");
    }
    assert!(!scraped.contains("catalog="), "a catalog label: {scraped}");
    let mut exported_points = 0;
    for metric in otlp::metrics(&requests) {
        for attributes in point_attributes(metric) {
            exported_points += 1;
            for pair in attributes {
                let value = pair.value.as_ref().map(otlp::any_text).unwrap_or_default();
                assert_ne!(pair.key, "catalog", "{} has a catalog key", metric.name);
                for name in &names {
                    assert!(
                        !value.contains(name.as_str()),
                        "{} carries {name} as {}",
                        metric.name,
                        pair.key
                    );
                }
            }
        }
    }
    assert!(exported_points > 0, "no OTLP metric points arrived");
}
