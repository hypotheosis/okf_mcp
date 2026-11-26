//! The HTTP server (design "Process and routes"): `/healthz`, `/mcp`, the
//! archive routes (`archives`), the JSON API under `/api/v1` (`api`) and the
//! web console under `/ui/` (`ui`), the OAuth routes (`crate::oauth`) when
//! OAuth is on, with request tracing, timeouts and the key layer, on
//! `/mcp` the session binding (`sessions`), and console sign-in sessions
//! on `/api/v1` (`console_session`).

pub(crate) mod api;
mod archives;
pub mod console_session;
pub mod limits;
pub mod oidc;
pub(crate) mod pages;
pub mod passkeys;
mod read;
pub mod requester;
pub mod sessions;
pub mod signin;
mod ui;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{MatchedPath, State};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde::Serialize;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::{DefaultOnResponse, OnResponse, TraceLayer};
use tracing::Level;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::app::App;
use crate::auth::require_key;
use crate::clock_check::ClockStatus;
use crate::error::OkfError;
use crate::http::sessions::bind_session;
use crate::mcp::OkfServer;
use crate::oauth;
use crate::telemetry::metrics::RedisOp;
use crate::telemetry::subscriber::split_event;

/// How long a request on any route but `/mcp` may take; for an import, how
/// long reading and checking its upload may take (the import itself has no
/// deadline, so a committed import is never answered with a timeout).
/// `/mcp`'s `GET` is a long-lived event stream, which rmcp's keep-alive and
/// the session idle timeout bound instead.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// The timeout layer for `/healthz` and the export route:
/// [`REQUEST_TIMEOUT`], then 503. Not 408, which says the *client* was slow
/// to send its request (and which some clients and proxies retry on their
/// own).
///
/// # Returns
/// The layer, for `Router::layer`.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
    )
)]
pub fn timeout_layer() -> TimeoutLayer {
    timeout_layer_after(REQUEST_TIMEOUT)
}

/// [`timeout_layer`] with the deadline given (tests pass a short one).
///
/// # Arguments
/// * `timeout` - how long a request may take before the 503.
///
/// # Returns
/// The layer, for `Router::layer`.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
    )
)]
pub fn timeout_layer_after(timeout: Duration) -> TimeoutLayer {
    TimeoutLayer::with_status_code(StatusCode::SERVICE_UNAVAILABLE, timeout)
}

/// How long `/healthz` waits for the database (a pool connection included)
/// before it answers "unreachable": well inside a probe's own timeout, and a
/// bound on how long an anonymous caller can hold a pool slot.
pub const HEALTH_TIMEOUT: Duration = Duration::from_secs(2);

/// An [`OkfError`] as an HTTP response: a status and the error's JSON.
// A newtype (a one-field tuple struct) so this crate can implement axum's
// `IntoResponse` for it: Rust only lets a crate implement a foreign trait
// (`IntoResponse`) for a type it defines itself.
#[derive(Debug)]
pub struct AppError(pub OkfError);

// `From` lets `?` turn an `OkfError` into an `AppError` in handlers.
impl From<OkfError> for AppError {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    fn from(error: OkfError) -> Self {
        Self(error)
    }
}

impl AppError {
    /// The HTTP status of the wrapped error.
    ///
    /// # Returns
    /// 404 for a path, catalog or changeset that is not there (or not reachable), 401
    /// for a request that proves no identity, 403 for an access level that is
    /// not enough, a suspended team's catalog, a catalog that needs an unlock
    /// or the maintenance runner on a catalog with maintenance off, 409 for a
    /// refused request or a publish conflict, 410 for a used or expired link, 422 for a publish
    /// whose result fails lint or a self-service request past a limit, 400 for
    /// a bad input, 500 for the server's own failures, 503 for a request the
    /// database aborted (retry it).
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn status(&self) -> StatusCode {
        // No `_` arm: a new variant fails to compile here until it has a status.
        match &self.0 {
            OkfError::NotFound { .. }
            | OkfError::CatalogNotSelected { .. }
            | OkfError::UnknownChangeset { .. }
            | OkfError::UnknownName { .. } => StatusCode::NOT_FOUND,
            OkfError::Forbidden { .. }
            | OkfError::Suspended { .. }
            | OkfError::UnlockRequired { .. }
            | OkfError::ReauthStale { .. }
            | OkfError::MaintenanceOff { .. } => StatusCode::FORBIDDEN,
            OkfError::Unauthenticated { .. } => StatusCode::UNAUTHORIZED,
            OkfError::Gone { .. } => StatusCode::GONE,
            OkfError::Refused { .. } | OkfError::Conflict { .. } | OkfError::StillLinked { .. } => {
                StatusCode::CONFLICT
            }
            OkfError::LintFailed { .. } | OkfError::LimitExceeded { .. } => {
                StatusCode::UNPROCESSABLE_ENTITY
            }
            OkfError::Busy { .. } | OkfError::StoreUnavailable { .. } => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            OkfError::InvalidPath { .. }
            | OkfError::FrontmatterParse { .. }
            | OkfError::SecretDetected { .. }
            | OkfError::Shape { .. }
            | OkfError::EditMismatch { .. }
            | OkfError::ImportRefused { .. } => StatusCode::BAD_REQUEST,
            OkfError::Io { .. }
            | OkfError::Database { .. }
            | OkfError::Migrate { .. }
            | OkfError::Config { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for AppError {
    #[tracing::instrument(level = "trace", skip_all)]
    fn into_response(self) -> Response {
        // As for tool errors: internal ones (every 500) are logged with their
        // details; the others at debug level, by kind only (a message can
        // name a path).
        if self.0.is_internal() {
            split_event!(
                error,
                { kind = self.0.kind(), },
                { error = %self.0, },
                "request failed"
            );
        } else {
            tracing::debug!(kind = self.0.kind(), "request refused");
        }
        let mut body = self.0.to_json();
        if self.0.is_internal() {
            // Left out when there is no valid trace (issue #1093).
            if let Some(trace_id) = request_trace_id() {
                body["trace_id"] = trace_id.into();
            }
        }
        (self.status(), Json(body)).into_response()
    }
}

/// `/healthz`'s body.
#[derive(Debug, Serialize)]
struct Health {
    /// `"ok"` or `"unreachable"`.
    database: &'static str,
    /// How many catalogs this replica holds in its catalog cache now (no
    /// names: the route needs no key). Catalogs load on first use, so this
    /// is not how many exist.
    cached_catalogs: u64,
    /// The last clock check (design "Time"); `null` without a `time_server`
    /// or before the first check. It never changes the status.
    clock: Option<ClockStatus>,
    /// `"ok"` or `"unavailable"`; `null` without `[redis]`. It never changes
    /// the status, and it never names the Redis URL or host.
    redis: Option<&'static str>,
    /// `"shared"` or `"degraded"`; `null` without `[redis]`. `"degraded"`
    /// means the last shared rate-limit call failed (usually the Redis user's
    /// ACL, issue #832), so each replica counts its own buckets. It never
    /// changes the status.
    rate_limits: Option<&'static str>,
}

/// `GET /healthz`: 200 while the database answers within
/// [`HEALTH_TIMEOUT`], else 503, with the last clock check and the Redis
/// state, neither of which changes the status: a Redis outage must not mark
/// every replica unready.
///
/// # Arguments
/// * `app` - the shared state (axum's `State` extractor hands it over).
///
/// # Returns
/// The status and a [`Health`] body.
#[tracing::instrument(level = "debug", skip_all)]
async fn healthz(State(app): State<Arc<App>>) -> Response {
    let probe = sqlx::query("SELECT 1").execute(app.accounts().executor());
    // `timeout` gives `Err` if the deadline passes first (waiting for a pool
    // connection included), else the query's own `Result`; only `Ok(Ok(_))`
    // means the database answered in time.
    let database = async { matches!(tokio::time::timeout(HEALTH_TIMEOUT, probe).await, Ok(Ok(_))) };
    // Both probes run at once, so the route's deadline is still one
    // `HEALTH_TIMEOUT`, not two.
    let (reachable, redis) = tokio::join!(database, redis_health(&app));
    let health = Health {
        database: if reachable { "ok" } else { "unreachable" },
        cached_catalogs: app.catalog_cache().cached(),
        clock: app.clock_status(),
        redis,
        rate_limits: app.redis().map(|shared| {
            if shared.limits_degraded() {
                "degraded"
            } else {
                "shared"
            }
        }),
    };
    let status = if reachable {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(health)).into_response()
}

/// Probes the shared Redis with `PING` under [`HEALTH_TIMEOUT`].
///
/// # Arguments
/// * `app` - the shared state.
///
/// # Returns
/// `None` without `[redis]`, else `"ok"` or `"unavailable"`. A failed probe is
/// counted through [`crate::shared::Redis::failed`] (its text stays local); a
/// late one is counted without a driver error.
#[tracing::instrument(level = "debug", skip_all)]
async fn redis_health(app: &App) -> Option<&'static str> {
    let redis = app.redis()?;
    let mut connection = redis.connection();
    // The `Cmd` is bound to a name because `query_async` borrows it.
    let command = redis::cmd("PING");
    let ping = command.query_async::<String>(&mut connection);
    match tokio::time::timeout(HEALTH_TIMEOUT, ping).await {
        Ok(Ok(_)) => Some("ok"),
        Ok(Err(error)) => {
            // `failed` counts the error and logs it locally; the returned
            // `Busy` is not needed here.
            let _ = redis.failed(RedisOp::Get, &error);
            Some("unavailable")
        }
        Err(_) => {
            redis.count(RedisOp::Get);
            Some("unavailable")
        }
    }
}

/// `GET /ui/brand.json`'s body: the branding settings, flattened into the
/// top level, and the feedback form.
#[derive(Debug, serde::Serialize)]
struct ConsoleBrand {
    /// `product_name`, `support_contact`, `feedback_url`, `docs_url` and
    /// `billing_url`.
    // `flatten`: the brand's fields sit beside `feedback_form`, not under
    // a `brand` key.
    #[serde(flatten)]
    brand: crate::store::settings::Brand,
    /// The in-console feedback form, `null` without `[feedback]`.
    feedback_form: Option<api::feedback::FeedbackForm>,
}

/// `GET /ui/brand.json`: the product name, support contact, feedback URL,
/// documentation URL and billing URL (`{"product_name": ...,
/// "support_contact": ... or null, "feedback_url": ... or null, "docs_url":
/// ... or null, "billing_url": ... or null}`), plus
/// `feedback_form` (`{"disclosure", "max_message_chars", "kinds"}`, or
/// `null` without a `[feedback]` table), for the console shell before
/// anyone signs in. Public: the sign-in page shows the first two anyway,
/// the URLs are links the operator wants people to open, and the form
/// holds no secret (the Forminit key never leaves the server).
/// `no-cache`, so a changed setting shows on the next load.
///
/// # Arguments
/// * `app` - the shared state.
///
/// # Returns
/// The branding settings as JSON.
#[tracing::instrument(level = "debug", skip_all)]
async fn brand(State(app): State<Arc<App>>) -> Response {
    let body = ConsoleBrand {
        brand: app.settings().brand(),
        feedback_form: api::feedback::form(&app),
    };
    (
        [(axum::http::header::CACHE_CONTROL, "no-cache")],
        Json(body),
    )
        .into_response()
}

/// rmcp's streamable HTTP service over the app: one [`OkfServer`] per
/// session, the configured `Host` names, sessions closed after
/// `session_idle_timeout`, and every session ended when `shutdown` is.
///
/// # Arguments
/// * `app` - the shared state.
/// * `shutdown` - cancelling it ends every session.
///
/// # Returns
/// The service, for `Router::route_service`.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "router builder: runs once at startup, outside any request, so a span would be an orphan root"
    )
)]
fn mcp_service(
    app: &Arc<App>,
    shutdown: &CancellationToken,
) -> StreamableHttpService<OkfServer, LocalSessionManager> {
    let mut sessions = LocalSessionManager::default();
    sessions.session_config.keep_alive = Some(app.config().session_idle_timeout);
    // The same switch for the later POSTs of a session (`tools/list`, ...):
    // rmcp reads it from the session config, not from the HTTP config.
    sessions.session_config.sse_retry = None;
    let mut config = StreamableHttpServerConfig::default()
        .with_allowed_hosts(app.config().allowed_hosts.iter().cloned())
        // No SSE priming event on the `initialize` reply. For protocol
        // versions before 2026-07-28 rmcp streams every POST reply and, when
        // `sse_retry` is set, prepends an empty-data event that strict
        // clients (Google Gemini's connector) reject. It only aids
        // resumption, which `LocalSessionManager` does not support for POST
        // streams anyway.
        .with_sse_retry(None)
        // A child token: cancelling `shutdown` cancels it too, and rmcp
        // cancelling its own copy never reaches `shutdown`.
        .with_cancellation_token(shutdown.child_token());
    // With `[redis]`, rmcp persists each session's `initialize` so another
    // replica can restore it (spec Decision 9). The same value holds the
    // bindings `bind_session` checks; `Arc<RedisSessions>` coerces to the
    // trait object `Arc<dyn SessionStore>` here.
    if let Some(store) = app.sessions().store() {
        config.session_store = Some(store);
    }
    let app = Arc::clone(app);
    // The factory runs once per session; `move` gives the closure its own
    // `Arc` (like a Python closure capturing `app`, but by explicit transfer).
    StreamableHttpService::new(
        move || Ok(OkfServer::new(Arc::clone(&app))),
        Arc::new(sessions),
        config,
    )
}

/// The whole router.
///
/// # Arguments
/// * `app` - the shared state.
/// * `shutdown` - cancelling it ends every MCP session.
///
/// # Returns
/// The router, ready for [`axum::serve()`] (which [`serve`] calls).
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "router builder: runs once at startup, outside any request, so a span would be an orphan root"
    )
)]
pub fn router(app: Arc<App>, shutdown: &CancellationToken) -> Router {
    // Layers wrap what is added before them, so a layer added later runs
    // first on the way in: `require_key` (on `keyed`) runs before
    // `bind_session` (on `mcp`), which can then read the principal.
    let mcp = Router::new()
        .route_service("/mcp", mcp_service(&app, shutdown))
        .route_layer(middleware::from_fn_with_state(
            Arc::clone(&app),
            bind_session,
        ));
    // `route_layer` applies to the routes added before it, merged ones
    // included, so every keyed route goes above it.
    let keyed = Router::new()
        .merge(mcp)
        .merge(archives::routes(&app))
        .route_layer(middleware::from_fn_with_state(
            Arc::clone(&app),
            require_key,
        ));
    let health = Router::new()
        .route("/healthz", get(healthz))
        .layer(timeout_layer());
    // The OAuth routes exist only with OAuth on; without it `/oauth/*` and
    // `/.well-known/*` stay 404, as before M10. They take no bearer token.
    let open = if app.oauth().is_some() {
        Router::new().merge(oauth::routes().layer(timeout_layer()))
    } else {
        Router::new()
    };
    // The passkey routes exist only with passkeys on (a domain
    // `public_url`); otherwise `/auth/passkey/*` stays 404.
    let open = if app.passkeys().is_some() {
        open.merge(passkeys::routes())
    } else {
        open
    };
    // The OIDC routes exist only with providers configured.
    let open = if app.oidc().is_some() {
        open.merge(oidc::routes())
    } else {
        open
    };
    // `/ui/brand.json` reads the settings, so it is routed here, beside
    // `ui::routes` (which has no state), with the console's headers.
    let brand = Router::new().route(
        "/ui/brand.json",
        get(brand).layer(middleware::map_response(ui::security_headers)),
    );
    open.merge(ui::routes())
        .merge(read::routes())
        .merge(brand)
        .merge(signin::routes(&app))
        // The API applies `require_principal` (a bearer credential or a
        // console session cookie) itself, inside its header layer, so even
        // its 401 carries `no-store` and `nosniff`. MCP and the archive
        // routes (`keyed`) take only a bearer credential.
        .nest("/api/v1", api::routes(&app))
        .merge(keyed)
        // Every route but `/healthz` (merged after it, so a probe never
        // waits on the database) starts by checking the settings when they
        // are older than `credential_ttl` (M18 spec Decision 9), so what
        // its handlers read through `App::settings` is never older. The
        // check has its own bound (`SETTINGS_CHECK_TIMEOUT`), since it runs
        // before the routes' timeouts.
        .layer(middleware::from_fn_with_state(
            Arc::clone(&app),
            check_settings,
        ))
        .merge(health)
        // Every route's own writes reach this replica's caches before it
        // answers (read-your-writes on the writing replica, M18 Task 8): the
        // API, the archive routes, the console's sign-in and account forms,
        // the OAuth pages and MCP. In process only; a write made on a task
        // the handler spawned is not collected, and the change feed
        // delivers it a moment later.
        .layer(middleware::from_fn_with_state(
            Arc::clone(&app),
            api::apply_own_writes,
        ))
        // One `info` line per response (status, latency) in a span naming the
        // method and the matched route. The span records no headers, so a
        // bearer key never reaches the log; no query, so neither does an
        // emailed link's token (`/account/setup?token=...`) or an OAuth code;
        // and no literal path, so neither does an id in a path segment (a
        // session's id prefix, a mail job id). `Router::layer` wraps the
        // routes, so it runs after routing and sees `MatchedPath`.
        // The telemetry middleware goes first so it is inside `TraceLayer`
        // (the last `.layer` is the outermost): it runs with the request span
        // current.
        .layer(middleware::from_fn_with_state(
            Arc::clone(&app),
            request_telemetry,
        ))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(request_span)
                .on_response(record_response),
        )
        // Outermost, so the fallback 404, a 405, an auth 401, a redirect, a
        // timeout and a static file all carry it (issue #1115).
        .layer(middleware::map_response(no_index))
        // Hands `app` to every handler that asks for `State<Arc<App>>`.
        .with_state(app)
}

/// The `X-Robots-Tag` value on every response: no page of this server
/// belongs in a search index (issue #1115). A `robots.txt` `Disallow` is not
/// used for this, since a crawler must fetch a page to see it is marked.
pub const ROBOTS_TAG: &str = "noindex, nofollow";

/// Adds `X-Robots-Tag: noindex, nofollow` to a response, whatever its status.
///
/// # Arguments
/// * `response` - the response the router produced.
///
/// # Returns
/// The same response with the header set.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "response mapper around every request: sets one header, no work worth a span"
    )
)]
async fn no_index(mut response: Response) -> Response {
    response.headers_mut().insert(
        HeaderName::from_static("x-robots-tag"),
        HeaderValue::from_static(ROBOTS_TAG),
    );
    response
}

/// Checks the settings before the request goes on ([`App::fresh_settings`]):
/// a cheap read while they are younger than `credential_ttl`, one
/// `admin_generation` read (shared by the requests that arrive together)
/// once they are older.
///
/// # Arguments
/// * `app` - the shared state.
/// * `request` - the request, after routing.
/// * `next` - the rest of the stack.
///
/// # Returns
/// The route's response.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "middleware around every request: a span here would parent every handler's; fresh_settings has its own"
    )
)]
async fn check_settings(
    State(app): State<Arc<App>>,
    request: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    app.fresh_settings().await;
    next.run(request).await
}

/// The request span's OpenTelemetry trace id: what an operator opens to find
/// an internal failure, and what every 500 answer outside the MCP tools
/// quotes (issues #1093 and #1104).
///
/// It reads the request span [`request_telemetry`] scoped, never
/// `Span::current()`: a helper span a handler left current may carry no
/// trace context. A response header can reuse it (issue #1105).
///
/// # Returns
/// 32 lower-case hex digits, or `None` outside a routed request or when the
/// request span has no valid trace context (telemetry off, or the span
/// filtered out: its id would be all zeros, which names nothing).
#[tracing::instrument(level = "trace", skip_all)]
pub(crate) fn request_trace_id() -> Option<String> {
    REQUEST_SPAN
        .try_with(crate::telemetry::propagation::trace_id_of)
        .ok()
        .flatten()
}

tokio::task_local! {
    /// The request span, for [`request_trace_id`], which has no
    /// request to read [`RequestSpan`] from and cannot trust
    /// `Span::current()`: a helper span a handler left current may exist for
    /// one layer only and carry no trace context (issue #1093).
    static REQUEST_SPAN: tracing::Span;
}

/// The request span `TraceLayer` opened, for code deeper in the stack that
/// records the request's root fields (the principal's ids). Recording on a
/// handle, not on `Span::current()`, is what keeps the fields on the root
/// once the sweep's helper spans are current and a raised `traces_filter`
/// exports them.
#[derive(Debug, Clone)]
pub struct RequestSpan(pub tracing::Span);

/// The OpenTelemetry semantic-convention name of an HTTP method: the nine
/// standard ones as they are, anything else `_OTHER`, so a client cannot
/// mint a label or attribute value.
///
/// # Arguments
/// * `method` - the request's method.
///
/// # Returns
/// A name from a closed set.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(uninstrumented_fn, reason = "a pure match over the method")
)]
fn semconv_method(method: &axum::http::Method) -> &'static str {
    use axum::http::Method;
    match *method {
        Method::GET => "GET",
        Method::POST => "POST",
        Method::PUT => "PUT",
        Method::DELETE => "DELETE",
        Method::HEAD => "HEAD",
        Method::OPTIONS => "OPTIONS",
        Method::PATCH => "PATCH",
        Method::TRACE => "TRACE",
        Method::CONNECT => "CONNECT",
        _ => "_OTHER",
    }
}

/// The request span: a root span named `<METHOD> <route>`, with the matched
/// route template, never the literal path (which can carry an id or a
/// token), the query (which can carry a single-use token) or any header. A
/// request no route matched gets no span, so a scanner probing random paths
/// does not create a trace each (it still gets its 404).
///
/// The span is a root (`parent: None`), never the child of whatever span is
/// current; a caller's `traceparent` becomes a span link, never the parent.
/// This function is exempt from `uninstrumented_fn` for the same reason: an
/// `#[instrument]` here would make its own span the parent of every request
/// and export a span even for an unmatched path.
///
/// # Arguments
/// * `request` - the incoming request, after routing.
///
/// # Returns
/// The span, or [`tracing::Span::none`] for an unmatched route.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(uninstrumented_fn, reason = "it opens the request span itself")
)]
fn request_span<B>(request: &axum::http::Request<B>) -> tracing::Span {
    // `MatchedPath` is the template axum's router put in the request's
    // extensions (a typed per-request map) when a route matched.
    let Some(route) = request
        .extensions()
        .get::<MatchedPath>()
        .map(MatchedPath::as_str)
    else {
        return tracing::Span::none();
    };
    let method = semconv_method(request.method());
    // Integer fields are recorded as `i64` later: other integer types export
    // as strings. `Empty` fields are declared now and filled in as known.
    let span = tracing::info_span!(
        parent: None,
        "request",
        otel.name = %format_args!("{method} {route}"),
        otel.kind = "server",
        otel.status_code = tracing::field::Empty,
        http.request.method = method,
        http.route = route,
        http.response.status_code = tracing::field::Empty,
        enduser.id = tracing::field::Empty,
        okf.key.id = tracing::field::Empty,
    );
    if let Some(link) = crate::telemetry::propagation::link_from(request.headers()) {
        // The caller's trace is linked, never adopted (spec Decision 4).
        span.add_link(link);
    }
    span
}

/// `TraceLayer`'s response hook: records the status on the request span
/// (an `i64`; `error` only for a 5xx, never a 4xx) and writes the same
/// `info` line `DefaultOnResponse` did.
///
/// # Arguments
/// * `response` - the response about to be sent.
/// * `latency` - how long the handler took.
/// * `span` - the request span (a disabled one for an unmatched route).
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(uninstrumented_fn, reason = "it records on the request span handle")
)]
fn record_response<B>(response: &Response<B>, latency: Duration, span: &tracing::Span) {
    let status = response.status();
    span.record("http.response.status_code", i64::from(status.as_u16()));
    if status.is_server_error() {
        span.record("otel.status_code", "error");
    }
    // A health probe's completion line is routine noise: it drops to DEBUG
    // (the span and its status stay exported at their own filter).
    let level = if response.extensions().get::<RoutineProbe>().is_some() {
        Level::DEBUG
    } else {
        Level::INFO
    };
    DefaultOnResponse::new()
        .level(level)
        .on_response(response, latency, span);
}

/// The route health probes poll; its completion line is logged at DEBUG.
const HEALTH_ROUTE: &str = "/healthz";

/// The response header that names the request's trace (issue #1105).
pub(crate) const TRACE_ID_HEADER: HeaderName = HeaderName::from_static("x-trace-id");

/// A response extension (a typed per-response map) that marks a response as
/// an embedded static file, so `request_telemetry` leaves `X-Trace-Id` off
/// it: the bytes are the same for every request, and a cache or CDN that
/// kept the header with the body would hand out the first caller's id to
/// everyone after (issue #1105).
#[derive(Debug, Clone, Copy)]
pub(crate) struct StaticFile;

/// A response extension (a typed per-response map) that `request_telemetry`
/// sets on a health probe's response so [`record_response`], which sees the
/// response but not the request, can log its completion line at DEBUG.
#[derive(Debug, Clone, Copy)]
struct RoutineProbe;

/// axum middleware inside `TraceLayer`: stores the request span in the
/// request's extensions ([`RequestSpan`]) for the auth layers, and measures
/// the request (`http.server.request.duration` and
/// `http.server.active_requests`). An unmatched route has no span and is
/// not measured.
///
/// Exempt from `uninstrumented_fn`: its own span would become
/// `Span::current()` and it would capture that instead of the request span.
///
/// # Arguments
/// * `app` - the shared state, for its metrics.
/// * `request` - the request, after routing.
/// * `next` - the rest of the stack.
///
/// # Returns
/// The route's response.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(uninstrumented_fn, reason = "it must capture the request span")
)]
async fn request_telemetry(
    State(app): State<Arc<App>>,
    mut request: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    let Some(route) = request
        .extensions()
        .get::<MatchedPath>()
        .map(|matched| matched.as_str().to_string())
    else {
        return next.run(request).await;
    };
    let method = semconv_method(request.method());
    request
        .extensions_mut()
        .insert(RequestSpan(tracing::Span::current()));
    // The guard decrements on drop, so a client that disconnects (dropping
    // this future) still balances the counter.
    let _in_flight = app.metrics().http_in_flight(method, &route);
    let started = std::time::Instant::now();
    let span = tracing::Span::current();
    // `scope` makes the span reachable from `AppError::into_response`, which
    // runs inside `next`.
    let mut response = REQUEST_SPAN.scope(span.clone(), next.run(request)).await;
    // The same id an internal error's body quotes, so a user can name any
    // request, not only a failed one. Left off when the span has no valid
    // trace context (never zeros) and off embedded static files.
    if response.extensions().get::<StaticFile>().is_none() {
        if let Some(trace_id) = crate::telemetry::propagation::trace_id_of(&span) {
            // 32 hex digits are always a valid header value; `if let Ok`
            // only avoids an `unwrap`.
            if let Ok(value) = HeaderValue::from_str(&trace_id) {
                response.headers_mut().insert(TRACE_ID_HEADER, value);
            }
        }
    }
    app.metrics().http_request(
        method,
        &route,
        response.status().as_u16(),
        started.elapsed(),
    );
    if route == HEALTH_ROUTE {
        response.extensions_mut().insert(RoutineProbe);
    }
    response
}

/// How long a stopping server waits for its background access writes.
const ACCESS_SETTLE_LIMIT: std::time::Duration = std::time::Duration::from_secs(30);

/// What `serve_with` starts beside the HTTP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServeOptions {
    /// Whether to run the background workers (mail and housekeeping) and
    /// the housekeeping ticker. Always
    /// `true` in production: every replica runs them (issue #458). Test
    /// servers that only enqueue set it to `false`.
    pub worker: bool,
}

impl ServeOptions {
    /// A server that starts no worker, for tests that only enqueue mail.
    pub const NO_WORKER: Self = Self { worker: false };
}

impl Default for ServeOptions {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "a constant value, called once at start-up"
        )
    )]
    fn default() -> Self {
        Self { worker: true }
    }
}

/// Serves until `shutdown` is cancelled, with every worker: [`serve_with`]
/// and the default [`ServeOptions`].
///
/// # Arguments
/// * `app` - the shared state.
/// * `listener` - a bound socket.
/// * `shutdown` - cancelled to stop.
///
/// # Errors
/// As [`serve_with`].
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "long-lived server loop: its span would stay open until shutdown and be exported only then"
    )
)]
pub async fn serve(
    app: Arc<App>,
    listener: TcpListener,
    shutdown: CancellationToken,
) -> std::io::Result<()> {
    serve_with(app, listener, shutdown, ServeOptions::default()).await
}

/// Waits for a worker (mail or housekeeping) and enforces the shutdown
/// grace: once `shutdown` is cancelled the worker has `grace` to finish the
/// job in hand; past that it is aborted and an error is logged (another
/// replica takes the job after the orphan timeout). It returns when the
/// worker has finished or been aborted, and runs beside the server's own
/// draining, so a slow request or message never lengthens the grace.
///
/// # Arguments
/// * `worker` - the worker's task.
/// * `shutdown` - the server's stop signal; the grace starts when it is cancelled.
/// * `grace` - how long the worker may take after the signal.
/// * `queue` - the worker's queue (`send_mail`, `housekeeping`), for the
///   log lines.
#[tracing::instrument(level = "debug", skip_all)]
async fn supervise_worker(
    mut worker: tokio::task::JoinHandle<()>,
    shutdown: CancellationToken,
    grace: std::time::Duration,
    queue: &'static str,
) {
    // `&mut worker` polls the handle without consuming it, so it can still
    // be aborted afterwards.
    let finished = tokio::select! {
        result = &mut worker => Some(result),
        () = shutdown.cancelled() => None,
    };
    let result = match finished {
        Some(result) => result,
        None => match tokio::time::timeout(grace, &mut worker).await {
            Ok(result) => result,
            Err(_) => {
                worker.abort();
                tracing::error!(
                    queue,
                    grace_secs = grace.as_secs_f64(),
                    "a worker did not finish within worker_shutdown_grace; \
                     its job in hand is retried by another replica"
                );
                return;
            }
        },
    };
    if let Err(error) = result {
        split_event!(
            error,
            { queue, },
            { %error, },
            "a worker panicked"
        );
    }
}

/// Serves until `shutdown` is cancelled, running the change-feed listener,
/// the announcement subscribers, the clock check and (unless `options.worker` is off) the mail worker, the
/// housekeeping worker and the housekeeping ticker beside it.
///
/// # Arguments
/// * `app` - the shared state.
/// * `listener` - a bound socket.
/// * `shutdown` - cancelled to stop: it ends the MCP sessions, the listener,
///   the subscribers, the clock check, the workers, the ticker and the listener, and `serve`
///   returns once open connections close, each worker has finished the job
///   in hand (at most `worker_shutdown_grace`) and the access counts in
///   flight are written.
/// * `options` - what to start beside the server.
///
/// # Errors
/// The listener's I/O error, after the loops and the sessions are
/// stopped (`shutdown` is cancelled on every return).
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "long-lived server loop: its span would stay open until shutdown and be exported only then"
    )
)]
pub async fn serve_with(
    app: Arc<App>,
    listener: TcpListener,
    shutdown: CancellationToken,
    options: ServeOptions,
) -> std::io::Result<()> {
    let router = router(Arc::clone(&app), &shutdown);
    // Refreshes a catalog's snapshot on every catalog generation
    // announcement (issue #659); ends at once without `[redis]`.
    let catalog_announcements = tokio::spawn(crate::shared::announce::listen_catalogs(
        Arc::clone(&app),
        shutdown.clone(),
    ));
    // Applies the Postgres change feed (milestone 18) to this replica's caches.
    let invalidations = tokio::spawn(crate::invalidate::listen(
        Arc::clone(&app),
        shutdown.clone(),
    ));
    // The same for each separate store's own feed: a listener per opened
    // store (Task 17), stopped with the store's pool or the shutdown.
    app.stores().follow(Arc::downgrade(&app), shutdown.clone());
    let clock_checker = tokio::spawn(Arc::clone(&app).check_clock_until(shutdown.clone()));
    // The workers' one pool, shared by every worker this process runs.
    // `bool::then` runs the closure only for `true`, giving an `Option`.
    let worker_pool = options
        .worker
        .then(|| crate::mail::queue::WorkerPool::new(app.accounts().executor()));
    // Each worker runs under a supervisor that enforces
    // `worker_shutdown_grace` from the moment `shutdown` is cancelled,
    // concurrently with the HTTP drain and the other joins below.
    let grace = app.config().worker_shutdown_grace;
    let supervisors = worker_pool.as_ref().map(|pool| {
        [
            tokio::spawn(supervise_worker(
                crate::mail::queue::spawn_worker(Arc::clone(&app), pool, shutdown.clone()),
                shutdown.clone(),
                grace,
                crate::mail::queue::QUEUE,
            )),
            tokio::spawn(supervise_worker(
                crate::housekeeping::spawn_worker(Arc::clone(&app), pool, shutdown.clone()),
                shutdown.clone(),
                grace,
                crate::housekeeping::QUEUE,
            )),
        ]
    });
    // Schedules each housekeeping slot (once per cluster: the other
    // replicas' pushes of a slot are ignored) and reads the cluster stats
    // into this replica's gauges, once a minute; with the workers only.
    let ticker = options.worker.then(|| {
        tokio::spawn(crate::housekeeping::tick_until(
            Arc::clone(&app),
            shutdown.clone(),
        ))
    });
    // The feedback worker (issue #674): only with `[feedback]` and its
    // `worker` switch on, so one replica forwards.
    let feedback_worker = app
        .config()
        .feedback
        .as_ref()
        .is_some_and(|feedback| feedback.worker)
        .then(|| crate::mail::feedback::spawn_worker(Arc::clone(&app), shutdown.clone()))
        .flatten();
    let stop = shutdown.clone();
    // `async move { ... }` is a future that resolves once the token is
    // cancelled; axum then stops accepting and waits for open connections.
    // `ConnectInfo<SocketAddr>` in every request's extensions: the peer
    // address `console_session::client_ip` falls back to (plan Decision 7).
    let served = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move { stop.cancelled().await })
    .await;
    // On an error too, stop what this function started (the loops and
    // rmcp's sessions) before returning; cancelling twice is harmless.
    shutdown.cancel();
    // The loops return once `shutdown` is cancelled; a panic in one is logged.
    if let Err(error) = catalog_announcements.await {
        split_event!(
            error,
            {},
            { %error, },
            "the catalog announcement subscriber panicked"
        );
    }
    if let Err(error) = invalidations.await {
        split_event!(
            error,
            {},
            { %error, },
            "the change feed listener panicked"
        );
    }
    if let Err(error) = clock_checker.await {
        split_event!(
            error,
            {},
            { %error, },
            "the clock check panicked"
        );
    }
    if let Some(ticker) = ticker {
        if let Err(error) = ticker.await {
            split_event!(
                error,
                {},
                { %error, },
                "the housekeeping ticker panicked"
            );
        }
    }
    // The supervisors started their clocks when `shutdown` was cancelled,
    // so the grace did not wait for the drains above. Each ends once its
    // worker has finished or been aborted; after both, nothing uses the pool.
    // `into_iter().flatten()`: the `Option`'s array, or nothing.
    for supervisor in supervisors.into_iter().flatten() {
        if let Err(error) = supervisor.await {
            split_event!(
                error,
                {},
                { %error, },
                "a worker supervisor panicked"
            );
        }
    }
    if let Some(worker_pool) = &worker_pool {
        worker_pool.close().await;
    }
    if let Some(feedback_worker) = feedback_worker {
        if let Err(error) = feedback_worker.await {
            split_event!(
                error,
                {},
                { %error, },
                "the feedback worker panicked"
            );
        }
    }
    // The access counts still being written (plan Decision 5), bounded so a
    // database that stopped answering cannot hold the shutdown forever.
    if tokio::time::timeout(ACCESS_SETTLE_LIMIT, app.settle_access())
        .await
        .is_err()
    {
        tracing::error!("gave up waiting for the access counts being written");
    }
    served
}

#[cfg(test)]
mod telemetry_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// Task 1b: a request's internal error is exported by kind only.
    #[test]
    fn an_internal_request_error_exports_its_kind_and_not_its_text() {
        use crate::telemetry::config::LogFormat;
        use crate::telemetry::subscriber::capture::observe;

        let error = OkfError::Io {
            path: "/zq-sentinel-7f3a-path.md".to_string(),
            source: std::io::Error::other("disk"),
        };
        let observed = observe("trace", "trace", "trace", LogFormat::Text, || {
            let _ = AppError(error).into_response();
        });
        let exported = observed.exported_text();
        assert!(exported.contains("request failed"), "{exported}");
        assert!(exported.contains("Io"), "{exported}");
        assert!(!exported.contains("zq-sentinel-7f3a"), "{exported}");
        assert!(observed.stderr.contains("zq-sentinel-7f3a-path"));
    }

    /// A log sink for one test: everything the `fmt` subscriber writes.
    // `Arc<Mutex<..>>` so every writer the subscriber makes appends to one
    // buffer the test reads afterwards.
    #[derive(Clone, Default)]
    struct Captured(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("the log buffer")
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    // `MakeWriter` is how `fmt` asks for a writer per line.
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Captured;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    impl Captured {
        /// Everything logged so far, as text.
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().expect("the log buffer")).into_owned()
        }

        /// Forgets everything logged so far.
        fn clear(&self) {
            self.0.lock().expect("the log buffer").clear();
        }
    }

    // A current-thread runtime (the `tokio::test` default), so the
    // thread-local subscriber below sees every span the requests open.
    #[tokio::test]
    async fn the_request_span_logs_the_route_template_never_the_literal_path() {
        use crate::http::api::testing::{app, every_catalog_admin, send, Browser};

        let test = app().await;
        let root = every_catalog_admin(&test).await;
        let (created, _) = send(
            &test,
            "POST",
            "/api/v1/users",
            Some(&root),
            Some(serde_json::json!({"name": "alice"})),
        )
        .await;
        assert_eq!(created.as_u16(), 201);
        let _session = Browser::session_for(&test, "alice").await;
        let (_, listed) = send(
            &test,
            "GET",
            "/api/v1/users/alice/sessions",
            Some(&root),
            None,
        )
        .await;
        let prefix = listed["sessions"][0]["id_prefix"]
            .as_str()
            .expect("a session prefix")
            .to_string();
        let captured = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(captured.clone())
            .with_ansi(false)
            .with_max_level(Level::INFO)
            // Each span's creation is written as a line of its own, so the
            // check does not depend on an event firing inside the span.
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::NEW)
            .finish();
        // `set_default` installs it for this thread until the guard drops.
        let guard = crate::test_tracing::set_default(subscriber);
        // tracing caches whether each log callsite is enabled. Tests on
        // other threads, with no subscriber, can cache "disabled" for the
        // callsites these requests use while this one starts. So run the
        // same two kinds of request once (registering every callsite they
        // touch), recompute every cached answer against this subscriber,
        // and only then capture.
        send(
            &test,
            "DELETE",
            "/api/v1/users/alice/sessions/000000000000",
            Some(&root),
            None,
        )
        .await;
        send(&test, "GET", "/warm-up", None, None).await;
        tracing::callsite::rebuild_interest_cache();
        captured.clear();

        let (revoked, _) = send(
            &test,
            "DELETE",
            &format!("/api/v1/users/alice/sessions/{prefix}"),
            Some(&root),
            None,
        )
        .await;
        let (unmatched, _) = send(&test, "GET", "/no-such-route-marker", None, None).await;
        drop(guard);
        let logged = captured.text();

        assert_eq!(revoked.as_u16(), 204);
        assert_eq!(unmatched.as_u16(), 404);
        assert!(
            logged.contains(r#"http.route="/api/v1/users/{name}/sessions/{id_prefix}""#),
            "the route template is logged: {logged}"
        );
        assert!(
            !logged.contains("<unmatched>"),
            "an unmatched request opens no span: {logged}"
        );
        assert!(
            !logged.contains(&prefix),
            "the session prefix leaked: {logged}"
        );
        assert!(
            !logged.contains("no-such-route-marker"),
            "a client path leaked: {logged}"
        );
    }
    use axum::body::Body;
    use axum::http::Request;
    // `oneshot` sends one request through a router without a socket.
    use tower::ServiceExt;

    use crate::config::Config;
    use crate::error::LinkSource;
    use crate::mcp::testing::{app_on_redis, app_with, app_with_redis};

    /// A response's body as JSON.
    async fn json_body(response: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn suspended_is_403() {
        let suspended = AppError(OkfError::Suspended {
            catalog: "shared".to_string(),
            catalog_label: "shared".to_string(),
            team: "family".to_string(),
            team_label: "family".to_string(),
            notice: None,
        });
        assert_eq!(suspended.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn unlock_required_and_maintenance_off_are_403() {
        let locked = AppError(OkfError::UnlockRequired {
            catalog: "main".to_string(),
            has_user: true,
        });
        let maintenance_off = AppError(OkfError::MaintenanceOff {
            catalog: "main".to_string(),
        });
        assert_eq!(locked.status(), StatusCode::FORBIDDEN);
        assert_eq!(maintenance_off.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn app_errors_have_a_status_and_the_error_json() {
        let missing = AppError(OkfError::CatalogNotSelected {
            catalog: "nope".to_string(),
        });
        let forbidden = AppError::from(OkfError::Forbidden {
            reason: "import needs write".to_string(),
        });
        let refused = AppError(OkfError::Refused {
            reason: "not empty".to_string(),
        });
        let bad = AppError(OkfError::ImportRefused {
            problems: vec!["x".to_string()],
        });
        let io = AppError(OkfError::Io {
            path: "a.md".to_string(),
            source: std::io::Error::other("disk on fire"),
        });
        let internal = AppError(OkfError::Database {
            source: sqlx::Error::PoolClosed,
        });
        let unknown = AppError(OkfError::UnknownChangeset {
            changeset: "made-up".to_string(),
        });

        let errors = [
            &missing, &forbidden, &refused, &bad, &io, &internal, &unknown,
        ];
        let statuses = errors.map(AppError::status);
        // A 500 is exactly an internal error: logged in full, redacted here.
        let internal_flags = errors.map(|error| error.0.is_internal());
        let response = internal.into_response();

        assert_eq!(
            statuses,
            [
                StatusCode::NOT_FOUND,
                StatusCode::FORBIDDEN,
                StatusCode::CONFLICT,
                StatusCode::BAD_REQUEST,
                StatusCode::INTERNAL_SERVER_ERROR,
                StatusCode::INTERNAL_SERVER_ERROR,
                StatusCode::NOT_FOUND,
            ]
        );
        assert_eq!(
            internal_flags,
            [false, false, false, false, true, true, false]
        );
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        // The driver's message stays in the log.
        assert_eq!(
            json_body(response).await,
            serde_json::json!({
                "error": "Database",
                "message": "internal error; the server log has the details",
            })
        );
    }

    #[test]
    fn busy_is_503_and_says_to_retry() {
        let busy = AppError(OkfError::Busy {
            reason: "deadlock".to_string(),
        });

        let status = busy.status();
        let body = busy.0.to_json();

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"], "Busy");
        assert!(body["message"].as_str().unwrap().ends_with("retry"));
    }

    #[test]
    fn store_unavailable_is_503_and_hides_the_store_name() {
        let store = crate::stores::StoreName::parse("shared-1").unwrap();
        let error = AppError(OkfError::store_unavailable(&store));

        let status = error.status();
        let body = error.0.to_json();

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"], "StoreUnavailable");
        assert!(!error.0.is_internal());
        assert!(!body.to_string().contains("shared-1"));
        assert_eq!(
            body["message"],
            "the catalog's data is temporarily unavailable; retry later"
        );
        // The Display text (what a tool error shows) hides it too.
        assert!(!error.0.to_string().contains("shared-1"));
    }

    #[test]
    fn an_unknown_user_or_team_is_a_404() {
        let error = AppError(OkfError::UnknownName {
            noun: "user",
            name: "alice".into(),
        });

        let status = error.status();
        let body = error.0.to_json();

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(error.0.to_string(), "user \"alice\" does not exist");
        assert_eq!(
            body,
            serde_json::json!({
                "error": "UnknownName",
                "message": "user \"alice\" does not exist",
                "noun": "user",
                "name": "alice",
            })
        );
    }

    #[test]
    fn publish_refusals_have_their_statuses() {
        let conflict = AppError(OkfError::Conflict { files: Vec::new() });
        let lint = AppError(OkfError::LintFailed {
            errors: Vec::new(),
            warnings: Vec::new(),
        });

        assert_eq!(conflict.status(), StatusCode::CONFLICT);
        assert_eq!(lint.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[test]
    fn a_delete_refused_while_linked_is_a_conflict() {
        let still_linked = AppError(OkfError::StillLinked {
            path: "/x.md".to_string(),
            links: vec![LinkSource {
                source: "/hub.md".to_string(),
                line: 5,
            }],
        });

        assert_eq!(still_linked.status(), StatusCode::CONFLICT);
    }

    #[test]
    fn an_edit_mismatch_is_a_bad_request() {
        let mismatch = AppError(OkfError::EditMismatch {
            path: "a.md".to_string(),
            matches: 0,
        });

        let status = mismatch.status();

        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_refused_secret_is_a_bad_request_with_its_rule_and_line() {
        let refused = AppError(OkfError::SecretDetected {
            path: "notes.md".to_string(),
            line: 6,
            rule: "secret:assignment".to_string(),
            message: "possible secret: 'password' followed by a literal value Ab1A...".to_string(),
        });

        let status = refused.status();
        let body = json_body(refused.into_response()).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "SecretDetected");
        assert_eq!(
            (&body["line"], &body["rule"]),
            (&6.into(), &"secret:assignment".into())
        );
    }

    // `start_paused`: tokio's clock jumps ahead whenever every task is
    // waiting on a timer, so the 120-second timeout passes at once.
    #[tokio::test(start_paused = true)]
    async fn a_request_past_the_timeout_gets_503() {
        let slow = Router::new()
            .route(
                "/slow",
                get(|| async {
                    tokio::time::sleep(REQUEST_TIMEOUT * 2).await;
                    "too late"
                }),
            )
            .layer(timeout_layer());

        let request = Request::get("/slow").body(Body::empty()).expect("request");
        let response = slow.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    /// Every request checks settings older than `credential_ttl` before its
    /// handler reads them (M18 spec Decision 9): with no listener and no
    /// poll, a setting written through the store reaches an anonymous page
    /// once the loaded settings are older than the TTL.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_request_checks_settings_older_than_the_credential_ttl() {
        use crate::store::audit::AuditActor;
        use crate::store::settings::{self, SettingKey};
        let test = crate::mcp::testing::app_with(
            &[],
            Config {
                credential_ttl: Duration::from_millis(100),
                ..Config::default()
            },
        )
        .await;
        let product_name = || async {
            let response = router(Arc::clone(&test.app), &CancellationToken::new())
                .oneshot(Request::get("/ui/brand.json").body(Body::empty()).unwrap())
                .await
                .unwrap();
            json_body(response).await["product_name"].clone()
        };
        let before = product_name().await;

        settings::set(
            test.app.accounts().executor(),
            SettingKey::ProductName,
            "Acme Notes",
            &AuditActor::cli(),
            chrono::Utc::now(),
        )
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let after = product_name().await;

        assert_eq!(before, "okf");
        assert_eq!(after, "Acme Notes");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn brand_json_is_public_and_follows_the_settings() {
        use crate::store::audit::AuditActor;
        use crate::store::settings::{self, SettingKey};
        let test = crate::http::api::testing::app().await;
        let get_brand = || async {
            let response = router(Arc::clone(&test.app), &CancellationToken::new())
                .oneshot(Request::get("/ui/brand.json").body(Body::empty()).unwrap())
                .await
                .unwrap();
            let headers = response.headers().clone();
            (response.status(), headers, json_body(response).await)
        };
        let (status, headers, before) = get_brand().await;

        settings::set(
            test.app.accounts().executor(),
            SettingKey::ProductName,
            "Acme Notes",
            &AuditActor::cli(),
            chrono::Utc::now(),
        )
        .await
        .unwrap();
        settings::set(
            test.app.accounts().executor(),
            SettingKey::SupportContact,
            "help@example.com",
            &AuditActor::cli(),
            chrono::Utc::now(),
        )
        .await
        .unwrap();
        crate::invalidate::testing::applied(&test.app, async {
            settings::set(
                test.app.accounts().executor(),
                SettingKey::FeedbackUrl,
                "https://example.com/feedback",
                &AuditActor::cli(),
                chrono::Utc::now(),
            )
            .await
            .unwrap();
            settings::set(
                test.app.accounts().executor(),
                SettingKey::DocsUrl,
                "https://docs.example.com",
                &AuditActor::cli(),
                chrono::Utc::now(),
            )
            .await
            .unwrap();
            settings::set(
                test.app.accounts().executor(),
                SettingKey::BillingUrl,
                "https://billing.example.com/login",
                &AuditActor::cli(),
                chrono::Utc::now(),
            )
            .await
            .unwrap();
            // Not in the brand: only the setup redirect reads it.
            settings::set(
                test.app.accounts().executor(),
                SettingKey::OnboardingUrl,
                "https://docs.example.com/start/",
                &AuditActor::cli(),
                chrono::Utc::now(),
            )
            .await
            .unwrap();
        })
        .await;
        let (_, _, after) = get_brand().await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["cache-control"], "no-cache");
        assert_eq!(
            headers["content-security-policy"],
            ui::CONTENT_SECURITY_POLICY
        );
        assert_eq!(
            before,
            serde_json::json!({
                "product_name": "okf",
                "support_contact": null,
                "feedback_url": null,
                "docs_url": null,
                "billing_url": null,
                "feedback_form": null
            })
        );
        assert_eq!(
            after,
            serde_json::json!({
                "product_name": "Acme Notes",
                "support_contact": "help@example.com",
                "feedback_url": "https://example.com/feedback",
                "docs_url": "https://docs.example.com",
                "billing_url": "https://billing.example.com/login",
                "feedback_form": null
            })
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn brand_json_offers_the_feedback_form_only_with_feedback_configured() {
        use crate::config::{AuthConfig, FeedbackConfig};
        let config = Config {
            auth: AuthConfig {
                product_name: "Acme Notes".to_string(),
                ..AuthConfig::default()
            },
            feedback: Some(FeedbackConfig::for_form("form-placeholder_1")),
            ..Config::default()
        };
        let test = app_with(&[("pg", &[])], config).await;

        let response = router(Arc::clone(&test.app), &CancellationToken::new())
            .oneshot(Request::get("/ui/brand.json").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = json_body(response).await;

        assert_eq!(
            body["feedback_form"],
            serde_json::json!({
                "disclosure": "The feedback goes to Acme Notes's team through a third-party \
                               form service (Forminit). What is sent: your message, the kind \
                               and rating you choose, your user name, the server version and \
                               the feedback's id. Leave out anything you don't want to share.",
                "max_message_chars": 4000,
                "kinds": ["bug", "confusing", "missing", "praise", "other"]
            })
        );
        // Nothing about the form's delivery: no form id, no endpoint, no
        // key's variable name.
        let text = body.to_string();
        for secret in ["form-placeholder_1", "forminit.com", "FORMINIT"] {
            assert!(!text.contains(secret), "{text}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn healthz_gives_up_after_its_own_deadline() {
        let test = app_with(&[("pg", &[])], Config::default()).await;
        let router = router(Arc::clone(&test.app), &CancellationToken::new());
        // Hold every pool connection, so the probe waits for one (sqlx's own
        // acquire timeout is 30 s).
        let size = test
            .app
            .accounts()
            .executor()
            .options()
            .get_max_connections();
        let mut held = Vec::new();
        for _ in 0..size {
            held.push(test.app.accounts().executor().acquire().await.unwrap());
        }

        let started = std::time::Instant::now();
        let request = Request::get("/healthz")
            .body(Body::empty())
            .expect("request");
        let response = router.oneshot(request).await.unwrap();
        let waited = started.elapsed();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            waited >= HEALTH_TIMEOUT,
            "answered before the deadline: {waited:?}"
        );
        assert!(
            waited < HEALTH_TIMEOUT * 5,
            "waited past the deadline: {waited:?}"
        );
        drop(held);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn healthz_reports_an_unreachable_database_as_503() {
        let test = app_with(&[("pg", &[])], Config::default()).await;
        let router = router(Arc::clone(&test.app), &CancellationToken::new());
        test.app.accounts().executor().close().await;

        let request = Request::get("/healthz")
            .body(Body::empty())
            .expect("request");
        let response = router.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            json_body(response).await,
            serde_json::json!({"database": "unreachable", "cached_catalogs": 0, "clock": null, "redis": null, "rate_limits": null})
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn healthz_without_redis_reports_null_and_keeps_its_status() {
        let test = app_with(&[("pg", &[])], Config::default()).await;
        let router = router(Arc::clone(&test.app), &CancellationToken::new());
        let request = Request::get("/healthz")
            .body(Body::empty())
            .expect("request");
        let response = router.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        // The key is present and null (lessons.md: check the key itself).
        assert_eq!(
            json_body(response).await.get("redis"),
            Some(&serde_json::Value::Null)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn healthz_reports_redis_without_changing_its_status() {
        let redis = crate::db::redis_support::TestRedis::start().await;
        let (test, redis) = app_on_redis(Config::default(), redis.dedicated().await).await;
        healthz_follows_redis(test, redis).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn healthz_reports_a_degraded_rate_limiter_without_changing_its_status() {
        let (test, _redis) = app_with_redis(Config::default()).await;
        let router = router(Arc::clone(&test.app), &CancellationToken::new());
        let get = || {
            Request::get("/healthz")
                .body(Body::empty())
                .expect("request")
        };
        let shared = json_body(router.clone().oneshot(get()).await.unwrap()).await;
        assert_eq!(shared["rate_limits"], "shared");

        test.app.redis().expect("redis").set_limits_degraded(true);
        let response = router.oneshot(get()).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let degraded = json_body(response).await;
        assert_eq!(degraded["rate_limits"], "degraded");
        assert_eq!(degraded["redis"], "ok", "Redis itself answers");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn healthz_reports_a_redis_cluster_without_changing_its_status() {
        let cluster = crate::db::redis_support::TestRedis::start_cluster().await;
        let (test, redis) = app_on_redis(Config::default(), cluster.dedicated().await).await;
        healthz_follows_redis(test, redis).await;
    }

    /// `/healthz` says `"ok"` while Redis is up and `"unavailable"` (still
    /// 200) once it is stopped.
    async fn healthz_follows_redis(
        test: crate::mcp::testing::TestApp,
        redis: crate::db::redis_support::TestRedis,
    ) {
        let router = router(Arc::clone(&test.app), &CancellationToken::new());
        let get = || {
            Request::get("/healthz")
                .body(Body::empty())
                .expect("request")
        };
        let wait = Duration::from_secs(30);
        let up = tokio::time::timeout(wait, router.clone().oneshot(get()))
            .await
            .expect("healthz answers while Redis is up")
            .unwrap();
        let up = json_body(up).await;
        assert_eq!(up["redis"], "ok");
        // The body never carries the Redis address.
        assert!(!up.to_string().contains("redis://"), "{up}");
        redis.stop().await;
        let response = tokio::time::timeout(wait, router.oneshot(get()))
            .await
            .expect("healthz answers while Redis is down")
            .unwrap();
        // A Redis outage never makes the replica unready: the database decides.
        assert_eq!(response.status(), StatusCode::OK);
        let down = json_body(response).await;
        assert_eq!(down["redis"], "unavailable");
        assert!(!down.to_string().contains("redis://"), "{down}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn healthz_reports_the_last_clock_check_without_changing_its_status() {
        let test = app_with(&[("pg", &[])], Config::default()).await;
        let get = || {
            Request::get("/healthz")
                .body(Body::empty())
                .expect("request")
        };
        let before = router(Arc::clone(&test.app), &CancellationToken::new())
            .oneshot(get())
            .await
            .unwrap();
        let before = json_body(before).await;
        test.app.set_clock_status(ClockStatus {
            ok: false,
            checked_at: "2026-11-03T12:00:00Z".to_string(),
            offset_ms: None,
            stratum: None,
        });

        let after = router(Arc::clone(&test.app), &CancellationToken::new())
            .oneshot(get())
            .await
            .unwrap();
        let status = after.status();
        let after = json_body(after).await;

        assert_eq!(before["clock"], serde_json::Value::Null);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(after["database"], "ok");
        assert_eq!(
            after["clock"],
            serde_json::json!({"ok": false, "checked_at": "2026-11-03T12:00:00Z"})
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn every_response_says_noindex_whatever_its_status() {
        let test = app_with(
            &[("main", &[])],
            crate::store::oauth::testing::oauth_config("https://okf.example.com"),
        )
        .await;
        let router = router(Arc::clone(&test.app), &CancellationToken::new());
        let cases = [
            ("GET", "/ui/", StatusCode::OK),
            ("GET", "/signin", StatusCode::OK),
            ("GET", "/oauth/authorize", StatusCode::BAD_REQUEST),
            (
                "GET",
                "/.well-known/oauth-protected-resource",
                StatusCode::OK,
            ),
            ("GET", "/mcp", StatusCode::UNAUTHORIZED),
            ("GET", "/api/v1/whoami", StatusCode::UNAUTHORIZED),
            ("GET", "/read", StatusCode::PERMANENT_REDIRECT),
            ("GET", "/no/such/route", StatusCode::NOT_FOUND),
            ("POST", "/healthz", StatusCode::METHOD_NOT_ALLOWED),
        ];

        for (method, uri, status) in cases {
            let request = Request::builder()
                .method(method)
                .uri(uri)
                .header("host", "okf.example.com")
                .body(Body::empty())
                .expect("request");
            let response = router.clone().oneshot(request).await.unwrap();

            assert_eq!(response.status(), status, "{method} {uri}");
            assert_eq!(
                response.headers().get("x-robots-tag").map(|v| v.as_bytes()),
                Some(&b"noindex, nofollow"[..]),
                "{method} {uri}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oauth_routes_exist_only_with_oauth_on() {
        let off = app_with(&[("main", &[])], Config::default()).await;
        let on = app_with(
            &[("main", &[])],
            crate::store::oauth::testing::oauth_config("https://okf.example.com"),
        )
        .await;
        let get = |test: &crate::mcp::testing::TestApp| {
            router(Arc::clone(&test.app), &CancellationToken::new()).oneshot(
                Request::get("/mcp")
                    .header("host", "okf.example.com")
                    .body(Body::empty())
                    .expect("request"),
            )
        };

        let refused_off = get(&off).await.unwrap();
        let refused_on = get(&on).await.unwrap();

        assert_eq!(refused_off.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(refused_off.headers()["www-authenticate"], "Bearer");
        assert_eq!(refused_on.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            refused_on.headers()["www-authenticate"],
            "Bearer resource_metadata=\"https://okf.example.com/.well-known/oauth-protected-resource\""
        );
    }

    #[test]
    fn sign_in_errors_have_their_statuses() {
        let statuses = [
            AppError(OkfError::Unauthenticated {
                reason: "no".into(),
            })
            .status(),
            AppError(OkfError::Gone {
                reason: "expired".into(),
            })
            .status(),
            AppError(OkfError::LimitExceeded {
                limit: "ceiling",
                reason: "above".into(),
            })
            .status(),
        ];

        assert_eq!(
            statuses,
            [
                StatusCode::UNAUTHORIZED,
                StatusCode::GONE,
                StatusCode::UNPROCESSABLE_ENTITY,
            ]
        );
    }

    /// `serve` returns only after the mail worker's task has finished. The
    /// test holds the worker's end (`App::mail_worker_exit_hold`), so the
    /// worker cannot finish until it lets go: `serve` must still be
    /// waiting, and the worker's own flag must be set by the time it
    /// returns (read right after, with no wait for anything to settle).
    #[tokio::test(flavor = "multi_thread")]
    async fn serve_returns_only_after_the_mail_worker_stopped() {
        let db = crate::db::test_db().await;
        let app = App::load(
            db.pool.clone(),
            Config::default(),
            Arc::new(crate::clock::SystemClock),
        )
        .await
        .unwrap();
        let hold = Arc::clone(&app.mail_worker_exit_hold).lock_owned().await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let shutdown = CancellationToken::new();
        let mut served = tokio::spawn(serve(Arc::clone(&app), listener, shutdown.clone()));

        shutdown.cancel();
        // Everything `serve` does besides the held worker takes well under
        // this; only the worker's wait keeps it from returning.
        let early = tokio::time::timeout(std::time::Duration::from_secs(3), &mut served).await;
        // Checked before `served` is awaited again: a finished handle must
        // not be polled twice.
        assert!(
            early.is_err(),
            "serve returned while the mail worker's task was held: {early:?}"
        );
        assert!(
            !app.mail_worker_stopped
                .load(std::sync::atomic::Ordering::SeqCst),
            "the held worker finished"
        );
        drop(hold);
        let returned = tokio::time::timeout(std::time::Duration::from_secs(20), served).await;
        let stopped = app
            .mail_worker_stopped
            .load(std::sync::atomic::Ordering::SeqCst);

        assert!(
            matches!(returned, Ok(Ok(Ok(())))),
            "serve returns once the worker stops: {returned:?}"
        );
        assert!(
            stopped,
            "serve returned before the mail worker's task finished"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn serve_runs_the_mail_worker_on_its_own_connections_and_waits_for_it() {
        let db = crate::db::test_db().await;
        // The smallest request pool the config accepts: one connection. The
        // worker's LISTEN must not take it.
        let one = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_with(db.pool.connect_options().as_ref().clone())
            .await
            .unwrap();
        let config = Config {
            database_max_connections: 1,
            ..Config::default()
        };
        let app = App::load(one, config, Arc::new(crate::clock::SystemClock))
            .await
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let healthz = format!("http://{}/healthz", listener.local_addr().unwrap());
        let shutdown = CancellationToken::new();
        let served = tokio::spawn(serve(Arc::clone(&app), listener, shutdown.clone()));

        crate::mail::queue::enqueue(
            &db.pool,
            "alice@example.com",
            &crate::mail::Template::Test,
            chrono::Utc::now(),
            &crate::store::settings::Brand::default(),
        )
        .await
        .unwrap();
        let mut status = String::new();
        for _ in 0..200 {
            status = sqlx::query_scalar("SELECT status FROM apalis.jobs WHERE job_type = $1")
                .bind(crate::mail::queue::QUEUE)
                .fetch_one(&db.pool)
                .await
                .unwrap();
            if status == "Done" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        // The worker's connections, by name: its LISTEN among them. A single
        // `pg_stat_activity` snapshot raced the worker's connection setup under
        // load (issue #751), so poll until the LISTEN shows, under a timeout.
        let snapshot = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                let (connections, listening): (i64, i64) = sqlx::query_as(
                    "SELECT count(*), count(*) FILTER (WHERE query ILIKE 'LISTEN%') \
                     FROM pg_stat_activity \
                     WHERE datname = current_database() AND application_name = $1",
                )
                .bind(crate::mail::queue::WORKER_APPLICATION_NAME)
                .fetch_one(&db.pool)
                .await
                .unwrap();
                if listening >= 1 {
                    return (connections, listening);
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        })
        .await;
        let health = reqwest::Client::new()
            .get(&healthz)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
            .map(|response| response.status().as_u16());
        shutdown.cancel();
        let stopped = tokio::time::timeout(std::time::Duration::from_secs(20), served).await;

        assert_eq!(status, "Done", "the worker serve started sent it");
        let (worker_connections, listening) = snapshot
            .expect("the worker's LISTEN connection never appeared in pg_stat_activity in 30 s");
        assert_eq!(
            listening, 1,
            "the worker listens ({worker_connections} connections)"
        );
        assert!(
            worker_connections <= i64::from(crate::mail::queue::WORKER_POOL_SIZE),
            "{worker_connections}"
        );
        assert!(
            matches!(health, Ok(200)),
            "requests are served on the one connection: {health:?}"
        );
        assert!(
            matches!(stopped, Ok(Ok(Ok(())))),
            "serve returns once the worker stops: {stopped:?}"
        );
    }

    /// The grace runs from the cancellation, and a worker that ignores it is
    /// aborted while the rest of shutdown (here: nothing awaited) goes on.
    #[tokio::test]
    async fn a_stuck_mail_worker_is_aborted_at_the_grace_deadline() {
        let worker = tokio::spawn(std::future::pending::<()>());
        let watch = worker.abort_handle();
        let shutdown = CancellationToken::new();
        let supervisor = tokio::spawn(supervise_worker(
            worker,
            shutdown.clone(),
            std::time::Duration::from_millis(200),
            "send_mail",
        ));
        // Before the signal the clock has not started, however long it takes.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert!(!watch.is_finished() && !supervisor.is_finished());

        shutdown.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(30), supervisor)
            .await
            .expect("the supervisor ends at the grace deadline")
            .unwrap();

        // `abort` takes effect at the task's next poll; give it a moment.
        for _ in 0..100 {
            if watch.is_finished() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(watch.is_finished(), "the stuck worker was aborted");
    }

    #[tokio::test]
    async fn a_mail_worker_that_finishes_within_the_grace_is_not_aborted() {
        let shutdown = CancellationToken::new();
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&done);
        let stop = shutdown.clone();
        let worker = tokio::spawn(async move {
            stop.cancelled().await;
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        let supervisor = tokio::spawn(supervise_worker(
            worker,
            shutdown.clone(),
            std::time::Duration::from_secs(20),
            "send_mail",
        ));

        shutdown.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(30), supervisor)
            .await
            .expect("the supervisor ends once the worker does")
            .unwrap();

        assert!(
            done.load(std::sync::atomic::Ordering::SeqCst),
            "the mail in hand finished"
        );
    }
}
