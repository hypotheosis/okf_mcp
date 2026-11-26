//! `/api/v1/unlocks` (spec Part 6, "An unlock"): what `okf-mcp unlock
//! create`, `list` and `end` do, over HTTP, with the same `store::unlocks`
//! functions.
//!
//! Access:
//! - creating needs an admin credential at `admin` on the catalog whose
//!   own user is an admin user (403 otherwise, and for a credential with no
//!   user; issue #447). The unlock is for the caller's own user unless the
//!   body names another, which needs an admin that reaches every catalog.
//!   The store refuses a target user who is not an admin (409);
//! - an admin reads full records (the reason and the unlocked staff
//!   member), a scoped admin only those of the catalogs it administers;
//! - a team's owner, through a console session only and naming the team
//!   (`?team=`), reads that team's catalogs' unlocks redacted (plan
//!   Decisions 14 and 18): the category, the times, the state and the
//!   tallies, by "an operator", never the reason or who it was;
//! - ending needs an admin; a scoped admin ends only the unlocks of the
//!   catalogs it administers (any other id is 404, like an unknown one).
//!   Ending needs no admin user (issue #447): it only takes access away,
//!   so any admin credential that administers the catalog may close it;
//! - a provisioner is refused by the provisioner gate, and anyone else 403.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{delete, get};
use axum::{Extension, Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::app::App;
use crate::auth::{CredentialKind, Principal};
use crate::clock::format_timestamp;
use crate::error::OkfError;
use crate::http::api::{require_admin, require_every_catalog};
use crate::http::AppError;
use crate::store::audit::AuditActor;
use crate::store::keys::Access;
use crate::store::teams;
use crate::store::unlocks::{
    self, Category, NewUnlock, UnlockFilter, UnlockPolicy, UnlockRecord, UnlockState,
};

/// What an owner sees as the unlock's author (spec Part 6).
const OPERATOR: &str = "an operator";

/// This area's routes.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "router builder: runs once at startup, outside any request, so a span would be an orphan root"
    )
)]
pub(crate) fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/unlocks", get(list_unlocks).post(create_unlock))
        .route("/unlocks/{id}", delete(end_unlock))
}

/// An unlock as an admin sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct UnlockView {
    /// Its id.
    pub id: i64,
    /// The catalog's name.
    pub catalog: String,
    /// The catalog's owner team now, if any.
    pub owner_team: Option<String>,
    /// The admin user's name, or `deleted user #N`.
    pub user: String,
    /// Why, as a category.
    pub category: Category,
    /// Why, in words.
    pub reason: String,
    /// When it began.
    pub starts: String,
    /// When it expires.
    pub expires: String,
    /// When it was ended early, if it was.
    pub ended: Option<String>,
    /// `live`, `expired` or `ended`.
    pub state: UnlockState,
    /// Content reads under it.
    pub reads: i64,
    /// Writes under it.
    pub writes: i64,
    /// Exports under it.
    pub exports: i64,
}

impl UnlockView {
    /// The admin's view of a record.
    ///
    /// # Arguments
    /// * `record` - the unlock.
    /// * `now` - the server clock, for its state.
    ///
    /// # Returns
    /// The view.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    fn of(record: UnlockRecord, now: DateTime<Utc>) -> Self {
        Self {
            state: record.state(now),
            starts: format_timestamp(record.created_at),
            expires: format_timestamp(record.expires_at),
            // `map` on an `Option`: format the time only when there is one.
            ended: record.ended_at.map(format_timestamp),
            id: record.id,
            catalog: record.catalog,
            owner_team: record.owner_team,
            // `person`: the live name, else `deleted user #N` (never `None`).
            user: record.person,
            category: record.category,
            reason: record.reason,
            reads: record.reads,
            writes: record.writes,
            exports: record.exports,
        }
    }
}

/// An unlock as an owner of its catalog's team sees it: no reason and no
/// staff member (spec: "an operator opened <catalog>: <category>, <start>
/// to <end>, N reads, M writes").
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct OwnerUnlockView {
    /// Its id.
    pub id: i64,
    /// The catalog's name.
    pub catalog: String,
    /// Why, as a category.
    pub category: Category,
    /// When it began.
    pub starts: String,
    /// When it stops (or stopped): its early end, else its expiry.
    pub ends: String,
    /// `live`, `expired` or `ended`.
    pub state: UnlockState,
    /// Content reads under it.
    pub reads: i64,
    /// Writes under it.
    pub writes: i64,
    /// Exports under it.
    pub exports: i64,
    /// Always "an operator".
    pub by: &'static str,
}

impl OwnerUnlockView {
    /// The owner's view of a record. It takes the record by value and
    /// drops the reason and the person with it, so neither can leak.
    ///
    /// # Arguments
    /// * `record` - the unlock.
    /// * `now` - the server clock, for its state.
    ///
    /// # Returns
    /// The view.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    fn of(record: UnlockRecord, now: DateTime<Utc>) -> Self {
        Self {
            state: record.state(now),
            starts: format_timestamp(record.created_at),
            ends: format_timestamp(record.ends()),
            id: record.id,
            catalog: record.catalog,
            category: record.category,
            reads: record.reads,
            writes: record.writes,
            exports: record.exports,
            by: OPERATOR,
        }
    }
}

/// `GET /unlocks`' answer: the admin's views or the owner's, newest first.
// `untagged`: serialize as whichever variant's list it holds, with no
// wrapper naming the variant.
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum UnlockList {
    /// An admin's.
    Full { unlocks: Vec<UnlockView> },
    /// An owner's.
    Redacted { unlocks: Vec<OwnerUnlockView> },
}

/// `POST /unlocks`' body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateUnlockBody {
    catalog: String,
    category: Category,
    reason: String,
    /// `unlock_max_hours` when omitted.
    hours: Option<u32>,
    /// The caller's own user when omitted.
    user: Option<String>,
}

/// `GET /unlocks`' query string.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListParams {
    catalog: Option<String>,
    team: Option<String>,
    #[serde(default)]
    live: bool,
}

/// Whether the caller administers `catalog`: an admin credential at
/// `admin` there (every catalog for an every-catalog admin).
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
    )
)]
fn administers(principal: &Principal, catalog: &str) -> bool {
    principal.level(catalog) == Some(Access::Admin)
}

/// `POST /unlocks`: unlocks a catalog for an admin user.
///
/// # Returns
/// 201 with the [`UnlockView`].
///
/// # Errors
/// 403 below admin, for a credential with no user or whose user is not an
/// admin user (whether or not the body names a user), or naming another
/// user without reaching every catalog; 404 for a catalog the caller does
/// not administer or that does not exist, or an unknown user; 409 for a
/// target user who is not an admin; 422 for hours over `unlock_max_hours` (or 0), a
/// blank or over-long reason, or a bad body; 500 for a database failure.
#[tracing::instrument(level = "debug", skip_all)]
async fn create_unlock(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    actor: AuditActor,
    Json(body): Json<CreateUnlockBody>,
) -> Result<(StatusCode, Json<UnlockView>), AppError> {
    require_admin(&principal)?;
    // Worded like an unknown catalog, as `Principal::require` refuses one
    // out of reach, so a scoped admin cannot probe which catalogs exist.
    if !administers(&principal, &body.catalog) {
        return Err(OkfError::CatalogNotSelected {
            catalog: body.catalog,
        }
        .into());
    }
    // A caller with no user is refused before any target is chosen: an
    // unlock is made by an admin user (spec Part 6), so an unassigned key
    // may not act for anyone, itself or another user.
    let Some(own) = principal.user.as_deref() else {
        return Err(OkfError::Forbidden {
            reason: "a key with no user cannot be unlocked; give it a user".to_string(),
        }
        .into());
    };
    // Only operator staff make unlocks (spec Part 6, decided 2026-11-15,
    // issue #447): the credential's own user must be an admin user, so the
    // audit log's granting person is always staff. Refused here, from the
    // user cache (as of this request), before any target is chosen;
    // `unlocks::create` checks it again under the user's lock, so a
    // demotion committed meanwhile is refused too.
    let staff = principal
        .facts
        .user
        .as_deref()
        .is_some_and(|user| user.is_admin);
    if !staff {
        return Err(OkfError::Forbidden {
            reason: format!("{own:?} is not an admin user; only operator staff make unlocks"),
        }
        .into());
    }
    let user = match body.user.as_deref() {
        Some(named) if named != own => {
            require_every_catalog(&principal)?;
            named
        }
        _ => own,
    };
    let settings = app.settings();
    let policy = UnlockPolicy {
        max_hours: settings.unlock_max_hours,
        notify_owners: settings.unlock_notify_owners,
        brand_defaults: app.config().auth.default_brand(),
    };
    let new = NewUnlock {
        catalog: &body.catalog,
        user,
        category: body.category,
        reason: &body.reason,
        hours: body.hours.unwrap_or(policy.max_hours),
    };
    let now = app.clock().now();
    let created = unlocks::create(app.accounts().executor(), &new, &policy, &actor, now).await?;
    Ok((StatusCode::CREATED, Json(UnlockView::of(created, now))))
}

/// `GET /unlocks?catalog=&team=&live=true`: unlocks newest first.
///
/// # Errors
/// 403 for a caller that is neither an admin nor, through a console
/// session, an owner of the named team; 400 for an unknown parameter; 500
/// for a database failure.
#[tracing::instrument(level = "debug", skip_all)]
async fn list_unlocks(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    Query(params): Query<ListParams>,
) -> Result<Json<UnlockList>, AppError> {
    let now = app.clock().now();
    let filter = UnlockFilter {
        catalog: params.catalog.as_deref(),
        team: params.team.as_deref(),
        live_at: params.live.then_some(now),
    };
    if require_admin(&principal).is_ok() {
        let unlocks = unlocks::list(app.accounts().executor(), &filter)
            .await?
            .into_iter()
            .filter(|record| administers(&principal, &record.catalog))
            .map(|record| UnlockView::of(record, now))
            .collect();
        return Ok(Json(UnlockList::Full { unlocks }));
    }
    let refused = || OkfError::Forbidden {
        reason: "unlocks need an admin, or a team owner's console session naming the team \
                 (?team=)"
            .to_string(),
    };
    // `let ... else` binds both or returns: a session and a named team.
    let (CredentialKind::Session { user_id }, Some(team)) = (&principal.kind, filter.team) else {
        return Err(refused().into());
    };
    // `owned_by` answers `None` for a malformed name without SQL. The
    // filter then matches the catalogs that team owns now.
    teams::owned_by(app.accounts().executor(), user_id, team)
        .await?
        .ok_or_else(refused)?;
    let unlocks = unlocks::list(app.accounts().executor(), &filter)
        .await?
        .into_iter()
        .map(|record| OwnerUnlockView::of(record, now))
        .collect();
    Ok(Json(UnlockList::Redacted { unlocks }))
}

/// `DELETE /unlocks/{id}`: ends a live unlock early.
///
/// # Returns
/// 200 with the ended [`UnlockView`].
///
/// # Errors
/// 403 below admin; 404 for an unknown id or one of a catalog the caller
/// does not administer; 409 for one already ended or expired; 500 for a
/// database failure.
#[tracing::instrument(level = "debug", skip_all)]
async fn end_unlock(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    actor: AuditActor,
    Path(id): Path<i64>,
) -> Result<Json<UnlockView>, AppError> {
    require_admin(&principal)?;
    // An unlock's catalog never changes, so checking it before `end`
    // cannot go stale.
    let found = unlocks::find(app.accounts().executor(), id).await?;
    if !administers(&principal, &found.catalog) {
        return Err(OkfError::NotFound {
            path: format!("unlocks/{id}"),
        }
        .into());
    }
    let now = app.clock().now();
    let ended = unlocks::end(app.accounts().executor(), id, &actor, now).await?;
    Ok(Json(UnlockView::of(ended, now)))
}

#[cfg(test)]
mod tests;
