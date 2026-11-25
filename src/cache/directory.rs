//! The catalog directory (M18 spec Decisions 6 and 7, store side): catalog
//! rows by name, with the access facts of Decision 9, so a request can ask
//! "does this catalog exist, who owns it, is it suspended" without loading
//! its content.
//!
//! Two caches back it, because a present row and an absent name live for
//! different times and [`Bounded`] has one lifetime per cache:
//!
//! * `rows` keeps a row for `catalog_ttl` (10 s by default);
//! * `absent` remembers that a name has no catalog for [`ABSENT_TTL`] (5 s).
//!
//! Creating a catalog sends [`Invalidation::CatalogMeta`], which drops both
//! entries of its name. A publish or import sends [`Invalidation::Catalog`],
//! which drops a cached row that is behind the new generation and keeps
//! every load running at that moment out of the cache (the minimum
//! generation of Decision 11, by the cache's fence).
//!
//! # For Python readers
//!
//! The directory is shared by reference (`Arc<CatalogDirectory>` or `&`);
//! every method takes `&self`. `Option<Arc<CatalogRow>>` is "a row or
//! `None`", with the row shared between callers instead of copied.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use futures_util::StreamExt as _;
use sqlx::PgPool;

use crate::auth::{Principal, Reach};
use crate::cache::{Bounded, CacheName, Limit};
use crate::error::OkfError;
use crate::invalidate::Invalidation;
use crate::store::catalogs::{self, CatalogRow};
use crate::store::keys::Access;
use crate::stores::{StoreName, Stores, READ_CONCURRENCY};
use crate::telemetry::metrics::Metrics;

/// How long a name with no catalog is remembered (spec Decision 6).
pub const ABSENT_TTL: Duration = Duration::from_secs(5);

/// What one shared read found, with the invalidation counter it started
/// under. Every caller that shares the read (single flight) judges the
/// answer by that counter, not by one it read itself later: a follower that
/// joined after a create was applied would otherwise take an overtaken
/// "no such catalog" for a current one.
#[derive(Debug, Clone)]
struct Loaded {
    row: Option<Arc<CatalogRow>>,
    seen: u64,
}

/// A store's answer to the generation probe: `(catalog id, generation)`.
type Generations = Vec<(i64, i64)>;

/// A catalog's directory row, and whether its store could be reached to
/// complete it ([`CatalogDirectory::resolve_or_unavailable`]).
#[derive(Debug, Clone)]
pub(crate) enum Lookup {
    /// The row, with its generation.
    Available(Arc<CatalogRow>),
    /// The row as the account database has it, generation 0: the store
    /// holding the catalog's data cannot be reached.
    Unavailable(Arc<CatalogRow>),
}

/// How many rows a listing page holds when the client names no `limit`
/// (spec Decision 7).
pub const DEFAULT_PAGE: u32 = 100;

/// The most rows a listing page holds, whatever `limit` asks for.
pub const MAX_PAGE: u32 = 1000;

/// One page of a listing and the cursor that continues it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page<T> {
    /// The page's items, in order.
    pub items: Vec<T>,
    /// What to send as `after` for the next page; `None` on the last page.
    pub next: Option<String>,
    /// The names of the catalogs among `items` whose store could not be
    /// reached (sorted, each once): their rows are listed as the account
    /// database has them, with generation 0, and a caller that loads their
    /// data leaves them out and names them instead. Only rows the listing
    /// returns, so a catalog the caller does not reach is never named.
    pub unavailable: Vec<String>,
}

impl Page<Arc<CatalogRow>> {
    /// The first `size` of `rows` (sorted by name), with a cursor when more
    /// follow.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: maps a value, no I/O"
        )
    )]
    fn cut(mut rows: Vec<Arc<CatalogRow>>, size: usize) -> Self {
        let more = rows.len() > size;
        rows.truncate(size);
        // Only a page that has a row after it gets a cursor: its last name.
        let next = more
            .then(|| rows.last().map(|row| cursor_of(&row.name)))
            .flatten();
        Self {
            items: rows,
            next,
            unavailable: Vec::new(),
        }
    }
}

/// The page size a listing serves for the client's `limit`.
///
/// # Arguments
/// * `limit` - what the client asked for; `None` or 0 means the default.
///
/// # Returns
/// [`DEFAULT_PAGE`] for none, else `limit` capped at [`MAX_PAGE`].
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: maps a value, no I/O"
    )
)]
pub fn page_size(limit: Option<u32>) -> u32 {
    match limit {
        None | Some(0) => DEFAULT_PAGE,
        Some(limit) => limit.min(MAX_PAGE),
    }
}

/// The cursor of a page whose last catalog is `name`: unpadded base64url of
/// the name, so clients treat it as opaque rather than as a name.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: maps a value, no I/O"
    )
)]
pub(crate) fn cursor_of(name: &str) -> String {
    URL_SAFE_NO_PAD.encode(name)
}

/// Reads a client's `after` cursor back into the catalog name it continues
/// from. Surrounding whitespace is dropped and an empty value is no cursor.
/// Anything this server could not have given (not canonical unpadded
/// base64url, not UTF-8, or not a valid catalog name, a NUL included) is
/// refused the same way, before any query runs.
///
/// # Arguments
/// * `cursor` - the client's value, if any.
///
/// # Returns
/// The name to continue after, or `None` for the first page.
///
/// # Errors
/// [`OkfError::InvalidPath`] (400, naming `after`) for a malformed cursor.
#[tracing::instrument(level = "trace", skip_all)]
pub fn parse_cursor(cursor: Option<&str>) -> Result<Option<String>, OkfError> {
    // `filter` turns `Some("")` into `None`: an empty field is no cursor.
    let Some(cursor) = cursor.map(str::trim).filter(|cursor| !cursor.is_empty()) else {
        return Ok(None);
    };
    URL_SAFE_NO_PAD
        .decode(cursor)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .filter(|name| catalogs::is_valid_name(name))
        .map(Some)
        .ok_or_else(|| OkfError::InvalidPath {
            path: "after".to_string(),
            reason: "not a cursor this server gave: pass a listing's `next` as it came".to_string(),
        })
}

/// Catalog rows by name, with absent names remembered briefly.
pub struct CatalogDirectory {
    pool: PgPool,
    // Where each catalog's generation is read: its store's `catalog_state`.
    // The server's own registry, so a store's pool is the one pool the
    // process holds for it.
    stores: Stores,
    // `None` is a name with no catalog. It is dropped from here at once (see
    // `remember_absent`) and kept in `absent` instead, for the shorter time.
    rows: Bounded<String, Loaded>,
    // Names known to have no catalog. Not reported as `okf.cache.*`: its
    // requests are the row cache's misses, already counted there.
    absent: Bounded<String, ()>,
    // Moved by every `CatalogMeta` message, so a lookup that read "no such
    // catalog" before a create can tell it must not remember that answer.
    meta_changes: AtomicU64,
}

impl std::fmt::Debug for CatalogDirectory {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "a Debug impl")
    )]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CatalogDirectory")
            .field("rows", &self.rows)
            .finish_non_exhaustive()
    }
}

impl CatalogDirectory {
    /// An empty directory.
    ///
    /// # Arguments
    /// * `pool` - the account database.
    /// * `stores` - the registry of stores (each catalog's generation is
    ///   read from its store).
    /// * `entries` - the most rows (and, separately, absent names) kept
    ///   (`catalog_directory_entries`).
    /// * `ttl` - how long a row is served (`catalog_ttl`).
    /// * `metrics` - the instruments `okf.cache.*` is recorded on.
    ///
    /// # Returns
    /// The directory.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn new(
        pool: PgPool,
        stores: Stores,
        entries: u64,
        ttl: Duration,
        metrics: Metrics,
    ) -> Self {
        Self::with_absent_ttl(pool, stores, entries, ttl, ABSENT_TTL, metrics)
    }

    /// [`CatalogDirectory::new`] with a registry of its own over `pool`
    /// (the tests: one pool, `main` in the account database).
    #[cfg(test)]
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "test constructor")
    )]
    pub(crate) fn for_tests(pool: PgPool, entries: u64, ttl: Duration, metrics: Metrics) -> Self {
        let stores = Stores::new(
            pool.clone(),
            crate::stores::PoolSettings::from_config(&crate::config::Config::default()),
        );
        Self::new(pool, stores, entries, ttl, metrics)
    }

    /// [`CatalogDirectory::new`] with the lifetime of an absent name given:
    /// the seam a test uses to see one expire without waiting 5 s.
    #[tracing::instrument(level = "trace", skip_all)]
    fn with_absent_ttl(
        pool: PgPool,
        stores: Stores,
        entries: u64,
        ttl: Duration,
        absent_ttl: Duration,
        metrics: Metrics,
    ) -> Self {
        Self {
            pool,
            stores,
            rows: Bounded::new(CacheName::Directory, Limit::Entries(entries), ttl)
                .with_metrics(metrics),
            absent: Bounded::new(CacheName::Directory, Limit::Entries(entries), absent_ttl),
            meta_changes: AtomicU64::new(0),
        }
    }

    /// Sets each row's `generation` from its store's `catalog_state`: the
    /// registry rows no longer carry it. A catalog with no state row keeps 0.
    ///
    /// The stores are asked together, at most [`READ_CONCURRENCY`] at a time,
    /// each within `store_connect_timeout` ([`Stores::connect_timeout`]). A
    /// store that cannot be reached, or does not answer in that time, does
    /// not fail the others: its rows keep generation 0 and the store is
    /// returned, so the caller decides whether that fails the read (a
    /// single-catalog lookup) or only names the catalogs (a listing).
    ///
    /// # Returns
    /// The stores that could not be reached.
    ///
    /// # Errors
    /// [`OkfError::Database`] when a reachable store's query fails.
    #[tracing::instrument(level = "debug", skip_all)]
    async fn fill_generations(
        &self,
        rows: &mut [CatalogRow],
    ) -> Result<HashSet<StoreName>, OkfError> {
        let mut by_store: HashMap<StoreName, Vec<i64>> = HashMap::new();
        for row in rows.iter() {
            by_store.entry(row.store.clone()).or_default().push(row.id);
        }
        let bound = self.stores.connect_timeout();
        // `buffer_unordered(n)` runs at most `n` of these futures at once,
        // like a thread pool of size `n`, and yields each result as it
        // finishes. The stores sit behind `&self`, so the futures borrow it.
        let answers: Vec<(StoreName, Result<Generations, OkfError>)> =
            futures_util::stream::iter(by_store)
                .map(|(store, ids)| async move {
                    let read = async {
                        // Boxed: opening a pool is a deep future, and this
                        // sits on every request's path.
                        let db = Box::pin(self.stores.get(&store)).await?;
                        catalogs::generations(db.executor(), &ids).await.map_err(
                            |error| match error {
                                // A connection that failed mid-query is the
                                // store being unreachable, as for a connect.
                                OkfError::Database { source } => db.classify(source),
                                other => other,
                            },
                        )
                    };
                    let answer = tokio::time::timeout(bound, read)
                        .await
                        .unwrap_or_else(|_| Err(OkfError::store_unavailable(&store)));
                    (store, answer)
                })
                .buffer_unordered(READ_CONCURRENCY)
                .collect()
                .await;
        let mut generations: HashMap<i64, i64> = HashMap::with_capacity(rows.len());
        let mut down = HashSet::new();
        for (store, answer) in answers {
            match answer {
                Ok(found) => generations.extend(found),
                Err(OkfError::StoreUnavailable { .. }) => {
                    down.insert(store);
                }
                Err(error) => return Err(error),
            }
        }
        for row in rows.iter_mut() {
            row.generation = generations.get(&row.id).copied().unwrap_or(0);
        }
        Ok(down)
    }

    /// [`CatalogDirectory::fill_generations`] for one row, which must be
    /// reachable.
    ///
    /// # Errors
    /// [`OkfError::StoreUnavailable`] if the row's store cannot be reached;
    /// otherwise as [`CatalogDirectory::fill_generations`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) async fn with_generation(
        &self,
        row: Option<CatalogRow>,
    ) -> Result<Option<CatalogRow>, OkfError> {
        let Some(row) = row else { return Ok(None) };
        let mut rows = [row];
        let down = self.fill_generations(&mut rows).await?;
        let [row] = rows;
        if down.contains(&row.store) {
            return Err(OkfError::store_unavailable(&row.store));
        }
        Ok(Some(row))
    }

    /// [`CatalogDirectory::resolve`] that does not fail for a catalog whose
    /// store cannot be reached: the row (read from the account database,
    /// with generation 0, and not cached) comes back marked
    /// [`Lookup::Unavailable`], so the caller can still decide whether the
    /// key may see the catalog before naming it as unavailable.
    ///
    /// # Arguments
    /// * `name` - the catalog's name.
    /// * `known_down` - stores already found unreachable in this call: a
    ///   row on one of them is not probed again, so a call pays one
    ///   connect timeout per store, not one per catalog.
    ///
    /// # Returns
    /// The lookup, or `None` if there is no such catalog.
    ///
    /// # Errors
    /// As [`CatalogDirectory::resolve`], but never
    /// [`OkfError::StoreUnavailable`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) async fn resolve_or_unavailable(
        &self,
        name: &str,
        known_down: &HashSet<StoreName>,
    ) -> Result<Option<Lookup>, OkfError> {
        let loaded = self
            .resolve_by(name, || async {
                let row = catalogs::row_by_name(&self.pool, name).await?;
                if let Some(row) = row.as_ref().filter(|row| known_down.contains(&row.store)) {
                    return Err(OkfError::store_unavailable(&row.store));
                }
                Ok(self.with_generation(row).await?.map(Arc::new))
            })
            .await;
        match loaded {
            Ok(row) => Ok(row.map(Lookup::Available)),
            Err(OkfError::StoreUnavailable { .. }) => {
                // The failed read was not cached; the account row is enough.
                Ok(catalogs::row_by_name(&self.pool, name)
                    .await?
                    .map(|row| Lookup::Unavailable(Arc::new(row))))
            }
            Err(error) => Err(error),
        }
    }

    /// The row of catalog `name`, read from the database once however many
    /// callers ask together and then served until it expires or is
    /// invalidated.
    ///
    /// # Arguments
    /// * `name` - the catalog's name; a malformed one is the same as an
    ///   unknown one, and reaches neither the cache nor the database.
    ///
    /// # Returns
    /// The row, or `None` if there is no such catalog.
    ///
    /// # Errors
    /// [`OkfError::Database`] if the read fails; [`OkfError::Busy`] for a
    /// caller that shared another caller's failed read (retry).
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn resolve(&self, name: &str) -> Result<Option<Arc<CatalogRow>>, OkfError> {
        self.resolve_by(name, || async {
            let row = catalogs::row_by_name(&self.pool, name).await?;
            Ok(self.with_generation(row).await?.map(Arc::new))
        })
        .await
    }

    /// [`CatalogDirectory::resolve`] with the read given, so a test can hold
    /// a read open while it sends an invalidation.
    #[tracing::instrument(level = "trace", skip_all)]
    async fn resolve_by<F, Fut>(
        &self,
        name: &str,
        load: F,
    ) -> Result<Option<Arc<CatalogRow>>, OkfError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Option<Arc<CatalogRow>>, OkfError>>,
    {
        if !catalogs::is_valid_name(name) {
            return Ok(None);
        }
        let key = name.to_owned();
        if self.absent.peek(&key).await.is_some() {
            return Ok(None);
        }
        let loaded = self
            .rows
            .get_or_load(key.clone(), || self.read(load))
            .await?;
        if loaded.row.is_none() {
            self.remember_absent(key, loaded.seen).await;
        }
        Ok(loaded.row)
    }

    /// One read of the database, stamped with the counter as it was before
    /// the read started: a create that lands after moves the counter, so
    /// every caller sharing this result sees the answer was overtaken.
    #[tracing::instrument(level = "trace", skip_all)]
    async fn read<F, Fut>(&self, load: F) -> Result<Loaded, OkfError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Option<Arc<CatalogRow>>, OkfError>>,
    {
        let seen = self.meta_changes.load(Ordering::SeqCst);
        let row = load().await?;
        Ok(Loaded { row, seen })
    }

    /// Remembers that `key` has no catalog, unless a `CatalogMeta` message
    /// arrived since `seen` (a create may have committed after the read that
    /// found nothing).
    #[tracing::instrument(level = "trace", skip_all)]
    async fn remember_absent(&self, key: String, seen: u64) {
        // The row cache does not keep an absence: the shorter-lived cache does.
        self.rows.invalidate(&key).await;
        if self
            .absent
            .get_or_load(key.clone(), || async { Ok(()) })
            .await
            .is_err()
        {
            return;
        }
        // The message bumps the counter before it drops the entry, so either
        // it drops this entry or this check sees its bump.
        if self.meta_changes.load(Ordering::SeqCst) != seen {
            self.absent.invalidate(&key).await;
        }
    }

    /// The rows of `names`: those cached are served from the cache, the rest
    /// are read together in one query. The rows read are not cached, so a
    /// listing never adds entries (it cannot evict what requests use). A row
    /// already cached that the listing visits is counted as recently used
    /// (moka has no read that does not); that is bounded by the page size and
    /// is acceptable, since a catalog a principal can list is one it uses.
    ///
    /// # Arguments
    /// * `names` - catalog names; duplicates, malformed names and names with
    ///   no catalog add nothing.
    ///
    /// # Returns
    /// The rows found, ordered by name, and the stores that could not be
    /// reached (each is probed within `store_connect_timeout`): their rows are in
    /// the list with generation 0.
    ///
    /// # Errors
    /// [`OkfError::Database`] if the read fails.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn resolve_many(
        &self,
        names: &[String],
    ) -> Result<(Vec<Arc<CatalogRow>>, HashSet<StoreName>), OkfError> {
        let mut seen: HashSet<&str> = HashSet::with_capacity(names.len());
        let mut found: Vec<Arc<CatalogRow>> = Vec::new();
        let mut missing: Vec<String> = Vec::new();
        for name in names {
            if !seen.insert(name.as_str()) {
                continue;
            }
            match self.rows.peek_fresh(name).await {
                Some(Loaded { row: Some(row), .. }) => found.push(row),
                // Cached as no catalog (only ever briefly) is not trusted
                // here: the query below is the authority for a bulk read.
                Some(Loaded { row: None, .. }) | None => missing.push(name.clone()),
            }
        }
        let mut down = HashSet::new();
        if !missing.is_empty() {
            let mut rows = catalogs::rows_by_names(&self.pool, &missing).await?;
            down = self.fill_generations(&mut rows).await?;
            found.extend(rows.into_iter().map(Arc::new));
        }
        found.sort_by(|left, right| left.name.cmp(&right.name));
        Ok((found, down))
    }

    /// The catalogs `principal` reaches, one page at a time, by name (spec
    /// Decision 7): what every catalog listing returns.
    ///
    /// # Arguments
    /// * `principal` - the caller.
    /// * `after` - the `next` of the previous page, as the client sent it
    ///   (see [`parse_cursor`]); `None` or empty for the first page.
    /// * `limit` - the page size ([`page_size`]); held to 1 to [`MAX_PAGE`].
    ///
    /// # Returns
    /// The page's rows, ordered by name, and the cursor of the page after.
    ///
    /// # Errors
    /// As [`CatalogDirectory::offered_page`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn reachable_page(
        &self,
        principal: &Principal,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Page<Arc<CatalogRow>>, OkfError> {
        self.offered_page(principal, None, after, limit).await
    }

    /// The catalogs `principal` reaches and may read now, one page at a time
    /// (issue #955): [`CatalogDirectory::reachable_page`] followed by the
    /// content admission ([`Principal::admission`] for a read), so a catalog
    /// an administrator reaches but has no live unlock for is left out. The
    /// admission is pure: nothing is recorded, so listing counts as no read.
    ///
    /// Reach pages are read until `limit + 1` readable rows are found or the
    /// reach list ends, so a page is full whenever more readable catalogs
    /// follow (a run of locked catalogs, even a whole reach page of them, is
    /// skipped) and `next` is set only when a readable row follows.
    ///
    /// # Arguments
    /// * `principal` - the caller.
    /// * `after` - as for [`CatalogDirectory::reachable_page`].
    /// * `limit` - as for [`CatalogDirectory::reachable_page`].
    /// * `now` - the server clock, against which unlocks expire.
    ///
    /// # Returns
    /// The page's rows, ordered by name, and the cursor of the page after.
    ///
    /// # Errors
    /// As [`CatalogDirectory::offered_page`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn readable_page(
        &self,
        principal: &Principal,
        after: Option<&str>,
        limit: u32,
        now: DateTime<Utc>,
    ) -> Result<Page<Arc<CatalogRow>>, OkfError> {
        // A scan page at least as large as the answer, so a run of readable
        // catalogs takes one read; the cursor still only moves forward.
        let scan = limit.clamp(1, MAX_PAGE).max(DEFAULT_PAGE);
        self.readable_page_by(principal, after, limit, now, scan)
            .await
    }

    /// [`CatalogDirectory::readable_page`] reading the reach list `scan`
    /// catalogs at a time (the tests use a small `scan` to cross scan pages).
    ///
    /// # Arguments
    /// * `scan` - how many reachable catalogs one read looks at.
    ///
    /// # Errors
    /// As [`CatalogDirectory::readable_page`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) async fn readable_page_by(
        &self,
        principal: &Principal,
        after: Option<&str>,
        limit: u32,
        now: DateTime<Utc>,
        scan: u32,
    ) -> Result<Page<Arc<CatalogRow>>, OkfError> {
        let limit = limit.clamp(1, MAX_PAGE);
        let mut readable: Vec<Arc<CatalogRow>> = Vec::new();
        let mut down: HashSet<String> = HashSet::new();
        let mut cursor = after.map(str::to_owned);
        loop {
            let page = self
                .reachable_page(principal, cursor.as_deref(), scan)
                .await?;
            down.extend(page.unavailable);
            readable.extend(
                page.items
                    .into_iter()
                    .filter(|row| principal.admission(row, Access::Read, now).is_ok()),
            );
            cursor = page.next;
            // `limit + 1`: the extra row tells `cut` that a page follows.
            if cursor.is_none() || readable.len() > limit as usize {
                break;
            }
        }
        let mut page = Page::cut(readable, limit as usize);
        let mut unavailable: Vec<String> = page
            .items
            .iter()
            .filter(|row| down.contains(&row.name))
            .map(|row| row.name.clone())
            .collect();
        unavailable.sort();
        page.unavailable = unavailable;
        Ok(page)
    }

    /// [`CatalogDirectory::reachable_page`] narrowed to an MCP connection's
    /// catalog list, when it has one (`X-OKF-Catalogs`). The rows read are
    /// not cached, as for [`CatalogDirectory::resolve_many`].
    ///
    /// * With a list, or for [`Reach::Catalogs`]: the listed or granted
    ///   catalogs the principal reaches and that exist, read together
    ///   (bounded by the list or the grants), then cut to the page.
    /// * For [`Reach::Ceiling`] (a console session): one query of up to
    ///   `limit + 1` rows of its user's ceiling after the cursor, read per
    ///   owner team from an index ([`catalogs::ceiling_page`]), never the
    ///   whole ceiling (issue #728).
    /// * Otherwise ([`Reach::All`]): one query of up to `limit + 1` rows
    ///   after the cursor, so `next` is set only when a row follows.
    ///
    /// Reach (and the list) is the only filter: suspension, maintenance and
    /// unlocks are shown or checked by the caller, from the principal, as
    /// before. The cursor only moves the start of the list, never widens
    /// it, so a cursor naming a catalog the principal does not reach
    /// reveals nothing.
    ///
    /// # Arguments
    /// * `principal` - the caller.
    /// * `connection` - the connection's list; `None` or empty for none.
    /// * `after` - as for [`CatalogDirectory::reachable_page`].
    /// * `limit` - as for [`CatalogDirectory::reachable_page`].
    ///
    /// # Returns
    /// The page's rows, ordered by name, and the cursor of the page after.
    ///
    /// # Errors
    /// [`OkfError::InvalidPath`] (400) for a malformed cursor, before any
    /// read; [`OkfError::Database`] if the read fails.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn offered_page(
        &self,
        principal: &Principal,
        connection: Option<&[String]>,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Page<Arc<CatalogRow>>, OkfError> {
        let after = parse_cursor(after)?;
        let limit = limit.clamp(1, MAX_PAGE);
        // The bounded set of names to read, or `None` to page through every
        // catalog. `filter` drops an empty list (no list at all).
        // One row more than the page, to know whether one follows.
        let fetch = i64::from(limit) + 1;
        let names: Option<Vec<String>> =
            match (connection.filter(|list| !list.is_empty()), &principal.reach) {
                // Reach is checked on each row below: a ceiling needs it.
                (Some(list), _) => Some(
                    list.iter()
                        .filter(|name| !principal.excludes(name))
                        .cloned()
                        .collect(),
                ),
                (None, Reach::Catalogs(levels)) => Some(levels.keys().cloned().collect()),
                (None, Reach::All | Reach::Ceiling) => None,
            };
        let (rows, down) = match (names, &principal.reach) {
            (Some(names), _) => {
                let (mut rows, down) = self.resolve_many(&names).await?;
                // `retain` keeps the rows after the cursor (sorted by name)
                // that the principal reaches.
                rows.retain(|row| {
                    principal.reaches_row(row)
                        && after
                            .as_deref()
                            .is_none_or(|after| row.name.as_str() > after)
                });
                (rows, down)
            }
            // A session's ceiling, a page at a time per owner team (issue
            // #728), from the teams and entitlements its checks use.
            (None, Reach::Ceiling) => {
                let Some(gates) = principal.facts.user.as_deref() else {
                    return Ok(Page::cut(Vec::new(), limit as usize));
                };
                let teams: Vec<String> = gates.teams.iter().cloned().collect();
                let entitled: Vec<i64> = gates.entitled.keys().copied().collect();
                let mut rows =
                    catalogs::ceiling_page(&self.pool, &teams, &entitled, after.as_deref(), fetch)
                        .await?;
                let down = self.fill_generations(&mut rows).await?;
                let rows = rows
                    .into_iter()
                    .map(Arc::new)
                    // Each row checked as every access check does: the
                    // query and `level_on` agree, and this keeps it so.
                    .filter(|row| principal.reaches_row(row))
                    .collect();
                (rows, down)
            }
            (None, Reach::All | Reach::Catalogs(_)) => {
                let mut rows = catalogs::page(&self.pool, after.as_deref(), fetch).await?;
                let down = self.fill_generations(&mut rows).await?;
                (rows.into_iter().map(Arc::new).collect(), down)
            }
        };
        // A `u32` always fits a `usize` on the 64-bit targets this builds for.
        let mut page = Page::cut(rows, limit as usize);
        // Named after the cut and the reach checks: only a catalog this page
        // lists, so one the caller does not reach is never named.
        page.unavailable = page
            .items
            .iter()
            .filter(|row| down.contains(&row.store))
            .map(|row| row.name.clone())
            .collect();
        Ok(page)
    }

    /// Applies a change-feed message. Only [`Invalidation::CatalogMeta`] and
    /// [`Invalidation::Catalog`] touch the directory.
    ///
    /// # Arguments
    /// * `invalidation` - what changed.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn apply(&self, invalidation: &Invalidation) {
        match invalidation {
            Invalidation::CatalogMeta { name, .. } => {
                // Counter first (see `remember_absent`). The row is keyed by
                // name and the message names it: no scan by id is needed.
                self.meta_changes.fetch_add(1, Ordering::SeqCst);
                self.rows.invalidate(name).await;
                self.absent.invalidate(name).await;
            }
            Invalidation::Catalog { id, generation } => {
                // Drops a row behind `generation`, and (the cache's fence)
                // keeps every read running now from being cached: one that
                // read an older generation must not outlive this message.
                // A read that starts later reads after the writer's commit.
                // The scan is bounded by the cache's size.
                self.rows
                    .invalidate_where(|_, row| {
                        row.row
                            .as_ref()
                            .is_some_and(|row| row.id == *id && row.generation < *generation)
                    })
                    .await;
            }
            // A suspended row holds its team's label (and a personal team's
            // holds its owner's): drop those the change can touch.
            Invalidation::Team { id } => {
                self.rows
                    .invalidate_where(|_, row| {
                        row.row.as_ref().is_some_and(|row| {
                            row.suspended.is_some() && row.owner_team_id.as_deref() == Some(id)
                        })
                    })
                    .await;
            }
            Invalidation::User { .. } => {
                self.rows
                    .invalidate_where(|_, row| {
                        row.row.as_ref().is_some_and(|row| row.suspended.is_some())
                    })
                    .await;
            }
            Invalidation::Credential { .. }
            | Invalidation::Settings
            | Invalidation::Store { .. } => {}
        }
    }

    /// After a store's change feed lost its connection, messages from that
    /// store were missed: every cached row of a catalog on it is dropped
    /// (read again on next use), and no absence is trusted, since a catalog
    /// created there may have been announced meanwhile. Rows of catalogs on
    /// other stores stay cached.
    ///
    /// # Arguments
    /// * `store` - the store whose feed was interrupted.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn drop_store(&self, store: &StoreName) {
        self.meta_changes.fetch_add(1, Ordering::SeqCst);
        self.rows
            .invalidate_where(|_, loaded| {
                loaded.row.as_ref().is_some_and(|row| row.store == *store)
            })
            .await;
        self.absent.invalidate_where(|_, _| true).await;
    }

    /// Tests: whether a row is cached for `name`.
    #[cfg(test)]
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "a test hook")
    )]
    pub(crate) async fn is_cached(&self, name: &str) -> bool {
        self.rows.peek(&name.to_owned()).await.is_some()
    }

    /// After the change feed's connection was lost, messages were missed:
    /// every row is reread on its next use, and no absence is trusted.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn mark_all_stale(&self) {
        self.meta_changes.fetch_add(1, Ordering::SeqCst);
        self.rows.mark_all_stale();
        self.absent.invalidate_where(|_, _| true).await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use chrono::Utc;
    use tokio::sync::oneshot;
    use tokio::time::timeout;

    use super::*;
    use crate::db::{test_db, TEST_TEAM};
    use crate::labels::DisplayName;
    use crate::mcp::testing::{local_metrics, render};
    use crate::store::audit::AuditActor;
    use crate::store::catalogs::{create, NewCatalog};

    /// Every wait in these tests is bounded by this.
    const BOUND: Duration = Duration::from_secs(30);
    const LONG: Duration = Duration::from_secs(3600);

    fn directory(pool: &PgPool) -> CatalogDirectory {
        CatalogDirectory::for_tests(pool.clone(), 100, LONG, Metrics::global())
    }

    async fn seed(pool: &PgPool, name: &str) -> i64 {
        let new = NewCatalog {
            name,
            display_name: &DisplayName::default(),
            description: "first",
            owner_team: TEST_TEAM,
        };
        create(pool, &new, &AuditActor::cli(), Utc::now())
            .await
            .unwrap()
            .id
    }

    async fn set_description(pool: &PgPool, name: &str, text: &str) {
        sqlx::query("UPDATE catalogs SET description = $2 WHERE name = $1")
            .bind(name)
            .bind(text)
            .execute(pool)
            .await
            .unwrap();
    }

    fn row(id: i64, generation: i64) -> Option<Arc<CatalogRow>> {
        Some(Arc::new(CatalogRow {
            store: crate::stores::StoreName::main(),
            id,
            name: "pg".to_owned(),
            display_name: String::new(),
            description: String::new(),
            generation,
            owner_team: None,
            owner_team_id: None,
            suspended: None,
            maintenance_off: false,
            audit_team: None,
            member_access: crate::store::ceiling::MemberAccess::Write,
        }))
    }

    /// Creates the shared team `ops` and a catalog `shared` it owns, then
    /// suspends it.
    async fn seed_suspended_shared(pool: &PgPool) {
        use crate::db::TEST_USER;
        use crate::store::teams::{self, NewTeam};
        let new = NewTeam {
            name: "ops",
            display_name: &DisplayName::parse("Operations").unwrap(),
            owner: TEST_USER,
        };
        teams::create(pool, &new, &AuditActor::cli(), Utc::now())
            .await
            .unwrap();
        let catalog = NewCatalog {
            name: "shared",
            display_name: &DisplayName::default(),
            description: "",
            owner_team: "ops",
        };
        create(pool, &catalog, &AuditActor::cli(), Utc::now())
            .await
            .unwrap();
        teams::suspend(pool, "ops", None, &AuditActor::cli(), Utc::now())
            .await
            .unwrap();
    }

    /// The label of the suspended owner team in `name`'s resolved row.
    async fn team_label_of(directory: &CatalogDirectory, name: &str) -> String {
        directory
            .resolve(name)
            .await
            .unwrap()
            .unwrap()
            .suspended
            .as_ref()
            .expect("suspended")
            .team_label
            .clone()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_cached_suspension_gets_a_renamed_shared_teams_label_from_its_team_message() {
        use crate::invalidate::testing::invalidations_of;
        use crate::store::teams::{self, TeamDisplayNameChange};
        let db = test_db().await;
        seed_suspended_shared(&db.pool).await;
        let directory = directory(&db.pool);
        assert_eq!(team_label_of(&directory, "shared").await, "Operations");

        let renamed = DisplayName::parse("Platform").unwrap();
        let sent = invalidations_of(&db.pool, async {
            let change = TeamDisplayNameChange {
                team: "ops",
                display_name: &renamed,
                owner: None,
            };
            teams::set_display_name(&db.pool, &change, &AuditActor::cli(), Utc::now())
                .await
                .unwrap();
        })
        .await;
        assert_eq!(
            team_label_of(&directory, "shared").await,
            "Operations",
            "nothing applied yet: the cached row is served"
        );
        for invalidation in &sent {
            directory.apply(invalidation).await;
        }

        assert_eq!(team_label_of(&directory, "shared").await, "Platform");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_cached_suspension_gets_a_renamed_personal_owners_label_from_its_user_message() {
        use crate::db::TEST_USER;
        use crate::invalidate::testing::invalidations_of;
        use crate::store::{teams, users};
        let db = test_db().await;
        seed(&db.pool, "mine").await;
        teams::suspend(&db.pool, TEST_TEAM, None, &AuditActor::cli(), Utc::now())
            .await
            .unwrap();
        let directory = directory(&db.pool);
        let before = team_label_of(&directory, "mine").await;

        let renamed = DisplayName::parse("Renamed Person").unwrap();
        let sent = invalidations_of(&db.pool, async {
            users::set_display_name(
                &db.pool,
                TEST_USER,
                &renamed,
                &AuditActor::cli(),
                Utc::now(),
            )
            .await
            .unwrap();
        })
        .await;
        assert_eq!(team_label_of(&directory, "mine").await, before);
        for invalidation in &sent {
            directory.apply(invalidation).await;
        }

        let after = team_label_of(&directory, "mine").await;
        assert_ne!(after, before, "the personal team is named after its owner");
        assert!(after.contains("Renamed Person"), "{after}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resolve_reads_once_then_serves_from_cache() {
        let db = test_db().await;
        seed(&db.pool, "pg").await;
        let (_provider, metrics, registry) = local_metrics();
        let directory = CatalogDirectory::for_tests(db.pool.clone(), 100, LONG, metrics);

        let first = directory.resolve("pg").await.unwrap().unwrap();
        // Changed behind the directory's back: only a read would see it.
        set_description(&db.pool, "pg", "second").await;
        let second = directory.resolve("pg").await.unwrap().unwrap();

        assert_eq!(first.description, "first");
        assert_eq!(second.description, "first", "served from the cache");
        let text = render(&registry);
        assert!(
            text.contains(r#"okf_cache_requests_total{cache="directory",result="miss"} 1"#),
            "one read in\n{text}"
        );
        assert!(
            text.contains(r#"okf_cache_requests_total{cache="directory",result="hit"} 1"#),
            "one hit in\n{text}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unknown_name_is_remembered_briefly_and_forgotten_by_create() {
        let db = test_db().await;
        let directory = directory(&db.pool);
        assert!(directory.resolve("x").await.unwrap().is_none());

        // Created behind the directory's back: the absence is remembered.
        let id = seed(&db.pool, "x").await;
        assert!(directory.resolve("x").await.unwrap().is_none());

        // What `create` sends once Task 2 lands: the message names the catalog.
        directory
            .apply(&Invalidation::CatalogMeta {
                id,
                name: "x".to_owned(),
            })
            .await;
        let found = directory.resolve("x").await.unwrap().expect("now found");
        assert_eq!(found.id, id);
    }

    /// Issue #972: a deleted catalog stays in the registry until its store
    /// cleanup, and the directory serves no reader a row that is marked.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_deleted_catalog_resolves_to_nothing_before_its_cleanup() {
        let db = test_db().await;
        seed(&db.pool, "gone").await;
        seed(&db.pool, "kept").await;
        crate::store::catalogs::delete(&db.pool, "gone", false, &AuditActor::cli(), Utc::now())
            .await
            .unwrap();
        let directory = directory(&db.pool);

        let gone = directory.resolve("gone").await.unwrap();
        let (many, _) = directory
            .resolve_many(&["gone".to_string(), "kept".to_string()])
            .await
            .unwrap();

        assert!(gone.is_none());
        let names: Vec<&str> = many.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, ["kept"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_absent_name_is_forgotten_after_its_lifetime() {
        let db = test_db().await;
        let directory = CatalogDirectory::with_absent_ttl(
            db.pool.clone(),
            Stores::new(
                db.pool.clone(),
                crate::stores::PoolSettings::from_config(&crate::config::Config::default()),
            ),
            100,
            LONG,
            Duration::from_millis(100),
            Metrics::global(),
        );
        assert!(directory.resolve("x").await.unwrap().is_none());
        seed(&db.pool, "x").await;

        // That the absence is remembered is proved with a long lifetime by
        // `an_unknown_name_is_remembered_briefly_and_forgotten_by_create`.
        // It is not asserted here: with a 100 ms lifetime, a loaded machine
        // can expire the entry before any such check (issue #921). Only the
        // expiry is checked, and a late wake-up cannot fail it.
        // moka reads the real clock, so this waits in real time.
        tokio::time::sleep(Duration::from_millis(300)).await;

        assert!(directory.resolve("x").await.unwrap().is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_create_that_lands_while_the_absence_is_read_is_not_remembered() {
        let db = test_db().await;
        let directory = Arc::new(directory(&db.pool));
        let (release, released) = oneshot::channel::<()>();
        let (reading, started) = oneshot::channel::<()>();
        let task = {
            let directory = Arc::clone(&directory);
            tokio::spawn(async move {
                directory
                    .resolve_by("x", || async {
                        reading.send(()).unwrap();
                        released.await.unwrap();
                        // What the read saw before the create committed.
                        Ok(None)
                    })
                    .await
            })
        };
        timeout(BOUND, started)
            .await
            .expect("the read started")
            .unwrap();
        let id = seed(&db.pool, "x").await;
        directory
            .apply(&Invalidation::CatalogMeta {
                id,
                name: "x".to_owned(),
            })
            .await;
        release.send(()).unwrap();
        let answered = timeout(BOUND, task).await.expect("finished").unwrap();
        assert!(answered.unwrap().is_none(), "the caller saw the old answer");

        let found = directory.resolve("x").await.unwrap();

        assert!(found.is_some(), "the old answer was not remembered");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_reader_that_joins_an_overtaken_read_does_not_remember_its_absence() {
        let db = test_db().await;
        let directory = Arc::new(directory(&db.pool));
        // A: the leader of a shared read that will find nothing. It parks
        // after it has stamped the counter, and runs no cleanup of its own
        // (it is a bare cache call), so only B's judgement is under test.
        let (release, released) = oneshot::channel::<()>();
        let (reading, started) = oneshot::channel::<()>();
        let leader = {
            let directory = Arc::clone(&directory);
            tokio::spawn(async move {
                directory
                    .rows
                    .get_or_load("x".to_owned(), || {
                        directory.read(|| async {
                            reading.send(()).unwrap();
                            released.await.unwrap();
                            Ok(None)
                        })
                    })
                    .await
            })
        };
        timeout(BOUND, started)
            .await
            .expect("the read started")
            .unwrap();
        // The create commits and its message is applied in full.
        let id = seed(&db.pool, "x").await;
        directory
            .apply(&Invalidation::CatalogMeta {
                id,
                name: "x".to_owned(),
            })
            .await;
        // B starts now, after the apply, and joins A's read in flight.
        let follower = {
            let directory = Arc::clone(&directory);
            tokio::spawn(async move { directory.resolve("x").await })
        };
        // B has reached the shared read once the cache counts it as a waiter;
        // there is no signal for that, so give it a moment to join.
        tokio::time::sleep(Duration::from_millis(200)).await;
        release.send(()).unwrap();
        timeout(BOUND, leader)
            .await
            .expect("A finished")
            .unwrap()
            .unwrap();
        let answered = timeout(BOUND, follower).await.expect("B finished").unwrap();
        assert!(answered.unwrap().is_none(), "B shared the old answer");

        // The old answer was not remembered: the next resolve finds it.
        assert!(directory.absent.peek(&"x".to_owned()).await.is_none());
        assert!(directory.resolve("x").await.unwrap().is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_catalog_meta_invalidation_drops_the_row() {
        let db = test_db().await;
        let id = seed(&db.pool, "pg").await;
        let directory = directory(&db.pool);
        directory.resolve("pg").await.unwrap().unwrap();
        set_description(&db.pool, "pg", "second").await;

        directory
            .apply(&Invalidation::CatalogMeta {
                id,
                name: "pg".to_owned(),
            })
            .await;

        let row = directory.resolve("pg").await.unwrap().unwrap();
        assert_eq!(row.description, "second");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_deleted_catalog_is_unknown_after_its_meta_message() {
        let db = test_db().await;
        let id = seed(&db.pool, "pg").await;
        let directory = directory(&db.pool);
        directory.resolve("pg").await.unwrap().unwrap();
        sqlx::query("DELETE FROM catalogs WHERE id = $1")
            .bind(id)
            .execute(&db.pool)
            .await
            .unwrap();

        directory
            .apply(&Invalidation::CatalogMeta {
                id,
                name: "pg".to_owned(),
            })
            .await;

        assert!(directory.resolve("pg").await.unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_catalog_invalidation_raises_the_minimum_generation() {
        let db = test_db().await;
        let directory = Arc::new(directory(&db.pool));

        // A read that saw generation 8 is running when generation 9 is sent.
        let (release, released) = oneshot::channel::<()>();
        let (reading, started) = oneshot::channel::<()>();
        let task = {
            let directory = Arc::clone(&directory);
            tokio::spawn(async move {
                directory
                    .resolve_by("pg", || async {
                        reading.send(()).unwrap();
                        released.await.unwrap();
                        Ok(row(1, 8))
                    })
                    .await
            })
        };
        timeout(BOUND, started)
            .await
            .expect("the read started")
            .unwrap();
        directory
            .apply(&Invalidation::Catalog {
                id: 1,
                generation: 9,
            })
            .await;
        release.send(()).unwrap();
        let served = timeout(BOUND, task).await.expect("finished").unwrap();

        // The caller got what it read, but the older row was not kept.
        assert_eq!(served.unwrap().unwrap().generation, 8);
        assert!(directory.rows.peek(&"pg".to_owned()).await.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_catalog_invalidation_drops_only_a_row_that_is_behind() {
        let db = test_db().await;
        let directory = directory(&db.pool);
        for (name, id, generation) in [("a", 1, 8), ("b", 2, 8), ("c", 3, 9)] {
            directory
                .resolve_by(name, || async { Ok(row(id, generation)) })
                .await
                .unwrap();
        }

        directory
            .apply(&Invalidation::Catalog {
                id: 1,
                generation: 9,
            })
            .await;
        // The same message for a row already at that generation changes nothing.
        directory
            .apply(&Invalidation::Catalog {
                id: 3,
                generation: 9,
            })
            .await;

        assert!(directory.rows.peek(&"a".to_owned()).await.is_none());
        assert!(directory.rows.peek(&"b".to_owned()).await.is_some());
        assert!(directory.rows.peek(&"c".to_owned()).await.is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn marking_stale_rereads_rows_and_forgets_absences() {
        let db = test_db().await;
        seed(&db.pool, "pg").await;
        let directory = directory(&db.pool);
        directory.resolve("pg").await.unwrap().unwrap();
        assert!(directory.resolve("x").await.unwrap().is_none());
        set_description(&db.pool, "pg", "second").await;
        seed(&db.pool, "x").await;

        directory.mark_all_stale().await;

        let row = directory.resolve("pg").await.unwrap().unwrap();
        assert_eq!(row.description, "second");
        assert!(directory.resolve("x").await.unwrap().is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resolve_many_serves_cached_rows_and_reads_the_rest_together() {
        let db = test_db().await;
        for name in ["a", "b", "c"] {
            seed(&db.pool, name).await;
        }
        let directory = directory(&db.pool);
        directory.resolve("a").await.unwrap().unwrap();
        // Changed behind the directory's back: `a` must come from the cache.
        set_description(&db.pool, "a", "second").await;
        set_description(&db.pool, "b", "second").await;
        let ask: Vec<String> = ["c", "a", "nope", "b", "a", "b\0"]
            .iter()
            .map(|name| (*name).to_owned())
            .collect();

        let (rows, down) = directory.resolve_many(&ask).await.unwrap();
        assert!(down.is_empty());

        let seen: Vec<(&str, &str)> = rows
            .iter()
            .map(|row| (row.name.as_str(), row.description.as_str()))
            .collect();
        assert_eq!(seen, [("a", "first"), ("b", "second"), ("c", "first")]);
        // A bulk read fills nothing: `c` is still not cached.
        assert!(directory.rows.peek(&"c".to_owned()).await.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_malformed_name_is_unknown_and_never_cached_or_read() {
        let db = test_db().await;
        let directory = directory(&db.pool);
        let reads = AtomicUsize::new(0);

        let found = directory
            .resolve_by("p\0g", || async {
                reads.fetch_add(1, Ordering::SeqCst);
                Ok(None)
            })
            .await
            .unwrap();

        assert!(found.is_none());
        assert_eq!(reads.load(Ordering::SeqCst), 0);
        assert!(directory.resolve("P G").await.unwrap().is_none());
    }
}

// Its own file: the listing's tests (M18 Task 10).
#[cfg(test)]
#[path = "directory_listing_tests.rs"]
mod listing_tests;
