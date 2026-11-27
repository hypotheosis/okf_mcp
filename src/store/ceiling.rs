//! A user's ceiling (spec "The ceiling"): the most a self-issued credential
//! may reach. It is the catalogs owned by the user's teams, at each one's
//! `member_access`, merged with the user's entitlements (`user_catalogs`);
//! the higher level wins. Every write here that changes a ceiling sends its
//! user's or team's invalidation, so every replica re-caps at once.
//!
//! A ceiling is never loaded whole to check access (issue #728): a check
//! resolves it for the one catalog it acts on, from the catalog's directory
//! row (owner team, `member_access`) and the user's gates (teams,
//! entitlements), with [`level_on`]. A `member_access` or owner change sends
//! the catalog's `catalog_meta` message, which drops the directory row; a
//! membership or entitlement change sends the user's or team's, which drops
//! the user's gates.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde_json::json;
use sqlx::{PgConnection, PgExecutor, PgPool};

use crate::auth::{Principal, Reach};
use crate::error::OkfError;
use crate::invalidate::{invalidate, Invalidation};
use crate::store::audit::{self, Action, AuditActor, Entry, People, RowLock};
use crate::store::catalogs::{self, CatalogEvent, CatalogRow};
use crate::store::decode_column;
use crate::store::gates::UserGates;
use crate::store::keys::Access;
use crate::store::users;

/// Catalog name to the highest level a user's self-issued credentials may
/// have there: `Read` or `Write`, never `Admin`.
pub type Ceiling = BTreeMap<String, Access>;

/// What members of a catalog's owning team may reach through self-issued
/// credentials.
// `clap::ValueEnum` parses `--access none|read|write`; `serde::Deserialize`
// with `rename_all = "lowercase"` reads `"none"`/`"read"`/`"write"` from REST
// bodies. A variant named `None` is fine: `Option`'s `None` is a different
// item, and this one is always written `MemberAccess::None`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum MemberAccess {
    /// Team members get nothing through the team.
    None,
    /// Read only.
    Read,
    /// Read and write (the default).
    Write,
}

impl MemberAccess {
    /// The stored spelling.
    ///
    /// # Returns
    /// `"none"`, `"read"` or `"write"`.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: maps a value, no I/O"
        )
    )]
    pub fn as_str(self) -> &'static str {
        match self {
            MemberAccess::None => "none",
            MemberAccess::Read => "read",
            MemberAccess::Write => "write",
        }
    }

    /// Parses the stored spelling.
    ///
    /// # Arguments
    /// * `text` - a `catalogs.member_access` value.
    ///
    /// # Returns
    /// The value, or `None` for anything else.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: maps a value, no I/O"
        )
    )]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "none" => Some(MemberAccess::None),
            "read" => Some(MemberAccess::Read),
            "write" => Some(MemberAccess::Write),
            _ => None,
        }
    }

    /// The level it gives a member.
    ///
    /// # Returns
    /// `Read` or `Write`, or `None` for [`MemberAccess::None`].
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: maps a value, no I/O"
        )
    )]
    pub fn level(self) -> Option<Access> {
        match self {
            MemberAccess::None => None,
            MemberAccess::Read => Some(Access::Read),
            MemberAccess::Write => Some(Access::Write),
        }
    }
}

/// One entitlement: a catalog given to a user beyond their teams' catalogs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entitlement {
    /// The catalog's name.
    pub catalog: String,
    /// `Read` or `Write`.
    pub access: Access,
    /// When it was first given.
    pub created_at: DateTime<Utc>,
}

/// One catalog of one user's ceiling, as the query below returns it.
#[derive(sqlx::FromRow)]
struct CeilingRow {
    catalog: String,
    // `true` when any source gives write: the higher level wins.
    write: bool,
}

/// One user's ceiling (`$1`), one row per catalog. The union is the two
/// sources; `bool_or` merges them. Tests only (issue #736): it is the
/// reference the bounded reads ([`page`], [`ceiling_in`], [`level_on`]) are
/// checked against.
#[cfg(test)]
const CEILING_ROWS: &str = "\
    SELECT c.name AS catalog, bool_or(s.access = 'write') AS write \
    FROM (SELECT m.user_id, c.id AS catalog_id, c.member_access AS access \
          FROM team_members m JOIN catalogs c ON c.owner_team_id = m.team_id \
          WHERE c.member_access <> 'none' AND c.provisioned_at IS NOT NULL AND c.deleted_at IS NULL \
          UNION ALL \
          SELECT e.user_id, e.catalog_id, e.access FROM user_catalogs e) s \
    JOIN catalogs c ON c.id = s.catalog_id \
    WHERE s.user_id = $1 AND c.provisioned_at IS NOT NULL AND c.deleted_at IS NULL \
    GROUP BY c.name";

/// The ceiling rows of the user with this id.
///
/// # Arguments
/// * `executor` - a `&PgPool`, or `&mut *tx` to read inside a transaction.
/// * `user_id` - the user's id.
///
/// # Errors
/// [`OkfError::Database`].
// `impl PgExecutor<'_>` accepts either a pool or a connection, as
// `user_sessions::revoke_all` does.
#[cfg(test)]
#[tracing::instrument(level = "trace", skip_all, fields(db.system.name = "postgresql", db.operation.name = "ceiling.ceiling_rows"))]
async fn ceiling_rows(
    executor: impl PgExecutor<'_>,
    user_id: &str,
) -> Result<Vec<CeilingRow>, OkfError> {
    Ok(sqlx::query_as::<_, CeilingRow>(CEILING_ROWS)
        .bind(user_id)
        .fetch_all(executor)
        .await?)
}

/// The level a row stands for.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: maps a value, no I/O"
    )
)]
fn row_level(write: bool) -> Access {
    if write {
        Access::Write
    } else {
        Access::Read
    }
}

/// One user's rows as a ceiling.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: maps a value, no I/O"
    )
)]
fn one_ceiling(rows: Vec<CeilingRow>) -> Ceiling {
    rows.into_iter()
        .map(|row| (row.catalog, row_level(row.write)))
        .collect()
}

/// The refusal for a catalog name that names no catalog.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial conversion: builds an error value, no I/O"
    )
)]
fn unknown_catalog(catalog: &str) -> OkfError {
    OkfError::CatalogNotSelected {
        catalog: catalog.to_string(),
    }
}

/// Refuses, before any query, a user or catalog name that cannot name
/// one (not `[a-z0-9-]+`: a NUL, which Postgres refuses as a database
/// error, among others), as the unknown name it is.
///
/// # Errors
/// [`OkfError::UnknownName`] or [`OkfError::CatalogNotSelected`].
#[tracing::instrument(level = "trace", skip_all)]
fn check_names(user: Option<&str>, catalog: &str) -> Result<(), OkfError> {
    if let Some(user) = user.filter(|user| !catalogs::is_valid_name(user)) {
        return Err(users::unknown(user));
    }
    if !catalogs::is_valid_name(catalog) {
        return Err(unknown_catalog(catalog));
    }
    Ok(())
}

/// One user's whole ceiling: one entry per catalog the user's teams own (at
/// its `member_access`) or the user is entitled to. Tests only: no request
/// reads it (issues #728 and #736). Access checks resolve the ceiling per
/// catalog ([`level_on`], [`ceiling_in`]); the pages that show it read it a
/// page at a time ([`page`]). It stays as the reference those are checked
/// against.
///
/// # Arguments
/// * `pool` - the database.
/// * `user` - the user's name.
///
/// # Returns
/// The ceiling (empty when the user reaches nothing).
///
/// # Errors
/// [`OkfError::UnknownName`] for an unknown user; [`OkfError::Database`].
#[cfg(test)]
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "ceiling.ceiling_of"))]
pub async fn ceiling_of(pool: &PgPool, user: &str) -> Result<Ceiling, OkfError> {
    let user_id = users::id_by_name(pool, user).await?;
    Ok(one_ceiling(ceiling_rows(pool, &user_id).await?))
}

/// One user's ceiling on the named catalogs only (`$1` the user's id, `$2`
/// the names): the same two sources as [`CEILING_ROWS`], looked up per
/// catalog, so the cost is bounded by the names, never by how many
/// catalogs the user's teams own (issue #728).
const NAMED_CEILING_ROWS: &str = "\
    SELECT c.name AS catalog, bool_or(s.access = 'write') AS write \
    FROM catalogs c \
    CROSS JOIN LATERAL ( \
        SELECT c.member_access AS access FROM team_members m \
        WHERE m.team_id = c.owner_team_id AND m.user_id = $1 \
          AND c.member_access <> 'none' \
        UNION ALL \
        SELECT e.access FROM user_catalogs e \
        WHERE e.catalog_id = c.id AND e.user_id = $1) s \
    WHERE c.name = ANY($2) AND c.provisioned_at IS NOT NULL AND c.deleted_at IS NULL \
    GROUP BY c.name";

/// One user's ceiling on the named catalogs, by id, read on the caller's
/// connection: what `store::oauth::grants::create_self_issued` checks a
/// consent against while it holds the user's row, and what `POST /me/keys`
/// checks a new key against. A catalog left out of the answer is not in
/// the ceiling (or does not exist).
///
/// # Arguments
/// * `conn` - the connection (an open transaction's, or one from the pool).
/// * `user_id` - the user's id.
/// * `names` - the catalogs asked about; malformed names are left out.
///
/// # Returns
/// The ceiling restricted to `names` (empty for an unknown user or one who
/// reaches none of them).
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "ceiling.ceiling_in"))]
pub(crate) async fn ceiling_in(
    conn: &mut PgConnection,
    user_id: &str,
    names: &[String],
) -> Result<Ceiling, OkfError> {
    // Postgres refuses a NUL; a malformed name names no catalog anyway.
    let valid: Vec<&str> = names
        .iter()
        .map(String::as_str)
        .filter(|name| catalogs::is_valid_name(name))
        .collect();
    if valid.is_empty() {
        return Ok(Ceiling::new());
    }
    let rows = sqlx::query_as::<_, CeilingRow>(NAMED_CEILING_ROWS)
        .bind(user_id)
        .bind(valid)
        .fetch_all(conn)
        .await?;
    Ok(one_ceiling(rows))
}

/// Is any catalog of user `$1`'s ceiling at `write`? The team half seeks
/// the owner teams' writable catalogs through the partial index
/// `catalogs_member_write` (migration 0026), so a team's read-only catalogs
/// are never scanned, however many it owns.
const HIGHEST_SQL: &str = "\
    SELECT EXISTS (SELECT 1 FROM team_members m \
                   JOIN catalogs c ON c.owner_team_id = m.team_id \
                   WHERE m.user_id = $1 AND c.member_access = 'write' \
                     AND c.provisioned_at IS NOT NULL AND c.deleted_at IS NULL) \
        OR EXISTS (SELECT 1 FROM user_catalogs e \
                   JOIN catalogs c ON c.id = e.catalog_id \
                   WHERE e.user_id = $1 AND e.access = 'write' \
                     AND c.provisioned_at IS NOT NULL AND c.deleted_at IS NULL)";

/// Whether any catalog of the user's ceiling is at `write`: a user's
/// highest ceiling level, which `GET /whoami` shows as a console
/// session's `access` (`read` when the ceiling is empty, as before). One
/// row, and an index seek, whatever the ceiling's size (`HIGHEST_SQL`).
///
/// # Arguments
/// * `pool` - the database.
/// * `user_id` - the user's id.
///
/// # Returns
/// `Write` when some catalog of the ceiling is at `write`, else `Read`.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "ceiling.highest"))]
pub async fn highest(pool: &PgPool, user_id: &str) -> Result<Access, OkfError> {
    let write: bool = sqlx::query_scalar(HIGHEST_SQL)
        .bind(user_id)
        .fetch_one(pool)
        .await?;
    Ok(row_level(write))
}

/// The bound above every catalog name that starts with a prefix, in byte
/// order: catalog names are `[a-z0-9-]+`, all below `~` (0x7e).
const PREFIX_END: char = '~';

/// One page of user `$1`'s ceiling (issue #736), in byte order (`COLLATE
/// "C"`), the names starting with a prefix: `$2` the prefix, `$3` the
/// prefix followed by [`PREFIX_END`], `$4` the last name of the previous
/// page (`''` for the first), `$5` the most rows. Each owner team's half is
/// one range scan of `catalogs_member_reach_bytes` (owner team, name in
/// byte order; migration 0027) stopping after `$5` rows; the entitlements
/// are the user's own (as many as its gates hold), cut to `$5` too. So the
/// cost is bounded by the user's teams, entitlements and the page, never
/// by how many catalogs a team owns. The levels merge as in
/// [`CEILING_ROWS`]: the higher wins.
const PAGE_SQL: &str = "\
    SELECT s.catalog, bool_or(s.access = 'write') AS write \
    FROM (SELECT o.name AS catalog, o.member_access AS access \
          FROM team_members m \
          CROSS JOIN LATERAL ( \
              SELECT i.name, i.member_access FROM catalogs i \
              WHERE i.owner_team_id = m.team_id AND i.member_access <> 'none' \
                AND i.provisioned_at IS NOT NULL AND i.deleted_at IS NULL \
                AND i.name COLLATE \"C\" >= $2 AND i.name COLLATE \"C\" < $3 \
                AND i.name COLLATE \"C\" > $4 \
              ORDER BY i.name COLLATE \"C\" LIMIT $5) o \
          WHERE m.user_id = $1 \
          UNION ALL \
          (SELECT c.name, e.access FROM user_catalogs e \
           JOIN catalogs c ON c.id = e.catalog_id \
           WHERE e.user_id = $1 AND c.provisioned_at IS NOT NULL AND c.deleted_at IS NULL \
             AND c.name COLLATE \"C\" >= $2 AND c.name COLLATE \"C\" < $3 \
             AND c.name COLLATE \"C\" > $4 \
           ORDER BY c.name COLLATE \"C\" LIMIT $5)) s \
    GROUP BY s.catalog \
    ORDER BY s.catalog COLLATE \"C\" \
    LIMIT $5";

/// One page of a user's ceiling, as [`page`] reads it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CeilingPage {
    /// The page's catalogs and levels. A `BTreeMap` of `String` iterates
    /// in byte order, the order the page was read in.
    pub entries: Ceiling,
    /// The last name of this page when another page follows (send it back
    /// as `after`, encoded as the API's cursor); `None` on the last page.
    pub next: Option<String>,
}

/// Whether `prefix` could start a catalog name: empty, or `[a-z0-9-]+`.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: checks a value, no I/O"
    )
)]
pub fn is_valid_prefix(prefix: &str) -> bool {
    prefix.is_empty() || catalogs::is_valid_name(prefix)
}

/// One page of a user's ceiling (issue #736): the catalogs whose names
/// start with `prefix`, after `after`, in byte order, at most `limit`, each
/// at the level `ceiling_of` gives it. Walking the pages (each `next` as
/// the following `after`) visits exactly the whole ceiling's entries with
/// that prefix, once each, but no request reads more than a page per owner
/// team: it is what `GET /me/ceiling`, `GET /me` and the consent page read
/// instead of the whole ceiling.
///
/// # Arguments
/// * `executor` - a pool, connection or transaction.
/// * `user_id` - the user's id.
/// * `prefix` - the names' start; empty for every name. One that cannot
///   start a name ([`is_valid_prefix`]) matches nothing.
/// * `after` - the previous page's `next`; `None` for the first page. One
///   that is not a catalog name matches nothing.
/// * `limit` - the most entries; at least 1.
///
/// # Returns
/// The page and, when one follows, its last name.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "ceiling.page"))]
pub async fn page(
    executor: impl PgExecutor<'_>,
    user_id: &str,
    prefix: &str,
    after: Option<&str>,
    limit: u32,
) -> Result<CeilingPage, OkfError> {
    // Postgres refuses a NUL, and neither can match a name anyway.
    if !is_valid_prefix(prefix) || after.is_some_and(|after| !catalogs::is_valid_name(after)) {
        return Ok(CeilingPage::default());
    }
    let limit = limit.max(1);
    let end = format!("{prefix}{PREFIX_END}");
    // One row more than the page, to know whether another follows.
    let rows = sqlx::query_as::<_, CeilingRow>(PAGE_SQL)
        .bind(user_id)
        .bind(prefix)
        .bind(end)
        .bind(after.unwrap_or(""))
        .bind(i64::from(limit) + 1)
        .fetch_all(executor)
        .await?;
    let more = rows.len() > limit as usize;
    // `take` keeps the page; the extra row only said that one follows.
    let entries: Ceiling = one_ceiling(rows.into_iter().take(limit as usize).collect());
    let next = more.then(|| entries.keys().next_back().cloned()).flatten();
    Ok(CeilingPage { entries, next })
}

/// The highest level of a user's ceiling, or `None` when it reaches no
/// catalog: what `GET /me` shows (`ceiling_access`) instead of the whole
/// ceiling (issue #736). Two bounded reads: a one-entry [`page`] and
/// [`highest`].
///
/// # Arguments
/// * `pool` - the database.
/// * `user_id` - the user's id.
///
/// # Returns
/// `Write`, `Read`, or `None` for an empty ceiling.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all)]
pub async fn top_level(pool: &PgPool, user_id: &str) -> Result<Option<Access>, OkfError> {
    if page(pool, user_id, "", None, 1).await?.entries.is_empty() {
        return Ok(None);
    }
    Ok(Some(highest(pool, user_id).await?))
}

/// The level a user's ceiling has on one catalog (spec "The ceiling"),
/// resolved from that catalog's directory row and the user's gates instead
/// of a list of every catalog the user's teams own (issue #728): the
/// catalog's `member_access` when the user is in its owner team, merged
/// with the user's entitlement to it; the higher level wins.
///
/// It is what the whole-ceiling query (`CEILING_ROWS`) answers for this
/// catalog: the owner team and entitlements are matched by id (`teams.id`,
/// `catalogs.id`), as the query joins them.
///
/// # Arguments
/// * `row` - the catalog's directory row.
/// * `gates` - the user's teams and entitlements.
///
/// # Returns
/// `Read` or `Write`, or `None` when the ceiling does not reach the catalog.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: two lookups and a max, no I/O; it runs on every check of a session"
    )
)]
pub fn level_on(row: &CatalogRow, gates: &UserGates) -> Option<Access> {
    let through_team = row
        .owner_team_id
        .as_ref()
        .filter(|team| gates.teams.contains(*team))
        .and_then(|_| row.member_access.level());
    let entitled = gates.entitled.get(&row.id).copied();
    // `Option`'s `max` treats `None` as lowest: the higher level wins.
    through_team.max(entitled)
}

/// The ceiling restricted to `rows`: each row's [`level_on`], the rows the
/// ceiling does not reach left out. What a self-issued credential is capped
/// by ([`cap`] reads only its own catalogs), built from the rows of those
/// catalogs alone.
///
/// # Arguments
/// * `rows` - the directory rows of the catalogs asked about.
/// * `gates` - the user's teams and entitlements.
///
/// # Returns
/// Catalog name to level, for the rows the ceiling reaches.
#[tracing::instrument(level = "trace", skip_all)]
pub fn ceiling_over<'r>(
    rows: impl IntoIterator<Item = &'r CatalogRow>,
    gates: &UserGates,
) -> Ceiling {
    rows.into_iter()
        .filter_map(|row| Some((row.name.clone(), level_on(row, gates)?)))
        .collect()
}

/// Sets what members of a catalog's owning team may reach, and records
/// `catalog.member_access_changed` and bumps `admin_generation` when it
/// changes.
///
/// # Arguments
/// * `pool` - the database.
/// * `catalog` - the catalog's name.
/// * `access` - the new value.
/// * `actor` - who, for the audit log.
/// * `now` - the server clock.
///
/// # Returns
/// The value it had before.
///
/// # Errors
/// [`OkfError::CatalogNotSelected`] for an unknown (or malformed) catalog;
/// [`OkfError::Database`], a failed audit write included.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "ceiling.set_member_access"))]
pub async fn set_member_access(
    pool: &PgPool,
    catalog: &str,
    access: MemberAccess,
    actor: &AuditActor,
    now: DateTime<Utc>,
) -> Result<MemberAccess, OkfError> {
    let change = MemberAccessChange {
        catalog,
        access,
        owner: None,
    };
    change_member_access(pool, &change, actor, now).await
}

/// The refusal for a caller that is no owner of the catalog's shared team.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial conversion: builds an error value, no I/O"
    )
)]
fn not_team_owner() -> OkfError {
    OkfError::Forbidden {
        reason: "this needs an admin that reaches every catalog, or an owner of the catalog's \
                 team signed in to the console"
            .to_string(),
    }
}

/// A change of a catalog's member access: [`change_member_access`]'s input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemberAccessChange<'a> {
    /// The catalog's name.
    pub catalog: &'a str,
    /// The new value.
    pub access: MemberAccess,
    /// `Some(users.id)` when the caller may make the change only as an
    /// owner of the catalog's shared team (a console session that is not an
    /// every-catalog admin): checked under the locks, so an owner demoted
    /// or a catalog moved to another team meanwhile is refused. `None` when
    /// the caller is already allowed (an every-catalog admin, the CLI).
    pub owner: Option<&'a str>,
}

/// As [`set_member_access`], with the owner check of
/// [`MemberAccessChange::owner`] (issue #352): the owner's membership row
/// is locked in the transaction (`lock_shared_team_owner`), so a demotion
/// or an owner change meanwhile cannot race the write.
///
/// Lock order (`tools/agentic/lessons.md`): the users the entry resolves,
/// then the owner's membership row, then the catalog row, then
/// `server_state`.
///
/// # Arguments
/// * `pool` - the database.
/// * `change` - the catalog, the value, and the owner to check, if any.
/// * `actor` - who, for the audit log.
/// * `now` - the server clock.
///
/// # Returns
/// The value it had before.
///
/// # Errors
/// [`OkfError::CatalogNotSelected`] for an unknown or malformed catalog
/// without an owner check; [`OkfError::Forbidden`] when the owner check
/// fails (a personal team's owner, and an unknown catalog, included, so it
/// reveals nothing); [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "ceiling.change_member_access"))]
pub async fn change_member_access(
    pool: &PgPool,
    change: &MemberAccessChange<'_>,
    actor: &AuditActor,
    now: DateTime<Utc>,
) -> Result<MemberAccess, OkfError> {
    let MemberAccessChange {
        catalog,
        access,
        owner,
    } = *change;
    // The refusal for "no such catalog": an owner learns nothing from it.
    let not_found = || match owner {
        Some(_) => not_team_owner(),
        None => unknown_catalog(catalog),
    };
    if owner.is_none() {
        check_names(None, catalog)?;
    } else if !catalogs::is_valid_name(catalog) {
        return Err(not_found());
    }
    let mut tx = pool.begin().await?;
    audit::lock_people(
        &mut tx,
        actor,
        &People {
            catalogs: &[catalog],
            ..People::default()
        },
    )
    .await?;
    // `lock_shared_team_owner` holds the owner's membership row, so a
    // concurrent removal or demotion waits for this change.
    let owner_team = match owner {
        Some(user_id) => {
            Some(catalogs::lock_shared_team_owner(&mut tx, catalog, user_id, not_team_owner).await?)
        }
        None => None,
    };
    // As in `catalogs::set_owner`: the subquery reads the row as it was,
    // under its row lock, so this returns the value it replaced.
    let previous: Option<(i64, String, Option<String>)> = sqlx::query_as(
        "UPDATE catalogs c SET member_access = $2 \
         FROM (SELECT id, member_access FROM catalogs \
               WHERE name = $1 AND provisioned_at IS NOT NULL AND deleted_at IS NULL FOR UPDATE) old \
         WHERE c.id = old.id RETURNING c.id, old.member_access, c.owner_team_id",
    )
    .bind(catalog)
    .bind(access.as_str())
    .fetch_optional(&mut *tx)
    .await?;
    let (catalog_id, previous, owner_team_id) = previous.ok_or_else(not_found)?;
    // Under the row lock the owner team is fixed: a catalog moved to
    // another team after the membership was read is refused.
    if owner_team.is_some() && owner_team != owner_team_id {
        return Err(not_team_owner());
    }
    let previous = decode_column(MemberAccess::parse(&previous), "catalogs.member_access")?;
    if previous != access {
        catalogs::record_catalog(
            &mut tx,
            actor,
            CatalogEvent {
                action: Action::CatalogMemberAccessChanged,
                catalog_id,
                catalog,
                details: serde_json::json!({"from": previous.as_str(), "to": access.as_str()}),
            },
            now,
        )
        .await?;
        // The catalog, and its owner team's members' reach.
        let mut invalidations = vec![Invalidation::CatalogMeta {
            id: catalog_id,
            name: catalog.to_string(),
        }];
        invalidations.extend(owner_team_id.map(|id| Invalidation::Team { id }));
        invalidate(&mut tx, &invalidations).await?;
    }
    tx.commit().await?;
    Ok(previous)
}

/// A catalog's member access, for the console's catalog drawer.
///
/// # Arguments
/// * `pool` - the database.
/// * `catalog` - the catalog's name.
///
/// # Returns
/// Its `member_access`.
///
/// # Errors
/// [`OkfError::CatalogNotSelected`] for an unknown or malformed catalog
/// name; [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "ceiling.member_access"))]
pub async fn member_access(pool: &PgPool, catalog: &str) -> Result<MemberAccess, OkfError> {
    check_names(None, catalog)?;
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT member_access FROM catalogs \
             WHERE name = $1 AND provisioned_at IS NOT NULL AND deleted_at IS NULL",
    )
    .bind(catalog)
    .fetch_optional(pool)
    .await?;
    let stored = stored.ok_or_else(|| unknown_catalog(catalog))?;
    decode_column(MemberAccess::parse(&stored), "catalogs.member_access")
}

/// One member of a catalog's owning team, as [`team_access`] lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamMember {
    /// The user's name.
    pub user: String,
    /// `owner` or `member`.
    pub role: String,
}

/// One entitlement to a catalog, as [`team_access`] lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogEntitlement {
    /// The user's name.
    pub user: String,
    /// `Read` or `Write`.
    pub access: Access,
}

/// The most rows of each list one [`team_access`] page holds, and the
/// default (issue #352 review: a large team's drawer reads a page at a
/// time).
pub const TEAM_PAGE_MAX: i64 = 1000;

/// Which lists [`team_access`] reads: a continuation of one list skips the
/// other's query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeamLists {
    /// Both lists (the drawer's first read).
    Both,
    /// Only the members.
    Members,
    /// Only the entitlements.
    Entitlements,
}

/// Which page of each list [`team_access`] reads: the rows after a user name
/// (byte order; `""` for the first page), at most `limit` of each list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TeamPage<'a> {
    /// The last member of the previous page of members (`""`: the first).
    pub members_after: &'a str,
    /// The last user of the previous page of entitlements (`""`: the first).
    pub entitlements_after: &'a str,
    /// The most rows of each list; clamped to `1..=`[`TEAM_PAGE_MAX`].
    pub limit: i64,
    /// The lists to read; the other comes back empty with no cursor.
    pub lists: TeamLists,
}

/// What the catalog drawer shows of who reaches a catalog through its
/// owning team: one page of [`team_access`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamAccess {
    /// The owning team's members after the cursor, sorted by user name in
    /// byte order (empty for an unassigned catalog).
    pub members: Vec<TeamMember>,
    /// The last member of this page when more follow (send it back as
    /// `members_after`); `None` on the last page.
    pub members_next: Option<String>,
    /// The catalog's entitlements after the cursor, sorted like the members.
    /// For an owner, only those of the team's members: another tenant's
    /// names stay hidden.
    pub entitlements: Vec<CatalogEntitlement>,
    /// As `members_next`, for the entitlements.
    pub entitlements_next: Option<String>,
}

/// Locks the owner (when there is one) and the catalog row under a share
/// lock, and reads what both pages and the scalar need.
///
/// # Returns
/// The catalog's id, its member access and its owner team's id, and the
/// owner's team when `owner` is `Some`.
///
/// # Errors
/// As [`team_access`].
#[tracing::instrument(level = "trace", skip_all, fields(db.system.name = "postgresql", db.operation.name = "ceiling.read_catalog_locked"))]
async fn read_catalog_locked(
    conn: &mut PgConnection,
    catalog: &str,
    owner: Option<&str>,
) -> Result<(i64, MemberAccess, Option<String>, Option<String>), OkfError> {
    let not_found = || match owner {
        Some(_) => not_team_owner(),
        None => unknown_catalog(catalog),
    };
    if !catalogs::is_valid_name(catalog) {
        return Err(not_found());
    }
    let team = match owner {
        Some(user_id) => {
            Some(catalogs::lock_shared_team_owner(conn, catalog, user_id, not_team_owner).await?)
        }
        None => None,
    };
    let row: Option<(i64, String, Option<String>)> = sqlx::query_as(
        "SELECT id, member_access, owner_team_id FROM catalogs \
         WHERE name = $1 AND provisioned_at IS NOT NULL AND deleted_at IS NULL FOR SHARE",
    )
    .bind(catalog)
    .fetch_optional(&mut *conn)
    .await?;
    let (catalog_id, member_access, owner_team) = row.ok_or_else(not_found)?;
    // A catalog moved to another team after the membership was read.
    if team.is_some() && team != owner_team {
        return Err(not_team_owner());
    }
    let member_access = decode_column(
        MemberAccess::parse(&member_access),
        "catalogs.member_access",
    )?;
    Ok((catalog_id, member_access, owner_team, team))
}

/// A catalog's member access for an owner of its shared team, checked under
/// the owner's membership row lock: the scalar read, with no list (issue
/// #352).
///
/// # Arguments
/// * `pool` - the database.
/// * `catalog` - the catalog's name.
/// * `owner` - `users.id` of the caller.
///
/// # Errors
/// [`OkfError::Forbidden`] when the owner check fails (a personal team's
/// owner, and an unknown catalog, included); [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "ceiling.member_access_as_owner"))]
pub async fn member_access_as_owner(
    pool: &PgPool,
    catalog: &str,
    owner: &str,
) -> Result<MemberAccess, OkfError> {
    let mut tx = pool.begin().await?;
    let (_, access, _, _) = read_catalog_locked(&mut tx, catalog, Some(owner)).await?;
    tx.commit().await?;
    Ok(access)
}

/// Cuts `rows` (fetched one past `limit`) to a page, and names the cursor
/// of the page after it.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: maps a value, no I/O"
    )
)]
fn cut_page<T>(mut rows: Vec<T>, limit: i64, key: impl Fn(&T) -> &str) -> (Vec<T>, Option<String>) {
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    if rows.len() <= limit {
        return (rows, None);
    }
    rows.truncate(limit);
    let next = rows.last().map(|row| key(row).to_string());
    (rows, next)
}

/// One page of a catalog's owning team's members and of its entitlements,
/// in one transaction (issue #352). Each list is read with a keyset cursor
/// and a limit, so a large team costs one page, never the whole team.
///
/// # Arguments
/// * `pool` - the database.
/// * `catalog` - the catalog's name.
/// * `owner` - `Some(users.id)` when the caller may read it only as an owner
///   of the catalog's shared team (a console session that is not an admin):
///   checked under the owner's membership row lock. `None` for a caller
///   that is already allowed.
/// * `page` - the cursors and the page size.
///
/// # Returns
/// The page.
///
/// # Errors
/// [`OkfError::CatalogNotSelected`] for an unknown or malformed catalog
/// without an owner check; [`OkfError::Forbidden`] when the owner check
/// fails (a personal team's owner, and an unknown catalog, included);
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "ceiling.team_access"))]
pub async fn team_access(
    pool: &PgPool,
    catalog: &str,
    owner: Option<&str>,
    page: &TeamPage<'_>,
) -> Result<TeamAccess, OkfError> {
    let limit = page.limit.clamp(1, TEAM_PAGE_MAX);
    let mut tx = pool.begin().await?;
    let (catalog_id, _, owner_team, team) = read_catalog_locked(&mut tx, catalog, owner).await?;
    // One row past the page says whether another follows. `COLLATE "C"` is
    // byte order, which the cursor compares in.
    let members: Vec<(String, String)> = if page.lists == TeamLists::Entitlements {
        Vec::new()
    } else {
        sqlx::query_as(
            "SELECT u.name, m.role FROM team_members m JOIN users u ON u.id = m.user_id \
             WHERE m.team_id = $1 AND u.name COLLATE \"C\" > $2 \
             ORDER BY u.name COLLATE \"C\" LIMIT $3",
        )
        .bind(&owner_team)
        .bind(page.members_after)
        .bind(limit + 1)
        .fetch_all(&mut *tx)
        .await?
    };
    // `$2` is the owner's team, or NULL for an admin, who sees every user.
    let entitlements: Vec<(String, String)> = if page.lists == TeamLists::Members {
        Vec::new()
    } else {
        sqlx::query_as(
            "SELECT u.name, e.access FROM user_catalogs e JOIN users u ON u.id = e.user_id \
             WHERE e.catalog_id = $1 \
               AND ($2::text IS NULL \
                    OR e.user_id IN (SELECT user_id FROM team_members WHERE team_id = $2)) \
               AND u.name COLLATE \"C\" > $3 \
             ORDER BY u.name COLLATE \"C\" LIMIT $4",
        )
        .bind(catalog_id)
        .bind(&team)
        .bind(page.entitlements_after)
        .bind(limit + 1)
        .fetch_all(&mut *tx)
        .await?
    };
    tx.commit().await?;
    let members: Vec<TeamMember> = members
        .into_iter()
        .map(|(user, role)| TeamMember { user, role })
        .collect();
    let entitlements: Vec<CatalogEntitlement> = entitlements
        .into_iter()
        .map(|(user, access)| {
            Ok(CatalogEntitlement {
                user,
                access: decode_column(Access::parse(&access), "user_catalogs.access")?,
            })
        })
        .collect::<Result<_, OkfError>>()?;
    let (members, members_next) = cut_page(members, limit, |member| &member.user);
    let (entitlements, entitlements_next) = cut_page(entitlements, limit, |entry| &entry.user);
    Ok(TeamAccess {
        members,
        members_next,
        entitlements,
        entitlements_next,
    })
}

/// A user's entitlement to a catalog, by names: what [`unentitle_as_owner`]
/// removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntitlementRef<'a> {
    /// The user's name.
    pub user: &'a str,
    /// The catalog's name.
    pub catalog: &'a str,
}

/// An entitlement to give: what [`entitle`] is asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewEntitlement<'a> {
    /// The user's name.
    pub user: &'a str,
    /// The catalog's name.
    pub catalog: &'a str,
    /// `Read` or `Write`.
    pub access: Access,
}

/// Gives a user a catalog at `access`, or changes the level of one already
/// given; when anything changed, bumps `admin_generation` and records
/// `user.entitlement_set` (in the catalog's owner team's log). An
/// unchanged level writes and records nothing.
///
/// # Arguments
/// * `pool` - the database.
/// * `new` - the user, the catalog and the level.
/// * `actor` - who, for the audit log.
/// * `now` - when it is given (server clock); kept when only the level changes.
///
/// # Errors
/// [`OkfError::Refused`] for `Admin`; [`OkfError::UnknownName`] for an
/// unknown user; [`OkfError::CatalogNotSelected`] for an unknown catalog;
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "ceiling.entitle"))]
pub async fn entitle(
    pool: &PgPool,
    new: &NewEntitlement<'_>,
    actor: &AuditActor,
    now: DateTime<Utc>,
) -> Result<(), OkfError> {
    entitle_checked(pool, new, None, actor, now).await
}

/// As [`entitle`], for an owner of the catalog's shared team signed in to
/// the console (issue #352): the user must be a member of that team, and the
/// owner is checked, with the membership, under their row locks. A user who
/// is unknown, malformed or outside the team gets one refusal, and so does a
/// catalog that is unknown, owned by another team or by a personal team, so
/// nothing leaks across tenants.
///
/// # Arguments
/// * `pool` - the database.
/// * `owner` - `users.id` of the caller.
/// * `new` - the user, the catalog and the level.
/// * `actor` - who, for the audit log (the owner).
/// * `now` - when it is given (server clock).
///
/// # Errors
/// [`OkfError::Refused`] for `Admin`; [`OkfError::Forbidden`] for a caller
/// that is no owner of the catalog's shared team, or a user outside it;
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "ceiling.entitle_as_owner"))]
pub async fn entitle_as_owner(
    pool: &PgPool,
    owner: &str,
    new: &NewEntitlement<'_>,
    actor: &AuditActor,
    now: DateTime<Utc>,
) -> Result<(), OkfError> {
    entitle_checked(pool, new, Some(owner), actor, now).await
}

/// The write behind [`entitle`] and [`entitle_as_owner`] (`owner` is
/// `Some(users.id)` for the latter).
///
/// # Errors
/// As the two callers.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "ceiling.entitle_checked"))]
async fn entitle_checked(
    pool: &PgPool,
    new: &NewEntitlement<'_>,
    owner: Option<&str>,
    actor: &AuditActor,
    now: DateTime<Utc>,
) -> Result<(), OkfError> {
    // Destructuring copies the fields out (`NewEntitlement` is `Copy`).
    let NewEntitlement {
        user,
        catalog,
        access,
    } = *new;
    if access == Access::Admin {
        return Err(OkfError::Refused {
            reason: "an entitlement is read or write; admin comes only from an admin key"
                .to_string(),
        });
    }
    let (mut tx, user_id, catalog_id) =
        begin_entitlement(pool, &EntitlementRef { user, catalog }, owner, actor).await?;
    // The `WHERE` on the update makes an unchanged level write nothing, so
    // no row comes back. A row that does: `xmax = 0` is Postgres' mark of a
    // row this statement inserted (an updated row carries this transaction
    // as its `xmax`). Read in the same statement, it cannot race another
    // entitle the way a separate SELECT of the old level could.
    let inserted: Option<bool> = sqlx::query_scalar(
        "INSERT INTO user_catalogs (user_id, catalog_id, access, created_at) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (user_id, catalog_id) DO UPDATE SET access = EXCLUDED.access \
         WHERE user_catalogs.access <> EXCLUDED.access \
         RETURNING xmax = 0",
    )
    .bind(&user_id)
    .bind(catalog_id)
    .bind(access.as_str())
    .bind(now)
    .fetch_optional(&mut *tx)
    .await?;
    // `let ... else`: unchanged (no row) commits nothing and records nothing.
    let Some(inserted) = inserted else {
        return Ok(());
    };
    invalidate(&mut tx, &[Invalidation::user(&user_id)]).await?;
    // There are two levels, so a changed level was the other one.
    let previous = match (inserted, access) {
        (true, _) => None,
        (false, Access::Write) => Some(Access::Read.as_str()),
        (false, Access::Read | Access::Admin) => Some(Access::Write.as_str()),
    };
    let details = json!({
        "catalog": catalog,
        "access": access.as_str(),
        "previous": previous,
    });
    // The entry belongs to the catalog's owner team, as a catalog event does.
    let teams = audit::catalog_teams(&mut tx, catalog_id).await?;
    let entry = Entry {
        teams: &teams,
        ..users::user_entry(Action::UserEntitlementSet, &user_id, details)
    };
    audit::record(&mut tx, actor, &entry, now).await?;
    tx.commit().await?;
    Ok(())
}

/// Takes a catalog entitlement from a user (not given is a no-op, which
/// records nothing); when one was removed, bumps `admin_generation` and
/// records `user.entitlement_removed`.
///
/// # Arguments
/// * `pool` - the database.
/// * `user` - the user's name.
/// * `catalog` - the catalog's name.
/// * `actor` - who, for the audit log.
/// * `now` - the server clock.
///
/// # Errors
/// [`OkfError::UnknownName`] for an unknown user;
/// [`OkfError::CatalogNotSelected`] for an unknown catalog;
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "ceiling.unentitle"))]
pub async fn unentitle(
    pool: &PgPool,
    user: &str,
    catalog: &str,
    actor: &AuditActor,
    now: DateTime<Utc>,
) -> Result<(), OkfError> {
    unentitle_checked(pool, &EntitlementRef { user, catalog }, None, actor, now).await
}

/// As [`unentitle`], for an owner of the catalog's shared team signed in to
/// the console, with the checks and the refusals of [`entitle_as_owner`].
///
/// # Arguments
/// * `pool` - the database.
/// * `owner` - `users.id` of the caller.
/// * `target` - the user and the catalog.
/// * `actor` - who, for the audit log (the owner).
/// * `now` - the server clock.
///
/// # Errors
/// [`OkfError::Forbidden`] as for [`entitle_as_owner`];
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "ceiling.unentitle_as_owner"))]
pub async fn unentitle_as_owner(
    pool: &PgPool,
    owner: &str,
    target: &EntitlementRef<'_>,
    actor: &AuditActor,
    now: DateTime<Utc>,
) -> Result<(), OkfError> {
    unentitle_checked(pool, target, Some(owner), actor, now).await
}

/// The write behind [`unentitle`] and [`unentitle_as_owner`].
///
/// # Errors
/// As the two callers.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "ceiling.unentitle_checked"))]
async fn unentitle_checked(
    pool: &PgPool,
    target: &EntitlementRef<'_>,
    owner: Option<&str>,
    actor: &AuditActor,
    now: DateTime<Utc>,
) -> Result<(), OkfError> {
    let (mut tx, user_id, catalog_id) = begin_entitlement(pool, target, owner, actor).await?;
    let catalog = target.catalog;
    let removed = sqlx::query("DELETE FROM user_catalogs WHERE user_id = $1 AND catalog_id = $2")
        .bind(&user_id)
        .bind(catalog_id)
        .execute(&mut *tx)
        .await?;
    if removed.rows_affected() != 0 {
        invalidate(&mut tx, &[Invalidation::user(&user_id)]).await?;
        let details = json!({ "catalog": catalog });
        // The entry belongs to the catalog's owner team, as a catalog event does.
        let teams = audit::catalog_teams(&mut tx, catalog_id).await?;
        let entry = Entry {
            teams: &teams,
            ..users::user_entry(Action::UserEntitlementRemoved, &user_id, details)
        };
        audit::record(&mut tx, actor, &entry, now).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// The refusal for a user an owner may not entitle: unknown, malformed or
/// outside the owning team, all the same, with no name in it.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial conversion: builds an error value, no I/O"
    )
)]
fn not_team_member() -> OkfError {
    OkfError::Forbidden {
        reason: "an owner may entitle only members of the catalog's owning team".to_string(),
    }
}

/// Opens an entitlement change's transaction and takes its locks, in the
/// order of `tools/agentic/lessons.md`: the users, then (for an owner) the
/// owner's and the user's membership rows and the catalog row, then
/// whatever the change takes.
///
/// For an owner every refusal that would tell something about another
/// tenant is the same: an unknown or malformed user and a user outside the
/// owning team are [`not_team_member`], an unknown catalog or one the
/// caller does not own is [`not_team_owner`], and the owner check comes
/// first, so a non-owner learns nothing about the user.
///
/// # Arguments
/// * `pool` - the database.
/// * `target` - the user and the catalog.
/// * `owner` - `users.id` of the caller when it acts as an owner.
/// * `actor` - who acts.
///
/// # Returns
/// The transaction, the user's id and the catalog's id.
///
/// # Errors
/// [`OkfError::Forbidden`] for an owner's refusals; for an admin
/// [`OkfError::UnknownName`] and [`OkfError::CatalogNotSelected`];
/// [`OkfError::Database`].
#[tracing::instrument(level = "trace", skip_all, fields(db.system.name = "postgresql", db.operation.name = "ceiling.begin_entitlement"))]
async fn begin_entitlement(
    pool: &PgPool,
    target: &EntitlementRef<'_>,
    owner: Option<&str>,
    actor: &AuditActor,
) -> Result<(sqlx::Transaction<'static, sqlx::Postgres>, String, i64), OkfError> {
    let EntitlementRef { user, catalog } = *target;
    let Some(owner) = owner else {
        check_names(Some(user), catalog)?;
        let mut tx = pool.begin().await?;
        let user_id = lock_entitlement_users(&mut tx, user, catalog, actor).await?;
        let catalog_id = catalogs::find(&mut *tx, catalog).await?.id;
        return Ok((tx, user_id, catalog_id));
    };
    // A malformed name names nothing Postgres could take: refuse it as the
    // unknown it is.
    if !catalogs::is_valid_name(catalog) {
        return Err(not_team_owner());
    }
    let mut tx = pool.begin().await?;
    let people = People {
        target: catalogs::is_valid_name(user).then_some((user, RowLock::KeyShare)),
        catalogs: &[catalog],
        ..People::default()
    };
    // An unknown user is not refused here: the owner check comes first.
    let user_id = match audit::lock_people(&mut tx, actor, &people).await {
        Ok(found) => found.map(|(id, _)| id),
        Err(OkfError::UnknownName { .. }) => None,
        Err(error) => return Err(error),
    };
    let team = catalogs::lock_shared_team_owner(&mut tx, catalog, owner, not_team_owner).await?;
    let user_id = user_id.ok_or_else(not_team_member)?;
    // The user's membership row under a share lock, so a removal waits.
    let member: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM team_members WHERE team_id = $1 AND user_id = $2 FOR SHARE",
    )
    .bind(&team)
    .bind(&user_id)
    .fetch_optional(&mut *tx)
    .await?;
    member.ok_or_else(not_team_member)?;
    // The catalog row under a share lock: a move to another team waits for
    // this change, and one that happened since the membership was read is
    // refused.
    let row: Option<(i64, Option<String>)> = sqlx::query_as(
        "SELECT id, owner_team_id FROM catalogs \
             WHERE name = $1 AND provisioned_at IS NOT NULL AND deleted_at IS NULL FOR SHARE",
    )
    .bind(catalog)
    .fetch_optional(&mut *tx)
    .await?;
    let catalog_id = match row {
        Some((id, owner_team)) if owner_team.as_deref() == Some(team.as_str()) => id,
        _ => return Err(not_team_owner()),
    };
    Ok((tx, user_id, catalog_id))
}

/// Locks the user rows an entitlement change and its entry take: the user
/// (`FOR KEY SHARE` until commit, so a concurrent `users::delete` waits),
/// the actor's rows and the catalog's personal owner, if any (its entry's
/// team), in `audit::lock_people`' order, before `server_state`.
///
/// # Arguments
/// * `conn` - the change's transaction.
/// * `user` - the user's name.
/// * `catalog` - the catalog's name.
/// * `actor` - who acts.
///
/// # Returns
/// The user's id.
///
/// # Errors
/// [`OkfError::UnknownName`]; [`OkfError::Database`].
#[tracing::instrument(level = "trace", skip_all, fields(db.system.name = "postgresql", db.operation.name = "ceiling.lock_entitlement_users"))]
async fn lock_entitlement_users(
    conn: &mut PgConnection,
    user: &str,
    catalog: &str,
    actor: &AuditActor,
) -> Result<String, OkfError> {
    let people = People {
        target: Some((user, RowLock::KeyShare)),
        catalogs: &[catalog],
        ..People::default()
    };
    // `lock_people` answers `Some` whenever it is given a target.
    let (user_id, _) = audit::lock_people(conn, actor, &people)
        .await?
        .ok_or_else(|| users::unknown(user))?;
    Ok(user_id)
}

/// A user's entitlements, by catalog name.
///
/// # Arguments
/// * `pool` - the database.
/// * `user` - the user's name.
///
/// # Returns
/// Each entitlement, sorted by catalog.
///
/// # Errors
/// [`OkfError::UnknownName`] for an unknown user; [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "ceiling.entitlements"))]
pub async fn entitlements(pool: &PgPool, user: &str) -> Result<Vec<Entitlement>, OkfError> {
    let mut conn = pool.acquire().await?;
    entitlements_in(&mut conn, user).await
}

/// Reads entitlements on the caller's connection, so a guarded detail read
/// never borrows another pool slot while holding a user row lock.
///
/// # Errors
/// As [`entitlements`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "ceiling.entitlements_in"))]
pub(crate) async fn entitlements_in(
    conn: &mut PgConnection,
    user: &str,
) -> Result<Vec<Entitlement>, OkfError> {
    let user_id = users::id_by_name(&mut *conn, user).await?;
    let rows: Vec<(String, String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT c.name, e.access, e.created_at FROM user_catalogs e \
         JOIN catalogs c ON c.id = e.catalog_id \
         WHERE e.user_id = $1 AND c.provisioned_at IS NOT NULL AND c.deleted_at IS NULL ORDER BY c.name",
    )
    .bind(&user_id)
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter()
        .map(|(catalog, access, created_at)| {
            Ok(Entitlement {
                catalog,
                access: decode_column(Access::parse(&access), "user_catalogs.access")?,
                created_at,
            })
        })
        .collect()
}

/// May `principal` set `catalog`'s member access (or a user's
/// entitlement to it)? The one permission check the CLI, the REST routes
/// and the console use, so team owners can be allowed later here alone.
///
/// # Arguments
/// * `principal` - the caller.
/// * `catalog` - the catalog.
///
/// # Returns
/// `true` for a provisioner or an admin that reaches every catalog.
/// Entitlement routes separately require an every-catalog admin.
#[tracing::instrument(level = "debug", skip_all)]
pub fn may_set_member_access(principal: &Principal, catalog: &str) -> bool {
    principal.provisioner
        || (matches!(principal.reach, Reach::All)
            && principal.level(catalog) == Some(Access::Admin))
}

/// Caps a credential's levels by a ceiling.
///
/// # Arguments
/// * `grant` - the credential's catalogs and levels.
/// * `ceiling` - the user's ceiling.
///
/// # Returns
/// The catalogs in both, each at the lower of the two levels.
#[tracing::instrument(level = "debug", skip_all)]
pub fn cap(grant: &BTreeMap<String, Access>, ceiling: &Ceiling) -> BTreeMap<String, Access> {
    grant
        .iter()
        // `filter_map` keeps the `Some`s: a catalog the ceiling lacks drops out.
        .filter_map(|(catalog, level)| {
            ceiling
                .get(catalog)
                .map(|limit| (catalog.clone(), (*level).min(*limit)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{test_db, TEST_TEAM, TEST_USER};
    use crate::store::audit::AuditActor;
    use crate::store::teams::{self, NewTeam, Role};
    use crate::store::users::NewUser;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 12, 12, 0, 0).unwrap()
    }

    /// The map `{catalog: level}` of these pairs.
    fn ceiling(pairs: &[(&str, Access)]) -> Ceiling {
        pairs
            .iter()
            .map(|(catalog, level)| (catalog.to_string(), *level))
            .collect()
    }

    /// `users.id` of `name`.
    async fn id(pool: &PgPool, name: &str) -> String {
        sqlx::query_scalar("SELECT id FROM users WHERE name = $1")
            .bind(name)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// Users alice and bob; team `family` (alice owner, bob member) owns
    /// catalog `shared`; alice's personal team owns `solo`; the fixture
    /// user's team owns `other`.
    async fn family(pool: &PgPool) {
        for name in ["alice", "bob"] {
            users::create(
                pool,
                &NewUser {
                    name,
                    display_name: &crate::labels::DisplayName::default(),
                },
                &AuditActor::cli(),
                now(),
            )
            .await
            .unwrap();
        }
        let team = NewTeam {
            name: "family",
            display_name: &crate::labels::DisplayName::default(),
            owner: "alice",
        };
        teams::create(pool, &team, &AuditActor::cli(), now())
            .await
            .unwrap();
        teams::set_member(
            pool,
            &crate::store::teams::Membership {
                team: "family",
                user: "bob",
                role: Role::Member,
            },
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();
        for (catalog, owner) in [
            ("shared", "family"),
            ("solo", "alice"),
            ("other", TEST_TEAM),
        ] {
            catalogs::create(
                pool,
                &crate::store::catalogs::NewCatalog {
                    name: catalog,
                    display_name: &crate::labels::DisplayName::default(),
                    description: "",
                    owner_team: owner,
                },
                &AuditActor::cli(),
                now(),
            )
            .await
            .unwrap();
        }
    }

    #[test]
    fn cap_keeps_the_lower_level_and_only_catalogs_in_both() {
        let grant = ceiling(&[
            ("pg", Access::Write),
            ("work", Access::Read),
            ("mine", Access::Write),
        ]);
        let limit = ceiling(&[
            ("pg", Access::Read),
            ("work", Access::Write),
            ("docs", Access::Write),
        ]);

        let capped = cap(&grant, &limit);

        assert_eq!(
            capped,
            ceiling(&[("pg", Access::Read), ("work", Access::Read)])
        );
        assert!(cap(&grant, &Ceiling::new()).is_empty());
    }

    #[hegel::test]
    fn cap_never_raises_a_level_or_adds_a_catalog(tc: hegel::TestCase) {
        use hegel::generators;
        let names = ["a", "b", "c", "d"];
        let levels = [Access::Read, Access::Write];
        let draw = || -> Ceiling {
            let drawn: BTreeMap<&str, Access> = tc.draw(generators::btree_maps(
                generators::sampled_from(&names),
                generators::sampled_from(&levels),
            ));
            drawn
                .into_iter()
                .map(|(name, level)| (name.to_string(), level))
                .collect()
        };
        let grant = draw();
        let limit = draw();

        let capped = cap(&grant, &limit);

        for (catalog, level) in &capped {
            assert!(Some(level) <= grant.get(catalog), "never above the grant");
            assert!(Some(level) <= limit.get(catalog), "never above the ceiling");
        }
        for catalog in grant.keys().filter(|catalog| limit.contains_key(*catalog)) {
            assert!(capped.contains_key(catalog), "a catalog in both stays");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_ceiling_is_the_teams_catalogs_at_member_access_and_the_users_entitlements() {
        let db = test_db().await;
        family(&db.pool).await;

        let alice = ceiling_of(&db.pool, "alice").await.unwrap();
        let bob = ceiling_of(&db.pool, "bob").await.unwrap();

        assert_eq!(
            alice,
            ceiling(&[("shared", Access::Write), ("solo", Access::Write)])
        );
        assert_eq!(bob, ceiling(&[("shared", Access::Write)]));
        assert_eq!(by_id(&db.pool, &id(&db.pool, "alice").await).await, alice);
        assert_eq!(by_id(&db.pool, &id(&db.pool, "bob").await).await, bob);
        assert_eq!(
            by_id(&db.pool, &id(&db.pool, TEST_USER).await).await,
            ceiling(&[("other", Access::Write)])
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn member_access_none_leaves_a_catalog_out_and_the_higher_level_wins() {
        let db = test_db().await;
        family(&db.pool).await;

        set_member_access(
            &db.pool,
            "shared",
            MemberAccess::Read,
            &AuditActor::cli(),
            chrono::Utc::now(),
        )
        .await
        .unwrap();
        set_member_access(
            &db.pool,
            "solo",
            MemberAccess::None,
            &AuditActor::cli(),
            chrono::Utc::now(),
        )
        .await
        .unwrap();
        entitle(
            &db.pool,
            &NewEntitlement {
                user: "bob",
                catalog: "shared",
                access: Access::Write,
            },
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();
        entitle(
            &db.pool,
            &NewEntitlement {
                user: "bob",
                catalog: "other",
                access: Access::Read,
            },
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();
        entitle(
            &db.pool,
            &NewEntitlement {
                user: "alice",
                catalog: "shared",
                access: Access::Read,
            },
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();

        assert_eq!(
            ceiling_of(&db.pool, "alice").await.unwrap(),
            ceiling(&[("shared", Access::Read)])
        );
        assert_eq!(
            ceiling_of(&db.pool, "bob").await.unwrap(),
            ceiling(&[("other", Access::Read), ("shared", Access::Write)])
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_member_access_returns_the_previous_value_and_bumps_only_on_a_change() {
        let db = test_db().await;
        family(&db.pool).await;
        let before = catalogs::admin_generation(&db.pool).await.unwrap();

        let first = set_member_access(
            &db.pool,
            "shared",
            MemberAccess::Read,
            &AuditActor::cli(),
            Utc::now(),
        )
        .await
        .unwrap();
        let again = set_member_access(
            &db.pool,
            "shared",
            MemberAccess::Read,
            &AuditActor::cli(),
            Utc::now(),
        )
        .await
        .unwrap();
        let unknown = set_member_access(
            &db.pool,
            "nope",
            MemberAccess::Read,
            &AuditActor::cli(),
            Utc::now(),
        )
        .await;

        assert_eq!((first, again), (MemberAccess::Write, MemberAccess::Read));
        assert_eq!(
            catalogs::admin_generation(&db.pool).await.unwrap(),
            before + 1
        );
        assert!(
            matches!(unknown, Err(OkfError::CatalogNotSelected { .. })),
            "{unknown:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn entitlements_are_listed_replaced_and_removed_and_each_change_bumps() {
        let db = test_db().await;
        family(&db.pool).await;
        let generation = || catalogs::admin_generation(&db.pool);
        let before = generation().await.unwrap();

        entitle(
            &db.pool,
            &NewEntitlement {
                user: "bob",
                catalog: "other",
                access: Access::Read,
            },
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();
        entitle(
            &db.pool,
            &NewEntitlement {
                user: "bob",
                catalog: "other",
                access: Access::Read,
            },
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();
        let after_entitle = generation().await.unwrap();
        entitle(
            &db.pool,
            &NewEntitlement {
                user: "bob",
                catalog: "other",
                access: Access::Write,
            },
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();
        let listed = entitlements(&db.pool, "bob").await.unwrap();
        unentitle(&db.pool, "bob", "other", &AuditActor::cli(), now())
            .await
            .unwrap();
        let after_unentitle = generation().await.unwrap();
        unentitle(&db.pool, "bob", "other", &AuditActor::cli(), now())
            .await
            .unwrap();

        assert_eq!(
            after_entitle,
            before + 1,
            "the same entitlement twice is one change"
        );
        assert_eq!(
            listed,
            [Entitlement {
                catalog: "other".to_string(),
                access: Access::Write,
                created_at: now(),
            }]
        );
        assert_eq!(after_unentitle, before + 3);
        assert_eq!(
            generation().await.unwrap(),
            after_unentitle,
            "removing nothing is a no-op"
        );
        assert!(entitlements(&db.pool, "bob").await.unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_admin_entitlement_unknown_users_and_unknown_catalogs_are_refused() {
        let db = test_db().await;
        family(&db.pool).await;

        let admin = entitle(
            &db.pool,
            &NewEntitlement {
                user: "bob",
                catalog: "other",
                access: Access::Admin,
            },
            &AuditActor::cli(),
            now(),
        )
        .await;
        let nobody = entitle(
            &db.pool,
            &NewEntitlement {
                user: "carol",
                catalog: "other",
                access: Access::Read,
            },
            &AuditActor::cli(),
            now(),
        )
        .await;
        let nowhere = entitle(
            &db.pool,
            &NewEntitlement {
                user: "bob",
                catalog: "nope",
                access: Access::Read,
            },
            &AuditActor::cli(),
            now(),
        )
        .await;
        let no_ceiling = ceiling_of(&db.pool, "carol").await;
        let no_list = entitlements(&db.pool, "carol").await;
        let no_removal = unentitle(&db.pool, "bob", "nope", &AuditActor::cli(), now()).await;

        assert!(matches!(admin, Err(OkfError::Refused { .. })), "{admin:?}");
        for refused in [nobody, no_ceiling.map(|_| ()), no_list.map(|_| ())] {
            assert!(
                matches!(refused, Err(OkfError::UnknownName { noun: "user", .. })),
                "{refused:?}"
            );
        }
        for refused in [nowhere, no_removal] {
            assert!(
                matches!(refused, Err(OkfError::CatalogNotSelected { .. })),
                "{refused:?}"
            );
        }
    }

    /// Checks one step of a ceiling change: `admin_generation` moved past
    /// `last` (which becomes the new value) and the user is at `expected`.
    async fn moved(pool: &PgPool, step: &str, last: &mut i64, user_id: &str, expected: Ceiling) {
        let generation = catalogs::admin_generation(pool).await.unwrap();
        assert!(generation > *last, "{step}: the generation moved");
        *last = generation;
        let found = by_id(pool, user_id).await;
        assert_eq!(found, expected, "{step}");
    }

    /// One user's ceiling by id, as the user cache reads it.
    async fn by_id(pool: &PgPool, user_id: &str) -> Ceiling {
        one_ceiling(ceiling_rows(pool, user_id).await.unwrap())
    }

    /// Every write that can change a ceiling's inputs (team membership,
    /// catalog creation, ownership and deletion, member access,
    /// entitlements and user deletion) changes `ceilings()` and bumps
    /// `admin_generation` (its change messages re-cap the cached users).
    #[tokio::test(flavor = "multi_thread")]
    async fn every_write_that_moves_a_ceiling_bumps_the_generation() {
        let db = test_db().await;
        family(&db.pool).await;
        let pool = &db.pool;
        let bob = id(pool, "bob").await;
        let mut last = catalogs::admin_generation(pool).await.unwrap();
        let shared_write = ceiling(&[("shared", Access::Write)]);

        teams::remove_member(pool, "family", "bob", &AuditActor::cli(), Utc::now())
            .await
            .unwrap();
        moved(pool, "leaving the team", &mut last, &bob, Ceiling::new()).await;

        teams::set_member(
            pool,
            &crate::store::teams::Membership {
                team: "family",
                user: "bob",
                role: Role::Member,
            },
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();
        moved(
            pool,
            "joining the team",
            &mut last,
            &bob,
            shared_write.clone(),
        )
        .await;

        catalogs::create(
            pool,
            &crate::store::catalogs::NewCatalog {
                name: "extra",
                display_name: &crate::labels::DisplayName::default(),
                description: "",
                owner_team: "family",
            },
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();
        let both = ceiling(&[("extra", Access::Write), ("shared", Access::Write)]);
        moved(pool, "a new catalog of the team", &mut last, &bob, both).await;

        catalogs::set_owner(pool, "extra", TEST_TEAM, &AuditActor::cli(), Utc::now())
            .await
            .unwrap();
        moved(pool, "a new owner", &mut last, &bob, shared_write.clone()).await;

        entitle(
            pool,
            &NewEntitlement {
                user: "bob",
                catalog: "extra",
                access: Access::Read,
            },
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();
        let entitled = ceiling(&[("extra", Access::Read), ("shared", Access::Write)]);
        moved(pool, "an entitlement", &mut last, &bob, entitled).await;

        set_member_access(
            pool,
            "shared",
            MemberAccess::Read,
            &AuditActor::cli(),
            Utc::now(),
        )
        .await
        .unwrap();
        let lowered = ceiling(&[("extra", Access::Read), ("shared", Access::Read)]);
        moved(pool, "member access", &mut last, &bob, lowered).await;

        // The entitlement to `extra` cascades with the catalog.
        catalogs::delete_settled(pool, "extra", false, &AuditActor::cli(), Utc::now())
            .await
            .unwrap();
        let shared_read = ceiling(&[("shared", Access::Read)]);
        moved(
            pool,
            "deleting a catalog",
            &mut last,
            &bob,
            shared_read.clone(),
        )
        .await;

        entitle(
            pool,
            &NewEntitlement {
                user: "bob",
                catalog: "other",
                access: Access::Write,
            },
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();
        last = catalogs::admin_generation(pool).await.unwrap();
        unentitle(pool, "bob", "other", &AuditActor::cli(), now())
            .await
            .unwrap();
        moved(
            pool,
            "removing an entitlement",
            &mut last,
            &bob,
            shared_read,
        )
        .await;

        // A deleted user's entitlements go with it.
        users::create(
            pool,
            &NewUser {
                name: "carol",
                display_name: &crate::labels::DisplayName::default(),
            },
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();
        entitle(
            pool,
            &NewEntitlement {
                user: "carol",
                catalog: "other",
                access: Access::Read,
            },
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();
        let carol = id(pool, "carol").await;
        assert!(!by_id(pool, &carol).await.is_empty());
        last = catalogs::admin_generation(pool).await.unwrap();
        users::delete(
            pool,
            "carol",
            users::DeleteOptions::default(),
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();
        moved(pool, "deleting a user", &mut last, &carol, Ceiling::new()).await;
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM user_catalogs WHERE user_id = $1")
            .bind(&carol)
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(left, 0, "the entitlement cascaded");
    }

    #[test]
    fn member_access_round_trips_its_spelling() {
        for access in [MemberAccess::None, MemberAccess::Read, MemberAccess::Write] {
            assert_eq!(MemberAccess::parse(access.as_str()), Some(access));
        }
        assert_eq!(MemberAccess::parse("admin"), None);
        assert_eq!(MemberAccess::None.level(), None);
        assert_eq!(MemberAccess::Write.level(), Some(Access::Write));
    }

    #[test]
    fn only_an_admin_that_reaches_every_catalog_may_set_member_access() {
        let every = crate::mcp::testing::principal_as("root", Access::Admin, &[], None);
        let scoped = crate::mcp::testing::principal_as("scoped", Access::Admin, &["pg"], None);
        let writer = crate::mcp::testing::principal_as("writer", Access::Write, &[], None);

        assert!(may_set_member_access(&every, "pg"));
        assert!(!may_set_member_access(&scoped, "pg"));
        assert!(!may_set_member_access(&writer, "pg"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_member_access_records_a_change_and_none_for_no_change() {
        let db = test_db().await;
        family(&db.pool).await;
        let actor = AuditActor::cli();

        set_member_access(&db.pool, "shared", MemberAccess::Read, &actor, now())
            .await
            .unwrap();
        set_member_access(&db.pool, "shared", MemberAccess::Read, &actor, now())
            .await
            .unwrap();
        let bad_name =
            set_member_access(&db.pool, "sh\0ared", MemberAccess::None, &actor, now()).await;

        assert!(matches!(bad_name, Err(OkfError::CatalogNotSelected { .. })));
        let changed =
            crate::store::audit::testing::entries(&db.pool, "catalog.member_access_changed").await;
        assert_eq!(changed.len(), 1, "{changed:?}");
        assert_eq!(changed[0].target.as_deref(), Some("shared"));
        assert_eq!(changed[0].teams, ["family"]);
        assert_eq!(
            changed[0].details,
            serde_json::json!({"from": "write", "to": "read"})
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_audit_write_fails_the_member_access_change() {
        let db = test_db().await;
        family(&db.pool).await;
        crate::http::api::testing::fail_on(&db.pool, "BEFORE INSERT ON audit_entries FOR EACH ROW")
            .await;

        let refused = set_member_access(
            &db.pool,
            "shared",
            MemberAccess::None,
            &AuditActor::cli(),
            now(),
        )
        .await;

        assert!(refused.is_err());
        assert_eq!(
            member_access(&db.pool, "shared").await.unwrap(),
            MemberAccess::Write,
            "the change rolled back"
        );
    }

    /// Gives `user` catalog `catalog` at `access`, as the CLI.
    async fn give(pool: &PgPool, user: &str, catalog: &str, access: Access) {
        let new = NewEntitlement {
            user,
            catalog,
            access,
        };
        entitle(pool, &new, &AuditActor::cli(), now())
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn entitle_records_the_level_and_the_one_before_and_nothing_unchanged() {
        let db = test_db().await;
        family(&db.pool).await;

        give(&db.pool, "bob", "shared", Access::Read).await;
        give(&db.pool, "bob", "shared", Access::Read).await;
        give(&db.pool, "bob", "shared", Access::Write).await;
        give(&db.pool, "bob", "shared", Access::Read).await;

        let entries = crate::store::audit::testing::entries(&db.pool, "user.entitlement_set").await;
        assert_eq!(entries.len(), 3, "an unchanged level records nothing");
        assert!(entries
            .iter()
            .all(|entry| entry.person.as_deref() == Some("bob")));
        assert_eq!(entries[0].target_kind, "user");
        assert_eq!(entries[0].teams, ["family"], "the catalog's owner team");
        assert_eq!(
            entries[0].details,
            json!({"catalog": "shared", "access": "read", "previous": null})
        );
        assert_eq!(
            entries[1].details,
            json!({"catalog": "shared", "access": "write", "previous": "read"})
        );
        assert_eq!(
            entries[2].details,
            json!({"catalog": "shared", "access": "read", "previous": "write"})
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unentitle_records_a_removal_and_nothing_when_none_was_given() {
        let db = test_db().await;
        family(&db.pool).await;
        let cli = AuditActor::cli();

        unentitle(&db.pool, "bob", "other", &cli, now())
            .await
            .unwrap();
        give(&db.pool, "bob", "other", Access::Write).await;
        unentitle(&db.pool, "bob", "other", &cli, now())
            .await
            .unwrap();
        unentitle(&db.pool, "bob", "other", &cli, now())
            .await
            .unwrap();

        let entries =
            crate::store::audit::testing::entries(&db.pool, "user.entitlement_removed").await;
        assert_eq!(entries.len(), 1, "removing nothing records nothing");
        assert_eq!(entries[0].person.as_deref(), Some("bob"));
        assert_eq!(entries[0].teams, [TEST_TEAM], "the catalog's owner team");
        assert_eq!(entries[0].details, json!({"catalog": "other"}));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_audit_write_fails_the_entitlement_change() {
        let db = test_db().await;
        family(&db.pool).await;
        give(&db.pool, "bob", "other", Access::Read).await;
        crate::http::api::testing::fail_on(&db.pool, "BEFORE INSERT ON audit_entries FOR EACH ROW")
            .await;
        let cli = AuditActor::cli();
        let new = NewEntitlement {
            user: "bob",
            catalog: "shared",
            access: Access::Read,
        };

        let given = entitle(&db.pool, &new, &cli, now()).await;
        let taken = unentitle(&db.pool, "bob", "other", &cli, now()).await;

        assert!(given.is_err() && taken.is_err());
        let left: Vec<String> = entitlements(&db.pool, "bob")
            .await
            .unwrap()
            .into_iter()
            .map(|entitlement| entitlement.catalog)
            .collect();
        assert_eq!(left, ["other"], "both changes rolled back");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_admins_entitlement_change_and_a_concurrent_set_admin_both_finish() {
        use crate::store::users::tests::{admin_session_actor, queued_with_set_admin};
        let db = test_db().await;
        family(&db.pool).await;
        let staff = admin_session_actor(&db.pool, "staff").await;
        let pool = db.pool.clone();
        let actor = staff.clone();
        // `solo` belongs to alice's personal team: its entry locks alice too.
        let given = async move {
            let new = NewEntitlement {
                user: "bob",
                catalog: "solo",
                access: Access::Read,
            };
            entitle(&pool, &new, &actor, now()).await
        };
        let pool = db.pool.clone();
        let taken = async move { unentitle(&pool, "bob", "solo", &staff, now()).await };

        let (entitled, first) = queued_with_set_admin(&db.pool, "alice", given).await;
        let (unentitled, second) = queued_with_set_admin(&db.pool, "bob", taken).await;

        entitled.expect("the entitlement");
        first.expect("the first promotion");
        unentitled.expect("the removal");
        second.expect("the second promotion");
        let set = crate::store::audit::testing::entries(&db.pool, "user.entitlement_set").await;
        assert_eq!(
            set[0].teams,
            ["alice"],
            "the personal team reads as its person"
        );
    }

    /// Every row a plan's nodes read: their output rows (times their loops)
    /// and the rows their filters threw away.
    fn rows_examined(node: &serde_json::Value) -> f64 {
        let number = |key: &str| {
            node.get(key)
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0)
        };
        let own = number("Actual Rows") * number("Actual Loops").max(1.0)
            + number("Rows Removed by Filter")
            + number("Rows Removed by Index Recheck");
        let children: f64 = node
            .get("Plans")
            .and_then(serde_json::Value::as_array)
            .map_or(0.0, |plans| plans.iter().map(rows_examined).sum());
        own + children
    }

    /// Issue #728 (review of PR #739): `highest` for a user whose team owns
    /// 50,000 read-only catalogs seeks the writable ones through
    /// `catalogs_member_write`, so it examines a bounded number of rows,
    /// not one per catalog the team owns.
    #[tokio::test(flavor = "multi_thread")]
    async fn highest_examines_no_read_only_catalog() {
        let db = test_db().await;
        sqlx::query(
            "INSERT INTO catalogs (name, description, created_at, owner_team_id, member_access) \
             SELECT 'r' || lpad(n::text, 6, '0'), '', now(), \
                    (SELECT id FROM teams WHERE name = $1), 'read' \
             FROM generate_series(1, 50000) AS n",
        )
        .bind(TEST_TEAM)
        .execute(&db.pool)
        .await
        .unwrap();
        sqlx::query("ANALYZE catalogs")
            .execute(&db.pool)
            .await
            .unwrap();
        let user_id = id(&db.pool, TEST_USER).await;

        let level = highest(&db.pool, &user_id).await.unwrap();
        // `AssertSqlSafe`: the text is a constant of this module.
        let plan: serde_json::Value = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "EXPLAIN (ANALYZE, FORMAT JSON) {HIGHEST_SQL}"
        )))
        .bind(&user_id)
        .fetch_one(&db.pool)
        .await
        .unwrap();

        assert_eq!(level, Access::Read, "every catalog is read-only");
        let examined = rows_examined(&plan[0]["Plan"]);
        assert!(examined < 100.0, "examined {examined} rows: {plan:#}");
    }

    /// Issue #736: a page of a user's ceiling, first, continued and by
    /// prefix, over 50,000 catalogs of one team examines a bounded number
    /// of rows (`PAGE_SQL` is one range scan of
    /// `catalogs_member_reach_bytes` per owner team), and a prefix nothing
    /// starts with examines next to none, whatever the database's
    /// collation.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_ceiling_page_examines_a_page_not_the_ceiling() {
        let db = test_db().await;
        sqlx::query(
            "INSERT INTO catalogs (name, description, created_at, owner_team_id) \
             SELECT 'c' || lpad(n::text, 6, '0'), '', now(), \
                    (SELECT id FROM teams WHERE name = $1) \
             FROM generate_series(1, 50000) AS n",
        )
        .bind(TEST_TEAM)
        .execute(&db.pool)
        .await
        .unwrap();
        sqlx::query("ANALYZE catalogs")
            .execute(&db.pool)
            .await
            .unwrap();
        let user_id = id(&db.pool, TEST_USER).await;
        let examined = |prefix: &'static str, after: &'static str| {
            let pool = db.pool.clone();
            let user_id = user_id.clone();
            async move {
                let plan: serde_json::Value = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                    "EXPLAIN (ANALYZE, FORMAT JSON) {PAGE_SQL}"
                )))
                .bind(&user_id)
                .bind(prefix)
                .bind(format!("{prefix}{PREFIX_END}"))
                .bind(after)
                .bind(101_i64)
                .fetch_one(&pool)
                .await
                .unwrap();
                (rows_examined(&plan[0]["Plan"]), plan)
            }
        };

        let first = page(&db.pool, &user_id, "", None, 100).await.unwrap();
        let later = page(&db.pool, &user_id, "", Some("c025000"), 100)
            .await
            .unwrap();
        let found = page(&db.pool, &user_id, "c0499", None, 100).await.unwrap();

        assert_eq!(first.entries.len(), 100);
        assert_eq!(first.next.as_deref(), Some("c000100"));
        assert_eq!(
            later.entries.keys().next().map(String::as_str),
            Some("c025001")
        );
        assert_eq!(found.entries.len(), 100);
        assert!(found.entries.keys().all(|name| name.starts_with("c0499")));
        for (prefix, after) in [("", ""), ("", "c025000"), ("c0499", ""), ("zz", "")] {
            let (rows, plan) = examined(prefix, after).await;
            assert!(
                rows < 1_000.0,
                "{prefix:?} after {after:?}: {rows} rows: {plan:#}"
            );
        }
    }

    // ---- Team owners manage member access and entitlements (issue #352) ----

    /// An actor that is `user_id`, as a console session's is.
    fn owner_actor(user_id: &str) -> AuditActor {
        AuditActor {
            user_id: Some(user_id.to_string()),
            ..AuditActor::cli()
        }
    }

    /// An owner's write of `catalog`'s member access.
    async fn owner_sets(
        pool: &PgPool,
        owner: &str,
        catalog: &str,
        access: MemberAccess,
    ) -> Result<MemberAccess, OkfError> {
        let change = MemberAccessChange {
            catalog,
            access,
            owner: Some(owner),
        };
        change_member_access(pool, &change, &owner_actor(owner), now()).await
    }

    /// An owner's entitlement of `user` to `catalog` at `access`.
    async fn owner_entitles(
        pool: &PgPool,
        owner: &str,
        user: &str,
        catalog: &str,
        access: Access,
    ) -> Result<(), OkfError> {
        let new = NewEntitlement {
            user,
            catalog,
            access,
        };
        entitle_as_owner(pool, owner, &new, &owner_actor(owner), now()).await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_owner_sets_member_access_and_bumps_the_generation() {
        // Arrange
        let db = test_db().await;
        family(&db.pool).await;
        let alice = id(&db.pool, "alice").await;
        let before = catalogs::admin_generation(&db.pool).await.unwrap();

        // Act
        let previous = owner_sets(&db.pool, &alice, "shared", MemberAccess::Read).await;

        // Assert
        assert_eq!(previous.unwrap(), MemberAccess::Write);
        assert_eq!(
            member_access(&db.pool, "shared").await.unwrap(),
            MemberAccess::Read
        );
        assert!(catalogs::admin_generation(&db.pool).await.unwrap() > before);
        let bob = id(&db.pool, "bob").await;
        assert_eq!(
            by_id(&db.pool, &bob).await,
            ceiling(&[("shared", Access::Read)])
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_owner_cannot_act_on_a_catalog_that_is_not_provisioned_yet() {
        // Arrange: alice owns `shared`, bob is a member; the registry row
        // has not been provisioned (plan Task 10b, decision P11).
        let db = test_db().await;
        family(&db.pool).await;
        let (alice, bob) = (id(&db.pool, "alice").await, id(&db.pool, "bob").await);
        sqlx::query("UPDATE catalogs SET provisioned_at = NULL WHERE name = 'shared'")
            .execute(&db.pool)
            .await
            .unwrap();

        // Act
        let entitled = owner_entitles(&db.pool, &alice, "bob", "shared", Access::Read).await;
        let access = owner_sets(&db.pool, &alice, "shared", MemberAccess::None).await;

        // Assert: the owner checks refuse it as they refuse an unknown
        // catalog, and nothing was written.
        assert!(
            matches!(entitled, Err(OkfError::Forbidden { .. })),
            "{entitled:?}"
        );
        assert!(
            matches!(access, Err(OkfError::Forbidden { .. })),
            "{access:?}"
        );
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM user_catalogs WHERE user_id = $1")
            .bind(&bob)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(rows, 0);

        // Once provisioned, the same owner request succeeds.
        sqlx::query("UPDATE catalogs SET provisioned_at = now() WHERE name = 'shared'")
            .execute(&db.pool)
            .await
            .unwrap();
        owner_entitles(&db.pool, &alice, "bob", "shared", Access::Read)
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_owner_is_refused_member_access_where_it_does_not_own_a_shared_team() {
        // Arrange: bob is only a member of `family`; `solo` is alice's
        // personal catalog; `other` belongs to the fixture user's team.
        let db = test_db().await;
        family(&db.pool).await;
        let (alice, bob) = (id(&db.pool, "alice").await, id(&db.pool, "bob").await);
        let before = catalogs::admin_generation(&db.pool).await.unwrap();

        // Act
        let refused = [
            owner_sets(&db.pool, &bob, "shared", MemberAccess::None).await,
            owner_sets(&db.pool, &alice, "solo", MemberAccess::None).await,
            owner_sets(&db.pool, &alice, "other", MemberAccess::None).await,
            owner_sets(&db.pool, &alice, "nowhere", MemberAccess::None).await,
            owner_sets(&db.pool, &alice, "a\0b", MemberAccess::None).await,
        ];

        // Assert: every refusal is the same one, and nothing changed.
        for result in &refused {
            assert!(
                matches!(result, Err(OkfError::Forbidden { .. })),
                "{result:?}"
            );
        }
        let first = format!("{:?}", refused[0]);
        assert!(refused.iter().all(|result| format!("{result:?}") == first));
        assert_eq!(
            member_access(&db.pool, "shared").await.unwrap(),
            MemberAccess::Write
        );
        assert_eq!(
            member_access(&db.pool, "solo").await.unwrap(),
            MemberAccess::Write
        );
        assert_eq!(catalogs::admin_generation(&db.pool).await.unwrap(), before);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_owner_entitles_a_member_and_takes_it_away() {
        // Arrange
        let db = test_db().await;
        family(&db.pool).await;
        let alice = id(&db.pool, "alice").await;
        let before = catalogs::admin_generation(&db.pool).await.unwrap();

        // Act
        owner_entitles(&db.pool, &alice, "bob", "shared", Access::Read)
            .await
            .unwrap();
        let given = entitlements(&db.pool, "bob").await.unwrap();
        let target = EntitlementRef {
            user: "bob",
            catalog: "shared",
        };
        unentitle_as_owner(&db.pool, &alice, &target, &owner_actor(&alice), now())
            .await
            .unwrap();
        let after = entitlements(&db.pool, "bob").await.unwrap();

        // Assert
        assert_eq!(given.len(), 1);
        assert_eq!(
            (given[0].catalog.as_str(), given[0].access),
            ("shared", Access::Read)
        );
        assert!(after.is_empty());
        assert!(catalogs::admin_generation(&db.pool).await.unwrap() > before);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_owner_cannot_entitle_to_admin_and_the_refusal_comes_before_any_lookup() {
        let db = test_db().await;
        family(&db.pool).await;
        let alice = id(&db.pool, "alice").await;

        let refused = owner_entitles(&db.pool, &alice, "bob", "shared", Access::Admin).await;

        assert!(
            matches!(refused, Err(OkfError::Refused { .. })),
            "{refused:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unknown_user_and_one_outside_the_team_get_the_same_refusal() {
        // Arrange: the fixture user is in no team with alice's `family`.
        let db = test_db().await;
        family(&db.pool).await;
        let alice = id(&db.pool, "alice").await;
        let target = |user| EntitlementRef {
            user,
            catalog: "shared",
        };

        // Act
        let mut puts = Vec::new();
        let mut deletes = Vec::new();
        for user in ["nobody", TEST_USER, "a\0b", "BOB", ""] {
            puts.push(owner_entitles(&db.pool, &alice, user, "shared", Access::Read).await);
            deletes.push(
                unentitle_as_owner(&db.pool, &alice, &target(user), &owner_actor(&alice), now())
                    .await,
            );
        }

        // Assert
        for result in puts.iter().chain(&deletes) {
            assert!(
                matches!(result, Err(OkfError::Forbidden { .. })),
                "{result:?}"
            );
        }
        let first = format!("{:?}", puts[0]);
        assert!(puts
            .iter()
            .chain(&deletes)
            .all(|result| format!("{result:?}") == first));
        assert!(entitlements(&db.pool, TEST_USER).await.unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_owner_is_refused_entitlements_outside_its_shared_team() {
        // Arrange: bob (a member only), alice on her personal `solo`, on
        // `other` (another team's), on an unknown catalog.
        let db = test_db().await;
        family(&db.pool).await;
        let (alice, bob) = (id(&db.pool, "alice").await, id(&db.pool, "bob").await);

        // Act
        let refused = [
            owner_entitles(&db.pool, &bob, "bob", "shared", Access::Read).await,
            owner_entitles(&db.pool, &alice, "alice", "solo", Access::Read).await,
            owner_entitles(&db.pool, &alice, "bob", "other", Access::Read).await,
            owner_entitles(&db.pool, &alice, "bob", "nowhere", Access::Read).await,
            owner_entitles(&db.pool, &alice, "bob", "a\0b", Access::Read).await,
        ];

        // Assert
        for result in &refused {
            assert!(
                matches!(result, Err(OkfError::Forbidden { .. })),
                "{result:?}"
            );
        }
        let first = format!("{:?}", refused[0]);
        assert!(refused.iter().all(|result| format!("{result:?}") == first));
        assert!(entitlements(&db.pool, "bob").await.unwrap().is_empty());
        assert!(entitlements(&db.pool, "alice").await.unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_admin_entitlement_paths_still_answer_unknown_names() {
        let db = test_db().await;
        family(&db.pool).await;
        let new = |user, catalog| NewEntitlement {
            user,
            catalog,
            access: Access::Read,
        };

        let user = entitle(
            &db.pool,
            &new("nobody", "shared"),
            &AuditActor::cli(),
            now(),
        )
        .await;
        let catalog = entitle(&db.pool, &new("bob", "nowhere"), &AuditActor::cli(), now()).await;
        let outside = entitle(
            &db.pool,
            &new(TEST_USER, "shared"),
            &AuditActor::cli(),
            now(),
        )
        .await;

        assert!(
            matches!(user, Err(OkfError::UnknownName { .. })),
            "{user:?}"
        );
        assert!(
            matches!(catalog, Err(OkfError::CatalogNotSelected { .. })),
            "{catalog:?}"
        );
        assert!(outside.is_ok(), "an admin may entitle any user");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn team_access_shows_an_owner_only_its_own_members_and_their_entitlements() {
        // Arrange: bob and the fixture user are entitled to `shared`.
        let db = test_db().await;
        family(&db.pool).await;
        let alice = id(&db.pool, "alice").await;
        for user in ["bob", TEST_USER] {
            let new = NewEntitlement {
                user,
                catalog: "shared",
                access: Access::Read,
            };
            entitle(&db.pool, &new, &AuditActor::cli(), now())
                .await
                .unwrap();
        }

        // Act
        let first = TeamPage {
            members_after: "",
            entitlements_after: "",
            limit: 100,
            lists: TeamLists::Both,
        };
        let owner = team_access(&db.pool, "shared", Some(&alice), &first)
            .await
            .unwrap();
        let admin = team_access(&db.pool, "shared", None, &first).await.unwrap();
        let personal = team_access(&db.pool, "solo", Some(&alice), &first).await;
        let stranger = team_access(&db.pool, "nowhere", Some(&alice), &first).await;
        let scalar = member_access_as_owner(&db.pool, "shared", &alice).await;
        let scalar_personal = member_access_as_owner(&db.pool, "solo", &alice).await;

        // Assert
        let users = |view: &TeamAccess| {
            view.entitlements
                .iter()
                .map(|e| e.user.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(scalar.unwrap(), MemberAccess::Write);
        assert!(
            matches!(scalar_personal, Err(OkfError::Forbidden { .. })),
            "{scalar_personal:?}"
        );
        assert_eq!(
            owner
                .members
                .iter()
                .map(|m| m.user.as_str())
                .collect::<Vec<_>>(),
            ["alice", "bob"]
        );
        assert_eq!(users(&owner), ["bob"], "the outsider's name stays hidden");
        assert_eq!(users(&admin), ["bob", TEST_USER]);
        assert!(
            matches!(personal, Err(OkfError::Forbidden { .. })),
            "{personal:?}"
        );
        assert!(
            matches!(stranger, Err(OkfError::Forbidden { .. })),
            "{stranger:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_members_and_entitlements_come_a_page_at_a_time_with_a_cursor() {
        // Arrange: family has alice and bob; add five more members, all
        // entitled to `shared`.
        let db = test_db().await;
        family(&db.pool).await;
        let alice = id(&db.pool, "alice").await;
        for name in ["c1", "c2", "c3", "c4", "c5"] {
            users::create(
                &db.pool,
                &NewUser {
                    name,
                    display_name: &crate::labels::DisplayName::default(),
                },
                &AuditActor::cli(),
                now(),
            )
            .await
            .unwrap();
            let membership = crate::store::teams::Membership {
                team: "family",
                user: name,
                role: Role::Member,
            };
            teams::set_member(&db.pool, &membership, &AuditActor::cli(), now())
                .await
                .unwrap();
            let new = NewEntitlement {
                user: name,
                catalog: "shared",
                access: Access::Read,
            };
            entitle(&db.pool, &new, &AuditActor::cli(), now())
                .await
                .unwrap();
        }

        // Act: walk both lists, three rows at a time, through the cursors.
        let mut members = Vec::new();
        let mut entitled = Vec::new();
        let (mut after_member, mut after_entitled) = (String::new(), String::new());
        let mut pages = 0;
        loop {
            let page = TeamPage {
                members_after: &after_member,
                entitlements_after: &after_entitled,
                limit: 3,
                lists: TeamLists::Both,
            };
            let got = team_access(&db.pool, "shared", Some(&alice), &page)
                .await
                .unwrap();
            assert!(got.members.len() <= 3 && got.entitlements.len() <= 3);
            members.extend(got.members.iter().map(|m| m.user.clone()));
            entitled.extend(got.entitlements.iter().map(|e| e.user.clone()));
            pages += 1;
            if got.members_next.is_none() && got.entitlements_next.is_none() {
                break;
            }
            after_member = got.members_next.unwrap_or_else(|| "~".to_string());
            after_entitled = got.entitlements_next.unwrap_or_else(|| "~".to_string());
            assert!(pages < 10, "the cursors never end");
        }

        // Assert: seven members in three pages, each row once, in order.
        assert_eq!(members, ["alice", "bob", "c1", "c2", "c3", "c4", "c5"]);
        assert_eq!(entitled, ["c1", "c2", "c3", "c4", "c5"]);
        assert_eq!(pages, 3);
    }

    /// Demotes `user` of `family` to a plain member, as an admin does.
    async fn demote(pool: &PgPool, user: &str) -> Result<(), OkfError> {
        let membership = crate::store::teams::Membership {
            team: "family",
            user,
            role: Role::Member,
        };
        teams::set_member(pool, &membership, &AuditActor::cli(), now())
            .await
            .map(|_| ())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_owner_demoted_while_its_write_waits_is_refused() {
        // Arrange: hold `server_state`, so alice's demotion takes her
        // membership row and then waits there.
        let db = test_db().await;
        family(&db.pool).await;
        let alice = id(&db.pool, "alice").await;
        // A second owner, so demoting alice is no last-owner refusal.
        let promotion = crate::store::teams::Membership {
            team: "family",
            user: "bob",
            role: Role::Owner,
        };
        teams::set_member(&db.pool, &promotion, &AuditActor::cli(), now())
            .await
            .unwrap();
        let limit = std::time::Duration::from_secs(30);
        let mut holder = db.pool.begin().await.unwrap();
        sqlx::query("SELECT 1 FROM server_state FOR UPDATE")
            .execute(&mut *holder)
            .await
            .unwrap();
        let pool = db.pool.clone();
        let demotion = tokio::spawn(async move { demote(&pool, "alice").await });
        tokio::time::timeout(
            limit,
            crate::http::api::testing::wait_for_lock_waiters(&db.pool, 1),
        )
        .await
        .expect("the demotion waits on server_state");
        // Alice's two writes then wait on her membership row.
        let (pool, owner) = (db.pool.clone(), alice.clone());
        let access =
            tokio::spawn(
                async move { owner_sets(&pool, &owner, "shared", MemberAccess::None).await },
            );
        let (pool, owner) = (db.pool.clone(), alice.clone());
        let entitle = tokio::spawn(async move {
            owner_entitles(&pool, &owner, "bob", "shared", Access::Read).await
        });
        tokio::time::timeout(
            limit,
            crate::http::api::testing::wait_for_lock_waiters(&db.pool, 3),
        )
        .await
        .expect("both writes wait on the membership row");

        // Act
        holder.rollback().await.unwrap();
        let demoted = tokio::time::timeout(limit, demotion)
            .await
            .unwrap()
            .unwrap();
        let access = tokio::time::timeout(limit, access).await.unwrap().unwrap();
        let entitle = tokio::time::timeout(limit, entitle).await.unwrap().unwrap();

        // Assert: both checked the membership after the demotion.
        assert!(demoted.is_ok(), "{demoted:?}");
        assert!(
            matches!(access, Err(OkfError::Forbidden { .. })),
            "{access:?}"
        );
        assert!(
            matches!(entitle, Err(OkfError::Forbidden { .. })),
            "{entitle:?}"
        );
        assert_eq!(
            member_access(&db.pool, "shared").await.unwrap(),
            MemberAccess::Write
        );
        assert!(entitlements(&db.pool, "bob").await.unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_entitlement_to_a_member_removed_while_it_waits_is_refused() {
        // Arrange: hold `server_state`, so bob's removal takes his
        // membership row and then waits there.
        let db = test_db().await;
        family(&db.pool).await;
        let alice = id(&db.pool, "alice").await;
        let limit = std::time::Duration::from_secs(30);
        let mut holder = db.pool.begin().await.unwrap();
        sqlx::query("SELECT 1 FROM server_state FOR UPDATE")
            .execute(&mut *holder)
            .await
            .unwrap();
        let pool = db.pool.clone();
        let removal = tokio::spawn(async move {
            teams::remove_member(&pool, "family", "bob", &AuditActor::cli(), now()).await
        });
        tokio::time::timeout(
            limit,
            crate::http::api::testing::wait_for_lock_waiters(&db.pool, 1),
        )
        .await
        .expect("the removal waits on server_state");
        let (pool, owner) = (db.pool.clone(), alice.clone());
        let entitle = tokio::spawn(async move {
            owner_entitles(&pool, &owner, "bob", "shared", Access::Read).await
        });
        tokio::time::timeout(
            limit,
            crate::http::api::testing::wait_for_lock_waiters(&db.pool, 2),
        )
        .await
        .expect("the entitlement waits on bob's membership row");

        // Act
        holder.rollback().await.unwrap();
        let removed = tokio::time::timeout(limit, removal).await.unwrap().unwrap();
        let entitled = tokio::time::timeout(limit, entitle).await.unwrap().unwrap();

        // Assert
        assert!(removed.is_ok(), "{removed:?}");
        assert!(
            matches!(entitled, Err(OkfError::Forbidden { .. })),
            "{entitled:?}"
        );
        assert!(entitlements(&db.pool, "bob").await.unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_entitlement_on_a_catalog_moved_to_another_team_while_it_waits_is_refused() {
        // Arrange: hold `server_state`, so the move of `shared` to another
        // team updates the catalog row and then waits, uncommitted.
        let db = test_db().await;
        family(&db.pool).await;
        let alice = id(&db.pool, "alice").await;
        let limit = std::time::Duration::from_secs(30);
        let mut holder = db.pool.begin().await.unwrap();
        sqlx::query("SELECT 1 FROM server_state FOR UPDATE")
            .execute(&mut *holder)
            .await
            .unwrap();
        let pool = db.pool.clone();
        let mover = tokio::spawn(async move {
            catalogs::set_owner(&pool, "shared", TEST_TEAM, &AuditActor::cli(), now()).await
        });
        tokio::time::timeout(
            limit,
            crate::http::api::testing::wait_for_lock_waiters(&db.pool, 1),
        )
        .await
        .expect("the move waits on server_state");
        // The move holds the catalog row's lock by now: a second lock
        // attempt on the row fails at once.
        let mut probe = db.pool.begin().await.unwrap();
        let locked = sqlx::query("SELECT 1 FROM catalogs WHERE name = 'shared' FOR UPDATE NOWAIT")
            .execute(&mut *probe)
            .await;
        assert!(locked.is_err(), "the move has not locked the catalog row");
        probe.rollback().await.unwrap();
        // Alice still owns the catalog's team as far as the uncommitted
        // move shows, so her entitlement passes the owner and membership
        // checks and then waits on the catalog row.
        let (pool, owner) = (db.pool.clone(), alice.clone());
        let entitle = tokio::spawn(async move {
            owner_entitles(&pool, &owner, "bob", "shared", Access::Read).await
        });
        tokio::time::timeout(
            limit,
            crate::http::api::testing::wait_for_lock_waiters(&db.pool, 2),
        )
        .await
        .expect("the entitlement waits on the catalog row");

        // Act
        holder.rollback().await.unwrap();
        let moved = tokio::time::timeout(limit, mover).await.unwrap().unwrap();
        let entitled = tokio::time::timeout(limit, entitle).await.unwrap().unwrap();

        // Assert: the entitlement re-checked the owner team after the move,
        // and wrote nothing.
        assert_eq!(moved.unwrap().as_deref(), Some("family"));
        assert!(
            matches!(entitled, Err(OkfError::Forbidden { .. })),
            "{entitled:?}"
        );
        assert!(entitlements(&db.pool, "bob").await.unwrap().is_empty());
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM user_catalogs")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(rows, 0);
    }

    /// Issue #642: a stored member access or entitlement level that is not a
    /// known spelling is a database error from the readers, and a write that
    /// reads it rolls back, not a panic.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_corrupt_member_access_or_entitlement_is_a_database_error() {
        let db = test_db().await;
        family(&db.pool).await;
        let alice = id(&db.pool, "alice").await;
        let new = NewEntitlement {
            user: "bob",
            catalog: "shared",
            access: Access::Read,
        };
        entitle(&db.pool, &new, &AuditActor::cli(), now())
            .await
            .unwrap();
        crate::store::drop_check_constraints(&db.pool, "catalogs").await;
        crate::store::drop_check_constraints(&db.pool, "user_catalogs").await;
        sqlx::query("UPDATE catalogs SET member_access = 'bogus-member'")
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE user_catalogs SET access = 'bogus-level'")
            .execute(&db.pool)
            .await
            .unwrap();
        let page = TeamPage {
            members_after: "",
            entitlements_after: "",
            limit: 100,
            lists: TeamLists::Both,
        };
        let change = MemberAccessChange {
            catalog: "shared",
            access: MemberAccess::Read,
            owner: None,
        };

        let scalar = member_access(&db.pool, "shared").await;
        let as_owner = member_access_as_owner(&db.pool, "shared", &alice).await;
        let changed = change_member_access(&db.pool, &change, &AuditActor::cli(), now()).await;
        // The refused change was rolled back: the value it replaced stays.
        let stored: String =
            sqlx::query_scalar("SELECT member_access FROM catalogs WHERE name = 'shared'")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        let entitled = entitlements(&db.pool, "bob").await;
        sqlx::query("UPDATE catalogs SET member_access = 'write'")
            .execute(&db.pool)
            .await
            .unwrap();
        let team = team_access(&db.pool, "shared", None, &page).await;

        crate::store::assert_decode_error(scalar, "catalogs.member_access", "bogus-member");
        crate::store::assert_decode_error(as_owner, "catalogs.member_access", "bogus-member");
        crate::store::assert_decode_error(changed, "catalogs.member_access", "bogus-member");
        crate::store::assert_decode_error(entitled, "user_catalogs.access", "bogus-level");
        crate::store::assert_decode_error(team, "user_catalogs.access", "bogus-level");
        assert_eq!(stored, "bogus-member");
    }
}
