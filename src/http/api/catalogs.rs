//! `/api/v1/catalogs` (web console design, "API": Catalogs, Import /
//! export): the catalogs a key reaches, their administration as
//! `okf-mcp catalog create|update|delete` does it, and the archive routes
//! mounted again under `/api/v1` with the same handlers, body cap and
//! deadline as `/catalogs/{name}/export|import`.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, patch, put};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};

use super::{navigable_page, owner_session, require_every_catalog, PageQuery};
use crate::app::App;
use crate::auth::{CredentialKind, Principal};
use crate::catalog::CatalogState;
use crate::clock::format_timestamp;
use crate::error::OkfError;
use crate::http::api::context;
use crate::http::api::stores::{may_see_stores, places, StorePlace};
use crate::http::requester::Requester;
use crate::http::{archives, AppError};
use crate::labels::directory::{Labels, Wanted};
use crate::labels::{catalog_label, DisplayName};
use crate::mcp::catalogs::SuspensionInfo;
use crate::okf::ops::STARTER_FILE_COUNT;
use crate::store::audit::AuditActor;
use crate::store::catalogs::{self as catalog_rows, CatalogRow, MaintenanceChange, Switch};
use crate::store::ceiling::{self, MemberAccess};
use crate::store::keys::Access;
use crate::telemetry::subscriber::split_event;

/// This area's routes.
///
/// # Arguments
/// * `app` - the application (the import route's body cap comes from its config).
///
/// # Returns
/// The router. Every path parameter is `{catalog}`, as in the other areas:
/// axum refuses two routes that name the same position differently.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "router builder: runs once at startup, outside any request, so a span would be an orphan root"
    )
)]
pub(crate) fn routes(app: &App) -> Router<Arc<App>> {
    Router::new()
        // `get(a).post(b)` is one method router: one path, a handler per method.
        .route("/catalogs", get(list_catalogs).post(create_catalog))
        .route(
            "/catalogs/{catalog}",
            patch(update_catalog).delete(delete_catalog),
        )
        .route("/catalogs/{catalog}/owner", put(set_owner))
        .route("/catalogs/{catalog}/maintenance", put(set_maintenance))
        .route(
            "/catalogs/{catalog}/member-access",
            get(member_access).put(set_member_access),
        )
        .route("/catalogs/{catalog}/entitlements", get(team_entitlements))
        .route("/catalogs/{catalog}/export", archives::export_route())
        .route("/catalogs/{catalog}/import", archives::import_route(app))
}

/// One catalog as the console lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct CatalogSummary {
    /// Its name.
    pub name: String,
    /// Its stored display name; empty when unset.
    pub display_name: String,
    /// What people are shown for it: its display name, else its
    /// description (see [`catalog_label`]).
    pub label: String,
    /// Its description.
    pub description: String,
    /// The name of the team that owns it; null for a catalog from before
    /// users and teams not yet assigned.
    pub owner_team: Option<String>,
    /// The owner team's label; null when it has no owner team.
    pub owner_team_label: Option<String>,
    /// How many files it holds (every stored file).
    pub files: usize,
    /// How many of them are concepts.
    pub concepts: usize,
    /// The generation of the loaded snapshot: what an import's
    /// `expect_generation` compares against.
    pub generation: i64,
    /// Its newest change (`2026-11-02T21:00:00Z`), or null when empty.
    pub last_change: Option<String>,
    /// Set when its owner team is suspended (M13): `{"team", "notice"}`;
    /// null for an active catalog.
    pub suspended: Option<SuspensionInfo>,
    /// Whether the maintenance runner may check it (`on`, the default) or
    /// is kept out (`off`), as the key cache holds it.
    pub maintenance: Switch,
    /// The store holding its data and where that store is: the name, and
    /// `account-database` or the host, port and database, as `GET /stores`
    /// shows them. Left out (the key is absent) unless the caller may read
    /// `GET /stores` ([`may_see_stores`]); never a credential detail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<StorePlace>,
}

impl CatalogSummary {
    /// The summary of one listed catalog, as `principal` sees it.
    ///
    /// # Arguments
    /// * `row` - its directory row: name, display name, description, owner.
    /// * `state` - its loaded state, of the same incarnation as `row`: the
    ///   file and concept counts, generation and last change.
    /// * `principal` - the caller, which must reach the catalog (its
    ///   suspension is shown only to those who do).
    /// * `labels` - the label directory, for the owner team's label.
    /// * `places` - every store's place, for a caller who may see stores;
    ///   `None` for any other caller.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    fn of(
        row: &CatalogRow,
        state: &CatalogState,
        principal: &Principal,
        labels: &Labels,
        places: Option<&Places>,
    ) -> Self {
        // `current()` is an `Arc` of the loaded snapshot: one consistent
        // generation, file count and concept count.
        let loaded = state.current();
        Self {
            name: row.name.clone(),
            label: catalog_label(&row.display_name, &row.description),
            display_name: row.display_name.clone(),
            description: row.description.clone(),
            owner_team_label: row
                .owner_team
                .as_deref()
                .and_then(|team| labels.team(team))
                .map(str::to_string),
            owner_team: row.owner_team.clone(),
            files: loaded.files(),
            concepts: loaded.snapshot().concepts().count(),
            generation: loaded.generation(),
            last_change: loaded.last_change().map(format_timestamp),
            // Suspension and maintenance come from the catalog's row, as
            // every admission decision does.
            suspended: principal.suspension(row).map(SuspensionInfo::from),
            maintenance: Switch::from(!row.maintenance_off),
            store: places.and_then(|places| places.get(row.store.as_str()).cloned()),
        }
    }
}

/// Every store's place by name, read only for a caller who may see stores.
type Places = HashMap<String, StorePlace>;

/// The stores' places when `principal` may see them, else `None`.
///
/// # Errors
/// [`OkfError::Database`] for a failed read.
#[tracing::instrument(level = "debug", skip_all)]
async fn places_for(app: &App, principal: &Principal) -> Result<Option<Places>, OkfError> {
    if may_see_stores(principal) {
        Ok(Some(places(app).await?))
    } else {
        Ok(None)
    }
}

/// `GET /catalogs`' answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct CatalogListing {
    /// One page of the catalogs the key reaches, by name.
    pub catalogs: Vec<CatalogSummary>,
    /// The `after` of the next page; absent on the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    /// The ids of catalogs on this page left out because the store holding
    /// them could not be reached (sorted); absent when every store answered.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unavailable: Vec<String>,
    /// For a caller who may see stores ([`may_see_stores`]): the store of
    /// each catalog in `unavailable`, by catalog name, so the operator sees
    /// which store is down. Absent for any other caller and when every store
    /// answered.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub unavailable_stores: BTreeMap<String, String>,
}

/// `POST /catalogs`' body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NewCatalog {
    /// `[a-z0-9-]+`.
    name: String,
    /// Its display name, checked by [`DisplayName::parse`]; empty when left
    /// out.
    #[serde(default)]
    display_name: String,
    /// Free text; empty when left out, as with the CLI.
    #[serde(default)]
    description: String,
    /// The team that owns it. Required: a body without it is a 422.
    owner: String,
}

/// `PATCH /catalogs/{catalog}`'s body: the fields to change, at least one.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DetailsBody {
    /// The new display name, checked by [`DisplayName::parse`] (empty
    /// clears it); left as it is when absent.
    display_name: Option<String>,
    /// The new description (empty is allowed); left as it is when absent.
    description: Option<String>,
}

/// `PATCH /catalogs/{catalog}`'s answer: a strict superset of the one this
/// route gave when it changed only the description, so a client that reads
/// `name`, `previous` and `description` is unaffected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct DetailsChanged {
    /// The catalog.
    pub name: String,
    /// The description it had (always a string, whichever field changed).
    pub previous: String,
    /// The description it has now.
    pub description: String,
    /// The display name it had; empty when unset.
    pub previous_display_name: String,
    /// The display name it has now; empty when unset.
    pub display_name: String,
    /// What people are shown for it now.
    pub label: String,
}

/// `PUT /catalogs/{catalog}/owner`'s body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnerChange {
    /// The team that owns it from now on.
    team: String,
}

/// `PUT /catalogs/{catalog}/owner`'s answer, as `okf-mcp catalog set-owner`
/// prints it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct OwnerChanged {
    /// The catalog.
    pub name: String,
    /// The team that owned it; null when it was unassigned.
    pub previous: Option<String>,
    /// The team that owns it now.
    pub owner_team: String,
}

/// `DELETE /catalogs/{catalog}`'s query.
#[derive(Debug, Default, Deserialize)]
struct DeleteQuery {
    /// Delete even with unpublished changes in an open changeset.
    #[serde(default)]
    force: bool,
}

/// `DELETE /catalogs/{catalog}`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct CatalogDeleted {
    /// The catalog that was deleted.
    pub name: String,
}

/// `GET /catalogs?after=&limit=`: one page of the catalogs the key reaches,
/// any access level (M18 spec Decision 7).
///
/// With `readable=true` (issue #955) only the catalogs the key may also read
/// now (`require_unlock`: unlocked ones, and the team's own) are listed.
///
/// # Returns
/// The page, by name, with `next` while more follow. Each listed catalog's
/// snapshot fields (files, concepts, last change) come from the catalog
/// cache, so a page loads at most its own catalogs. A catalog whose stored
/// data does not build (or deleted since the page was read) is left out of
/// its page, as it was left out of the list.
///
/// # Errors
/// 400 for a malformed `after`; 500 if the directory read or a load fails.
/// A catalog whose store cannot be reached is left out and named in
/// `unavailable`.
#[tracing::instrument(level = "debug", skip_all)]
async fn list_catalogs(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    requester: Requester,
    Query(query): Query<PageQuery>,
) -> Result<Json<CatalogListing>, AppError> {
    // The call's context remembers which stores are down, so a store that
    // fails once costs this request one bound, not one per catalog on it.
    let ctx = context(&app, Arc::clone(&principal), requester);
    let page = navigable_page(&app, &principal, &query).await?;
    let mut wanted = Wanted::default();
    for row in &page.items {
        if let Some(team) = row.owner_team.as_deref() {
            wanted.team(team);
        }
    }
    let labels = app.labels().labels_for(&wanted).await?;
    let places = places_for(&app, &principal).await?;
    let mut catalogs = Vec::with_capacity(page.items.len());
    let mut unavailable = page.unavailable;
    // Rows on a store the page found down stay unloaded; the rest load
    // concurrently, and the results come back in page order.
    let mut loadable = Vec::with_capacity(page.items.len());
    for row in &page.items {
        if unavailable.contains(&row.name) {
            ctx.mark_down(row);
            continue;
        }
        loadable.push(row);
    }
    let states = ctx.listed_states(&loadable).await;
    for (row, state) in loadable.into_iter().zip(states) {
        match state {
            Ok(Some(state)) => catalogs.push(CatalogSummary::of(
                row,
                &state,
                &principal,
                &labels,
                places.as_ref(),
            )),
            Ok(None) => {}
            // The store went away since the page was read.
            Err(OkfError::StoreUnavailable { .. }) => {
                ctx.mark_down(row);
                unavailable.push(row.name.clone());
            }
            Err(error) => return Err(error.into()),
        }
    }
    unavailable.sort();
    // The store of each catalog left out, for a caller who may see stores.
    // Every id in `unavailable` is a row of this page.
    let unavailable_stores = if places.is_some() {
        page.items
            .iter()
            .filter(|row| unavailable.contains(&row.name))
            .map(|row| (row.name.clone(), row.store.to_string()))
            .collect()
    } else {
        BTreeMap::new()
    };
    Ok(Json(CatalogListing {
        catalogs,
        next: page.next,
        unavailable,
        unavailable_stores,
    }))
}

/// `POST /catalogs`: creates a catalog holding only its starter files (a
/// root `index.md` and a `log.md`); it is served at once. It needs an admin key that reaches every catalog: a key granted
/// named catalogs would not reach the new one, and grants are made with the
/// keys routes.
///
/// # Returns
/// 201 with the new catalog's summary.
///
/// # Errors
/// 403 for any other key; 404 ([`OkfError::UnknownName`]) for an unknown
/// owner; 409 ([`OkfError::Refused`]) for an invalid or taken name; 422 for a
/// body without `owner`, or a refused `display_name` ([`OkfError::LimitExceeded`]);
/// 500 for a database failure.
#[tracing::instrument(level = "debug", skip_all)]
async fn create_catalog(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    actor: AuditActor,
    // `Json` reads the body, so it must be the last extractor.
    Json(new): Json<NewCatalog>,
) -> Result<(StatusCode, Json<CatalogSummary>), AppError> {
    crate::http::api::require_manager(&principal)?;
    // Validated before the store sees it: a refused name is a 422 that
    // writes nothing.
    let display_name = DisplayName::parse(&new.display_name)?;
    let row = catalog_rows::create_in(
        app.stores(),
        &catalog_rows::NewCatalog {
            name: &new.name,
            display_name: &display_name,
            description: &new.description,
            owner_team: &new.owner,
        },
        &actor,
        app.clock().now(),
    )
    .await?;
    // The audit log records the change (`catalog.created`); so does the log line.
    split_event!(
        info,
        { catalog = %row.name, },
        { key = %principal.key_id, actor = %principal.actor, },
        "catalog created"
    );
    // The owner may be a suspended team: read the new row's access facts
    // from the database (one query), not from the directory, which may
    // still remember the name as absent until the create's message lands.
    let created = catalog_rows::row_by_name(app.accounts().executor(), &row.name).await?;
    let suspended = created
        .as_ref()
        .and_then(|created| principal.suspension(created))
        .map(SuspensionInfo::from);
    // Where it was placed, for a caller who may see stores.
    let store = match (&created, places_for(&app, &principal).await?) {
        (Some(created), Some(mut places)) => places.remove(created.store.as_str()),
        _ => None,
    };
    // Built from the row, not the loaded state: a new catalog holds only its
    // starter files (a root index.md and a log.md, no concepts), written
    // when it was created, and the answer must not depend on the catalog
    // cache having loaded it.
    let mut wanted = Wanted::default();
    if let Some(team) = row.owner_team.as_deref() {
        wanted.team(team);
    }
    let labels = app.labels().labels_for(&wanted).await?;
    let summary = CatalogSummary {
        name: row.name,
        label: catalog_label(&row.display_name, &row.description),
        display_name: row.display_name,
        description: row.description,
        owner_team_label: row
            .owner_team
            .as_deref()
            .and_then(|team| labels.team(team).map(str::to_string)),
        owner_team: row.owner_team,
        files: STARTER_FILE_COUNT,
        concepts: 0,
        generation: row.generation,
        last_change: Some(format_timestamp(row.created_at)),
        suspended,
        // `catalogs.maintenance` defaults to true and `create` never sets it.
        maintenance: Switch::On,
        store,
    };
    Ok((StatusCode::CREATED, Json(summary)))
}

/// Whether `principal`, a console session, may edit `catalog` as an owner
/// of its team: refused while the team is suspended (plan Decision 6; the
/// store does not check suspension).
///
/// The suspension is told only to a session that reaches the catalog or
/// belongs to its team, so a session cannot probe which other catalogs are
/// suspended. Anyone else goes on to the store, which refuses a non-owner.
///
/// # Arguments
/// * `app` - the application, for the catalog's row and the team labels.
/// * `principal` - the caller.
/// * `catalog` - the catalog to edit.
///
/// # Errors
/// [`OkfError::Suspended`]; [`OkfError::Database`] when the row's read fails.
#[tracing::instrument(level = "debug", skip_all)]
async fn refuse_suspended_owner(
    app: &App,
    principal: &Principal,
    catalog: &str,
) -> Result<(), OkfError> {
    // An unknown catalog is not suspended: the store refuses it.
    let Some(row) = app.directory().resolve(catalog).await? else {
        return Ok(());
    };
    let Some(suspended) = principal.suspension(&row) else {
        return Ok(());
    };
    let in_team = match principal.user.as_deref() {
        Some(user) => app.labels().teams_of(user).await?.contains(&suspended.team),
        None => false,
    };
    if in_team || principal.reaches_row(&row) {
        principal.check_not_suspended(&row)
    } else {
        Ok(())
    }
}

/// `PATCH /catalogs/{catalog}`: replaces the display name, the description
/// or both; the listing shows them at once (the write's own change
/// messages are applied before the answer).
///
/// Who may: a provisioner (it reaches no catalog content; this touches only
/// the row), a key or session with `admin` on the catalog, or a console
/// session of an owner of the catalog's owner team, refused while that team
/// is suspended. The store checks the owner under its locks.
///
/// # Errors
/// 403 for anyone else (a member who is not an owner, a key below `admin`,
/// and for a session an unknown catalog too); 404 for a catalog a key does
/// not reach or that does not exist; 403 with `Suspended` for an owner of a
/// suspended team; 422 for a refused display name, an unknown field or a
/// body with neither field; 500 for a database failure.
#[tracing::instrument(level = "debug", skip_all)]
async fn update_catalog(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    actor: AuditActor,
    Path(catalog): Path<String>,
    Json(change): Json<DetailsBody>,
) -> Result<Json<DetailsChanged>, AppError> {
    if change.display_name.is_none() && change.description.is_none() {
        return Err(OkfError::LimitExceeded {
            limit: "body",
            reason: "give display_name, description or both".to_string(),
        }
        .into());
    }
    let display_name = change
        .display_name
        .as_deref()
        .map(DisplayName::parse)
        .transpose()?;
    // Who is asking decides whether the store checks an owner. A tuple match
    // lists every combination: a provisioner and an admin pass as they are;
    // a console session may still be an owner; any other credential gets
    // the admin refusal.
    let admin = principal
        .require_in(app.directory(), Access::Admin, &catalog)
        .await;
    let owner = match (principal.provisioner, admin, &principal.kind) {
        (true, _, _) | (false, Ok(()), _) => None,
        // A failed read of the catalog's row is not a refusal.
        (false, Err(error @ (OkfError::Database { .. } | OkfError::Busy { .. })), _) => {
            return Err(error.into())
        }
        (false, Err(_), CredentialKind::Session { user_id }) => {
            refuse_suspended_owner(&app, &principal, &catalog).await?;
            Some(user_id.as_str())
        }
        (false, Err(refusal), CredentialKind::ApiKey | CredentialKind::OAuth { .. }) => {
            return Err(refusal.into())
        }
    };
    let details = catalog_rows::DetailsChange {
        catalog: &catalog,
        display_name: display_name.as_ref(),
        description: change.description.as_deref(),
        owner,
    };
    let previous = catalog_rows::update_details(
        app.accounts().executor(),
        &details,
        &actor,
        app.clock().now(),
    )
    .await?;
    split_event!(
        info,
        { %catalog, },
        { key = %principal.key_id, actor = %principal.actor, },
        "catalog details changed"
    );
    let description = change
        .description
        .unwrap_or_else(|| previous.description.clone());
    let display_name = display_name.map_or_else(
        || previous.display_name.clone(),
        |parsed| parsed.as_str().to_string(),
    );
    Ok(Json(DetailsChanged {
        name: catalog,
        previous: previous.description,
        label: catalog_label(&display_name, &description),
        description,
        previous_display_name: previous.display_name,
        display_name,
    }))
}

/// `PUT /catalogs/{catalog}/owner`: makes `team` the catalog's owner; the
/// listing shows it at once. Ownership grants no access, so it
/// needs an admin key that reaches every catalog, like the users and teams
/// routes (plan Decision 3), not `admin` on the catalog.
///
/// # Errors
/// 403 for any other key; 404 for an unknown catalog or team; 422 for a body
/// without `team`; 500 for a database failure.
#[tracing::instrument(level = "debug", skip_all)]
async fn set_owner(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    actor: AuditActor,
    Path(catalog): Path<String>,
    Json(change): Json<OwnerChange>,
) -> Result<Json<OwnerChanged>, AppError> {
    require_every_catalog(&principal)?;
    let previous = catalog_rows::set_owner(
        app.accounts().executor(),
        &catalog,
        &change.team,
        &actor,
        app.clock().now(),
    )
    .await?;
    split_event!(
        info,
        { %catalog, },
        { key = %principal.key_id, actor = %principal.actor, },
        "catalog owner changed"
    );
    Ok(Json(OwnerChanged {
        name: catalog,
        previous,
        owner_team: change.team,
    }))
}

/// `PUT /catalogs/{catalog}/maintenance`'s body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MaintenanceBody {
    /// `on` or `off`.
    maintenance: Switch,
}

/// `PUT /catalogs/{catalog}/maintenance`'s answer, as `okf-mcp catalog
/// set-maintenance` prints it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct MaintenanceChanged {
    /// The catalog.
    pub name: String,
    /// The flag before.
    pub previous: Switch,
    /// The flag now.
    pub maintenance: Switch,
}

/// `PUT /catalogs/{catalog}/maintenance`: lets the maintenance runner in, or
/// keeps it out (spec Part 6); the catalog directory refuses or admits the
/// runner at once. An admin that reaches every catalog, or an owner of
/// the catalog's owner team signed in to the console: the one
/// owner-managed control of this milestone. The store checks the owner
/// under its locks.
///
/// # Errors
/// 403 for anyone else (a non-owner's session, an unknown catalog included,
/// and every other key); 404 for an unknown catalog asked by an admin; 422
/// without `maintenance`, with another value or an unknown field; 500 for a
/// database failure.
#[tracing::instrument(level = "debug", skip_all)]
async fn set_maintenance(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    actor: AuditActor,
    Path(catalog): Path<String>,
    Json(body): Json<MaintenanceBody>,
) -> Result<Json<MaintenanceChanged>, AppError> {
    // Matching on a tuple decides on both at once. Anything but a console
    // session gets the admin refusal; a session may still be an owner.
    let owner = match (require_every_catalog(&principal), &principal.kind) {
        (Ok(()), _) => None,
        (Err(_), CredentialKind::Session { user_id }) => Some(user_id.as_str()),
        (Err(refusal), CredentialKind::ApiKey | CredentialKind::OAuth { .. }) => {
            return Err(refusal.into())
        }
    };
    let change = MaintenanceChange {
        catalog: &catalog,
        to: body.maintenance,
        owner,
    };
    let previous = catalog_rows::set_maintenance(
        app.accounts().executor(),
        &change,
        &actor,
        app.clock().now(),
    )
    .await?;
    split_event!(
        info,
        { %catalog, previous = previous.as_str(), maintenance = body.maintenance.as_str(), },
        { key = %principal.key_id, actor = %principal.actor, },
        "catalog maintenance set"
    );
    Ok(Json(MaintenanceChanged {
        name: catalog,
        previous,
        maintenance: body.maintenance,
    }))
}

/// `PUT /catalogs/{catalog}/member-access`'s body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MemberAccessBody {
    /// `none`, `read` or `write`.
    access: String,
}

/// `GET /catalogs/{catalog}/member-access`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct MemberAccessView {
    /// The catalog.
    pub name: String,
    /// `none`, `read` or `write`.
    pub member_access: &'static str,
}

/// `PUT /catalogs/{catalog}/member-access`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct MemberAccessChanged {
    /// The catalog.
    pub name: String,
    /// The level before.
    pub previous: &'static str,
    /// The level now.
    pub member_access: &'static str,
}

/// The refusal of a member-access or entitlement control for a key or an
/// OAuth grant, as it has always been.
const ADMIN_ONLY: &str = "this needs an admin key that reaches every catalog";

/// Whether an admin check refused because the caller does not reach the
/// catalog (the 404 these reads have always answered), as opposed to
/// reaching it below `admin` (403).
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: checks a value, no I/O"
    )
)]
fn is_unreached(admin: &Result<(), OkfError>) -> bool {
    matches!(admin, Err(OkfError::CatalogNotSelected { .. }))
}

/// An owner check's refusal for a session that does not reach the catalog
/// stays the 404 the admin check gave, so a session cannot tell an unknown
/// catalog from one it merely does not reach; any other error is kept.
///
/// # Arguments
/// * `error` - what the store answered.
/// * `unreached` - [`is_unreached`] of the admin check.
/// * `catalog` - the catalog asked about.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: maps a value, no I/O"
    )
)]
fn keep_not_found(error: OkfError, unreached: bool, catalog: &str) -> OkfError {
    match error {
        OkfError::Forbidden { .. } if unreached => OkfError::CatalogNotSelected {
            catalog: catalog.to_string(),
        },
        other => other,
    }
}

/// `GET /catalogs/{catalog}/member-access`: what the owning team's members
/// may reach through their self-issued credentials. For `admin` on the
/// catalog, or an owner of its shared team signed in to the console (issue
/// #352); the store checks the owner under its locks.
///
/// # Errors
/// 404 for a catalog the caller does not reach or that does not exist; 403
/// below `admin` on it, for a key or grant that is not an admin, and for a
/// session that reaches the catalog but does not own its shared team (a
/// personal team's catalog included).
#[tracing::instrument(level = "debug", skip_all)]
async fn member_access(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    Path(catalog): Path<String>,
) -> Result<Json<MemberAccessView>, AppError> {
    let admin = principal
        .require_in(app.directory(), Access::Admin, &catalog)
        .await;
    let unreached = is_unreached(&admin);
    let access = match owner_session(&principal, admin)? {
        None => ceiling::member_access(app.accounts().executor(), &catalog).await?,
        // The scalar only: no member or entitlement list is read for it.
        Some(owner) => ceiling::member_access_as_owner(app.accounts().executor(), &catalog, owner)
            .await
            .map_err(|refusal| keep_not_found(refusal, unreached, &catalog))?,
    };
    Ok(Json(MemberAccessView {
        name: catalog,
        member_access: access.as_str(),
    }))
}

/// `PUT /catalogs/{catalog}/member-access`: sets it; every member's
/// self-issued credentials are re-capped at once. The permission is
/// `ceiling::may_set_member_access` (a provisioner or an every-catalog
/// admin), or an owner of the catalog's shared team signed in to the
/// console (issue #352), which the store checks under its locks. Keys and
/// OAuth grants, an owner's included, get the admin refusal.
///
/// # Errors
/// 403 for a caller `may_set_member_access` refuses that is not such an
/// owner (a non-owner's session, an unknown catalog and a personal team's
/// catalog included); 400 for an unknown level; 404 for an unknown catalog
/// asked by an admin; 422 without `access`.
#[tracing::instrument(level = "debug", skip_all)]
async fn set_member_access(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    actor: AuditActor,
    Path(catalog): Path<String>,
    Json(body): Json<MemberAccessBody>,
) -> Result<Json<MemberAccessChanged>, AppError> {
    let admin = if ceiling::may_set_member_access(&principal, &catalog) {
        Ok(())
    } else {
        Err(OkfError::Forbidden {
            reason: ADMIN_ONLY.to_string(),
        })
    };
    let owner = owner_session(&principal, admin)?;
    let access = MemberAccess::parse(&body.access).ok_or_else(|| OkfError::InvalidPath {
        path: "access".to_string(),
        reason: format!("must be none, read or write, not {:?}", body.access),
    })?;
    let change = ceiling::MemberAccessChange {
        catalog: &catalog,
        access,
        owner,
    };
    let previous = ceiling::change_member_access(
        app.accounts().executor(),
        &change,
        &actor,
        app.clock().now(),
    )
    .await?;
    split_event!(
        info,
        { %catalog, previous = previous.as_str(), access = access.as_str(), },
        { key = %principal.key_id, actor = %principal.actor, },
        "member access set"
    );
    Ok(Json(MemberAccessChanged {
        name: catalog,
        previous: previous.as_str(),
        member_access: access.as_str(),
    }))
}

/// One member of a catalog's owning team, as the drawer picks entitlement
/// targets from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct TeamMemberView {
    /// The user's name.
    pub user: String,
    /// The user's label, as `GET /labels` gives it.
    pub label: String,
    /// `owner` or `member`.
    pub role: String,
}

/// One entitlement to the catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct CatalogEntitlementView {
    /// The user's name.
    pub user: String,
    /// The user's label.
    pub label: String,
    /// `read` or `write`.
    pub access: &'static str,
}

/// `GET /catalogs/{catalog}/entitlements`' answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct TeamEntitlementsView {
    /// The catalog.
    pub name: String,
    /// The owning team's members, the only users an owner may entitle.
    pub members: Vec<TeamMemberView>,
    /// The last member of this page when more follow: send it back as
    /// `members_after`; `null` on the last page.
    pub members_next: Option<String>,
    /// The entitlements to the catalog (an owner sees its members' only).
    pub entitlements: Vec<CatalogEntitlementView>,
    /// As `members_next`, for `entitlements_after`.
    pub entitlements_next: Option<String>,
}

/// `GET /catalogs/{catalog}/entitlements`' query: the cursors of the two
/// lists, the page size and which list to read.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TeamEntitlementsQuery {
    /// The `members_next` of the previous page; absent for the first.
    members_after: Option<String>,
    /// The `entitlements_next` of the previous page; absent for the first.
    entitlements_after: Option<String>,
    /// The page size of each list: default 100, at most 1000 (a larger one
    /// is cut).
    limit: Option<u32>,
    /// `members` or `entitlements` to read only that list (a continuation);
    /// absent for both.
    list: Option<String>,
}

/// `GET /catalogs/{catalog}/entitlements`: the owning team's members and the
/// catalog's entitlements, for the catalog drawer's entitlement section
/// (issue #352). The same callers and refusals as
/// `GET /catalogs/{catalog}/member-access`.
///
/// # Errors
/// As `GET /catalogs/{catalog}/member-access`.
#[tracing::instrument(level = "debug", skip_all)]
async fn team_entitlements(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    Path(catalog): Path<String>,
    Query(query): Query<TeamEntitlementsQuery>,
) -> Result<Json<TeamEntitlementsView>, AppError> {
    let lists = match query.list.as_deref() {
        None => ceiling::TeamLists::Both,
        Some("members") => ceiling::TeamLists::Members,
        Some("entitlements") => ceiling::TeamLists::Entitlements,
        Some(other) => {
            return Err(OkfError::InvalidPath {
                path: "list".to_string(),
                reason: format!("must be members or entitlements, not {other:?}"),
            }
            .into())
        }
    };
    let page = ceiling::TeamPage {
        members_after: query.members_after.as_deref().unwrap_or(""),
        entitlements_after: query.entitlements_after.as_deref().unwrap_or(""),
        limit: i64::from(query.limit.filter(|limit| *limit > 0).unwrap_or(100)),
        lists,
    };
    let admin = principal
        .require_in(app.directory(), Access::Admin, &catalog)
        .await;
    let unreached = is_unreached(&admin);
    let owner = owner_session(&principal, admin)?;
    let access = ceiling::team_access(app.accounts().executor(), &catalog, owner, &page)
        .await
        .map_err(|refusal| keep_not_found(refusal, unreached, &catalog))?;
    let mut wanted = Wanted::default();
    for member in &access.members {
        wanted.user(&member.user);
    }
    for entitlement in &access.entitlements {
        wanted.user(&entitlement.user);
    }
    let labels = app.labels().labels_for(&wanted).await?;
    // A user created since the last load has no label yet: show the slug.
    let label_of = |user: &str| labels.user(user).unwrap_or(user).to_string();
    Ok(Json(TeamEntitlementsView {
        name: catalog,
        members: access
            .members
            .into_iter()
            .map(|member| TeamMemberView {
                label: label_of(&member.user),
                user: member.user,
                role: member.role,
            })
            .collect(),
        entitlements: access
            .entitlements
            .into_iter()
            .map(|entitlement| CatalogEntitlementView {
                label: label_of(&entitlement.user),
                user: entitlement.user,
                access: entitlement.access.as_str(),
            })
            .collect(),
        members_next: access.members_next,
        entitlements_next: access.entitlements_next,
    }))
}

/// `DELETE /catalogs/{catalog}[?force=true]`: deletes the catalog with its
/// documents, history, changesets and grants (`admin` on the catalog); it is
/// gone at once.
///
/// # Errors
/// 404 for a catalog the key does not reach or that does not exist; 403
/// below `admin`; 409 ([`OkfError::Refused`]) with unpublished changes and
/// no `force`; 500 for a database failure.
#[tracing::instrument(level = "debug", skip_all)]
async fn delete_catalog(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    actor: AuditActor,
    Path(catalog): Path<String>,
    Query(query): Query<DeleteQuery>,
) -> Result<Json<CatalogDeleted>, AppError> {
    if !principal.provisioner {
        principal
            .require_in(app.directory(), Access::Admin, &catalog)
            .await?;
    }
    catalog_rows::delete_in(
        app.stores(),
        &catalog,
        query.force,
        &actor,
        app.clock().now(),
    )
    .await
    // The store's hint names the CLI flag; say what HTTP needs.
    .map_err(|error| match error {
        OkfError::Refused { reason } => OkfError::Refused {
            reason: reason.replace("pass --force", "add ?force=true"),
        },
        other => other,
    })?;
    split_event!(
        info,
        { %catalog, force = query.force, },
        { key = %principal.key_id, actor = %principal.actor, },
        "catalog deleted"
    );
    Ok(Json(CatalogDeleted { name: catalog }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{HeaderMap, Request};
    use chrono::Utc;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;
    use tower::ServiceExt;

    use crate::db::TEST_TEAM;
    use crate::http::api::testing::{
        app, assert_matrix, assert_matrix_with, every_catalog_admin, keys, send, Browser, Payload,
        Row,
    };
    use crate::http::archives::tarball_of;
    use crate::mcp::testing::{open, pending_entry, writer, PendingEdit, TestApp};
    use crate::store::audit::AuditActor;
    use crate::store::users::AdminChange;

    /// One request with a raw body through the whole router; the status,
    /// the headers and the body bytes.
    async fn raw(
        test: &TestApp,
        method: &str,
        uri: &str,
        key: &str,
        body: Vec<u8>,
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {key}"))
            .body(Body::from(body))
            .expect("a valid request");
        let response = crate::http::router(Arc::clone(&test.app), &CancellationToken::new())
            .oneshot(request)
            .await
            .expect("the router answers");
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read the body");
        (status, headers, bytes.to_vec())
    }

    /// The names a `GET /catalogs` lists for `key`.
    async fn names(test: &TestApp, key: &str) -> Vec<String> {
        let (status, body) = send(test, "GET", "/api/v1/catalogs", Some(key), None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["catalogs"]
            .as_array()
            .expect("a list")
            .iter()
            .map(|catalog| catalog["name"].as_str().expect("a name").to_string())
            .collect()
    }

    #[tokio::test]
    async fn member_access_reads_need_admin_on_the_catalog_and_writes_an_every_catalog_admin() {
        let test = app().await;
        let keys = keys(&test).await;
        let root = every_catalog_admin(&test).await;
        assert_matrix_with(
            &test,
            &keys,
            &[
                Row {
                    method: "GET",
                    uri: "/api/v1/catalogs/main/member-access".to_string(),
                    body: None,
                    read: 403,
                    write: 403,
                    admin: 200,
                    stranger: 404,
                },
                Row {
                    method: "PUT",
                    uri: "/api/v1/catalogs/main/member-access".to_string(),
                    body: Some(json!({"access": "read"})),
                    read: 403,
                    write: 403,
                    admin: 403,
                    stranger: 403,
                },
            ],
        )
        .await;
        let put = |catalog: &str, access: &str| {
            let uri = format!("/api/v1/catalogs/{catalog}/member-access");
            let body = json!({ "access": access });
            let (test, root) = (&test, root.as_str());
            async move { send(test, "PUT", &uri, Some(root), Some(body)).await }
        };

        let (_, before) = send(
            &test,
            "GET",
            "/api/v1/catalogs/main/member-access",
            Some(&root),
            None,
        )
        .await;
        let (set, body) = put("main", "read").await;
        let (_, now) = send(
            &test,
            "GET",
            "/api/v1/catalogs/main/member-access",
            Some(&root),
            None,
        )
        .await;
        let (bad, _) = put("main", "admin").await;
        let (unknown, _) = put("nowhere", "read").await;
        let (unknown_read, _) = send(
            &test,
            "GET",
            "/api/v1/catalogs/nowhere/member-access",
            Some(&root),
            None,
        )
        .await;

        assert_eq!(before, json!({"name": "main", "member_access": "write"}));
        assert_eq!(set.as_u16(), 200, "{body}");
        assert_eq!(
            body,
            json!({"name": "main", "previous": "write", "member_access": "read"})
        );
        assert_eq!(now, json!({"name": "main", "member_access": "read"}));
        assert_eq!(bad.as_u16(), 400);
        assert_eq!(unknown.as_u16(), 404);
        assert_eq!(unknown_read.as_u16(), 404);
    }

    #[tokio::test]
    async fn member_access_follows_the_session_rules_too() {
        // An admin user's session reaches every catalog and may set member
        // access; a plain user's session is below admin everywhere.
        let test = app().await;
        let root = every_catalog_admin(&test).await;
        let (status, body) = send(
            &test,
            "POST",
            "/api/v1/users",
            Some(&root),
            Some(json!({"name": "alice"})),
        )
        .await;
        assert_eq!(status.as_u16(), 201, "{body}");
        let mut plain = Browser::session_for(&test, "alice").await;
        let (read_plain, _) = plain
            .send(
                &test,
                "GET",
                "/api/v1/catalogs/main/member-access",
                Payload::Empty,
            )
            .await
            .parts();
        let (write_plain, _) = plain
            .send(
                &test,
                "PUT",
                "/api/v1/catalogs/main/member-access",
                Payload::Json(json!({"access": "none"})),
            )
            .await
            .parts();
        crate::invalidate::testing::applied(&test.app, async {
            crate::store::users::set_admin(
                test.app.accounts().executor(),
                "alice",
                AdminChange {
                    on: true,
                    allow_last: false,
                },
                &AuditActor::cli(),
                Utc::now(),
            )
            .await
            .expect("make alice an admin");
        })
        .await;
        let mut admin = Browser::session_for(&test, "alice").await;
        let (write_admin, answer) = admin
            .send(
                &test,
                "PUT",
                "/api/v1/catalogs/main/member-access",
                Payload::Json(json!({"access": "none"})),
            )
            .await
            .parts();

        assert_eq!(
            read_plain.as_u16(),
            404,
            "a plain session that does not reach the catalog"
        );
        assert_eq!(write_plain.as_u16(), 403);
        assert_eq!(write_admin.as_u16(), 200, "{answer}");
        assert_eq!(answer["member_access"], "none");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_catalog_routes_follow_the_access_matrix() {
        let test = app().await;
        // The delete runs last: it removes `main`. Every key in the matrix
        // reaches one catalog only, so none may create one or change an
        // owner (all 403).
        assert_matrix(
            &test,
            &[
                Row {
                    method: "GET",
                    uri: "/api/v1/catalogs".into(),
                    body: None,
                    read: 200,
                    write: 200,
                    admin: 200,
                    stranger: 200,
                },
                Row {
                    method: "GET",
                    uri: "/api/v1/catalogs/main/export".into(),
                    body: None,
                    read: 200,
                    write: 200,
                    admin: 200,
                    stranger: 404,
                },
                // `{}` is not gzip: a writer or an admin gets past the key
                // check to 400 (import needs `write`, issue #260).
                Row {
                    method: "POST",
                    uri: "/api/v1/catalogs/main/import".into(),
                    body: Some(json!({})),
                    read: 403,
                    write: 400,
                    admin: 400,
                    stranger: 404,
                },
                Row {
                    method: "POST",
                    uri: "/api/v1/catalogs".into(),
                    body: Some(json!({ "name": "made-in-matrix", "owner": TEST_TEAM })),
                    read: 403,
                    write: 403,
                    admin: 403,
                    stranger: 403,
                },
                Row {
                    method: "PUT",
                    uri: "/api/v1/catalogs/main/owner".into(),
                    body: Some(json!({ "team": TEST_TEAM })),
                    read: 403,
                    write: 403,
                    admin: 403,
                    stranger: 403,
                },
                Row {
                    method: "PATCH",
                    uri: "/api/v1/catalogs/main".into(),
                    body: Some(json!({ "description": "patched" })),
                    read: 403,
                    write: 403,
                    admin: 200,
                    stranger: 404,
                },
                Row {
                    method: "DELETE",
                    uri: "/api/v1/catalogs/main?force=true".into(),
                    body: None,
                    read: 403,
                    write: 403,
                    admin: 200,
                    stranger: 404,
                },
            ],
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_list_shows_reachable_catalogs_with_counts_and_generation() {
        let test = app().await;
        let keys = keys(&test).await;
        let generation =
            catalog_rows::test_generation(test.app.accounts().executor(), "main").await;

        let (status, body) = send(&test, "GET", "/api/v1/catalogs", Some(&keys.read), None).await;

        assert_eq!(status, StatusCode::OK);
        let main = &body["catalogs"][0];
        assert_eq!(body["catalogs"].as_array().map(Vec::len), Some(1));
        assert_eq!(main["name"], "main");
        assert_eq!(main["description"], "");
        assert_eq!(main["owner_team"], TEST_TEAM);
        assert_eq!(main["files"], 6);
        // systems/alpha.md and systems/beta.md; index, log and AGENTS.md are not concepts.
        assert_eq!(main["concepts"], 2);
        assert_eq!(main["generation"], generation);
        assert!(main["last_change"].is_string());
        assert_eq!(names(&test, &keys.stranger).await, ["other"]);
    }

    /// M13 Task 4: the listing marks a suspended catalog rather than
    /// hiding it, to every key that reaches it, and a catalog created for a
    /// suspended team is marked in the answer.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_list_marks_a_suspended_catalog_and_never_hides_it() {
        use crate::mcp::testing::{suspend_catalogs, SUSPENDED_TEAM, SUSPENSION_NOTICE};
        let test = app().await;
        let keys = keys(&test).await;
        let root = every_catalog_admin(&test).await;
        suspend_catalogs(&test, &["main"]).await;
        let listed = |body: &serde_json::Value, name: &str| -> serde_json::Value {
            body["catalogs"]
                .as_array()
                .expect("a list")
                .iter()
                .find(|catalog| catalog["name"] == name)
                .cloned()
                .unwrap_or_else(|| panic!("{name} in {body}"))
        };
        let marked =
            json!({ "team": SUSPENDED_TEAM, "team_label": "Family", "notice": SUSPENSION_NOTICE });

        let (_, readers) = send(&test, "GET", "/api/v1/catalogs", Some(&keys.read), None).await;
        let (_, roots) = send(&test, "GET", "/api/v1/catalogs", Some(&root), None).await;
        let (_, strangers) =
            send(&test, "GET", "/api/v1/catalogs", Some(&keys.stranger), None).await;
        let (created, in_suspended) = send(
            &test,
            "POST",
            "/api/v1/catalogs",
            Some(&root),
            Some(json!({ "name": "paused", "owner": SUSPENDED_TEAM })),
        )
        .await;
        let (_, in_active) = send(
            &test,
            "POST",
            "/api/v1/catalogs",
            Some(&root),
            Some(json!({ "name": "active", "owner": TEST_TEAM })),
        )
        .await;

        assert_eq!(listed(&readers, "main")["suspended"], marked);
        assert_eq!(
            listed(&roots, "main")["suspended"],
            marked,
            "an admin sees it too"
        );
        // `get`, not `[]`: a missing key would read as null too.
        assert_eq!(
            listed(&roots, "other").get("suspended"),
            Some(&serde_json::Value::Null)
        );
        assert_eq!(names(&test, &keys.stranger).await, ["other"]);
        assert_eq!(
            listed(&strangers, "other").get("suspended"),
            Some(&serde_json::Value::Null),
            "a key that does not reach main never learns it is suspended"
        );
        assert_eq!(created, StatusCode::CREATED, "{in_suspended}");
        assert_eq!(in_suspended["suspended"]["team"], SUSPENDED_TEAM);
        assert_eq!(in_active.get("suspended"), Some(&serde_json::Value::Null));
    }

    /// The catalog in `body`'s list called `name`.
    fn listed_catalog(body: &serde_json::Value, name: &str) -> serde_json::Value {
        body["catalogs"]
            .as_array()
            .expect("a list")
            .iter()
            .find(|catalog| catalog["name"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("{name} in {body}"))
    }

    /// The keys of a JSON object, sorted.
    fn keys_of(value: &serde_json::Value) -> Vec<String> {
        let mut keys: Vec<String> = value
            .as_object()
            .expect("an object")
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    }

    /// Issue #1030: an admin's listing and a new catalog's answer name the
    /// store each catalog is on, in the shape `GET /stores` gives; a key
    /// below admin, and a provisioner, get no `store` key at all.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_admin_sees_each_catalogs_store_and_no_one_else_does() {
        // Arrange
        let test = app().await;
        test.catalog_on_new_store("store-b", "remote", &[("note.md", "# Remote\n")])
            .await;
        let keys = keys(&test).await;
        let root = every_catalog_admin(&test).await;
        let provisioner = crate::store::keys::create_provisioner(
            test.app.accounts().executor(),
            &crate::store::keys::NewProvisioner {
                name: "signup",
                actor: "signup",
                user: crate::db::TEST_USER,
            },
            &AuditActor::cli(),
            Utc::now(),
        )
        .await
        .expect("a provisioner key")
        .key;

        // Act
        let (_, as_root) = send(&test, "GET", "/api/v1/catalogs", Some(&root), None).await;
        let (_, as_scoped_admin) =
            send(&test, "GET", "/api/v1/catalogs", Some(&keys.admin), None).await;
        let (_, as_reader) = send(&test, "GET", "/api/v1/catalogs", Some(&keys.read), None).await;
        let (_, as_writer) = send(&test, "GET", "/api/v1/catalogs", Some(&keys.write), None).await;
        let (_, stores) = send(&test, "GET", "/api/v1/stores", Some(&root), None).await;
        let (created, by_root) = send(
            &test,
            "POST",
            "/api/v1/catalogs",
            Some(&root),
            Some(json!({ "name": "fresh", "owner": TEST_TEAM })),
        )
        .await;
        let (created_by_provisioner, by_provisioner) = send(
            &test,
            "POST",
            "/api/v1/catalogs",
            Some(&provisioner),
            Some(json!({ "name": "fresh-too", "owner": TEST_TEAM })),
        )
        .await;

        // Assert: each catalog reports its own store, in `GET /stores`' shape
        // and with nothing beyond it (no credential kind or reference).
        let place = ["database", "host", "location", "name", "port"];
        for (catalog, store_name, location) in [
            ("main", "main", "account-database"),
            ("other", "main", "account-database"),
            ("remote", "store-b", "host"),
        ] {
            let store = &listed_catalog(&as_root, catalog)["store"];
            assert_eq!(store["name"], store_name, "{catalog}: {store}");
            assert_eq!(store["location"], location, "{catalog}: {store}");
            assert_eq!(keys_of(store), place, "{catalog}: {store}");
            let entry = stores["stores"]
                .as_array()
                .expect("the stores")
                .iter()
                .find(|entry| entry["name"] == store_name)
                .unwrap_or_else(|| panic!("{store_name} in {stores}"));
            for field in place {
                assert_eq!(store[field], entry[field], "{catalog}.{field}");
            }
        }
        assert!(listed_catalog(&as_root, "remote")["store"]["host"].is_string());
        assert!(listed_catalog(&as_root, "remote")["store"]["port"].is_u64());
        assert_eq!(
            listed_catalog(&as_scoped_admin, "main")["store"]["name"],
            "main",
            "any admin may read GET /stores, so any admin sees it"
        );
        // Not even a null: the answer is the one non-admins got before.
        for (who, body) in [("read", &as_reader), ("write", &as_writer)] {
            let main = listed_catalog(body, "main");
            assert!(main.get("store").is_none(), "{who}: {main}");
            assert!(!keys_of(&main).contains(&"store".to_string()), "{who}");
        }
        assert_eq!(created, StatusCode::CREATED, "{by_root}");
        assert_eq!(by_root["store"]["name"], "main");
        assert_eq!(
            created_by_provisioner,
            StatusCode::CREATED,
            "{by_provisioner}"
        );
        assert!(by_provisioner.get("store").is_none(), "{by_provisioner}");
    }

    /// Issue #1030: a catalog left out of a listing because its store is
    /// down is named by store for an admin, in a field a client that reads
    /// only `unavailable` never sees; a non-admin sees `unavailable` alone.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_unavailable_catalog_is_named_by_store_for_an_admin_only() {
        // Arrange
        let test = app().await;
        let store = test
            .catalog_on_new_store("store-b", "remote", &[("note.md", "# Remote\n")])
            .await;
        test.make_unreachable(&store).await;
        let root = every_catalog_admin(&test).await;
        let grants = ["main".to_string(), "remote".to_string()];
        let reader = key_rows_for(&test, "remote-reader", &grants).await;

        // Act
        let (_, as_root) = send(&test, "GET", "/api/v1/catalogs", Some(&root), None).await;
        let (_, as_reader) = send(&test, "GET", "/api/v1/catalogs", Some(&reader), None).await;

        // Assert
        assert_eq!(as_root["unavailable"], json!(["remote"]), "{as_root}");
        assert_eq!(
            as_root["unavailable_stores"],
            json!({ "remote": "store-b" }),
            "{as_root}"
        );
        assert_eq!(as_reader["unavailable"], json!(["remote"]), "{as_reader}");
        assert!(as_reader.get("unavailable_stores").is_none(), "{as_reader}");
        assert_eq!(
            listed_catalog(&as_root, "main")["store"]["name"],
            "main",
            "the reachable catalogs still list"
        );
    }

    /// Issue #1030: `GET /stores` counts the catalogs each store holds.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_stores_list_counts_each_stores_catalogs() {
        let test = app().await;
        test.catalog_on_new_store("store-b", "remote", &[("note.md", "# Remote\n")])
            .await;
        let root = every_catalog_admin(&test).await;

        let (status, body) = send(&test, "GET", "/api/v1/stores", Some(&root), None).await;

        assert_eq!(status, StatusCode::OK, "{body}");
        let count = |name: &str| {
            body["stores"]
                .as_array()
                .expect("the stores")
                .iter()
                .find(|store| store["name"] == name)
                .map(|store| store["catalogs"].clone())
        };
        assert_eq!(count("main"), Some(json!(2)), "main and other: {body}");
        assert_eq!(count("store-b"), Some(json!(1)), "remote: {body}");
    }

    /// A read key named `name` that reaches `catalogs`.
    async fn key_rows_for(test: &TestApp, name: &'static str, catalogs: &[String]) -> String {
        use crate::store::keys::{create_with_id, Grants, NewKey};
        let new = NewKey {
            name,
            access: Access::Read,
            maintenance_report: false,
            grants: Grants::Catalogs(catalogs),
            default_catalog: Some("main"),
            actor: name,
            user: crate::db::TEST_USER,
            self_issued: false,
            expires_at: None,
            levels: &[],
        };
        create_with_id(
            test.app.accounts().executor(),
            &new,
            &AuditActor::cli(),
            Utc::now(),
        )
        .await
        .expect("create a key")
        .key
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_stores_a_parsed_display_name_and_refuses_a_bad_one() {
        // Arrange
        let test = app().await;
        let root = every_catalog_admin(&test).await;

        // Act: one name the rule trims, one it refuses.
        let (created, body) = send(
            &test,
            "POST",
            "/api/v1/catalogs",
            Some(&root),
            Some(json!({ "name": "work", "display_name": "  Work notes ", "owner": TEST_TEAM })),
        )
        .await;
        let (refused, error) = send(
            &test,
            "POST",
            "/api/v1/catalogs",
            Some(&root),
            Some(json!({ "name": "bad", "display_name": "a\nb", "owner": TEST_TEAM })),
        )
        .await;

        // Assert: stored trimmed; the refusal is a 422 that wrote nothing.
        assert_eq!(created, StatusCode::CREATED, "{body}");
        let row = catalog_rows::find(test.app.accounts().executor(), "work")
            .await
            .unwrap();
        assert_eq!(row.display_name, "Work notes");
        assert_eq!(refused, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
        assert!(matches!(
            catalog_rows::find(test.app.accounts().executor(), "bad").await,
            Err(OkfError::CatalogNotSelected { .. })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_every_catalog_admin_creates_a_catalog_that_is_served_at_once() {
        let test = app().await;
        let keys = keys(&test).await;
        let root = every_catalog_admin(&test).await;
        let new = json!({ "name": "scratch", "description": "Scratch space", "owner": TEST_TEAM });

        let (status, created) = send(
            &test,
            "POST",
            "/api/v1/catalogs",
            Some(&root),
            Some(new.clone()),
        )
        .await;
        let (again, taken) = send(&test, "POST", "/api/v1/catalogs", Some(&root), Some(new)).await;
        let (invalid, _) = send(
            &test,
            "POST",
            "/api/v1/catalogs",
            Some(&root),
            Some(json!({ "name": "Not Valid", "owner": TEST_TEAM })),
        )
        .await;
        let (unowned, _) = send(
            &test,
            "POST",
            "/api/v1/catalogs",
            Some(&root),
            Some(json!({ "name": "unowned" })),
        )
        .await;
        let (unknown_owner, nobody) = send(
            &test,
            "POST",
            "/api/v1/catalogs",
            Some(&root),
            Some(json!({ "name": "orphan", "owner": "nobody" })),
        )
        .await;
        let (scoped, refused) = send(
            &test,
            "POST",
            "/api/v1/catalogs",
            Some(&keys.admin),
            Some(json!({ "name": "mine", "owner": TEST_TEAM })),
        )
        .await;

        assert_eq!(status, StatusCode::CREATED, "{created}");
        assert_eq!(created["name"], "scratch");
        assert_eq!(created["description"], "Scratch space");
        assert_eq!(created["owner_team"], TEST_TEAM);
        // Its starter files: a root index.md and a log.md, no concepts.
        assert_eq!(created["files"], 2);
        assert_eq!(created["concepts"], 0);
        assert!(created["last_change"].is_string(), "{created}");
        let served = test
            .app
            .catalog("scratch")
            .await
            .unwrap()
            .expect("loaded at once");
        assert_eq!(served.current().files(), 2);
        assert!(served.current().snapshot().get("index.md").is_some());
        // Polled at once: loaded, and listed for a key that reaches it.
        assert!(test.app.catalog("scratch").await.unwrap().is_some());
        assert_eq!(names(&test, &root).await, ["main", "other", "scratch"]);
        assert_eq!(names(&test, &keys.read).await, ["main"]);
        assert_eq!(again, StatusCode::CONFLICT);
        assert!(taken["message"]
            .as_str()
            .unwrap()
            .contains("already exists"));
        assert_eq!(invalid, StatusCode::CONFLICT);
        // Every new catalog has an owner (Decision 2: a missing field is 422).
        assert_eq!(unowned, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(unknown_owner, StatusCode::NOT_FOUND, "{nobody}");
        assert_eq!(names(&test, &root).await, ["main", "other", "scratch"]);
        assert_eq!(scoped, StatusCode::FORBIDDEN);
        assert!(
            refused["message"]
                .as_str()
                .unwrap()
                .contains("every catalog"),
            "{refused}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_admin_changes_a_description_and_the_list_shows_it() {
        let test = app().await;
        let keys = keys(&test).await;

        let (status, changed) = send(
            &test,
            "PATCH",
            "/api/v1/catalogs/main",
            Some(&keys.admin),
            Some(json!({ "description": "The main catalog" })),
        )
        .await;
        let (_, listed) = send(&test, "GET", "/api/v1/catalogs", Some(&keys.read), None).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            changed,
            json!({ "name": "main", "previous": "",
                                    "description": "The main catalog",
                                    "previous_display_name": "", "display_name": "",
                                    "label": "The main catalog" })
        );
        assert_eq!(listed["catalogs"][0]["description"], "The main catalog");
    }

    /// Issue #752: the DELETE does not carry the catalog's bulk data with it.
    /// It marks the catalog deleted and owes the cleanup (a durable row the
    /// housekeeping task drains), so its cost does not grow with the
    /// documents and revisions, and the answer arrives while the data is
    /// still on the store. An unrelated catalog stays readable throughout;
    /// a second DELETE of the same catalog is a 404; and the name is
    /// reserved until the cleanup has run.
    #[tokio::test(flavor = "multi_thread")]
    async fn delete_returns_before_the_bulk_data_is_cleaned_up() {
        let test = app().await;
        let keys = keys(&test).await;
        let id: i64 = sqlx::query_scalar("SELECT id FROM catalogs WHERE name = 'main'")
            .fetch_one(test.app.accounts().executor())
            .await
            .unwrap();
        let store = crate::db::store_pool(test.app.accounts().executor());
        sqlx::query(
            "INSERT INTO catalog.documents (catalog_id, path, content, executable, revision, \
             updated_at, updated_by) \
             SELECT $1, 'bulk/d' || g || '.md', convert_to('# Bulk ' || g, 'UTF8'), false, 3, \
             now(), 'seed' FROM generate_series(1, 500) g",
        )
        .bind(id)
        .execute(&store)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO catalog.revisions (catalog_id, path, revision, op, content, executable, \
             actor, message, created_at) \
             SELECT $1, 'bulk/d' || g || '.md', r, 'update', convert_to('# Bulk ' || g, 'UTF8'), \
             false, 'seed', 'm', now() FROM generate_series(1, 500) g, generate_series(1, 3) r",
        )
        .bind(id)
        .execute(&store)
        .await
        .unwrap();
        // (revisions, documents) the store holds for the catalog.
        let rows = || {
            let store = store.clone();
            async move {
                sqlx::query_as::<_, (i64, i64)>(
                    "SELECT (SELECT count(*) FROM catalog.revisions WHERE catalog_id = $1), \
                            (SELECT count(*) FROM catalog.documents WHERE catalog_id = $1)",
                )
                .bind(id)
                .fetch_one(&store)
                .await
                .unwrap()
            }
        };

        let before = rows().await;
        assert!(before.0 >= 1500 && before.1 >= 500);

        let (status, deleted) = send(
            &test,
            "DELETE",
            "/api/v1/catalogs/main?force=true",
            Some(&keys.admin),
            None,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(deleted, json!({ "name": "main" }));
        // Gone from every lookup, with its data still to be removed.
        assert!(test.app.catalog("main").await.unwrap().is_none());
        assert_eq!(rows().await, before, "the data waits for the cleanup");
        let owed: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM store_cleanups WHERE kind = 'catalog' AND subject = $1",
        )
        .bind(id.to_string())
        .fetch_one(test.app.accounts().executor())
        .await
        .unwrap();
        assert_eq!(owed, 1, "the cleanup is owed");
        // Unrelated catalogs stay usable, and a repeat is a 404.
        let (unrelated, _) =
            send(&test, "GET", "/api/v1/catalogs", Some(&keys.stranger), None).await;
        let (again, _) = send(
            &test,
            "DELETE",
            "/api/v1/catalogs/main?force=true",
            Some(&keys.admin),
            None,
        )
        .await;
        assert_eq!(unrelated, StatusCode::OK);
        assert_eq!(again, StatusCode::NOT_FOUND);

        test.settle_cleanups().await;

        assert_eq!(rows().await, (0, 0));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn delete_refuses_unpublished_work_unless_forced() {
        let test = app().await;
        let keys = keys(&test).await;
        let ctx = writer(&test, "drafter", "main").await;
        let handle = open(&ctx).await;
        let edit = PendingEdit {
            path: "systems/gamma.md",
            text: Some("# Gamma\n"),
            at: Utc::now(),
        };
        pending_entry(&test, &handle, &edit).await;

        let (refused, body) = send(
            &test,
            "DELETE",
            "/api/v1/catalogs/main",
            Some(&keys.admin),
            None,
        )
        .await;
        let still_there = test.app.catalog("main").await.unwrap().is_some();
        let (forced, deleted) = send(
            &test,
            "DELETE",
            "/api/v1/catalogs/main?force=true",
            Some(&keys.admin),
            None,
        )
        .await;

        assert_eq!(refused, StatusCode::CONFLICT);
        assert!(
            body["message"]
                .as_str()
                .unwrap()
                .contains("add ?force=true"),
            "{body}"
        );
        assert!(still_there);
        assert_eq!(forced, StatusCode::OK);
        assert_eq!(deleted, json!({ "name": "main" }));
        // No listener runs here: the route applied its own write.
        assert!(test.app.catalog("main").await.unwrap().is_none());
        assert!(names(&test, &keys.read).await.is_empty());
    }

    /// Read-your-writes on the replica that made a write: with no change
    /// feed listener running (none does in a unit test), a REST write's own
    /// invalidations reach this replica's directory before it answers, so
    /// the very next read sees the cached row replaced.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_rest_admin_write_is_seen_by_the_next_read_on_this_replica() {
        let test = app().await;
        let keys = keys(&test).await;
        // Cached before the write, as a catalog in use is.
        let before = test.app.catalog_row("main").await.unwrap().expect("main");
        assert_ne!(before.description, "Changed through the API.");

        let (status, _) = send(
            &test,
            "PATCH",
            "/api/v1/catalogs/main",
            Some(&keys.admin),
            Some(json!({ "description": "Changed through the API." })),
        )
        .await;
        let after = test.app.catalog_row("main").await.unwrap().expect("main");

        assert_eq!(status, StatusCode::OK);
        assert_eq!(after.description, "Changed through the API.");
    }

    /// As above, for labels: a REST rename reaches this replica's label
    /// cache before the route answers, so the next labels read shows it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_rest_rename_is_seen_by_the_next_labels_read_on_this_replica() {
        let test = app().await;
        let keys = keys(&test).await;
        let mut wanted = crate::labels::directory::Wanted::default();
        wanted.catalog("main");
        // Cached before the write.
        let before = test.app.labels().labels_for(&wanted).await.unwrap();
        assert_ne!(before.catalog("main"), Some("Renamed Main"));

        let (status, _) = send(
            &test,
            "PATCH",
            "/api/v1/catalogs/main",
            Some(&keys.admin),
            Some(json!({ "display_name": "Renamed Main" })),
        )
        .await;
        let after = test.app.labels().labels_for(&wanted).await.unwrap();

        assert_eq!(status, StatusCode::OK);
        assert_eq!(after.catalog("main"), Some("Renamed Main"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn export_and_import_under_api_v1_share_the_archive_routes() {
        let test = app().await;
        let keys = keys(&test).await;

        let (status, headers, bytes) = raw(
            &test,
            "GET",
            "/api/v1/catalogs/main/export",
            &keys.read,
            Vec::new(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["content-type"], "application/gzip");
        assert_eq!(headers["cache-control"], "no-store");
        assert_eq!(bytes[..2], [0x1f, 0x8b], "gzip magic");

        // Machinery only: AGENTS.md changes, the concepts stay.
        let machinery = tarball_of(&[("AGENTS.md", "# Agents, new\n")]);
        let (status, _, body) = raw(
            &test,
            "POST",
            "/api/v1/catalogs/main/import?replace=true&only_machinery=true",
            &keys.admin,
            machinery,
        )
        .await;
        let report: serde_json::Value = serde_json::from_slice(&body).expect("a report");
        assert_eq!(status, StatusCode::OK, "{report}");
        assert_eq!(report["updated"], json!(["AGENTS.md"]));
        assert_eq!(report["deleted"], json!([]));

        let generation =
            catalog_rows::test_generation(test.app.accounts().executor(), "main").await;
        let archive = || tarball_of(&[("index.md", "# Main\n\nImported.\n")]);
        let stale = format!(
            "/api/v1/catalogs/main/import?replace=true&expect_generation={}",
            generation + 1
        );
        // A `write` key imports (issue #260); the admin key did above.
        let (status, _, body) = raw(&test, "POST", &stale, &keys.write, archive()).await;
        let refused: serde_json::Value = serde_json::from_slice(&body).expect("an error");
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(
            refused["message"]
                .as_str()
                .unwrap()
                .contains("export it again"),
            "{refused}"
        );

        let current =
            format!("/api/v1/catalogs/main/import?replace=true&expect_generation={generation}");
        let (status, _, body) = raw(&test, "POST", &current, &keys.write, archive()).await;
        let report: serde_json::Value = serde_json::from_slice(&body).expect("a report");
        assert_eq!(status, StatusCode::OK, "{report}");
        assert_eq!(report["updated"], json!(["index.md"]));
        // The snapshot was swapped in before the answer.
        let (_, listed) = send(&test, "GET", "/api/v1/catalogs", Some(&keys.read), None).await;
        assert_eq!(listed["catalogs"][0]["generation"], generation + 1);
        assert_eq!(listed["catalogs"][0]["files"], 1);
    }

    /// M13 Task 3: the per-catalog admin routes (update, member access,
    /// delete) check `require(Admin, ..)` themselves. Below admin they still answer `Forbidden` (the level
    /// check comes before suspension), and an admin, who is exempt, still
    /// manages a suspended catalog.
    #[tokio::test]
    async fn catalog_admin_routes_stay_open_to_an_admin_while_suspended() {
        use crate::http::api::testing::{assert_answers, Expect};
        use crate::mcp::testing::suspend_catalogs;
        let test = app().await;
        let keys = keys(&test).await;
        suspend_catalogs(&test, &["main"]).await;
        let described = || Some(json!({"description": "Paused."}));
        let catalog = "/api/v1/catalogs/main";
        let member_access = "/api/v1/catalogs/main/member-access";
        let delete = "/api/v1/catalogs/main?force=true";

        assert_answers(
            &test,
            &[
                Expect {
                    method: "PATCH",
                    uri: catalog,
                    body: described(),
                    key: &keys.write,
                    who: "write",
                    status: 403,
                    error: Some("Forbidden"),
                },
                Expect {
                    method: "PATCH",
                    uri: catalog,
                    body: described(),
                    key: &keys.admin,
                    who: "admin",
                    status: 200,
                    error: None,
                },
                Expect {
                    method: "GET",
                    uri: member_access,
                    body: None,
                    key: &keys.read,
                    who: "read",
                    status: 403,
                    error: Some("Forbidden"),
                },
                Expect {
                    method: "GET",
                    uri: member_access,
                    body: None,
                    key: &keys.admin,
                    who: "admin",
                    status: 200,
                    error: None,
                },
                // Last: the admin's delete removes `main`.
                Expect {
                    method: "DELETE",
                    uri: delete,
                    body: None,
                    key: &keys.write,
                    who: "write",
                    status: 403,
                    error: Some("Forbidden"),
                },
                Expect {
                    method: "DELETE",
                    uri: delete,
                    body: None,
                    key: &keys.admin,
                    who: "admin",
                    status: 200,
                    error: None,
                },
            ],
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_catalog_change_over_rest_records_the_calling_key() {
        let test = app().await;
        let keys = keys(&test).await;

        let (status, _) = send(
            &test,
            "PATCH",
            "/api/v1/catalogs/main",
            Some(&keys.admin),
            Some(json!({ "description": "The main catalog" })),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let changed = crate::store::audit::testing::entries(
            test.app.accounts().executor(),
            "catalog.description_changed",
        )
        .await;
        assert_eq!(changed.len(), 1, "{changed:?}");
        assert_eq!(changed[0].source, "api");
        assert_eq!(changed[0].actor_label, "api key");
        assert_eq!(
            changed[0].actor_person.as_deref(),
            Some(crate::db::TEST_USER)
        );
        assert_eq!(changed[0].target.as_deref(), Some("main"));
    }

    // ---- The maintenance flag (M14 Task 9c) ----

    #[tokio::test(flavor = "multi_thread")]
    async fn an_owner_or_an_admin_switches_maintenance_and_a_member_cannot() {
        let test = app().await;
        let root = every_catalog_admin(&test).await;
        let keys = keys(&test).await;
        send(
            &test,
            "POST",
            "/api/v1/users",
            Some(&root),
            Some(json!({"name": "bob"})),
        )
        .await;
        send(
            &test,
            "PUT",
            &format!("/api/v1/teams/{TEST_TEAM}/members/bob"),
            Some(&root),
            Some(json!({"role": "member"})),
        )
        .await;
        let mut owner = Browser::session_for(&test, crate::db::TEST_USER).await;
        let mut member = Browser::session_for(&test, "bob").await;
        let uri = "/api/v1/catalogs/main/maintenance";
        let body = |flag: &str| Some(json!({ "maintenance": flag }));

        let off = owner
            .send(
                &test,
                "PUT",
                uri,
                Payload::Json(json!({"maintenance": "off"})),
            )
            .await;
        let (listed_off, listing_off) =
            send(&test, "GET", "/api/v1/catalogs", Some(&root), None).await;
        let refused = member
            .send(
                &test,
                "PUT",
                uri,
                Payload::Json(json!({"maintenance": "on"})),
            )
            .await;
        // A catalog-admin key that does not reach every catalog is no owner.
        let (scoped, _) = send(&test, "PUT", uri, Some(&keys.admin), body("on")).await;
        let (unknown, _) = send(
            &test,
            "PUT",
            "/api/v1/catalogs/nowhere/maintenance",
            Some(&root),
            body("on"),
        )
        .await;
        let (invalid, _) = send(&test, "PUT", uri, Some(&root), body("maybe")).await;
        let (on, answer) = send(&test, "PUT", uri, Some(&root), body("on")).await;
        let (_, listing) = send(&test, "GET", "/api/v1/catalogs", Some(&root), None).await;

        assert_eq!(off.status, StatusCode::OK);
        assert_eq!(
            off.json(),
            json!({"name": "main", "previous": "on", "maintenance": "off"})
        );
        assert_eq!(listed_off, StatusCode::OK);
        assert_eq!(
            listing_off["catalogs"][0]["maintenance"], "off",
            "polled at once"
        );
        assert_eq!(refused.status, StatusCode::FORBIDDEN);
        assert_eq!(refused.json()["error"], "Forbidden");
        assert_eq!(scoped, StatusCode::FORBIDDEN);
        assert_eq!(unknown, StatusCode::NOT_FOUND);
        assert_eq!(invalid, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(on, StatusCode::OK);
        assert_eq!(answer["previous"], "off");
        assert_eq!(listing["catalogs"][0]["name"], "main");
        assert_eq!(listing["catalogs"][0]["maintenance"], "on");
        let changes = crate::store::audit::testing::entries(
            test.app.accounts().executor(),
            "catalog.maintenance_changed",
        )
        .await;
        assert_eq!(changes.len(), 2, "{changes:?}");
        assert_eq!(changes[0].source, "console");
        assert_eq!(
            changes[0].actor_person.as_deref(),
            Some(crate::db::TEST_USER)
        );
        assert_eq!(changes[0].details, json!({"from": "on", "to": "off"}));
        assert_eq!(changes[1].details, json!({"from": "off", "to": "on"}));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_runner_is_refused_a_catalog_whose_owners_turned_maintenance_off() {
        use crate::mcp::testing::{require_unlock, staff_key, staff_root};
        let test = app().await;
        staff_root(&test).await;
        // A reporting key of `staff`, outside main's owner team: the
        // maintenance exemption lets it in, not membership.
        let runner = staff_key(&test, "runner", Access::Read, true).await;
        require_unlock(&test).await;
        let mut owner = Browser::session_for(&test, crate::db::TEST_USER).await;
        let tree = "/api/v1/catalogs/main/tree";
        let (before, _) = send(&test, "GET", tree, Some(&runner), None).await;

        let off = owner
            .send(
                &test,
                "PUT",
                "/api/v1/catalogs/main/maintenance",
                Payload::Json(json!({"maintenance": "off"})),
            )
            .await;
        let (status, body) = send(&test, "GET", tree, Some(&runner), None).await;

        assert_eq!(
            before,
            StatusCode::OK,
            "with the flag on, no unlock or membership is needed"
        );
        assert_eq!(off.status, StatusCode::OK);
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"], "Forbidden");
        assert_eq!(body["maintenance"], "off");
    }
}
