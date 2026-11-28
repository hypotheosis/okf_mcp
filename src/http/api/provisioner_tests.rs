use super::{provisioner_may, PROVISIONER_ROUTES};
use crate::http::api::testing::{app, every_catalog_admin, send};
use serde_json::json;

/// A provisioner key for TEST_USER.
async fn provisioner(test: &crate::mcp::testing::TestApp) -> String {
    let created = crate::store::keys::create_provisioner(
        test.app.accounts().executor(),
        &crate::store::keys::NewProvisioner {
            name: "signup",
            actor: "signup",
            user: crate::db::TEST_USER,
        },
        &crate::store::audit::AuditActor::cli(),
        chrono::Utc::now(),
    )
    .await
    .unwrap();
    created.key
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provisioner_runs_the_signup_and_offboarding_routes() {
    let test = crate::mcp::testing::app_with(
        &[("main", crate::http::api::testing::MAIN)],
        crate::config::Config {
            public_url: Some("https://example.com".to_string()),
            ..Default::default()
        },
    )
    .await;
    let key = provisioner(&test).await;
    let p = Some(key.as_str());

    let steps: Vec<(&str, &str, Option<serde_json::Value>, u16)> = vec![
        ("POST", "/api/v1/users", Some(json!({"name": "alice"})), 201),
        (
            "PUT",
            "/api/v1/users/alice/email",
            Some(json!({"email": "alice@example.com"})),
            200,
        ),
        ("GET", "/api/v1/users?email=ALICE@example.com", None, 200),
        ("POST", "/api/v1/users/alice/invite", None, 202),
        ("GET", "/api/v1/users/alice", None, 200),
        (
            "POST",
            "/api/v1/teams",
            Some(json!({"name": "acme", "owner": "alice"})),
            201,
        ),
        ("GET", "/api/v1/teams/acme", None, 200),
        ("POST", "/api/v1/users", Some(json!({"name": "bob"})), 201),
        (
            "PUT",
            "/api/v1/teams/acme/members/bob",
            Some(json!({"role": "member"})),
            200,
        ),
        (
            "POST",
            "/api/v1/catalogs",
            Some(json!({"name": "acme-notes", "owner": "acme"})),
            201,
        ),
        (
            "PUT",
            "/api/v1/catalogs/acme-notes/member-access",
            Some(json!({"access": "read"})),
            200,
        ),
        (
            "PUT",
            "/api/v1/teams/acme/suspension",
            Some(json!({"suspended": true})),
            200,
        ),
        (
            "PUT",
            "/api/v1/teams/acme/suspension",
            Some(json!({"suspended": false})),
            200,
        ),
        ("DELETE", "/api/v1/teams/acme/members/bob", None, 200),
        ("DELETE", "/api/v1/catalogs/acme-notes", None, 200),
        ("DELETE", "/api/v1/teams/acme", None, 204),
        ("DELETE", "/api/v1/users/bob", None, 204),
    ];
    for (method, uri, body, want) in steps {
        let (status, answer) = send(&test, method, uri, p, body).await;
        assert_eq!(status.as_u16(), want, "{method} {uri}: {answer}");
        if method == "DELETE" && uri.starts_with("/api/v1/catalogs/") {
            test.settle_cleanups().await;
        }
    }
    let rows =
        crate::store::audit::testing::entries(test.app.accounts().executor(), "user.created").await;
    let alice = rows
        .iter()
        .find(|r| r.person.as_deref() == Some("alice"))
        .unwrap();
    assert!(alice.actor_operator);
    assert_eq!(alice.source, "api");
    let credential: String = sqlx::query_scalar("SELECT c.credential_id FROM audit_entries e JOIN audit_credentials c ON c.id = e.actor_credential_ref WHERE e.action = 'user.invited'").fetch_one(test.app.accounts().executor()).await.unwrap();
    let cached = crate::mcp::testing::cached_principal(&test, &key).await;
    assert_eq!(credential, cached.key_id);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provisioner_never_reaches_an_admin_user() {
    let test = app().await;
    let root = every_catalog_admin(&test).await;
    let key = provisioner(&test).await;
    let p = Some(key.as_str());
    send(
        &test,
        "POST",
        "/api/v1/users",
        Some(&root),
        Some(json!({"name": "staff"})),
    )
    .await;
    send(
        &test,
        "PUT",
        "/api/v1/users/staff/email",
        Some(&root),
        Some(json!({"email": "staff@example.com"})),
    )
    .await;
    send(
        &test,
        "PUT",
        "/api/v1/users/staff/admin",
        Some(&root),
        Some(json!({"admin": true})),
    )
    .await;
    send(
        &test,
        "POST",
        "/api/v1/teams",
        Some(&root),
        Some(json!({"name": "ops", "owner": crate::db::TEST_USER})),
    )
    .await;

    let (lookup, found) = send(
        &test,
        "GET",
        "/api/v1/users?email=staff@example.com",
        p,
        None,
    )
    .await;
    let refused = [
        ("GET", "/api/v1/users/staff", None),
        (
            "PUT",
            "/api/v1/users/staff/email",
            Some(json!({"email": "x@example.com"})),
        ),
        ("POST", "/api/v1/users/staff/invite", None),
        ("DELETE", "/api/v1/users/staff", None),
        (
            "PUT",
            "/api/v1/teams/ops/members/staff",
            Some(json!({"role": "member"})),
        ),
        (
            "POST",
            "/api/v1/teams",
            Some(json!({"name": "mine", "owner": "staff"})),
        ),
    ];

    assert_eq!(lookup, axum::http::StatusCode::OK);
    assert_eq!(
        found["users"],
        json!([]),
        "an admin is not found by a provisioner"
    );
    for (method, uri, body) in refused {
        let (status, answer) = send(&test, method, uri, p, body).await;
        assert_eq!(status.as_u16(), 403, "{method} {uri}: {answer}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provisioner_is_refused_every_other_route_and_every_catalog() {
    let test = app().await;
    let key = provisioner(&test).await;
    let p = Some(key.as_str());
    let refused = [
        ("GET", "/api/v1/users", None),
        // Only the GET of the team list is allowed (issue #502).
        ("HEAD", "/api/v1/teams", None),
        ("GET", "/api/v1/keys", None),
        (
            "POST",
            "/api/v1/keys",
            Some(
                json!({"name": "x", "access": "admin", "all": true, "user": crate::db::TEST_USER}),
            ),
        ),
        (
            "PUT",
            "/api/v1/settings/product_name",
            Some(json!({"value": "x"})),
        ),
        ("PUT", "/api/v1/users/x/admin", Some(json!({"admin": true}))),
        (
            "PUT",
            "/api/v1/users/x/entitlements/main",
            Some(json!({"access": "read"})),
        ),
        ("DELETE", "/api/v1/users/x/sessions", None),
        ("GET", "/api/v1/unassigned", None),
        ("GET", "/api/v1/catalogs/main/tree", None),
        ("GET", "/api/v1/catalogs/main/export", None),
        ("GET", "/api/v1/search?q=alpha", None),
        ("GET", "/api/v1/whoami", None),
        ("HEAD", "/api/v1/users?email=alice@example.com", None),
        ("GET", "/api/v1/catalogs", None),
        ("POST", "/api/v1/catalogs/main/import", None),
        ("GET", "/api/v1/labels", None),
        ("PATCH", "/api/v1/me", Some(json!({"display_name": "x"}))),
        (
            "POST",
            "/api/v1/changesets",
            Some(json!({"catalog": "main"})),
        ),
        ("GET", "/catalogs/main/export", None),
    ];
    for (method, uri, body) in refused {
        let (status, answer) = send(&test, method, uri, p, body).await;
        assert_eq!(status.as_u16(), 403, "{method} {uri}: {answer}");
    }
    // Not an MCP credential either.
    let (mcp, _) = send(
        &test,
        "POST",
        "/mcp",
        p,
        Some(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"})),
    )
    .await;
    assert_eq!(mcp.as_u16(), 403);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provisioners_user_detail_never_lists_credentials_even_with_no_grants() {
    let test = app().await;
    let key = provisioner(&test).await;
    let (status, body) = send(
        &test,
        "GET",
        &format!("/api/v1/users/{}", crate::db::TEST_USER),
        Some(&key),
        None,
    )
    .await;
    assert_eq!(status.as_u16(), 200);
    assert_eq!(body.get("keys"), Some(&json!([])));
    assert_eq!(body.get("codes"), Some(&json!([])));
    let (status, body) = send(
        &test,
        "GET",
        "/api/v1/users?email=nobody@example.com",
        Some(&key),
        None,
    )
    .await;
    assert_eq!(status.as_u16(), 200);
    assert_eq!(body["users"], json!([]));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provisioners_detail_waits_for_a_live_admin_promotion() {
    use crate::store::{
        audit::AuditActor,
        users::{self, NewUser},
    };
    use std::{sync::Arc, time::Duration};
    let test = Arc::new(app().await);
    let key = provisioner(&test).await;
    users::create(
        test.app.accounts().executor(),
        &NewUser {
            name: "staff",
            display_name: &crate::labels::DisplayName::default(),
        },
        &AuditActor::cli(),
        chrono::Utc::now(),
    )
    .await
    .unwrap();
    let mut tx = test.app.accounts().executor().begin().await.unwrap();
    sqlx::query("SELECT id FROM users WHERE name = 'staff' FOR UPDATE")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET is_admin = true WHERE name = 'staff'")
        .execute(&mut *tx)
        .await
        .unwrap();
    let other = Arc::clone(&test);
    let request =
        tokio::spawn(
            async move { send(&other, "GET", "/api/v1/users/staff", Some(&key), None).await },
        );
    crate::http::api::testing::wait_for_lock_waiters(test.app.accounts().executor(), 1).await;
    tx.commit().await.unwrap();
    let (status, body) = tokio::time::timeout(Duration::from_secs(10), request)
        .await
        .expect("detail did not finish after promotion")
        .unwrap();
    assert_eq!(status.as_u16(), 403, "{body}");
}

#[test]
fn the_allow_list_is_the_specs_and_names_routes_by_template() {
    let expected = [
        ("GET", "/api/v1/users"),
        ("GET", "/api/v1/users/{name}"),
        ("POST", "/api/v1/users"),
        ("PUT", "/api/v1/users/{name}/email"),
        ("POST", "/api/v1/users/{name}/invite"),
        ("PATCH", "/api/v1/users/{name}"),
        ("DELETE", "/api/v1/users/{name}"),
        ("GET", "/api/v1/teams"),
        ("GET", "/api/v1/teams/{name}"),
        ("POST", "/api/v1/teams"),
        ("PUT", "/api/v1/teams/{name}/members/{user}"),
        ("DELETE", "/api/v1/teams/{name}/members/{user}"),
        ("PATCH", "/api/v1/teams/{name}"),
        ("DELETE", "/api/v1/teams/{name}"),
        ("PUT", "/api/v1/teams/{name}/suspension"),
        ("POST", "/api/v1/catalogs"),
        ("PATCH", "/api/v1/catalogs/{catalog}"),
        ("DELETE", "/api/v1/catalogs/{catalog}"),
        ("PUT", "/api/v1/catalogs/{catalog}/member-access"),
        ("GET", "/api/v1/audit"),
    ];
    assert_eq!(PROVISIONER_ROUTES, expected);
    for (method, path) in expected {
        assert!(provisioner_may(&method.parse().unwrap(), path));
    }
    assert!(provisioner_may(
        &axum::http::Method::DELETE,
        "/api/v1/users/{name}"
    ));
    assert!(!provisioner_may(
        &axum::http::Method::PUT,
        "/api/v1/users/{name}/admin"
    ));
    // The team list is a read only: no other method on it (issue #502).
    for method in [
        axum::http::Method::HEAD,
        axum::http::Method::PUT,
        axum::http::Method::PATCH,
        axum::http::Method::DELETE,
    ] {
        assert!(!provisioner_may(&method, "/api/v1/teams"), "{method}");
    }
    assert!(!provisioner_may(
        &axum::http::Method::GET,
        "/api/v1/unassigned"
    ));
    // The three display-name edits, and nothing else new: not `/me`'s edit
    // and not the labels read.
    assert!(!provisioner_may(&axum::http::Method::PATCH, "/api/v1/me"));
    assert!(!provisioner_may(&axum::http::Method::GET, "/api/v1/labels"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provisioner_lists_every_team_as_an_admin_sees_them() {
    let test = app().await;
    let root = every_catalog_admin(&test).await;
    let key = provisioner(&test).await;
    // A shared team, a suspended one, and an admin user's personal team:
    // the list leaves none of them out for a provisioner.
    for (uri, body) in [
        ("/api/v1/users", json!({"name": "alice"})),
        ("/api/v1/users", json!({"name": "staff"})),
        ("/api/v1/teams", json!({"name": "acme", "owner": "alice"})),
        (
            "/api/v1/teams",
            json!({"name": "dormant", "owner": "alice"}),
        ),
    ] {
        let (status, answer) = send(&test, "POST", uri, Some(&root), Some(body)).await;
        assert_eq!(status.as_u16(), 201, "POST {uri}: {answer}");
    }
    for (uri, body) in [
        ("/api/v1/users/staff/admin", json!({"admin": true})),
        (
            "/api/v1/teams/dormant/suspension",
            json!({"suspended": true}),
        ),
    ] {
        let (status, answer) = send(&test, "PUT", uri, Some(&root), Some(body)).await;
        assert_eq!(status.as_u16(), 200, "PUT {uri}: {answer}");
    }

    let (status, listed) = send(&test, "GET", "/api/v1/teams", Some(&key), None).await;
    let (_, as_admin) = send(&test, "GET", "/api/v1/teams", Some(&root), None).await;

    assert_eq!(status.as_u16(), 200, "{listed}");
    assert_eq!(listed, as_admin, "the provisioner sees the admin's list");
    let names: Vec<&str> = listed["teams"]
        .as_array()
        .expect("a team array")
        .iter()
        .filter_map(|team| team["name"].as_str())
        .collect();
    for name in ["acme", "alice", "dormant", "staff", crate::db::TEST_USER] {
        assert!(names.contains(&name), "{name} missing from {names:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn listing_teams_widens_a_provisioner_to_nothing_else() {
    let test = app().await;
    let key = provisioner(&test).await;
    let p = Some(key.as_str());
    let (listed, _) = send(&test, "GET", "/api/v1/teams", p, None).await;
    assert_eq!(listed.as_u16(), 200);
    // The neighbouring reads it could not make before stay refused.
    for uri in [
        "/api/v1/users",
        "/api/v1/unassigned",
        "/api/v1/catalogs",
        "/api/v1/keys",
        "/api/v1/audit/export",
    ] {
        let (status, answer) = send(&test, "GET", uri, p, None).await;
        assert_eq!(status.as_u16(), 403, "GET {uri}: {answer}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provisioner_deletes_a_user_unless_a_shared_team_member() {
    let test = app().await;
    let root = every_catalog_admin(&test).await;
    let key = provisioner(&test).await;
    let p = Some(key.as_str());
    for (uri, body) in [
        ("/api/v1/users", json!({"name": "alice"})),
        ("/api/v1/users", json!({"name": "bob"})),
        ("/api/v1/teams", json!({"name": "acme", "owner": "alice"})),
    ] {
        let (status, answer) = send(&test, "POST", uri, Some(&root), Some(body)).await;
        assert_eq!(status.as_u16() / 100, 2, "{uri}: {answer}");
    }
    let (_, _) = send(
        &test,
        "PUT",
        "/api/v1/teams/acme/members/bob",
        Some(&root),
        Some(json!({"role": "member"})),
    )
    .await;

    let (refused, error) = send(
        &test,
        "DELETE",
        "/api/v1/users/bob?unless_shared_member=true",
        p,
        None,
    )
    .await;
    let (still, _) = send(&test, "GET", "/api/v1/users/bob", Some(&root), None).await;
    let (plain, _) = send(&test, "DELETE", "/api/v1/users/bob", p, None).await;

    assert_eq!(refused.as_u16(), 409, "{error}");
    assert!(error["message"]
        .as_str()
        .is_some_and(|m| m.starts_with(crate::store::users::SHARED_MEMBER_REFUSAL)));
    assert_eq!(still.as_u16(), 200);
    assert_eq!(plain.as_u16(), 204, "without the parameter nothing changes");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provisioner_sets_display_names_but_never_an_admins() {
    let test = app().await;
    let root = every_catalog_admin(&test).await;
    let key = provisioner(&test).await;
    let p = Some(key.as_str());
    let name = |text: &str| Some(json!({"display_name": text}));
    for (uri, body) in [
        ("/api/v1/users", json!({"name": "alice"})),
        ("/api/v1/users", json!({"name": "staff"})),
        ("/api/v1/teams", json!({"name": "acme", "owner": "alice"})),
    ] {
        let (status, _) = send(&test, "POST", uri, Some(&root), Some(body)).await;
        assert_eq!(status.as_u16(), 201, "{uri}");
    }
    send(
        &test,
        "PUT",
        "/api/v1/users/staff/admin",
        Some(&root),
        Some(json!({"admin": true})),
    )
    .await;

    let (user, user_view) = send(&test, "PATCH", "/api/v1/users/alice", p, name("Alice A")).await;
    let (admin_user, _) = send(&test, "PATCH", "/api/v1/users/staff", p, name("Staff S")).await;
    let (team, team_view) = send(&test, "PATCH", "/api/v1/teams/acme", p, name("Acme A")).await;
    let (catalog, catalog_view) = send(
        &test,
        "PATCH",
        "/api/v1/catalogs/main",
        p,
        Some(json!({"display_name": "Main", "description": "d"})),
    )
    .await;
    let (_, staff) = send(&test, "GET", "/api/v1/users/staff", Some(&root), None).await;

    assert_eq!(user.as_u16(), 200, "{user_view}");
    assert_eq!(user_view["label"], "Alice A");
    assert_eq!(
        admin_user.as_u16(),
        403,
        "a provisioner never reaches an admin user"
    );
    assert_eq!(
        staff["user"]["display_name"], "",
        "the refused write changed nothing"
    );
    assert_eq!(team.as_u16(), 200, "{team_view}");
    assert_eq!(team_view["label"], "Acme A");
    assert_eq!(catalog.as_u16(), 200, "{catalog_view}");
    assert_eq!(catalog_view["label"], "Main");
}

/// Every audit entry's row, and every counter bucket, as one text, to search
/// for what must never be stored.
async fn everything_stored(test: &crate::mcp::testing::TestApp) -> String {
    test.app.settle_access().await;
    sqlx::query_scalar(
        "SELECT COALESCE((SELECT string_agg(to_jsonb(e)::text, ' ') FROM audit_entries e), '') \
         || ' ' || COALESCE((SELECT string_agg(bucket, ' ') FROM audit_access_counts), '')",
    )
    .fetch_one(test.app.accounts().executor())
    .await
    .expect("read what the audit log stores")
}

async fn entry_count(test: &crate::mcp::testing::TestApp) -> i64 {
    test.app.settle_access().await;
    sqlx::query_scalar("SELECT count(*) FROM audit_entries")
        .fetch_one(test.app.accounts().executor())
        .await
        .expect("count the entries")
}

#[tokio::test(flavor = "multi_thread")]
async fn an_email_lookup_is_audited_with_the_outcome_and_never_the_address() {
    let test = app().await;
    let key = provisioner(&test).await;
    let p = Some(key.as_str());
    let root = every_catalog_admin(&test).await;
    send(
        &test,
        "POST",
        "/api/v1/users",
        p,
        Some(json!({"name": "alice"})),
    )
    .await;
    send(
        &test,
        "PUT",
        "/api/v1/users/alice/email",
        p,
        Some(json!({"email": "alice@example.com"})),
    )
    .await;

    let (found, _) = send(
        &test,
        "GET",
        "/api/v1/users?email=Alice@Example.com",
        p,
        None,
    )
    .await;
    let (missing, _) = send(
        &test,
        "GET",
        "/api/v1/users?email=nobody@example.com",
        p,
        None,
    )
    .await;

    assert_eq!((found.as_u16(), missing.as_u16()), (200, 200));
    test.app.settle_access().await;
    let stored = everything_stored(&test).await.to_lowercase();
    assert!(!stored.contains("alice@example.com"), "{stored}");
    assert!(!stored.contains("nobody"), "{stored}");
    assert!(!stored.contains("example.com"), "{stored}");
    let mut entries =
        crate::store::audit::testing::entries(test.app.accounts().executor(), "user.looked_up")
            .await;
    assert_eq!(entries.len(), 2);
    // The writes are background tasks, so their ids follow no request
    // order: find each entry by its outcome (`found` sorts before `not_found`).
    entries.sort_by_key(|entry| entry.details["outcome"].to_string());
    assert_eq!(entries[0].details["outcome"], "found");
    assert_eq!(entries[0].person.as_deref(), Some("alice"));
    assert_eq!(entries[0].target_kind, "user");
    assert_eq!(entries[1].details["outcome"], "not_found");
    assert_eq!(entries[1].target.as_deref(), Some("GET /api/v1/users"));
    for entry in &entries {
        assert!(entry.actor_operator);
        assert_eq!(entry.source, "api");
        assert_eq!(entry.actor_label, "api key");
        assert!(entry.teams.is_empty(), "no team's owners see it");
        assert_eq!(entry.requests, Some(1));
    }
    // The actor is the provisioner key itself.
    let credential: String = sqlx::query_scalar(
        "SELECT c.credential_id FROM audit_entries e \
         JOIN audit_credentials c ON c.id = e.actor_credential_ref \
         WHERE e.action = 'user.looked_up' LIMIT 1",
    )
    .fetch_one(test.app.accounts().executor())
    .await
    .unwrap();
    assert_eq!(
        credential,
        crate::mcp::testing::cached_principal(&test, &key)
            .await
            .key_id
    );

    // An admin reads them; so does the key, which sees only its own.
    for reader in [&root, &key] {
        let (status, page) = send(
            &test,
            "GET",
            "/api/v1/audit?action=user.looked_up",
            Some(reader),
            None,
        )
        .await;
        assert_eq!(status.as_u16(), 200, "{page}");
        assert_eq!(page["entries"].as_array().map(Vec::len), Some(2), "{page}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn looking_up_an_admin_by_email_is_recorded_as_not_found() {
    let test = app().await;
    let root = every_catalog_admin(&test).await;
    let key = provisioner(&test).await;
    send(
        &test,
        "POST",
        "/api/v1/users",
        Some(&root),
        Some(json!({"name": "staff"})),
    )
    .await;
    send(
        &test,
        "PUT",
        "/api/v1/users/staff/email",
        Some(&root),
        Some(json!({"email": "staff@example.com"})),
    )
    .await;
    send(
        &test,
        "PUT",
        "/api/v1/users/staff/admin",
        Some(&root),
        Some(json!({"admin": true})),
    )
    .await;

    send(
        &test,
        "GET",
        "/api/v1/users?email=staff@example.com",
        Some(&key),
        None,
    )
    .await;

    test.app.settle_access().await;
    let entries =
        crate::store::audit::testing::entries(test.app.accounts().executor(), "user.looked_up")
            .await;
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].details["outcome"], "not_found",
        "what the key was shown"
    );
    assert_eq!(entries[0].person, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_route_is_audited_by_template_method_and_a_stable_code() {
    let test = app().await;
    let key = provisioner(&test).await;
    let p = Some(key.as_str());

    let (status, _) = send(
        &test,
        "GET",
        "/api/v1/catalogs/main/tree?secret=hunter2",
        p,
        None,
    )
    .await;
    let (users, _) = send(&test, "GET", "/api/v1/users", p, None).await;

    assert_eq!((status.as_u16(), users.as_u16()), (403, 403));
    test.app.settle_access().await;
    let stored = everything_stored(&test).await;
    assert!(!stored.contains("hunter2"), "{stored}");
    assert!(
        !stored.contains("/catalogs/main"),
        "no literal path: {stored}"
    );
    let mut entries = crate::store::audit::testing::entries(
        test.app.accounts().executor(),
        "provisioner.refused",
    )
    .await;
    assert_eq!(entries.len(), 2);
    // Background writes: find each entry by its reason, not by id order
    // (`forbidden` sorts before `route_not_allowed`).
    entries.sort_by_key(|entry| entry.details["reason"].to_string());
    // A route on the list that its handler refuses (a listing with no email).
    assert_eq!(entries[0].details["route"], "/api/v1/users");
    assert_eq!(entries[0].details["reason"], "forbidden");
    assert_eq!(
        entries[1].details,
        json!({
            "method": "GET",
            "route": "/api/v1/catalogs/{catalog}/tree",
            "reason": "route_not_allowed",
            "hour": entries[1].details["hour"],
        })
    );
    assert_eq!(
        entries[1].target.as_deref(),
        Some("GET /api/v1/catalogs/{catalog}/tree")
    );
    assert!(entries
        .iter()
        .all(|entry| entry.actor_operator && entry.teams.is_empty()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refusal_on_mcp_or_an_archive_route_is_audited_like_an_api_one() {
    let test = app().await;
    let key = provisioner(&test).await;
    let p = Some(key.as_str());

    let (export, _) = send(
        &test,
        "GET",
        "/catalogs/main/export?secret=hunter2",
        p,
        None,
    )
    .await;
    let (import, _) = send(&test, "POST", "/catalogs/main/import", p, None).await;
    let (mcp, _) = send(
        &test,
        "POST",
        "/mcp",
        p,
        Some(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"})),
    )
    .await;

    assert_eq!(
        (export.as_u16(), import.as_u16(), mcp.as_u16()),
        (403, 403, 403)
    );
    let stored = everything_stored(&test).await;
    assert!(!stored.contains("hunter2"), "{stored}");
    assert!(
        !stored.contains("/catalogs/main"),
        "no literal path: {stored}"
    );
    let mut entries = crate::store::audit::testing::entries(
        test.app.accounts().executor(),
        "provisioner.refused",
    )
    .await;
    // Background writes: find each entry by its target, not by id order.
    entries.sort_by_key(|entry| entry.target.clone());
    let targets: Vec<_> = entries
        .iter()
        .map(|entry| entry.target.as_deref())
        .collect();
    assert_eq!(
        targets,
        [
            Some("GET /catalogs/{name}/export"),
            Some("POST /catalogs/{name}/import"),
            Some("POST /mcp"),
        ]
    );
    assert!(entries.iter().all(|entry| {
        entry.details["reason"] == "route_not_allowed"
            && entry.requests == Some(1)
            && entry.actor_operator
            && entry.teams.is_empty()
    }));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_burst_of_identical_refusals_collapses_into_one_counted_entry() {
    let test = app().await;
    let key = provisioner(&test).await;
    let before = entry_count(&test).await;

    for _ in 0..25 {
        let (status, _) = send(&test, "GET", "/api/v1/keys", Some(&key), None).await;
        assert_eq!(status.as_u16(), 403);
    }

    assert_eq!(entry_count(&test).await, before + 1, "one refusal entry");
    let entries = crate::store::audit::testing::entries(
        test.app.accounts().executor(),
        "provisioner.refused",
    )
    .await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].requests, Some(25));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_successful_change_still_writes_exactly_one_event_and_other_reads_none() {
    let test = app().await;
    let key = provisioner(&test).await;
    let p = Some(key.as_str());
    let before = entry_count(&test).await;

    let (created, _) = send(
        &test,
        "POST",
        "/api/v1/users",
        p,
        Some(json!({"name": "alice"})),
    )
    .await;
    assert_eq!(created.as_u16(), 201);
    assert_eq!(
        entry_count(&test).await,
        before + 1,
        "one state change, one entry"
    );
    let (team, _) = send(
        &test,
        "POST",
        "/api/v1/teams",
        p,
        Some(json!({"name": "acme", "owner": "alice"})),
    )
    .await;
    assert_eq!(team.as_u16(), 201);
    let after_changes = entry_count(&test).await;

    for uri in ["/api/v1/users/alice", "/api/v1/teams/acme", "/api/v1/teams"] {
        let (status, answer) = send(&test, "GET", uri, p, None).await;
        assert_eq!(status.as_u16(), 200, "{uri}: {answer}");
    }
    // A change that changes nothing: the same display name again.
    send(
        &test,
        "PATCH",
        "/api/v1/users/alice",
        p,
        Some(json!({"display_name": ""})),
    )
    .await;

    assert_eq!(
        entry_count(&test).await,
        after_changes,
        "reads and no-ops write no entry"
    );
    let looked_up =
        crate::store::audit::testing::entries(test.app.accounts().executor(), "user.looked_up")
            .await;
    assert!(looked_up.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_owner_of_the_users_team_never_sees_provisioner_lookups_or_refusals() {
    let test = app().await;
    let key = provisioner(&test).await;
    send(
        &test,
        "GET",
        "/api/v1/users?email=nobody@example.com",
        Some(&key),
        None,
    )
    .await;
    send(&test, "GET", "/api/v1/keys", Some(&key), None).await;
    test.app.settle_access().await;
    let (team_id, audit_team): (String, i64) =
        sqlx::query_as("SELECT t.id, audit_team_ref(t.id) FROM teams t WHERE t.name = $1")
            .bind(crate::db::TEST_TEAM)
            .fetch_one(test.app.accounts().executor())
            .await
            .unwrap();
    let viewer = crate::store::audit::Viewer::Owner {
        team_id,
        audit_team,
    };

    let seen = crate::store::audit::list(
        test.app.stores(),
        &viewer,
        &crate::store::audit::AuditQuery::default(),
        test.app.labels(),
    )
    .await
    .unwrap();

    assert!(
        seen.iter()
            .all(|entry| entry.action != "user.looked_up" && entry.action != "provisioner.refused"),
        "{seen:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_flood_past_the_pending_write_limit_loses_the_overflow_and_recovers() {
    use crate::app::MAX_PENDING_PROVISIONER_WRITES;
    use crate::store::audit::{
        AdmittedActor, ProvisionerEvent, ProvisionerUse, RefusalReason, Source,
    };
    let test = app().await;
    let at = chrono::Utc::now();
    let usage = || ProvisionerUse {
        actor: AdmittedActor {
            source: Source::Api,
            credential: None,
            person: None,
            label: "api key".to_string(),
            operator: true,
            ip: None,
            at,
        },
        event: ProvisionerEvent::Refused {
            method: "GET",
            route: "/api/v1/keys".to_string(),
            reason: RefusalReason::RouteNotAllowed,
        },
    };
    let requests = |test: &crate::mcp::testing::TestApp| {
        let pool = test.app.accounts().executor().clone();
        async move {
            sqlx::query_scalar::<_, Option<i64>>(
                "SELECT sum(c.requests)::bigint FROM audit_access_counts c \
                 JOIN audit_entries e ON e.id = c.entry_id \
                 WHERE e.action = 'provisioner.refused'",
            )
            .fetch_one(&pool)
            .await
            .unwrap()
            .unwrap_or(0)
        }
    };
    let limit = i64::try_from(MAX_PENDING_PROVISIONER_WRITES).unwrap();

    // Every write is held, so every permit stays taken until the hold goes.
    let hold = std::sync::Arc::clone(&test.app.access_hold)
        .lock_owned()
        .await;
    for _ in 0..MAX_PENDING_PROVISIONER_WRITES + 1 {
        test.app.note_provisioner_use(usage());
    }
    drop(hold);
    tokio::time::timeout(std::time::Duration::from_secs(30), test.app.settle_access())
        .await
        .expect("the held writes finish");
    assert_eq!(
        requests(&test).await,
        limit,
        "the request past the limit was dropped"
    );

    // Its permit is back, so a later use is recorded.
    test.app.note_provisioner_use(usage());
    tokio::time::timeout(std::time::Duration::from_secs(30), test.app.settle_access())
        .await
        .expect("the write finishes");
    assert_eq!(requests(&test).await, limit + 1, "capacity came back");
}

/// Issue #980: a provisioner may create a user with its email and delete it
/// only while it has none; a taken address answers alike whoever holds it,
/// an admin user (whom the provisioner otherwise cannot see) included.
#[tokio::test(flavor = "multi_thread")]
async fn a_provisioner_creates_with_an_email_and_cleans_up_only_an_email_less_user() {
    let test = app().await;
    let root = every_catalog_admin(&test).await;
    let key = provisioner(&test).await;
    let p = Some(key.as_str());
    for (name, address, admin) in [
        ("staff", "staff@example.com", true),
        ("member", "member@example.com", false),
    ] {
        send(
            &test,
            "POST",
            "/api/v1/users",
            Some(&root),
            Some(json!({"name": name})),
        )
        .await;
        send(
            &test,
            "PUT",
            &format!("/api/v1/users/{name}/email"),
            Some(&root),
            Some(json!({"email": address})),
        )
        .await;
        if admin {
            send(
                &test,
                "PUT",
                &format!("/api/v1/users/{name}/admin"),
                Some(&root),
                Some(json!({"admin": true})),
            )
            .await;
        }
    }

    let (created, made) = send(
        &test,
        "POST",
        "/api/v1/users",
        p,
        Some(json!({"name": "alice", "email": "alice@example.com"})),
    )
    .await;
    let (by_admin, admin_answer) = send(
        &test,
        "POST",
        "/api/v1/users",
        p,
        Some(json!({"name": "bob", "email": "staff@example.com"})),
    )
    .await;
    let (by_member, member_answer) = send(
        &test,
        "POST",
        "/api/v1/users",
        p,
        Some(json!({"name": "carol", "email": "member@example.com"})),
    )
    .await;
    let (set, set_answer) = send(
        &test,
        "PUT",
        "/api/v1/users/alice/email",
        p,
        Some(json!({"email": "staff@example.com"})),
    )
    .await;
    let (lookup, found) = send(
        &test,
        "GET",
        "/api/v1/users?email=staff@example.com",
        p,
        None,
    )
    .await;
    let (kept, kept_answer) = send(
        &test,
        "DELETE",
        "/api/v1/users/alice?unless_has_email=true",
        p,
        None,
    )
    .await;
    send(
        &test,
        "POST",
        "/api/v1/users",
        p,
        Some(json!({"name": "dave"})),
    )
    .await;
    let (cleaned, _) = send(
        &test,
        "DELETE",
        "/api/v1/users/dave?unless_has_email=true",
        p,
        None,
    )
    .await;
    let (admin_target, _) = send(
        &test,
        "DELETE",
        "/api/v1/users/staff?unless_has_email=true",
        p,
        None,
    )
    .await;

    assert_eq!(created.as_u16(), 201, "{made}");
    assert_eq!(by_admin.as_u16(), 409);
    assert_eq!(by_member.as_u16(), 409);
    assert_eq!(
        admin_answer, member_answer,
        "an admin's address is not told apart"
    );
    assert_eq!(set.as_u16(), 409);
    assert_eq!(
        set_answer, admin_answer,
        "create and set-email answer alike"
    );
    assert_eq!(lookup.as_u16(), 200);
    assert!(found["users"].as_array().unwrap().is_empty(), "{found}");
    assert_eq!(kept.as_u16(), 409, "{kept_answer}");
    assert_eq!(
        kept_answer["message"],
        crate::store::users::HAS_EMAIL_REFUSAL
    );
    assert_eq!(cleaned.as_u16(), 204);
    assert_eq!(
        admin_target.as_u16(),
        403,
        "an admin user stays out of reach"
    );
    let (gone, _) = send(&test, "GET", "/api/v1/users/bob", p, None).await;
    assert_eq!(gone.as_u16(), 404, "a refused create made nothing");
}
