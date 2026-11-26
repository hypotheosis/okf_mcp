//! The MCP server (design "Tools"): the rmcp handler and the tools,
//! one module per group. Each `#[tool]` method builds a
//! [`context::ToolContext`] from the HTTP request rmcp hands it and calls a
//! plain function, which the module's tests call directly.

pub(crate) mod catalogs;
pub(crate) mod cited_by;
// The tool classification the annotations are checked against (tests), and
// the closed list of tool names the `tool` metric label draws from.
pub(crate) mod annotations;
pub(crate) mod changesets;
pub(crate) mod documents;
pub(crate) mod feedback;
// `pub` (not `pub(crate)`): the context's helpers, like `ToolResult`,
// `tool_error` and `reply` below, are used by later tool modules, and a
// public item is never flagged as dead code meanwhile.
pub mod context;
pub(crate) mod history;
#[cfg(test)]
mod history_people_tests;
#[cfg(test)]
mod labels_tests;
pub(crate) mod maintenance;
#[cfg(test)]
mod overlay_stores_tests;
#[cfg(test)]
mod reference;
pub(crate) mod reports;
mod restructure;
pub(crate) mod search;
#[cfg(test)]
pub(crate) mod testing;
#[cfg(test)]
mod unavailable_tests;
pub(crate) mod whoami;
pub(crate) mod writes;

use std::sync::Arc;

use axum::http::request::Parts;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, Implementation, ServerCapabilities,
    ServerConfig,
};
use rmcp::{tool_handler, Json, ServerHandler};
use tracing::Instrument;

use crate::app::App;
use crate::error::OkfError;
use crate::mcp::context::ToolContext;
use crate::telemetry::metrics::{tool_label, Outcome, SessionGuard};
use crate::telemetry::subscriber::split_event;

// `task_local!` is a value scoped to one async task, like a
// `contextvars.ContextVar` in Python.
tokio::task_local! {
    /// The span of the tool call in progress (set by `call_tool`): how
    /// [`tool_error`] records the error kind on the tool's root span rather
    /// than on `Span::current()`, which under a raised `traces_filter` is the
    /// tool method's own `debug` span or a helper's.
    static TOOL_SPAN: tracing::Span;
}

/// What the server tells a client it is for.
pub const INSTRUCTIONS: &str = "Open Knowledge Format catalogs: concepts (markdown with YAML \
    frontmatter) about systems, projects, people and decisions. Start with `search`, then \
    `get_concept`. Paths look like /infra/time-server.md (the leading slash and .md are \
    optional). Without a `catalog` argument, tools use this connection's selection: the \
    catalogs the connection or the key's default names, else the only catalog the key \
    reaches (`list_catalogs` shows it). When `list_catalogs` shows an empty selection, pass \
    `catalog`. Catalogs, users and teams each have an id (`name`) and a human \
    `label`. Say the label when talking to people, pass the id in tool calls, and never ask a \
    person to type an id. To write, call `open_changeset` first and pass the handle it \
    returns as `changeset` to every edit, `publish`, `discard`, `revert` and `pending_changes`; \
    reads take it too to see its drafts, and `list_changesets` finds a lost one. Nothing is \
    visible to others until `publish`. A call that depends on an earlier one (an edit to a \
    concept you are creating, a read after `publish`) must wait for its result: parallel calls \
    on one changeset run in no fixed order. \
    `whoami` says who you act for and the server's time. \
    When the person asks you to remember something, save it to the catalog (under a fitting \
    folder) rather than only in the conversation; asking is their consent, so don't ask again. \
    Don't save what they didn't ask you to keep.";

/// The line [`INSTRUCTIONS`] gains when `[feedback]` is set and
/// `send_feedback` is offered (issue #674), naming the product.
///
/// # Arguments
/// * `product_name` - the effective `product_name` setting.
///
/// # Returns
/// The line, "To tell Acme Notes's team about ...".
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial conversion: pure arithmetic or formatting on its arguments, no I/O"
    )
)]
pub fn feedback_instructions(product_name: &str) -> String {
    format!(
        "To tell {product_name}'s team about something confusing, broken, missing or good, call \
         `send_feedback` (it goes through a third-party form service; ask the user before \
         including catalog content or personal details)."
    )
}

/// The public tool reference (`docs/reference/tools.md`), generated from the tool list by a test.
/// Each tool's description ends with a link to its heading here (the tool's name is the anchor).
pub const TOOL_REFERENCE_URL: &str =
    "https://github.com/hypotheosis/okf_mcp/blob/main/docs/reference/tools.md";

/// A tool's description as `tools/list` sends it: its own text, then a
/// link to its heading in the public reference.
///
/// # Arguments
/// * `description` - the tool's own description.
/// * `name` - the tool's name (the heading's anchor).
///
/// # Returns
/// `"<description> Reference: <TOOL_REFERENCE_URL>#<name>"`.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial conversion: pure arithmetic or formatting on its arguments, no I/O"
    )
)]
fn with_reference(description: &str, name: &str) -> String {
    format!("{description} Reference: {TOOL_REFERENCE_URL}#{name}")
}

/// The MCP handler: one per session (rmcp's factory makes it).
pub struct OkfServer {
    app: Arc<App>,
    tool_router: ToolRouter<OkfServer>,
    // `INSTRUCTIONS`, plus `feedback_instructions` when the tool is offered.
    instructions: String,
    // `serverInfo`: `[auth] mcp_server_name`, the version, and the product
    // name as its display title.
    server_info: Implementation,
    // Held for the session's life: `okf.mcp.sessions.active` counts it open
    // until the handler is dropped.
    _session: SessionGuard,
}

impl OkfServer {
    /// A handler over the shared state.
    ///
    /// # Arguments
    /// * `app` - the shared state.
    ///
    /// # Returns
    /// The handler, with every tool registered, except `send_feedback`
    /// without `[feedback]`: then it is neither listed nor callable. With
    /// it, the tool's description and the instructions name the product
    /// as the `product_name` setting says when the session starts, as
    /// `serverInfo.title` does; `serverInfo.name` is `[auth]
    /// mcp_server_name`.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn new(app: Arc<App>) -> Self {
        let mut tool_router = Self::tools();
        let product_name = app.settings().product_name.clone();
        // `env!` reads this crate's version at compile time.
        let server_info = Implementation::new(
            &app.config().auth.mcp_server_name,
            env!("CARGO_PKG_VERSION"),
        )
        .with_title(&product_name);
        let instructions = if app.config().feedback.is_some() {
            if let Some(route) = tool_router.map.get_mut(feedback::TOOL) {
                route.attr.description = Some(
                    with_reference(&feedback::description(&product_name), feedback::TOOL).into(),
                );
            }
            format!("{INSTRUCTIONS} {}", feedback_instructions(&product_name))
        } else {
            tool_router.remove_route(feedback::TOOL);
            INSTRUCTIONS.to_string()
        };
        Self {
            _session: app.metrics().mcp_session(),
            app,
            tool_router,
            instructions,
            server_info,
        }
    }

    /// The `catalog` label and attribute for a call: the argument's value
    /// when it names an existing catalog, else `none`. A name the client
    /// typed that no catalog bears never becomes a label or an attribute.
    /// The check is a catalog directory lookup (no content is loaded); a
    /// failed lookup counts as `none`.
    ///
    /// # Arguments
    /// * `arguments` - the call's arguments.
    ///
    /// # Returns
    /// An existing catalog's name, or `"none"`.
    #[tracing::instrument(level = "trace", skip_all)]
    async fn catalog_label(&self, arguments: Option<&rmcp::model::JsonObject>) -> String {
        let named = arguments
            .and_then(|arguments| arguments.get("catalog"))
            .and_then(serde_json::Value::as_str);
        match named {
            Some(name) if matches!(self.app.catalog_row(name).await, Ok(Some(_))) => {
                name.to_string()
            }
            _ => "none".to_string(),
        }
    }

    /// Every tool: each module's router, joined with `+`.
    ///
    /// # Returns
    /// The router `#[tool_handler]` dispatches calls through.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn tools() -> ToolRouter<Self> {
        let mut router = Self::catalogs_router()
            + Self::search_router()
            + Self::documents_router()
            + Self::history_router()
            + Self::reports_router()
            + Self::writes_router()
            + Self::bookkeeping_router()
            + Self::lifecycle_router()
            + Self::open_router()
            + Self::publish_router()
            + Self::changes_router()
            + Self::restructure_router()
            + Self::maintenance_router()
            + Self::feedback_router()
            + Self::whoami_router()
            + Self::cited_by_router();
        // Point every description at the tool's heading in the public reference. `map` holds
        // each tool as `{ call, attr }`; `attr` is the `Tool` that `tools/list` sends.
        for (name, route) in router.map.iter_mut() {
            let description = route.attr.description.as_deref().unwrap_or_default();
            route.attr.description = Some(with_reference(description, name).into());
        }
        router
    }

    /// The context of the request rmcp handed a tool. The error is already a
    /// tool result, so a tool writes `let ctx = self.context(&parts)?;`.
    ///
    /// # Arguments
    /// * `parts` - the HTTP request.
    ///
    /// # Returns
    /// The call's context.
    ///
    /// # Errors
    /// As [`ToolContext::from_parts`], as an `isError` result.
    // `CallToolResult` is a large error type, but rmcp fixes it: a tool's
    // error must be one, and `?` here passes it straight through.
    #[allow(clippy::result_large_err)]
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub(crate) fn context(&self, parts: &Parts) -> Result<ToolContext, CallToolResult> {
        ToolContext::from_parts(&self.app, parts).map_err(|error| tool_error(&error))
    }
}

// `#[tool_handler]` writes `ServerHandler`'s `list_tools` over the stored
// router; `get_info` and `call_tool` are ours (the macro skips a method that
// is already written).
#[tool_handler(router = self.tool_router)]
impl ServerHandler for OkfServer {
    /// Every tool call, the single choke point: opens the `mcp <tool>` span
    /// (a child of the HTTP request span), records its outcome, and measures
    /// the call. The tool's own spans are its children.
    ///
    /// # Arguments
    /// * `request` - the tool name and arguments.
    /// * `context` - rmcp's request context (the HTTP request's parts are in
    ///   its extensions).
    ///
    /// # Returns
    /// The tool's result, unchanged.
    ///
    /// # Errors
    /// The router's protocol error (an unknown tool or bad arguments).
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "it opens the tool span itself")
    )]
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<CallToolResponse, rmcp::ErrorData> {
        let tool = tool_label(&request.name);
        let catalog = self.catalog_label(request.arguments.as_ref()).await;
        let parent = tool_span_parent(&context.extensions);
        let span = tracing::info_span!(
            parent: parent,
            "mcp",
            otel.name = %format_args!("mcp {tool}"),
            otel.kind = "server",
            otel.status_code = tracing::field::Empty,
            okf.tool = tool,
            okf.catalog = catalog.as_str(),
            okf.outcome = tracing::field::Empty,
            okf.error.kind = tracing::field::Empty,
            enduser.id = tracing::field::Empty,
            okf.key.id = tracing::field::Empty,
        );
        // The principal the auth layer put on the HTTP request, as
        // `ToolContext::from_parts` reads it; recorded on the handle.
        if let Some(principal) = context
            .extensions
            .get::<Parts>()
            .and_then(|parts| parts.extensions.get::<Arc<crate::auth::Principal>>())
        {
            crate::auth::record_principal_ids(&span, principal);
        }
        let started = std::time::Instant::now();
        let call = ToolCallContext::new(self, request, context);
        // `Instrument::instrument` enters the span on every poll of the
        // future, so the tool's own spans and events are its children;
        // `TOOL_SPAN.scope` makes the same span reachable from `tool_error`.
        let result = TOOL_SPAN
            .scope(
                span.clone(),
                self.tool_router.call(call).instrument(span.clone()),
            )
            .await;
        let outcome = outcome_of(&result);
        span.record("okf.outcome", outcome.as_str());
        if outcome == Outcome::Error {
            span.record("otel.status_code", "error");
        }
        self.app
            .metrics()
            .tool_call(tool, &catalog, outcome, started.elapsed());
        result
    }

    #[tracing::instrument(level = "trace", skip_all)]
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(self.server_info.clone())
            .with_instructions(self.instructions.clone())
    }
}

/// How a tool call ended: `Error` for a protocol error or an `isError`
/// result, else `Ok`.
///
/// # Arguments
/// * `result` - what the router returned.
///
/// # Returns
/// The outcome label.
#[tracing::instrument(level = "trace", skip_all)]
fn outcome_of(result: &Result<CallToolResponse, rmcp::ErrorData>) -> Outcome {
    match result {
        Ok(CallToolResponse::Complete(done)) if done.is_error != Some(true) => Outcome::Ok,
        _ => Outcome::Error,
    }
}

/// What every tool returns: a structured result, or an `isError` result.
///
/// A `#[tool]` method spells this type out (`Result<Json<T>, CallToolResult>`)
/// rather than naming the alias: rmcp's macro builds the tool's output schema
/// only when it sees `Json<...>` in the written return type.
pub type ToolResult<T> = Result<Json<T>, CallToolResult>;

/// An error as a tool result: `isError: true`, its JSON in the content.
/// Internal errors are logged here, with their details; the others at debug
/// level, by kind only (their messages can name paths from the catalog).
///
/// # Arguments
/// * `error` - what went wrong.
///
/// # Returns
/// The result to send the client.
#[tracing::instrument(level = "debug", skip_all)]
pub fn tool_error(error: &OkfError) -> CallToolResult {
    // The kind goes on the tool's root span through the task-local handle
    // (a no-op outside a tool call, as in the unit tests that call this).
    let _ = TOOL_SPAN.try_with(|span| {
        span.record("okf.error.kind", error.kind());
    });
    if error.is_internal() {
        // The detail (`Display`) can name a path or echo input, so it stays
        // on stderr; the exported event keeps the kind.
        split_event!(
            error,
            { kind = error.kind(), },
            { %error, },
            "tool failed"
        );
    } else {
        tracing::debug!(kind = error.kind(), "tool error");
    }
    CallToolResult::structured_error(error.to_json())
}

/// A tool function's outcome as a tool result.
///
/// # Arguments
/// * `result` - the plain function's value or error.
///
/// # Returns
/// The value as structured content, or the error as an `isError` result.
// A large error type, fixed by rmcp, as for `OkfServer::context`.
#[allow(clippy::result_large_err)]
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
    )
)]
pub fn reply<T>(result: Result<T, OkfError>) -> ToolResult<T> {
    result.map(Json).map_err(|error| tool_error(&error))
}

/// Runs CPU work (an analysis, a search) on a blocking thread, so the async
/// threads stay free; a panic in it is re-raised here.
///
/// # Arguments
/// * `work` - the closure to run; `'static` because it may outlive the
///   caller's borrows, so it must own (or `Arc`) what it reads.
///
/// # Returns
/// What `work` returns.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "carries the caller's span onto a blocking thread; a span of its own would replace the caller"
    )
)]
pub async fn blocking<T, F>(work: F) -> T
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    // Blocking threads do not inherit the current span or the scoped
    // subscriber, so spans opened by `work` would be unrelated roots. Carry
    // them across, as the rayon passes do.
    let caller = crate::okf::caller::Caller::capture();
    tokio::task::spawn_blocking(move || {
        let _entered = caller.enter();
        work()
    })
    .await
    .unwrap_or_else(|error| std::panic::resume_unwind(error.into_panic()))
}

#[cfg(test)]
mod telemetry_tests;

/// The parent of a tool call's `mcp <tool>` span.
///
/// The stateful session runs the handler on its worker task, not in the HTTP
/// request's task, so `Span::current()` is empty there. The request span the
/// HTTP layer stored in the request's extensions ([`crate::http::RequestSpan`])
/// is the parent instead; with none (a handler called directly) it is
/// whatever span is current, and with that also empty the tool span is a
/// root. (Rust note: `Option<Id>` works as a `parent:`, where `None` makes a
/// root span.)
///
/// # Arguments
/// * `extensions` - the MCP request's extensions, which hold the HTTP `Parts`.
///
/// # Returns
/// The parent span's id, or `None` for a root.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "its own span would become the current span it falls back to"
    )
)]
fn tool_span_parent(extensions: &rmcp::model::Extensions) -> Option<tracing::span::Id> {
    extensions
        .get::<Parts>()
        .and_then(|parts| parts.extensions.get::<crate::http::RequestSpan>())
        .and_then(|request_span| request_span.0.id())
        .or_else(|| tracing::Span::current().id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;

    use crate::config::Config;

    /// Issue #831: a client told to "remember this" must save to the catalog, and the
    /// instruction must not push the model to save what the person didn't ask to keep.
    #[test]
    fn the_instructions_tell_a_remember_request_to_save_to_the_catalog() {
        assert!(INSTRUCTIONS
            .contains("When the person asks you to remember something, save it to the catalog"));
        assert!(INSTRUCTIONS.contains("Don't save what they didn't ask you to keep."));
    }

    /// Issue #758: clients must not send dependent calls in parallel.
    #[test]
    fn the_instructions_tell_clients_to_sequence_dependent_calls() {
        assert!(INSTRUCTIONS.contains("must wait for its result"));
        assert!(INSTRUCTIONS.contains("run in no fixed order"));
    }

    /// Task 1b: an internal error's text can name a path or echo input, so
    /// the exported event keeps only its kind.
    #[test]
    fn an_internal_tool_error_exports_its_kind_and_not_its_text() {
        use crate::telemetry::config::LogFormat;
        use crate::telemetry::subscriber::capture::observe;

        let error = OkfError::Io {
            path: "/zq-sentinel-7f3a-path.md".to_string(),
            source: std::io::Error::other("disk"),
        };
        let observed = observe("trace", "trace", "trace", LogFormat::Text, || {
            tool_error(&error);
        });
        let exported = observed.exported_text();
        assert!(exported.contains("tool failed"), "{exported}");
        assert!(exported.contains("Io"), "{exported}");
        assert!(!exported.contains("zq-sentinel-7f3a"), "{exported}");
        assert!(observed.stderr.contains("zq-sentinel-7f3a-path"));
    }

    #[test]
    fn reply_passes_values_and_turns_errors_into_error_results() {
        let found: Result<u32, OkfError> = Ok(7);
        let missing: Result<u32, OkfError> = Err(OkfError::NotFound {
            path: "/x.md".to_string(),
        });

        let found = reply(found);
        let missing = reply(missing);

        // `Json<T>` is a one-field tuple struct; `.0` is the value inside.
        assert_eq!(found.ok().map(|json| json.0), Some(7));
        let error = missing.err().expect("an error result");
        assert_eq!(error.is_error, Some(true));
        assert_eq!(
            error.structured_content.as_ref().unwrap()["error"],
            "NotFound"
        );
        assert_eq!(error.structured_content.unwrap()["path"], "/x.md");
    }

    #[tokio::test]
    async fn blocking_returns_the_work_and_re_raises_its_panic() {
        let value = blocking(|| 2 + 2).await;
        // `tokio::spawn` catches the re-raised panic as a `JoinError`.
        let panicked = tokio::spawn(blocking(|| panic!("boom"))).await;

        assert_eq!(value, 4);
        let error = panicked.expect_err("the panic is re-raised");
        assert!(error.is_panic());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_server_info_takes_the_configured_name_and_the_product_title() {
        use crate::config::AuthConfig;
        use crate::store::audit::AuditActor;
        use crate::store::settings::{self, SettingKey};
        let config = Config {
            auth: AuthConfig {
                mcp_server_name: "acme-notes".to_string(),
                product_name: "Acme Notes".to_string(),
                ..AuthConfig::default()
            },
            ..Config::default()
        };
        let test = testing::app_with(&[("pg", &[])], config).await;

        let info = OkfServer::new(Arc::clone(&test.app)).get_info().server_info;
        assert_eq!(info.name, "acme-notes");
        assert_eq!(info.title.as_deref(), Some("Acme Notes"));
        assert_eq!(info.version, env!("CARGO_PKG_VERSION"));

        // A Settings override retitles the next session; the name stays.
        crate::invalidate::testing::applied(&test.app, async {
            settings::set(
                test.app.accounts().executor(),
                SettingKey::ProductName,
                "Other",
                &AuditActor::cli(),
                chrono::Utc::now(),
            )
            .await
            .expect("set product_name");
        })
        .await;
        let info = OkfServer::new(Arc::clone(&test.app)).get_info().server_info;
        assert_eq!(info.name, "acme-notes");
        assert_eq!(info.title.as_deref(), Some("Other"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_server_names_itself_and_needs_a_principal_on_the_request() {
        let test = testing::app_with(&[("pg", &[])], Config::default()).await;
        let server = OkfServer::new(Arc::clone(&test.app));
        let (mut with_key, _) = Request::new(()).into_parts();
        with_key.extensions.insert(testing::principal(&[], None));
        let (without_key, _) = Request::new(()).into_parts();

        let info = server.get_info();
        let admitted = server.context(&with_key);
        let refused = server.context(&without_key);

        assert_eq!(info.server_info.name, env!("CARGO_PKG_NAME"));
        assert_eq!(info.server_info.title.as_deref(), Some("okf"));
        assert!(info.capabilities.tools.is_some());
        assert_eq!(
            admitted
                .ok()
                .map(|ctx| ctx.principal().key_id.clone())
                .as_deref(),
            Some("tester")
        );
        let error = refused.err().expect("no principal, no context");
        assert_eq!(error.is_error, Some(true));
        assert_eq!(error.structured_content.unwrap()["error"], "Forbidden");
    }
}
