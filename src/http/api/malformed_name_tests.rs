//! Route inventory (re-derived from every `.route(` in `src/http` with a
//! `{...}` segment): every one is sent a malformed name below except
//! `/ui/{*path}` (static assets, no lookup), `/auth/oidc/{provider}/...` and
//! `/me/identities/{provider}/link` (a configured provider id looked up in
//! memory, never in SQL), and `/unlocks/{id}` as a 400 (a numeric id axum
//! parses first).
//!
//! Every `/api/v1` route that takes a user, team, catalog or key name or id
//! from its path answers an unknown name with the usual not-found answer, never a
//! 500, whatever the name holds (issue #339). Postgres refuses a NUL in text,
//! so a NUL that reached a query once became a 500. The same routes are also
//! tried with a name that is merely too long, in capitals or `..`.

use serde_json::{json, Value};

use crate::http::api::testing::{every_catalog_admin, login_app, send, Browser, Payload};

/// One request template: `{}` in the URI is replaced by the name under test.
type Route = (&'static str, &'static str, Option<Value>);

/// The names every route is tried with. `%00` is a NUL byte once decoded.
fn bad_names() -> Vec<String> {
    vec![
        "a%00b".to_string(),
        "%00".to_string(),
        "UPPER".to_string(),
        "%2e%2e".to_string(),
        "a".repeat(5_000),
    ]
}

/// Routes whose path holds a user name.
fn user_routes() -> Vec<Route> {
    vec![
        ("GET", "/api/v1/users/{}", None),
        (
            "PATCH",
            "/api/v1/users/{}",
            Some(json!({"display_name": "Alice"})),
        ),
        ("DELETE", "/api/v1/users/{}", None),
        ("DELETE", "/api/v1/users/{}?unless_has_email=true", None),
        (
            "PUT",
            "/api/v1/users/{}/admin",
            Some(json!({"admin": true})),
        ),
        (
            "PUT",
            "/api/v1/users/{}/email",
            Some(json!({"email": "someone@example.com"})),
        ),
        ("POST", "/api/v1/users/{}/invite", None),
        (
            "PUT",
            "/api/v1/users/{}/entitlements/main",
            Some(json!({"access": "read"})),
        ),
        ("DELETE", "/api/v1/users/{}/entitlements/main", None),
        ("GET", "/api/v1/users/{}/sessions", None),
        ("DELETE", "/api/v1/users/{}/sessions", None),
        ("DELETE", "/api/v1/users/{}/sessions/abcdef012345", None),
        ("GET", "/api/v1/users/{}/sign-ins", None),
        (
            "PUT",
            "/api/v1/teams/{team}/members/{}",
            Some(json!({"role": "member"})),
        ),
        ("DELETE", "/api/v1/teams/{team}/members/{}", None),
    ]
}

/// Routes whose path holds a team name.
fn team_routes() -> Vec<Route> {
    vec![
        ("GET", "/api/v1/teams/{}", None),
        (
            "PATCH",
            "/api/v1/teams/{}",
            Some(json!({"display_name": "Family"})),
        ),
        ("DELETE", "/api/v1/teams/{}", None),
        (
            "PUT",
            "/api/v1/teams/{}/members/alice",
            Some(json!({"role": "member"})),
        ),
        ("DELETE", "/api/v1/teams/{}/members/alice", None),
        (
            "PUT",
            "/api/v1/teams/{}/suspension",
            Some(json!({"suspended": true})),
        ),
    ]
}

/// Routes whose path holds a catalog name.
fn catalog_routes() -> Vec<Route> {
    let routes = vec![
        (
            "PATCH",
            "/api/v1/catalogs/{}",
            Some(json!({"description": "x"})),
        ),
        ("DELETE", "/api/v1/catalogs/{}", None),
        (
            "PUT",
            "/api/v1/catalogs/{}/owner",
            Some(json!({"team": "family"})),
        ),
        (
            "PUT",
            "/api/v1/catalogs/{}/maintenance",
            Some(json!({"maintenance": "on"})),
        ),
        ("GET", "/api/v1/catalogs/{}/member-access", None),
        (
            "PUT",
            "/api/v1/catalogs/{}/member-access",
            Some(json!({"access": "read"})),
        ),
        ("GET", "/api/v1/catalogs/{}/export", None),
        ("GET", "/catalogs/{}/export", None),
        ("GET", "/api/v1/catalogs/{}/tree", None),
        ("GET", "/api/v1/catalogs/{}/concepts/index.md", None),
        ("GET", "/api/v1/catalogs/{}/links/index.md", None),
        ("GET", "/api/v1/catalogs/{}/history/index.md", None),
        ("GET", "/api/v1/catalogs/{}/history/1/index.md", None),
        ("GET", "/api/v1/catalogs/{}/documents/index.md", None),
        ("GET", "/api/v1/catalogs/{}/revisions/1/index.md", None),
        ("GET", "/api/v1/catalogs/main/maintenance/runs/{}", None),
        ("GET", "/api/v1/catalogs/main/maintenance/findings/{}", None),
        ("GET", "/api/v1/catalogs/main/reports/{}", None),
        ("GET", "/api/v1/catalogs/main/concepts/{}", None),
        ("DELETE", "/api/v1/users/alice/sessions/{}", None),
        ("GET", "/api/v1/catalogs/{}/recent-changes", None),
        ("GET", "/api/v1/catalogs/{}/health", None),
        ("GET", "/api/v1/catalogs/{}/reports/orphans", None),
        ("GET", "/api/v1/catalogs/{}/maintenance/runs", None),
        ("GET", "/api/v1/catalogs/{}/maintenance/runs/abc", None),
        ("GET", "/api/v1/catalogs/{}/maintenance/findings", None),
        ("GET", "/api/v1/catalogs/{}/maintenance/findings/abc", None),
        ("GET", "/api/v1/catalogs/{}/maintenance/counts", None),
        (
            "PUT",
            "/api/v1/users/alice/entitlements/{}",
            Some(json!({"access": "read"})),
        ),
        ("DELETE", "/api/v1/users/alice/entitlements/{}", None),
        ("PUT", "/api/v1/keys/{key}/catalogs/{}", None),
        ("DELETE", "/api/v1/keys/{key}/catalogs/{}", None),
        (
            "POST",
            "/api/v1/catalogs/main/maintenance/findings/{}/disposition",
            Some(json!({"disposition": "dismissed", "rationale": "x", "idempotency_key": "k"})),
        ),
        (
            "POST",
            "/api/v1/catalogs/{}/maintenance/findings/abc/disposition",
            Some(json!({"disposition": "dismissed", "rationale": "x", "idempotency_key": "k"})),
        ),
    ];
    routes
}

/// Routes whose path holds a key, grant, sign-in code, session or similar id.
fn id_routes() -> Vec<Route> {
    vec![
        (
            "PATCH",
            "/api/v1/keys/{}",
            Some(json!({"maintenance_report": true})),
        ),
        (
            "PUT",
            "/api/v1/keys/{}/user",
            Some(json!({"user": "alice"})),
        ),
        ("POST", "/api/v1/keys/{}/revoke", None),
        ("PUT", "/api/v1/keys/{}/catalogs/main", None),
        ("DELETE", "/api/v1/keys/{}/catalogs/main", None),
        ("DELETE", "/api/v1/oauth/codes/{}", None),
        (
            "PUT",
            "/api/v1/oauth/codes/{}/user",
            Some(json!({"user": "alice"})),
        ),
        ("DELETE", "/api/v1/changesets/{}", None),
        ("GET", "/api/v1/changesets/{}/diff", None),
        (
            "POST",
            "/api/v1/changesets/{}/publish",
            Some(json!({"message": "m"})),
        ),
        ("DELETE", "/api/v1/unlocks/{}", None),
        ("PUT", "/api/v1/settings/{}", Some(json!({"value": "1"}))),
        ("DELETE", "/api/v1/settings/{}", None),
        ("POST", "/api/v1/mail/queue/{}/retry", None),
        ("DELETE", "/api/v1/mail/queue/{}", None),
    ]
}

/// Routes under `/me`, which need a console session.
fn me_routes() -> Vec<Route> {
    vec![
        (
            "PATCH",
            "/api/v1/me/keys/{}",
            Some(json!({"name": "renamed"})),
        ),
        ("DELETE", "/api/v1/me/keys/{}", None),
        ("DELETE", "/api/v1/me/grants/{}", None),
        ("DELETE", "/api/v1/me/passkeys/{}", None),
        (
            "PATCH",
            "/api/v1/me/passkeys/{}",
            Some(json!({"name": "renamed"})),
        ),
        ("DELETE", "/api/v1/me/identities/{}", None),
        ("DELETE", "/api/v1/me/sessions/{}", None),
    ]
}

/// Body fields that name a user, team, catalog or key, each with a NUL.
fn body_routes() -> Vec<Route> {
    vec![
        ("POST", "/api/v1/users", Some(json!({"name": "a\u{0}b"}))),
        (
            "POST",
            "/api/v1/teams",
            Some(json!({"name": "a\u{0}b", "owner": "alice"})),
        ),
        (
            "POST",
            "/api/v1/teams",
            Some(json!({"name": "newteam", "owner": "a\u{0}b"})),
        ),
        (
            "POST",
            "/api/v1/catalogs",
            Some(json!({"name": "a\u{0}b", "owner": "family"})),
        ),
        (
            "POST",
            "/api/v1/catalogs",
            Some(json!({"name": "newcat", "owner": "a\u{0}b"})),
        ),
        (
            "PUT",
            "/api/v1/catalogs/main/owner",
            Some(json!({"team": "a\u{0}b"})),
        ),
        (
            "POST",
            "/api/v1/keys",
            Some(
                json!({"name": "a\u{0}b", "access": "read", "user": "alice", "catalogs": ["main"]}),
            ),
        ),
        (
            "POST",
            "/api/v1/keys",
            Some(json!({"name": "nk", "access": "read", "user": "a\u{0}b", "catalogs": ["main"]})),
        ),
        (
            "POST",
            "/api/v1/keys",
            Some(json!({"name": "nk", "access": "read", "user": "alice", "catalogs": ["a\u{0}b"]})),
        ),
        (
            "POST",
            "/api/v1/keys",
            Some(
                json!({"name": "nk", "access": "read", "user": "alice", "catalogs": ["main"], "default_catalog": "a\u{0}b"}),
            ),
        ),
        (
            "PUT",
            "/api/v1/keys/{key}/user",
            Some(json!({"user": "a\u{0}b"})),
        ),
        (
            "POST",
            "/api/v1/oauth/codes",
            Some(json!({"name": "a\u{0}b", "access": "read", "all": true, "user": "alice"})),
        ),
        (
            "POST",
            "/api/v1/oauth/codes",
            Some(json!({"name": "nc", "access": "read", "all": true, "user": "a\u{0}b"})),
        ),
        (
            "POST",
            "/api/v1/oauth/codes",
            Some(json!({"name": "nc", "access": "read", "catalogs": ["a\u{0}b"], "user": "alice"})),
        ),
        (
            "POST",
            "/api/v1/oauth/codes",
            Some(
                json!({"name": "nc", "access": "read", "catalogs": ["main"], "default_catalog": "a\u{0}b", "user": "alice"}),
            ),
        ),
        (
            "PUT",
            "/api/v1/oauth/codes/{key}/user",
            Some(json!({"user": "a\u{0}b"})),
        ),
        (
            "PUT",
            "/api/v1/users/alice/email",
            Some(json!({"email": "a\u{0}b@example.com"})),
        ),
        (
            "POST",
            "/api/v1/unlocks",
            Some(json!({"catalog": "a\u{0}b", "category": "support", "reason": "r"})),
        ),
        (
            "POST",
            "/api/v1/unlocks",
            Some(
                json!({"catalog": "main", "category": "support", "reason": "r", "user": "a\u{0}b"}),
            ),
        ),
        ("GET", "/api/v1/unlocks?catalog=a%00b", None),
        ("GET", "/api/v1/unlocks?user=a%00b", None),
        (
            "GET",
            "/api/v1/catalogs/main/maintenance/runs?after=a%00b",
            None,
        ),
        (
            "GET",
            "/api/v1/catalogs/main/maintenance/findings?after=a%00b",
            None,
        ),
        ("GET", "/api/v1/audit?team=a%00b", None),
        ("GET", "/api/v1/audit?actor=a%00b", None),
        ("GET", "/api/v1/audit?target=a%00b", None),
        ("GET", "/api/v1/audit/export?team=a%00b", None),
        ("GET", "/api/v1/users?email=a%00b", None),
        (
            "DELETE",
            "/api/v1/users/alice?unless_shared_member=a%00b",
            None,
        ),
    ]
}

/// Creates the user `alice`, her team `family`, and returns a key's id.
async fn fixtures(test: &crate::mcp::testing::TestApp, root: &str) -> String {
    let (status, body) = send(
        test,
        "POST",
        "/api/v1/users",
        Some(root),
        Some(json!({"name": "alice"})),
    )
    .await;
    assert_eq!(status.as_u16(), 201, "{body}");
    let (status, body) = send(
        test,
        "POST",
        "/api/v1/teams",
        Some(root),
        Some(json!({"name": "family", "owner": "alice"})),
    )
    .await;
    assert_eq!(status.as_u16(), 201, "{body}");
    let keys = crate::http::api::testing::keys(test).await;
    let (status, body) = send(test, "GET", "/api/v1/keys", Some(&keys.admin), None).await;
    assert_eq!(status.as_u16(), 200, "{body}");
    body["keys"][0]["id"]
        .as_str()
        .expect("a key id")
        .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn no_name_taking_route_answers_500_for_a_malformed_name() {
    // Arrange: `login_app` has `public_url`, so the invite route reaches
    // its name. Real users, teams and keys exist, so a route that checks
    // its first name reaches the second.
    let test = login_app().await;
    let root = every_catalog_admin(&test).await;
    let key = fixtures(&test, &root).await;
    let mut routes = user_routes();
    routes.extend(team_routes());
    routes.extend(catalog_routes());
    routes.extend(id_routes());
    routes.extend(body_routes());
    let mut browser = Browser::session_for(&test, "alice").await;
    let staff = crate::mcp::testing::staff_root(&test).await;

    // Act
    let mut failures = Vec::new();
    for (method, template, body) in routes.iter().chain(&me_routes()) {
        for name in bad_names() {
            let uri = template
                .replacen("{}", &name, 1)
                .replace("{key}", &key)
                .replace("{team}", "family");
            let (status, answer) = if template.contains("/me/") {
                let reply = browser
                    .send(
                        &test,
                        method,
                        &uri,
                        body.clone().map_or(Payload::Empty, Payload::Json),
                    )
                    .await;
                (
                    reply.status,
                    serde_json::from_slice(&reply.body).unwrap_or(Value::Null),
                )
            } else {
                // Only operator staff make unlocks.
                let key = if *template == "/api/v1/unlocks" {
                    &staff
                } else {
                    &root
                };
                send(&test, method, &uri, Some(key), body.clone()).await
            };
            // A name in the path names nothing: the route's usual 404, whatever
            // the name holds. The one convention that differs is an unlock's
            // id, a number, which axum refuses as a 400 before any handler.
            let want_404 = template.contains("{}") && !template.starts_with("/api/v1/unlocks/");
            if want_404 && status.as_u16() != 404 {
                failures.push(format!(
                    "{method} {template} with {name:.12}: want 404, got {status} {answer}"
                ));
            }
            if status.is_server_error() {
                let shown = uri.chars().take(90).collect::<String>();
                failures.push(format!("{method} {shown}: {status} {answer}"));
            }
        }
    }

    // The import routes read a body first, so give them a real archive.
    let tarball =
        crate::http::archives::tarball_of(&[("index.md", "# X\n"), ("log.md", "# Log\n")]);
    for template in ["/api/v1/catalogs/{}/import", "/catalogs/{}/import"] {
        for name in bad_names() {
            let uri = template.replacen("{}", &name, 1);
            let request = axum::http::Request::builder()
                .method("POST")
                .uri(&uri)
                .header("authorization", format!("Bearer {root}"))
                .body(axum::body::Body::from(tarball.clone()))
                .expect("a valid request");
            let response = tower::ServiceExt::oneshot(
                crate::http::router(
                    std::sync::Arc::clone(&test.app),
                    &tokio_util::sync::CancellationToken::new(),
                ),
                request,
            )
            .await
            .expect("the router answers");
            let status = response.status();
            if status.as_u16() != 404 {
                failures.push(format!(
                    "POST {template} with {name:.12}: want 404, got {status}"
                ));
            }
            if status.is_server_error() {
                failures.push(format!(
                    "POST {} : {status}",
                    uri.chars().take(90).collect::<String>()
                ));
            }
        }
    }

    // Assert
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
