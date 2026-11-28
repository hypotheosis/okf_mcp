//! Task 3: the HTTP request span, its principal ids and its metrics. Each
//! span test runs at two span filters (`info` and `trace`) through
//! [`FILTERS`], so Task 10 can re-run them over the swept tree.

use std::future::Future;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use opentelemetry::trace::{SpanId, Status};
use tower::ServiceExt;
use tracing::Instrument;

use super::*;
use crate::config::Config;
use crate::http::api::testing::{app, every_catalog_admin, send, Browser, Payload};
use crate::mcp::testing::{app_with_metrics, render, TestApp};
use crate::telemetry::config::LogFormat;
use crate::telemetry::subscriber::capture::{attribute, Observed, Observer};

/// The two span filters every test here runs at.
const FILTERS: [&str; 2] = ["info", "trace"];

/// A value that must never appear in an exported span.
const SENTINEL: &str = "zq-sentinel-5d1c";

/// Runs `body` under a subscriber with the span filter `filter` and hands
/// back what was exported.
async fn observed<Fut: Future<Output = ()>>(filter: &str, body: Fut) -> Observed {
    let observer = Observer::start("off", filter, "off", LogFormat::Text);
    body.await;
    observer.finish()
}

/// Sends `GET uri` with extra headers through the whole router.
async fn fetch(
    test: &TestApp,
    uri: &str,
    key: Option<&str>,
    headers: &[(&str, &str)],
) -> StatusCode {
    let mut request = Request::get(uri);
    if let Some(key) = key {
        request = request.header("authorization", format!("Bearer {key}"));
    }
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    router(Arc::clone(&test.app), &CancellationToken::new())
        .oneshot(request.body(Body::empty()).expect("request"))
        .await
        .expect("the router answers")
        .status()
}

/// Sends `GET uri` through the whole router and hands back the status, the
/// headers and the body text.
async fn fetch_full(
    test: &TestApp,
    uri: &str,
    key: Option<&str>,
) -> (StatusCode, axum::http::HeaderMap, String) {
    let mut request = Request::get(uri);
    if let Some(key) = key {
        request = request.header("authorization", format!("Bearer {key}"));
    }
    let response = router(Arc::clone(&test.app), &CancellationToken::new())
        .oneshot(request.body(Body::empty()).expect("request"))
        .await
        .expect("the router answers");
    let (parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .expect("the body");
    (
        parts.status,
        parts.headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

/// The text of every exported span, for "never contains" checks.
fn dump(observed: &Observed) -> String {
    format!("{:?}", observed.spans)
}

#[tokio::test]
async fn a_request_span_names_method_and_route() {
    let test = app().await;
    let key = every_catalog_admin(&test).await;
    for filter in FILTERS {
        let observed = observed(filter, async {
            let uri = format!("/api/v1/catalogs/main/health?{SENTINEL}=1");
            let status = fetch(&test, &uri, Some(&key), &[("x-probe", SENTINEL)]).await;
            assert_eq!(status, StatusCode::OK, "{filter}");
            fetch(
                &test,
                &format!("/api/v1/catalogs/{SENTINEL}/health"),
                Some(&key),
                &[],
            )
            .await;
        })
        .await;

        let spans = observed.spans_named("GET /api/v1/catalogs/{catalog}/health");
        assert_eq!(spans.len(), 2, "{filter}: {}", dump(&observed));
        let ok = spans
            .iter()
            .find(|span| attribute(span, "http.response.status_code") == Some(&200.into()))
            .unwrap_or_else(|| panic!("{filter}: a 200 span"));
        assert_eq!(
            attribute(ok, "http.request.method")
                .map(ToString::to_string)
                .as_deref(),
            Some("GET")
        );
        assert_eq!(
            attribute(ok, "http.route")
                .map(ToString::to_string)
                .as_deref(),
            Some("/api/v1/catalogs/{catalog}/health")
        );
        assert_eq!(ok.status, Status::Unset, "{filter}");
        assert!(
            !dump(&observed).contains(SENTINEL),
            "{filter}: {}",
            dump(&observed)
        );
        assert!(
            !dump(&observed).contains(&key),
            "{filter}: the bearer value leaked"
        );
    }
}

#[tokio::test]
async fn a_5xx_is_an_error_and_a_4xx_is_not() {
    let test = app().await;
    let key = every_catalog_admin(&test).await;
    for filter in FILTERS {
        let observed = observed(filter, async {
            let missing = fetch(&test, "/api/v1/catalogs/nope/health", Some(&key), &[]).await;
            assert_eq!(missing, StatusCode::NOT_FOUND);
        })
        .await;
        let span = &observed.spans_named("GET /api/v1/catalogs/{catalog}/health")[0];
        assert_eq!(
            attribute(span, "http.response.status_code"),
            Some(&404.into()),
            "{filter}"
        );
        assert_eq!(
            span.status,
            Status::Unset,
            "{filter}: a 4xx is not an error"
        );
    }
    test.app.accounts().executor().close().await;
    for filter in FILTERS {
        let observed = observed(filter, async {
            let status = fetch(&test, "/healthz", None, &[]).await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        })
        .await;
        let span = &observed.spans_named("GET /healthz")[0];
        assert_eq!(
            attribute(span, "http.response.status_code"),
            Some(&503.into()),
            "{filter}: the status is an i64"
        );
        assert!(matches!(span.status, Status::Error { .. }), "{filter}");
    }
}

/// Issue #1093: an internal API error's `trace_id` is the request span's
/// OpenTelemetry trace id (the one the exported span carries), the detail
/// stays out of the body, and a refusal (4xx) has no `trace_id`.
#[tokio::test]
async fn an_internal_api_error_carries_the_request_spans_trace_id() {
    let test = app().await;
    let key = every_catalog_admin(&test).await;
    crate::http::api::testing::fail_on(
        test.app.accounts().executor(),
        "BEFORE INSERT ON audit_entries FOR EACH ROW",
    )
    .await;
    for filter in FILTERS {
        let mut answers = Vec::new();
        let observed = observed(filter, async {
            answers.push(
                send(
                    &test,
                    "GET",
                    "/api/v1/catalogs/main/export",
                    Some(&key),
                    None,
                )
                .await,
            );
            answers.push(
                send(
                    &test,
                    "GET",
                    "/api/v1/catalogs/nope/health",
                    Some(&key),
                    None,
                )
                .await,
            );
        })
        .await;
        let [(failed, body), (missing, refusal)] = &answers[..] else {
            panic!("two answers");
        };

        assert_eq!(*failed, StatusCode::INTERNAL_SERVER_ERROR, "{filter}");
        let span = &observed.spans_named("GET /api/v1/catalogs/{catalog}/export")[0];
        assert_eq!(
            body["trace_id"],
            span.span_context.trace_id().to_string(),
            "{filter}"
        );
        assert_ne!(body["trace_id"], "0".repeat(32), "{filter}");
        assert!(!body.to_string().contains("injected failure"), "{filter}");
        assert_eq!(*missing, StatusCode::NOT_FOUND, "{filter}");
        assert!(refusal.get("trace_id").is_none(), "{filter}: {refusal}");
    }
    crate::http::api::testing::allow(test.app.accounts().executor(), "audit_entries").await;
}

/// Issue #1093: with no trace exported the id is left out, never zeros.
#[tokio::test]
async fn an_internal_api_error_omits_trace_id_without_a_trace() {
    let test = app().await;
    let key = every_catalog_admin(&test).await;
    crate::http::api::testing::fail_on(
        test.app.accounts().executor(),
        "BEFORE INSERT ON audit_entries FOR EACH ROW",
    )
    .await;

    let (status, body) = send(
        &test,
        "GET",
        "/api/v1/catalogs/main/export",
        Some(&key),
        None,
    )
    .await;

    crate::http::api::testing::allow(test.app.accounts().executor(), "audit_entries").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(body.get("trace_id").is_none(), "{body}");
}

/// Renames `table` away, so every statement on it fails with a database
/// error (a 500 on the OAuth routes); [`restore_table`] puts it back.
async fn break_table(test: &TestApp, table: &str) {
    // `AssertSqlSafe`: `table` is a literal of the calling test.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "ALTER TABLE {table} RENAME TO {table}_away"
    )))
    .execute(test.app.accounts().executor())
    .await
    .expect("rename the table away");
}

/// Undoes [`break_table`].
async fn restore_table(test: &TestApp, table: &str) {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "ALTER TABLE {table}_away RENAME TO {table}"
    )))
    .execute(test.app.accounts().executor())
    .await
    .expect("restore the table");
}

/// A refresh-token request for a token that does not exist: `invalid_grant`
/// (400) while `oauth_tokens` is readable, `server_error` (500) when it is
/// not.
fn refresh_form() -> String {
    crate::oauth::testing::form(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", "okfrt_unknown"),
        ("client_id", crate::oauth::testing::CLIENT_ID),
        ("resource", crate::oauth::testing::RESOURCE),
    ])
}

/// Issue #1104: an OAuth `server_error` (500) names the request span's trace
/// id, keeps its detail out, and a refusal (400) names none.
#[tokio::test]
async fn an_oauth_server_error_carries_the_request_spans_trace_id() {
    let test = crate::oauth::testing::oauth_app().await;
    let token_form = refresh_form();
    for filter in FILTERS {
        let mut answers = Vec::new();
        // The refusal first, while the table is in place.
        let observed = observed(filter, async {
            answers.push(
                crate::oauth::testing::send(
                    &test,
                    "POST",
                    "/oauth/token",
                    Some(token_form.clone()),
                )
                .await,
            );
            break_table(&test, "oauth_tokens").await;
            answers.push(
                crate::oauth::testing::send(
                    &test,
                    "POST",
                    "/oauth/token",
                    Some(token_form.clone()),
                )
                .await,
            );
            restore_table(&test, "oauth_tokens").await;
        })
        .await;
        let [(refused, _, refusal), (failed, _, body)] = &answers[..] else {
            panic!("two answers");
        };
        let refusal: serde_json::Value = serde_json::from_str(refusal).expect("JSON");
        let body: serde_json::Value = serde_json::from_str(body).expect("JSON");

        assert_eq!(*refused, StatusCode::BAD_REQUEST, "{filter}: {refusal}");
        assert!(refusal.get("trace_id").is_none(), "{filter}: {refusal}");
        assert_eq!(
            *failed,
            StatusCode::INTERNAL_SERVER_ERROR,
            "{filter}: {body}"
        );
        assert_eq!(body["error"], "server_error", "{filter}");
        let span = &observed.spans_named("POST /oauth/token")[1];
        assert_eq!(
            body["trace_id"],
            span.span_context.trace_id().to_string(),
            "{filter}"
        );
        assert_ne!(body["trace_id"], "0".repeat(32), "{filter}");
        assert!(
            !body.to_string().contains("oauth_tokens"),
            "{filter}: {body}"
        );
    }
}

/// Issue #1104: the authorize page's 500 shows the trace id, and the 400
/// page for an unknown client shows none.
#[tokio::test]
async fn the_authorize_error_page_shows_the_request_spans_trace_id() {
    let test = crate::oauth::testing::oauth_app().await;
    let unknown = crate::oauth::testing::authorize_query().replace(
        &format!("client_id={}", crate::oauth::testing::CLIENT_ID),
        "client_id=no-such-client",
    );
    let query = crate::oauth::testing::authorize_query();
    for filter in FILTERS {
        let mut answers = Vec::new();
        let observed = observed(filter, async {
            answers.push(
                crate::oauth::testing::send(
                    &test,
                    "GET",
                    &format!("/oauth/authorize?{unknown}"),
                    None,
                )
                .await,
            );
            break_table(&test, "oauth_clients").await;
            answers.push(
                crate::oauth::testing::send(
                    &test,
                    "GET",
                    &format!("/oauth/authorize?{query}"),
                    None,
                )
                .await,
            );
            restore_table(&test, "oauth_clients").await;
        })
        .await;
        let [(refused, _, refusal), (failed, _, page)] = &answers[..] else {
            panic!("two answers");
        };

        assert_eq!(*refused, StatusCode::BAD_REQUEST, "{filter}: {refusal}");
        assert!(!refusal.contains("consent-trace-id"), "{filter}: {refusal}");
        assert_eq!(
            *failed,
            StatusCode::INTERNAL_SERVER_ERROR,
            "{filter}: {page}"
        );
        let span = &observed.spans_named("GET /oauth/authorize")[1];
        assert!(
            page.contains(&format!(
                r#"<code id="consent-trace-id">{}</code>"#,
                span.span_context.trace_id()
            )),
            "{filter}: {page}"
        );
        assert!(!page.contains("oauth_clients"), "{filter}: {page}");
    }
}

/// Issue #1104: with no trace exported, neither OAuth answer names one.
#[tokio::test]
async fn oauth_server_errors_omit_the_trace_id_without_a_trace() {
    let test = crate::oauth::testing::oauth_app().await;
    break_table(&test, "oauth_clients").await;
    break_table(&test, "oauth_tokens").await;
    let query = crate::oauth::testing::authorize_query();

    let (page_status, _, page) =
        crate::oauth::testing::send(&test, "GET", &format!("/oauth/authorize?{query}"), None).await;
    let (token_status, _, body) =
        crate::oauth::testing::send(&test, "POST", "/oauth/token", Some(refresh_form())).await;

    restore_table(&test, "oauth_clients").await;
    restore_table(&test, "oauth_tokens").await;
    assert_eq!(page_status, StatusCode::INTERNAL_SERVER_ERROR, "{page}");
    assert!(!page.contains("consent-trace-id"), "{page}");
    assert_eq!(token_status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert!(!body.contains("trace_id"), "{body}");
}

/// Issue #1104: the bearer-key middleware (MCP and archive routes) answers a
/// failed credential lookup through `AppError`, so it names the trace too.
#[tokio::test]
async fn a_failed_credential_lookup_in_the_auth_middleware_carries_the_trace_id() {
    let test = app().await;
    let key = every_catalog_admin(&test).await;
    // A key the cache has never seen is looked up in the database.
    sqlx::query("ALTER TABLE api_keys RENAME TO api_keys_away")
        .execute(test.app.accounts().executor())
        .await
        .expect("rename the keys table");
    let unknown = format!("{key}x");
    for filter in FILTERS {
        let mut status = None;
        let mut body = serde_json::Value::Null;
        let observed = observed(filter, async {
            let response = router(Arc::clone(&test.app), &CancellationToken::new())
                .oneshot(
                    Request::post("/mcp")
                        .header("authorization", format!("Bearer {unknown}"))
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("the router answers");
            status = Some(response.status());
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
                .await
                .expect("body");
            body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        })
        .await;

        assert_eq!(status, Some(StatusCode::INTERNAL_SERVER_ERROR), "{filter}");
        let span = &observed.spans_named("POST /mcp")[0];
        assert_eq!(
            body["trace_id"],
            span.span_context.trace_id().to_string(),
            "{filter}: {body}"
        );
    }
    sqlx::query("ALTER TABLE api_keys_away RENAME TO api_keys")
        .execute(test.app.accounts().executor())
        .await
        .expect("restore the keys table");
}

#[tokio::test]
async fn an_unmatched_path_makes_no_span() {
    let test = app().await;
    for filter in FILTERS {
        let observed = observed(filter, async {
            let status = fetch(&test, &format!("/no/such/{SENTINEL}"), None, &[]).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
        })
        .await;
        assert!(observed.spans.is_empty(), "{filter}: {}", dump(&observed));
    }
}

#[tokio::test]
async fn a_forged_traceparent_is_a_link_not_the_parent() {
    let test = app().await;
    let forged = "0af7651916cd43dd8448eb211c80319c";
    let header = format!("00-{forged}-b7ad6b7169203331-01");
    for filter in FILTERS {
        let observed = observed(filter, async {
            fetch(&test, "/healthz", None, &[("traceparent", &header)]).await;
        })
        .await;
        let span = &observed.spans_named("GET /healthz")[0];
        assert_ne!(span.span_context.trace_id().to_string(), forged, "{filter}");
        assert_eq!(span.parent_span_id, SpanId::INVALID, "{filter}: a root");
        assert_eq!(span.links.links.len(), 1, "{filter}");
        assert_eq!(
            span.links.links[0].span_context.trace_id().to_string(),
            forged,
            "{filter}"
        );
    }
}

#[tokio::test]
async fn a_request_without_traceparent_has_no_link_at_either_filter() {
    let test = app().await;
    for filter in FILTERS {
        let observed = observed(filter, async {
            // Under an ambient span too: that span must never become a link.
            async {
                fetch(&test, "/healthz", None, &[]).await;
                fetch(&test, "/healthz", None, &[("traceparent", "garbage")]).await;
            }
            .instrument(tracing::info_span!("ambient"))
            .await;
        })
        .await;
        let spans = observed.spans_named("GET /healthz");
        assert_eq!(spans.len(), 2, "{filter}");
        for span in spans {
            assert!(span.links.links.is_empty(), "{filter}: a fabricated link");
        }
    }
}

#[tokio::test]
async fn a_request_span_is_a_root_even_inside_another_span() {
    let test = app().await;
    for filter in FILTERS {
        let observed = observed(filter, async {
            async { fetch(&test, "/healthz", None, &[]).await }
                .instrument(tracing::info_span!("ambient"))
                .await;
        })
        .await;
        let span = &observed.spans_named("GET /healthz")[0];
        assert_eq!(span.parent_span_id, SpanId::INVALID, "{filter}");
        let ambient = &observed.spans_named("ambient")[0];
        assert_ne!(
            span.span_context.trace_id(),
            ambient.span_context.trace_id(),
            "{filter}"
        );
    }
}

#[tokio::test]
async fn an_authenticated_request_records_principal_ids() {
    let test = app().await;
    let key = every_catalog_admin(&test).await;
    let user_id: String = sqlx::query_scalar("SELECT id FROM users WHERE name = $1")
        .bind(crate::db::TEST_USER)
        .fetch_one(test.app.accounts().executor())
        .await
        .expect("the key's user");
    let key_id: String = sqlx::query_scalar("SELECT id FROM api_keys")
        .fetch_one(test.app.accounts().executor())
        .await
        .expect("the one key");
    for filter in FILTERS {
        let observed = observed(filter, async {
            let status = fetch(&test, "/api/v1/catalogs/main/health", Some(&key), &[]).await;
            assert_eq!(status, StatusCode::OK);
        })
        .await;
        let span = &observed.spans_named("GET /api/v1/catalogs/{catalog}/health")[0];
        assert_eq!(
            attribute(span, "enduser.id").map(ToString::to_string),
            Some(user_id.clone()),
            "{filter}"
        );
        assert_eq!(
            attribute(span, "okf.key.id")
                .map(ToString::to_string)
                .as_deref(),
            Some(key_id.as_str()),
            "{filter}"
        );
        assert!(
            !dump(&observed).contains(&key),
            "{filter}: the bearer value leaked"
        );
    }
}

/// `require_key` run under an extra `trace` span, standing in for the
/// sweep's `#[instrument]` on the auth middleware.
async fn spanned_require_key(
    State(app): State<Arc<App>>,
    request: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    crate::auth::require_key(State(app), request, next)
        .instrument(tracing::trace_span!("helper"))
        .await
}

#[tokio::test]
async fn principal_ids_land_on_the_request_span_under_helper_spans() {
    let test = app().await;
    let key = every_catalog_admin(&test).await;
    let key_id: String = sqlx::query_scalar("SELECT id FROM api_keys")
        .fetch_one(test.app.accounts().executor())
        .await
        .expect("the one key");
    // The layers in the production order: the key layer on the route, then
    // the telemetry middleware, then `TraceLayer` outermost.
    let probe = || {
        axum::Router::new()
            .route("/probe", get(|| async { "ok" }))
            .route_layer(middleware::from_fn_with_state(
                Arc::clone(&test.app),
                spanned_require_key,
            ))
            .layer(middleware::from_fn_with_state(
                Arc::clone(&test.app),
                request_telemetry,
            ))
            .layer(
                TraceLayer::new_for_http()
                    .make_span_with(request_span)
                    .on_response(record_response),
            )
            .with_state(Arc::clone(&test.app))
    };
    for filter in FILTERS {
        let observed = observed(filter, async {
            let request = Request::get("/probe")
                .header("authorization", format!("Bearer {key}"))
                .body(Body::empty())
                .expect("request");
            let response = probe().oneshot(request).await.expect("answers");
            assert_eq!(response.status(), StatusCode::OK);
        })
        .await;
        let span = &observed.spans_named("GET /probe")[0];
        assert!(
            attribute(span, "enduser.id").is_some(),
            "{filter}: {}",
            dump(&observed)
        );
        assert_eq!(
            attribute(span, "okf.key.id")
                .map(ToString::to_string)
                .as_deref(),
            Some(key_id.as_str()),
            "{filter}"
        );
        // At `trace` the helper span is exported, as a child.
        assert_eq!(
            observed.spans_named("helper").len(),
            usize::from(filter == "trace"),
            "{filter}"
        );
    }
}

#[tokio::test]
async fn a_console_session_records_the_user_id_and_never_the_user_name() {
    let test = app().await;
    let root = every_catalog_admin(&test).await;
    let name = "zq-sentinel-alice";
    let (created, _) = send(
        &test,
        "POST",
        "/api/v1/users",
        Some(&root),
        Some(serde_json::json!({"name": name})),
    )
    .await;
    assert_eq!(created.as_u16(), 201);
    let user_id: String = sqlx::query_scalar("SELECT id FROM users WHERE name = $1")
        .bind(name)
        .fetch_one(test.app.accounts().executor())
        .await
        .expect("the new user");
    let mut browser = Browser::session_for(&test, name).await;
    for filter in FILTERS {
        let observed = observed(filter, async {
            let reply = browser
                .send(&test, "GET", "/api/v1/session", Payload::Empty)
                .await;
            assert_eq!(reply.status, StatusCode::OK, "{filter}");
        })
        .await;
        let span = &observed.spans_named("GET /api/v1/session")[0];
        assert_eq!(
            attribute(span, "enduser.id").map(ToString::to_string),
            Some(user_id.clone()),
            "{filter}"
        );
        assert_eq!(
            attribute(span, "okf.key.id")
                .map(ToString::to_string)
                .as_deref(),
            Some("session"),
            "{filter}"
        );
        assert!(
            !dump(&observed).contains(name),
            "{filter}: {}",
            dump(&observed)
        );
    }
}

/// Design, request span: a method outside the nine standard ones is
/// `_OTHER` on the span and the metric label, so a client cannot mint a
/// value.
#[tokio::test]
async fn an_unknown_method_is_other_on_the_span_and_the_metric() {
    let (test, registry) = app_with_metrics(&[("main", &[])], Config::default()).await;
    let router = router(Arc::clone(&test.app), &CancellationToken::new());
    for filter in FILTERS {
        let observer = Observer::start("off", filter, "off", LogFormat::Text);
        let request = Request::builder()
            .method(format!("ZQ{}", SENTINEL.len()).as_str())
            .uri("/mcp")
            .body(Body::empty())
            .expect("request");
        // The span ends with the response, so read the status and drop it
        // before the exporter is flushed.
        let status = router
            .clone()
            .oneshot(request)
            .await
            .expect("answers")
            .status();
        let observed = observer.finish();

        // No key: the route exists (a span) and refuses the caller.
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{filter}");
        let spans = observed.spans_named("_OTHER /mcp");
        assert_eq!(spans.len(), 1, "{filter}: {}", dump(&observed));
        assert_eq!(
            attribute(spans[0], "http.request.method")
                .map(ToString::to_string)
                .as_deref(),
            Some("_OTHER"),
            "{filter}"
        );
        assert!(
            !dump(&observed).contains("ZQ"),
            "{filter}: {}",
            dump(&observed)
        );
    }
    let text = render(&registry);
    assert!(
        text.lines().any(|line| {
            line.starts_with("http_server_request_duration_seconds_count{")
                && line.contains(r#"http_request_method="_OTHER""#)
        }),
        "{text}"
    );
    assert!(!text.contains("ZQ"), "{text}");
}

#[tokio::test]
async fn http_requests_are_measured_by_method_route_and_status() {
    let (test, registry) = app_with_metrics(&[("main", &[])], Config::default()).await;
    let router = router(Arc::clone(&test.app), &CancellationToken::new());
    for uri in ["/healthz", "/healthz", &format!("/no/such/{SENTINEL}")] {
        let request = Request::get(uri).body(Body::empty()).expect("request");
        router.clone().oneshot(request).await.expect("answers");
    }

    let text = render(&registry);

    let count = text
        .lines()
        .find(|line| line.starts_with("http_server_request_duration_seconds_count{"))
        .unwrap_or_else(|| panic!("no duration series in {text}"));
    assert!(count.contains(r#"http_route="/healthz""#), "{count}");
    assert!(count.contains(r#"http_request_method="GET""#), "{count}");
    assert!(
        count.contains(r#"http_response_status_code="200""#),
        "{count}"
    );
    assert!(count.ends_with(" 2"), "{count}");
    assert!(
        !text.contains(SENTINEL),
        "an unmatched path is not measured: {text}"
    );
    // Every request finished, so none is in flight.
    assert!(
        text.lines()
            .filter(|line| line.starts_with("http_server_active_requests{"))
            .all(|line| line.ends_with(" 0")),
        "{text}"
    );
}

/// Sends each uri through the router under a console logger at `INFO` and
/// returns the captured console text.
async fn console_for(format: LogFormat, uris: &[&str]) -> String {
    let test = app().await;
    let observer = Observer::start("info", "off", "off", format);
    for uri in uris {
        fetch(&test, uri, None, &[]).await;
    }
    observer.finish().stderr
}

#[tokio::test]
async fn health_probes_write_no_completion_line_to_the_console() {
    for format in [LogFormat::Text, LogFormat::Json] {
        let console = console_for(format, &["/healthz", "/healthz?probe=1", "/healthz"]).await;
        assert!(
            !console.contains("/healthz") && !console.contains("on_response"),
            "{console}"
        );
    }
}

#[tokio::test]
async fn other_routes_keep_their_completion_line() {
    for format in [LogFormat::Text, LogFormat::Json] {
        let console = console_for(
            format,
            &["/healthz", "/api/v1/catalogs/main/health", "/healthz"],
        )
        .await;
        assert!(console.contains("on_response"), "{console}");
        assert_eq!(console.matches("on_response").count(), 1, "{console}");
    }
}

#[tokio::test]
async fn a_failing_health_probe_is_quiet_but_still_exported_as_an_error() {
    let observer = Observer::start("info", "info", "off", LogFormat::Text);
    let test = app().await;
    test.app.accounts().executor().close().await;
    let status = fetch(&test, "/healthz", None, &[]).await;
    let observed = observer.finish();
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        !observed.stderr.contains("on_response"),
        "{}",
        observed.stderr
    );
    let spans = observed.spans_named("GET /healthz");
    assert_eq!(spans.len(), 1);
    assert!(matches!(spans[0].status, Status::Error { .. }));
    assert_eq!(
        attribute(spans[0], "http.response.status_code"),
        Some(&503.into())
    );
}

/// Issue #1105: a 200 carries `X-Trace-Id`, the exported request span's
/// trace id, on the MCP-adjacent API, a console page and a health probe;
/// an embedded static file carries none (a cache would pin one id on it).
#[tokio::test]
async fn a_response_carries_the_request_spans_trace_id_header() {
    let test = app().await;
    let key = every_catalog_admin(&test).await;
    for filter in FILTERS {
        let mut answers = Vec::new();
        let observed = observed(filter, async {
            for (uri, key) in [
                ("/api/v1/catalogs/main/health", Some(key.as_str())),
                ("/healthz", None),
                ("/ui/brand.json", None),
                ("/ui/", None),
                ("/read/sw.js", None),
            ] {
                answers.push((uri, fetch_full(&test, uri, key).await));
            }
        })
        .await;
        for (uri, (status, headers, _)) in &answers {
            assert_eq!(*status, StatusCode::OK, "{filter} {uri}");
            let is_static = *uri == "/ui/" || *uri == "/read/sw.js";
            if is_static {
                assert!(
                    !headers.contains_key("x-trace-id"),
                    "{filter} {uri}: a static file carries no trace id"
                );
                continue;
            }
            let name = match *uri {
                "/api/v1/catalogs/main/health" => "GET /api/v1/catalogs/{catalog}/health".into(),
                other => format!("GET {other}"),
            };
            let spans = observed.spans_named(&name);
            let span = spans
                .first()
                .unwrap_or_else(|| panic!("{filter} {uri}: a span"));
            assert_eq!(
                headers["x-trace-id"].to_str().expect("ascii"),
                span.span_context.trace_id().to_string(),
                "{filter} {uri}"
            );
            assert_ne!(headers["x-trace-id"], "0".repeat(32), "{filter} {uri}");
        }
    }
}

/// Issue #1105: an error answer's header is the `trace_id` in its body.
#[tokio::test]
async fn an_error_response_header_equals_its_body_trace_id() {
    let test = app().await;
    let key = every_catalog_admin(&test).await;
    crate::http::api::testing::fail_on(
        test.app.accounts().executor(),
        "BEFORE INSERT ON audit_entries FOR EACH ROW",
    )
    .await;
    for filter in FILTERS {
        let mut answer = None;
        observed(filter, async {
            answer = Some(fetch_full(&test, "/api/v1/catalogs/main/export", Some(&key)).await);
        })
        .await;
        let (status, headers, text) = answer.expect("an answer");
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{filter}");
        let body: serde_json::Value = serde_json::from_str(&text).expect("json");
        assert_eq!(
            body["trace_id"].as_str(),
            headers["x-trace-id"].to_str().ok(),
            "{filter}"
        );
    }
    crate::http::api::testing::allow(test.app.accounts().executor(), "audit_entries").await;
}

/// Issue #1105: with no exported trace there is no header, never zeros.
#[tokio::test]
async fn a_response_has_no_trace_id_header_without_a_trace() {
    let test = app().await;
    let key = every_catalog_admin(&test).await;
    for uri in [
        "/api/v1/catalogs/main/health",
        "/healthz",
        "/api/v1/catalogs/nope/health",
    ] {
        let (_, headers, _) = fetch_full(&test, uri, Some(&key)).await;
        assert!(!headers.contains_key("x-trace-id"), "{uri}: {headers:?}");
    }
}

// --- Issue #1012: why a bearer credential was refused ---

/// One request through the whole router, with the given headers.
///
/// # Returns
/// The status, the headers as sorted `name: value` lines, and the body.
async fn call(
    test: &TestApp,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, Vec<String>, String) {
    let mut request = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = router(Arc::clone(&test.app), &CancellationToken::new())
        .oneshot(request.body(Body::empty()).expect("request"))
        .await
        .expect("the router answers");
    let (parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .expect("the body");
    let mut lines: Vec<String> = parts
        .headers
        .iter()
        .map(|(name, value)| format!("{name}: {}", String::from_utf8_lossy(value.as_bytes())))
        .collect();
    lines.sort();
    (
        parts.status,
        lines,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

/// The `Authorization` header of each refusal reason, and the credential
/// ids a refused request must never leak. OAuth is on, with one grant.
async fn refusals(
    test: &TestApp,
) -> (Vec<(crate::auth::AuthFailure, Option<String>)>, Vec<String>) {
    use crate::auth::AuthFailure;
    use crate::oauth::testing::{CLIENT_ID, RESOURCE};
    use crate::store::keys::Access;
    use crate::store::oauth::testing::{insert_grant, mint_access};
    use chrono::TimeDelta;

    let pool = test.app.accounts().executor();
    let now = test.app.clock().now();
    let grant = insert_grant(pool, CLIENT_ID, Access::Write, &["main"], now).await;
    let dead_grant = insert_grant(pool, CLIENT_ID, Access::Write, &["main"], now).await;
    let other = mint_access(
        &test.app,
        &grant,
        "https://other.example.com/mcp",
        TimeDelta::hours(1),
    )
    .await;
    let expired = mint_access(&test.app, &grant, RESOURCE, -TimeDelta::minutes(1)).await;
    let revoked = mint_access(&test.app, &grant, RESOURCE, TimeDelta::hours(1)).await;
    let of_dead_grant = mint_access(&test.app, &dead_grant, RESOURCE, TimeDelta::hours(1)).await;
    crate::store::oauth::tokens::revoke(pool, &crate::keys::hash(&revoked), now)
        .await
        .expect("revoke the token");
    test.app
        .oauth()
        .expect("OAuth is on")
        .access
        .forget(&[crate::keys::hash(&revoked)])
        .await;
    crate::invalidate::testing::applied(&test.app, async {
        crate::store::keys::revoke_by_id(
            pool,
            &dead_grant,
            &crate::store::audit::AuditActor::cli(),
            now,
        )
        .await
        .expect("revoke the grant");
    })
    .await;
    let bearer = |token: &str| Some(format!("Bearer {token}"));
    let cases = vec![
        (AuthFailure::Missing, None),
        (AuthFailure::Missing, Some("Bearer ".to_string())),
        (AuthFailure::Scheme, Some(format!("Basic {SENTINEL}"))),
        (AuthFailure::Scheme, Some(SENTINEL.to_string())),
        (AuthFailure::Unknown, bearer(&format!("okfat_{SENTINEL}"))),
        (AuthFailure::Unknown, bearer(&format!("okf_{SENTINEL}"))),
        (AuthFailure::Unknown, bearer(SENTINEL)),
        (AuthFailure::Expired, bearer(&expired)),
        (AuthFailure::Revoked, bearer(&revoked)),
        (AuthFailure::Revoked, bearer(&of_dead_grant)),
        (AuthFailure::WrongResource, bearer(&other)),
    ];
    (cases, vec![grant, dead_grant])
}

/// Issue #1012: every 401 from `require_key` and `require_principal` names
/// its reason on the request span, under the default `info` filter and
/// under `trace`, and `DELETE /mcp` with no header is `missing`.
#[tokio::test]
async fn a_401_records_why_the_credential_was_refused() {
    let test = crate::oauth::testing::oauth_app().await;
    let (cases, _) = refusals(&test).await;
    let routes = [
        ("DELETE", "/mcp", "DELETE /mcp"),
        ("GET", "/api/v1/whoami", "GET /api/v1/whoami"),
    ];
    for filter in FILTERS {
        for (failure, authorization) in &cases {
            for (method, uri, name) in routes {
                let headers: Vec<(&str, &str)> = authorization
                    .iter()
                    .map(|value| ("authorization", value.as_str()))
                    .collect();
                let observed = observed(filter, async {
                    let (status, _, _) = call(&test, method, uri, &headers).await;
                    assert_eq!(status, StatusCode::UNAUTHORIZED, "{filter} {name}");
                })
                .await;
                let spans = observed.spans_named(name);
                assert_eq!(spans.len(), 1, "{filter} {name}: {}", dump(&observed));
                assert_eq!(
                    attribute(spans[0], "okf.refusal")
                        .map(ToString::to_string)
                        .as_deref(),
                    Some(failure.as_str()),
                    "{filter} {name} {authorization:?}"
                );
            }
        }
    }
}

/// Issue #1012: a request that is admitted records no failure.
#[tokio::test]
async fn an_admitted_request_records_no_auth_failure() {
    let test = app().await;
    let key = every_catalog_admin(&test).await;
    for filter in FILTERS {
        let observed = observed(filter, async {
            let status = fetch(&test, "/api/v1/whoami", Some(&key), &[]).await;
            assert_eq!(status, StatusCode::OK, "{filter}");
        })
        .await;
        let span = &observed.spans_named("GET /api/v1/whoami")[0];
        assert!(attribute(span, "okf.refusal").is_none(), "{filter}");
    }
}

/// Issue #1012: on the API, a console session cookie that matches no live
/// session is `unknown`, and the cookie's value is never exported.
#[tokio::test]
async fn a_dead_session_cookie_on_the_api_is_unknown() {
    let test = app().await;
    let cookie = format!("{}={SENTINEL}", crate::http::console_session::COOKIE);
    for filter in FILTERS {
        let observed = observed(filter, async {
            let (status, _, _) = call(
                &test,
                "GET",
                "/api/v1/session",
                &[("cookie", cookie.as_str())],
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{filter}");
        })
        .await;
        let span = &observed.spans_named("GET /api/v1/session")[0];
        assert_eq!(
            attribute(span, "okf.refusal")
                .map(ToString::to_string)
                .as_deref(),
            Some("unknown"),
            "{filter}"
        );
        assert!(!dump(&observed).contains(SENTINEL), "{filter}");
    }
}

/// Issue #1012: telling the reasons apart changes nothing the client sees:
/// the six reasons answer one status, one set of headers (the
/// `WWW-Authenticate` challenge, with no `error` or description) and one
/// body, byte for byte.
#[tokio::test]
async fn every_refusal_answers_with_the_same_bytes() {
    let test = crate::oauth::testing::oauth_app().await;
    let (cases, _) = refusals(&test).await;
    for (method, uri) in [("DELETE", "/mcp"), ("GET", "/api/v1/whoami")] {
        let mut answers = Vec::new();
        for (_, authorization) in &cases {
            let headers: Vec<(&str, &str)> = authorization
                .iter()
                .map(|value| ("authorization", value.as_str()))
                .collect();
            answers.push(call(&test, method, uri, &headers).await);
        }
        let (status, headers, body) = answers.first().expect("an answer");
        assert_eq!(*status, StatusCode::UNAUTHORIZED);
        assert!(
            headers
                .iter()
                .any(|line| line.starts_with("www-authenticate: Bearer resource_metadata=")),
            "{headers:?}"
        );
        assert!(
            headers.iter().all(|line| !line.contains("invalid_token")),
            "{headers:?}"
        );
        for answer in &answers {
            assert_eq!(&answer.0, status, "{method} {uri}");
            assert_eq!(&answer.1, headers, "{method} {uri}");
            assert_eq!(&answer.2, body, "{method} {uri}");
        }
    }
}

/// Issue #1012: the credential never reaches a span, a log record, stderr
/// or the body: not the marker a bad token carries, not a live-looking
/// token's value, its hash or its grant's id.
#[tokio::test]
async fn a_refused_credential_never_reaches_a_span_a_log_or_a_body() {
    let test = crate::oauth::testing::oauth_app().await;
    let (cases, grants) = refusals(&test).await;
    for filter in FILTERS {
        for (_, authorization) in &cases {
            let headers: Vec<(&str, &str)> = authorization
                .iter()
                .map(|value| ("authorization", value.as_str()))
                .collect();
            let observer = Observer::start("trace", filter, "trace", LogFormat::Text);
            let (_, response_headers, body) = call(&test, "DELETE", "/mcp", &headers).await;
            let observed = observer.finish();
            let seen = format!("{}|{}|{}", observed.exported_text(), observed.stderr, body);
            let token = authorization
                .as_deref()
                .and_then(|value| value.split_once(' '))
                .map_or("", |(_, token)| token.trim());
            let mut hidden = vec![SENTINEL.to_string()];
            hidden.extend(grants.iter().cloned());
            if !token.is_empty() {
                hidden.push(token.to_string());
                hidden.push(
                    crate::keys::hash(token)
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect(),
                );
            }
            for value in hidden {
                assert!(!seen.contains(&value), "{filter}: {value} leaked");
                assert!(
                    response_headers.iter().all(|line| !line.contains(&value)),
                    "{filter}: {value} in a header"
                );
            }
        }
    }
}

/// Issue #1012: the `okf.refusal` values (once `okf.auth.failure`) hold nothing a trace backend's
/// scrubber hides.
#[test]
fn the_auth_failure_values_hold_nothing_a_scrubber_hides() {
    use crate::auth::AuthFailure;
    for failure in [
        AuthFailure::Missing,
        AuthFailure::Scheme,
        AuthFailure::Unknown,
        AuthFailure::Expired,
        AuthFailure::Revoked,
        AuthFailure::WrongResource,
    ] {
        for hidden in [
            "auth", "token", "secret", "password", "key", "cookie", "session",
        ] {
            assert!(!failure.as_str().contains(hidden), "{failure:?}");
        }
    }
}

/// Substrings a trace backend's default scrubber hides, in a key or a value.
///
/// The first twelve are Sentry's documented default list
/// (<https://docs.sentry.io/security-legal-pii/scrubbing/server-side-scrubbing/>);
/// the last three are a conservative project extra. Issue #884 (77e3614b)
/// renamed `okf.oauth.grant` to `okf.grant` for the same reason, and #1205
/// renamed `okf.auth.failure` to `okf.refusal`.
const SCRUBBED_SUBSTRINGS: [&str; 15] = [
    "password",
    "secret",
    "passwd",
    "api_key",
    "apikey",
    "auth",
    "credentials",
    "mysql_pwd",
    "privatekey",
    "private_key",
    "token",
    "bearer",
    "cookie",
    "session",
    "csrf",
];

/// Collects every `okf.*` span attribute key declared or recorded in `src`.
///
/// A key is declared unquoted before an `=` inside `fields(...)` or
/// `info_span!(...)` (possibly on its own line), or recorded as a quoted
/// literal by `record("okf....", ..)`. Span names and metric names are quoted
/// and never followed by `=`, so they are not collected. Code after the first
/// `#[cfg(test)]` and files named `*tests.rs` are skipped.
fn okf_span_keys() -> std::collections::BTreeSet<String> {
    let declared = regex::Regex::new(r#"(?:^|[^"A-Za-z0-9_.])(okf\.[a-z0-9_.]+)\s*=[^=]"#)
        .expect("a valid pattern");
    let recorded =
        regex::Regex::new(r#"record\(\s*"(okf\.[a-z0-9_.]+)""#).expect("a valid pattern");
    let mut keys = std::collections::BTreeSet::new();
    let mut pending = vec![std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")];
    while let Some(path) = pending.pop() {
        if path.is_dir() {
            pending.extend(
                std::fs::read_dir(&path)
                    .expect("src is readable")
                    .map(|entry| entry.expect("an entry").path()),
            );
            continue;
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if !name.ends_with(".rs") || name.ends_with("tests.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("a source file");
        let code = text.split("#[cfg(test)]").next().unwrap_or("");
        for pattern in [&declared, &recorded] {
            keys.extend(pattern.captures_iter(code).map(|found| found[1].to_owned()));
        }
    }
    keys
}

/// Issue #1205: no `okf.*` span attribute key holds a substring a trace
/// backend's scrubber hides (a key containing `auth` shows as `[Filtered]`).
#[test]
fn no_okf_span_key_holds_a_substring_a_scrubber_hides() {
    let keys = okf_span_keys();
    for expected in [
        "okf.key.id",
        "okf.grant",
        "okf.refusal",
        "okf.refresh",
        "okf.error",
    ] {
        assert!(
            keys.contains(expected),
            "the scan missed {expected}: {keys:?}"
        );
    }
    for key in &keys {
        for hidden in SCRUBBED_SUBSTRINGS {
            assert!(
                !key.contains(hidden),
                "{key} holds {hidden}, which a trace backend scrubs"
            );
        }
    }
}
