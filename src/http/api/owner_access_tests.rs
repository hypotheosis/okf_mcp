//! Route tests for team owners managing member access and entitlements
//! (issue #352): who may (an owner of the catalog's shared team in a console
//! session), who may not (keys, OAuth grants, non-owners, owners of other
//! teams, owners of personal teams), and that nothing leaks across tenants.

use axum::http::StatusCode;
use chrono::Utc;
use serde_json::{json, Value};

use crate::db::TEST_USER;
use crate::http::api::testing::{every_catalog_admin, send, Browser, Payload, MAIN};
use crate::mcp::testing::{app_with, TestApp};
use crate::store::audit::AuditActor;
use crate::store::keys::{self as key_rows, Access, Grants, NewKey};
use crate::store::oauth::testing::{insert_client, insert_grant, mint_access, oauth_config};

const RESOURCE: &str = "https://okf.example.com/mcp";

/// The fixture: `staff` (owner olive, member mia) owns `docs`; `rival`
/// (owner pat, member rex) owns `theirs`; otto is in no shared team. The
/// fixture user's personal team owns `main`.
struct World {
    test: TestApp,
    root: String,
}

/// Builds the [`World`] (OAuth on, so a grant's token is accepted).
async fn world() -> World {
    let test = app_with(&[("main", MAIN)], oauth_config("https://okf.example.com")).await;
    let root = every_catalog_admin(&test).await;
    for user in ["olive", "mia", "otto", "pat", "rex"] {
        let (status, body) = send(
            &test,
            "POST",
            "/api/v1/users",
            Some(&root),
            Some(json!({ "name": user })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    for (team, owner, member) in [("staff", "olive", "mia"), ("rival", "pat", "rex")] {
        let (status, body) = send(
            &test,
            "POST",
            "/api/v1/teams",
            Some(&root),
            Some(json!({ "name": team, "owner": owner })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let (status, body) = send(
            &test,
            "PUT",
            &format!("/api/v1/teams/{team}/members/{member}"),
            Some(&root),
            Some(json!({ "role": "member" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    for (catalog, team) in [("docs", "staff"), ("theirs", "rival")] {
        let (status, body) = send(
            &test,
            "POST",
            "/api/v1/catalogs",
            Some(&root),
            Some(json!({ "name": catalog, "description": "", "owner": team })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    World { test, root }
}

/// A key of `user`: `Access::Write` on every catalog, or `Admin` on `docs`
/// only.
async fn key_of(test: &TestApp, user: &str, name: &'static str, scoped_admin: bool) -> String {
    let docs = ["docs".to_string()];
    let new = NewKey {
        name,
        access: if scoped_admin {
            Access::Admin
        } else {
            Access::Write
        },
        maintenance_report: false,
        grants: if scoped_admin {
            Grants::Catalogs(&docs)
        } else {
            Grants::All
        },
        default_catalog: None,
        actor: name,
        user,
        self_issued: false,
        expires_at: None,
        levels: &[],
    };
    key_rows::create_with_id(
        test.app.accounts().executor(),
        &new,
        &AuditActor::cli(),
        Utc::now(),
    )
    .await
    .expect("create a key")
    .key
}

/// An OAuth grant of `user`, and its access token.
async fn grant_token_of(test: &TestApp, user: &str) -> String {
    let now = test.app.clock().now();
    let client = insert_client(
        test.app.accounts().executor(),
        "https://client.example.com/client.json",
        "Example Client",
        now,
    )
    .await;
    let grant = insert_grant(
        test.app.accounts().executor(),
        &client,
        Access::Write,
        &["docs"],
        now,
    )
    .await;
    sqlx::query(
        "UPDATE api_keys SET user_id = (SELECT id FROM users WHERE name = $1) WHERE id = $2",
    )
    .bind(user)
    .bind(&grant)
    .execute(test.app.accounts().executor())
    .await
    .expect("assign the grant to its user");
    mint_access(&test.app, &grant, RESOURCE, chrono::TimeDelta::hours(1)).await
}

/// One request of a console session.
async fn as_session(
    browser: &mut Browser,
    world: &World,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let payload = body.map_or(Payload::Empty, Payload::Json);
    browser
        .send(&world.test, method, uri, payload)
        .await
        .parts()
}

const MEMBER_ACCESS: &str = "/api/v1/catalogs/docs/member-access";
const TEAM_ENTITLEMENTS: &str = "/api/v1/catalogs/docs/entitlements";
const MIA_ON_DOCS: &str = "/api/v1/users/mia/entitlements/docs";

/// Every owner-managed request, as `(method, uri, body)`.
fn controls() -> Vec<(&'static str, &'static str, Option<Value>)> {
    vec![
        ("GET", MEMBER_ACCESS, None),
        ("PUT", MEMBER_ACCESS, Some(json!({"access": "read"}))),
        ("GET", TEAM_ENTITLEMENTS, None),
        ("PUT", MIA_ON_DOCS, Some(json!({"access": "read"}))),
        ("DELETE", MIA_ON_DOCS, None),
    ]
}

/// The stored member access of `docs`.
async fn stored_member_access(world: &World) -> String {
    sqlx::query_scalar("SELECT member_access FROM catalogs WHERE name = 'docs'")
        .fetch_one(world.test.app.accounts().executor())
        .await
        .expect("read the member access")
}

/// How many entitlements `docs` has.
async fn docs_entitlements(world: &World) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM user_catalogs e JOIN catalogs c ON c.id = e.catalog_id \
         WHERE c.name = 'docs'",
    )
    .fetch_one(world.test.app.accounts().executor())
    .await
    .expect("count the entitlements")
}

#[tokio::test(flavor = "multi_thread")]
async fn an_owner_session_sets_member_access_and_it_is_audited_with_the_owner_as_actor() {
    // Arrange
    let world = world().await;
    let mut olive = Browser::session_for(&world.test, "olive").await;
    let generation = crate::store::catalogs::admin_generation(world.test.app.accounts().executor())
        .await
        .unwrap();

    // Act
    let (read, now) = as_session(&mut olive, &world, "GET", MEMBER_ACCESS, None).await;
    let (put, answer) = as_session(
        &mut olive,
        &world,
        "PUT",
        MEMBER_ACCESS,
        Some(json!({"access": "read"})),
    )
    .await;
    let (_, after) = as_session(&mut olive, &world, "GET", MEMBER_ACCESS, None).await;
    let (bad, _) = as_session(
        &mut olive,
        &world,
        "PUT",
        MEMBER_ACCESS,
        Some(json!({"access": "admin"})),
    )
    .await;

    // Assert
    assert_eq!(read, StatusCode::OK);
    assert_eq!(now, json!({"name": "docs", "member_access": "write"}));
    assert_eq!(put, StatusCode::OK, "{answer}");
    assert_eq!(
        answer,
        json!({"name": "docs", "previous": "write", "member_access": "read"})
    );
    assert_eq!(after["member_access"], "read");
    assert_eq!(bad, StatusCode::BAD_REQUEST);
    assert!(
        crate::store::catalogs::admin_generation(world.test.app.accounts().executor())
            .await
            .unwrap()
            > generation
    );
    let changes = crate::store::audit::testing::entries(
        world.test.app.accounts().executor(),
        "catalog.member_access_changed",
    )
    .await;
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!(changes[0].source, "console");
    assert_eq!(changes[0].actor_person.as_deref(), Some("olive"));
    assert_eq!(changes[0].details, json!({"from": "write", "to": "read"}));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_owner_session_entitles_a_team_member_and_sees_only_that_catalog() {
    // Arrange: an admin gave mia another catalog already.
    let world = world().await;
    let (status, _) = send(
        &world.test,
        "PUT",
        "/api/v1/users/mia/entitlements/main",
        Some(&world.root),
        Some(json!({"access": "read"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let mut olive = Browser::session_for(&world.test, "olive").await;

    // Act
    let (put, entitled) = as_session(
        &mut olive,
        &world,
        "PUT",
        MIA_ON_DOCS,
        Some(json!({"access": "write"})),
    )
    .await;
    let (_, view) = as_session(&mut olive, &world, "GET", TEAM_ENTITLEMENTS, None).await;
    let (_, admins_view) = send(
        &world.test,
        "GET",
        "/api/v1/users/mia",
        Some(&world.root),
        None,
    )
    .await;
    let (delete, removed) = as_session(&mut olive, &world, "DELETE", MIA_ON_DOCS, None).await;

    // Assert: the echo names only `docs`, though mia has `main` too.
    assert_eq!(put, StatusCode::OK, "{entitled}");
    assert_eq!(entitled["name"], "mia");
    let echoed = entitled["entitlements"].as_array().unwrap();
    assert_eq!(echoed.len(), 1, "{entitled}");
    assert_eq!(echoed[0]["catalog"], "docs");
    assert_eq!(echoed[0]["access"], "write");
    assert_eq!(
        admins_view["entitlements"].as_array().unwrap().len(),
        2,
        "the admin still sees both"
    );
    assert_eq!(view["name"], "docs");
    assert_eq!(
        view["members"],
        json!([
            {"user": "mia", "label": "Unnamed user", "role": "member"},
            {"user": "olive", "label": "Unnamed user", "role": "owner"},
        ])
    );
    assert_eq!(
        view["entitlements"],
        json!([{"user": "mia", "label": "Unnamed user", "access": "write"}])
    );
    assert_eq!(delete, StatusCode::OK, "{removed}");
    assert_eq!(
        removed["entitlements"],
        json!([]),
        "only docs, and it is gone"
    );
    let events = crate::store::audit::testing::entries(
        world.test.app.accounts().executor(),
        "user.entitlement_set",
    )
    .await;
    // The admin's entitlement of `main` and the owner's of `docs`.
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[1].actor_person.as_deref(), Some("olive"));
    assert_eq!(events[1].source, "console");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_owners_keys_and_oauth_grants_are_refused_every_control() {
    // Arrange: olive owns `staff`; her keys and her grant are not her session.
    let world = world().await;
    let write_key = key_of(&world.test, "olive", "olive-write", false).await;
    let admin_key = key_of(&world.test, "olive", "olive-docs-admin", true).await;
    let grant = grant_token_of(&world.test, "olive").await;

    // Act and assert
    for (who, token) in [("write key", &write_key), ("grant", &grant)] {
        for (method, uri, body) in controls() {
            let (status, answer) = send(&world.test, method, uri, Some(token), body).await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "{who} {method} {uri}: {answer}"
            );
            assert_eq!(answer["error"], "Forbidden", "{who} {method} {uri}");
        }
    }
    // A key that is admin on `docs` only keeps its reads (as before) but
    // cannot write, and an owner-ness it does not carry changes nothing.
    for (method, uri, body) in controls() {
        let (status, answer) = send(&world.test, method, uri, Some(&admin_key), body).await;
        let read = method == "GET";
        assert_eq!(
            status.as_u16(),
            if read { 200 } else { 403 },
            "scoped admin {method} {uri}: {answer}"
        );
    }
    assert_eq!(stored_member_access(&world).await, "write");
    assert_eq!(docs_entitlements(&world).await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_non_owner_another_teams_owner_a_stranger_and_a_personal_owner_are_refused_alike() {
    // Arrange: mia is a plain member; pat owns another team; otto is in
    // none; the fixture user owns only their personal team, which owns
    // `main`.
    let world = world().await;
    let mut refusals: Vec<(String, StatusCode, Value)> = Vec::new();

    // Act
    for user in ["mia", "pat", "otto"] {
        let mut browser = Browser::session_for(&world.test, user).await;
        for (method, uri, body) in controls() {
            let (status, answer) = as_session(&mut browser, &world, method, uri, body).await;
            refusals.push((format!("{user} {method} {uri}"), status, answer));
        }
    }
    let mut personal = Browser::session_for(&world.test, TEST_USER).await;
    let main_controls = [
        ("GET", "/api/v1/catalogs/main/member-access", None),
        (
            "PUT",
            "/api/v1/catalogs/main/member-access",
            Some(json!({"access": "none"})),
        ),
        ("GET", "/api/v1/catalogs/main/entitlements", None),
        (
            "PUT",
            "/api/v1/users/mia/entitlements/main",
            Some(json!({"access": "read"})),
        ),
        ("DELETE", "/api/v1/users/mia/entitlements/main", None),
    ];
    for (method, uri, body) in main_controls {
        let (status, answer) = as_session(&mut personal, &world, method, uri, body).await;
        refusals.push((format!("personal {method} {uri}"), status, answer));
    }

    // Assert: a write is always the one 403; a read is that 403 for a session
    // that reaches the catalog, and the 404 it has always been for one that
    // does not (so an unknown catalog cannot be told from one not reached).
    for (what, status, answer) in &refusals {
        let read = what.contains("GET ");
        if read && *status == StatusCode::NOT_FOUND {
            assert!(
                what.starts_with("pat") || what.starts_with("otto"),
                "{what}: {answer}"
            );
            continue;
        }
        assert_eq!(*status, StatusCode::FORBIDDEN, "{what}: {answer}");
        assert_eq!(answer["error"], "Forbidden", "{what}");
    }
    let main_member_access: String =
        sqlx::query_scalar("SELECT member_access FROM catalogs WHERE name = 'main'")
            .fetch_one(world.test.app.accounts().executor())
            .await
            .unwrap();
    assert_eq!(
        main_member_access, "write",
        "a personal team's owner changed nothing"
    );
    assert_eq!(stored_member_access(&world).await, "write");
    assert_eq!(docs_entitlements(&world).await, 0);
    // The body does not depend on who asked or on what the catalog is.
    let forbidden: Vec<&Value> = refusals
        .iter()
        .filter(|(_, status, _)| *status == StatusCode::FORBIDDEN)
        .map(|(_, _, answer)| answer)
        .collect();
    assert!(forbidden.len() > 10, "{refusals:?}");
    assert!(
        forbidden.iter().all(|answer| *answer == forbidden[0]),
        "{refusals:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_user_and_one_outside_the_team_get_one_refusal_and_nothing_leaks() {
    // Arrange: rex (another tenant) has entitlements the owner must not see.
    let world = world().await;
    let (status, _) = send(
        &world.test,
        "PUT",
        "/api/v1/users/rex/entitlements/main",
        Some(&world.root),
        Some(json!({"access": "read"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let mut olive = Browser::session_for(&world.test, "olive").await;
    let mut answers = Vec::new();

    // Act
    for user in ["ghost", "rex", "otto", "a%00b", TEST_USER, "OLIVE-NOT"] {
        for method in ["PUT", "DELETE"] {
            let body = (method == "PUT").then(|| json!({"access": "read"}));
            let (status, answer) = as_session(
                &mut olive,
                &world,
                method,
                &format!("/api/v1/users/{user}/entitlements/docs"),
                body,
            )
            .await;
            answers.push((format!("{method} {user}"), status, answer));
        }
    }

    // Assert
    for (what, status, answer) in &answers {
        assert_eq!(*status, StatusCode::FORBIDDEN, "{what}: {answer}");
    }
    let first = &answers[0].2;
    assert!(
        answers.iter().all(|(_, _, answer)| answer == first),
        "{answers:?}"
    );
    assert!(
        !first.to_string().contains("rex") && !first.to_string().contains("ghost"),
        "{first}"
    );
    assert_eq!(docs_entitlements(&world).await, 0);
    // The admin routes answer as they did.
    let (unknown, _) = send(
        &world.test,
        "PUT",
        "/api/v1/users/ghost/entitlements/docs",
        Some(&world.root),
        Some(json!({"access": "read"})),
    )
    .await;
    let (outside, answer) = send(
        &world.test,
        "PUT",
        "/api/v1/users/rex/entitlements/docs",
        Some(&world.root),
        Some(json!({"access": "read"})),
    )
    .await;
    assert_eq!(unknown, StatusCode::NOT_FOUND);
    assert_eq!(outside, StatusCode::OK, "{answer}");
    assert_eq!(
        answer["entitlements"].as_array().unwrap().len(),
        2,
        "an admin still gets every entitlement back"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_owner_cannot_entitle_at_admin_and_the_admin_routes_are_unchanged() {
    let world = world().await;
    let mut olive = Browser::session_for(&world.test, "olive").await;

    let (owner, _) = as_session(
        &mut olive,
        &world,
        "PUT",
        MIA_ON_DOCS,
        Some(json!({"access": "admin"})),
    )
    .await;
    let (admin, _) = send(
        &world.test,
        "PUT",
        MIA_ON_DOCS,
        Some(&world.root),
        Some(json!({"access": "admin"})),
    )
    .await;
    let (set, answer) = send(
        &world.test,
        "PUT",
        MEMBER_ACCESS,
        Some(&world.root),
        Some(json!({"access": "none"})),
    )
    .await;

    assert_eq!(owner, StatusCode::BAD_REQUEST);
    assert_eq!(admin, StatusCode::BAD_REQUEST);
    assert_eq!(set, StatusCode::OK, "{answer}");
    assert_eq!(stored_member_access(&world).await, "none");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_team_lists_come_a_page_at_a_time_and_a_continuation_reads_one_list() {
    // Arrange: staff has five members (olive, mia, u1 to u3); mia and the
    // three new ones are entitled to `docs`.
    let world = world().await;
    for user in ["u1", "u2", "u3"] {
        for (method, uri, body) in [
            ("POST", "/api/v1/users".to_string(), json!({ "name": user })),
            (
                "PUT",
                format!("/api/v1/teams/staff/members/{user}"),
                json!({ "role": "member" }),
            ),
        ] {
            let (status, answer) =
                send(&world.test, method, &uri, Some(&world.root), Some(body)).await;
            assert!(status.is_success(), "{answer}");
        }
    }
    for user in ["mia", "u1", "u2", "u3"] {
        let (status, answer) = send(
            &world.test,
            "PUT",
            &format!("/api/v1/users/{user}/entitlements/docs"),
            Some(&world.root),
            Some(json!({"access": "read"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{answer}");
    }
    let mut olive = Browser::session_for(&world.test, "olive").await;
    let names = |view: &Value, list: &str| -> Vec<String> {
        view[list]
            .as_array()
            .expect("a list")
            .iter()
            .map(|row| row["user"].as_str().expect("a user").to_string())
            .collect()
    };

    // Act
    let (status, first) = as_session(
        &mut olive,
        &world,
        "GET",
        &format!("{TEAM_ENTITLEMENTS}?limit=2"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let members_next = first["members_next"]
        .as_str()
        .expect("more members")
        .to_string();
    let (_, members_only) = as_session(
        &mut olive,
        &world,
        "GET",
        &format!("{TEAM_ENTITLEMENTS}?limit=2&list=members&members_after={members_next}"),
        None,
    )
    .await;
    let given_next = first["entitlements_next"]
        .as_str()
        .expect("more entitlements")
        .to_string();
    let (_, given_only) = as_session(
        &mut olive,
        &world,
        "GET",
        &format!("{TEAM_ENTITLEMENTS}?limit=2&list=entitlements&entitlements_after={given_next}"),
        None,
    )
    .await;
    let (bad, _) = as_session(
        &mut olive,
        &world,
        "GET",
        &format!("{TEAM_ENTITLEMENTS}?list=everything"),
        None,
    )
    .await;

    // Assert: two rows a page, in byte order, each row once.
    assert_eq!(names(&first, "members"), ["mia", "olive"]);
    assert_eq!(names(&first, "entitlements"), ["mia", "u1"]);
    assert_eq!(names(&members_only, "members"), ["u1", "u2"]);
    assert_eq!(
        members_only["entitlements"],
        json!([]),
        "only the members were read"
    );
    assert_eq!(members_only["entitlements_next"], Value::Null);
    assert_eq!(names(&given_only, "entitlements"), ["u2", "u3"]);
    assert_eq!(
        given_only["entitlements_next"],
        Value::Null,
        "the last page"
    );
    assert_eq!(given_only["members"], json!([]));
    assert_eq!(bad, StatusCode::BAD_REQUEST);
}
