//! Teams: the owners of catalogs (spec "Data model"). Every user has a
//! personal team, made with it; shared teams have owners and members.
//! Teams are addressed by name; ids stay inside SQL. Every membership
//! write locks the team row `FOR UPDATE`, so changes to one team's
//! membership run one at a time and the last-owner check cannot race; a
//! display-name change takes `FOR NO KEY UPDATE`, which still waits for them.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool};

use crate::error::OkfError;
use crate::invalidate::{invalidate, Invalidation};
use crate::labels::DisplayName;
use crate::store::audit::{self, Action, AuditActor, Entry, People, RowLock, Target};
use crate::store::catalogs::is_valid_name;
use crate::store::decode_column;
use crate::store::stores;
use crate::store::users;
use crate::stores::StoreName;

/// A member's role in a team.
// `clap::ValueEnum` parses `--role owner|member`; `serde::Deserialize` with
// `rename_all = "lowercase"` reads `"owner"`/`"member"` from REST bodies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// May be the team's administrative contact; every team keeps one.
    Owner,
    /// A plain member.
    Member,
}

impl Role {
    /// The stored spelling.
    ///
    /// # Returns
    /// `"owner"` or `"member"`.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: maps a value, no I/O"
        )
    )]
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Owner => "owner",
            Role::Member => "member",
        }
    }

    /// Parses the stored spelling.
    ///
    /// # Arguments
    /// * `text` - a `team_members.role` value.
    ///
    /// # Returns
    /// The role, or `None` for anything other than [`Role::as_str`]'s spellings.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: maps a value, no I/O"
        )
    )]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "owner" => Some(Role::Owner),
            "member" => Some(Role::Member),
            _ => None,
        }
    }
}

/// A team to create.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewTeam<'a> {
    /// `[a-z0-9-]+`, not taken by a user or team.
    pub name: &'a str,
    /// Its display name (empty when unset), validated by
    /// [`DisplayName::parse`].
    pub display_name: &'a DisplayName,
    /// The user who becomes its first owner.
    pub owner: &'a str,
}

/// A membership to set: [`set_member`]'s input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Membership<'a> {
    /// The team's name.
    pub team: &'a str,
    /// The user's name.
    pub user: &'a str,
    /// The role it should have.
    pub role: Role,
}

/// One membership of a team.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// The user's name.
    pub user: String,
    /// Its role.
    pub role: Role,
}

/// A stored team.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamRecord {
    /// Its name.
    pub name: String,
    /// Its display name (may be empty).
    pub display_name: String,
    /// The user whose personal team this is.
    pub personal_of: Option<String>,
    /// When it was created (server clock).
    pub created_at: DateTime<Utc>,
    /// Sorted by user name.
    pub members: Vec<Member>,
    /// Names of the catalogs it owns, sorted.
    pub catalogs: Vec<String>,
    /// Its suspension; `None` while it is active.
    pub suspension: Option<Suspension>,
}

/// What [`set_member`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberChange {
    /// The user was not a member and now is.
    Added,
    /// The user was a member with the other role.
    RoleChanged,
    /// The user already had that role; nothing was written.
    Unchanged,
}

/// The longest suspension notice, in characters (the schema checks it too).
pub const MAX_NOTICE_CHARS: usize = 2000;

/// A team's suspension: since when, and what members are told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suspension {
    /// When it was first suspended (server clock); a second suspend keeps it.
    pub since: DateTime<Utc>,
    /// The notice members see, if the operator gave one.
    pub notice: Option<String>,
}

/// One catalog whose owner team is suspended, as the key cache holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuspendedCatalog {
    /// The owner team's name.
    pub team: String,
    /// Its notice, if any.
    pub notice: Option<String>,
    /// The catalog's label.
    pub catalog_label: String,
    /// The owner team's label.
    pub team_label: String,
}

/// A team row without its members (they come from a second query).
#[derive(sqlx::FromRow)]
struct TeamRow {
    name: String,
    display_name: String,
    personal_of: Option<String>,
    created_at: DateTime<Utc>,
    suspended_at: Option<DateTime<Utc>>,
    suspension_notice: Option<String>,
    catalogs: Vec<String>,
}

/// A membership row, with its team's name for grouping.
#[derive(sqlx::FromRow)]
struct MemberRow {
    team: String,
    user: String,
    role: String,
}

// The row and member SELECTs shared by `list` and `find` (macros so
// `concat!` can append each query's tail at compile time).
macro_rules! select_teams {
    () => {
        "SELECT t.name, t.display_name, p.name AS personal_of, t.created_at, \
                t.suspended_at, t.suspension_notice, \
                COALESCE((SELECT array_agg(c.name ORDER BY c.name) FROM catalogs c \
                          WHERE c.owner_team_id = t.id \
                            AND c.provisioned_at IS NOT NULL AND c.deleted_at IS NULL), '{}'::text[]) AS catalogs \
         FROM teams t LEFT JOIN users p ON p.id = t.personal_of"
    };
}
macro_rules! select_members {
    () => {
        "SELECT t.name AS team, u.name AS user, m.role \
         FROM team_members m \
         JOIN teams t ON t.id = m.team_id \
         JOIN users u ON u.id = m.user_id"
    };
}

/// Joins team rows with their membership rows (two queries, so there is no
/// query per team).
///
/// # Errors
/// [`OkfError::Database`] for a stored role that is not a known spelling.
#[tracing::instrument(level = "trace", skip_all)]
fn assemble(rows: Vec<TeamRow>, member_rows: Vec<MemberRow>) -> Result<Vec<TeamRecord>, OkfError> {
    // The member rows arrive sorted by user, so each team's list stays sorted.
    let mut members: HashMap<String, Vec<Member>> = HashMap::with_capacity(rows.len());
    for row in member_rows {
        members.entry(row.team).or_default().push(Member {
            user: row.user,
            role: decode_column(Role::parse(&row.role), "team_members.role")?,
        });
    }
    Ok(rows
        .into_iter()
        .map(|row| TeamRecord {
            members: members.remove(&row.name).unwrap_or_default(),
            // `map` runs only for `Some`: a team with a time is suspended.
            // Struct-literal fields are evaluated in the order written, so
            // this moves `suspension_notice` out of `row` before the
            // fields below move the rest.
            suspension: row.suspended_at.map(|since| Suspension {
                since,
                notice: row.suspension_notice,
            }),
            name: row.name,
            display_name: row.display_name,
            personal_of: row.personal_of,
            created_at: row.created_at,
            catalogs: row.catalogs,
        })
        .collect())
}

/// Creates a shared team with `owner` as its first owner, records
/// `team.created`, and bumps the administrative generation, in one
/// transaction.
///
/// # Arguments
/// * `pool` - the database.
/// * `new` - its name, display name and owner.
/// * `actor` - who, for the audit log; a provisioner is refused an admin owner.
/// * `now` - the creation time (server clock).
///
/// # Returns
/// The new team.
///
/// # Errors
/// [`OkfError::Refused`] for an invalid name or one a user or team already
/// has (including one taken by a concurrent create);
/// [`OkfError::UnknownName`] for an unknown (or malformed) owner;
/// [`OkfError::Forbidden`] for a provisioner and an admin owner;
/// [`OkfError::Database`], a failed audit write included.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "teams.create"))]
pub async fn create(
    pool: &PgPool,
    new: &NewTeam<'_>,
    actor: &AuditActor,
    now: DateTime<Utc>,
) -> Result<TeamRecord, OkfError> {
    users::check_name("team", new.name)?;
    let mut tx = pool.begin().await?;
    // The owner, with the actor, before anything else. `FOR KEY SHARE` on the
    // owner conflicts with `users::set_admin`'s `FOR UPDATE`, so the guard
    // below cannot race a promotion.
    let owner = People {
        target: Some((new.owner, RowLock::KeyShare)),
        ..People::default()
    };
    let (owner_id, owner_is_admin) = lock_user(&mut tx, actor, &owner, new.owner).await?;
    users::check_free(&mut tx, new.name).await?;
    audit::refuse_admin_target(actor, owner_is_admin, new.owner)?;
    let inserted = sqlx::query_scalar::<_, String>(
        "INSERT INTO teams (name, display_name, created_at) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(new.name)
    .bind(new.display_name.as_str())
    .bind(now)
    .fetch_one(&mut *tx)
    .await;
    let team_id = users::or_name_taken(inserted, new.name)?;
    insert_member(&mut tx, &team_id, &owner_id, Role::Owner, now).await?;
    record_team(
        &mut tx,
        actor,
        TeamEvent {
            action: Action::TeamCreated,
            team: new.name,
            person: Some(&owner_id),
            details: serde_json::json!({}),
        },
        now,
    )
    .await?;
    invalidate(&mut tx, &[Invalidation::team(&team_id)]).await?;
    tx.commit().await?;
    Ok(TeamRecord {
        name: new.name.to_string(),
        display_name: new.display_name.as_str().to_string(),
        personal_of: None,
        created_at: now,
        members: vec![Member {
            user: new.owner.to_string(),
            role: Role::Owner,
        }],
        catalogs: Vec::new(),
        suspension: None,
    })
}

/// Every team, by name, with its members and catalogs.
///
/// # Arguments
/// * `pool` - the database.
///
/// # Returns
/// The teams, sorted by name.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "teams.list"))]
pub async fn list(pool: &PgPool) -> Result<Vec<TeamRecord>, OkfError> {
    let rows = sqlx::query_as::<_, TeamRow>(concat!(select_teams!(), " ORDER BY t.name"))
        .fetch_all(pool)
        .await?;
    let member_rows =
        sqlx::query_as::<_, MemberRow>(concat!(select_members!(), " ORDER BY u.name"))
            .fetch_all(pool)
            .await?;
    assemble(rows, member_rows)
}

/// The team called `name`, with its members and catalogs.
///
/// # Arguments
/// * `pool` - the database.
/// * `name` - the team's name.
///
/// # Returns
/// The team.
///
/// # Errors
/// [`OkfError::UnknownName`] if there is none (a malformed name included);
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "teams.find"))]
pub async fn find(pool: &PgPool, name: &str) -> Result<TeamRecord, OkfError> {
    // A malformed name (a NUL, which Postgres refuses) names no team: the
    // same 404 as an unknown one, never a database error (issue #339).
    if !is_valid_name(name) {
        return Err(unknown(name));
    }
    let row = sqlx::query_as::<_, TeamRow>(concat!(select_teams!(), " WHERE t.name = $1"))
        .bind(name)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| unknown(name))?;
    let member_rows = sqlx::query_as::<_, MemberRow>(concat!(
        select_members!(),
        " WHERE t.name = $1 ORDER BY u.name"
    ))
    .bind(name)
    .fetch_all(pool)
    .await?;
    // `assemble` returns exactly one record for one row.
    Ok(assemble(vec![row], member_rows)?
        .pop()
        .expect("one team row gives one record"))
}

/// The team called `team`, if `user_id` is one of its owners: the scope of
/// that owner's audit view.
///
/// # Arguments
/// * `pool` - the database.
/// * `user_id` - the owner's `users.id`.
/// * `team` - the team's name.
///
/// # Returns
/// Its `teams.id` and its `audit_teams` id, or `None` when the user owns no
/// team by that name (a malformed name included).
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "teams.owned_by"))]
pub async fn owned_by(
    pool: &PgPool,
    user_id: &str,
    team: &str,
) -> Result<Option<(String, i64)>, OkfError> {
    // A malformed name (a NUL from `?team=`) owns nothing, never a database
    // error, as for every team lookup (#339).
    if !is_valid_name(team) {
        return Ok(None);
    }
    // A plain read of the `audit_teams` row, which every team gets in the
    // transaction that creates it (migration 0014), so a read never writes.
    Ok(sqlx::query_as(
        "SELECT t.id, a.id FROM teams t \
         JOIN team_members m ON m.team_id = t.id \
         JOIN audit_teams a ON a.team_id = t.id \
         WHERE t.name = $1 AND m.user_id = $2 AND m.role = 'owner'",
    )
    .bind(team)
    .bind(user_id)
    .fetch_optional(pool)
    .await?)
}

/// A team a user owns, as the account page lists it.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct OwnedTeam {
    /// The team's name.
    pub name: String,
    /// Its display name (may be empty).
    pub display_name: String,
    /// Whether it is a personal team (its label follows its user).
    pub personal: bool,
}

/// The teams `user_id` owns, its personal team included: the teams whose
/// audit log its console session can read, and the ones it may rename.
///
/// # Arguments
/// * `pool` - the database.
/// * `user_id` - the user's `users.id`.
///
/// # Returns
/// The teams, sorted by name.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "teams.owned"))]
pub async fn owned(pool: &PgPool, user_id: &str) -> Result<Vec<OwnedTeam>, OkfError> {
    Ok(sqlx::query_as(
        "SELECT t.name, t.display_name, t.personal_of IS NOT NULL AS personal \
         FROM teams t JOIN team_members m ON m.team_id = t.id \
         WHERE m.user_id = $1 AND m.role = 'owner' ORDER BY t.name",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?)
}

/// Adds a user to a team with a role, or changes its role, records
/// `team.member_added` or `team.role_changed`, and bumps the administrative
/// generation, when anything changed.
///
/// # Arguments
/// * `pool` - the database.
/// * `membership` - the team, the user and the role it should have.
/// * `actor` - who, for the audit log; a provisioner is refused an admin user.
/// * `now` - the membership's creation time, when added (server clock).
///
/// # Returns
/// Whether the user was added, its role changed, or nothing changed.
///
/// # Errors
/// [`OkfError::UnknownName`] for the team or user (a malformed name
/// included); [`OkfError::Forbidden`] for a provisioner and an admin user;
/// [`OkfError::Refused`] for demoting the team's last owner;
/// [`OkfError::Database`], a failed audit write included.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "teams.set_member"))]
pub async fn set_member(
    pool: &PgPool,
    membership: &Membership<'_>,
    actor: &AuditActor,
    now: DateTime<Utc>,
) -> Result<MemberChange, OkfError> {
    let Membership { team, user, role } = *membership;
    let mut tx = pool.begin().await?;
    // Users before the team: `users::delete` locks the user and then its
    // teams, so taking them in the other order could deadlock.
    let people = People {
        target: Some((user, RowLock::KeyShare)),
        teams: &[team],
        ..People::default()
    };
    let (user_id, is_admin) = lock_user(&mut tx, actor, &people, user).await?;
    audit::refuse_admin_target(actor, is_admin, user)?;
    let locked = lock_team(&mut tx, team).await?;
    let (change, action, from) = match current_role(&mut tx, &locked.id, &user_id).await? {
        None => {
            insert_member(&mut tx, &locked.id, &user_id, role, now).await?;
            (MemberChange::Added, Action::TeamMemberAdded, None)
        }
        // Dropping `tx` here rolls back the (read-only) transaction.
        Some(existing) if existing == role => return Ok(MemberChange::Unchanged),
        Some(existing) => {
            if existing == Role::Owner {
                refuse_last_owner(&mut tx, &locked.id, team, user).await?;
            }
            sqlx::query("UPDATE team_members SET role = $3 WHERE team_id = $1 AND user_id = $2")
                .bind(&locked.id)
                .bind(&user_id)
                .bind(role.as_str())
                .execute(&mut *tx)
                .await?;
            (
                MemberChange::RoleChanged,
                Action::TeamRoleChanged,
                Some(existing.as_str()),
            )
        }
    };
    record_team(
        &mut tx,
        actor,
        TeamEvent {
            action,
            team,
            person: Some(&user_id),
            // `from` is `None` (JSON null) for a new member.
            details: serde_json::json!({"role": role.as_str(), "from": from}),
        },
        now,
    )
    .await?;
    invalidate(
        &mut tx,
        &[Invalidation::team(&locked.id), Invalidation::user(&user_id)],
    )
    .await?;
    tx.commit().await?;
    Ok(change)
}

/// Removes a user from a team, records `team.member_removed`, and bumps
/// the administrative generation.
///
/// # Arguments
/// * `pool` - the database.
/// * `team` - the team's name.
/// * `user` - the user's name.
/// * `actor` - who, for the audit log; a provisioner is refused an admin user.
/// * `now` - the server clock.
///
/// # Errors
/// [`OkfError::UnknownName`] for the team or user (a malformed name
/// included); [`OkfError::Forbidden`] for a provisioner and an admin user;
/// [`OkfError::Refused`] when the user is not a member, is the team's last
/// owner, or is the user whose personal team this is;
/// [`OkfError::Database`], a failed audit write included.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "teams.remove_member"))]
pub async fn remove_member(
    pool: &PgPool,
    team: &str,
    user: &str,
    actor: &AuditActor,
    now: DateTime<Utc>,
) -> Result<(), OkfError> {
    let mut tx = pool.begin().await?;
    // Users before the team, as in `set_member`.
    let people = People {
        target: Some((user, RowLock::KeyShare)),
        teams: &[team],
        ..People::default()
    };
    let (user_id, is_admin) = lock_user(&mut tx, actor, &people, user).await?;
    audit::refuse_admin_target(actor, is_admin, user)?;
    let locked = lock_team(&mut tx, team).await?;
    if locked.personal_of.as_deref() == Some(user_id.as_str()) {
        return Err(OkfError::Refused {
            reason: format!("{team:?} is {user:?}'s personal team; it cannot leave it"),
        });
    }
    match current_role(&mut tx, &locked.id, &user_id).await? {
        None => {
            return Err(OkfError::Refused {
                reason: format!("{user:?} is not a member of team {team:?}"),
            })
        }
        Some(Role::Owner) => refuse_last_owner(&mut tx, &locked.id, team, user).await?,
        Some(Role::Member) => {}
    }
    sqlx::query("DELETE FROM team_members WHERE team_id = $1 AND user_id = $2")
        .bind(&locked.id)
        .bind(&user_id)
        .execute(&mut *tx)
        .await?;
    record_team(
        &mut tx,
        actor,
        TeamEvent {
            action: Action::TeamMemberRemoved,
            team,
            person: Some(&user_id),
            details: serde_json::json!({}),
        },
        now,
    )
    .await?;
    invalidate(
        &mut tx,
        &[Invalidation::team(&locked.id), Invalidation::user(&user_id)],
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// A change of a shared team's display name: [`set_display_name`]'s input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TeamDisplayNameChange<'a> {
    /// The team's name.
    pub team: &'a str,
    /// The new display name (empty clears it).
    pub display_name: &'a DisplayName,
    /// `Some(users.id)` when the caller may make the change only as an
    /// owner of the team (a console session that is neither a provisioner
    /// nor an every-catalog admin): checked under the team's lock, so an
    /// owner removed or demoted meanwhile is refused. `None` when the
    /// caller is already allowed (an admin, a provisioner, the CLI).
    pub owner: Option<&'a str>,
}

/// Sets a shared team's display name, records `team.display_name_changed`
/// (its new length in characters, never the text) and bumps the
/// administrative generation (the key cache's labels read it). The same
/// text again is no change: it writes and records nothing. A personal
/// team's label follows its user, so it is refused.
///
/// Lock order (`tools/agentic/lessons.md`): the users the entry resolves
/// ([`audit::lock_people`]), then the team row (`FOR NO KEY UPDATE`: the
/// column is no key, so a membership or catalog insert's foreign-key check
/// need not wait for it), then, for an owner, its membership row, then
/// `server_state`.
///
/// # Arguments
/// * `pool` - the database.
/// * `change` - the team, its new display name, and the owner to check, if any.
/// * `actor` - who, for the audit log.
/// * `now` - the server clock.
///
/// # Errors
/// [`OkfError::UnknownName`] (a malformed name included);
/// [`OkfError::Refused`] for a personal team; [`OkfError::Forbidden`] when
/// the owner check fails; [`OkfError::Database`], a failed audit write
/// included.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "teams.set_display_name"))]
pub async fn set_display_name(
    pool: &PgPool,
    change: &TeamDisplayNameChange<'_>,
    actor: &AuditActor,
    now: DateTime<Utc>,
) -> Result<(), OkfError> {
    let TeamDisplayNameChange {
        team,
        display_name,
        owner,
    } = *change;
    // As in `find`: a malformed name names no team (issue #339).
    if !is_valid_name(team) {
        return Err(unknown(team));
    }
    let mut tx = pool.begin().await?;
    audit::lock_people(
        &mut tx,
        actor,
        &People {
            teams: &[team],
            ..People::default()
        },
    )
    .await?;
    let (team_id, personal) = sqlx::query_as::<_, (String, bool)>(
        "SELECT id, personal_of IS NOT NULL FROM teams WHERE name = $1 FOR NO KEY UPDATE",
    )
    .bind(team)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| unknown(team))?;
    if personal {
        return Err(OkfError::Refused {
            reason: "a personal team's label follows its user; change the user's display name"
                .to_string(),
        });
    }
    if let Some(user_id) = owner {
        // `FOR SHARE` holds the owner's membership row, so a concurrent
        // removal or demotion waits for this change rather than racing it.
        let is_owner: Option<i32> = sqlx::query_scalar(
            "SELECT 1 FROM team_members \
             WHERE team_id = $1 AND user_id = $2 AND role = 'owner' FOR SHARE",
        )
        .bind(&team_id)
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?;
        if is_owner.is_none() {
            return Err(OkfError::Forbidden {
                reason: format!(
                    "this needs an owner of team {team:?} or an admin that reaches every catalog"
                ),
            });
        }
    }
    // The subquery reads the old value (the row is already locked), so
    // `changed` compares it with the new one in the same statement.
    let changed = sqlx::query_scalar::<_, bool>(
        "UPDATE teams t SET display_name = $2 \
         FROM (SELECT id, display_name FROM teams WHERE id = $1) old \
         WHERE t.id = old.id \
         RETURNING old.display_name <> t.display_name",
    )
    .bind(&team_id)
    .bind(display_name.as_str())
    .fetch_one(&mut *tx)
    .await?;
    if !changed {
        // Dropping `tx` rolls back the `UPDATE`, which wrote the same text.
        return Ok(());
    }
    record_team(
        &mut tx,
        actor,
        TeamEvent {
            action: Action::TeamDisplayNameChanged,
            team,
            person: None,
            // The length in characters, never the text.
            details: serde_json::json!({"length": display_name.as_str().chars().count()}),
        },
        now,
    )
    .await?;
    invalidate(&mut tx, &[Invalidation::team(&team_id)]).await?;
    tx.commit().await?;
    Ok(())
}

/// Pins a team to a store (`Some`), or removes its pin (`None`): catalogs
/// the team owns from then on are created there, instead of on the default
/// store. Catalogs that exist stay where they are. Records
/// `team.store_pinned` or `team.store_unpinned` (details: the store it
/// was on and the one now, `null` when unpinned) and sends the team's invalidation, in one
/// transaction. Pinning to the store it is on, or unpinning an unpinned
/// team, is no change: nothing is written or recorded.
///
/// Lock order (`tools/agentic/lessons.md`): the users the entry resolves
/// ([`audit::lock_people`]), the team row (`FOR NO KEY UPDATE`), the
/// store's row (`FOR SHARE`), then `server_state`.
///
/// # Arguments
/// * `pool` - the account database.
/// * `team` - the team's name.
/// * `store` - the store to pin to, or `None` to unpin.
/// * `actor` - who, for the audit log.
/// * `now` - the server clock.
///
/// # Returns
/// `true` if the pin changed.
///
/// # Errors
/// [`OkfError::UnknownName`] for an unknown team (a malformed name
/// included) or store; [`OkfError::Database`], a failed audit write
/// included.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "teams.set_store"))]
pub async fn set_store(
    pool: &PgPool,
    team: &str,
    store: Option<&StoreName>,
    actor: &AuditActor,
    now: DateTime<Utc>,
) -> Result<bool, OkfError> {
    if !is_valid_name(team) {
        return Err(unknown(team));
    }
    let mut tx = pool.begin().await?;
    audit::lock_people(
        &mut tx,
        actor,
        &People {
            teams: &[team],
            ..People::default()
        },
    )
    .await?;
    let (team_id, previous) = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT id, store FROM teams WHERE name = $1 FOR NO KEY UPDATE",
    )
    .bind(team)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| unknown(team))?;
    if previous.as_deref() == store.map(StoreName::as_str) {
        return Ok(false);
    }
    if let Some(store) = store {
        stores::lock_store(&mut tx, store).await?;
    }
    sqlx::query("UPDATE teams SET store = $2 WHERE id = $1")
        .bind(&team_id)
        .bind(store.map(StoreName::as_str))
        .execute(&mut *tx)
        .await?;
    let action = if store.is_some() {
        Action::TeamStorePinned
    } else {
        Action::TeamStoreUnpinned
    };
    record_team(
        &mut tx,
        actor,
        TeamEvent {
            action,
            team,
            person: None,
            details: serde_json::json!({"from": previous, "to": store.map(StoreName::as_str)}),
        },
        now,
    )
    .await?;
    invalidate(&mut tx, &[Invalidation::team(&team_id)]).await?;
    tx.commit().await?;
    Ok(true)
}

/// The refusal for a team or user whose catalog was deleted but whose registry
/// row the store cleanup has not yet removed (it keeps its owner until then),
/// or `None` when no such catalog is among `owned` (name, deleted) pairs.
///
/// # Arguments
/// * `owned` - the owner's catalogs as (name, whether it is deleted).
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(uninstrumented_fn, reason = "a pure function of its argument")
)]
pub(crate) fn still_deleting_reason(owned: &[(String, bool)]) -> Option<String> {
    let names: Vec<&str> = owned
        .iter()
        .filter(|(_, deleted)| *deleted)
        .map(|(name, _)| name.as_str())
        .collect();
    match names.as_slice() {
        [] => None,
        [one] => Some(format!(
            "catalog {one:?} is still being deleted; try again shortly"
        )),
        many => Some(format!(
            "catalogs {} are still being deleted; try again shortly",
            many.iter()
                .map(|name| format!("{name:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Deletes a shared team that owns no catalog (its memberships cascade),
/// records `team.deleted`, and bumps the administrative generation (spec
/// "Deletion").
///
/// # Arguments
/// * `pool` - the database.
/// * `name` - the team's name.
/// * `actor` - who, for the audit log.
/// * `now` - the server clock.
///
/// # Errors
/// [`OkfError::UnknownName`] (a malformed name included);
/// [`OkfError::Refused`] for a personal team (it goes with its user), one
/// that owns a catalog, or one whose deleted catalog is still being cleaned
/// up; [`OkfError::Database`], a failed audit write
/// included.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "teams.delete"))]
pub async fn delete(
    pool: &PgPool,
    name: &str,
    actor: &AuditActor,
    now: DateTime<Utc>,
) -> Result<(), OkfError> {
    let mut tx = pool.begin().await?;
    audit::lock_people(
        &mut tx,
        actor,
        &People {
            teams: &[name],
            ..People::default()
        },
    )
    .await?;
    let locked = lock_team(&mut tx, name).await?;
    if locked.personal_of.is_some() {
        return Err(OkfError::Refused {
            reason: format!("{name:?} is a personal team; it is deleted with its user"),
        });
    }
    // A deleted catalog keeps its registry row (and so its owner) until the
    // store cleanup removes it: say so, rather than "owns a catalog" about
    // one the person has already deleted.
    let owned_rows: Vec<(String, bool)> = sqlx::query_as(
        "SELECT name, deleted_at IS NOT NULL FROM catalogs WHERE owner_team_id = $1 ORDER BY name",
    )
    .bind(&locked.id)
    .fetch_all(&mut *tx)
    .await?;
    if let Some(reason) = still_deleting_reason(&owned_rows) {
        return Err(OkfError::Refused { reason });
    }
    let owned = owned_rows.len();
    if owned > 0 {
        return Err(OkfError::Refused {
            reason: format!("team {name:?} owns {owned} catalog(s); give them another owner first"),
        });
    }
    // Recorded while the row exists: the entry's team reference is
    // resolved from it.
    record_team(
        &mut tx,
        actor,
        TeamEvent {
            action: Action::TeamDeleted,
            team: name,
            person: None,
            details: serde_json::json!({}),
        },
        now,
    )
    .await?;
    sqlx::query("DELETE FROM teams WHERE id = $1")
        .bind(&locked.id)
        .execute(&mut *tx)
        .await?;
    invalidate(&mut tx, &[Invalidation::team(&locked.id)]).await?;
    tx.commit().await?;
    Ok(())
}

/// A notice as stored: trimmed, `None` when blank.
///
/// # Arguments
/// * `notice` - the notice as given.
///
/// # Returns
/// The trimmed notice, or `None` for a missing or blank one.
///
/// # Errors
/// [`OkfError::Refused`] for one longer than [`MAX_NOTICE_CHARS`], or one
/// holding a NUL character (Postgres text cannot store it, so it would
/// otherwise be a database error, a 500).
#[tracing::instrument(level = "trace", skip_all)]
fn clean_notice(notice: Option<&str>) -> Result<Option<&str>, OkfError> {
    // `filter` turns a blank (after trimming) notice into `None`.
    let notice = notice.map(str::trim).filter(|text| !text.is_empty());
    if let Some(text) = notice {
        if text.contains('\0') {
            return Err(OkfError::Refused {
                reason: "a suspension notice cannot contain a NUL character".to_string(),
            });
        }
        // `chars().count()`: characters, as the schema's `char_length`
        // counts, not bytes.
        if text.chars().count() > MAX_NOTICE_CHARS {
            return Err(OkfError::Refused {
                reason: format!("a suspension notice is at most {MAX_NOTICE_CHARS} characters"),
            });
        }
    }
    Ok(notice)
}

/// Suspends a team, or replaces the notice of one already suspended
/// (keeping its first `suspended_at`), records `team.suspended`, and bumps
/// the administrative generation. Suspending a team already suspended with
/// the same notice changes nothing, so it writes and records nothing.
///
/// # Arguments
/// * `pool` - the database.
/// * `name` - the team's name.
/// * `notice` - what members are told; blank or `None` for no notice.
/// * `actor` - who, for the audit log.
/// * `now` - the suspension time, if it was not suspended (server clock).
///
/// # Returns
/// The team as it now is.
///
/// # Errors
/// [`OkfError::UnknownName`] (a malformed name included);
/// [`OkfError::Refused`] for a notice over [`MAX_NOTICE_CHARS`] or holding
/// a NUL; [`OkfError::Database`], a failed audit write included.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "teams.suspend"))]
pub async fn suspend(
    pool: &PgPool,
    name: &str,
    notice: Option<&str>,
    actor: &AuditActor,
    now: DateTime<Utc>,
) -> Result<TeamRecord, OkfError> {
    let notice = clean_notice(notice)?;
    let mut tx = pool.begin().await?;
    audit::lock_people(
        &mut tx,
        actor,
        &People {
            teams: &[name],
            ..People::default()
        },
    )
    .await?;
    let locked = lock_team(&mut tx, name).await?;
    // The state before the write, under the team's lock.
    let (already, previous_notice): (bool, Option<String>) = sqlx::query_as(
        "SELECT suspended_at IS NOT NULL, suspension_notice FROM teams WHERE id = $1",
    )
    .bind(&locked.id)
    .fetch_one(&mut *tx)
    .await?;
    if already && previous_notice.as_deref() == notice {
        // Dropping `tx` rolls back the read-only transaction.
        drop(tx);
        return find(pool, name).await;
    }
    // `COALESCE` keeps the first suspension time on a repeat.
    sqlx::query(
        "UPDATE teams SET suspended_at = COALESCE(suspended_at, $2), suspension_notice = $3 \
         WHERE id = $1",
    )
    .bind(&locked.id)
    .bind(now)
    .bind(notice)
    .execute(&mut *tx)
    .await?;
    record_team(
        &mut tx,
        actor,
        TeamEvent {
            action: Action::TeamSuspended,
            team: name,
            person: None,
            // Whether there is a notice, never its text.
            details: serde_json::json!({"notice": notice.is_some(), "already": already}),
        },
        now,
    )
    .await?;
    let invalidations = suspension_invalidations(&locked.id);
    invalidate(&mut tx, &invalidations).await?;
    tx.commit().await?;
    find(pool, name).await
}

/// Lifts a team's suspension, and records `team.unsuspended` and bumps
/// the administrative generation when there was one.
///
/// # Arguments
/// * `pool` - the database.
/// * `name` - the team's name.
/// * `actor` - who, for the audit log.
/// * `now` - the server clock.
///
/// # Returns
/// The team as it now is.
///
/// # Errors
/// [`OkfError::UnknownName`] (a malformed name included);
/// [`OkfError::Database`], a failed audit write included.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "teams.unsuspend"))]
pub async fn unsuspend(
    pool: &PgPool,
    name: &str,
    actor: &AuditActor,
    now: DateTime<Utc>,
) -> Result<TeamRecord, OkfError> {
    let mut tx = pool.begin().await?;
    audit::lock_people(
        &mut tx,
        actor,
        &People {
            teams: &[name],
            ..People::default()
        },
    )
    .await?;
    let locked = lock_team(&mut tx, name).await?;
    let lifted = sqlx::query(
        "UPDATE teams SET suspended_at = NULL, suspension_notice = NULL \
         WHERE id = $1 AND suspended_at IS NOT NULL",
    )
    .bind(&locked.id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if lifted > 0 {
        record_team(
            &mut tx,
            actor,
            TeamEvent {
                action: Action::TeamUnsuspended,
                team: name,
                person: None,
                details: serde_json::json!({}),
            },
            now,
        )
        .await?;
        let invalidations = suspension_invalidations(&locked.id);
        invalidate(&mut tx, &invalidations).await?;
    }
    tx.commit().await?;
    find(pool, name).await
}

/// A team row held `FOR UPDATE` by the caller's transaction.
struct LockedTeam {
    /// Its `teams.id`.
    id: String,
    /// The `users.id` whose personal team it is, if any.
    personal_of: Option<String>,
}

/// Locks the team row `FOR UPDATE`, so concurrent membership changes to one
/// team run one at a time (the last-owner check cannot race), and so a
/// concurrent catalog assignment to it waits for a delete.
///
/// Lock order: a caller locks every user it needs first
/// ([`audit::lock_people`], which includes a personal team's person),
/// because `users::delete` locks the user and then its teams.
///
/// # Errors
/// [`OkfError::UnknownName`]; [`OkfError::Database`].
#[tracing::instrument(level = "trace", skip_all, fields(db.system.name = "postgresql", db.operation.name = "teams.lock_team"))]
async fn lock_team(conn: &mut PgConnection, name: &str) -> Result<LockedTeam, OkfError> {
    // As in `find`: a malformed name names no team (issue #339).
    if !is_valid_name(name) {
        return Err(unknown(name));
    }
    let (id, personal_of) = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT id, personal_of FROM teams WHERE name = $1 FOR UPDATE",
    )
    .bind(name)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| unknown(name))?;
    Ok(LockedTeam { id, personal_of })
}

/// The user's role in the team, or `None` if it is not a member.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "trace", skip_all, fields(db.system.name = "postgresql", db.operation.name = "teams.current_role"))]
async fn current_role(
    conn: &mut PgConnection,
    team_id: &str,
    user_id: &str,
) -> Result<Option<Role>, OkfError> {
    let role: Option<String> =
        sqlx::query_scalar("SELECT role FROM team_members WHERE team_id = $1 AND user_id = $2")
            .bind(team_id)
            .bind(user_id)
            .fetch_optional(&mut *conn)
            .await?;
    // `Option::map` applies the closure only to `Some`, like a None-safe call.
    role.map(|text| decode_column(Role::parse(&text), "team_members.role"))
        .transpose()
}

/// Inserts one membership row.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "trace", skip_all, fields(db.system.name = "postgresql", db.operation.name = "teams.insert_member"))]
async fn insert_member(
    conn: &mut PgConnection,
    team_id: &str,
    user_id: &str,
    role: Role,
    now: DateTime<Utc>,
) -> Result<(), OkfError> {
    sqlx::query(
        "INSERT INTO team_members (team_id, user_id, role, created_at) VALUES ($1, $2, $3, $4)",
    )
    .bind(team_id)
    .bind(user_id)
    .bind(role.as_str())
    .bind(now)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Refuses when `user` is the only owner of the team. The caller holds the
/// team row from [`lock_team`], so the count cannot change under it.
///
/// # Errors
/// [`OkfError::Refused`] for the last owner; [`OkfError::Database`].
#[tracing::instrument(level = "trace", skip_all, fields(db.system.name = "postgresql", db.operation.name = "teams.refuse_last_owner"))]
async fn refuse_last_owner(
    conn: &mut PgConnection,
    team_id: &str,
    team: &str,
    user: &str,
) -> Result<(), OkfError> {
    let owners: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM team_members WHERE team_id = $1 AND role = 'owner'",
    )
    .bind(team_id)
    .fetch_one(&mut *conn)
    .await?;
    if owners <= 1 {
        return Err(OkfError::Refused {
            reason: format!("{user:?} is the last owner of team {team:?}; add another owner first"),
        });
    }
    Ok(())
}

/// What a suspension or its lifting changes: the team (its label and
/// members) and every catalog it owns (a suspended team's catalogs refuse
/// their members). Two messages whatever the team owns: the catalogs are
/// named by the team, not listed, so nothing is read and nothing grows with
/// their number (issue #757). A catalog moved to the team meanwhile waits
/// on the team's lock, and its own `set_owner` invalidates it.
///
/// # Arguments
/// * `team_id` - the team's id.
///
/// # Returns
/// The team's invalidation, then its catalogs'.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
    )
)]
fn suspension_invalidations(team_id: &str) -> [Invalidation; 2] {
    [
        Invalidation::team(team_id),
        Invalidation::team_catalogs(team_id),
    ]
}

/// The id of the team called `name`, inside a caller's transaction.
///
/// The row is held `FOR KEY SHARE` until that transaction ends, so a
/// concurrent [`delete`] (which locks the team `FOR UPDATE`) waits for the
/// caller and then sees what it wrote (a catalog the team now owns); one that
/// got there first leaves no row, which is `UnknownName` here rather than a
/// foreign-key failure at the caller's write.
///
/// # Arguments
/// * `conn` - the caller's transaction (pass `&mut tx`).
/// * `name` - the team's name.
///
/// # Returns
/// Its `teams.id`.
///
/// # Errors
/// [`OkfError::UnknownName`]; [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "teams.id_of"))]
pub(crate) async fn id_of(conn: &mut PgConnection, name: &str) -> Result<String, OkfError> {
    // As in `find`: a malformed name names no team (issue #339).
    if !is_valid_name(name) {
        return Err(unknown(name));
    }
    sqlx::query_scalar("SELECT id FROM teams WHERE name = $1 FOR KEY SHARE")
        .bind(name)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| unknown(name))
}

/// [`audit::lock_people`] for a change about one user: its id and admin
/// flag, read under the lock (`FOR KEY SHARE`, which `users::set_admin`'s
/// `FOR UPDATE` conflicts with, so a guard on the flag cannot race it).
///
/// # Errors
/// [`OkfError::UnknownName`] for an unknown or malformed user;
/// [`OkfError::Database`].
#[tracing::instrument(level = "trace", skip_all, fields(db.system.name = "postgresql", db.operation.name = "teams.lock_user"))]
async fn lock_user(
    conn: &mut PgConnection,
    actor: &AuditActor,
    people: &People<'_>,
    user: &str,
) -> Result<(String, bool), OkfError> {
    // `lock_people` answers `Some` whenever it is given a target.
    audit::lock_people(conn, actor, people)
        .await?
        .ok_or_else(|| users::unknown(user))
}

/// One team event to record: [`record_team`]'s input.
struct TeamEvent<'a> {
    /// What happened.
    action: Action,
    /// The team's name; it must still exist.
    team: &'a str,
    /// The `users.id` the event is also about (a member, the owner).
    person: Option<&'a str>,
    /// The entry's details.
    details: serde_json::Value,
}

/// Records an entry about a team in the change's transaction: the team is
/// both the target and the one team the entry belongs to.
///
/// # Errors
/// [`OkfError::Database`] (which fails the change).
#[tracing::instrument(level = "trace", skip_all, fields(db.system.name = "postgresql", db.operation.name = "teams.record_team"))]
async fn record_team(
    conn: &mut PgConnection,
    actor: &AuditActor,
    event: TeamEvent<'_>,
    now: DateTime<Utc>,
) -> Result<(), OkfError> {
    let team_ref = audit::team_ref(conn, event.team).await?;
    audit::record(
        conn,
        actor,
        &Entry {
            action: event.action,
            target: Target::Team(event.team),
            person: event.person,
            teams: &[team_ref],
            details: event.details,
        },
        now,
    )
    .await?;
    Ok(())
}

/// The error for a team name that names no team.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial conversion: builds an error value, no I/O"
    )
)]
fn unknown(name: &str) -> OkfError {
    OkfError::UnknownName {
        noun: "team",
        name: name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{test_db, TEST_USER};
    use crate::store::audit::AuditActor;
    use crate::store::users::{self, NewUser};

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-11T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    async fn family(pool: &PgPool) {
        users::create(
            pool,
            &NewUser {
                name: "alice",
                display_name: &DisplayName::default(),
            },
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();
        create(
            pool,
            &NewTeam {
                name: "family",
                display_name: &DisplayName::parse("The family").unwrap(),
                owner: "alice",
            },
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();
    }

    #[test]
    fn role_spellings_round_trip() {
        for role in [Role::Owner, Role::Member] {
            assert_eq!(Role::parse(role.as_str()), Some(role));
        }
        assert_eq!(Role::parse("admin"), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_team_starts_with_its_owner() {
        let db = test_db().await;
        family(&db.pool).await;

        let team = find(&db.pool, "family").await.unwrap();

        assert_eq!(team.personal_of, None);
        assert_eq!(team.display_name, "The family");
        assert_eq!(
            team.members
                .iter()
                .map(|m| (m.user.as_str(), m.role))
                .collect::<Vec<_>>(),
            [("alice", Role::Owner)]
        );
        assert!(team.catalogs.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_groups_members_by_team() {
        let db = test_db().await;
        family(&db.pool).await;
        set_member(
            &db.pool,
            &crate::store::teams::Membership {
                team: "family",
                user: TEST_USER,
                role: Role::Member,
            },
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();

        let teams = list(&db.pool).await.unwrap();

        let summary: Vec<(&str, Vec<&str>)> = teams
            .iter()
            .map(|t| {
                let members = t.members.iter().map(|m| m.user.as_str()).collect();
                (t.name.as_str(), members)
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("alice", vec!["alice"]),
                ("family", vec!["alice", TEST_USER]),
                (crate::db::TEST_TEAM, vec![TEST_USER]),
            ]
        );
        assert_eq!(find(&db.pool, "family").await.unwrap(), teams[1]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unknown_owner_is_unknown_name() {
        let db = test_db().await;
        let made = create(
            &db.pool,
            &NewTeam {
                name: "x",
                display_name: &DisplayName::default(),
                owner: "nobody",
            },
            &AuditActor::cli(),
            now(),
        )
        .await;
        assert!(matches!(
            made,
            Err(OkfError::UnknownName { noun: "user", .. })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_member_adds_then_changes_the_role_then_is_unchanged() {
        let db = test_db().await;
        family(&db.pool).await;

        let added = set_member(
            &db.pool,
            &crate::store::teams::Membership {
                team: "family",
                user: TEST_USER,
                role: Role::Member,
            },
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();
        let changed = set_member(
            &db.pool,
            &crate::store::teams::Membership {
                team: "family",
                user: TEST_USER,
                role: Role::Owner,
            },
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();
        let same = set_member(
            &db.pool,
            &crate::store::teams::Membership {
                team: "family",
                user: TEST_USER,
                role: Role::Owner,
            },
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();

        assert_eq!(
            (added, changed, same),
            (
                MemberChange::Added,
                MemberChange::RoleChanged,
                MemberChange::Unchanged
            )
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_member_on_an_unknown_team_or_user_is_unknown_name() {
        let db = test_db().await;
        family(&db.pool).await;

        let no_team = set_member(
            &db.pool,
            &crate::store::teams::Membership {
                team: "nowhere",
                user: TEST_USER,
                role: Role::Member,
            },
            &AuditActor::cli(),
            now(),
        )
        .await;
        let no_user = set_member(
            &db.pool,
            &crate::store::teams::Membership {
                team: "family",
                user: "nobody",
                role: Role::Member,
            },
            &AuditActor::cli(),
            now(),
        )
        .await;

        assert!(matches!(
            no_team,
            Err(OkfError::UnknownName { noun: "team", .. })
        ));
        assert!(matches!(
            no_user,
            Err(OkfError::UnknownName { noun: "user", .. })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_last_owner_cannot_be_removed_or_demoted() {
        let db = test_db().await;
        family(&db.pool).await;

        let removed =
            remove_member(&db.pool, "family", "alice", &AuditActor::cli(), Utc::now()).await;
        let demoted = set_member(
            &db.pool,
            &crate::store::teams::Membership {
                team: "family",
                user: "alice",
                role: Role::Member,
            },
            &AuditActor::cli(),
            now(),
        )
        .await;

        assert!(
            matches!(removed, Err(OkfError::Refused { .. })),
            "{removed:?}"
        );
        assert!(
            matches!(demoted, Err(OkfError::Refused { .. })),
            "{demoted:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_removal_of_two_owners_leaves_one() {
        // Both removals would pass an unlocked "more than one owner" check;
        // the team-row lock makes the second see the first's delete.
        let db = test_db().await;
        family(&db.pool).await;
        set_member(
            &db.pool,
            &crate::store::teams::Membership {
                team: "family",
                user: TEST_USER,
                role: Role::Owner,
            },
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();

        let actor = AuditActor::cli();
        let (first, second) = tokio::join!(
            remove_member(&db.pool, "family", "alice", &actor, now()),
            remove_member(&db.pool, "family", TEST_USER, &actor, now()),
        );

        assert_eq!(
            [first.is_ok(), second.is_ok()]
                .iter()
                .filter(|ok| **ok)
                .count(),
            1,
            "{first:?} {second:?}"
        );
        let owners = find(&db.pool, "family").await.unwrap().members;
        assert_eq!(owners.len(), 1);
        assert_eq!(owners[0].role, Role::Owner);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_personal_teams_own_user_cannot_be_removed() {
        let db = test_db().await;
        family(&db.pool).await;
        set_member(
            &db.pool,
            &crate::store::teams::Membership {
                team: "alice",
                user: TEST_USER,
                role: Role::Owner,
            },
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();

        let removed =
            remove_member(&db.pool, "alice", "alice", &AuditActor::cli(), Utc::now()).await;

        assert!(
            matches!(removed, Err(OkfError::Refused { .. })),
            "{removed:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_member_is_removed_but_a_non_member_is_refused() {
        let db = test_db().await;
        family(&db.pool).await;
        set_member(
            &db.pool,
            &crate::store::teams::Membership {
                team: "family",
                user: TEST_USER,
                role: Role::Member,
            },
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();

        remove_member(
            &db.pool,
            "family",
            TEST_USER,
            &AuditActor::cli(),
            Utc::now(),
        )
        .await
        .unwrap();
        let again = remove_member(
            &db.pool,
            "family",
            TEST_USER,
            &AuditActor::cli(),
            Utc::now(),
        )
        .await;

        assert_eq!(find(&db.pool, "family").await.unwrap().members.len(), 1);
        assert!(matches!(again, Err(OkfError::Refused { .. })), "{again:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_personal_team_is_never_deleted_but_a_shared_one_is() {
        let db = test_db().await;
        family(&db.pool).await;

        let personal = delete(&db.pool, "alice", &AuditActor::cli(), Utc::now()).await;
        delete(&db.pool, "family", &AuditActor::cli(), Utc::now())
            .await
            .unwrap();

        assert!(matches!(personal, Err(OkfError::Refused { .. })));
        assert!(matches!(
            find(&db.pool, "family").await,
            Err(OkfError::UnknownName { .. })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_team_that_owns_a_catalog_is_not_deleted() {
        let db = test_db().await;
        family(&db.pool).await;
        users::tests::seed_catalog_owned_by(&db.pool, "family").await;
        let generation = crate::store::catalogs::admin_generation(&db.pool)
            .await
            .unwrap();

        let refused = delete(&db.pool, "family", &AuditActor::cli(), Utc::now()).await;

        assert!(
            matches!(&refused, Err(OkfError::Refused { reason }) if reason.contains("catalog")),
            "{refused:?}"
        );
        let team = find(&db.pool, "family").await.unwrap();
        assert_eq!(team.catalogs, ["shared"]);
        assert_eq!(team.members.len(), 1);
        assert_eq!(
            crate::store::catalogs::admin_generation(&db.pool)
                .await
                .unwrap(),
            generation
        );
    }

    /// Which membership change races the user's deletion.
    #[derive(Debug, Clone, Copy)]
    enum Change {
        SetMember,
        RemoveMember,
    }

    /// Deletes `alice` the way `users::delete` takes its locks (the user
    /// `FOR UPDATE`, then her personal team `FOR UPDATE`) while `change` on
    /// her personal team runs between the two, and returns that change's
    /// outcome.
    async fn race_membership_with_deleting_alice(change: Change) -> Result<(), OkfError> {
        let db = test_db().await;
        users::create(
            &db.pool,
            &NewUser {
                name: "alice",
                display_name: &DisplayName::default(),
            },
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();
        let mut deleting = db.pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM users WHERE name = 'alice' FOR UPDATE")
            .execute(&mut *deleting)
            .await
            .unwrap();
        // `tokio::spawn` runs the change concurrently, like a thread. It must
        // wait on the user without holding alice's team: with the team taken
        // first, the next statement and the change would deadlock.
        let pool = db.pool.clone();
        let changing = tokio::spawn(async move {
            match change {
                Change::SetMember => set_member(
                    &pool,
                    &crate::store::teams::Membership {
                        team: "alice",
                        user: "alice",
                        role: Role::Owner,
                    },
                    &AuditActor::cli(),
                    now(),
                )
                .await
                .map(|_| ()),
                Change::RemoveMember => {
                    remove_member(&pool, "alice", "alice", &AuditActor::cli(), Utc::now()).await
                }
            }
        });
        crate::store::catalogs::tests::until_a_lock_is_awaited(&db.pool).await;

        // A deadlock makes Postgres abort one side (after `deadlock_timeout`);
        // the timeout only bounds a hang.
        let team_lock = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            sqlx::query(
                "SELECT id FROM teams WHERE personal_of = \
                 (SELECT id FROM users WHERE name = 'alice') FOR UPDATE",
            )
            .execute(&mut *deleting),
        )
        .await
        .expect("the team lock does not hang");
        assert!(team_lock.is_ok(), "{change:?}: {team_lock:?}");
        sqlx::query(
            "DELETE FROM teams WHERE personal_of = (SELECT id FROM users WHERE name = 'alice')",
        )
        .execute(&mut *deleting)
        .await
        .unwrap();
        sqlx::query("DELETE FROM users WHERE name = 'alice'")
            .execute(&mut *deleting)
            .await
            .unwrap();
        deleting.commit().await.unwrap();
        changing.await.unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_membership_change_racing_a_user_delete_waits_instead_of_deadlocking() {
        for change in [Change::SetMember, Change::RemoveMember] {
            let outcome = race_membership_with_deleting_alice(change).await;

            // The change waited on the user, then found it gone: not a
            // deadlock or foreign-key error (a 500).
            assert!(
                matches!(&outcome, Err(OkfError::UnknownName { noun: "user", .. })),
                "{change:?}: {outcome:?}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn suspend_keeps_the_first_time_and_replaces_the_notice() {
        let db = test_db().await;
        family(&db.pool).await;
        let later = now() + chrono::TimeDelta::hours(1);
        let before = crate::store::catalogs::admin_generation(&db.pool)
            .await
            .unwrap();

        let first = suspend(
            &db.pool,
            "family",
            Some("Contact the operator."),
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();
        let after_first = crate::store::catalogs::admin_generation(&db.pool)
            .await
            .unwrap();
        let again = suspend(
            &db.pool,
            "family",
            Some("Still paused."),
            &AuditActor::cli(),
            later,
        )
        .await
        .unwrap();

        assert!(after_first > before, "suspending bumps the generation");

        assert_eq!(
            first.suspension,
            Some(Suspension {
                since: now(),
                notice: Some("Contact the operator.".to_string()),
            })
        );
        let again = again.suspension.unwrap();
        assert_eq!(again.since, now(), "a second suspend keeps the first time");
        assert_eq!(again.notice.as_deref(), Some("Still paused."));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_blank_notice_is_none_and_a_long_one_is_refused() {
        let db = test_db().await;
        family(&db.pool).await;

        let blank = suspend(&db.pool, "family", Some("   "), &AuditActor::cli(), now())
            .await
            .unwrap();
        let long = "x".repeat(MAX_NOTICE_CHARS + 1);
        let refused = suspend(&db.pool, "family", Some(&long), &AuditActor::cli(), now()).await;

        let nul = suspend(&db.pool, "family", Some("a\0b"), &AuditActor::cli(), now()).await;
        let at_limit = "\u{e9}".repeat(MAX_NOTICE_CHARS);
        let multibyte = suspend(
            &db.pool,
            "family",
            Some(&at_limit),
            &AuditActor::cli(),
            now(),
        )
        .await;

        assert_eq!(blank.suspension.unwrap().notice, None);
        assert!(
            matches!(refused, Err(OkfError::Refused { .. })),
            "{refused:?}"
        );
        assert!(matches!(nul, Err(OkfError::Refused { .. })), "{nul:?}");
        // 2000 two-byte characters: the limit counts characters, not bytes.
        let multibyte = multibyte.unwrap().suspension.unwrap().notice.unwrap();
        assert_eq!(multibyte.chars().count(), MAX_NOTICE_CHARS);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_schema_refuses_a_notice_without_a_suspension_or_over_the_limit() {
        // The store never writes either; the constraints guard hand edits.
        let db = test_db().await;
        family(&db.pool).await;
        let long = "x".repeat(MAX_NOTICE_CHARS + 1);

        let orphan = sqlx::query("UPDATE teams SET suspension_notice = 'x' WHERE name = 'family'")
            .execute(&db.pool)
            .await;
        let too_long = sqlx::query(
            "UPDATE teams SET suspended_at = now(), suspension_notice = $1 WHERE name = 'family'",
        )
        .bind(&long)
        .execute(&db.pool)
        .await;

        assert!(orphan.is_err(), "a notice needs a suspension");
        assert!(
            too_long.is_err(),
            "a notice is at most {MAX_NOTICE_CHARS} characters"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unsuspend_lifts_and_is_a_no_op_when_active() {
        let db = test_db().await;
        family(&db.pool).await;
        suspend(&db.pool, "family", None, &AuditActor::cli(), now())
            .await
            .unwrap();
        let before = crate::store::catalogs::admin_generation(&db.pool)
            .await
            .unwrap();

        let lifted = unsuspend(&db.pool, "family", &AuditActor::cli(), Utc::now())
            .await
            .unwrap();
        let after_lift = crate::store::catalogs::admin_generation(&db.pool)
            .await
            .unwrap();
        let unchanged = unsuspend(&db.pool, "family", &AuditActor::cli(), Utc::now())
            .await
            .unwrap();
        let after_noop = crate::store::catalogs::admin_generation(&db.pool)
            .await
            .unwrap();

        assert_eq!(lifted.suspension, None);
        assert_eq!(unchanged.suspension, None);
        assert!(after_lift > before, "lifting bumps the generation");
        assert_eq!(after_noop, after_lift, "lifting nothing does not");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn suspending_an_unknown_team_is_unknown_name() {
        let db = test_db().await;

        let suspended = suspend(&db.pool, "nobody", None, &AuditActor::cli(), now()).await;
        let lifted = unsuspend(&db.pool, "nobody", &AuditActor::cli(), Utc::now()).await;

        assert!(matches!(
            suspended,
            Err(OkfError::UnknownName { noun: "team", .. })
        ));
        assert!(matches!(
            lifted,
            Err(OkfError::UnknownName { noun: "team", .. })
        ));
    }

    // ---- The audit log (M14 Task 6b) ----

    use crate::store::audit::testing::entries;
    use crate::store::audit::{Source, Via};
    use serde_json::json;

    /// How long a race test waits for a held lock before it fails.
    const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

    /// Creates user `name` (and its personal team of the same name).
    async fn seed_user(pool: &PgPool, name: &str) {
        users::create(
            pool,
            &NewUser {
                name,
                display_name: &DisplayName::default(),
            },
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();
    }

    /// Makes `name` an admin user directly, without `users::set_admin`.
    async fn make_admin(pool: &PgPool, name: &str) {
        sqlx::query("UPDATE users SET is_admin = true WHERE name = $1")
            .bind(name)
            .execute(pool)
            .await
            .unwrap();
    }

    /// A provisioner, as Task 5's keys will set it.
    fn provisioner() -> AuditActor {
        AuditActor {
            provisioner: true,
            ..AuditActor::cli()
        }
    }

    /// The `users.id` of `name`.
    async fn user_id(pool: &PgPool, name: &str) -> String {
        sqlx::query_scalar("SELECT id FROM users WHERE name = $1")
            .bind(name)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// A console session of user `name`, as `AuditActor::of` builds it.
    async fn session_of(pool: &PgPool, name: &str) -> AuditActor {
        AuditActor {
            source: Source::Console,
            user_id: Some(user_id(pool, name).await),
            label: name.to_string(),
            via: Via::Session,
            operator: true,
            ..AuditActor::cli()
        }
    }

    fn membership<'a>(team: &'a str, user: &'a str, role: Role) -> Membership<'a> {
        Membership { team, user, role }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_records_team_created_with_its_owner() {
        let db = test_db().await;

        family(&db.pool).await;

        let recorded = entries(&db.pool, "team.created").await;
        assert_eq!(recorded.len(), 1, "{recorded:?}");
        assert_eq!(recorded[0].target_kind, "team");
        assert_eq!(recorded[0].target.as_deref(), Some("family"));
        assert_eq!(recorded[0].person.as_deref(), Some("alice"));
        assert_eq!(recorded[0].teams, ["family"]);
        assert_eq!(recorded[0].details, json!({}));
        assert_eq!(recorded[0].source, "cli");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_member_records_an_addition_and_a_role_change_and_none_for_no_change() {
        let db = test_db().await;
        family(&db.pool).await;
        let actor = AuditActor::cli();

        set_member(
            &db.pool,
            &membership("family", TEST_USER, Role::Member),
            &actor,
            now(),
        )
        .await
        .unwrap();
        set_member(
            &db.pool,
            &membership("family", TEST_USER, Role::Owner),
            &actor,
            now(),
        )
        .await
        .unwrap();
        let unchanged = set_member(
            &db.pool,
            &membership("family", TEST_USER, Role::Owner),
            &actor,
            now(),
        )
        .await
        .unwrap();

        assert_eq!(unchanged, MemberChange::Unchanged);
        let added = entries(&db.pool, "team.member_added").await;
        assert_eq!(added.len(), 1, "{added:?}");
        assert_eq!(added[0].target.as_deref(), Some("family"));
        assert_eq!(added[0].person.as_deref(), Some(TEST_USER));
        assert_eq!(added[0].teams, ["family"]);
        assert_eq!(added[0].details, json!({"role": "member", "from": null}));
        let changed = entries(&db.pool, "team.role_changed").await;
        assert_eq!(changed.len(), 1, "no entry for no change: {changed:?}");
        assert_eq!(changed[0].person.as_deref(), Some(TEST_USER));
        assert_eq!(
            changed[0].details,
            json!({"role": "owner", "from": "member"})
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remove_member_records_one_entry_and_none_when_refused() {
        let db = test_db().await;
        family(&db.pool).await;
        let actor = AuditActor::cli();
        set_member(
            &db.pool,
            &membership("family", TEST_USER, Role::Member),
            &actor,
            now(),
        )
        .await
        .unwrap();

        remove_member(&db.pool, "family", TEST_USER, &actor, now())
            .await
            .unwrap();
        let not_a_member = remove_member(&db.pool, "family", TEST_USER, &actor, now()).await;
        let last_owner = remove_member(&db.pool, "family", "alice", &actor, now()).await;

        assert!(matches!(not_a_member, Err(OkfError::Refused { .. })));
        assert!(matches!(last_owner, Err(OkfError::Refused { .. })));
        let removed = entries(&db.pool, "team.member_removed").await;
        assert_eq!(removed.len(), 1, "{removed:?}");
        assert_eq!(removed[0].target.as_deref(), Some("family"));
        assert_eq!(removed[0].person.as_deref(), Some(TEST_USER));
        assert_eq!(removed[0].teams, ["family"]);
        assert_eq!(removed[0].details, json!({}));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn delete_records_the_team_before_it_goes_and_nothing_when_refused() {
        let db = test_db().await;
        family(&db.pool).await;
        let actor = AuditActor::cli();

        let personal = delete(&db.pool, "alice", &actor, now()).await;
        delete(&db.pool, "family", &actor, now()).await.unwrap();

        assert!(matches!(personal, Err(OkfError::Refused { .. })));
        let deleted = entries(&db.pool, "team.deleted").await;
        assert_eq!(deleted.len(), 1, "{deleted:?}");
        // The team row is gone; its `audit_teams` row keeps the name.
        assert_eq!(deleted[0].target.as_deref(), Some("family"));
        assert_eq!(deleted[0].teams, ["family"]);
        assert_eq!(deleted[0].details, json!({}));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn suspend_records_each_change_and_nothing_for_a_repeat_or_a_refusal() {
        let db = test_db().await;
        family(&db.pool).await;
        let actor = AuditActor::cli();
        let long = "x".repeat(MAX_NOTICE_CHARS + 1);

        suspend(&db.pool, "family", Some("Paused."), &actor, now())
            .await
            .unwrap();
        let generation = crate::store::catalogs::admin_generation(&db.pool)
            .await
            .unwrap();
        let repeat = suspend(&db.pool, "family", Some(" Paused. "), &actor, now())
            .await
            .unwrap();
        let after_repeat = crate::store::catalogs::admin_generation(&db.pool)
            .await
            .unwrap();
        suspend(&db.pool, "family", Some("Still paused."), &actor, now())
            .await
            .unwrap();
        suspend(&db.pool, "family", Some("   "), &actor, now())
            .await
            .unwrap();
        let nul = suspend(&db.pool, "family", Some("a\0b"), &actor, now()).await;
        let too_long = suspend(&db.pool, "family", Some(&long), &actor, now()).await;
        let bad_name = suspend(&db.pool, "fam\0ily", None, &actor, now()).await;

        assert_eq!(
            repeat.suspension.unwrap().notice.as_deref(),
            Some("Paused.")
        );
        assert_eq!(after_repeat, generation, "a repeat writes nothing");
        assert!(matches!(nul, Err(OkfError::Refused { .. })), "{nul:?}");
        assert!(matches!(too_long, Err(OkfError::Refused { .. })));
        assert!(matches!(bad_name, Err(OkfError::UnknownName { .. })));
        let suspended = entries(&db.pool, "team.suspended").await;
        let details: Vec<_> = suspended.iter().map(|entry| &entry.details).collect();
        assert_eq!(
            details,
            [
                &json!({"notice": true, "already": false}),
                &json!({"notice": true, "already": true}),
                &json!({"notice": false, "already": true}),
            ]
        );
        assert_eq!(suspended[0].target.as_deref(), Some("family"));
        assert_eq!(suspended[0].teams, ["family"]);
        assert!(
            !suspended
                .iter()
                .any(|e| e.details.to_string().contains("aused")),
            "never the notice's text"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unsuspend_records_a_lift_and_nothing_for_an_active_team() {
        let db = test_db().await;
        family(&db.pool).await;
        let actor = AuditActor::cli();
        suspend(&db.pool, "family", None, &actor, now())
            .await
            .unwrap();

        unsuspend(&db.pool, "family", &actor, now()).await.unwrap();
        unsuspend(&db.pool, "family", &actor, now()).await.unwrap();
        let bad_name = unsuspend(&db.pool, "fam\0ily", &actor, now()).await;

        assert!(matches!(bad_name, Err(OkfError::UnknownName { .. })));
        let lifted = entries(&db.pool, "team.unsuspended").await;
        assert_eq!(lifted.len(), 1, "{lifted:?}");
        assert_eq!(lifted[0].target.as_deref(), Some("family"));
        assert_eq!(lifted[0].teams, ["family"]);
        assert_eq!(lifted[0].details, json!({}));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_audit_write_fails_the_team_change() {
        let db = test_db().await;
        family(&db.pool).await;
        crate::http::api::testing::fail_on(&db.pool, "BEFORE INSERT ON audit_entries FOR EACH ROW")
            .await;
        let actor = AuditActor::cli();

        let ops = NewTeam {
            name: "ops",
            display_name: &DisplayName::default(),
            owner: "alice",
        };

        let created = create(&db.pool, &ops, &actor, now()).await;
        let added = set_member(
            &db.pool,
            &membership("family", TEST_USER, Role::Member),
            &actor,
            now(),
        )
        .await;
        let suspended = suspend(&db.pool, "family", None, &actor, now()).await;
        let deleted = delete(&db.pool, "family", &actor, now()).await;

        assert!(created.is_err() && added.is_err() && suspended.is_err() && deleted.is_err());
        assert!(matches!(
            find(&db.pool, "ops").await,
            Err(OkfError::UnknownName { .. })
        ));
        let team = find(&db.pool, "family").await.unwrap();
        assert_eq!(team.members.len(), 1, "the addition rolled back");
        assert_eq!(team.suspension, None, "the suspension rolled back");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_provisioner_cannot_make_an_admin_an_owner_or_change_an_admins_membership() {
        let db = test_db().await;
        family(&db.pool).await;
        seed_user(&db.pool, "staff").await;
        set_member(
            &db.pool,
            &membership("family", "staff", Role::Member),
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();
        make_admin(&db.pool, "staff").await;
        let provisioner = provisioner();
        let owned_by_staff = NewTeam {
            name: "ops",
            display_name: &DisplayName::default(),
            owner: "staff",
        };

        let created = create(&db.pool, &owned_by_staff, &provisioner, now()).await;
        let promoted = set_member(
            &db.pool,
            &membership("family", "staff", Role::Owner),
            &provisioner,
            now(),
        )
        .await;
        let removed = remove_member(&db.pool, "family", "staff", &provisioner, now()).await;
        // A plain user is still the provisioner's to manage.
        let plain = set_member(
            &db.pool,
            &membership("family", TEST_USER, Role::Member),
            &provisioner,
            now(),
        )
        .await;

        for refused in [created.map(|_| ()), promoted.map(|_| ()), removed] {
            assert!(
                matches!(refused, Err(OkfError::Forbidden { .. })),
                "{refused:?}"
            );
        }
        assert_eq!(plain.unwrap(), MemberChange::Added);
        assert!(matches!(
            find(&db.pool, "ops").await,
            Err(OkfError::UnknownName { .. })
        ));
        let members = find(&db.pool, "family").await.unwrap().members;
        assert!(members.contains(&Member {
            user: "staff".to_string(),
            role: Role::Member,
        }));
        assert_eq!(
            entries(&db.pool, "team.created").await.len(),
            1,
            "family only"
        );
        assert_eq!(entries(&db.pool, "team.role_changed").await.len(), 0);
        assert_eq!(entries(&db.pool, "team.member_removed").await.len(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_provisioners_membership_change_waits_for_a_promotion_in_flight() {
        let db = test_db().await;
        family(&db.pool).await;
        seed_user(&db.pool, "staff").await;
        // A promotion in flight: `users::set_admin` holds the row `FOR UPDATE`.
        let mut promoting = db.pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM users WHERE name = 'staff' FOR UPDATE")
            .execute(&mut *promoting)
            .await
            .unwrap();
        sqlx::query("UPDATE users SET is_admin = true WHERE name = 'staff'")
            .execute(&mut *promoting)
            .await
            .unwrap();
        let pool = db.pool.clone();
        let adding = tokio::spawn(async move {
            let staff = membership("family", "staff", Role::Member);
            set_member(&pool, &staff, &provisioner(), now()).await
        });
        tokio::time::timeout(
            LOCK_WAIT,
            crate::store::catalogs::tests::until_a_lock_is_awaited(&db.pool),
        )
        .await
        .expect("the provisioner's change waits on the promotion's row lock");

        promoting.commit().await.unwrap();
        let added = tokio::time::timeout(LOCK_WAIT, adding)
            .await
            .expect("the change finishes once the promotion commits")
            .unwrap();

        // It read the flag after the promotion: refused, not added.
        assert!(
            matches!(added, Err(OkfError::Forbidden { .. })),
            "{added:?}"
        );
        assert_eq!(find(&db.pool, "family").await.unwrap().members.len(), 1);
    }

    /// Which change races a user's deletion (the user's row, then its teams).
    #[derive(Debug, Clone, Copy)]
    enum Recorded {
        /// Suspending alice's personal team: its entry resolves alice.
        SuspendPersonalTeam,
        /// Alice's own session changing a membership of `family`, a team she
        /// is in: its entry resolves her as the actor.
        MembershipBySession,
        /// A catalog created for alice's personal team.
        CatalogForPersonalTeam,
    }

    /// Holds alice's row `FOR UPDATE`, as `users::delete` takes it first,
    /// while `change` runs, then takes `team` `FOR UPDATE`, as the deletion
    /// takes her teams next. A change that locked the team before the user
    /// would deadlock here; one that waits on the user first lets the
    /// deletion through.
    async fn race_with_deleting_alice(change: Recorded) {
        let db = test_db().await;
        family(&db.pool).await;
        let session = session_of(&db.pool, "alice").await;
        let mut deleting = db.pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM users WHERE name = 'alice' FOR UPDATE")
            .execute(&mut *deleting)
            .await
            .unwrap();
        let pool = db.pool.clone();
        let changing = tokio::spawn(async move {
            match change {
                Recorded::SuspendPersonalTeam => {
                    suspend(&pool, "alice", None, &AuditActor::cli(), now())
                        .await
                        .map(|_| ())
                }
                Recorded::MembershipBySession => {
                    let member = membership("family", TEST_USER, Role::Member);
                    set_member(&pool, &member, &session, now())
                        .await
                        .map(|_| ())
                }
                Recorded::CatalogForPersonalTeam => {
                    let new = crate::store::catalogs::NewCatalog {
                        name: "notes",
                        display_name: &DisplayName::default(),
                        description: "",
                        owner_team: "alice",
                    };
                    crate::store::catalogs::create(&pool, &new, &AuditActor::cli(), now())
                        .await
                        .map(|_| ())
                }
            }
        });
        tokio::time::timeout(
            LOCK_WAIT,
            crate::store::catalogs::tests::until_a_lock_is_awaited(&db.pool),
        )
        .await
        .expect("the change waits on alice's row");
        let team = match change {
            Recorded::MembershipBySession => "family",
            Recorded::SuspendPersonalTeam | Recorded::CatalogForPersonalTeam => "alice",
        };

        // A deadlock makes Postgres abort one side; the timeout bounds a hang.
        let team_lock = tokio::time::timeout(
            LOCK_WAIT,
            sqlx::query("SELECT id FROM teams WHERE name = $1 FOR UPDATE")
                .bind(team)
                .execute(&mut *deleting),
        )
        .await
        .expect("the team lock does not hang");
        assert!(team_lock.is_ok(), "{change:?}: {team_lock:?}");
        // The deletion gives up; the change then goes ahead and records.
        deleting.rollback().await.unwrap();
        let changed = tokio::time::timeout(LOCK_WAIT, changing)
            .await
            .expect("the change finishes")
            .unwrap();
        assert!(changed.is_ok(), "{change:?}: {changed:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_recorded_change_locks_the_users_it_names_before_the_team() {
        for change in [
            Recorded::SuspendPersonalTeam,
            Recorded::MembershipBySession,
            Recorded::CatalogForPersonalTeam,
        ] {
            race_with_deleting_alice(change).await;
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_admins_change_to_another_admin_takes_admins_in_set_admins_order() {
        let db = test_db().await;
        family(&db.pool).await;
        seed_user(&db.pool, "staff").await;
        make_admin(&db.pool, "alice").await;
        make_admin(&db.pool, "staff").await;
        // `users::set_admin` locks every admin `FOR UPDATE` in id order, then
        // bumps the generation. Make the acting admin the later id, so a
        // change that locked its actor first would hold the second admin
        // while waiting on the first.
        let (alice, staff) = (
            user_id(&db.pool, "alice").await,
            user_id(&db.pool, "staff").await,
        );
        let (actor, target, first, second) = if alice > staff {
            ("alice", "staff", staff, alice)
        } else {
            ("staff", "alice", alice, staff)
        };
        let session = session_of(&db.pool, actor).await;
        let mut promoting = db.pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM users WHERE id = $1 FOR UPDATE")
            .bind(&first)
            .execute(&mut *promoting)
            .await
            .unwrap();
        let pool = db.pool.clone();
        let changing = tokio::spawn(async move {
            let member = membership("family", target, Role::Owner);
            set_member(&pool, &member, &session, now()).await
        });
        tokio::time::timeout(
            LOCK_WAIT,
            crate::store::catalogs::tests::until_a_lock_is_awaited(&db.pool),
        )
        .await
        .expect("the change waits on the first admin");

        // A deadlock makes Postgres abort one side; the timeout bounds a hang.
        let second_lock = tokio::time::timeout(
            LOCK_WAIT,
            sqlx::query("SELECT id FROM users WHERE id = $1 FOR UPDATE")
                .bind(&second)
                .execute(&mut *promoting),
        )
        .await
        .expect("the second admin's lock does not hang");
        assert!(second_lock.is_ok(), "{second_lock:?}");
        promoting.rollback().await.unwrap();
        let changed = tokio::time::timeout(LOCK_WAIT, changing)
            .await
            .expect("the change finishes")
            .unwrap();
        assert!(changed.is_ok(), "{changed:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_audit_write_fails_a_removal_and_a_lift() {
        let db = test_db().await;
        family(&db.pool).await;
        let actor = AuditActor::cli();
        set_member(
            &db.pool,
            &membership("family", TEST_USER, Role::Member),
            &actor,
            now(),
        )
        .await
        .unwrap();
        suspend(&db.pool, "family", None, &actor, now())
            .await
            .unwrap();
        crate::http::api::testing::fail_on(&db.pool, "BEFORE INSERT ON audit_entries FOR EACH ROW")
            .await;

        let removed = remove_member(&db.pool, "family", TEST_USER, &actor, now()).await;
        let lifted = unsuspend(&db.pool, "family", &actor, now()).await;

        assert!(removed.is_err() && lifted.is_err());
        let team = find(&db.pool, "family").await.unwrap();
        assert_eq!(team.members.len(), 2, "the removal rolled back");
        assert!(team.suspension.is_some(), "the lift rolled back");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_malformed_team_or_user_name_is_unknown_and_records_nothing() {
        let db = test_db().await;
        family(&db.pool).await;
        let actor = AuditActor::cli();
        let before: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_entries")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        let bad = "fam\0ily";
        let owned_by_bad = NewTeam {
            name: "ops",
            display_name: &DisplayName::default(),
            owner: bad,
        };

        let outcomes = [
            create(&db.pool, &owned_by_bad, &actor, now())
                .await
                .map(|_| ()),
            set_member(
                &db.pool,
                &membership(bad, TEST_USER, Role::Member),
                &actor,
                now(),
            )
            .await
            .map(|_| ()),
            set_member(
                &db.pool,
                &membership("family", bad, Role::Member),
                &actor,
                now(),
            )
            .await
            .map(|_| ()),
            remove_member(&db.pool, bad, TEST_USER, &actor, now()).await,
            remove_member(&db.pool, "family", bad, &actor, now()).await,
            delete(&db.pool, bad, &actor, now()).await,
            unsuspend(&db.pool, bad, &actor, now()).await.map(|_| ()),
        ];

        // A NUL would be a database error (a 500) if it were bound.
        for outcome in outcomes {
            assert!(
                matches!(outcome, Err(OkfError::UnknownName { .. })),
                "{outcome:?}"
            );
        }
        let entries: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_entries")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(entries, before, "malformed operations record nothing");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reciprocal_suspensions_of_two_admins_personal_teams_both_succeed() {
        // Hold a stale audit-person row while reciprocal real team writes run.
        // The former upserts wait for this holder; the read-first, SKIP LOCKED
        // resolvers must let both writes commit before it is released.
        let db = test_db().await;
        let [(alice_name, alice), (bob_name, bob)] = users::tests::two_admins(&db.pool).await;
        let held = alice.user_id.clone().unwrap();
        let pool = db.pool.clone();
        let first = async move {
            suspend(&pool, &bob_name, None, &alice, now())
                .await
                .map(|_| ())
        };
        let pool = db.pool.clone();
        let second = async move {
            suspend(&pool, &alice_name, None, &bob, now())
                .await
                .map(|_| ())
        };
        let (by_alice, by_bob) =
            users::tests::both_finish_while_a_person_is_held(&db.pool, &held, first, second).await;

        by_alice.expect("the first reciprocal suspension commits while the audit row is held");
        by_bob.expect("the second reciprocal suspension commits while the audit row is held");
        let entries = audit::testing::entries(&db.pool, "team.suspended").await;
        assert_eq!(entries.len(), 2, "both actual suspensions are recorded");
    }

    // ---- Display names (M17 Task 3) ----

    /// A change of `team`'s display name to `display_name`, checked as
    /// `owner` if given.
    fn rename<'a>(
        team: &'a str,
        display_name: &'a DisplayName,
        owner: Option<&'a str>,
    ) -> TeamDisplayNameChange<'a> {
        TeamDisplayNameChange {
            team,
            display_name,
            owner,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_display_name_changes_it_records_its_length_and_bumps() {
        // Arrange
        let db = test_db().await;
        family(&db.pool).await;
        let name = DisplayName::parse("Famïly").unwrap();
        let cli = AuditActor::cli();
        let before = crate::store::catalogs::admin_generation(&db.pool)
            .await
            .unwrap();

        // Act: set it, then the same text again.
        set_display_name(&db.pool, &rename("family", &name, None), &cli, now())
            .await
            .unwrap();
        let after_change = crate::store::catalogs::admin_generation(&db.pool)
            .await
            .unwrap();
        set_display_name(&db.pool, &rename("family", &name, None), &cli, now())
            .await
            .unwrap();

        // Assert
        assert_eq!(
            find(&db.pool, "family").await.unwrap().display_name,
            "Famïly"
        );
        assert_eq!(after_change, before + 1);
        assert_eq!(
            crate::store::catalogs::admin_generation(&db.pool)
                .await
                .unwrap(),
            after_change,
            "the same text bumps nothing"
        );
        let renamed = entries(&db.pool, "team.display_name_changed").await;
        assert_eq!(
            renamed.len(),
            1,
            "the same text records nothing: {renamed:?}"
        );
        assert_eq!(renamed[0].target.as_deref(), Some("family"));
        assert_eq!(renamed[0].teams, ["family"]);
        // Characters, not bytes; never the text.
        assert_eq!(renamed[0].details, json!({"length": 6}));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_display_name_refuses_a_personal_team_and_an_unknown_one() {
        // Arrange
        let db = test_db().await;
        seed_user(&db.pool, "alice").await;
        let name = DisplayName::parse("x").unwrap();
        let cli = AuditActor::cli();

        // Act
        let personal = set_display_name(&db.pool, &rename("alice", &name, None), &cli, now()).await;
        let unknown = set_display_name(&db.pool, &rename("nope", &name, None), &cli, now()).await;
        let malformed = set_display_name(&db.pool, &rename("a\0b", &name, None), &cli, now()).await;

        // Assert
        assert!(
            matches!(personal, Err(OkfError::Refused { .. })),
            "{personal:?}"
        );
        assert_eq!(find(&db.pool, "alice").await.unwrap().display_name, "");
        for outcome in [unknown, malformed] {
            assert!(
                matches!(outcome, Err(OkfError::UnknownName { .. })),
                "{outcome:?}"
            );
        }
        assert!(entries(&db.pool, "team.display_name_changed")
            .await
            .is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_owner_may_rename_their_team_and_a_member_may_not() {
        // Arrange: alice owns `family`, bob is a member of it.
        let db = test_db().await;
        family(&db.pool).await;
        seed_user(&db.pool, "bob").await;
        set_member(
            &db.pool,
            &membership("family", "bob", Role::Member),
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();
        let (alice, bob) = (
            user_id(&db.pool, "alice").await,
            user_id(&db.pool, "bob").await,
        );
        let name = DisplayName::parse("Acme").unwrap();
        let other = DisplayName::parse("Bob's").unwrap();

        // Act
        let by_owner = set_display_name(
            &db.pool,
            &rename("family", &name, Some(&alice)),
            &session_of(&db.pool, "alice").await,
            now(),
        )
        .await;
        let by_member = set_display_name(
            &db.pool,
            &rename("family", &other, Some(&bob)),
            &session_of(&db.pool, "bob").await,
            now(),
        )
        .await;

        // Assert
        by_owner.expect("an owner may rename their team");
        assert!(
            matches!(by_member, Err(OkfError::Forbidden { .. })),
            "{by_member:?}"
        );
        assert_eq!(find(&db.pool, "family").await.unwrap().display_name, "Acme");
        assert_eq!(
            entries(&db.pool, "team.display_name_changed").await.len(),
            1
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_owner_demoted_while_their_rename_waits_is_refused() {
        // Arrange: alice and bob both own `family`.
        let db = test_db().await;
        family(&db.pool).await;
        seed_user(&db.pool, "bob").await;
        set_member(
            &db.pool,
            &membership("family", "bob", Role::Owner),
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();
        let bob = user_id(&db.pool, "bob").await;
        let bob_session = session_of(&db.pool, "bob").await;
        // Hold `server_state`: bob's demotion takes the team row and his
        // membership, then waits here at its generation bump.
        let mut holder = db.pool.begin().await.unwrap();
        sqlx::query("SELECT 1 FROM server_state FOR UPDATE")
            .execute(&mut *holder)
            .await
            .unwrap();
        let pool = db.pool.clone();
        let demotion = tokio::spawn(async move {
            let demoted = membership("family", "bob", Role::Member);
            set_member(&pool, &demoted, &AuditActor::cli(), now()).await
        });
        tokio::time::timeout(
            LOCK_WAIT,
            crate::store::catalogs::tests::until_locks_are_awaited(&db.pool, 1),
        )
        .await
        .expect("the demotion waits on server_state");
        // Bob's rename then waits on the team row the demotion holds.
        let pool = db.pool.clone();
        let renaming = tokio::spawn(async move {
            let name = DisplayName::parse("Bob's").unwrap();
            set_display_name(
                &pool,
                &rename("family", &name, Some(&bob)),
                &bob_session,
                now(),
            )
            .await
        });
        tokio::time::timeout(
            LOCK_WAIT,
            crate::store::catalogs::tests::until_locks_are_awaited(&db.pool, 2),
        )
        .await
        .expect("the rename waits on the team row");

        // Act
        holder.rollback().await.unwrap();
        let demoted = tokio::time::timeout(LOCK_WAIT, demotion)
            .await
            .expect("the demotion finishes once server_state is free")
            .unwrap();
        let renamed = tokio::time::timeout(LOCK_WAIT, renaming)
            .await
            .expect("the rename finishes once the demotion commits")
            .unwrap();

        // Assert: the rename read the membership after the demotion.
        demoted.expect("the demotion commits");
        assert!(
            matches!(renamed, Err(OkfError::Forbidden { .. })),
            "{renamed:?}"
        );
        assert_eq!(
            find(&db.pool, "family").await.unwrap().display_name,
            "The family"
        );
    }

    /// Issue #642: a stored role that is not a known spelling is a database
    /// error from the list and the role lookup, not a panic.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_corrupt_role_is_a_database_error() {
        let db = test_db().await;
        family(&db.pool).await;
        let (team_id, user_id): (String, String) = sqlx::query_as(
            "SELECT t.id, u.id FROM teams t, users u WHERE t.name = 'family' AND u.name = 'alice'",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        crate::store::drop_check_constraints(&db.pool, "team_members").await;
        sqlx::query("UPDATE team_members SET role = 'bogus-role' WHERE team_id = $1")
            .bind(&team_id)
            .execute(&db.pool)
            .await
            .unwrap();

        let listed = list(&db.pool).await;
        let found = find(&db.pool, "family").await;
        let mut conn = db.pool.acquire().await.unwrap();
        let role = current_role(&mut conn, &team_id, &user_id).await;

        crate::store::assert_decode_error(listed, "team_members.role", "bogus-role");
        crate::store::assert_decode_error(found, "team_members.role", "bogus-role");
        crate::store::assert_decode_error(role, "team_members.role", "bogus-role");
    }
}
