//! Overlays (M5 Decision 6): a connection's view of a catalog with its open
//! changeset applied, so its reads see its own pending edits.
//!
//! An overlay is the published workspace with the changeset's entries
//! applied and a full snapshot of the result (about 10 ms for the real
//! catalog), built on a blocking thread. [`Overlays`] caches one per
//! changeset, valid while the changeset's `version` and the published
//! `generation` stay the same; no incremental snapshot. The cache is
//! bounded in estimated bytes (`overlay_cache_bytes`, weighed as a catalog
//! snapshot is, over the overlay's own files) and drops an overlay unused
//! for `session_idle_timeout`; it builds each changeset's overlay once
//! however many reads ask at once.

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgExecutor};

use crate::cache::catalogs::weight;
use crate::cache::{Bounded, CacheName, Limit};
use crate::catalog::{CatalogState, Loaded};
use crate::error::OkfError;
use crate::keyed_lock::KeyedLocks;
use crate::mcp::blocking;
use crate::okf::analysis::lint::{lint, CommitTimes, LintReport};
use crate::okf::snapshot::Snapshot;
use crate::okf::workspace::{MemWorkspace, StoredFile};
use crate::store::changesets::{self, ChangesetKey, ChangesetRow, EntryRow};
use crate::stores::CatalogDb;
use crate::telemetry::metrics::Metrics;

/// A catalog's published state with one changeset's entries applied.
/// Immutable.
#[derive(Debug)]
pub struct Overlay {
    base: Arc<Loaded>,
    key: ChangesetKey,
    version: i64,
    workspace: MemWorkspace,
    snapshot: Snapshot,
    dirty: BTreeSet<String>,
    // The base's commit times with `dirty` marked uncommitted, made once.
    commit_times: CommitTimes,
    // Whether `base` is older than some entry's base revision (an entry made
    // on a replica whose snapshot had moved on, issue #659). A write's
    // `get_in` may cache such an overlay, since it cannot refresh on its
    // connection; a read never reuses it (`Overlays::get`).
    behind_entries: bool,
    // What it costs in the cache, computed once at build.
    weight: u64,
}

impl Overlay {
    /// The snapshot of the overlaid files.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial field accessor: returns a field or a borrow of one"
        )
    )]
    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    /// The overlaid files (what a write op reads, through a copy).
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial field accessor: returns a field or a borrow of one"
        )
    )]
    pub fn workspace(&self) -> &MemWorkspace {
        &self.workspace
    }

    /// The published state it was built on.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial field accessor: returns a field or a borrow of one"
        )
    )]
    pub fn base(&self) -> &Arc<Loaded> {
        &self.base
    }

    /// The changeset's paths, sorted (deleted ones included).
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial field accessor: returns a field or a borrow of one"
        )
    )]
    pub fn dirty(&self) -> &BTreeSet<String> {
        &self.dirty
    }

    /// The changeset it shows.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial field accessor: returns a field or a borrow of one"
        )
    )]
    pub fn changeset_id(&self) -> i64 {
        self.key.id
    }

    /// The changeset `version` it was built at.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial field accessor: returns a field or a borrow of one"
        )
    )]
    pub fn version(&self) -> i64 {
        self.version
    }

    /// Lints the overlaid snapshot, the changeset's paths counted as
    /// uncommitted (`lint pending`).
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
            reason = "one-line delegation to an instrumented function"
        )
    )]
    pub fn lint(&self, now: DateTime<Utc>) -> LintReport {
        lint(&self.snapshot, now, Some(&self.commit_times))
    }
}

/// Builds an overlay on a blocking thread.
///
/// # Arguments
/// * `base` - the published state.
/// * `changeset` - the changeset (its id and `version` are recorded).
/// * `entries` - its entries; an entry with `content: None` deletes its path.
///
/// # Returns
/// The overlay.
///
/// # Errors
/// [`OkfError::InvalidPath`] if the entries do not form a tree with the
/// published files, or if the snapshot cannot read a pending file (a `.md`
/// that is not UTF-8), its reason naming the changeset (every read through
/// this overlay fails until the changeset is fixed, so the error says where
/// to look); any other snapshot error, as for a published catalog.
#[tracing::instrument(level = "debug", skip_all)]
pub async fn build_overlay(
    base: Arc<Loaded>,
    changeset: &ChangesetRow,
    entries: Vec<EntryRow>,
) -> Result<Overlay, OkfError> {
    let key = changeset.key();
    let version = changeset.version;
    // Owned, so the blocking closure can take it: errors name the handle,
    // which is what a client knows the changeset by (M8 Decision 1).
    let handle = changeset.handle.clone();
    // Before `entries` moves: whether any entry was made on a newer revision
    // than `base` holds (or on a path `base` lacks), as
    // `CatalogState::current_covering` decides it.
    let behind_entries = entries.iter().any(|entry| {
        entry
            .base_revision
            .is_some_and(|wanted| base.revision(&entry.path).is_none_or(|have| have < wanted))
    });
    // `move` hands `base` and `entries` to the blocking thread, which must
    // own what it reads.
    blocking(move || {
        let dirty: BTreeSet<String> = entries.iter().map(|entry| entry.path.clone()).collect();
        let changes = entries.into_iter().map(|entry| {
            let executable = entry.executable;
            let file = entry.content.map(|content| StoredFile {
                content,
                executable,
            });
            (entry.path, file)
        });
        let workspace = base
            .workspace()
            .with_changes(changes)
            .map_err(|error| in_changeset(error, &handle))?;
        // The published files built into the base snapshot, so a failure
        // here comes from the pending entries (a `.md` whose bytes are not
        // UTF-8): named as theirs, like a clash.
        let snapshot = Snapshot::build(&workspace).map_err(|error| in_changeset(error, &handle))?;
        let commit_times = base.commit_times().with_dirty(dirty.iter().cloned());
        let weight = weight_of_workspace(&workspace);
        Ok(Overlay {
            base,
            key,
            version,
            workspace,
            snapshot,
            dirty,
            commit_times,
            behind_entries,
            weight,
        })
    })
    .await
}

/// Turns an error that applying a changeset's entries caused into an
/// [`OkfError::InvalidPath`] that names the changeset and how to recover, so
/// it is not masked as internal and a read can tell it from a database
/// failure ([`crate::mcp::context::is_broken_overlay`]).
///
/// # Arguments
/// * `error` - the error from applying the entries or building the
///   snapshot of the result.
/// * `handle` - the handle of the changeset whose entries were applied.
///
/// # Returns
/// An `InvalidPath` (a clash) or an `Io` (a pending file the snapshot cannot
/// read) as an `InvalidPath` with the changeset in its reason; any other
/// error as it is.
#[tracing::instrument(level = "trace", skip_all)]
fn in_changeset(error: OkfError, handle: &str) -> OkfError {
    // Each `format!` runs before the struct is built, because building it
    // moves `path` into the struct.
    match error {
        // The clash is reported at the file's path, which is either the
        // pending file or a published one with pending paths under it.
        OkfError::InvalidPath { path, reason } => {
            let reason = format!(
                "{reason}, in pending changeset {handle}; discard the clashing path \
                 ({path} or the paths under it) or the changeset"
            );
            OkfError::InvalidPath { path, reason }
        }
        OkfError::Io { path, source } => {
            let reason =
                format!("{source}, in pending changeset {handle}; discard {path} or the changeset");
            OkfError::InvalidPath { path, reason }
        }
        other => other,
    }
}

/// What an overlay was built at: the base `generation`, then the changeset
/// `version`. Tuples compare field by field, like Python's, so a later
/// generation always counts as newer.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial field accessor: returns a field or a borrow of one"
    )
)]
fn built_at(overlay: &Overlay) -> (i64, i64) {
    (overlay.base.generation(), overlay.version)
}

/// Whether a build may replace the overlay already cached for its changeset:
/// not when the cached one is newer (a slower build finishing last).
#[tracing::instrument(level = "trace", skip_all)]
fn supersedes(cached: &Arc<Overlay>, built: &Arc<Overlay>) -> bool {
    built_at(built) >= built_at(cached)
}

/// What an overlay costs in the cache: [`weight`] of its whole workspace (the
/// base's files copied plus the pending entries), counted as a snapshot's are
/// (an empty file is not free). The shared `Arc<Loaded>` base itself is not
/// charged again: the catalog cache weighs it.
///
/// # Arguments
/// * `workspace` - the overlaid files.
///
/// # Returns
/// The estimated resident bytes.
#[tracing::instrument(level = "trace", skip_all)]
fn weight_of_workspace(workspace: &MemWorkspace) -> u64 {
    let (files, stored_bytes) = workspace.totals();
    weight(stored_bytes, files)
}

/// How long an overlay may stay cached at most. Not the bound (the byte
/// budget and the idle time are): only a backstop for an entry that is read
/// often enough never to go idle.
const OVERLAY_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Every connection's overlay, by changeset ([`ChangesetKey`]: the catalog
/// and the id in its store), bounded in estimated bytes (spec Decision 5).
#[derive(Debug)]
pub struct Overlays {
    cached: Bounded<ChangesetKey, Arc<Overlay>>,
    // One build at a time per changeset: a read that misses waits for a
    // build already running, then finds its result cached.
    builds: KeyedLocks<ChangesetKey>,
}

impl Overlays {
    /// An empty cache whose entries are dropped after `idle` without use,
    /// holding at most `budget_bytes` of estimated memory.
    ///
    /// # Arguments
    /// * `idle` - `session_idle_timeout`.
    /// * `budget_bytes` - `overlay_cache_bytes`.
    /// * `metrics` - where the `okf.cache.*` series are recorded.
    ///
    /// # Returns
    /// The cache.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn new(idle: Duration, budget_bytes: u64, metrics: Metrics) -> Self {
        let cached = Bounded::new(CacheName::Overlay, Limit::Bytes(budget_bytes), OVERLAY_TTL)
            .with_weigher(|_id, overlay: &Arc<Overlay>| overlay.weight)
            .with_idle(idle)
            .with_metrics(metrics);
        Self {
            cached,
            builds: KeyedLocks::new(),
        }
    }

    /// The overlay of `changeset` on `state`'s current published state,
    /// for a read: the cached one if it was built at this `version` and
    /// `generation`, else a new one (its entries read now from `db`).
    /// Concurrent misses on one changeset build it once: the others wait,
    /// then find it cached.
    ///
    /// # Arguments
    /// * `db` - the catalog's store, where to read the entries.
    /// * `rebuilds` - the rebuild locks (`App::rebuild_locks`), for the
    ///   refresh of a snapshot behind the entries.
    /// * `state` - the changeset's catalog.
    /// * `changeset` - an open changeset, as just read.
    ///
    /// # Returns
    /// The overlay, shared.
    ///
    /// # Errors
    /// [`OkfError::Database`] reading the entries; as [`build_overlay`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn get(
        &self,
        db: &CatalogDb,
        rebuilds: &KeyedLocks,
        state: &CatalogState,
        changeset: &ChangesetRow,
    ) -> Result<Arc<Overlay>, OkfError> {
        // A cached overlay built on a snapshot behind its entries (a write's
        // `get_in` on a lagging replica caches one, even when the write is
        // then refused) is not reused: the build below refreshes first.
        let wanted = (state.current().generation(), changeset.version);
        let current =
            |overlay: &Arc<Overlay>| !overlay.behind_entries && built_at(overlay) == wanted;
        if let Some(overlay) = self.cached.get_if(&changeset.key(), current).await {
            return Ok(overlay);
        }
        // `_building` is held (and other reads of this changeset wait) until
        // this function returns. It is taken before a pooled connection, so
        // the builder may wait for one while holding it; that is why
        // `get_in`, whose caller already holds a connection, never waits on
        // it (it would close a cycle with a builder waiting for that
        // connection).
        let _building = self.builds.lock(changeset.key()).await;
        // Read the state again, without counting a second request: a build
        // that finished while this one waited may be the overlay wanted, and
        // the published state may have moved.
        if let Some(overlay) = self
            .lookup(changeset, state.current().generation())
            .await
            .filter(|overlay| !overlay.behind_entries)
        {
            return Ok(overlay);
        }
        // The build registers its fence before it starts, so a `forget` while
        // it runs (a write or discard) keeps its result out of the cache.
        self.cached
            .build(
                changeset.key(),
                || async {
                    let entries = changesets::entries(db.executor(), changeset.id).await?;
                    // Unlike `get_in`, a read may take a second connection:
                    // so before building, bring a snapshot that is behind
                    // the entries' bases up to date (an entry made on
                    // another replica, issue #659). Otherwise the paths the
                    // changeset does not touch would show older text than
                    // the entries were made on.
                    let bases = entries
                        .iter()
                        .map(|entry| (entry.path.as_str(), entry.base_revision));
                    let base = state.current_covering(db, rebuilds, bases).await?;
                    Ok(Arc::new(build_overlay(base, changeset, entries).await?))
                },
                supersedes,
            )
            .await
    }

    /// As [`Overlays::get`], for a write that holds a connection (its
    /// transaction, with the changeset's row locked): reads the entries on
    /// it and never waits for a read's build, so a full pool cannot stall
    /// the two on each other. A read building at the same time may build the
    /// same overlay too; `store` keeps the newer.
    ///
    /// # Arguments
    /// * `conn` - the write's transaction (`&mut *tx`), so a write needs no
    ///   second connection.
    /// * `state` - the changeset's catalog.
    /// * `changeset` - the changeset, as read under its row lock.
    ///
    /// # Returns
    /// The overlay, shared.
    ///
    /// # Errors
    /// [`OkfError::Database`] reading the entries; as [`build_overlay`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn get_in(
        &self,
        conn: &mut PgConnection,
        state: &CatalogState,
        changeset: &ChangesetRow,
    ) -> Result<Arc<Overlay>, OkfError> {
        self.lookup_or_build(conn, state, changeset).await
    }

    /// The cached overlay if current, else one built from entries read on
    /// `executor` and cached.
    // `impl PgExecutor<'_>` takes a `&PgPool` or a `&mut PgConnection`
    // alike, like a duck-typed "anything with execute()" argument in Python,
    // checked at compile time.
    #[tracing::instrument(level = "trace", skip_all)]
    async fn lookup_or_build(
        &self,
        executor: impl PgExecutor<'_>,
        state: &CatalogState,
        changeset: &ChangesetRow,
    ) -> Result<Arc<Overlay>, OkfError> {
        let base = state.current();
        let wanted = (base.generation(), changeset.version);
        let current = |overlay: &Arc<Overlay>| built_at(overlay) == wanted;
        if let Some(overlay) = self.cached.get_if(&changeset.key(), current).await {
            return Ok(overlay);
        }
        // Read after the caller read the row (a read resolves it unlocked in
        // `ToolContext::changeset`), so the entries may be newer than
        // `changeset.version`; harmless: the overlay is tagged with the
        // older version, and the next read at the newer one rebuilds it.
        self.cached
            .build(
                changeset.key(),
                || async {
                    let entries = changesets::entries(executor, changeset.id).await?;
                    Ok(Arc::new(build_overlay(base, changeset, entries).await?))
                },
                supersedes,
            )
            .await
    }

    /// Drops a changeset's overlay (after a write, publish or discard).
    ///
    /// # Arguments
    /// * `changeset` - the changeset, by [`ChangesetRow::key`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn forget(&self, changeset: ChangesetKey) {
        self.cached.invalidate(&changeset).await;
    }

    /// The cached overlay, if it is current; a read of it counts as a use
    /// (so it does not go idle).
    ///
    /// It drops this changeset's entry if it is older than the one asked for
    /// (each entry pins its base `Loaded`, so a stale one would keep an old
    /// generation in memory). A newer entry stays, for the callers that read
    /// the newer row.
    #[tracing::instrument(level = "trace", skip_all)]
    async fn lookup(&self, changeset: &ChangesetRow, generation: i64) -> Option<Arc<Overlay>> {
        let overlay = self.cached.peek(&changeset.key()).await?;
        let wanted = (generation, changeset.version);
        // `cmp` on tuples compares field by field, like Python's.
        match built_at(&overlay).cmp(&wanted) {
            Ordering::Equal => Some(overlay),
            Ordering::Less => {
                self.cached.invalidate(&changeset.key()).await;
                None
            }
            Ordering::Greater => None,
        }
    }

    /// How many overlays are cached, as the cache counts them.
    ///
    /// # Returns
    /// The entry count.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "reads one counter")
    )]
    pub fn entry_count(&self) -> u64 {
        self.cached.entry_count()
    }
}

/// What one tool call reads of one catalog.
// Both arms hold an `Arc`, so a `View` is cheap to move to a blocking thread.
#[derive(Debug, Clone)]
pub enum View {
    /// The published state: the call names no changeset in this catalog.
    Published(Arc<Loaded>),
    /// The named changeset's overlay.
    Pending(Arc<Overlay>),
}

impl View {
    /// The snapshot to read.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn snapshot(&self) -> &Snapshot {
        match self {
            View::Published(loaded) => loaded.snapshot(),
            View::Pending(overlay) => overlay.snapshot(),
        }
    }

    /// Lints what this view shows, with its commit times.
    ///
    /// # Arguments
    /// * `now` - the current time (server clock).
    ///
    /// # Returns
    /// The findings.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn lint(&self, now: DateTime<Utc>) -> LintReport {
        match self {
            View::Published(loaded) => loaded.lint(now),
            View::Pending(overlay) => overlay.lint(now),
        }
    }

    /// Does this view show pending edits?
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial field accessor: returns a field or a borrow of one"
        )
    )]
    pub fn pending(&self) -> bool {
        matches!(self, View::Pending(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::files;
    use crate::config::Config;
    use crate::mcp::testing::{
        app_with, app_with_metrics, context, local_metrics, open, pending_entry, principal_as,
        render, sample_of, store_key, PendingEdit,
    };
    use crate::okf::workspace::Workspace;
    use crate::store::changesets::ChangesetState;
    use crate::store::import::{import, ImportOptions, ImportScope};
    use crate::store::keys::Access;

    fn entry(path: &str, text: Option<&str>) -> EntryRow {
        EntryRow {
            path: path.to_string(),
            base_revision: None,
            content: text.map(|text| text.as_bytes().to_vec()),
            executable: false,
            replay: None,
        }
    }

    /// The key of the changeset [`changeset`] builds for `id`.
    fn key(id: i64) -> ChangesetKey {
        changeset(id, 1).key()
    }

    /// An open changeset row, never stored: for overlays built directly.
    fn changeset(id: i64, version: i64) -> ChangesetRow {
        ChangesetRow {
            id,
            handle: format!("handle-{id}"),
            catalog_id: 1,
            key_id: None,
            actor: "t".to_string(),
            state: ChangesetState::Open,
            version,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_overlay_shows_the_entries_over_the_published_files() {
        let test = app_with(
            &[("pg", &[("a.md", "# Old\n"), ("b.md", "# B\n")])],
            Config::default(),
        )
        .await;
        let base = test.app.catalog("pg").await.unwrap().unwrap().current();
        let changeset = changeset(7, 3);
        let entries = vec![
            entry("a.md", Some("# New\n")),
            entry("b.md", None),
            entry("c.md", Some("# C\n")),
        ];

        let overlay = build_overlay(Arc::clone(&base), &changeset, entries)
            .await
            .unwrap();

        let snapshot = overlay.snapshot();
        assert_eq!(snapshot.get("a.md").unwrap().text(), "# New\n");
        assert!(snapshot.get("b.md").is_none(), "a None entry deletes");
        assert_eq!(snapshot.get("c.md").unwrap().text(), "# C\n");
        let dirty: Vec<&str> = overlay.dirty().iter().map(String::as_str).collect();
        assert_eq!(dirty, ["a.md", "b.md", "c.md"]);
        assert_eq!((overlay.changeset_id(), overlay.version()), (7, 3));
        assert!(Arc::ptr_eq(overlay.base(), &base));
        // The published state is untouched.
        assert_eq!(base.snapshot().get("a.md").unwrap().text(), "# Old\n");
        assert!(base.workspace().is_file("b.md"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_cache_is_reused_until_the_version_or_the_generation_moves() {
        let test = app_with(&[("pg", &[("a.md", "# A\n")])], Config::default()).await;
        store_key(&test, "writer").await;
        let ctx = context(
            &test,
            principal_as("writer", Access::Write, &[], Some("pg")),
            None,
        );
        let state = test.app.catalog("pg").await.unwrap().unwrap();
        let pool = test.app.accounts().executor();
        let overlays = test.app.overlays();
        let rebuilds = test.app.rebuild_locks();
        let edit = |text: &'static str| PendingEdit {
            path: "a.md",
            text: Some(text),
            at: Utc::now(),
        };
        let handle = open(&ctx).await;
        let first_row = pending_entry(&test, &handle, &edit("# One\n")).await;

        let first = overlays
            .get(
                &crate::stores::CatalogDb::main_of(pool),
                rebuilds,
                &state,
                &first_row,
            )
            .await
            .unwrap();
        let again = overlays
            .get(
                &crate::stores::CatalogDb::main_of(pool),
                rebuilds,
                &state,
                &first_row,
            )
            .await
            .unwrap();
        let second_row = pending_entry(&test, &handle, &edit("# Two\n")).await;
        let after_edit = overlays
            .get(
                &crate::stores::CatalogDb::main_of(pool),
                rebuilds,
                &state,
                &second_row,
            )
            .await
            .unwrap();
        let options = ImportOptions {
            replace: true,
            scope: ImportScope::Everything,
            expect_generation: None,
            expect_catalog_id: None,
            actor: "t",
            audit: &crate::store::audit::AuditActor::cli(),
            now: Utc::now(),
        };
        let republished = files(&[("a.md", "# A\n"), ("b.md", "# B\n")]);
        import(
            &crate::stores::CatalogDb::main_in(pool),
            "pg",
            republished,
            &options,
        )
        .await
        .unwrap();
        state
            .refresh(&crate::stores::CatalogDb::main_of(pool), rebuilds)
            .await
            .unwrap();
        let after_import = overlays
            .get(
                &crate::stores::CatalogDb::main_of(pool),
                rebuilds,
                &state,
                &second_row,
            )
            .await
            .unwrap();
        overlays.forget(second_row.key()).await;
        let after_forget = overlays
            .get(
                &crate::stores::CatalogDb::main_of(pool),
                rebuilds,
                &state,
                &second_row,
            )
            .await
            .unwrap();

        assert!(
            Arc::ptr_eq(&first, &again),
            "same version: the cached overlay"
        );
        assert!(!Arc::ptr_eq(&again, &after_edit));
        assert_eq!(after_edit.snapshot().get("a.md").unwrap().text(), "# Two\n");
        assert!(!Arc::ptr_eq(&after_edit, &after_import));
        assert!(
            after_import.snapshot().get("b.md").is_some(),
            "rebuilt on the new base"
        );
        assert_eq!(
            after_import.snapshot().get("a.md").unwrap().text(),
            "# Two\n"
        );
        assert!(!Arc::ptr_eq(&after_import, &after_forget));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_clashing_entry_names_the_pending_changeset() {
        let test = app_with(&[("pg", &[("a.md", "# A\n")])], Config::default()).await;
        let base = test.app.catalog("pg").await.unwrap().unwrap().current();

        let result =
            build_overlay(base, &changeset(9, 1), vec![entry("a.md/b.md", Some("x"))]).await;

        let Err(OkfError::InvalidPath { reason, .. }) = result else {
            panic!("expected InvalidPath, got {result:?}");
        };
        assert!(
            reason.ends_with(
                "in pending changeset handle-9; discard the clashing path (a.md or the paths under \
                 it) or the changeset"
            ),
            "{reason}"
        );
    }

    /// The overlays cache with a byte budget, and its registry.
    /// The provider must stay alive while the instruments are used.
    fn overlays_with(
        budget: u64,
    ) -> (
        Overlays,
        prometheus::Registry,
        opentelemetry_sdk::metrics::SdkMeterProvider,
    ) {
        let (provider, metrics, registry) = local_metrics();
        (
            Overlays::new(Duration::from_secs(60), budget, metrics),
            registry,
            provider,
        )
    }

    /// Caches `overlay` as a finished build would.
    async fn store(overlays: &Overlays, overlay: Arc<Overlay>) {
        let key = overlay.key;
        overlays
            .cached
            .build(key, || async { Ok(overlay) }, supersedes)
            .await
            .unwrap();
    }

    /// An overlay of changeset `id` at `version` over `base`, with `files`
    /// pending entries of `size` bytes each.
    async fn built(
        base: &Arc<Loaded>,
        id: i64,
        version: i64,
        files: usize,
        size: usize,
    ) -> Arc<Overlay> {
        let entries = (0..files)
            .map(|n| entry(&format!("pending/f{n}.md"), Some(&"x".repeat(size))))
            .collect();
        let row = changeset(id, version);
        Arc::new(
            build_overlay(Arc::clone(base), &row, entries)
                .await
                .unwrap(),
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_cache_drops_stale_entries_and_keeps_the_newer_build() {
        let test = app_with(&[("pg", &[("a.md", "# A\n")])], Config::default()).await;
        let base = test.app.catalog("pg").await.unwrap().unwrap().current();
        let generation = base.generation();
        let (newer, older, other) = (
            built(&base, 1, 2, 0, 0).await,
            built(&base, 1, 1, 0, 0).await,
            built(&base, 2, 1, 0, 0).await,
        );
        let (overlays, _registry, _provider) = overlays_with(1 << 20);

        store(&overlays, Arc::clone(&newer)).await;
        store(&overlays, Arc::clone(&older)).await;
        let behind = overlays.lookup(&changeset(1, 1), generation).await;
        let kept = overlays.lookup(&changeset(1, 2), generation).await;
        store(&overlays, Arc::clone(&other)).await;
        let stale = overlays.lookup(&changeset(2, 5), generation).await;
        let stale_dropped = overlays.cached.peek(&key(2)).await.is_none();

        assert!(
            kept.is_some_and(|kept| Arc::ptr_eq(&kept, &newer)),
            "an older build never replaces a newer one"
        );
        assert!(
            behind.is_none(),
            "an older row does not get the newer build"
        );
        assert!(stale.is_none());
        assert!(stale_dropped, "an out-of-date entry is dropped on lookup");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn overlays_are_evicted_by_bytes_not_count() {
        // Arrange: three overlays of one 10 KiB file each, a budget that
        // fits two of them but not three. A count limit of 64 would keep all.
        let test = app_with(&[("pg", &[("a.md", "# A\n")])], Config::default()).await;
        let base = test.app.catalog("pg").await.unwrap().unwrap().current();
        let generation = base.generation();
        let mut overlays_built = Vec::with_capacity(3);
        for id in 1..=3 {
            overlays_built.push(built(&base, id, 1, 1, 10 * 1024).await);
        }
        let each = overlays_built[0].weight;
        let (overlays, registry, _provider) = overlays_with(each * 5 / 2);

        // Act: a third overlay does not fit. Which one moka drops is its
        // admission policy's choice, so only the count is asserted.
        store(&overlays, Arc::clone(&overlays_built[0])).await;
        store(&overlays, Arc::clone(&overlays_built[1])).await;
        let first_used = overlays.lookup(&changeset(1, 1), generation).await;
        store(&overlays, Arc::clone(&overlays_built[2])).await;
        overlays.cached.settle().await;

        // Assert
        assert!(first_used.is_some());
        let mut kept = Vec::new();
        for id in 1..=3 {
            if overlays.cached.peek(&key(id)).await.is_some() {
                kept.push(id);
            }
        }
        assert_eq!(kept.len(), 2, "two fit, one was left out: {kept:?}");
        let text = render(&registry);
        let bytes = sample_of(&text, r#"okf_cache_bytes{cache="overlay"}"#).unwrap();
        assert!(bytes <= (each * 5 / 2) as f64, "within budget: {bytes}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_overlay_weighs_its_own_files_even_when_they_are_empty() {
        let test = app_with(&[("pg", &[("a.md", "# A\n")])], Config::default()).await;
        let base = test.app.catalog("pg").await.unwrap().unwrap().current();

        let none = built(&base, 1, 1, 0, 0).await;
        let many_empty = built(&base, 2, 1, 200, 0).await;
        let one_big = built(&base, 3, 1, 1, 100_000).await;

        // The workspace is the base's one file plus the pending ones.
        let expect = |files: u64, bytes: u64| weight(bytes, files);
        let base_bytes = base.stored_bytes();
        assert_eq!(none.weight, expect(1, base_bytes));
        assert_eq!(many_empty.weight, expect(201, base_bytes));
        assert!(
            many_empty.weight > none.weight + 200 * 1000,
            "empty files are not free"
        );
        assert_eq!(one_big.weight, expect(2, base_bytes + 100_000));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_overlay_heavier_than_the_budget_is_served_but_not_kept() {
        let test = app_with(&[("pg", &[("a.md", "# A\n")])], Config::default()).await;
        let base = test.app.catalog("pg").await.unwrap().unwrap().current();
        let big = built(&base, 1, 1, 1, 50_000).await;
        let (overlays, _registry, _provider) = overlays_with(big.weight - 1);

        store(&overlays, Arc::clone(&big)).await;

        assert!(overlays.cached.peek(&key(1)).await.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn changesets_with_one_id_in_two_catalogs_are_cached_and_forgotten_apart() {
        // Arrange: changeset 1 of catalog 1 and changeset 1 of catalog 2.
        let test = app_with(&[("pg", &[("a.md", "# A\n")])], Config::default()).await;
        let base = test.app.catalog("pg").await.unwrap().unwrap().current();
        let (overlays, _registry, _provider) = overlays_with(1 << 20);
        let first = built(&base, 1, 1, 1, 10).await;
        let mut other_row = changeset(1, 1);
        other_row.catalog_id = 2;
        let second = Arc::new(
            build_overlay(Arc::clone(&base), &other_row, Vec::new())
                .await
                .unwrap(),
        );
        store(&overlays, Arc::clone(&first)).await;
        store(&overlays, Arc::clone(&second)).await;

        // Act: forget the first.
        overlays.forget(key(1)).await;

        // Assert: the second is still cached, and is its own entry.
        let kept = overlays.cached.peek(&other_row.key()).await.unwrap();
        assert!(Arc::ptr_eq(&kept, &second));
        assert!(overlays.cached.peek(&key(1)).await.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn forget_drops_the_overlay() {
        let test = app_with(&[("pg", &[("a.md", "# A\n")])], Config::default()).await;
        let base = test.app.catalog("pg").await.unwrap().unwrap().current();
        let (overlays, _registry, _provider) = overlays_with(1 << 20);
        store(&overlays, built(&base, 1, 1, 0, 0).await).await;

        overlays.forget(key(1)).await;

        assert!(overlays
            .lookup(&changeset(1, 1), base.generation())
            .await
            .is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_first_reads_build_one_overlay() {
        // Arrange
        let test = app_with(&[("pg", &[("a.md", "# A\n")])], Config::default()).await;
        store_key(&test, "writer").await;
        let ctx = context(
            &test,
            principal_as("writer", Access::Write, &[], Some("pg")),
            None,
        );
        let handle = open(&ctx).await;
        let edit = PendingEdit {
            path: "a.md",
            text: Some("# One\n"),
            at: Utc::now(),
        };
        let row = Arc::new(pending_entry(&test, &handle, &edit).await);

        // Act: eight reads that all miss the empty cache at once.
        let reads: Vec<_> = (0..8)
            .map(|_| {
                let (app, row) = (Arc::clone(&test.app), Arc::clone(&row));
                tokio::spawn(async move {
                    let state = app.catalog("pg").await.unwrap().unwrap();
                    app.overlays()
                        .get(&app.main_db().await, app.rebuild_locks(), &state, &row)
                        .await
                        .unwrap()
                })
            })
            .collect();
        let mut overlays = Vec::with_capacity(reads.len());
        for read in reads {
            overlays.push(read.await.unwrap());
        }

        // Assert: every read got the one overlay built.
        assert!(
            overlays
                .iter()
                .all(|overlay| Arc::ptr_eq(overlay, &overlays[0])),
            "one build, shared"
        );
        assert_eq!(
            overlays[0].snapshot().get("a.md").unwrap().text(),
            "# One\n"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_write_does_not_wait_for_a_read_building_on_a_full_pool() {
        // Arrange: a writer's transaction holds one connection and the test
        // holds every other, so a read's build waits for a connection while
        // it holds the changeset's build lock.
        let test = app_with(&[("pg", &[("a.md", "# A\n")])], Config::default()).await;
        store_key(&test, "writer").await;
        let ctx = context(
            &test,
            principal_as("writer", Access::Write, &[], Some("pg")),
            None,
        );
        let handle = open(&ctx).await;
        let edit = PendingEdit {
            path: "a.md",
            text: Some("# One\n"),
            at: Utc::now(),
        };
        let row = Arc::new(pending_entry(&test, &handle, &edit).await);
        // Loaded before the pool is full: a catalog loads on demand.
        let state = test.app.catalog("pg").await.unwrap().unwrap();
        // The build reads through the store's own pool, so that is the one filled.
        let db = test.app.main_db().await;
        let pool = db.executor();
        let mut tx = pool.begin().await.unwrap();
        let size = pool.options().get_max_connections();
        let mut held = Vec::with_capacity(size as usize);
        for _ in 1..size {
            held.push(pool.acquire().await.unwrap());
        }
        let read = {
            let (app, row, state, db) = (
                Arc::clone(&test.app),
                Arc::clone(&row),
                Arc::clone(&state),
                db.clone(),
            );
            tokio::spawn(async move {
                app.overlays()
                    .get(&db, app.rebuild_locks(), &state, &row)
                    .await
            })
        };
        // Wait until the read holds the build lock (its entry exists).
        while test.app.overlays().builds.len() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Act: well inside sqlx's 30 s acquire timeout, which a write
        // waiting on the read's build lock would run the read into.
        let written = tokio::time::timeout(
            Duration::from_secs(10),
            test.app.overlays().get_in(&mut tx, &state, &row),
        )
        .await;
        drop(tx);
        let read = tokio::time::timeout(Duration::from_secs(10), read).await;

        // Assert
        let written = written.expect("the write is not held up").unwrap();
        assert_eq!(written.snapshot().get("a.md").unwrap().text(), "# One\n");
        read.expect("the read finishes once the write lets go")
            .unwrap()
            .unwrap();
        drop(held);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_forget_while_an_overlay_builds_keeps_the_build_out_of_the_cache() {
        // Arrange: a read's build waits for a connection (the test holds
        // every one), after its fence is registered.
        let test = app_with(&[("pg", &[("a.md", "# A\n")])], Config::default()).await;
        store_key(&test, "writer").await;
        let ctx = context(
            &test,
            principal_as("writer", Access::Write, &[], Some("pg")),
            None,
        );
        let handle = open(&ctx).await;
        let edit = PendingEdit {
            path: "a.md",
            text: Some("# One\n"),
            at: Utc::now(),
        };
        let row = Arc::new(pending_entry(&test, &handle, &edit).await);
        let state = test.app.catalog("pg").await.unwrap().unwrap();
        // The build reads through the store's own pool, so that is the one filled.
        let db = test.app.main_db().await;
        let pool = db.executor();
        let size = pool.options().get_max_connections();
        let mut held = Vec::with_capacity(size as usize);
        for _ in 0..size {
            held.push(pool.acquire().await.unwrap());
        }
        let read = {
            let (app, row, state, db) = (
                Arc::clone(&test.app),
                Arc::clone(&row),
                Arc::clone(&state),
                db.clone(),
            );
            tokio::spawn(async move {
                app.overlays()
                    .get(&db, app.rebuild_locks(), &state, &row)
                    .await
            })
        };
        while test.app.overlays().builds.len() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Act: a write or discard forgets the overlay mid-build.
        test.app.overlays().forget(row.key()).await;
        drop(held);
        let served = tokio::time::timeout(Duration::from_secs(30), read)
            .await
            .expect("the build finishes once a connection is free")
            .unwrap()
            .unwrap();

        // Assert: the caller got its overlay, but it was not kept.
        assert_eq!(served.snapshot().get("a.md").unwrap().text(), "# One\n");
        assert!(
            test.app.overlays().cached.peek(&row.key()).await.is_none(),
            "an overlay overtaken by a forget stays out of the cache"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn overlay_lookups_and_builds_are_counted() {
        // Arrange
        let (test, registry) =
            app_with_metrics(&[("pg", &[("a.md", "# A\n")])], Config::default()).await;
        store_key(&test, "writer").await;
        let ctx = context(
            &test,
            principal_as("writer", Access::Write, &[], Some("pg")),
            None,
        );
        let handle = open(&ctx).await;
        let edit = |path| PendingEdit {
            path,
            text: Some("# One\n"),
            at: Utc::now(),
        };
        let row = pending_entry(&test, &handle, &edit("b.md")).await;
        let state = test.app.catalog("pg").await.unwrap().unwrap();
        let (pool, rebuilds) = (test.app.accounts().executor(), test.app.rebuild_locks());
        let overlays = test.app.overlays();

        // Act: a miss that builds, then a hit, then a build that fails (the
        // new entry's path is below a published file).
        overlays
            .get(
                &crate::stores::CatalogDb::main_of(pool),
                rebuilds,
                &state,
                &row,
            )
            .await
            .unwrap();
        overlays
            .get(
                &crate::stores::CatalogDb::main_of(pool),
                rebuilds,
                &state,
                &row,
            )
            .await
            .unwrap();
        let clash = pending_entry(&test, &handle, &edit("a.md/c.md")).await;
        let failed = overlays
            .get(
                &crate::stores::CatalogDb::main_of(pool),
                rebuilds,
                &state,
                &clash,
            )
            .await;

        // Assert: one request per call (the second lookup under the build
        // lock is not counted).
        assert!(failed.is_err());
        let text = render(&registry);
        let series = |name: &str| sample_of(&text, name).unwrap_or(0.0);
        assert_eq!(
            series(r#"okf_cache_requests_total{cache="overlay",result="miss"}"#),
            2.0
        );
        assert_eq!(
            series(r#"okf_cache_requests_total{cache="overlay",result="hit"}"#),
            1.0
        );
        assert_eq!(
            series(r#"okf_cache_load_duration_seconds_count{cache="overlay",outcome="ok"}"#),
            1.0
        );
        assert_eq!(
            series(r#"okf_cache_load_duration_seconds_count{cache="overlay",outcome="error"}"#),
            1.0
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_pending_md_that_is_not_utf8_names_the_changeset() {
        let test = app_with(&[("pg", &[("a.md", "# A\n")])], Config::default()).await;
        let base = test.app.catalog("pg").await.unwrap().unwrap().current();
        let garbled = EntryRow {
            path: "bad.md".to_string(),
            base_revision: None,
            content: Some(b"# \xff\xfe\n".to_vec()),
            executable: false,
            replay: None,
        };

        let result = build_overlay(base, &changeset(4, 1), vec![garbled]).await;

        let Err(error) = result else {
            panic!("expected an error, got {result:?}");
        };
        // Not masked as internal, so `search` can skip it.
        assert!(crate::mcp::context::is_broken_overlay(&error));
        let message = error.to_string();
        assert!(message.starts_with("bad.md: "), "{message}");
        assert!(
            message.ends_with("in pending changeset handle-4; discard bad.md or the changeset"),
            "{message}"
        );
    }
}
