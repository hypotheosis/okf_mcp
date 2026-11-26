//! `Bounded<K, V>`: the one cache every M18 cache is built on (spec
//! Decisions 2, 11, 12 and 19).
//!
//! It wraps a moka `future::Cache` and adds what moka does not give:
//!
//! * **Single flight.** However many callers miss on one key together, one
//!   of them runs the loader and the others share its result (moka's
//!   `try_get_with`).
//! * **Invalidation epochs.** A load that is overtaken by an invalidation
//!   of its key (the database changed while it was reading) still serves
//!   its caller, but is not kept. The epochs exist only for keys with a load
//!   in flight, so invalidating a key that nobody is loading keeps no state
//!   and the map is bounded by the number of concurrent loads.
//! * **A cache-wide fence.** [`Bounded::invalidate_where`] cannot name a key
//!   whose load is still running, so it also moves one counter that every
//!   load compares at its end.
//! * **Stale marking.** [`Bounded::mark_all_stale`] makes every entry loaded
//!   before it ask a caller-supplied check on its next use, instead of being
//!   dropped (used after a lost `NOTIFY` connection).
//! * **Metrics** (`okf.cache.*`), once [`Bounded::with_metrics`] is called.
//!
//! # For Python readers
//!
//! `Bounded` is shared by reference (`&self`, usually inside an `Arc`) and
//! every method takes `&self`: the interior state sits behind a
//! `std::sync::Mutex` (held only for a map lookup, never across an `.await`)
//! and atomics, like a Python object guarded by a `threading.Lock`.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::hash::Hash;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use moka::future::Cache;
use moka::notification::RemovalCause;

use crate::error::OkfError;
use crate::telemetry::metrics::{Metrics, Outcome};

/// How often one cache logs that it refused to keep an over-budget value.
const OVERSIZE_WARN_EVERY: Duration = Duration::from_secs(3600);

/// Which cache, for metric labels and logs: a closed set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheName {
    /// Catalog snapshots by catalog id.
    Catalog,
    /// Per-connection overlays.
    Overlay,
    /// Catalog rows by name.
    Directory,
    /// API keys, by hash.
    Credential,
    /// OAuth grants, by id.
    Grant,
    /// Users.
    User,
    /// Labels.
    Labels,
}

impl CacheName {
    /// The label value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Catalog => "catalog",
            Self::Overlay => "overlay",
            Self::Directory => "directory",
            Self::Credential => "credential",
            Self::Grant => "grant",
            Self::User => "user",
            Self::Labels => "labels",
        }
    }
}

/// A cache's capacity: by entry count, or by estimated bytes through the
/// weigher given to [`Bounded::with_weigher`] (an unweighed entry counts 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    /// At most this many entries.
    Entries(u64),
    /// At most this many bytes of weighed entries.
    Bytes(u64),
}

/// What a lookup found: the `result` label of `okf.cache.requests`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookup {
    /// A current entry.
    Hit,
    /// Nothing cached: the loader ran.
    Miss,
    /// An entry marked stale: it was revalidated or reloaded.
    Stale,
}

impl Lookup {
    /// The label value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Miss => "miss",
            Self::Stale => "stale",
        }
    }
}

/// Why an entry left the cache: the `cause` label of `okf.cache.evictions`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Eviction {
    /// The cache was full.
    Size,
    /// The entry outlived the time to live.
    Ttl,
    /// It was dropped on purpose (an invalidation).
    Invalidated,
}

impl Eviction {
    /// The label value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Size => "size",
            Self::Ttl => "ttl",
            Self::Invalidated => "invalidated",
        }
    }
}

/// What the `okf.cache.entries` and `okf.cache.bytes` gauges read.
pub trait CacheSize: Send + Sync {
    /// Entries held.
    fn entries(&self) -> u64;
    /// Estimated bytes held, for a cache limited by bytes.
    fn bytes(&self) -> Option<u64>;
}

/// A cached value with the stale counter at the time its load began.
#[derive(Clone)]
struct Entry<V> {
    value: V,
    stamp: u64,
}

/// A load or revalidation registered for a key (see [`Flight`]).
struct InFlight {
    loads: u32,
    epoch: u64,
}

/// Why moka's `try_get_with` did not insert: shared among the callers that
/// waited on the one load.
enum Load<V> {
    /// The loader failed (the leader keeps the error; see `load_once`).
    /// `outage` names the store when the failure was
    /// [`OkfError::StoreUnavailable`], so the callers that shared the load
    /// are told the store is unavailable too, not a generic `Busy` retry.
    Failed { outage: Option<String> },
    /// The loader succeeded but its value must not be kept.
    Uncached(V),
    /// The loader found nothing, which is never cached
    /// ([`Bounded::get_or_load_present`]).
    Absent,
}

/// The gauges' view of a cache: a second handle on the moka cache, held
/// behind an `Arc` so the meter holds only a `Weak` and the cache can drop.
struct Probe<K, V> {
    cache: Cache<K, Entry<V>>,
    by_bytes: bool,
}

impl<K, V> CacheSize for Probe<K, V>
where
    K: Hash + Eq + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "a gauge callback reading one counter")
    )]
    fn entries(&self) -> u64 {
        self.cache.entry_count()
    }

    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "a gauge callback reading one counter")
    )]
    fn bytes(&self) -> Option<u64> {
        self.by_bytes.then(|| self.cache.weighted_size())
    }
}

/// A load in progress for one key. While it lives, the key has an epoch
/// that [`Bounded::invalidate`] moves; [`Flight::current`] tells the load
/// whether anything overtook it. Dropping it (finished, failed or
/// cancelled) deregisters the key, removing the epoch with the last load.
struct Flight<'a, K: Hash + Eq> {
    inflight: &'a Mutex<HashMap<K, InFlight>>,
    fence: &'a AtomicU64,
    key: K,
    epoch: u64,
    fence_seen: u64,
}

impl<K: Hash + Eq> Flight<'_, K> {
    /// Whether neither this key nor the whole cache was invalidated since
    /// the load began.
    #[tracing::instrument(level = "trace", skip_all)]
    fn current(&self) -> bool {
        let epoch = locked(self.inflight)
            .get(&self.key)
            .map_or(0, |flight| flight.epoch);
        epoch == self.epoch && self.fence.load(Ordering::SeqCst) == self.fence_seen
    }
}

// `Drop` runs when the value goes out of scope, like a Python context
// manager's `__exit__`, and also when a cancelled future is dropped.
impl<K: Hash + Eq> Drop for Flight<'_, K> {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "a drop hook that edits one map entry")
    )]
    fn drop(&mut self) {
        let mut map = locked(self.inflight);
        if let Some(flight) = map.get_mut(&self.key) {
            flight.loads -= 1;
            if flight.loads == 0 {
                map.remove(&self.key);
            }
        }
    }
}

/// Locks a mutex, ignoring poisoning: the guarded maps hold plain counters,
/// so a panic elsewhere cannot leave them half-updated.
#[tracing::instrument(level = "trace", skip_all)]
fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The weigher type: how many bytes (or units) one value counts.
type Weigher<K, V> = Arc<dyn Fn(&K, &V) -> u32 + Send + Sync>;

/// A bounded async cache with single-flight loads, per-key invalidation
/// epochs and stale marking.
pub struct Bounded<K, V> {
    name: CacheName,
    limit: Limit,
    ttl: Duration,
    idle: Option<Duration>,
    weigher: Option<Weigher<K, V>>,
    metrics: Option<Metrics>,
    cache: Cache<K, Entry<V>>,
    inflight: Mutex<HashMap<K, InFlight>>,
    fence: AtomicU64,
    stale_before: AtomicU64,
    probe: Option<Arc<Probe<K, V>>>,
    oversize_warned: Mutex<Option<Instant>>,
}

impl<K, V> fmt::Debug for Bounded<K, V> {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "a Debug impl")
    )]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Bounded")
            .field("name", &self.name)
            .field("limit", &self.limit)
            .field("ttl", &self.ttl)
            .finish_non_exhaustive()
    }
}

impl<K, V> Bounded<K, V>
where
    K: Hash + Eq + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    /// An empty cache.
    ///
    /// # Arguments
    /// * `name` - which cache (metric labels, logs).
    /// * `limit` - the capacity; with [`Limit::Bytes`] also call
    ///   [`Bounded::with_weigher`].
    /// * `ttl` - how long an entry lives after it is stored.
    ///
    /// # Returns
    /// The cache.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn new(name: CacheName, limit: Limit, ttl: Duration) -> Self {
        let mut cache = Self {
            name,
            limit,
            ttl,
            idle: None,
            weigher: None,
            metrics: None,
            cache: Cache::builder().build(),
            inflight: Mutex::new(HashMap::new()),
            fence: AtomicU64::new(0),
            stale_before: AtomicU64::new(0),
            probe: None,
            oversize_warned: Mutex::new(None),
        };
        cache.cache = cache.build_cache();
        cache
    }

    /// Sets how much one value weighs, in the units of [`Limit::Bytes`].
    /// Moka weighs when an entry is stored or replaced, not when data
    /// behind an `Arc` later changes: see [`Bounded::reweigh`]. Call this
    /// before [`Bounded::with_metrics`] and before the cache is used.
    ///
    /// # Arguments
    /// * `weigher` - the weight of a key and value, saturating at
    ///   `u32::MAX`.
    ///
    /// # Returns
    /// The cache.
    #[tracing::instrument(level = "trace", skip_all)]
    #[must_use]
    pub fn with_weigher(mut self, weigher: impl Fn(&K, &V) -> u32 + Send + Sync + 'static) -> Self {
        debug_assert!(
            self.probe.is_none(),
            "with_weigher goes before with_metrics"
        );
        self.weigher = Some(Arc::new(weigher));
        self.cache = self.build_cache();
        self
    }

    /// Also drops an entry that has not been read or stored for `idle`
    /// (moka's time to idle), whatever its time to live. Call it before
    /// [`Bounded::with_metrics`] and before the cache is used.
    ///
    /// # Arguments
    /// * `idle` - how long an entry may go unused.
    ///
    /// # Returns
    /// The cache.
    #[tracing::instrument(level = "trace", skip_all)]
    #[must_use]
    pub fn with_idle(mut self, idle: Duration) -> Self {
        debug_assert!(self.probe.is_none(), "with_idle goes before with_metrics");
        self.idle = Some(idle);
        self.cache = self.build_cache();
        self
    }

    /// Records `okf.cache.*` on `metrics`, and registers the size gauges.
    /// Call it last, before the cache is used.
    ///
    /// # Arguments
    /// * `metrics` - the server's instruments.
    ///
    /// # Returns
    /// The cache.
    #[tracing::instrument(level = "trace", skip_all)]
    #[must_use]
    pub fn with_metrics(mut self, metrics: Metrics) -> Self {
        self.metrics = Some(metrics.clone());
        self.cache = self.build_cache();
        let probe = Arc::new(Probe {
            cache: self.cache.clone(),
            by_bytes: matches!(self.limit, Limit::Bytes(_)),
        });
        let size: Weak<dyn CacheSize> = Arc::downgrade(&probe) as Weak<dyn CacheSize>;
        metrics.observe_cache(self.name, size);
        self.probe = Some(probe);
        self
    }

    /// Builds the moka cache from the settings so far.
    #[tracing::instrument(level = "trace", skip_all)]
    fn build_cache(&self) -> Cache<K, Entry<V>> {
        let capacity = match self.limit {
            Limit::Entries(count) | Limit::Bytes(count) => count,
        };
        let mut builder = Cache::builder()
            .max_capacity(capacity)
            .time_to_live(self.ttl);
        if let Some(idle) = self.idle {
            builder = builder.time_to_idle(idle);
        }
        if let Some(weigher) = self.weigher.clone() {
            builder = builder.weigher(move |key: &K, entry: &Entry<V>| weigher(key, &entry.value));
        }
        if let Some(metrics) = self.metrics.clone() {
            let name = self.name;
            builder = builder.eviction_listener(
                move |_key: Arc<K>, _entry: Entry<V>, cause: RemovalCause| {
                    let cause = match cause {
                        RemovalCause::Size => Eviction::Size,
                        RemovalCause::Expired => Eviction::Ttl,
                        RemovalCause::Explicit => Eviction::Invalidated,
                        // A replaced entry is still cached under its key.
                        RemovalCause::Replaced => return,
                    };
                    metrics.cache_eviction(name, cause);
                },
            );
        }
        builder.build()
    }

    /// The cached value, loading it once however many callers miss
    /// together. A stale entry is reloaded.
    ///
    /// A load overtaken by an invalidation of its key (or by
    /// [`Bounded::invalidate_where`]) is returned to its callers but not
    /// kept; so is a value heavier than the whole budget.
    ///
    /// # Arguments
    /// * `key` - the key.
    /// * `load` - reads the value; runs for one caller of a given miss.
    ///
    /// # Returns
    /// The value.
    ///
    /// # Errors
    /// The loader's error for the caller that ran it. A caller that shared
    /// that load gets [`OkfError::Busy`] (the error itself cannot be
    /// cloned): it should retry. The one exception is a loader failure of
    /// [`OkfError::StoreUnavailable`], which the sharing callers get too
    /// (an outage is not a reason to retry at once). A failed load is never
    /// cached.
    #[tracing::instrument(level = "trace", skip_all)]
    pub async fn get_or_load<F, Fut>(&self, key: K, load: F) -> Result<V, OkfError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<V, OkfError>>,
    {
        if let Some(entry) = self.cache.get(&key).await {
            if self.is_fresh(&entry) {
                self.count(Lookup::Hit);
                return Ok(entry.value);
            }
            self.count(Lookup::Stale);
            // Drop the stale entry so the single-flight load below runs.
            self.cache.invalidate(&key).await;
        } else {
            self.count(Lookup::Miss);
        }
        self.load_once(key, load).await
    }

    /// As [`Bounded::get_or_load`] for a loader that may find nothing: a
    /// value found is cached as usual, and nothing found is never cached, so
    /// the next lookup of that key loads again (M18 spec Decision 9: an
    /// unknown credential costs one query per lookup and takes no room).
    /// Callers that miss together still share one load, found or not.
    ///
    /// # Arguments
    /// * `key` - the key.
    /// * `load` - reads the value, or `None` when there is none.
    ///
    /// # Returns
    /// The value, or `None`.
    ///
    /// # Errors
    /// As [`Bounded::get_or_load`].
    #[tracing::instrument(level = "trace", skip_all)]
    pub async fn get_or_load_present<F, Fut>(&self, key: K, load: F) -> Result<Option<V>, OkfError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Option<V>, OkfError>>,
    {
        if let Some(entry) = self.cache.get(&key).await {
            if self.is_fresh(&entry) {
                self.count(Lookup::Hit);
                return Ok(Some(entry.value));
            }
            self.count(Lookup::Stale);
            self.cache.invalidate(&key).await;
        } else {
            self.count(Lookup::Miss);
        }
        self.load_present_once(key, load).await
    }

    /// The cached values of `keys`, reading every key not cached (or marked
    /// stale) with one call to `load`: a batch of lookups costs one round
    /// trip, not one per miss. Each loaded value is kept unless an
    /// invalidation of its key (or of the whole cache) overtook the read,
    /// as for [`Bounded::get_or_load`]. Not single flight: callers that
    /// miss together each read.
    ///
    /// # Arguments
    /// * `keys` - the keys wanted (repeats are read once).
    /// * `load` - given the keys to read, returns the `(key, value)` pairs
    ///   that exist; a key it leaves out is absent, and never cached.
    ///
    /// # Returns
    /// The value of each key found, cached or loaded.
    ///
    /// # Errors
    /// The loader's error; nothing is cached then.
    #[tracing::instrument(level = "trace", skip_all)]
    pub async fn get_or_load_many<F, Fut>(
        &self,
        keys: Vec<K>,
        load: F,
    ) -> Result<HashMap<K, V>, OkfError>
    where
        F: FnOnce(Vec<K>) -> Fut,
        Fut: Future<Output = Result<Vec<(K, V)>, OkfError>>,
    {
        let mut found = HashMap::new();
        // The flights are registered before the read, so an invalidation
        // that lands while the loader runs is seen when the value is kept.
        let mut flights: HashMap<K, Flight<'_, K>> = HashMap::new();
        for key in keys {
            if found.contains_key(&key) || flights.contains_key(&key) {
                continue;
            }
            match self.cache.get(&key).await {
                Some(entry) if self.is_fresh(&entry) => {
                    self.count(Lookup::Hit);
                    found.insert(key, entry.value);
                }
                seen => {
                    self.count(if seen.is_some() {
                        Lookup::Stale
                    } else {
                        Lookup::Miss
                    });
                    let flight = self.begin(&key);
                    flights.insert(key, flight);
                }
            }
        }
        if flights.is_empty() {
            return Ok(found);
        }
        let stamp = self.stale_before.load(Ordering::SeqCst);
        let started = Instant::now();
        let loaded = load(flights.keys().cloned().collect()).await;
        self.record_load(started, loaded.is_ok());
        for (key, value) in loaded? {
            // A pair for a key nobody asked for is dropped.
            if let Some(flight) = flights.get(&key) {
                self.keep(&key, value.clone(), stamp, flight).await;
                found.insert(key, value);
            }
        }
        Ok(found)
    }

    /// As [`Bounded::get_or_load`], except that an entry marked stale is
    /// first passed to `check`, which returns `Ok(None)` to keep it or
    /// `Ok(Some(new))` to replace it. Revalidations are not single flight.
    ///
    /// # Arguments
    /// * `key` - the key.
    /// * `load` - reads the value on a miss.
    /// * `check` - given the stale value, says whether it is still current.
    ///
    /// # Returns
    /// The value.
    ///
    /// # Errors
    /// The loader's or the check's error.
    #[tracing::instrument(level = "trace", skip_all)]
    pub async fn get_or_revalidate<F, Fut, C, CFut>(
        &self,
        key: K,
        load: F,
        check: C,
    ) -> Result<V, OkfError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<V, OkfError>>,
        C: FnOnce(V) -> CFut,
        CFut: Future<Output = Result<Option<V>, OkfError>>,
    {
        let Some(entry) = self.cache.get(&key).await else {
            self.count(Lookup::Miss);
            return self.load_once(key, load).await;
        };
        if self.is_fresh(&entry) {
            self.count(Lookup::Hit);
            return Ok(entry.value);
        }
        self.count(Lookup::Stale);
        self.revalidate(key, entry, load, check).await
    }

    /// Revalidates `seen`, a stale entry read earlier. The flight is
    /// registered first and the entry read again: an invalidation that ran
    /// between the first read and the registration removed the entry and
    /// bumped no epoch, so without the second read `check` could keep the
    /// invalidated value. If the entry is gone, load it.
    #[tracing::instrument(level = "trace", skip_all)]
    async fn revalidate<F, Fut, C, CFut>(
        &self,
        key: K,
        seen: Entry<V>,
        load: F,
        check: C,
    ) -> Result<V, OkfError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<V, OkfError>>,
        C: FnOnce(V) -> CFut,
        CFut: Future<Output = Result<Option<V>, OkfError>>,
    {
        let flight = self.begin(&key);
        let Some(entry) = self.cache.get(&key).await else {
            drop(flight);
            return self.load_once(key, load).await;
        };
        if self.is_fresh(&entry) {
            return Ok(entry.value);
        }
        debug_assert!(entry.stamp >= seen.stamp, "an entry only gets fresher");
        let stamp = self.stale_before.load(Ordering::SeqCst);
        let started = Instant::now();
        let checked = check(entry.value.clone()).await;
        self.record_load(started, checked.is_ok());
        let value = checked?.unwrap_or(entry.value);
        self.keep(&key, value.clone(), stamp, &flight).await;
        Ok(value)
    }

    /// The cached value if it is current and `usable`, counting one request:
    /// a hit when it is served, a miss otherwise (nothing cached, marked
    /// stale, or not usable, which the caller will build again). For a
    /// caller that decides by more than the key, as an overlay checks its
    /// version; the entry is neither dropped nor loaded here.
    ///
    /// # Arguments
    /// * `key` - the key.
    /// * `usable` - whether a cached value answers this request.
    ///
    /// # Returns
    /// The value, or `None` for the caller to build.
    #[tracing::instrument(level = "trace", skip_all)]
    pub async fn get_if(&self, key: &K, usable: impl Fn(&V) -> bool) -> Option<V> {
        let found = self
            .cache
            .get(key)
            .await
            .filter(|entry| self.is_fresh(entry) && usable(&entry.value));
        self.count(if found.is_some() {
            Lookup::Hit
        } else {
            Lookup::Miss
        });
        found.map(|entry| entry.value)
    }

    /// Builds the value of `key` with `build` and keeps it, for a caller that
    /// does its own single flight (a per-key lock) and so cannot use
    /// [`Bounded::get_or_load`]. The key's fence is registered before the
    /// build starts, so an [`Bounded::invalidate`] of `key` while it runs
    /// makes the result be served but not kept. The build counts as a load
    /// (duration and outcome).
    ///
    /// # Arguments
    /// * `key` - the key.
    /// * `build` - makes the value.
    /// * `replaces` - given the entry already cached and the new value,
    ///   whether the new value may replace it (an older build must not
    ///   replace a newer one).
    ///
    /// # Returns
    /// The value built, kept or not.
    ///
    /// # Errors
    /// The builder's error; nothing is cached then.
    #[tracing::instrument(level = "trace", skip_all)]
    pub async fn build<F, Fut>(
        &self,
        key: K,
        build: F,
        replaces: impl Fn(&V, &V) -> bool,
    ) -> Result<V, OkfError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<V, OkfError>>,
    {
        let flight = self.begin(&key);
        let stamp = self.stale_before.load(Ordering::SeqCst);
        let started = Instant::now();
        let built = build().await;
        self.record_load(started, built.is_ok());
        let value = built?;
        // A peek then a keep: two builds finishing together can still leave
        // the older, which the caller's own validity check then rejects.
        let replace = self
            .cache
            .get(&key)
            .await
            .is_none_or(|entry| replaces(&entry.value, &value));
        if replace {
            self.keep(&key, value.clone(), stamp, &flight).await;
        }
        Ok(value)
    }

    /// The cached value without loading it or counting a request (tests,
    /// listings).
    ///
    /// # Arguments
    /// * `key` - the key.
    ///
    /// # Returns
    /// The value, stale or not, or `None`.
    #[tracing::instrument(level = "trace", skip_all)]
    pub async fn peek(&self, key: &K) -> Option<V> {
        self.cache.get(key).await.map(|entry| entry.value)
    }

    /// The cached value if it is current: a fresh entry counts as a hit; a
    /// stale or missing one counts nothing and loads nothing, so the caller
    /// can read its misses together (a bulk read that must not fill the
    /// cache). It is an ordinary moka `get`, so it does record an access:
    /// a fresh entry it finds counts as recently used. moka 0.12 has no
    /// lookup that returns a value without that (`contains_key` skips it but
    /// returns no value).
    ///
    /// # Arguments
    /// * `key` - the key.
    ///
    /// # Returns
    /// The value, or `None` if it is missing or marked stale.
    #[tracing::instrument(level = "trace", skip_all)]
    pub async fn peek_fresh(&self, key: &K) -> Option<V> {
        let entry = self.cache.get(key).await?;
        if !self.is_fresh(&entry) {
            return None;
        }
        self.count(Lookup::Hit);
        Some(entry.value)
    }

    /// Drops `key`, and makes a load of it that is running now serve its
    /// caller without keeping the result.
    ///
    /// # Arguments
    /// * `key` - the key.
    #[tracing::instrument(level = "trace", skip_all)]
    pub async fn invalidate(&self, key: &K) {
        if let Some(flight) = locked(&self.inflight).get_mut(key) {
            flight.epoch += 1;
        }
        self.cache.invalidate(key).await;
    }

    /// Drops every entry `predicate` selects, and fences every load running
    /// now: a predicate sees only resident entries, so it cannot name a key
    /// whose load has not finished. Those loads serve their callers without
    /// keeping the result.
    ///
    /// # Arguments
    /// * `predicate` - selects entries by key and value.
    #[tracing::instrument(level = "trace", skip_all)]
    pub async fn invalidate_where(&self, predicate: impl Fn(&K, &V) -> bool) {
        self.fence.fetch_add(1, Ordering::SeqCst);
        // The scan is bounded by the cache's size. Collect the keys first so
        // no moka iterator is held across the `.await`s below.
        let selected: Vec<K> = self
            .cache
            .iter()
            .filter(|(key, entry)| predicate(key, &entry.value))
            .map(|(key, _)| (*key).clone())
            .collect();
        // Through `invalidate`, so each removed key's epoch moves too: a
        // revalidation or reweigh that registered after the fence moved above
        // but read the entry before this removal is overtaken by it.
        for key in selected {
            self.invalidate(&key).await;
        }
    }

    /// Marks every current entry (and every load running now) stale: the
    /// next use of one goes through [`Bounded::get_or_revalidate`]'s check,
    /// or is reloaded by [`Bounded::get_or_load`].
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn mark_all_stale(&self) {
        self.stale_before.fetch_add(1, Ordering::SeqCst);
    }

    /// Stores `key`'s cached value again so the weigher runs again: moka
    /// weighs on insert and replace only, not when data behind an `Arc`
    /// changes. Like a load, it is dropped if an invalidation overtakes it.
    /// Does nothing when `key` is not cached.
    ///
    /// # Arguments
    /// * `key` - the key.
    #[tracing::instrument(level = "trace", skip_all)]
    pub async fn reweigh(&self, key: &K) {
        let flight = self.begin(key);
        if let Some(entry) = self.cache.get(key).await {
            self.keep(key, entry.value, entry.stamp, &flight).await;
        }
    }

    /// How many entries the cache holds, as moka counts them (it settles
    /// its bookkeeping lazily, so a just-inserted entry may count a moment
    /// later).
    ///
    /// # Returns
    /// The entry count.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "reads one counter")
    )]
    pub fn entry_count(&self) -> u64 {
        self.cache.entry_count()
    }

    /// Keys with a load in flight: the size of the epoch map.
    #[cfg(test)]
    pub fn tracked_epochs(&self) -> usize {
        locked(&self.inflight).len()
    }

    /// Runs moka's deferred bookkeeping so counts and listeners settle.
    #[cfg(test)]
    pub async fn settle(&self) {
        self.cache.run_pending_tasks().await;
    }

    /// Whether an entry was loaded since the last [`Bounded::mark_all_stale`].
    #[tracing::instrument(level = "trace", skip_all)]
    fn is_fresh(&self, entry: &Entry<V>) -> bool {
        entry.stamp >= self.stale_before.load(Ordering::SeqCst)
    }

    /// Registers a load of `key` and records the epoch and fence it starts
    /// under.
    #[tracing::instrument(level = "trace", skip_all)]
    fn begin(&self, key: &K) -> Flight<'_, K> {
        let mut map = locked(&self.inflight);
        let flight = map
            .entry(key.clone())
            .or_insert(InFlight { loads: 0, epoch: 0 });
        flight.loads += 1;
        Flight {
            inflight: &self.inflight,
            fence: &self.fence,
            key: key.clone(),
            epoch: flight.epoch,
            fence_seen: self.fence.load(Ordering::SeqCst),
        }
    }

    /// Runs `load` for one of the callers that miss on `key` together.
    #[tracing::instrument(level = "trace", skip_all)]
    async fn load_once<F, Fut>(&self, key: K, load: F) -> Result<V, OkfError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<V, OkfError>>,
    {
        // `map(Some)` wraps the value, so the loader never finds nothing.
        let found = self
            .load_present_once(key, || async { load().await.map(Some) })
            .await?;
        Ok(found.expect("a loader that always returns a value found one"))
    }

    /// Runs `load` for one of the callers that miss on `key` together; a
    /// load that finds nothing (`Ok(None)`) is shared but not cached.
    #[tracing::instrument(level = "trace", skip_all)]
    async fn load_present_once<F, Fut>(&self, key: K, load: F) -> Result<Option<V>, OkfError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Option<V>, OkfError>>,
    {
        // The leader parks its `Flight` here, so the key stays registered
        // until moka has inserted the value and the check below has run: an
        // invalidation landing in between is seen by that check.
        let parked: Mutex<Option<Flight<'_, K>>> = Mutex::new(None);
        let own_error: Mutex<Option<OkfError>> = Mutex::new(None);
        let loaded = self
            .cache
            .try_get_with(key.clone(), async {
                let flight = self.begin(&key);
                let stamp = self.stale_before.load(Ordering::SeqCst);
                let started = Instant::now();
                let result = load().await;
                self.record_load(started, result.is_ok());
                let value = match result {
                    Ok(Some(value)) => value,
                    Ok(None) => return Err(Load::Absent),
                    Err(error) => {
                        let outage = match &error {
                            OkfError::StoreUnavailable { store } => Some(store.clone()),
                            _ => None,
                        };
                        *locked(&own_error) = Some(error);
                        return Err(Load::Failed { outage });
                    }
                };
                if !flight.current() || self.too_heavy(&key, &value) {
                    return Err(Load::Uncached(value));
                }
                *locked(&parked) = Some(flight);
                Ok(Entry { value, stamp })
            })
            .await;
        match loaded {
            Ok(entry) => {
                let flight = locked(&parked).take();
                if flight.is_some_and(|flight| !flight.current()) {
                    self.cache.invalidate(&key).await;
                }
                Ok(Some(entry.value))
            }
            Err(shared) => match &*shared {
                Load::Uncached(value) => Ok(Some(value.clone())),
                Load::Absent => Ok(None),
                Load::Failed { outage } => {
                    Err(locked(&own_error).take().unwrap_or_else(|| match outage {
                        Some(store) => OkfError::StoreUnavailable {
                            store: store.clone(),
                        },
                        None => OkfError::Busy {
                            reason: "a load this request shared failed".to_string(),
                        },
                    }))
                }
            },
        }
    }

    /// Stores `value` under `key` unless something overtook the load that
    /// produced it, or it is heavier than the whole budget (then `key`'s old
    /// entry is dropped too).
    #[tracing::instrument(level = "trace", skip_all)]
    async fn keep(&self, key: &K, value: V, stamp: u64, flight: &Flight<'_, K>) {
        if !flight.current() {
            return;
        }
        if self.too_heavy(key, &value) {
            self.cache.invalidate(key).await;
            return;
        }
        self.cache.insert(key.clone(), Entry { value, stamp }).await;
        if !flight.current() {
            self.cache.invalidate(key).await;
        }
    }

    /// Whether `value` alone outweighs a byte-limited cache. Logs that, at
    /// most once an hour for the cache (never per key: that would be
    /// unbounded state).
    #[tracing::instrument(level = "trace", skip_all)]
    fn too_heavy(&self, key: &K, value: &V) -> bool {
        let (Limit::Bytes(budget), Some(weigher)) = (self.limit, &self.weigher) else {
            return false;
        };
        let weight = weigher(key, value);
        if u64::from(weight) <= budget {
            return false;
        }
        let mut warned = locked(&self.oversize_warned);
        if warned.is_none_or(|at| at.elapsed() >= OVERSIZE_WARN_EVERY) {
            *warned = Some(Instant::now());
            tracing::warn!(
                cache = self.name.as_str(),
                weight,
                budget,
                "a value heavier than the whole cache budget is served but not kept"
            );
        }
        true
    }

    /// Counts one lookup, when metrics are on.
    #[tracing::instrument(level = "trace", skip_all)]
    fn count(&self, result: Lookup) {
        if let Some(metrics) = &self.metrics {
            metrics.cache_request(self.name, result);
        }
    }

    /// Records one finished load, when metrics are on.
    #[tracing::instrument(level = "trace", skip_all)]
    fn record_load(&self, started: Instant, ok: bool) {
        if let Some(metrics) = &self.metrics {
            let outcome = if ok { Outcome::Ok } else { Outcome::Error };
            metrics.cache_load(self.name, outcome, started.elapsed());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, AtomicUsize};

    use tokio::sync::oneshot;
    use tokio::time::timeout;

    use super::*;
    use crate::mcp::testing::{local_metrics, render};

    const TTL: Duration = Duration::from_secs(3600);
    const MS_50: Duration = Duration::from_millis(50);
    /// Every wait in these tests is bounded by this.
    const BOUND: Duration = Duration::from_secs(30);

    #[tokio::test]
    async fn concurrent_misses_load_once() {
        let cache = Bounded::<i64, Arc<str>>::new(CacheName::Directory, Limit::Entries(10), TTL);
        let loads = AtomicUsize::new(0);
        let load = || async {
            loads.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(MS_50).await;
            Ok::<_, OkfError>(Arc::from("v"))
        };
        let (a, b) = timeout(BOUND, async {
            tokio::join!(cache.get_or_load(1, load), cache.get_or_load(1, load))
        })
        .await
        .expect("the two loads finish");
        assert_eq!((a.unwrap(), b.unwrap()), (Arc::from("v"), Arc::from("v")));
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn nothing_found_is_shared_but_never_cached() {
        let cache = Bounded::<i64, i64>::new(CacheName::Credential, Limit::Entries(10), TTL);
        let loads = AtomicUsize::new(0);
        let absent = || async {
            loads.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(MS_50).await;
            Ok::<_, OkfError>(None)
        };

        let (a, b) = timeout(BOUND, async {
            tokio::join!(
                cache.get_or_load_present(1, absent),
                cache.get_or_load_present(1, absent)
            )
        })
        .await
        .expect("the two loads finish");
        let again = cache.get_or_load_present(1, absent).await.unwrap();
        let found = cache
            .get_or_load_present(2, || async { Ok::<_, OkfError>(Some(7)) })
            .await
            .unwrap();

        assert_eq!((a.unwrap(), b.unwrap(), again), (None, None, None));
        assert_eq!(
            loads.load(Ordering::SeqCst),
            2,
            "one shared load, then one more"
        );
        assert_eq!(cache.peek(&1).await, None, "nothing found is not cached");
        assert_eq!((found, cache.peek(&2).await), (Some(7), Some(7)));
    }

    #[tokio::test]
    async fn a_load_overtaken_by_an_invalidation_is_served_but_not_kept() {
        let cache = Bounded::<i64, i64>::new(CacheName::Directory, Limit::Entries(10), TTL);
        let (release, wait) = oneshot::channel::<()>();
        // `join!` polls its futures in order: the load registers and parks on
        // the channel before the invalidation runs.
        let pending = cache.get_or_load(1, || async {
            wait.await.ok();
            Ok::<_, OkfError>(16)
        });
        let invalidate = async {
            cache.invalidate(&1).await;
            release.send(()).ok();
        };
        let (served, ()) = timeout(BOUND, async { tokio::join!(pending, invalidate) })
            .await
            .expect("the load finishes");
        assert_eq!(served.unwrap(), 16, "the caller still gets what it loaded");
        assert_eq!(cache.peek(&1).await, None, "but it is not cached");
    }

    #[tokio::test]
    async fn invalidating_keys_that_are_not_cached_keeps_no_state() {
        let cache: Bounded<i64, u32> = Bounded::new(CacheName::User, Limit::Entries(10), TTL);
        for key in 0..100_000 {
            cache.invalidate(&key).await;
        }
        assert_eq!(
            cache.tracked_epochs(),
            0,
            "no load in flight, so no epoch kept"
        );
    }

    #[tokio::test]
    async fn an_epoch_outlives_the_entry_until_the_older_load_finishes() {
        let cache = Bounded::<i64, i64>::new(CacheName::Directory, Limit::Entries(10), TTL);
        let (release, wait) = oneshot::channel::<()>();
        let pending = cache.get_or_load(1, || async {
            wait.await.ok();
            Ok::<_, OkfError>(7)
        });
        let invalidate_twice = async {
            cache.invalidate(&1).await;
            cache.invalidate(&1).await;
            assert_eq!(
                cache.tracked_epochs(),
                1,
                "the load in flight keeps its epoch"
            );
            release.send(()).ok();
        };
        let (served, ()) = timeout(BOUND, async { tokio::join!(pending, invalidate_twice) })
            .await
            .expect("the load finishes");
        assert_eq!(served.unwrap(), 7);
        assert_eq!(cache.peek(&1).await, None, "an older load is never kept");
        assert_eq!(
            cache.tracked_epochs(),
            0,
            "the epoch goes with the last load"
        );
    }

    #[tokio::test]
    async fn an_indirect_invalidation_fences_a_load_in_flight() {
        let cache = Bounded::<i64, i64>::new(CacheName::Directory, Limit::Entries(10), TTL);
        let (release, wait) = oneshot::channel::<()>();
        let pending = cache.get_or_load(1, || async {
            wait.await.ok();
            Ok::<_, OkfError>(5)
        });
        let fence = async {
            // The cache is empty: the predicate matches nothing resident.
            cache.invalidate_where(|_, _| true).await;
            release.send(()).ok();
        };
        let (served, ()) = timeout(BOUND, async { tokio::join!(pending, fence) })
            .await
            .expect("the load finishes");
        assert_eq!(served.unwrap(), 5);
        assert_eq!(cache.peek(&1).await, None, "the fenced load is not kept");
    }

    #[tokio::test]
    async fn a_revalidation_registered_after_the_fence_cannot_keep_a_removed_entry() {
        use std::cell::{Cell, RefCell};
        use std::task::{Context, Poll, Waker};

        let cache = Bounded::<i64, i64>::new(CacheName::Credential, Limit::Entries(10), TTL);
        cache.get_or_load(1, || async { Ok(1) }).await.unwrap();
        cache.mark_all_stale();
        let (release, wait) = oneshot::channel::<()>();
        let revalidation = RefCell::new(Box::pin(cache.get_or_revalidate(
            1,
            || async { Ok(9) },
            |_| async {
                wait.await.ok();
                Ok(None)
            },
        )));
        let started = Cell::new(false);
        // The predicate runs after the fence moved and before the removal:
        // start the revalidation there, so it registers under the new fence,
        // reads the old entry and parks in `check`.
        cache
            .invalidate_where(|_, _| {
                if !started.replace(true) {
                    let mut context = Context::from_waker(Waker::noop());
                    let polled = revalidation.borrow_mut().as_mut().poll(&mut context);
                    assert!(matches!(polled, Poll::Pending), "parked in check");
                    assert_eq!(cache.tracked_epochs(), 1, "registered before the removal");
                }
                true
            })
            .await;
        release.send(()).ok();
        let served = timeout(BOUND, revalidation.into_inner())
            .await
            .expect("the revalidation finishes")
            .unwrap();
        assert_eq!(served, 1, "the caller still gets what the check kept");
        assert_eq!(
            cache.peek(&1).await,
            None,
            "but the removed entry stays removed"
        );
    }

    #[tokio::test]
    async fn get_or_load_many_reads_the_misses_together_and_caches_them() {
        let cache = Bounded::<i64, i64>::new(CacheName::Labels, Limit::Entries(10), TTL);
        let reads = AtomicUsize::new(0);
        let load = |keys: Vec<i64>| async {
            reads.fetch_add(1, Ordering::SeqCst);
            // Key 9 does not exist.
            Ok(keys
                .into_iter()
                .filter(|key| *key != 9)
                .map(|key| (key, key * 10))
                .collect())
        };

        let first = cache
            .get_or_load_many(vec![1, 2, 9, 1], load)
            .await
            .unwrap();
        let second = cache.get_or_load_many(vec![1, 2], load).await.unwrap();

        assert_eq!(first, HashMap::from([(1, 10), (2, 20)]));
        assert_eq!(second, first);
        assert_eq!(reads.load(Ordering::SeqCst), 1, "the second call is cached");
        assert_eq!(cache.peek(&9).await, None, "an absent key is not cached");
    }

    #[tokio::test]
    async fn get_or_load_many_does_not_keep_a_value_overtaken_by_an_invalidation() {
        let cache = Arc::new(Bounded::<i64, i64>::new(
            CacheName::Labels,
            Limit::Entries(10),
            TTL,
        ));
        let (entered, started) = oneshot::channel();
        let (release, released) = oneshot::channel::<()>();
        let reader = {
            let cache = Arc::clone(&cache);
            tokio::spawn(async move {
                cache
                    .get_or_load_many(vec![1], |keys| async move {
                        entered.send(()).unwrap();
                        released.await.unwrap();
                        Ok(keys.into_iter().map(|key| (key, 5)).collect())
                    })
                    .await
            })
        };
        timeout(BOUND, started).await.unwrap().unwrap();

        cache.invalidate(&1).await;
        release.send(()).unwrap();
        let served = timeout(BOUND, reader).await.unwrap().unwrap().unwrap();

        assert_eq!(served, HashMap::from([(1, 5)]), "the caller is served");
        assert_eq!(cache.peek(&1).await, None, "but the old read is not kept");
    }

    #[tokio::test]
    async fn get_or_load_many_reads_a_stale_entry_again() {
        let cache = Bounded::<i64, i64>::new(CacheName::Labels, Limit::Entries(10), TTL);
        cache.get_or_load(1, || async { Ok(1) }).await.unwrap();
        cache.mark_all_stale();

        let got = cache
            .get_or_load_many(vec![1], |keys| async move {
                Ok(keys.into_iter().map(|key| (key, 2)).collect())
            })
            .await
            .unwrap();

        assert_eq!(got, HashMap::from([(1, 2)]));
        assert_eq!(cache.peek(&1).await, Some(2));
    }

    #[tokio::test]
    async fn invalidate_where_drops_the_entries_it_selects() {
        let cache = Bounded::<i64, i64>::new(CacheName::User, Limit::Entries(10), TTL);
        for key in 1..=4 {
            cache
                .get_or_load(key, || async move { Ok(key * 10) })
                .await
                .unwrap();
        }
        cache.invalidate_where(|_, value| value % 20 == 0).await;
        assert_eq!(cache.peek(&1).await, Some(10));
        assert_eq!(cache.peek(&2).await, None);
        assert_eq!(cache.peek(&3).await, Some(30));
        assert_eq!(cache.peek(&4).await, None);
    }

    #[tokio::test]
    async fn a_stale_entry_is_revalidated_on_next_use() {
        let cache = Bounded::<i64, i64>::new(CacheName::Credential, Limit::Entries(10), TTL);
        cache.get_or_load(1, || async { Ok(1) }).await.unwrap();
        let checks = AtomicUsize::new(0);
        let keep = |_: i64| async {
            checks.fetch_add(1, Ordering::SeqCst);
            Ok(None)
        };
        let unused = || async { Ok::<i64, OkfError>(99) };
        assert_eq!(cache.get_or_revalidate(1, unused, keep).await.unwrap(), 1);
        assert_eq!(
            checks.load(Ordering::SeqCst),
            0,
            "a current entry is not checked"
        );

        cache.mark_all_stale();
        assert_eq!(cache.get_or_revalidate(1, unused, keep).await.unwrap(), 1);
        assert_eq!(
            checks.load(Ordering::SeqCst),
            1,
            "a stale entry is checked once"
        );
        assert_eq!(cache.get_or_revalidate(1, unused, keep).await.unwrap(), 1);
        assert_eq!(
            checks.load(Ordering::SeqCst),
            1,
            "and is current again afterwards"
        );

        cache.mark_all_stale();
        let replace = |old: i64| async move { Ok(Some(old + 1)) };
        assert_eq!(
            cache.get_or_revalidate(1, unused, replace).await.unwrap(),
            2
        );
        assert_eq!(cache.peek(&1).await, Some(2), "a replacement is kept");
    }

    #[tokio::test]
    async fn an_entry_invalidated_before_revalidation_registers_is_not_kept() {
        let cache = Bounded::<i64, i64>::new(CacheName::Credential, Limit::Entries(10), TTL);
        cache.get_or_load(1, || async { Ok(1) }).await.unwrap();
        cache.mark_all_stale();
        // The stale entry is read, then an invalidation lands before the
        // revalidation registers its flight (no epoch to bump yet).
        let seen = cache.cache.get(&1).await.expect("cached");
        cache.invalidate(&1).await;
        let checks = AtomicUsize::new(0);
        let keep = |_: i64| async {
            checks.fetch_add(1, Ordering::SeqCst);
            Ok(None)
        };
        let served = cache
            .revalidate(1, seen, || async { Ok(2) }, keep)
            .await
            .unwrap();
        assert_eq!(served, 2, "the invalidated value is replaced by a load");
        assert_eq!(checks.load(Ordering::SeqCst), 0, "the check never saw it");
        assert_eq!(cache.peek(&1).await, Some(2));
    }

    #[tokio::test]
    async fn get_or_load_reloads_a_stale_entry() {
        let cache = Bounded::<i64, i64>::new(CacheName::Credential, Limit::Entries(10), TTL);
        cache.get_or_load(1, || async { Ok(1) }).await.unwrap();
        cache.mark_all_stale();
        assert_eq!(cache.get_or_load(1, || async { Ok(2) }).await.unwrap(), 2);
        assert_eq!(cache.get_or_load(1, || async { Ok(3) }).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn an_entry_heavier_than_the_whole_budget_is_served_and_not_kept() {
        let cache = Bounded::<i64, i64>::new(CacheName::Catalog, Limit::Bytes(100), TTL)
            .with_weigher(|_, _| 1000);
        let loads = AtomicUsize::new(0);
        let load = || async {
            loads.fetch_add(1, Ordering::SeqCst);
            Ok::<_, OkfError>(1)
        };
        assert_eq!(cache.get_or_load(1, load).await.unwrap(), 1);
        cache.settle().await;
        assert_eq!(cache.peek(&1).await, None, "too heavy to keep");
        assert_eq!(cache.get_or_load(1, load).await.unwrap(), 1);
        assert_eq!(
            loads.load(Ordering::SeqCst),
            2,
            "so the next request loads again"
        );
    }

    /// Loads `0..keys` into `cache`, lets moka evict, then asks for every
    /// key again and returns how many of the second requests had to load.
    async fn reloads_after_filling(cache: &Bounded<i64, i64>, keys: i64) -> usize {
        for key in 0..keys {
            cache
                .get_or_load(key, || async move { Ok(key) })
                .await
                .unwrap();
            // Moka admits and evicts asynchronously: settle as we go so the
            // cache is full of its limit, not of what is still queued.
            cache.settle().await;
        }
        let reloads = AtomicUsize::new(0);
        for key in 0..keys {
            cache
                .get_or_load(key, || async {
                    reloads.fetch_add(1, Ordering::SeqCst);
                    Ok(key)
                })
                .await
                .unwrap();
        }
        reloads.load(Ordering::SeqCst)
    }

    #[tokio::test]
    async fn an_entry_count_limit_evicts_to_the_limit() {
        // Without a `max_capacity` nothing is evicted and this fails.
        let cache = Bounded::<i64, i64>::new(CacheName::Directory, Limit::Entries(5), TTL);
        let reloads = reloads_after_filling(&cache, 50).await;
        cache.settle().await;
        assert!(
            cache.cache.entry_count() <= 5,
            "{}",
            cache.cache.entry_count()
        );
        assert!(reloads > 0, "at least one evicted key reloads");
    }

    #[tokio::test]
    async fn a_byte_limit_evicts_to_the_budget_and_counts_size_evictions() {
        let (_provider, metrics, registry) = local_metrics();
        let cache = Bounded::<i64, i64>::new(CacheName::Catalog, Limit::Bytes(100), TTL)
            .with_weigher(|_, _| 10)
            .with_metrics(metrics);
        let reloads = reloads_after_filling(&cache, 50).await;
        cache.settle().await;
        assert!(
            cache.cache.weighted_size() <= 100,
            "{}",
            cache.cache.weighted_size()
        );
        assert!(reloads > 0, "at least one evicted key reloads");
        let text = render(&registry);
        assert!(
            text.contains(r#"okf_cache_evictions_total{cache="catalog",cause="size"}"#),
            "no size eviction counted in\n{text}"
        );
    }

    #[tokio::test]
    async fn peek_fresh_serves_a_current_entry_and_nothing_else() {
        let cache = Bounded::<i64, i64>::new(CacheName::Directory, Limit::Entries(10), TTL);
        assert_eq!(cache.peek_fresh(&1).await, None, "nothing cached yet");
        cache.get_or_load(1, || async { Ok(7) }).await.unwrap();
        assert_eq!(cache.peek_fresh(&1).await, Some(7));
        cache.mark_all_stale();
        assert_eq!(
            cache.peek_fresh(&1).await,
            None,
            "a stale entry is not served"
        );
        assert_eq!(cache.peek(&1).await, Some(7), "but it is still resident");
    }

    #[tokio::test]
    async fn entries_expire_after_the_ttl() {
        // moka reads the real clock, so this test cannot pause tokio's.
        // Reuse is proved with a long lifetime and expiry with a short one,
        // so no step has to land inside the short lifetime (issue #921).
        let loads = AtomicUsize::new(0);
        let load = || async {
            loads.fetch_add(1, Ordering::SeqCst);
            Ok::<_, OkfError>(1)
        };
        let reused = Bounded::<i64, i64>::new(CacheName::Directory, Limit::Entries(10), TTL);
        reused.get_or_load(1, load).await.unwrap();
        reused.get_or_load(1, load).await.unwrap();
        assert_eq!(loads.load(Ordering::SeqCst), 1);

        let expiring = Bounded::<i64, i64>::new(
            CacheName::Directory,
            Limit::Entries(10),
            Duration::from_millis(100),
        );
        expiring.get_or_load(1, load).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        expiring.get_or_load(1, load).await.unwrap();
        assert_eq!(loads.load(Ordering::SeqCst), 3, "an expired entry reloads");
    }

    #[tokio::test]
    async fn an_idle_entry_expires_but_a_used_one_stays() {
        // moka reads the real clock, so this test cannot pause tokio's.
        // The reads span 1.2 s against a 0.8 s idle time, so the read entry
        // outlives a whole idle lifetime only if reads refresh it. Each read
        // still has to land within 0.8 s of the last, so a scheduler stall of
        // that length can fail the test; the margin is wider than the
        // original 400 ms against 150 ms sleeps, not unlimited (issue #921).
        let cache = Bounded::<i64, i64>::new(CacheName::Overlay, Limit::Entries(10), TTL)
            .with_idle(Duration::from_millis(800));
        cache
            .build(1, || async { Ok(10) }, |_, _| true)
            .await
            .unwrap();
        cache
            .build(2, || async { Ok(20) }, |_, _| true)
            .await
            .unwrap();

        for _ in 0..8 {
            tokio::time::sleep(Duration::from_millis(150)).await;
            assert_eq!(cache.peek(&1).await, Some(10), "a read keeps it alive");
        }

        assert_eq!(cache.peek(&2).await, None, "the unread entry went idle");
        assert_eq!(cache.peek(&1).await, Some(10), "the read entry stayed");
    }

    #[tokio::test]
    async fn a_build_is_kept_and_replaces_only_when_allowed() {
        let cache = Bounded::<i64, i64>::new(CacheName::Overlay, Limit::Entries(10), TTL);

        cache
            .build(1, || async { Ok(10) }, |_, _| true)
            .await
            .unwrap();
        cache
            .build(1, || async { Ok(5) }, |old, new| new >= old)
            .await
            .unwrap();
        let kept = cache.peek(&1).await;
        cache
            .build(1, || async { Ok(11) }, |old, new| new >= old)
            .await
            .unwrap();

        assert_eq!(kept, Some(10), "a smaller build does not replace");
        assert_eq!(cache.peek(&1).await, Some(11));
    }

    #[tokio::test]
    async fn an_invalidation_during_a_build_makes_it_served_but_not_kept() {
        let cache = Arc::new(Bounded::<i64, i64>::new(
            CacheName::Overlay,
            Limit::Entries(10),
            TTL,
        ));
        let (started_tx, started_rx) = oneshot::channel::<()>();
        let (release_tx, release_rx) = oneshot::channel::<()>();
        let building = {
            let cache = Arc::clone(&cache);
            tokio::spawn(async move {
                cache
                    .build(
                        1,
                        || async {
                            started_tx.send(()).ok();
                            release_rx.await.ok();
                            Ok(7)
                        },
                        |_, _| true,
                    )
                    .await
            })
        };
        timeout(BOUND, started_rx)
            .await
            .expect("the build starts")
            .unwrap();

        cache.invalidate(&1).await;
        release_tx.send(()).unwrap();
        let served = timeout(BOUND, building).await.expect("the build ends");

        assert_eq!(served.unwrap().unwrap(), 7, "the caller still gets it");
        assert_eq!(cache.peek(&1).await, None, "but it is not kept");
    }

    #[tokio::test]
    async fn get_if_counts_one_request_and_a_build_counts_a_load() {
        let (_provider, metrics, registry) = local_metrics();
        let cache = Bounded::<i64, i64>::new(CacheName::Overlay, Limit::Entries(10), TTL)
            .with_metrics(metrics);

        let missed = cache.get_if(&1, |_| true).await;
        cache
            .build(1, || async { Ok(3) }, |_, _| true)
            .await
            .unwrap();
        let unusable = cache.get_if(&1, |value| *value > 3).await;
        let hit = cache.get_if(&1, |_| true).await;
        let failed = cache
            .build(
                2,
                || async { Err::<i64, _>(OkfError::Busy { reason: "x".into() }) },
                |_, _| true,
            )
            .await;

        assert_eq!((missed, unusable, hit), (None, None, Some(3)));
        assert!(failed.is_err());
        let text = render(&registry);
        let series = |name: &str| crate::mcp::testing::sample_of(&text, name);
        assert_eq!(
            series(r#"okf_cache_requests_total{cache="overlay",result="miss"}"#),
            Some(2.0)
        );
        assert_eq!(
            series(r#"okf_cache_requests_total{cache="overlay",result="hit"}"#),
            Some(1.0)
        );
        assert_eq!(
            series(r#"okf_cache_load_duration_seconds_count{cache="overlay",outcome="ok"}"#),
            Some(1.0)
        );
        assert_eq!(
            series(r#"okf_cache_load_duration_seconds_count{cache="overlay",outcome="error"}"#),
            Some(1.0)
        );
    }

    #[tokio::test]
    async fn a_failed_load_is_not_cached_and_its_waiters_are_told_to_retry() {
        let cache = Bounded::<i64, i64>::new(CacheName::Directory, Limit::Entries(10), TTL);
        let failing = || async {
            tokio::time::sleep(MS_50).await;
            Err::<i64, _>(OkfError::Config {
                path: "x".to_string(),
                message: "boom".to_string(),
            })
        };
        let (a, b) = timeout(BOUND, async {
            tokio::join!(cache.get_or_load(1, failing), cache.get_or_load(1, failing))
        })
        .await
        .expect("both finish");
        assert!(
            matches!(a, Err(OkfError::Config { .. })),
            "the leader: {a:?}"
        );
        assert!(matches!(b, Err(OkfError::Busy { .. })), "the waiter: {b:?}");
        assert_eq!(cache.get_or_load(1, || async { Ok(4) }).await.unwrap(), 4);
        assert_eq!(cache.tracked_epochs(), 0);
    }

    #[tokio::test]
    async fn reweigh_runs_the_weigher_again() {
        let weight = Arc::new(AtomicU64::new(10));
        let seen = weight.clone();
        let cache = Bounded::<i64, i64>::new(CacheName::Catalog, Limit::Bytes(1000), TTL)
            .with_weigher(move |_, _| u32::try_from(seen.load(Ordering::SeqCst)).unwrap());
        cache.get_or_load(1, || async { Ok(1) }).await.unwrap();
        cache.settle().await;
        assert_eq!(cache.cache.weighted_size(), 10);
        weight.store(400, Ordering::SeqCst);
        cache.reweigh(&1).await;
        cache.settle().await;
        assert_eq!(cache.cache.weighted_size(), 400);
        cache.reweigh(&2).await;
        assert_eq!(
            cache.tracked_epochs(),
            0,
            "a reweigh of an absent key leaves no state"
        );
    }

    #[tokio::test]
    async fn a_reweigh_overtaken_by_an_invalidation_is_dropped() {
        let cache = Bounded::<i64, i64>::new(CacheName::Catalog, Limit::Entries(10), TTL);
        cache.get_or_load(1, || async { Ok(1) }).await.unwrap();
        let flight = cache.begin(&1);
        cache.invalidate(&1).await;
        cache.keep(&1, 1, 0, &flight).await;
        assert_eq!(
            cache.peek(&1).await,
            None,
            "a stale re-insert must not resurrect it"
        );
    }

    #[tokio::test]
    async fn metrics_count_requests_evictions_loads_and_size() {
        let (_provider, metrics, registry) = local_metrics();
        let cache = Bounded::<i64, i64>::new(CacheName::User, Limit::Bytes(1000), TTL)
            .with_weigher(|_, _| 8)
            .with_metrics(metrics);
        cache.get_or_load(1, || async { Ok(1) }).await.unwrap();
        cache.get_or_load(1, || async { Ok(1) }).await.unwrap();
        cache.mark_all_stale();
        cache.get_or_load(1, || async { Ok(1) }).await.unwrap();
        cache.invalidate(&1).await;
        cache.settle().await;
        let text = render(&registry);
        for expected in [
            r#"okf_cache_requests_total{cache="user",result="miss"} 1"#,
            r#"okf_cache_requests_total{cache="user",result="hit"} 1"#,
            r#"okf_cache_requests_total{cache="user",result="stale"} 1"#,
            r#"okf_cache_evictions_total{cache="user",cause="invalidated"}"#,
            r#"okf_cache_entries{cache="user"} 0"#,
            r#"okf_cache_bytes{cache="user"} 0"#,
            r#"okf_cache_load_duration_seconds_count{cache="user",outcome="ok"} 2"#,
        ] {
            assert!(text.contains(expected), "missing {expected} in\n{text}");
        }
    }
}

#[cfg(test)]
#[path = "bounded_property_tests.rs"]
mod property_tests;
