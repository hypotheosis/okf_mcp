//! Optional shared state for running several `serve` replicas (spec
//! `2026-11-17-okf-multi-replica-design.md`): one Redis connection and the
//! key names every component uses. Nothing here runs without `[redis]`.

pub mod announce;
pub mod ephemeral;
pub mod limits;
pub mod sessions;

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use redis::aio::{ConnectionLike, ConnectionManager, ConnectionManagerConfig};
use redis::cluster::ClusterClientBuilder;
use redis::cluster_async::ClusterConnection;
use redis::{
    Cmd, ConnectionInfo, IntoConnectionInfo, Pipeline, ProtocolVersion, PushInfo, PushKind,
    RedisFuture, RedisResult, Value,
};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::config::{RedisConfig, RedisMode};
use crate::error::OkfError;
use crate::telemetry::metrics::{Metrics, RedisOp};
use crate::telemetry::subscriber::split_event;

/// How long a connection attempt may take.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long one command may take before it counts as failed.
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);

/// The instance's connection: standalone or cluster (spec Decision 20). Cheap
/// to clone; both arms multiplex one or more sockets and reconnect by
/// themselves.
#[derive(Clone)]
pub enum RedisConnection {
    /// One Redis or Valkey primary.
    Standalone(ConnectionManager),
    /// A cluster, including ElastiCache Serverless.
    Cluster(ClusterConnection),
}

// Both arms already implement the trait; each method forwards to the arm
// (Python: delegating `__getattr__`, written out because Rust has no
// inheritance to do it for us).
impl ConnectionLike for RedisConnection {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trait delegation to the arm: the arm's own call is the span"
        )
    )]
    fn req_packed_command<'a>(&'a mut self, cmd: &'a Cmd) -> RedisFuture<'a, Value> {
        match self {
            Self::Standalone(connection) => connection.req_packed_command(cmd),
            Self::Cluster(connection) => connection.req_packed_command(cmd),
        }
    }

    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trait delegation to the arm: the arm's own call is the span"
        )
    )]
    fn req_packed_commands<'a>(
        &'a mut self,
        pipeline: &'a Pipeline,
        offset: usize,
        count: usize,
    ) -> RedisFuture<'a, Vec<Value>> {
        match self {
            Self::Standalone(connection) => connection.req_packed_commands(pipeline, offset, count),
            Self::Cluster(connection) => connection.req_packed_commands(pipeline, offset, count),
        }
    }

    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trait delegation to the arm: the arm's own call is the span"
        )
    )]
    fn get_db(&self) -> i64 {
        match self {
            Self::Standalone(connection) => connection.get_db(),
            Self::Cluster(connection) => connection.get_db(),
        }
    }
}

/// The instance's Redis: a connection that reconnects by itself, what a
/// dedicated subscription connection needs, the hash-tagged key prefix and the
/// metrics. Cheap to clone (every field is a handle).
#[derive(Clone)]
pub struct Redis {
    // The parsed URL (it may hold a password, so `Debug` omits it): a
    // subscription opens its own connection from it.
    info: Arc<ConnectionInfo>,
    mode: RedisMode,
    connection: RedisConnection,
    // `{<key_prefix>}`: the one hash tag every key and channel carries.
    prefix: Arc<str>,
    metrics: Metrics,
    // Whether the shared rate limiter's last Redis call failed (issue #832):
    // `/healthz` reports it. Shared by every clone of this handle.
    limits_degraded: Arc<AtomicBool>,
}

// Hand-written: the client holds the URL, which may carry a password.
impl std::fmt::Debug for Redis {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "one-line accessor or key formatter: a span would record nothing"
        )
    )]
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Redis")
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

impl Redis {
    /// Connects and checks the server answers, for `serve` start-up.
    ///
    /// # Arguments
    /// * `config` - the `[redis]` table (validated).
    /// * `env` - reads an environment variable (injectable for tests).
    /// * `metrics` - where `okf.redis.errors` is counted.
    ///
    /// # Returns
    /// The handle.
    ///
    /// # Errors
    /// [`OkfError::Config`] (path `[redis]`) when the variable is unset or
    /// blank, its value is not a Redis URL, or Redis does not answer `PING`
    /// within [`CONNECT_TIMEOUT`]. The message names the variable and the
    /// error's kind, never the URL.
    #[tracing::instrument(level = "info", skip_all, fields(db.system.name = "redis", db.operation.name = "connect"))]
    pub async fn connect(
        config: &RedisConfig,
        env: &dyn Fn(&str) -> Option<String>,
        metrics: &Metrics,
    ) -> Result<Self, OkfError> {
        // `rediss://` builds a rustls config from the process default; with
        // two providers compiled in, rustls would panic without this. `Err`
        // only means one is installed already.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let name = &config.url_env;
        let fail = |message: String| OkfError::Config {
            path: "[redis]".to_string(),
            message,
        };
        let url = env(name)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                fail(format!(
                    "redis.url_env names {name}, which is not set or is empty"
                ))
            })?;
        let info = url
            .as_str()
            .into_connection_info()
            .map_err(|_| fail(format!("{name} does not hold a redis:// or rediss:// URL")))?;
        let unreachable = |error: redis::RedisError| {
            metrics.redis_error(RedisOp::Connect);
            fail(format!(
                "the Redis {name} names did not answer ({:?})",
                error.kind()
            ))
        };
        let mut connection = match config.mode {
            RedisMode::Standalone => {
                let client = redis::Client::open(info.clone()).map_err(unreachable)?;
                let manager_config = ConnectionManagerConfig::new()
                    .set_connection_timeout(Some(CONNECT_TIMEOUT))
                    .set_response_timeout(Some(RESPONSE_TIMEOUT))
                    .set_number_of_retries(1);
                RedisConnection::Standalone(
                    ConnectionManager::new_with_config(client, manager_config)
                        .await
                        .map_err(unreachable)?,
                )
            }
            RedisMode::Cluster => RedisConnection::Cluster(
                cluster_builder(&info)
                    .build()
                    .map_err(unreachable)?
                    .get_async_connection()
                    .await
                    .map_err(unreachable)?,
            ),
        };
        let _: String = redis::cmd("PING")
            .query_async(&mut connection)
            .await
            .map_err(unreachable)?;
        Ok(Self {
            info: Arc::new(info),
            mode: config.mode,
            connection,
            prefix: Arc::from(format!("{{{}}}", config.key_prefix)),
            metrics: metrics.clone(),
            limits_degraded: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Whether the shared rate limiter's last Redis call failed, so every
    /// limit is being decided by this replica's own bucket (`/healthz`'s
    /// `rate_limits`).
    ///
    /// # Returns
    /// `true` after a failed call, until a later one succeeds.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "one-line accessor or key formatter: a span would record nothing"
        )
    )]
    pub fn limits_degraded(&self) -> bool {
        self.limits_degraded.load(Ordering::Relaxed)
    }

    /// Records whether the shared rate limiter's last Redis call failed.
    ///
    /// # Arguments
    /// * `degraded` - `true` after a failed call, `false` after a good one.
    ///
    /// # Returns
    /// What the flag was before.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "one-line accessor or key formatter: a span would record nothing"
        )
    )]
    pub fn set_limits_degraded(&self, degraded: bool) -> bool {
        self.limits_degraded.swap(degraded, Ordering::Relaxed)
    }

    /// Counts one rate-limit decision made by a replica's own bucket.
    ///
    /// # Arguments
    /// * `limiter` - which limiter fell back.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn count_fallback(&self, limiter: crate::telemetry::metrics::Limiter) {
        self.metrics.rate_limit_fallback(limiter);
    }

    /// The full key of `id` in `space` (`{<prefix>}<space>:<id>`): the braces
    /// are the hash tag that puts every key on one cluster slot.
    ///
    /// # Arguments
    /// * `space` - the key space (`"oauth:code"`, ...).
    /// * `id` - already hashed where it came from a client ([`hashed`]).
    ///
    /// # Returns
    /// The key.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "one-line accessor or key formatter: a span would record nothing"
        )
    )]
    pub fn key(&self, space: &str, id: &str) -> String {
        format!("{}{space}:{id}", self.prefix)
    }

    /// The pub/sub channel `name` under the prefix (`{<prefix>}<name>`).
    ///
    /// # Arguments
    /// * `name` - the channel's own name.
    ///
    /// # Returns
    /// The channel.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "one-line accessor or key formatter: a span would record nothing"
        )
    )]
    pub fn channel(&self, name: &str) -> String {
        format!("{}{name}", self.prefix)
    }

    /// A handle on the shared connection, for one call (a clone is cheap:
    /// every clone multiplexes the same socket or sockets).
    ///
    /// # Returns
    /// The connection.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "one-line accessor or key formatter: a span would record nothing"
        )
    )]
    pub fn connection(&self) -> RedisConnection {
        self.connection.clone()
    }

    /// Opens a dedicated RESP3 connection with a push sender and automatic
    /// resubscription, and subscribes it to `channel`.
    ///
    /// # Arguments
    /// * `channel` - a name from [`Redis::channel`].
    ///
    /// # Returns
    /// The live [`Subscription`]; its first event is
    /// [`SubscriptionEvent::Subscribed`].
    ///
    /// # Errors
    /// The driver's error when the connection or the `SUBSCRIBE` fails.
    #[tracing::instrument(level = "info", skip_all, fields(db.system.name = "redis", db.operation.name = "subscribe"))]
    pub async fn subscribe(&self, channel: &str) -> RedisResult<Subscription> {
        // Unbounded: a bounded channel that dropped on overflow could lose a
        // `Subscribed`, and the library's sender must never block.
        let (sender, events) = tokio::sync::mpsc::unbounded_channel::<PushInfo>();
        let connection = match self.mode {
            RedisMode::Standalone => {
                let settings = self
                    .info
                    .redis_settings()
                    .clone()
                    .set_protocol(ProtocolVersion::RESP3);
                let client =
                    redis::Client::open(self.info.as_ref().clone().set_redis_settings(settings))?;
                let config = ConnectionManagerConfig::new()
                    .set_connection_timeout(Some(CONNECT_TIMEOUT))
                    .set_response_timeout(Some(RESPONSE_TIMEOUT))
                    .set_push_sender(sender)
                    .set_automatic_resubscription();
                let mut manager = ConnectionManager::new_with_config(client, config).await?;
                manager.subscribe(channel).await?;
                RedisConnection::Standalone(manager)
            }
            RedisMode::Cluster => {
                let mut cluster = cluster_builder(&self.info)
                    .use_protocol(ProtocolVersion::RESP3)
                    .push_sender(sender)
                    .build()?
                    .get_async_connection()
                    .await?;
                cluster.subscribe(channel).await?;
                RedisConnection::Cluster(cluster)
            }
        };
        Ok(Subscription {
            events,
            _connection: connection,
        })
    }

    /// Counts and logs a failed call, and gives the error a request answers
    /// with: 503, retryable.
    ///
    /// # Arguments
    /// * `op` - what the call was doing.
    /// * `error` - the driver's error (its text stays local).
    ///
    /// # Returns
    /// [`OkfError::Busy`].
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn failed(&self, op: RedisOp, error: &redis::RedisError) -> OkfError {
        self.metrics.redis_error(op);
        split_event!(
            error,
            { operation = op.as_str(), kind = ?error.kind(), },
            { %error, },
            "a Redis call failed"
        );
        OkfError::Busy {
            reason: "the shared session store is unavailable".to_string(),
        }
    }

    /// Counts a failure that has no driver error (a pub/sub stream that
    /// ended).
    ///
    /// # Arguments
    /// * `op` - what was failing.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn count(&self, op: RedisOp) {
        self.metrics.redis_error(op);
    }

    /// Counts a value that would not decode (another version, or corrupt).
    ///
    /// # Arguments
    /// * `space` - its key space (a closed set, safe to log).
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn undecodable(&self, space: &'static str) {
        self.metrics.redis_error(RedisOp::Decode);
        tracing::warn!(space, "a stored value did not decode; treated as absent");
    }
}

/// A cluster client builder over `info` with this module's timeouts.
///
/// # Arguments
/// * `info` - the parsed URL (TLS comes from `rediss://`).
///
/// # Returns
/// The builder, for the caller to add a protocol and a push sender.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "builds a client builder without I/O: a span would record nothing"
    )
)]
fn cluster_builder(info: &ConnectionInfo) -> ClusterClientBuilder {
    ClusterClientBuilder::new(vec![info.clone()])
        .connection_timeout(CONNECT_TIMEOUT)
        .response_timeout(RESPONSE_TIMEOUT)
}

/// One event from a subscription (spec Decision 20).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubscriptionEvent {
    /// The server confirmed the subscription: the first time, and again after
    /// every automatic resubscribe. Messages sent before it may be lost.
    Subscribed,
    /// A message on the channel, its payload's bytes.
    Message(Vec<u8>),
    /// The library lost the connection; a `Subscribed` follows when it is back.
    Disconnected,
}

/// A live subscription; dropping it closes its connection.
pub struct Subscription {
    events: UnboundedReceiver<PushInfo>,
    // Held only to keep the connection (and its resubscribing task) alive.
    _connection: RedisConnection,
}

impl Subscription {
    /// The next event.
    ///
    /// # Returns
    /// The event, or `None` once the connection is gone for good.
    #[tracing::instrument(level = "trace", skip_all)]
    pub async fn next(&mut self) -> Option<SubscriptionEvent> {
        loop {
            let push = self.events.recv().await?;
            match push.kind {
                PushKind::Subscribe => return Some(SubscriptionEvent::Subscribed),
                PushKind::Disconnection => return Some(SubscriptionEvent::Disconnected),
                PushKind::Message => {
                    // `data` is `[channel, payload]`; the payload is last.
                    if let Some(Value::BulkString(payload)) = push.data.into_iter().next_back() {
                        return Some(SubscriptionEvent::Message(payload));
                    }
                }
                // Other kinds (pattern, sharded, invalidation) are not used.
                _ => {}
            }
        }
    }
}

/// The hex SHA-256 of a client-presented id, the only form an id takes in a
/// key name (spec Decision 7).
///
/// # Arguments
/// * `id` - the id as presented.
///
/// # Returns
/// 64 lowercase hex characters.
#[tracing::instrument(level = "debug", skip_all)]
pub fn hashed(id: &str) -> String {
    // `fold` with `write!` appends each byte's two hex digits to one
    // preallocated `String` (Python: `hashlib.sha256(...).hexdigest()`).
    crate::keys::hash(id)
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            // Writing to a `String` cannot fail.
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// Helpers for the library's unit tests.
#[cfg(test)]
pub(crate) mod testing {
    use super::Redis;
    use crate::config::RedisConfig;
    use crate::config::RedisMode;
    use crate::db::redis_support::{TestRedis, URL_VAR};
    use crate::telemetry::metrics::Metrics;

    /// The `[redis]` table that points at `server`, with its random prefix.
    pub(crate) fn config(server: &TestRedis) -> RedisConfig {
        RedisConfig {
            url_env: URL_VAR.to_string(),
            key_prefix: server.prefix.clone(),
            mode: if server.cluster {
                RedisMode::Cluster
            } else {
                RedisMode::Standalone
            },
        }
    }

    /// Connects to `server` through [`Redis::connect`].
    pub(crate) async fn connect(server: &TestRedis) -> Redis {
        Redis::connect(&config(server), &server.env(), &Metrics::global())
            .await
            .expect("connect to the test redis")
    }

    /// How many connections are subscribed to `name` (a channel under the
    /// prefix), as `PUBSUB NUMSUB` counts them on a standalone server.
    pub(crate) async fn subscribers(redis: &Redis, name: &str) -> i64 {
        let (_channel, count): (String, i64) = redis::cmd("PUBSUB")
            .arg("NUMSUB")
            .arg(redis.channel(name))
            .query_async(&mut redis.connection())
            .await
            .expect("PUBSUB NUMSUB");
        count
    }

    /// Starts `app`'s catalog announcement subscriber and waits (up to 10 s)
    /// until Redis counts it.
    ///
    /// # Returns
    /// The token that stops it and its task.
    pub(crate) async fn listening_for_catalogs(
        app: &std::sync::Arc<crate::app::App>,
    ) -> (
        tokio_util::sync::CancellationToken,
        tokio::task::JoinHandle<()>,
    ) {
        use super::announce::{listen_catalogs, CATALOG_CHANNEL};
        let redis = app.redis().expect("the app has redis").clone();
        let before = subscribers(&redis, CATALOG_CHANNEL).await;
        let shutdown = tokio_util::sync::CancellationToken::new();
        let task = tokio::spawn(listen_catalogs(
            std::sync::Arc::clone(app),
            shutdown.clone(),
        ));
        let subscribed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while subscribers(&redis, CATALOG_CHANNEL).await <= before {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(subscribed.is_ok(), "the catalog subscriber subscribed");
        (shutdown, task)
    }

    /// The value at `key` as text, whatever its type: a string as stored, a
    /// hash (a rate-limit bucket) as its fields and values joined by spaces.
    pub(crate) async fn value_text(redis: &Redis, key: &str) -> String {
        let mut connection = redis.connection();
        let kind: String = redis::cmd("TYPE")
            .arg(key)
            .query_async(&mut connection)
            .await
            .expect("TYPE");
        if kind == "hash" {
            let fields: Vec<String> = redis::cmd("HGETALL")
                .arg(key)
                .query_async(&mut connection)
                .await
                .expect("HGETALL");
            return fields.join(" ");
        }
        let value: Vec<u8> = redis::cmd("GET")
            .arg(key)
            .query_async(&mut connection)
            .await
            .expect("GET");
        String::from_utf8_lossy(&value).into_owned()
    }

    /// The value of `okf_redis_errors_total{operation="<op>"}` in a scrape.
    pub(crate) fn error_count(rendered: &str, op: &str) -> u64 {
        let series = format!("okf_redis_errors_total{{operation=\"{op}\"}}");
        rendered
            .lines()
            .find_map(|line| line.strip_prefix(series.as_str()))
            .map_or(0, |value| {
                // Prometheus renders a counter as a float: "1" or "1.0".
                value.trim().parse::<f64>().expect("a numeric sample") as u64
            })
    }
}

#[cfg(test)]
mod tests {
    use super::testing::error_count;
    use super::*;

    #[tokio::test]
    async fn connect_refuses_an_unset_or_blank_variable_and_names_only_it() {
        let config = RedisConfig {
            url_env: "OKF_TEST_REDIS_URL".into(),
            key_prefix: "okf:".into(),
            mode: RedisMode::Standalone,
        };
        for value in [None, Some("  ")] {
            let error = Redis::connect(&config, &|_| value.map(str::to_string), &Metrics::global())
                .await
                .unwrap_err();
            let message = error.to_string();
            assert!(message.contains("OKF_TEST_REDIS_URL"), "{message}");
        }
    }

    #[tokio::test]
    async fn connect_refuses_an_unreachable_or_malformed_url_without_printing_it() {
        let config = RedisConfig {
            url_env: "OKF_TEST_REDIS_URL".into(),
            key_prefix: "okf:".into(),
            mode: RedisMode::Standalone,
        };
        for url in [
            "redis://:s3cretpw@127.0.0.1:1/0",
            "http://user:s3cretpw@example.com",
        ] {
            let error = Redis::connect(&config, &|_| Some(url.to_string()), &Metrics::global())
                .await
                .unwrap_err();
            let message = error.to_string();
            assert!(
                !message.contains("s3cretpw")
                    && !message.contains("127.0.0.1")
                    && !message.contains("example.com"),
                "{message}"
            );
            assert!(message.contains("OKF_TEST_REDIS_URL"), "{message}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn connect_answers_a_live_redis() {
        let server = crate::db::redis_support::TestRedis::start().await;
        let redis = tokio::time::timeout(Duration::from_secs(60), testing::connect(&server))
            .await
            .expect("connecting to the test redis within 60 s");
        assert_eq!(
            redis.key("oauth:code", "abc"),
            format!("{{{}}}oauth:code:abc", server.prefix)
        );
        assert_eq!(
            redis.channel("announce"),
            format!("{{{}}}announce", server.prefix)
        );
        assert!(!format!("{redis:?}").contains(&server.url));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn two_replicas_share_one_redis_and_a_single_app_has_one() {
        let (first, second, _redis) = tokio::time::timeout(
            Duration::from_secs(120),
            crate::mcp::testing::replica_pair(&[], crate::config::Config::default()),
        )
        .await
        .expect("starting two replicas within 120 s");
        let (one, two) = (
            first.app.redis().expect("the first app's redis"),
            second.redis().expect("the second app's redis"),
        );
        let key = one.key("test", "shared");
        assert_eq!(key, two.key("test", "shared"));
        let mut connection = one.connection();
        let _: () = redis::cmd("SET")
            .arg(&key)
            .arg("42")
            .query_async(&mut connection)
            .await
            .expect("set through the first replica");
        let mut other = two.connection();
        let read: Option<String> = redis::cmd("GET")
            .arg(&key)
            .query_async(&mut other)
            .await
            .expect("get through the second replica");
        assert_eq!(read.as_deref(), Some("42"));

        let (single, _redis) =
            crate::mcp::testing::app_with_redis(crate::config::Config::default()).await;
        assert!(single.app.redis().is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn app_load_refuses_a_redis_table_whose_variable_is_unset() {
        let db = crate::db::test_db().await;
        let config = crate::config::Config {
            redis: Some(RedisConfig {
                url_env: "OKF_TEST_REDIS_URL_NEVER_SET".into(),
                key_prefix: "okf:".into(),
                mode: RedisMode::Standalone,
            }),
            ..crate::config::Config::default()
        };
        let error =
            crate::app::App::load(db.pool.clone(), config, Arc::new(crate::clock::SystemClock))
                .await
                .map(|_| ())
                .unwrap_err();
        let message = error.to_string();
        assert!(message.starts_with("[redis]:"), "{message}");
        assert!(
            message.contains("OKF_TEST_REDIS_URL_NEVER_SET"),
            "{message}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn failures_are_counted_by_operation_and_map_to_a_busy_without_the_driver_text() {
        let server = crate::db::redis_support::TestRedis::start().await;
        let (_provider, metrics, registry) = crate::mcp::testing::local_metrics();
        let redis = tokio::time::timeout(
            Duration::from_secs(60),
            Redis::connect(&testing::config(&server), &server.env(), &metrics),
        )
        .await
        .expect("connecting to the test redis within 60 s")
        .expect("connect to the test redis");

        let error = redis::RedisError::from((redis::ErrorKind::Io, "sentinel-secret"));
        let mapped = redis.failed(RedisOp::Get, &error);
        assert!(matches!(mapped, OkfError::Busy { .. }), "{mapped:?}");
        assert!(!mapped.to_string().contains("sentinel-secret"), "{mapped}");
        redis.count(RedisOp::Subscribe);
        redis.undecodable("oauth:code");

        let rendered = crate::mcp::testing::render(&registry);
        for op in ["get", "subscribe", "decode"] {
            assert_eq!(error_count(&rendered, op), 1, "{op}: {rendered}");
        }
        assert_eq!(error_count(&rendered, "set"), 0, "{rendered}");
        // A connect that works counts nothing.
        assert_eq!(error_count(&rendered, "connect"), 0, "{rendered}");
    }

    #[tokio::test]
    async fn a_failed_connect_counts_one_connect_error() {
        let (_provider, metrics, registry) = crate::mcp::testing::local_metrics();
        let config = RedisConfig {
            url_env: "OKF_TEST_REDIS_URL".into(),
            key_prefix: "okf:".into(),
            mode: RedisMode::Standalone,
        };
        Redis::connect(
            &config,
            &|_| Some("redis://127.0.0.1:1/0".to_string()),
            &metrics,
        )
        .await
        .unwrap_err();
        let rendered = crate::mcp::testing::render(&registry);
        assert!(error_count(&rendered, "connect") >= 1, "{rendered}");
    }

    /// The hash tag of `name`: the text between the first `{` and the next `}`.
    fn hash_tag(name: &str) -> &str {
        let start = name.find('{').expect("an opening brace") + 1;
        let end = start + name[start..].find('}').expect("a closing brace");
        &name[start..end]
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn keys_and_channels_from_one_handle_share_one_hash_tag() {
        let server = crate::db::redis_support::TestRedis::start().await;
        let redis = testing::connect(&server).await;
        let names = [
            redis.key("oauth:code", "a"),
            redis.key("mcp:session", "b"),
            redis.channel("announce"),
        ];
        for name in &names {
            assert_eq!(hash_tag(name), server.prefix, "{name}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_cluster_connects_answers_ping_and_loads_an_app() {
        let server = crate::db::redis_support::TestRedis::start_cluster().await;
        let redis = tokio::time::timeout(Duration::from_secs(60), testing::connect(&server))
            .await
            .expect("connecting to the test cluster within 60 s");
        assert!(matches!(redis.connection(), RedisConnection::Cluster(_)));
        let mut connection = redis.connection();
        let pong: String = redis::cmd("PING")
            .query_async(&mut connection)
            .await
            .expect("PING the cluster");
        assert_eq!(pong, "PONG");
        let (app, _server) = tokio::time::timeout(
            Duration::from_secs(120),
            crate::mcp::testing::app_on_redis(crate::config::Config::default(), server),
        )
        .await
        .expect("loading an app on the cluster within 120 s");
        assert!(app.app.redis().is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_cluster_runs_a_script_over_tagged_keys_and_refuses_untagged_ones() {
        let server = crate::db::redis_support::TestRedis::start_cluster().await;
        let redis = testing::connect(&server).await;
        let script =
            redis::Script::new("redis.call('SET', KEYS[1], 1); return redis.call('GET', KEYS[2])");
        let mut connection = redis.connection();
        let (first, second) = (redis.key("test", "a"), redis.key("test", "b"));
        let value: Option<String> = script
            .key(&first)
            .key(&second)
            .invoke_async(&mut connection)
            .await
            .expect("a script over two tagged keys");
        assert_eq!(value, None);
        // Without the tag the two keys land on different slots (this is what
        // proves the container is a real cluster).
        let error = script
            .key("okf:a")
            .key("okf:b")
            .invoke_async::<Option<String>>(&mut connection)
            .await
            .expect_err("untagged keys must not run");
        // The cluster client refuses it before sending (a `CrossSlot` error);
        // a server would answer `CROSSSLOT`.
        assert!(
            error.to_string().to_uppercase().contains("CROSSSLOT"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_failed_cluster_connect_names_the_variable_and_no_secret() {
        let config = RedisConfig {
            url_env: "OKF_TEST_REDIS_URL".into(),
            key_prefix: "okf:".into(),
            mode: RedisMode::Cluster,
        };
        let error = Redis::connect(
            &config,
            &|_| Some("redis://:s3cretpw@127.0.0.1:1".to_string()),
            &Metrics::global(),
        )
        .await
        .unwrap_err();
        let message = error.to_string();
        assert!(
            !message.contains("s3cretpw") && !message.contains("127.0.0.1"),
            "{message}"
        );
        assert!(message.contains("OKF_TEST_REDIS_URL"), "{message}");
    }

    /// The next event within 20 s.
    async fn next_event(subscription: &mut Subscription) -> SubscriptionEvent {
        tokio::time::timeout(Duration::from_secs(20), subscription.next())
            .await
            .expect("an event within 20 s")
            .expect("the subscription is alive")
    }

    /// Publishes `payload` on `channel` until the subscription yields it (a
    /// message sent while a resubscribe is under way may be lost, so retry),
    /// skipping the events that are not it.
    async fn publish_until_received(
        redis: &Redis,
        subscription: &mut Subscription,
        channel: &str,
        payload: &[u8],
    ) {
        let mut connection = redis.connection();
        for _ in 0..40 {
            let _: i64 = redis::cmd("PUBLISH")
                .arg(channel)
                .arg(payload)
                .query_async(&mut connection)
                .await
                .expect("publish");
            if let Ok(Some(SubscriptionEvent::Message(got))) =
                tokio::time::timeout(Duration::from_millis(500), subscription.next()).await
            {
                assert_eq!(got, payload);
                return;
            }
        }
        panic!("the message never arrived");
    }

    /// Subscribe, publish, kill the pub/sub connections, and publish again.
    async fn subscription_survives_a_killed_connection(
        server: crate::db::redis_support::TestRedis,
    ) {
        // `CLIENT KILL TYPE pubsub` below ends every subscriber on the server,
        // so a shared server's other tests would lose theirs.
        let server = server.dedicated().await;
        let redis = testing::connect(&server).await;
        let channel = redis.channel("announce");
        let mut subscription = redis.subscribe(&channel).await.expect("subscribe");
        assert_eq!(
            next_event(&mut subscription).await,
            SubscriptionEvent::Subscribed
        );
        let mut connection = redis.connection();
        let receivers: i64 = redis::cmd("PUBLISH")
            .arg(&channel)
            .arg("one")
            .query_async(&mut connection)
            .await
            .expect("publish");
        assert_eq!(receivers, 1);
        assert_eq!(
            next_event(&mut subscription).await,
            SubscriptionEvent::Message(b"one".to_vec())
        );

        // On a cluster the subscriber sits on one node; killing by type reaches
        // it (the one-node stand-in has only that node).
        let _: i64 = redis::cmd("CLIENT")
            .arg("KILL")
            .arg("TYPE")
            .arg("pubsub")
            .query_async(&mut connection)
            .await
            .expect("kill the pubsub clients");
        let mut again = false;
        for _ in 0..4 {
            match next_event(&mut subscription).await {
                SubscriptionEvent::Subscribed => {
                    again = true;
                    break;
                }
                SubscriptionEvent::Disconnected => {}
                SubscriptionEvent::Message(other) => panic!("unexpected message {other:?}"),
            }
        }
        assert!(again, "no Subscribed after the kill");
        publish_until_received(&redis, &mut subscription, &channel, b"two").await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_standalone_subscription_resubscribes_after_its_connection_is_killed() {
        let server = crate::db::redis_support::TestRedis::start().await;
        tokio::time::timeout(
            Duration::from_secs(120),
            subscription_survives_a_killed_connection(server),
        )
        .await
        .expect("the standalone subscription test within 120 s");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_cluster_subscription_resubscribes_after_its_connection_is_killed() {
        let server = crate::db::redis_support::TestRedis::start_cluster().await;
        tokio::time::timeout(
            Duration::from_secs(120),
            subscription_survives_a_killed_connection(server),
        )
        .await
        .expect("the cluster subscription test within 120 s");
    }

    #[test]
    fn hashed_is_the_hex_sha256() {
        assert_eq!(
            hashed("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
