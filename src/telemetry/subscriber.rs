//! The subscriber: one stderr layer (text, or JSON with trace ids), and the
//! OTLP span and log layers when their providers exist.
//!
//! Nothing filters the whole stack. Each layer has its own filter, so a
//! quiet stderr never silences the exporters (spec Decision 3). Each filter
//! is ANDed with a fixed cap (`stderr_cap`, `otlp_cap`) that the
//! configuration cannot lift.

use std::sync::{Mutex, PoisonError};

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing::Level;
use tracing_subscriber::filter::{EnvFilter, FilterExt, LevelFilter, Targets};
use tracing_subscriber::fmt;
use tracing_subscriber::fmt::writer::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{Layer, Registry};

use super::config::{
    parse_filter, LogFormat, TelemetryConfig, DEFAULT_OTLP_FILTER, DEFAULT_STDERR_FILTER,
};
use super::json::{CaptureDispatch, DispatchSlot, JsonWithTrace};

/// Targets the OTLP layers never take: the export stack's own events
/// (`internal-logs` uses each crate's package name as target, some with
/// hyphens) and the HTTP stack it sends with. An export failure exported
/// over the export that failed would loop.
///
/// The OTLP cap is an allow-list of this crate's targets, so none of these
/// needs turning off by name. The list stays as the record of what must
/// never be allow-listed.
pub const EXPORT_STACK: [&str; 10] = [
    "opentelemetry",
    "opentelemetry_sdk",
    "opentelemetry-otlp",
    "opentelemetry-http",
    "opentelemetry-prometheus",
    "opentelemetry-appender-tracing",
    "hyper",
    "hyper_util",
    "reqwest",
    "h2",
];

/// The target of events that may carry text the privacy rules keep out of
/// OTLP: a caller-supplied value, an internal error's full text, the `log`
/// mail provider's message. Such events reach stderr only. `pub` so every
/// module writes `tracing::warn!(target: LOCAL_ONLY, ...)`, or `split_event!`.
pub const LOCAL_ONLY: &str = "okf_mcp::local_only";

/// Emits one event in two halves: the exported half, with values the
/// privacy rules allow in OTLP, and a [`LOCAL_ONLY`] half beside it, with the
/// values they keep out (caller text, an error's full text, content). On
/// stderr that is two lines, one per sink; the second ends in `(detail)`.
///
/// ```ignore
/// split_event!(
///     warn,
///     { kind = error.kind(), },
///     { client_id = %id, reason = %error, },
///     "refused an OAuth client"
/// );
/// ```
///
/// Each field list ends in a comma (or is empty). The message is a literal,
/// which may capture variables (`{what}`).
macro_rules! split_event {
    ($level:ident, { $($exported:tt)* }, { $($local:tt)* }, $message:literal) => {{
        tracing::$level!($($exported)* $message);
        tracing::$level!(
            target: $crate::telemetry::subscriber::LOCAL_ONLY,
            $($local)*
            concat!($message, " (detail)")
        );
    }};
}
pub(crate) use split_event;

/// The highest level any layer takes from `rmcp`: at `debug` it logs whole
/// JSON-RPC messages, credentials among them.
const RMCP_CAP: Level = Level::INFO;

/// Whether `target` is this crate's own, and may reach OTLP: exactly
/// `okf_mcp`, or a module path under it (`okf_mcp::`), never a longer name
/// that merely starts with it (`okf_mcp_other`), and never [`LOCAL_ONLY`] or
/// anything under it.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "runs inside the subscriber's per-event filter: a span here re-enters the subscriber"
    )
)]
fn is_exportable_target(target: &str) -> bool {
    let own = target == "okf_mcp" || target.starts_with("okf_mcp::");
    let local_only = target == LOCAL_ONLY
        || target
            .strip_prefix(LOCAL_ONLY)
            .is_some_and(|rest| rest.starts_with("::"));
    own && !local_only
}

/// What the OTLP layers AND with their own filter: an allow-list of this
/// crate's own targets (see [`is_exportable_target`]), not a deny-list.
/// Dependency events are out of the privacy rules' reach: `sqlx` logs
/// statement text at `debug` (and slow queries at `warn`), and `apalis`,
/// `openidconnect`, the AWS SDK and the export stack ([`EXPORT_STACK`]) have
/// their own.
///
/// A `filter_fn` rather than `Targets`, whose directives match by string
/// prefix (`okf_mcp` would take `okf_mcp_other`).
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "runs before the subscriber exists (configuration, builder or start-up), so a span would be recorded nowhere"
    )
)]
fn otlp_cap<S>() -> impl tracing_subscriber::layer::Filter<S>
where
    S: tracing::Subscriber,
{
    // A closure (Python's lambda) over the span or event's metadata.
    tracing_subscriber::filter::filter_fn(|metadata| is_exportable_target(metadata.target()))
}

/// What stderr ANDs with its own filter: every target, `rmcp` capped.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "runs before the subscriber exists (configuration, builder or start-up), so a span would be recorded nowhere"
    )
)]
fn stderr_cap() -> Targets {
    Targets::new()
        .with_default(LevelFilter::TRACE)
        .with_target("rmcp", RMCP_CAP)
}

/// The filters and format the layers use, already resolved (`RUST_LOG`
/// applied), so [`build`] is a pure function of its inputs.
#[derive(Debug, Clone)]
pub struct LayerSettings {
    /// stderr's line format.
    pub format: LogFormat,
    /// stderr's filter.
    pub stderr: EnvFilter,
    /// The OTLP span layer's filter.
    pub traces: EnvFilter,
    /// The OTLP log layer's filter.
    pub logs: EnvFilter,
}

/// Resolves the filters for the layers.
///
/// stderr's filter comes from `rust_log`, else `[telemetry.stderr] filter`,
/// else [`DEFAULT_STDERR_FILTER`]; an invalid or blank `RUST_LOG` falls back
/// to the next source, as it always has. The OTLP filters come from
/// `[telemetry.otlp]`, else [`DEFAULT_OTLP_FILTER`]. Directive components are
/// trimmed before parsing.
///
/// # Arguments
/// * `config` - `[telemetry]`, already validated.
/// * `rust_log` - the value of `RUST_LOG`, if set.
///
/// # Returns
/// The settings.
///
/// # Errors
/// A configured filter that does not parse (validation refuses these first).
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "runs before the subscriber exists (configuration, builder or start-up), so a span would be recorded nowhere"
    )
)]
pub fn settings(config: &TelemetryConfig, rust_log: Option<&str>) -> Result<LayerSettings, String> {
    let from_env = rust_log
        .filter(|text| !text.trim().is_empty())
        .and_then(|text| parse_filter(text).ok());
    let stderr = match from_env {
        Some(filter) => filter,
        None => parse_filter(
            config
                .stderr
                .filter
                .as_deref()
                .unwrap_or(DEFAULT_STDERR_FILTER),
        )
        .map_err(|error| format!("telemetry.stderr.filter: {error}"))?,
    };
    let (traces, logs) = match &config.otlp {
        Some(otlp) => (
            parse_filter(&otlp.traces_filter)
                .map_err(|error| format!("telemetry.otlp.traces_filter: {error}"))?,
            parse_filter(&otlp.logs_filter)
                .map_err(|error| format!("telemetry.otlp.logs_filter: {error}"))?,
        ),
        None => (
            parse_filter(DEFAULT_OTLP_FILTER)?,
            parse_filter(DEFAULT_OTLP_FILTER)?,
        ),
    };
    Ok(LayerSettings {
        format: config.stderr.format,
        stderr,
        traces,
        logs,
    })
}

/// The subscriber: stderr, plus the OTLP span and log layers when their
/// providers exist.
///
/// # Arguments
/// * `settings` - the resolved filters and format.
/// * `tracer` - the span provider, when OTLP traces are on.
/// * `logger` - the log provider, when OTLP logs are on.
///
/// # Returns
/// A subscriber writing to stderr.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "runs before the subscriber exists (configuration, builder or start-up), so a span would be recorded nowhere"
    )
)]
pub fn build(
    settings: LayerSettings,
    tracer: Option<&SdkTracerProvider>,
    logger: Option<&SdkLoggerProvider>,
) -> impl tracing::Subscriber + Send + Sync {
    build_with_writer(settings, tracer, logger, std::io::stderr)
}

/// [`build`] with the stderr layer's writer as a parameter, so tests can
/// capture what it writes.
///
/// It keeps a handle on each provider until the process exits
/// (`KEPT_TRACERS`, `KEPT_LOGGERS`), so dropping the subscriber never drops a
/// provider.
///
/// # Arguments
/// * `settings`, `tracer`, `logger` - as for [`build`].
/// * `writer` - where the text or JSON lines go (`std::io::stderr` in
///   production). A `MakeWriter` is anything that hands out a `Write` per
///   event; a bare function returning a writer is one.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "runs before the subscriber exists (configuration, builder or start-up), so a span would be recorded nowhere"
    )
)]
pub fn build_with_writer<W>(
    settings: LayerSettings,
    tracer: Option<&SdkTracerProvider>,
    logger: Option<&SdkLoggerProvider>,
    writer: W,
) -> impl tracing::Subscriber + Send + Sync
where
    W: for<'writer> MakeWriter<'writer> + Send + Sync + 'static,
{
    keep_for_the_process(tracer, logger);
    let slot = DispatchSlot::default();
    // A filter's type names the subscriber it is added to, so each arm builds
    // its own. `FilterExt::and` combines two filters: an event passes when both pass.
    // `Option<Layer>` is itself a layer that does nothing when `None`, so the
    // layers that are off need no separate type.
    let (text, json) = match settings.format {
        LogFormat::Text => (
            Some(
                fmt::layer()
                    .with_writer(writer)
                    .with_filter(settings.stderr.and(stderr_cap())),
            ),
            None,
        ),
        LogFormat::Json => (
            None,
            Some(
                fmt::layer()
                    .with_writer(writer)
                    .event_format(JsonWithTrace(slot.clone()))
                    .with_filter(settings.stderr.and(stderr_cap())),
            ),
        ),
    };
    let spans = tracer.map(|provider| {
        tracing_opentelemetry::layer()
            .with_tracer(provider.tracer("okf-mcp"))
            // Error text never becomes an attribute, an event or a status
            // description (privacy rules): an ERROR event still marks the
            // span `Error`, with no description.
            .with_error_fields_to_exceptions(false)
            .with_error_records_to_exceptions(false)
            .with_error_events_to_exceptions(false)
            .with_error_events_to_status(false)
            .with_location(false)
            .with_threads(false)
            .with_target(false)
            .with_tracked_inactivity(false)
            // Entering a span makes its OTel context the thread's current one,
            // which is how the log bridge gives a record the span's trace and
            // span ids (`opentelemetry-appender-tracing` 0.33 has no feature
            // for it). Stated here so a changed default cannot drop it.
            .with_context_activation(true)
            .with_filter(settings.traces.and(otlp_cap()))
    });
    let logs = logger.map(|provider| {
        OpenTelemetryTracingBridge::new(provider).with_filter(settings.logs.and(otlp_cap()))
    });
    Registry::default()
        .with(CaptureDispatch(slot))
        .with(text)
        .with(json)
        .with(spans)
        .with(logs)
}

/// Every provider a subscriber has been built on, kept until the process
/// exits, so that no subscriber ever drops the last handle on a provider.
///
/// Dropping a provider's last handle runs its `Drop`, and with
/// `internal-logs` on (as here) that emits a `tracing` event, already shut
/// down or not (`LoggerProvider.Drop.AlreadyShutdown`,
/// `TracerProvider.Drop.AlreadyShutdown`). When the subscriber holding that
/// handle is dropped inside `tracing_core`'s dispatcher registry lock (its
/// interest rebuild upgrades each registered subscriber's weak handle for a
/// moment, and the upgraded one can be the last), the event's first
/// callsite registration takes the same lock again and deadlocks every
/// thread that registers a callsite or a subscriber (issue #738). A provider
/// that is never dropped emits no such event.
///
/// In production this changes nothing: the only subscriber built on a
/// provider is the global default, which is never dropped, so it already
/// held its providers for the life of the process. The cost is one clone per
/// subscriber built: one in production, one per capture in tests.
static KEPT_TRACERS: Mutex<Vec<SdkTracerProvider>> = Mutex::new(Vec::new());
/// The log providers, as for [`KEPT_TRACERS`].
static KEPT_LOGGERS: Mutex<Vec<SdkLoggerProvider>> = Mutex::new(Vec::new());

/// Adds `tracer` and `logger` to [`KEPT_TRACERS`] and [`KEPT_LOGGERS`].
///
/// # Arguments
/// * `tracer`, `logger` - the providers a subscriber is about to be built on.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "runs before the subscriber exists (configuration, builder or start-up), so a span would be recorded nowhere"
    )
)]
fn keep_for_the_process(tracer: Option<&SdkTracerProvider>, logger: Option<&SdkLoggerProvider>) {
    // A panic while the lock was held cannot leave a `Vec` half-pushed, so a
    // poisoned lock's contents are still good: take them.
    if let Some(tracer) = tracer {
        KEPT_TRACERS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(tracer.clone());
    }
    if let Some(logger) = logger {
        KEPT_LOGGERS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(logger.clone());
    }
}

/// The subscriber used while the configuration is being read and telemetry
/// started: stderr text at `RUST_LOG`, else [`DEFAULT_STDERR_FILTER`], no
/// OTLP. It is scoped to start-up only (see [`super::start_up`]).
///
/// # Arguments
/// * `rust_log` - the value of `RUST_LOG`, if set.
/// * `writer` - where the lines go.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "runs before the subscriber exists (configuration, builder or start-up), so a span would be recorded nowhere"
    )
)]
pub fn fallback<W>(rust_log: Option<&str>, writer: W) -> impl tracing::Subscriber + Send + Sync
where
    W: for<'writer> MakeWriter<'writer> + Send + Sync + 'static,
{
    let filter = rust_log
        .filter(|text| !text.trim().is_empty())
        .and_then(|text| parse_filter(text).ok())
        .or_else(|| parse_filter(DEFAULT_STDERR_FILTER).ok())
        .unwrap_or_else(|| EnvFilter::new("warn"));
    Registry::default().with(
        fmt::layer()
            .with_writer(writer)
            .with_filter(filter.and(stderr_cap())),
    )
}

/// A capturing stderr for tests: an `Arc<Mutex<Vec<u8>>>` that implements
/// `MakeWriter`.
#[cfg(test)]
pub(crate) mod capture {
    use std::io;
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::fmt::writer::MakeWriter;

    /// Everything written through it, shared between clones.
    #[derive(Debug, Clone, Default)]
    pub struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Capture {
        /// What has been written so far, as text.
        pub fn text(&self) -> String {
            let bytes = self.0.lock().expect("capture lock").clone();
            String::from_utf8_lossy(&bytes).into_owned()
        }
    }

    /// One event's writer.
    pub struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

    impl io::Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("capture lock").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Capture {
        type Writer = CaptureWriter;

        fn make_writer(&'a self) -> Self::Writer {
            CaptureWriter(Arc::clone(&self.0))
        }
    }

    use opentelemetry_sdk::logs::{InMemoryLogExporter, SdkLoggerProvider};
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};

    /// A field's value as `tracing` recorded it.
    #[derive(Debug, Clone, PartialEq)]
    pub(crate) enum SeenValue {
        /// Recorded as a signed integer (`record_i64`).
        I64(i64),
        /// Recorded as a string, or through `Display`/`Debug`.
        Text(String),
        /// Any other type (`u64`, `bool`, and so on).
        Other,
    }

    /// One span as the recorder saw it.
    #[derive(Debug, Clone)]
    pub(crate) struct SeenSpan {
        /// A number unique within the recorder.
        pub id: u64,
        /// The span's name.
        pub name: String,
        /// The id of its parent; `None` for a root.
        pub parent: Option<u64>,
        /// Its fields, as recorded at creation and by later `record` calls.
        pub fields: Vec<(String, SeenValue)>,
    }

    impl SeenSpan {
        /// A field's value.
        pub(crate) fn field(&self, name: &str) -> Option<&SeenValue> {
            self.fields
                .iter()
                .rev()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value)
        }
    }

    struct SeenVisitor<'a>(&'a mut Vec<(String, SeenValue)>);

    impl tracing::field::Visit for SeenVisitor<'_> {
        fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
            self.0
                .push((field.name().to_string(), SeenValue::I64(value)));
        }
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.0
                .push((field.name().to_string(), SeenValue::Text(value.to_string())));
        }
        fn record_u64(&mut self, field: &tracing::field::Field, _value: u64) {
            self.0.push((field.name().to_string(), SeenValue::Other));
        }
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0.push((
                field.name().to_string(),
                SeenValue::Text(format!("{value:?}")),
            ));
        }
    }

    /// The layer behind [`SpanRecorder`]: notes each span as it is created.
    struct Collect(Arc<Mutex<Vec<SeenSpan>>>);

    impl<S> tracing_subscriber::Layer<S> for Collect
    where
        S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            id: &tracing::span::Id,
            ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let parent = if attrs.is_root() {
                None
            } else if let Some(parent) = attrs.parent() {
                Some(parent.into_u64())
            } else {
                ctx.current_span().id().map(tracing::span::Id::into_u64)
            };
            let mut fields = Vec::new();
            attrs.record(&mut SeenVisitor(&mut fields));
            self.0.lock().expect("recorder lock").push(SeenSpan {
                id: id.into_u64(),
                name: attrs.metadata().name().to_string(),
                parent,
                fields,
            });
        }

        fn on_record(
            &self,
            id: &tracing::span::Id,
            values: &tracing::span::Record<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut seen = self.0.lock().expect("recorder lock");
            if let Some(span) = seen.iter_mut().find(|span| span.id == id.into_u64()) {
                values.record(&mut SeenVisitor(&mut span.fields));
            }
        }
    }

    /// A span recorder for async tests: run a future under
    /// [`SpanRecorder::dispatch`] with `tracing::instrument::WithSubscriber`
    /// (a thread-local default would miss a multi-thread runtime's other
    /// workers), then read [`SpanRecorder::finished`]. It records at the
    /// `tracing` level, so what a test sees is what the code recorded, with no
    /// exporter, sampler or thread-local OpenTelemetry context in between.
    pub(crate) struct SpanRecorder {
        dispatch: tracing::Dispatch,
        seen: Arc<Mutex<Vec<SeenSpan>>>,
    }

    impl SpanRecorder {
        /// A recorder with no spans yet.
        pub(crate) fn new() -> Self {
            use tracing_subscriber::layer::SubscriberExt;

            let seen = Arc::new(Mutex::new(Vec::new()));
            let subscriber = tracing_subscriber::registry().with(Collect(Arc::clone(&seen)));
            Self {
                dispatch: crate::test_tracing::keep_alive(subscriber),
                seen,
            }
        }

        /// Runs `future` to completion with this recorder as the default
        /// subscriber, on a thread and a current-thread runtime of its own,
        /// so every poll of it sees the default (a future on a multi-thread
        /// runtime hops between workers, and other tests' subscribers share
        /// those). The caller blocks meanwhile, so call it from a test body,
        /// not from a runtime worker; what `future` uses from the test's own
        /// runtime (the pool) keeps being served by that runtime's workers.
        ///
        /// # Panics
        /// If the runtime cannot be built, or `future` panics.
        pub(crate) fn run<F>(&self, future: F) -> F::Output
        where
            F: std::future::Future + Send,
            F::Output: Send,
        {
            // A scoped thread may borrow from the caller's stack.
            std::thread::scope(|scope| {
                scope
                    .spawn(|| {
                        let runtime = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .expect("a current-thread runtime");
                        tracing::dispatcher::with_default(&self.dispatch, || {
                            runtime.block_on(future)
                        })
                    })
                    .join()
                    .expect("the recorded future finished without panicking")
            })
        }

        /// The spans created so far, in creation order.
        pub(crate) fn finished(&self) -> Vec<SeenSpan> {
            self.seen.lock().expect("recorder lock").clone()
        }
    }

    /// A span's attribute, by key.
    pub(crate) fn attribute<'a>(
        span: &'a opentelemetry_sdk::trace::SpanData,
        key: &str,
    ) -> Option<&'a opentelemetry::Value> {
        span.attributes
            .iter()
            .find(|attribute| attribute.key.as_str() == key)
            .map(|attribute| &attribute.value)
    }

    /// Everything one run of a subscriber produced.
    pub(crate) struct Observed {
        pub stderr: String,
        pub spans: Vec<opentelemetry_sdk::trace::SpanData>,
        pub logs: Vec<String>,
        /// The log records, with their trace context.
        pub records: Vec<opentelemetry_sdk::logs::SdkLogRecord>,
    }

    impl Observed {
        /// The exported spans called `name` (their `otel.name`).
        pub(crate) fn spans_named(&self, name: &str) -> Vec<&opentelemetry_sdk::trace::SpanData> {
            self.spans.iter().filter(|span| span.name == name).collect()
        }

        /// The exported spans in every trace whose root request span links to
        /// the trace id `caller_trace` (32 hex digits): the spans a test's own
        /// requests made, whatever other tests exported at the same time
        /// (issue #848).
        ///
        /// A request span is a root that links, never adopts, the trace of a
        /// caller's `traceparent`, so a test sends a `traceparent` of its own
        /// choosing with each request and reads back only what that id marks.
        pub(crate) fn spans_of_requests_from(
            &self,
            caller_trace: &str,
        ) -> Vec<&opentelemetry_sdk::trace::SpanData> {
            let own_traces: Vec<_> = self
                .spans
                .iter()
                .filter(|span| {
                    span.links
                        .links
                        .iter()
                        .any(|link| link.span_context.trace_id().to_string() == caller_trace)
                })
                .map(|span| span.span_context.trace_id())
                .collect();
            self.spans
                .iter()
                .filter(|span| own_traces.contains(&span.span_context.trace_id()))
                .collect()
        }

        /// Everything exported, as one text: each log record (target, body
        /// and attributes) and each span, for a test that a value never
        /// reached OTLP.
        pub(crate) fn exported_text(&self) -> String {
            format!("{:?}|{:?}|{:?}", self.logs, self.records, self.spans)
        }
    }

    /// A subscriber with in-memory exporters, installed as the thread's
    /// default until [`Observer::finish`]. Unlike [`observe`], the scope
    /// spans `.await`s: use it from a `#[tokio::test]` (a current-thread
    /// runtime, so every task the test spawns runs on this thread and sees
    /// the default).
    pub(crate) struct Observer {
        span_exporter: InMemorySpanExporter,
        log_exporter: InMemoryLogExporter,
        tracer: SdkTracerProvider,
        logger: SdkLoggerProvider,
        capture: Capture,
        guard: tracing::subscriber::DefaultGuard,
    }

    impl Observer {
        /// Installs the subscriber.
        ///
        /// # Arguments
        /// * `stderr`, `traces`, `logs` - the three filters, as in `observe`.
        /// * `format` - the stderr format.
        pub(crate) fn start(
            stderr: &str,
            traces: &str,
            logs: &str,
            format: super::LogFormat,
        ) -> Self {
            let span_exporter = InMemorySpanExporter::default();
            let log_exporter = InMemoryLogExporter::default();
            let tracer = SdkTracerProvider::builder()
                .with_simple_exporter(span_exporter.clone())
                .build();
            let logger = SdkLoggerProvider::builder()
                .with_simple_exporter(log_exporter.clone())
                .build();
            let capture = Capture::default();
            let settings = super::LayerSettings {
                format,
                stderr: super::parse_filter(stderr).expect("stderr filter"),
                traces: super::parse_filter(traces).expect("traces filter"),
                logs: super::parse_filter(logs).expect("logs filter"),
            };
            let subscriber =
                super::build_with_writer(settings, Some(&tracer), Some(&logger), capture.clone());
            // The helper keeps a first registration on another thread from
            // caching "disabled" for a callsite this test uses (issue #481).
            let guard = crate::test_tracing::set_default(subscriber);
            Self {
                span_exporter,
                log_exporter,
                tracer,
                logger,
                capture,
                guard,
            }
        }

        /// Uninstalls the subscriber, flushes, and returns what it saw.
        pub(crate) fn finish(self) -> Observed {
            // The dispatcher goes first, so the spans and records it saw
            // are all in the exporters before they are read. (The providers
            // themselves are never dropped: `build_with_writer` keeps them
            // for the process, issue #738.)
            drop(self.guard);
            self.tracer.force_flush().expect("flush spans");
            self.logger.force_flush().expect("flush logs");
            let emitted = self.log_exporter.get_emitted_logs().expect("logs");
            let observed = Observed {
                stderr: self.capture.text(),
                spans: self.span_exporter.get_finished_spans().expect("spans"),
                records: emitted.iter().map(|record| record.record.clone()).collect(),
                logs: emitted
                    .iter()
                    .map(|record| {
                        let body = record.record.body().map(|value| format!("{value:?}"));
                        format!(
                            "{}|{}",
                            record
                                .record
                                .target()
                                .map(ToString::to_string)
                                .unwrap_or_default(),
                            body.unwrap_or_default()
                        )
                    })
                    .collect(),
            };
            let _ = self.tracer.shutdown();
            let _ = self.logger.shutdown();
            // The providers live on (see `build_with_writer`), and their
            // processors hold the exporters: empty them, so a kept provider
            // holds no test's spans or records.
            self.span_exporter.reset();
            self.log_exporter.reset();
            observed
        }
    }

    /// Builds the subscriber with in-memory exporters, runs `body` under it
    /// as the thread's default, then shuts the providers down.
    pub(crate) fn observe(
        stderr: &str,
        traces: &str,
        logs: &str,
        format: super::LogFormat,
        body: impl FnOnce(),
    ) -> Observed {
        let observer = Observer::start(stderr, traces, logs, format);
        body();
        observer.finish()
    }
}

#[cfg(test)]
mod tests {
    use opentelemetry::trace::Status;

    use super::capture::{observe, Capture};
    use super::*;

    #[test]
    fn stderr_at_warn_still_exports_debug_spans() {
        let observed = observe("warn", "debug", "info", LogFormat::Text, || {
            let span = tracing::debug_span!(target: "okf_mcp::quiet", "debug_work");
            let _entered = span.enter();
            tracing::debug!(target: "okf_mcp::quiet", "hidden-debug-line");
        });
        assert!(
            observed.spans.iter().any(|span| span.name == "debug_work"),
            "the debug span reaches the exporter"
        );
        assert!(
            !observed.stderr.contains("hidden-debug-line"),
            "{}",
            observed.stderr
        );
        assert!(
            !observed.stderr.contains("debug_work"),
            "{}",
            observed.stderr
        );
    }

    #[test]
    fn rmcp_debug_never_reaches_any_layer() {
        let observed = observe("trace", "trace", "trace", LogFormat::Text, || {
            let span = tracing::trace_span!(target: "rmcp", "rmcp_span");
            let _entered = span.enter();
            tracing::debug!(target: "rmcp", "rmcp-debug-line");
            tracing::trace!(target: "rmcp", "rmcp-trace-line");
            tracing::info!(target: "rmcp", "rmcp-info-line");
        });
        assert!(
            !observed.stderr.contains("rmcp-debug-line"),
            "{}",
            observed.stderr
        );
        assert!(
            !observed.stderr.contains("rmcp-trace-line"),
            "{}",
            observed.stderr
        );
        // At `info` stderr still shows `rmcp`; the OTLP layers take nothing from it.
        assert!(
            observed.stderr.contains("rmcp-info-line"),
            "{}",
            observed.stderr
        );
        assert!(
            observed.logs.iter().all(|log| !log.contains("rmcp")),
            "{:?}",
            observed.logs
        );
        assert!(observed.spans.is_empty(), "{:?}", observed.spans);
    }

    #[test]
    fn export_stack_events_reach_stderr_only() {
        let observed = observe("trace", "trace", "trace", LogFormat::Text, || {
            tracing::error!(target: "opentelemetry-otlp", "export-failed-line");
            tracing::error!(target: "opentelemetry_sdk", "sdk-failed-line");
            tracing::error!(target: "reqwest", "reqwest-failed-line");
        });
        for line in [
            "export-failed-line",
            "sdk-failed-line",
            "reqwest-failed-line",
        ] {
            assert!(
                observed.stderr.contains(line),
                "{line}: {}",
                observed.stderr
            );
        }
        assert!(observed.logs.is_empty(), "{:?}", observed.logs);
    }

    /// `LOCAL_ONLY` is a sub-target of `okf_mcp`; the cap's most-specific
    /// match must still turn it off.
    #[test]
    fn a_split_event_exports_the_closed_half_and_prints_both() {
        let observed = observe("trace", "trace", "trace", LogFormat::Text, || {
            split_event!(
                warn,
                { kind = "Io", },
                { error = %"SENTINEL-DETAIL", },
                "request failed"
            );
        });
        let exported = observed.exported_text();
        assert!(exported.contains("request failed"), "{exported}");
        assert!(exported.contains("Io"), "{exported}");
        assert!(!exported.contains("SENTINEL-DETAIL"), "{exported}");
        assert!(!exported.contains("(detail)"), "{exported}");
        assert!(observed.stderr.contains("request failed (detail)"));
        assert!(observed.stderr.contains("SENTINEL-DETAIL"));
    }

    #[test]
    fn local_only_events_reach_stderr_only() {
        let observed = observe("trace", "trace", "trace", LogFormat::Text, || {
            let span = tracing::info_span!(target: "okf_mcp::test", "outer");
            let _entered = span.enter();
            tracing::error!(target: LOCAL_ONLY, "local-error-line");
            tracing::warn!(target: LOCAL_ONLY, "local-warn-line");
            tracing::info!(target: LOCAL_ONLY, "local-info-line");
            tracing::debug!(target: LOCAL_ONLY, "local-debug-line");
            tracing::trace!(target: LOCAL_ONLY, "local-trace-line");
        });
        for level in ["error", "warn", "info", "debug", "trace"] {
            let line = format!("local-{level}-line");
            assert!(
                observed.stderr.contains(&line),
                "{line}: {}",
                observed.stderr
            );
        }
        assert!(observed.logs.is_empty(), "{:?}", observed.logs);
        let outer = observed
            .spans
            .iter()
            .find(|span| span.name == "outer")
            .expect("outer span");
        assert!(outer.events.is_empty(), "no span event: {:?}", outer.events);
        assert_eq!(
            outer.status,
            Status::Unset,
            "a local-only error does not mark the span"
        );
    }

    #[test]
    fn a_target_that_merely_starts_with_okf_mcp_is_not_exported() {
        assert!(is_exportable_target("okf_mcp"));
        assert!(is_exportable_target("okf_mcp::store::publish"));
        for target in [
            "okf_mcp_other",
            "okf_mcpx::a",
            "okf_mcp-other",
            LOCAL_ONLY,
            "okf_mcp::local_only::x",
        ] {
            assert!(!is_exportable_target(target), "{target}");
        }
        let observed = observe("trace", "trace", "trace", LogFormat::Text, || {
            let span = tracing::info_span!(target: "okf_mcp_other", "other_span");
            let _entered = span.enter();
            tracing::error!(target: "okf_mcp_other", "other-event");
            tracing::error!(target: "okf_mcp", "root-event");
        });
        assert!(observed.spans.iter().all(|span| span.name != "other_span"));
        assert!(
            observed.logs.iter().all(|log| !log.contains("other-event")),
            "{:?}",
            observed.logs
        );
        assert!(
            observed.logs.iter().any(|log| log.contains("root-event")),
            "{:?}",
            observed.logs
        );
        assert!(
            observed.stderr.contains("other-event"),
            "{}",
            observed.stderr
        );
    }

    #[test]
    fn an_exported_log_carries_the_enclosing_spans_ids() {
        let observed = observe("warn", "info", "info", LogFormat::Text, || {
            let span = tracing::info_span!(target: "okf_mcp::test", "parent");
            let _entered = span.enter();
            tracing::info!(target: "okf_mcp::test", "correlated");
        });
        let span = observed
            .spans
            .iter()
            .find(|span| span.name == "parent")
            .expect("span");
        let context = observed
            .records
            .iter()
            .find_map(|record| record.trace_context())
            .expect("the log record has a trace context");
        assert_eq!(context.trace_id, span.span_context.trace_id());
        assert_eq!(context.span_id, span.span_context.span_id());
    }

    /// Emits an event at every level and a span, all with `$target`.
    macro_rules! emit_everywhere {
        ($target:literal) => {{
            let span = tracing::info_span!(target: $target, "dep_span");
            let _entered = span.enter();
            tracing::error!(target: $target, "dep-event");
            tracing::warn!(target: $target, "dep-event");
            tracing::info!(target: $target, "dep-event");
            tracing::debug!(target: $target, "dep-event");
            tracing::trace!(target: $target, "dep-event");
        }};
    }

    #[test]
    fn dependency_events_never_reach_otlp() {
        let observed = observe("trace", "trace", "trace", LogFormat::Text, || {
            emit_everywhere!("sqlx::query");
            emit_everywhere!("apalis");
            emit_everywhere!("openidconnect");
            emit_everywhere!("aws_config");
            emit_everywhere!("tower_http");
            emit_everywhere!("rmcp");
            tracing::info!(target: "okf_mcp::store", "own-event");
            let span = tracing::info_span!(target: "okf_mcp::store", "own_span");
            drop(span);
        });
        assert!(
            observed.logs.iter().all(|log| !log.contains("dep-event")),
            "{:?}",
            observed.logs
        );
        assert!(
            observed.logs.iter().any(|log| log.contains("own-event")),
            "{:?}",
            observed.logs
        );
        assert!(observed.spans.iter().all(|span| span.name != "dep_span"));
        assert!(observed.spans.iter().any(|span| span.name == "own_span"));
        // stderr still sees the dependencies (rmcp capped at info).
        assert!(observed.stderr.contains("dep-event"), "{}", observed.stderr);
    }

    #[test]
    fn error_event_marks_span_error() {
        let observed = observe("warn", "info", "info", LogFormat::Text, || {
            let span = tracing::info_span!(target: "okf_mcp::test", "failing");
            let _entered = span.enter();
            tracing::error!(target: "okf_mcp::test", "it failed");
        });
        let span = observed
            .spans
            .iter()
            .find(|span| span.name == "failing")
            .expect("span");
        // The status is Error with no description, and no error text became
        // an exception attribute: the privacy rules keep error text out.
        assert_eq!(span.status, Status::error(""));
        assert!(
            !span
                .attributes
                .iter()
                .any(|kv| kv.key.as_str().starts_with("exception")),
            "{:?}",
            span.attributes
        );
    }

    #[test]
    fn settings_prefer_rust_log_then_config_then_default() {
        let mut config = TelemetryConfig::default();
        config.stderr.filter = Some("debug".to_string());
        let from_env = settings(&config, Some("error")).expect("settings");
        assert_eq!(from_env.stderr.to_string(), "error");
        let from_config = settings(&config, Some("=[")).expect("invalid RUST_LOG falls back");
        assert_eq!(from_config.stderr.to_string(), "debug");
        let blank = settings(&config, Some("  ")).expect("blank RUST_LOG falls back");
        assert_eq!(blank.stderr.to_string(), "debug");
        config.stderr.filter = None;
        let default = settings(&config, None).expect("settings");
        assert_eq!(
            default.stderr.to_string(),
            parse_filter(DEFAULT_STDERR_FILTER).unwrap().to_string()
        );
        assert_eq!(default.traces.to_string(), "info");
    }

    #[test]
    fn the_fallback_writes_text_at_the_default_filter() {
        let capture = Capture::default();
        let subscriber = fallback(None, capture.clone());
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "okf_mcp::test", "fallback-info-line");
            tracing::debug!(target: "okf_mcp::test", "fallback-debug-line");
            tracing::info!(target: "somewhere_else", "fallback-dependency-info");
        });
        let text = capture.text();
        assert!(text.contains("fallback-info-line"), "{text}");
        assert!(!text.contains("fallback-debug-line"), "{text}");
        assert!(!text.contains("fallback-dependency-info"), "{text}");
    }

    /// Issue #738: dropping a subscriber never drops the last handle on its
    /// providers (whose `Drop` emits an event, which deadlocks when the
    /// subscriber is dropped inside the dispatcher registry's lock). Each
    /// provider here has a processor that notes being dropped, which happens
    /// only when the provider itself is.
    #[test]
    fn dropping_a_subscriber_never_drops_its_providers() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use std::time::Duration;

        use opentelemetry_sdk::error::OTelSdkResult;

        #[derive(Debug)]
        struct NotesDrop(Arc<AtomicBool>);

        impl Drop for NotesDrop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        impl opentelemetry_sdk::trace::SpanProcessor for NotesDrop {
            fn on_start(&self, _: &mut opentelemetry_sdk::trace::Span, _: &opentelemetry::Context) {
            }
            fn on_end(&self, _: opentelemetry_sdk::trace::SpanData) {}
            fn force_flush(&self) -> OTelSdkResult {
                Ok(())
            }
            fn shutdown_with_timeout(&self, _: Duration) -> OTelSdkResult {
                Ok(())
            }
        }

        impl opentelemetry_sdk::logs::LogProcessor for NotesDrop {
            fn emit(
                &self,
                _: &mut opentelemetry_sdk::logs::SdkLogRecord,
                _: &opentelemetry::InstrumentationScope,
            ) {
            }
            fn force_flush(&self) -> OTelSdkResult {
                Ok(())
            }
        }

        let tracer_dropped = Arc::new(AtomicBool::new(false));
        let logger_dropped = Arc::new(AtomicBool::new(false));
        let tracer = SdkTracerProvider::builder()
            .with_span_processor(NotesDrop(Arc::clone(&tracer_dropped)))
            .build();
        let logger = SdkLoggerProvider::builder()
            .with_log_processor(NotesDrop(Arc::clone(&logger_dropped)))
            .build();
        let settings = LayerSettings {
            format: LogFormat::Text,
            stderr: parse_filter("off").expect("filter"),
            traces: parse_filter("info").expect("filter"),
            logs: parse_filter("info").expect("filter"),
        };
        let subscriber =
            build_with_writer(settings, Some(&tracer), Some(&logger), Capture::default());
        // The test's own handles go first, as when a capture is dropped
        // without `finish`: the subscriber is then the last holder in sight.
        drop(tracer);
        drop(logger);
        drop(subscriber);
        assert!(
            !tracer_dropped.load(Ordering::SeqCst),
            "dropping the subscriber dropped its tracer provider"
        );
        assert!(
            !logger_dropped.load(Ordering::SeqCst),
            "dropping the subscriber dropped its logger provider"
        );
    }

    /// A callsite that another thread reached first, with no subscriber of
    /// its own, still records for an `Observer` started afterwards or while
    /// the other thread runs (issues #481, #836): `test_tracing`'s anchor
    /// keeps the answer computed from every live subscriber.
    #[test]
    fn a_callsite_another_thread_reached_first_still_records() {
        use super::capture::Observer;

        fn work(label: &'static str) {
            let _ =
                tracing::info_span!(target: "okf_mcp::first_elsewhere", "first_elsewhere", label)
                    .entered();
        }
        // The other thread registers the callsite with no default subscriber.
        std::thread::spawn(|| work("before"))
            .join()
            .expect("thread");

        let observer = Observer::start("off", "trace", "off", LogFormat::Text);
        std::thread::spawn(|| work("during"))
            .join()
            .expect("thread");
        work("here");
        let observed = observer.finish();

        let seen: Vec<_> = observed
            .spans
            .iter()
            .map(|span| span.name.as_ref())
            .collect();
        assert_eq!(seen, ["first_elsewhere"], "captured {seen:?}");
    }

    /// Issue #738: captures made and dropped on many threads at once, some
    /// dropped without `finish` (so the subscriber holds the last handle on
    /// its providers), while other threads install plain scoped subscribers
    /// and rebuild the interest cache, all finish. A deadlock in
    /// `tracing_core`'s dispatcher registry would hang every thread that
    /// touches it, the rest of the test binary included, so on a timeout
    /// this aborts the process (a failure the gate reports) instead of
    /// panicking into a binary that then hangs until the gate's timeout.
    #[test]
    fn captures_made_and_dropped_on_many_threads_do_not_deadlock() {
        use std::io::Write as _;
        use std::sync::mpsc::{self, RecvTimeoutError};
        use std::sync::{Arc, Barrier};
        use std::time::{Duration, Instant};

        use super::capture::Observer;

        const THREADS: usize = 16;
        const ROUNDS: usize = 30;
        let barrier = Arc::new(Barrier::new(THREADS));
        let (done, finished) = mpsc::channel();
        for thread in 0..THREADS {
            let barrier = Arc::clone(&barrier);
            let done = done.clone();
            std::thread::spawn(move || {
                barrier.wait();
                for round in 0..ROUNDS {
                    match (thread + round) % 3 {
                        0 => drop(Observer::start("off", "trace", "trace", LogFormat::Text)),
                        1 => {
                            let observer =
                                Observer::start("off", "trace", "trace", LogFormat::Text);
                            tracing::info!(target: "okf_mcp::test", "inside a capture");
                            let _ = observer.finish();
                        }
                        _ => {
                            let _guard =
                                tracing::subscriber::set_default(tracing_subscriber::registry());
                            tracing::callsite::rebuild_interest_cache();
                        }
                    }
                }
                let _ = done.send(());
            });
        }
        drop(done);
        let deadline = Instant::now() + Duration::from_secs(120);
        for _ in 0..THREADS {
            match finished.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(()) => {}
                Err(RecvTimeoutError::Timeout) => {
                    // Straight to the process's stderr: the test harness
                    // captures `eprintln!` and would lose it in the abort.
                    let _ = writeln!(
                        std::io::stderr(),
                        "captures_made_and_dropped_on_many_threads_do_not_deadlock: \
                         a thread did not finish within 120 s; the dispatcher \
                         registry is deadlocked (issue #738). Aborting."
                    );
                    std::process::abort();
                }
                Err(RecvTimeoutError::Disconnected) => panic!("a capture thread panicked"),
            }
        }
    }
}
