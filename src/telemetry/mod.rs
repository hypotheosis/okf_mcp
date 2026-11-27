//! Observability for `serve`: the OTLP span and log providers, the
//! subscriber that feeds them and stderr, the meter provider behind both OTLP
//! metrics and `/metrics`, and shutdown.
//!
//! Everything else in the crate uses `tracing` macros only. Telemetry starts
//! for `serve` alone; the other subcommands get the stderr layer (see
//! [`install`]).

pub mod config;
pub mod json;
pub mod metrics;
pub mod otlp;
pub mod propagation;
pub mod scrape;
pub mod subscriber;

use std::path::Path;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use opentelemetry::global;
use opentelemetry::KeyValue;
use opentelemetry_sdk::error::OTelSdkError;
use opentelemetry_sdk::logs::log_processor_with_async_runtime::BatchLogProcessor;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::periodic_reader_with_async_runtime::PeriodicReader;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::runtime;
use opentelemetry_sdk::trace::span_processor_with_async_runtime::BatchSpanProcessor;
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};
use opentelemetry_sdk::Resource;
use tracing_subscriber::fmt::writer::MakeWriter;

use crate::config::Config;
use crate::error::OkfError;
use config::TelemetryConfig;
use otlp::{ExportFailures, OtlpTarget};
use subscriber::{split_event, LayerSettings, LOCAL_ONLY};

/// Logs that a signal's exporter could not be built, so that signal is off.
/// The error's text can carry the endpoint, so only stderr gets it (the
/// exported event keeps the signal's name).
///
/// # Arguments
/// * `signal` - `"traces"`, `"logs"` or `"metrics"`.
/// * `error` - what the exporter builder said.
#[tracing::instrument(level = "trace", skip_all)]
fn warn_provider_off(signal: &'static str, error: &dyn std::fmt::Display) {
    tracing::warn!(provider = signal, "OTLP export is off for this signal");
    tracing::warn!(
        target: LOCAL_ONLY,
        provider = signal,
        %error,
        "OTLP export is off for this signal (detail)"
    );
}

/// The sampler `telemetry.otlp.sample_ratio` selects: a trace that starts
/// here is kept with probability `ratio`, and a trace that arrives with a
/// sampled (or unsampled) parent follows its parent. A ratio of `0.0` keeps
/// no new trace and `1.0` keeps every one.
///
/// # Arguments
/// * `ratio` - the validated ratio, `0.0..=1.0`.
#[tracing::instrument(level = "trace", skip_all)]
fn sampler_for(ratio: f64) -> Sampler {
    Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(ratio)))
}

/// Warns that `telemetry.otlp.endpoint` already ends in `/v1/...`, so the
/// exporters would post to `/v1/.../v1/<signal>`. It is a warning, not an
/// error: a receiver can serve such a path. The endpoint is logged with
/// userinfo, query and fragment removed.
///
/// # Arguments
/// * `endpoint` - `telemetry.otlp.endpoint`.
#[tracing::instrument(level = "trace", skip_all)]
fn warn_if_endpoint_has_signal_path(endpoint: &str) {
    if let Some(sanitized) = config::endpoint_with_signal_path(endpoint) {
        tracing::warn!(
            endpoint = %sanitized,
            "telemetry.otlp.endpoint should be a base URL: /v1/<signal> is appended to it"
        );
    }
}

/// Logs that a shutdown thread did not start. The I/O error's text stays on
/// stderr; the exported event keeps the provider's name.
///
/// # Arguments
/// * `provider` - `"traces"` or `"logs"`.
/// * `error` - why the thread did not start.
#[tracing::instrument(level = "trace", skip_all)]
fn warn_thread_not_started(provider: &'static str, error: &std::io::Error) {
    split_event!(
        warn,
        { provider, },
        { %error, },
        "telemetry shutdown thread did not start"
    );
}

/// How long shutdown waits for the last flushes (inside Kubernetes' default
/// 30 s grace, after the HTTP drain).
pub const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(4);

/// How one provider fared at shutdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderOutcome {
    /// The provider was never started (that signal is off).
    NotRunning,
    /// Flushed and shut down (or had already been).
    Flushed,
    /// The flush failed, with the SDK's message (which carries no payload).
    Failed(String),
    /// Still running at the deadline, and left behind.
    Abandoned,
}

/// What [`Telemetry::shutdown`] did, one outcome per provider. Task 7's
/// sentinel test asserts on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownReport {
    /// The span provider.
    pub traces: ProviderOutcome,
    /// The log provider.
    pub logs: ProviderOutcome,
    /// The meter provider (the `/metrics` reader, and the OTLP metric reader
    /// when OTLP is on).
    pub metrics: ProviderOutcome,
}

impl ShutdownReport {
    /// `true` when every provider that was running flushed.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn all_flushed(&self) -> bool {
        [&self.traces, &self.logs, &self.metrics]
            .iter()
            .all(|outcome| {
                matches!(
                    outcome,
                    ProviderOutcome::NotRunning | ProviderOutcome::Flushed
                )
            })
    }
}

/// The running telemetry of `serve`: the providers, kept for shutdown.
#[derive(Debug)]
pub struct Telemetry {
    tracer: Option<SdkTracerProvider>,
    logger: Option<SdkLoggerProvider>,
    // Always present: `/metrics` needs it with or without OTLP.
    meter: SdkMeterProvider,
    // What the Prometheus reader renders into; `scrape::serve` reads it.
    registry: prometheus::Registry,
    failures: Arc<ExportFailures>,
    // The signals with a running OTLP exporter, for the start-up line.
    otlp_signals: Vec<otlp::Signal>,
}

/// The resource every signal carries.
///
/// `Resource::builder` runs the SDK's detectors, so `OTEL_RESOURCE_ATTRIBUTES`
/// adds keys such as `k8s.pod.name`; what is set here in code overrides the
/// same key from the environment.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "runs before the subscriber exists (configuration, builder or start-up), so a span would be recorded nowhere"
    )
)]
fn resource(config: &TelemetryConfig) -> Resource {
    let mut attributes = vec![KeyValue::new("service.version", env!("CARGO_PKG_VERSION"))];
    if let Some(environment) = &config.environment {
        attributes.push(KeyValue::new(
            "deployment.environment.name",
            environment.clone(),
        ));
    }
    Resource::builder()
        .with_service_name(config.service_name.clone())
        .with_attributes(attributes)
        .build()
}

impl Telemetry {
    /// Builds the resource, the exporters and the providers for `serve`, sets
    /// the global tracer provider and the W3C propagator. An exporter that
    /// cannot be built is logged (to stderr) and its signal stays off.
    ///
    /// Must run inside a tokio runtime: the batch processors spawn their
    /// worker on it.
    ///
    /// # Arguments
    /// * `config` - `[telemetry]`, already validated.
    /// * `env` - reads an environment variable (injected for tests).
    ///
    /// # Returns
    /// The running telemetry; with no `[telemetry.otlp]` (or
    /// `OTEL_SDK_DISABLED=true`) it holds no tracer or logger, but always the
    /// meter provider and the registry `/metrics` renders.
    ///
    /// # Errors
    /// [`OkfError::Config`] when a variable named by `headers_env` or
    /// `sentry_dsn_env` is unset or blank, or its value does not parse.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "runs before the subscriber exists (configuration, builder or start-up), so a span would be recorded nowhere"
        )
    )]
    pub fn start(
        config: &TelemetryConfig,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, OkfError> {
        let failures = Arc::new(ExportFailures::default());
        let resource = resource(config);
        // Before the target is resolved: a disabled SDK never needs the
        // credentials, so removing them must not stop the service starting.
        let sdk_disabled =
            env("OTEL_SDK_DISABLED").is_some_and(|value| value.trim().eq_ignore_ascii_case("true"));
        let otlp = match &config.otlp {
            Some(_) if sdk_disabled => {
                tracing::info!("OTLP export is off: OTEL_SDK_DISABLED is true");
                None
            }
            Some(otlp) => {
                if let Some(endpoint) = &otlp.endpoint {
                    warn_if_endpoint_has_signal_path(endpoint);
                }
                Some((otlp, OtlpTarget::resolve(otlp, env)?))
            }
            None => None,
        };

        let mut tracer = None;
        let mut logger = None;
        // A signal that is off builds nothing: no exporter, no provider.
        if let Some((otlp, target)) = otlp
            .as_ref()
            .filter(|(otlp, _)| otlp.exports(otlp::Signal::Traces))
        {
            tracer = match otlp::span_exporter(target, &failures) {
                Ok(exporter) => {
                    let processor = BatchSpanProcessor::builder(exporter, runtime::Tokio).build();
                    let mut builder = SdkTracerProvider::builder()
                        .with_resource(resource.clone())
                        .with_span_processor(processor);
                    if let Some(ratio) = otlp.sample_ratio {
                        builder = builder.with_sampler(sampler_for(ratio));
                    }
                    Some(builder.build())
                }
                Err(error) => {
                    warn_provider_off("traces", &error);
                    None
                }
            };
        }
        if let Some((_, target)) = otlp
            .as_ref()
            .filter(|(otlp, _)| otlp.exports(otlp::Signal::Logs))
        {
            logger = match otlp::log_exporter(target, &failures) {
                Ok(exporter) => {
                    let processor = BatchLogProcessor::builder(exporter, runtime::Tokio).build();
                    Some(
                        SdkLoggerProvider::builder()
                            .with_resource(resource.clone())
                            .with_log_processor(processor)
                            .build(),
                    )
                }
                Err(error) => {
                    warn_provider_off("logs", &error);
                    None
                }
            };
        }

        let registry = prometheus::Registry::new();
        let metrics_otlp = otlp
            .as_ref()
            .filter(|(otlp, _)| otlp.exports(otlp::Signal::Metrics));
        let (meter, metrics_pushed) =
            build_meter_provider(resource, &registry, metrics_otlp, &failures)?;
        let otlp_signals = [
            (otlp::Signal::Traces, tracer.is_some()),
            (otlp::Signal::Logs, logger.is_some()),
            (otlp::Signal::Metrics, metrics_pushed),
        ]
        .into_iter()
        .filter_map(|(signal, running)| running.then_some(signal))
        .collect();
        metrics::observe_export_failures(
            &opentelemetry::metrics::MeterProvider::meter(&meter, "okf-mcp"),
            Arc::clone(&failures),
        );
        // Before `App::load`: its instruments are built on the global meter.
        global::set_meter_provider(meter.clone());
        if let Some(provider) = &tracer {
            global::set_tracer_provider(provider.clone());
            global::set_text_map_propagator(TraceContextPropagator::new());
        }
        Ok(Self {
            tracer,
            logger,
            meter,
            registry,
            failures,
            otlp_signals,
        })
    }

    /// Wraps providers that were built elsewhere (tests with in-memory
    /// exporters, or a hung one).
    #[cfg(test)]
    fn from_providers(
        tracer: Option<SdkTracerProvider>,
        logger: Option<SdkLoggerProvider>,
    ) -> Self {
        Self {
            tracer,
            logger,
            meter: SdkMeterProvider::builder().build(),
            registry: prometheus::Registry::new(),
            failures: Arc::new(ExportFailures::default()),
            otlp_signals: Vec::new(),
        }
    }

    /// Whether OTLP spans are being exported.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn traces_enabled(&self) -> bool {
        self.tracer.is_some()
    }

    /// Whether OTLP logs are being exported.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn logs_enabled(&self) -> bool {
        self.logger.is_some()
    }

    /// The signals with a running OTLP exporter, in [`otlp::Signal::ALL`]
    /// order; empty when OTLP is off.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "one-line accessor")
    )]
    pub fn otlp_signals(&self) -> &[otlp::Signal] {
        &self.otlp_signals
    }

    /// The registry `/metrics` renders: the Prometheus reader writes into it.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "one-line accessor")
    )]
    pub fn registry(&self) -> prometheus::Registry {
        self.registry.clone()
    }

    /// The failed-export counts the exporters keep.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn failures(&self) -> &Arc<ExportFailures> {
        &self.failures
    }

    /// The subscriber for `serve`: stderr plus whichever OTLP layers started.
    ///
    /// # Arguments
    /// * `settings` - the resolved filters and format.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn subscriber(&self, settings: LayerSettings) -> impl tracing::Subscriber + Send + Sync {
        subscriber::build(settings, self.tracer.as_ref(), self.logger.as_ref())
    }

    /// Flushes and shuts down every provider in parallel, each on a plain
    /// thread, within [`SHUTDOWN_DEADLINE`]. A flush still running at the
    /// deadline is abandoned and named on stderr.
    ///
    /// # Returns
    /// Each provider's outcome. A provider that was already shut down counts
    /// as flushed, and says nothing.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "exporter or its start-up helper: a span here feeds back into the exporter, or runs before the subscriber exists"
        )
    )]
    pub async fn shutdown(self) -> ShutdownReport {
        self.shutdown_within(SHUTDOWN_DEADLINE).await
    }

    /// [`Telemetry::shutdown`] with the deadline as a parameter.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "exporter or its start-up helper: a span here feeds back into the exporter, or runs before the subscriber exists"
        )
    )]
    async fn shutdown_within(self, deadline: Duration) -> ShutdownReport {
        let Self {
            tracer,
            logger,
            meter,
            ..
        } = self;
        // `spawn_blocking` moves the waiting off the async workers: it blocks
        // on a channel until the providers answer or the deadline passes.
        tokio::task::spawn_blocking(move || shutdown_blocking(tracer, logger, meter, deadline))
            .await
            .unwrap_or_else(|_| ShutdownReport {
                traces: ProviderOutcome::Failed("the shutdown task failed".to_string()),
                logs: ProviderOutcome::Failed("the shutdown task failed".to_string()),
                metrics: ProviderOutcome::Failed("the shutdown task failed".to_string()),
            })
    }
}

/// Builds the meter provider: the Prometheus reader always, and the OTLP
/// periodic reader when OTLP is on and its exporter builds.
///
/// # Arguments
/// * `resource` - what OTLP metrics carry (`/metrics` renders no resource).
/// * `registry` - what the Prometheus reader renders into.
/// * `otlp` - the resolved OTLP settings, when OTLP is on.
/// * `failures` - the counts the metric exporter adds to.
///
/// # Returns
/// The provider, and whether an OTLP periodic reader was added.
///
/// # Errors
/// [`OkfError::Config`] when the Prometheus reader cannot be built.
#[tracing::instrument(level = "trace", skip_all)]
fn build_meter_provider(
    resource: Resource,
    registry: &prometheus::Registry,
    otlp: Option<&(&config::OtlpConfig, OtlpTarget)>,
    failures: &Arc<ExportFailures>,
) -> Result<(SdkMeterProvider, bool), OkfError> {
    let reader = opentelemetry_prometheus::exporter()
        .with_registry(registry.clone())
        // No `target_info` series and no `otel_scope_*` labels: every series
        // carries only the labels the typed handle gives it.
        .without_target_info()
        .scope_info_enabled(false)
        .build()
        .map_err(|error| OkfError::Config {
            path: "[telemetry.prometheus]".to_string(),
            message: format!("the Prometheus reader: {error}"),
        })?;
    let mut builder = SdkMeterProvider::builder()
        .with_resource(resource)
        .with_reader(reader);
    let mut pushed = false;
    if let Some((otlp, target)) = otlp {
        match otlp::metric_exporter(target, failures) {
            Ok(exporter) => {
                builder = builder.with_reader(
                    PeriodicReader::builder(exporter, runtime::Tokio)
                        .with_interval(otlp.export_interval)
                        .build(),
                );
                pushed = true;
            }
            Err(error) => warn_provider_off("metrics", &error),
        }
    }
    Ok((builder.build(), pushed))
}

/// Shuts the providers down, each on its own thread, and waits for them
/// until `deadline`.
///
/// Plain threads, not tasks: the tokio batch processor ignores the timeout
/// given to `shutdown_with_timeout`, and a dead receiver costs several retry
/// timeouts.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "exporter or its start-up helper: a span here feeds back into the exporter, or runs before the subscriber exists"
    )
)]
fn shutdown_blocking(
    tracer: Option<SdkTracerProvider>,
    logger: Option<SdkLoggerProvider>,
    meter: SdkMeterProvider,
    deadline: Duration,
) -> ShutdownReport {
    /// Which provider a finished thread was.
    #[derive(Clone, Copy, PartialEq)]
    enum Which {
        Traces,
        Logs,
        Metrics,
    }
    let (sender, receiver) = mpsc::channel::<(Which, Result<(), OTelSdkError>)>();
    let mut pending = Vec::new();
    let mut spawn =
        |which: Which,
         name: &'static str,
         work: Box<dyn FnOnce() -> Result<(), OTelSdkError> + Send>| {
            let sender = sender.clone();
            let started = std::thread::Builder::new()
                .name(format!("telemetry-shutdown-{name}"))
                .spawn(move || {
                    // A closed channel means the deadline passed; nobody is listening.
                    let _ = sender.send((which, work()));
                });
            match started {
                Ok(_) => pending.push((which, name)),
                Err(error) => warn_thread_not_started(name, &error),
            }
        };
    if let Some(provider) = tracer {
        spawn(
            Which::Traces,
            "traces",
            Box::new(move || provider.shutdown()),
        );
    }
    if let Some(provider) = logger {
        spawn(Which::Logs, "logs", Box::new(move || provider.shutdown()));
    }
    spawn(
        Which::Metrics,
        "metrics",
        Box::new(move || meter.shutdown()),
    );
    drop(sender);

    let mut outcomes: Vec<(Which, ProviderOutcome)> = Vec::new();
    let started = Instant::now();
    while outcomes.len() < pending.len() {
        let remaining = deadline.saturating_sub(started.elapsed());
        match receiver.recv_timeout(remaining) {
            Ok((which, result)) => outcomes.push((
                which,
                match result {
                    Ok(()) | Err(OTelSdkError::AlreadyShutdown) => ProviderOutcome::Flushed,
                    Err(error) => ProviderOutcome::Failed(error.to_string()),
                },
            )),
            Err(_) => break,
        }
    }
    let outcome = |which: Which, name: &str, was_started: bool| {
        if let Some((_, outcome)) = outcomes.iter().find(|(found, _)| *found == which) {
            return outcome.clone();
        }
        if !was_started {
            return ProviderOutcome::NotRunning;
        }
        // It reaches stderr: the stderr layer is still installed.
        tracing::warn!(provider = name, "telemetry flush abandoned at shutdown");
        ProviderOutcome::Abandoned
    };
    let started_with = |which: Which| pending.iter().any(|(started, _)| *started == which);
    ShutdownReport {
        traces: outcome(Which::Traces, "traces", started_with(Which::Traces)),
        logs: outcome(Which::Logs, "logs", started_with(Which::Logs)),
        metrics: outcome(Which::Metrics, "metrics", started_with(Which::Metrics)),
    }
}

/// The configuration and, for `serve`, the telemetry [`start_up`] produced.
#[derive(Debug)]
pub struct StartUp {
    /// The loaded and validated configuration.
    pub config: Config,
    /// Telemetry, for `serve` only.
    pub telemetry: Option<Telemetry>,
}

/// Loads the configuration and, for `serve`, starts telemetry, under a
/// temporary stderr subscriber.
///
/// The subscriber is a thread-local default (`set_default`), scoped to this
/// function: its guard is dropped before the caller calls [`install`].
/// `set_global_default` does not override a thread-local default that is
/// still active, so a kept guard would leave the start-up thread (and every
/// future polled on it) on the fallback, without OTLP.
///
/// # Arguments
/// * `path` - the `--config` file, if any.
/// * `serve` - whether to start telemetry.
/// * `env` - reads an environment variable.
/// * `writer` - where the temporary fallback subscriber writes (stderr).
///
/// # Errors
/// [`OkfError::Config`] from [`Config::load`] or [`Telemetry::start`].
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "runs before the subscriber exists (configuration, builder or start-up), so a span would be recorded nowhere"
    )
)]
pub fn start_up<W>(
    path: Option<&Path>,
    serve: bool,
    env: &dyn Fn(&str) -> Option<String>,
    writer: W,
) -> Result<StartUp, OkfError>
where
    W: for<'writer> MakeWriter<'writer> + Send + Sync + 'static,
{
    let rust_log = env("RUST_LOG");
    let _startup =
        tracing::subscriber::set_default(subscriber::fallback(rust_log.as_deref(), writer));
    let config = Config::load(path)?;
    let telemetry = serve
        .then(|| Telemetry::start(&config.telemetry, env))
        .transpose()?;
    Ok(StartUp { config, telemetry })
    // `_startup` drops here.
}

/// Installs the process's subscriber: stderr alone, or with the OTLP layers
/// of `telemetry`.
///
/// # Arguments
/// * `config` - `[telemetry]`.
/// * `telemetry` - `serve`'s running telemetry, or `None` for every other
///   command.
/// * `rust_log` - the value of `RUST_LOG`, if set.
///
/// # Errors
/// [`OkfError::Config`] when a configured filter does not parse or a
/// subscriber is already installed.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "runs before the subscriber exists (configuration, builder or start-up), so a span would be recorded nowhere"
    )
)]
pub fn install(
    config: &TelemetryConfig,
    telemetry: Option<&Telemetry>,
    rust_log: Option<&str>,
) -> Result<(), OkfError> {
    install_with_writer(config, telemetry, rust_log, std::io::stderr)
}

/// [`install`] with stderr's writer as a parameter, for tests.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "runs before the subscriber exists (configuration, builder or start-up), so a span would be recorded nowhere"
    )
)]
pub fn install_with_writer<W>(
    config: &TelemetryConfig,
    telemetry: Option<&Telemetry>,
    rust_log: Option<&str>,
    writer: W,
) -> Result<(), OkfError>
where
    W: for<'writer> MakeWriter<'writer> + Send + Sync + 'static,
{
    let error = |message: String| OkfError::Config {
        path: "[telemetry]".to_string(),
        message,
    };
    let settings = subscriber::settings(config, rust_log).map_err(error)?;
    let (tracer, logger) = match telemetry {
        Some(telemetry) => (telemetry.tracer.as_ref(), telemetry.logger.as_ref()),
        None => (None, None),
    };
    let subscriber = subscriber::build_with_writer(settings, tracer, logger, writer);
    tracing::subscriber::set_global_default(subscriber)
        .map_err(|_| error("a tracing subscriber is already installed".to_string()))?;
    if let Some(telemetry) = telemetry.filter(|telemetry| !telemetry.otlp_signals.is_empty()) {
        tracing::info!(signals = %otlp_signals_text(&telemetry.otlp_signals), "OTLP export is on");
    }
    Ok(())
}

/// The exported signals as `traces,logs`, for the start-up line.
#[tracing::instrument(level = "trace", skip_all)]
fn otlp_signals_text(signals: &[otlp::Signal]) -> String {
    signals
        .iter()
        .map(|signal| signal.label())
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use async_trait_free::HangingSpanExporter;

    use super::*;

    /// A span exporter whose `export` never completes.
    mod async_trait_free {
        use opentelemetry_sdk::error::OTelSdkResult;
        use opentelemetry_sdk::trace::{SpanData, SpanExporter};

        #[derive(Debug)]
        pub struct HangingSpanExporter;

        impl SpanExporter for HangingSpanExporter {
            async fn export(&self, _batch: Vec<SpanData>) -> OTelSdkResult {
                std::future::pending().await
            }
        }
    }

    #[test]
    fn start_without_otlp_builds_no_providers() {
        let telemetry = Telemetry::start(&TelemetryConfig::default(), &|_| None).expect("starts");
        assert!(!telemetry.traces_enabled());
        assert!(!telemetry.logs_enabled());
    }

    #[test]
    fn start_with_unset_headers_env_names_the_variable() {
        let mut config = TelemetryConfig::default();
        config.otlp = Some(config::OtlpConfig {
            endpoint: Some("https://otlp.example.com".to_string()),
            headers_env: Some("OTLP_HEADERS_FOR_TEST".to_string()),
            ..config::OtlpConfig::default()
        });
        let error = Telemetry::start(&config, &|_| None).expect_err("refused");
        assert!(
            error.to_string().contains("OTLP_HEADERS_FOR_TEST"),
            "{error}"
        );
        let error = Telemetry::start(&config, &|_| Some("no-equals-secret".to_string()))
            .expect_err("refused");
        assert!(
            error.to_string().contains("OTLP_HEADERS_FOR_TEST"),
            "{error}"
        );
        assert!(!error.to_string().contains("no-equals-secret"), "{error}");
    }

    #[test]
    fn otel_sdk_disabled_needs_no_credentials() {
        let mut config = TelemetryConfig::default();
        config.otlp = Some(config::OtlpConfig {
            endpoint: Some("https://otlp.example.com".to_string()),
            headers_env: Some("OTLP_HEADERS_FOR_TEST".to_string()),
            ..config::OtlpConfig::default()
        });
        // The headers variable is unset, and `OTEL_SDK_DISABLED` is the only
        // variable the callback answers.
        let only_disabled = |name: &str| (name == "OTEL_SDK_DISABLED").then(|| "true".to_string());
        let telemetry =
            Telemetry::start(&config, &only_disabled).expect("starts without credentials");
        assert!(!telemetry.traces_enabled() && !telemetry.logs_enabled());
        let dsn = TelemetryConfig {
            otlp: Some(config::OtlpConfig {
                sentry_dsn_env: Some("DSN_FOR_TEST".to_string()),
                ..config::OtlpConfig::default()
            }),
            ..TelemetryConfig::default()
        };
        Telemetry::start(&dsn, &only_disabled).expect("starts without a DSN");
    }

    #[tokio::test]
    async fn otel_sdk_disabled_turns_otlp_off() {
        let mut config = TelemetryConfig::default();
        config.otlp = Some(config::OtlpConfig {
            sentry_dsn_env: Some("DSN".to_string()),
            ..config::OtlpConfig::default()
        });
        let env: HashMap<&str, &str> = HashMap::from([
            ("DSN", "https://public@sentry.example.com/42"),
            ("OTEL_SDK_DISABLED", "TRUE"),
        ]);
        let telemetry = Telemetry::start(&config, &|name| env.get(name).map(ToString::to_string))
            .expect("starts");
        assert!(!telemetry.traces_enabled() && !telemetry.logs_enabled());
    }

    #[tokio::test]
    async fn otel_sdk_disabled_still_populates_the_prometheus_exposition() {
        let mut config = TelemetryConfig::default();
        config.otlp = Some(config::OtlpConfig {
            sentry_dsn_env: Some("DSN".to_string()),
            ..config::OtlpConfig::default()
        });
        let telemetry = Telemetry::start(&config, &|name| {
            (name == "OTEL_SDK_DISABLED").then(|| "true".to_string())
        })
        .expect("starts");
        // What `App::load` does with the global provider, on this telemetry's
        // own meter, so parallel tests cannot interfere.
        let _metrics = metrics::Metrics::new(&opentelemetry::metrics::MeterProvider::meter(
            &telemetry.meter,
            "okf-mcp",
        ));

        let families = telemetry.registry().gather();

        for expected in metrics::Metrics::FAMILIES {
            assert!(
                families.iter().any(|family| family.name() == *expected),
                "{expected} missing from {:?}",
                families
                    .iter()
                    .map(|family| family.name())
                    .collect::<Vec<_>>()
            );
        }
        let failures = families
            .iter()
            .find(|family| family.name() == "okf_telemetry_export_failures_total")
            .expect("the failure family");
        assert_eq!(failures.get_metric().len(), 12);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_returns_within_the_deadline_with_a_hung_exporter() {
        let processor = BatchSpanProcessor::builder(HangingSpanExporter, runtime::Tokio).build();
        let tracer = SdkTracerProvider::builder()
            .with_span_processor(processor)
            .build();
        // Put one span in the queue so the flush has something to hang on.
        {
            use opentelemetry::trace::{Tracer, TracerProvider};
            let _ = tracer.tracer("test").start("hung");
        }
        let telemetry = Telemetry::from_providers(Some(tracer), None);
        let started = Instant::now();
        let report = tokio::time::timeout(
            SHUTDOWN_DEADLINE + Duration::from_secs(1),
            telemetry.shutdown(),
        )
        .await
        .expect("shutdown returns within the deadline plus a second");
        assert!(started.elapsed() >= SHUTDOWN_DEADLINE.saturating_sub(Duration::from_millis(200)));
        assert_eq!(report.traces, ProviderOutcome::Abandoned);
        assert_eq!(report.logs, ProviderOutcome::NotRunning);
        assert!(!report.all_flushed());
    }

    #[tokio::test]
    async fn second_shutdown_is_quiet() {
        let tracer = SdkTracerProvider::builder().build();
        let logger = SdkLoggerProvider::builder().build();
        // Someone else already shut them down: AlreadyShutdown is not a failure.
        tracer.shutdown().expect("first");
        logger.shutdown().expect("first");
        let report = Telemetry::from_providers(Some(tracer), Some(logger))
            .shutdown()
            .await;
        assert!(report.all_flushed(), "{report:?}");
        assert_eq!(report.traces, ProviderOutcome::Flushed);
    }

    /// Accepts one connection per request, answers `200`, and sends each
    /// request's head (request line and headers) down the channel.
    async fn receiver() -> (u16, tokio::sync::mpsc::UnboundedReceiver<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let (sender, heads) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let sender = sender.clone();
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let mut chunk = [0_u8; 8192];
                    // Read until the head is complete; the body is not needed.
                    while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                        }
                    }
                    let _ = sender.send(String::from_utf8_lossy(&buffer).into_owned());
                    let _ = socket
                        .write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                        )
                        .await;
                });
            }
        });
        (port, heads)
    }

    /// Design, `sample_ratio`: 0.0 exports no new trace, 1.0 exports all of
    /// them, through the sampler `start` installs.
    #[test]
    fn the_sample_ratio_keeps_no_trace_at_zero_and_every_trace_at_one() {
        use opentelemetry::trace::{Span as _, Tracer, TracerProvider as _};
        use opentelemetry_sdk::trace::InMemorySpanExporter;

        for (ratio, kept) in [(0.0, 0), (1.0, 20)] {
            let exporter = InMemorySpanExporter::default();
            let provider = SdkTracerProvider::builder()
                .with_sampler(sampler_for(ratio))
                .with_simple_exporter(exporter.clone())
                .build();
            let tracer = provider.tracer("sample-ratio-test");

            for _ in 0..20 {
                tracer.start("root").end();
            }

            let exported = exporter.get_finished_spans().expect("spans");
            assert_eq!(exported.len(), kept, "ratio {ratio}");
        }
    }

    /// Design, endpoint paragraph: a base that already ends in `/v1/...` is
    /// a warning (with userinfo, query and fragment dropped), a plain base
    /// is silent.
    #[test]
    fn an_endpoint_that_already_ends_in_v1_warns_and_a_plain_base_does_not() {
        use crate::telemetry::config::LogFormat;
        use crate::telemetry::subscriber::capture::observe;

        let observed = observe("trace", "trace", "trace", LogFormat::Text, || {
            for endpoint in [
                "https://otlp.example.com",
                "https://otlp.example.com/otlp",
                "https://otlp.example.com/v10",
            ] {
                warn_if_endpoint_has_signal_path(endpoint);
            }
        });
        assert!(!observed.stderr.contains("base URL"), "{}", observed.stderr);

        let observed = observe("trace", "trace", "trace", LogFormat::Text, || {
            for ending in ["/v1/traces", "/v1/metrics", "/v1/logs", "/v1", "/v1/"] {
                // Userinfo, a query and a fragment must not reach the log.
                warn_if_endpoint_has_signal_path(&format!(
                    "https://zq-user:zq-pass@otlp.example.com:4318/otlp{ending}?zq-query=1#zq-frag"
                ));
            }
        });
        assert_eq!(
            observed.stderr.matches("base URL").count(),
            5,
            "{}",
            observed.stderr
        );
        assert!(
            observed
                .stderr
                .contains("https://otlp.example.com:4318/otlp/v1/traces"),
            "{}",
            observed.stderr
        );
        let exported = observed.exported_text();
        for secret in ["zq-user", "zq-pass", "zq-query", "zq-frag"] {
            assert!(!exported.contains(secret), "{secret} in {exported}");
            assert!(!observed.stderr.contains(secret), "{secret}");
        }
    }

    /// Task 1b: error text of these events reaches stderr only.
    #[test]
    fn startup_and_shutdown_errors_export_no_error_text() {
        use crate::telemetry::config::LogFormat;
        use crate::telemetry::subscriber::capture::observe;

        let observed = observe("trace", "trace", "trace", LogFormat::Text, || {
            warn_provider_off(
                "traces",
                &"bad endpoint http://zq-sentinel-7f3a.example.invalid",
            );
            warn_thread_not_started("logs", &std::io::Error::other("zq-sentinel-7f3a-io"));
        });
        let exported = observed.exported_text();
        assert!(exported.contains("OTLP export is off"), "{exported}");
        assert!(exported.contains("did not start"), "{exported}");
        assert!(
            exported.contains("traces") && exported.contains("logs"),
            "{exported}"
        );
        assert!(!exported.contains("zq-sentinel-7f3a"), "{exported}");
        assert!(observed.stderr.contains("zq-sentinel-7f3a.example.invalid"));
        assert!(observed.stderr.contains("zq-sentinel-7f3a-io"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spans_logs_and_metrics_reach_the_receiver_with_their_headers() {
        let (port, mut heads) = receiver().await;
        let mut config = TelemetryConfig::default();
        config.otlp = Some(config::OtlpConfig {
            endpoint: Some(format!("http://127.0.0.1:{port}")),
            headers_env: Some("H".to_string()),
            ..config::OtlpConfig::default()
        });
        let telemetry = Telemetry::start(&config, &|name| {
            (name == "H").then(|| "x-test-auth=token-1".to_string())
        })
        .expect("starts");
        assert!(telemetry.traces_enabled() && telemetry.logs_enabled());
        let settings = subscriber::settings(&config, Some("error")).expect("settings");
        let dispatch = telemetry.subscriber(settings);
        tracing::subscriber::with_default(dispatch, || {
            let span = tracing::info_span!(target: "okf_mcp::test", "exported");
            let _entered = span.enter();
            tracing::info!(target: "okf_mcp::test", "exported-log");
        });
        let report = telemetry.shutdown().await;
        assert!(report.all_flushed(), "{report:?}");
        let mut paths = Vec::new();
        while let Ok(Some(head)) = tokio::time::timeout(Duration::from_secs(5), heads.recv()).await
        {
            assert!(
                head.to_ascii_lowercase().contains("x-test-auth: token-1"),
                "{head}"
            );
            paths.push(head.lines().next().unwrap_or_default().to_string());
            if paths.len() == 3 {
                break;
            }
        }
        paths.sort();
        // The meter provider's final export is the third request.
        assert_eq!(paths.len(), 3, "{paths:?}");
        assert!(paths[0].starts_with("POST /v1/logs "), "{paths:?}");
        assert!(paths[1].starts_with("POST /v1/metrics "), "{paths:?}");
        assert!(paths[2].starts_with("POST /v1/traces "), "{paths:?}");
    }

    fn sentry_config(signals: Option<Vec<otlp::Signal>>) -> TelemetryConfig {
        TelemetryConfig {
            otlp: Some(config::OtlpConfig {
                sentry_dsn_env: Some("DSN".to_string()),
                signals,
                ..config::OtlpConfig::default()
            }),
            ..TelemetryConfig::default()
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_disabled_signal_builds_no_exporter_provider_or_reader() {
        // Exporters may dial at flush or shutdown: point them at a refusing port.
        let refusing = crate::test_net::Refused::new();
        let dsn = format!("http://public@{}/42", refusing.address());
        let env = |name: &str| (name == "DSN").then(|| dsn.clone());
        let sentry_default = Telemetry::start(&sentry_config(None), &env).expect("starts");
        assert!(sentry_default.traces_enabled() && sentry_default.logs_enabled());
        assert_eq!(
            sentry_default.otlp_signals(),
            [otlp::Signal::Traces, otlp::Signal::Logs]
        );

        let logs_only =
            Telemetry::start(&sentry_config(Some(vec![otlp::Signal::Logs])), &env).expect("starts");
        assert!(!logs_only.traces_enabled() && logs_only.logs_enabled());
        assert_eq!(logs_only.otlp_signals(), [otlp::Signal::Logs]);

        let mut endpoint = TelemetryConfig::default();
        endpoint.otlp = Some(config::OtlpConfig {
            endpoint: Some(format!("http://{}", refusing.address())),
            headers_env: Some("none".to_string()),
            signals: Some(vec![otlp::Signal::Metrics]),
            ..config::OtlpConfig::default()
        });
        let metrics_only = Telemetry::start(&endpoint, &|_| None).expect("starts");
        assert!(!metrics_only.traces_enabled() && !metrics_only.logs_enabled());
        assert_eq!(metrics_only.otlp_signals(), [otlp::Signal::Metrics]);
        // All twelve failure series exist from the first scrape, at 0.
        let families = logs_only.registry().gather();
        let family = families
            .iter()
            .find(|family| family.name() == "okf_telemetry_export_failures_total")
            .expect("failure counts");
        assert_eq!(family.get_metric().len(), 12);
        assert!(family
            .get_metric()
            .iter()
            .all(|metric| metric.get_counter().get_value() == 0.0));
        // Dropping a running provider blocks on its worker; shut each down.
        for telemetry in [sentry_default, logs_only, metrics_only] {
            crate::test_net::bounded("a telemetry shutdown", telemetry.shutdown()).await;
        }
    }

    /// Sentry's intake has no metrics endpoint: with `sentry_dsn_env` and the
    /// default signals no request goes to `/v1/metrics`, and traces and logs
    /// still arrive at their Sentry paths.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn with_a_sentry_dsn_no_request_goes_to_the_metrics_path() {
        let (port, mut heads) = receiver().await;
        let dsn = format!("http://public@127.0.0.1:{port}/42");
        let config = sentry_config(None);
        let telemetry = Telemetry::start(&config, &|name| (name == "DSN").then(|| dsn.clone()))
            .expect("starts");
        let settings = subscriber::settings(&config, Some("error")).expect("settings");
        let dispatch = telemetry.subscriber(settings);
        tracing::subscriber::with_default(dispatch, || {
            let span = tracing::info_span!(target: "okf_mcp::test", "exported");
            let _entered = span.enter();
            tracing::info!(target: "okf_mcp::test", "exported-log");
        });
        let report = telemetry.shutdown().await;
        assert!(report.all_flushed(), "{report:?}");
        let mut paths = Vec::new();
        while let Ok(Some(head)) = tokio::time::timeout(Duration::from_secs(3), heads.recv()).await
        {
            paths.push(head.lines().next().unwrap_or_default().to_string());
        }
        paths.sort();
        assert_eq!(paths.len(), 2, "{paths:?}");
        assert!(
            paths[0].starts_with("POST /api/42/integration/otlp/v1/logs "),
            "{paths:?}"
        );
        assert!(
            paths[1].starts_with("POST /api/42/integration/otlp/v1/traces "),
            "{paths:?}"
        );
    }

    #[tokio::test]
    async fn shutdown_without_providers_reports_not_running() {
        let report = Telemetry::from_providers(None, None).shutdown().await;
        assert_eq!(report.traces, ProviderOutcome::NotRunning);
        assert_eq!(report.logs, ProviderOutcome::NotRunning);
        assert!(report.all_flushed());
    }

    #[tokio::test]
    async fn start_without_otlp_still_serves_the_failure_counts_and_reports_metrics() {
        let telemetry = Telemetry::start(&TelemetryConfig::default(), &|_| None).expect("starts");
        // `okf_build_info` appears once `App::load` builds the handle; the
        // failure counts are registered by `start` itself.
        let families = telemetry.registry().gather();
        let names: Vec<&str> = families.iter().map(|family| family.name()).collect();
        assert_eq!(names, ["okf_telemetry_export_failures_total"]);
        let report = telemetry.shutdown().await;
        assert_eq!(report.traces, ProviderOutcome::NotRunning);
        assert_eq!(report.metrics, ProviderOutcome::Flushed);
        assert!(report.all_flushed());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn metrics_reach_the_receiver_when_the_provider_shuts_down() {
        let (port, mut heads) = receiver().await;
        let mut config = TelemetryConfig::default();
        config.otlp = Some(config::OtlpConfig {
            endpoint: Some(format!("http://127.0.0.1:{port}")),
            headers_env: Some("H".to_string()),
            // Far beyond the test: only the final flush at shutdown exports.
            export_interval: Duration::from_secs(3600),
            ..config::OtlpConfig::default()
        });
        let telemetry = Telemetry::start(&config, &|name| {
            (name == "H").then(|| "x-test-auth=token-1".to_string())
        })
        .expect("starts");

        let report = telemetry.shutdown().await;

        assert!(report.all_flushed(), "{report:?}");
        let head = tokio::time::timeout(Duration::from_secs(10), heads.recv())
            .await
            .expect("a metrics export arrives within 10 s")
            .expect("receiver open");
        assert!(head.starts_with("POST /v1/metrics "), "{head}");
        assert!(
            head.to_ascii_lowercase().contains("x-test-auth: token-1"),
            "{head}"
        );
    }
}
