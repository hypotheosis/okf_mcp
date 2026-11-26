//! The catalog cache (M18 spec Decisions 2 and 11): catalog snapshots by
//! `catalogs.id`, loaded on first use and bounded by an estimate of the
//! bytes they hold, instead of every catalog loaded at start-up.
//!
//! * **Freshness has one point of control**, the directory row
//!   ([`CatalogRow`]): [`CatalogCache::get`] is given the row a request
//!   resolved and refreshes the cached state in place when the row's
//!   `generation` is ahead of its snapshot. So no entry needs a stale mark
//!   or a short lifetime of its own: marking the directory stale (a lost
//!   change feed, spec Decision 12) revalidates every snapshot on its next
//!   use, against the row read again.
//! * **A `Catalog` message** ([`CatalogCache::catch_up`]) refreshes a
//!   resident entry in place; for an entry that is not resident it fences a
//!   load in flight, so a load that read an older generation is served to
//!   its own request but not kept (the minimum generation of Decision 11).
//! * **Every in-place refresh re-weighs** the entry ([`CatalogCache::refreshed`]):
//!   moka weighs on insert only, so a catalog that grew past the budget is
//!   dropped instead of keeping its old weight.
//! * **A generation whose stored data will not build** is remembered in a
//!   small second cache keyed by `(id, generation)`, so it is not rebuilt on
//!   every request; a request for it gets today's answer for a catalog that
//!   is not loaded ([`OkfError::CatalogNotSelected`]), or the older snapshot
//!   still cached.
//!
//! # For Python readers
//!
//! `Arc<CatalogState>` is a shared handle: the cache, and every request
//! that uses a catalog, hold the same state, and an eviction only drops the
//! cache's handle, never a request's.

use std::sync::Arc;
use std::time::Duration;

use crate::cache::{Bounded, CacheName, Limit};
use crate::catalog::{CatalogState, Loaded};
use crate::error::OkfError;
use crate::keyed_lock::KeyedLocks;
use crate::store::catalogs::CatalogRow;
use crate::stores::Stores;
use crate::telemetry::metrics::Metrics;
use crate::telemetry::subscriber::split_event;

/// Resident bytes per stored byte of content: what one byte of a catalog's
/// stored files costs once loaded (the snapshot's parsed documents, the
/// workspace copy and their text). Fitted, with [`PER_FILE_BYTES`], to the
/// measurements of `tests/snapshot_weight.rs` (method and numbers in spec
/// Decision 2) and rounded up.
pub const WEIGHT_FACTOR: u64 = 8;

/// The fixed cost of one file, whatever its size: its path (in the
/// workspace, the snapshot and the revision map), its revision, its commit
/// time and the map entries that hold them. Measured as about 985 bytes per
/// empty document at paths like `notes/topic-1/concept-00001.md`; rounded
/// up, so a catalog of many tiny or empty files is charged for them.
pub const PER_FILE_BYTES: u64 = 1024;

/// The fixed cost of one cached catalog, files aside: its state, its name,
/// the empty snapshot's maps and the cache entry itself. Measured as under
/// 100 bytes of state; rounded up generously to include the cache's own
/// bookkeeping, so an empty catalog is never free.
pub const PER_CATALOG_BYTES: u64 = 1024;

/// How long a snapshot may stay cached at most. Not a freshness bound (the
/// directory row is, see the module docs) nor a size bound (the budget is):
/// only a backstop that lets go of an entry nobody invalidated, such as a
/// catalog deleted while the change feed was down.
const SNAPSHOT_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// The most `(id, generation)` pairs remembered as not building.
const BROKEN_ENTRIES: u64 = 10_000;

/// How long a generation is remembered as not building. The same data
/// fails the same way, so this is long; a new generation is a new key.
const BROKEN_TTL: Duration = Duration::from_secs(60 * 60);

/// The weight of a snapshot, as moka takes it: [`PER_CATALOG_BYTES`], plus
/// [`PER_FILE_BYTES`] per file, plus [`WEIGHT_FACTOR`] per stored byte. The
/// estimate is exact in a `u64` (saturating only at `u64::MAX`); the cache
/// hands moka a rounded-up coarser unit when the budget needs one.
///
/// # Arguments
/// * `stored_bytes` - the stored files' total content size.
/// * `files` - how many files the catalog holds.
///
/// # Returns
/// The estimated resident bytes.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "the weigher: runs on every insert, arithmetic only"
    )
)]
pub fn weight(stored_bytes: u64, files: u64) -> u64 {
    stored_bytes
        .saturating_mul(WEIGHT_FACTOR)
        .saturating_add(files.saturating_mul(PER_FILE_BYTES))
        .saturating_add(PER_CATALOG_BYTES)
}

/// [`weight`] of a loaded snapshot.
///
/// # Arguments
/// * `loaded` - the snapshot.
///
/// # Returns
/// Its estimated resident bytes.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "the weigher: runs on every insert, arithmetic only"
    )
)]
pub fn weight_of(loaded: &Loaded) -> u64 {
    // `usize` to `u64` never loses anything on the 64-bit targets this
    // builds for; `try_from` says so without a silent cast.
    weight(
        loaded.stored_bytes(),
        u64::try_from(loaded.files()).unwrap_or(u64::MAX),
    )
}

/// Whether a failed load or refresh is the stored data's fault, so the same
/// generation would fail the same way: not a deleted catalog, and not a
/// transient database failure.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: maps a value, no I/O"
    )
)]
fn from_stored_data(error: &OkfError) -> bool {
    // `!matches!(..)`: every other variant comes from building the data. A
    // store that cannot be reached says nothing about the data: it is not
    // remembered as broken, and the next request tries the store again.
    !matches!(
        error,
        OkfError::CatalogNotSelected { .. }
            | OkfError::Database { .. }
            | OkfError::Busy { .. }
            | OkfError::StoreUnavailable { .. }
    )
}

/// What [`CatalogCache::catch_up`] did with a resident entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatchUp {
    /// The catalog is not cached here; nothing to refresh.
    NotResident,
    /// Its snapshot had already reached the generation.
    Current,
    /// A new snapshot was swapped in.
    Applied,
}

/// Catalog snapshots by catalog id, bounded in estimated bytes.
pub struct CatalogCache {
    // Each catalog is read from its own store, which the registry opens on
    // first use.
    stores: Stores,
    states: Bounded<i64, Arc<CatalogState>>,
    // `(id, generation)` pairs whose stored data would not build.
    broken: Bounded<(i64, i64), ()>,
    // One request at a time brings a resident catalog up to its row, by
    // catalog id: the broken check, the refresh and the recording of its
    // failure happen together, so requests queued behind a generation that
    // does not build see it recorded instead of building it again. Taken
    // before (never inside) the rebuild lock `CatalogState::refresh` takes.
    catching_up: KeyedLocks,
}

impl std::fmt::Debug for CatalogCache {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "a Debug impl")
    )]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CatalogCache")
            .field("states", &self.states)
            .finish_non_exhaustive()
    }
}

impl CatalogCache {
    /// An empty cache.
    ///
    /// # Arguments
    /// * `stores` - the registry the catalogs it holds are read through,
    ///   each from its own store.
    /// * `budget_bytes` - the most estimated bytes kept
    ///   (`catalog_cache_bytes`).
    /// * `metrics` - the instruments `okf.cache.*` is recorded on.
    ///
    /// # Returns
    /// The cache.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn new(stores: Stores, budget_bytes: u64, metrics: Metrics) -> Self {
        let states = Bounded::new(CacheName::Catalog, Limit::Bytes(budget_bytes), SNAPSHOT_TTL)
            .with_weigher(|_id: &i64, state: &Arc<CatalogState>| weight_of(&state.current()))
            .with_metrics(metrics);
        Self {
            stores,
            states,
            broken: Bounded::new(
                CacheName::Catalog,
                Limit::Entries(BROKEN_ENTRIES),
                BROKEN_TTL,
            ),
            catching_up: KeyedLocks::new(),
        }
    }

    /// The state of `row`'s catalog at `row.generation` or newer: cached,
    /// refreshed in place, or loaded (once, however many requests miss
    /// together).
    ///
    /// # Arguments
    /// * `row` - the catalog's directory row, as the request resolved it.
    /// * `rebuilds` - the rebuild locks, by catalog id (`App::rebuild_locks`).
    ///
    /// # Returns
    /// The state. A refresh that fails keeps serving the cached snapshot.
    ///
    /// # Errors
    /// [`OkfError::CatalogNotSelected`] if the catalog was deleted, or if
    /// its stored data does not build at this generation and no older
    /// snapshot is cached; [`OkfError::Database`] if a load fails (not
    /// cached); [`OkfError::Busy`] for a request that shared another's
    /// failed load (retry).
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn get(
        &self,
        row: &CatalogRow,
        rebuilds: &KeyedLocks,
    ) -> Result<Arc<CatalogState>, OkfError> {
        if self.is_broken(row.id, row.generation).await {
            // An older snapshot, if one is still cached, keeps being served.
            return self
                .states
                .peek(&row.id)
                .await
                .ok_or_else(|| not_loaded(row));
        }
        let loaded = self
            .states
            .get_or_load(row.id, || async {
                // Boxed: the load's future is large, and a debug build would
                // copy it onto the stack at each layer.
                self.within_store_bound(
                    &row.store,
                    Box::pin(async {
                        let db = self.stores.get(&row.store).await?;
                        CatalogState::load(&db, row).await.map(Arc::new)
                    }),
                )
                .await
            })
            .await;
        let state = match loaded {
            Ok(state) => state,
            Err(error) => return Err(self.load_failed(row, error).await),
        };
        if state.current().generation() >= row.generation {
            return Ok(state);
        }
        // The guard (`_` prefix: kept for its lifetime) is held until the
        // refresh's outcome is recorded below.
        let _catching_up = self.catching_up.lock(row.id).await;
        // Again under the lock: a request ahead of this one may have
        // refreshed it, or found that this generation does not build.
        if state.current().generation() >= row.generation
            || self.is_broken(row.id, row.generation).await
        {
            return Ok(state);
        }
        let refreshed = self
            .within_store_bound(
                state.store(),
                Box::pin(async {
                    let db = self.stores.get(state.store()).await?;
                    state.refresh(&db, rebuilds).await
                }),
            )
            .await;
        match refreshed {
            Ok(true) => self.refreshed(row.id).await,
            Ok(false) => {}
            Err(error @ OkfError::CatalogNotSelected { .. }) => {
                self.states.invalidate(&row.id).await;
                return Err(error);
            }
            // An outage is not a stale answer: the catalog has changed and
            // the store cannot say how, so the read is unavailable (the old
            // snapshot is still served for any other failure, below).
            Err(error @ OkfError::StoreUnavailable { .. }) => return Err(error),
            Err(error) => {
                self.refresh_failed(row.id, row.generation, &row.name, &error)
                    .await
            }
        }
        Ok(state)
    }

    /// Brings a resident catalog up to `generation` after a change-feed
    /// message, an announcement or this replica's own import. A catalog
    /// that is not resident is not loaded (no load storm), but a load of it
    /// running now is kept out of the cache: it may have read an older
    /// generation (spec Decision 11).
    ///
    /// # Arguments
    /// * `id` - the catalog's id.
    /// * `generation` - the generation its write committed.
    /// * `rebuilds` - the rebuild locks, by catalog id.
    ///
    /// # Returns
    /// What was done.
    ///
    /// # Errors
    /// [`OkfError::CatalogNotSelected`] if the catalog was deleted (its
    /// entry is dropped); [`OkfError::Database`]; a build error (the old
    /// snapshot stays, and the next use tries the refresh again).
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn catch_up(
        &self,
        id: i64,
        generation: i64,
        rebuilds: &KeyedLocks,
    ) -> Result<CatchUp, OkfError> {
        let Some(state) = self.states.peek(&id).await else {
            // Moves the epoch of a load in flight (only then: an id nobody
            // is loading keeps no state), so it serves without being kept.
            self.states.invalidate(&id).await;
            return Ok(CatchUp::NotResident);
        };
        if generation <= state.current().generation() {
            return Ok(CatchUp::Current);
        }
        let refreshed = match self.stores.get(state.store()).await {
            Ok(db) => state.refresh(&db, rebuilds).await,
            Err(error) => Err(error),
        };
        match refreshed {
            Ok(true) => {
                self.refreshed(id).await;
                Ok(CatchUp::Applied)
            }
            Ok(false) => Ok(CatchUp::Current),
            Err(error @ OkfError::CatalogNotSelected { .. }) => {
                self.states.invalidate(&id).await;
                Err(error)
            }
            // Not remembered as broken here: the next use retries the
            // refresh once (`get`), and remembers it if it fails again.
            Err(error) => Err(error),
        }
    }

    /// Re-weighs `id`'s entry after its state was refreshed in place (moka
    /// weighs on insert only), dropping it if it no longer fits the budget.
    /// Every in-place refresh ends here: [`CatalogCache::get`]'s,
    /// [`CatalogCache::catch_up`]'s and a publisher's own.
    ///
    /// # Arguments
    /// * `id` - the catalog's id; nothing happens if it is not cached.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn refreshed(&self, id: i64) {
        self.states.reweigh(&id).await;
    }

    /// [`CatalogCache::refreshed`] for a caller that may have refreshed
    /// `state` in place itself (an overlay or a diff that needed a newer
    /// snapshot, `CatalogState::current_covering`): re-weighs only if the
    /// snapshot moved past `before`.
    ///
    /// # Arguments
    /// * `state` - the state the caller used.
    /// * `before` - its snapshot's generation before the call.
    #[tracing::instrument(level = "trace", skip_all)]
    pub async fn refreshed_if_moved(&self, state: &CatalogState, before: i64) {
        if state.current().generation() != before {
            self.refreshed(state.id()).await;
        }
    }

    /// Drops `id`'s entry (a deleted catalog), and keeps a load of it that is
    /// running now out of the cache.
    ///
    /// # Arguments
    /// * `id` - the catalog's id.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn forget(&self, id: i64) {
        self.states.invalidate(&id).await;
    }

    /// The cached state of catalog `id`, without loading it or counting a
    /// request.
    ///
    /// # Arguments
    /// * `id` - the catalog's id.
    ///
    /// # Returns
    /// The state, or `None` if it is not cached.
    #[tracing::instrument(level = "trace", skip_all)]
    pub async fn resident(&self, id: i64) -> Option<Arc<CatalogState>> {
        self.states.peek(&id).await
    }

    /// How many catalogs are cached (`/healthz`'s `cached_catalogs`).
    ///
    /// # Returns
    /// The entry count, as moka settles it.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "reads one counter")
    )]
    pub fn cached(&self) -> u64 {
        self.states.entry_count()
    }

    /// Runs moka's deferred bookkeeping, so counts and gauges settle.
    #[cfg(test)]
    pub async fn settle(&self) {
        self.states.settle().await;
    }

    /// Whether `(id, generation)` is remembered as not building.
    #[tracing::instrument(level = "trace", skip_all)]
    async fn is_broken(&self, id: i64, generation: i64) -> bool {
        self.broken.peek(&(id, generation)).await.is_some()
    }

    /// Remembers that `(id, generation)` does not build.
    #[tracing::instrument(level = "trace", skip_all)]
    async fn remember_broken(&self, id: i64, generation: i64) {
        // `Bounded` stores through a load; this one cannot fail. `ok()`
        // drops the `Busy` a shared load could report: there is no value.
        self.broken
            .get_or_load((id, generation), || async { Ok(()) })
            .await
            .ok();
    }

    /// Runs a read of `store` under the store's bound
    /// ([`Stores::state_load_timeout`]) and reads its failures as a store
    /// outage: a transport error from an established connection, a pool that
    /// cannot hand one out, or a read that stalls past the bound is
    /// [`OkfError::StoreUnavailable`], not [`OkfError::Database`].
    ///
    /// # Arguments
    /// * `store` - the store the read runs on.
    /// * `read` - the read.
    ///
    /// # Returns
    /// What `read` returns, with a transport failure or a stall as
    /// `StoreUnavailable`.
    ///
    /// # Errors
    /// As `read`; [`OkfError::StoreUnavailable`] on a stall.
    #[tracing::instrument(level = "debug", skip_all)]
    async fn within_store_bound<T>(
        &self,
        store: &crate::stores::StoreName,
        read: impl std::future::Future<Output = Result<T, OkfError>>,
    ) -> Result<T, OkfError> {
        match tokio::time::timeout(self.stores.state_load_timeout(), read).await {
            Ok(outcome) => {
                outcome.map_err(|error| crate::stores::classify_store_failure(store, error))
            }
            Err(_) => Err(OkfError::store_unavailable(store)),
        }
    }

    /// Logs a failed load and turns it into the request's answer: a build
    /// error is remembered for that generation and answered as "not loaded",
    /// as a catalog that failed to load was absent before; a database
    /// failure or a deleted catalog passes through.
    #[tracing::instrument(level = "debug", skip_all)]
    async fn load_failed(&self, row: &CatalogRow, error: OkfError) -> OkfError {
        if !from_stored_data(&error) {
            return error;
        }
        self.remember_broken(row.id, row.generation).await;
        split_event!(
            warn,
            { catalog = %row.name, kind = error.kind(), },
            { %error, },
            "catalog load failed; retried when the catalog changes"
        );
        not_loaded(row)
    }

    /// Logs a failed in-place refresh; the caller keeps serving the old
    /// snapshot. A build error is remembered for that generation, so the
    /// next request does not rebuild it.
    #[tracing::instrument(level = "debug", skip_all)]
    async fn refresh_failed(&self, id: i64, generation: i64, name: &str, error: &OkfError) {
        let retry = if from_stored_data(error) {
            self.remember_broken(id, generation).await;
            "when the catalog changes"
        } else {
            "at the next request"
        };
        split_event!(
            warn,
            { catalog = %name, kind = error.kind(), retry, },
            { %error, },
            "catalog refresh failed; the old snapshot is served"
        );
    }
}

/// The answer for a catalog with no snapshot to serve: what a request got
/// before milestone 18 for a catalog that was not loaded.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: maps a value, no I/O"
    )
)]
fn not_loaded(row: &CatalogRow) -> OkfError {
    OkfError::CatalogNotSelected {
        catalog: row.name.clone(),
    }
}

#[cfg(test)]
#[path = "catalogs_tests.rs"]
mod tests;
