//! `/api/v1/stores`, `PUT /settings/default_store` and
//! `PUT|DELETE /teams/{team}/store` (catalog stores design, "Placement and
//! the admin surface"): what `okf-mcp stores ...` does.
//!
//! Access: listing needs any admin; every change needs an admin that reaches
//! every catalog (a server-wide change, M11's rule). A provisioner reaches
//! none of these: they are not in [`super::PROVISIONER_ROUTES`].
//!
//! A reply never carries a credential. A store's `credential_ref` is the
//! name of a variable or a secret, not its value. A refused connection check
//! names no host, port or credential.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, put};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};

use crate::app::App;
use crate::auth::Principal;
use crate::clock::format_timestamp;
use crate::http::api::{require_admin, require_every_catalog};
use crate::http::AppError;
use crate::store::audit::AuditActor;
use crate::store::stores::{self, CredentialKind, StoreLocation, StoreRow, StoreSummary};
use crate::store::teams;
use crate::stores::StoreName;
use crate::telemetry::subscriber::split_event;

/// One store as the API shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct StoreView {
    /// Its name.
    pub name: String,
    /// `account-database` or `host`.
    pub location: &'static str,
    /// The server's host (a `host` store).
    pub host: Option<String>,
    /// Its port (a `host` store).
    pub port: Option<u16>,
    /// The database's name (a `host` store).
    pub database: Option<String>,
    /// How its credential is found (a `host` store).
    pub credential_kind: Option<&'static str>,
    /// What names the credential: a variable, a secret ARN or an IAM
    /// identity. Never the credential.
    pub credential_ref: Option<String>,
    /// When it was added.
    pub created_at: String,
    /// Whether new catalogs go here unless their team is pinned elsewhere.
    pub is_default: bool,
    /// How many teams are pinned to it.
    pub pinned_teams: i64,
}

impl From<StoreSummary> for StoreView {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "maps a value, no I/O")
    )]
    fn from(summary: StoreSummary) -> Self {
        let StoreSummary {
            row,
            created_at,
            is_default,
            pinned_teams,
        } = summary;
        let mut view = Self {
            name: row.name.to_string(),
            location: "account-database",
            host: None,
            port: None,
            database: None,
            credential_kind: None,
            credential_ref: None,
            created_at: format_timestamp(created_at),
            is_default,
            pinned_teams,
        };
        if let StoreLocation::Host {
            host,
            port,
            database,
            credential_kind,
            credential_ref,
        } = row.location
        {
            view.location = "host";
            view.host = Some(host);
            view.port = Some(port);
            view.database = Some(database);
            view.credential_kind = Some(credential_kind.as_str());
            view.credential_ref = Some(credential_ref);
        }
        view
    }
}

/// `GET /stores`' answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct StoresView {
    /// Every store, in name order.
    pub stores: Vec<StoreView>,
}

/// `POST /stores`' body: a store in its own database.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AddStoreBody {
    /// Its name (`[a-z0-9-]+`, at most 63 characters).
    name: StoreName,
    /// The database server's host.
    host: String,
    /// Its port.
    port: u16,
    /// The database's name.
    database: String,
    /// `env`, `secrets-manager` or `iam`.
    credential_kind: CredentialKind,
    /// A variable name, a secret ARN or an IAM identity; never the secret.
    credential_ref: String,
}

/// `PUT /settings/default_store`'s body, shaped like the other settings'.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DefaultStoreBody {
    /// The store's name.
    value: StoreName,
}

/// `PUT /settings/default_store`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct DefaultStoreView {
    /// Always `default_store`.
    pub key: &'static str,
    /// The store new catalogs go on now.
    pub value: String,
}

/// `PUT /teams/{team}/store`'s body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PinBody {
    /// The store's name.
    store: StoreName,
}

/// The answer to a pin or an unpin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct PinView {
    /// The team.
    pub team: String,
    /// Its pinned store now, `null` when unpinned.
    pub store: Option<String>,
    /// Whether the pin changed (`false`: it already was so).
    pub changed: bool,
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
        .route("/stores", get(list_stores).post(add_store))
        .route("/settings/default_store", put(set_default_store))
        .route("/teams/{team}/store", put(pin_team).delete(unpin_team))
}

/// `GET /stores`: every store, with the default marked.
///
/// # Errors
/// 403 below admin; 500 for a database failure.
#[tracing::instrument(level = "debug", skip_all)]
async fn list_stores(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
) -> Result<Json<StoresView>, AppError> {
    require_admin(&principal)?;
    let summaries = stores::summaries(app.accounts().executor()).await?;
    Ok(Json(StoresView {
        stores: summaries.into_iter().map(StoreView::from).collect(),
    }))
}

/// `POST /stores`: adds a store, after checking that it can be reached.
///
/// # Returns
/// 201 and the store.
///
/// # Errors
/// 403 unless the caller is an admin reaching every catalog; 409 when the
/// store cannot be reached with these settings (the reply names no host,
/// port or credential), or for a
/// name or host, port and database already in use; 422 for a malformed
/// body; 500 for a database failure.
#[tracing::instrument(level = "debug", skip_all)]
async fn add_store(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    actor: AuditActor,
    Json(body): Json<AddStoreBody>,
) -> Result<(StatusCode, Json<StoreView>), AppError> {
    require_every_catalog(&principal)?;
    let row = StoreRow {
        name: body.name,
        location: StoreLocation::Host {
            host: body.host,
            port: body.port,
            database: body.database,
            credential_kind: body.credential_kind,
            credential_ref: body.credential_ref,
        },
    };
    let added = stores::add(
        app.accounts().executor(),
        app.stores(),
        row,
        &actor,
        app.clock().now(),
    )
    .await?;
    // The store's name is operator detail, not content; the host is not
    // logged at all.
    split_event!(
        info,
        { store = %added.row.name, },
        { key_id = %principal.key_id, actor = %principal.actor, },
        "store added"
    );
    Ok((StatusCode::CREATED, Json(StoreView::from(added))))
}

/// `PUT /settings/default_store`: the store new catalogs go on. Existing
/// catalogs stay where they are.
///
/// # Errors
/// 403 unless the caller is an admin reaching every catalog; 404 for an
/// unknown store; 422 for a malformed body; 500 for a database failure.
#[tracing::instrument(level = "debug", skip_all)]
async fn set_default_store(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    actor: AuditActor,
    Json(body): Json<DefaultStoreBody>,
) -> Result<Json<DefaultStoreView>, AppError> {
    require_every_catalog(&principal)?;
    stores::set_default(
        app.accounts().executor(),
        &body.value,
        &actor,
        app.clock().now(),
    )
    .await?;
    split_event!(
        info,
        { store = %body.value, },
        { key_id = %principal.key_id, actor = %principal.actor, },
        "default store changed"
    );
    Ok(Json(DefaultStoreView {
        key: "default_store",
        value: body.value.to_string(),
    }))
}

/// `PUT /teams/{team}/store`: pins a team, so the catalogs it owns are
/// created on that store from now on.
///
/// # Errors
/// 403 unless the caller is an admin reaching every catalog; 404 for an
/// unknown team or store; 422 for a malformed body; 500 for a database
/// failure.
#[tracing::instrument(level = "debug", skip_all)]
async fn pin_team(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    actor: AuditActor,
    Path(team): Path<String>,
    Json(body): Json<PinBody>,
) -> Result<Json<PinView>, AppError> {
    require_every_catalog(&principal)?;
    let changed = teams::set_store(
        app.accounts().executor(),
        &team,
        Some(&body.store),
        &actor,
        app.clock().now(),
    )
    .await?;
    Ok(Json(PinView {
        team,
        store: Some(body.store.to_string()),
        changed,
    }))
}

/// `DELETE /teams/{team}/store`: removes a team's pin, so the default store
/// applies again.
///
/// # Errors
/// 403 unless the caller is an admin reaching every catalog; 404 for an
/// unknown team; 500 for a database failure.
#[tracing::instrument(level = "debug", skip_all)]
async fn unpin_team(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    actor: AuditActor,
    Path(team): Path<String>,
) -> Result<Json<PinView>, AppError> {
    require_every_catalog(&principal)?;
    let changed = teams::set_store(
        app.accounts().executor(),
        &team,
        None,
        &actor,
        app.clock().now(),
    )
    .await?;
    Ok(Json(PinView {
        team,
        store: None,
        changed,
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::http::api::testing::{
        app, assert_matrix_with, every_catalog_admin, keys, send, Row,
    };
    use crate::store::audit::testing::entries;

    #[tokio::test]
    async fn store_admin_endpoints_refuse_non_admin_credentials() {
        let test = app().await;
        let keys = keys(&test).await;
        let row = |method: &'static str, uri: &str, body: Option<serde_json::Value>| Row {
            method,
            uri: uri.to_string(),
            body,
            read: 403,
            write: 403,
            // An admin key on `main` is an admin, but reaches one catalog.
            admin: 403,
            stranger: 403,
        };
        assert_matrix_with(
            &test,
            &keys,
            &[
                Row {
                    admin: 200,
                    stranger: 200,
                    ..row("GET", "/api/v1/stores", None)
                },
                row(
                    "POST",
                    "/api/v1/stores",
                    Some(json!({
                        "name": "shared-1", "host": "db.example.internal", "port": 5432,
                        "database": "okf", "credential_kind": "env",
                        "credential_ref": "SHARED_1_PASSWORD",
                    })),
                ),
                row(
                    "PUT",
                    "/api/v1/settings/default_store",
                    Some(json!({"value": "main"})),
                ),
                row(
                    "PUT",
                    "/api/v1/teams/anything/store",
                    Some(json!({"store": "main"})),
                ),
                row("DELETE", "/api/v1/teams/anything/store", None),
            ],
        )
        .await;
    }

    #[tokio::test]
    async fn a_provisioner_key_reaches_none_of_the_store_routes() {
        let test = app().await;
        let root = every_catalog_admin(&test).await;
        let (created_status, created) = send(
            &test,
            "POST",
            "/api/v1/keys",
            Some(&root),
            Some(json!({"name": "prov", "provisioner": true, "user": crate::db::TEST_USER})),
        )
        .await;
        assert_eq!(created_status.as_u16(), 200, "{created}");
        let secret = created["secret"].as_str().expect("a bearer secret");

        let shared = json!({
            "name": "shared-1", "host": "db.example.internal", "port": 5432,
            "database": "okf", "credential_kind": "env", "credential_ref": "SHARED_1_PASSWORD",
        });
        let routes = [
            ("GET", "/api/v1/stores", None),
            ("POST", "/api/v1/stores", Some(shared)),
            (
                "PUT",
                "/api/v1/settings/default_store",
                Some(json!({"value": "main"})),
            ),
            (
                "PUT",
                "/api/v1/teams/main/store",
                Some(json!({"store": "main"})),
            ),
            ("DELETE", "/api/v1/teams/main/store", None),
        ];
        for (method, uri, body) in routes {
            let (status, _) = send(&test, method, uri, Some(secret), body).await;
            assert_eq!(status.as_u16(), 403, "{method} {uri}");
        }
    }

    #[tokio::test]
    async fn the_listing_shows_main_as_the_default() {
        let test = app().await;
        crate::db::main_in_the_account_database(test.app.accounts().executor()).await;
        let root = every_catalog_admin(&test).await;

        let (status, body) = send(&test, "GET", "/api/v1/stores", Some(&root), None).await;

        assert_eq!(status.as_u16(), 200);
        assert_eq!(body["stores"].as_array().map(Vec::len), Some(1));
        assert_eq!(body["stores"][0]["name"], "main");
        assert_eq!(body["stores"][0]["location"], "account-database");
        assert_eq!(body["stores"][0]["is_default"], true);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn adding_an_unreachable_store_through_the_api_leaks_nothing() {
        let test = app().await;
        let root = every_catalog_admin(&test).await;
        let refusing = crate::test_net::Refused::new();
        let closed = refusing.port();

        let (status, body) = crate::test_net::bounded(
            "adding a store at a refusing address",
            send(
                &test,
                "POST",
                "/api/v1/stores",
                Some(&root),
                Some(json!({
                    "name": "shared-1", "host": "127.0.0.1", "port": closed,
                    "database": "okf", "credential_kind": "env",
                    "credential_ref": "NO_SUCH_VARIABLE_FOR_THE_TEST",
                })),
            ),
        )
        .await;

        assert_eq!(status.as_u16(), 409);
        let reply = body.to_string();
        for private in ["127.0.0.1", &closed.to_string(), "NO_SUCH_VARIABLE"] {
            assert!(!reply.contains(private), "{private} leaked into {reply}");
        }
        let (_, listing) = send(&test, "GET", "/api/v1/stores", Some(&root), None).await;
        assert_eq!(listing["stores"].as_array().map(Vec::len), Some(1));
        assert!(entries(test.app.accounts().executor(), "store.added")
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn the_default_and_a_pin_are_set_and_audited_through_the_api() {
        let test = app().await;
        let root = every_catalog_admin(&test).await;
        let (_, team) = send(&test, "GET", "/api/v1/teams", Some(&root), None).await;
        let team_name = team["teams"][0]["name"]
            .as_str()
            .expect("a team")
            .to_string();

        let (default_status, default) = send(
            &test,
            "PUT",
            "/api/v1/settings/default_store",
            Some(&root),
            Some(json!({"value": "main"})),
        )
        .await;
        let (unknown, _) = send(
            &test,
            "PUT",
            "/api/v1/settings/default_store",
            Some(&root),
            Some(json!({"value": "nowhere"})),
        )
        .await;
        let (pin_status, pin) = send(
            &test,
            "PUT",
            &format!("/api/v1/teams/{team_name}/store"),
            Some(&root),
            Some(json!({"store": "main"})),
        )
        .await;
        let (unpin_status, unpin) = send(
            &test,
            "DELETE",
            &format!("/api/v1/teams/{team_name}/store"),
            Some(&root),
            None,
        )
        .await;

        assert_eq!(default_status.as_u16(), 200);
        assert_eq!(default, json!({"key": "default_store", "value": "main"}));
        assert_eq!(unknown.as_u16(), 404);
        assert_eq!(pin_status.as_u16(), 200);
        assert_eq!(pin["changed"], true);
        assert_eq!(unpin_status.as_u16(), 200);
        assert_eq!(unpin["changed"], true);
        let pool = test.app.accounts().executor();
        assert_eq!(entries(pool, "team.store_pinned").await.len(), 1);
        assert_eq!(entries(pool, "team.store_unpinned").await.len(), 1);
    }
}
