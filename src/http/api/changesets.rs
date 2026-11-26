//! `/api/v1/changesets` (web console design, "API": Changesets): open
//! changesets, their pending diffs, publishing one and discarding one. Thin
//! adapters over the `list_changesets`, `pending_changes`, `publish` and
//! `discard` tools, so the access rules are the tools' own (M8 Decisions 3
//! and 8):
//! - list: the key's own changesets, an admin's every key's, in the
//!   catalogs it reaches (a read key owns none);
//! - diff: the owning key or an admin;
//! - publish and discard: the owning write key or an admin.
//!
//! A console session owns no changeset (its `key_id` is `session:<name>`,
//! which no changeset row carries), so a session reaches one only as an
//! admin. Its publish is then recorded as the session's user, never as the
//! key that opened the changeset.
//!
//! Another key's handle is 404 (`UnknownChangeset`), never 403, so a key
//! cannot tell a real handle from a made-up one.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::routing::{delete, get, post};
use axum::{Extension, Json, Router};
use serde::Deserialize;

use crate::app::App;
use crate::auth::Principal;
use crate::http::api::context;
use crate::http::requester::Requester;
use crate::http::AppError;
use crate::mcp::changesets::{
    discard, list_changesets, pending_changes, publish, DiscardParams, Discarded, ListParams,
    Listed, PendingChanges, PendingParams, PublishParams, Published,
};

/// `GET /changesets`' query.
#[derive(Debug, Deserialize)]
struct ListQuery {
    /// Only this catalog (absent or empty: every catalog the key reaches).
    catalog: Option<String>,
}

/// `POST /changesets/{handle}/publish`'s body.
#[derive(Debug, Deserialize)]
struct PublishBody {
    /// The revision message, as the `publish` tool's `message`.
    message: String,
}

/// This area's routes.
///
/// # Returns
/// `GET /changesets`, `GET /changesets/{handle}/diff`,
/// `POST /changesets/{handle}/publish` and `DELETE /changesets/{handle}`.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "router builder: runs once at startup, outside any request, so a span would be an orphan root"
    )
)]
pub(crate) fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/changesets", get(list))
        .route("/changesets/{handle}", delete(remove))
        .route("/changesets/{handle}/diff", get(diff))
        .route("/changesets/{handle}/publish", post(publish_one))
}

/// `GET /changesets?catalog=`: as the `list_changesets` tool.
///
/// # Errors
/// 404 for a catalog the key does not reach; 500 if the store fails.
#[tracing::instrument(level = "debug", skip_all)]
async fn list(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    requester: Requester,
    Query(query): Query<ListQuery>,
) -> Result<Json<Listed>, AppError> {
    let ctx = context(&app, principal, requester);
    // `filter` turns `?catalog=` (an empty select) into "every catalog".
    let catalog = query.catalog.filter(|name| !name.is_empty());
    Ok(Json(list_changesets(&ctx, ListParams { catalog }).await?))
}

/// `GET /changesets/{handle}/diff`: as the `pending_changes` tool.
///
/// # Errors
/// 404 for a handle the key may not use; 409 for a published or discarded one.
#[tracing::instrument(level = "debug", skip_all)]
async fn diff(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    requester: Requester,
    Path(handle): Path<String>,
) -> Result<Json<PendingChanges>, AppError> {
    let ctx = context(&app, principal, requester);
    let params = PendingParams { changeset: handle };
    Ok(Json(pending_changes(&ctx, params).await?))
}

/// `POST /changesets/{handle}/publish`: as the `publish` tool, with the
/// caller's own principal, so the revisions and the `catalog.published`
/// entry name the caller (an admin publishing an agent's changeset is
/// recorded as the admin, not as the key that opened it).
///
/// # Returns
/// The tool's [`Published`]: `published: false` (the changeset stays open)
/// for one with no entries; `published: true` with no revisions for one
/// whose edits all match the published text (it is closed).
///
/// # Errors
/// 404 for a handle the key may not use (another key's, unless the caller
/// is an admin, or a made-up one); 403 for a suspended or locked catalog,
/// or the maintenance runner where maintenance is off; 409 for a blank
/// message, a closed changeset or a conflict (`files`); 422 when a changed
/// file fails lint (`errors`, `warnings`), or for a body without `message`.
/// A refused publish leaves the changeset open and unchanged.
#[tracing::instrument(level = "debug", skip_all)]
async fn publish_one(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    requester: Requester,
    Path(handle): Path<String>,
    Json(body): Json<PublishBody>,
) -> Result<Json<Published>, AppError> {
    let ctx = context(&app, principal, requester);
    let params = PublishParams {
        message: body.message,
        changeset: handle,
    };
    Ok(Json(publish(&ctx, params).await?))
}

/// `DELETE /changesets/{handle}`: drops every entry and closes the
/// changeset for good, as `discard` without `paths`.
///
/// # Errors
/// 404 for a handle the key may not use; 403 for an owner below write
/// (none exists: opening needs write); 409 if it was published or
/// discarded meanwhile.
#[tracing::instrument(level = "debug", skip_all)]
async fn remove(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    requester: Requester,
    Path(handle): Path<String>,
) -> Result<Json<Discarded>, AppError> {
    let ctx = context(&app, principal, requester);
    let params = DiscardParams {
        paths: None,
        changeset: handle,
    };
    Ok(Json(discard(&ctx, params).await?))
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::json;

    use crate::http::api::context;
    use crate::http::api::testing::{app, assert_matrix_with, keys, send, Row};
    use crate::http::requester::Requester;
    use crate::mcp::changesets::{open_changeset, OpenParams};
    use crate::mcp::context::ToolContext;
    use crate::mcp::testing::{pending_entry, PendingEdit, TestApp};
    use crate::store::audit::AuditActor;
    use crate::store::audit::Source;
    use crate::store::users::AdminChange;

    /// The text [`open_as`] stores: a lint-clean concept, so it publishes.
    const DRAFT: &str = "---\ntype: system\ntitle: Gamma\n---\n# Gamma\n\nDraft.\n";

    /// Opens a changeset in `main` as the key `secret` and stores one new
    /// file in it; returns the handle.
    async fn open_as(test: &TestApp, secret: &str, path: &str) -> String {
        open_with(test, secret, path, DRAFT).await
    }

    /// [`open_as`] with the file's text.
    async fn open_with(test: &TestApp, secret: &str, path: &str, text: &str) -> String {
        let handle = open_empty(test, secret).await;
        let edit = PendingEdit {
            path,
            text: Some(text),
            at: Utc::now(),
        };
        pending_entry(test, &handle, &edit).await;
        handle
    }

    /// The MCP tool context of the key `secret`, from the REST API.
    async fn ctx_of(test: &TestApp, secret: &str) -> ToolContext {
        let principal = test
            .app
            .find_key(secret)
            .await
            .unwrap()
            .expect("a live key");
        let requester = Requester {
            ip: None,
            source: Source::Api,
        };
        context(&test.app, principal, requester)
    }

    /// Opens a changeset in `main` as the key `secret`, with no entries;
    /// returns the handle.
    async fn open_empty(test: &TestApp, secret: &str) -> String {
        open_changeset(
            &ctx_of(test, secret).await,
            OpenParams {
                catalog: Some("main".to_string()),
            },
        )
        .await
        .expect("open a changeset")
        .changeset
    }

    /// `POST /api/v1/changesets/{handle}/publish` with `message` as `secret`.
    async fn publish_with(
        test: &TestApp,
        secret: &str,
        handle: &str,
        message: &str,
    ) -> (u16, serde_json::Value) {
        let uri = format!("/api/v1/changesets/{handle}/publish");
        let body = json!({ "message": message });
        let (status, body) = send(test, "POST", &uri, Some(secret), Some(body)).await;
        (status.as_u16(), body)
    }

    /// Whether `handle` is in the admin key's `GET /changesets` list.
    async fn listed(test: &TestApp, admin: &str, handle: &str) -> bool {
        let (_, listed) = send(test, "GET", "/api/v1/changesets", Some(admin), None).await;
        handles(&listed).iter().any(|open| open == handle)
    }

    /// The (actor, message) of `path`'s revision `revision` in `main`.
    async fn revision_of(test: &TestApp, path: &str, revision: i32) -> (String, String) {
        let accounts = test.app.accounts().executor();
        sqlx::query_as(
            "SELECT actor, message FROM catalog.revisions \
             WHERE catalog_id = $1 AND path = $2 AND revision = $3",
        )
        .bind(crate::db::catalog_id(accounts, "main").await)
        .bind(path)
        .bind(revision)
        .fetch_one(&crate::db::store_pool(accounts))
        .await
        .expect("the revision exists")
    }

    /// The handles of a `GET /changesets` body, in its order.
    fn handles(listed: &serde_json::Value) -> Vec<String> {
        listed["changesets"]
            .as_array()
            .expect("a list")
            .iter()
            .filter_map(|row| row["changeset"].as_str().map(str::to_string))
            .collect()
    }

    /// A matrix row; the body is always empty here.
    fn row(method: &'static str, uri: String, statuses: [u16; 4]) -> Row {
        let [read, write, admin, stranger] = statuses;
        Row {
            method,
            uri,
            body: None,
            read,
            write,
            admin,
            stranger,
        }
    }

    /// M13 Task 4: `GET /changesets` runs `list_changesets`, so a
    /// suspended catalog's changesets are left out for a member key and
    /// naming the catalog is a 403; an admin still lists them.
    #[tokio::test]
    async fn the_changeset_list_leaves_out_a_suspended_catalog_but_to_an_admin() {
        use crate::http::api::testing::{assert_answers, Expect};
        use crate::mcp::testing::suspend_catalogs;
        let test = app().await;
        let keys = keys(&test).await;
        let writers = open_as(&test, &keys.write, "systems/gamma.md").await;
        suspend_catalogs(&test, &["main"]).await;

        let (status, members) =
            send(&test, "GET", "/api/v1/changesets", Some(&keys.write), None).await;
        let (_, admins) = send(&test, "GET", "/api/v1/changesets", Some(&keys.admin), None).await;

        assert_eq!(status.as_u16(), 200, "{members}");
        assert!(handles(&members).is_empty(), "{members}");
        assert_eq!(handles(&admins), [writers]);
        let named = "/api/v1/changesets?catalog=main";
        let expect = |who, key, status, error| Expect {
            method: "GET",
            uri: named,
            body: None,
            key,
            who,
            status,
            error,
        };
        assert_answers(
            &test,
            &[
                expect("write", &keys.write, 403, Some("Suspended")),
                expect("stranger", &keys.stranger, 404, Some("CatalogNotSelected")),
                expect("admin", &keys.admin, 200, None),
            ],
        )
        .await;
    }

    #[tokio::test]
    async fn changeset_routes_follow_the_tools_access_rules() {
        let test = app().await;
        let keys = keys(&test).await;
        let writers = open_as(&test, &keys.write, "systems/gamma.md").await;
        let admins = open_as(&test, &keys.admin, "systems/delta.md").await;
        let published = open_as(&test, &keys.admin, "systems/epsilon.md").await;
        assert_matrix_with(
            &test,
            &keys,
            &[
                row("GET", "/api/v1/changesets".into(), [200, 200, 200, 200]),
                row(
                    "GET",
                    "/api/v1/changesets?catalog=main".into(),
                    [200, 200, 200, 404],
                ),
                row(
                    "GET",
                    format!("/api/v1/changesets/{writers}/diff"),
                    [404, 200, 200, 404],
                ),
                // Another key's handle is unknown to all but an admin; the
                // admin publishes it (and runs last, after the refusals).
                Row {
                    body: Some(json!({"message": "Add epsilon"})),
                    ..row(
                        "POST",
                        format!("/api/v1/changesets/{published}/publish"),
                        [404, 404, 200, 404],
                    )
                },
                row(
                    "DELETE",
                    format!("/api/v1/changesets/{admins}"),
                    [404, 404, 200, 404],
                ),
            ],
        )
        .await;
    }

    #[tokio::test]
    async fn a_key_lists_its_own_changesets_and_an_admin_every_keys() {
        let test = app().await;
        let keys = keys(&test).await;
        let writers = open_as(&test, &keys.write, "systems/gamma.md").await;
        let admins = open_as(&test, &keys.admin, "systems/delta.md").await;
        let uri = "/api/v1/changesets?catalog=main";

        let (_, as_writer) = send(&test, "GET", uri, Some(&keys.write), None).await;
        assert_eq!(handles(&as_writer), vec![writers.clone()]);
        assert_eq!(as_writer["changesets"][0]["key_name"], "writer");
        assert_eq!(
            as_writer["changesets"][0]["paths"],
            json!(["/systems/gamma.md"])
        );
        assert_eq!(as_writer["changesets"][0]["idle"], false);

        let (_, as_admin) = send(&test, "GET", uri, Some(&keys.admin), None).await;
        let mut all = handles(&as_admin);
        all.sort();
        let mut want = vec![writers, admins];
        want.sort();
        assert_eq!(all, want);

        let (_, as_reader) = send(&test, "GET", uri, Some(&keys.read), None).await;
        assert!(handles(&as_reader).is_empty());
    }

    #[tokio::test]
    async fn the_diff_shows_each_pending_file() {
        let test = app().await;
        let keys = keys(&test).await;
        let handle = open_as(&test, &keys.write, "systems/gamma.md").await;
        let uri = format!("/api/v1/changesets/{handle}/diff");
        let (status, body) = send(&test, "GET", &uri, Some(&keys.write), None).await;
        assert_eq!(status.as_u16(), 200, "{body}");
        assert_eq!(body["catalog"], "main");
        assert_eq!(body["files"][0]["path"], "/systems/gamma.md");
        assert_eq!(body["files"][0]["change"], "create");
        let text = body["files"][0]["diff"].as_str().expect("a diff");
        assert!(text.contains("+# Gamma"), "{text}");
        let unknown = "/api/v1/changesets/no-such-handle/diff";
        let (status, _) = send(&test, "GET", unknown, Some(&keys.admin), None).await;
        assert_eq!(status.as_u16(), 404);
    }

    /// M14 Task 7a: the REST routes pass the request's source to the
    /// tools, so an admin session's discard of another key's changeset is
    /// recorded as the console, and the owner's own is not recorded.
    #[tokio::test]
    async fn an_admin_sessions_discard_is_recorded_from_the_console() {
        use crate::http::api::testing::{Browser, Payload};
        use crate::store::audit::testing::entries;
        let test = app().await;
        let keys = keys(&test).await;
        let theirs = open_as(&test, &keys.write, "systems/gamma.md").await;
        let own = open_as(&test, &keys.write, "systems/delta.md").await;
        let user = crate::db::TEST_USER;
        crate::invalidate::testing::applied(&test.app, async {
            crate::store::users::set_admin(
                test.app.accounts().executor(),
                user,
                AdminChange {
                    on: true,
                    allow_last: false,
                },
                &AuditActor::cli(),
                Utc::now(),
            )
            .await
            .expect("make the user an admin");
        })
        .await;
        let mut admin = Browser::session_for(&test, user).await;

        let mine = format!("/api/v1/changesets/{own}");
        let (by_owner, _) = send(&test, "DELETE", &mine, Some(&keys.write), None).await;
        let uri = format!("/api/v1/changesets/{theirs}");
        let (status, body) = admin
            .send(&test, "DELETE", &uri, Payload::Empty)
            .await
            .parts();

        assert_eq!(by_owner.as_u16(), 200);
        assert_eq!(status.as_u16(), 200, "{body}");
        let recorded = entries(
            test.app.accounts().executor(),
            "catalog.changeset_discarded",
        )
        .await;
        assert_eq!(recorded.len(), 1, "{recorded:?}");
        let entry = &recorded[0];
        assert_eq!(entry.source, "console");
        assert_eq!(entry.actor_label, "console session");
        assert_eq!(entry.actor_person.as_deref(), Some(user));
        assert!(entry.actor_operator);
        assert_eq!(entry.target.as_deref(), Some("main"));
        assert_eq!(
            entry.details,
            json!({"changeset": theirs, "files": 1, "reason": "admin"})
        );
    }

    #[tokio::test]
    async fn the_owner_discards_its_changeset_for_good() {
        let test = app().await;
        let keys = keys(&test).await;
        let handle = open_as(&test, &keys.write, "systems/gamma.md").await;
        let uri = format!("/api/v1/changesets/{handle}");
        let (status, body) = send(&test, "DELETE", &uri, Some(&keys.write), None).await;
        assert_eq!(status.as_u16(), 200, "{body}");
        assert_eq!(body["removed"], 1);
        assert_eq!(body["closed"], true);
        let (_, listed) = send(&test, "GET", "/api/v1/changesets", Some(&keys.write), None).await;
        assert!(handles(&listed).is_empty());
        // The handle is closed: its diff and a second discard are refused.
        let diff = format!("{uri}/diff");
        let (status, _) = send(&test, "GET", &diff, Some(&keys.write), None).await;
        assert_eq!(status.as_u16(), 409);
        let (status, _) = send(&test, "DELETE", &uri, Some(&keys.write), None).await;
        assert_eq!(status.as_u16(), 409);
    }

    /// M13 Task 3: the changeset routes resolve a handle through the
    /// tools' `ToolContext::changeset`, so a suspended catalog's changeset
    /// is refused to its non-admin owner with `Suspended`, stays unknown to
    /// a key that does not reach it, and still answers an admin.
    #[tokio::test]
    async fn changeset_routes_refuse_a_suspended_catalog_but_to_an_admin() {
        use crate::http::api::testing::{assert_answers, Expect};
        use crate::mcp::testing::suspend_catalogs;
        let test = app().await;
        let keys = keys(&test).await;
        let writers = open_as(&test, &keys.write, "systems/gamma.md").await;
        let admins = open_as(&test, &keys.admin, "systems/delta.md").await;
        suspend_catalogs(&test, &["main"]).await;
        let diff = format!("/api/v1/changesets/{writers}/diff");
        let mine = format!("/api/v1/changesets/{writers}");
        let theirs = format!("/api/v1/changesets/{admins}");
        let expect = |method, uri, who, key, status, error| Expect {
            method,
            uri,
            body: None,
            key,
            who,
            status,
            error,
        };

        assert_answers(
            &test,
            &[
                expect("GET", &diff, "write", &keys.write, 403, Some("Suspended")),
                expect(
                    "GET",
                    &diff,
                    "stranger",
                    &keys.stranger,
                    404,
                    Some("UnknownChangeset"),
                ),
                expect("GET", &diff, "admin", &keys.admin, 200, None),
                expect(
                    "DELETE",
                    &mine,
                    "write",
                    &keys.write,
                    403,
                    Some("Suspended"),
                ),
                expect(
                    "DELETE",
                    &theirs,
                    "stranger",
                    &keys.stranger,
                    404,
                    Some("UnknownChangeset"),
                ),
                expect("DELETE", &theirs, "admin", &keys.admin, 200, None),
            ],
        )
        .await;
    }

    /// The owner publishes its changeset: the file is published with the
    /// message and the owner as actor, and the handle is closed.
    #[tokio::test]
    async fn the_owner_publishes_its_changeset_and_the_handle_closes() {
        let test = app().await;
        let keys = keys(&test).await;
        let handle = open_as(&test, &keys.write, "systems/gamma.md").await;

        let (status, body) = publish_with(&test, &keys.write, &handle, "  Add gamma  ").await;

        assert_eq!(status, 200, "{body}");
        assert_eq!(body["published"], true);
        assert_eq!(body["catalog"], "main");
        assert_eq!(body["changeset"], handle.as_str());
        assert_eq!(
            body["revisions"],
            json!([{"path": "/systems/gamma.md", "revision": 1, "op": "create"}])
        );
        assert!(body["warnings"].is_array(), "{body}");
        assert_eq!(body["replayed"], json!([]));
        // The message is trimmed, as the tool trims it.
        assert_eq!(
            revision_of(&test, "systems/gamma.md", 1).await,
            ("writer".to_string(), "Add gamma".to_string())
        );
        let read = "/api/v1/catalogs/main/concepts/systems/gamma.md";
        let (status, document) = send(&test, "GET", read, Some(&keys.read), None).await;
        assert_eq!(status.as_u16(), 200, "{document}");
        assert!(
            document["body"]
                .as_str()
                .is_some_and(|text| text.contains("Draft.")),
            "{document}"
        );
        assert!(!listed(&test, &keys.admin, &handle).await);
        // The handle is spent: another publish and its diff are refused.
        let (again, refused) = publish_with(&test, &keys.write, &handle, "Again").await;
        assert_eq!((again, refused["error"].as_str()), (409, Some("Refused")));
        let diff = format!("/api/v1/changesets/{handle}/diff");
        let (status, _) = send(&test, "GET", &diff, Some(&keys.write), None).await;
        assert_eq!(status.as_u16(), 409);
    }

    /// An admin, by key or by console session, publishes an agent's
    /// changeset as itself: the revision and the `catalog.published` entry
    /// name the publishing admin, never the key that opened the changeset.
    /// A non-admin session owns no changeset, so to it the handle is unknown.
    #[tokio::test]
    async fn an_admin_publishes_an_agents_changeset_as_itself() {
        use crate::http::api::testing::{Browser, Payload};
        use crate::store::audit::testing::entries;
        let test = app().await;
        let keys = keys(&test).await;
        let by_key = open_as(&test, &keys.write, "systems/gamma.md").await;
        let by_session = open_as(&test, &keys.write, "systems/delta.md").await;
        let user = crate::db::TEST_USER;
        let uri = format!("/api/v1/changesets/{by_session}/publish");
        let body = || Payload::Json(json!({"message": "Reviewed delta"}));
        let mut member = Browser::session_for(&test, user).await;
        let (unknown, refused) = member.send(&test, "POST", &uri, body()).await.parts();
        crate::invalidate::testing::applied(&test.app, async {
            crate::store::users::set_admin(
                test.app.accounts().executor(),
                user,
                AdminChange {
                    on: true,
                    allow_last: false,
                },
                &AuditActor::cli(),
                Utc::now(),
            )
            .await
            .expect("make the user an admin");
        })
        .await;
        let mut admin = Browser::session_for(&test, user).await;

        let (status, keyed) = publish_with(&test, &keys.admin, &by_key, "Reviewed gamma").await;
        let (session_status, sessions) = admin.send(&test, "POST", &uri, body()).await.parts();

        assert_eq!(
            (unknown.as_u16(), refused["error"].as_str()),
            (404, Some("UnknownChangeset")),
            "a member's session owns no changeset: {refused}"
        );
        assert_eq!(status, 200, "{keyed}");
        assert_eq!(session_status.as_u16(), 200, "{sessions}");
        assert_eq!(sessions["published"], true);
        assert_eq!(
            revision_of(&test, "systems/gamma.md", 1).await,
            ("admin".to_string(), "Reviewed gamma".to_string())
        );
        assert_eq!(
            revision_of(&test, "systems/delta.md", 1).await,
            (user.to_string(), "Reviewed delta".to_string())
        );
        let recorded = entries(test.app.accounts().executor(), "catalog.published").await;
        assert_eq!(recorded.len(), 2, "{recorded:?}");
        let keyed = recorded
            .iter()
            .find(|entry| entry.details["changeset"] == by_key.as_str())
            .expect("the key's publish is recorded");
        assert_eq!(
            (keyed.source.as_str(), keyed.actor_label.as_str()),
            ("api", "api key")
        );
        let session = recorded
            .iter()
            .find(|entry| entry.details["changeset"] == by_session.as_str())
            .expect("the session's publish is recorded");
        assert_eq!(session.source, "console");
        assert_eq!(session.actor_label, "console session");
        assert_eq!(session.actor_person.as_deref(), Some(user));
        assert!(session.actor_operator);
        assert_eq!(session.target.as_deref(), Some("main"));
        assert_eq!(session.details["files"], 1);
    }

    /// A blank message, a body without one, an unknown handle and a
    /// discarded one publish nothing; the first two leave the changeset open.
    #[tokio::test]
    async fn a_refused_publish_publishes_nothing() {
        let test = app().await;
        let keys = keys(&test).await;
        let handle = open_as(&test, &keys.write, "systems/gamma.md").await;
        let uri = format!("/api/v1/changesets/{handle}/publish");

        let (blank, blank_body) = publish_with(&test, &keys.write, &handle, " \n\t ").await;
        let (no_message, _) = send(&test, "POST", &uri, Some(&keys.write), Some(json!({}))).await;
        let (unknown, unknown_body) =
            publish_with(&test, &keys.admin, "no-such-handle", "Publish").await;

        assert_eq!(
            (blank, blank_body["error"].as_str()),
            (409, Some("Refused")),
            "{blank_body}"
        );
        assert_eq!(no_message.as_u16(), 422);
        assert_eq!(
            (unknown, unknown_body["error"].as_str()),
            (404, Some("UnknownChangeset"))
        );
        assert!(listed(&test, &keys.admin, &handle).await);
        assert!(test
            .app
            .catalog("main")
            .await
            .unwrap()
            .expect("main")
            .current()
            .snapshot()
            .get("systems/gamma.md")
            .is_none());
        let gone = format!("/api/v1/changesets/{handle}");
        let (discarded, _) = send(&test, "DELETE", &gone, Some(&keys.write), None).await;
        assert_eq!(discarded.as_u16(), 200);
        let (closed, closed_body) = publish_with(&test, &keys.write, &handle, "Publish").await;
        assert_eq!(
            (closed, closed_body["error"].as_str()),
            (409, Some("Refused"))
        );
    }

    /// An empty changeset reports `published: false` and stays open; one
    /// whose only edit matches the published text is closed with
    /// `published: true` and no revisions, and records no publish.
    #[tokio::test]
    async fn an_empty_changeset_stays_open_and_a_no_op_one_closes() {
        use crate::mcp::changesets::{revert, RevertParams};
        use crate::store::audit::testing::entries;
        let test = app().await;
        let keys = keys(&test).await;
        let empty = open_empty(&test, &keys.write).await;
        let no_op = open_empty(&test, &keys.write).await;
        let same = RevertParams {
            path: "/systems/beta.md".to_string(),
            revision: 1,
            changeset: no_op.clone(),
        };
        revert(&ctx_of(&test, &keys.write).await, same)
            .await
            .expect("stage the published text");

        let (status, nothing) = publish_with(&test, &keys.write, &empty, "Nothing").await;
        let (closed, unchanged) = publish_with(&test, &keys.write, &no_op, "Same").await;

        assert_eq!(status, 200, "{nothing}");
        assert_eq!(nothing["published"], false);
        assert_eq!(nothing["revisions"], json!([]));
        assert!(
            nothing["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("same turn")),
            "an empty publish says why and names the parallel-call case: {nothing}"
        );
        assert!(
            unchanged.get("reason").is_none(),
            "a no-op publish has no reason"
        );
        assert!(listed(&test, &keys.admin, &empty).await, "it stays open");
        assert_eq!(closed, 200, "{unchanged}");
        assert_eq!(unchanged["published"], true);
        assert_eq!(unchanged["revisions"], json!([]));
        assert!(!listed(&test, &keys.admin, &no_op).await, "it is closed");
        assert!(entries(test.app.accounts().executor(), "catalog.published")
            .await
            .is_empty());
    }

    /// A conflict (409, with its files) and a lint failure (422, with its
    /// errors) publish nothing and leave the changeset open with its entry.
    #[tokio::test]
    async fn a_conflict_or_lint_failure_keeps_the_changeset() {
        let test = app().await;
        let keys = keys(&test).await;
        let first = open_as(&test, &keys.write, "systems/gamma.md").await;
        let second = open_with(
            &test,
            &keys.write,
            "systems/gamma.md",
            "---\ntype: system\ntitle: Gamma\n---\n# Gamma\n\nOther.\n",
        )
        .await;
        let broken = open_with(
            &test,
            &keys.write,
            "systems/zeta.md",
            "---\ntype: system\ntitle: Zeta\n---\n# Zeta\n\nSee [gone](/gone.md).\n",
        )
        .await;
        let (ok, _) = publish_with(&test, &keys.write, &first, "Gamma first").await;
        assert_eq!(ok, 200);

        let (conflict, conflicting) =
            publish_with(&test, &keys.write, &second, "Gamma second").await;
        let (lint, failing) = publish_with(&test, &keys.write, &broken, "Zeta").await;

        assert_eq!(conflict, 409, "{conflicting}");
        assert_eq!(conflicting["error"], "Conflict");
        assert_eq!(
            conflicting["files"],
            json!([{"path": "/systems/gamma.md", "base_revision": null, "current_revision": 1}])
        );
        assert_eq!(lint, 422, "{failing}");
        assert_eq!(failing["error"], "LintFailed");
        let errors = failing["errors"].as_array().expect("errors");
        assert!(
            errors
                .iter()
                .any(|e| e["file"] == "/systems/zeta.md" && e["rule"] == "broken-link"),
            "{failing}"
        );
        let snapshot = test
            .app
            .catalog("main")
            .await
            .unwrap()
            .expect("main")
            .current();
        assert!(snapshot
            .snapshot()
            .get("systems/gamma.md")
            .is_some_and(|doc| doc.text().contains("Draft.")));
        assert!(snapshot.snapshot().get("systems/zeta.md").is_none());
        for handle in [&second, &broken] {
            assert!(
                listed(&test, &keys.admin, handle).await,
                "{handle} stays open"
            );
            let diff = format!("/api/v1/changesets/{handle}/diff");
            let (status, pending) = send(&test, "GET", &diff, Some(&keys.write), None).await;
            assert_eq!(status.as_u16(), 200);
            assert_eq!(
                pending["files"].as_array().map(Vec::len),
                Some(1),
                "{pending}"
            );
        }
    }

    /// The route refuses a suspended catalog's changeset as the `publish`
    /// tool does: `Suspended` to its owner, unknown to a key that does not
    /// reach it; an admin still publishes it.
    #[tokio::test]
    async fn the_route_refuses_a_suspended_catalog_as_the_tool_does() {
        use crate::mcp::changesets::{publish, PublishParams};
        use crate::mcp::testing::suspend_catalogs;
        let test = app().await;
        let keys = keys(&test).await;
        let writers = open_as(&test, &keys.write, "systems/gamma.md").await;
        suspend_catalogs(&test, &["main"]).await;

        let (status, refused) = publish_with(&test, &keys.write, &writers, "Gamma").await;
        let tool = publish(
            &ctx_of(&test, &keys.write).await,
            PublishParams {
                message: "Gamma".to_string(),
                changeset: writers.clone(),
            },
        )
        .await
        .expect_err("the tool refuses too");
        let (stranger, unknown) = publish_with(&test, &keys.stranger, &writers, "Gamma").await;
        let (admin, _) = publish_with(&test, &keys.admin, &writers, "Gamma").await;

        assert_eq!(
            (status, refused["error"].as_str()),
            (403, Some("Suspended"))
        );
        assert_eq!(tool.kind(), "Suspended");
        assert_eq!(
            (stranger, unknown["error"].as_str()),
            (404, Some("UnknownChangeset"))
        );
        assert_eq!(admin, 200);
    }

    /// The route refuses as the `publish` tool does where the catalog needs
    /// an unlock (`UnlockRequired`) and, for the maintenance runner, where
    /// the catalog turned maintenance off (`maintenance: "off"`); neither
    /// changeset is published or closed.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_route_refuses_a_locked_or_opted_out_catalog_as_the_tool_does() {
        use crate::mcp::changesets::{publish, PublishParams};
        use crate::mcp::testing::{require_unlock, staff_key, staff_root};
        use crate::store::keys::Access;
        let test = app().await;
        let keys = keys(&test).await;
        staff_root(&test).await;
        // Keys of a user outside main's owner team: a writer, and a
        // reporting writer (the maintenance runner, exempt for reads only).
        let staff = staff_key(&test, "staff-writer", Access::Write, false).await;
        let runner = staff_key(&test, "writing-runner", Access::Write, true).await;
        let locked = open_as(&test, &staff, "systems/gamma.md").await;
        let opted_out = open_as(&test, &runner, "systems/delta.md").await;
        require_unlock(&test).await;
        crate::invalidate::testing::applied(&test.app, async {
            let mut tx = test.app.accounts().executor().begin().await.expect("begin");
            let id: i64 = sqlx::query_scalar(
                "UPDATE catalogs SET maintenance = false WHERE name = 'main' RETURNING id",
            )
            .fetch_one(&mut *tx)
            .await
            .expect("turn maintenance off");
            let main = crate::invalidate::Invalidation::CatalogMeta {
                id,
                name: "main".to_owned(),
            };
            crate::invalidate::invalidate(&mut tx, &[main])
                .await
                .expect("invalidate");
            tx.commit().await.expect("commit");
        })
        .await;
        let tool = async |secret: &str, handle: &str| {
            let ctx = ctx_of(&test, secret).await;
            let params = PublishParams {
                message: "Publish".to_string(),
                changeset: handle.to_string(),
            };
            publish(&ctx, params).await
        };

        let (unlock, needs) = publish_with(&test, &staff, &locked, "Publish").await;
        let locked_tool = tool(&staff, &locked).await.expect_err("the tool refuses");
        let (off, maintenance) = publish_with(&test, &runner, &opted_out, "Publish").await;
        let off_tool = tool(&runner, &opted_out)
            .await
            .expect_err("the tool refuses");

        assert_eq!(
            (unlock, needs["error"].as_str(), needs["catalog"].as_str()),
            (403, Some("UnlockRequired"), Some("main")),
            "{needs}"
        );
        assert_eq!(locked_tool.kind(), "UnlockRequired");
        assert_eq!(off, 403, "{maintenance}");
        assert_eq!(maintenance["maintenance"], "off");
        assert!(matches!(
            off_tool,
            crate::error::OkfError::MaintenanceOff { .. }
        ));
        for handle in [&locked, &opted_out] {
            assert!(
                listed(&test, &keys.admin, handle).await,
                "{handle} stays open"
            );
        }
    }
}
