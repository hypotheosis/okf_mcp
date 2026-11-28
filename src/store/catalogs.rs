//! Catalog rows, the per-catalog lock, and the administrative generation.

use std::time::Duration;

use chrono::{DateTime, Utc};
// `PgExecutor` is "anything a query can run on": a pool, a connection or a
// transaction. Functions generic over it work inside or outside a transaction.
use sqlx::{PgConnection, PgExecutor, PgPool};

use crate::error::OkfError;
use crate::invalidate::{invalidate, Invalidation};
use crate::labels::DisplayName;
use crate::okf::ops::replay::ROOT_HEADING;
use crate::okf::ops::starter_files;
use crate::okf::snapshot::Snapshot;
use crate::okf::workspace::{MemWorkspace, StoredFile};
use crate::store::audit::{self, Action, AuditActor, Entry, People, Target};
use crate::store::ceiling::MemberAccess;
use crate::store::changesets;
use crate::store::cleanups;
use crate::store::documents::{self, Derived, DocumentWrite, RevisionOp, RevisionRecord};
use crate::store::teams;
use crate::stores::bounded::{account_tx, store_tx};
use crate::stores::{placement, CatalogDb, StoreName, Stores};

/// The `revisions.message` of a new catalog's [`starter_files`].
pub const STARTER_MESSAGE: &str = "create catalog";

/// The first key of every catalog lock (`okf` in ASCII), separating this
/// crate's advisory locks from anyone else's on a shared database.
pub(crate) const CATALOG_LOCK_NAMESPACE: i32 = 0x006f_6b66;

/// A catalog's published generation as one committed write left it: what
/// a replica announces so the others refresh that catalog's snapshot
/// (issue #659).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogGeneration {
    /// The catalog's id (`catalogs.id`), never reused, so an announcement
    /// cannot reach a catalog created again under the same name.
    pub catalog_id: i64,
    /// The generation the write committed.
    pub generation: i64,
}

/// A catalog row.
// `derive(sqlx::FromRow)` maps a result row's columns onto the fields by
// name, like building a dataclass from a `dict` row.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Catalog {
    /// Database id.
    pub id: i64,
    /// Unique name, `[a-z0-9-]+`.
    pub name: String,
    /// Its display name; empty when unset.
    pub display_name: String,
    /// Free text.
    pub description: String,
    /// Bumped by every publish and import.
    pub generation: i64,
    /// When it was created (server clock).
    pub created_at: DateTime<Utc>,
    /// The name of the team that owns it; `None` only for a catalog from
    /// before migration 0009 not yet assigned.
    pub owner_team: Option<String>,
    /// The name of the store holding its documents (`catalogs.store`).
    pub store: String,
}

// One SELECT shared by `list` and `find` (a macro so `concat!` can append
// each query's tail at compile time; see `select_users!` in users.rs).
macro_rules! select_catalogs {
    () => {
        "SELECT c.store, c.id, c.name, c.display_name, c.description, 0::bigint AS generation, c.created_at, \
                (SELECT t.name FROM teams t WHERE t.id = c.owner_team_id) AS owner_team \
         FROM catalogs c"
    };
}

/// The `okf.catalog` attribute of an MCP tool call that
/// resolved no catalog (issue #1117). The parentheses keep it out of [`is_valid_name`],
/// so no catalog can ever be named like it.
pub const CATALOG_LABEL_NONE: &str = "(none)";

/// The `okf.catalog` attribute of an MCP tool call that
/// reached several catalogs (issue #1117). Like [`CATALOG_LABEL_NONE`], never a valid name.
pub const CATALOG_LABEL_MULTIPLE: &str = "(multiple)";

/// Is `name` a valid catalog name (non-empty, ASCII lowercase letters, digits, `-`)?
///
/// # Arguments
/// * `name` - the proposed name.
///
/// # Returns
/// `true` if the schema's `CHECK (name ~ '^[a-z0-9-]+$')` would accept it.
#[tracing::instrument(level = "debug", skip_all)]
pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// A catalog to create: `create`'s input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewCatalog<'a> {
    /// The new catalog's name, `[a-z0-9-]+`.
    pub name: &'a str,
    /// Its display name (empty when unset), validated by
    /// [`DisplayName::parse`].
    pub display_name: &'a DisplayName,
    /// Free text.
    pub description: &'a str,
    /// The name of the team that owns it.
    pub owner_team: &'a str,
}

/// Creates a catalog holding only its [`starter_files`] (a root
/// `index.md` and a `log.md`, revision 1 each, `create` revisions with the
/// message [`STARTER_MESSAGE`]) and records `catalog.created`. The catalog's
/// own generation stays 0: an import into it before any publish or import
/// replaces the starter files as if the catalog were empty.
///
/// Creation takes three steps, so the registry (the account database) and
/// the store holding the catalog's data are never written in one
/// transaction (plan Task 10b, decision P9):
///
/// 1. `register`: one account transaction chooses the store, inserts the
///    registry row with `provisioned_at` NULL (a row every lookup and
///    listing treats as absent), and records `catalog.created`.
/// 2. `provision_store_side`: one store transaction takes the catalog's
///    lock on the store, checks that the registry row is still there and
///    still unprovisioned, and writes the starter files.
/// 3. `mark_provisioned`: one account transaction sets `provisioned_at`
///    and announces the catalog to the other replicas.
///
/// A create that stops after step 1 or 2 leaves a half-created row, which
/// the `provision_catalogs` housekeeping task finishes or removes ([`recover`]).
/// This form is for the tests, which hold only the account pool: the store
/// is the `main` store inside the account database, opened on a pool of its
/// own with the store's `search_path` ([`CatalogDb::main_in`]). The server
/// and the command line use [`create_in`], with the registry's handle for
/// whichever store the catalog is placed on.
///
/// # Arguments
/// * `pool` - the database.
/// * `new` - its name, description and owner team.
/// * `actor` - who, for the audit log.
/// * `now` - the creation time (server clock).
///
/// # Returns
/// The new row.
///
/// # Errors
/// [`OkfError::Refused`] for an invalid or taken name;
/// [`OkfError::UnknownName`] for an unknown team; [`OkfError::Busy`] if the
/// recovery task removed the half-created row first (nothing was written:
/// retry); [`OkfError::Database`] otherwise, a failed audit write included.
#[cfg(test)]
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "catalogs.create"))]
pub(crate) async fn create(
    pool: &PgPool,
    new: &NewCatalog<'_>,
    actor: &AuditActor,
    now: DateTime<Utc>,
) -> Result<Catalog, OkfError> {
    let (catalog, store) = register(pool, new, actor, now).await?;
    // This test-only form opens `main` alone. Stop, leaving the half-created
    // row for the recovery task, rather than write a catalog's files to the
    // wrong store.
    if store != StoreName::main() {
        return Err(OkfError::Refused {
            reason: format!(
                "catalog {:?} was placed on a store this form cannot open",
                new.name
            ),
        });
    }
    let db = CatalogDb::main_in(pool);
    finish_create(&db, catalog, actor, now).await
}

/// Creates a catalog on the store the registry holds for its placement:
/// what the server's handlers and the command line call.
///
/// # Arguments
/// * `stores` - the registry of stores (its account database holds the
///   registry row).
/// * `new` - the catalog to create.
/// * `actor` - who, for the audit log.
/// * `now` - the creation time (server clock).
///
/// # Returns
/// The new row.
///
/// # Errors
/// As `create`; also [`OkfError::StoreUnavailable`] when the placed
/// store cannot be opened (the half-created row is left for the recovery
/// task).
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "catalogs.create_in"))]
pub async fn create_in(
    stores: &Stores,
    new: &NewCatalog<'_>,
    actor: &AuditActor,
    now: DateTime<Utc>,
) -> Result<Catalog, OkfError> {
    let accounts = stores.accounts();
    let (catalog, store) = register(accounts.executor(), new, actor, now).await?;
    let db = stores.get(&store).await?;
    finish_create(&db, catalog, actor, now).await
}

/// Steps 2 and 3 of `create` on the store handle `db`.
///
/// # Errors
/// As `create`.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "catalogs.finish_create", catalog_id = catalog.id))]
async fn finish_create(
    db: &CatalogDb,
    catalog: Catalog,
    actor: &AuditActor,
    now: DateTime<Utc>,
) -> Result<Catalog, OkfError> {
    provision_store_side(db, &catalog, &actor.label, now, std::future::ready(())).await?;
    mark_provisioned(db.accounts().executor(), catalog.id, now).await?;
    Ok(catalog)
}

/// Step 1 of `create`: inserts the registry row, unprovisioned, with the
/// store placement chose for the owner team, and records `catalog.created`,
/// in one account transaction. `placement::choose` runs in this
/// transaction so the choice and the insert read one snapshot.
///
/// # Arguments
/// * `pool` - the account database.
/// * `new` - the catalog to create.
/// * `actor` - who, for the audit log.
/// * `now` - the creation time (`created_at`).
///
/// # Returns
/// The new row and the store it was placed on.
///
/// # Errors
/// As `create`, less the abort of step 2.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "catalogs.register"))]
pub(crate) async fn register(
    pool: &PgPool,
    new: &NewCatalog<'_>,
    actor: &AuditActor,
    now: DateTime<Utc>,
) -> Result<(Catalog, StoreName), OkfError> {
    let NewCatalog {
        name,
        display_name,
        description,
        owner_team,
    } = *new;
    if !is_valid_name(name) {
        return Err(OkfError::Refused {
            reason: format!("catalog name {name:?} must be lowercase letters, digits and '-'"),
        });
    }
    // Dropping `tx` without `commit()` (an early `return` or `?`) rolls back.
    let mut tx = pool.begin().await?;
    // `&mut tx` is a `&mut Transaction`, which Rust converts to the
    // `&mut PgConnection` the helper takes (deref coercion).
    audit::lock_people(
        &mut tx,
        actor,
        &People {
            teams: &[owner_team],
            ..People::default()
        },
    )
    .await?;
    let team_id = teams::id_of(&mut tx, owner_team).await?;
    // Where the catalog lives is decided once, here, and recorded on its
    // row: a later change of the default or of a pin moves no catalog.
    let store = placement::choose(&mut tx, owner_team).await?;
    // `RETURNING` cannot run the owner subquery of `select_catalogs!` on the
    // new row, but the owner's name is the one just resolved: hand it back
    // as `$5`. `provisioned_at` is written as NULL (not left to its column
    // default, which is `now()` for release-1 replicas): this row is not a
    // catalog until step 3.
    let inserted = sqlx::query_as::<_, Catalog>(
        "INSERT INTO catalogs (name, description, created_at, owner_team_id, display_name, \
                               store, provisioned_at) \
         VALUES ($1, $2, $3, $4, $6, $7, NULL) \
         RETURNING id, name, display_name, description, 0::bigint AS generation, created_at, \
                   $5::text AS owner_team, store",
    )
    .bind(name)
    .bind(description)
    .bind(now)
    .bind(&team_id)
    .bind(owner_team)
    .bind(display_name.as_str())
    .bind(store.as_str())
    // `&mut *tx` borrows the transaction's connection: the query runs inside it.
    .fetch_one(&mut *tx)
    .await;
    let catalog = match inserted {
        Ok(catalog) => catalog,
        Err(error) if is_unique_violation(&error) => {
            return Err(OkfError::Refused {
                reason: format!("catalog {name:?} already exists"),
            })
        }
        Err(error) => return Err(error.into()),
    };
    record_catalog(
        &mut tx,
        actor,
        CatalogEvent {
            action: Action::CatalogCreated,
            catalog_id: catalog.id,
            catalog: name,
            details: serde_json::json!({}),
        },
        now,
    )
    .await?;
    tx.commit().await?;
    Ok((catalog, store))
}

/// What the registry says about a catalog, read by [`registration`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum Registration {
    /// No row: the recovery task removed it.
    Gone,
    /// `provisioned_at` is set: the catalog is whole.
    Provisioned,
    /// Half-created: the row's name and owner team, for finishing it.
    Unprovisioned {
        name: String,
        owner_team_id: Option<String>,
    },
}

/// Reads the registry row of catalog `id` and locks it (`FOR UPDATE`).
///
/// # Arguments
/// * `conn` - a connection on the account database (an account
///   transaction: the row stays locked until it ends).
/// * `id` - the catalog's id.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "catalogs.registration", catalog_id = id))]
async fn registration(conn: &mut PgConnection, id: i64) -> Result<Registration, OkfError> {
    let row: Option<(String, Option<String>, bool)> = sqlx::query_as(
        "SELECT name, owner_team_id, provisioned_at IS NOT NULL \
         FROM catalogs WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(conn)
    .await?;
    Ok(match row {
        None => Registration::Gone,
        Some((_, _, true)) => Registration::Provisioned,
        Some((name, owner_team_id, false)) => Registration::Unprovisioned {
            name,
            owner_team_id,
        },
    })
}

/// Whether catalog `id` has its store side: the data `create` writes
/// under the catalog's lock: its `catalog.catalog_state` row, which
/// [`write_starter_files`] writes first, in the same transaction as the
/// starter files, so the row says the whole store side is there. This is the
/// only place the check lives.
///
/// # Arguments
/// * `catalog_db` - a connection on the store's database (the transaction
///   that holds the catalog's lock passes its own).
/// * `id` - the catalog's id.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "catalogs.store_side_exists", catalog_id = id))]
pub(crate) async fn store_side_exists(
    catalog_db: &mut PgConnection,
    id: i64,
) -> Result<bool, OkfError> {
    Ok(
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM catalog_state WHERE id = $1)")
            .bind(id)
            .fetch_one(catalog_db)
            .await?,
    )
}

/// Step 2 of `create` (decision P9): in one store transaction, takes the
/// catalog's lock, checks that the registry row still exists and is still
/// unprovisioned, writes the starter files, and commits.
///
/// # Arguments
/// * `db` - the store the catalog was placed on.
/// * `catalog` - the registered row.
/// * `actor` - the label recorded on the starter revisions.
/// * `now` - the creation time.
/// * `hold` - awaited after the files are written and before the commit,
///   with the lock held: the tests' way to stall a create without a sleep.
///   Production passes an already-ready future.
///
/// # Errors
/// [`OkfError::Busy`] if the row is gone (the recovery task removed it
/// while this waited for the lock): nothing was written. Otherwise
/// [`OkfError::Database`] or [`OkfError::StoreUnavailable`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "catalogs.provision_store_side", catalog_id = catalog.id))]
async fn provision_store_side(
    db: &CatalogDb,
    catalog: &Catalog,
    actor: &str,
    now: DateTime<Utc>,
    hold: impl std::future::Future<Output = ()>,
) -> Result<(), OkfError> {
    // The account connection first, then the store's, as every change that
    // uses both takes them; the database locks go the other way, the
    // catalog's advisory lock before the registry row (as `recover` does).
    let mut account_tx = db.accounts().begin().await?;
    let mut tx = db.begin().await?;
    lock(&mut tx, catalog.id).await?;
    match registration(&mut account_tx, catalog.id).await? {
        Registration::Unprovisioned { .. } => {}
        Registration::Gone => {
            return Err(OkfError::Busy {
                reason: format!("catalog {:?} was not finished in time", catalog.name),
            })
        }
        // Only this create writes the store side, and it has not yet.
        Registration::Provisioned => return Ok(()),
    }
    write_starter_files(&mut tx, catalog, actor, now).await?;
    hold.await;
    tx.commit().await?;
    // Nothing was written on the account side; the transaction only held the
    // registry row against a concurrent finish.
    account_tx.commit().await?;
    Ok(())
}

/// Sets `provisioned_at` on an unprovisioned row and announces the catalog
/// and its owner team to the other replicas, in one account transaction.
/// A row that is already provisioned (the recovery task finished it first)
/// is left as it is.
///
/// # Arguments
/// * `conn` - a connection on the account database.
/// * `id` - the catalog's id.
/// * `now` - the time to record.
///
/// # Errors
/// [`OkfError::Busy`] if the row is gone; [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "catalogs.mark_provisioned", catalog_id = id))]
async fn mark_provisioned_in(
    conn: &mut PgConnection,
    id: i64,
    now: DateTime<Utc>,
) -> Result<(), OkfError> {
    match registration(conn, id).await? {
        Registration::Gone => Err(OkfError::Busy {
            reason: "the catalog was not finished in time".to_string(),
        }),
        Registration::Provisioned => Ok(()),
        Registration::Unprovisioned {
            name,
            owner_team_id,
        } => {
            sqlx::query("UPDATE catalogs SET provisioned_at = $2 WHERE id = $1")
                .bind(id)
                .bind(now)
                .execute(&mut *conn)
                .await?;
            // The catalog, and its owner's list of catalogs.
            let mut invalidations = vec![Invalidation::CatalogMeta { id, name }];
            invalidations.extend(owner_team_id.map(|id| Invalidation::Team { id }));
            invalidate(conn, &invalidations).await
        }
    }
}

/// Step 3 of `create`: [`mark_provisioned_in`] in its own transaction.
///
/// # Arguments
/// * `pool` - the account database.
/// * `id` - the catalog's id.
/// * `now` - the time to record.
///
/// # Errors
/// As [`mark_provisioned_in`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "catalogs.mark_provisioned_tx", catalog_id = id))]
async fn mark_provisioned(pool: &PgPool, id: i64, now: DateTime<Utc>) -> Result<(), OkfError> {
    let mut tx = pool.begin().await?;
    mark_provisioned_in(&mut tx, id, now).await?;
    tx.commit().await?;
    Ok(())
}

/// A half-created catalog the recovery task looks at.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Pending {
    /// The catalog's id.
    pub id: i64,
    /// Its name.
    pub name: String,
    /// The store it was placed on.
    pub store: String,
    /// When its registry row was written; with `id`, its place in the
    /// recovery order ([`PendingCursor`]).
    pub created_at: DateTime<Utc>,
}

/// How many half-created catalogs one query returns: a bound on a query's
/// size. A recovery run pages through the rest ([`pending`]).
pub const PENDING_BATCH: i64 = 100;

/// Where the last page of [`pending`] ended, in `(created_at, id)` order.
pub type PendingCursor = (DateTime<Utc>, i64);

impl Pending {
    /// This row's place in the recovery order: the cursor that makes the next
    /// page start after it.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn cursor(&self) -> PendingCursor {
        (self.created_at, self.id)
    }
}

/// The half-created catalogs created before `created_before`, oldest first
/// (an index scan of `catalogs_unprovisioned`, migration 0040). The age is
/// an efficiency filter only: it keeps the task off creates still running.
/// Safety is the catalog's lock ([`recover`]), not this.
///
/// One page is at most [`PENDING_BATCH`] rows. A recovery run pages with
/// `after` and leaves out a store it found unreachable (`skip_stores`), so
/// rows on a lost store neither fill the page nor hide the rows behind them
/// (a lost store costs only its own catalogs).
///
/// # Arguments
/// * `executor` - the account database.
/// * `created_before` - the newest `created_at` to include.
/// * `after` - the last row of the previous page, or `None` for the first.
/// * `skip_stores` - stores whose rows are left out.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "catalogs.pending"))]
pub async fn pending<'e>(
    executor: impl PgExecutor<'e>,
    created_before: DateTime<Utc>,
    after: Option<PendingCursor>,
    skip_stores: &[String],
) -> Result<Vec<Pending>, OkfError> {
    let (after_at, after_id) = after.unzip();
    Ok(sqlx::query_as::<_, Pending>(
        "SELECT id, name, store, created_at FROM catalogs \
         WHERE provisioned_at IS NULL AND created_at < $1 \
           AND ($2::timestamptz IS NULL OR (created_at, id) > ($2, $3)) \
           AND NOT (store = ANY($4)) \
         ORDER BY created_at, id LIMIT $5",
    )
    .bind(created_before)
    .bind(after_at)
    .bind(after_id)
    .bind(skip_stores)
    .bind(PENDING_BATCH)
    .fetch_all(executor)
    .await?)
}

/// What [`recover`] did with a half-created catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    /// The create finished, or someone else did, or the row is gone: nothing to do.
    Settled,
    /// The store side existed: `provisioned_at` was set.
    Finished,
    /// The store side never happened: the registry row was removed.
    Removed,
}

/// Finishes or removes one half-created catalog (decision P9). In one
/// store transaction it takes the catalog's lock, checks the registry row,
/// and checks the store side (`store_side_exists`). If the store side is
/// there it sets `provisioned_at`. If not it records the failed create in
/// the audit log and deletes the registry row, and commits that before the
/// lock is released. A create that stalled therefore either committed first
/// (and this finds its data) or waits behind this, finds its row gone and
/// writes nothing. No age is checked here.
///
/// # Arguments
/// * `db` - the store the catalog was placed on (its handle also holds the
///   account database).
/// * `catalog` - the half-created catalog.
/// * `now` - the server clock.
/// * `wait` - the database-side bound on every lock wait and statement of
///   the account and store transactions (`lock_timeout` and
///   `statement_timeout`, as `stores::bounded` sets them), so a catalog lock
///   another connection holds, or a store that stops answering, fails the
///   recovery instead of waiting on (issue #1028).
///
/// # Returns
/// What was done.
///
/// # Errors
/// [`OkfError::Database`] or [`OkfError::StoreUnavailable`] (a wait past
/// `wait` is a `Database` error, SQLSTATE `55P03` or `57014`); nothing is
/// changed, and the next run tries again.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "catalogs.recover", catalog_id = catalog.id))]
pub async fn recover(
    db: &CatalogDb,
    catalog: &Pending,
    now: DateTime<Utc>,
    wait: Duration,
) -> Result<Recovery, OkfError> {
    recover_holding(db, catalog, now, wait, std::future::ready(())).await
}

/// [`recover`] with `hold` awaited before the commit, with the lock held:
/// the tests' way to stall a recovery without a sleep.
///
/// # Errors
/// As [`recover`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "catalogs.recover_holding", catalog_id = catalog.id))]
async fn recover_holding(
    db: &CatalogDb,
    catalog: &Pending,
    now: DateTime<Utc>,
    wait: Duration,
    hold: impl std::future::Future<Output = ()>,
) -> Result<Recovery, OkfError> {
    let actor = AuditActor::system("provision-catalogs");
    // The account transaction wraps the store's; the store commits first.
    let mut account_tx = account_tx(db.accounts(), wait).await?;
    // The people the audit entry belongs to, locked before the store's
    // connection and the catalog's lock, as `delete` does.
    audit::lock_people(
        &mut account_tx,
        &actor,
        &People {
            catalogs: &[catalog.name.as_str()],
            ..People::default()
        },
    )
    .await?;
    // Acquisition order (plan Task 11, issue #970): the account transaction
    // and every account row lock (`lock_people`) come BEFORE the store
    // connection. A change that held the store's connection while it waited
    // for a user row would block a user deletion (which holds those rows and
    // then needs a store connection) in a cycle PostgreSQL cannot see,
    // because one edge is a pool wait.
    let mut tx = store_tx(db, wait).await?;
    lock(&mut tx, catalog.id).await?;
    let Registration::Unprovisioned { .. } = registration(&mut account_tx, catalog.id).await?
    else {
        return Ok(Recovery::Settled);
    };
    let outcome = if store_side_exists(&mut tx, catalog.id).await? {
        mark_provisioned_in(&mut account_tx, catalog.id, now).await?;
        Recovery::Finished
    } else {
        record_catalog(
            &mut account_tx,
            &actor,
            CatalogEvent {
                action: Action::CatalogDeleted,
                catalog_id: catalog.id,
                catalog: &catalog.name,
                // The create never wrote its store side: this is the
                // failed create, not a delete anyone asked for.
                details: serde_json::json!({
                    "forced": false,
                    "generation": 0,
                    "failed_create": true,
                }),
            },
            now,
        )
        .await?;
        sqlx::query("DELETE FROM catalogs WHERE id = $1")
            .bind(catalog.id)
            .execute(&mut *account_tx)
            .await?;
        Recovery::Removed
    };
    hold.await;
    tx.commit().await?;
    account_tx.commit().await?;
    Ok(outcome)
}

/// Writes a new catalog's `catalog_state` row and its [`starter_files`] as
/// revision 1 of each path.
///
/// # Arguments
/// * `conn` - the creating transaction's connection.
/// * `catalog` - the new row (its display name, else "Catalog", is the
///   index's heading, and its description the paragraph).
/// * `actor` - recorded as `updated_by` and the revisions' `actor`.
/// * `now` - the creation time.
///
/// # Errors
/// [`OkfError::Database`]; the starter files themselves always parse.
#[tracing::instrument(level = "trace", skip_all, fields(db.system.name = "postgresql", db.operation.name = "catalogs.write_starter_files"))]
async fn write_starter_files(
    conn: &mut PgConnection,
    catalog: &Catalog,
    actor: &str,
    now: DateTime<Utc>,
) -> Result<(), OkfError> {
    let heading = if catalog.display_name.is_empty() {
        ROOT_HEADING
    } else {
        &catalog.display_name
    };
    let files = starter_files(heading, &catalog.description, now);
    // The derived columns (kind, title) come from a snapshot, as an
    // import's do; two small files, so building it inline is cheap.
    let workspace =
        MemWorkspace::from_files(files.iter().map(|(path, text)| (*path, text.as_str())));
    let snapshot = Snapshot::build(&workspace)?;
    // First: the documents' foreign key, and the row `store_side_exists`
    // looks for.
    sqlx::query("INSERT INTO catalog_state (id) VALUES ($1)")
        .bind(catalog.id)
        .execute(&mut *conn)
        .await?;
    for (path, text) in &files {
        let file = StoredFile {
            content: text.clone().into_bytes(),
            executable: false,
        };
        let write = DocumentWrite {
            catalog_id: catalog.id,
            path,
            revision: 1,
            actor,
            now,
        };
        let record = RevisionRecord {
            op: RevisionOp::Create,
            file: Some(&file),
            changeset_id: None,
            message: STARTER_MESSAGE,
            user_id: None,
        };
        documents::write_document(conn, &write, &file, &Derived::of(snapshot.get(path))).await?;
        documents::record_revision(conn, &write, &record).await?;
    }
    Ok(())
}

/// Makes `team` the owner of catalog `name` (moving it, or assigning a
/// catalog from before migration 0009), records `catalog.owner_changed`,
/// and bumps the administrative generation, in one transaction. Giving a
/// catalog to the team that owns it already changes nothing, so it writes
/// and records nothing.
///
/// # Arguments
/// * `pool` - the database.
/// * `name` - the catalog's name.
/// * `team` - the new owner's team name.
/// * `actor` - who, for the audit log.
/// * `now` - the server clock.
///
/// # Returns
/// The previous owner's name (`None` for a catalog that had none).
///
/// # Errors
/// [`OkfError::CatalogNotSelected`] for an unknown (or malformed) catalog;
/// [`OkfError::UnknownName`] for an unknown team; [`OkfError::Database`],
/// a failed audit write included.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "catalogs.set_owner"))]
pub async fn set_owner(
    pool: &PgPool,
    name: &str,
    team: &str,
    actor: &AuditActor,
    now: DateTime<Utc>,
) -> Result<Option<String>, OkfError> {
    check_name(name)?;
    let mut tx = pool.begin().await?;
    // The new owner's person, and the old one's (through the catalog). A
    // concurrent `set_owner` committing between this and the `UPDATE` below
    // leaves the old person to `audit_team_ref`, locked late; at worst
    // Postgres then aborts one of the two as a deadlock (`Busy`, retryable).
    audit::lock_people(
        &mut tx,
        actor,
        &People {
            teams: &[team],
            catalogs: &[name],
            ..People::default()
        },
    )
    .await?;
    let team_id = teams::id_of(&mut tx, team).await?;
    // As in `update_details`: the subquery reads the row as it was
    // before the `UPDATE`, under its row lock, so this returns the owner it
    // replaced even with two changes racing. The outer `Option` is "no such
    // catalog"; the old owner's id and name are `None` for a catalog that
    // had none.
    let previous: Option<(i64, Option<String>, Option<String>)> = sqlx::query_as(
        "UPDATE catalogs c SET owner_team_id = $2 \
         FROM (SELECT id, owner_team_id FROM catalogs \
               WHERE name = $1 AND provisioned_at IS NOT NULL AND deleted_at IS NULL FOR UPDATE) old \
         WHERE c.id = old.id \
         RETURNING c.id, old.owner_team_id, \
                   (SELECT t.name FROM teams t WHERE t.id = old.owner_team_id)",
    )
    .bind(name)
    .bind(&team_id)
    .fetch_optional(&mut *tx)
    .await?;
    let (catalog_id, previous_id, previous) =
        previous.ok_or_else(|| OkfError::CatalogNotSelected {
            catalog: name.to_string(),
        })?;
    if previous_id.as_deref() == Some(team_id.as_str()) {
        // Dropping `tx` rolls back the `UPDATE`, which wrote the same value.
        return Ok(previous);
    }
    // `audit_team_ref` of NULL is NULL: a catalog that had no owner.
    let from: Option<i64> = sqlx::query_scalar("SELECT audit_team_ref($1)")
        .bind(&previous_id)
        .fetch_one(&mut *tx)
        .await?;
    let to = audit::team_ref(&mut tx, team).await?;
    // `chain` appends `to` after `from` (if any): the old team, then the new.
    let teams: Vec<i64> = from.into_iter().chain([to]).collect();
    audit::record(
        &mut tx,
        actor,
        &Entry {
            action: Action::CatalogOwnerChanged,
            target: Target::Catalog(name),
            person: None,
            teams: &teams,
            details: serde_json::json!({"from": from, "to": to}),
        },
        now,
    )
    .await?;
    // The catalog, and both owners: each team's catalogs changed.
    let mut invalidations = vec![Invalidation::CatalogMeta {
        id: catalog_id,
        name: name.to_string(),
    }];
    invalidations.extend(previous_id.map(|id| Invalidation::Team { id }));
    invalidations.push(Invalidation::Team { id: team_id });
    invalidate(&mut tx, &invalidations).await?;
    tx.commit().await?;
    Ok(previous)
}

/// A change of a catalog's display name and/or description:
/// [`update_details`]' input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DetailsChange<'a> {
    /// The catalog's name.
    pub catalog: &'a str,
    /// The new display name, or `None` to leave it.
    pub display_name: Option<&'a DisplayName>,
    /// The new description (empty is allowed, as for `create`), or `None`
    /// to leave it.
    pub description: Option<&'a str>,
    /// As [`MaintenanceChange::owner`]: `Some(users.id)` when the caller may
    /// make the change only as an owner of the catalog's owner team.
    pub owner: Option<&'a str>,
}

/// What a catalog's details were before [`update_details`].
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Details {
    /// Its display name; empty when unset.
    pub display_name: String,
    /// Its description.
    pub description: String,
}

/// Replaces a catalog's display name and/or description, records
/// `catalog.display_name_changed` and `catalog.description_changed` for
/// each field that changed (its new length in characters, never the text),
/// and bumps the administrative generation (the key cache's labels read
/// both), in one transaction. The catalog's `generation` is left alone: its
/// content did not change, so its snapshot is not rebuilt. A field given
/// its current value is no change: when nothing changes, nothing is
/// written or recorded.
///
/// Lock order (`tools/agentic/lessons.md`), as [`set_maintenance`]'s: the
/// users the entries resolve ([`audit::lock_people`]), then, for an owner,
/// its membership row (`lock_owner`), then the catalog row, then
/// `server_state`.
///
/// # Arguments
/// * `pool` - the database.
/// * `change` - the catalog, the fields to set, and the owner to check, if any.
/// * `actor` - who, for the audit log.
/// * `now` - the server clock.
///
/// # Returns
/// The display name and description it had before.
///
/// # Errors
/// [`OkfError::CatalogNotSelected`] for an unknown or malformed catalog
/// without an owner check; [`OkfError::Forbidden`] when the owner check
/// fails (an unknown catalog included, so it reveals nothing);
/// [`OkfError::Database`] otherwise, a failed audit write included.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "catalogs.update_details"))]
pub async fn update_details(
    pool: &PgPool,
    change: &DetailsChange<'_>,
    actor: &AuditActor,
    now: DateTime<Utc>,
) -> Result<Details, OkfError> {
    let DetailsChange {
        catalog: name,
        display_name,
        description,
        owner,
    } = *change;
    let not_owner = || OkfError::Forbidden {
        reason: format!(
            "this needs admin on catalog {name:?}, or an owner of its team signed in to the \
             console"
        ),
    };
    // The refusal for "no such catalog": an owner learns nothing from it.
    let not_found = || match owner {
        Some(_) => not_owner(),
        None => OkfError::CatalogNotSelected {
            catalog: name.to_string(),
        },
    };
    if !is_valid_name(name) {
        // A NUL would be a database error if it were bound.
        return Err(not_found());
    }
    let mut tx = pool.begin().await?;
    audit::lock_people(
        &mut tx,
        actor,
        &People {
            catalogs: &[name],
            ..People::default()
        },
    )
    .await?;
    let owner_team = match owner {
        Some(user_id) => Some(lock_owner(&mut tx, name, user_id, not_owner).await?),
        None => None,
    };
    // `DisplayName::as_str` borrows the text; `Option::map` applies it only
    // to `Some`, and a `None` binds as SQL NULL, which `COALESCE` turns into
    // the column's current value. The subquery reads the row as it was
    // before the `UPDATE` (one statement sees one snapshot), under its row
    // lock, so this returns the old values even with two changes racing.
    // `FOR NO KEY UPDATE`: neither column is a key, so foreign-key checks
    // (`FOR KEY SHARE`) on the catalog need not wait for it.
    let row: Option<(i64, Option<String>, String, String)> = sqlx::query_as(
        "UPDATE catalogs c SET display_name = COALESCE($2, c.display_name), \
                               description = COALESCE($3, c.description) \
         FROM (SELECT id, owner_team_id, display_name, description FROM catalogs \
               WHERE name = $1 AND provisioned_at IS NOT NULL AND deleted_at IS NULL FOR NO KEY UPDATE) old \
         WHERE c.id = old.id \
         RETURNING c.id, old.owner_team_id, old.display_name, old.description",
    )
    .bind(name)
    .bind(display_name.map(DisplayName::as_str))
    .bind(description)
    .fetch_optional(&mut *tx)
    .await?;
    let (catalog_id, team, previous_display_name, previous_description) =
        row.ok_or_else(not_found)?;
    // Under the row lock the owner team is fixed: a catalog moved to
    // another team after the membership was read is refused.
    if owner_team.is_some() && owner_team != team {
        return Err(not_owner());
    }
    let previous = Details {
        display_name: previous_display_name,
        description: previous_description,
    };
    // Each field that changed: its action and its new text. `filter` keeps
    // a `Some` only when the closure answers `true`.
    let changed = [
        (
            Action::CatalogDisplayNameChanged,
            display_name
                .map(DisplayName::as_str)
                .filter(|new| *new != previous.display_name),
        ),
        (
            Action::CatalogDescriptionChanged,
            description.filter(|new| *new != previous.description),
        ),
    ];
    if changed.iter().all(|(_, text)| text.is_none()) {
        // Dropping `tx` rolls back the `UPDATE`, which wrote the same values.
        return Ok(previous);
    }
    for (action, text) in changed {
        // `let .. else`: skip a field that did not change.
        let Some(text) = text else { continue };
        record_catalog(
            &mut tx,
            actor,
            CatalogEvent {
                action,
                catalog_id,
                catalog: name,
                // The length in characters, never the text.
                details: serde_json::json!({"length": text.chars().count()}),
            },
            now,
        )
        .await?;
    }
    invalidate(
        &mut tx,
        &[Invalidation::CatalogMeta {
            id: catalog_id,
            name: name.to_string(),
        }],
    )
    .await?;
    tx.commit().await?;
    Ok(previous)
}

/// Holds `user_id`'s owner membership of catalog `name`'s owner team for the
/// rest of the transaction (`FOR SHARE OF m`), so a concurrent removal or
/// demotion waits for the change rather than racing it. The caller still
/// checks, under the catalog's row lock, that the catalog's owner team is
/// the one this returns (a catalog moved meanwhile).
///
/// # Arguments
/// * `conn` - the change's transaction.
/// * `name` - the catalog (a valid name; the caller checks it).
/// * `user_id` - `users.id` of the would-be owner.
/// * `not_owner` - builds the refusal to return.
///
/// # Returns
/// The owner team's id.
///
/// # Errors
/// `not_owner()` when the user is no owner of the catalog's team (an
/// unknown catalog included); [`OkfError::Database`].
// `impl Fn() -> OkfError` takes any closure that builds the error, like
// passing a zero-argument callable in Python; it runs only on a refusal.
#[tracing::instrument(level = "trace", skip_all, fields(db.system.name = "postgresql", db.operation.name = "catalogs.lock_owner"))]
pub(crate) async fn lock_owner(
    conn: &mut PgConnection,
    name: &str,
    user_id: &str,
    not_owner: impl Fn() -> OkfError,
) -> Result<String, OkfError> {
    sqlx::query_scalar::<_, String>(
        "SELECT m.team_id FROM catalogs c \
         JOIN team_members m ON m.team_id = c.owner_team_id \
         WHERE c.name = $1 AND m.user_id = $2 AND m.role = 'owner' FOR SHARE OF m",
    )
    .bind(name)
    .bind(user_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(not_owner)
}

/// As [`lock_owner`], but only for a shared team: an owner of a personal
/// team gets nothing, so it cannot share its personal catalogs (issue #352).
/// Used by the member-access and entitlement controls, not by the
/// maintenance switch or the details editor, which keep their reach.
///
/// # Arguments
/// * `conn` - the change's transaction.
/// * `name` - the catalog (a valid name; the caller checks it).
/// * `user_id` - `users.id` of the would-be owner.
/// * `not_owner` - builds the refusal to return.
///
/// # Returns
/// The owner team's id.
///
/// # Errors
/// `not_owner()` when the user is no owner of the catalog's team, the team
/// is personal, or the catalog does not exist; [`OkfError::Database`].
#[tracing::instrument(level = "trace", skip_all, fields(db.system.name = "postgresql", db.operation.name = "catalogs.lock_shared_team_owner"))]
pub(crate) async fn lock_shared_team_owner(
    conn: &mut PgConnection,
    name: &str,
    user_id: &str,
    not_owner: impl Fn() -> OkfError,
) -> Result<String, OkfError> {
    sqlx::query_scalar::<_, String>(
        "SELECT m.team_id FROM catalogs c \
         JOIN teams t ON t.id = c.owner_team_id \
         JOIN team_members m ON m.team_id = t.id \
         WHERE c.name = $1 AND m.user_id = $2 AND m.role = 'owner' \
           AND t.personal_of IS NULL FOR SHARE OF m",
    )
    .bind(name)
    .bind(user_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(not_owner)
}

/// A catalog's maintenance flag, `on` or `off`, as the CLI, the REST body
/// and the listing spell it (spec Part 6).
// `clap::ValueEnum` parses `on|off` on the command line; serde's
// `rename_all = "lowercase"` reads and writes `"on"`/`"off"` in JSON.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, serde::Deserialize, serde::Serialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Switch {
    /// The maintenance runner may check the catalog (the default).
    On,
    /// The maintenance runner is kept out.
    Off,
}

impl Switch {
    /// The spelling the CLI and the audit log use.
    ///
    /// # Returns
    /// `"on"` or `"off"`.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: maps a value, no I/O"
        )
    )]
    pub fn as_str(self) -> &'static str {
        match self {
            Switch::On => "on",
            Switch::Off => "off",
        }
    }

    /// The flag as `catalogs.maintenance` stores it.
    ///
    /// # Returns
    /// `true` for [`Switch::On`].
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: maps a value, no I/O"
        )
    )]
    pub fn is_on(self) -> bool {
        self == Switch::On
    }
}

// `From<bool>` lets `Switch::from(flag)` (or `flag.into()`) turn the stored
// column back into the enum.
impl From<bool> for Switch {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: maps a value, no I/O"
        )
    )]
    fn from(on: bool) -> Self {
        if on {
            Switch::On
        } else {
            Switch::Off
        }
    }
}

/// A change of a catalog's maintenance flag: [`set_maintenance`]'s input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaintenanceChange<'a> {
    /// The catalog's name.
    pub catalog: &'a str,
    /// The flag it should have.
    pub to: Switch,
    /// `Some(users.id)` when the caller may make the change only as an
    /// owner of the catalog's owner team (a console session that is not an
    /// every-catalog admin): checked under the locks, so an owner removed
    /// or a catalog moved to another team meanwhile is refused. `None` when
    /// the caller is already allowed (an every-catalog admin, the CLI).
    pub owner: Option<&'a str>,
}

/// Lets the maintenance runner into a catalog, or keeps it out (spec Part
/// 6), records `catalog.maintenance_changed` in the same transaction, and
/// bumps the administrative generation (the key cache holds the flag). The
/// flag it already has changes and records nothing.
///
/// Lock order (`tools/agentic/lessons.md`): the users the entry resolves
/// ([`audit::lock_people`]), then, for an owner, its membership row, then
/// the catalog row, then `server_state`.
///
/// # Arguments
/// * `pool` - the database.
/// * `change` - the catalog, the new flag, and the owner to check, if any.
/// * `actor` - who, for the audit log.
/// * `now` - the server clock.
///
/// # Returns
/// The flag before.
///
/// # Errors
/// [`OkfError::CatalogNotSelected`] for an unknown or malformed catalog
/// without an owner check; [`OkfError::Forbidden`] when the owner check
/// fails (an unknown catalog included, so it reveals nothing);
/// [`OkfError::Database`] otherwise, a failed audit write included.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "catalogs.set_maintenance"))]
pub async fn set_maintenance(
    pool: &PgPool,
    change: &MaintenanceChange<'_>,
    actor: &AuditActor,
    now: DateTime<Utc>,
) -> Result<Switch, OkfError> {
    let MaintenanceChange {
        catalog: name,
        to,
        owner,
    } = *change;
    let not_owner = || OkfError::Forbidden {
        reason: format!(
            "this needs an admin that reaches every catalog, or an owner of catalog {name:?}'s \
             team signed in to the console"
        ),
    };
    if !is_valid_name(name) {
        // A NUL would be a database error if it were bound.
        return Err(match owner {
            Some(_) => not_owner(),
            None => OkfError::CatalogNotSelected {
                catalog: name.to_string(),
            },
        });
    }
    let mut tx = pool.begin().await?;
    audit::lock_people(
        &mut tx,
        actor,
        &People {
            catalogs: &[name],
            ..People::default()
        },
    )
    .await?;
    // `lock_owner` holds the owner's membership row, so a concurrent
    // removal or demotion waits for this change rather than racing it.
    let owner_team = match owner {
        Some(user_id) => Some(lock_owner(&mut tx, name, user_id, not_owner).await?),
        None => None,
    };
    // The subquery reads the row before the `UPDATE`, so this returns the
    // old flag. `FOR NO KEY UPDATE`: the column is no key, so foreign-key
    // checks (`FOR KEY SHARE`) on the catalog need not wait for it.
    let row: Option<(i64, bool, Option<String>)> = sqlx::query_as(
        "UPDATE catalogs c SET maintenance = $2 \
         FROM (SELECT id, maintenance, owner_team_id FROM catalogs \
               WHERE name = $1 AND provisioned_at IS NOT NULL AND deleted_at IS NULL FOR NO KEY UPDATE) old \
         WHERE c.id = old.id RETURNING c.id, old.maintenance, old.owner_team_id",
    )
    .bind(name)
    .bind(to.is_on())
    .fetch_optional(&mut *tx)
    .await?;
    let Some((catalog_id, previous, team)) = row else {
        return Err(match owner {
            Some(_) => not_owner(),
            None => OkfError::CatalogNotSelected {
                catalog: name.to_string(),
            },
        });
    };
    // Under the row lock the owner team is fixed: a catalog moved to
    // another team after the membership was read is refused.
    if owner_team.is_some() && owner_team != team {
        return Err(not_owner());
    }
    let previous = Switch::from(previous);
    if previous == to {
        // Dropping `tx` rolls back the `UPDATE`, which wrote the same flag.
        return Ok(previous);
    }
    record_catalog(
        &mut tx,
        actor,
        CatalogEvent {
            action: Action::CatalogMaintenanceChanged,
            catalog_id,
            catalog: name,
            details: serde_json::json!({"from": previous.as_str(), "to": to.as_str()}),
        },
        now,
    )
    .await?;
    invalidate(
        &mut tx,
        &[Invalidation::CatalogMeta {
            id: catalog_id,
            name: name.to_string(),
        }],
    )
    .await?;
    tx.commit().await?;
    Ok(previous)
}

/// Every catalog, by name. A half-created catalog (`provisioned_at` NULL,
/// plan Task 10b) is not one yet and is left out.
///
/// # Arguments
/// * `pool` - the database.
///
/// # Returns
/// The rows, ordered by name.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "catalogs.list"))]
pub async fn list(pool: &PgPool) -> Result<Vec<Catalog>, OkfError> {
    let catalogs = sqlx::query_as::<_, Catalog>(concat!(
        select_catalogs!(),
        " WHERE c.provisioned_at IS NOT NULL AND c.deleted_at IS NULL ORDER BY c.name"
    ))
    .fetch_all(pool)
    .await?;
    Ok(catalogs)
}

/// The generation of the catalog called `name`, read from its store's state
/// row: what the tests assert on now that the registry row carries none.
///
/// # Panics
/// If the catalog or its state row is missing, or the query fails.
#[cfg(test)]
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(uninstrumented_fn, reason = "a test helper")
)]
pub(crate) async fn test_generation(pool: &PgPool, name: &str) -> i64 {
    // Two databases in `separate` store mode: the id from the registry, the
    // generation from the store.
    let id: i64 = sqlx::query_scalar("SELECT id FROM catalogs WHERE name = $1")
        .bind(name)
        .fetch_one(pool)
        .await
        .expect("the catalog is registered");
    sqlx::query_scalar("SELECT generation FROM catalog.catalog_state WHERE id = $1")
        .bind(id)
        .fetch_one(CatalogDb::main_of(pool).executor())
        .await
        .expect("the catalog has a state row")
}

/// [`list`] with each catalog's generation, read from its store's
/// `catalog_state` (the registry rows do not carry it): what the command
/// line prints.
///
/// # Arguments
/// * `stores` - the registry of stores.
///
/// # Returns
/// The rows, ordered by name.
///
/// # Errors
/// [`OkfError::Database`]; [`OkfError::StoreUnavailable`] if a store cannot
/// be opened.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "catalogs.list_in"))]
pub async fn list_in(stores: &Stores) -> Result<Vec<Catalog>, OkfError> {
    let accounts = stores.accounts();
    let mut catalogs = list(accounts.executor()).await?;
    let placed: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, store FROM catalogs WHERE provisioned_at IS NOT NULL AND deleted_at IS NULL",
    )
    .fetch_all(accounts.executor())
    .await?;
    let mut by_store: std::collections::HashMap<String, Vec<i64>> =
        std::collections::HashMap::new();
    for (id, store) in placed {
        by_store.entry(store).or_default().push(id);
    }
    let mut generation_of: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    for (store, ids) in by_store {
        let db = stores.get(&StoreName::parse(&store)?).await?;
        generation_of.extend(generations(db.executor(), &ids).await?);
    }
    for catalog in &mut catalogs {
        catalog.generation = generation_of.get(&catalog.id).copied().unwrap_or(0);
    }
    Ok(catalogs)
}

/// The catalog called `name`. A half-created catalog (`provisioned_at`
/// NULL, plan Task 10b) is not one yet: it is unknown here, so nothing that
/// finds a catalog by name can read, change or delete it.
///
/// # Arguments
/// * `executor` - a pool, connection or transaction (`&mut *tx`).
/// * `name` - the catalog's name.
///
/// # Returns
/// Its row.
///
/// # Errors
/// [`OkfError::CatalogNotSelected`] if there is none (a malformed name
/// included); [`OkfError::Database`].
// `'e` is a lifetime parameter: the executor is borrowed for the query only.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "catalogs.find"))]
pub async fn find<'e>(executor: impl PgExecutor<'e>, name: &str) -> Result<Catalog, OkfError> {
    check_name(name)?;
    sqlx::query_as::<_, Catalog>(concat!(
        select_catalogs!(),
        " WHERE c.provisioned_at IS NOT NULL AND c.deleted_at IS NULL AND c.name = $1"
    ))
    .bind(name)
    .fetch_optional(executor)
    .await?
    .ok_or_else(|| OkfError::CatalogNotSelected {
        catalog: name.to_string(),
    })
}

/// An owner team's suspension, as the directory holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuspendedRow {
    /// The owner team's name.
    pub team: String,
    /// Its notice, if any.
    pub notice: Option<String>,
    /// The owner team's label, so a refusal names the team without a label
    /// lookup (M18 Task 12). The directory drops the row when the team's
    /// label or its personal owner's can change.
    pub team_label: String,
}

/// A catalog's row with its access facts (M18 spec Decisions 6 and 9): what a
/// request needs to know about a catalog without reading its content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogRow {
    /// `catalogs.id`, never reused.
    pub id: i64,
    /// Unique name, `[a-z0-9-]+`.
    pub name: String,
    /// Its display name; empty when unset.
    pub display_name: String,
    /// Free text.
    pub description: String,
    /// Bumped by every publish and import.
    pub generation: i64,
    /// The owner team's name.
    pub owner_team: Option<String>,
    /// The owner team's id (`teams.id`).
    pub owner_team_id: Option<String>,
    /// The owner team's suspension, if it is suspended.
    pub suspended: Option<SuspendedRow>,
    /// `NOT catalogs.maintenance`.
    pub maintenance_off: bool,
    /// The owner team's `audit_teams.id` (the gates' `CatalogScope::team`).
    pub audit_team: Option<i64>,
    /// What members of the owner team reach through self-issued credentials
    /// and console sessions: one half of every user's ceiling on this
    /// catalog ([`crate::store::ceiling::level_on`], issue #728).
    pub member_access: MemberAccess,
    /// The store holding its documents, history and changesets
    /// (`catalogs.store`).
    pub store: StoreName,
}

/// [`CatalogRow`] as the query returns it: the suspension as two columns.
#[derive(sqlx::FromRow)]
struct CatalogRowColumns {
    store: String,
    id: i64,
    name: String,
    display_name: String,
    description: String,
    generation: i64,
    owner_team: Option<String>,
    owner_team_id: Option<String>,
    suspended: bool,
    suspension_notice: Option<String>,
    team_display_name: Option<String>,
    team_personal: bool,
    team_owner_display_name: Option<String>,
    team_owner_email: Option<String>,
    maintenance_off: bool,
    audit_team: Option<i64>,
    member_access: String,
}

impl TryFrom<CatalogRowColumns> for CatalogRow {
    type Error = OkfError;

    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    fn try_from(columns: CatalogRowColumns) -> Result<Self, Self::Error> {
        // A catalog is suspended when its owner team is.
        let suspended = columns
            .suspended
            .then(|| columns.owner_team.clone())
            .flatten()
            .map(|team| {
                let owner = columns.team_personal.then(|| {
                    crate::labels::user_label(
                        columns
                            .team_owner_display_name
                            .as_deref()
                            .unwrap_or_default(),
                        columns.team_owner_email.as_deref(),
                    )
                });
                SuspendedRow {
                    team_label: crate::labels::team_label(
                        columns.team_display_name.as_deref().unwrap_or_default(),
                        owner.as_deref(),
                    ),
                    team,
                    notice: columns.suspension_notice,
                }
            });
        // The foreign key and the stores' CHECK keep the name valid; a
        // hand-edited row is an error, not a guess.
        let store =
            crate::store::decode_column(StoreName::parse(&columns.store).ok(), "catalogs.store")?;
        Ok(Self {
            store,
            id: columns.id,
            name: columns.name,
            display_name: columns.display_name,
            description: columns.description,
            generation: columns.generation,
            owner_team: columns.owner_team,
            owner_team_id: columns.owner_team_id,
            suspended,
            maintenance_off: columns.maintenance_off,
            audit_team: columns.audit_team,
            // The schema's CHECK allows only the three spellings; anything
            // else would reach nothing through the team, never more.
            member_access: MemberAccess::parse(&columns.member_access)
                .unwrap_or(MemberAccess::None),
        })
    }
}

// The directory's SELECT, shared by the four queries below. The owner,
// suspension, maintenance and audit-team columns are what every access
// check reads about the catalog it acts on (M18 spec Decision 9).
macro_rules! select_catalog_rows {
    () => {
        "SELECT c.store, c.id, c.name, c.display_name, c.description, 0::bigint AS generation, \
                t.name AS owner_team, c.owner_team_id, \
                (t.suspended_at IS NOT NULL) AS suspended, t.suspension_notice, \
                t.display_name AS team_display_name, \
                (t.personal_of IS NOT NULL) AS team_personal, \
                pu.display_name AS team_owner_display_name, \
                pu.email::text AS team_owner_email, \
                NOT c.maintenance AS maintenance_off, a.id AS audit_team, \
                c.member_access \
         FROM catalogs c \
         LEFT JOIN teams t ON t.id = c.owner_team_id \
         LEFT JOIN users pu ON pu.id = t.personal_of \
         LEFT JOIN audit_teams a ON a.team_id = c.owner_team_id"
    };
}

/// The row of the catalog called `name`, with its access facts.
///
/// # Arguments
/// * `executor` - a pool, connection or transaction.
/// * `name` - the catalog's name; a malformed one (which Postgres might
///   refuse, such as one with a NUL) is the same as an unknown one.
///
/// # Returns
/// Its row, or `None` if there is no such catalog.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "catalogs.row_by_name"))]
pub async fn row_by_name<'e>(
    executor: impl PgExecutor<'e>,
    name: &str,
) -> Result<Option<CatalogRow>, OkfError> {
    if !is_valid_name(name) {
        return Ok(None);
    }
    let row = sqlx::query_as::<_, CatalogRowColumns>(concat!(
        select_catalog_rows!(),
        " WHERE c.provisioned_at IS NOT NULL AND c.deleted_at IS NULL AND c.name = $1"
    ))
    .bind(name)
    .fetch_optional(executor)
    .await?;
    row.map(CatalogRow::try_from).transpose()
}

/// The row of the catalog with this `catalogs.id`, with its access facts:
/// for a caller that holds an id (a changeset's catalog), so it reaches that
/// incarnation and never another catalog created since under the name.
///
/// # Arguments
/// * `executor` - a pool, connection or transaction.
/// * `id` - the catalog's id.
///
/// # Returns
/// Its row, or `None` if there is no catalog with that id (it was deleted).
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "catalogs.row_by_id"))]
pub async fn row_by_id<'e>(
    executor: impl PgExecutor<'e>,
    id: i64,
) -> Result<Option<CatalogRow>, OkfError> {
    let row = sqlx::query_as::<_, CatalogRowColumns>(concat!(
        select_catalog_rows!(),
        " WHERE c.provisioned_at IS NOT NULL AND c.deleted_at IS NULL AND c.id = $1"
    ))
    .bind(id)
    .fetch_optional(executor)
    .await?;
    row.map(CatalogRow::try_from).transpose()
}

/// The rows of the catalogs called `names`, in one query.
///
/// # Arguments
/// * `executor` - a pool, connection or transaction.
/// * `names` - catalog names; malformed ones and ones that do not exist are
///   left out of the answer.
///
/// # Returns
/// The rows found, ordered by name.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "catalogs.rows_by_names"))]
pub async fn rows_by_names<'e>(
    executor: impl PgExecutor<'e>,
    names: &[String],
) -> Result<Vec<CatalogRow>, OkfError> {
    // Drop what Postgres could not take (a NUL) or cannot match.
    let valid: Vec<&str> = names
        .iter()
        .map(String::as_str)
        .filter(|name| is_valid_name(name))
        .collect();
    if valid.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query_as::<_, CatalogRowColumns>(concat!(
        select_catalog_rows!(),
        " WHERE c.provisioned_at IS NOT NULL AND c.deleted_at IS NULL AND c.name = ANY($1) ORDER BY c.name"
    ))
    .bind(valid)
    .fetch_all(executor)
    .await?;
    rows.into_iter().map(CatalogRow::try_from).collect()
}

/// Every catalog placed on `store`, by name: what a read that cannot reach the
/// store names as unavailable. Read from the account database, so it works
/// while the store is down.
///
/// # Arguments
/// * `executor` - a pool, connection or transaction on the account database.
/// * `store` - the store's name.
///
/// # Returns
/// The store's catalogs, ordered by name, with generation 0 (the store holds
/// the real one).
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "catalogs.rows_on_store"))]
pub async fn rows_on_store<'e>(
    executor: impl PgExecutor<'e>,
    store: &StoreName,
) -> Result<Vec<CatalogRow>, OkfError> {
    let rows = sqlx::query_as::<_, CatalogRowColumns>(concat!(
        select_catalog_rows!(),
        " WHERE c.provisioned_at IS NOT NULL AND c.deleted_at IS NULL AND c.store = $1 ORDER BY c.name"
    ))
    .bind(store.as_str())
    .fetch_all(executor)
    .await?;
    rows.into_iter().map(CatalogRow::try_from).collect()
}

/// The generations of catalogs `ids`, read from a store's `catalog_state`
/// (the registry rows do not carry them).
///
/// # Arguments
/// * `executor` - a pool, connection or transaction on the store.
/// * `ids` - catalog ids.
///
/// # Returns
/// The generation of each of `ids` that has a state row on this store.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "catalogs.generations"))]
pub async fn generations<'e>(
    executor: impl PgExecutor<'e>,
    ids: &[i64],
) -> Result<Vec<(i64, i64)>, OkfError> {
    Ok(
        sqlx::query_as("SELECT id, generation FROM catalog_state WHERE id = ANY($1)")
            .bind(ids)
            .fetch_all(executor)
            .await?,
    )
}

/// One page of catalog rows, ordered by name.
///
/// # Arguments
/// * `executor` - a pool, connection or transaction.
/// * `after` - the last name of the previous page (exclusive); `None` for
///   the first page.
/// * `limit` - the most rows to return.
///
/// # Returns
/// Up to `limit` rows, ordered by name.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "catalogs.page"))]
pub async fn page<'e>(
    executor: impl PgExecutor<'e>,
    after: Option<&str>,
    limit: i64,
) -> Result<Vec<CatalogRow>, OkfError> {
    // A cursor Postgres cannot take (a NUL) can match nothing, so it is the
    // end of the list.
    if after.is_some_and(|name| name.contains('\0')) {
        return Ok(Vec::new());
    }
    let rows = sqlx::query_as::<_, CatalogRowColumns>(concat!(
        select_catalog_rows!(),
        " WHERE c.provisioned_at IS NOT NULL AND c.deleted_at IS NULL AND ($1::text IS NULL OR c.name > $1) \
         ORDER BY c.name LIMIT $2"
    ))
    .bind(after)
    .bind(limit)
    .fetch_all(executor)
    .await?;
    rows.into_iter().map(CatalogRow::try_from).collect()
}

/// One page of the catalogs a user's ceiling reaches, ordered by name
/// (issue #728): those owned by one of `teams` with a `member_access` other
/// than `none`, and those in `entitled`. Each team's catalogs are read from
/// the `catalogs_member_reach` index (owner team, name) one page at a time,
/// so the cost is bounded by the teams, the entitlements and the page,
/// never by how many catalogs a team owns.
///
/// # Arguments
/// * `executor` - a pool, connection or transaction.
/// * `teams` - the user's `teams.id`s.
/// * `entitled` - the `catalogs.id`s the user is entitled to.
/// * `after` - the last name of the previous page (exclusive); `None` for
///   the first page.
/// * `limit` - the most rows to return.
///
/// # Returns
/// Up to `limit` rows, ordered by name.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "catalogs.ceiling_page"))]
pub async fn ceiling_page<'e>(
    executor: impl PgExecutor<'e>,
    teams: &[String],
    entitled: &[i64],
    after: Option<&str>,
    limit: i64,
) -> Result<Vec<CatalogRow>, OkfError> {
    // As in `page`: a cursor Postgres cannot take matches nothing.
    if after.is_some_and(|name| name.contains('\0')) || (teams.is_empty() && entitled.is_empty()) {
        return Ok(Vec::new());
    }
    // `LATERAL` runs the inner query once per team (like a loop over
    // `unnest`), each an index range scan stopping after `limit` rows.
    let rows = sqlx::query_as::<_, CatalogRowColumns>(concat!(
        select_catalog_rows!(),
        " WHERE c.provisioned_at IS NOT NULL AND c.deleted_at IS NULL AND c.id IN ( \
              SELECT owned.id FROM unnest($1::text[]) AS team (id) \
              CROSS JOIN LATERAL ( \
                  SELECT i.id FROM catalogs i \
                  WHERE i.owner_team_id = team.id AND i.member_access <> 'none' \
                    AND i.provisioned_at IS NOT NULL AND i.deleted_at IS NULL \
                    AND ($3::text IS NULL OR i.name > $3) \
                  ORDER BY i.name LIMIT $4) owned \
              UNION \
              (SELECT i.id FROM catalogs i \
               WHERE i.id = ANY($2) AND i.provisioned_at IS NOT NULL AND i.deleted_at IS NULL \
                 AND ($3::text IS NULL OR i.name > $3) \
               ORDER BY i.name LIMIT $4)) \
          ORDER BY c.name LIMIT $4"
    ))
    .bind(teams)
    .bind(entitled)
    .bind(after)
    .bind(limit)
    .fetch_all(executor)
    .await?;
    rows.into_iter().map(CatalogRow::try_from).collect()
}

// Its own file: the directory's queries are tested away from `tests` below.
#[cfg(test)]
#[path = "catalogs_rows_tests.rs"]
mod rows_tests;
#[cfg(test)]
#[path = "catalogs_unprovisioned_tests.rs"]
mod unprovisioned_tests;

/// A pause inside [`delete_on`], between its store steps and its account
/// writes, for the race tests: a catalog id (in a named account database,
/// since tests run side by side and every database counts ids from 1) with
/// a pair of barriers makes the delete of that catalog stop there until the
/// test lets it go.
#[cfg(test)]
pub(crate) mod delete_pause {
    use std::collections::HashMap;
    use std::sync::{Arc, LazyLock, Mutex};

    use tokio::sync::Barrier;

    /// The longest either side waits at a barrier.
    pub(crate) const LIMIT: std::time::Duration = std::time::Duration::from_secs(60);

    impl Pause {
        /// The test's meeting with the delete at `barrier`, bounded.
        ///
        /// # Panics
        /// When the delete does not arrive within [`LIMIT`].
        pub(crate) async fn meet(barrier: &Barrier) {
            tokio::time::timeout(LIMIT, barrier.wait())
                .await
                .expect("the delete met the barrier in time");
        }
    }

    /// The two barriers one catalog's delete meets: it arrives at `reached`
    /// and then waits at `resume`; the test waits at `reached`, does its
    /// work, and waits at `resume`.
    #[derive(Clone)]
    pub(crate) struct Pause {
        /// Met when the delete has taken its store locks.
        pub reached: Arc<Barrier>,
        /// Met when the test lets the delete go on.
        pub resume: Arc<Barrier>,
    }

    static PAUSES: LazyLock<Mutex<HashMap<(String, i64), Pause>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    /// Names one account database of one server: tests run side by side,
    /// each in a database (and often a server) of its own that counts catalog
    /// ids from 1, and the database name alone can repeat between servers.
    pub(crate) const SERVER_IDENTITY: &str =
        "SELECT current_database() || '@' || pg_postmaster_start_time()::text";

    /// Makes the next delete of catalog `id` in the account database named
    /// `database` (a [`SERVER_IDENTITY`]) pause.
    ///
    /// # Returns
    /// The barriers the test meets.
    pub(crate) fn set(database: &str, id: i64) -> Pause {
        let pause = Pause {
            reached: Arc::new(Barrier::new(2)),
            resume: Arc::new(Barrier::new(2)),
        };
        PAUSES
            .lock()
            .expect("the pauses lock")
            .insert((database.to_string(), id), pause.clone());
        pause
    }

    /// Called by the delete: meets the barriers if a test set them.
    ///
    /// # Errors
    /// [`OkfError::Database`] reading the database's name.
    pub(super) async fn wait(
        conn: &mut sqlx::PgConnection,
        id: i64,
    ) -> Result<(), crate::error::OkfError> {
        let database: String = sqlx::query_scalar(SERVER_IDENTITY)
            .fetch_one(&mut *conn)
            .await?;
        let pause = PAUSES
            .lock()
            .expect("the pauses lock")
            .remove(&(database, id));
        if let Some(pause) = pause {
            // Bounded: a test that failed before it met the barriers must
            // not leave this delete (and the runtime) waiting for ever.
            let _ = tokio::time::timeout(LIMIT, pause.reached.wait()).await;
            let _ = tokio::time::timeout(LIMIT, pause.resume.wait()).await;
        }
        Ok(())
    }
}

// Two-step creation and its recovery, tested away from `tests` below.
#[cfg(test)]
#[path = "catalogs_provision_tests.rs"]
mod provision_tests;

// Two-step deletion, tested away from `tests` below.
#[cfg(test)]
#[path = "catalogs_delete_tests.rs"]
mod delete_tests;

/// Deletes a catalog with its documents, history, changesets and grants
/// (cascading foreign keys), under the catalog's lock, and bumps the
/// administrative generation. This form is for the tests, which hold only
/// the account pool (see `create`): the catalog's store is the `main`
/// store inside the account database. The server and the command line use
/// [`delete_in`].
///
/// # Arguments
/// * `pool` - the account database.
/// * `name` - the catalog's name.
/// * `force` - delete even with unpublished changes in an open changeset.
/// * `actor` - who, for the audit log.
/// * `now` - the server clock.
///
/// # Errors
/// As [`delete_on`]; also [`OkfError::Refused`] if the catalog is not on the
/// `main` store.
#[cfg(test)]
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "catalogs.delete"))]
pub(crate) async fn delete(
    pool: &PgPool,
    name: &str,
    force: bool,
    actor: &AuditActor,
    now: DateTime<Utc>,
) -> Result<(), OkfError> {
    delete_on(&CatalogDb::main_in(pool), name, force, actor, now).await
}

/// [`delete`], then the store cleanup that frees the catalog's name and
/// removes its registry row, for tests that only need the catalog gone (and
/// its name free again) and do not look at the window in between.
///
/// # Arguments
/// As [`delete`].
///
/// # Errors
/// As [`delete`].
#[cfg(test)]
pub(crate) async fn delete_settled(
    pool: &PgPool,
    name: &str,
    force: bool,
    actor: &AuditActor,
    now: DateTime<Utc>,
) -> Result<(), OkfError> {
    delete(pool, name, force, actor, now).await?;
    crate::store::cleanups::settle_for_tests(pool).await;
    Ok(())
}

/// [`delete_on`] on the store the registry holds for the catalog: what the
/// server's handlers and the command line call.
///
/// # Arguments
/// * `stores` - the registry of stores.
/// * `name` - the catalog's name.
/// * `force` - delete even with unpublished changes in an open changeset.
/// * `actor` - who, for the audit log.
/// * `now` - the server clock.
///
/// # Errors
/// As [`delete_on`]; also [`OkfError::StoreUnavailable`] when the catalog's
/// store cannot be opened.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "catalogs.delete_in"))]
pub async fn delete_in(
    stores: &Stores,
    name: &str,
    force: bool,
    actor: &AuditActor,
    now: DateTime<Utc>,
) -> Result<(), OkfError> {
    check_name(name)?;
    let accounts = stores.accounts();
    let store: Option<String> = sqlx::query_scalar("SELECT store FROM catalogs WHERE name = $1")
        .bind(name)
        .fetch_optional(accounts.executor())
        .await?;
    let store = store.ok_or_else(|| OkfError::CatalogNotSelected {
        catalog: name.to_string(),
    })?;
    let db = stores.get(&StoreName::parse(&store)?).await?;
    delete_on(&db, name, force, actor, now).await
}

/// Deletes catalog `name`, whose data is on the store `db`, in two steps
/// (plan decisions P12 and P13, issue #972): this marks it deleted, and the
/// housekeeping task `CleanupStores` removes its data and then its registry
/// row ([`crate::store::cleanups`]).
///
/// # The order, and why it cannot deadlock
/// Every lock and connection is taken in the one global order of
/// [`CatalogDb::begin`]: the account connection; the account row locks (the
/// people the entry resolves, [`audit::lock_people`]; the keys naming the
/// catalog as their default; the catalog's registry row `FOR UPDATE`; the
/// `stores` row `FOR KEY SHARE`, which the cleanup row's foreign key would
/// take); the account audit ids the entry names ([`audit::resolve_actor`]),
/// created or waited for now; and only then the store connection, whose
/// locks are the catalog's advisory lock, its `catalog_state` row
/// `FOR UPDATE` and its open changesets. Everything after that on the
/// account connection (the entry, `deleted_at`, the cleanup row, the
/// invalidation) is an insert or an update of a row this transaction
/// holds, which waits for nobody. So a transaction that holds the store's
/// connection or locks never waits for an account row: the edge that would
/// close a cross-database cycle, which PostgreSQL cannot see when the
/// store is a separate database (or has one connection), does not exist.
///
/// # What it guarantees
/// Writers take `catalog_state` `FOR KEY SHARE` first
/// ([`changesets::lock_catalog`]), so under the `FOR UPDATE` here an edit
/// either committed before (a delete without `force` then sees its entries
/// and refuses), or waits, and finds the catalog marked deleted once this
/// ends, and fails. With `force`, the open changesets are discarded in this
/// transaction. The account transaction commits first: the catalog is gone
/// from every lookup (`catalogs.deleted_at`; its name stays reserved, P6)
/// and the cleanup is owed (`store_cleanups`, written in the same
/// transaction, P13). If that commit fails, the store transaction, still
/// open, rolls back and nothing has changed. If the store commit fails
/// after it, the data is only unmarked: the cleanup removes it anyway.
///
/// # Arguments
/// * `db` - the store the catalog is on.
/// * `name` - the catalog's name.
/// * `force` - delete even with unpublished changes in an open changeset.
/// * `actor` - who, for the audit log.
/// * `now` - the server clock.
///
/// # Errors
/// [`OkfError::CatalogNotSelected`] if there is no such catalog (a
/// malformed name included) or it is not on `db`; [`OkfError::Refused`] if
/// it has unpublished changes and `force` is false; [`OkfError::Database`]
/// otherwise, a failed audit write included.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "catalogs.delete_on"))]
pub async fn delete_on(
    db: &CatalogDb,
    name: &str,
    force: bool,
    actor: &AuditActor,
    now: DateTime<Utc>,
) -> Result<(), OkfError> {
    // 1. The account connection, and every account row lock.
    let mut account_tx = db.accounts().begin().await?;
    audit::lock_people(
        &mut account_tx,
        actor,
        &People {
            catalogs: &[name],
            ..People::default()
        },
    )
    .await?;
    let catalog = find(&mut *account_tx, name).await?;
    // The keys whose default this catalog is, which the cleanup clears
    // (`ON DELETE SET NULL`), in id order.
    sqlx::query("SELECT id FROM api_keys WHERE default_catalog_id = $1 ORDER BY id FOR UPDATE")
        .bind(catalog.id)
        .execute(&mut *account_tx)
        .await?;
    let registered: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT store, owner_team_id FROM catalogs \
         WHERE id = $1 AND provisioned_at IS NOT NULL AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(catalog.id)
    .fetch_optional(&mut *account_tx)
    .await?;
    // A delete that waited on the row may find it already deleted.
    let Some((store, owner_team_id)) = registered.filter(|(store, _)| store == db.store().as_str())
    else {
        return Err(OkfError::CatalogNotSelected {
            catalog: name.to_string(),
        });
    };
    sqlx::query("SELECT 1 FROM stores WHERE name = $1 FOR KEY SHARE")
        .bind(&store)
        .execute(&mut *account_tx)
        .await?;
    // The entry's account ids, resolved (and any row another transaction
    // is still creating waited for) before the store's connection.
    audit::resolve_actor(&mut account_tx, actor, catalog.id).await?;

    // 2. The store: its locks, then its checks.
    let mut tx = db.begin().await?;
    lock(&mut tx, catalog.id).await?;
    // The generation no publish can move any more. A half-created catalog
    // has no row yet: nothing was published to it.
    let generation: i64 =
        sqlx::query_scalar("SELECT generation FROM catalog_state WHERE id = $1 FOR UPDATE")
            .bind(catalog.id)
            .fetch_optional(&mut *tx)
            .await?
            .unwrap_or(0);
    // In id order, so two transactions locking several changesets cannot
    // take them in opposite orders.
    let open: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM changesets WHERE catalog_id = $1 AND state = 'open' \
         ORDER BY id FOR UPDATE",
    )
    .bind(catalog.id)
    .fetch_all(&mut *tx)
    .await?;
    // An open changeset with no entries holds no work, so it does not block.
    let unpublished: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM changesets c \
         JOIN changeset_entries e ON e.changeset_id = c.id \
         WHERE c.catalog_id = $1 AND c.state = 'open')",
    )
    .bind(catalog.id)
    .fetch_one(&mut *tx)
    .await?;
    if unpublished && !force {
        return Err(OkfError::Refused {
            reason: format!(
                "catalog {name:?} has unpublished changes; pass --force to delete them too"
            ),
        });
    }
    for id in open {
        changesets::set_state(&mut tx, id, changesets::ChangesetState::Discarded, now).await?;
    }
    // What a writer that waited on the lock reads when it gets the row.
    sqlx::query("UPDATE catalog_state SET deleted_at = $2 WHERE id = $1")
        .bind(catalog.id)
        .bind(now)
        .execute(&mut *tx)
        .await?;

    // A test stops here to start an edit that queues behind the lock.
    #[cfg(test)]
    delete_pause::wait(&mut account_tx, catalog.id).await?;

    // 3. The account writes, on the transaction that already holds its rows.
    record_catalog(
        &mut account_tx,
        actor,
        CatalogEvent {
            action: Action::CatalogDeleted,
            catalog_id: catalog.id,
            catalog: name,
            // `forced`: unpublished changes went with it.
            details: serde_json::json!({"forced": unpublished, "generation": generation}),
        },
        now,
    )
    .await?;
    sqlx::query("UPDATE catalogs SET deleted_at = $2 WHERE id = $1")
        .bind(catalog.id)
        .bind(now)
        .execute(&mut *account_tx)
        .await?;
    cleanups::owe_catalog(&mut account_tx, db.store(), catalog.id, now).await?;
    // The catalog, and its owner's list of catalogs.
    let mut invalidations = vec![Invalidation::CatalogMeta {
        id: catalog.id,
        name: catalog.name,
    }];
    invalidations.extend(owner_team_id.map(|id| Invalidation::Team { id }));
    invalidate(&mut account_tx, &invalidations).await?;

    // 4. Account first: the catalog is gone from lookups. A failure here
    // drops `tx`, which rolls the store back.
    account_tx.commit().await?;
    // The delete has happened. A store commit that fails now only leaves
    // the data unmarked, which the owed cleanup removes all the same, so
    // the caller is not told the delete failed.
    if let Err(error) = tx.commit().await {
        tracing::warn!(
            kind = db.classify(error).kind(),
            "the store side of a catalog deletion did not commit; the cleanup removes it"
        );
    }
    Ok(())
}

/// Refuses a malformed catalog name as unknown before it reaches SQL: a NUL,
/// which Postgres refuses as a database error (a 500), names no catalog
/// (issue #339).
///
/// # Errors
/// [`OkfError::CatalogNotSelected`] for a name [`is_valid_name`] refuses.
#[tracing::instrument(level = "trace", skip_all)]
fn check_name(name: &str) -> Result<(), OkfError> {
    if is_valid_name(name) {
        Ok(())
    } else {
        Err(OkfError::CatalogNotSelected {
            catalog: name.to_string(),
        })
    }
}

/// One catalog event to record: [`record_catalog`]'s input.
pub(crate) struct CatalogEvent<'a> {
    /// What happened.
    pub action: Action,
    /// The catalog's id; its row must still exist (its owner team is read).
    pub catalog_id: i64,
    /// The catalog's name.
    pub catalog: &'a str,
    /// The entry's details.
    pub details: serde_json::Value,
}

/// Records an entry about a catalog in the change's transaction, belonging
/// to the catalog's owner team ([`audit::catalog_teams`]).
///
/// # Arguments
/// * `conn` - the change's transaction.
/// * `actor` - who.
/// * `event` - what.
/// * `now` - when (server clock).
///
/// # Errors
/// [`OkfError::Database`] (which fails the change).
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "catalogs.record_catalog"))]
pub(crate) async fn record_catalog(
    conn: &mut PgConnection,
    actor: &AuditActor,
    event: CatalogEvent<'_>,
    now: DateTime<Utc>,
) -> Result<(), OkfError> {
    let teams = audit::catalog_teams(conn, event.catalog_id).await?;
    audit::record(
        conn,
        actor,
        &Entry {
            action: event.action,
            target: Target::Catalog(event.catalog),
            person: None,
            teams: &teams,
            details: event.details,
        },
        now,
    )
    .await?;
    Ok(())
}

/// Takes the catalog's publish lock for the rest of the transaction (design
/// "Publish"): publishes, imports and deletes of one catalog run one at a time.
///
/// # Arguments
/// * `conn` - the transaction's connection (pass `&mut tx`).
/// * `catalog_id` - the catalog's id.
///
/// # Errors
/// [`OkfError::Database`], including for an id above `i32::MAX` (the
/// two-key form takes `int4` keys; the server's cast reports "integer out of
/// range" rather than this process panicking).
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "catalogs.lock", catalog_id = catalog_id))]
pub(crate) async fn lock(conn: &mut PgConnection, catalog_id: i64) -> Result<(), OkfError> {
    // The two-key form, under this crate's namespace, so the lock cannot
    // collide with another application's advisory locks on a shared database.
    // `pg_advisory_xact_lock` returns `void`, so run it with `execute`.
    sqlx::query("SELECT pg_advisory_xact_lock($1, $2::int4)")
        .bind(CATALOG_LOCK_NAMESPACE)
        .bind(catalog_id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Bumps the catalog's generation on its store and returns the new value.
/// The row is `catalog.catalog_state`, so `conn` is a store transaction
/// that already holds the catalog's lock.
///
/// # Arguments
/// * `conn` - the store transaction's connection.
/// * `catalog_id` - the catalog.
///
/// # Returns
/// The generation after the bump.
///
/// # Errors
/// [`OkfError::Database`]; a missing row is `RowNotFound` (the catalog has no
/// store side).
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "catalogs.bump_generation", catalog_id = catalog_id))]
pub(crate) async fn bump_generation(
    conn: &mut PgConnection,
    catalog_id: i64,
) -> Result<i64, OkfError> {
    Ok(sqlx::query_scalar(
        "UPDATE catalog_state SET generation = generation + 1 WHERE id = $1 RETURNING generation",
    )
    .bind(catalog_id)
    .fetch_one(&mut *conn)
    .await?)
}

/// Finds catalog `name` on the account database and takes its lock
/// ([`lock`], on the store), then reads it again: a delete holding the lock
/// may have removed (or replaced) the catalog while this waited. The
/// catalog's `generation` is read from the store's `catalog_state` under the
/// lock, so no publish or import can move it until the transaction ends.
///
/// # Arguments
/// * `account` - the account transaction's connection.
/// * `store` - the store transaction's connection.
/// * `name` - the catalog's name.
///
/// # Returns
/// The catalog's row, read under its lock, with its generation.
///
/// # Errors
/// [`OkfError::CatalogNotSelected`] for an unknown (or malformed) catalog,
/// or one deleted or replaced while this waited; [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "catalogs.lock_by_name"))]
pub(crate) async fn lock_by_name(
    account: &mut PgConnection,
    store: &mut PgConnection,
    name: &str,
) -> Result<Catalog, OkfError> {
    let found = find(&mut *account, name).await?;
    lock(&mut *store, found.id).await?;
    let mut locked = find(&mut *account, name).await?;
    if locked.id != found.id {
        return Err(OkfError::CatalogNotSelected {
            catalog: name.to_string(),
        });
    }
    let generation: Option<i64> =
        sqlx::query_scalar("SELECT generation FROM catalog_state WHERE id = $1")
            .bind(locked.id)
            .fetch_optional(&mut *store)
            .await?;
    // No state row: a delete of the store side committed first.
    locked.generation = generation.ok_or_else(|| OkfError::CatalogNotSelected {
        catalog: name.to_string(),
    })?;
    Ok(locked)
}

/// Refuses a catalog that is no longer the incarnation a caller was
/// admitted to: the access gates are loaded by name, so after a catalog is
/// deleted and created again under its name, a request admitted from gates
/// that predate the replacement (an unlock, a membership) must not reach
/// the replacement. Shared by export and import, which look the catalog up
/// by name in their own transaction.
///
/// # Arguments
/// * `catalog` - the catalog's name, for the message.
/// * `found` - the `catalogs.id` the transaction found.
/// * `expected` - the id the caller was admitted to; `None` checks nothing.
///
/// # Errors
/// [`OkfError::Busy`] on a mismatch: retryable, since a retry is admitted
/// against fresh gates (and then refused or let in on its own terms).
#[tracing::instrument(level = "debug", skip_all)]
pub fn check_incarnation(catalog: &str, found: i64, expected: Option<i64>) -> Result<(), OkfError> {
    match expected {
        Some(expected) if expected != found => Err(OkfError::Busy {
            reason: format!("catalog {catalog:?} was replaced since this request was admitted"),
        }),
        Some(_) | None => Ok(()),
    }
}

/// The current administrative generation (design "Noticing changes made
/// outside the server").
///
/// # Arguments
/// * `executor` - a pool, connection or transaction.
///
/// # Returns
/// The counter every `catalog` and `key` change bumps.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "catalogs.admin_generation"))]
pub async fn admin_generation<'e>(executor: impl PgExecutor<'e>) -> Result<i64, OkfError> {
    let generation = sqlx::query_scalar("SELECT admin_generation FROM server_state")
        .fetch_one(executor)
        .await?;
    Ok(generation)
}

/// Is this a unique-constraint violation (SQLSTATE 23505)?
///
/// # Arguments
/// * `error` - a driver error.
///
/// # Returns
/// `true` only for a database error with code 23505.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) fn is_unique_violation(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .is_some_and(|db_error| db_error.is_unique_violation())
}

#[cfg(test)]
// `pub(crate)` so other store modules' tests share the lock helper below.
pub(crate) mod tests {
    use super::*;

    /// Issue #1117: the telemetry sentinels can never be a catalog's name.
    #[test]
    fn the_telemetry_sentinels_are_not_valid_catalog_names() {
        assert!(!is_valid_name(CATALOG_LABEL_NONE));
        assert!(!is_valid_name(CATALOG_LABEL_MULTIPLE));
        assert!(is_valid_name("none"), "the old words are ordinary names");
    }

    #[test]
    fn check_incarnation_refuses_only_another_catalog_id() {
        assert!(
            check_incarnation("main", 7, None).is_ok(),
            "nothing to check"
        );
        assert!(check_incarnation("main", 7, Some(7)).is_ok());
        assert!(matches!(
            check_incarnation("main", 8, Some(7)),
            Err(OkfError::Busy { .. })
        ));
    }
    use crate::db::{test_db, TEST_TEAM};
    use crate::store::audit::AuditActor;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 2, 12, 0, 0).unwrap()
    }

    /// [`update_details`] of the description alone, with no owner check:
    /// what `update_description` was before M17.
    ///
    /// # Returns
    /// The description before.
    async fn describe(
        pool: &PgPool,
        name: &str,
        description: &str,
        actor: &AuditActor,
        now: DateTime<Utc>,
    ) -> Result<String, OkfError> {
        let change = DetailsChange {
            catalog: name,
            display_name: None,
            description: Some(description),
            owner: None,
        };
        update_details(pool, &change, actor, now)
            .await
            .map(|previous| previous.description)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_stores_the_display_name_and_find_reads_it() {
        // Arrange
        let db = test_db().await;
        let display = DisplayName::parse("Work notes").unwrap();

        // Act
        let row = create(
            &db.pool,
            &NewCatalog {
                name: "m-0000000000000001",
                display_name: &display,
                description: "",
                owner_team: TEST_TEAM,
            },
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();

        // Assert: both the `RETURNING` row and a later read carry it.
        assert_eq!(row.display_name, "Work notes");
        let found = find(&db.pool, "m-0000000000000001").await.unwrap();
        assert_eq!(found.display_name, "Work notes");
        let listed = list(&db.pool).await.unwrap();
        assert_eq!(listed[0].display_name, "Work notes");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_database_refuses_an_over_long_display_name_on_every_table() {
        // Arrange: a user and its personal team (the fixture), and a catalog.
        let db = test_db().await;
        create(
            &db.pool,
            &NewCatalog {
                name: "pg",
                display_name: &DisplayName::default(),
                description: "",
                owner_team: TEST_TEAM,
            },
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();

        // `AssertSqlSafe`: `table` is one of the literals below.
        for table in ["users", "teams", "catalogs"] {
            // Act and assert: 80 characters fit (`char_length` counts
            // characters, so 80 two-byte ones do too); 81 are refused.
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE {table} SET display_name = repeat('\u{e9}', 80)"
            )))
            .execute(&db.pool)
            .await
            .unwrap_or_else(|error| panic!("{table}: 80 characters should fit: {error}"));
            let refused = sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE {table} SET display_name = repeat('x', 81)"
            )))
            .execute(&db.pool)
            .await;
            let error = refused.expect_err("the CHECK should refuse 81 characters");
            assert_eq!(
                error.as_database_error().and_then(|e| e.constraint()),
                Some(format!("{table}_display_name_length").as_str()),
                "{table}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_list_and_find() {
        let db = test_db().await;
        // Read first: the fixture user already bumped the generation.
        let start = admin_generation(&db.pool).await.unwrap();

        let pg = create(
            &db.pool,
            &NewCatalog {
                name: "pg",
                display_name: &DisplayName::default(),
                description: "pgrace's catalog",
                owner_team: TEST_TEAM,
            },
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();
        create(
            &db.pool,
            &NewCatalog {
                name: "aa-work",
                display_name: &DisplayName::default(),
                description: "",
                owner_team: TEST_TEAM,
            },
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();

        assert_eq!(
            (pg.name.as_str(), pg.generation, pg.created_at),
            ("pg", 0, now())
        );
        let names: Vec<String> = list(&db.pool)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, ["aa-work", "pg"]);
        assert_eq!(find(&db.pool, "pg").await.unwrap(), pg);
        assert!(matches!(
            find(&db.pool, "nope").await,
            Err(OkfError::CatalogNotSelected { catalog }) if catalog == "nope"
        ));
        assert_eq!(admin_generation(&db.pool).await.unwrap(), start + 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_new_catalogs_heading_is_its_display_name_else_catalog() {
        let db = test_db().await;
        let named = DisplayName::parse("Work notes").unwrap();
        let mut headings = Vec::new();
        for (name, display_name) in [("named", &named), ("plain", &DisplayName::default())] {
            let catalog = create(
                &db.pool,
                &NewCatalog {
                    name,
                    display_name,
                    description: "About it.",
                    owner_team: TEST_TEAM,
                },
                &AuditActor::cli(),
                now(),
            )
            .await
            .unwrap();
            let rows = documents::published(&db.store(), catalog.id).await.unwrap();
            let index = rows.iter().find(|row| row.path == "index.md").unwrap();
            headings.push(String::from_utf8(index.content.clone()).unwrap());
        }

        assert!(
            headings[0].contains("# Work notes\n\nAbout it.\n"),
            "{}",
            headings[0]
        );
        assert!(
            headings[1].contains("# Catalog\n\nAbout it.\n"),
            "{}",
            headings[1]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_new_catalog_holds_its_starter_files_at_generation_0() {
        let db = test_db().await;

        let pg = create(
            &db.pool,
            &NewCatalog {
                name: "pg",
                display_name: &DisplayName::default(),
                description: "pgrace's catalog",
                owner_team: TEST_TEAM,
            },
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();

        let rows = documents::published(&db.store(), pg.id).await.unwrap();
        let stored: Vec<(&str, &[u8], i32)> = rows
            .iter()
            .map(|row| (row.path.as_str(), row.content.as_slice(), row.revision))
            .collect();
        let starter = starter_files("Catalog", "pgrace's catalog", now());
        assert_eq!(
            stored,
            [
                ("index.md", starter[0].1.as_bytes(), 1),
                ("log.md", starter[1].1.as_bytes(), 1),
            ]
        );
        assert!(starter[0].1.contains("# Catalog\n\npgrace's catalog\n"));
        assert_eq!(pg.generation, 0);
        // The derived columns are filled in, as an import's are.
        let kinds: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT kind, title FROM catalog.documents WHERE catalog_id = $1 ORDER BY path",
        )
        .bind(pg.id)
        .fetch_all(&crate::db::store_pool(&db.pool))
        .await
        .unwrap();
        assert_eq!(kinds[0].0, "index");
        assert_eq!(kinds[1].0, "log");
        let revisions: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT path, op, message FROM catalog.revisions WHERE catalog_id = $1 ORDER BY path",
        )
        .bind(pg.id)
        .fetch_all(&crate::db::store_pool(&db.pool))
        .await
        .unwrap();
        let create_catalog = |path: &str| {
            (
                path.to_string(),
                "create".to_string(),
                STARTER_MESSAGE.to_string(),
            )
        };
        assert_eq!(
            revisions,
            [create_catalog("index.md"), create_catalog("log.md")]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_new_catalog_is_owned_by_the_named_team() {
        let db = test_db().await;

        let catalog = create(
            &db.pool,
            &NewCatalog {
                name: "pg",
                display_name: &DisplayName::default(),
                description: "",
                owner_team: TEST_TEAM,
            },
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();

        assert_eq!(catalog.owner_team.as_deref(), Some(TEST_TEAM));
        assert_eq!(find(&db.pool, "pg").await.unwrap(), catalog);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unknown_owner_is_unknown_name_and_creates_nothing() {
        let db = test_db().await;
        // Read first: the fixture user already bumped the generation.
        let start = admin_generation(&db.pool).await.unwrap();

        let made = create(
            &db.pool,
            &NewCatalog {
                name: "pg",
                display_name: &DisplayName::default(),
                description: "",
                owner_team: "nobody",
            },
            &AuditActor::cli(),
            now(),
        )
        .await;

        assert!(
            matches!(&made, Err(OkfError::UnknownName { noun: "team", name }) if name == "nobody"),
            "{made:?}"
        );
        assert!(list(&db.pool).await.unwrap().is_empty());
        assert_eq!(admin_generation(&db.pool).await.unwrap(), start);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_owner_moves_a_catalog_and_returns_the_previous_owner() {
        let db = test_db().await;
        let alice = crate::store::users::NewUser {
            name: "alice",
            display_name: &DisplayName::default(),
        };
        crate::store::users::create(&db.pool, &alice, &AuditActor::cli(), now())
            .await
            .unwrap();
        create(
            &db.pool,
            &NewCatalog {
                name: "pg",
                display_name: &DisplayName::default(),
                description: "",
                owner_team: TEST_TEAM,
            },
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();
        let before = admin_generation(&db.pool).await.unwrap();

        let previous = set_owner(&db.pool, "pg", "alice", &AuditActor::cli(), Utc::now())
            .await
            .unwrap();

        assert_eq!(previous.as_deref(), Some(TEST_TEAM));
        assert_eq!(
            find(&db.pool, "pg").await.unwrap().owner_team.as_deref(),
            Some("alice")
        );
        assert_eq!(admin_generation(&db.pool).await.unwrap(), before + 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_owner_assigns_a_legacy_catalog() {
        let db = test_db().await;
        sqlx::query("INSERT INTO catalogs (name, created_at) VALUES ('legacy', now())")
            .execute(&db.pool)
            .await
            .unwrap();
        assert_eq!(find(&db.pool, "legacy").await.unwrap().owner_team, None);

        let previous = set_owner(
            &db.pool,
            "legacy",
            TEST_TEAM,
            &AuditActor::cli(),
            Utc::now(),
        )
        .await
        .unwrap();

        assert_eq!(previous, None);
        assert_eq!(
            find(&db.pool, "legacy")
                .await
                .unwrap()
                .owner_team
                .as_deref(),
            Some(TEST_TEAM)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_owner_refuses_an_unknown_catalog_or_team_and_changes_nothing() {
        let db = test_db().await;
        create(
            &db.pool,
            &NewCatalog {
                name: "pg",
                display_name: &DisplayName::default(),
                description: "",
                owner_team: TEST_TEAM,
            },
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();
        let start = admin_generation(&db.pool).await.unwrap();

        let no_catalog =
            set_owner(&db.pool, "nope", TEST_TEAM, &AuditActor::cli(), Utc::now()).await;
        let no_team = set_owner(&db.pool, "pg", "nobody", &AuditActor::cli(), Utc::now()).await;

        assert!(
            matches!(&no_catalog, Err(OkfError::CatalogNotSelected { catalog }) if catalog == "nope"),
            "{no_catalog:?}"
        );
        assert!(
            matches!(&no_team, Err(OkfError::UnknownName { noun: "team", .. })),
            "{no_team:?}"
        );
        assert_eq!(
            find(&db.pool, "pg").await.unwrap().owner_team.as_deref(),
            Some(TEST_TEAM)
        );
        assert_eq!(admin_generation(&db.pool).await.unwrap(), start);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn update_details_replaces_the_description_and_returns_the_old_text() {
        let db = test_db().await;
        // Read first: the fixture user already bumped the generation.
        let start = admin_generation(&db.pool).await.unwrap();
        let pg = create(
            &db.pool,
            &NewCatalog {
                name: "pg",
                display_name: &DisplayName::default(),
                description: "copy",
                owner_team: TEST_TEAM,
            },
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();

        let previous = describe(
            &db.pool,
            "pg",
            "source of truth",
            &AuditActor::cli(),
            Utc::now(),
        )
        .await
        .unwrap();
        let cleared = describe(&db.pool, "pg", "", &AuditActor::cli(), Utc::now())
            .await
            .unwrap();

        assert_eq!(
            (previous.as_str(), cleared.as_str()),
            ("copy", "source of truth")
        );
        let row = find(&db.pool, "pg").await.unwrap();
        assert_eq!(
            (row.description.as_str(), row.generation),
            ("", pg.generation)
        );
        assert_eq!(admin_generation(&db.pool).await.unwrap(), start + 3);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_updates_each_return_the_text_they_replaced() {
        let db = test_db().await;
        create(
            &db.pool,
            &NewCatalog {
                name: "pg",
                display_name: &DisplayName::default(),
                description: "start",
                owner_team: TEST_TEAM,
            },
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();
        // Hold the row so both updates queue behind it and then race.
        let mut holder = db.pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM catalogs WHERE name = 'pg' FOR UPDATE")
            .execute(&mut *holder)
            .await
            .unwrap();
        let spawn_update = |text: &'static str| {
            let pool = db.pool.clone();
            tokio::spawn(async move {
                describe(&pool, "pg", text, &AuditActor::cli(), Utc::now()).await
            })
        };
        let first = spawn_update("one");
        let second = spawn_update("two");
        until_locks_are_awaited(&db.pool, 2).await;

        holder.commit().await.unwrap();
        let mut previous = [
            first.await.unwrap().unwrap(),
            second.await.unwrap().unwrap(),
        ];
        let last = find(&db.pool, "pg").await.unwrap().description;

        // One update replaced "start"; the other replaced the first's text.
        // Without the row lock in the subquery, both could report "start".
        previous.sort();
        let winner_first = if last == "one" { "two" } else { "one" };
        let mut expected = ["start".to_string(), winner_first.to_string()];
        expected.sort();
        assert_eq!(previous, expected);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn update_details_refuses_an_unknown_catalog() {
        let db = test_db().await;
        // Read first: the fixture user already bumped the generation.
        let start = admin_generation(&db.pool).await.unwrap();

        let outcome = describe(&db.pool, "nope", "x", &AuditActor::cli(), Utc::now()).await;

        assert!(matches!(
            outcome,
            Err(OkfError::CatalogNotSelected { catalog }) if catalog == "nope"
        ));
        assert_eq!(admin_generation(&db.pool).await.unwrap(), start);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bad_and_duplicate_names_are_refused() {
        let db = test_db().await;
        // Read first: the fixture user already bumped the generation.
        let start = admin_generation(&db.pool).await.unwrap();
        create(
            &db.pool,
            &NewCatalog {
                name: "pg",
                display_name: &DisplayName::default(),
                description: "",
                owner_team: TEST_TEAM,
            },
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();

        for name in ["", "Bad Name", "a/b", "ünï"] {
            assert!(
                matches!(
                    create(
                        &db.pool,
                        &NewCatalog {
                            name,
                            display_name: &DisplayName::default(),
                            description: "",
                            owner_team: TEST_TEAM
                        },
                        &AuditActor::cli(),
                        now()
                    )
                    .await,
                    Err(OkfError::Refused { .. })
                ),
                "{name}"
            );
        }
        let duplicate = create(
            &db.pool,
            &NewCatalog {
                name: "pg",
                display_name: &DisplayName::default(),
                description: "",
                owner_team: TEST_TEAM,
            },
            &AuditActor::cli(),
            now(),
        )
        .await;

        assert!(
            matches!(duplicate, Err(OkfError::Refused { reason }) if reason.contains("already exists"))
        );
        assert_eq!(admin_generation(&db.pool).await.unwrap(), start + 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn delete_refuses_unpublished_work_unless_forced() {
        let db = test_db().await;
        // Read first: the fixture user already bumped the generation.
        let start = admin_generation(&db.pool).await.unwrap();
        let pg = create(
            &db.pool,
            &NewCatalog {
                name: "pg",
                display_name: &DisplayName::default(),
                description: "",
                owner_team: TEST_TEAM,
            },
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();
        let changeset: i64 = sqlx::query_scalar(
            "INSERT INTO catalog.changesets (catalog_id, actor, state, created_at, updated_at) \
             VALUES ($1, 'a', 'open', $2, $2) RETURNING id",
        )
        .bind(pg.id)
        .bind(now())
        .fetch_one(&crate::db::store_pool(&db.pool))
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO catalog.changeset_entries (changeset_id, path, content) VALUES ($1, 'a.md', $2)",
        )
        .bind(changeset)
        .bind(b"x".to_vec())
        .execute(&crate::db::store_pool(&db.pool))
        .await
        .unwrap();

        let refused = delete(&db.pool, "pg", false, &AuditActor::cli(), Utc::now()).await;
        assert!(matches!(refused, Err(OkfError::Refused { reason }) if reason.contains("--force")));

        delete(&db.pool, "pg", true, &AuditActor::cli(), Utc::now())
            .await
            .unwrap();

        assert!(list(&db.pool).await.unwrap().is_empty());
        // The data goes with the cleanup the delete owes.
        crate::store::cleanups::settle_for_tests(&db.pool).await;
        let entries: i64 = sqlx::query_scalar("SELECT count(*) FROM catalog.changeset_entries")
            .fetch_one(&crate::db::store_pool(&db.pool))
            .await
            .unwrap();
        assert_eq!(entries, 0);
        assert_eq!(admin_generation(&db.pool).await.unwrap(), start + 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_empty_open_changeset_does_not_block_delete() {
        let db = test_db().await;
        let pg = create(
            &db.pool,
            &NewCatalog {
                name: "pg",
                display_name: &DisplayName::default(),
                description: "",
                owner_team: TEST_TEAM,
            },
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO catalog.changesets (catalog_id, actor, state, created_at, updated_at) \
             VALUES ($1, 'a', 'open', $2, $2)",
        )
        .bind(pg.id)
        .bind(now())
        .execute(&crate::db::store_pool(&db.pool))
        .await
        .unwrap();

        delete(&db.pool, "pg", false, &AuditActor::cli(), Utc::now())
            .await
            .unwrap();

        assert!(list(&db.pool).await.unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_lock_holds_other_transactions_off_until_commit() {
        let db = test_db().await;
        let pg = create(
            &db.pool,
            &NewCatalog {
                name: "pg",
                display_name: &DisplayName::default(),
                description: "",
                owner_team: TEST_TEAM,
            },
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();
        let mut holder = db.pool.begin().await.unwrap();

        lock(&mut holder, pg.id).await.unwrap();

        let try_lock = "SELECT pg_try_advisory_xact_lock($1, $2::int4)";
        let mut other = db.pool.begin().await.unwrap();
        let taken: bool = sqlx::query_scalar(try_lock)
            .bind(CATALOG_LOCK_NAMESPACE)
            .bind(pg.id)
            .fetch_one(&mut *other)
            .await
            .unwrap();
        assert!(!taken, "a second transaction got the held lock");
        holder.commit().await.unwrap();
        let taken: bool = sqlx::query_scalar(try_lock)
            .bind(CATALOG_LOCK_NAMESPACE)
            .bind(pg.id)
            .fetch_one(&mut *other)
            .await
            .unwrap();
        assert!(taken, "the lock outlived its transaction");
    }

    /// Drops a catalog's every document and revision (its starter files, in
    /// a new catalog): an empty catalog, as one created before starter
    /// files existed is, for tests that build its rows themselves.
    pub(crate) async fn drop_starter_files(pool: &PgPool, catalog_id: i64) {
        // Two static statements: sqlx refuses SQL built at run time.
        for statement in [
            "DELETE FROM catalog.revisions WHERE catalog_id = $1",
            "DELETE FROM catalog.documents WHERE catalog_id = $1",
        ] {
            sqlx::query(statement)
                .bind(catalog_id)
                .execute(CatalogDb::main_of(pool).executor())
                .await
                .expect("drop the starter files");
        }
    }

    /// Counts the lock requests blocked in this test's own database: the
    /// test server may be shared by other tests, whose waits are not ours.
    /// In `separate` store mode the store is a database named after this
    /// one, so the match is on the name's start.
    pub(crate) const AWAITED_LOCKS: &str = concat!(
        "SELECT count(*) FROM pg_locks l JOIN pg_stat_activity a ON a.pid = l.pid \
         WHERE NOT l.granted AND ",
        crate::test_locks::own_databases!("a.datname")
    );

    /// Waits until some backend is blocked on a lock, so a test knows the
    /// spawned delete (or import) has reached the point it should wait at.
    pub(crate) async fn until_a_lock_is_awaited(pool: &PgPool) {
        until_locks_are_awaited(pool, 1).await;
    }

    /// Waits until at least `count` lock requests are blocked.
    pub(crate) async fn until_locks_are_awaited(pool: &PgPool, count: i64) {
        for _ in 0..200 {
            let waiting: i64 = sqlx::query_scalar(AWAITED_LOCKS)
                .fetch_one(pool)
                .await
                .unwrap();
            if waiting >= count {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("fewer than {count} lock requests were ever blocked");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_create_that_waited_on_a_team_delete_is_unknown_name() {
        // Arrange: a transaction deletes team `family` and holds its commit
        // while the create runs.
        let db = test_db().await;
        let family = crate::store::teams::NewTeam {
            name: "family",
            display_name: &DisplayName::default(),
            owner: crate::db::TEST_USER,
        };
        crate::store::teams::create(&db.pool, &family, &AuditActor::cli(), now())
            .await
            .unwrap();
        let mut deleting = db.pool.begin().await.unwrap();
        sqlx::query("DELETE FROM teams WHERE name = 'family'")
            .execute(&mut *deleting)
            .await
            .unwrap();
        // `tokio::spawn` runs the create concurrently, like a thread; its
        // `FOR KEY SHARE` read of the team waits on the delete's row lock.
        let pool = db.pool.clone();
        let creating = tokio::spawn(async move {
            create(
                &pool,
                &NewCatalog {
                    name: "pg",
                    display_name: &DisplayName::default(),
                    description: "",
                    owner_team: "family",
                },
                &AuditActor::cli(),
                now(),
            )
            .await
        });
        until_a_lock_is_awaited(&db.pool).await;

        deleting.commit().await.unwrap();
        let outcome = creating.await.unwrap();

        // Not a foreign-key violation (a 500): the team is simply unknown.
        assert!(
            matches!(&outcome, Err(OkfError::UnknownName { noun: "team", .. })),
            "{outcome:?}"
        );
        assert!(list(&db.pool).await.unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_delete_that_waited_finds_the_catalog_gone() {
        let db = test_db().await;
        // Read first: the fixture user already bumped the generation.
        let start = admin_generation(&db.pool).await.unwrap();
        let pg = create(
            &db.pool,
            &NewCatalog {
                name: "pg",
                display_name: &DisplayName::default(),
                description: "",
                owner_team: TEST_TEAM,
            },
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();
        // Another delete's account side: the registry row, held until it
        // commits (a delete takes it before it takes the store).
        let mut first = db.pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM catalogs WHERE id = $1 FOR UPDATE")
            .bind(pg.id)
            .execute(&mut *first)
            .await
            .unwrap();
        // `tokio::spawn` runs the second delete concurrently, like a thread;
        // it blocks on the row `first` holds.
        let pool = db.pool.clone();
        let second = tokio::spawn(async move {
            delete(&pool, "pg", false, &AuditActor::cli(), Utc::now()).await
        });
        until_a_lock_is_awaited(&db.pool).await;

        sqlx::query("UPDATE catalogs SET deleted_at = now() WHERE id = $1")
            .bind(pg.id)
            .execute(&mut *first)
            .await
            .unwrap();
        first.commit().await.unwrap();
        let outcome = second.await.unwrap();

        assert!(matches!(
            outcome,
            Err(OkfError::CatalogNotSelected { catalog }) if catalog == "pg"
        ));
        assert_eq!(admin_generation(&db.pool).await.unwrap(), start + 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_entry_written_during_a_delete_is_refused_not_cascaded() {
        let db = test_db().await;
        let pg = create(
            &db.pool,
            &NewCatalog {
                name: "pg",
                display_name: &DisplayName::default(),
                description: "",
                owner_team: TEST_TEAM,
            },
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();
        let changeset: i64 = sqlx::query_scalar(
            "INSERT INTO catalog.changesets (catalog_id, actor, state, created_at, updated_at) \
             VALUES ($1, 'a', 'open', $2, $2) RETURNING id",
        )
        .bind(pg.id)
        .bind(now())
        .fetch_one(&crate::db::store_pool(&db.pool))
        .await
        .unwrap();
        // A session's write, not yet committed when the delete starts.
        let mut writer = crate::db::store_pool(&db.pool).begin().await.unwrap();
        sqlx::query(
            "INSERT INTO catalog.changeset_entries (changeset_id, path, content) VALUES ($1, 'a.md', $2)",
        )
        .bind(changeset)
        .bind(b"x".to_vec())
        .execute(&mut *writer)
        .await
        .unwrap();
        let pool = db.pool.clone();
        let deleting = tokio::spawn(async move {
            delete(&pool, "pg", false, &AuditActor::cli(), Utc::now()).await
        });
        until_a_lock_is_awaited(&db.pool).await;

        writer.commit().await.unwrap();
        let outcome = deleting.await.unwrap();

        assert!(matches!(outcome, Err(OkfError::Refused { reason }) if reason.contains("--force")));
        assert_eq!(list(&db.pool).await.unwrap().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_id_beyond_int4_is_a_database_error() {
        let db = test_db().await;
        let mut tx = db.pool.begin().await.unwrap();

        let outcome = lock(&mut tx, i64::from(i32::MAX) + 1).await;

        assert!(matches!(outcome, Err(OkfError::Database { .. })));
    }

    // ---- The audit log (M14 Task 6b) ----

    use crate::store::audit::testing::entries;
    use serde_json::json;

    /// Creates catalog `name`, owned by [`TEST_TEAM`].
    async fn seed(pool: &PgPool, name: &str) -> Catalog {
        let new = NewCatalog {
            name,
            display_name: &DisplayName::default(),
            description: "",
            owner_team: TEST_TEAM,
        };
        create(pool, &new, &AuditActor::cli(), now()).await.unwrap()
    }

    /// Creates the shared team `family`, owned by the fixture user.
    async fn family(pool: &PgPool) {
        let team = crate::store::teams::NewTeam {
            name: "family",
            display_name: &DisplayName::default(),
            owner: crate::db::TEST_USER,
        };
        crate::store::teams::create(pool, &team, &AuditActor::cli(), now())
            .await
            .unwrap();
    }

    /// Gives catalog `catalog_id` an open changeset with one entry.
    async fn unpublished_work(pool: &PgPool, catalog_id: i64) {
        let changeset: i64 = sqlx::query_scalar(
            "INSERT INTO catalog.changesets (catalog_id, actor, state, created_at, updated_at) \
             VALUES ($1, 'a', 'open', $2, $2) RETURNING id",
        )
        .bind(catalog_id)
        .bind(now())
        .fetch_one(&crate::db::store_pool(pool))
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO catalog.changeset_entries (changeset_id, path, content) VALUES ($1, 'a.md', 'x')",
        )
        .bind(changeset)
        .execute(&crate::db::store_pool(pool))
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_records_catalog_created_for_its_owner_team() {
        let db = test_db().await;

        seed(&db.pool, "pg").await;

        let created = entries(&db.pool, "catalog.created").await;
        assert_eq!(created.len(), 1, "{created:?}");
        assert_eq!(created[0].target_kind, "catalog");
        assert_eq!(created[0].target.as_deref(), Some("pg"));
        assert_eq!(created[0].person, None);
        // A personal team shows as its person.
        assert_eq!(created[0].teams, [crate::db::TEST_USER]);
        assert_eq!(created[0].details, json!({}));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_owner_records_both_teams_and_nothing_for_the_same_owner() {
        let db = test_db().await;
        family(&db.pool).await;
        seed(&db.pool, "pg").await;
        let before = admin_generation(&db.pool).await.unwrap();

        set_owner(&db.pool, "pg", "family", &AuditActor::cli(), now())
            .await
            .unwrap();
        let after = admin_generation(&db.pool).await.unwrap();
        let again = set_owner(&db.pool, "pg", "family", &AuditActor::cli(), now())
            .await
            .unwrap();

        assert_eq!(again.as_deref(), Some("family"));
        assert_eq!(after, before + 1);
        assert_eq!(
            admin_generation(&db.pool).await.unwrap(),
            after,
            "no change"
        );
        let moved = entries(&db.pool, "catalog.owner_changed").await;
        assert_eq!(moved.len(), 1, "{moved:?}");
        assert_eq!(moved[0].target.as_deref(), Some("pg"));
        assert_eq!(moved[0].teams, [crate::db::TEST_USER, "family"]);
        let (from, to) = (&moved[0].details["from"], &moved[0].details["to"]);
        assert!(
            from.is_i64() && to.is_i64() && from != to,
            "{:?}",
            moved[0].details
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_owner_of_a_legacy_catalog_records_no_old_team() {
        let db = test_db().await;
        sqlx::query("INSERT INTO catalogs (name, created_at) VALUES ('legacy', now())")
            .execute(&db.pool)
            .await
            .unwrap();

        set_owner(&db.pool, "legacy", TEST_TEAM, &AuditActor::cli(), now())
            .await
            .unwrap();

        let moved = entries(&db.pool, "catalog.owner_changed").await;
        assert_eq!(moved.len(), 1, "{moved:?}");
        assert_eq!(moved[0].teams, [crate::db::TEST_USER]);
        assert_eq!(moved[0].details["from"], serde_json::Value::Null);
        assert!(moved[0].details.get("from").is_some(), "the key is there");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn update_details_records_the_description_length_and_nothing_for_the_same_text() {
        let db = test_db().await;
        seed(&db.pool, "pg").await;

        describe(&db.pool, "pg", "naïve", &AuditActor::cli(), now())
            .await
            .unwrap();
        let previous = describe(&db.pool, "pg", "naïve", &AuditActor::cli(), now())
            .await
            .unwrap();

        assert_eq!(previous, "naïve");
        let changed = entries(&db.pool, "catalog.description_changed").await;
        assert_eq!(changed.len(), 1, "{changed:?}");
        assert_eq!(changed[0].target.as_deref(), Some("pg"));
        assert_eq!(changed[0].teams, [crate::db::TEST_USER]);
        // Characters, not bytes; never the text.
        assert_eq!(changed[0].details, json!({"length": 5}));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn delete_records_each_deletion_and_nothing_when_refused() {
        let db = test_db().await;
        seed(&db.pool, "plain").await;
        let busy = seed(&db.pool, "busy").await;
        unpublished_work(&db.pool, busy.id).await;
        let actor = AuditActor::cli();

        let refused = delete(&db.pool, "busy", false, &actor, now()).await;
        delete(&db.pool, "plain", false, &actor, now())
            .await
            .unwrap();
        delete(&db.pool, "busy", true, &actor, now()).await.unwrap();

        assert!(matches!(refused, Err(OkfError::Refused { .. })));
        let deleted = entries(&db.pool, "catalog.deleted").await;
        assert_eq!(deleted.len(), 2, "{deleted:?}");
        assert_eq!(deleted[0].target.as_deref(), Some("plain"));
        // Read before the row went.
        assert_eq!(deleted[0].teams, [crate::db::TEST_USER]);
        assert_eq!(
            deleted[0].details,
            json!({"forced": false, "generation": 0})
        );
        assert_eq!(deleted[1].target.as_deref(), Some("busy"));
        assert_eq!(deleted[1].details, json!({"forced": true, "generation": 0}));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_audit_write_fails_the_catalog_change() {
        let db = test_db().await;
        seed(&db.pool, "pg").await;
        family(&db.pool).await;
        crate::http::api::testing::fail_on(&db.pool, "BEFORE INSERT ON audit_entries FOR EACH ROW")
            .await;
        let actor = AuditActor::cli();
        let new = NewCatalog {
            name: "other",
            display_name: &DisplayName::default(),
            description: "",
            owner_team: TEST_TEAM,
        };

        let created = create(&db.pool, &new, &actor, now()).await;
        let moved = set_owner(&db.pool, "pg", "family", &actor, now()).await;
        let described = describe(&db.pool, "pg", "new", &actor, now()).await;
        let deleted = delete(&db.pool, "pg", false, &actor, now()).await;

        assert!(created.is_err() && moved.is_err() && described.is_err() && deleted.is_err());
        // The starter files went with the rolled-back row: only pg's remain.
        let starters: i64 =
            sqlx::query_scalar("SELECT count(*) FROM catalog.revisions WHERE message = $1")
                .bind(STARTER_MESSAGE)
                .fetch_one(&crate::db::store_pool(&db.pool))
                .await
                .unwrap();
        assert_eq!(starters, 2);
        let pg = find(&db.pool, "pg").await.unwrap();
        assert_eq!(pg.owner_team.as_deref(), Some(TEST_TEAM));
        assert_eq!(pg.description, "");
        assert!(matches!(
            find(&db.pool, "other").await,
            Err(OkfError::CatalogNotSelected { .. })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_malformed_catalog_name_is_unknown_and_never_reaches_sql() {
        let db = test_db().await;
        let actor = AuditActor::cli();
        let bad = "p\0g";

        let found = find(&db.pool, bad).await.map(|_| ());
        let moved = set_owner(&db.pool, bad, TEST_TEAM, &actor, now())
            .await
            .map(|_| ());
        let described = describe(&db.pool, bad, "", &actor, now()).await.map(|_| ());
        let deleted = delete(&db.pool, bad, false, &actor, now()).await;

        // A NUL would be a database error (a 500) if it were bound.
        for outcome in [found, moved, described, deleted] {
            assert!(
                matches!(outcome, Err(OkfError::CatalogNotSelected { .. })),
                "{outcome:?}"
            );
        }
    }

    // ---- The maintenance flag (M14 Task 9c) ----

    /// Creates user `name` and adds it to [`TEST_TEAM`] with `role`.
    async fn join_test_team(pool: &PgPool, name: &str, role: crate::store::teams::Role) -> String {
        let user = crate::store::users::NewUser {
            name,
            display_name: &DisplayName::default(),
        };
        crate::store::users::create(pool, &user, &AuditActor::cli(), now())
            .await
            .unwrap();
        let membership = crate::store::teams::Membership {
            team: TEST_TEAM,
            user: name,
            role,
        };
        crate::store::teams::set_member(pool, &membership, &AuditActor::cli(), now())
            .await
            .unwrap();
        sqlx::query_scalar("SELECT id FROM users WHERE name = $1")
            .bind(name)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// A catalog's stored flag.
    async fn maintenance_of(pool: &PgPool, name: &str) -> bool {
        sqlx::query_scalar("SELECT maintenance FROM catalogs WHERE name = $1")
            .bind(name)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_maintenance_records_a_change_bumps_and_nothing_for_the_same_flag() {
        let db = test_db().await;
        seed(&db.pool, "pg").await;
        let change = |to| MaintenanceChange {
            catalog: "pg",
            to,
            owner: None,
        };
        let actor = AuditActor::cli();
        let before = admin_generation(&db.pool).await.unwrap();

        let first = set_maintenance(&db.pool, &change(Switch::Off), &actor, now()).await;
        let after_change = admin_generation(&db.pool).await.unwrap();
        let again = set_maintenance(&db.pool, &change(Switch::Off), &actor, now()).await;
        let after_noop = admin_generation(&db.pool).await.unwrap();
        let unknown = set_maintenance(
            &db.pool,
            &MaintenanceChange {
                catalog: "nowhere",
                ..change(Switch::On)
            },
            &actor,
            now(),
        )
        .await;
        let malformed = set_maintenance(
            &db.pool,
            &MaintenanceChange {
                catalog: "p\0g",
                ..change(Switch::On)
            },
            &actor,
            now(),
        )
        .await;

        assert_eq!(first.unwrap(), Switch::On, "on by default");
        assert_eq!(again.unwrap(), Switch::Off);
        assert!(!maintenance_of(&db.pool, "pg").await);
        assert_eq!(after_change, before + 1);
        assert_eq!(after_noop, after_change, "a no-op bumps nothing");
        for outcome in [unknown, malformed] {
            assert!(
                matches!(outcome, Err(OkfError::CatalogNotSelected { .. })),
                "{outcome:?}"
            );
        }
        let changed = entries(&db.pool, "catalog.maintenance_changed").await;
        assert_eq!(changed.len(), 1, "a no-op records nothing: {changed:?}");
        assert_eq!(changed[0].target.as_deref(), Some("pg"));
        assert_eq!(changed[0].teams, [crate::db::TEST_USER]);
        assert_eq!(changed[0].details, json!({"from": "on", "to": "off"}));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn only_an_owner_of_the_owner_team_passes_the_owner_check() {
        use crate::store::teams::Role;
        let db = test_db().await;
        seed(&db.pool, "pg").await;
        family(&db.pool).await;
        seed(&db.pool, "elsewhere").await;
        set_owner(&db.pool, "elsewhere", "family", &AuditActor::cli(), now())
            .await
            .unwrap();
        let owner = join_test_team(&db.pool, "olive", Role::Owner).await;
        let member = join_test_team(&db.pool, "max", Role::Member).await;
        let as_user = |user_id: &str| AuditActor {
            user_id: Some(user_id.to_string()),
            ..AuditActor::cli()
        };
        let off = |catalog| MaintenanceChange {
            catalog,
            to: Switch::Off,
            owner: None,
        };

        let by_member = set_maintenance(
            &db.pool,
            &MaintenanceChange {
                owner: Some(&member),
                ..off("pg")
            },
            &as_user(&member),
            now(),
        )
        .await;
        let elsewhere = set_maintenance(
            &db.pool,
            &MaintenanceChange {
                owner: Some(&owner),
                ..off("elsewhere")
            },
            &as_user(&owner),
            now(),
        )
        .await;
        let unknown = set_maintenance(
            &db.pool,
            &MaintenanceChange {
                owner: Some(&owner),
                ..off("nowhere")
            },
            &as_user(&owner),
            now(),
        )
        .await;
        let by_owner = set_maintenance(
            &db.pool,
            &MaintenanceChange {
                owner: Some(&owner),
                ..off("pg")
            },
            &as_user(&owner),
            now(),
        )
        .await;

        for outcome in [by_member, elsewhere, unknown] {
            assert!(
                matches!(outcome, Err(OkfError::Forbidden { .. })),
                "{outcome:?}"
            );
        }
        assert_eq!(by_owner.unwrap(), Switch::On);
        assert!(!maintenance_of(&db.pool, "pg").await);
        assert!(maintenance_of(&db.pool, "elsewhere").await, "unchanged");
        assert_eq!(
            entries(&db.pool, "catalog.maintenance_changed").await.len(),
            1
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_owners_switch_and_their_demotion_at_once_both_finish() {
        use crate::store::teams::{self, Membership, Role};
        let db = test_db().await;
        seed(&db.pool, "pg").await;
        let owner = join_test_team(&db.pool, "olive", Role::Owner).await;
        // Hold the catalog row: the switch takes the owner's membership row,
        // then waits here.
        let mut holder = db.pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM catalogs WHERE name = 'pg' FOR UPDATE")
            .execute(&mut *holder)
            .await
            .unwrap();
        let pool = db.pool.clone();
        let owner_id = owner.clone();
        let switch = tokio::spawn(async move {
            let actor = AuditActor {
                user_id: Some(owner_id.clone()),
                ..AuditActor::cli()
            };
            let change = MaintenanceChange {
                catalog: "pg",
                to: Switch::Off,
                owner: Some(&owner_id),
            };
            set_maintenance(&pool, &change, &actor, now()).await
        });
        until_locks_are_awaited(&db.pool, 1).await;
        let pool = db.pool.clone();
        let demotion = tokio::spawn(async move {
            let membership = Membership {
                team: TEST_TEAM,
                user: "olive",
                role: Role::Member,
            };
            teams::set_member(&pool, &membership, &AuditActor::cli(), now()).await
        });
        until_locks_are_awaited(&db.pool, 2).await;

        holder.rollback().await.unwrap();
        let limit = std::time::Duration::from_secs(30);
        let switched = tokio::time::timeout(limit, switch)
            .await
            .expect("the switch finishes within 30 seconds")
            .unwrap();
        let demoted = tokio::time::timeout(limit, demotion)
            .await
            .expect("the demotion finishes within 30 seconds")
            .unwrap();

        // The demotion waited for the switch's hold on the membership.
        assert_eq!(switched.unwrap(), Switch::On);
        assert!(demoted.is_ok(), "{demoted:?}");
        assert!(!maintenance_of(&db.pool, "pg").await);
    }

    // ---- Display names (M17 Task 3) ----

    #[tokio::test(flavor = "multi_thread")]
    async fn update_details_changes_either_field_and_records_each() {
        // Arrange
        let db = test_db().await;
        seed(&db.pool, "pg").await;
        let display = DisplayName::parse("Work notes").unwrap();
        let cli = AuditActor::cli();
        let change = |display_name, description| DetailsChange {
            catalog: "pg",
            display_name,
            description,
            owner: None,
        };
        let start = admin_generation(&db.pool).await.unwrap();

        // Act: the display name alone, then both, then both again.
        let first = update_details(&db.pool, &change(Some(&display), None), &cli, now())
            .await
            .unwrap();
        let after_first = admin_generation(&db.pool).await.unwrap();
        let renamed = DisplayName::parse("Notes, naïve").unwrap();
        let both = change(Some(&renamed), Some("about"));
        let second = update_details(&db.pool, &both, &cli, now()).await.unwrap();
        let after_second = admin_generation(&db.pool).await.unwrap();
        let again = update_details(&db.pool, &both, &cli, now()).await.unwrap();

        // Assert: each answer is the state before it.
        assert_eq!(
            first,
            Details {
                display_name: String::new(),
                description: String::new(),
            }
        );
        assert_eq!(
            (second.display_name.as_str(), second.description.as_str()),
            ("Work notes", "")
        );
        assert_eq!(
            (again.display_name.as_str(), again.description.as_str()),
            ("Notes, naïve", "about")
        );
        let row = find(&db.pool, "pg").await.unwrap();
        assert_eq!(
            (
                row.display_name.as_str(),
                row.description.as_str(),
                row.generation
            ),
            ("Notes, naïve", "about", 0),
            "the content generation is left alone"
        );
        assert_eq!(after_first, start + 1);
        assert_eq!(after_second, after_first + 1, "one bump for two fields");
        assert_eq!(
            admin_generation(&db.pool).await.unwrap(),
            after_second,
            "the same values bump nothing"
        );
        let renames = entries(&db.pool, "catalog.display_name_changed").await;
        assert_eq!(renames.len(), 2, "{renames:?}");
        assert_eq!(renames[0].target.as_deref(), Some("pg"));
        assert_eq!(renames[0].teams, [crate::db::TEST_USER]);
        // Characters, not bytes; never the text.
        assert_eq!(renames[0].details, json!({"length": 10}));
        assert_eq!(renames[1].details, json!({"length": 12}));
        let described = entries(&db.pool, "catalog.description_changed").await;
        assert_eq!(described.len(), 1, "{described:?}");
        assert_eq!(described[0].details, json!({"length": 5}));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn update_details_as_owner_checks_the_owner_team() {
        use crate::store::teams::Role;
        // Arrange: `pg` belongs to the test team, `elsewhere` to `family`.
        let db = test_db().await;
        seed(&db.pool, "pg").await;
        family(&db.pool).await;
        seed(&db.pool, "elsewhere").await;
        set_owner(&db.pool, "elsewhere", "family", &AuditActor::cli(), now())
            .await
            .unwrap();
        let owner = join_test_team(&db.pool, "olive", Role::Owner).await;
        let member = join_test_team(&db.pool, "max", Role::Member).await;
        let as_user = |user_id: &str| AuditActor {
            user_id: Some(user_id.to_string()),
            ..AuditActor::cli()
        };
        let display = DisplayName::parse("Mine").unwrap();
        let change = |catalog, owner| DetailsChange {
            catalog,
            display_name: Some(&display),
            description: Some("ours"),
            owner: Some(owner),
        };

        // Act
        let by_member =
            update_details(&db.pool, &change("pg", &member), &as_user(&member), now()).await;
        let elsewhere = update_details(
            &db.pool,
            &change("elsewhere", &owner),
            &as_user(&owner),
            now(),
        )
        .await;
        let unknown =
            update_details(&db.pool, &change("nope", &owner), &as_user(&owner), now()).await;
        let malformed =
            update_details(&db.pool, &change("p\0g", &owner), &as_user(&owner), now()).await;
        let by_owner =
            update_details(&db.pool, &change("pg", &owner), &as_user(&owner), now()).await;

        // Assert: every refusal is the same `Forbidden`, so an unknown
        // catalog reveals nothing, and only the owner's change landed.
        for outcome in [by_member, elsewhere, unknown, malformed] {
            assert!(
                matches!(outcome, Err(OkfError::Forbidden { .. })),
                "{outcome:?}"
            );
        }
        assert_eq!(by_owner.unwrap().display_name, "");
        let pg = find(&db.pool, "pg").await.unwrap();
        assert_eq!(
            (pg.display_name.as_str(), pg.description.as_str()),
            ("Mine", "ours")
        );
        let other = find(&db.pool, "elsewhere").await.unwrap();
        assert_eq!(
            (other.display_name.as_str(), other.description.as_str()),
            ("", "")
        );
        assert_eq!(
            entries(&db.pool, "catalog.display_name_changed")
                .await
                .len(),
            1
        );
        assert_eq!(
            entries(&db.pool, "catalog.description_changed").await.len(),
            1
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_owners_rename_and_their_demotion_at_once_both_finish() {
        use crate::store::teams::{self, Membership, Role};
        // Arrange
        let db = test_db().await;
        seed(&db.pool, "pg").await;
        let owner = join_test_team(&db.pool, "olive", Role::Owner).await;
        let limit = std::time::Duration::from_secs(30);
        // Hold the catalog row: the rename takes the owner's membership row
        // (`lock_owner`), then waits here.
        let mut holder = db.pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM catalogs WHERE name = 'pg' FOR UPDATE")
            .execute(&mut *holder)
            .await
            .unwrap();
        let pool = db.pool.clone();
        let owner_id = owner.clone();
        let rename = tokio::spawn(async move {
            let actor = AuditActor {
                user_id: Some(owner_id.clone()),
                ..AuditActor::cli()
            };
            let display = DisplayName::parse("Mine").unwrap();
            let change = DetailsChange {
                catalog: "pg",
                display_name: Some(&display),
                description: None,
                owner: Some(&owner_id),
            };
            update_details(&pool, &change, &actor, now()).await
        });
        tokio::time::timeout(limit, until_locks_are_awaited(&db.pool, 1))
            .await
            .expect("the rename waits on the catalog row");
        let pool = db.pool.clone();
        let demotion = tokio::spawn(async move {
            let membership = Membership {
                team: TEST_TEAM,
                user: "olive",
                role: Role::Member,
            };
            teams::set_member(&pool, &membership, &AuditActor::cli(), now()).await
        });
        tokio::time::timeout(limit, until_locks_are_awaited(&db.pool, 2))
            .await
            .expect("the demotion waits on the rename's membership hold");

        // Act
        holder.rollback().await.unwrap();
        let renamed = tokio::time::timeout(limit, rename)
            .await
            .expect("the rename finishes within 30 seconds")
            .unwrap();
        let demoted = tokio::time::timeout(limit, demotion)
            .await
            .expect("the demotion finishes within 30 seconds")
            .unwrap();

        // Assert: the demotion waited for the rename's hold on the membership.
        assert!(renamed.is_ok(), "{renamed:?}");
        assert!(demoted.is_ok(), "{demoted:?}");
        assert_eq!(find(&db.pool, "pg").await.unwrap().display_name, "Mine");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_owner_demoted_while_their_rename_waits_is_refused() {
        use crate::store::teams::{self, Membership, Role};
        // Arrange
        let db = test_db().await;
        seed(&db.pool, "pg").await;
        let owner = join_test_team(&db.pool, "olive", Role::Owner).await;
        let limit = std::time::Duration::from_secs(30);
        // Hold `server_state`: olive's demotion takes her membership row,
        // then waits here at its generation bump.
        let mut holder = db.pool.begin().await.unwrap();
        sqlx::query("SELECT 1 FROM server_state FOR UPDATE")
            .execute(&mut *holder)
            .await
            .unwrap();
        let pool = db.pool.clone();
        let demotion = tokio::spawn(async move {
            let membership = Membership {
                team: TEST_TEAM,
                user: "olive",
                role: Role::Member,
            };
            teams::set_member(&pool, &membership, &AuditActor::cli(), now()).await
        });
        tokio::time::timeout(limit, until_locks_are_awaited(&db.pool, 1))
            .await
            .expect("the demotion waits on server_state");
        // Olive's rename then waits on the membership row (`lock_owner`).
        let pool = db.pool.clone();
        let rename = tokio::spawn(async move {
            let actor = AuditActor {
                user_id: Some(owner.clone()),
                ..AuditActor::cli()
            };
            let display = DisplayName::parse("Mine").unwrap();
            let change = DetailsChange {
                catalog: "pg",
                display_name: Some(&display),
                description: None,
                owner: Some(&owner),
            };
            update_details(&pool, &change, &actor, now()).await
        });
        tokio::time::timeout(limit, until_locks_are_awaited(&db.pool, 2))
            .await
            .expect("the rename waits on the membership row");

        // Act
        holder.rollback().await.unwrap();
        let demoted = tokio::time::timeout(limit, demotion)
            .await
            .expect("the demotion finishes within 30 seconds")
            .unwrap();
        let renamed = tokio::time::timeout(limit, rename)
            .await
            .expect("the rename finishes within 30 seconds")
            .unwrap();

        // Assert: the owner check read the membership after the demotion.
        assert!(demoted.is_ok(), "{demoted:?}");
        assert!(
            matches!(renamed, Err(OkfError::Forbidden { .. })),
            "{renamed:?}"
        );
        assert_eq!(find(&db.pool, "pg").await.unwrap().display_name, "");
        assert!(entries(&db.pool, "catalog.display_name_changed")
            .await
            .is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_owners_rename_of_a_catalog_moved_while_it_waits_is_refused() {
        use crate::store::teams::Role;
        // Arrange: olive owns the test team, which owns `pg`; `family` is
        // another team.
        let db = test_db().await;
        seed(&db.pool, "pg").await;
        family(&db.pool).await;
        let owner = join_test_team(&db.pool, "olive", Role::Owner).await;
        let limit = std::time::Duration::from_secs(30);
        // Hold the catalog row: the rename passes `lock_owner` (olive owns
        // the catalog's team now), then waits here.
        let mut holder = db.pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM catalogs WHERE name = 'pg' FOR UPDATE")
            .execute(&mut *holder)
            .await
            .unwrap();
        let pool = db.pool.clone();
        let rename = tokio::spawn(async move {
            let actor = AuditActor {
                user_id: Some(owner.clone()),
                ..AuditActor::cli()
            };
            let display = DisplayName::parse("Mine").unwrap();
            let change = DetailsChange {
                catalog: "pg",
                display_name: Some(&display),
                description: Some("ours"),
                owner: Some(&owner),
            };
            update_details(&pool, &change, &actor, now()).await
        });
        tokio::time::timeout(limit, until_locks_are_awaited(&db.pool, 1))
            .await
            .expect("the rename waits on the catalog row");
        // The holder moves the catalog to `family` while the rename waits.
        sqlx::query(
            "UPDATE catalogs SET owner_team_id = (SELECT id FROM teams WHERE name = 'family') \
             WHERE name = 'pg'",
        )
        .execute(&mut *holder)
        .await
        .unwrap();
        let before = admin_generation(&mut *holder).await.unwrap();

        // Act
        holder.commit().await.unwrap();
        let renamed = tokio::time::timeout(limit, rename)
            .await
            .expect("the rename finishes within 30 seconds")
            .unwrap();

        // Assert: under the row lock the owner team is no longer olive's,
        // so the old team's owner is refused and nothing is written.
        assert!(
            matches!(renamed, Err(OkfError::Forbidden { .. })),
            "{renamed:?}"
        );
        let pg = find(&db.pool, "pg").await.unwrap();
        assert_eq!(
            (pg.display_name.as_str(), pg.description.as_str()),
            ("", "")
        );
        assert_eq!(pg.owner_team.as_deref(), Some("family"));
        assert_eq!(admin_generation(&db.pool).await.unwrap(), before);
        for action in [
            "catalog.display_name_changed",
            "catalog.description_changed",
        ] {
            assert!(entries(&db.pool, action).await.is_empty(), "{action}");
        }
    }
}
