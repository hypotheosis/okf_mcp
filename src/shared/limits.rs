//! Rate limits shared by every replica (issue #357). With `[redis]` a
//! limiter's token buckets live in Redis, so the configured budget holds
//! across replicas and survives a restart; without it each limiter is
//! today's in-memory one, unchanged.
//!
//! - One Lua script (`TAKE`) refills, takes and renews a bucket in one
//!   atomic step on Redis's own clock, so two replicas racing for the last
//!   token let one through, and no replica's clock can refill a bucket early.
//! - Every bucket has a TTL: the time it takes to refill from empty, renewed
//!   by every take. A bucket Redis drops (expired, or evicted) was full by
//!   then, so the full bucket that replaces it grants nothing extra.
//! - Keys are `{<prefix>}ratelimit:<limiter>:<hashed id>`: the module's one
//!   hash tag, the limiter's name (the `limiter` metric label) and the hex
//!   SHA-256 of the address, email or user id, never the id itself.
//! - **A Redis failure fails open to this replica's own bucket**, which the
//!   limiter keeps for that purpose: the request is decided by the same
//!   budget counted per replica, as it was before #357, never unlimited. The
//!   failure is counted (`okf.redis.errors`, operation `limit`, and
//!   `okf.ratelimit.fallbacks` per limiter) and logged, as a warning at most
//!   once a minute per limiter.
//! - **The script needs the Redis commands it calls.** `TAKE` calls `TIME`,
//!   `HMGET`, `HSET` and `PEXPIRE`; a Redis user whose ACL lacks one of them
//!   fails every call (issue #832). [`check`] runs the script once at
//!   start-up, logs an error naming the ACL problem and sets the flag
//!   `/healthz` reports as `rate_limits`. The trade-off of dropping `TIME`
//!   for the caller's clock was not taken: it would give up the single clock.

use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use super::{hashed, Redis};
use crate::http::limits::KeyedLimiter;
use crate::oauth::ratelimit::TokenBucket;
use crate::shared::ephemeral::millis;
use crate::telemetry::metrics::{Limiter, RedisOp};
use crate::telemetry::subscriber::split_event;

/// Takes one token from the bucket `KEYS[1]` if it has one: `ARGV[1]` is the
/// burst, `ARGV[2]` the milliseconds an empty bucket takes to refill (also
/// its TTL). Returns 1 taken, 0 refused. The bucket is a hash of tokens left
/// (`t`) and when they were counted (`at`, Redis's milliseconds); a missing
/// one is full. Time is Redis's `TIME`, so every replica counts on one clock;
/// `at` never moves back, so a clock step back refills nothing. Its one key
/// is named in `KEYS`, so a cluster routes it to that key's slot.
const TAKE: &str = "local capacity = tonumber(ARGV[1])\n\
    local per = tonumber(ARGV[2])\n\
    local clock = redis.call('TIME')\n\
    local now = tonumber(clock[1]) * 1000 + math.floor(tonumber(clock[2]) / 1000)\n\
    local state = redis.call('HMGET', KEYS[1], 't', 'at')\n\
    local tokens = tonumber(state[1])\n\
    local at = tonumber(state[2])\n\
    if tokens == nil or at == nil then\n\
      tokens = capacity\n\
      at = now\n\
    end\n\
    local elapsed = math.max(0, now - at)\n\
    local available = math.min(capacity, tokens + elapsed * capacity / per)\n\
    local taken = 0\n\
    if available >= 1 then\n\
      available = available - 1\n\
      taken = 1\n\
    end\n\
    redis.call('HSET', KEYS[1], 't', string.format('%.17g', available), 'at', string.format('%.17g', math.max(at, now)))\n\
    redis.call('PEXPIRE', KEYS[1], per)\n\
    return taken";

// `LazyLock`: built on first use, then shared (Python: a module-level value
// computed lazily). `Script` sends `EVALSHA`, falling back to `EVAL` when the
// server does not know the script yet.
static TAKE_SCRIPT: LazyLock<redis::Script> = LazyLock::new(|| redis::Script::new(TAKE));

/// The most often a limiter logs its fallback warning (every fallback is
/// still counted).
const FALLBACK_LOG_EVERY: Duration = Duration::from_secs(60);

/// The id of a global limiter's one bucket (the OAuth limits are not keyed).
const GLOBAL_ID: &str = "global";

/// One limiter's buckets in Redis.
struct SharedBuckets {
    redis: Redis,
    limiter: Limiter,
    capacity: u32,
    per: Duration,
    /// When this limiter last logged a fallback warning.
    last_logged: Mutex<Option<Instant>>,
}

impl SharedBuckets {
    /// A limiter's buckets in `redis`.
    ///
    /// # Arguments
    /// * `redis` - the shared Redis.
    /// * `limiter` - which limiter (names its key space).
    /// * `capacity` - the burst.
    /// * `per` - how long an empty bucket takes to refill.
    ///
    /// # Returns
    /// The buckets.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial constructor: builds a value from its arguments"
        )
    )]
    fn new(redis: &Redis, limiter: Limiter, capacity: u32, per: Duration) -> Self {
        Self {
            redis: redis.clone(),
            limiter,
            capacity,
            per,
            last_logged: Mutex::new(None),
        }
    }

    /// Whether a fallback warning is due now, and if so marks it logged.
    ///
    /// # Returns
    /// `true` for the first fallback and then at most once per
    /// [`FALLBACK_LOG_EVERY`].
    #[tracing::instrument(level = "trace", skip_all)]
    fn warning_due(&self) -> bool {
        // A poisoned lock only means another thread panicked holding it; the
        // timestamp inside is still usable.
        let mut last = self
            .last_logged
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        let due = last.is_none_or(|logged| now.duration_since(logged) >= FALLBACK_LOG_EVERY);
        if due {
            *last = Some(now);
        }
        due
    }

    /// The full key of `id`'s bucket.
    ///
    /// # Arguments
    /// * `id` - as the caller presents it; only its hash reaches the key.
    ///
    /// # Returns
    /// `{<prefix>}ratelimit:<limiter>:<hashed id>`.
    #[tracing::instrument(level = "trace", skip_all)]
    fn key(&self, id: &str) -> String {
        self.redis
            .key(&format!("ratelimit:{}", self.limiter.as_str()), &hashed(id))
    }

    /// Takes one token from `id`'s bucket.
    ///
    /// # Arguments
    /// * `id` - the bucket's id (an address key, an email, a user id).
    ///
    /// # Returns
    /// `true` if the request may go ahead.
    ///
    /// # Errors
    /// The driver's error when the script fails or times out.
    #[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "redis", db.operation.name = "limit"))]
    async fn take(&self, id: &str) -> redis::RedisResult<bool> {
        TAKE_SCRIPT
            .key(self.key(id))
            .arg(self.capacity)
            .arg(millis(self.per))
            .invoke_async(&mut self.redis.connection())
            .await
    }

    /// Decides with Redis, falling back to `local` when Redis fails.
    ///
    /// # Arguments
    /// * `id` - the bucket's id.
    /// * `local` - this replica's own bucket for `id`, asked only on failure.
    ///
    /// # Returns
    /// Whether the request may go ahead.
    #[tracing::instrument(level = "debug", skip_all)]
    async fn take_or(&self, id: &str, local: impl FnOnce() -> bool) -> bool {
        match self.take(id).await {
            Ok(taken) => {
                self.redis.set_limits_degraded(false);
                taken
            }
            Err(error) => {
                self.redis.count(RedisOp::Limit);
                self.redis.count_fallback(self.limiter);
                self.redis.set_limits_degraded(true);
                if self.warning_due() {
                    split_event!(
                        warn,
                        { limiter = self.limiter.event_name(), kind = ?error.kind(), },
                        { %error, },
                        "a shared rate limit failed; this replica's own bucket decided"
                    );
                }
                local()
            }
        }
    }
}

/// Runs the `TAKE` script once against a throwaway key, so a Redis user that
/// may not run a command the script calls (`TIME`, issue #832) is found at
/// start-up, not by the first request.
///
/// A refusal is logged as an error that names the ACL problem and sets
/// [`Redis::limits_degraded`], which `/healthz` reports. It never stops
/// start-up: every limit still works from this replica's own bucket.
///
/// # Arguments
/// * `redis` - the shared Redis.
///
/// # Returns
/// `true` if the script ran.
#[tracing::instrument(level = "info", skip_all, fields(db.system.name = "redis", db.operation.name = "limit"))]
pub async fn check(redis: &Redis) -> bool {
    // One token in a bucket that expires after a second: nothing it leaves
    // behind outlives the check.
    let probe = TAKE_SCRIPT
        .key(redis.key("ratelimit:startup_check", "probe"))
        .arg(1_u32)
        .arg(1_000_u32)
        .invoke_async::<i64>(&mut redis.connection())
        .await;
    match probe {
        Ok(_) => {
            redis.set_limits_degraded(false);
            true
        }
        Err(error) => {
            redis.count(RedisOp::Limit);
            redis.set_limits_degraded(true);
            split_event!(
                error,
                { kind = ?error.kind(), },
                { %error, },
                "the shared rate limiter's script cannot run on Redis (usually the Redis user's ACL lacks a command it calls, such as TIME; see docs/operations.md, Redis); every rate limit is decided by each replica's own bucket"
            );
            false
        }
    }
}

/// A token bucket per key (an address, an email, a user id): shared by every
/// replica with `[redis]`, else this process's [`KeyedLimiter`].
pub struct KeyedLimit {
    /// This process's buckets: the limiter without `[redis]`, the fallback
    /// with it.
    local: KeyedLimiter,
    /// The Redis buckets, with `[redis]`.
    shared: Option<SharedBuckets>,
}

impl KeyedLimit {
    /// An empty limiter.
    ///
    /// # Arguments
    /// * `limiter` - which one (names its key space).
    /// * `capacity` - the burst per key, and the refill per `per`.
    /// * `per` - how long an empty bucket takes to refill, and how long an
    ///   untouched one is kept; more than zero.
    /// * `max_keys` - the most keys this process tracks (Redis has no cap of
    ///   its own; its `maxmemory` bounds it).
    /// * `redis` - the shared Redis, with `[redis]`.
    ///
    /// # Returns
    /// The limiter.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial constructor: builds a value from its arguments"
        )
    )]
    pub fn new(
        limiter: Limiter,
        capacity: u32,
        per: Duration,
        max_keys: usize,
        redis: Option<&Redis>,
    ) -> Self {
        Self {
            local: KeyedLimiter::new(capacity, per, max_keys),
            shared: redis.map(|redis| SharedBuckets::new(redis, limiter, capacity, per)),
        }
    }

    /// Takes one request for `key` if its bucket has one.
    ///
    /// # Arguments
    /// * `key` - an address (`http::limits::ip_key`), a normalized email or a
    ///   user id.
    /// * `now` - the current instant, for this process's buckets (Redis
    ///   counts on its own clock).
    ///
    /// # Returns
    /// `true` if the request may go ahead.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn try_take(&self, key: &str, now: Instant) -> bool {
        match &self.shared {
            None => self.local.try_take(key, now),
            Some(shared) => shared.take_or(key, || self.local.try_take(key, now)).await,
        }
    }
}

/// One token bucket for a whole endpoint (the OAuth limits): shared by every
/// replica with `[redis]`, else this process's [`TokenBucket`].
pub struct GlobalLimit {
    /// This process's bucket: the limiter without `[redis]`, the fallback
    /// with it.
    local: TokenBucket,
    /// The Redis bucket, with `[redis]`.
    shared: Option<SharedBuckets>,
}

impl GlobalLimit {
    /// A full bucket.
    ///
    /// # Arguments
    /// * `limiter` - which one (names its key).
    /// * `capacity` - the burst.
    /// * `refill_per_sec` - the sustained rate; more than zero.
    /// * `redis` - the shared Redis, with `[redis]`.
    ///
    /// # Returns
    /// The limiter. In Redis the bucket refills from empty in
    /// `capacity / refill_per_sec`, which is also its TTL.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial constructor: builds a value from its arguments"
        )
    )]
    pub fn new(
        limiter: Limiter,
        capacity: u32,
        refill_per_sec: f64,
        redis: Option<&Redis>,
    ) -> Self {
        Self {
            local: TokenBucket::new(capacity, refill_per_sec),
            shared: redis.map(|redis| {
                let per = Duration::from_secs_f64(f64::from(capacity) / refill_per_sec);
                SharedBuckets::new(redis, limiter, capacity, per)
            }),
        }
    }

    /// Takes one token if there is one.
    ///
    /// # Arguments
    /// * `now` - the current instant, for this process's bucket.
    ///
    /// # Returns
    /// `true` if the request may go ahead.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn try_take(&self, now: Instant) -> bool {
        match &self.shared {
            None => self.local.try_take(now),
            Some(shared) => shared.take_or(GLOBAL_ID, || self.local.try_take(now)).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::redis_support::TestRedis;
    use crate::shared::testing::{self, error_count};
    use crate::telemetry::metrics::Metrics;

    /// The bound on every Redis test.
    const BOUND: Duration = Duration::from_secs(120);

    /// A keyed limiter of `capacity` per `per` on `redis`.
    fn keyed(redis: Option<&Redis>, capacity: u32, per: Duration) -> KeyedLimit {
        KeyedLimit::new(Limiter::SignInPerIp, capacity, per, 100, redis)
    }

    /// How many of `count` takes for `key` pass, one after another.
    async fn passes(limit: &KeyedLimit, key: &str, count: usize) -> usize {
        let mut passed = 0;
        for _ in 0..count {
            if limit.try_take(key, Instant::now()).await {
                passed += 1;
            }
        }
        passed
    }

    #[tokio::test]
    async fn without_redis_a_keyed_limit_is_the_in_memory_limiter() {
        let limit = keyed(None, 3, Duration::from_secs(900));
        let start = Instant::now();
        let mut taken = Vec::new();
        for _ in 0..4 {
            taken.push(limit.try_take("a", start).await);
        }
        assert_eq!(taken, [true, true, true, false]);
        assert!(limit.try_take("b", start).await, "keys are apart");
        // `now` drives the memory buckets: one token back every 300 s.
        assert!(!limit.try_take("a", start + Duration::from_secs(200)).await);
        assert!(limit.try_take("a", start + Duration::from_secs(301)).await);
    }

    #[tokio::test]
    async fn without_redis_a_global_limit_is_the_in_memory_bucket() {
        let limit = GlobalLimit::new(Limiter::OauthToken, 2, 1.0, None);
        let start = Instant::now();
        let mut taken = Vec::new();
        for _ in 0..3 {
            taken.push(limit.try_take(start).await);
        }
        assert_eq!(taken, [true, true, false]);
        assert!(limit.try_take(start + Duration::from_millis(1100)).await);
    }

    /// A burst of `capacity`, then refusals, per key; the bucket's key is
    /// hashed under the limiter's space and always has a TTL of at most `per`.
    async fn a_shared_bucket_holds_its_burst(server: TestRedis) {
        let redis = testing::connect(&server).await;
        let per = Duration::from_secs(900);
        let limit = keyed(Some(&redis), 3, per);

        let first = passes(&limit, "192.0.2.7", 5).await;
        let other = passes(&limit, "192.0.2.8", 1).await;

        assert_eq!(first, 3);
        assert_eq!(other, 1, "another key has its own bucket");
        let key = redis.key("ratelimit:sign_in_per_ip", &hashed("192.0.2.7"));
        assert!(!key.contains("192.0.2.7"), "{key}");
        let ttl: i64 = redis::cmd("PTTL")
            .arg(&key)
            .query_async(&mut redis.connection())
            .await
            .expect("PTTL");
        assert!(ttl > 0 && ttl <= 900_000, "{ttl}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_shared_bucket_holds_its_burst_on_standalone() {
        let server = TestRedis::start().await;
        tokio::time::timeout(BOUND, a_shared_bucket_holds_its_burst(server))
            .await
            .expect("the standalone test within 120 s");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_shared_bucket_holds_its_burst_on_a_cluster() {
        let server = TestRedis::start_cluster().await;
        tokio::time::timeout(BOUND, a_shared_bucket_holds_its_burst(server))
            .await
            .expect("the cluster test within 120 s");
    }

    /// Two replicas (two handles, two limiters) taking at once from one key
    /// let exactly the burst through between them.
    async fn two_replicas_share_one_budget(server: TestRedis) {
        let (one, two) = (
            testing::connect(&server).await,
            testing::connect(&server).await,
        );
        let per = Duration::from_secs(900);
        // `Arc`: each spawned task holds its replica's limiter.
        let replicas = [
            std::sync::Arc::new(keyed(Some(&one), 10, per)),
            std::sync::Arc::new(keyed(Some(&two), 10, per)),
        ];
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(40));
        let tasks: Vec<_> = (0..40)
            .map(|n| {
                let limit = std::sync::Arc::clone(&replicas[n % 2]);
                let barrier = std::sync::Arc::clone(&barrier);
                tokio::spawn(async move {
                    barrier.wait().await;
                    limit.try_take("198.51.100.1", Instant::now()).await
                })
            })
            .collect();
        let mut passed = 0;
        for task in tasks {
            if task.await.expect("a take task") {
                passed += 1;
            }
        }

        assert_eq!(passed, 10, "one budget across both replicas");
        assert!(!replicas[0].try_take("198.51.100.1", Instant::now()).await);
        assert!(!replicas[1].try_take("198.51.100.1", Instant::now()).await);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn two_replicas_share_one_budget_on_standalone() {
        let server = TestRedis::start().await;
        tokio::time::timeout(BOUND, two_replicas_share_one_budget(server))
            .await
            .expect("the standalone test within 120 s");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn two_replicas_share_one_budget_on_a_cluster() {
        let server = TestRedis::start_cluster().await;
        tokio::time::timeout(BOUND, two_replicas_share_one_budget(server))
            .await
            .expect("the cluster test within 120 s");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_shared_bucket_refills_over_its_window_and_then_expires() {
        tokio::time::timeout(BOUND, async {
            let server = TestRedis::start().await;
            let redis = testing::connect(&server).await;
            // 2 per second: one token back every 500 ms, full again in 1 s.
            let per = Duration::from_secs(1);
            let limit = keyed(Some(&redis), 2, per);
            assert_eq!(passes(&limit, "a", 3).await, 2);

            tokio::time::sleep(Duration::from_millis(600)).await;
            let one_back = passes(&limit, "a", 2).await;
            tokio::time::sleep(Duration::from_millis(1200)).await;
            let key = redis.key("ratelimit:sign_in_per_ip", &hashed("a"));
            let exists: i64 = redis::cmd("EXISTS")
                .arg(&key)
                .query_async(&mut redis.connection())
                .await
                .expect("EXISTS");
            let refilled = passes(&limit, "a", 3).await;

            assert_eq!(one_back, 1, "half the window refills one of two");
            assert_eq!(exists, 0, "an idle bucket expires once it is full");
            assert_eq!(refilled, 2, "a whole window restores the burst, no more");
        })
        .await
        .expect("the refill test within 120 s");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_global_limit_is_shared_by_two_replicas() {
        tokio::time::timeout(BOUND, async {
            let server = TestRedis::start().await;
            let (one, two) = (
                testing::connect(&server).await,
                testing::connect(&server).await,
            );
            let first = GlobalLimit::new(Limiter::OauthRegister, 3, 1.0 / 6.0, Some(&one));
            let second = GlobalLimit::new(Limiter::OauthRegister, 3, 1.0 / 6.0, Some(&two));

            let mut taken = Vec::new();
            for n in 0..5 {
                let limit = if n % 2 == 0 { &first } else { &second };
                taken.push(limit.try_take(Instant::now()).await);
            }

            assert_eq!(taken, [true, true, true, false, false]);
            let key = one.key("ratelimit:oauth_register", &hashed(GLOBAL_ID));
            let ttl: i64 = redis::cmd("PTTL")
                .arg(&key)
                .query_async(&mut one.connection())
                .await
                .expect("PTTL");
            // 3 tokens at one per 6 s: full again (and dropped) after 18 s.
            assert!(ttl > 0 && ttl <= 18_000, "{ttl}");
        })
        .await
        .expect("the global test within 120 s");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_apps_oauth_limits_are_shared_by_two_replicas() {
        tokio::time::timeout(Duration::from_secs(180), async {
            let config = crate::config::Config {
                public_url: Some("https://okf.example.com".to_string()),
                ..crate::config::Config::default()
            };
            let (first, second, _redis) = crate::mcp::testing::replica_pair(&[], config).await;
            let (one, two) = (
                first.app.oauth().expect("OAuth on the first replica"),
                second.oauth().expect("OAuth on the second replica"),
            );
            let now = Instant::now();
            for limit in [&one.register_limit, &one.token_limit, &one.consent_limit] {
                while limit.try_take(now).await {}
            }

            assert!(!two.register_limit.try_take(now).await, "registration");
            assert!(!two.token_limit.try_take(now).await, "token");
            assert!(!two.consent_limit.try_take(now).await, "consent");
        })
        .await
        .expect("the shared OAuth limits test within 180 s");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_redis_call_falls_back_to_this_replicas_own_bucket_and_is_counted() {
        tokio::time::timeout(BOUND, async {
            let server = TestRedis::start().await;
            let (_provider, metrics, registry) = crate::mcp::testing::local_metrics();
            let redis = Redis::connect(&testing::config(&server), &server.env(), &metrics)
                .await
                .expect("connect to the test redis");
            let limit = keyed(Some(&redis), 3, Duration::from_secs(900));
            // A string where the bucket's hash should be: the script fails
            // with WRONGTYPE on every call, on any server.
            let key = redis.key("ratelimit:sign_in_per_ip", &hashed("a"));
            let _: () = redis::cmd("SET")
                .arg(&key)
                .arg("not a bucket")
                .query_async(&mut redis.connection())
                .await
                .expect("SET");

            let passed = passes(&limit, "a", 5).await;

            assert_eq!(passed, 3, "the replica's own budget, never unlimited");
            let rendered = crate::mcp::testing::render(&registry);
            assert_eq!(error_count(&rendered, "limit"), 5, "{rendered}");
        })
        .await
        .expect("the fallback test within 120 s");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stopped_redis_falls_back_to_this_replicas_own_bucket() {
        tokio::time::timeout(BOUND, async {
            // A shared server cannot be stopped for everyone.
            let server = TestRedis::start().await.dedicated().await;
            let redis =
                Redis::connect(&testing::config(&server), &server.env(), &Metrics::global())
                    .await
                    .expect("connect to the test redis");
            let limit = GlobalLimit::new(Limiter::OauthConsent, 2, 1.0, Some(&redis));
            // One token taken in Redis before the outage.
            assert!(limit.try_take(Instant::now()).await);
            server.stop().await;

            let start = Instant::now();
            let mut taken = Vec::new();
            for _ in 0..3 {
                taken.push(limit.try_take(start).await);
            }

            // This replica's bucket is full (it was not asked while Redis
            // answered): its own burst of 2, then refused.
            assert_eq!(taken, [true, true, false]);
        })
        .await
        .expect("the outage test within 120 s");
    }

    /// The production Redis user's access string (issue #832), plus `~*`
    /// (a user needs key access to do anything) and `&*` (channels).
    const PRODUCTION_ACL: &str = "-@all +@read +@write +@keyspace +@connection \
        +@transaction +@scripting +@pubsub -@dangerous ~* &*";

    /// Connects as the user `url` names, counting on `metrics`.
    async fn connect_as(server: &TestRedis, url: String, metrics: &Metrics) -> Redis {
        let env =
            move |name: &str| (name == crate::db::redis_support::URL_VAR).then(|| url.clone());
        Redis::connect(&testing::config(server), &env, metrics)
            .await
            .expect("connect as the restricted user")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_redis_user_without_time_is_found_at_startup_and_falls_back() {
        tokio::time::timeout(BOUND, async {
            let server = TestRedis::start().await;
            let (_provider, metrics, registry) = crate::mcp::testing::local_metrics();
            let url = server.url_for_user(PRODUCTION_ACL).await;
            let redis = connect_as(&server, url, &metrics).await;
            let limit = GlobalLimit::new(Limiter::OauthToken, 2, 1.0, Some(&redis));

            let script_runs = check(&redis).await;

            assert!(!script_runs, "TIME is not in the production ACL");
            assert!(redis.limits_degraded(), "/healthz reports it");
            let mut taken = Vec::new();
            for _ in 0..3 {
                taken.push(limit.try_take(Instant::now()).await);
            }
            assert_eq!(taken, [true, true, false], "the replica's own bucket");
            let rendered = crate::mcp::testing::render(&registry);
            assert_eq!(error_count(&rendered, "limit"), 4, "{rendered}");
            assert!(
                rendered.contains("okf_ratelimit_fallbacks_total{limiter=\"oauth_token\"} 3"),
                "{rendered}"
            );
        })
        .await
        .expect("the production ACL test within 120 s");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn granting_time_makes_the_check_pass_and_the_limiter_share() {
        tokio::time::timeout(BOUND, async {
            let server = TestRedis::start().await;
            let url = server
                .url_for_user(&format!("{PRODUCTION_ACL} +time"))
                .await;
            let redis = connect_as(&server, url, &Metrics::global()).await;
            let limit = GlobalLimit::new(Limiter::OauthToken, 2, 1.0, Some(&redis));

            assert!(check(&redis).await, "the script runs once +time is granted");
            assert!(!redis.limits_degraded());
            let mut taken = Vec::new();
            for _ in 0..3 {
                taken.push(limit.try_take(Instant::now()).await);
            }

            assert_eq!(taken, [true, true, false]);
            assert!(
                !redis.limits_degraded(),
                "decided by Redis, not the fallback"
            );
        })
        .await
        .expect("the granted ACL test within 120 s");
    }

    #[tokio::test]
    async fn the_fallback_warning_is_due_once_a_minute() {
        // No connection is made: `warning_due` only reads the clock.
        let server = TestRedis::start().await;
        let redis = testing::connect(&server).await;
        let buckets = SharedBuckets::new(&redis, Limiter::OauthToken, 1, Duration::from_secs(1));

        assert!(buckets.warning_due(), "the first fallback logs");
        assert!(
            !buckets.warning_due(),
            "the next ones within a minute do not"
        );
        *buckets.last_logged.lock().expect("lock") =
            Some(Instant::now() - FALLBACK_LOG_EVERY - Duration::from_secs(1));
        assert!(buckets.warning_due(), "after a minute it logs again");
    }

    #[test]
    fn an_exported_limiter_name_survives_sentrys_scrubbing() {
        for limiter in [
            Limiter::SignInPerIp,
            Limiter::ForgotPerEmail,
            Limiter::ForgotPerIp,
            Limiter::LinkStarts,
            Limiter::OauthRegister,
            Limiter::OauthToken,
            Limiter::OauthConsent,
        ] {
            assert!(
                !limiter.event_name().contains("token"),
                "{} would be [Filtered]",
                limiter.event_name()
            );
        }
        assert_eq!(
            Limiter::OauthToken.as_str(),
            "oauth_token",
            "the label stays"
        );
    }
}
