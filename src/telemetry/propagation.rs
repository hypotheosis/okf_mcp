//! W3C trace context at the edges. An inbound `traceparent` becomes a span
//! link ([`link_from`]); an outbound request carries the span it is made
//! under ([`inject`]).

use axum::http::HeaderMap;
use opentelemetry::propagation::TextMapPropagator;
use opentelemetry::trace::{SpanContext, TraceContextExt, TraceState};
use opentelemetry::Context;
use opentelemetry_http::{HeaderExtractor, HeaderInjector};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// The caller's trace context from a request's `traceparent`, for a span
/// link. The request span stays a root: a client (or anyone, since the
/// header is unauthenticated) never chooses our trace id (spec Decision 4).
///
/// Only the ids and flags are kept. The header's `tracestate` is dropped, so
/// no header text travels into an exported link.
///
/// # Arguments
/// * `headers` - the request's headers.
///
/// # Returns
/// The remote span context, or `None` when the header is absent, malformed or
/// not valid W3C trace context.
#[tracing::instrument(level = "trace", skip_all)]
pub fn link_from(headers: &HeaderMap) -> Option<SpanContext> {
    // A propagator is a stateless parser. `extract_with_context` starts from
    // the context it is given and returns it unchanged when the header is
    // absent or malformed; plain `extract` starts from `Context::current()`,
    // which (with context activation on) is the local active span, so it
    // would return that span as a fabricated "remote" link. Start from an
    // empty context instead, so only a parsed header can yield a link.
    let context = TraceContextPropagator::new()
        .extract_with_context(&Context::new(), &HeaderExtractor(headers));
    let remote = context.span().span_context().clone();
    remote.is_valid().then(|| {
        SpanContext::new(
            remote.trace_id(),
            remote.span_id(),
            remote.trace_flags(),
            true,
            TraceState::default(),
        )
    })
}

/// The OpenTelemetry trace id of `span`, for quoting to a person who can
/// give it to the operator (an internal error's `trace_id`, issues #766 and
/// #1093).
///
/// # Arguments
/// * `span` - the tracing span to name (a handle: it need not be current).
///
/// # Returns
/// 32 lower-case hex digits, or `None` when the span has no valid trace
/// context (telemetry off, or the span filtered out: its id would be all
/// zeros, which names nothing).
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(uninstrumented_fn, reason = "a pure read of the span it is given")
)]
pub fn trace_id_of(span: &tracing::Span) -> Option<String> {
    let span_context = span.context().span().span_context().clone();
    span_context
        .is_valid()
        .then(|| span_context.trace_id().to_string())
}

/// Adds a `traceparent` for the span the request is made under to an
/// outbound request's headers, so the callee can join the trace. Nothing is
/// added when that span is not exported (no trace layer, or filtered out).
///
/// The context is the current *tracing span's*, never a bare
/// `Context::current()`: the active OpenTelemetry context is not where
/// `tracing` keeps the caller. This function is deliberately not
/// instrumented: a span of its own would become the current one, and the
/// header would name the helper instead of the caller.
///
/// # Arguments
/// * `headers` - the outbound request's headers (reqwest 0.13 and axum share
///   http 1.x's `HeaderMap`).
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "a span of its own would replace the caller's in the header"
    )
)]
pub fn inject(headers: &mut HeaderMap) {
    let context = tracing::Span::current().context();
    TraceContextPropagator::new().inject_context(&context, &mut HeaderInjector(headers));
}

/// A local server that records the `traceparent` of every request, for the
/// outbound clients' tests.
#[cfg(test)]
pub(crate) mod testing {
    use std::sync::{Arc, Mutex};

    use axum::http::HeaderMap;
    use axum::routing::any;
    use axum::{Json, Router};

    use crate::telemetry::config::LogFormat;
    use crate::telemetry::subscriber::capture::{Observed, Observer};

    /// The `traceparent` values (or `None` for a request without one) the
    /// stub saw, oldest first.
    pub(crate) type Seen = Arc<Mutex<Vec<Option<String>>>>;

    /// Serves `{}` to any request on a free port and records each request's
    /// `traceparent`.
    ///
    /// # Returns
    /// The base URL (`http://127.0.0.1:<port>`) and what the stub saw.
    pub(crate) async fn echo_stub() -> (String, Seen) {
        let seen = Seen::default();
        let record = Arc::clone(&seen);
        let router = Router::new().fallback(any(move |headers: HeaderMap| async move {
            let value = headers
                .get("traceparent")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            record.lock().expect("stub lock").push(value);
            Json(serde_json::json!({}))
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("address"));
        tokio::spawn(async move { axum::serve(listener, router).await });
        (url, seen)
    }

    /// Runs `request` (one outbound call to `stub`) inside a span called
    /// `caller` at each span filter, and checks the stub received a
    /// `traceparent` carrying that span's trace id.
    ///
    /// # Arguments
    /// * `seen` - what [`echo_stub`] returned.
    /// * `request` - makes the call; run once per filter.
    pub(crate) async fn assert_joins_the_callers_trace<F, Fut>(seen: &Seen, request: F)
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        use tracing::Instrument;
        for filter in ["info", "trace"] {
            seen.lock().expect("stub lock").clear();
            let observer = Observer::start("off", filter, "off", LogFormat::Text);
            request().instrument(tracing::info_span!("caller")).await;
            let observed: Observed = observer.finish();
            let span = &observed.spans_named("caller")[0];
            let seen = seen.lock().expect("stub lock").clone();
            let header = seen
                .last()
                .cloned()
                .flatten()
                .unwrap_or_else(|| panic!("{filter}: no traceparent sent: {seen:?}"));
            let trace_id = span.span_context.trace_id().to_string();
            assert!(
                header.starts_with(&format!("00-{trace_id}-")),
                "{filter}: {header} is not in trace {trace_id}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_valid_traceparent_becomes_a_remote_context_without_tracestate() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "traceparent",
            "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"
                .parse()
                .expect("a header value"),
        );
        headers.insert("tracestate", "vendor=zq-sentinel".parse().expect("value"));

        let link = link_from(&headers).expect("a link");

        assert_eq!(
            link.trace_id().to_string(),
            "0af7651916cd43dd8448eb211c80319c"
        );
        assert_eq!(link.span_id().to_string(), "b7ad6b7169203331");
        assert!(link.is_remote());
        assert!(link.is_sampled());
        assert_eq!(link.trace_state().header(), "");
    }

    /// Runs `body` inside an active, exported span at each span filter, as a
    /// request handled under another span (or at `trace`, under this
    /// function's own span) would be.
    fn under_an_active_span(body: impl Fn() + Copy) {
        use crate::telemetry::config::LogFormat;
        use crate::telemetry::subscriber::capture::observe;
        for filter in ["info", "trace"] {
            observe("off", filter, "off", LogFormat::Text, || {
                let ambient = tracing::info_span!("ambient");
                let _entered = ambient.enter();
                body();
            });
        }
    }

    #[test]
    fn no_header_gives_no_link_under_an_active_span() {
        under_an_active_span(|| assert!(link_from(&HeaderMap::new()).is_none()));
    }

    #[test]
    fn a_malformed_header_gives_no_link_under_an_active_span() {
        under_an_active_span(|| {
            let mut headers = HeaderMap::new();
            for bad in [
                "garbage",
                "00-00000000000000000000000000000000-b7ad6b7169203331-01",
                "00-0af7651916cd43dd8448eb211c80319c-0000000000000000-01",
            ] {
                headers.insert("traceparent", bad.parse().expect("value"));
                assert!(link_from(&headers).is_none(), "{bad}");
            }
        });
    }

    #[test]
    fn a_valid_header_still_links_under_an_active_span() {
        under_an_active_span(|| {
            let mut headers = HeaderMap::new();
            headers.insert(
                "traceparent",
                "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"
                    .parse()
                    .expect("value"),
            );
            let link = link_from(&headers).expect("a link");
            assert_eq!(
                link.trace_id().to_string(),
                "0af7651916cd43dd8448eb211c80319c"
            );
        });
    }

    #[test]
    fn a_missing_or_malformed_header_gives_no_link() {
        let mut headers = HeaderMap::new();
        assert!(link_from(&headers).is_none());
        headers.insert("traceparent", "garbage".parse().expect("value"));
        assert!(link_from(&headers).is_none());
        headers.insert(
            "traceparent",
            "00-00000000000000000000000000000000-b7ad6b7169203331-01"
                .parse()
                .expect("value"),
        );
        assert!(link_from(&headers).is_none());
    }

    /// The `(trace id, span id)` of a `traceparent` header.
    fn ids_of(headers: &HeaderMap) -> (String, String) {
        let value = headers
            .get("traceparent")
            .expect("a traceparent")
            .to_str()
            .expect("ascii");
        let parts: Vec<&str> = value.split('-').collect();
        assert_eq!(parts.len(), 4, "{value}");
        (parts[1].to_string(), parts[2].to_string())
    }

    #[test]
    fn inject_names_the_span_the_request_is_made_under_at_both_filters() {
        use crate::telemetry::config::LogFormat;
        use crate::telemetry::subscriber::capture::observe;
        for filter in ["info", "trace"] {
            let mut headers = HeaderMap::new();
            let observed = observe("off", filter, "off", LogFormat::Text, || {
                let ambient = tracing::info_span!("ambient");
                let _entered = ambient.enter();
                inject(&mut headers);
            });
            let span = &observed.spans_named("ambient")[0];
            let (trace, parent) = ids_of(&headers);
            assert_eq!(trace, span.span_context.trace_id().to_string(), "{filter}");
            assert_eq!(parent, span.span_context.span_id().to_string(), "{filter}");
        }
    }

    #[test]
    fn inject_ignores_an_unrelated_active_opentelemetry_context() {
        use crate::telemetry::config::LogFormat;
        use crate::telemetry::subscriber::capture::observe;
        use opentelemetry::trace::{SpanId, TraceFlags, TraceId};
        let mut headers = HeaderMap::new();
        let observed = observe("off", "info", "off", LogFormat::Text, || {
            let ambient = tracing::info_span!("ambient");
            let _entered = ambient.enter();
            // Another library's context is active; the caller is still the
            // tracing span.
            let other = SpanContext::new(
                TraceId::from(7_u128),
                SpanId::from(9_u64),
                TraceFlags::SAMPLED,
                true,
                TraceState::default(),
            );
            let _attached = Context::new().with_remote_span_context(other).attach();
            inject(&mut headers);
        });
        let span = &observed.spans_named("ambient")[0];
        let (trace, parent) = ids_of(&headers);
        assert_eq!(trace, span.span_context.trace_id().to_string());
        assert_eq!(parent, span.span_context.span_id().to_string());
    }

    #[test]
    fn inject_adds_nothing_without_an_exported_span() {
        let mut headers = HeaderMap::new();

        inject(&mut headers);

        assert!(headers.get("traceparent").is_none());
        assert!(headers.get("tracestate").is_none());
    }
}
