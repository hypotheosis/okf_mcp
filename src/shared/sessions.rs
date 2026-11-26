//! MCP sessions in Redis (spec Decisions 9-11): rmcp's `SessionStore`, and
//! the binding of each session to the key that opened it.
//!
//! Each session has two keys under the hash of its id (spec "Key layout"):
//! `mcp:state:<h>` holds rmcp's `SessionState` (what a replica needs to
//! restore the session), and `mcp:bind:<h>` holds the id of the key that
//! opened it. Both carry the same idle TTL, slid together on every admitted
//! request. Both live under the one hash tag every key carries, so the
//! scripts below, which name both in `KEYS`, also run on a cluster.

use std::cell::Cell;
use std::future::Future;
use std::sync::LazyLock;
use std::time::Duration;

use rmcp::transport::streamable_http_server::session::store::{
    SessionState, SessionStore, SessionStoreError,
};

use super::ephemeral::{millis, Envelope};
use super::{hashed, Redis};
use crate::error::OkfError;
use crate::telemetry::metrics::RedisOp;

/// The version of the stored `SessionState` envelope (spec Decision 6).
const SESSION_STATE_VERSION: u32 = 1;

/// The key space of the state, for the decode counter's log line.
const STATE_SPACE: &str = "mcp:state";

/// `KEYS[1]` binding, `KEYS[2]` state; `ARGV[1]` key id, `ARGV[2]` idle ms.
/// Admits only the binding's key, and only while the state exists (an
/// evicted state must fail closed on every replica, not only on one that has
/// to restore it: review of PR #552). Only then does it slide both TTLs.
const ADMIT: &str = "local v = redis.call('GET', KEYS[1])\n\
    if v == false or v ~= ARGV[1] then return 0 end\n\
    if redis.call('EXISTS', KEYS[2]) == 0 then return 0 end\n\
    redis.call('PEXPIRE', KEYS[1], ARGV[2])\n\
    redis.call('PEXPIRE', KEYS[2], ARGV[2])\n\
    return 1";

/// `KEYS[1]` binding, `KEYS[2]` state; `ARGV[1]` key id, `ARGV[2]` idle ms.
/// Binds only a session whose state exists, and gives both keys the same TTL.
const BIND: &str = "if redis.call('EXISTS', KEYS[2]) == 0 then return 0 end\n\
    redis.call('SET', KEYS[1], ARGV[1], 'PX', ARGV[2])\n\
    redis.call('PEXPIRE', KEYS[2], ARGV[2])\n\
    return 1";

// `LazyLock`: built on first use, then shared (Python: a module-level value
// computed lazily). A `Script` holds its SHA-1 for `EVALSHA`.
static ADMIT_SCRIPT: LazyLock<redis::Script> = LazyLock::new(|| redis::Script::new(ADMIT));
static BIND_SCRIPT: LazyLock<redis::Script> = LazyLock::new(|| redis::Script::new(BIND));

// A value per task (Python: a `contextvars.ContextVar`), set only inside
// `watch_restore`. rmcp calls `load` on the task that runs the request, so the
// middleware's scope is visible there.
tokio::task_local! {
    /// Set by `load` when Redis failed during a restore in this request.
    static RESTORE_FAILED: Cell<bool>;
}

/// Runs `future` (rmcp's handling of one request) and reports whether a
/// restore in it failed on Redis. `load` cannot say so itself: an error would
/// reach rmcp, which logs it with the session id, so it answers `Ok(None)`
/// (rmcp's 404) and sets this request-scoped flag for the middleware.
///
/// # Arguments
/// * `future` - the rest of the request.
///
/// # Returns
/// The future's output, and `true` when a restore in it failed on Redis (the
/// request should answer 503, not rmcp's 404).
#[tracing::instrument(level = "trace", skip_all)]
pub async fn watch_restore<F: Future>(future: F) -> (F::Output, bool) {
    RESTORE_FAILED
        .scope(Cell::new(false), async move {
            let output = future.await;
            (output, RESTORE_FAILED.with(Cell::get))
        })
        .await
}

/// The error a request answers when the shared session store fails: 503,
/// retryable.
///
/// # Returns
/// [`OkfError::Busy`].
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial constructor: builds a value from its arguments"
    )
)]
pub fn unavailable() -> OkfError {
    OkfError::Busy {
        reason: "the shared session store is unavailable".to_string(),
    }
}

/// What `MGET bind state` answers: each key's bytes, `None` when missing.
type BoundState = (Option<Vec<u8>>, Option<Vec<u8>>);

/// Sessions and their bindings in Redis. One value serves both the
/// `bind_session` middleware (through `SessionBindings`) and rmcp (as its
/// `session_store`).
#[derive(Debug)]
pub struct RedisSessions {
    redis: Redis,
    idle: Duration,
    // Test hook: the next `store` drops its write, as a failed `SET` would.
    #[cfg(test)]
    fail_next_store: std::sync::atomic::AtomicBool,
}

/// The binding key and the state key of `session`, by the hash of its id
/// (a session id is a credential: it never enters a key name as given).
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "one-line accessor or key formatter: a span would record nothing"
    )
)]
fn keys(redis: &Redis, session: &str) -> (String, String) {
    let id = hashed(session);
    (redis.key("mcp:bind", &id), redis.key(STATE_SPACE, &id))
}

impl RedisSessions {
    /// Sessions over `redis` that expire after `idle` without use.
    ///
    /// # Arguments
    /// * `redis` - the shared Redis.
    /// * `idle` - `session_idle_timeout`.
    ///
    /// # Returns
    /// The store.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial constructor: builds a value from its arguments"
        )
    )]
    pub fn new(redis: Redis, idle: Duration) -> Self {
        Self {
            redis,
            idle,
            #[cfg(test)]
            fail_next_store: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// May `key_id` use `session`? Slides both keys' TTL when it may.
    ///
    /// # Arguments
    /// * `session` - the request's `Mcp-Session-Id`.
    /// * `key_id` - the request's key.
    ///
    /// # Returns
    /// `true` only if `key_id` opened the session and its state is still
    /// stored.
    ///
    /// # Errors
    /// [`OkfError::Busy`] when Redis fails (the request answers 503).
    #[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "redis", db.operation.name = "admit"))]
    pub async fn admits(&self, session: &str, key_id: &str) -> Result<bool, OkfError> {
        let (bind, state) = keys(&self.redis, session);
        let admitted: i64 = ADMIT_SCRIPT
            .key(bind)
            .key(state)
            .arg(key_id)
            .arg(millis(self.idle))
            .invoke_async(&mut self.redis.connection())
            .await
            .map_err(|error| self.redis.failed(RedisOp::Admit, &error))?;
        Ok(admitted == 1)
    }

    /// Binds a session rmcp just issued to `key_id`, but only if rmcp's
    /// `store` really persisted its state: `store` swallows its errors (it
    /// must, see [`SessionStore::load`]), so a state write that failed would
    /// otherwise leave a binding that restores nowhere (review of PR #552).
    /// Gives both keys the idle TTL.
    ///
    /// # Arguments
    /// * `session` - the `Mcp-Session-Id` rmcp issued.
    /// * `key_id` - the key of the request that opened it.
    ///
    /// # Errors
    /// [`OkfError::Busy`] when Redis fails, or when the state is missing (the
    /// request answers 503 and the client initializes again).
    #[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "redis", db.operation.name = "bind"))]
    pub async fn bind(&self, session: &str, key_id: &str) -> Result<(), OkfError> {
        let (bind, state) = keys(&self.redis, session);
        let bound: i64 = BIND_SCRIPT
            .key(bind)
            .key(state)
            .arg(key_id)
            .arg(millis(self.idle))
            .invoke_async(&mut self.redis.connection())
            .await
            .map_err(|error| self.redis.failed(RedisOp::Bind, &error))?;
        if bound == 1 {
            return Ok(());
        }
        self.redis.count(RedisOp::Bind);
        tracing::warn!("an MCP session's state was not persisted; refusing to bind it");
        Err(unavailable())
    }

    /// Forgets a session (its `DELETE /mcp`): removes both keys.
    ///
    /// # Arguments
    /// * `session` - its id.
    ///
    /// # Errors
    /// [`OkfError::Busy`] when Redis fails (the session stays for a retry).
    #[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "redis", db.operation.name = "remove"))]
    pub async fn release(&self, session: &str) -> Result<(), OkfError> {
        let (bind, state) = keys(&self.redis, session);
        let _: i64 = redis::cmd("DEL")
            .arg(bind)
            .arg(state)
            .query_async(&mut self.redis.connection())
            .await
            .map_err(|error| self.redis.failed(RedisOp::Remove, &error))?;
        Ok(())
    }

    /// Makes the next `store` drop its write, counted as a failed `set`
    /// (tests only: a state write that failed).
    #[cfg(test)]
    pub(crate) fn fail_next_store(&self) {
        self.fail_next_store
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// [`SessionStore::load`]: the state, only while its binding exists.
    #[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "redis", db.operation.name = "get"))]
    async fn load_state(&self, session: &str) -> Option<SessionState> {
        let (bind, state) = keys(&self.redis, session);
        let fetched: redis::RedisResult<BoundState> = redis::cmd("MGET")
            .arg(bind)
            .arg(state)
            .query_async(&mut self.redis.connection())
            .await;
        let (binding, stored) = match fetched {
            Ok(pair) => pair,
            Err(error) => {
                // Counted and logged without the id. The session may exist,
                // so the middleware answers 503 rather than rmcp's 404
                // (`watch_restore`); outside its scope there is no flag to set.
                let _ = self.redis.failed(RedisOp::Get, &error);
                let _ = RESTORE_FAILED.try_with(|failed| failed.set(true));
                return None;
            }
        };
        // No binding: never restore (spec Decision 10), even if a request
        // reached rmcp without passing `bind_session`.
        let (Some(_), Some(stored)) = (binding, stored) else {
            return None;
        };
        match serde_json::from_slice::<Envelope<SessionState>>(&stored) {
            Ok(envelope) if envelope.v == SESSION_STATE_VERSION => Some(envelope.d),
            _ => {
                self.redis.undecodable(STATE_SPACE);
                None
            }
        }
    }

    /// [`SessionStore::store`]: `SET state PX idle`; a failure is counted
    /// and logged, never returned.
    #[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "redis", db.operation.name = "set"))]
    async fn store_state(&self, session: &str, state: &SessionState) {
        #[cfg(test)]
        if self
            .fail_next_store
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            self.redis.count(RedisOp::Set);
            return;
        }
        let (_, key) = keys(&self.redis, session);
        // `Envelope<&SessionState>`: serializing borrows, no clone.
        let Ok(bytes) = serde_json::to_vec(&Envelope {
            v: SESSION_STATE_VERSION,
            d: state,
        }) else {
            self.redis.undecodable(STATE_SPACE);
            return;
        };
        let stored: redis::RedisResult<()> = redis::cmd("SET")
            .arg(key)
            .arg(bytes)
            .arg("PX")
            .arg(millis(self.idle))
            .query_async(&mut self.redis.connection())
            .await;
        if let Err(error) = stored {
            let _ = self.redis.failed(RedisOp::Set, &error);
        }
    }

    /// [`SessionStore::delete`]: removes the state; a failure is counted and
    /// logged, never returned.
    #[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "redis", db.operation.name = "remove"))]
    async fn delete_state(&self, session: &str) {
        // `bind_session` already released both keys before rmcp ran; this
        // covers a DELETE that reached rmcp some other way.
        let (_, key) = keys(&self.redis, session);
        let deleted: redis::RedisResult<i64> = redis::cmd("DEL")
            .arg(key)
            .query_async(&mut self.redis.connection())
            .await;
        if let Err(error) = deleted {
            let _ = self.redis.failed(RedisOp::Remove, &error);
        }
    }
}

// `#[async_trait]` turns each `async fn` into one returning a boxed future,
// which is what makes the trait usable as `Arc<dyn SessionStore>` (Python's
// ABC with async methods, roughly). Every method answers `Ok`: rmcp logs the
// session id with any error (plan "Verified facts"), and a session id is a
// credential. A failure is counted and logged by the method it delegates to.
#[async_trait::async_trait]
impl SessionStore for RedisSessions {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "thin delegate to an instrumented method: a span would record nothing"
        )
    )]
    async fn load(&self, session_id: &str) -> Result<Option<SessionState>, SessionStoreError> {
        Ok(self.load_state(session_id).await)
    }

    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "thin delegate to an instrumented method: a span would record nothing"
        )
    )]
    async fn store(&self, session_id: &str, state: &SessionState) -> Result<(), SessionStoreError> {
        self.store_state(session_id, state).await;
        Ok(())
    }

    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "thin delegate to an instrumented method: a span would record nothing"
        )
    )]
    async fn delete(&self, session_id: &str) -> Result<(), SessionStoreError> {
        self.delete_state(session_id).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::redis_support::TestRedis;
    use crate::shared::testing;

    const IDLE: Duration = Duration::from_secs(60);
    /// The bound on every test body (containers start inside it).
    const BOUND: Duration = Duration::from_secs(120);

    /// A state as rmcp would store it.
    fn state() -> SessionState {
        serde_json::from_value(serde_json::json!({
            "initialize_params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "t", "version": "0"}
            }
        }))
        .expect("a session state")
    }

    /// Runs a raw command on `redis` (test set-up and inspection).
    async fn raw<T: redis::FromRedisValue>(redis: &Redis, cmd: &mut redis::Cmd) -> T {
        cmd.query_async(&mut redis.connection())
            .await
            .expect("a raw redis command")
    }

    /// The two keys' remaining lifetimes, in ms (`-2` when missing).
    async fn ttls(redis: &Redis, session: &str) -> (i64, i64) {
        let (bind, state) = keys(redis, session);
        (
            raw(redis, redis::cmd("PTTL").arg(bind)).await,
            raw(redis, redis::cmd("PTTL").arg(state)).await,
        )
    }

    /// Admit and bind: only the binding's key, only while the state exists,
    /// and both keys slide together.
    async fn scripts_bind_and_admit(server: TestRedis) {
        let redis = testing::connect(&server).await;
        let sessions = RedisSessions::new(redis.clone(), IDLE);

        // No state yet: BIND refuses and writes nothing.
        let unbound = sessions.bind("s1", "alice").await;
        assert!(matches!(unbound, Err(OkfError::Busy { .. })), "{unbound:?}");
        assert_eq!(ttls(&redis, "s1").await, (-2, -2));

        sessions.store("s1", &state()).await.expect("store");
        sessions.bind("s1", "alice").await.expect("bind");
        assert!(sessions.admits("s1", "alice").await.expect("admit"));
        assert!(!sessions.admits("s1", "bob").await.expect("admit"));
        assert!(!sessions.admits("unknown", "alice").await.expect("admit"));

        // Shorten both, then an admitted request slides both back to IDLE;
        // a refused one slides neither.
        let (bind, state_key) = keys(&redis, "s1");
        for key in [&bind, &state_key] {
            let _: bool = raw(&redis, redis::cmd("PEXPIRE").arg(key).arg(5_000)).await;
        }
        assert!(!sessions.admits("s1", "bob").await.expect("admit"));
        let (b, s) = ttls(&redis, "s1").await;
        assert!(b <= 5_000 && s <= 5_000, "{b} {s}");
        assert!(sessions.admits("s1", "alice").await.expect("admit"));
        let (b, s) = ttls(&redis, "s1").await;
        assert!(b > 5_000 && s > 5_000, "{b} {s}");

        // The state gone (evicted): ADMIT refuses even the right key.
        let _: i64 = raw(&redis, redis::cmd("DEL").arg(&state_key)).await;
        assert!(!sessions.admits("s1", "alice").await.expect("admit"));

        // The key names hold the hash, never the id.
        assert!(!bind.contains("s1") && bind.ends_with(&hashed("s1")));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_scripts_bind_and_admit_only_the_binding_key_while_the_state_exists() {
        tokio::time::timeout(BOUND, async {
            scripts_bind_and_admit(TestRedis::start().await).await;
        })
        .await
        .expect("the test finishes within its bound");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_scripts_bind_and_admit_in_cluster_mode() {
        tokio::time::timeout(BOUND, async {
            scripts_bind_and_admit(TestRedis::start_cluster().await).await;
        })
        .await
        .expect("the test finishes within its bound");
    }

    /// Load restores only a bound state; release removes both keys.
    async fn load_and_release(server: TestRedis) {
        let redis = testing::connect(&server).await;
        let sessions = RedisSessions::new(redis.clone(), IDLE);
        sessions.store("s2", &state()).await.expect("store");

        // A state without its binding is never restored.
        assert!(sessions.load("s2").await.expect("load").is_none());

        sessions.bind("s2", "alice").await.expect("bind");
        let loaded = sessions.load("s2").await.expect("load").expect("a state");
        assert_eq!(loaded.initialize_params, state().initialize_params);

        sessions.release("s2").await.expect("release");
        assert_eq!(ttls(&redis, "s2").await, (-2, -2));
        assert!(sessions.load("s2").await.expect("load").is_none());

        // `delete` (rmcp's) removes the state.
        sessions.store("s3", &state()).await.expect("store");
        sessions.delete("s3").await.expect("delete");
        assert_eq!(ttls(&redis, "s3").await.1, -2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn load_restores_only_a_bound_state_and_release_removes_both_keys() {
        tokio::time::timeout(BOUND, async {
            load_and_release(TestRedis::start().await).await;
        })
        .await
        .expect("the test finishes within its bound");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn load_and_release_in_cluster_mode() {
        tokio::time::timeout(BOUND, async {
            load_and_release(TestRedis::start_cluster().await).await;
        })
        .await
        .expect("the test finishes within its bound");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_state_of_another_version_is_not_restored_and_counts_as_decode() {
        tokio::time::timeout(BOUND, async {
            let server = TestRedis::start().await;
            let (_provider, metrics, registry) = crate::mcp::testing::local_metrics();
            let redis = Redis::connect(&testing::config(&server), &server.env(), &metrics)
                .await
                .expect("connect");
            let sessions = RedisSessions::new(redis.clone(), IDLE);
            sessions.store("s4", &state()).await.expect("store");
            sessions.bind("s4", "alice").await.expect("bind");
            let (_, state_key) = keys(&redis, "s4");
            let _: () = raw(
                &redis,
                redis::cmd("SET").arg(&state_key).arg(r#"{"v":2,"d":{}}"#),
            )
            .await;

            let (loaded, restore_failed) = watch_restore(sessions.load("s4")).await;
            assert!(loaded.expect("load").is_none());
            assert!(
                !restore_failed,
                "an undecodable state is absent, not an outage"
            );
            let rendered = crate::mcp::testing::render(&registry);
            assert_eq!(testing::error_count(&rendered, "decode"), 1, "{rendered}");
        })
        .await
        .expect("the test finishes within its bound");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_redis_failure_is_never_an_error_to_rmcp_and_is_busy_to_the_middleware() {
        tokio::time::timeout(BOUND, async {
            // A shared server cannot be stopped for everyone.
            let server = TestRedis::start().await.dedicated().await;
            let (_provider, metrics, registry) = crate::mcp::testing::local_metrics();
            let redis = Redis::connect(&testing::config(&server), &server.env(), &metrics)
                .await
                .expect("connect");
            let sessions = RedisSessions::new(redis, IDLE);
            server.stop().await;

            let (loaded, restore_failed) = watch_restore(sessions.load("s5")).await;
            assert!(loaded.expect("load").is_none());
            assert!(restore_failed, "the middleware learns of the failure");
            sessions.store("s5", &state()).await.expect("store");
            sessions.delete("s5").await.expect("delete");
            for refused in [
                sessions.admits("s5", "alice").await.map(|_| ()),
                sessions.bind("s5", "alice").await,
                sessions.release("s5").await,
            ] {
                assert!(matches!(refused, Err(OkfError::Busy { .. })), "{refused:?}");
            }
            let rendered = crate::mcp::testing::render(&registry);
            for op in ["get", "set", "admit", "bind"] {
                assert_eq!(testing::error_count(&rendered, op), 1, "{op}: {rendered}");
            }
            assert_eq!(testing::error_count(&rendered, "remove"), 2, "{rendered}");
        })
        .await
        .expect("the test finishes within its bound");
    }
}
