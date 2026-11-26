//! Task 3: the `mcp <tool>` span, its error kind, and the tool and session
//! metrics, through a real MCP session over a loopback socket.

use std::sync::Arc;
use std::time::Duration;

use opentelemetry::trace::Status;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use super::*;
use crate::config::Config;
use crate::mcp::testing::{app_with_metrics, render, McpProbe, TestApp};
use crate::store::keys::{self, Access, Grants, NewKey};
use crate::telemetry::config::LogFormat;
use crate::telemetry::subscriber::capture::{attribute, Observer};

const FILTERS: [&str; 2] = ["info", "trace"];

/// A value that must never appear in an exported span or a metric.
const SENTINEL: &str = "zq-sentinel-8e2b";

/// A served app, its key, and the means to stop it.
struct Served {
    test: TestApp,
    registry: prometheus::Registry,
    endpoint: String,
    key: String,
    shutdown: CancellationToken,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Served {
    /// Starts `http::serve` for an app holding catalog `pg`, with a read key
    /// named `reader`. A `#[tokio::test]` runtime is current-thread, so the
    /// server's tasks run on the test's thread and see its subscriber.
    async fn start() -> Self {
        Self::start_with(Config::default()).await
    }

    /// [`Served::start`] under `config`.
    async fn start_with(config: Config) -> Self {
        let (test, registry) = app_with_metrics(
            &[(
                "pg",
                &[(
                    "a.md",
                    "---\ntype: Note\ntitle: Alpha\n---\n# Alpha\nText.\n",
                )],
            )],
            config,
        )
        .await;
        let key = keys::create(
            test.app.accounts().executor(),
            &NewKey {
                name: "reader",
                access: Access::Read,
                maintenance_report: false,
                grants: Grants::Catalogs(&["pg".into()]),
                default_catalog: None,
                actor: "reader",
                user: crate::db::TEST_USER,
                self_issued: false,
                expires_at: None,
                levels: &[],
            },
            &crate::store::audit::AuditActor::cli(),
            chrono::Utc::now(),
        )
        .await
        .expect("create a key");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let endpoint = format!("http://{}/mcp", listener.local_addr().expect("address"));
        let shutdown = CancellationToken::new();
        let server = tokio::spawn(crate::http::serve_with(
            Arc::clone(&test.app),
            listener,
            shutdown.clone(),
            crate::http::ServeOptions::NO_WORKER,
        ));
        Self {
            test,
            registry,
            endpoint,
            key,
            shutdown,
            server,
        }
    }

    /// Stops the server and waits for it, bounded.
    async fn stop(self) {
        self.shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(30), self.server)
            .await
            .expect("the server stops")
            .expect("the server task")
            .expect("the server returns");
    }
}

#[tokio::test]
async fn every_tool_call_opens_one_mcp_span() {
    let served = Served::start().await;
    for filter in FILTERS {
        let observer = Observer::start("off", filter, "off", LogFormat::Text);
        let probe = McpProbe::open(&served.endpoint, &served.key).await;
        let answer = probe
            .call(
                "get_concept",
                serde_json::json!({"path": "/a.md", "catalog": "pg"}),
            )
            .await;
        assert_eq!(answer["result"]["isError"], false, "{answer}");
        let unloaded = probe
            .call(
                "get_concept",
                serde_json::json!({"path": "/a.md", "catalog": SENTINEL}),
            )
            .await;
        assert!(unloaded.get("result").is_some(), "{unloaded}");
        let key_id: String = sqlx::query_scalar("SELECT id FROM api_keys")
            .fetch_one(served.test.app.accounts().executor())
            .await
            .expect("the one key");
        probe.close().await;
        let observed = observer.finish();

        let spans = observed.spans_named("mcp get_concept");
        assert_eq!(spans.len(), 2, "{filter}: {:?}", observed.spans);
        let loaded = spans
            .iter()
            .find(|span| {
                attribute(span, "okf.catalog")
                    .map(ToString::to_string)
                    .as_deref()
                    == Some("pg")
            })
            .unwrap_or_else(|| panic!("{filter}: a span for pg"));
        assert_eq!(
            attribute(loaded, "okf.tool")
                .map(ToString::to_string)
                .as_deref(),
            Some("get_concept")
        );
        assert_eq!(
            attribute(loaded, "okf.outcome")
                .map(ToString::to_string)
                .as_deref(),
            Some("ok")
        );
        assert_eq!(
            attribute(loaded, "okf.key.id")
                .map(ToString::to_string)
                .as_deref(),
            Some(key_id.as_str())
        );
        assert!(attribute(loaded, "enduser.id").is_some(), "{filter}");
        assert_ne!(loaded.status, Status::error(""), "{filter}");
        // An unloaded catalog name is never an attribute.
        let none = spans
            .iter()
            .find(|span| {
                attribute(span, "okf.catalog")
                    .map(ToString::to_string)
                    .as_deref()
                    == Some("none")
            })
            .unwrap_or_else(|| panic!("{filter}: a span for no catalog"));
        assert_eq!(none.name, "mcp get_concept");
        assert!(
            !format!("{:?}", observed.spans).contains(SENTINEL),
            "{filter}: the typed catalog name leaked"
        );
    }
    served.stop().await;
}

#[tokio::test]
async fn a_tool_error_records_its_kind_not_its_message() {
    let served = Served::start().await;
    for filter in FILTERS {
        let observer = Observer::start("off", filter, "off", LogFormat::Text);
        let probe = McpProbe::open(&served.endpoint, &served.key).await;
        let answer = probe
            .call(
                "get_concept",
                serde_json::json!({"path": format!("/{SENTINEL}.md"), "catalog": "pg"}),
            )
            .await;
        assert_eq!(answer["result"]["isError"], true, "{answer}");
        probe.close().await;
        let observed = observer.finish();

        let span = &observed.spans_named("mcp get_concept")[0];
        assert_eq!(
            attribute(span, "okf.outcome")
                .map(ToString::to_string)
                .as_deref(),
            Some("error"),
            "{filter}"
        );
        assert_eq!(
            attribute(span, "okf.error.kind")
                .map(ToString::to_string)
                .as_deref(),
            Some("NotFound"),
            "{filter}"
        );
        assert!(matches!(span.status, Status::Error { .. }), "{filter}");
        assert!(
            !format!("{:?}", observed.spans).contains(SENTINEL),
            "{filter}: the path leaked"
        );
    }
    served.stop().await;
}

/// `tool_error` records the kind on the tool's own span even when a helper
/// span (as the sweep adds) is current.
#[tokio::test]
async fn error_kind_lands_on_the_tool_span_under_helper_spans() {
    for filter in FILTERS {
        let observer = Observer::start("off", filter, "off", LogFormat::Text);
        let span = tracing::info_span!("mcp", okf.error.kind = tracing::field::Empty,);
        TOOL_SPAN
            .scope(span.clone(), async {
                async {
                    let _ = tool_error(&OkfError::NotFound {
                        path: "/x.md".to_string(),
                    });
                }
                .instrument(tracing::debug_span!("tool method"))
                .await;
            })
            .instrument(span)
            .await;
        let observed = observer.finish();

        let tool = &observed.spans_named("mcp")[0];
        assert_eq!(
            attribute(tool, "okf.error.kind")
                .map(ToString::to_string)
                .as_deref(),
            Some("NotFound"),
            "{filter}"
        );
    }
}

/// Issue #766: an internal error's `trace_id` is the tool span's OpenTelemetry trace id, the
/// one the exported span carries, and the detail stays out of the body.
#[tokio::test]
async fn an_internal_error_carries_the_tool_spans_trace_id() {
    for filter in FILTERS {
        let observer = Observer::start("off", filter, "off", LogFormat::Text);
        let span = tracing::info_span!("mcp", okf.error.kind = tracing::field::Empty);
        let result = TOOL_SPAN
            .scope(span.clone(), async {
                tool_error(&OkfError::Database {
                    source: sqlx::Error::Protocol(SENTINEL.to_string()),
                })
            })
            .instrument(span)
            .await;
        let observed = observer.finish();

        let body = result.structured_content.expect("a structured body");
        let exported = observed.spans_named("mcp")[0].span_context.trace_id();
        assert_eq!(body["trace_id"], exported.to_string(), "{filter}");
        assert_eq!(body["trace_id"].as_str().map(str::len), Some(32));
        assert_ne!(body["trace_id"], "0".repeat(32), "{filter}");
        assert!(!body.to_string().contains(SENTINEL), "{filter}");
    }
}

/// Issue #766: with no valid trace context the id is left out rather than sent as zeros.
#[tokio::test]
async fn an_internal_error_omits_trace_id_without_a_trace() {
    let span = tracing::Span::none();
    let result = TOOL_SPAN
        .scope(span, async {
            tool_error(&OkfError::Database {
                source: sqlx::Error::Protocol("x".to_string()),
            })
        })
        .await;

    let body = result.structured_content.expect("a structured body");
    assert!(body.get("trace_id").is_none(), "{body}");
}

/// Issue #766, end to end: a failing call's body carries the exported tool span's trace id,
/// and the `report` hint exactly when `[feedback]` offers `send_feedback`.
#[tokio::test]
async fn a_served_internal_error_quotes_the_trace_id_and_points_at_send_feedback() {
    for feedback in [true, false] {
        let config = Config {
            feedback: feedback.then(|| crate::config::FeedbackConfig::for_form("form-766")),
            ..Config::default()
        };
        let served = Served::start_with(config).await;
        let observer = Observer::start("off", "debug", "off", LogFormat::Text);
        let probe = McpProbe::open(&served.endpoint, &served.key).await;
        // A catalog table the tool reads goes missing: a driver error, not a policy refusal.
        sqlx::query("ALTER TABLE catalog.changesets RENAME TO changesets_gone")
            .execute(&crate::db::store_pool(
                served.test.app.accounts().executor(),
            ))
            .await
            .expect("break the table");
        let answer = probe
            .call("list_changesets", serde_json::json!({"catalog": "pg"}))
            .await;
        probe.close().await;
        let observed = observer.finish();

        assert_eq!(answer["result"]["isError"], true, "{answer}");
        let body = &answer["result"]["structuredContent"];
        assert_eq!(body["error"], "Database", "{feedback}: {body}");
        let span = &observed.spans_named("mcp list_changesets")[0];
        assert_eq!(
            body["trace_id"],
            span.span_context.trace_id().to_string(),
            "{feedback}"
        );
        if feedback {
            let report = body["report"].as_str().expect("a report");
            assert!(report.contains("send_feedback (kind: bug, tool: list_changesets)"));
            assert!(report.contains("quoting trace_id"));
        } else {
            assert!(body.get("report").is_none(), "{body}");
        }
        served.stop().await;
    }
}

#[tokio::test]
async fn an_unknown_tool_name_is_labelled_other() {
    let served = Served::start().await;
    let probe = McpProbe::open(&served.endpoint, &served.key).await;

    let answer = probe.call(SENTINEL, serde_json::json!({})).await;
    probe.close().await;

    assert!(answer.get("error").is_some(), "{answer}");
    let text = render(&served.registry);
    let line = text
        .lines()
        .find(|line| line.starts_with("okf_mcp_tool_duration_seconds_count{"))
        .unwrap_or_else(|| panic!("no tool series in {text}"));
    assert!(line.contains(r#"tool="other""#), "{line}");
    assert!(line.contains(r#"outcome="error""#), "{line}");
    assert!(!text.contains(SENTINEL), "{text}");
    served.stop().await;
}

#[tokio::test]
async fn tool_duration_is_recorded_per_tool_catalog_outcome() {
    let served = Served::start().await;
    let probe = McpProbe::open(&served.endpoint, &served.key).await;
    probe
        .call(
            "get_concept",
            serde_json::json!({"path": "/a.md", "catalog": "pg"}),
        )
        .await;
    probe
        .call(
            "get_concept",
            serde_json::json!({"path": "/missing.md", "catalog": "pg"}),
        )
        .await;
    probe.close().await;

    let text = render(&served.registry);

    let series = |tool: &str, outcome: &str| {
        text.lines()
            .filter(|line| line.starts_with("okf_mcp_tool_duration_seconds_count{"))
            .find(|line| {
                line.contains(&format!(r#"tool="{tool}""#))
                    && line.contains(r#"catalog="pg""#)
                    && line.contains(&format!(r#"outcome="{outcome}""#))
            })
            .map(str::to_string)
    };
    assert!(
        series("get_concept", "ok").is_some_and(|line| line.ends_with(" 1")),
        "{text}"
    );
    assert!(
        series("get_concept", "error").is_some_and(|line| line.ends_with(" 1")),
        "{text}"
    );
    assert!(
        text.contains("okf_mcp_tool_duration_seconds_bucket{"),
        "{text}"
    );
    served.stop().await;
}

#[tokio::test]
async fn an_open_session_counts_until_it_closes() {
    let served = Served::start().await;
    let gauge = |text: &str| {
        text.lines()
            .find(|line| line.starts_with("okf_mcp_sessions_active"))
            .map(str::to_string)
    };
    let probe = McpProbe::open(&served.endpoint, &served.key).await;
    assert!(
        gauge(&render(&served.registry)).is_some_and(|line| line.ends_with(" 1")),
        "{}",
        render(&served.registry)
    );

    probe.close().await;

    // rmcp drops the handler after it has torn the session down.
    tokio::time::timeout(Duration::from_secs(30), async {
        while !gauge(&render(&served.registry)).is_some_and(|line| line.ends_with(" 0")) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the session gauge returns to 0");
    served.stop().await;
}

/// The `mcp <tool>` span is a child of the `POST /mcp` request span in the
/// same trace, and at `trace` the tool's own spans nest under it (issue #523).
#[tokio::test]
async fn the_tool_span_is_a_child_of_the_request_span() {
    let served = Served::start().await;
    for filter in FILTERS {
        let observer = Observer::start("off", filter, "off", LogFormat::Text);
        let probe = McpProbe::open(&served.endpoint, &served.key).await;
        let answer = probe
            .call(
                "get_concept",
                serde_json::json!({"path": "/a.md", "catalog": "pg"}),
            )
            .await;
        assert_eq!(answer["result"]["isError"], false, "{answer}");
        probe.close().await;
        let observed = observer.finish();

        let tool = &observed.spans_named("mcp get_concept")[0];
        let parent = observed
            .spans
            .iter()
            .find(|span| span.span_context.span_id() == tool.parent_span_id)
            .unwrap_or_else(|| panic!("{filter}: the tool span has no exported parent"));
        assert_eq!(parent.name, "POST /mcp", "{filter}");
        assert_eq!(
            parent.span_context.trace_id(),
            tool.span_context.trace_id(),
            "{filter}: the tool span is in another trace"
        );
        if filter == "trace" {
            let children = observed
                .spans
                .iter()
                .filter(|span| span.parent_span_id == tool.span_context.span_id())
                .count();
            assert!(children > 0, "trace: no span under the tool span");
        }
    }
    served.stop().await;
}

/// Without a request span in the extensions the parent is the current span,
/// and with none of those the tool span is a root; with one it is the request
/// span even when another span is current.
#[tokio::test]
async fn the_tool_span_parent_falls_back_to_current_then_root() {
    let _observer = Observer::start("off", "info", "off", LogFormat::Text);
    let request = tracing::info_span!("request");
    let mut parts = axum::http::Request::new(()).into_parts().0;
    parts
        .extensions
        .insert(crate::http::RequestSpan(request.clone()));
    let mut with_request = rmcp::model::Extensions::new();
    with_request.insert(parts);
    let without = rmcp::model::Extensions::new();

    assert_eq!(tool_span_parent(&without), None);
    let current = tracing::info_span!("current");
    let _entered = current.enter();
    assert_eq!(tool_span_parent(&without), current.id());
    assert_eq!(tool_span_parent(&with_request), request.id());
}

/// A write key for `pg`, for the tests that open a changeset.
async fn write_key(served: &Served) -> String {
    keys::create(
        served.test.app.accounts().executor(),
        &NewKey {
            name: "writer",
            access: Access::Write,
            maintenance_report: false,
            grants: Grants::Catalogs(&["pg".into()]),
            default_catalog: None,
            actor: "writer",
            user: crate::db::TEST_USER,
            self_issued: false,
            expires_at: None,
            levels: &[],
        },
        &crate::store::audit::AuditActor::cli(),
        chrono::Utc::now(),
    )
    .await
    .expect("create a write key")
}

/// An attribute's value as text.
fn text_of(span: &opentelemetry_sdk::trace::SpanData, name: &str) -> Option<String> {
    attribute(span, name).map(ToString::to_string)
}

/// Issues #1113 and #834: the tool span carries a digest of the session and of the changeset
/// handle (never the raw values), and the catalog a changeset or the selection resolved.
#[tokio::test]
async fn the_tool_span_records_the_session_the_changeset_and_the_resolved_catalog() {
    let served = Served::start().await;
    let writer = write_key(&served).await;
    for filter in FILTERS {
        let observer = Observer::start("off", filter, "off", LogFormat::Text);
        let probe = McpProbe::open(&served.endpoint, &writer).await;
        let opened = probe
            .call("open_changeset", serde_json::json!({"catalog": "pg"}))
            .await;
        let handle = opened["result"]["structuredContent"]["changeset"]
            .as_str()
            .unwrap_or_else(|| panic!("{filter}: a handle in {opened}"))
            .to_string();
        // Only a changeset: no `catalog` argument.
        let pending = probe
            .call("pending_changes", serde_json::json!({"changeset": handle}))
            .await;
        assert_eq!(pending["result"]["isError"], false, "{pending}");
        // Only the key's one catalog: no argument, no changeset.
        let found = probe
            .call("search", serde_json::json!({"query": "Alpha"}))
            .await;
        assert_eq!(found["result"]["isError"], false, "{found}");
        let session = probe.session().to_string();
        probe.close().await;
        let observed = observer.finish();

        let session_token = digest_token("session", &session);
        let handle_token = digest_token("changeset", &handle);
        let pending = &observed.spans_named("mcp pending_changes")[0];
        assert_eq!(text_of(pending, "okf.catalog").as_deref(), Some("pg"));
        // Recorded once: a second record would be exported as a second attribute.
        let catalogs = pending
            .attributes
            .iter()
            .filter(|attribute| attribute.key.as_str() == "okf.catalog")
            .count();
        assert_eq!(catalogs, 1, "{filter}");
        assert_eq!(
            text_of(pending, "okf.changeset").as_deref(),
            Some(handle_token.as_str()),
            "{filter}"
        );
        assert_eq!(
            text_of(pending, "mcp.session.id").as_deref(),
            Some(session_token.as_str()),
            "{filter}"
        );
        let search = &observed.spans_named("mcp search")[0];
        assert_eq!(text_of(search, "okf.catalog").as_deref(), Some("pg"));
        assert_eq!(
            text_of(search, "mcp.session.id").as_deref(),
            Some(session_token.as_str()),
            "{filter}: one session's calls share a token"
        );
        // A call that names no changeset has no `okf.changeset`.
        assert!(attribute(search, "okf.changeset").is_none(), "{filter}");
        let exported = format!("{:?}", observed.spans);
        assert!(!exported.contains(&session), "{filter}: the raw session id");
        assert!(!exported.contains(&handle), "{filter}: the raw handle");
    }
    served.stop().await;
}

/// Issue #834: the metrics label is the resolved catalog too, and `none` when nothing resolved.
#[tokio::test]
async fn the_metrics_label_is_the_resolved_catalog() {
    let served = Served::start().await;
    let probe = McpProbe::open(&served.endpoint, &served.key).await;
    probe
        .call("search", serde_json::json!({"query": "Alpha"}))
        .await;
    probe
        .call(
            "get_concept",
            serde_json::json!({"path": "/a.md", "catalog": SENTINEL}),
        )
        .await;
    probe.close().await;

    let text = render(&served.registry);
    let counted = |tool: &str, catalog: &str| {
        text.lines().any(|line| {
            line.starts_with("okf_mcp_tool_duration_seconds_count{")
                && line.contains(&format!(r#"tool="{tool}""#))
                && line.contains(&format!(r#"catalog="{catalog}""#))
        })
    };
    assert!(counted("search", "pg"), "{text}");
    assert!(counted("get_concept", "none"), "{text}");
    assert!(!text.contains(SENTINEL), "{text}");
    served.stop().await;
}

/// Issue #1113: a call that did not arrive over HTTP has no session to name.
#[test]
fn a_call_without_a_session_has_no_session_digest() {
    let (without, _) = axum::http::Request::new(()).into_parts();
    let (with, _) = axum::http::Request::builder()
        .header(crate::selection::SESSION_HEADER, "abc")
        .body(())
        .expect("a request")
        .into_parts();

    assert_eq!(session_digest(None), None);
    assert_eq!(session_digest(Some(&without)), None);
    let token = session_digest(Some(&with)).expect("a digest");
    assert_eq!(token.len(), 16);
    assert!(!token.contains("abc"));
    assert_ne!(Some(token), session_digest(None), "digests differ by value");
    assert_ne!(digest_token("session", "x"), digest_token("changeset", "x"));
}

/// Issue #834: one catalog is its name, two are `multiple`, none is `none`.
#[tokio::test]
async fn note_catalog_tracks_one_name_then_multiple() {
    let seen = Arc::new(Mutex::new(CatalogSeen::default()));
    assert_eq!(seen.lock().expect("lock").label(), "none");
    TOOL_CATALOG
        .scope(Arc::clone(&seen), async {
            note_catalog("pg");
            note_catalog("pg");
            assert_eq!(seen.lock().expect("lock").label(), "pg");
            note_catalog("other");
        })
        .await;
    assert_eq!(seen.lock().expect("lock").label(), "multiple");
    // Outside a call it does nothing.
    note_catalog("ignored");
    assert_eq!(seen.lock().expect("lock").label(), "multiple");
}
