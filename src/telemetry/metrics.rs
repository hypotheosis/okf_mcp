//! The server's metric instruments, behind one typed handle.
//!
//! Callers record through [`Metrics`]'s methods only: never an instrument,
//! never a raw attribute. Every label value is a closed set (an enum's
//! `as_str`, or [`tool_label`]'s allow-list), so a series count cannot grow
//! with what a client sends (spec "The `Metrics` handle").

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use opentelemetry::metrics::{Counter, Gauge, Histogram, Meter, UpDownCounter};
use opentelemetry::KeyValue;

use super::otlp::{ExportFailures, FailureReason, Signal};
use crate::app::App;
use crate::cache::bounded::{CacheName, CacheSize, Eviction, Lookup};
use crate::config::MailProvider;
use crate::housekeeping::Task;
use crate::mcp::annotations::{ADDITIVE, DESTRUCTIVE, EXTERNAL, READS, STAGED_DESTRUCTIVE};
use crate::store::sign_in_events::{Method, Outcome as SignInOutcome};

/// Seconds-histogram boundaries: 1 ms to 60 s. The SDK's defaults are
/// scaled for milliseconds (plan "Verified facts").
pub const SECONDS_BUCKETS: [f64; 14] = [
    0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 60.0,
];

/// What an operation ended as: a closed set, so a label never grows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The operation succeeded.
    Ok,
    /// The operation failed.
    Error,
}

impl Outcome {
    /// The label value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
        }
    }
}

/// What a Redis call was doing, the `operation` label of `okf.redis.errors`
/// (a closed set: spec Decision 17).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedisOp {
    /// Connecting, or the start-up `PING`.
    Connect,
    /// Reading a value.
    Get,
    /// Writing a value.
    Set,
    /// Reading and removing a value.
    Take,
    /// Changing a value in place (compare and set).
    Update,
    /// Deleting a value.
    Remove,
    /// Maintaining a per-actor index.
    Index,
    /// Admitting a request against a stored binding.
    Admit,
    /// Binding a session to a key.
    Bind,
    /// Publishing an announcement.
    Publish,
    /// Subscribing to announcements.
    Subscribe,
    /// Reading or bumping a counter.
    Counter,
    /// Taking from a shared rate-limit bucket (the request then falls back
    /// to the replica's own bucket).
    Limit,
    /// A stored value that would not decode.
    Decode,
}

impl RedisOp {
    /// The label value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::Get => "get",
            Self::Set => "set",
            Self::Take => "take",
            Self::Update => "update",
            Self::Remove => "remove",
            Self::Index => "index",
            Self::Admit => "admit",
            Self::Bind => "bind",
            Self::Publish => "publish",
            Self::Subscribe => "subscribe",
            Self::Counter => "counter",
            Self::Limit => "limit",
            Self::Decode => "decode",
        }
    }
}

/// What one catalog announcement came to on the replica that received it
/// (issue #659): a closed set for the `result` label of
/// `okf.catalog.announcements`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnouncementResult {
    /// The catalog's snapshot was rebuilt to the announced generation or a
    /// newer one.
    Applied,
    /// Nothing to rebuild: the snapshot already reached that generation (its
    /// own publish, the change feed or an earlier announcement got there).
    Current,
    /// This replica has not loaded the catalog, or it was deleted: it loads
    /// on its next use, or is gone.
    Unknown,
    /// The refresh failed; the old snapshot stays and its next use retries.
    Failed,
}

impl AnnouncementResult {
    /// The label value (also the `okf.announcement.result` span attribute).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Current => "current",
            Self::Unknown => "unknown",
            Self::Failed => "failed",
        }
    }
}

/// The `rule` label for a lint finding's rule id: the id itself when it is
/// one of the lint's own fixed rules, `secret` for every `secret:<id>` rule
/// (the token table is the lint's, and a label need not name which pattern
/// matched), and `other` for anything else. A label never carries text.
///
/// # Arguments
/// * `rule` - a finding's `rule`.
///
/// # Returns
/// A name from a fixed list.
#[tracing::instrument(level = "trace", skip_all)]
pub fn lint_rule_label(rule: &str) -> &'static str {
    if rule.starts_with("secret:") {
        return "secret";
    }
    LINT_RULES
        .iter()
        .find(|known| **known == rule)
        .copied()
        .unwrap_or("other")
}

/// The lint's own rule ids (`okf::analysis::lint`, plus publish's
/// `link-into-deleted`).
const LINT_RULES: &[&str] = &[
    "frontmatter-parse",
    "index-frontmatter",
    "no-frontmatter",
    "no-type",
    "no-title",
    "no-description",
    "sources-shape",
    "generated",
    "okf-version",
    "sprawl",
    "status",
    "generated-after-commit",
    "generated-shape",
    "verified-shape",
    "reviewed-shape",
    "reviewed-outcome",
    "stale",
    "timestamp",
    "timestamp-utc",
    "future-timestamp",
    "broken-link",
    "broken-anchor",
    "relative-link",
    "orphan",
    "index-drift",
    "link-into-deleted",
];

/// The server's instruments.
///
/// Cheap to clone: the instruments sit behind an `Arc`, so [`crate::app::App`]
/// and any callback share one set.
#[derive(Debug, Clone)]
pub struct Metrics {
    // Tasks 3 and 6 add one field per instrument to `Instruments`, and the
    // typed methods that read them.
    inner: Arc<Instruments>,
    // Observable gauges are registered after the `App` exists, on this meter.
    meter: Meter,
}

/// Every instrument, built once from a [`Meter`].
#[derive(Debug)]
struct Instruments {
    publish_duration: Histogram<f64>,
    publish_lock_wait: Histogram<f64>,
    lint_rejections: Counter<u64>,
    catalog_announcements: Counter<u64>,
    // Task 1 of milestone 18: the change feed.
    invalidations_queued: Counter<u64>,
    invalidations_received: Counter<u64>,
    invalidations_unreadable: Counter<u64>,
    invalidation_lag: Histogram<f64>,
    keycache_reloads: Counter<u64>,
    redis_errors: Counter<u64>,
    // Task 3: HTTP and MCP.
    http_duration: Histogram<f64>,
    http_active: UpDownCounter<i64>,
    tool_duration: Histogram<f64>,
    mcp_sessions: UpDownCounter<i64>,
    // Auth, rate limits and mail (Task 5).
    auth_attempts: Counter<u64>,
    oauth_tokens_issued: Counter<u64>,
    ratelimit_rejections: Counter<u64>,
    ratelimit_fallbacks: Counter<u64>,
    mail_jobs: Counter<u64>,
    mail_queue_depth: Arc<MailQueueDepth>,
    // The cluster-wide open changeset count `okf.changesets.open` reports,
    // as the last `cluster_stats` row read held it (issue #693).
    open_changesets: Arc<AtomicI64>,
    // Housekeeping jobs (issue #693).
    housekeeping_runs: Counter<u64>,
    housekeeping_duration: Histogram<f64>,
    // Clock checks and archives (Task 6).
    clock_checks: Counter<u64>,
    clock_offset: Gauge<f64>,
    archive_size: Counter<u64>,
    archive_duration: Histogram<f64>,
    // The bounded caches (M18 task 4).
    cache_requests: Counter<u64>,
    cache_evictions: Counter<u64>,
    cache_load_duration: Histogram<f64>,
}

impl Metrics {
    /// The Prometheus family names that exist before any traffic: the
    /// observable ones, which report from the first scrape. [`Metrics::new`]
    /// and [`observe_export_failures`] together make them present. Tasks 4-6
    /// add the catalog, changeset, mail-queue, pool and clock gauges.
    pub const FAMILIES: &'static [&'static str] = &[
        "okf_build_info",
        "okf_telemetry_export_failures_total",
        "okf_mail_queue_depth",
        "okf_changesets_open",
    ];
    // The pool gauge joins them once `App::load_with_metrics` registers it
    // (`observe_pool`).

    /// Every Prometheus family the server can export, as `/metrics` names
    /// it: the four in [`Metrics::FAMILIES`], plus each one that appears
    /// only once its event has happened (a counter or histogram has no
    /// series until the first record) or once an `App` or a cache exists
    /// (the observable gauges [`Metrics::observe_pool`] and
    /// [`Metrics::observe_cache`] register). The Grafana example's test
    /// (`tests/grafana_dashboard.rs`) checks the dashboard against this
    /// list, and the `all_families_...` unit test checks the list against
    /// the instruments, so a renamed instrument fails a test.
    pub const ALL_FAMILIES: &'static [&'static str] = &[
        "okf_build_info",
        "okf_telemetry_export_failures_total",
        "okf_mail_queue_depth",
        "okf_changesets_open",
        "http_server_request_duration_seconds",
        "http_server_active_requests",
        "okf_mcp_tool_duration_seconds",
        "okf_mcp_sessions_active",
        "okf_publish_duration_seconds",
        "okf_publish_lock_wait_seconds",
        "okf_lint_rejections_total",
        "okf_catalog_announcements_total",
        "okf_invalidations_queued_total",
        "okf_invalidations_received_total",
        "okf_invalidations_unreadable_total",
        "okf_invalidations_lag_seconds",
        "okf_keycache_reloads_total",
        "okf_redis_errors_total",
        "okf_auth_attempts_total",
        "okf_oauth_tokens_issued_total",
        "okf_ratelimit_rejections_total",
        "okf_ratelimit_fallbacks_total",
        "okf_mail_jobs_total",
        "okf_clock_checks_total",
        "okf_clock_offset_seconds",
        "okf_archive_size_bytes_total",
        "okf_archive_duration_seconds",
        "okf_housekeeping_runs_total",
        "okf_housekeeping_duration_seconds",
        "okf_cache_requests_total",
        "okf_cache_evictions_total",
        "okf_cache_load_duration_seconds",
        // Registered by `observe_cache`: a gauge reports only while its
        // cache is alive.
        "okf_cache_entries",
        "okf_cache_bytes",
        // Registered by `observe_pool`: it needs a loaded `App`, so its own
        // test (`app.rs`) checks the rendered name.
        "db_client_connection_count",
    ];

    /// Builds every instrument on `meter` and records `okf.build.info`: a
    /// gauge fixed at 1 whose `version` label is the crate version.
    ///
    /// # Arguments
    /// * `meter` - the meter to build on.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn new(meter: &Meter) -> Self {
        meter
            .u64_gauge("okf.build.info")
            .with_description("The running build; always 1.")
            .build()
            .record(1, &[KeyValue::new("version", env!("CARGO_PKG_VERSION"))]);
        Self {
            inner: Arc::new(Instruments {
                publish_duration: seconds_histogram(
                    meter,
                    "okf.publish.duration",
                    "Publishes, from the first lock to the refreshed snapshot.",
                ),
                publish_lock_wait: seconds_histogram(
                    meter,
                    "okf.publish.lock_wait",
                    "Time a publish waited for the catalog's locks.",
                ),
                lint_rejections: meter
                    .u64_counter("okf.lint.rejections")
                    .with_description("Publish lint errors, by rule.")
                    .build(),
                catalog_announcements: meter
                    .u64_counter("okf.catalog.announcements")
                    .with_description(
                        "Catalog generation announcements received from Redis, by result.",
                    )
                    .build(),
                invalidations_queued: meter
                    .u64_counter("okf.invalidations.queued")
                    .with_description("Change-feed messages queued in a transaction, by kind; a rollback discards them.")
                    .build(),
                invalidations_received: meter
                    .u64_counter("okf.invalidations.received")
                    .with_description("Change-feed messages received, by kind.")
                    .build(),
                invalidations_unreadable: meter
                    .u64_counter("okf.invalidations.unreadable")
                    .with_description("Change-feed payloads this build could not read.")
                    .build(),
                invalidation_lag: seconds_histogram(
                    meter,
                    "okf.invalidations.lag",
                    "Time from a change-feed message's send to its application.",
                ),
                keycache_reloads: meter
                    .u64_counter("okf.keycache.reloads")
                    .with_description("Key cache and settings reloads, by outcome.")
                    .build(),
                redis_errors: meter
                    .u64_counter("okf.redis.errors")
                    .with_description("Failed Redis calls, by operation.")
                    .build(),
                http_duration: seconds_histogram(
                    meter,
                    "http.server.request.duration",
                    "HTTP server request duration, by method, route and status.",
                ),
                http_active: meter
                    .i64_up_down_counter("http.server.active_requests")
                    .with_description("HTTP requests being served, by method and route.")
                    .build(),
                tool_duration: seconds_histogram(
                    meter,
                    "okf.mcp.tool.duration",
                    "MCP tool call duration, by tool, catalog and outcome.",
                ),
                mcp_sessions: meter
                    .i64_up_down_counter("okf.mcp.sessions.active")
                    .with_description("Open MCP sessions.")
                    .build(),
                auth_attempts: counter(meter, "okf.auth.attempts", "Sign-in attempts."),
                oauth_tokens_issued: counter(
                    meter,
                    "okf.oauth.tokens.issued",
                    "OAuth token pairs issued, by grant.",
                ),
                ratelimit_rejections: counter(
                    meter,
                    "okf.ratelimit.rejections",
                    "Requests a rate limiter refused.",
                ),
                ratelimit_fallbacks: counter(
                    meter,
                    "okf.ratelimit.fallbacks",
                    "Rate-limit decisions a replica's own bucket made because the shared one failed.",
                ),
                mail_jobs: counter(
                    meter,
                    "okf.mail.jobs",
                    "Mail jobs, by provider and outcome.",
                ),
                mail_queue_depth: MailQueueDepth::observe(meter),
                open_changesets: observe_open_changesets(meter),
                housekeeping_runs: counter(
                    meter,
                    "okf.housekeeping.runs",
                    "Housekeeping jobs run, by task and outcome.",
                ),
                housekeeping_duration: seconds_histogram(
                    meter,
                    "okf.housekeeping.duration",
                    "Housekeeping job duration, by task.",
                ),
                clock_checks: counter(meter, "okf.clock.checks", "Clock checks, by outcome."),
                clock_offset: meter
                    .f64_gauge("okf.clock.offset")
                    .with_unit("s")
                    .with_description("The time server's clock minus ours, at the last check.")
                    .build(),
                archive_size: meter
                    .u64_counter("okf.archive.size")
                    .with_unit("By")
                    .with_description("Archive bytes moved, by direction and catalog.")
                    .build(),
                archive_duration: seconds_histogram(
                    meter,
                    "okf.archive.duration",
                    "Archive exports and imports, by direction and outcome.",
                ),
                cache_requests: counter(
                    meter,
                    "okf.cache.requests",
                    "Bounded cache lookups, by cache and result.",
                ),
                cache_evictions: counter(
                    meter,
                    "okf.cache.evictions",
                    "Bounded cache entries removed, by cache and cause.",
                ),
                cache_load_duration: seconds_histogram(
                    meter,
                    "okf.cache.load.duration",
                    "Bounded cache loads, by cache and outcome.",
                ),
            }),
            meter: meter.clone(),
        }
    }

    /// Records a finished publish.
    ///
    /// # Arguments
    /// * `catalog` - the catalog's name (a validated catalog name).
    /// * `outcome` - whether it committed (or had nothing to write).
    /// * `elapsed` - the whole publish.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn publish(&self, catalog: &str, outcome: Outcome, elapsed: Duration) {
        self.inner.publish_duration.record(
            elapsed.as_secs_f64(),
            &[
                KeyValue::new("catalog", catalog.to_owned()),
                KeyValue::new("outcome", outcome.as_str()),
            ],
        );
    }

    /// Records how long a publish waited for the catalog's locks.
    ///
    /// # Arguments
    /// * `catalog` - the catalog's name.
    /// * `waited` - the time from asking to holding every lock.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn publish_lock_wait(&self, catalog: &str, waited: Duration) {
        self.inner.publish_lock_wait.record(
            waited.as_secs_f64(),
            &[KeyValue::new("catalog", catalog.to_owned())],
        );
    }

    /// Counts one lint error that refused a publish.
    ///
    /// # Arguments
    /// * `catalog` - the catalog's name.
    /// * `rule` - the finding's rule id; folded by [`lint_rule_label`].
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn lint_rejection(&self, catalog: &str, rule: &str) {
        self.inner.lint_rejections.add(
            1,
            &[
                KeyValue::new("catalog", catalog.to_owned()),
                KeyValue::new("rule", lint_rule_label(rule)),
            ],
        );
    }

    /// Counts one catalog announcement received (issue #659).
    ///
    /// # Arguments
    /// * `result` - what it came to.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn catalog_announcement(&self, result: AnnouncementResult) {
        self.inner
            .catalog_announcements
            .add(1, &[KeyValue::new("result", result.as_str())]);
    }

    /// Counts one change-feed message queued in a transaction. The message is
    /// delivered only if that transaction commits, so a rollback leaves this
    /// counter ahead of `received`.
    ///
    /// # Arguments
    /// * `kind` - the invalidation's kind (a fixed set, never an id).
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn invalidation_queued(&self, kind: &'static str) {
        self.inner
            .invalidations_queued
            .add(1, &[KeyValue::new("kind", kind)]);
    }

    /// Counts one change-feed message received and records its lag.
    ///
    /// # Arguments
    /// * `kind` - the invalidation's kind.
    /// * `lag` - send to receipt (zero when the clocks disagree).
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn invalidation_received(&self, kind: &'static str, lag: Duration) {
        let labels = [KeyValue::new("kind", kind)];
        self.inner.invalidations_received.add(1, &labels);
        self.inner.invalidation_lag.record(lag.as_secs_f64(), &[]);
    }

    /// Counts one change-feed payload that could not be read.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn invalidation_unreadable(&self) {
        self.inner.invalidations_unreadable.add(1, &[]);
    }

    /// Counts one key cache and settings reload.
    ///
    /// # Arguments
    /// * `outcome` - whether it succeeded.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn keycache_reload(&self, outcome: Outcome) {
        self.inner
            .keycache_reloads
            .add(1, &[KeyValue::new("outcome", outcome.as_str())]);
    }

    /// Counts one failed Redis call.
    ///
    /// # Arguments
    /// * `op` - what the call was doing.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn redis_error(&self, op: RedisOp) {
        self.inner
            .redis_errors
            .add(1, &[KeyValue::new("operation", op.as_str())]);
    }

    /// The instruments on the global meter provider: a no-op until `serve`
    /// sets one, so tests and the other subcommands record nothing.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn global() -> Self {
        Self::new(&opentelemetry::global::meter("okf-mcp"))
    }
}

/// Task 3: the HTTP and MCP instruments' typed methods.
impl Metrics {
    /// Records one finished HTTP request.
    ///
    /// # Arguments
    /// * `method` - the semconv method (`GET`, ... or `_OTHER`).
    /// * `route` - the matched route template; never the literal path.
    /// * `status` - the response status code.
    /// * `elapsed` - how long the request took.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn http_request(&self, method: &'static str, route: &str, status: u16, elapsed: Duration) {
        self.inner.http_duration.record(
            elapsed.as_secs_f64(),
            &[
                KeyValue::new("http.request.method", method),
                KeyValue::new("http.route", route.to_string()),
                KeyValue::new("http.response.status_code", i64::from(status)),
            ],
        );
    }

    /// Counts a request as in flight until the returned guard drops
    /// (including when the request's future is cancelled).
    ///
    /// # Arguments
    /// * `method` - the semconv method.
    /// * `route` - the matched route template.
    ///
    /// # Returns
    /// The guard; dropping it decrements the counter.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn http_in_flight(&self, method: &'static str, route: &str) -> InFlight {
        let attributes = vec![
            KeyValue::new("http.request.method", method),
            KeyValue::new("http.route", route.to_string()),
        ];
        self.inner.http_active.add(1, &attributes);
        InFlight {
            counter: self.inner.http_active.clone(),
            attributes,
        }
    }

    /// Records one finished MCP tool call.
    ///
    /// # Arguments
    /// * `tool` - a [`tool_label`] value.
    /// * `catalog` - a loaded catalog's name, or `none`.
    /// * `outcome` - how the call ended.
    /// * `elapsed` - how long it took.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn tool_call(
        &self,
        tool: &'static str,
        catalog: &str,
        outcome: Outcome,
        elapsed: Duration,
    ) {
        self.inner.tool_duration.record(
            elapsed.as_secs_f64(),
            &[
                KeyValue::new("tool", tool),
                KeyValue::new("catalog", catalog.to_string()),
                KeyValue::new("outcome", outcome.as_str()),
            ],
        );
    }

    /// Counts an MCP session as open until the returned guard drops.
    ///
    /// # Returns
    /// The guard; dropping it decrements `okf.mcp.sessions.active`.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn mcp_session(&self) -> SessionGuard {
        self.inner.mcp_sessions.add(1, &[]);
        SessionGuard {
            counter: self.inner.mcp_sessions.clone(),
        }
    }
}

/// An HTTP request in flight; decrements `http.server.active_requests` when
/// dropped.
#[derive(Debug)]
pub struct InFlight {
    counter: UpDownCounter<i64>,
    attributes: Vec<KeyValue>,
}

// `Drop` runs when the value goes out of scope, like a Python context
// manager's `__exit__`, and also when a cancelled future is dropped.
impl Drop for InFlight {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "a drop hook that only adds to one counter"
        )
    )]
    fn drop(&mut self) {
        self.counter.add(-1, &self.attributes);
    }
}

/// An open MCP session; decrements `okf.mcp.sessions.active` when dropped.
#[derive(Debug)]
pub struct SessionGuard {
    counter: UpDownCounter<i64>,
}

impl Drop for SessionGuard {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "a drop hook that only adds to one counter"
        )
    )]
    fn drop(&mut self) {
        self.counter.add(-1, &[]);
    }
}

/// The bounded caches' instruments (M18 task 4). Labels are closed sets:
/// the cache, the lookup result and the eviction cause; never a key.
impl Metrics {
    /// Counts one lookup of a bounded cache.
    ///
    /// # Arguments
    /// * `cache` - which cache.
    /// * `result` - hit, miss, or a stale entry that was revalidated.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn cache_request(&self, cache: CacheName, result: Lookup) {
        self.inner.cache_requests.add(
            1,
            &[
                KeyValue::new("cache", cache.as_str()),
                KeyValue::new("result", result.as_str()),
            ],
        );
    }

    /// Counts one entry removed from a bounded cache.
    ///
    /// # Arguments
    /// * `cache` - which cache.
    /// * `cause` - why it left.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn cache_eviction(&self, cache: CacheName, cause: Eviction) {
        self.inner.cache_evictions.add(
            1,
            &[
                KeyValue::new("cache", cache.as_str()),
                KeyValue::new("cause", cause.as_str()),
            ],
        );
    }

    /// Records one finished load of a bounded cache.
    ///
    /// # Arguments
    /// * `cache` - which cache.
    /// * `outcome` - whether the loader succeeded.
    /// * `elapsed` - how long it ran.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn cache_load(&self, cache: CacheName, outcome: Outcome, elapsed: Duration) {
        self.inner.cache_load_duration.record(
            elapsed.as_secs_f64(),
            &[
                KeyValue::new("cache", cache.as_str()),
                KeyValue::new("outcome", outcome.as_str()),
            ],
        );
    }

    /// Registers `okf.cache.entries` and `okf.cache.bytes` for one cache.
    /// The callbacks hold only a [`Weak`] handle (the meter provider
    /// outlives the cache), and report nothing once it is dropped.
    ///
    /// # Arguments
    /// * `cache` - which cache the series is labelled with.
    /// * `size` - the cache's size probe.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn observe_cache(&self, cache: CacheName, size: Weak<dyn CacheSize>) {
        let label = [KeyValue::new("cache", cache.as_str())];
        let entries_probe = size.clone();
        let entries_label = label.clone();
        self.meter
            .u64_observable_gauge("okf.cache.entries")
            .with_description("Entries held by a bounded cache.")
            .with_callback(move |observer| {
                if let Some(probe) = entries_probe.upgrade() {
                    observer.observe(probe.entries(), &entries_label);
                }
            })
            .build();
        self.meter
            .u64_observable_gauge("okf.cache.bytes")
            .with_description("Estimated bytes held by a bounded cache that is limited by bytes.")
            .with_callback(move |observer| {
                if let Some(probe) = size.upgrade() {
                    if let Some(bytes) = probe.bytes() {
                        observer.observe(bytes, &label);
                    }
                }
            })
            .build();
    }
}

/// A counter of events (`okf.auth.attempts` renders as
/// `okf_auth_attempts_total`).
#[tracing::instrument(level = "trace", skip_all)]
fn counter(meter: &Meter, name: &'static str, description: &'static str) -> Counter<u64> {
    meter
        .u64_counter(name)
        .with_description(description)
        .build()
}

/// The rate limiters, a closed set: the `limiter` label of
/// `okf.ratelimit.rejections`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limiter {
    /// Password, passkey and OIDC sign-in starts, by address.
    SignInPerIp,
    /// `POST /signin/forgot`, by normalized email.
    ForgotPerEmail,
    /// `POST /signin/forgot`, by address.
    ForgotPerIp,
    /// A signed-in user's OIDC link starts.
    LinkStarts,
    /// OAuth dynamic client registration.
    OauthRegister,
    /// OAuth token and revocation endpoints.
    OauthToken,
    /// OAuth consent posts.
    OauthConsent,
}

impl Limiter {
    /// The label value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SignInPerIp => "sign_in_per_ip",
            Self::ForgotPerEmail => "forgot_per_email",
            Self::ForgotPerIp => "forgot_per_ip",
            Self::LinkStarts => "link_starts",
            Self::OauthRegister => "oauth_register",
            Self::OauthToken => "oauth_token",
            Self::OauthConsent => "oauth_consent",
        }
    }

    /// The name an exported event carries instead of [`Limiter::as_str`].
    /// Sentry's data scrubbing replaces a value that contains "token" with
    /// `[Filtered]`, so `oauth_token` becomes `oauth-tok` here: the event
    /// still says which limiter failed. Every other name is its label.
    pub const fn event_name(self) -> &'static str {
        match self {
            Self::OauthToken => "oauth-tok",
            other => other.as_str(),
        }
    }
}

/// The OAuth grants the token endpoint serves: the `grant` label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grant {
    /// `grant_type=authorization_code`.
    AuthorizationCode,
    /// `grant_type=refresh_token`.
    RefreshToken,
}

impl Grant {
    /// The label value, the grant type's own spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AuthorizationCode => "authorization_code",
            Self::RefreshToken => "refresh_token",
        }
    }
}

/// How a mail job ended: the `outcome` label of `okf.mail.jobs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailOutcome {
    /// The provider accepted the mail.
    Sent,
    /// A temporary failure; the job is scheduled again.
    Retry,
    /// A permanent failure, or the retry window ran out.
    Failed,
}

impl MailOutcome {
    /// The label value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sent => "sent",
            Self::Retry => "retry",
            Self::Failed => "failed",
        }
    }
}

/// Which way an archive moved: the `direction` label of the archive
/// instruments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// A catalog exported as a tarball.
    Export,
    /// A tarball imported into a catalog.
    Import,
}

impl Direction {
    /// The label value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Export => "export",
            Self::Import => "import",
        }
    }
}

/// The states a queued mail is shown in (`mail queue`): the `status` label
/// of `okf.mail.queue.depth`. Done jobs are not counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailStatus {
    /// Waiting for its first try.
    Pending,
    /// Claimed by a worker.
    Sending,
    /// Failed and scheduled again.
    Retrying,
    /// Given up on, until someone retries it.
    Failed,
}

impl MailStatus {
    /// Every status, in the order the gauge keeps them.
    pub const ALL: [Self; 4] = [Self::Pending, Self::Sending, Self::Retrying, Self::Failed];

    /// The label value, the spelling `mail::queue::list` gives.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Sending => "sending",
            Self::Retrying => "retrying",
            Self::Failed => "failed",
        }
    }

    /// The status for a stored spelling, `None` for anything else.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|status| status.as_str() == text)
    }

    /// The slot this status keeps its count in.
    const fn slot(self) -> usize {
        match self {
            Self::Pending => 0,
            Self::Sending => 1,
            Self::Retrying => 2,
            Self::Failed => 3,
        }
    }
}

/// The mail queue's depth by status, as the last `cluster_stats` row read
/// held it. A scrape reads these numbers; it never queries Postgres.
#[derive(Debug, Default)]
struct MailQueueDepth {
    counts: [AtomicI64; MailStatus::ALL.len()],
}

impl MailQueueDepth {
    /// Registers the `okf.mail.queue.depth` gauge on `meter`: one series per
    /// status from the first scrape (zero until the first row is read).
    #[tracing::instrument(level = "trace", skip_all)]
    fn observe(meter: &Meter) -> Arc<Self> {
        let depth = Arc::new(Self::default());
        let read = Arc::clone(&depth);
        // The instrument handle may be dropped: the callback stays
        // registered with the meter provider.
        meter
            .i64_observable_gauge("okf.mail.queue.depth")
            .with_description(
                "Mail jobs not yet sent, by status, as of the last cluster_stats job.",
            )
            .with_callback(move |observer| {
                for status in MailStatus::ALL {
                    observer.observe(
                        read.counts[status.slot()].load(Ordering::Relaxed),
                        &[KeyValue::new("status", status.as_str())],
                    );
                }
            })
            .build();
        depth
    }
}

/// Registers the `okf.changesets.open` gauge on `meter`: one series, the
/// cluster's total (zero until the first `cluster_stats` row is read). Every
/// replica reports the same value, so dashboards take `max()`.
///
/// # Returns
/// The number the gauge reports; [`Metrics::set_open_changesets`] stores it.
#[tracing::instrument(level = "trace", skip_all)]
fn observe_open_changesets(meter: &Meter) -> Arc<AtomicI64> {
    let open = Arc::new(AtomicI64::new(0));
    let read = Arc::clone(&open);
    // The instrument handle may be dropped: the callback stays registered.
    meter
        .i64_observable_gauge("okf.changesets.open")
        .with_description("Open changesets in the cluster, as of the last cluster_stats job.")
        .with_callback(move |observer| observer.observe(read.load(Ordering::Relaxed), &[]))
        .build();
    open
}

impl Metrics {
    /// Counts one sign-in attempt, at the two choke points that record a
    /// `sign_in_events` row. The labels are the audit's own closed enums.
    ///
    /// # Arguments
    /// * `method` - how they tried.
    /// * `outcome` - how it ended.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn auth_attempt(&self, method: Method, outcome: SignInOutcome) {
        self.inner.auth_attempts.add(
            1,
            &[
                KeyValue::new("method", method.as_str()),
                KeyValue::new("outcome", outcome.as_str()),
            ],
        );
    }

    /// Counts one OAuth token pair issued.
    ///
    /// # Arguments
    /// * `grant` - the grant that issued it.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn oauth_token_issued(&self, grant: Grant) {
        self.inner
            .oauth_tokens_issued
            .add(1, &[KeyValue::new("grant", grant.as_str())]);
    }

    /// Counts one request a rate limiter refused.
    ///
    /// # Arguments
    /// * `limiter` - which limiter.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn rate_limited(&self, limiter: Limiter) {
        self.inner
            .ratelimit_rejections
            .add(1, &[KeyValue::new("limiter", limiter.as_str())]);
    }

    /// Counts one rate-limit decision this replica's own bucket made
    /// because the shared (Redis) bucket failed.
    ///
    /// # Arguments
    /// * `limiter` - which limiter fell back.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn rate_limit_fallback(&self, limiter: Limiter) {
        self.inner
            .ratelimit_fallbacks
            .add(1, &[KeyValue::new("limiter", limiter.as_str())]);
    }

    /// Counts one mail job's try.
    ///
    /// # Arguments
    /// * `provider` - the configured provider.
    /// * `outcome` - how the try ended.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn mail_job(&self, provider: MailProvider, outcome: MailOutcome) {
        self.inner.mail_jobs.add(
            1,
            &[
                KeyValue::new("provider", provider.as_str()),
                KeyValue::new("outcome", outcome.as_str()),
            ],
        );
    }

    /// Stores the mail queue's depth for the gauge: the count per status
    /// the last `cluster_stats` row read held. A status missing from
    /// `counts` is zero.
    ///
    /// # Arguments
    /// * `counts` - `(status, jobs)` pairs.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn set_mail_queue_depth(&self, counts: &[(MailStatus, i64)]) {
        for status in MailStatus::ALL {
            let jobs = counts
                .iter()
                .filter(|(counted, _)| *counted == status)
                .map(|(_, jobs)| *jobs)
                .sum();
            self.inner.mail_queue_depth.counts[status.slot()].store(jobs, Ordering::Relaxed);
        }
    }

    /// Stores the cluster's open changeset count for `okf.changesets.open`.
    ///
    /// # Arguments
    /// * `open` - the count the last `cluster_stats` row read held.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn set_open_changesets(&self, open: i64) {
        self.inner.open_changesets.store(open, Ordering::Relaxed);
    }

    /// Records one housekeeping job's run.
    ///
    /// # Arguments
    /// * `task` - which task ran.
    /// * `outcome` - whether it succeeded (a failure is retried by apalis).
    /// * `elapsed` - how long it took.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn housekeeping_run(&self, task: Task, outcome: Outcome, elapsed: Duration) {
        self.inner.housekeeping_runs.add(
            1,
            &[
                KeyValue::new("task", task.as_str()),
                KeyValue::new("outcome", outcome.as_str()),
            ],
        );
        self.inner.housekeeping_duration.record(
            elapsed.as_secs_f64(),
            &[KeyValue::new("task", task.as_str())],
        );
    }
}

/// Task 6: pool, clock and archive instruments.
impl Metrics {
    /// Registers `db.client.connection.count`: the pool's connections, one
    /// series per `state` (`idle` and `used`, which sum to the pool's size).
    /// The callback holds only a [`Weak`] handle and reads the pool's own
    /// counters, so a scrape never queries Postgres.
    ///
    /// # Arguments
    /// * `app` - the app whose pool to read; a callback finding it dropped
    ///   reports nothing.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn observe_pool(&self, app: Weak<App>) {
        // The gauge handle may be dropped: the callback stays registered
        // with the provider (see `observe_export_failures`).
        self.meter
            .i64_observable_gauge("db.client.connection.count")
            .with_description("Postgres pool connections, by state.")
            .with_callback(move |observer| {
                let Some(app) = app.upgrade() else {
                    return;
                };
                let pool = app.accounts().executor();
                let size = i64::from(pool.size());
                // `size` and `num_idle` are read a moment apart, so idle can
                // briefly exceed size; clamp, so used is never negative and
                // the two always sum to `size`.
                let idle = i64::try_from(pool.num_idle()).unwrap_or(i64::MAX).min(size);
                observer.observe(idle, &[KeyValue::new("state", "idle")]);
                observer.observe(size - idle, &[KeyValue::new("state", "used")]);
            })
            .build();
    }

    /// Records one clock check.
    ///
    /// # Arguments
    /// * `outcome` - `Ok` when the time server gave a usable reply.
    /// * `offset_ms` - the server's clock minus ours; `None` on failure,
    ///   which leaves the offset gauge at its last value.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn clock_check(&self, outcome: Outcome, offset_ms: Option<i64>) {
        self.inner
            .clock_checks
            .add(1, &[KeyValue::new("outcome", outcome.as_str())]);
        if let Some(offset_ms) = offset_ms {
            // An offset is milliseconds well under 2^53: exact as an `f64`.
            #[allow(clippy::cast_precision_loss)]
            self.inner
                .clock_offset
                .record(offset_ms as f64 / 1000.0, &[]);
        }
    }

    /// Records a finished archive export or import of a catalog that exists.
    ///
    /// # Arguments
    /// * `direction` - export or import.
    /// * `catalog` - a loaded catalog's name (never a raw path segment).
    /// * `outcome` - how it ended.
    /// * `elapsed` - how long it took.
    /// * `bytes` - the archive's size when it succeeded; `None` adds no bytes.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn archive(
        &self,
        direction: Direction,
        catalog: &str,
        outcome: Outcome,
        elapsed: Duration,
        bytes: Option<u64>,
    ) {
        self.inner.archive_duration.record(
            elapsed.as_secs_f64(),
            &[
                KeyValue::new("direction", direction.as_str()),
                KeyValue::new("outcome", outcome.as_str()),
            ],
        );
        if let Some(bytes) = bytes {
            self.inner.archive_size.add(
                bytes,
                &[
                    KeyValue::new("direction", direction.as_str()),
                    KeyValue::new("catalog", catalog.to_owned()),
                ],
            );
        }
    }
}

/// Registers `okf.telemetry.export.failures` (rendered as
/// `okf_telemetry_export_failures_total`): an observable counter reading
/// `failures` at each collection, one series per `(signal, reason)`, every
/// one present from the first scrape (zero until a failure).
///
/// # Arguments
/// * `meter` - the meter to register on.
/// * `failures` - the counts the counting exporters keep.
#[tracing::instrument(level = "trace", skip_all)]
pub fn observe_export_failures(meter: &Meter, failures: Arc<ExportFailures>) {
    // The instrument handle may be dropped: the callback stays registered
    // with the meter provider.
    meter
        .u64_observable_counter("okf.telemetry.export.failures")
        .with_description("OTLP exports that failed, by signal and reason.")
        .with_callback(move |observer| {
            for signal in Signal::ALL {
                for reason in FailureReason::ALL {
                    observer.observe(
                        failures.count(signal, reason),
                        &[
                            KeyValue::new("signal", signal.label()),
                            KeyValue::new("reason", reason.label()),
                        ],
                    );
                }
            }
        })
        .build();
}

/// A histogram of durations in seconds, with [`SECONDS_BUCKETS`].
///
/// # Arguments
/// * `meter` - the meter to build on.
/// * `name` - the instrument name (`okf.mcp.tool.duration`); the `s` unit
///   makes Prometheus name it `..._seconds`.
/// * `description` - the `# HELP` text.
#[tracing::instrument(level = "trace", skip_all)]
pub fn seconds_histogram(
    meter: &Meter,
    name: &'static str,
    description: &'static str,
) -> Histogram<f64> {
    meter
        .f64_histogram(name)
        .with_unit("s")
        .with_description(description)
        .with_boundaries(SECONDS_BUCKETS.to_vec())
        .build()
}

/// The `tool` label for an MCP tool name: the name itself when it is one of
/// the server's registered tools, else `other`. A client can send any name;
/// only a registered one becomes a series.
///
/// # Arguments
/// * `name` - the tool name from the request.
///
/// # Returns
/// A name from `mcp::annotations`'s lists, or `"other"`.
#[tracing::instrument(level = "trace", skip_all)]
pub fn tool_label(name: &str) -> &'static str {
    READS
        .iter()
        .chain(ADDITIVE)
        .chain(STAGED_DESTRUCTIVE)
        .chain(DESTRUCTIVE)
        .chain(EXTERNAL)
        .find(|known| **known == name)
        .copied()
        .unwrap_or("other")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_label_passes_registered_names_and_folds_the_rest() {
        assert_eq!(tool_label("get_concept"), "get_concept");
        assert_eq!(tool_label("publish"), "publish");
        assert_eq!(tool_label("open_changeset"), "open_changeset");
        assert_eq!(tool_label("not_a_tool"), "other");
        assert_eq!(tool_label(""), "other");
        assert_eq!(tool_label("get_concept "), "other");
    }

    #[test]
    fn every_rule_the_lint_names_is_a_label_of_its_own() {
        // The rule ids are string literals in the lint's source, as the
        // second argument of `error`/`warn` or the third of `finding`.
        let source = include_str!("../okf/analysis/lint.rs");
        let production = source.split("#[cfg(test)]").next().expect("the source");
        let rules = regex::Regex::new(
            r#"(?s)(?:\.(?:error|warn)\(\s*[^,"]*,\s*|\bfinding\(\s*[^,]+,\s*[^,]+,\s*)"([a-z]+(?:-[a-z]+)*)""#,
        )
        .expect("the pattern");
        let found: Vec<&str> = rules
            .captures_iter(production)
            .map(|capture| capture.get(1).expect("a rule").as_str())
            .collect();
        assert!(found.len() > 20, "the scan found {found:?}");
        for rule in found {
            assert_eq!(
                lint_rule_label(rule),
                rule,
                "{rule} is missing from LINT_RULES"
            );
        }
        for rule in ["link-into-deleted", "no-title", "no-description"] {
            assert_eq!(lint_rule_label(rule), rule);
        }
        assert_eq!(lint_rule_label("secret:github-token"), "secret");
        assert_eq!(lint_rule_label("secret:assignment"), "secret");
        assert_eq!(lint_rule_label("made up"), "other");
    }

    #[test]
    fn all_families_names_every_family_the_instruments_render() {
        use crate::mcp::testing::local_metrics;
        // The gauge that reads a loaded `App`.
        const NEEDS_APP: [&str; 1] = ["db_client_connection_count"];
        let (_provider, metrics, registry) = local_metrics();
        let elapsed = Duration::from_millis(5);

        metrics.publish("notes", Outcome::Ok, elapsed);
        metrics.publish_lock_wait("notes", elapsed);
        metrics.lint_rejection("notes", "orphan");
        metrics.catalog_announcement(AnnouncementResult::Applied);
        metrics.invalidation_queued("settings");
        metrics.invalidation_received("settings", Duration::from_millis(5));
        metrics.invalidation_unreadable();
        metrics.keycache_reload(Outcome::Ok);
        metrics.redis_error(RedisOp::Get);
        metrics.http_request("GET", "/health", 200, elapsed);
        let _in_flight = metrics.http_in_flight("GET", "/health");
        metrics.tool_call("get_concept", "notes", Outcome::Ok, elapsed);
        let _session = metrics.mcp_session();
        metrics.auth_attempt(Method::Password, SignInOutcome::Ok);
        metrics.oauth_token_issued(Grant::RefreshToken);
        metrics.rate_limited(Limiter::OauthToken);
        metrics.rate_limit_fallback(Limiter::OauthToken);
        metrics.mail_job(MailProvider::Log, MailOutcome::Sent);
        metrics.clock_check(Outcome::Ok, Some(10));
        metrics.archive(Direction::Export, "notes", Outcome::Ok, elapsed, Some(10));
        metrics.housekeeping_run(Task::PruneOauth, Outcome::Ok, elapsed);
        metrics.cache_request(CacheName::Directory, Lookup::Hit);
        metrics.cache_eviction(CacheName::Directory, Eviction::Size);
        metrics.cache_load(CacheName::Directory, Outcome::Ok, elapsed);
        // The gauges report while their cache is alive.
        let _cache = crate::cache::bounded::Bounded::<i64, i64>::new(
            CacheName::Directory,
            crate::cache::bounded::Limit::Bytes(10),
            Duration::from_secs(60),
        )
        .with_metrics(metrics.clone());

        let rendered: std::collections::BTreeSet<String> = registry
            .gather()
            .iter()
            .map(|family| family.name().to_string())
            .collect();
        let listed: std::collections::BTreeSet<String> = Metrics::ALL_FAMILIES
            .iter()
            .filter(|name| !NEEDS_APP.contains(name))
            .map(ToString::to_string)
            .collect();
        assert_eq!(rendered, listed);
        for name in NEEDS_APP {
            assert!(Metrics::ALL_FAMILIES.contains(&name), "{name}");
        }
        for name in Metrics::FAMILIES {
            assert!(Metrics::ALL_FAMILIES.contains(name), "{name}");
        }
    }

    #[test]
    fn announcement_result_labels_are_fixed() {
        assert_eq!(AnnouncementResult::Applied.as_str(), "applied");
        assert_eq!(AnnouncementResult::Current.as_str(), "current");
        assert_eq!(AnnouncementResult::Unknown.as_str(), "unknown");
        assert_eq!(AnnouncementResult::Failed.as_str(), "failed");
    }

    #[test]
    fn outcome_labels_are_fixed() {
        assert_eq!(Outcome::Ok.as_str(), "ok");
        assert_eq!(Outcome::Error.as_str(), "error");
    }

    #[test]
    fn the_task_5_labels_are_fixed_snake_case_words() {
        let limiters = [
            (Limiter::SignInPerIp, "sign_in_per_ip"),
            (Limiter::ForgotPerEmail, "forgot_per_email"),
            (Limiter::ForgotPerIp, "forgot_per_ip"),
            (Limiter::LinkStarts, "link_starts"),
            (Limiter::OauthRegister, "oauth_register"),
            (Limiter::OauthToken, "oauth_token"),
            (Limiter::OauthConsent, "oauth_consent"),
        ];
        for (limiter, label) in limiters {
            assert_eq!(limiter.as_str(), label);
        }
        assert_eq!(Grant::AuthorizationCode.as_str(), "authorization_code");
        assert_eq!(Grant::RefreshToken.as_str(), "refresh_token");
        assert_eq!(MailOutcome::Sent.as_str(), "sent");
        assert_eq!(MailOutcome::Retry.as_str(), "retry");
        assert_eq!(MailOutcome::Failed.as_str(), "failed");
    }

    #[test]
    fn a_mail_status_parses_only_its_own_spelling() {
        for status in MailStatus::ALL {
            assert_eq!(MailStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(MailStatus::parse("done"), None);
        assert_eq!(MailStatus::parse("Pending"), None);
    }

    #[test]
    fn the_buckets_rise_and_span_a_millisecond_to_a_minute() {
        assert!(SECONDS_BUCKETS.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(SECONDS_BUCKETS.first(), Some(&0.001));
        assert_eq!(SECONDS_BUCKETS.last(), Some(&60.0));
    }

    #[test]
    fn a_failed_clock_check_counts_as_an_error_and_keeps_the_last_offset() {
        use crate::mcp::testing::{local_metrics, render, sample_of};
        let (_provider, metrics, registry) = local_metrics();

        metrics.clock_check(Outcome::Ok, Some(250));
        metrics.clock_check(Outcome::Error, None);

        let text = render(&registry);
        assert_eq!(
            sample_of(&text, r#"okf_clock_checks_total{outcome="ok"}"#),
            Some(1.0),
            "{text}"
        );
        assert_eq!(
            sample_of(&text, r#"okf_clock_checks_total{outcome="error"}"#),
            Some(1.0),
            "{text}"
        );
        assert_eq!(
            sample_of(&text, "okf_clock_offset_seconds"),
            Some(0.25),
            "{text}"
        );
    }
}
