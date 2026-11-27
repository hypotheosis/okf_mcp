//! Short-lived keyed state (spec Decisions 3-8): pending authorizations,
//! codes, flows and ceremonies. `Memory` is today's [`ExpiringMap`],
//! unchanged; `Redis` keeps the entries in Redis under hashed keys, with
//! Redis TTLs, so every replica sees the same entries.
//!
//! - [`Ephemeral::take`] is atomic (`GETDEL`): an entry is taken once across
//!   every instance.
//! - [`Ephemeral::update`] is a compare-and-set (a Lua script) that keeps the
//!   entry's remaining lifetime. Its change closure may run more than once.
//! - A stored value is the JSON envelope `{"v": <version>, "d": <value>}`; one
//!   of another version, or one that does not decode, counts as absent and is
//!   counted as a `decode` error.
//! - No id a client presents appears in a key name: the Redis arm hashes
//!   every key ([`super::hashed`]) and every index value.

use std::marker::PhantomData;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::{hashed, Redis};
use crate::error::OkfError;
use crate::oauth::expiring::ExpiringMap;
use crate::telemetry::metrics::RedisOp;

/// Writes `ARGV[2]` over `KEYS[1]` only if it still holds `ARGV[1]`, keeping
/// its remaining TTL, or `ARGV[3]` milliseconds when that is longer (0 keeps
/// the TTL as is; see [`Ephemeral::update_with_ttl_floor`]). The floor is
/// applied in the same script as the write, so it never reaches an entry the
/// write did not land on. 1 written, 0 changed meanwhile, -1 gone (it ran: see
/// the plan's verified facts). Its one key is named in `KEYS`, so a cluster
/// routes it to that key's slot.
const COMPARE_AND_SET: &str = "local cur = redis.call('GET', KEYS[1])\n\
    if cur == false then return -1 end\n\
    if cur ~= ARGV[1] then return 0 end\n\
    local ttl = redis.call('PTTL', KEYS[1])\n\
    if ttl <= 0 then return -1 end\n\
    local floor = tonumber(ARGV[3])\n\
    if floor > ttl then ttl = floor end\n\
    redis.call('SET', KEYS[1], ARGV[2], 'PX', ttl)\n\
    return 1";

// `LazyLock`: built on first use, then shared (Python: a module-level value
// computed lazily). `Script` hashes its source once and sends `EVALSHA`,
// falling back to `EVAL` when the server does not know it yet.
static COMPARE_AND_SET_SCRIPT: LazyLock<redis::Script> =
    LazyLock::new(|| redis::Script::new(COMPARE_AND_SET));

/// How many times `update` tries a compare-and-set before answering `Busy`.
const UPDATE_ATTEMPTS: usize = 8;

/// A key space: its key name and the version of its stored shape. Bump a
/// version whenever its value type changes shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keyspace {
    /// OAuth pending authorizations (the consent page in progress).
    OauthPending,
    /// OAuth authorization codes.
    OauthCode,
    /// OIDC sign-in and link flows.
    OidcFlow,
    /// Finished OIDC re-authentications waiting for an account deletion.
    OidcReauth,
    /// Passkey registration and sign-in ceremonies.
    PasskeyCeremony,
}

impl Keyspace {
    /// The key-name part.
    ///
    /// # Returns
    /// `oauth:pending`, `oauth:code`, `oidc:flow`, `oidc:reauth` or
    /// `passkey:ceremony`.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns a constant"
        )
    )]
    pub fn name(self) -> &'static str {
        match self {
            Self::OauthPending => "oauth:pending",
            Self::OauthCode => "oauth:code",
            Self::OidcFlow => "oidc:flow",
            Self::OidcReauth => "oidc:reauth",
            Self::PasskeyCeremony => "passkey:ceremony",
        }
    }

    /// The stored shape's version.
    ///
    /// # Returns
    /// The version every stored envelope of this space carries (all 1 at
    /// first).
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns a constant"
        )
    )]
    pub fn version(self) -> u32 {
        // No `_` arm: a new key space fails to compile until it has a version.
        match self {
            Self::OauthPending
            | Self::OauthCode
            | Self::OidcFlow
            | Self::OidcReauth
            | Self::PasskeyCeremony => 1,
        }
    }
}

/// A per-actor index space (spec Decision 8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexSpace {
    /// A console session's one OIDC link flow.
    OidcLink,
    /// A console session's one passkey ceremony.
    PasskeySession,
}

impl IndexSpace {
    /// The key-name part.
    ///
    /// # Returns
    /// `oidc:link` or `passkey:session`.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns a constant"
        )
    )]
    pub fn name(self) -> &'static str {
        match self {
            Self::OidcLink => "oidc:link",
            Self::PasskeySession => "passkey:session",
        }
    }
}

/// The stored envelope (spec Decision 6). `T` is `&V` when encoding and `V`
/// when decoding, so encoding borrows instead of cloning.
///
/// Crate-visible so `shared::sessions` stores rmcp's `SessionState` in the
/// same shape, under its own version.
#[derive(Serialize, Deserialize)]
pub(crate) struct Envelope<T> {
    /// The stored shape's version.
    pub(crate) v: u32,
    /// The value.
    pub(crate) d: T,
}

/// Values with a key and a lifetime fixed when they are stored.
// `PhantomData<fn() -> V>`: the Redis arm stores no `V`, but the type still
// names the value it holds; the `fn() -> V` form keeps the enum `Send` and
// `Sync` whatever `V` is.
pub enum Ephemeral<V> {
    /// One process's memory (no `[redis]`): bounded by its capacity.
    Memory(ExpiringMap<V>),
    /// Redis: unbounded here (Redis's `maxmemory` bounds it, spec Decision 8).
    Redis {
        /// The shared handle.
        redis: Redis,
        /// The key space (its name and version).
        space: Keyspace,
        /// Each entry's lifetime.
        ttl: Duration,
        /// The value type, without holding one.
        _value: PhantomData<fn() -> V>,
    },
}

/// An entry an [`Index`] no longer points to, to be removed from its
/// [`Ephemeral`] with [`Ephemeral::remove_stale`]. Opaque: in Redis it holds
/// the entry's hash, in memory its raw id (a cookie value), so its `Debug`
/// prints neither.
#[derive(PartialEq, Eq)]
pub struct Stale(String);

// Hand-written: in memory the id is a cookie value, which `{:?}` must not
// print (`lessons.md`, "No secret reaches a log line or `Debug`").
impl std::fmt::Debug for Stale {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "one-line formatter: a span would record nothing"
        )
    )]
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Stale(<redacted>)")
    }
}

/// What [`Index::replace`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Swap {
    /// The actor now points at the new entry; `Some` is the one it replaced.
    Swapped(Option<Stale>),
    /// Memory only: the index is full, nothing was stored.
    Full,
}

/// Per-actor pointers to their one live entry, with a sliding lifetime.
pub enum Index {
    /// One process's memory: actor key to the entry's raw id.
    Memory(ExpiringMap<String>),
    /// Redis: `<prefix><space>:<actor key>` holds the entry's hash.
    Redis {
        /// The shared handle.
        redis: Redis,
        /// The index space.
        space: IndexSpace,
        /// The pointer's lifetime, renewed by every replace.
        ttl: Duration,
    },
}

/// `ttl` in whole milliseconds for `PX`, at least 1 (Redis refuses 0).
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
    )
)]
pub(crate) fn millis(ttl: Duration) -> u64 {
    u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX).max(1)
}

// The bounds: `Serialize + DeserializeOwned` so the Redis arm can store the
// value as JSON, `Send + Sync + 'static` so a store can sit in `App` and be
// shared across tasks.
impl<V> Ephemeral<V>
where
    V: Serialize + DeserializeOwned + Send + Sync + 'static,
{
    /// A store: in memory without `[redis]`, else in Redis.
    ///
    /// # Arguments
    /// * `redis` - the instance's Redis, if `[redis]` is set.
    /// * `space` - the key space (Redis only).
    /// * `ttl` - each entry's lifetime.
    /// * `capacity` - the most live entries (memory only).
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
    pub fn new(redis: Option<&Redis>, space: Keyspace, ttl: Duration, capacity: usize) -> Self {
        match redis {
            None => Self::Memory(ExpiringMap::new(ttl, capacity)),
            Some(redis) => Self::Redis {
                redis: redis.clone(),
                space,
                ttl,
                _value: PhantomData,
            },
        }
    }

    /// Stores `value` under `key` for the store's lifetime, replacing any
    /// entry of that key.
    ///
    /// # Arguments
    /// * `key` - its key as the caller knows it (hashed before it reaches
    ///   Redis).
    /// * `value` - the value.
    /// * `now` - the current instant (read by the memory arm only).
    ///
    /// # Returns
    /// `false`, storing nothing, when the memory store is full; the Redis
    /// store is never full.
    ///
    /// # Errors
    /// [`OkfError::Busy`] when Redis fails (counted as `set`).
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "dispatch to the arm: each arm's own call is the span"
        )
    )]
    pub async fn insert(&self, key: &str, value: V, now: Instant) -> Result<bool, OkfError> {
        match self {
            Self::Memory(map) => Ok(map.insert(key.to_string(), value, now)),
            Self::Redis {
                redis, space, ttl, ..
            } => redis_insert(redis, *space, *ttl, key, &value).await,
        }
    }

    /// Removes and returns a live entry: a single-use read, atomic across
    /// instances (`GETDEL`).
    ///
    /// # Arguments
    /// * `key` - its key.
    /// * `now` - the current instant (memory only).
    ///
    /// # Returns
    /// The value, or `None` if absent, expired, or stored in a shape this
    /// version cannot read (counted as `decode`).
    ///
    /// # Errors
    /// [`OkfError::Busy`] when Redis fails (counted as `take`).
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "dispatch to the arm: each arm's own call is the span"
        )
    )]
    pub async fn take(&self, key: &str, now: Instant) -> Result<Option<V>, OkfError> {
        match self {
            Self::Memory(map) => Ok(map.take(key, now)),
            Self::Redis { redis, space, .. } => redis_take(redis, *space, key).await,
        }
    }

    /// Reads a live entry without removing it or changing its lifetime: a
    /// plain `GET` in Redis, a lookup in memory. Nothing is written.
    ///
    /// # Arguments
    /// * `key` - its key.
    /// * `now` - the current instant (memory only).
    /// * `read` - what to read from the value; its result is returned.
    ///
    /// # Returns
    /// `read`'s result, or `None` if absent, expired, or stored in a shape
    /// this version cannot read (counted as `decode`).
    ///
    /// # Errors
    /// [`OkfError::Busy`] when Redis fails (counted as `get`).
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "dispatch to the arm: each arm's own call is the span"
        )
    )]
    pub async fn peek<R>(
        &self,
        key: &str,
        now: Instant,
        read: impl FnOnce(&V) -> R,
    ) -> Result<Option<R>, OkfError> {
        match self {
            Self::Memory(map) => Ok(map.peek(key, now, read)),
            Self::Redis { redis, space, .. } => Ok(redis_peek::<V>(redis, *space, key)
                .await?
                .map(|value| read(&value))),
        }
    }

    /// Changes a live entry, keeping its lifetime.
    ///
    /// In memory, `change` runs once under the map's lock. In Redis it runs
    /// on a freshly decoded copy, which is written back only if the stored
    /// bytes did not change meanwhile; when they did, it runs again on a new
    /// copy, up to 8 times. So **`change` may run more than once, each time
    /// on a fresh copy, and must compute its result from the value alone**:
    /// clone captured inputs inside it, never `Option::take` or move them
    /// out, and do no I/O (`lessons.md`).
    ///
    /// # Arguments
    /// * `key` - its key.
    /// * `now` - the current instant (memory only).
    /// * `change` - what to do with the entry; its result is returned.
    ///
    /// # Returns
    /// `change`'s result from the run that was stored, or `None` if the
    /// entry is absent, expired or undecodable (counted as `decode`).
    ///
    /// # Errors
    /// [`OkfError::Busy`] when Redis fails, or when every attempt lost a
    /// race (both counted as `update`).
    // `impl FnMut(&mut V) -> R`: a closure that may be called several times
    // with a mutable borrow of the value (Python: a callback that mutates its
    // argument and may be retried).
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "dispatch to the arm: each arm's own call is the span"
        )
    )]
    pub async fn update<R>(
        &self,
        key: &str,
        now: Instant,
        change: impl FnMut(&mut V) -> R,
    ) -> Result<Option<R>, OkfError> {
        self.update_with_ttl_floor(key, now, Duration::ZERO, change)
            .await
    }

    /// [`Self::update`], and in Redis the entry then lives at least `floor`
    /// longer from the write, in the same atomic script as the write.
    ///
    /// For a change that leaves the entry in a terminal state a later reader
    /// must still find (a used authorization code, which is how a replay is
    /// told from an expired code). It never shortens an entry, and applies
    /// only when this write lands, so never call it for a change that keeps
    /// an entry usable. The memory arm judges liveness by `now`, so a floor
    /// changes nothing there.
    ///
    /// # Arguments
    /// * `key` - its key.
    /// * `now` - the current instant (memory only).
    /// * `floor` - the least remaining lifetime after the write
    ///   ([`Duration::ZERO`] keeps the remaining lifetime).
    /// * `change` - as for [`Self::update`].
    ///
    /// # Returns
    /// As [`Self::update`].
    ///
    /// # Errors
    /// As [`Self::update`].
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "dispatch to the arm: each arm's own call is the span"
        )
    )]
    pub async fn update_with_ttl_floor<R>(
        &self,
        key: &str,
        now: Instant,
        floor: Duration,
        change: impl FnMut(&mut V) -> R,
    ) -> Result<Option<R>, OkfError> {
        match self {
            // An `FnMut` closure is also an `FnOnce`, which the map takes.
            Self::Memory(map) => Ok(map.update(key, now, change)),
            Self::Redis { redis, space, .. } => {
                redis_update(redis, *space, key, floor, change).await
            }
        }
    }

    /// Drops an entry, live or not.
    ///
    /// # Arguments
    /// * `key` - its key.
    ///
    /// # Errors
    /// [`OkfError::Busy`] when Redis fails (counted as `remove`).
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "dispatch to the arm: each arm's own call is the span"
        )
    )]
    pub async fn remove(&self, key: &str) -> Result<(), OkfError> {
        match self {
            Self::Memory(map) => {
                map.remove(key);
                Ok(())
            }
            Self::Redis { redis, space, .. } => {
                redis_remove(redis, &redis.key(space.name(), &hashed(key))).await
            }
        }
    }

    /// Drops the entry an [`Index`] stopped pointing to.
    ///
    /// # Arguments
    /// * `stale` - from [`Index::replace`] on the index over this store.
    ///
    /// # Errors
    /// [`OkfError::Busy`] when Redis fails (counted as `remove`).
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "dispatch to the arm: each arm's own call is the span"
        )
    )]
    pub async fn remove_stale(&self, stale: &Stale) -> Result<(), OkfError> {
        match self {
            Self::Memory(map) => {
                map.remove(&stale.0);
                Ok(())
            }
            // The stale id is already the entry's hash: not hashed again.
            Self::Redis { redis, space, .. } => {
                redis_remove(redis, &redis.key(space.name(), &stale.0)).await
            }
        }
    }

    /// How many entries the memory store holds, expired ones included.
    ///
    /// # Panics
    /// On the Redis arm: Redis is not counted (spec Decision 8).
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        match self {
            Self::Memory(map) => map.len(),
            Self::Redis { .. } => panic!("len is memory-only"),
        }
    }
}

/// Encodes `value` in its space's envelope.
///
/// # Errors
/// [`OkfError::Busy`] if the value does not serialize (a type bug; counted
/// as `decode`, the operation for stored shapes).
#[tracing::instrument(level = "trace", skip_all)]
fn encode<V: Serialize>(redis: &Redis, space: Keyspace, value: &V) -> Result<Vec<u8>, OkfError> {
    serde_json::to_vec(&Envelope {
        v: space.version(),
        d: value,
    })
    .map_err(|_| {
        redis.count(RedisOp::Decode);
        OkfError::Busy {
            reason: "a value could not be stored".to_string(),
        }
    })
}

/// Decodes a stored envelope of `space`; another version or shape is
/// counted as `decode` and read as absent.
#[tracing::instrument(level = "trace", skip_all)]
fn decode<V: DeserializeOwned>(redis: &Redis, space: Keyspace, bytes: &[u8]) -> Option<V> {
    match serde_json::from_slice::<Envelope<V>>(bytes) {
        Ok(envelope) if envelope.v == space.version() => Some(envelope.d),
        _ => {
            redis.undecodable(space.name());
            None
        }
    }
}

/// `SET <key> <envelope> PX <ttl>`.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "redis", db.operation.name = "set"))]
async fn redis_insert<V: Serialize>(
    redis: &Redis,
    space: Keyspace,
    ttl: Duration,
    key: &str,
    value: &V,
) -> Result<bool, OkfError> {
    let bytes = encode(redis, space, value)?;
    let _: () = redis::cmd("SET")
        .arg(redis.key(space.name(), &hashed(key)))
        .arg(bytes)
        .arg("PX")
        .arg(millis(ttl))
        .query_async(&mut redis.connection())
        .await
        .map_err(|error| redis.failed(RedisOp::Set, &error))?;
    Ok(true)
}

/// `GETDEL <key>`, then decode.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "redis", db.operation.name = "take"))]
async fn redis_take<V: DeserializeOwned>(
    redis: &Redis,
    space: Keyspace,
    key: &str,
) -> Result<Option<V>, OkfError> {
    let bytes: Option<Vec<u8>> = redis::cmd("GETDEL")
        .arg(redis.key(space.name(), &hashed(key)))
        .query_async(&mut redis.connection())
        .await
        .map_err(|error| redis.failed(RedisOp::Take, &error))?;
    Ok(bytes.and_then(|bytes| decode(redis, space, &bytes)))
}

/// `GET`, decoded; nothing is written.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "redis", db.operation.name = "get"))]
async fn redis_peek<V: DeserializeOwned>(
    redis: &Redis,
    space: Keyspace,
    key: &str,
) -> Result<Option<V>, OkfError> {
    let bytes: Option<Vec<u8>> = redis::cmd("GET")
        .arg(redis.key(space.name(), &hashed(key)))
        .query_async(&mut redis.connection())
        .await
        .map_err(|error| redis.failed(RedisOp::Get, &error))?;
    Ok(bytes.and_then(|bytes| decode(redis, space, &bytes)))
}

/// `GET`, change a decoded copy, then [`COMPARE_AND_SET`] the old bytes for
/// the new; up to [`UPDATE_ATTEMPTS`] times.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "redis", db.operation.name = "update"))]
async fn redis_update<V, R>(
    redis: &Redis,
    space: Keyspace,
    key: &str,
    floor: Duration,
    mut change: impl FnMut(&mut V) -> R,
) -> Result<Option<R>, OkfError>
where
    V: Serialize + DeserializeOwned,
{
    let floor_millis = if floor.is_zero() { 0 } else { millis(floor) };
    let key = redis.key(space.name(), &hashed(key));
    let mut connection = redis.connection();
    for _ in 0..UPDATE_ATTEMPTS {
        let read: Option<Vec<u8>> = redis::cmd("GET")
            .arg(&key)
            .query_async(&mut connection)
            .await
            .map_err(|error| redis.failed(RedisOp::Update, &error))?;
        let Some(old) = read else {
            return Ok(None);
        };
        let Some(mut value) = decode::<V>(redis, space, &old) else {
            return Ok(None);
        };
        let result = change(&mut value);
        let new = encode(redis, space, &value)?;
        let written: i64 = COMPARE_AND_SET_SCRIPT
            .key(&key)
            .arg(old)
            .arg(new)
            .arg(floor_millis)
            .invoke_async(&mut connection)
            .await
            .map_err(|error| redis.failed(RedisOp::Update, &error))?;
        match written {
            1 => return Ok(Some(result)),
            -1 => return Ok(None),
            // 0: another writer changed it after our read; run again on a
            // fresh copy.
            _ => {}
        }
    }
    redis.count(RedisOp::Update);
    Err(OkfError::Busy {
        reason: "the entry is changing too fast".to_string(),
    })
}

/// `DEL <key>` (a full key name).
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "redis", db.operation.name = "remove"))]
async fn redis_remove(redis: &Redis, key: &str) -> Result<(), OkfError> {
    let _: i64 = redis::cmd("DEL")
        .arg(key)
        .query_async(&mut redis.connection())
        .await
        .map_err(|error| redis.failed(RedisOp::Remove, &error))?;
    Ok(())
}

impl Index {
    /// An index: in memory without `[redis]`, else in Redis.
    ///
    /// # Arguments
    /// * `redis` - the instance's Redis, if `[redis]` is set.
    /// * `space` - the index space (Redis only).
    /// * `ttl` - the pointer's sliding lifetime.
    /// * `capacity` - the most live actors (memory only).
    ///
    /// # Returns
    /// The index.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial constructor: builds a value from its arguments"
        )
    )]
    pub fn new(redis: Option<&Redis>, space: IndexSpace, ttl: Duration, capacity: usize) -> Self {
        match redis {
            None => Self::Memory(ExpiringMap::new(ttl, capacity)),
            Some(redis) => Self::Redis {
                redis: redis.clone(),
                space,
                ttl,
            },
        }
    }

    /// Points `actor_key` at `entry`, renewing the pointer's lifetime, in one
    /// atomic step; reports the entry it pointed at before.
    ///
    /// # Arguments
    /// * `actor_key` - the actor, already a hash (callers pass the
    ///   `URL_SAFE_NO_PAD` SHA-256 of the console session id).
    /// * `entry` - the new entry's key in the matching [`Ephemeral`], as
    ///   presented (Redis stores only its hash).
    /// * `now` - the current instant (memory only).
    ///
    /// # Returns
    /// [`Swap::Swapped`] with the replaced entry when it differs from
    /// `entry`, or [`Swap::Full`] when the memory index is full.
    ///
    /// # Errors
    /// [`OkfError::Busy`] when Redis fails (counted as `index`).
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "dispatch to the arm: each arm's own call is the span"
        )
    )]
    pub async fn replace(
        &self,
        actor_key: &str,
        entry: &str,
        now: Instant,
    ) -> Result<Swap, OkfError> {
        match self {
            Self::Memory(map) => Ok(map
                .update_or_insert_sliding(
                    actor_key,
                    now,
                    || entry.to_string(),
                    |held| std::mem::replace(held, entry.to_string()),
                )
                .map_or(Swap::Full, |previous| {
                    Swap::Swapped((previous != entry).then_some(Stale(previous)))
                })),
            Self::Redis { redis, space, ttl } => {
                redis_replace(redis, *space, *ttl, actor_key, entry).await
            }
        }
    }
}

/// `SET <actor key> h(entry) PX <ttl> GET`: the swap and the old pointer in
/// one command (Redis 6.2+).
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "redis", db.operation.name = "index"))]
async fn redis_replace(
    redis: &Redis,
    space: IndexSpace,
    ttl: Duration,
    actor_key: &str,
    entry: &str,
) -> Result<Swap, OkfError> {
    let entry = hashed(entry);
    let previous: Option<String> = redis::cmd("SET")
        .arg(redis.key(space.name(), actor_key))
        .arg(&entry)
        .arg("PX")
        .arg(millis(ttl))
        .arg("GET")
        .query_async(&mut redis.connection())
        .await
        .map_err(|error| redis.failed(RedisOp::Index, &error))?;
    Ok(Swap::Swapped(
        previous.filter(|previous| *previous != entry).map(Stale),
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use super::*;
    use crate::db::redis_support::TestRedis;
    use crate::shared::testing;

    const TTL: Duration = Duration::from_secs(60);
    /// The bound on every test body (containers start inside it).
    const BOUND: Duration = Duration::from_secs(120);

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    struct Sample {
        n: u32,
        used: bool,
    }

    const ONE: Sample = Sample { n: 1, used: false };

    /// A memory store and a Redis store over `redis`.
    fn stores(redis: &Redis, ttl: Duration) -> [Ephemeral<Sample>; 2] {
        [
            Ephemeral::new(None, Keyspace::OauthCode, ttl, 10),
            Ephemeral::new(Some(redis), Keyspace::OauthCode, ttl, 10),
        ]
    }

    /// Runs a raw command on `redis` (test set-up and inspection).
    async fn raw<T: redis::FromRedisValue>(redis: &Redis, cmd: &mut redis::Cmd) -> T {
        cmd.query_async(&mut redis.connection())
            .await
            .expect("a raw redis command")
    }

    async fn insert_then_take_is_single_use(server: TestRedis) {
        let redis = testing::connect(&server).await;
        for store in stores(&redis, TTL) {
            let now = Instant::now();
            assert!(store.insert("a", ONE, now).await.unwrap());
            assert_eq!(store.take("a", now).await.unwrap(), Some(ONE));
            assert_eq!(store.take("a", now).await.unwrap(), None);
            assert_eq!(store.update("a", now, |s| s.n).await.unwrap(), None);
            if let Ephemeral::Memory(_) = store {
                assert_eq!(store.len(), 0);
            }
        }
    }

    async fn peek_reads_without_consuming_or_renewing(server: TestRedis) {
        let redis = testing::connect(&server).await;
        for store in stores(&redis, TTL) {
            let now = Instant::now();
            assert!(store.insert("a", ONE, now).await.unwrap());
            let later = now + Duration::from_secs(30);
            // Two reads see the value; neither consumes it.
            assert_eq!(store.peek("a", later, |s| s.n).await.unwrap(), Some(1));
            assert_eq!(store.peek("a", later, |s| s.n).await.unwrap(), Some(1));
            assert_eq!(store.peek("absent", later, |s| s.n).await.unwrap(), None);
            // The lifetime was not renewed: the memory arm's entry still
            // expires at `now + TTL`; the Redis arm is checked below.
            if let Ephemeral::Memory(_) = store {
                assert_eq!(store.peek("a", now + TTL, |s| s.n).await.unwrap(), None);
            }
            assert_eq!(store.take("a", later).await.unwrap(), Some(ONE));
            assert_eq!(store.peek("a", later, |s| s.n).await.unwrap(), None);
        }
        // Redis: a peek leaves the remaining lifetime alone.
        let store = Ephemeral::<Sample>::new(Some(&redis), Keyspace::OauthCode, TTL, 10);
        let now = Instant::now();
        assert!(store.insert("b", ONE, now).await.unwrap());
        let key = redis.key(Keyspace::OauthCode.name(), &hashed("b"));
        let before: i64 = raw(&redis, redis::cmd("PTTL").arg(&key)).await;
        assert_eq!(store.peek("b", now, |s| s.n).await.unwrap(), Some(1));
        let after: i64 = raw(&redis, redis::cmd("PTTL").arg(&key)).await;
        assert!(
            after <= before && after > 0,
            "peek must not renew: {before} then {after}"
        );
    }

    async fn two_takes_racing_get_one_value(server: TestRedis) {
        // Two connections: two "replicas" race one GETDEL per key.
        let (one, two) = (
            testing::connect(&server).await,
            testing::connect(&server).await,
        );
        let a = Ephemeral::<Sample>::new(Some(&one), Keyspace::OauthCode, TTL, 10);
        let b = Ephemeral::<Sample>::new(Some(&two), Keyspace::OauthCode, TTL, 10);
        for round in 0..20 {
            let key = format!("k{round}");
            a.insert(&key, ONE, Instant::now()).await.unwrap();
            let (x, y) = tokio::join!(a.take(&key, Instant::now()), b.take(&key, Instant::now()));
            let got = [x.unwrap(), y.unwrap()];
            assert_eq!(got.iter().flatten().count(), 1, "round {round}: {got:?}");
        }
    }

    /// 16 tasks over two handles each add one, retrying a `Busy` (a client
    /// retrying the 503): every increment applies exactly once.
    async fn concurrent_updates_all_apply_once(server: TestRedis) {
        const TASKS: u32 = 16;
        let (one, two) = (
            testing::connect(&server).await,
            testing::connect(&server).await,
        );
        let handles = [
            Arc::new(Ephemeral::<Sample>::new(None, Keyspace::OauthCode, TTL, 10)),
            Arc::new(Ephemeral::new(Some(&one), Keyspace::OauthCode, TTL, 10)),
            Arc::new(Ephemeral::new(Some(&two), Keyspace::OauthCode, TTL, 10)),
        ];
        // Memory alone, then the two Redis handles together.
        for group in [&handles[..1], &handles[1..]] {
            let now = Instant::now();
            group[0]
                .insert("k", Sample { n: 0, used: false }, now)
                .await
                .unwrap();
            let tasks: Vec<_> = (0..TASKS)
                .map(|task| {
                    let store = Arc::clone(&group[task as usize % group.len()]);
                    tokio::spawn(async move {
                        loop {
                            match store
                                .update("k", now, |s| {
                                    s.n += 1;
                                    s.n
                                })
                                .await
                            {
                                Ok(seen) => return seen.expect("the entry is live"),
                                Err(OkfError::Busy { .. }) => tokio::task::yield_now().await,
                                Err(other) => panic!("{other:?}"),
                            }
                        }
                    })
                })
                .collect();
            let mut seen = Vec::with_capacity(TASKS as usize);
            for task in tasks {
                seen.push(task.await.expect("an update task"));
            }
            // Each stored run saw a distinct value: no update was lost or
            // applied twice.
            seen.sort_unstable();
            assert_eq!(seen, (1..=TASKS).collect::<Vec<_>>());
            let stored = group[0].take("k", now).await.unwrap().expect("the entry");
            assert_eq!(stored.n, TASKS);
        }
    }

    async fn update_keeps_the_fixed_lifetime_and_an_expired_entry_is_gone(server: TestRedis) {
        let redis = testing::connect(&server).await;
        // Memory: explicit instants.
        let memory = Ephemeral::<Sample>::new(None, Keyspace::OauthCode, TTL, 10);
        let start = Instant::now();
        memory.insert("k", ONE, start).await.unwrap();
        let later = start + TTL / 2;
        assert_eq!(memory.update("k", later, |s| s.n).await.unwrap(), Some(1));
        assert_eq!(memory.take("k", start + TTL).await.unwrap(), None);

        // Redis: the remaining TTL is kept, not renewed, and then it expires.
        let ttl = Duration::from_millis(1500);
        let store = Ephemeral::<Sample>::new(Some(&redis), Keyspace::OauthCode, ttl, 10);
        let key = redis.key(Keyspace::OauthCode.name(), &hashed("k"));
        store.insert("k", ONE, Instant::now()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        let updated = store
            .update("k", Instant::now(), |s| {
                s.used = true;
                s.n
            })
            .await
            .unwrap();
        assert_eq!(updated, Some(1));
        let left: i64 = raw(&redis, redis::cmd("PTTL").arg(&key)).await;
        assert!(left > 0 && left <= 1000, "remaining {left} ms");
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert_eq!(store.take("k", Instant::now()).await.unwrap(), None);
    }

    /// A floor lengthens a written entry's life and never shortens it; no
    /// floor leaves the remaining life; a lost entry stays lost.
    async fn a_ttl_floor_only_lengthens_the_entry_it_writes(server: TestRedis) {
        let redis = testing::connect(&server).await;
        let store = Ephemeral::<Sample>::new(Some(&redis), Keyspace::OauthCode, TTL, 10);
        let now = Instant::now();
        let key = redis.key(Keyspace::OauthCode.name(), &hashed("k"));
        store
            .insert("k", Sample { n: 0, used: false }, now)
            .await
            .unwrap();
        let _: bool = raw(&redis, redis::cmd("PEXPIRE").arg(&key).arg(5_000)).await;

        store.update("k", now, |s| s.n += 1).await.unwrap();
        let unchanged: i64 = raw(&redis, redis::cmd("PTTL").arg(&key)).await;
        store
            .update_with_ttl_floor("k", now, Duration::from_secs(1), |s| s.n += 1)
            .await
            .unwrap();
        let not_shortened: i64 = raw(&redis, redis::cmd("PTTL").arg(&key)).await;
        store
            .update_with_ttl_floor("k", now, Duration::from_secs(30), |s| s.n += 1)
            .await
            .unwrap();
        let lengthened: i64 = raw(&redis, redis::cmd("PTTL").arg(&key)).await;
        let _: bool = raw(&redis, redis::cmd("PEXPIRE").arg(&key).arg(1)).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let gone = store
            .update_with_ttl_floor("k", now, Duration::from_secs(30), |s| s.n)
            .await
            .unwrap();

        assert!(unchanged <= 5_000, "no floor: {unchanged}");
        assert!(
            not_shortened > 1_000 && not_shortened <= 5_000,
            "{not_shortened}"
        );
        assert!(lengthened > 25_000 && lengthened <= 30_000, "{lengthened}");
        assert_eq!(gone, None, "an expired entry is not revived");
        let exists: i64 = raw(&redis, redis::cmd("EXISTS").arg(&key)).await;
        assert_eq!(exists, 0);
    }

    async fn a_value_of_another_version_or_shape_is_absent_and_counted(server: TestRedis) {
        let (_provider, metrics, registry) = crate::mcp::testing::local_metrics();
        let redis = Redis::connect(&testing::config(&server), &server.env(), &metrics)
            .await
            .expect("connect to the test redis");
        let store = Ephemeral::<Sample>::new(Some(&redis), Keyspace::OauthCode, TTL, 10);
        let key = redis.key(Keyspace::OauthCode.name(), &hashed("k"));
        let bad: [&[u8]; 3] = [
            br#"{"v":99,"d":{"n":1,"used":false}}"#,
            b"not json",
            br#"{"v":1,"d":{"other":1}}"#,
        ];
        for value in bad {
            let _: () = raw(&redis, redis::cmd("SET").arg(&key).arg(value)).await;
            assert_eq!(store.take("k", Instant::now()).await.unwrap(), None);
        }
        let _: () = raw(&redis, redis::cmd("SET").arg(&key).arg(bad[0])).await;
        assert_eq!(
            store.update("k", Instant::now(), |s| s.n).await.unwrap(),
            None
        );
        let rendered = crate::mcp::testing::render(&registry);
        assert_eq!(testing::error_count(&rendered, "decode"), 4, "{rendered}");
    }

    async fn redis_keys_never_hold_the_presented_id(server: TestRedis) {
        let redis = testing::connect(&server).await;
        let store = Ephemeral::<Sample>::new(Some(&redis), Keyspace::OauthCode, TTL, 10);
        let index = Index::new(Some(&redis), IndexSpace::PasskeySession, TTL, 10);
        store
            .insert("cookie-id-xyz", ONE, Instant::now())
            .await
            .unwrap();
        index
            .replace("actor-hash", "cookie-id-xyz", Instant::now())
            .await
            .unwrap();
        let pattern = format!("{{{}}}*", server.prefix);
        let keys: Vec<String> = raw(&redis, redis::cmd("KEYS").arg(&pattern)).await;
        assert_eq!(keys.len(), 2, "{keys:?}");
        assert!(
            keys.iter().all(|key| !key.contains("cookie-id-xyz")),
            "{keys:?}"
        );
        assert!(keys.contains(&redis.key("oauth:code", &hashed("cookie-id-xyz"))));
        let pointer: String = raw(
            &redis,
            redis::cmd("GET").arg(redis.key("passkey:session", "actor-hash")),
        )
        .await;
        assert_eq!(pointer, hashed("cookie-id-xyz"));
    }

    async fn an_index_swaps_atomically_and_returns_the_entry_it_replaced(server: TestRedis) {
        let redis = testing::connect(&server).await;
        let now = Instant::now();
        let pairs = [
            (
                Ephemeral::<Sample>::new(None, Keyspace::PasskeyCeremony, TTL, 10),
                Index::new(None, IndexSpace::PasskeySession, TTL, 10),
            ),
            (
                Ephemeral::new(Some(&redis), Keyspace::PasskeyCeremony, TTL, 10),
                Index::new(Some(&redis), IndexSpace::PasskeySession, TTL, 10),
            ),
        ];
        for (store, index) in pairs {
            for entry in ["e1", "e2"] {
                store.insert(entry, ONE, now).await.unwrap();
            }
            assert_eq!(
                index.replace("actor", "e1", now).await.unwrap(),
                Swap::Swapped(None)
            );
            let Swap::Swapped(Some(stale)) = index.replace("actor", "e2", now).await.unwrap()
            else {
                panic!("replacing e1 returns it as stale");
            };
            assert_eq!(format!("{stale:?}"), "Stale(<redacted>)");
            store.remove_stale(&stale).await.unwrap();
            assert_eq!(store.take("e1", now).await.unwrap(), None);
            assert_eq!(store.take("e2", now).await.unwrap(), Some(ONE));
            // The same entry again is not stale.
            assert_eq!(
                index.replace("actor", "e2", now).await.unwrap(),
                Swap::Swapped(None)
            );
        }
        // The Redis pointer slides: every replace renews it.
        let short = Index::new(
            Some(&redis),
            IndexSpace::OidcLink,
            Duration::from_millis(5000),
            10,
        );
        short.replace("slider", "e1", now).await.unwrap();
        let key = redis.key(IndexSpace::OidcLink.name(), "slider");
        let _: bool = raw(&redis, redis::cmd("PEXPIRE").arg(&key).arg(100)).await;
        short.replace("slider", "e1", now).await.unwrap();
        let left: i64 = raw(&redis, redis::cmd("PTTL").arg(&key)).await;
        assert!(left > 1000, "renewed: {left} ms");
    }

    async fn a_redis_outage_is_busy_and_counted(server: TestRedis) {
        // A shared server cannot be stopped for everyone.
        let server = server.dedicated().await;
        let (_provider, metrics, registry) = crate::mcp::testing::local_metrics();
        let redis = Redis::connect(&testing::config(&server), &server.env(), &metrics)
            .await
            .expect("connect to the test redis");
        let store = Ephemeral::<Sample>::new(Some(&redis), Keyspace::OauthCode, TTL, 10);
        server.stop().await;
        let error = store.insert("k", ONE, Instant::now()).await.unwrap_err();
        assert!(matches!(error, OkfError::Busy { .. }), "{error:?}");
        let rendered = crate::mcp::testing::render(&registry);
        assert_eq!(testing::error_count(&rendered, "set"), 1, "{rendered}");
    }

    /// A value whose `Serialize` fails when `poison` is set (a type bug).
    #[derive(Debug, PartialEq, Deserialize)]
    struct Fragile {
        n: u32,
        poison: bool,
    }

    // Hand-written so it can fail: `serde::ser::Error::custom` is how a
    // `Serialize` impl reports an error.
    impl Serialize for Fragile {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            use serde::ser::SerializeStruct;
            if self.poison {
                return Err(serde::ser::Error::custom("poisoned"));
            }
            let mut fields = serializer.serialize_struct("Fragile", 2)?;
            fields.serialize_field("n", &self.n)?;
            fields.serialize_field("poison", &self.poison)?;
            fields.end()
        }
    }

    /// A value that does not encode answers `Busy`, counts one `decode`, and
    /// writes nothing: no key on insert, the old bytes kept on update.
    async fn a_value_that_does_not_encode_is_busy_and_writes_nothing(server: TestRedis) {
        let (_provider, metrics, registry) = crate::mcp::testing::local_metrics();
        let redis = Redis::connect(&testing::config(&server), &server.env(), &metrics)
            .await
            .expect("connect to the test redis");
        let store = Ephemeral::<Fragile>::new(Some(&redis), Keyspace::OauthCode, TTL, 10);
        let key = redis.key(Keyspace::OauthCode.name(), &hashed("k"));
        let poisoned = Fragile { n: 1, poison: true };
        let error = store
            .insert("k", poisoned, Instant::now())
            .await
            .unwrap_err();
        assert!(matches!(error, OkfError::Busy { .. }), "{error:?}");
        let decode = |registry: &prometheus::Registry| {
            testing::error_count(&crate::mcp::testing::render(registry), "decode")
        };
        assert_eq!(decode(&registry), 1);
        let exists: i64 = raw(&redis, redis::cmd("EXISTS").arg(&key)).await;
        assert_eq!(exists, 0, "a failed encode wrote the key");

        let healthy = Fragile {
            n: 1,
            poison: false,
        };
        store.insert("k", healthy, Instant::now()).await.unwrap();
        let before: Vec<u8> = raw(&redis, redis::cmd("GET").arg(&key)).await;
        let error = store
            .update("k", Instant::now(), |value| {
                value.n = 2;
                value.poison = true;
            })
            .await
            .unwrap_err();
        assert!(matches!(error, OkfError::Busy { .. }), "{error:?}");
        assert_eq!(decode(&registry), 2);
        let after: Vec<u8> = raw(&redis, redis::cmd("GET").arg(&key)).await;
        assert_eq!(after, before, "a failed encode changed the stored bytes");
    }

    /// The change closure reruns on a fresh copy when another writer gets in
    /// between its read and its write (the `lessons.md` rule).
    async fn a_lost_race_reruns_the_change_on_a_fresh_copy(server: TestRedis) {
        let (one, two) = (
            testing::connect(&server).await,
            testing::connect(&server).await,
        );
        let store = Ephemeral::<Sample>::new(Some(&one), Keyspace::OauthCode, TTL, 10);
        let other = Ephemeral::<Sample>::new(Some(&two), Keyspace::OauthCode, TTL, 10);
        store.insert("k", ONE, Instant::now()).await.unwrap();
        let runs = AtomicUsize::new(0);
        let result = store
            .update("k", Instant::now(), |s| {
                if runs.fetch_add(1, Ordering::SeqCst) == 0 {
                    // Another replica writes between this run's read and its
                    // write. `block_in_place` lets this sync closure wait on
                    // async code (multi-thread runtime only).
                    tokio::task::block_in_place(|| {
                        tokio::runtime::Handle::current().block_on(other.insert(
                            "k",
                            Sample { n: 10, used: false },
                            Instant::now(),
                        ))
                    })
                    .expect("the interfering write");
                }
                s.n += 1;
                s.n
            })
            .await
            .unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 2);
        assert_eq!(result, Some(11));
        assert_eq!(
            store.take("k", Instant::now()).await.unwrap(),
            Some(Sample { n: 11, used: false })
        );
    }

    /// An entry that changes under every attempt answers `Busy` after 8.
    async fn an_update_that_always_loses_is_busy_after_eight_attempts(server: TestRedis) {
        let (one, two) = (
            testing::connect(&server).await,
            testing::connect(&server).await,
        );
        let store = Ephemeral::<Sample>::new(Some(&one), Keyspace::OauthCode, TTL, 10);
        let other = Ephemeral::<Sample>::new(Some(&two), Keyspace::OauthCode, TTL, 10);
        store.insert("k", ONE, Instant::now()).await.unwrap();
        let runs = AtomicUsize::new(0);
        let error = store
            .update("k", Instant::now(), |s| {
                let run = runs.fetch_add(1, Ordering::SeqCst);
                let n = u32::try_from(run).expect("a small count") + 100;
                tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(other.insert(
                        "k",
                        Sample { n, used: false },
                        Instant::now(),
                    ))
                })
                .expect("the interfering write");
                s.n += 1;
            })
            .await
            .unwrap_err();
        assert!(matches!(error, OkfError::Busy { .. }), "{error:?}");
        assert_eq!(runs.load(Ordering::SeqCst), UPDATE_ATTEMPTS);
    }

    #[tokio::test]
    async fn a_memory_index_is_full_and_stale_ids_never_print() {
        let index = Index::new(None, IndexSpace::OidcLink, TTL, 1);
        let now = Instant::now();
        assert_eq!(
            index.replace("one", "e1", now).await.unwrap(),
            Swap::Swapped(None)
        );
        assert_eq!(index.replace("two", "e2", now).await.unwrap(), Swap::Full);
        assert!(!format!("{:?}", Stale("cookie-id-xyz".into())).contains("cookie-id-xyz"));
    }

    #[test]
    fn every_keyspace_has_a_distinct_name_and_version_one() {
        let spaces = [
            Keyspace::OauthPending,
            Keyspace::OauthCode,
            Keyspace::OidcFlow,
            Keyspace::OidcReauth,
            Keyspace::PasskeyCeremony,
        ];
        let names: std::collections::HashSet<_> = spaces.iter().map(|s| s.name()).collect();
        assert_eq!(names.len(), spaces.len());
        assert!(spaces.iter().all(|s| s.version() == 1));
        assert_ne!(
            IndexSpace::OidcLink.name(),
            IndexSpace::PasskeySession.name()
        );
    }

    // Each body above runs twice: on a standalone Redis and on a one-node
    // cluster. `macro_rules!` writes the two `#[tokio::test]` wrappers for
    // every name (Python: a parametrized test).
    macro_rules! in_both_modes {
        ($($name:ident),* $(,)?) => {
            mod standalone {
                $(
                    #[tokio::test(flavor = "multi_thread")]
                    async fn $name() {
                        let server = crate::db::redis_support::TestRedis::start().await;
                        tokio::time::timeout(super::BOUND, super::$name(server))
                            .await
                            .expect(concat!(stringify!($name), " finished within the bound"));
                    }
                )*
            }
            mod cluster {
                $(
                    #[tokio::test(flavor = "multi_thread")]
                    async fn $name() {
                        let server = crate::db::redis_support::TestRedis::start_cluster().await;
                        tokio::time::timeout(super::BOUND, super::$name(server))
                            .await
                            .expect(concat!(stringify!($name), " finished within the bound"));
                    }
                )*
            }
        };
    }

    in_both_modes!(
        insert_then_take_is_single_use,
        peek_reads_without_consuming_or_renewing,
        two_takes_racing_get_one_value,
        concurrent_updates_all_apply_once,
        update_keeps_the_fixed_lifetime_and_an_expired_entry_is_gone,
        a_ttl_floor_only_lengthens_the_entry_it_writes,
        a_value_of_another_version_or_shape_is_absent_and_counted,
        redis_keys_never_hold_the_presented_id,
        an_index_swaps_atomically_and_returns_the_entry_it_replaced,
        a_redis_outage_is_busy_and_counted,
        a_lost_race_reruns_the_change_on_a_fresh_copy,
        an_update_that_always_loses_is_busy_after_eight_attempts,
        a_value_that_does_not_encode_is_busy_and_writes_nothing,
    );
}
