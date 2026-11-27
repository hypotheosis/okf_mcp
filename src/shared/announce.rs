//! Redis pub/sub announcements: catalog generations (issue #659).
//!
//! After a publish, an import or a repair commits, the writer publishes
//! `<catalog id> <generation>`; every instance's subscriber refreshes that
//! catalog's snapshot ([`App::apply_catalog_announcement`]), so other
//! replicas normally pick the write up well before `catalog_ttl`. Best
//! effort: the writer does not wait for the publish or for anyone's refresh,
//! so this is prompt eventual consistency, not read-your-writes.
//!
//! Messages are not stored: one sent while an instance is not subscribed is
//! lost. So each subscriber catches up once after every (re)subscribe, and
//! the change feed and the cache TTLs (`catalog_ttl`, `credential_ttl`)
//! stay as the backstop. The subscriber's loop (`run`) reconnects with
//! backoff and grace, with a `Handler` saying what a message and a
//! resubscribe do.

use std::sync::Arc;
use std::time::Duration;

use redis::AsyncCommands;
use tokio_util::sync::CancellationToken;

use crate::app::App;
use crate::backoff::Backoff;
use crate::config::Config;
use crate::shared::{Redis, SubscriptionEvent};
use crate::store::catalogs::CatalogGeneration;
use crate::telemetry::metrics::{Metrics, RedisOp};

/// The catalog generation channel's name under the prefix.
pub const CATALOG_CHANNEL: &str = "catalog-generation";
/// The first wait before resubscribing; doubles up to [`BACKOFF_MAX`].
const BACKOFF_MIN: Duration = Duration::from_secs(1);
/// The longest wait between resubscribe attempts.
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// How long a subscription may stay disconnected before it is replaced. The
/// driver resubscribes by itself but gives up after a few failed attempts (a
/// Redis that stays down longer), and then no `Subscribed` would ever follow.
const RESUBSCRIBE_GRACE: Duration = Duration::from_secs(10);

/// Publishes a catalog's new generation (issue #659). A failure is counted
/// and logged; the change feed's `catalog` message (and, failing both,
/// `catalog_ttl`) covers a lost announcement.
///
/// # Arguments
/// * `redis` - the shared Redis (owned: [`App::announce_catalog`] runs this
///   on its own task).
/// * `change` - the catalog and the generation its write committed.
#[tracing::instrument(
    level = "debug",
    skip_all,
    fields(db.system.name = "redis", db.operation.name = "publish")
)]
pub async fn publish_catalog(redis: Redis, change: CatalogGeneration) {
    let published: redis::RedisResult<usize> = redis
        .connection()
        .publish(redis.channel(CATALOG_CHANNEL), catalog_payload(change))
        .await;
    if let Err(error) = published {
        let _ = redis.failed(RedisOp::Publish, &error);
    }
}

/// A catalog announcement's payload: `<catalog id> <generation>`.
///
/// # Arguments
/// * `change` - what to announce.
///
/// # Returns
/// The payload text.
#[tracing::instrument(level = "trace", skip_all)]
fn catalog_payload(change: CatalogGeneration) -> String {
    format!("{} {}", change.catalog_id, change.generation)
}

/// The catalog and generation a catalog announcement names; `None` for any
/// other payload, which the subscriber answers by checking every loaded
/// catalog ([`App::catch_up_catalogs`]), as for a resubscribe.
///
/// # Arguments
/// * `payload` - the message's bytes.
///
/// # Returns
/// The announced change, if the payload is `<id> <generation>`.
#[tracing::instrument(level = "trace", skip_all)]
fn announced_catalog(payload: &[u8]) -> Option<CatalogGeneration> {
    let text = std::str::from_utf8(payload).ok()?;
    let (id, generation) = text.split_once(' ')?;
    Some(CatalogGeneration {
        catalog_id: id.parse().ok()?,
        generation: generation.parse().ok()?,
    })
}

/// After a CLI command, announces each catalog in `catalogs` (an import or a
/// repair the command committed) on the catalog channel (issue #659).
/// Administrative changes need no announcement: serving instances learn of
/// them through the change feed. Best effort: the change is committed
/// whatever happens here, so a missing variable or an unreachable Redis is a
/// `warn`, never a failure. Nothing printed names the URL.
///
/// # Arguments
/// * `config` - the loaded configuration; without `[redis]` nothing happens.
/// * `env` - reads an environment variable (injectable for tests).
/// * `metrics` - where `okf.redis.errors` is counted.
/// * `catalogs` - the catalog generations the command committed.
#[tracing::instrument(level = "debug", skip_all)]
pub async fn announce_from_cli(
    config: &Config,
    env: &dyn Fn(&str) -> Option<String>,
    metrics: &Metrics,
    catalogs: &[CatalogGeneration],
) {
    let Some(table) = &config.redis else { return };
    if catalogs.is_empty() {
        return;
    }
    match Redis::connect(table, env, metrics).await {
        Ok(redis) => {
            for change in catalogs {
                publish_catalog(redis.clone(), *change).await;
            }
        }
        // The error's text names the variable and a kind, never the URL, but
        // only the kind is logged: the rest adds nothing a log reader needs.
        Err(error) => tracing::warn!(
            kind = error.kind(),
            "announcement skipped; serving instances notice the change through the change feed"
        ),
    }
}

/// Runs until `shutdown`: subscribes to catalog announcements, checks every
/// loaded catalog once (messages sent while it was not subscribed are lost),
/// then refreshes the catalog each announcement names
/// ([`App::apply_catalog_announcement`]). Messages are answered one at a
/// time, in order. Reconnects with backoff when the connection ends. Returns at once without
/// `[redis]`.
///
/// # Arguments
/// * `app` - the instance whose snapshots are refreshed.
/// * `shutdown` - cancelled when the server stops.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "long-lived loop: a span would stay open until shutdown"
    )
)]
pub async fn listen_catalogs(app: Arc<App>, shutdown: CancellationToken) {
    let Some(redis) = app.redis().cloned() else {
        return;
    };
    let source = RedisSource {
        redis: redis.clone(),
        channel: CATALOG_CHANNEL,
    };
    run(
        &Snapshots(&app),
        &redis,
        &source,
        RESUBSCRIBE_GRACE,
        &shutdown,
    )
    .await;
}

/// What a subscriber does with its channel's events.
#[async_trait::async_trait]
trait Handler: Send + Sync {
    /// Names this subscriber in its log lines (`catalog`): a
    /// closed set, never a channel payload, key or URL.
    const NAME: &'static str;

    /// After every (re)subscribe: catch up on whatever was missed.
    async fn subscribed(&self);

    /// On one message.
    async fn message(&self, payload: &[u8]);
}

/// The catalog subscriber: refreshes catalog snapshots.
struct Snapshots<'a>(&'a App);

#[async_trait::async_trait]
impl Handler for Snapshots<'_> {
    const NAME: &'static str = "catalog";

    #[tracing::instrument(level = "debug", skip_all)]
    async fn subscribed(&self) {
        self.0.catch_up_catalogs().await;
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn message(&self, payload: &[u8]) {
        match announced_catalog(payload) {
            Some(change) => {
                self.0.apply_catalog_announcement(change).await;
            }
            None => self.0.catch_up_catalogs().await,
        }
    }
}

/// Where the subscriber's events come from: a live [`crate::shared::Subscription`]
/// in production, a script in tests.
#[async_trait::async_trait]
trait Events: Send {
    /// The next event, or `None` once the stream is gone for good.
    async fn next(&mut self) -> Option<SubscriptionEvent>;
}

/// Opens subscriptions; injectable so a test can script outages.
#[async_trait::async_trait]
trait Source: Send + Sync {
    /// The stream type one subscription yields.
    type Stream: Events;

    /// Subscribes to the announcement channel.
    async fn subscribe(&self) -> redis::RedisResult<Self::Stream>;
}

/// The real source: a subscription to one channel on the shared Redis.
struct RedisSource {
    /// The shared Redis.
    redis: Redis,
    /// The channel's name under the prefix.
    channel: &'static str,
}

#[async_trait::async_trait]
impl Events for crate::shared::Subscription {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "long-lived loop or thin delegate: a span would record nothing"
        )
    )]
    async fn next(&mut self) -> Option<SubscriptionEvent> {
        crate::shared::Subscription::next(self).await
    }
}

#[async_trait::async_trait]
impl Source for RedisSource {
    type Stream = crate::shared::Subscription;

    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "long-lived loop or thin delegate: a span would record nothing"
        )
    )]
    async fn subscribe(&self) -> redis::RedisResult<Self::Stream> {
        self.redis
            .subscribe(&self.redis.channel(self.channel))
            .await
    }
}

/// A duration in whole milliseconds, saturating, for a log field.
///
/// # Arguments
/// * `duration` - the span to report.
///
/// # Returns
/// Its milliseconds.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "one-line accessor or key formatter: a span would record nothing"
    )
)]
fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// The subscriber's loop, with its handler, subscription source and grace
/// period injected: subscribes, calls the handler on `Subscribed` and on
/// every message, replaces a subscription that stays `Disconnected` for
/// `grace`, and subscribes again with backoff after the stream ends or a
/// subscribe fails.
///
/// # Arguments
/// * `handler` - what `Subscribed` and a message do.
/// * `redis` - where failures are counted.
/// * `source` - opens each subscription.
/// * `grace` - how long a subscription may stay disconnected.
/// * `shutdown` - cancelled when the server stops.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "long-lived loop: a span would stay open until shutdown"
    )
)]
async fn run<H: Handler, S: Source>(
    handler: &H,
    redis: &Redis,
    source: &S,
    grace: Duration,
    shutdown: &CancellationToken,
) {
    let mut backoff = Backoff::new(BACKOFF_MIN, BACKOFF_MAX);
    // Whether any subscription of this loop has been reported before.
    let mut subscribed_before = false;
    // When the loop lost its subscription, until the next `Subscribed`. It
    // outlives a replaced subscription and failed subscribes, so the recovery
    // line reports the whole outage.
    let mut outage_began: Option<tokio::time::Instant> = None;
    while !shutdown.is_cancelled() {
        match source.subscribe().await {
            Ok(mut subscription) => {
                // When the library reported the connection lost, until it
                // reports the subscription again; it times this subscription's
                // grace, while `outage_began` times the log's outage.
                let mut disconnected_at: Option<tokio::time::Instant> = None;
                loop {
                    // Never ready while connected; `pending` waits forever.
                    let replace_at = async {
                        match disconnected_at {
                            Some(since) => tokio::time::sleep_until(since + grace).await,
                            None => std::future::pending().await,
                        }
                    };
                    let event = tokio::select! {
                        () = shutdown.cancelled() => return,
                        // Drop the subscription; the code below backs off and
                        // subscribes again.
                        () = replace_at => {
                            tracing::warn!(
                                subscriber = H::NAME,
                                grace_ms = duration_ms(grace),
                                "pub/sub subscription stayed disconnected past the grace; \
                                 replacing it"
                            );
                            None
                        }
                        event = subscription.next() => event,
                    };
                    match event {
                        // The first subscribe and every automatic resubscribe:
                        // messages sent before it may have been lost, so catch up.
                        Some(SubscriptionEvent::Subscribed) => {
                            backoff.reset();
                            tracing::info!(
                                subscriber = H::NAME,
                                resubscribe = subscribed_before,
                                disconnected_ms =
                                    outage_began
                                        .filter(|_| subscribed_before)
                                        .map(|since| duration_ms(since.elapsed())),
                                "pub/sub subscribed; catching up on missed announcements"
                            );
                            subscribed_before = true;
                            disconnected_at = None;
                            outage_began = None;
                            handler.subscribed().await;
                        }
                        Some(SubscriptionEvent::Message(payload)) => {
                            handler.message(&payload).await;
                        }
                        // The library resubscribes by itself; a `Subscribed`
                        // follows, or the grace above replaces the subscription.
                        Some(SubscriptionEvent::Disconnected) => {
                            redis.count(RedisOp::Subscribe);
                            if disconnected_at.is_none() {
                                tracing::warn!(
                                    subscriber = H::NAME,
                                    grace_ms = duration_ms(grace),
                                    "pub/sub connection lost; waiting for the library to \
                                     resubscribe"
                                );
                                disconnected_at = Some(tokio::time::Instant::now());
                            }
                            outage_began.get_or_insert_with(tokio::time::Instant::now);
                        }
                        None => {
                            redis.count(RedisOp::Subscribe);
                            outage_began.get_or_insert_with(tokio::time::Instant::now);
                            tracing::warn!(
                                subscriber = H::NAME,
                                "pub/sub subscription ended; subscribing again after a backoff"
                            );
                            break;
                        }
                    }
                }
            }
            Err(error) => {
                let _ = redis.failed(RedisOp::Subscribe, &error);
                outage_began.get_or_insert_with(tokio::time::Instant::now);
                tracing::warn!(
                    subscriber = H::NAME,
                    "pub/sub subscribe failed; retrying after a backoff"
                );
            }
        }
        tokio::select! {
            () = shutdown.cancelled() => return,
            () = tokio::time::sleep(backoff.next_delay()) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{announced_catalog, catalog_payload};
    use crate::store::catalogs::CatalogGeneration;

    #[test]
    fn a_catalog_payload_round_trips_and_anything_else_names_none() {
        let change = CatalogGeneration {
            catalog_id: 7,
            generation: 42,
        };
        assert_eq!(catalog_payload(change), "7 42");
        assert_eq!(
            announced_catalog(catalog_payload(change).as_bytes()),
            Some(change)
        );
        for other in [&b""[..], b"7", b"7 x", b"x 42", b"7 42 1", &[0xff, 0xfe]] {
            assert_eq!(announced_catalog(other), None, "{other:?}");
        }
    }
}

#[cfg(test)]
mod catalog_tests;

/// What `run` logs about the subscription's life (issue #592).
#[cfg(test)]
mod log_tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::time::Duration;

    use tokio_util::sync::CancellationToken;

    use super::{run, Events, Handler, Source};
    use crate::db::redis_support::TestRedis;
    use crate::shared::SubscriptionEvent;
    use crate::telemetry::subscriber::capture::Capture;

    /// A handler that does nothing, named like the catalog subscriber.
    struct Quiet;

    #[async_trait::async_trait]
    impl Handler for Quiet {
        const NAME: &'static str = "catalog";

        #[cfg_attr(
            dylint_lib = "hyp_tracing",
            allow(uninstrumented_fn, reason = "test double")
        )]
        async fn subscribed(&self) {}

        #[cfg_attr(
            dylint_lib = "hyp_tracing",
            allow(uninstrumented_fn, reason = "test double")
        )]
        async fn message(&self, _payload: &[u8]) {}
    }

    /// Yields its events, each after its delay on the (paused) clock, then
    /// ends the stream or waits forever.
    struct Script(VecDeque<(Duration, SubscriptionEvent)>, bool);

    /// An event with no delay.
    fn now(event: SubscriptionEvent) -> (Duration, SubscriptionEvent) {
        (Duration::ZERO, event)
    }

    #[async_trait::async_trait]
    impl Events for Script {
        #[cfg_attr(
            dylint_lib = "hyp_tracing",
            allow(uninstrumented_fn, reason = "test double")
        )]
        async fn next(&mut self) -> Option<SubscriptionEvent> {
            match self.0.pop_front() {
                Some((delay, event)) => {
                    tokio::time::sleep(delay).await;
                    Some(event)
                }
                None if self.1 => None,
                None => std::future::pending().await,
            }
        }
    }

    /// Attempt 1: `Subscribed`, `Disconnected`, a second `Disconnected` 2 s
    /// later, `Subscribed` 5 s after that (a 7 s loss), then the stream ends. Attempt 2: `Disconnected` and silence (replaced after the grace).
    /// Attempt 3: fails. Attempt 4: `Subscribed`, then the stream ends. Attempt 5 stops the loop.
    struct Lifetimes {
        attempts: Mutex<usize>,
        shutdown: CancellationToken,
    }

    #[async_trait::async_trait]
    impl Source for Lifetimes {
        type Stream = Script;

        #[cfg_attr(
            dylint_lib = "hyp_tracing",
            allow(uninstrumented_fn, reason = "test double")
        )]
        async fn subscribe(&self) -> redis::RedisResult<Script> {
            let attempt = {
                let mut attempts = self.attempts.lock().expect("attempts lock");
                *attempts += 1;
                *attempts
            };
            use SubscriptionEvent::{Disconnected, Subscribed};
            match attempt {
                1 => Ok(Script(
                    [
                        now(Subscribed),
                        now(Disconnected),
                        (Duration::from_secs(2), Disconnected),
                        (Duration::from_secs(5), Subscribed),
                    ]
                    .into_iter()
                    .collect(),
                    true,
                )),
                2 => Ok(Script([now(Disconnected)].into_iter().collect(), false)),
                3 => Err(redis::RedisError::from((redis::ErrorKind::Io, "refused"))),
                4 => Ok(Script([now(Subscribed)].into_iter().collect(), true)),
                _ => {
                    self.shutdown.cancel();
                    Ok(Script(VecDeque::new(), false))
                }
            }
        }
    }

    /// Long enough that the 7 s loss of attempt 1 is inside it.
    const GRACE: Duration = Duration::from_secs(60);

    /// Runs the loop through the four scripted lives and returns its log.
    async fn logged_lives() -> String {
        let server = TestRedis::start().await;
        let redis = crate::shared::testing::connect(&server).await;
        let capture = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_ansi(false)
            .finish();
        let _guard = crate::test_tracing::set_default(subscriber);
        // The backoffs and the grace run on the paused clock.
        tokio::time::pause();
        let shutdown = CancellationToken::new();
        let source = Lifetimes {
            attempts: Mutex::new(0),
            shutdown: shutdown.clone(),
        };
        tokio::time::timeout(
            Duration::from_secs(600),
            run(&Quiet, &redis, &source, GRACE, &shutdown),
        )
        .await
        .expect("the loop stops within 600 s (paused clock)");
        capture.text()
    }

    /// Every (re)subscribe is at info, every loss at warn, each naming the
    /// subscriber and nothing a payload or URL could carry.
    /// ("ended" counts the stream that ended, and the one the grace dropped.)
    #[tokio::test]
    async fn subscribe_disconnect_grace_failure_and_end_are_logged() {
        let logged = logged_lives().await;
        let lines: Vec<&str> = logged.lines().collect();
        let count = |level: &str, text: &str| {
            lines
                .iter()
                .filter(|line| line.contains(level) && line.contains(text))
                .count()
        };
        assert_eq!(count("INFO", "pub/sub subscribed"), 3, "{logged}");
        assert_eq!(count("INFO", "resubscribe=true"), 2, "{logged}");
        // Two losses (attempts 1 and 2); attempt 1's repeated `Disconnected`
        // adds no warning.
        assert_eq!(count("WARN", "connection lost"), 2, "{logged}");
        // Each recovery line carries its outage's length on the paused clock,
        // and the first subscribe none. Attempt 1's recovery counts from its
        // first `Disconnected`: 7 s. Attempt 4's outage spans a replaced
        // subscription and a failed subscribe, and counts from the end of
        // attempt 1's stream: that stream's 1 s backoff, attempt 2's 60 s
        // grace and 2 s backoff, and attempt 3's 4 s backoff: 67 s.
        // (The timer rounds each sleep up to a millisecond, so allow a few.)
        let reported: Vec<u64> = lines
            .iter()
            .filter(|line| line.contains("INFO"))
            .filter_map(|line| line.split("disconnected_ms=").nth(1))
            .map(|rest| {
                let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
                digits.parse().expect("a number after disconnected_ms=")
            })
            .collect();
        assert!(
            matches!(reported.as_slice(), [same, whole]
                if (7000..7010).contains(same) && (67_000..67_010).contains(whole)),
            "{reported:?} in {logged}"
        );
        assert_eq!(count("WARN", "past the grace"), 1, "{logged}");
        assert_eq!(count("WARN", "subscribe failed"), 1, "{logged}");
        assert_eq!(count("WARN", "subscription ended"), 3, "{logged}");
        assert!(
            lines
                .iter()
                .filter(|line| line.contains("pub/sub"))
                .all(|line| line.contains("subscriber=\"catalog\"")),
            "{logged}"
        );
        assert!(!logged.contains("redis://"), "{logged}");
    }
}
