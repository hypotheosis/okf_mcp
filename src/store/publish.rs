//! Publish (design "Data flow", Publish): a changeset's entries become the
//! catalog's published documents, under the catalog's publish lock.
//!
//! The lock is two locks, taken in this order. First the in-process mutex
//! ([`App::publish_lock`], keyed by catalog id so every `CatalogState` of
//! one catalog shares it), held until the snapshot is swapped.
//! Then, in the transaction, the user rows its audit entry resolves
//! (`lock_people`), so the publish takes users before its changeset as
//! `users::delete` does, then the advisory lock, which a CLI import takes
//! too. The transaction then takes the catalog row (`lock_catalog`) and the
//! changeset row (`changesets::lock`), in the order every changeset
//! transaction uses. While the locks are held, nothing else can change the
//! catalog's documents or this changeset's entries, so the checks see
//! exactly what is written.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::Serialize;
use sqlx::PgConnection;
use tracing::Instrument;

use crate::app::App;
use crate::catalog::{CatalogState, Loaded};
use crate::error::{ConflictFile, OkfError};
use crate::invalidate::{invalidate, Invalidation};
use crate::mcp::blocking;
use crate::okf::analysis::lint::LintReport;
use crate::okf::analysis::Finding;
use crate::okf::ops::replay::{replay, ReplayOp};
use crate::okf::rules;
use crate::okf::snapshot::Snapshot;
use crate::overlay::{build_overlay, Overlay};
use crate::store::audit::{self, Action, AuditActor, CatalogEntry, People};
use crate::store::catalogs::{self, is_unique_violation, CatalogGeneration};
use crate::store::changesets::{self, ChangesetRow, ChangesetState, EntryRow};
use crate::store::documents::{self, Derived, DocumentWrite, RevisionOp, RevisionRecord};
use crate::stores::CatalogDb;
use crate::telemetry::metrics::Outcome;
use crate::telemetry::subscriber::split_event;

/// What a publish did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, JsonSchema)]
pub struct PublishReport {
    /// `false` when the changeset held nothing to publish.
    pub published: bool,
    /// The revisions written, in path order. A file whose content ended up as published gets none.
    pub revisions: Vec<PublishedRevision>,
    /// Lint advice: errors in files this publish does not change, errors in files it does change
    /// that the published file already had, and warnings in those it changes.
    pub warnings: Vec<Finding>,
    /// `/`-prefixed paths whose bookkeeping (log, index listings) was replayed onto newer text.
    pub replayed: Vec<String>,
}

/// One revision a publish wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct PublishedRevision {
    /// `/`-prefixed path.
    pub path: String,
    /// Its new revision.
    pub revision: i32,
    /// `create`, `update` or `delete`.
    pub op: &'static str,
}

/// A changed path's row in `documents`, tombstone or live.
#[derive(Debug)]
struct CurrentRow {
    content: Vec<u8>,
    executable: bool,
    revision: i32,
    live: bool,
}

/// The entries once replayed onto current text.
#[derive(Debug)]
struct Merged {
    entries: Vec<EntryRow>,
    replayed: Vec<String>,
}

/// What to publish, and as whom the documents record it.
#[derive(Debug, Clone, Copy)]
pub struct PublishInput<'a> {
    /// The open changeset (read without its lock; [`publish`] locks it and
    /// reads it again).
    pub changeset: &'a ChangesetRow,
    /// The publishing key's actor, recorded on every document and revision
    /// (an admin publishing another key's changeset records its own
    /// `actor`, not the opener's).
    pub actor: &'a str,
    /// Recorded on every revision.
    pub message: &'a str,
}

/// The publish's identity: what every row it writes records.
#[derive(Debug)]
struct Publication<'a> {
    changeset: &'a ChangesetRow,
    actor: &'a str,
    message: &'a str,
    /// The publishing credential's user (`users.id`), recorded on every
    /// revision beside the actor.
    user_id: Option<&'a str>,
    now: DateTime<Utc>,
}

/// Publishes a changeset (design "Publish", steps 1 to 6).
///
/// # Arguments
/// * `app` - the shared state (pool, clock, overlays).
/// * `state` - the changeset's catalog.
/// * `input` - the changeset, the actor its documents record, and the
///   message.
/// * `audit` - who publishes, for the `catalog.published` entry the
///   publish records in its own transaction when it writes any revision.
///
/// # Returns
/// The revisions written, lint advice and the replayed paths;
/// `published: false` if the changeset held no entries.
///
/// # Errors
/// - [`OkfError::Conflict`] for entries whose base is stale and that cannot
///   be replayed (a deletion entry with no base names a path published
///   after the changeset's first edit, so it is one too);
/// - [`OkfError::SecretDetected`] or [`OkfError::FrontmatterParse`] from the
///   content rules;
/// - [`OkfError::LintFailed`] for lint errors in changed files or links
///   into deleted paths;
/// - [`OkfError::Refused`] if the changeset is no longer open, or the
///   catalog was deleted while this waited;
/// - [`OkfError::CatalogNotSelected`] if the catalog was deleted;
/// - [`OkfError::Busy`] if the snapshot could not be brought up to date;
/// - an overlay that no longer applies ([`OkfError::InvalidPath`]);
/// - [`OkfError::Database`], the audit entry's included.
///
/// On every error the transaction rolls back and the changeset stays as it was.
///
/// Records `okf.publish.duration` for every call, and one
/// `okf.lint.rejections` per lint error of a refusal.
#[tracing::instrument(
    level = "info",
    name = "okf.publish",
    skip_all,
    fields(
        db.system.name = "postgresql",
        db.operation.name = "publish.publish",
        okf.catalog = %state.name(),
        okf.changeset.entries = tracing::field::Empty
    )
)]
pub async fn publish(
    app: &App,
    state: &CatalogState,
    input: &PublishInput<'_>,
    audit: &AuditActor,
) -> Result<PublishReport, OkfError> {
    let started = std::time::Instant::now();
    let before = state.current().generation();
    let result = Box::pin(publish_locked(app, state, input, audit)).await;
    // Whatever the outcome: the publish may have refreshed the state in
    // place, before it committed (`current_base`, even when the merge or
    // lint then refuses) or after (step 6). The catalog cache weighs the
    // new snapshot (M18 Decision 2), and drops one that no longer fits.
    // An in-process cache operation only: nothing here waits on I/O.
    app.catalog_cache().refreshed_if_moved(state, before).await;
    let outcome = if result.is_ok() {
        Outcome::Ok
    } else {
        Outcome::Error
    };
    app.metrics().publish(outcome, started.elapsed());
    if let Err(OkfError::LintFailed { errors, .. }) = &result {
        for finding in errors {
            app.metrics().lint_rejection(&finding.rule);
        }
    }
    result
}

/// [`publish`] without its metrics, inside its `okf.publish` span.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(uninstrumented_fn, reason = "runs inside the okf.publish span")
)]
async fn publish_locked(
    app: &App,
    state: &CatalogState,
    input: &PublishInput<'_>,
    audit: &AuditActor,
) -> Result<PublishReport, OkfError> {
    // Destructuring a struct borrowed by reference copies its `&` fields.
    let PublishInput {
        changeset,
        actor,
        message,
    } = *input;
    // The catalog's store: every read and write of the publish goes to it.
    let db = app.store_db(state.store()).await?;
    // Step 1, before any lock: an empty changeset never waits for one.
    if changesets::entries(db.executor(), changeset.id)
        .await?
        .is_empty()
    {
        return Ok(PublishReport::default());
    }
    // Step 2: every lock, timed as the wait. The guard (`_publishing`)
    // releases the mutex when it goes out of scope at the end of the
    // function, after the refresh. Dropping `tx` without `commit()` (any `?`
    // or early return) rolls back.
    let lock_started = std::time::Instant::now();
    let locks = async {
        let publishing = app.publish_lock(state.id()).await;
        // The account transaction (the audit entry's rows, the invalidation
        // notice) wraps the store transaction; the store commits first, then
        // this. The entry itself is written in the store transaction, from
        // ids resolved here (plan Task 12).
        let mut account_tx = app.accounts().begin().await?;
        // The users the entry resolves (the actor, its key's owner and the
        // person of a personal owner team), before the changeset: the
        // order `users::delete` takes them in (issue #418). After the
        // mutex, so no waiter on it holds a row lock.
        lock_people(&mut account_tx, audit, state.name()).await?;
        // Still before the store connection: the entry's account ids.
        let resolved = audit::resolve_actor(&mut account_tx, audit, state.id()).await?;
        let mut tx = db.begin().await?;
        catalogs::lock(&mut tx, state.id()).await?;
        changesets::lock_catalog(&mut tx, state.id()).await?;
        let locked = changesets::lock(&mut tx, changeset.id)
            .await?
            .filter(|row| row.state == ChangesetState::Open)
            .ok_or_else(|| OkfError::Refused {
                reason: format!("changeset {} is no longer open", changeset.handle),
            })?;
        Ok::<_, OkfError>((publishing, account_tx, tx, locked, resolved))
    }
    .instrument(tracing::info_span!("okf.publish.lock"))
    .await;
    // A refusal waited too, so the wait is recorded before `?` returns it.
    app.metrics().publish_lock_wait(lock_started.elapsed());
    let (_publishing, account_tx, mut tx, locked, resolved) = locks?;
    let now = app.clock().now();
    // Again under the lock: a discard may have emptied it meanwhile.
    let entries = changesets::entries(&mut *tx, locked.id).await?;
    if entries.is_empty() {
        return Ok(PublishReport::default());
    }
    // `i64`, not `usize`: the span layer exports a `usize` as a string.
    tracing::Span::current().record(
        "okf.changeset.entries",
        i64::try_from(entries.len()).unwrap_or(i64::MAX),
    );
    let base = current_base(state, &mut tx).await?;

    // Step 3: base revisions against the documents as stored now.
    let paths: Vec<String> = entries.iter().map(|entry| entry.path.clone()).collect();
    let current = current_rows(&mut tx, state.id(), &paths).await?;
    let merged = merge(entries, &current)?;

    // Step 4: the rules, lint and links on the result.
    let (overlay, warnings) = check(base, &locked, merged.entries, now)
        .instrument(tracing::info_span!("okf.publish.lint"))
        .await?;

    // Step 5: documents, revisions, generation, state; one commit.
    let publication = Publication {
        changeset: &locked,
        actor,
        message,
        user_id: audit.user_id.as_deref(),
        now,
    };
    // `tx` moves into the block (it is consumed by `commit`); the rest is borrowed.
    let (revisions, changed) = async {
        let revisions = match write_all(&mut tx, &publication, &overlay, &current, &paths).await {
            Ok(revisions) => revisions,
            // The backstop's conflict: roll back first, so the re-read below
            // sees only what is committed, and report the revision there now.
            Err(OkfError::Conflict { files }) => {
                tx.rollback().await?;
                account_tx.rollback().await?;
                let files = with_current_revisions(&db, state.id(), files).await?;
                return Err(OkfError::Conflict { files });
            }
            Err(error) => return Err(error),
        };
        // A publish that wrote no revision changed no document: it is not a
        // catalog write, so it bumps nothing, records nothing and announces
        // nothing.
        let mut changed = None;
        if !revisions.is_empty() {
            let generation = catalogs::bump_generation(&mut tx, state.id()).await?;
            let entry = CatalogEntry {
                action: Action::CatalogPublished,
                catalog_id: state.id(),
                catalog: state.name(),
                // The diff stays in the catalog's history, found by the handle.
                details: serde_json::json!({
                    "changeset": locked.handle,
                    "files": revisions.len(),
                    "generation_from": generation - 1,
                    "generation_to": generation,
                }),
            };
            // In the store transaction: it commits with the revisions it records.
            audit::record_catalog(&mut tx, &resolved, &entry, now).await?;
            // In the store transaction too: the message is sent when the generation
            // commits, on the store's own change feed (Task 17), and only then.
            invalidate(
                &mut tx,
                &[Invalidation::Catalog {
                    id: state.id(),
                    generation,
                }],
            )
            .await?;
            changed = Some(CatalogGeneration {
                catalog_id: state.id(),
                generation,
            });
        }
        changesets::set_state(&mut tx, locked.id, ChangesetState::Published, now).await?;
        tx.commit().await?;
        account_tx.commit().await?;
        Ok::<_, OkfError>((revisions, changed))
    }
    .instrument(tracing::info_span!("okf.publish.write"))
    .await?;
    // Committed, so another replica that reads now sees it: announce it, on
    // its own task (issue #659). Never before the commit, or a replica could
    // refresh to the old generation and skip this one's announcement.
    if let Some(change) = changed {
        app.announce_catalog(change);
    }

    // Step 6, still under the in-process lock (the advisory lock ended with
    // the transaction): the next publish of this catalog checks against
    // this one's result. A failed refresh does not undo a committed publish;
    // the next use retries it (it checks the snapshot against the catalog's
    // row), and the next publish refreshes first. `publish` then has the
    // catalog cache weigh the new snapshot.
    if let Err(error) = state.refresh(&db, app.rebuild_locks()).await {
        split_event!(
            error,
            { catalog = %state.name(), kind = error.kind(), },
            { %error, },
            "refresh after publish failed; the next use retries"
        );
    }
    app.overlays().forget(locked.key()).await;
    Ok(PublishReport {
        published: true,
        revisions,
        warnings,
        replayed: merged.replayed,
    })
}

/// Locks, first in a changeset transaction that records a catalog entry,
/// every user row that entry will lock ([`audit::lock_people`] with the
/// catalog): the actor, its credential's owner and the person of a
/// personal owner team. Taken after them, the changeset row can no longer
/// close a cycle with `users::delete`, which locks the user, then its keys,
/// then their changesets (issue #418). Publish, discard and an audited edit
/// all call it.
///
/// # Arguments
/// * `conn` - the change's transaction (`&mut tx`).
/// * `actor` - who records the entry.
/// * `catalog` - the changeset's catalog, by name.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "publish.lock_people"))]
pub(crate) async fn lock_people(
    conn: &mut PgConnection,
    actor: &AuditActor,
    catalog: &str,
) -> Result<(), OkfError> {
    let people = People {
        catalogs: &[catalog],
        ..People::default()
    };
    audit::lock_people(conn, actor, &people).await?;
    Ok(())
}

/// The snapshot at the catalog's current generation (design "Publish",
/// step 2): rebuilt first if the database moved past it.
///
/// # Errors
/// As [`CatalogState::refresh`]; [`OkfError::Busy`] if the snapshot still
/// does not match (only a refresh failing silently could cause it).
#[tracing::instrument(level = "trace", skip_all, fields(db.system.name = "postgresql", db.operation.name = "publish.current_base"))]
async fn current_base(
    state: &CatalogState,
    conn: &mut PgConnection,
) -> Result<Arc<Loaded>, OkfError> {
    let generation: i64 = sqlx::query_scalar("SELECT generation FROM catalog_state WHERE id = $1")
        .bind(state.id())
        .fetch_one(&mut *conn)
        .await?;
    // The advisory lock keeps every other import and publish out, so the
    // refresh builds exactly this generation, and it reads on this
    // transaction's connection: a publish never holds a second one.
    if state.current().generation() < generation {
        state.refresh_locked(conn).await?;
    }
    let base = state.current();
    if base.generation() == generation {
        Ok(base)
    } else {
        Err(OkfError::Busy {
            reason: format!(
                "catalog {:?}'s snapshot is behind the database",
                state.name()
            ),
        })
    }
}

/// The changed paths' rows in `documents` (tombstones included), by path.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "trace", skip_all, fields(db.system.name = "postgresql", db.operation.name = "publish.current_rows", catalog_id = catalog_id))]
async fn current_rows(
    conn: &mut PgConnection,
    catalog_id: i64,
    paths: &[String],
) -> Result<HashMap<String, CurrentRow>, OkfError> {
    // `= ANY($2)` matches any element of the bound array: one query for all
    // changed paths, served by the primary key.
    let rows: Vec<(String, Vec<u8>, bool, i32, bool)> = sqlx::query_as(
        "SELECT path, content, executable, revision, deleted_at IS NULL FROM documents \
         WHERE catalog_id = $1 AND path = ANY($2)",
    )
    .bind(catalog_id)
    .bind(paths)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(path, content, executable, revision, live)| {
            let row = CurrentRow {
                content,
                executable,
                revision,
                live,
            };
            (path, row)
        })
        .collect())
}

/// Checks each entry's base against the current revision (design "Publish",
/// step 3). A fresh entry is kept. A stale one with a replay has its
/// operations redone on the current text. Any other stale one is a conflict.
///
/// # Errors
/// [`OkfError::Conflict`] listing every conflicting file; a replay's error.
#[tracing::instrument(level = "trace", skip_all)]
fn merge(
    entries: Vec<EntryRow>,
    current: &HashMap<String, CurrentRow>,
) -> Result<Merged, OkfError> {
    let mut merged = Vec::with_capacity(entries.len());
    let mut replayed = Vec::new();
    let mut conflicts = Vec::new();
    // `mut entry`: the loop owns each entry and may rewrite its content.
    for mut entry in entries {
        let live = current.get(&entry.path).filter(|row| row.live);
        let current_revision = live.map(|row| row.revision);
        if entry.base_revision == current_revision {
            merged.push(entry);
            continue;
        }
        match replayed_text(&entry, live.map(|row| row.content.as_slice()))? {
            Some(text) => {
                replayed.push(format!("/{}", entry.path));
                entry.content = Some(text.into_bytes());
                entry.base_revision = current_revision;
                merged.push(entry);
            }
            None => conflicts.push(ConflictFile {
                path: format!("/{}", entry.path),
                base_revision: entry.base_revision,
                current_revision,
            }),
        }
    }
    if conflicts.is_empty() {
        Ok(Merged {
            entries: merged,
            replayed,
        })
    } else {
        Err(OkfError::Conflict { files: conflicts })
    }
}

/// A stale entry's content redone on the current text: the bytes publish
/// writes for it. `pending_changes` diffs the same result (issue #787), so
/// what it shows cannot drift from what publish does.
///
/// # Arguments
/// * `entry` - the changeset's entry for the path.
/// * `live` - the path's live published content now (`None`: no live
///   document, a tombstone included).
///
/// # Returns
/// `None` when it cannot be replayed: a conflict. That is a deletion, a
/// free edit, unreadable operations, current content that is not text, or
/// a path deleted since the entry's base (the edit was made on a document
/// that is gone, so replaying would silently re-create it).
///
/// # Errors
/// The replay's own error (text it cannot place a bullet in).
#[tracing::instrument(level = "trace", skip_all)]
pub(crate) fn replayed_text(
    entry: &EntryRow,
    live: Option<&[u8]>,
) -> Result<Option<String>, OkfError> {
    // A deletion is never replayed: whatever was published since is
    // something the changeset did not see.
    let (Some(_), Some(stored)) = (&entry.content, &entry.replay) else {
        return Ok(None);
    };
    // `write::apply` stored these; anything unreadable conflicts instead.
    let Ok(ops) = serde_json::from_value::<Vec<ReplayOp>>(stored.clone()) else {
        return Ok(None);
    };
    let current = match (live, entry.base_revision) {
        // A new path (no base) that is still not live: replay onto nothing.
        (None, None) => None,
        // Deleted since the entry's base: a conflict, not a re-creation.
        (None, Some(_)) => return Ok(None),
        (Some(content), _) => match std::str::from_utf8(content) {
            Ok(text) => Some(text),
            Err(_) => return Ok(None),
        },
    };
    replay(&entry.path, current, &ops).map(Some)
}

/// The checks on the post-replay result (design "Publish", step 4).
///
/// # Returns
/// The overlay to write from (the current snapshot plus the entries) and
/// the lint advice.
///
/// # Errors
/// A content rule's error; an overlay that does not apply;
/// [`OkfError::LintFailed`].
#[tracing::instrument(level = "trace", skip_all)]
async fn check(
    base: Arc<Loaded>,
    changeset: &ChangesetRow,
    entries: Vec<EntryRow>,
    now: DateTime<Utc>,
) -> Result<(Overlay, Vec<Finding>), OkfError> {
    let changed: BTreeSet<String> = entries.iter().map(|entry| entry.path.clone()).collect();
    let deleted: BTreeSet<String> = entries
        .iter()
        .filter(|entry| entry.content.is_none())
        .map(|entry| entry.path.clone())
        .collect();
    // The secret scan is CPU work; the entries go to the blocking thread and back.
    let entries = blocking(move || check_contents(entries)).await?;
    let overlay = build_overlay(Arc::clone(&base), changeset, entries).await?;
    let (overlay, findings) = blocking(move || {
        let findings = split_lint(&base, &overlay, &changed, &deleted, now);
        (overlay, findings)
    })
    .await;
    if findings.errors.is_empty() {
        Ok((overlay, findings.warnings))
    } else {
        Err(OkfError::LintFailed {
            errors: findings.errors,
            warnings: findings.warnings,
        })
    }
}

/// The content rules on every changed text file. Write already checked
/// each entry, but publish checks the result again: replayed text is new,
/// and an entry may predate a rule.
///
/// # Errors
/// The first refused file's error, in path order (so a refusal is the same
/// on every attempt).
#[tracing::instrument(level = "trace", skip_all)]
fn check_contents(entries: Vec<EntryRow>) -> Result<Vec<EntryRow>, OkfError> {
    for entry in &entries {
        // A file that is not text is stored and exported but not scanned
        // (design "Storage"); a deletion has no content.
        let text = entry
            .content
            .as_deref()
            .and_then(|content| std::str::from_utf8(content).ok());
        if let Some(text) = text {
            rules::check_content(&entry.path, text)?;
        }
    }
    Ok(entries)
}

/// Lint of the overlay, split for publish. New errors in changed files
/// refuse, as do links from unchanged documents into deleted paths. An
/// error in a changed file that the published file already has is advice
/// instead (see [`demote_preexisting`]), so a parser or lint change cannot
/// strand content that predates it. Errors elsewhere become advice, beside
/// the warnings in changed files. Warnings elsewhere are left out: they
/// predate this publish.
#[tracing::instrument(level = "trace", skip_all)]
fn split_lint(
    base: &Loaded,
    overlay: &Overlay,
    changed: &BTreeSet<String>,
    deleted: &BTreeSet<String>,
    now: DateTime<Utc>,
) -> LintReport {
    let report = overlay.lint(now);
    // `Finding::file` has no leading `/`.
    let in_changed = |finding: &Finding| changed.contains(&finding.file);
    // `partition` splits one list in two by the predicate, in one pass.
    let (mut errors, mut warnings): (Vec<Finding>, Vec<Finding>) =
        report.errors.into_iter().partition(in_changed);
    // The published catalog is linted only when something would refuse.
    if !errors.is_empty() {
        let (new_errors, preexisting) = demote_preexisting(errors, || base.lint(now).errors);
        errors = new_errors;
        warnings.extend(preexisting);
    }
    errors.extend(links_into_deleted(overlay.snapshot(), changed, deleted));
    warnings.extend(report.warnings.into_iter().filter(in_changed));
    LintReport { errors, warnings }
}

/// Splits errors in changed files into those the changeset introduced and
/// those the published files already had.
///
/// Findings match on file, rule and message, never on line number: adding
/// lines moves every finding below them. Matching is by count, so one
/// published broken link plus one new identical one is one new error: each
/// published finding excuses at most one finding of the result.
///
/// # Arguments
/// * `errors` - the error findings in changed files, from the result.
/// * `published_errors` - the published catalog's errors, computed only
///   when `errors` is not empty.
///
/// # Returns
/// `(new, preexisting)`. A pre-existing finding keeps its place but its
/// message says it was already there.
#[tracing::instrument(level = "trace", skip_all)]
fn demote_preexisting(
    errors: Vec<Finding>,
    published_errors: impl FnOnce() -> Vec<Finding>,
) -> (Vec<Finding>, Vec<Finding>) {
    let mut excused: HashMap<(String, String, String), usize> = HashMap::new();
    let changed_files: BTreeSet<&str> = errors.iter().map(|error| error.file.as_str()).collect();
    for finding in published_errors() {
        if changed_files.contains(finding.file.as_str()) {
            *excused
                .entry((finding.file, finding.rule, finding.message))
                .or_default() += 1;
        }
    }
    let mut new = Vec::with_capacity(errors.len());
    let mut preexisting = Vec::new();
    for mut finding in errors {
        let key = (
            finding.file.clone(),
            finding.rule.clone(),
            finding.message.clone(),
        );
        match excused.get_mut(&key) {
            Some(left) if *left > 0 => {
                *left -= 1;
                finding.message.push_str(" (already in the published file)");
                preexisting.push(finding);
            }
            _ => new.push(finding),
        }
    }
    (new, preexisting)
}

/// Links from documents this publish does not change into paths it deletes
/// (design "Publish", step 4). Changed documents' links into them are
/// already `broken-link` errors in changed files.
#[tracing::instrument(level = "trace", skip_all)]
fn links_into_deleted(
    snapshot: &Snapshot,
    changed: &BTreeSet<String>,
    deleted: &BTreeSet<String>,
) -> Vec<Finding> {
    if deleted.is_empty() {
        return Vec::new();
    }
    snapshot
        .docs()
        .iter()
        .filter(|doc| doc.in_link_graph() && !changed.contains(doc.path()))
        // `flat_map` turns each document into its offending links, then
        // chains them; `move` gives the inner closure its own `doc` handle.
        .flat_map(|doc| {
            doc.links()
                .iter()
                .filter(|link| !link.in_page && deleted.contains(&link.target))
                .map(move |link| Finding {
                    file: doc.path().to_string(),
                    line: link.line,
                    rule: "link-into-deleted".to_string(),
                    message: format!("links to /{}, which this publish deletes", link.target),
                })
        })
        .collect()
}

/// Writes every changed path from the overlay (design "Publish", step 5).
///
/// # Returns
/// The revisions written, in path order. A path whose result equals the
/// published file gets no revision.
///
/// # Errors
/// [`OkfError::Database`]; a unique violation (two writers of one revision,
/// which the lock rules out) as [`OkfError::Conflict`].
#[tracing::instrument(level = "trace", skip_all, fields(db.system.name = "postgresql", db.operation.name = "publish.write_all"))]
async fn write_all(
    conn: &mut PgConnection,
    publication: &Publication<'_>,
    overlay: &Overlay,
    current: &HashMap<String, CurrentRow>,
    paths: &[String],
) -> Result<Vec<PublishedRevision>, OkfError> {
    let changeset = publication.changeset;
    let mut revisions = Vec::with_capacity(paths.len());
    for path in paths {
        let row = current.get(path);
        let live = row.filter(|row| row.live);
        let file = overlay.workspace().stored(path);
        let op = match (&file, live) {
            (Some(file), Some(row))
                if row.content == file.content && row.executable == file.executable =>
            {
                continue
            }
            (Some(_), Some(_)) => RevisionOp::Update,
            (Some(_), None) => RevisionOp::Create,
            (None, Some(_)) => RevisionOp::Delete,
            // A deletion of a path that has no live document: nothing to do.
            (None, None) => continue,
        };
        // A tombstone keeps its number, so a path created again continues it.
        let revision = row.map_or(1, |row| row.revision + 1);
        let write = DocumentWrite {
            catalog_id: changeset.catalog_id,
            path,
            revision,
            actor: publication.actor,
            now: publication.now,
        };
        let record = RevisionRecord {
            op,
            file: file.as_ref(),
            changeset_id: Some(changeset.id),
            message: publication.message,
            user_id: publication.user_id,
        };
        let backstop = |error| as_conflict(error, path, live.map(|row| row.revision));
        match &file {
            Some(file) => {
                let derived = Derived::of(overlay.snapshot().get(path));
                documents::write_document(conn, &write, file, &derived)
                    .await
                    .map_err(backstop)?;
            }
            None => documents::delete_document(conn, &write)
                .await
                .map_err(backstop)?,
        }
        documents::record_revision(conn, &write, &record)
            .await
            .map_err(backstop)?;
        revisions.push(PublishedRevision {
            path: format!("/{path}"),
            revision,
            op: op.as_str(),
        });
    }
    Ok(revisions)
}

/// The design's backstop: a unique violation while writing is a conflict,
/// never the opaque `Database` error.
#[tracing::instrument(level = "trace", skip_all)]
fn as_conflict(error: OkfError, path: &str, expected: Option<i32>) -> OkfError {
    match error {
        // `if` guard: this arm matches only a unique violation.
        OkfError::Database { source } if is_unique_violation(&source) => OkfError::Conflict {
            files: vec![ConflictFile {
                path: format!("/{path}"),
                base_revision: expected,
                // Unknown here: `publish` re-reads it after the rollback
                // (`with_current_revisions`).
                current_revision: None,
            }],
        },
        other => other,
    }
}

/// Fills in each conflicting file's live revision as committed now, for the
/// backstop's conflict (read on the store's pool, after the publish rolled back).
///
/// # Arguments
/// * `db` - the catalog's store.
/// * `catalog_id` - the catalog.
/// * `files` - the conflicting files.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "trace", skip_all, fields(db.system.name = "postgresql", db.operation.name = "publish.with_current_revisions", catalog_id = catalog_id))]
async fn with_current_revisions(
    db: &CatalogDb,
    catalog_id: i64,
    mut files: Vec<ConflictFile>,
) -> Result<Vec<ConflictFile>, OkfError> {
    // `&mut files`: the loop edits each file in place (one for the backstop).
    for file in &mut files {
        file.current_revision = sqlx::query_scalar(
            "SELECT revision FROM documents \
             WHERE catalog_id = $1 AND path = $2 AND deleted_at IS NULL",
        )
        .bind(catalog_id)
        .bind(file.path.trim_start_matches('/'))
        .fetch_optional(db.executor())
        .await?;
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, base: Option<i32>, text: Option<&str>) -> EntryRow {
        EntryRow {
            path: path.to_string(),
            base_revision: base,
            content: text.map(|text| text.as_bytes().to_vec()),
            executable: false,
            replay: None,
        }
    }

    fn row(text: &str, revision: i32, live: bool) -> CurrentRow {
        CurrentRow {
            content: text.as_bytes().to_vec(),
            executable: false,
            revision,
            live,
        }
    }

    fn finding(file: &str, line: usize, message: &str) -> Finding {
        Finding {
            file: file.to_string(),
            line,
            rule: "broken-link".to_string(),
            message: message.to_string(),
        }
    }

    #[test]
    fn a_published_error_excuses_the_same_error_on_a_moved_line() {
        let published = vec![finding("log.md", 3, "link target does not exist: a b.md")];
        let result = vec![finding("log.md", 9, "link target does not exist: a b.md")];

        let (new, old) = demote_preexisting(result, || published);

        assert!(new.is_empty());
        assert_eq!(old.len(), 1);
        assert!(old[0].message.ends_with("(already in the published file)"));
    }

    #[test]
    fn one_published_error_excuses_only_one_of_two_identical_errors() {
        let published = vec![finding("log.md", 3, "m")];
        let result = vec![finding("log.md", 3, "m"), finding("log.md", 8, "m")];

        let (new, old) = demote_preexisting(result, || published);

        assert_eq!((new.len(), old.len()), (1, 1));
        assert_eq!(new[0].line, 8);
    }

    #[test]
    fn a_published_error_does_not_excuse_another_message_or_file() {
        let published = vec![finding("a.md", 1, "m")];
        let result = vec![finding("a.md", 1, "other"), finding("b.md", 1, "m")];

        let (new, old) = demote_preexisting(result, || published);

        assert_eq!((new.len(), old.len()), (2, 0));
    }

    #[test]
    fn a_deletion_published_since_the_first_edit_conflicts_even_with_a_replay() {
        // `base_revision: None` on a deletion: the path was published after
        // the changeset's first edit (write::apply stores no other kind).
        let mut deletion = entry("log.md", None, None);
        deletion.replay = Some(serde_json::json!([]));
        let current = HashMap::from([("log.md".to_string(), row("# Log\n", 1, true))]);

        let error = merge(vec![deletion], &current).unwrap_err();

        let OkfError::Conflict { files } = error else {
            panic!("not a conflict: {error:?}")
        };
        assert_eq!(
            files,
            [ConflictFile {
                path: "/log.md".to_string(),
                base_revision: None,
                current_revision: Some(1),
            }]
        );
    }

    #[test]
    fn a_replay_onto_a_path_deleted_since_its_base_conflicts() {
        // A `log` entry made on log.md revision 1; log.md is a tombstone now.
        let mut logged = entry("log.md", Some(1), Some("# Log\n\n## 2026-11-03\n\n* x\n"));
        let op = ReplayOp::Log {
            date: "2026-11-03".to_string(),
            bullet: "* x".to_string(),
        };
        logged.replay = Some(serde_json::json!([op]));
        let current = HashMap::from([("log.md".to_string(), row("", 2, false))]);

        let error = merge(vec![logged], &current).unwrap_err();

        let OkfError::Conflict { files } = error else {
            panic!("not a conflict: {error:?}")
        };
        assert_eq!(
            files,
            [ConflictFile {
                path: "/log.md".to_string(),
                base_revision: Some(1),
                current_revision: None,
            }]
        );
    }

    #[test]
    fn a_tombstone_counts_as_no_live_document() {
        // Base None, and the path is only a tombstone now: still fresh.
        let created = entry("a.md", None, Some("x"));
        let current = HashMap::from([("a.md".to_string(), row("", 4, false))]);

        let merged = merge(vec![created], &current).unwrap();

        assert_eq!(merged.entries.len(), 1);
        assert!(merged.replayed.is_empty());
    }

    #[test]
    fn as_conflict_passes_other_database_errors_through() {
        let error = as_conflict(
            OkfError::Database {
                source: sqlx::Error::PoolTimedOut,
            },
            "a.md",
            Some(1),
        );

        assert_eq!(error.kind(), "Database");
    }

    /// One catalog loaded twice, as an evicted cache entry still in use and
    /// its reload would be (M18 Decision 4): a publish through the second
    /// state waits for the publish lock a publish through the first holds.
    /// Before the locks moved to `App`, each state had its own mutex, and
    /// the second publish ran (and committed) at once.
    #[tokio::test(flavor = "multi_thread")]
    async fn publishes_of_one_catalog_serialize_across_two_states() {
        use crate::config::Config;
        use crate::mcp::testing::{app_with, open, pending_entry, writer, PendingEdit};
        use std::time::Duration;

        // Arrange: two states of `pg`, and a changeset with one edit.
        const NOTE: &str = "---\ntype: Note\ntitle: B\ndescription: A note.\n---\n# B\n";
        let test = app_with(&[("pg", &[("a.md", NOTE)])], Config::default()).await;
        let first = test.app.catalog("pg").await.unwrap().expect("pg is loaded");
        let second = Arc::new(
            CatalogState::load(
                &test.app.main_db().await,
                &test.app.catalog_row("pg").await.unwrap().expect("pg"),
            )
            .await
            .expect("load pg again"),
        );
        assert!(!Arc::ptr_eq(&first, &second), "two separate states");
        let ctx = writer(&test, "writer", "pg").await;
        let handle = open(&ctx).await;
        let edit = PendingEdit {
            path: "b.md",
            text: Some(NOTE),
            at: Utc::now(),
        };
        let changeset = pending_entry(&test, &handle, &edit).await;
        // Held as a publish through `first` holds it, for its whole length.
        let held = test.app.publish_lock(first.id()).await;

        // Act: publish through `second`, on its own task.
        let app = Arc::clone(&test.app);
        let publishing = tokio::spawn(async move {
            let input = PublishInput {
                changeset: &changeset,
                actor: "writer",
                message: "through the second state",
            };
            publish(&app, &second, &input, &AuditActor::cli()).await
        });
        // A publish takes well under this; one sharing the lock never ends.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let waited = !publishing.is_finished();
        let still_open = crate::mcp::testing::changeset_row(&test, &handle)
            .await
            .state;
        drop(held);
        let published = tokio::time::timeout(Duration::from_secs(30), publishing)
            .await
            .expect("the publish finishes once the lock is released")
            .expect("the publish task")
            .expect("the publish");

        // Assert: it waited, wrote nothing meanwhile, then published.
        assert!(waited, "the publish waited for the other state's lock");
        assert_eq!(still_open, ChangesetState::Open);
        assert!(published.published);
        assert_eq!(published.revisions.len(), 1);
    }
}
