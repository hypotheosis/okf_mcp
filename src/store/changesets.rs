//! Changesets and their entries (design "Storage", "Session lifecycle"):
//! a changeset, named by the handle [`open`] draws (M8 Decision 1), holds
//! one key's unpublished edits in one catalog until `publish` or `discard`
//! closes it. A key may have several open at once.
//!
//! # Lock order (Decision 3)
//!
//! Every transaction that writes a changeset or its entries takes:
//! 1. the catalog row, `FOR KEY SHARE` ([`lock_catalog`]), first;
//! 2. the changeset row (the `UPDATE` that bumps `version`);
//! 3. the entry rows.
//!
//! `catalogs::delete` takes the catalog row `FOR UPDATE` before the open
//! changesets, so a write and a delete wait for each other in the same order
//! and cannot deadlock: a write that waited sees the catalog gone. Every
//! function here that writes takes its caller's transaction, after
//! [`lock_catalog`]; only [`prune_empty`] runs on its own, without the
//! catalog lock, skipping rows another transaction holds (`SKIP LOCKED`), so
//! it never waits for anyone. A deadlock Postgres still detects is
//! [`OkfError::Busy`], through `From<sqlx::Error>`.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgExecutor};

use crate::error::OkfError;
use crate::store::catalogs;
use crate::store::decode_column;
use crate::stores::{AccountDb, CatalogDb};

/// What [`lock_catalog`] reports when the catalog row is gone: it was
/// deleted while the write waited for the delete to finish.
pub const CATALOG_GONE: &str =
    "the catalog was deleted while this request waited; nothing was written";

/// The columns of a changeset row, in [`RawChangeset`]'s order.
// A macro rather than a `const`: `concat!` (below) joins string literals at
// compile time, and it expands a macro call among its arguments into one,
// while it cannot read a `const`. So every query stays one literal string.
macro_rules! changeset_columns {
    () => {
        "id, handle, catalog_id, key_id, actor, state, version, created_at, updated_at"
    };
}

/// Where a changeset is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangesetState {
    /// Receiving edits.
    Open,
    /// Its entries became documents and revisions.
    Published,
    /// Dropped by `discard` or by the prune.
    Discarded,
}

impl ChangesetState {
    /// The spelling stored in `changesets.state`.
    ///
    /// # Returns
    /// `"open"`, `"published"` or `"discarded"`.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: maps a value, no I/O"
        )
    )]
    pub fn as_str(self) -> &'static str {
        match self {
            ChangesetState::Open => "open",
            ChangesetState::Published => "published",
            ChangesetState::Discarded => "discarded",
        }
    }

    /// Parses the stored spelling.
    ///
    /// # Arguments
    /// * `text` - a `changesets.state` value.
    ///
    /// # Returns
    /// The state, or `None` for anything [`ChangesetState::as_str`] does not
    /// produce.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: maps a value, no I/O"
        )
    )]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "open" => Some(ChangesetState::Open),
            "published" => Some(ChangesetState::Published),
            "discarded" => Some(ChangesetState::Discarded),
            _ => None,
        }
    }
}

/// What names a changeset across stores: its catalog's id (unique server
/// wide) and its own `changesets.id`, which each store numbers from its own
/// sequence, so two stores hand out the same one (issue #1022). Every
/// in-memory map or lock keyed by a changeset uses this, never the bare id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChangesetKey {
    /// The catalog's id.
    pub catalog_id: i64,
    /// The changeset's id within its store.
    pub id: i64,
}

/// A changeset row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangesetRow {
    /// Database id: entries, revisions and the overlay cache refer to it;
    /// clients see only the handle.
    pub id: i64,
    /// The handle the server issued (M8 Decision 1): a random UUID in text
    /// form, what `open_changeset` returns.
    pub handle: String,
    /// Its catalog.
    pub catalog_id: i64,
    /// The key it belongs to; `None` once that key's row is deleted.
    pub key_id: Option<String>,
    /// The actor of the key that opened it.
    pub actor: String,
    /// Open, published or discarded.
    pub state: ChangesetState,
    /// Bumped by every change to it or its entries.
    pub version: i64,
    /// When it was opened (server clock).
    pub created_at: DateTime<Utc>,
    /// Its last change (server clock).
    pub updated_at: DateTime<Utc>,
}

impl ChangesetRow {
    /// The key that names this changeset in caches and lock maps.
    ///
    /// # Returns
    /// Its catalog id and its id.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn key(&self) -> ChangesetKey {
        ChangesetKey {
            catalog_id: self.catalog_id,
            id: self.id,
        }
    }
}

/// One changed path of a changeset.
// `FromRow`: the columns map onto the fields by name. `replay` is `jsonb`,
// which sqlx's `json` feature decodes into `serde_json::Value`.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct EntryRow {
    /// Catalog-relative path.
    pub path: String,
    /// The published revision the edit was based on; `None` when the path
    /// had no live document.
    pub base_revision: Option<i32>,
    /// The new bytes; `None` deletes the path.
    pub content: Option<Vec<u8>>,
    /// The new file's execute bit.
    pub executable: bool,
    /// The replay operations (Task 3's `ReplayOp`s as JSON) while every
    /// change to this entry came from one; `None` otherwise.
    pub replay: Option<serde_json::Value>,
}

/// An open changeset as [`list_open`] returns it (`list_changesets`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedRow {
    /// The changeset (its handle, catalog id, key, actor, times).
    pub changeset: ChangesetRow,
    /// Its catalog's name (read from `catalogs`, on the account side).
    pub catalog: String,
    /// The credential's grouping name (read from `api_keys`, on the account
    /// side); null only if its key row was deleted.
    pub key_name: Option<String>,
    /// The `users.id` of the person who opened it (their key's user, or the
    /// console session's); null for a key no user owns, or one deleted.
    pub user_id: Option<String>,
    /// Its entries' paths, catalog-relative and sorted; empty for an empty
    /// changeset.
    pub paths: Vec<String>,
}

/// A changeset row as selected, before `state` is parsed.
#[derive(sqlx::FromRow)]
struct RawChangeset {
    id: i64,
    handle: String,
    catalog_id: i64,
    key_id: Option<String>,
    actor: String,
    state: String,
    version: i64,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl TryFrom<RawChangeset> for ChangesetRow {
    type Error = OkfError;

    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: maps a value, no I/O"
        )
    )]
    fn try_from(raw: RawChangeset) -> Result<Self, OkfError> {
        Ok(Self {
            id: raw.id,
            handle: raw.handle,
            catalog_id: raw.catalog_id,
            key_id: raw.key_id,
            actor: raw.actor,
            // The column's CHECK constraint allows only the three spellings.
            state: decode_column(ChangesetState::parse(&raw.state), "changesets.state")?,
            version: raw.version,
            created_at: raw.created_at,
            updated_at: raw.updated_at,
        })
    }
}

/// A [`ListedRow`] as the store selects it: the changeset and its paths,
/// before the account side supplies the catalog's name and the key's label.
#[derive(sqlx::FromRow)]
struct RawListed {
    // `flatten`: the changeset's columns sit beside `paths` in the same
    // result row.
    #[sqlx(flatten)]
    changeset: RawChangeset,
    paths: Vec<String>,
}

/// The error for a changeset that stopped being open under a write.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial conversion: builds an error value, no I/O"
    )
)]
fn no_longer_open(changeset_id: i64) -> OkfError {
    OkfError::Busy {
        reason: format!("changeset {changeset_id} was published or discarded meanwhile"),
    }
}

/// Takes the catalog's `catalog_state` row `FOR KEY SHARE` for the rest of
/// the transaction: the first lock of every write to changesets or entries
/// (Decision 3). The row is on the catalog's store, so `conn` is a store
/// transaction.
///
/// `FOR KEY SHARE` lets other writers and publishes (whose `UPDATE` of
/// `generation` takes `FOR NO KEY UPDATE`) through, and holds off only a
/// delete's `FOR UPDATE`.
///
/// # Arguments
/// * `conn` - the store transaction's connection (pass `&mut tx`).
/// * `catalog_id` - the catalog.
///
/// # Errors
/// [`OkfError::Refused`] with [`CATALOG_GONE`] if the row is gone or marked
/// deleted (a delete committed while this waited); [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "changesets.lock_catalog", catalog_id = catalog_id))]
pub async fn lock_catalog(conn: &mut PgConnection, catalog_id: i64) -> Result<(), OkfError> {
    let locked: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM catalog_state WHERE id = $1 AND deleted_at IS NULL FOR KEY SHARE",
    )
    .bind(catalog_id)
    .fetch_optional(&mut *conn)
    .await?;
    if locked.is_none() {
        return Err(OkfError::Refused {
            reason: CATALOG_GONE.to_string(),
        });
    }
    Ok(())
}

/// A changeset by id, in any state.
///
/// # Arguments
/// * `executor` - a pool, connection or transaction.
/// * `id` - the changeset's id.
///
/// # Returns
/// The row, or `None` if there is none.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "changesets.by_id"))]
pub async fn by_id<'e>(
    executor: impl PgExecutor<'e>,
    id: i64,
) -> Result<Option<ChangesetRow>, OkfError> {
    let row = sqlx::query_as::<_, RawChangeset>(concat!(
        "SELECT ",
        changeset_columns!(),
        " FROM changesets WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(executor)
    .await?;
    row.map(ChangesetRow::try_from).transpose()
}

/// Opens a new changeset for `key_id` in a catalog (M8 Decision 1). Several
/// may be open at once for one key and catalog. Postgres draws the handle:
/// the column's default is `gen_random_uuid()::text` (migration 0003).
///
/// # Arguments
/// * `conn` - the transaction's connection, after [`lock_catalog`].
/// * `catalog_id` - the catalog.
/// * `key_id` - the opening key.
/// * `actor` - its actor.
/// * `now` - the time (server clock), for its timestamps.
///
/// # Returns
/// The new, open, empty changeset.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "changesets.open", catalog_id = catalog_id))]
pub async fn open(
    conn: &mut PgConnection,
    catalog_id: i64,
    key_id: &str,
    actor: &str,
    now: DateTime<Utc>,
) -> Result<ChangesetRow, OkfError> {
    // `handle` is left out, so its column default draws it; `RETURNING`
    // hands the new row back, handle included.
    let row = sqlx::query_as::<_, RawChangeset>(concat!(
        "INSERT INTO changesets (catalog_id, key_id, actor, state, created_at, updated_at) \
         VALUES ($1, $2, $3, 'open', $4, $4) \
         RETURNING ",
        changeset_columns!()
    ))
    .bind(catalog_id)
    .bind(key_id)
    .bind(actor)
    .bind(now)
    .fetch_one(&mut *conn)
    .await?;
    ChangesetRow::try_from(row)
}

/// Can `handle` be one the server issued? Postgres draws each as
/// `gen_random_uuid()::text` (migration 0003): 36 characters of lowercase
/// hex in the 8-4-4-4-12 grouping. Anything else, a NUL included (which
/// Postgres refuses in text), names no changeset.
///
/// # Arguments
/// * `handle` - the handle, as a client sent it.
///
/// # Returns
/// `true` if `handle` has the shape of an issued handle.
#[tracing::instrument(level = "debug", skip_all)]
pub fn is_issued_handle(handle: &str) -> bool {
    // A byte loop, so a multi-byte character can never split a group.
    handle.len() == 36
        && handle.bytes().enumerate().all(|(at, byte)| match at {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte),
        })
}

/// A changeset by its handle, in any state.
///
/// # Arguments
/// * `executor` - a pool, connection or transaction.
/// * `handle` - the handle, as a client sent it.
///
/// # Returns
/// The row, or `None` if no changeset has that handle, or the handle is not
/// of the shape the server issues ([`is_issued_handle`]; a NUL would be a
/// database error, issue #339). One lookup in the handle's unique index.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "changesets.by_handle"))]
pub async fn by_handle<'e>(
    executor: impl PgExecutor<'e>,
    handle: &str,
) -> Result<Option<ChangesetRow>, OkfError> {
    if !is_issued_handle(handle) {
        return Ok(None);
    }
    let row = sqlx::query_as::<_, RawChangeset>(concat!(
        "SELECT ",
        changeset_columns!(),
        " FROM changesets WHERE handle = $1"
    ))
    .bind(handle)
    .fetch_optional(executor)
    .await?;
    row.map(ChangesetRow::try_from).transpose()
}

/// Locks one changeset's row for the rest of the transaction, then reads it
/// again.
///
/// Every transaction that changes a changeset's entries or state takes this
/// lock after [`lock_catalog`], so writes to one changeset, its publish and
/// its discard run one at a time. The row is read *after* the lock is
/// granted: under Postgres' default isolation (read committed) each statement
/// sees everything committed before it started, so the `version` and
/// `state` returned are those the previous holder committed.
///
/// # Arguments
/// * `conn` - the transaction's connection (pass `&mut tx`).
/// * `id` - the changeset's id.
///
/// # Returns
/// The row as it is now, or `None` if it no longer exists (its catalog was
/// deleted).
///
/// # Errors
/// [`OkfError::Database`] (a deadlock maps to [`OkfError::Busy`], as for
/// every query here).
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "changesets.lock"))]
pub async fn lock(conn: &mut PgConnection, id: i64) -> Result<Option<ChangesetRow>, OkfError> {
    // `fetch_optional` gives `None` when no row matched, so a vanished
    // changeset is not an error here; the caller decides.
    let locked: Option<i64> =
        sqlx::query_scalar("SELECT id FROM changesets WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *conn)
            .await?;
    if locked.is_none() {
        return Ok(None);
    }
    // `&mut *conn` reborrows the connection for one more query.
    by_id(&mut *conn, id).await
}

/// A changeset's entries.
///
/// # Arguments
/// * `executor` - a pool, connection or transaction.
/// * `changeset_id` - the changeset.
///
/// # Returns
/// The entries, by path (the primary key's order, so no sort).
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "changesets.entries", changeset_id = changeset_id))]
pub async fn entries<'e>(
    executor: impl PgExecutor<'e>,
    changeset_id: i64,
) -> Result<Vec<EntryRow>, OkfError> {
    let rows = sqlx::query_as::<_, EntryRow>(
        "SELECT path, base_revision, content, executable, replay FROM changeset_entries \
         WHERE changeset_id = $1 ORDER BY path",
    )
    .bind(changeset_id)
    .fetch_all(executor)
    .await?;
    Ok(rows)
}

/// Bumps an open changeset's `version` and `updated_at`, taking its row
/// lock: step 2 of the lock order, before any entry is written.
///
/// # Errors
/// [`OkfError::Busy`] if it is not open (any more); [`OkfError::Database`].
#[tracing::instrument(level = "trace", skip_all, fields(db.system.name = "postgresql", db.operation.name = "changesets.touch", changeset_id = changeset_id))]
async fn touch(
    conn: &mut PgConnection,
    changeset_id: i64,
    now: DateTime<Utc>,
) -> Result<(), OkfError> {
    let touched = sqlx::query(
        "UPDATE changesets SET version = version + 1, updated_at = $2 \
         WHERE id = $1 AND state = 'open'",
    )
    .bind(changeset_id)
    .bind(now)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if touched == 0 {
        return Err(no_longer_open(changeset_id));
    }
    Ok(())
}

/// Stores an entry, replacing the changeset's entry for the same path, and
/// bumps the changeset's `version`.
///
/// # Arguments
/// * `conn` - the transaction's connection, after [`lock_catalog`].
/// * `changeset_id` - an open changeset.
/// * `entry` - the path, base revision, content, execute bit and replay.
/// * `now` - the time (server clock).
///
/// # Errors
/// [`OkfError::Busy`] if the changeset is no longer open (callers take
/// [`lock`] first and refuse a closed one, so this is a backstop);
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "changesets.upsert_entry", changeset_id = changeset_id))]
pub async fn upsert_entry(
    conn: &mut PgConnection,
    changeset_id: i64,
    entry: &EntryRow,
    now: DateTime<Utc>,
) -> Result<(), OkfError> {
    touch(conn, changeset_id, now).await?;
    sqlx::query(
        "INSERT INTO changeset_entries \
         (changeset_id, path, base_revision, content, executable, replay) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (changeset_id, path) DO UPDATE SET \
         base_revision = EXCLUDED.base_revision, content = EXCLUDED.content, \
         executable = EXCLUDED.executable, replay = EXCLUDED.replay",
    )
    .bind(changeset_id)
    .bind(&entry.path)
    .bind(entry.base_revision)
    // `as_deref`: `&Option<Vec<u8>>` to `Option<&[u8]>`, binding without a copy.
    .bind(entry.content.as_deref())
    .bind(entry.executable)
    .bind(&entry.replay)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Drops entries and bumps the changeset's `version`.
///
/// # Arguments
/// * `conn` - the transaction's connection, after [`lock_catalog`].
/// * `changeset_id` - an open changeset.
/// * `paths` - the paths to drop; `None` drops every entry.
/// * `now` - the time (server clock).
///
/// # Returns
/// How many entries were dropped (a named path with no entry counts 0).
///
/// # Errors
/// [`OkfError::Busy`] if the changeset is no longer open;
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "changesets.remove_entries", changeset_id = changeset_id))]
pub async fn remove_entries(
    conn: &mut PgConnection,
    changeset_id: i64,
    paths: Option<&[String]>,
    now: DateTime<Utc>,
) -> Result<u64, OkfError> {
    touch(conn, changeset_id, now).await?;
    // `$2::text[] IS NULL`: a NULL array (`None`) matches every path.
    let removed = sqlx::query(
        "DELETE FROM changeset_entries WHERE changeset_id = $1 \
         AND ($2::text[] IS NULL OR path = ANY($2))",
    )
    .bind(changeset_id)
    .bind(paths)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(removed)
}

/// Closes an open changeset (publish or discard), bumping its `version`.
///
/// # Arguments
/// * `conn` - the transaction's connection, after [`lock_catalog`].
/// * `changeset_id` - the changeset.
/// * `state` - the new state.
/// * `now` - the time (server clock).
///
/// # Errors
/// [`OkfError::Refused`] if it is not open; [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "changesets.set_state", changeset_id = changeset_id))]
pub async fn set_state(
    conn: &mut PgConnection,
    changeset_id: i64,
    state: ChangesetState,
    now: DateTime<Utc>,
) -> Result<(), OkfError> {
    let changed = sqlx::query(
        "UPDATE changesets SET state = $2, version = version + 1, updated_at = $3 \
         WHERE id = $1 AND state = 'open'",
    )
    .bind(changeset_id)
    .bind(state.as_str())
    .bind(now)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if changed == 0 {
        return Err(OkfError::Refused {
            reason: format!("changeset {changeset_id} is not open"),
        });
    }
    Ok(())
}

/// Every open changeset in `catalog_ids`, optionally only one key's, empty
/// ones included: what `list_changesets` shows for a named catalog (M8
/// Decision 8).
///
/// # Arguments
/// * `accounts` - the account database (catalog names and key labels).
/// * `db` - the store holding the changesets.
/// * `catalog_ids` - the catalogs to list (the ones the calling key reaches).
/// * `key_id` - only this key's changesets (`None`: every key's, for an
///   `admin` caller), so other keys' paths never leave the database.
///
/// # Returns
/// The changesets with their catalog's name and sorted paths, by catalog
/// name, then oldest change first.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "changesets.list_open"))]
pub async fn list_open(
    accounts: &AccountDb,
    db: &CatalogDb,
    catalog_ids: &[i64],
    key_id: Option<&str>,
) -> Result<Vec<ListedRow>, OkfError> {
    listed(accounts, db, Some(catalog_ids), None, key_id).await
}

/// Every open changeset in the catalogs called `names` (every catalog for
/// `None`), optionally only one key's, empty ones included: what
/// `list_changesets` shows without a catalog (M18 spec Decision 7), read
/// without enumerating the catalogs first.
///
/// # Arguments
/// * `accounts` - the account database (catalog names and key labels).
/// * `db` - the store holding the changesets.
/// * `names` - the catalogs the calling key reaches, or `None` for a key
///   that reaches every catalog. The caller still checks each row's catalog
///   against its gates.
/// * `key_id` - as for [`list_open`].
///
/// # Returns
/// As [`list_open`].
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "changesets.open_for_catalogs"))]
pub async fn open_for_catalogs(
    accounts: &AccountDb,
    db: &CatalogDb,
    names: Option<&[String]>,
    key_id: Option<&str>,
) -> Result<Vec<ListedRow>, OkfError> {
    // Drop what Postgres could not take (a NUL) or cannot match, as
    // `catalogs::rows_by_names` does.
    let valid: Option<Vec<&str>> = names.map(|names| {
        names
            .iter()
            .map(String::as_str)
            .filter(|name| catalogs::is_valid_name(name))
            .collect()
    });
    // A list with nothing valid in it matches nothing: skip the query.
    if valid.as_ref().is_some_and(Vec::is_empty) {
        return Ok(Vec::new());
    }
    listed(accounts, db, None, valid.as_deref(), key_id).await
}

/// What [`list_open`] and [`open_for_catalogs`] share: a `NULL` filter (a
/// Rust `None`) is turned off.
///
/// The listing needs both sides, and no one statement may join them (plan
/// decision P1), so it is three reads joined here by id:
/// 1. when `names` narrows the list, the account database resolves the names
///    to catalog ids (`catalogs`), so the store can filter by id;
/// 2. the store reads the open changesets and their paths (`changesets`,
///    `changeset_entries`);
/// 3. the account database supplies each catalog's name, in name order, and
///    each key's label (`catalogs`, `api_keys`), one `= ANY($1)` query each.
///
/// The result order is the old statement's: catalog name, then `updated_at`,
/// then id. The name order is the one Postgres gives (`ORDER BY name` on the
/// account side), not Rust's byte order, so a database collation that
/// orders names differently from bytes still gives the same list. A
/// changeset whose catalog row is gone is left out, as the inner join did,
/// and one whose key row is gone has a `None` key name, as the left join gave.
#[tracing::instrument(level = "trace", skip_all)]
async fn listed(
    accounts: &AccountDb,
    db: &CatalogDb,
    catalog_ids: Option<&[i64]>,
    names: Option<&[&str]>,
    key_id: Option<&str>,
) -> Result<Vec<ListedRow>, OkfError> {
    // Step 1: names to ids, only when a name filter narrows the list. The
    // id filter is the catalogs both filters allow.
    let narrowed: Option<Vec<i64>> = match names {
        Some(names) => {
            let found: Vec<i64> = sqlx::query_scalar(
                "SELECT id FROM catalogs \
                 WHERE name = ANY($1) AND deleted_at IS NULL \
                   AND ($2::bigint[] IS NULL OR id = ANY($2))",
            )
            .bind(names)
            .bind(catalog_ids)
            .fetch_all(accounts.executor())
            .await?;
            // Nothing matched: no store query for an empty id list.
            if found.is_empty() {
                return Ok(Vec::new());
            }
            Some(found)
        }
        None => catalog_ids.map(<[i64]>::to_vec),
    };
    // Step 2: the store side. The `LEFT JOIN` keeps changesets without
    // entries; their one joined row has a `NULL` path, which `FILTER` leaves
    // out of `array_agg`, and `COALESCE` turns the resulting `NULL` array
    // into an empty one. Grouping by the primary key `c.id` lets the other
    // `c` columns be selected without listing them in `GROUP BY`. `= ANY($1)`
    // matches a Postgres array, which sqlx binds from a Rust slice. A `NULL`
    // filter turns that filter off.
    let raws = sqlx::query_as::<_, RawListed>(
        "SELECT c.id, c.handle, c.catalog_id, c.key_id, c.actor, c.state, c.version, \
         c.created_at, c.updated_at, \
         COALESCE(array_agg(e.path ORDER BY e.path) FILTER (WHERE e.path IS NOT NULL), \
         '{}') AS paths \
         FROM changesets c \
         LEFT JOIN changeset_entries e ON e.changeset_id = c.id \
         WHERE c.state = 'open' \
         AND ($1::bigint[] IS NULL OR c.catalog_id = ANY($1)) \
         AND ($2::text IS NULL OR c.key_id = $2) \
         GROUP BY c.id",
    )
    .bind(narrowed.as_deref())
    .bind(key_id)
    .fetch_all(db.executor())
    .await?;
    if raws.is_empty() {
        return Ok(Vec::new());
    }
    // Step 3: the account side, one query per kind, by id. Distinct ids keep
    // the arrays short when one catalog or key has many changesets.
    let mut wanted_catalogs: Vec<i64> = raws.iter().map(|raw| raw.changeset.catalog_id).collect();
    wanted_catalogs.sort_unstable();
    wanted_catalogs.dedup();
    let mut wanted_keys: Vec<&str> = raws
        .iter()
        .filter_map(|raw| raw.changeset.key_id.as_deref())
        .collect();
    wanted_keys.sort_unstable();
    wanted_keys.dedup();
    let named: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, name FROM catalogs WHERE id = ANY($1) ORDER BY name")
            .bind(&wanted_catalogs)
            .fetch_all(accounts.executor())
            .await?;
    let key_names: HashMap<String, String> =
        sqlx::query_as("SELECT id, name FROM api_keys WHERE id = ANY($1)")
            .bind(&wanted_keys)
            .fetch_all(accounts.executor())
            .await?
            .into_iter()
            .collect();
    let openers = opener_users(accounts, &wanted_keys).await?;
    // A catalog's position in the name-ordered answer is its sort key; its
    // name goes with it.
    let catalog_rank: HashMap<i64, (usize, String)> = named
        .into_iter()
        .enumerate()
        .map(|(rank, (id, name))| (id, (rank, name)))
        .collect();
    let mut rows = Vec::with_capacity(raws.len());
    for raw in raws {
        // A catalog deleted since the store read has no row: the inner join
        // dropped such a changeset too.
        let Some((rank, catalog)) = catalog_rank.get(&raw.changeset.catalog_id) else {
            continue;
        };
        let key_name = raw
            .changeset
            .key_id
            .as_ref()
            .and_then(|key| key_names.get(key))
            .cloned();
        let user_id = raw
            .changeset
            .key_id
            .as_ref()
            .and_then(|key| openers.get(key))
            .cloned();
        rows.push((
            *rank,
            ListedRow {
                changeset: ChangesetRow::try_from(raw.changeset)?,
                catalog: catalog.clone(),
                key_name,
                user_id,
                paths: raw.paths,
            },
        ));
    }
    rows.sort_by_key(|(rank, row)| (*rank, row.changeset.updated_at, row.changeset.id));
    Ok(rows.into_iter().map(|(_, row)| row).collect())
}

/// The people who opened changesets: the user of each credential.
///
/// A key or grant names its user in `api_keys`; a console session's id is
/// `session:<user name>` ([`crate::auth::SESSION_KEY_PREFIX`]), which names
/// the user directly. Read now, not recorded: an open changeset is not yet
/// history, and the revisions it publishes record their own person.
///
/// # Arguments
/// * `accounts` - the account database.
/// * `key_ids` - the changesets' `key_id` values, distinct.
///
/// # Returns
/// `key_id -> users.id` for the ones that have a user now.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "changesets.opener_users"))]
pub async fn opener_users(
    accounts: &AccountDb,
    key_ids: &[&str],
) -> Result<HashMap<String, String>, OkfError> {
    if key_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let mut found: HashMap<String, String> = sqlx::query_as(
        "SELECT id, user_id FROM api_keys WHERE id = ANY($1) AND user_id IS NOT NULL",
    )
    .bind(key_ids)
    .fetch_all(accounts.executor())
    .await?
    .into_iter()
    .collect();
    // A session's user is named by the id's suffix.
    let sessions: Vec<&str> = key_ids
        .iter()
        .filter_map(|key| key.strip_prefix(crate::auth::SESSION_KEY_PREFIX))
        .collect();
    if !sessions.is_empty() {
        let users: Vec<(String, String)> =
            sqlx::query_as("SELECT name, id FROM users WHERE name = ANY($1)")
                .bind(&sessions)
                .fetch_all(accounts.executor())
                .await?;
        found.extend(
            users
                .into_iter()
                .map(|(name, id)| (format!("{}{name}", crate::auth::SESSION_KEY_PREFIX), id)),
        );
    }
    Ok(found)
}

/// Discards every open changeset without entries whose last change is
/// before `older_than` (design "Session lifecycle": the 24-hour prune).
///
/// Changesets another transaction holds are skipped (`SKIP LOCKED`), so the
/// prune never waits on a lock, and it never discards a changeset under a
/// write. A write resolves its handle without a lock, then takes the row
/// with [`lock`] and reads its state again:
/// - if the write took the row first, the prune skips it, and the write's
///   entry (and `updated_at`) keep it from being pruned later;
/// - if the prune holds the row, [`lock`] waits for it to commit, then reads
///   `discarded`, and the write is refused ("was published or discarded
///   while this write waited"), storing nothing.
///
/// # Arguments
/// * `executor` - the store (a pool, or a connection the caller bounded).
/// * `older_than` - the cutoff.
/// * `now` - the time (server clock), as their `updated_at`.
///
/// # Returns
/// How many were discarded.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "changesets.prune_empty"))]
pub async fn prune_empty<'e>(
    executor: impl PgExecutor<'e>,
    older_than: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<u64, OkfError> {
    let pruned = sqlx::query(
        "UPDATE changesets SET state = 'discarded', version = version + 1, updated_at = $2 \
         WHERE state = 'open' AND id IN (\
             SELECT c.id FROM changesets c \
             WHERE c.state = 'open' AND c.updated_at < $1 \
             AND NOT EXISTS (SELECT 1 FROM changeset_entries e WHERE e.changeset_id = c.id) \
             FOR UPDATE OF c SKIP LOCKED)",
    )
    .bind(older_than)
    .bind(now)
    .execute(executor)
    .await?
    .rows_affected();
    Ok(pruned)
}

/// The statement behind [`open_total`]; `changesets_open` (migration 0018)
/// serves its filter, and a test plans it.
const OPEN_TOTAL_SQL: &str = "SELECT count(*) FROM changesets WHERE state = 'open'";

/// How many changesets are open, in every catalog on one store. One count over the
/// partial index of open rows: the `cluster_stats` housekeeping job stores
/// it for `okf.changesets.open` (M18 spec Decision 16), one store at a time.
///
/// # Arguments
/// * `executor` - the store (a pool, or a connection the caller bounded).
///
/// # Returns
/// The number of open changesets.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "changesets.open_total"))]
pub async fn open_total<'e>(executor: impl PgExecutor<'e>) -> Result<i64, OkfError> {
    Ok(sqlx::query_scalar(OPEN_TOTAL_SQL)
        .fetch_one(executor)
        .await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;
    use crate::store::catalogs;
    use crate::store::catalogs::tests::until_a_lock_is_awaited;
    use chrono::{TimeDelta, TimeZone};
    use sqlx::PgPool;
    use std::time::Duration;

    /// Generous: the tests share a loaded machine.
    const PATIENCE: Duration = Duration::from_secs(30);

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 3, 12, 0, 0).unwrap()
    }

    /// Creates catalog `pg` and the keys `alice` and `bob` (the foreign key
    /// `changesets.key_id` needs them), returning the catalog's id.
    async fn setup(pool: &PgPool) -> i64 {
        let pg = catalogs::create(
            pool,
            &crate::store::catalogs::NewCatalog {
                name: "pg",
                display_name: &crate::labels::DisplayName::default(),
                description: "",
                owner_team: crate::db::TEST_TEAM,
            },
            &crate::store::audit::AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();
        for id in ["alice", "bob"] {
            crate::store::keys::insert_legacy_test_key(
                pool,
                id,
                crate::store::keys::Access::Write,
                id,
                now(),
            )
            .await;
        }
        pg.id
    }

    /// Opens a changeset for `key_id` in its own transaction, in the lock order.
    async fn opened(
        pool: &PgPool,
        catalog_id: i64,
        key_id: &str,
        at: DateTime<Utc>,
    ) -> ChangesetRow {
        let mut tx = pool.begin().await.unwrap();
        lock_catalog(&mut tx, catalog_id).await.unwrap();
        let changeset = open(&mut tx, catalog_id, key_id, key_id, at).await.unwrap();
        tx.commit().await.unwrap();
        changeset
    }

    fn entry(path: &str, text: Option<&str>) -> EntryRow {
        EntryRow {
            path: path.to_string(),
            base_revision: None,
            content: text.map(|text| text.as_bytes().to_vec()),
            executable: false,
            replay: None,
        }
    }

    /// Stores one entry in its own transaction, in the lock order.
    async fn put(
        pool: &PgPool,
        changeset: &ChangesetRow,
        entry: &EntryRow,
        at: DateTime<Utc>,
    ) -> Result<(), OkfError> {
        let mut tx = pool.begin().await?;
        lock_catalog(&mut tx, changeset.catalog_id).await?;
        upsert_entry(&mut tx, changeset.id, entry, at).await?;
        tx.commit().await?;
        Ok(())
    }

    #[test]
    fn only_a_uuid_shaped_handle_can_have_been_issued() {
        assert!(is_issued_handle("0b1e6f2a-9c3d-4e5f-8a7b-123456789abc"));
        for bad in [
            "",
            "no-such-handle",
            "a\0b",
            "0b1e6f2a-9c3d-4e5f-8a7b-123456789abc\0",
            "0B1E6F2A-9C3D-4E5F-8A7B-123456789ABC",
            "0b1e6f2a9c3d4e5f8a7b123456789abc----",
            "0b1e6f2a-9c3d-4e5f-8a7b-123456789ab\u{e9}",
        ] {
            assert!(!is_issued_handle(bad), "{bad:?}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn open_returns_a_fresh_handle_each_time_and_by_handle_finds_it() {
        let db = test_db().await;
        let pg = setup(&db.pool).await;
        let mut tx = db.store().begin().await.unwrap();
        lock_catalog(&mut tx, pg).await.unwrap();

        let first = open(&mut tx, pg, "alice", "agent:a", now()).await.unwrap();
        let second = open(&mut tx, pg, "alice", "agent:a", now()).await.unwrap();
        tx.commit().await.unwrap();

        assert_ne!(first.handle, second.handle);
        assert_ne!(first.id, second.id);
        assert_eq!(
            (first.state, first.key_id.as_deref(), first.actor.as_str()),
            (ChangesetState::Open, Some("alice"), "agent:a")
        );
        assert_eq!(
            by_handle(&db.store(), &first.handle).await.unwrap(),
            Some(first)
        );
        assert_eq!(
            by_handle(&db.store(), "no-such-handle").await.unwrap(),
            None
        );
        // A NUL is no handle, not a database error (issue #339).
        assert_eq!(by_handle(&db.store(), "a\0b").await.unwrap(), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn upsert_replaces_the_entry_and_bumps_the_version() {
        let db = test_db().await;
        let pg = setup(&db.pool).await;
        let changeset = opened(&db.store(), pg, "alice", now()).await;
        let later = now() + TimeDelta::minutes(1);
        let mut replayed = entry("log.md", Some("# Log\n"));
        replayed.replay = Some(serde_json::json!([{"op": "log", "date": "2026-11-03"}]));

        put(
            &db.store(),
            &changeset,
            &entry("a.md", Some("one\n")),
            now(),
        )
        .await
        .unwrap();
        put(
            &db.store(),
            &changeset,
            &entry("a.md", Some("two\n")),
            later,
        )
        .await
        .unwrap();
        put(&db.store(), &changeset, &replayed, later)
            .await
            .unwrap();

        let stored = entries(&db.store(), changeset.id).await.unwrap();
        assert_eq!(stored, [entry("a.md", Some("two\n")), replayed]);
        let after = by_id(&db.store(), changeset.id).await.unwrap().unwrap();
        assert_eq!((after.version, after.updated_at), (3, later));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remove_entries_drops_the_named_paths_or_all_of_them() {
        let db = test_db().await;
        let pg = setup(&db.pool).await;
        let changeset = opened(&db.store(), pg, "alice", now()).await;
        for path in ["a.md", "b.md", "c.md"] {
            put(&db.store(), &changeset, &entry(path, Some("x\n")), now())
                .await
                .unwrap();
        }
        let named = ["b.md".to_string(), "nope.md".to_string()];

        let mut tx = db.store().begin().await.unwrap();
        lock_catalog(&mut tx, pg).await.unwrap();
        let one = remove_entries(&mut tx, changeset.id, Some(&named), now())
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let left: Vec<String> = entries(&db.store(), changeset.id)
            .await
            .unwrap()
            .into_iter()
            .map(|entry| entry.path)
            .collect();
        let mut tx = db.store().begin().await.unwrap();
        lock_catalog(&mut tx, pg).await.unwrap();
        let rest = remove_entries(&mut tx, changeset.id, None, now())
            .await
            .unwrap();
        tx.commit().await.unwrap();

        assert_eq!(one, 1);
        assert_eq!(left, ["a.md", "c.md"]);
        assert_eq!(rest, 2);
        assert!(entries(&db.store(), changeset.id).await.unwrap().is_empty());
        // Three puts and two removals.
        assert_eq!(
            by_id(&db.store(), changeset.id)
                .await
                .unwrap()
                .unwrap()
                .version,
            5
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_state_closes_a_changeset_once_and_refuses_a_late_write() {
        let db = test_db().await;
        let pg = setup(&db.pool).await;
        let changeset = opened(&db.store(), pg, "alice", now()).await;

        let mut tx = db.store().begin().await.unwrap();
        lock_catalog(&mut tx, pg).await.unwrap();
        set_state(&mut tx, changeset.id, ChangesetState::Published, now())
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let mut tx = db.store().begin().await.unwrap();
        let twice = set_state(&mut tx, changeset.id, ChangesetState::Discarded, now()).await;
        drop(tx);
        let late_write = put(&db.store(), &changeset, &entry("a.md", Some("x\n")), now()).await;

        let closed = by_id(&db.store(), changeset.id).await.unwrap().unwrap();
        assert_eq!(closed.state, ChangesetState::Published);
        assert!(matches!(twice, Err(OkfError::Refused { .. })));
        assert!(matches!(late_write, Err(OkfError::Busy { .. })));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_open_lists_open_changesets_with_their_sorted_paths() {
        let db = test_db().await;
        let pg = setup(&db.pool).await;
        let old = now() - TimeDelta::hours(3);
        let edited = opened(&db.store(), pg, "alice", old).await;
        put(&db.store(), &edited, &entry("b.md", Some("b\n")), old)
            .await
            .unwrap();
        put(&db.store(), &edited, &entry("a.md", None), old)
            .await
            .unwrap();
        let empty = opened(&db.store(), pg, "alice", now()).await;
        let published = opened(&db.store(), pg, "alice", old).await;
        put(&db.store(), &published, &entry("a.md", Some("a\n")), old)
            .await
            .unwrap();
        let mut tx = db.store().begin().await.unwrap();
        lock_catalog(&mut tx, pg).await.unwrap();
        set_state(&mut tx, published.id, ChangesetState::Published, old)
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let rows = list_open(
            &AccountDb::of(&db.pool),
            &CatalogDb::main_of(&db.pool),
            &[pg],
            None,
        )
        .await
        .unwrap();
        let owner = edited.key_id.as_deref();
        let by_owner = list_open(
            &AccountDb::of(&db.pool),
            &CatalogDb::main_of(&db.pool),
            &[pg],
            owner,
        )
        .await
        .unwrap();
        let by_other_key = list_open(
            &AccountDb::of(&db.pool),
            &CatalogDb::main_of(&db.pool),
            &[pg],
            Some("nobody"),
        )
        .await
        .unwrap();
        let by_other_catalog = list_open(
            &AccountDb::of(&db.pool),
            &CatalogDb::main_of(&db.pool),
            &[pg + 1],
            None,
        )
        .await
        .unwrap();
        let no_catalogs = list_open(
            &AccountDb::of(&db.pool),
            &CatalogDb::main_of(&db.pool),
            &[],
            None,
        )
        .await
        .unwrap();

        let ids: Vec<i64> = rows.iter().map(|row| row.changeset.id).collect();
        assert_eq!(ids, [edited.id, empty.id], "oldest change first");
        assert_eq!(rows[0].catalog, "pg");
        assert_eq!(rows[0].paths, ["a.md", "b.md"]);
        assert!(rows[1].paths.is_empty());
        assert_eq!(by_owner, rows);
        assert!(by_other_key.is_empty());
        assert!(by_other_catalog.is_empty());
        assert!(no_catalogs.is_empty());
    }

    /// M18 Task 10: the list without a catalog, narrowed by name in SQL.
    #[tokio::test(flavor = "multi_thread")]
    async fn open_for_catalogs_narrows_by_name_or_lists_every_catalog() {
        let db = test_db().await;
        let pg = setup(&db.pool).await;
        let first = opened(&db.store(), pg, "alice", now() - TimeDelta::hours(1)).await;
        let second = opened(&db.store(), pg, "alice", now()).await;
        let names =
            |list: &[&str]| -> Vec<String> { list.iter().map(|name| (*name).to_owned()).collect() };

        let every = open_for_catalogs(
            &AccountDb::of(&db.pool),
            &CatalogDb::main_of(&db.pool),
            None,
            None,
        )
        .await
        .unwrap();
        let named = open_for_catalogs(
            &AccountDb::of(&db.pool),
            &CatalogDb::main_of(&db.pool),
            Some(&names(&["pg", "nope"])),
            None,
        )
        .await
        .unwrap();
        let other = open_for_catalogs(
            &AccountDb::of(&db.pool),
            &CatalogDb::main_of(&db.pool),
            Some(&names(&["work"])),
            None,
        )
        .await
        .unwrap();
        let malformed = open_for_catalogs(
            &AccountDb::of(&db.pool),
            &CatalogDb::main_of(&db.pool),
            Some(&names(&["p\0g", "P G"])),
            None,
        )
        .await
        .unwrap();
        let none = open_for_catalogs(
            &AccountDb::of(&db.pool),
            &CatalogDb::main_of(&db.pool),
            Some(&[]),
            None,
        )
        .await
        .unwrap();
        let by_other_key = open_for_catalogs(
            &AccountDb::of(&db.pool),
            &CatalogDb::main_of(&db.pool),
            None,
            Some("nobody"),
        )
        .await
        .unwrap();

        let ids: Vec<i64> = every.iter().map(|row| row.changeset.id).collect();
        assert_eq!(ids, [first.id, second.id]);
        assert_eq!(named, every);
        assert!(other.is_empty());
        assert!(
            malformed.is_empty(),
            "a NUL matches nothing and fails nothing"
        );
        assert!(none.is_empty());
        assert!(by_other_key.is_empty());
    }

    /// Issue #963 (M21 Task 5): the listing's join is split into a store read
    /// and two account reads. On two catalogs, two keys and a deleted key it
    /// gives what the one joined statement gave: catalog name order (not
    /// creation order), oldest change first within a catalog, names and key
    /// labels filled in, and a null key name for the deleted key.
    #[tokio::test(flavor = "multi_thread")]
    async fn changeset_listing_names_catalogs_and_keys_after_the_split() {
        let db = test_db().await;
        let pg = setup(&db.pool).await;
        // Created after `pg`, so it has the larger id but sorts first by name.
        let alpha = catalogs::create(
            &db.pool,
            &crate::store::catalogs::NewCatalog {
                name: "alpha",
                display_name: &crate::labels::DisplayName::default(),
                description: "",
                owner_team: crate::db::TEST_TEAM,
            },
            &crate::store::audit::AuditActor::cli(),
            now(),
        )
        .await
        .unwrap()
        .id;
        assert!(alpha > pg);
        let hour = |hours: i64| now() - TimeDelta::hours(hours);
        // Two changesets tie on `updated_at` in `pg`: the id breaks the tie.
        let pg_bob_old = opened(&db.store(), pg, "bob", hour(2)).await;
        let pg_alice_tie = opened(&db.store(), pg, "alice", hour(1)).await;
        let pg_bob_tie = opened(&db.store(), pg, "bob", hour(1)).await;
        // The newest change, so only the catalog name puts it first.
        let alpha_alice = opened(&db.store(), alpha, "alice", hour(0)).await;
        put(
            &db.store(),
            &alpha_alice,
            &entry("z.md", Some("z\n")),
            hour(0),
        )
        .await
        .unwrap();
        put(
            &db.store(),
            &alpha_alice,
            &entry("m.md", Some("m\n")),
            hour(0),
        )
        .await
        .unwrap();
        // A published changeset is not listed.
        let closed = opened(&db.store(), pg, "alice", hour(4)).await;
        let mut tx = db.store().begin().await.unwrap();
        lock_catalog(&mut tx, pg).await.unwrap();
        set_state(&mut tx, closed.id, ChangesetState::Published, hour(4))
            .await
            .unwrap();
        tx.commit().await.unwrap();
        // The key row goes. No foreign key nulls the id its changesets keep
        // (0041): they are listed with no key name, a deleted key's.
        sqlx::query("DELETE FROM api_keys WHERE id = 'bob'")
            .execute(&db.pool)
            .await
            .unwrap();
        let accounts = AccountDb::of(&db.pool);
        let store = CatalogDb::main_of(&db.pool);

        let by_id = list_open(&accounts, &store, &[pg, alpha], None)
            .await
            .unwrap();
        let by_name = open_for_catalogs(&accounts, &store, None, None)
            .await
            .unwrap();
        let both = open_for_catalogs(&accounts, &store, Some(&["alpha".to_owned()]), None)
            .await
            .unwrap();
        // Names and ids narrow together: `pg` is named, but only `alpha`'s id is allowed.
        let disjoint = listed(&accounts, &store, Some(&[alpha]), Some(&["pg"]), None)
            .await
            .unwrap();
        let alice_only = list_open(&accounts, &store, &[pg, alpha], Some("alice"))
            .await
            .unwrap();

        let shape: Vec<(i64, &str, Option<&str>, Vec<&str>)> = by_id
            .iter()
            .map(|row| {
                (
                    row.changeset.id,
                    row.catalog.as_str(),
                    row.key_name.as_deref(),
                    row.paths.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        assert_eq!(
            shape,
            [
                (alpha_alice.id, "alpha", Some("alice"), vec!["m.md", "z.md"]),
                (pg_bob_old.id, "pg", None, vec![]),
                (pg_alice_tie.id, "pg", Some("alice"), vec![]),
                (pg_bob_tie.id, "pg", None, vec![]),
            ]
        );
        assert_eq!(by_name, by_id);
        assert_eq!(by_id[1].changeset.key_id.as_deref(), Some("bob"));
        let alpha_ids: Vec<i64> = both.iter().map(|row| row.changeset.id).collect();
        assert_eq!(alpha_ids, [alpha_alice.id]);
        assert!(disjoint.is_empty());
        let alice_ids: Vec<i64> = alice_only.iter().map(|row| row.changeset.id).collect();
        assert_eq!(alice_ids, [alpha_alice.id, pg_alice_tie.id]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn prune_discards_only_empty_changesets_idle_past_the_cutoff() {
        let db = test_db().await;
        let pg = setup(&db.pool).await;
        let old = now() - TimeDelta::hours(25);
        let empty_old = opened(&db.store(), pg, "alice", old).await;
        let empty_new = opened(&db.store(), pg, "alice", now()).await;
        let full_old = opened(&db.store(), pg, "alice", old).await;
        put(&db.store(), &full_old, &entry("a.md", Some("a\n")), old)
            .await
            .unwrap();

        let pruned = prune_empty(&db.store(), now() - TimeDelta::hours(24), now())
            .await
            .unwrap();

        let state = |row: Option<ChangesetRow>| row.unwrap().state;
        assert_eq!(pruned, 1);
        assert_eq!(
            state(by_id(&db.store(), empty_old.id).await.unwrap()),
            ChangesetState::Discarded
        );
        assert_eq!(
            state(by_id(&db.store(), empty_new.id).await.unwrap()),
            ChangesetState::Open
        );
        assert_eq!(
            state(by_id(&db.store(), full_old.id).await.unwrap()),
            ChangesetState::Open
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_prune_passes_by_a_changeset_a_write_holds() {
        let db = test_db().await;
        let pg = setup(&db.pool).await;
        let existing = opened(&db.store(), pg, "alice", now()).await;
        let mut writer = db.store().begin().await.unwrap();
        lock_catalog(&mut writer, pg).await.unwrap();
        // As a write takes it: the row stays locked until `writer` ends.
        let found = lock(&mut writer, existing.id)
            .await
            .unwrap()
            .expect("the changeset exists");
        let cutoff = now() + TimeDelta::hours(1);

        // `SKIP LOCKED`: the prune neither waits for `writer` nor discards
        // the changeset it holds, though it is empty and idle past the cutoff.
        let while_held = tokio::time::timeout(PATIENCE, prune_empty(&db.store(), cutoff, now()))
            .await
            .expect("the prune must skip the held row, not wait for it")
            .unwrap();
        let state_while_held = by_id(&db.store(), existing.id)
            .await
            .unwrap()
            .unwrap()
            .state;
        writer.rollback().await.unwrap();
        let after = prune_empty(&db.store(), cutoff, now()).await.unwrap();

        assert_eq!(found.id, existing.id);
        assert_eq!((while_held, state_while_held), (0, ChangesetState::Open));
        assert_eq!(after, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_deadlock_reported_by_postgres_is_busy() {
        let db = test_db().await;

        // `RAISE ... USING ERRCODE` makes Postgres fail with that SQLSTATE,
        // as if it had picked this statement as a deadlock victim.
        let error: OkfError =
            sqlx::query("DO $$ BEGIN RAISE EXCEPTION 'victim' USING ERRCODE = '40P01'; END $$")
                .execute(&db.pool)
                .await
                .unwrap_err()
                .into();

        assert_eq!(error.kind(), "Busy");
        assert!(!error.is_internal());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_delete_waits_for_a_write_then_refuses_its_work() {
        let db = test_db().await;
        let pg = setup(&db.pool).await;
        // `oneshot` channels: one message each way, so the test decides when
        // the writer commits.
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
        let (commit_tx, commit_rx) = tokio::sync::oneshot::channel::<()>();
        let pool = db.store();
        let writer = tokio::spawn(async move {
            let mut tx = pool.begin().await?;
            lock_catalog(&mut tx, pg).await?;
            let changeset = open(&mut tx, pg, "alice", "alice", now()).await?;
            upsert_entry(&mut tx, changeset.id, &entry("a.md", Some("x\n")), now()).await?;
            ready_tx.send(()).expect("the test waits");
            commit_rx.await.expect("the test signals");
            tx.commit().await?;
            Ok::<(), OkfError>(())
        });
        ready_rx.await.unwrap();
        let pool = db.pool.clone();
        let deleting = tokio::spawn(async move {
            catalogs::delete_settled(
                &pool,
                "pg",
                false,
                &crate::store::audit::AuditActor::cli(),
                chrono::Utc::now(),
            )
            .await
        });
        until_a_lock_is_awaited(&db.pool).await;

        commit_tx.send(()).unwrap();
        let written = tokio::time::timeout(PATIENCE, writer).await;
        let deleted = tokio::time::timeout(PATIENCE, deleting).await;

        written.expect("the write finishes").unwrap().unwrap();
        let deleted = deleted.expect("the delete finishes").unwrap();
        assert!(matches!(deleted, Err(OkfError::Refused { reason }) if reason.contains("--force")));
        assert_eq!(catalogs::list(&db.pool).await.unwrap().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_write_waits_for_a_delete_then_sees_the_catalog_gone() {
        let db = test_db().await;
        let pg = setup(&db.pool).await;
        // The store half of `catalogs::delete`, held open: the catalog's
        // state row `FOR UPDATE`, then its removal.
        let mut deleter = db.store().begin().await.unwrap();
        sqlx::query("SELECT id FROM catalog_state WHERE id = $1 FOR UPDATE")
            .bind(pg)
            .execute(&mut *deleter)
            .await
            .unwrap();
        let pool = db.store();
        let writer = tokio::spawn(async move {
            let mut tx = pool.begin().await?;
            lock_catalog(&mut tx, pg).await?;
            open(&mut tx, pg, "alice", "alice", now()).await?;
            tx.commit().await?;
            Ok::<(), OkfError>(())
        });
        until_a_lock_is_awaited(&db.pool).await;

        sqlx::query("DELETE FROM catalog_state WHERE id = $1")
            .bind(pg)
            .execute(&mut *deleter)
            .await
            .unwrap();
        deleter.commit().await.unwrap();
        let written = tokio::time::timeout(PATIENCE, writer)
            .await
            .expect("the write finishes")
            .unwrap();

        assert!(matches!(written, Err(OkfError::Refused { reason }) if reason == CATALOG_GONE));
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM catalog.changesets")
            .fetch_one(&crate::db::store_pool(&db.pool))
            .await
            .unwrap();
        assert_eq!(left, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn lock_waits_for_the_holder_and_reads_its_commit() {
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
            now(),
        )
        .await
        .unwrap();
        // `changesets.key_id` is a foreign key: the key needs a row.
        crate::store::keys::insert_legacy_test_key(
            &db.pool,
            "writer",
            crate::store::keys::Access::Write,
            "writer",
            now(),
        )
        .await;
        let row = opened(&db.store(), catalog.id, "writer", now()).await;
        let mut holder = db.store().begin().await.unwrap();
        lock(&mut holder, row.id).await.unwrap();
        let pool = db.store();
        // The second locker blocks until `holder` commits.
        let waiting = tokio::spawn(async move {
            let mut tx = pool.begin().await.unwrap();
            let seen = lock(&mut tx, row.id).await.unwrap();
            tx.commit().await.unwrap();
            seen
        });
        until_a_lock_is_awaited(&db.pool).await;

        set_state(&mut holder, row.id, ChangesetState::Discarded, now())
            .await
            .unwrap();
        holder.commit().await.unwrap();
        let seen = tokio::time::timeout(PATIENCE, waiting)
            .await
            .expect("the second locker gets the lock")
            .unwrap()
            .expect("the row still exists");

        assert_eq!(seen.state, ChangesetState::Discarded);
        let mut tx = db.store().begin().await.unwrap();
        assert!(lock(&mut tx, i64::MAX).await.unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn open_total_reads_open_rows_through_the_partial_index() {
        let db = test_db().await;
        let catalog_id = setup(&db.pool).await;
        // A long history of closed changesets, and two open ones.
        sqlx::query(
            "INSERT INTO catalog.changesets (catalog_id, actor, state, created_at, updated_at) \
             SELECT $1, 'alice', 'published', $2, $2 FROM generate_series(1, 20000)",
        )
        .bind(catalog_id)
        .bind(now())
        .execute(&crate::db::store_pool(&db.pool))
        .await
        .unwrap();
        opened(&db.store(), catalog_id, "alice", now()).await;
        opened(&db.store(), catalog_id, "bob", now()).await;
        sqlx::query("ANALYZE catalog.changesets")
            .execute(&crate::db::store_pool(&db.pool))
            .await
            .unwrap();

        let total = open_total(&db.store()).await.unwrap();
        // The statement `open_total` runs, planned.
        let plan: Vec<String> = // `AssertSqlSafe`: the text is a constant of this module.
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("EXPLAIN {OPEN_TOTAL_SQL}")))
            .fetch_all(&db.store())
            .await
            .unwrap();

        assert_eq!(total, 2);
        let plan = plan.join("\n");
        assert!(plan.contains("changesets_open"), "{plan}");
        assert!(!plan.contains("Seq Scan on changesets"), "{plan}");
    }

    /// Issue #642: a changeset whose stored state is not a known spelling
    /// (a hand-edited row) is a database error from the readers, not a panic. (`listed` selects only `open` rows, so a
    /// bad state never reaches it.)
    #[tokio::test(flavor = "multi_thread")]
    async fn a_corrupt_state_is_a_database_error_from_each_reader() {
        let db = test_db().await;
        let pg = setup(&db.pool).await;
        let row = opened(&db.store(), pg, "alice", now()).await;
        crate::store::drop_check_constraints(&db.pool, "catalog.changesets").await;
        sqlx::query("UPDATE catalog.changesets SET state = 'bogus-state' WHERE id = $1")
            .bind(row.id)
            .execute(&crate::db::store_pool(&db.pool))
            .await
            .unwrap();

        let by_id = by_id(&db.store(), row.id).await;
        let by_handle = by_handle(&db.store(), &row.handle).await;

        crate::store::assert_decode_error(by_id, "changesets.state", "bogus-state");
        crate::store::assert_decode_error(by_handle, "changesets.state", "bogus-state");
    }
}
