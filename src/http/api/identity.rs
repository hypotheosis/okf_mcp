//! `GET /api/v1/whoami`: the calling key, grant or console session, so the
//! console can greet it and hide what it cannot do. Never includes the key
//! or its hash. `GET /api/v1/session` and `DELETE /api/v1/session`: the
//! signed-in console session, and signing out (plan Decision 22).

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Json, Router};
use serde::Serialize;
use sqlx::PgPool;

use super::{navigable_page, PageQuery};
use crate::app::App;
use crate::auth::{CredentialKind, Principal, Reach};
use crate::clock::format_timestamp;
use crate::error::OkfError;
use crate::http::console_session::{self, ConsoleSession};
use crate::http::AppError;
use crate::labels::directory::Wanted;
use crate::store::keys::{self as key_rows, Access};
use crate::store::{ceiling, user_sessions, users};

/// The calling key, as the console sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct WhoAmI {
    /// The key's immutable id (`session:<user>` for a console session).
    pub key_id: String,
    /// Its grouping name (`None` for a session, or if the row vanished
    /// meanwhile).
    pub name: Option<String>,
    /// The actor its writes are recorded as.
    pub actor: String,
    /// `read`, `write` or `admin`.
    pub access: &'static str,
    /// Whether it may submit maintenance reports.
    pub maintenance_report: bool,
    /// Whether it reaches every catalog.
    pub all_catalogs: bool,
    /// One page of the loaded catalogs it reaches, sorted (M18 spec
    /// Decision 7: `?after=&limit=`, as `GET /catalogs`).
    pub catalogs: Vec<String>,
    /// Its level on each catalog of this page (`read`, `write` or
    /// `admin`): for a console session, its ceiling's level there, which
    /// differs per catalog (issue #736: the console reads levels here, a
    /// page at a time, instead of a whole ceiling from `GET /session`).
    pub levels: BTreeMap<String, &'static str>,
    /// The most catalogs one search may name (`search_fanout_max`), so the
    /// console can offer a chooser instead of naming every catalog.
    pub search_fanout_max: u32,
    /// The `after` of the next page of `catalogs`; absent on the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    /// Its default catalog.
    pub default_catalog: Option<String>,
    /// `key`, `oauth` for a grant, or `session` for a console session.
    pub kind: &'static str,
    /// A grant's client, as it names itself.
    pub client_name: Option<String>,
    /// The user it belongs to (`None` when unassigned, or the row vanished).
    pub user: Option<String>,
    /// That user's label (`None` without a user, or one created since the
    /// key cache last loaded).
    pub user_label: Option<String>,
    /// That user's teams, sorted (empty without a user, or once the user
    /// is deleted).
    pub teams: Vec<String>,
    /// Whether that user is an admin user (operator staff), the only users
    /// whose admin credentials may create an unlock (issue #447); `false`
    /// without a user, or once the user is deleted.
    pub user_admin: bool,
}

/// The signed-in console session, as the console reads it at boot.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub(crate) struct SessionView {
    /// The user's name.
    pub user: String,
    /// The user's email.
    pub email: Option<String>,
    /// Whether the user is an admin (the session acts as one).
    pub admin: bool,
    /// The token every write must send as `X-OKF-CSRF`.
    pub csrf: String,
    /// When the session ends regardless of activity.
    pub expires_at: String,
}

// By hand, not derived, so a stray `{:?}` never prints the CSRF token.
impl std::fmt::Debug for SessionView {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "Display/Debug impl: runs while the subscriber formats an event"
        )
    )]
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionView")
            .field("user", &self.user)
            .field("admin", &self.admin)
            .field("csrf", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
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
        .route("/whoami", get(whoami))
        .route("/session", get(session).delete(sign_out))
}

/// The teams of the user called `name`, sorted, and whether it is an admin
/// user.
///
/// # Arguments
/// * `pool` - the database.
/// * `name` - the user's name.
///
/// # Returns
/// Its teams and admin flag, or no teams and `false` once the user is
/// gone: a user deleted between reading the key and reading the user is
/// not an error here.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all)]
async fn teams_of(pool: &PgPool, name: &str) -> Result<(Vec<String>, bool), OkfError> {
    match users::find(pool, name).await {
        Ok(record) => Ok((record.teams, record.is_admin)),
        Err(OkfError::UnknownName { .. }) => Ok((Vec::new(), false)),
        Err(other) => Err(other),
    }
}

/// `GET /whoami?after=&limit=`.
///
/// # Errors
/// 400 for a malformed `after`; 500 if the key row, the user or the
/// catalog directory cannot be read.
#[tracing::instrument(level = "debug", skip_all)]
async fn whoami(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    Query(query): Query<PageQuery>,
) -> Result<Json<WhoAmI>, AppError> {
    // First, so a malformed cursor is refused before anything else is read.
    let page = navigable_page(&app, &principal, &query).await?;
    let (name, user) = match &principal.kind {
        // A session has no key row: it is its user.
        CredentialKind::Session { .. } => (None, principal.user.clone()),
        CredentialKind::ApiKey | CredentialKind::OAuth { .. } => {
            // The user comes from the one row read now, not the principal,
            // in case it was reassigned since the principal was cached. A
            // row that is gone has no name or user; a row revoked since the
            // principal was cached is still read (revoked or not), so it
            // keeps both, as the whole-table read did.
            // `unzip` splits an `Option<(a, b)>` into
            // `(Option<a>, Option<b>)`.
            let (name, user) = key_rows::record_by_id(app.accounts().executor(), &principal.key_id)
                .await?
                .map(|credential| (credential.record.name, credential.record.user))
                .unzip();
            // `flatten` turns the `Option<Option<String>>` (row found, user
            // set) into one `Option<String>`.
            (name, user.flatten())
        }
    };
    // A console session's levels are its ceiling's, resolved per catalog:
    // its highest is read now, one row (issue #728).
    let access = match (&principal.reach, principal.user_id.as_deref()) {
        (Reach::Ceiling, Some(user_id)) => {
            ceiling::highest(app.accounts().executor(), user_id).await?
        }
        _ => principal.access,
    };
    let (teams, user_admin) = match &user {
        Some(name) => teams_of(app.accounts().executor(), name).await?,
        None => (Vec::new(), false),
    };
    // The page is sorted by name. Only names are listed, so nothing is
    // loaded: every catalog the key reaches is named, whether or not this
    // replica holds it in its cache (M18 Task 8).
    let catalogs = page.items.iter().map(|row| row.name.clone()).collect();
    // Each row's level, resolved per catalog as every check does (a
    // ceiling's from the row and the user's gates); bounded by the page.
    let levels = page
        .items
        .iter()
        .filter_map(|row| Some((row.name.clone(), principal.level_at(row)?.as_str())))
        .collect();
    // `match` on a reference: `client_name` binds a `&Option<String>`.
    let (kind, client_name) = match &principal.kind {
        CredentialKind::ApiKey => ("key", None),
        CredentialKind::OAuth { client_name, .. } => ("oauth", client_name.clone()),
        CredentialKind::Session { .. } => ("session", None),
    };
    let user_label = match user.as_deref() {
        Some(name) => {
            let mut wanted = Wanted::default();
            wanted.user(name);
            app.labels()
                .labels_for(&wanted)
                .await?
                .user(name)
                .map(str::to_string)
        }
        None => None,
    };
    Ok(Json(WhoAmI {
        key_id: principal.key_id.clone(),
        name,
        actor: principal.actor.clone(),
        access: access.as_str(),
        maintenance_report: principal.maintenance_report,
        all_catalogs: matches!(principal.reach, Reach::All),
        catalogs,
        levels,
        search_fanout_max: app.config().search_fanout_max,
        next: page.next,
        default_catalog: principal.default_catalog.clone(),
        kind,
        client_name,
        user,
        user_label,
        teams,
        user_admin,
    }))
}

/// The calling session's user id and session, or 403 for a key or grant.
///
/// # Errors
/// [`OkfError::Forbidden`] when the request is not a console session.
#[tracing::instrument(level = "trace", skip_all)]
fn console(
    principal: &Principal,
    session: Option<Extension<ConsoleSession>>,
) -> Result<(String, ConsoleSession), OkfError> {
    match (&principal.kind, session) {
        (CredentialKind::Session { user_id }, Some(Extension(session))) => {
            Ok((user_id.clone(), session))
        }
        // A key or a grant (or, impossibly, a session principal without its
        // session): there is no console session here.
        _ => Err(OkfError::Forbidden {
            reason: "this request is not a console session".to_string(),
        }),
    }
}

/// `GET /session`: the signed-in user, the admin flag and the CSRF token.
/// The admin flag is the principal's, which the middleware built from the
/// user cache for this request. It holds no ceiling (issue #736): the
/// levels are on `GET /whoami`'s pages, and `GET /me/ceiling` pages the
/// ceiling.
///
/// # Errors
/// 403 for a key or grant; 401 once the user is gone; 500 on a lookup failure.
#[tracing::instrument(level = "debug", skip_all)]
async fn session(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    session: Option<Extension<ConsoleSession>>,
) -> Result<Json<SessionView>, AppError> {
    let (user_id, session) = console(&principal, session)?;
    let user = console_session::session_user(&app, &user_id)
        .await?
        .ok_or_else(|| OkfError::Unauthenticated {
            reason: "this session's user no longer exists".to_string(),
        })?;
    let admin = principal.access == Access::Admin;
    Ok(Json(SessionView {
        user: user.name,
        email: user.email,
        admin,
        csrf: session.csrf,
        expires_at: format_timestamp(session.expires_at),
    }))
}

/// `DELETE /session`: signs out (the session is deleted, the cookie
/// cleared). The middleware has already checked the CSRF header.
///
/// # Errors
/// 403 for a key or grant; 500 if the delete fails.
#[tracing::instrument(level = "debug", skip_all)]
async fn sign_out(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    session: Option<Extension<ConsoleSession>>,
) -> Result<Response, AppError> {
    let (user_id, session) = console(&principal, session)?;
    user_sessions::revoke(app.accounts().executor(), &session.id).await?;
    tracing::info!(user = %user_id, "console session signed out");
    Ok((StatusCode::NO_CONTENT, console_session::clear_cookie(&app)).into_response())
}

#[cfg(test)]
mod tests {
    use crate::http::api::testing::{app, assert_matrix, keys, send, Row};
    // `oneshot` sends one request through a router without a socket.
    use tower::ServiceExt;

    #[tokio::test]
    async fn whoami_describes_the_calling_key() {
        let test = app().await;
        let keys = keys(&test).await;
        let (status, body) = send(&test, "GET", "/api/v1/whoami", Some(&keys.read), None).await;
        assert_eq!(status.as_u16(), 200);
        assert_eq!(body["name"], "reader");
        assert_eq!(body["actor"], "reader");
        assert_eq!(body["access"], "read");
        assert_eq!(body["all_catalogs"], false);
        assert_eq!(body["catalogs"], serde_json::json!(["main"]));
        assert_eq!(body["default_catalog"], "main");
        assert_eq!(body["search_fanout_max"], 25);
        assert_eq!(body["kind"], "key");
        assert_eq!(body["client_name"], serde_json::Value::Null);
        assert!(body.get("sha256").is_none(), "never a hash or secret");
    }

    #[tokio::test]
    async fn whoami_names_the_keys_user_and_its_teams() {
        let test = app().await;
        let keys = keys(&test).await;

        let (status, body) = send(&test, "GET", "/api/v1/whoami", Some(&keys.read), None).await;

        assert_eq!(status.as_u16(), 200);
        assert_eq!(body["user"], crate::db::TEST_USER);
        assert_eq!(body["teams"], serde_json::json!([crate::db::TEST_TEAM]));
        assert_eq!(body.get("user_admin"), Some(&serde_json::json!(false)));
    }

    #[tokio::test]
    async fn whoami_says_whether_the_keys_user_is_an_admin_user() {
        let test = app().await;
        let keys = keys(&test).await;
        let on = crate::store::users::AdminChange {
            on: true,
            allow_last: true,
        };
        crate::store::users::set_admin(
            test.app.accounts().executor(),
            crate::db::TEST_USER,
            on,
            &crate::store::audit::AuditActor::cli(),
            chrono::Utc::now(),
        )
        .await
        .unwrap();

        let (status, body) = send(&test, "GET", "/api/v1/whoami", Some(&keys.read), None).await;

        assert_eq!(status.as_u16(), 200);
        assert_eq!(body.get("user_admin"), Some(&serde_json::json!(true)));
    }

    #[tokio::test]
    async fn whoami_of_an_unassigned_key_has_no_user() {
        let test = app().await;
        let key = crate::store::keys::insert_legacy_test_key(
            test.app.accounts().executor(),
            "legacy",
            crate::store::keys::Access::Read,
            "legacy",
            chrono::Utc::now(),
        )
        .await;
        keys(&test).await;

        let (status, body) = send(&test, "GET", "/api/v1/whoami", Some(&key), None).await;

        assert_eq!(status.as_u16(), 200);
        assert_eq!(body["user"], serde_json::Value::Null);
        assert_eq!(body["teams"], serde_json::json!([]));
        assert_eq!(body.get("user_admin"), Some(&serde_json::json!(false)));
    }

    #[tokio::test]
    async fn a_deleted_user_has_no_teams_not_an_error() {
        let test = app().await;

        let teams = super::teams_of(test.app.accounts().executor(), "no-such-user").await;

        assert_eq!(teams.expect("not an error"), (Vec::<String>::new(), false));
    }

    #[tokio::test]
    async fn whoami_needs_any_key() {
        let test = app().await;
        assert_matrix(
            &test,
            &[Row {
                method: "GET",
                uri: "/api/v1/whoami".into(),
                body: None,
                read: 200,
                write: 200,
                admin: 200,
                stranger: 200,
            }],
        )
        .await;
    }

    #[tokio::test]
    async fn api_responses_are_not_cached_or_sniffed() {
        let test = app().await;
        let keys = keys(&test).await;
        let valid = format!("Bearer {}", keys.read);
        // A good key, no key, an unknown key, and an unknown path: the
        // refusals carry the headers too, since `require_key` runs inside them.
        for (uri, authorization, want) in [
            ("/api/v1/whoami", Some(valid.as_str()), 200),
            ("/api/v1/whoami", None, 401),
            ("/api/v1/whoami", Some("Bearer okf_not-a-real-key"), 401),
            ("/api/v1/nothing-here", None, 404),
        ] {
            let mut request = axum::http::Request::builder().uri(uri);
            if let Some(value) = authorization {
                request = request.header("authorization", value);
            }
            let request = request.body(axum::body::Body::empty()).expect("request");
            let response = crate::http::router(
                std::sync::Arc::clone(&test.app),
                &tokio_util::sync::CancellationToken::new(),
            )
            .oneshot(request)
            .await
            .expect("response");
            let headers = response.headers();
            let case = format!("{uri} with {authorization:?}");
            assert_eq!(response.status().as_u16(), want, "{case}");
            assert_eq!(headers["cache-control"], "no-store", "{case}");
            assert_eq!(headers["x-content-type-options"], "nosniff", "{case}");
            if want == 401 {
                assert_eq!(headers["www-authenticate"], "Bearer", "{case}");
            }
        }
    }

    #[test]
    fn a_session_view_never_debug_prints_its_csrf_token() {
        let view = super::SessionView {
            user: "alice".to_string(),
            email: Some("alice@example.com".to_string()),
            admin: false,
            csrf: "csrf-token-secret".to_string(),
            expires_at: "2026-01-01T00:00:00Z".to_string(),
        };

        let printed = format!("{view:?}");

        assert!(!printed.contains("csrf-token-secret"), "{printed}");
        assert!(printed.contains("alice"), "{printed}");
    }

    #[tokio::test]
    async fn an_unknown_api_path_is_404_not_401() {
        let test = app().await;
        let (status, _) = send(&test, "GET", "/api/v1/nothing-here", None, None).await;
        assert_eq!(status.as_u16(), 404);
    }

    /// A key deleted after its credential was cached still authenticates
    /// until the cache expires; its row is gone, so `whoami` names no key
    /// and no user (the answer the whole-table read gave).
    #[tokio::test]
    async fn whoami_of_a_key_deleted_after_it_was_cached_has_no_name_or_user() {
        let test = app().await;
        let keys = keys(&test).await;
        let (warm, _) = send(&test, "GET", "/api/v1/whoami", Some(&keys.read), None).await;
        assert_eq!(warm.as_u16(), 200);
        sqlx::query("DELETE FROM api_keys WHERE name = 'reader'")
            .execute(test.app.accounts().executor())
            .await
            .expect("delete the key row");

        let (status, body) = send(&test, "GET", "/api/v1/whoami", Some(&keys.read), None).await;

        assert_eq!(status.as_u16(), 200, "{body}");
        assert_eq!(body.get("name"), Some(&serde_json::Value::Null));
        assert_eq!(body.get("user"), Some(&serde_json::Value::Null));
        assert_eq!(body["teams"], serde_json::json!([]));
    }

    /// Issue #737: `GET /whoami` read every key to find the caller's. With
    /// 50,000 other keys present (seeded by SQL), the request's queries
    /// return a handful of rows, and the answer is the caller's own.
    #[tokio::test(flavor = "multi_thread")]
    async fn whoami_reads_the_callers_key_not_every_key() {
        const OTHERS: i64 = 50_000;
        let test = app().await;
        let keys = keys(&test).await;
        sqlx::query(
            "INSERT INTO api_keys (id, name, actor, sha256, access, all_catalogs, created_at) \
             SELECT 'k' || n, 'bulk' || n, 'bulk' || n, sha256(('bulk' || n)::bytea), \
                    'read', false, now() \
             FROM generate_series(1, $1) AS n",
        )
        .bind(OTHERS)
        .execute(test.app.accounts().executor())
        .await
        .expect("seed keys");
        sqlx::query("ANALYZE api_keys")
            .execute(test.app.accounts().executor())
            .await
            .unwrap();
        // Warm the caches, so the measured request is the steady state.
        let (warm, _) = send(&test, "GET", "/api/v1/whoami", Some(&keys.read), None).await;
        assert_eq!(warm.as_u16(), 200);
        // The shared counter installs the subscriber and refreshes the
        // callsite interest cache first (issue #826).
        let ((status, body), rows, statements) = crate::rows_returned::counting(async {
            tokio::time::timeout(
                std::time::Duration::from_secs(30),
                send(&test, "GET", "/api/v1/whoami", Some(&keys.read), None),
            )
            .await
            .expect("whoami answers")
        })
        .await;

        assert_eq!(status.as_u16(), 200);
        assert_eq!(body["name"], "reader");
        assert!(statements > 0, "the request's statements are counted");
        assert!(
            rows < 50,
            "whoami returned {rows} rows in {statements} statements beside {OTHERS} other keys"
        );
    }
}
