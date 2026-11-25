//! The readable navigation listings (issue #955): `GET /catalogs?readable=true`
//! and `GET /whoami?readable=true` leave out a catalog an admin reaches but
//! may not read (`require_unlock` on, no live unlock), while the plain
//! listings still name it so it can be found and unlocked.

use axum::http::StatusCode;
use chrono::Utc;
use serde_json::Value;

use crate::cache::directory::cursor_of;
use crate::http::api::access_tests::{create_unlock_at, make_staff_admin, unlock};
use crate::http::api::testing::{app, keys, send};
use crate::invalidate::testing::applied;
use crate::mcp::testing::{require_unlock, staff_root, TestApp};
use crate::store::audit::AuditActor;
use crate::store::catalogs::{self, NewCatalog};
use crate::store::teams::{self, NewTeam};

/// An app with `main` and `other` (the test user's team's) and `mine`, a
/// catalog of team `staffers`, which admin user `staff` owns; `require_unlock`
/// is on.
///
/// # Returns
/// The app and the secret of `staff`'s every-catalog admin key.
async fn world() -> (TestApp, String) {
    let test = app().await;
    let staff = staff_root(&test).await;
    make_staff_admin(&test).await;
    let pool = test.app.accounts().executor();
    let cli = AuditActor::cli();
    let team = NewTeam {
        name: "staffers",
        display_name: &crate::labels::DisplayName::default(),
        owner: "staff",
    };
    teams::create(pool, &team, &cli, Utc::now()).await.unwrap();
    applied(&test.app, async {
        let mine = NewCatalog {
            name: "mine",
            display_name: &crate::labels::DisplayName::default(),
            description: "",
            owner_team: "staffers",
        };
        catalogs::create(pool, &mine, &cli, Utc::now())
            .await
            .unwrap();
    })
    .await;
    require_unlock(&test).await;
    (test, staff)
}

/// The catalog names of a `/catalogs` answer, or of a `/whoami` one.
fn listed(body: &Value) -> Vec<String> {
    body["catalogs"]
        .as_array()
        .expect("a list")
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .or_else(|| entry["name"].as_str())
                .expect("a name")
                .to_owned()
        })
        .collect()
}

/// `GET path` as `key`, which must succeed.
async fn get(test: &TestApp, path: &str, key: &str) -> Value {
    let (status, body) = send(test, "GET", path, Some(key), None).await;
    assert_eq!(status, StatusCode::OK, "{path}: {body}");
    body
}

#[tokio::test(flavor = "multi_thread")]
async fn a_locked_catalog_is_left_out_of_the_readable_lists_but_not_the_plain_ones() {
    let (test, staff) = world().await;

    let plain = get(&test, "/api/v1/catalogs", &staff).await;
    let readable = get(&test, "/api/v1/catalogs?readable=true", &staff).await;
    let me_plain = get(&test, "/api/v1/whoami", &staff).await;
    let me_readable = get(&test, "/api/v1/whoami?readable=true", &staff).await;

    // The administrative listing still finds every catalog, to unlock it.
    assert_eq!(listed(&plain), ["main", "mine", "other"]);
    assert_eq!(listed(&me_plain), ["main", "mine", "other"]);
    // The navigation lists keep only what `staff` may read: its team's.
    assert_eq!(listed(&readable), ["mine"]);
    assert_eq!(listed(&me_readable), ["mine"]);
    assert_eq!(
        me_readable["levels"].as_object().unwrap().keys().count(),
        1,
        "levels follow the list: {me_readable}"
    );
    // The administrative facts do not come from the reduced list.
    for field in ["access", "all_catalogs", "user_admin", "user", "key_id"] {
        assert_eq!(me_readable[field], me_plain[field], "{field}");
    }
    assert_eq!(me_readable["all_catalogs"], true);
    // Hiding is not the boundary: the contents are still refused.
    let (status, body) = send(
        &test,
        "GET",
        "/api/v1/catalogs/main/tree",
        Some(&staff),
        None,
    )
    .await;
    assert_ne!(status, StatusCode::OK, "{body}");
    assert_eq!(body["error"], "UnlockRequired", "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unlock_makes_a_catalog_readable_and_ending_or_expiring_it_hides_it_again() {
    let (test, staff) = world().await;
    let readable = async || {
        let body = get(&test, "/api/v1/catalogs?readable=true", &staff).await;
        let me = get(&test, "/api/v1/whoami?readable=true", &staff).await;
        assert_eq!(listed(&body), listed(&me));
        listed(&body)
    };
    assert_eq!(readable().await, ["mine"]);

    let main = unlock(&test, "main").await;
    let other = crate::invalidate::testing::applied(
        &test.app,
        create_unlock_at(&test, "other", Utc::now() - chrono::TimeDelta::hours(2)),
    )
    .await;

    assert_eq!(readable().await, ["main", "mine"], "other's unlock expired");
    let (status, _) = send(
        &test,
        "GET",
        "/api/v1/catalogs/main/tree",
        Some(&staff),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the unlock reads");
    let (status, _) = send(
        &test,
        "GET",
        "/api/v1/catalogs/other/tree",
        Some(&staff),
        None,
    )
    .await;
    assert_ne!(status, StatusCode::OK, "an expired unlock does not read");

    applied(&test.app, async {
        crate::store::unlocks::end(
            test.app.accounts().executor(),
            main,
            &AuditActor::cli(),
            Utc::now(),
        )
        .await
        .unwrap();
    })
    .await;

    assert_eq!(readable().await, ["mine"], "ended");
    assert_ne!(other, main);
}

#[tokio::test(flavor = "multi_thread")]
async fn without_require_unlock_the_readable_lists_are_the_plain_ones() {
    let test = app().await;
    let staff = staff_root(&test).await;
    make_staff_admin(&test).await;

    for path in ["/api/v1/catalogs", "/api/v1/whoami"] {
        let plain = get(&test, path, &staff).await;
        let readable = get(&test, &format!("{path}?readable=true"), &staff).await;

        assert_eq!(listed(&readable), listed(&plain), "{path}");
        assert_eq!(listed(&readable), ["main", "other"], "{path}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_readable_list_pages_through_the_catalogs_left_out() {
    let (test, staff) = world().await;
    unlock(&test, "other").await;

    // Readable: mine, other. `main` sits before both and is skipped.
    let first = get(&test, "/api/v1/catalogs?readable=true&limit=1", &staff).await;
    let cursor = first["next"].as_str().expect("a cursor").to_owned();
    let second = get(
        &test,
        &format!("/api/v1/catalogs?readable=true&limit=1&after={cursor}"),
        &staff,
    )
    .await;
    // A cursor at a locked catalog continues with the next readable one.
    let from_locked = get(
        &test,
        &format!(
            "/api/v1/catalogs?readable=true&limit=1&after={}",
            cursor_of("main")
        ),
        &staff,
    )
    .await;

    assert_eq!(listed(&first), ["mine"]);
    assert_eq!(listed(&second), ["other"]);
    assert_eq!(second.get("next"), None, "{second}");
    assert_eq!(listed(&from_locked), ["mine"]);
    assert!(from_locked["next"].is_string(), "{from_locked}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_scoped_key_sees_the_same_catalogs_either_way() {
    let test = app().await;
    let keys = keys(&test).await;
    require_unlock(&test).await;

    for (key, expected) in [(&keys.read, "main"), (&keys.stranger, "other")] {
        let plain = get(&test, "/api/v1/catalogs", key).await;
        let readable = get(&test, "/api/v1/catalogs?readable=true", key).await;

        assert_eq!(listed(&plain), [expected]);
        assert_eq!(listed(&readable), [expected], "no catalog is added");
    }
}
