//! Per-catalog state held in memory (design "In-memory snapshot"): the
//! snapshot of the catalog's live documents, rebuilt and swapped when the
//! catalog's `generation` moves.
//!
//! Readers call [`CatalogState::current`] and keep the `Arc` for as long as
//! they need: a swap never changes a snapshot someone is reading, and readers
//! never wait on the database.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgExecutor};

use crate::error::OkfError;
use crate::keyed_lock::KeyedLocks;
use crate::okf::analysis::lint::{lint, CommitTimes, LintReport};
use crate::okf::snapshot::Snapshot;
use crate::okf::workspace::{MemWorkspace, StoredFile};
use crate::store::catalogs::CatalogRow;
use crate::store::documents;
use crate::store::documents::DocumentRow;
use crate::stores::{CatalogDb, StoreName};

/// The catalog's current generation on its store, or `None` if the row is
/// gone.
const GENERATION: &str = "SELECT generation FROM catalog_state WHERE id = $1";

/// One loaded state of a catalog. Immutable.
#[derive(Debug)]
pub struct Loaded {
    generation: i64,
    snapshot: Snapshot,
    commit_times: CommitTimes,
    files: usize,
    last_change: Option<DateTime<Utc>>,
    // The published files themselves: an overlay copies this and applies
    // its changeset's entries (M5 Decision 6).
    workspace: MemWorkspace,
    // Each live path's current revision: a new changeset entry's base.
    revisions: HashMap<String, i32>,
    // The stored files' total size in bytes: what the catalog cache weighs
    // a snapshot by (M18 spec Decision 2).
    stored_bytes: u64,
}

impl Loaded {
    /// The catalog generation this was built from.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn generation(&self) -> i64 {
        self.generation
    }

    /// The snapshot of the live documents.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    /// Lint's commit times: each live document's last revision time (after a
    /// first import, the import time).
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn commit_times(&self) -> &CommitTimes {
        &self.commit_times
    }

    /// How many files the catalog holds (every stored file, markdown or not).
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn files(&self) -> usize {
        self.files
    }

    /// The newest document change (`updated_at`), or `None` for an empty catalog.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn last_change(&self) -> Option<DateTime<Utc>> {
        self.last_change
    }

    /// The total size of the stored files this was built from, in bytes:
    /// the base of the catalog cache's weight estimate (M18 spec Decision 2).
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn stored_bytes(&self) -> u64 {
        self.stored_bytes
    }

    /// The published files, as the snapshot was built from them.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn workspace(&self) -> &MemWorkspace {
        &self.workspace
    }

    /// A live document's current revision.
    ///
    /// # Arguments
    /// * `path` - normalized, catalog-relative.
    ///
    /// # Returns
    /// Its revision, or `None` if no live document has that path.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn revision(&self, path: &str) -> Option<i32> {
        self.revisions.get(path).copied()
    }

    /// Lints the snapshot with its commit times, so `generated-after-commit`
    /// runs.
    ///
    /// # Arguments
    /// * `now` - the current time (server clock).
    ///
    /// # Returns
    /// The findings, as [`lint`] returns them.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn lint(&self, now: DateTime<Utc>) -> LintReport {
        lint(&self.snapshot, now, Some(&self.commit_times))
    }
}

/// A catalog held in memory.
#[derive(Debug)]
pub struct CatalogState {
    id: i64,
    name: String,
    // The store its rows are in, as of the row it was loaded from. A
    // catalog never moves between stores (spec non-goal).
    store: StoreName,
    // The description and owner are not held here: they change without a
    // publish, so readers take them from the catalog's directory row
    // (`CatalogRow`, M18 spec Decision 6).
    //
    // `RwLock<Arc<_>>`: the lock is held only to clone or replace the `Arc`
    // (a pointer copy plus a reference count), never while a snapshot is read
    // or built. It is `std`'s lock, not tokio's: it is never held across an
    // `.await`, so it never blocks an async thread for longer than that copy.
    current: RwLock<Arc<Loaded>>,
    // No mutex lives here (spec M18 Decision 4): the rebuild lock and the
    // in-process half of the publish lock are keyed by catalog id on `App`
    // (`App::rebuild_locks`, `App::publish_lock`), so two states of one
    // catalog (an entry evicted and loaded again) still share them.
}

impl CatalogState {
    /// Loads a catalog: its current generation and snapshot.
    ///
    /// # Arguments
    /// * `db` - the catalog's store.
    /// * `row` - the catalog's directory row; the load reads by its
    ///   `catalogs.id`, so a catalog deleted and created again under the
    ///   name since the row was read is not loaded in its place.
    ///
    /// # Returns
    /// The state, holding the snapshot of the catalog's live documents.
    ///
    /// # Errors
    /// [`OkfError::CatalogNotSelected`] if that catalog no longer exists;
    /// [`OkfError::InvalidPath`] or another workspace error if the stored
    /// rows cannot form a workspace; [`OkfError::Database`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn load(db: &CatalogDb, row: &CatalogRow) -> Result<Self, OkfError> {
        let loaded = build(db, row.id, &row.name).await?;
        Ok(Self {
            id: row.id,
            name: row.name.clone(),
            store: row.store.clone(),
            current: RwLock::new(Arc::new(loaded)),
        })
    }

    /// The catalog's database id.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn id(&self) -> i64 {
        self.id
    }

    /// The store holding the catalog's data.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn store(&self) -> &StoreName {
        &self.store
    }

    /// The catalog's name.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The current loaded state. Cheap: it clones an `Arc` (bumps a
    /// reference count), and the caller may keep it as long as it likes.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn current(&self) -> Arc<Loaded> {
        // `read()` fails only if a thread panicked while holding the lock
        // ("poisoned"); nothing here can panic with it held.
        Arc::clone(&self.current.read().expect("catalog state lock poisoned"))
    }

    /// Rebuilds and swaps in the snapshot if the catalog changed since it was
    /// loaded.
    ///
    /// Concurrent refreshes of one catalog are safe and build at most once
    /// per change: a refresh that waited for another's rebuild returns `false`
    /// if that rebuild already reached the generation it saw. Rebuilds of one
    /// catalog run one at a time under its key in `rebuilds`, which every
    /// state of the catalog shares. The lock is taken before the build's
    /// pooled connection, so a caller already holding a connection (a
    /// publish) must not wait for it: see `refresh_locked`.
    ///
    /// # Arguments
    /// * `db` - the catalog's store.
    /// * `rebuilds` - the rebuild locks, by catalog id (`App::rebuild_locks`).
    ///
    /// # Returns
    /// Whether a new snapshot was swapped in.
    ///
    /// # Errors
    /// [`OkfError::CatalogNotSelected`] if the catalog was deleted;
    /// [`OkfError::Database`]; a workspace error as for [`CatalogState::load`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn refresh(&self, db: &CatalogDb, rebuilds: &KeyedLocks) -> Result<bool, OkfError> {
        let generation = self.generation_in(db.executor()).await?;
        self.refresh_to(rebuilds, generation, || build(db, self.id, &self.name))
            .await
    }

    /// The current loaded state, refreshed first if it is older than a
    /// changeset's entries: when any entry's base revision is newer than the
    /// snapshot's revision of its path (or the snapshot lacks the path). An
    /// entry's base is the revision it was made on, possibly on another
    /// replica whose snapshot had already moved on, so a snapshot behind it
    /// is simply out of date (issue #659). An entry with no base (a create)
    /// never triggers a refresh.
    ///
    /// The refresh is [`CatalogState::refresh`]: a read of the generation,
    /// and a rebuild only if it moved.
    ///
    /// # Arguments
    /// * `db` - the catalog's store.
    /// * `rebuilds` - the rebuild locks, by catalog id (`App::rebuild_locks`).
    /// * `bases` - each entry's normalized path and base revision.
    ///
    /// # Returns
    /// The loaded state to diff or overlay against.
    ///
    /// # Errors
    /// As [`CatalogState::refresh`].
    // `impl IntoIterator<Item = ...>` takes any sequence of pairs (a `Vec`,
    // or a lazy `map` over the entries) without collecting it first.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn current_covering<'a>(
        &self,
        db: &CatalogDb,
        rebuilds: &KeyedLocks,
        bases: impl IntoIterator<Item = (&'a str, Option<i32>)>,
    ) -> Result<Arc<Loaded>, OkfError> {
        let loaded = self.current();
        let behind = bases.into_iter().any(|(path, base)| match base {
            // `is_none_or`: a path missing from the snapshot counts as
            // behind too; the refresh then finds out whether it was deleted.
            Some(base) => loaded.revision(path).is_none_or(|current| current < base),
            None => false,
        });
        if !behind {
            return Ok(loaded);
        }
        self.refresh(db, rebuilds).await?;
        Ok(self.current())
    }

    /// [`CatalogState::refresh`] on the caller's connection, for a caller
    /// that holds the catalog's advisory lock ([`crate::store::catalogs::lock`]) in its
    /// transaction, so no import or publish can change the catalog while
    /// this reads it: a publish refreshing a stale snapshot needs no second
    /// pooled connection.
    ///
    /// It never takes the rebuild lock (issue #671). Its caller holds a
    /// pooled connection, and a refresh that holds the rebuild lock waits for
    /// a connection of its own (`build`), so waiting for the lock here could
    /// stall both until the pool's acquire timeout. The caller's own write is
    /// still visible when this returns: it builds the generation it read on
    /// its connection and swaps it in unless a newer one is already there
    /// ([`CatalogState::swap_if_newer`]).
    ///
    /// # Arguments
    /// * `conn` - the lock holder's transaction (`&mut tx`).
    ///
    /// # Returns
    /// Whether a new snapshot was swapped in.
    ///
    /// # Errors
    /// As [`CatalogState::refresh`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) async fn refresh_locked(&self, conn: &mut PgConnection) -> Result<bool, OkfError> {
        let generation = self.generation_in(&mut *conn).await?;
        if generation <= self.current().generation {
            return Ok(false);
        }
        let rows = documents::published(&mut *conn, self.id).await?;
        let loaded = assemble(generation, rows).await?;
        Ok(self.swap_if_newer(loaded))
    }

    /// Swaps `loaded` in unless the current snapshot is already as new.
    /// Generations only grow, so a snapshot never moves back, whichever
    /// builder finishes first.
    ///
    /// # Returns
    /// Whether `loaded` was swapped in.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    fn swap_if_newer(&self, loaded: Loaded) -> bool {
        // A statement-long write lock with no `.await` inside: the `std`
        // lock is never held across one.
        let mut current = self.current.write().expect("catalog state lock poisoned");
        if loaded.generation <= current.generation {
            return false;
        }
        *current = Arc::new(loaded);
        true
    }

    /// The catalog's generation in the database.
    ///
    /// # Errors
    /// [`OkfError::CatalogNotSelected`] if the catalog was deleted;
    /// [`OkfError::Database`].
    #[tracing::instrument(level = "debug", skip_all)]
    async fn generation_in(&self, executor: impl PgExecutor<'_>) -> Result<i64, OkfError> {
        let generation: Option<i64> = sqlx::query_scalar(GENERATION)
            .bind(self.id)
            .fetch_optional(executor)
            .await?;
        // `ok_or_else` turns `None` into the error, built only when needed.
        generation.ok_or_else(|| OkfError::CatalogNotSelected {
            catalog: self.name.clone(),
        })
    }

    /// Swaps in the snapshot `load` builds, unless the current one already
    /// reached `generation` (before or after waiting for another rebuild).
    ///
    /// # Arguments
    /// * `rebuilds` - the rebuild locks; this catalog's id is held for the
    ///   rebuild.
    /// * `generation` - the database's generation, as just read.
    /// * `load` - builds the snapshot; called at most once. It may wait for
    ///   a pooled connection while the lock is held, which is why a caller
    ///   already holding a connection must not take the lock
    ///   ([`CatalogState::refresh_locked`], issue #671).
    ///
    /// # Returns
    /// Whether a new snapshot was swapped in.
    ///
    /// # Errors
    /// `load`'s error.
    // `F: FnOnce() -> Fut` with `Fut: Future`: `load` is an async closure,
    // like passing a coroutine function in Python, called only if needed.
    #[tracing::instrument(level = "debug", skip_all)]
    async fn refresh_to<F, Fut>(
        &self,
        rebuilds: &KeyedLocks,
        generation: i64,
        load: F,
    ) -> Result<bool, OkfError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Loaded, OkfError>>,
    {
        // Generations only grow, so `<=` means "already this new or newer".
        if generation <= self.current().generation {
            return Ok(false);
        }
        // The guard (`_` prefix: kept only for its lifetime) releases the
        // lock when it goes out of scope at the end of the function. Keyed
        // by id, not held in `self`: another state of this catalog (an
        // evicted entry still in use, and its reload) waits here too.
        let _rebuilding = rebuilds.lock(self.id).await;
        // Another refresh may have swapped in this generation while we waited.
        if generation <= self.current().generation {
            return Ok(false);
        }
        // A publish's `refresh_locked` swaps without this lock, so a newer
        // snapshot may land during the build: `swap_if_newer` keeps it.
        let loaded = load().await?;
        Ok(self.swap_if_newer(loaded))
    }
}

/// Reads a catalog's generation and live documents consistently, then builds
/// the snapshot on a blocking thread.
///
/// # Errors
/// [`OkfError::CatalogNotSelected`] (naming `name`) if the row is gone;
/// [`OkfError::Database`]; a workspace or snapshot error.
#[tracing::instrument(level = "debug", skip_all)]
async fn build(db: &CatalogDb, catalog_id: i64, name: &str) -> Result<Loaded, OkfError> {
    // A repeatable-read transaction sees one moment of the database, so the
    // generation and the rows agree even if an import commits meanwhile.
    // `begin_with` sets the isolation level in the `BEGIN` itself.
    let mut tx = db
        .begin_with("BEGIN ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .await?;
    // `&mut *tx` reborrows the transaction as a connection for one query.
    let generation: Option<i64> = sqlx::query_scalar(GENERATION)
        .bind(catalog_id)
        .fetch_optional(&mut *tx)
        .await?;
    // A catalog deleted between `find` (or `refresh`'s check) and here.
    let Some(generation) = generation else {
        return Err(OkfError::CatalogNotSelected {
            catalog: name.to_string(),
        });
    };
    let rows = documents::published(&mut *tx, catalog_id).await?;
    tx.commit().await?;
    assemble(generation, rows).await
}

/// Builds the snapshot of a catalog's live documents at `generation`, on a
/// blocking thread.
///
/// # Errors
/// A workspace or snapshot error.
#[tracing::instrument(level = "debug", skip_all)]
async fn assemble(generation: i64, rows: Vec<DocumentRow>) -> Result<Loaded, OkfError> {
    // Before `rows` moves into the closure below: it borrows the rows.
    let commit_times = documents::commit_times(&rows);
    let files = rows.len();
    let revisions: HashMap<String, i32> = rows
        .iter()
        .map(|row| (row.path.clone(), row.revision))
        .collect();
    // `usize` to `u64` never loses anything on the 64-bit targets this
    // builds for; `try_from` says so without a silent cast.
    let stored_bytes = rows
        .iter()
        .map(|row| u64::try_from(row.content.len()).unwrap_or(u64::MAX))
        .fold(0_u64, u64::saturating_add);
    // `max()` over an empty iterator is `None`, so an empty catalog has no
    // last change.
    let last_change = rows.iter().map(|row| row.updated_at).max();
    // Building a snapshot is CPU work (rayon across documents): run it on a
    // blocking thread so the async runtime's threads stay free. `move` hands
    // `rows` to the closure. The workspace comes back with the snapshot, so
    // overlays can copy it.
    let (workspace, snapshot) = tokio::task::spawn_blocking(move || {
        let files = rows.into_iter().map(|row| {
            let file = StoredFile {
                content: row.content,
                executable: row.executable,
            };
            (row.path, file)
        });
        let workspace = MemWorkspace::from_stored(files)?;
        let snapshot = Snapshot::build(&workspace)?;
        // `Ok::<_, OkfError>`: the closure's error type, which `?` above
        // cannot infer on its own.
        Ok::<_, OkfError>((workspace, snapshot))
    })
    .await
    // A panic in the blocking task is re-raised here, as if it ran inline.
    .unwrap_or_else(|error| std::panic::resume_unwind(error.into_panic()))?;
    Ok(Loaded {
        generation,
        snapshot,
        commit_times,
        files,
        last_change,
        workspace,
        revisions,
        stored_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::Files;
    use crate::db::test_db;
    use crate::okf::workspace::Workspace;
    use crate::store::catalogs;
    use crate::store::import::{import, ImportOptions, ImportScope};
    use chrono::{TimeDelta, TimeZone};
    use sqlx::PgPool;

    fn files(entries: &[(&str, &[u8])]) -> Files {
        entries
            .iter()
            .map(|(path, content)| {
                let file = StoredFile {
                    content: content.to_vec(),
                    executable: false,
                };
                (path.to_string(), file)
            })
            .collect()
    }

    /// The directory row of catalog `name`, as the catalog cache passes it.
    async fn row(pool: &PgPool, name: &str) -> CatalogRow {
        catalogs::row_by_name(pool, name)
            .await
            .unwrap()
            .expect("the catalog exists")
    }

    async fn import_into(pool: &PgPool, entries: &[(&str, &[u8])], replace: bool) {
        let options = ImportOptions {
            replace,
            scope: ImportScope::Everything,
            expect_generation: None,
            expect_catalog_id: None,
            actor: "t",
            audit: &crate::store::audit::AuditActor::cli(),
            now: Utc::now(),
        };
        import(
            &crate::stores::CatalogDb::main_of(pool),
            "pg",
            files(entries),
            &options,
        )
        .await
        .unwrap();
    }

    /// A refresh and a publish's refresh (`refresh_locked`) racing on a full
    /// pool must both finish at once (issue #671). The publish holds one
    /// connection, the test holds the other, and the refresh (a request's or
    /// an announcement's) holds the rebuild lock and is queued for a
    /// connection. The publish's refresh used to wait for that lock, whose
    /// holder waited for the publish's connection, until the acquire timeout.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_refresh_waiting_for_a_connection_does_not_hold_up_a_publish_refresh() {
        use sqlx::postgres::PgPoolOptions;
        use std::time::Duration;

        // Arrange: a state one generation behind, and a pool of two whose
        // acquire timeout is long next to the deadline below but short
        // enough that a regression fails the test quickly.
        let db = test_db().await;
        catalogs::create(
            &db.pool,
            &crate::store::catalogs::NewCatalog {
                name: "pg",
                display_name: &crate::labels::DisplayName::default(),
                description: "",
                owner_team: crate::db::TEST_TEAM,
            },
            &crate::store::audit::AuditActor::cli(),
            Utc::now(),
        )
        .await
        .unwrap();
        let state = Arc::new(
            CatalogState::load(
                &crate::stores::CatalogDb::main_of(&db.pool),
                &row(&db.pool, "pg").await,
            )
            .await
            .unwrap(),
        );
        import_into(&db.pool, &[("a.md", b"# A\n")], false).await;
        let behind = state.current().generation();
        let small = PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(Duration::from_secs(8))
            // A store's pool: its connections see only schema `catalog`.
            .after_connect(|connection, _meta| {
                Box::pin(async move {
                    sqlx::query("SET search_path = catalog")
                        .execute(connection)
                        .await
                        .map(|_| ())
                })
            })
            .connect_with((*crate::db::store_pool(&db.pool).connect_options()).clone())
            .await
            .unwrap();
        let rebuilds = Arc::new(KeyedLocks::new());
        let mut publish_conn = small.acquire().await.unwrap();
        let other = small.acquire().await.unwrap();
        let generation = behind + 1;

        // Act: the refresh takes the rebuild lock (`refresh_to`, called with
        // its own load step so the test can signal), tells the test, and only
        // then asks the pool for a connection, which has none free. The
        // publish refreshes on the connection it holds once the lock is held.
        let (locked_tx, locked_rx) = tokio::sync::oneshot::channel();
        let refresher = {
            let (state, small, rebuilds) =
                (Arc::clone(&state), small.clone(), Arc::clone(&rebuilds));
            let db_pool = db.pool.clone();
            tokio::spawn(async move {
                let (id, name) = (state.id(), state.name().to_string());
                state
                    .refresh_to(&rebuilds, generation, || async {
                        // Runs under the lock, before the build's connection.
                        locked_tx.send(()).expect("the test waits for the signal");
                        build(
                            &crate::stores::CatalogDb::new(
                                crate::stores::StoreName::main(),
                                small,
                                crate::stores::AccountDb::of(&db_pool),
                            ),
                            id,
                            &name,
                        )
                        .await
                    })
                    .await
            })
        };
        tokio::time::timeout(Duration::from_secs(4), locked_rx)
            .await
            .expect("the refresh takes the rebuild lock")
            .expect("the refresh signals");
        assert!(
            !refresher.is_finished(),
            "the refresh waits for a connection of the exhausted pool"
        );
        let published = tokio::time::timeout(
            Duration::from_secs(4),
            state.refresh_locked(&mut publish_conn),
        )
        .await;
        drop(publish_conn);
        drop(other);
        let refreshed = tokio::time::timeout(Duration::from_secs(4), refresher).await;

        // Assert: the publish did not wait out the acquire timeout, and the
        // queued refresh finished once a connection came free.
        let swapped = published
            .expect("the publish's refresh is not held up by the queued refresh")
            .unwrap();
        assert!(
            swapped,
            "the publish's refresh swapped in the new generation"
        );
        refreshed
            .expect("the queued refresh finishes once a connection is free")
            .unwrap()
            .unwrap();
        assert_eq!(state.current().generation(), generation);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_swap_never_moves_the_snapshot_back() {
        // Arrange: a state at generation 2, and a build of generation 1.
        let db = test_db().await;
        catalogs::create(
            &db.pool,
            &crate::store::catalogs::NewCatalog {
                name: "pg",
                display_name: &crate::labels::DisplayName::default(),
                description: "",
                owner_team: crate::db::TEST_TEAM,
            },
            &crate::store::audit::AuditActor::cli(),
            Utc::now(),
        )
        .await
        .unwrap();
        let state = CatalogState::load(
            &crate::stores::CatalogDb::main_of(&db.pool),
            &row(&db.pool, "pg").await,
        )
        .await
        .unwrap();
        let older = build(
            &crate::stores::CatalogDb::main_of(&db.pool),
            state.id(),
            "pg",
        )
        .await
        .unwrap();
        import_into(&db.pool, &[("a.md", b"# A\n")], false).await;
        let mut conn = db.store().acquire().await.unwrap();
        assert!(state.refresh_locked(&mut conn).await.unwrap());
        let newer = state.current().generation();

        // Act: an older build finishes after the newer one was swapped in.
        let swapped = state.swap_if_newer(older);

        // Assert
        assert!(!swapped);
        assert_eq!(state.current().generation(), newer);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn load_builds_the_snapshot_of_the_live_documents() {
        let db = test_db().await;
        catalogs::create(
            &db.pool,
            &crate::store::catalogs::NewCatalog {
                name: "pg",
                display_name: &crate::labels::DisplayName::default(),
                description: "",
                owner_team: crate::db::TEST_TEAM,
            },
            &crate::store::audit::AuditActor::cli(),
            Utc::now(),
        )
        .await
        .unwrap();
        // A plain link: `okf.py` does not count an image embed (`![..](..)`) as a link.
        let entries: &[(&str, &[u8])] = &[
            ("a.md", b"---\ntype: Note\n---\n[logo](logo.png)\n"),
            ("logo.png", b"\x89PNG\0"),
        ];
        import_into(&db.pool, entries, false).await;

        let state = CatalogState::load(
            &crate::stores::CatalogDb::main_of(&db.pool),
            &row(&db.pool, "pg").await,
        )
        .await
        .unwrap();

        let loaded = state.current();
        assert_eq!((state.name(), loaded.generation()), ("pg", 1));
        assert_eq!(loaded.snapshot().docs().len(), 1);
        // The image is in the workspace, so the link to it resolves.
        assert!(loaded.snapshot().get("a.md").unwrap().links()[0].target_exists);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn load_records_the_file_count_size_and_last_change() {
        let db = test_db().await;
        let rebuilds = KeyedLocks::new();
        let created = Utc.with_ymd_and_hms(2026, 11, 1, 12, 0, 0).unwrap();
        catalogs::create(
            &db.pool,
            &crate::store::catalogs::NewCatalog {
                name: "pg",
                display_name: &crate::labels::DisplayName::default(),
                description: "pgrace's world",
                owner_team: crate::db::TEST_TEAM,
            },
            &crate::store::audit::AuditActor::cli(),
            created,
        )
        .await
        .unwrap();
        let empty = CatalogState::load(
            &crate::stores::CatalogDb::main_of(&db.pool),
            &row(&db.pool, "pg").await,
        )
        .await
        .unwrap();
        // A new catalog holds its starter files (index.md and log.md).
        assert_eq!(empty.current().files(), crate::okf::ops::STARTER_FILE_COUNT);
        assert_eq!(empty.current().last_change(), Some(created));
        let imported = Utc.with_ymd_and_hms(2026, 11, 2, 12, 0, 0).unwrap();
        let options = ImportOptions {
            replace: false,
            scope: ImportScope::Everything,
            expect_generation: None,
            expect_catalog_id: None,
            actor: "t",
            audit: &crate::store::audit::AuditActor::cli(),
            now: imported,
        };
        let entries = files(&[("a.md", b"# A\n"), ("b.txt", b"b\n")]);
        import(
            &crate::stores::CatalogDb::main_in(&db.pool),
            "pg",
            entries,
            &options,
        )
        .await
        .unwrap();

        empty
            .refresh(&crate::stores::CatalogDb::main_of(&db.pool), &rebuilds)
            .await
            .unwrap();

        let loaded = empty.current();
        assert_eq!(loaded.files(), 2);
        // `# A\n` and `b\n`: the stored bytes the cache weighs by.
        assert_eq!(loaded.stored_bytes(), 6);
        assert_eq!(loaded.last_change(), Some(imported));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn loaded_keeps_the_workspace_and_each_path_s_revision() {
        let db = test_db().await;
        catalogs::create(
            &db.pool,
            &crate::store::catalogs::NewCatalog {
                name: "pg",
                display_name: &crate::labels::DisplayName::default(),
                description: "",
                owner_team: crate::db::TEST_TEAM,
            },
            &crate::store::audit::AuditActor::cli(),
            Utc::now(),
        )
        .await
        .unwrap();
        // Typed, so the byte strings of different lengths share one type.
        let first: &[(&str, &[u8])] = &[("a.md", b"# A\n"), ("logo.png", b"\x89PNG\0")];
        let second: &[(&str, &[u8])] = &[("a.md", b"# A2\n"), ("logo.png", b"\x89PNG\0")];
        import_into(&db.pool, first, false).await;
        import_into(&db.pool, second, true).await;

        let loaded = CatalogState::load(
            &crate::stores::CatalogDb::main_of(&db.pool),
            &row(&db.pool, "pg").await,
        )
        .await
        .unwrap()
        .current();

        assert_eq!(loaded.workspace().read_to_string("a.md").unwrap(), "# A2\n");
        assert!(loaded.workspace().is_file("logo.png"));
        assert_eq!(loaded.revision("a.md"), Some(2));
        assert_eq!(loaded.revision("logo.png"), Some(1));
        assert_eq!(loaded.revision("gone.md"), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn load_refuses_a_row_whose_catalog_was_deleted() {
        let db = test_db().await;
        catalogs::create(
            &db.pool,
            &crate::store::catalogs::NewCatalog {
                name: "pg",
                display_name: &crate::labels::DisplayName::default(),
                description: "",
                owner_team: crate::db::TEST_TEAM,
            },
            &crate::store::audit::AuditActor::cli(),
            Utc::now(),
        )
        .await
        .unwrap();
        let stale = row(&db.pool, "pg").await;
        catalogs::delete_settled(
            &db.pool,
            "pg",
            false,
            &crate::store::audit::AuditActor::cli(),
            Utc::now(),
        )
        .await
        .unwrap();

        let result = CatalogState::load(&crate::stores::CatalogDb::main_of(&db.pool), &stale).await;

        assert!(matches!(result, Err(OkfError::CatalogNotSelected { .. })));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn refresh_swaps_in_a_new_snapshot_only_when_the_catalog_changed() {
        let db = test_db().await;
        let rebuilds = KeyedLocks::new();
        catalogs::create(
            &db.pool,
            &crate::store::catalogs::NewCatalog {
                name: "pg",
                display_name: &crate::labels::DisplayName::default(),
                description: "",
                owner_team: crate::db::TEST_TEAM,
            },
            &crate::store::audit::AuditActor::cli(),
            Utc::now(),
        )
        .await
        .unwrap();
        import_into(&db.pool, &[("a.md", b"# Old\n")], false).await;
        let state = CatalogState::load(
            &crate::stores::CatalogDb::main_of(&db.pool),
            &row(&db.pool, "pg").await,
        )
        .await
        .unwrap();
        let before = state.current();

        assert!(
            !state
                .refresh(&crate::stores::CatalogDb::main_of(&db.pool), &rebuilds)
                .await
                .unwrap(),
            "nothing changed yet"
        );
        import_into(&db.pool, &[("a.md", b"# New\n")], true).await;
        assert!(state
            .refresh(&crate::stores::CatalogDb::main_of(&db.pool), &rebuilds)
            .await
            .unwrap());

        let after = state.current();
        assert_eq!(after.generation(), 2);
        assert_eq!(after.snapshot().get("a.md").unwrap().text(), "# New\n");
        // A reader that took the old snapshot still sees it, whole.
        assert_eq!(before.snapshot().get("a.md").unwrap().text(), "# Old\n");
        assert!(!state
            .refresh(&crate::stores::CatalogDb::main_of(&db.pool), &rebuilds)
            .await
            .unwrap());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_refreshes_build_and_swap_once() {
        let db = test_db().await;
        let rebuilds = KeyedLocks::new();
        catalogs::create(
            &db.pool,
            &crate::store::catalogs::NewCatalog {
                name: "pg",
                display_name: &crate::labels::DisplayName::default(),
                description: "",
                owner_team: crate::db::TEST_TEAM,
            },
            &crate::store::audit::AuditActor::cli(),
            Utc::now(),
        )
        .await
        .unwrap();
        let state = CatalogState::load(
            &crate::stores::CatalogDb::main_of(&db.pool),
            &row(&db.pool, "pg").await,
        )
        .await
        .unwrap();
        import_into(&db.pool, &[("a.md", b"# New\n")], false).await;

        // `tokio::join!` runs both futures at once on this task.
        let store = crate::stores::CatalogDb::main_of(&db.pool);
        let (first, second) = tokio::join!(
            state.refresh(&store, &rebuilds),
            state.refresh(&store, &rebuilds)
        );

        assert_eq!(
            [first.unwrap(), second.unwrap()]
                .iter()
                .filter(|&&swapped| swapped)
                .count(),
            1
        );
        assert_eq!(state.current().generation(), 1);
    }

    /// Two states of one catalog (an evicted entry still in use and its
    /// reload, M18 Decision 4) rebuild under one lock: a refresh of either
    /// waits while the catalog's rebuild lock is held.
    #[tokio::test(flavor = "multi_thread")]
    async fn refreshes_of_two_states_share_the_rebuild_lock() {
        use std::time::Duration;

        let db = test_db().await;
        let rebuilds = Arc::new(KeyedLocks::new());
        catalogs::create(
            &db.pool,
            &crate::store::catalogs::NewCatalog {
                name: "pg",
                display_name: &crate::labels::DisplayName::default(),
                description: "",
                owner_team: crate::db::TEST_TEAM,
            },
            &crate::store::audit::AuditActor::cli(),
            Utc::now(),
        )
        .await
        .unwrap();
        let first = CatalogState::load(
            &crate::stores::CatalogDb::main_of(&db.pool),
            &row(&db.pool, "pg").await,
        )
        .await
        .unwrap();
        let second = Arc::new(
            CatalogState::load(
                &crate::stores::CatalogDb::main_of(&db.pool),
                &row(&db.pool, "pg").await,
            )
            .await
            .unwrap(),
        );
        import_into(&db.pool, &[("a.md", b"# New\n")], false).await;
        // Held as `first`'s rebuild would hold it.
        let held = rebuilds.lock(first.id()).await;

        let refreshing = {
            let (pool, rebuilds, second) =
                (db.pool.clone(), Arc::clone(&rebuilds), Arc::clone(&second));
            tokio::spawn(async move {
                second
                    .refresh(&crate::stores::CatalogDb::main_of(&pool), &rebuilds)
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(500)).await;
        let waited = !refreshing.is_finished();
        drop(held);
        let swapped = tokio::time::timeout(Duration::from_secs(30), refreshing)
            .await
            .expect("the refresh finishes once the lock is released")
            .unwrap()
            .unwrap();

        assert!(waited, "the second state's refresh waited for the lock");
        assert!(swapped);
        assert_eq!(second.current().generation(), 1);
        assert_eq!(first.current().generation(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_deleted_catalog_cannot_be_refreshed() {
        let db = test_db().await;
        let rebuilds = KeyedLocks::new();
        let catalog = catalogs::create(
            &db.pool,
            &crate::store::catalogs::NewCatalog {
                name: "pg",
                display_name: &crate::labels::DisplayName::default(),
                description: "",
                owner_team: crate::db::TEST_TEAM,
            },
            &crate::store::audit::AuditActor::cli(),
            Utc::now(),
        )
        .await
        .unwrap();
        let state = CatalogState::load(
            &crate::stores::CatalogDb::main_of(&db.pool),
            &row(&db.pool, "pg").await,
        )
        .await
        .unwrap();
        catalogs::delete_settled(
            &db.pool,
            "pg",
            false,
            &crate::store::audit::AuditActor::cli(),
            chrono::Utc::now(),
        )
        .await
        .unwrap();

        let result = state
            .refresh(&crate::stores::CatalogDb::main_of(&db.pool), &rebuilds)
            .await;

        assert_eq!(state.id(), catalog.id);
        assert!(matches!(result, Err(OkfError::CatalogNotSelected { .. })));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn build_names_a_catalog_deleted_before_it_read() {
        let db = test_db().await;
        let catalog = catalogs::create(
            &db.pool,
            &crate::store::catalogs::NewCatalog {
                name: "pg",
                display_name: &crate::labels::DisplayName::default(),
                description: "",
                owner_team: crate::db::TEST_TEAM,
            },
            &crate::store::audit::AuditActor::cli(),
            Utc::now(),
        )
        .await
        .unwrap();
        catalogs::delete_settled(
            &db.pool,
            "pg",
            false,
            &crate::store::audit::AuditActor::cli(),
            chrono::Utc::now(),
        )
        .await
        .unwrap();

        let result = build(
            &crate::stores::CatalogDb::main_of(&db.pool),
            catalog.id,
            "pg",
        )
        .await;

        assert!(matches!(
            result,
            Err(OkfError::CatalogNotSelected { catalog }) if catalog == "pg"
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn lint_flags_a_stamp_later_than_the_documents_last_revision() {
        let db = test_db().await;
        let imported = Utc.with_ymd_and_hms(2026, 11, 2, 12, 0, 0).unwrap();
        catalogs::create(
            &db.pool,
            &crate::store::catalogs::NewCatalog {
                name: "pg",
                display_name: &crate::labels::DisplayName::default(),
                description: "",
                owner_team: crate::db::TEST_TEAM,
            },
            &crate::store::audit::AuditActor::cli(),
            imported,
        )
        .await
        .unwrap();
        let later = "---\ntype: Note\ngenerated: { by: t, at: 2026-11-02T12:10:00Z }\n---\nBody\n";
        let earlier =
            "---\ntype: Note\ngenerated: { by: t, at: 2026-11-02T11:00:00Z }\n---\nBody\n";
        let options = ImportOptions {
            replace: false,
            scope: ImportScope::Everything,
            expect_generation: None,
            expect_catalog_id: None,
            actor: "t",
            audit: &crate::store::audit::AuditActor::cli(),
            now: imported,
        };
        let entries = files(&[
            ("later.md", later.as_bytes()),
            ("earlier.md", earlier.as_bytes()),
        ]);
        import(
            &crate::stores::CatalogDb::main_in(&db.pool),
            "pg",
            entries,
            &options,
        )
        .await
        .unwrap();
        let state = CatalogState::load(
            &crate::stores::CatalogDb::main_of(&db.pool),
            &row(&db.pool, "pg").await,
        )
        .await
        .unwrap();

        let report = state.current().lint(imported + TimeDelta::minutes(30));

        let flagged: Vec<&str> = report
            .warnings
            .iter()
            .filter(|finding| finding.rule == "generated-after-commit")
            .map(|finding| finding.file.as_str())
            .collect();
        assert_eq!(flagged, ["later.md"]);
    }
}
