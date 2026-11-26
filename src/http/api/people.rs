//! `/api/v1/users`, `/api/v1/teams` and `/api/v1/unassigned` (users and
//! teams design, "REST"): what `okf-mcp user ...`, `team ...` and
//! `unassigned` do, over HTTP, with the same `store::users`, `store::teams`
//! and `store::unassigned` functions.
//!
//! Access (plan Decision 3, milestone 14 provisioners):
//! - the provisioner allow-list permits non-admin account mutations and
//!   team management, user lookup by email, user/team detail and the team
//!   list (issue #502). It never lists all users or shows a user's keys and
//!   sign-in codes;
//! - all other routes keep the admin rules below;
//! - creating, deleting and changing users, teams and memberships, and
//!   listing what is unassigned, need an admin key that reaches every
//!   catalog: users and teams are not catalog-scoped; so do suspending a
//!   team and lifting its suspension (`PUT /teams/{name}/suspension`,
//!   milestone 13);
//! - listing and reading users and teams needs any admin key, so a scoped
//!   admin can pick the user of a key it creates; `GET /users?email=`
//!   (milestone 14) narrows the list to the user with that email, in any
//!   letter case (zero or one), under the same rule;
//! - a user's detail shows only the keys, grants and codes the caller
//!   manages (`api::keys`' rule), so it reveals no credential outside a
//!   scoped admin's reach;
//! - a user's sign-in controls (M12: admin flag, email, setup email,
//!   entitlements, ending sessions) are writes like the rest; listing a
//!   user's sessions and sign-ins is a read. The API never clears the last
//!   admin user (plan Decision 16): only the CLI, with operator access, may.
//!
//! An admin console session reaches every catalog, so it passes the same
//! checks as an every-catalog admin key; a non-admin session is below
//! admin. No answer or log line holds a session id, token or hash: a
//! session is shown by its id prefix only.
//!
//! Every write sends its change messages (`invalidate`); this replica applies
//! them before the answer goes out (`invalidate::capturing`), so the next
//! request sees the change, and the change feed carries them to the others.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post, put};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};

use crate::app::App;
use crate::auth::{CredentialKind, Principal};
use crate::clock::format_timestamp;
use crate::error::OkfError;
use crate::http::api::keys::{manages, manages_grants, parse_access, KeyView};
use crate::http::api::me::{SessionList, SessionView, SignInList, SignInView, RECENT_SIGN_INS};
use crate::http::api::oauth::SignInCodeView;
use crate::http::api::{
    owner_session, require_admin, require_every_catalog, require_manager, require_reader,
};
use crate::http::requester::Requester;
use crate::http::AppError;
use crate::labels::directory::{Labels, Wanted};
use crate::labels::{team_label, user_label, DisplayName};
use crate::mail::invite;
use crate::store::audit::{AuditActor, ProvisionerEvent, ProvisionerUse};
use crate::store::ceiling::{self, Entitlement, EntitlementRef, NewEntitlement};
use crate::store::keys::{self as key_rows, Access};
use crate::store::oauth::codes;
use crate::store::teams::{self, MemberChange, Membership, NewTeam, Role, TeamRecord};
use crate::store::unassigned;
use crate::store::users::{self, AdminChange, NewUser, UserRecord};
use crate::store::{sign_in_events, user_sessions};
use crate::telemetry::subscriber::split_event;

/// One user as the API shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct UserView {
    /// Its name (`[a-z0-9-]+`), which its personal team shares.
    pub name: String,
    /// Free text, possibly empty.
    pub display_name: String,
    /// What people are shown for it: its display name, else its email's
    /// local part (see [`user_label`]).
    pub label: String,
    /// When it was created (`2026-11-11T12:00:00Z`).
    pub created_at: String,
    /// How many keys, grants (live and revoked) and sign-in codes name it,
    /// whoever manages them.
    pub credentials: i64,
    /// Its teams' names, sorted; the personal team included.
    pub teams: Vec<String>,
    /// Its email address, if set.
    pub email: Option<String>,
    /// Whether its console session acts as an admin.
    pub is_admin: bool,
    /// Whether it has a local password.
    pub has_password: bool,
}

impl From<UserRecord> for UserView {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    fn from(record: UserRecord) -> Self {
        // Destructuring names every field, so one added to the record later
        // fails to compile here until someone decides whether to show it.
        let UserRecord {
            id: _,
            name,
            display_name,
            created_at,
            credentials,
            teams,
            email,
            is_admin,
            has_password,
        } = record;
        Self {
            name,
            label: user_label(&display_name, email.as_deref()),
            display_name,
            created_at: format_timestamp(created_at),
            credentials,
            teams,
            email,
            is_admin,
            has_password,
        }
    }
}

/// `GET /users`' query.
#[derive(Debug, Default, Deserialize)]
// An unknown parameter (`?mail=`, a typo) is a 400, never the whole list.
#[serde(deny_unknown_fields)]
struct UserQuery {
    /// Only the user with this email address (any letter case).
    email: Option<String>,
}

/// `DELETE /users/{name}`'s query.
#[derive(Debug, Default, Deserialize)]
// An unknown parameter (a typo of the precondition) is a 400, never a
// deletion that skipped the check.
#[serde(deny_unknown_fields)]
struct DeleteUserQuery {
    /// Refuses (409) while the user is a member of any shared team.
    #[serde(default)]
    unless_shared_member: bool,
}

/// `GET /users`' answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct UserList {
    /// Every user, by name.
    pub users: Vec<UserView>,
}

/// `GET /users/{name}`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct UserDetail {
    /// The user.
    pub user: UserView,
    /// Its keys and grants that the caller manages, revoked ones included.
    pub keys: Vec<KeyView>,
    /// Its sign-in codes that the caller manages, in every state.
    pub codes: Vec<SignInCodeView>,
    /// The catalogs it may reach beyond its teams', by catalog.
    pub entitlements: Vec<EntitlementView>,
}

/// One entitlement: a catalog given to a user beyond its teams' catalogs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct EntitlementView {
    /// The catalog.
    pub catalog: String,
    /// `read` or `write`.
    pub access: &'static str,
    /// When it was first given.
    pub created_at: String,
}

impl From<Entitlement> for EntitlementView {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    fn from(entitlement: Entitlement) -> Self {
        let Entitlement {
            catalog,
            access,
            created_at,
        } = entitlement;
        Self {
            catalog,
            access: access.as_str(),
            created_at: format_timestamp(created_at),
        }
    }
}

/// The entitlement routes' answer: the user's entitlements as they now are.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Entitlements {
    /// The user.
    pub name: String,
    /// Its entitlements, by catalog.
    pub entitlements: Vec<EntitlementView>,
}

/// `PUT /users/{name}/admin`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct AdminChanged {
    /// The user.
    pub name: String,
    /// The flag before.
    pub previous: bool,
    /// The flag now.
    pub admin: bool,
}

/// `POST /users/{name}/invite`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Invited {
    /// The user.
    pub name: String,
    /// Where the setup email goes.
    pub email: String,
}

/// `DELETE /users/{name}/sessions`' answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct SessionsRevoked {
    /// The user.
    pub name: String,
    /// How many sessions ended.
    pub revoked: u64,
}

/// One member of a team.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct MemberView {
    /// The user's name.
    pub user: String,
    /// `owner` or `member`.
    pub role: &'static str,
    /// The user's label, as `GET /labels` gives it.
    pub label: String,
}

/// One team as the API shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct TeamView {
    /// Its name.
    pub name: String,
    /// Free text, possibly empty.
    pub display_name: String,
    /// What people are shown for it: "{user label}'s team" for a personal
    /// team, else its display name (see [`team_label`]).
    pub label: String,
    /// The user whose personal team this is (`None` for a shared team).
    pub personal_of: Option<String>,
    /// When it was created.
    pub created_at: String,
    /// Its members, by user name.
    pub members: Vec<MemberView>,
    /// The catalogs it owns, sorted.
    pub catalogs: Vec<String>,
    /// When it was suspended; null while active.
    pub suspended_at: Option<String>,
    /// The notice its members see; null without one (or while active).
    pub suspension_notice: Option<String>,
}

impl TeamView {
    /// The views of team records, with the labels of every user they show
    /// read together (members and personal owners).
    ///
    /// # Arguments
    /// * `app` - the application, for its label lookup.
    /// * `records` - the teams.
    ///
    /// # Returns
    /// One view per record, in order.
    ///
    /// # Errors
    /// [`OkfError::Database`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) async fn many(app: &App, records: Vec<TeamRecord>) -> Result<Vec<Self>, OkfError> {
        let mut wanted = Wanted::default();
        for record in &records {
            if let Some(user) = record.personal_of.as_deref() {
                wanted.user(user);
            }
            for member in &record.members {
                wanted.user(&member.user);
            }
        }
        let labels = app.labels().labels_for(&wanted).await?;
        Ok(records
            .into_iter()
            .map(|record| Self::of(record, &labels))
            .collect())
    }

    /// [`TeamView::many`] for one record.
    ///
    /// # Arguments
    /// * `app` - the application, for its label lookup.
    /// * `record` - the team.
    ///
    /// # Returns
    /// Its view.
    ///
    /// # Errors
    /// [`OkfError::Database`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) async fn one(app: &App, record: TeamRecord) -> Result<Self, OkfError> {
        let mut views = Self::many(app, vec![record]).await?;
        Ok(views.remove(0))
    }

    /// The view of a team record.
    ///
    /// The record cannot see the user a personal team is named after, so
    /// the labels come from the lookup ([`TeamView::many`] reads them).
    ///
    /// # Arguments
    /// * `record` - the team.
    /// * `labels` - the labels of the users the record shows.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    fn of(record: TeamRecord, labels: &Labels) -> Self {
        let TeamRecord {
            name,
            display_name,
            personal_of,
            created_at,
            members,
            catalogs,
            suspension,
        } = record;
        Self {
            name,
            label: team_label(
                &display_name,
                personal_of.as_deref().and_then(|user| labels.user(user)),
            ),
            display_name,
            personal_of,
            created_at: format_timestamp(created_at),
            members: members
                .into_iter()
                .map(|member| MemberView {
                    // A user created since the last load has no label yet:
                    // show the slug rather than nothing.
                    label: labels
                        .user(&member.user)
                        .unwrap_or(&member.user)
                        .to_string(),
                    user: member.user,
                    role: member.role.as_str(),
                })
                .collect(),
            catalogs,
            // `as_ref` borrows inside the `Option`, so the second field can
            // still take the notice out of it by value.
            suspended_at: suspension
                .as_ref()
                .map(|suspended| format_timestamp(suspended.since)),
            suspension_notice: suspension.and_then(|suspended| suspended.notice),
        }
    }
}

/// `GET /teams`' answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct TeamList {
    /// Every team, by name.
    pub teams: Vec<TeamView>,
}

/// `PUT /teams/{name}/members/{user}`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct MembershipChanged {
    /// `added`, `role_changed` or `unchanged`.
    pub change: &'static str,
    /// The team as it now is.
    pub team: TeamView,
}

/// `GET /unassigned`'s answer: what predates users and teams and still has
/// neither.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct UnassignedView {
    /// Keys and grants with no user, live and revoked.
    pub keys: Vec<KeyView>,
    /// Sign-in codes with no user.
    pub codes: Vec<SignInCodeView>,
    /// Names of the catalogs with no owner team.
    pub catalogs: Vec<String>,
}

/// `POST /users`' body. `deny_unknown_fields` turns a misspelt field into a
/// 422 instead of silently ignoring it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateUserBody {
    /// `[a-z0-9-]+`, not taken by a user or team.
    name: String,
    /// Its display name, checked by [`DisplayName::parse`]; empty when left
    /// out.
    #[serde(default)]
    display_name: String,
}

/// `POST /teams`' body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateTeamBody {
    /// `[a-z0-9-]+`, not taken by a user or team.
    name: String,
    /// Its display name, checked by [`DisplayName::parse`]; empty when left
    /// out.
    #[serde(default)]
    display_name: String,
    /// The user who is its first owner (required; must exist).
    owner: String,
}

/// `PUT /teams/{name}/suspension`'s body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SuspensionBody {
    /// `true` suspends (or replaces the notice); `false` lifts.
    suspended: bool,
    /// What members are told; blank or left out for none; ignored when
    /// lifting.
    #[serde(default)]
    notice: Option<String>,
}

/// `PUT /teams/{name}/members/{user}`'s body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MemberBody {
    /// `owner` or `member`; anything else is a 422.
    role: Role,
}

/// `PUT /users/{name}/admin`'s body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AdminBody {
    /// The flag to set.
    admin: bool,
}

/// `PUT /users/{name}/email`'s body. `null` clears the email; the field
/// itself is required, so an empty body is a 422, not a silent clear.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmailBody {
    /// The address, or `null`.
    // serde treats an `Option` field as optional unless told otherwise;
    // `deserialize_with` a plain `Option` deserializer makes the key
    // required while still taking `null`.
    #[serde(deserialize_with = "Option::deserialize")]
    email: Option<String>,
}

/// `PUT /users/{name}/entitlements/{catalog}`'s body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EntitlementBody {
    /// `read` or `write`.
    access: String,
}

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
        .route("/users", get(list_users).post(create_user))
        .route(
            "/users/{name}",
            get(user_detail).patch(update_user).delete(delete_user),
        )
        .route("/users/{name}/admin", put(set_admin))
        .route("/users/{name}/email", put(set_email))
        .route("/users/{name}/invite", post(invite_user))
        .route(
            "/users/{name}/entitlements/{catalog}",
            put(entitle).delete(unentitle),
        )
        .route(
            "/users/{name}/sessions",
            get(list_sessions).delete(revoke_sessions),
        )
        .route("/users/{name}/sessions/{id_prefix}", delete(revoke_session))
        .route("/users/{name}/sign-ins", get(list_sign_ins))
        .route("/teams", get(list_teams).post(create_team))
        .route(
            "/teams/{name}",
            get(team_detail).patch(update_team).delete(delete_team),
        )
        .route(
            "/teams/{name}/members/{user}",
            put(set_member).delete(remove_member),
        )
        .route("/teams/{name}/suspension", put(set_suspension))
        .route("/unassigned", get(list_unassigned))
}

/// The wire spelling of a [`MemberChange`].
#[tracing::instrument(level = "trace", skip_all)]
fn change_name(change: MemberChange) -> &'static str {
    match change {
        MemberChange::Added => "added",
        MemberChange::RoleChanged => "role_changed",
        MemberChange::Unchanged => "unchanged",
    }
}

/// `GET /users`: every user, or with `?email=` only the user with that
/// email address (any letter case): zero or one, in the same view.
///
/// # Returns
/// 200 with the users, by name.
///
/// # Errors
/// 400 for an unknown query parameter; 403 below admin; 500 for a database
/// failure.
#[tracing::instrument(level = "debug", skip_all)]
async fn list_users(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    requester: Requester,
    Query(query): Query<UserQuery>,
) -> Result<Json<UserList>, AppError> {
    require_reader(&principal)?;
    // `Option::into_iter` yields the one user or nothing, so both arms
    // collect into the same `Vec<UserRecord>`.
    if principal.provisioner && query.email.is_none() {
        return Err(OkfError::Forbidden {
            reason: "a provisioner looks users up by email".to_string(),
        }
        .into());
    }
    let looked_up = query.email.is_some();
    let records = match query.email {
        Some(email) => users::find_record_by_email(app.accounts().executor(), &email)
            .await?
            .into_iter()
            .collect(),
        None => users::list(app.accounts().executor()).await?,
    };
    // An admin user is left out for a provisioner, so it counts as not found.
    let records: Vec<UserRecord> = records
        .into_iter()
        .filter(|user| !(principal.provisioner && user.is_admin))
        .collect();
    if principal.provisioner && looked_up {
        // The outcome and the matched user, never the address: the entry
        // is recorded in the background, from what this answer shows.
        app.note_provisioner_use(ProvisionerUse {
            actor: principal.admitted_actor(&requester, app.clock().now()),
            event: ProvisionerEvent::Lookup {
                matched: records.first().map(|user| user.id.clone()),
            },
        });
    }
    let users = records.into_iter().map(UserView::from).collect();
    Ok(Json(UserList { users }))
}

/// `POST /users`: creates a user and its personal team.
///
/// # Returns
/// 201 with the new user.
///
/// # Errors
/// 403 unless the key is an admin reaching every catalog; 409 for an invalid
/// or taken name; 422 for a body without `name` or a refused `display_name`
/// (over 80 characters, or holding a control character or line break); 500
/// for a database failure.
#[tracing::instrument(level = "debug", skip_all)]
async fn create_user(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    actor: AuditActor,
    Json(body): Json<CreateUserBody>,
) -> Result<(StatusCode, Json<UserView>), AppError> {
    require_manager(&principal)?;
    // Validated before the store sees it: a refused name is a 422 that
    // writes nothing.
    let display_name = DisplayName::parse(&body.display_name)?;
    let new = NewUser {
        name: &body.name,
        display_name: &display_name,
    };
    let created = users::create(app.accounts().executor(), &new, &actor, app.clock().now()).await?;
    Ok((StatusCode::CREATED, Json(created.into())))
}

/// `GET /users/{name}`: the user, with the keys, grants and codes naming it
/// that the caller manages.
///
/// # Errors
/// 403 below admin; 404 for an unknown user; 500 for a database failure.
#[tracing::instrument(level = "debug", skip_all)]
async fn user_detail(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    Path(name): Path<String>,
    actor: AuditActor,
) -> Result<Json<UserDetail>, AppError> {
    require_reader(&principal)?;
    if principal.provisioner {
        let mut tx = app
            .accounts()
            .executor()
            .begin()
            .await
            .map_err(OkfError::from)?;
        let (_, is_admin) = crate::store::audit::lock_people(
            &mut tx,
            &actor,
            &crate::store::audit::People {
                target: Some((&name, crate::store::audit::RowLock::KeyShare)),
                ..Default::default()
            },
        )
        .await?
        .expect("a named target is returned");
        crate::store::audit::refuse_admin_target(&actor, is_admin, &name)?;
        let user = users::find(&mut *tx, &name).await?;
        let entitlements = ceiling::entitlements_in(&mut tx, &name)
            .await?
            .into_iter()
            .map(EntitlementView::from)
            .collect();
        tx.commit().await.map_err(OkfError::from)?;
        return Ok(Json(UserDetail {
            user: user.into(),
            keys: Vec::new(),
            codes: Vec::new(),
            entitlements,
        }));
    }
    let user = users::find(app.accounts().executor(), &name).await?;
    // `as_deref` turns the record's `Option<String>` into an `Option<&str>`,
    // comparable with `Some(name)` without allocating.
    let named = |owner: Option<&str>| owner == Some(name.as_str());
    let reaching = crate::http::api::keys::reach_names(&principal);
    let keys = key_rows::list_filtered(
        app.accounts().executor(),
        &key_rows::KeyFilter {
            user_name: Some(&name),
            reaching: reaching.as_deref(),
            ..Default::default()
        },
    )
    .await?
    .into_iter()
    .filter(|key| !principal.provisioner && named(key.user.as_deref()) && manages(&principal, key))
    .map(KeyView::from)
    .collect();
    let now = app.clock().now();
    let codes = codes::list(app.accounts().executor())
        .await?
        .into_iter()
        .filter(|code| {
            !principal.provisioner
                && named(code.user.as_deref())
                && manages_grants(&principal, code.all_catalogs, &code.catalogs)
        })
        .map(|code| SignInCodeView::new(code, now))
        .collect();
    Ok(Json(UserDetail {
        user: user.into(),
        keys,
        codes,
        entitlements: entitlement_views(&app, &name).await?,
    }))
}

/// The user's entitlements as the API shows them.
///
/// # Errors
/// [`OkfError::UnknownName`]; [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all)]
async fn entitlement_views(app: &App, name: &str) -> Result<Vec<EntitlementView>, OkfError> {
    Ok(ceiling::entitlements(app.accounts().executor(), name)
        .await?
        .into_iter()
        .map(EntitlementView::from)
        .collect())
}

/// `PUT /users/{name}/admin`: turns the user's admin flag on or off. A
/// change ends the user's console sessions (Decision 20). The API never
/// clears the last admin user (Decision 16): `users::set_admin` counts the
/// admins under their row locks, so two concurrent clears of the last two
/// cannot both pass.
///
/// # Errors
/// 403 unless the caller is an admin reaching every catalog; 404 for an
/// unknown user; 409 for clearing the last admin; 422 without `admin`.
#[tracing::instrument(level = "debug", skip_all)]
async fn set_admin(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    actor: AuditActor,
    Path(name): Path<String>,
    Json(body): Json<AdminBody>,
) -> Result<Json<AdminChanged>, AppError> {
    require_every_catalog(&principal)?;
    let change = AdminChange {
        on: body.admin,
        allow_last: false,
    };
    let previous = users::set_admin(
        app.accounts().executor(),
        &name,
        change,
        &actor,
        app.clock().now(),
    )
    .await?;
    split_event!(
        info,
        { previous, admin = body.admin, },
        { user = %name, key = %principal.key_id, actor = %principal.actor, },
        "admin flag set"
    );
    Ok(Json(AdminChanged {
        name,
        previous,
        admin: body.admin,
    }))
}

/// `PUT /users/{name}/email`: sets or (with `null`) clears the email.
///
/// # Errors
/// 403 unless the caller is an admin reaching every catalog; 404 for an
/// unknown user; 409 for an email another user has; 422 for an address
/// that is not one, or without `email`.
#[tracing::instrument(level = "debug", skip_all)]
async fn set_email(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    actor: AuditActor,
    Path(name): Path<String>,
    Json(body): Json<EmailBody>,
) -> Result<Json<UserView>, AppError> {
    require_manager(&principal)?;
    let now = app.clock().now();
    users::set_email(
        app.accounts().executor(),
        &name,
        body.email.as_deref(),
        &actor,
        now,
    )
    .await?;
    // The address itself is not logged.
    split_event!(
        info,
        { cleared = body.email.is_none(), },
        { user = %name, key = %principal.key_id, actor = %principal.actor, },
        "email set"
    );
    Ok(Json(
        users::find(app.accounts().executor(), &name).await?.into(),
    ))
}

/// `POST /users/{name}/invite`: queues a setup email with a single-use link.
///
/// # Returns
/// 202 with the address: the mail is queued, not yet sent.
///
/// # Errors
/// 403 unless the caller is an admin reaching every catalog; 404 for an
/// unknown user; 409 without an email or without `public_url`.
#[tracing::instrument(level = "debug", skip_all)]
async fn invite_user(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    actor: AuditActor,
    Path(name): Path<String>,
) -> Result<(StatusCode, Json<Invited>), AppError> {
    require_manager(&principal)?;
    let now = app.clock().now();
    let email =
        invite::send_setup(app.accounts().executor(), app.config(), &name, &actor, now).await?;
    split_event!(
        info,
        {},
        { user = %name, key = %principal.key_id, actor = %principal.actor, },
        "setup email requested"
    );
    Ok((StatusCode::ACCEPTED, Json(Invited { name, email })))
}

/// The entitlement routes' caller: an every-catalog admin (`None`, the
/// answers as they have always been), or an owner of the catalog's shared
/// team signed in to the console (`Some(users.id)`, issue #352), which the
/// store checks under its locks. Keys and OAuth grants, an owner's
/// included, get the admin refusal.
///
/// # Errors
/// [`OkfError::Forbidden`] for anyone else.
#[tracing::instrument(level = "debug", skip_all)]
fn entitlement_caller(principal: &Principal) -> Result<Option<&str>, OkfError> {
    owner_session(principal, require_every_catalog(principal))
}

/// What an entitlement route answers: an admin gets the user's entitlements
/// on every catalog, as before; an owner only the one on the catalog it
/// acted on, so nothing about the user's other catalogs leaks across
/// tenants.
///
/// # Errors
/// [`OkfError::Database`], or an unknown user (a race with its deletion).
#[tracing::instrument(level = "debug", skip_all)]
async fn entitlements_answer(
    app: &App,
    name: String,
    catalog: &str,
    owner: Option<&str>,
) -> Result<Json<Entitlements>, OkfError> {
    let mut entitlements = entitlement_views(app, &name).await?;
    if owner.is_some() {
        entitlements.retain(|entitlement| entitlement.catalog == catalog);
    }
    Ok(Json(Entitlements { name, entitlements }))
}

/// `PUT /users/{name}/entitlements/{catalog}`: lets the user reach the
/// catalog at `access`, beyond its teams' catalogs. An every-catalog admin
/// may entitle any user; an owner of the catalog's shared team, signed in to
/// the console, only a member of that team, at `read` or `write`, and gets
/// only that catalog's entitlement back (issue #352).
///
/// # Errors
/// 403 unless the caller is an admin reaching every catalog, or such an
/// owner; for an owner, 403 with one body for an unknown user and a user
/// outside the team alike; 400 for an access other than `read` or `write`;
/// 404 for an unknown user or catalog asked by an admin; 422 without
/// `access`.
#[tracing::instrument(level = "debug", skip_all)]
async fn entitle(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    actor: AuditActor,
    Path((name, catalog)): Path<(String, String)>,
    Json(body): Json<EntitlementBody>,
) -> Result<Json<Entitlements>, AppError> {
    let owner = entitlement_caller(&principal)?;
    let access = parse_access(&body.access)?;
    if access == Access::Admin {
        return Err(OkfError::InvalidPath {
            path: "access".to_string(),
            reason: "an entitlement is read or write".to_string(),
        }
        .into());
    }
    let new = NewEntitlement {
        user: &name,
        catalog: &catalog,
        access,
    };
    match owner {
        None => {
            ceiling::entitle(app.accounts().executor(), &new, &actor, app.clock().now()).await?
        }
        Some(owner) => {
            ceiling::entitle_as_owner(
                app.accounts().executor(),
                owner,
                &new,
                &actor,
                app.clock().now(),
            )
            .await?
        }
    }
    split_event!(
        info,
        { %catalog, access = access.as_str(), },
        { user = %name, key = %principal.key_id, actor = %principal.actor, },
        "entitlement set"
    );
    Ok(entitlements_answer(&app, name, &catalog, owner).await?)
}

/// `DELETE /users/{name}/entitlements/{catalog}`: removes an entitlement
/// (none given: no change), with the callers, refusals and answer of
/// `PUT`.
///
/// # Errors
/// 403 unless the caller is an admin reaching every catalog, or such an
/// owner (one body for an unknown user and a user outside the team); 404 for
/// an unknown user or catalog asked by an admin.
#[tracing::instrument(level = "debug", skip_all)]
async fn unentitle(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    actor: AuditActor,
    Path((name, catalog)): Path<(String, String)>,
) -> Result<Json<Entitlements>, AppError> {
    let owner = entitlement_caller(&principal)?;
    let now = app.clock().now();
    match owner {
        None => ceiling::unentitle(app.accounts().executor(), &name, &catalog, &actor, now).await?,
        Some(owner) => {
            let target = EntitlementRef {
                user: &name,
                catalog: &catalog,
            };
            ceiling::unentitle_as_owner(app.accounts().executor(), owner, &target, &actor, now)
                .await?
        }
    }
    split_event!(
        info,
        { %catalog, },
        { user = %name, key = %principal.key_id, actor = %principal.actor, },
        "entitlement removed"
    );
    Ok(entitlements_answer(&app, name, &catalog, owner).await?)
}

/// `GET /users/{name}/sessions`: the user's console sessions, by id prefix
/// (none is marked current: the caller is not that browser).
///
/// # Errors
/// 403 below admin; 404 for an unknown user.
#[tracing::instrument(level = "debug", skip_all)]
async fn list_sessions(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    Path(name): Path<String>,
) -> Result<Json<SessionList>, AppError> {
    require_admin(&principal)?;
    let id = users::id_by_name(app.accounts().executor(), &name).await?;
    let sessions = user_sessions::list(app.accounts().executor(), &id, None)
        .await?
        .into_iter()
        .map(SessionView::from)
        .collect();
    Ok(Json(SessionList { sessions }))
}

/// `DELETE /users/{name}/sessions`: signs the user out of every console
/// session. Keys and OAuth grants are untouched.
///
/// # Errors
/// 403 unless the caller is an admin reaching every catalog; 404 for an
/// unknown user.
#[tracing::instrument(level = "debug", skip_all)]
async fn revoke_sessions(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    actor: AuditActor,
    Path(name): Path<String>,
) -> Result<Json<SessionsRevoked>, AppError> {
    require_every_catalog(&principal)?;
    let revoked =
        users::end_sessions(app.accounts().executor(), &name, &actor, app.clock().now()).await?;
    split_event!(
        info,
        { revoked, },
        { user = %name, key = %principal.key_id, actor = %principal.actor, },
        "sessions ended by an admin"
    );
    Ok(Json(SessionsRevoked { name, revoked }))
}

/// `DELETE /users/{name}/sessions/{id_prefix}`: ends one of the user's
/// sessions (the console's per-row revoke).
///
/// # Returns
/// 204.
///
/// # Errors
/// 403 unless the caller is an admin reaching every catalog; 404 for an
/// unknown user, or a malformed prefix or one that matches none of the
/// user's sessions; 409 for a prefix that matches several (none is ended).
#[tracing::instrument(level = "debug", skip_all)]
async fn revoke_session(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    actor: AuditActor,
    Path((name, id_prefix)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    require_every_catalog(&principal)?;
    let now = app.clock().now();
    users::end_session_by_prefix(app.accounts().executor(), &name, &id_prefix, &actor, now).await?;
    split_event!(
        info,
        {},
        { user = %name, key = %principal.key_id, actor = %principal.actor, },
        "session ended by an admin"
    );
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /users/{name}/sign-ins`: the user's recent sign-in attempts,
/// newest first.
///
/// # Errors
/// 403 below admin; 404 for an unknown user.
#[tracing::instrument(level = "debug", skip_all)]
async fn list_sign_ins(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    Path(name): Path<String>,
) -> Result<Json<SignInList>, AppError> {
    require_admin(&principal)?;
    let sign_ins = sign_in_events::recent(app.accounts().executor(), &name, RECENT_SIGN_INS)
        .await?
        .into_iter()
        .map(SignInView::from)
        .collect();
    Ok(Json(SignInList { sign_ins }))
}

/// `DELETE /users/{name}`: deletes a user with its keys, grants, sign-in
/// codes, sessions, memberships and personal team; its open changesets are
/// discarded. Catalogs and their history stay (`users::delete`). With
/// `?unless_shared_member=true` it deletes nothing while the user is a member
/// of a shared team, decided under the delete's own locks (issue #535).
///
/// # Returns
/// 204.
///
/// # Errors
/// 400 for an unknown query parameter or a non-boolean value; 403 unless
/// the key is an admin reaching every catalog; 404 for an unknown user; 409
/// for a shared-team membership (with the parameter), a personal team that
/// owns catalogs, the last owner of a shared team, or the last admin user;
/// 500 for a database failure.
#[tracing::instrument(level = "debug", skip_all)]
async fn delete_user(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    actor: AuditActor,
    Path(name): Path<String>,
    Query(query): Query<DeleteUserQuery>,
) -> Result<StatusCode, AppError> {
    require_manager(&principal)?;
    // `allow_last_admin` stays off: the API never deletes the last admin
    // user (the CLI may).
    let options = users::DeleteOptions {
        unless_shared_member: query.unless_shared_member,
        ..users::DeleteOptions::default()
    };
    users::delete_on(
        app.accounts().executor(),
        &name,
        options,
        &actor,
        app.clock().now(),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /teams`: every team. A provisioner may list them too (issue #502),
/// so it can reconcile its own records; the view holds no catalog content.
///
/// # Errors
/// 403 below admin, unless the key is a provisioner; 500 for a database
/// failure.
#[tracing::instrument(level = "debug", skip_all)]
async fn list_teams(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
) -> Result<Json<TeamList>, AppError> {
    require_reader(&principal)?;
    let teams = TeamView::many(&app, teams::list(app.accounts().executor()).await?).await?;
    Ok(Json(TeamList { teams }))
}

/// `POST /teams`: creates a shared team with `owner` as its first owner.
///
/// # Returns
/// 201 with the new team.
///
/// # Errors
/// 403 unless the key is an admin reaching every catalog; 404 for an unknown
/// owner; 409 for an invalid or taken name; 422 for a body without `name` or
/// `owner`, or a refused `display_name`; 500 for a database failure.
#[tracing::instrument(level = "debug", skip_all)]
async fn create_team(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    actor: AuditActor,
    Json(body): Json<CreateTeamBody>,
) -> Result<(StatusCode, Json<TeamView>), AppError> {
    require_manager(&principal)?;
    let display_name = DisplayName::parse(&body.display_name)?;
    let new = NewTeam {
        name: &body.name,
        display_name: &display_name,
        owner: &body.owner,
    };
    let created = teams::create(app.accounts().executor(), &new, &actor, app.clock().now()).await?;
    Ok((
        StatusCode::CREATED,
        Json(TeamView::one(&app, created).await?),
    ))
}

/// `GET /teams/{name}`: one team, with its members and catalogs.
///
/// # Errors
/// 403 below admin; 404 for an unknown team; 500 for a database failure.
#[tracing::instrument(level = "debug", skip_all)]
async fn team_detail(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    Path(name): Path<String>,
) -> Result<Json<TeamView>, AppError> {
    require_reader(&principal)?;
    let record = teams::find(app.accounts().executor(), &name).await?;
    Ok(Json(TeamView::one(&app, record).await?))
}

/// `DELETE /teams/{name}`: deletes a shared team that owns no catalog.
///
/// # Returns
/// 204.
///
/// # Errors
/// 403 unless the key is an admin reaching every catalog; 404 for an unknown
/// team; 409 for a personal team or one that owns a catalog; 500 for a
/// database failure.
#[tracing::instrument(level = "debug", skip_all)]
async fn delete_team(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    actor: AuditActor,
    Path(name): Path<String>,
) -> Result<StatusCode, AppError> {
    require_manager(&principal)?;
    teams::delete(app.accounts().executor(), &name, &actor, app.clock().now()).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `PUT /teams/{name}/members/{user}`: adds the user with `role`, or gives
/// a member that role.
///
/// # Errors
/// 403 unless the key is an admin reaching every catalog; 404 for an unknown
/// team or user; 409 for demoting the last owner; 422 for a missing or
/// unknown `role`; 500 for a database failure.
#[tracing::instrument(level = "debug", skip_all)]
async fn set_member(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    actor: AuditActor,
    // Two path parameters arrive as a tuple, in the order of the pattern.
    Path((team, user)): Path<(String, String)>,
    Json(body): Json<MemberBody>,
) -> Result<Json<MembershipChanged>, AppError> {
    require_manager(&principal)?;
    let membership = Membership {
        team: &team,
        user: &user,
        role: body.role,
    };
    let change = teams::set_member(
        app.accounts().executor(),
        &membership,
        &actor,
        app.clock().now(),
    )
    .await?;
    Ok(Json(MembershipChanged {
        change: change_name(change),
        team: TeamView::one(&app, teams::find(app.accounts().executor(), &team).await?).await?,
    }))
}

/// `DELETE /teams/{name}/members/{user}`: removes a member.
///
/// # Returns
/// The team as it now is.
///
/// # Errors
/// 403 unless the key is an admin reaching every catalog; 404 for an unknown
/// team or user; 409 for a non-member, the last owner, or the user whose
/// personal team this is; 500 for a database failure.
#[tracing::instrument(level = "debug", skip_all)]
async fn remove_member(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    actor: AuditActor,
    Path((team, user)): Path<(String, String)>,
) -> Result<Json<TeamView>, AppError> {
    require_manager(&principal)?;
    teams::remove_member(
        app.accounts().executor(),
        &team,
        &user,
        &actor,
        app.clock().now(),
    )
    .await?;
    let record = teams::find(app.accounts().executor(), &team).await?;
    Ok(Json(TeamView::one(&app, record).await?))
}

/// `PUT /teams/{name}/suspension`: suspends the team (or replaces its
/// notice, keeping the first suspension time), or lifts its suspension.
/// The write's own change messages are applied before the answer, so the
/// next request on this replica is refused (or served again); the others
/// follow through the change feed, or within `catalog_ttl` should a message
/// be lost.
///
/// # Returns
/// The team as it now is.
///
/// # Errors
/// 403 unless the key is an admin reaching every catalog; 404 for an unknown
/// or malformed team name; 409 for a notice over 2000 characters or holding
/// a NUL; 422 for a body without `suspended` or with an unknown field; 500
/// for a database failure.
#[tracing::instrument(level = "debug", skip_all)]
async fn set_suspension(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    actor: AuditActor,
    Path(name): Path<String>,
    Json(body): Json<SuspensionBody>,
) -> Result<Json<TeamView>, AppError> {
    require_manager(&principal)?;
    let now = app.clock().now();
    let team = if body.suspended {
        teams::suspend(
            app.accounts().executor(),
            &name,
            body.notice.as_deref(),
            &actor,
            now,
        )
        .await?
    } else {
        teams::unsuspend(app.accounts().executor(), &name, &actor, now).await?
    };
    Ok(Json(TeamView::one(&app, team).await?))
}

/// `PATCH /users/{name}`'s and `PATCH /me`'s body: the new display name.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DisplayNameBody {
    /// The new display name, checked by [`DisplayName::parse`]; empty
    /// clears it.
    pub display_name: String,
}

/// `PATCH /users/{name}`: sets the user's display name; the labels show it
/// at once.
///
/// # Returns
/// The user as it now is.
///
/// # Errors
/// 403 unless the key is an admin reaching every catalog or a provisioner
/// (which is also refused an admin user); 404 for an unknown user; 422 for
/// a refused display name or a body that is not exactly `display_name`;
/// 500 for a database failure.
#[tracing::instrument(level = "debug", skip_all)]
async fn update_user(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    actor: AuditActor,
    Path(name): Path<String>,
    Json(body): Json<DisplayNameBody>,
) -> Result<Json<UserView>, AppError> {
    require_manager(&principal)?;
    let display_name = DisplayName::parse(&body.display_name)?;
    users::set_display_name(
        app.accounts().executor(),
        &name,
        &display_name,
        &actor,
        app.clock().now(),
    )
    .await?;
    Ok(Json(
        users::find(app.accounts().executor(), &name).await?.into(),
    ))
}

/// `PATCH /teams/{name}`: sets a shared team's display name; the labels show
/// it at once. A manager (an admin reaching every catalog,
/// or a provisioner) may rename any shared team; a console session may
/// rename one it owns, which the store checks under its locks.
///
/// # Returns
/// The team as it now is.
///
/// # Errors
/// 403 for anyone else (a member who is not an owner, another user's team,
/// a key); 404 for an unknown team asked by a manager; 409 for a personal
/// team; 422 for a refused display name or a body that is not exactly
/// `display_name`; 500 for a database failure.
#[tracing::instrument(level = "debug", skip_all)]
async fn update_team(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    actor: AuditActor,
    Path(name): Path<String>,
    Json(body): Json<DisplayNameBody>,
) -> Result<Json<TeamView>, AppError> {
    // A tuple match decides on both at once: a manager passes; a console
    // session may still be an owner; any other credential gets the refusal.
    let owner = match (require_manager(&principal), &principal.kind) {
        (Ok(()), _) => None,
        (Err(_), CredentialKind::Session { user_id }) => Some(user_id.as_str()),
        (Err(refusal), CredentialKind::ApiKey | CredentialKind::OAuth { .. }) => {
            return Err(refusal.into())
        }
    };
    let display_name = DisplayName::parse(&body.display_name)?;
    let change = teams::TeamDisplayNameChange {
        team: &name,
        display_name: &display_name,
        owner,
    };
    teams::set_display_name(
        app.accounts().executor(),
        &change,
        &actor,
        app.clock().now(),
    )
    .await?;
    let record = teams::find(app.accounts().executor(), &name).await?;
    Ok(Json(TeamView::one(&app, record).await?))
}

/// `GET /unassigned`: every key, grant, code and catalog without a user or
/// owner. Unfiltered, so it needs a key that manages all of them.
///
/// # Errors
/// 403 unless the key is an admin reaching every catalog; 500 for a
/// database failure.
#[tracing::instrument(level = "debug", skip_all)]
async fn list_unassigned(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
) -> Result<Json<UnassignedView>, AppError> {
    require_every_catalog(&principal)?;
    let rows = unassigned::list(app.accounts().executor()).await?;
    let now = app.clock().now();
    Ok(Json(UnassignedView {
        keys: rows.keys.into_iter().map(KeyView::from).collect(),
        codes: rows
            .codes
            .into_iter()
            .map(|code| SignInCodeView::new(code, now))
            .collect(),
        catalogs: rows.catalogs,
    }))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use chrono::{TimeDelta, Utc};
    use serde_json::json;

    use std::time::Duration;

    use crate::db::{TEST_TEAM, TEST_USER};
    use crate::http::api::testing::{
        app, assert_matrix_with, every_catalog_admin, keys, login_app, send, wait_for_lock_waiters,
        Browser, Payload, Row,
    };
    use crate::mcp::testing::TestApp;
    use crate::store::audit::testing::entries;
    use crate::store::audit::AuditActor;
    use crate::store::ceiling;
    use crate::store::keys::{self as key_rows, Access, Grants, NewKey};
    use crate::store::oauth::codes::{self, NewSignInCode};
    use crate::store::sign_in_events::{self, Method, Outcome};
    use crate::store::teams::MAX_NOTICE_CHARS;
    use crate::store::user_sessions::{self, SessionStart};
    use crate::store::users as user_rows;
    use crate::store::users::AdminChange;

    /// The longest a race test waits for a request or a lock waiter.
    const RACE_WAIT: Duration = Duration::from_secs(30);

    /// The admin user routes, as `(method, uri, body)`, for user `name`:
    /// reads first (the first two), then writes.
    fn admin_user_routes(name: &str) -> Vec<(&'static str, String, Option<serde_json::Value>)> {
        let base = format!("/api/v1/users/{name}");
        vec![
            ("GET", format!("{base}/sessions"), None),
            ("GET", format!("{base}/sign-ins"), None),
            ("PUT", format!("{base}/admin"), Some(json!({"admin": true}))),
            (
                "PUT",
                format!("{base}/email"),
                Some(json!({"email": "someone@example.com"})),
            ),
            ("POST", format!("{base}/invite"), None),
            (
                "PUT",
                format!("{base}/entitlements/main"),
                Some(json!({"access": "read"})),
            ),
            ("DELETE", format!("{base}/entitlements/main"), None),
            ("DELETE", format!("{base}/sessions"), None),
            ("DELETE", format!("{base}/sessions/abcdef012345"), None),
        ]
    }

    /// Creates user `name` through the API as `root`, asserting the 201.
    async fn user(test: &TestApp, root: &str, name: &str) {
        let (status, body) = send(
            test,
            "POST",
            "/api/v1/users",
            Some(root),
            Some(json!({ "name": name })),
        )
        .await;
        assert_eq!(status.as_u16(), 201, "create user {name}: {body}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_user_routes_record_their_changes_as_the_api_caller() {
        let test = app().await;
        let root = every_catalog_admin(&test).await;
        user(&test, &root, "alice").await;
        let _first = Browser::session_for(&test, "alice").await;
        let _second = Browser::session_for(&test, "alice").await;
        let (_, listed) = send(
            &test,
            "GET",
            "/api/v1/users/alice/sessions",
            Some(&root),
            None,
        )
        .await;
        let prefix = listed["sessions"][0]["id_prefix"]
            .as_str()
            .expect("a prefix")
            .to_string();
        let writes = [
            (
                "PUT",
                "/api/v1/users/alice/email".to_string(),
                Some(json!({"email": "alice@example.com"})),
            ),
            (
                "DELETE",
                format!("/api/v1/users/alice/sessions/{prefix}"),
                None,
            ),
            (
                "PUT",
                "/api/v1/users/alice/admin".to_string(),
                Some(json!({"admin": true})),
            ),
            (
                "PUT",
                "/api/v1/users/alice/entitlements/main".to_string(),
                Some(json!({"access": "read"})),
            ),
            (
                "DELETE",
                "/api/v1/users/alice/entitlements/main".to_string(),
                None,
            ),
        ];

        for (method, path, body) in writes {
            let (status, answer) = send(&test, method, &path, Some(&root), body).await;
            assert!(status.is_success(), "{method} {path}: {status} {answer}");
        }
        // The admin change ended the other session (recorded as the admin
        // change only); sign alice in again so the bulk revoke ends one.
        let _third = Browser::session_for(&test, "alice").await;
        let (status, _) = send(
            &test,
            "DELETE",
            "/api/v1/users/alice/sessions",
            Some(&root),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        for (action, count) in [
            ("user.created", 1),
            ("user.email_changed", 1),
            ("user.admin_changed", 1),
            ("user.entitlement_set", 1),
            ("user.entitlement_removed", 1),
            ("user.sessions_revoked", 2),
        ] {
            let recorded: Vec<_> = entries(test.app.accounts().executor(), action)
                .await
                .into_iter()
                .filter(|entry| entry.person.as_deref() == Some("alice"))
                .collect();
            assert_eq!(recorded.len(), count, "{action}");
            assert!(
                recorded
                    .iter()
                    .all(|entry| entry.source == "api" && entry.actor_label == "api key"),
                "{action}: {recorded:?}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn users_by_email_answers_zero_or_one_and_needs_an_admin() {
        let test = app().await;
        let root = every_catalog_admin(&test).await;
        let scoped = keys(&test).await;
        user(&test, &root, "alice").await;
        user(&test, &root, "bob").await;
        let (set, _) = send(
            &test,
            "PUT",
            "/api/v1/users/alice/email",
            Some(&root),
            Some(json!({"email": "Alice@Example.com"})),
        )
        .await;
        assert_eq!(set, StatusCode::OK);

        let get = |uri: &'static str, key: String| {
            let test = &test;
            async move { send(test, "GET", uri, Some(&key), None).await }
        };
        let (_, every) = get("/api/v1/users", root.clone()).await;
        let (found, one) = get("/api/v1/users?email=ALICE@example.com", root.clone()).await;
        let (_, by_scoped) = get("/api/v1/users?email=alice@example.com", scoped.admin).await;
        let (_, none) = get("/api/v1/users?email=nobody@example.com", root.clone()).await;
        let (_, malformed) = get("/api/v1/users?email=not%20an%20address%00", root.clone()).await;
        let (refused, _) = get("/api/v1/users?email=alice@example.com", scoped.write).await;
        let (typo, _) = get("/api/v1/users?mail=alice@example.com", root.clone()).await;

        assert_eq!(found, StatusCode::OK);
        let alice = every["users"]
            .as_array()
            .unwrap()
            .iter()
            .find(|listed| listed["name"] == "alice")
            .unwrap();
        assert_eq!(one["users"], json!([alice]));
        assert_eq!(by_scoped["users"], json!([alice]));
        assert_eq!(none["users"], json!([]));
        assert_eq!(malformed["users"], json!([]));
        assert_eq!(refused, StatusCode::FORBIDDEN);
        assert_eq!(typo, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn the_new_user_routes_need_an_admin_and_writes_one_reaching_every_catalog() {
        let test = app().await;
        let keys = keys(&test).await;
        let root = every_catalog_admin(&test).await;
        user(&test, &root, "alice").await;
        let rows: Vec<Row> = admin_user_routes("alice")
            .into_iter()
            .enumerate()
            .map(|(index, (method, uri, body))| {
                // Reads: any admin. Writes: no key in the matrix reaches
                // every catalog, so all 403.
                let admin = if index < 2 { 200 } else { 403 };
                Row {
                    method,
                    uri,
                    body,
                    read: 403,
                    write: 403,
                    admin,
                    stranger: admin,
                }
            })
            .collect();

        assert_matrix_with(&test, &keys, &rows).await;
        let (_, alice) = send(&test, "GET", "/api/v1/users/alice", Some(&root), None).await;
        assert_eq!(alice["user"]["is_admin"], false, "no refused write landed");
        assert_eq!(alice["user"]["email"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn a_non_admin_session_is_refused_and_an_admin_session_may_act() {
        let test = login_app().await;
        let root = every_catalog_admin(&test).await;
        user(&test, &root, "alice").await;
        user(&test, &root, "bob").await;
        let mut plain = Browser::session_for(&test, "bob").await;
        let mut refused = Vec::new();
        for (method, uri, body) in admin_user_routes("alice") {
            let payload = body.map_or(Payload::Empty, Payload::Json);
            let (status, _) = plain.send(&test, method, &uri, payload).await.parts();
            refused.push((method, uri, status.as_u16()));
        }
        let (flag, _) = send(
            &test,
            "PUT",
            "/api/v1/users/alice/admin",
            Some(&root),
            Some(json!({"admin": true})),
        )
        .await;
        let mut admin = Browser::session_for(&test, "alice").await;

        let (listed, _) = admin
            .send(&test, "GET", "/api/v1/users/bob/sessions", Payload::Empty)
            .await
            .parts();
        let (wrote, body) = admin
            .send(
                &test,
                "PUT",
                "/api/v1/users/bob/email",
                Payload::Json(json!({"email": "bob@example.com"})),
            )
            .await
            .parts();
        admin.csrf = None;
        let (no_csrf, _) = admin
            .send(
                &test,
                "PUT",
                "/api/v1/users/bob/email",
                Payload::Json(json!({"email": null})),
            )
            .await
            .parts();

        for (method, uri, status) in refused {
            assert_eq!(status, 403, "{method} {uri} as a non-admin session");
        }
        assert_eq!(flag.as_u16(), 200);
        assert_eq!(listed.as_u16(), 200);
        assert_eq!(
            wrote.as_u16(),
            200,
            "an admin session reaches every catalog: {body}"
        );
        assert_eq!(body["email"], "bob@example.com");
        assert_eq!(no_csrf.as_u16(), 403, "a cookie write needs X-OKF-CSRF");
    }

    #[tokio::test]
    async fn the_admin_flag_round_trips_and_the_last_admin_is_refused() {
        let test = app().await;
        let root = every_catalog_admin(&test).await;
        user(&test, &root, "alice").await;
        let flag = |on: bool| Some(json!({"admin": on}));
        let mut session = Browser::session_for(&test, "alice").await;

        let (on, body) = send(
            &test,
            "PUT",
            "/api/v1/users/alice/admin",
            Some(&root),
            flag(true),
        )
        .await;
        let (signed_out, _) = session
            .send(&test, "GET", "/api/v1/me", Payload::Empty)
            .await
            .parts();
        let (last, refused) = send(
            &test,
            "PUT",
            "/api/v1/users/alice/admin",
            Some(&root),
            flag(false),
        )
        .await;
        let (_, still) = send(&test, "GET", "/api/v1/users/alice", Some(&root), None).await;
        send(
            &test,
            "PUT",
            &format!("/api/v1/users/{TEST_USER}/admin"),
            Some(&root),
            flag(true),
        )
        .await;
        let (off, after) = send(
            &test,
            "PUT",
            "/api/v1/users/alice/admin",
            Some(&root),
            flag(false),
        )
        .await;
        let (_, listed) = send(&test, "GET", "/api/v1/users", Some(&root), None).await;
        let (unknown, _) = send(
            &test,
            "PUT",
            "/api/v1/users/nobody/admin",
            Some(&root),
            flag(true),
        )
        .await;
        let (missing, _) = send(
            &test,
            "PUT",
            "/api/v1/users/alice/admin",
            Some(&root),
            Some(json!({})),
        )
        .await;

        assert_eq!(on.as_u16(), 200, "{body}");
        assert_eq!(
            body,
            json!({"name": "alice", "previous": false, "admin": true})
        );
        assert_eq!(signed_out.as_u16(), 401, "a flag change signs the user out");
        assert_eq!(
            last.as_u16(),
            409,
            "the API never clears the last admin: {refused}"
        );
        assert_eq!(still["user"]["is_admin"], true);
        assert_eq!(off.as_u16(), 200);
        assert_eq!(
            after,
            json!({"name": "alice", "previous": true, "admin": false})
        );
        let alice = listed["users"]
            .as_array()
            .expect("a list")
            .iter()
            .find(|row| row["name"] == "alice")
            .expect("alice")
            .clone();
        assert_eq!(alice["is_admin"], false);
        assert_eq!(alice["has_password"], false);
        assert_eq!(unknown.as_u16(), 404);
        assert_eq!(missing.as_u16(), 422);
    }

    #[tokio::test]
    async fn two_concurrent_clears_of_the_last_two_admins_leave_one() {
        // Both requests queue behind a held lock on the admin rows, so they
        // run their checks one after the other: the second must see only one
        // admin left and refuse.
        let test = app().await;
        let root = every_catalog_admin(&test).await;
        user(&test, &root, "alice").await;
        for name in ["alice", TEST_USER] {
            user_rows::set_admin(
                test.app.accounts().executor(),
                name,
                AdminChange {
                    on: true,
                    allow_last: false,
                },
                &AuditActor::cli(),
                Utc::now(),
            )
            .await
            .expect("make an admin");
        }
        let mut holder = test.app.accounts().executor().begin().await.expect("begin");
        sqlx::query("SELECT id FROM users WHERE is_admin FOR UPDATE")
            .execute(&mut *holder)
            .await
            .expect("hold the admin rows");
        let clear = |name: &str| {
            let uri = format!("/api/v1/users/{name}/admin");
            let (test, root) = (&test, root.as_str());
            async move { send(test, "PUT", &uri, Some(root), Some(json!({"admin": false}))).await }
        };
        let release = async {
            tokio::time::timeout(
                RACE_WAIT,
                wait_for_lock_waiters(test.app.accounts().executor(), 2),
            )
            .await
            .expect("both clears wait on the admin rows");
            holder.rollback().await.expect("release the admin rows");
        };

        // `join!` polls all three on this task; the two requests make
        // progress while their queries wait on the database.
        let joined = tokio::time::timeout(RACE_WAIT, async {
            tokio::join!(clear("alice"), clear(TEST_USER), release)
        })
        .await
        .expect("the two clears never finished");
        let ((first, _), (second, _), ()) = joined;
        let mut statuses = [first.as_u16(), second.as_u16()];
        statuses.sort_unstable();
        let admins: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE is_admin")
            .fetch_one(test.app.accounts().executor())
            .await
            .expect("count the admins");

        assert_eq!(statuses, [200, 409]);
        assert_eq!(admins, 1);
    }

    #[tokio::test]
    async fn email_is_set_cleared_and_unique() {
        let test = app().await;
        let root = every_catalog_admin(&test).await;
        user(&test, &root, "alice").await;
        user(&test, &root, "bob").await;
        let put = |name: &str, body: serde_json::Value| {
            let uri = format!("/api/v1/users/{name}/email");
            let (test, root) = (&test, root.as_str());
            async move { send(test, "PUT", &uri, Some(root), Some(body)).await }
        };

        let (set, view) = put("alice", json!({"email": "alice@example.com"})).await;
        let (duplicate, _) = put("bob", json!({"email": "Alice@Example.com"})).await;
        let (malformed, _) = put("bob", json!({"email": "bob@example.com, x@example.com"})).await;
        let (missing, _) = put("bob", json!({})).await;
        let (unknown, _) = put("nobody", json!({"email": "n@example.com"})).await;
        let (_, bob) = send(&test, "GET", "/api/v1/users/bob", Some(&root), None).await;
        let (cleared, after) = put("alice", json!({"email": null})).await;

        assert_eq!(set.as_u16(), 200, "{view}");
        assert_eq!(view["name"], "alice");
        assert_eq!(view["email"], "alice@example.com");
        assert_eq!(duplicate.as_u16(), 409);
        assert_eq!(malformed.as_u16(), 422);
        assert_eq!(missing.as_u16(), 422, "an absent email is not a clear");
        assert_eq!(unknown.as_u16(), 404);
        assert_eq!(bob["user"]["email"], serde_json::Value::Null);
        assert_eq!(cleared.as_u16(), 200);
        assert_eq!(after["email"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn invite_queues_a_setup_mail_once_there_is_an_email_and_a_public_url() {
        let test = login_app().await;
        let root = every_catalog_admin(&test).await;
        user(&test, &root, "alice").await;
        let (no_email, _) = send(
            &test,
            "POST",
            "/api/v1/users/alice/invite",
            Some(&root),
            None,
        )
        .await;
        send(
            &test,
            "PUT",
            "/api/v1/users/alice/email",
            Some(&root),
            Some(json!({"email": "alice@example.com"})),
        )
        .await;

        let (status, body) = send(
            &test,
            "POST",
            "/api/v1/users/alice/invite",
            Some(&root),
            None,
        )
        .await;

        assert_eq!(no_email.as_u16(), 409);
        assert_eq!(status.as_u16(), 202, "{body}");
        assert_eq!(body, json!({"name": "alice", "email": "alice@example.com"}));
        let queued = crate::mail::queue::list(test.app.accounts().executor())
            .await
            .expect("the queue");
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].to, "alice@example.com");
        assert_eq!(queued[0].template, "setup");

        let plain = app().await;
        let plain_root = every_catalog_admin(&plain).await;
        user_rows::set_email(
            plain.app.accounts().executor(),
            TEST_USER,
            Some("tester@example.com"),
            &AuditActor::cli(),
            Utc::now(),
        )
        .await
        .expect("email");
        let (no_url, _) = send(
            &plain,
            "POST",
            &format!("/api/v1/users/{TEST_USER}/invite"),
            Some(&plain_root),
            None,
        )
        .await;
        assert_eq!(no_url.as_u16(), 409, "login needs public_url (Decision 18)");
    }

    #[tokio::test]
    async fn entitlements_change_the_ceiling_and_show_in_the_detail() {
        let test = app().await;
        let root = every_catalog_admin(&test).await;
        user(&test, &root, "alice").await;
        let put = |catalog: &str, access: &str| {
            let uri = format!("/api/v1/users/alice/entitlements/{catalog}");
            let body = json!({ "access": access });
            let (test, root) = (&test, root.as_str());
            async move { send(test, "PUT", &uri, Some(root), Some(body)).await }
        };

        let (entitled, body) = put("main", "read").await;
        let (_, detail) = send(&test, "GET", "/api/v1/users/alice", Some(&root), None).await;
        let ceiling_now = ceiling::ceiling_of(test.app.accounts().executor(), "alice")
            .await
            .expect("ceiling");
        // Her session's ceiling on `main`, resolved from the cached gates
        // and the catalog's row (issue #728).
        let main_row = test
            .app
            .directory()
            .resolve("main")
            .await
            .unwrap()
            .expect("main");
        let cached = test
            .app
            .session_principal(&detail_id(&test, "alice").await)
            .await
            .unwrap()
            .and_then(|session| session.level_at(&main_row));
        let (admin, _) = put("main", "admin").await;
        let (bogus, _) = put("main", "owner").await;
        let (unknown_catalog, _) = put("nowhere", "read").await;
        let (removed, after) = send(
            &test,
            "DELETE",
            "/api/v1/users/alice/entitlements/main",
            Some(&root),
            None,
        )
        .await;

        assert_eq!(entitled.as_u16(), 200, "{body}");
        assert_eq!(body["name"], "alice");
        assert_eq!(body["entitlements"][0]["catalog"], "main");
        assert_eq!(body["entitlements"][0]["access"], "read");
        assert!(body["entitlements"][0]["created_at"].is_string());
        assert_eq!(detail["entitlements"], body["entitlements"]);
        assert_eq!(ceiling_now.get("main"), Some(&Access::Read));
        assert_eq!(
            cached,
            Some(Access::Read),
            "the route's messages reach the cache"
        );
        assert_eq!(admin.as_u16(), 400, "an entitlement is read or write");
        assert_eq!(bogus.as_u16(), 400);
        assert_eq!(unknown_catalog.as_u16(), 404);
        assert_eq!(removed.as_u16(), 200);
        assert_eq!(after, json!({"name": "alice", "entitlements": []}));
    }

    /// The id of user `name`.
    async fn detail_id(test: &TestApp, name: &str) -> String {
        user_rows::id_by_name(test.app.accounts().executor(), name)
            .await
            .expect("the user's id")
    }

    #[tokio::test]
    async fn an_admin_lists_and_revokes_a_users_sessions_and_reads_its_sign_ins() {
        let test = app().await;
        let root = every_catalog_admin(&test).await;
        user(&test, &root, "alice").await;
        let mut first = Browser::session_for(&test, "alice").await;
        let mut second = Browser::session_for(&test, "alice").await;
        let id = detail_id(&test, "alice").await;
        sign_in_events::record(
            test.app.accounts().executor(),
            Some(&id),
            Method::Password,
            Outcome::Ok,
            None,
            Utc::now(),
        )
        .await
        .expect("record");

        let (_, listed) = send(
            &test,
            "GET",
            "/api/v1/users/alice/sessions",
            Some(&root),
            None,
        )
        .await;
        let (_, sign_ins) = send(
            &test,
            "GET",
            "/api/v1/users/alice/sign-ins",
            Some(&root),
            None,
        )
        .await;
        let prefix = listed["sessions"][0]["id_prefix"]
            .as_str()
            .expect("a prefix")
            .to_string();
        let (one, _) = send(
            &test,
            "DELETE",
            &format!("/api/v1/users/alice/sessions/{prefix}"),
            Some(&root),
            None,
        )
        .await;
        let (again, _) = send(
            &test,
            "DELETE",
            &format!("/api/v1/users/alice/sessions/{prefix}"),
            Some(&root),
            None,
        )
        .await;
        let (_, left) = send(
            &test,
            "GET",
            "/api/v1/users/alice/sessions",
            Some(&root),
            None,
        )
        .await;
        let (all, revoked) = send(
            &test,
            "DELETE",
            "/api/v1/users/alice/sessions",
            Some(&root),
            None,
        )
        .await;
        let (first_out, _) = first
            .send(&test, "GET", "/api/v1/me", Payload::Empty)
            .await
            .parts();
        let (second_out, _) = second
            .send(&test, "GET", "/api/v1/me", Payload::Empty)
            .await
            .parts();
        let (unknown, _) = send(
            &test,
            "GET",
            "/api/v1/users/nobody/sessions",
            Some(&root),
            None,
        )
        .await;
        let (unknown_sign_ins, _) = send(
            &test,
            "GET",
            "/api/v1/users/nobody/sign-ins",
            Some(&root),
            None,
        )
        .await;

        let sessions = listed["sessions"].as_array().expect("a list");
        assert_eq!(sessions.len(), 2);
        let mut fields: Vec<&str> = sessions[0]
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        fields.sort_unstable();
        assert_eq!(
            fields,
            [
                "created_at",
                "current",
                "id_prefix",
                "ip",
                "last_seen_at",
                "user_agent"
            ],
            "a prefix only: never a session id, token or hash"
        );
        assert_eq!(prefix.len(), 12);
        assert!(
            sessions.iter().all(|row| row["current"] == false),
            "an admin's view has no current"
        );
        assert_eq!(sign_ins["sign_ins"][0]["method"], "password");
        assert_eq!(sign_ins["sign_ins"][0]["outcome"], "ok");
        assert_eq!(one.as_u16(), 204);
        assert_eq!(again.as_u16(), 404);
        assert_eq!(left["sessions"].as_array().map(Vec::len), Some(1));
        assert_eq!(all.as_u16(), 200);
        assert_eq!(revoked, json!({"name": "alice", "revoked": 1}));
        assert_eq!(first_out.as_u16(), 401);
        assert_eq!(second_out.as_u16(), 401);
        assert_eq!(unknown.as_u16(), 404);
        assert_eq!(unknown_sign_ins.as_u16(), 404);
    }

    #[tokio::test]
    async fn a_prefix_several_sessions_share_is_a_409_and_ends_none() {
        let test = app().await;
        let root = every_catalog_admin(&test).await;
        user(&test, &root, "alice").await;
        let id = detail_id(&test, "alice").await;
        // Two stored hashes with the same first six bytes, as two random ids
        // could have (no real id is needed to list or revoke).
        for last in [1_u8, 2] {
            let mut hash = vec![0xab_u8; 32];
            hash[31] = last;
            sqlx::query(
                "INSERT INTO user_sessions \
                     (id_hash, user_id, csrf_token, created_at, last_seen_at, expires_at) \
                 VALUES ($1, $2, '', now(), now(), now() + interval '1 day')",
            )
            .bind(hash)
            .bind(&id)
            .execute(test.app.accounts().executor())
            .await
            .expect("seed a session");
        }

        let (status, _) = send(
            &test,
            "DELETE",
            "/api/v1/users/alice/sessions/abababababab",
            Some(&root),
            None,
        )
        .await;
        let (_, listed) = send(
            &test,
            "GET",
            "/api/v1/users/alice/sessions",
            Some(&root),
            None,
        )
        .await;

        assert_eq!(status.as_u16(), 409);
        assert_eq!(listed["sessions"].as_array().map(Vec::len), Some(2));
    }

    #[tokio::test]
    async fn signing_out_everywhere_ends_a_session_whose_start_was_still_uncommitted() {
        // A sign-in holds the user's row `FOR SHARE` while its session insert
        // is uncommitted. The admin's sign-out must wait for it and end that
        // session too, not report success and leave it signed in.
        let test = app().await;
        let root = every_catalog_admin(&test).await;
        user(&test, &root, "alice").await;
        let id = detail_id(&test, "alice").await;
        let mut signing_in = test.app.accounts().executor().begin().await.expect("begin");
        user_sessions::start(
            &mut *signing_in,
            &SessionStart {
                user_id: &id,
                ip: None,
                user_agent: "okf-test",
                absolute: Duration::from_secs(3_600),
                now: Utc::now(),
                password: None,
            },
        )
        .await
        .expect("start a session in the open transaction");
        let commit = async {
            tokio::time::timeout(
                RACE_WAIT,
                wait_for_lock_waiters(test.app.accounts().executor(), 1),
            )
            .await
            .expect("the sign-out waits on the user's row");
            signing_in.commit().await.expect("commit the sign-in");
        };

        let ((status, body), ()) = tokio::time::timeout(RACE_WAIT, async {
            tokio::join!(
                send(
                    &test,
                    "DELETE",
                    "/api/v1/users/alice/sessions",
                    Some(&root),
                    None
                ),
                commit
            )
        })
        .await
        .expect("the sign-out never finished");
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM user_sessions WHERE user_id = $1")
            .bind(&id)
            .fetch_one(test.app.accounts().executor())
            .await
            .expect("count the sessions");

        assert_eq!(status.as_u16(), 200, "{body}");
        assert_eq!(body, json!({"name": "alice", "revoked": 1}));
        assert_eq!(left, 0);
    }

    #[tokio::test]
    async fn a_nul_or_malformed_name_in_a_path_is_a_404_never_a_500() {
        // Postgres refuses a NUL in text, so each name must be checked
        // before it reaches SQL (issue #339). `login_app` has `public_url`,
        // so the invite gets as far as the name.
        let test = login_app().await;
        let root = every_catalog_admin(&test).await;
        let mut routes = admin_user_routes("a%00b");
        routes.extend(admin_user_routes("UPPER"));
        routes.extend([
            ("GET", "/api/v1/users/a%00b".to_string(), None),
            (
                "PUT",
                "/api/v1/users/alice/entitlements/a%00b".to_string(),
                Some(json!({"access": "read"})),
            ),
            (
                "DELETE",
                "/api/v1/users/alice/entitlements/a%00b".to_string(),
                None,
            ),
            (
                "DELETE",
                format!("/api/v1/users/{TEST_USER}/sessions/a%00b"),
                None,
            ),
            (
                "GET",
                "/api/v1/catalogs/a%00b/member-access".to_string(),
                None,
            ),
            (
                "PUT",
                "/api/v1/catalogs/a%00b/member-access".to_string(),
                Some(json!({"access": "read"})),
            ),
            (
                "PUT",
                "/api/v1/settings/a%00b".to_string(),
                Some(json!({"value": "1"})),
            ),
            ("DELETE", "/api/v1/settings/a%00b".to_string(), None),
            ("POST", "/api/v1/mail/queue/a%00b/retry".to_string(), None),
            ("DELETE", "/api/v1/mail/queue/a%00b".to_string(), None),
        ]);
        for team in ["a%00b", "UPPER"] {
            let base = format!("/api/v1/teams/{team}");
            routes.extend([
                ("GET", base.clone(), None),
                ("DELETE", base.clone(), None),
                (
                    "PUT",
                    format!("{base}/members/alice"),
                    Some(json!({"role": "member"})),
                ),
                ("DELETE", format!("{base}/members/alice"), None),
                (
                    "PUT",
                    format!("{base}/suspension"),
                    Some(json!({"suspended": true})),
                ),
                (
                    "PUT",
                    format!("{base}/suspension"),
                    Some(json!({"suspended": false})),
                ),
            ]);
        }
        user(&test, &root, "alice").await;

        for (method, uri, body) in routes {
            let (status, answer) = send(&test, method, &uri, Some(&root), body).await;
            assert_eq!(status.as_u16(), 404, "{method} {uri}: {answer}");
        }
    }

    #[tokio::test]
    async fn user_and_team_creates_parse_the_display_name() {
        // Arrange
        let test = app().await;
        let root = every_catalog_admin(&test).await;
        let too_long = "x".repeat(81);

        // Act: a name the rule trims, then one too long for each create.
        let (created, user) = send(
            &test,
            "POST",
            "/api/v1/users",
            Some(&root),
            Some(json!({"name": "alice", "display_name": " Alice Example\t"})),
        )
        .await;
        let (user_refused, user_error) = send(
            &test,
            "POST",
            "/api/v1/users",
            Some(&root),
            Some(json!({"name": "bob", "display_name": too_long})),
        )
        .await;
        let (team_refused, team_error) = send(
            &test,
            "POST",
            "/api/v1/teams",
            Some(&root),
            Some(json!({"name": "family", "display_name": too_long, "owner": "alice"})),
        )
        .await;
        let (no_bob, _) = send(&test, "GET", "/api/v1/users/bob", Some(&root), None).await;
        let (no_family, _) = send(&test, "GET", "/api/v1/teams/family", Some(&root), None).await;

        // Assert: stored trimmed; each refusal is a 422 that wrote nothing.
        assert_eq!(created.as_u16(), 201, "{user}");
        assert_eq!(user["display_name"], "Alice Example");
        assert_eq!(user_refused.as_u16(), 422, "{user_error}");
        assert_eq!(team_refused.as_u16(), 422, "{team_error}");
        assert_eq!(no_bob.as_u16(), 404);
        assert_eq!(no_family.as_u16(), 404);
    }

    #[tokio::test]
    async fn an_admin_creates_lists_and_deletes_a_user() {
        let test = app().await;
        let root = every_catalog_admin(&test).await;

        let (created, body) = send(
            &test,
            "POST",
            "/api/v1/users",
            Some(&root),
            Some(json!({"name": "alice", "display_name": "Alice Example"})),
        )
        .await;
        let (_, listed) = send(&test, "GET", "/api/v1/users", Some(&root), None).await;
        let (_, personal) = send(&test, "GET", "/api/v1/teams/alice", Some(&root), None).await;
        let (deleted, _) = send(&test, "DELETE", "/api/v1/users/alice", Some(&root), None).await;
        let (gone, error) = send(&test, "GET", "/api/v1/users/alice", Some(&root), None).await;

        assert_eq!(created.as_u16(), 201, "{body}");
        assert_eq!(body["name"], "alice");
        assert_eq!(body["display_name"], "Alice Example");
        assert_eq!(body["credentials"], 0);
        assert_eq!(body["teams"], json!(["alice"]));
        assert_eq!(body["email"], serde_json::Value::Null);
        assert_eq!(body["is_admin"], false);
        assert_eq!(body["has_password"], false);
        assert!(body.get("password_hash").is_none(), "{body}");
        assert!(listed["users"]
            .as_array()
            .expect("a list")
            .iter()
            .any(|u| u["name"] == "alice"));
        assert_eq!(personal["personal_of"], "alice");
        assert_eq!(
            personal["members"],
            json!([{"user": "alice", "role": "owner", "label": "Alice Example"}])
        );
        assert_eq!(deleted.as_u16(), 204);
        assert_eq!(gone.as_u16(), 404);
        assert_eq!(error["error"], "UnknownName");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn deleting_a_user_over_rest_takes_its_keys_and_refuses_the_last_admin() {
        let test = app().await;
        let root = every_catalog_admin(&test).await;
        user(&test, &root, "alice").await;
        let (created, key) = send(
            &test,
            "POST",
            "/api/v1/keys",
            Some(&root),
            Some(
                json!({"name": "alice-k", "access": "read", "catalogs": ["main"], "user": "alice"}),
            ),
        )
        .await;
        // Not `{key}`: the answer holds the secret.
        assert_eq!(created.as_u16(), 200, "{}", key["error"]);
        sqlx::query("UPDATE users SET is_admin = (name = 'alice')")
            .execute(test.app.accounts().executor())
            .await
            .unwrap();

        let (last, body) = send(&test, "DELETE", "/api/v1/users/alice", Some(&root), None).await;
        sqlx::query("UPDATE users SET is_admin = true WHERE name = $1")
            .bind(TEST_USER)
            .execute(test.app.accounts().executor())
            .await
            .unwrap();
        let (deleted, _) = send(&test, "DELETE", "/api/v1/users/alice", Some(&root), None).await;
        let (_, keys) = send(&test, "GET", "/api/v1/keys", Some(&root), None).await;
        let (gone, _) = send(&test, "GET", "/api/v1/users/alice", Some(&root), None).await;

        assert_eq!(last, StatusCode::CONFLICT, "{body}");
        assert!(
            body["message"].to_string().contains("last admin user"),
            "{body}"
        );
        assert_eq!(deleted, StatusCode::NO_CONTENT);
        assert!(!keys.to_string().contains("alice-k"), "{keys}");
        assert_eq!(gone, StatusCode::NOT_FOUND);
        let source: String =
            sqlx::query_scalar("SELECT source FROM audit_entries WHERE action = 'user.deleted'")
                .fetch_one(test.app.accounts().executor())
                .await
                .unwrap();
        assert_eq!(source, "api");
    }

    #[tokio::test]
    async fn listing_needs_any_admin_and_writes_one_reaching_every_catalog() {
        let test = app().await;
        let keys = keys(&test).await;
        let root = every_catalog_admin(&test).await;
        user(&test, &root, "alice").await;
        let (status, body) = send(
            &test,
            "POST",
            "/api/v1/teams",
            Some(&root),
            Some(json!({"name": "family", "owner": "alice"})),
        )
        .await;
        assert_eq!(status.as_u16(), 201, "{body}");

        // Reads: any admin (callers run read, write, stranger, admin).
        let read = |uri: &str| Row {
            method: "GET",
            uri: uri.to_string(),
            body: None,
            read: 403,
            write: 403,
            admin: 200,
            stranger: 200,
        };
        // Writes: no key in the matrix reaches every catalog, so all 403,
        // the scoped admins on `main` and on `other` included.
        let write = |method: &'static str, uri: &str, body: Option<serde_json::Value>| Row {
            method,
            uri: uri.to_string(),
            body,
            read: 403,
            write: 403,
            admin: 403,
            stranger: 403,
        };
        let rows = [
            read("/api/v1/users"),
            read("/api/v1/users/alice"),
            read("/api/v1/teams"),
            read("/api/v1/teams/family"),
            write("POST", "/api/v1/users", Some(json!({"name": "bob"}))),
            write(
                "POST",
                "/api/v1/teams",
                Some(json!({"name": "friends", "owner": "bob"})),
            ),
            write(
                "PUT",
                "/api/v1/teams/family/members/test-user",
                Some(json!({"role": "member"})),
            ),
            write("DELETE", "/api/v1/teams/family/members/test-user", None),
            write("DELETE", "/api/v1/teams/family", None),
            write("DELETE", "/api/v1/teams/friends", None),
            write("DELETE", "/api/v1/users/alice", None),
            write("GET", "/api/v1/unassigned", None),
        ];
        assert_matrix_with(&test, &keys, &rows).await;
        // Every row's subject is untouched; root may do each one, in order.
        for row in &rows {
            let (status, body) =
                send(&test, row.method, &row.uri, Some(&root), row.body.clone()).await;
            assert!(
                status.is_success(),
                "{} {} as root: {status} {body}",
                row.method,
                row.uri
            );
        }
    }

    #[tokio::test]
    async fn deleting_a_user_unless_a_shared_team_member_refuses_a_member() {
        let test = app().await;
        let root = every_catalog_admin(&test).await;
        let admin = Some(root.as_str());
        user(&test, &root, "alice").await;
        user(&test, &root, "bob").await;
        user(&test, &root, "carol").await;
        send(
            &test,
            "POST",
            "/api/v1/teams",
            admin,
            Some(json!({"name": "family", "owner": "alice"})),
        )
        .await;
        send(
            &test,
            "PUT",
            "/api/v1/teams/family/members/bob",
            admin,
            Some(json!({"role": "member"})),
        )
        .await;

        let (refused, error) = send(
            &test,
            "DELETE",
            "/api/v1/users/bob?unless_shared_member=true",
            admin,
            None,
        )
        .await;
        let (still, _) = send(&test, "GET", "/api/v1/users/bob", admin, None).await;
        let (typo, _) = send(
            &test,
            "DELETE",
            "/api/v1/users/bob?unless_shared_memberr=true",
            admin,
            None,
        )
        .await;
        let (maybe, _) = send(
            &test,
            "DELETE",
            "/api/v1/users/bob?unless_shared_member=maybe",
            admin,
            None,
        )
        .await;
        let (off, _) = send(
            &test,
            "DELETE",
            "/api/v1/users/bob?unless_shared_member=false",
            admin,
            None,
        )
        .await;
        let (unshared, _) = send(
            &test,
            "DELETE",
            "/api/v1/users/carol?unless_shared_member=true",
            admin,
            None,
        )
        .await;

        assert_eq!(refused.as_u16(), 409, "{error}");
        assert_eq!(error["error"], "Refused");
        assert!(
            error["message"]
                .as_str()
                .is_some_and(|m| m == "the user is a member of shared team(s): \"family\""),
            "{error}"
        );
        assert_eq!(still.as_u16(), 200, "the user stays");
        assert_eq!(typo.as_u16(), 400, "a misspelled parameter deletes nothing");
        assert_eq!(maybe.as_u16(), 400, "a non-boolean value deletes nothing");
        assert_eq!(off.as_u16(), 204, "false is the unchanged behaviour");
        assert_eq!(unshared.as_u16(), 204, "no shared team: deleted as before");
    }

    #[tokio::test]
    async fn membership_routes_and_their_refusals() {
        let test = app().await;
        let root = every_catalog_admin(&test).await;
        let admin = Some(root.as_str());
        user(&test, &root, "alice").await;
        send(
            &test,
            "POST",
            "/api/v1/teams",
            admin,
            Some(json!({"name": "family", "owner": "alice"})),
        )
        .await;

        let (added, answer) = send(
            &test,
            "PUT",
            "/api/v1/teams/family/members/test-user",
            admin,
            Some(json!({"role": "member"})),
        )
        .await;
        let (again, unchanged) = send(
            &test,
            "PUT",
            "/api/v1/teams/family/members/test-user",
            admin,
            Some(json!({"role": "member"})),
        )
        .await;
        let (bad_role, _) = send(
            &test,
            "PUT",
            "/api/v1/teams/family/members/test-user",
            admin,
            Some(json!({"role": "boss"})),
        )
        .await;
        let (last_owner, _) = send(
            &test,
            "DELETE",
            "/api/v1/teams/family/members/alice",
            admin,
            None,
        )
        .await;
        let (unknown, _) = send(
            &test,
            "PUT",
            "/api/v1/teams/family/members/nobody",
            admin,
            Some(json!({"role": "member"})),
        )
        .await;
        let (_, detail) = send(&test, "GET", "/api/v1/teams/family", admin, None).await;
        let (removed, after) = send(
            &test,
            "DELETE",
            "/api/v1/teams/family/members/test-user",
            admin,
            None,
        )
        .await;
        let (personal, _) = send(&test, "DELETE", "/api/v1/teams/alice", admin, None).await;
        let (deleted, _) = send(&test, "DELETE", "/api/v1/teams/family", admin, None).await;

        assert_eq!(added.as_u16(), 200, "{answer}");
        assert_eq!(answer["change"], "added");
        assert_eq!(again.as_u16(), 200);
        assert_eq!(unchanged["change"], "unchanged");
        assert_eq!(bad_role.as_u16(), 422);
        assert_eq!(last_owner.as_u16(), 409);
        assert_eq!(unknown.as_u16(), 404);
        assert_eq!(
            detail["members"],
            json!([
                {"user": "alice", "role": "owner", "label": "Unnamed user"},
                {"user": "test-user", "role": "member", "label": "Unnamed user"},
            ])
        );
        assert_eq!(detail["personal_of"], serde_json::Value::Null);
        assert_eq!(removed.as_u16(), 200);
        assert_eq!(
            after["members"],
            json!([{"user": "alice", "role": "owner", "label": "Unnamed user"}])
        );
        assert_eq!(personal.as_u16(), 409, "a personal team goes with its user");
        assert_eq!(deleted.as_u16(), 204);
    }

    #[tokio::test]
    async fn a_missing_user_or_owner_in_a_create_body_is_a_422() {
        let test = app().await;
        let root = every_catalog_admin(&test).await;

        let (key, _) = send(
            &test,
            "POST",
            "/api/v1/keys",
            Some(&root),
            Some(json!({"name": "k", "access": "read", "all": true})),
        )
        .await;
        let (team, _) = send(
            &test,
            "POST",
            "/api/v1/teams",
            Some(&root),
            Some(json!({"name": "family"})),
        )
        .await;
        let (unknown_owner, _) = send(
            &test,
            "POST",
            "/api/v1/teams",
            Some(&root),
            Some(json!({"name": "family", "owner": "nobody"})),
        )
        .await;

        assert_eq!(key.as_u16(), 422);
        assert_eq!(team.as_u16(), 422);
        assert_eq!(unknown_owner.as_u16(), 404);
    }

    #[tokio::test]
    async fn assigning_a_legacy_key_and_listing_unassigned() {
        let test = app().await;
        crate::store::keys::insert_legacy_test_key(
            test.app.accounts().executor(),
            "legacy",
            Access::Read,
            "legacy",
            Utc::now(),
        )
        .await;
        let keys = keys(&test).await;
        let root = every_catalog_admin(&test).await;
        let assign = json!({"user": TEST_USER});

        let (_, before) = send(&test, "GET", "/api/v1/unassigned", Some(&root), None).await;
        // The legacy key reaches every catalog, so the scoped admin does not
        // manage it: a 404, as for an unknown id.
        let (scoped, _) = send(
            &test,
            "PUT",
            "/api/v1/keys/legacy/user",
            Some(&keys.admin),
            Some(assign.clone()),
        )
        .await;
        let (assigned, view) = send(
            &test,
            "PUT",
            "/api/v1/keys/legacy/user",
            Some(&root),
            Some(assign),
        )
        .await;
        let (_, after) = send(&test, "GET", "/api/v1/unassigned", Some(&root), None).await;

        assert_eq!(before["keys"][0]["id"], "legacy");
        assert_eq!(before["keys"].as_array().map(Vec::len), Some(1));
        assert_eq!(scoped.as_u16(), 404);
        assert_eq!(assigned.as_u16(), 200, "{view}");
        assert_eq!(view["user"], TEST_USER);
        assert_eq!(after, json!({"keys": [], "codes": [], "catalogs": []}));
    }

    #[tokio::test]
    async fn an_assign_reaches_the_key_cache_after_the_generation_bump() {
        // Spec "Testing: cache reload": the principal a request runs as
        // carries the new user once the route's poll has reloaded the cache.
        let test = app().await;
        let legacy = crate::store::keys::insert_legacy_test_key(
            test.app.accounts().executor(),
            "legacy",
            Access::Read,
            "legacy",
            Utc::now(),
        )
        .await;
        // The legacy insert bumps no generation, so the admin key is created
        // after it: its create bumps, and its poll loads both.
        let root = every_catalog_admin(&test).await;
        assert_eq!(
            test.app
                .find_key(&legacy)
                .await
                .unwrap()
                .expect("cached")
                .user,
            None
        );

        let (status, _) = send(
            &test,
            "PUT",
            "/api/v1/keys/legacy/user",
            Some(&root),
            Some(json!({"user": TEST_USER})),
        )
        .await;
        let (_, me) = send(&test, "GET", "/api/v1/whoami", Some(&legacy), None).await;

        assert_eq!(status.as_u16(), 200);
        assert_eq!(
            test.app
                .find_key(&legacy)
                .await
                .unwrap()
                .expect("cached")
                .user
                .as_deref(),
            Some(TEST_USER)
        );
        assert_eq!(me["user"], TEST_USER);
    }

    #[tokio::test]
    async fn a_users_detail_shows_only_the_credentials_the_caller_manages() {
        let test = app().await;
        let keys = keys(&test).await;
        let root = every_catalog_admin(&test).await;
        user(&test, &root, "alice").await;
        let other = ["other".to_string()];
        for (name, grants) in [
            ("alice-main", Grants::Catalogs(&["main".to_string()])),
            ("alice-other", Grants::Catalogs(&other)),
            ("alice-all", Grants::All),
        ] {
            key_rows::create_with_id(
                test.app.accounts().executor(),
                &NewKey {
                    name,
                    access: Access::Read,
                    maintenance_report: false,
                    grants,
                    default_catalog: None,
                    actor: name,
                    user: "alice",
                    self_issued: false,
                    expires_at: None,
                    levels: &[],
                },
                &crate::store::audit::AuditActor::cli(),
                Utc::now(),
            )
            .await
            .expect("create a key");
        }
        for (name, catalog) in [("code-main", "main"), ("code-other", "other")] {
            let grants = [catalog.to_string()];
            codes::create(
                test.app.accounts().executor(),
                &NewSignInCode {
                    name,
                    access: Access::Read,
                    grants: Grants::Catalogs(&grants),
                    default_catalog: None,
                    actor: "",
                    expires_at: Utc::now() + TimeDelta::hours(1),
                    user: "alice",
                },
                &crate::store::audit::AuditActor::cli(),
                Utc::now(),
            )
            .await
            .expect("mint a code");
        }
        let names = |detail: &serde_json::Value, field: &str| -> Vec<String> {
            let mut names: Vec<String> = detail[field]
                .as_array()
                .expect("a list")
                .iter()
                .filter_map(|row| row["name"].as_str().map(str::to_string))
                .collect();
            names.sort();
            names
        };

        let (status, scoped) =
            send(&test, "GET", "/api/v1/users/alice", Some(&keys.admin), None).await;
        let (_, everything) = send(&test, "GET", "/api/v1/users/alice", Some(&root), None).await;

        assert_eq!(status.as_u16(), 200, "{scoped}");
        assert_eq!(scoped["user"]["credentials"], 5);
        assert_eq!(names(&scoped, "keys"), ["alice-main"]);
        assert_eq!(names(&scoped, "codes"), ["code-main"]);
        assert_eq!(
            names(&everything, "keys"),
            ["alice-all", "alice-main", "alice-other"]
        );
        assert_eq!(names(&everything, "codes"), ["code-main", "code-other"]);
        // The fixture user's own keys never show under alice.
        assert!(!names(&everything, "keys").contains(&"admin".to_string()));
    }

    #[tokio::test]
    async fn set_owner_on_a_catalog() {
        let test = app().await;
        let keys = keys(&test).await;
        let root = every_catalog_admin(&test).await;
        user(&test, &root, "alice").await;
        let change = Some(json!({"team": "alice"}));

        let (scoped, _) = send(
            &test,
            "PUT",
            "/api/v1/catalogs/main/owner",
            Some(&keys.admin),
            change.clone(),
        )
        .await;
        let (status, body) = send(
            &test,
            "PUT",
            "/api/v1/catalogs/main/owner",
            Some(&root),
            change,
        )
        .await;
        let (unknown_team, _) = send(
            &test,
            "PUT",
            "/api/v1/catalogs/main/owner",
            Some(&root),
            Some(json!({"team": "nobody"})),
        )
        .await;
        let (unknown_catalog, _) = send(
            &test,
            "PUT",
            "/api/v1/catalogs/nowhere/owner",
            Some(&root),
            Some(json!({"team": "alice"})),
        )
        .await;
        let (_, listing) = send(&test, "GET", "/api/v1/catalogs", Some(&root), None).await;
        let (_, team) = send(&test, "GET", "/api/v1/teams/alice", Some(&root), None).await;

        assert_eq!(
            scoped.as_u16(),
            403,
            "a scoped admin cannot change an owner"
        );
        assert_eq!(status.as_u16(), 200, "{body}");
        assert_eq!(
            body,
            json!({"name": "main", "previous": TEST_TEAM, "owner_team": "alice"})
        );
        assert_eq!(unknown_team.as_u16(), 404);
        assert_eq!(unknown_catalog.as_u16(), 404);
        assert_eq!(listing["catalogs"][0]["name"], "main");
        assert_eq!(listing["catalogs"][0]["owner_team"], "alice");
        assert_eq!(team["catalogs"], json!(["main"]));
    }

    /// `PUT /api/v1/teams/{TEST_TEAM}/suspension` with `body` as `key`.
    async fn put_suspension(
        test: &TestApp,
        key: &str,
        body: serde_json::Value,
    ) -> (u16, serde_json::Value) {
        let uri = format!("/api/v1/teams/{TEST_TEAM}/suspension");
        let (status, answer) = send(test, "PUT", &uri, Some(key), Some(body)).await;
        (status.as_u16(), answer)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn suspension_route_suspends_lifts_and_needs_an_every_catalog_admin() {
        let test = app().await;
        let root = every_catalog_admin(&test).await;
        let scoped = keys(&test).await;
        let suspend = json!({"suspended": true, "notice": "Contact the operator."});
        let too_long = "x".repeat(MAX_NOTICE_CHARS + 1);

        let mut denied = Vec::new();
        for (who, key) in [
            ("read", &scoped.read),
            ("write", &scoped.write),
            ("scoped admin", &scoped.admin),
            ("stranger", &scoped.stranger),
        ] {
            denied.push((who, put_suspension(&test, key, suspend.clone()).await));
        }
        let (_, untouched) = send(
            &test,
            "GET",
            &format!("/api/v1/teams/{TEST_TEAM}"),
            Some(&root),
            None,
        )
        .await;
        let (missing, _) = put_suspension(&test, &root, json!({"notice": "x"})).await;
        let (unknown_field, _) =
            put_suspension(&test, &root, json!({"suspended": true, "reason": "x"})).await;
        let (long, long_body) =
            put_suspension(&test, &root, json!({"suspended": true, "notice": too_long})).await;
        let (nul, nul_body) = put_suspension(
            &test,
            &root,
            json!({"suspended": true, "notice": "a\u{0}b"}),
        )
        .await;
        let (no_team, _) = send(
            &test,
            "PUT",
            "/api/v1/teams/nobody/suspension",
            Some(&root),
            Some(suspend.clone()),
        )
        .await;
        let (suspended, team) = put_suspension(&test, &root, suspend.clone()).await;
        let (again, replaced) =
            put_suspension(&test, &root, json!({"suspended": true, "notice": "  "})).await;
        let (lifted, after) = put_suspension(&test, &root, json!({"suspended": false})).await;

        for (who, (status, body)) in denied {
            assert_eq!(status, 403, "{who}: {body}");
            assert_eq!(body["error"], "Forbidden", "{who}: {body}");
        }
        assert_eq!(
            untouched.get("suspended_at"),
            Some(&serde_json::Value::Null)
        );
        assert_eq!(missing, 422);
        assert_eq!(unknown_field, 422);
        assert_eq!(long, 409, "{long_body}");
        assert_eq!(long_body["error"], "Refused");
        assert_eq!(
            nul, 409,
            "a NUL is refused before SQL, never a 500: {nul_body}"
        );
        assert_eq!(no_team.as_u16(), 404);
        assert_eq!(suspended, 200, "{team}");
        assert_eq!(team["name"], TEST_TEAM);
        assert_eq!(team["suspension_notice"], "Contact the operator.");
        assert!(team["suspended_at"].is_string(), "{team}");
        assert_eq!(again, 200, "{replaced}");
        assert_eq!(
            replaced["suspended_at"], team["suspended_at"],
            "a repeat keeps the first suspension time"
        );
        assert_eq!(
            replaced.get("suspension_notice"),
            Some(&serde_json::Value::Null),
            "a blank notice replaces the old one with none"
        );
        assert_eq!(lifted, 200, "{after}");
        // `get`: `after["suspended_at"]` is `Null` for a missing key too.
        assert_eq!(after.get("suspended_at"), Some(&serde_json::Value::Null));
        assert_eq!(
            after.get("suspension_notice"),
            Some(&serde_json::Value::Null)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_member_of_a_suspended_team_may_export_but_not_browse_or_search() {
        let test = app().await;
        let root = every_catalog_admin(&test).await;
        let scoped = keys(&test).await;
        // `app()`'s catalogs are both owned by `TEST_TEAM`, so suspending it
        // through the route suspends `main` and `other`.
        let (status, _) = put_suspension(
            &test,
            &root,
            json!({"suspended": true, "notice": "Paused."}),
        )
        .await;
        assert_eq!(status, 200);

        // No `poll` here: the route settled before answering, so the very
        // next request already sees the suspension.
        let refusals = [
            "/api/v1/catalogs/main/tree",
            "/api/v1/search?q=alpha",
            "/api/v1/changesets?catalog=main",
        ];
        let mut refused = Vec::new();
        for uri in refusals {
            refused.push((uri, send(&test, "GET", uri, Some(&scoped.read), None).await));
        }
        let (changesets, open) = send(
            &test,
            "GET",
            "/api/v1/changesets",
            Some(&scoped.write),
            None,
        )
        .await;
        let (export, _) = send(
            &test,
            "GET",
            "/api/v1/catalogs/main/export",
            Some(&scoped.read),
            None,
        )
        .await;
        let (listed, listing) =
            send(&test, "GET", "/api/v1/catalogs", Some(&scoped.read), None).await;
        let mut admins = Vec::new();
        for key in [&root, &scoped.admin] {
            admins.push(send(&test, "GET", "/api/v1/catalogs/main/tree", Some(key), None).await);
        }
        let (status, _) = put_suspension(&test, &root, json!({"suspended": false})).await;
        assert_eq!(status, 200);
        let (served, _) = send(
            &test,
            "GET",
            "/api/v1/catalogs/main/tree",
            Some(&scoped.read),
            None,
        )
        .await;
        let (searched, _) = send(
            &test,
            "GET",
            "/api/v1/search?q=alpha",
            Some(&scoped.read),
            None,
        )
        .await;
        let (_, relisted) = send(&test, "GET", "/api/v1/catalogs", Some(&scoped.read), None).await;

        for (uri, (status, body)) in refused {
            assert_eq!(status, StatusCode::FORBIDDEN, "{uri}: {body}");
            assert_eq!(body["error"], "Suspended", "{uri}: {body}");
            assert_eq!(body["notice"], "Paused.", "{uri}: {body}");
        }
        assert_eq!(changesets, StatusCode::OK, "{open}");
        assert_eq!(open["changesets"], json!([]), "{open}");
        assert_eq!(export, StatusCode::OK, "export stays open");
        assert_eq!(listed, StatusCode::OK);
        assert_eq!(listing["catalogs"][0]["name"], "main");
        assert_eq!(listing["catalogs"][0]["suspended"]["team"], TEST_TEAM);
        assert_eq!(listing["catalogs"][0]["suspended"]["notice"], "Paused.");
        for (status, body) in admins {
            assert_eq!(status, StatusCode::OK, "admins are not suspended: {body}");
        }
        assert_eq!(served, StatusCode::OK, "the lift takes effect at once");
        assert_eq!(searched, StatusCode::OK);
        assert_eq!(
            relisted["catalogs"][0].get("suspended"),
            Some(&serde_json::Value::Null)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_team_change_over_rest_records_the_calling_key() {
        let test = app().await;
        let root = every_catalog_admin(&test).await;

        let (suspended, _) =
            put_suspension(&test, &root, json!({"suspended": true, "notice": "x"})).await;
        let (lifted, _) = put_suspension(&test, &root, json!({"suspended": false})).await;
        let (again, _) = put_suspension(&test, &root, json!({"suspended": false})).await;

        assert_eq!([suspended, lifted, again], [200, 200, 200]);
        let pool = test.app.accounts().executor();
        let on = crate::store::audit::testing::entries(pool, "team.suspended").await;
        let off = crate::store::audit::testing::entries(pool, "team.unsuspended").await;
        assert_eq!((on.len(), off.len()), (1, 1), "{on:?} {off:?}");
        for entry in [&on[0], &off[0]] {
            assert_eq!(entry.source, "api");
            assert_eq!(entry.actor_label, "api key");
            assert!(entry.actor_operator, "an every-catalog admin key");
            assert_eq!(entry.target.as_deref(), Some(TEST_TEAM));
        }
    }
}
