//! The API keys page: create a key and see its secret once, sign in with
//! it, revoke the signed-in key, and what a read key sees; mint, list and
//! revoke a sign-in code, and how an OAuth grant shows (M10 Task 7); change
//! a key's or code's user, and assign one that has none (M11 Task 8).

use chrono::{TimeDelta, Utc};
use okf_mcp::config::Config;
use okf_mcp::labels::UNNAMED_USER;
use okf_mcp::store::audit::AuditActor;
use okf_mcp::store::keys::{self as key_rows, Access, Grants};
use okf_mcp::store::oauth::clients;
use okf_mcp::store::oauth::codes::{self, NewSignInCode};
use okf_mcp::store::users::{self, NewUser};

use serde_json::json;

use crate::harness::drive;
use crate::support::server::{files, TestServer, OWNER};

/// Creates the user `alice`, a second one to reassign credentials to.
async fn alice(server: &TestServer) {
    let alice = NewUser {
        name: "alice",
        display_name: &okf_mcp::labels::DisplayName::parse("Alice Example").unwrap(),
    };
    users::create(&server.db.pool, &alice, &AuditActor::cli(), Utc::now())
        .await
        .expect("create alice");
}

async fn server() -> TestServer {
    TestServer::start(
        vec![("main", files(&[("index.md", "# Main\n")]))],
        Config::default(),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_admin_creates_a_key_sees_its_secret_once_and_signs_in_with_it() {
    let server = server().await;
    let admin = server
        .key("console-admin", Access::Admin, &["main"], Some("main"))
        .await;
    drive(server.base.clone(), move |ui| {
        ui.login(&admin);
        ui.open("/keys");
        ui.wait_visible("[data-test=key-console-admin]");
        ui.click("#key-new");
        ui.fill("#key-form input[name=name]", "console-made");
        ui.select("#key-form select[name=user]", OWNER);
        ui.click("#key-form input[name=catalog][value=main]");
        ui.click("#key-form button[type=submit]");
        ui.wait_visible("#key-secret");
        let secret = ui.text("#key-secret");
        assert!(secret.starts_with("okf_"), "a key, not {secret:?}");
        ui.click("#key-secret-done");
        ui.wait_hidden("#key-secret");
        ui.wait_visible("[data-test=key-console-made]");
        // Once dismissed, the secret is nowhere on the page.
        let page = ui.eval("() => document.body.textContent", serde_json::json!(null));
        assert!(!page.as_str().unwrap_or_default().contains(&secret));
        ui.click("#signout");
        ui.wait_visible("#login");
        ui.login(&secret);
        ui.wait_text("#whoami", "console-made");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoking_the_signed_in_key_returns_the_browser_to_login() {
    let server = server().await;
    let key = server
        .key("console-self", Access::Admin, &["main"], Some("main"))
        .await;
    drive(server.base.clone(), move |ui| {
        ui.login(&key);
        ui.open("/keys");
        ui.click("[data-test=key-console-self]");
        ui.wait_visible("#key-revoke");
        ui.click("#key-revoke");
        ui.click("#confirm-ok");
        // The list reload after the revoke is the key's next request: a 401.
        ui.wait_visible("#login");
        ui.wait_text("#login-error", "refused");
        ui.open("");
        ui.wait_visible("#login");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_read_key_never_sees_the_keys_page() {
    let server = server().await;
    let key = server
        .key("console-reader", Access::Read, &["main"], Some("main"))
        .await;
    drive(server.base.clone(), move |ui| {
        ui.login(&key);
        ui.wait_text("#whoami", "read");
        assert!(!ui.page.is_visible("a[data-nav=keys]").expect("visible"));
        // Typed in by hand, the page explains instead of calling the API.
        ui.open("/keys");
        ui.wait_text("#main", "needs an admin key");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_admin_mints_a_sign_in_code_sees_it_once_and_revokes_it() {
    let server = server().await;
    let admin = server
        .key("console-admin", Access::Admin, &["main"], Some("main"))
        .await;
    let ran = drive(server.base.clone(), move |ui| {
        ui.login(&admin);
        ui.open("/keys");
        ui.wait_visible("#code-new");
        ui.click("#code-new");
        ui.fill("#code-form input[name=name]", "console-code");
        ui.fill("#code-form input[name=actor]", "example-actor");
        ui.select("#code-form select[name=user]", OWNER);
        // No catalog: the server refuses (409), which the page toasts. No
        // code was minted, so no dialog opens, and the form stays filled.
        ui.click("#code-form button[type=submit]");
        ui.wait_text("#toasts", "must be granted a catalog");
        assert!(!ui.page.is_visible("#code-secret-dialog").expect("visible"));
        let kept = ui.eval(
            "() => ['input[name=name]', 'input[name=actor]', 'select[name=user]'].map(field => \
                 document.querySelector(`#code-form ${field}`).value)",
            serde_json::json!(null),
        );
        assert_eq!(
            kept,
            serde_json::json!(["console-code", "example-actor", OWNER])
        );
        // The retry, with a catalog, mints the code.
        ui.click("#code-form input[name=catalog][value=main]");
        ui.click("#code-form button[type=submit]");
        ui.wait_visible("#code-secret");
        let secret = ui.text("#code-secret");
        assert!(
            secret.starts_with("okfsc_"),
            "a sign-in code, not {secret:?}"
        );
        // Escape does not close the dialog: only "I have stored it" does.
        ui.page.press_key(None, "Escape").expect("press Escape");
        let open = ui.eval(
            "() => document.getElementById('code-secret-dialog').open",
            serde_json::json!(null),
        );
        assert_eq!(open, serde_json::json!(true), "Escape closed the dialog");
        assert!(ui.page.is_visible("#code-secret").expect("visible"));
        ui.click("#code-secret-done");
        ui.wait_hidden("#code-secret");
        ui.wait_text("[data-test=code-state-console-code]", "live");
        // Once dismissed, the code is nowhere on the page, nor in storage.
        let page = ui.eval(
            "() => document.body.textContent + JSON.stringify(localStorage) \
             + JSON.stringify(sessionStorage)",
            serde_json::json!(null),
        );
        assert!(!page.as_str().unwrap_or_default().contains(&secret));
        ui.click("[data-test=code-console-code]");
        ui.wait_visible("#code-revoke");
        ui.click("#code-revoke");
        ui.click("#confirm-ok");
        // A revoked code leaves the default (live-only) list...
        ui.wait_hidden("[data-test=code-console-code]");
        // ...and shows again, revoked, with every state shown.
        ui.click("#codes-show-all");
        ui.wait_text("[data-test=code-state-console-code]", "revoked");
    })
    .await;
    if !ran {
        return;
    }
    let minted = codes::list(&server.db.pool).await.expect("list codes");
    assert_eq!(minted.len(), 1);
    assert_eq!(minted[0].name, "console-code");
    assert_eq!(minted[0].actor, "example-actor");
    assert!(minted[0].revoked_at.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_oauth_grant_is_marked_and_cannot_be_given_reporting() {
    let server = server().await;
    let admin = server
        .key("console-admin", Access::Admin, &["main"], Some("main"))
        .await;
    // A client and a grant made the way the consent page makes them (Task 3
    // registers clients; a raw row keeps this journey independent of it). A
    // CIMD row needs `fetched_at` and `cache_expires_at`.
    let client_id = "https://client.example.com/client.json";
    sqlx::query(
        "INSERT INTO oauth_clients \
         (client_id, kind, client_name, redirect_uris, metadata, fetched_at, cache_expires_at, \
          created_at) \
         VALUES ($1, 'cimd', 'Example Client', ARRAY['https://client.example.com/callback'], \
                 '{}'::jsonb, now(), now() + interval '1 hour', now())",
    )
    .bind(client_id)
    .execute(&server.db.pool)
    .await
    .expect("insert a client");
    let client = clients::find(&server.db.pool, client_id)
        .await
        .expect("read the client")
        .expect("the client exists");
    let main = ["main".to_string()];
    let code = codes::create(
        &server.db.pool,
        &NewSignInCode {
            name: "laptop",
            access: Access::Write,
            grants: Grants::Catalogs(&main),
            default_catalog: Some("main"),
            actor: "example-actor",
            expires_at: Utc::now() + TimeDelta::hours(1),
            user: OWNER,
        },
        &okf_mcp::store::audit::AuditActor::cli(),
        Utc::now(),
    )
    .await
    .expect("mint a code");
    let grant_id = codes::redeem(&server.db.pool, &code.id, &client, Utc::now())
        .await
        .expect("redeem the code");
    let name = key_rows::list(&server.db.pool)
        .await
        .expect("list keys")
        .into_iter()
        .find(|key| key.id == grant_id)
        .expect("the grant")
        .name;
    // A token issued for the grant at a known time two days ago: its last
    // activity (issue #269). A raw row, so the time is exact; recent, so the
    // poll loop's idle revocation leaves the grant alone.
    let issued = okf_mcp::clock::format_timestamp(Utc::now() - TimeDelta::days(2));
    sqlx::query(
        "INSERT INTO oauth_tokens \
         (sha256, grant_id, kind, family_id, resource, created_at, expires_at) \
         VALUES ($1, $2, 'refresh', 'family-1', 'https://okf.example.com/mcp', \
                 $3::timestamptz, $3::timestamptz + interval '30 days')",
    )
    .bind([7_u8; 32].as_slice())
    .bind(&grant_id)
    .bind(&issued)
    .execute(&server.db.pool)
    .await
    .expect("insert a token");
    // Quoted: a grant's name has dots (Task 4, Decision 1).
    let row = format!("[data-test=\"key-{name}\"]");
    let active = format!("[data-test=\"key-active-{name}\"]");
    drive(server.base.clone(), move |ui| {
        ui.login(&admin);
        ui.open("/keys");
        let text = ui.wait_text(&row, "OAuth");
        assert!(
            text.contains("Example Client"),
            "the client is named: {text:?}"
        );
        // The page shows times in the browser's locale: ask it for the
        // spelling it gives this one.
        let shown = ui.eval("(iso) => new Date(iso).toLocaleString()", json!(issued));
        let shown = shown.as_str().expect("a string").to_string();
        assert_eq!(ui.wait_text(&active, &shown), shown);
        // A plain key has no token activity.
        assert_eq!(
            ui.wait_text("[data-test=key-active-console-admin]", "-"),
            "-"
        );
        ui.click(&row);
        ui.wait_text("#drawer", "OAuth grant");
        ui.wait_text("#drawer dl.facts", &format!("Last active{shown}"));
        ui.wait_visible("#key-revoke");
        assert!(!ui.page.is_visible("#key-report-toggle").expect("visible"));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_admin_changes_a_keys_user_and_assigns_a_legacy_key() {
    let server = server().await;
    // Every catalog: only such a key manages the legacy key, which has them.
    let admin = server
        .key("console-admin", Access::Admin, &[], Some("main"))
        .await;
    server.key("laptop", Access::Read, &["main"], None).await;
    server.legacy_key("legacy", Access::Read).await;
    crate::support::server::applied(&server.app, async {
        alice(&server).await;
    })
    .await;
    let ran = drive(server.base.clone(), move |ui| {
        ui.login(&admin);
        // The shell names the signed-in key's user.
        ui.wait_text("#whoami", &format!("{UNNAMED_USER} / console-admin"));
        ui.open("/keys");
        ui.wait_text("[data-test=key-user-laptop]", UNNAMED_USER);
        ui.click("[data-test=key-laptop]");
        ui.wait_text("#key-assign", "Change user");
        ui.select("#key-user-select", "alice");
        ui.click("#key-assign");
        ui.wait_text("[data-test=key-user-laptop]", "Alice Example");
        ui.click("#drawer-close");
        // A key from before users existed is marked, and its drawer offers
        // Assign instead.
        ui.wait_text("[data-test=key-user-legacy]", "unassigned");
        ui.click("[data-test=key-legacy]");
        ui.wait_text("#drawer-title", "legacy");
        ui.wait_text("#key-assign", "Assign");
        ui.select("#key-user-select", OWNER);
        ui.click("#key-assign");
        ui.wait_text("[data-test=key-user-legacy]", UNNAMED_USER);
    })
    .await;
    if !ran {
        return;
    }
    let rows = key_rows::list(&server.db.pool).await.expect("list keys");
    let user_of = |name: &str| {
        rows.iter()
            .find(|key| key.name == name)
            .and_then(|key| key.user.clone())
    };
    assert_eq!(user_of("laptop").as_deref(), Some("alice"));
    assert_eq!(user_of("legacy").as_deref(), Some(OWNER));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_admin_changes_a_sign_in_codes_user() {
    let server = server().await;
    let admin = server
        .key("console-admin", Access::Admin, &[], Some("main"))
        .await;
    crate::support::server::applied(&server.app, async {
        alice(&server).await;
    })
    .await;
    let main = ["main".to_string()];
    codes::create(
        &server.db.pool,
        &NewSignInCode {
            name: "tablet",
            access: Access::Read,
            grants: Grants::Catalogs(&main),
            default_catalog: None,
            actor: "",
            expires_at: Utc::now() + TimeDelta::hours(1),
            user: OWNER,
        },
        &okf_mcp::store::audit::AuditActor::cli(),
        Utc::now(),
    )
    .await
    .expect("mint a code");
    let ran = drive(server.base.clone(), move |ui| {
        ui.login(&admin);
        ui.open("/keys");
        ui.wait_text("[data-test=code-user-tablet]", UNNAMED_USER);
        ui.click("[data-test=code-tablet]");
        ui.wait_text("#code-assign", "Change user");
        ui.select("#code-user-select", "alice");
        ui.click("#code-assign");
        ui.wait_text("[data-test=code-user-tablet]", "Alice Example");
    })
    .await;
    if !ran {
        return;
    }
    let minted = codes::list(&server.db.pool).await.expect("list codes");
    assert_eq!(minted[0].user.as_deref(), Some("alice"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reassigning_the_signed_in_key_updates_the_shell_identity() {
    let server = server().await;
    let admin = server
        .key("console-admin", Access::Admin, &["main"], Some("main"))
        .await;
    alice(&server).await;
    drive(server.base.clone(), move |ui| {
        ui.login(&admin);
        ui.wait_text("#whoami", &format!("{UNNAMED_USER} / console-admin"));
        ui.open("/keys");
        ui.click("[data-test=key-console-admin]");
        ui.wait_text("#key-assign", "Change user");
        ui.select("#key-user-select", "alice");
        ui.click("#key-assign");
        ui.wait_text("[data-test=key-user-console-admin]", "Alice Example");
        // The shell follows without a reload: the identity line, and the
        // Users page's "you" marker.
        ui.wait_text("#whoami", "Alice Example / console-admin");
        ui.click("#drawer-close");
        ui.open("/users");
        ui.wait_text("[data-test=user-alice]", "you");
        let tester = ui.text(&format!("[data-test=user-{OWNER}]"));
        assert!(!tester.contains("you"), "still marked: {tester:?}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_late_answer_for_an_earlier_drawer_never_replaces_or_reopens_it() {
    let server = server().await;
    let admin = server
        .key("console-admin", Access::Admin, &["main"], Some("main"))
        .await;
    server.key("alpha", Access::Read, &["main"], None).await;
    server.key("bravo", Access::Read, &["main"], None).await;
    drive(server.base.clone(), move |ui| {
        ui.login(&admin);
        ui.open("/keys");
        ui.wait_visible("[data-test=key-bravo]");
        // alpha's drawer waits on its held answer; bravo's opens meanwhile.
        ui.hold_next("GET", "/api/v1/users");
        ui.click("[data-test=key-alpha]");
        ui.click("[data-test=key-bravo]");
        ui.wait_text("#drawer-title", "bravo");
        ui.release_held();
        assert_eq!(
            ui.text("#drawer-title"),
            "bravo",
            "alpha's late answer replaced the drawer"
        );
        // Dismissing the drawer cancels a load still pending.
        ui.click("#drawer-close");
        ui.wait_hidden("#drawer");
        ui.hold_next("GET", "/api/v1/users");
        ui.click("[data-test=key-alpha]");
        ui.page.press_key(None, "Escape").expect("press Escape");
        ui.release_held();
        assert!(
            !ui.page.is_visible("#drawer").expect("visible"),
            "the dismissed load opened"
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_late_assign_never_replaces_a_newer_drawer_or_reopens_a_dismissed_one() {
    let server = server().await;
    let admin = server
        .key("console-admin", Access::Admin, &["main"], Some("main"))
        .await;
    server.key("alpha", Access::Read, &["main"], None).await;
    server.key("bravo", Access::Read, &["main"], None).await;
    alice(&server).await;
    drive(server.base.clone(), move |ui| {
        ui.login(&admin);
        ui.open("/keys");
        // alpha's assign is held while the user moves on to bravo.
        ui.click("[data-test=key-alpha]");
        ui.wait_text("#drawer-title", "alpha");
        ui.select("#key-user-select", "alice");
        ui.hold_next("PUT", "/user");
        ui.click("#key-assign");
        ui.click("#drawer-close");
        ui.wait_hidden("#drawer");
        // Visible again only once bravo's drawer is drawn.
        ui.click("[data-test=key-bravo]");
        ui.wait_visible("#drawer");
        ui.wait_text("#drawer-title", "bravo");
        ui.release_held();
        // The table follows the assign either way.
        ui.wait_text("[data-test=key-user-alpha]", "Alice Example");
        let replaced = ui.text("#drawer-title") != "bravo";
        // bravo's assign is held, and the drawer dismissed meanwhile.
        ui.click("#drawer-close");
        ui.wait_hidden("#drawer");
        ui.click("[data-test=key-bravo]");
        ui.wait_visible("#drawer");
        ui.wait_text("#drawer-title", "bravo");
        ui.select("#key-user-select", "alice");
        ui.hold_next("PUT", "/user");
        ui.click("#key-assign");
        ui.page.press_key(None, "Escape").expect("press Escape");
        ui.wait_hidden("#drawer");
        ui.release_held();
        ui.wait_text("[data-test=key-user-bravo]", "Alice Example");
        let reopened = ui.page.is_visible("#drawer").expect("visible");
        assert!(
            !replaced && !reopened,
            "alpha's late assign replaced bravo's drawer: {replaced}; \
             bravo's late assign reopened the dismissed drawer: {reopened}"
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_assigned_user_shows_when_reading_the_users_again_fails() {
    let server = server().await;
    let admin = server
        .key("console-admin", Access::Admin, &["main"], Some("main"))
        .await;
    server.key("alpha", Access::Read, &["main"], None).await;
    alice(&server).await;
    let ran = drive(server.base.clone(), move |ui| {
        ui.login(&admin);
        ui.open("/keys");
        ui.click("[data-test=key-alpha]");
        ui.wait_text("#drawer-title", "alpha");
        // The assign succeeds, and the users read for the redrawn drawer
        // fails: the drawer still shows the user the PUT answered.
        ui.select("#key-user-select", "alice");
        ui.fail_next("GET", "/api/v1/users", 503, "the server is unavailable");
        ui.click("#key-assign");
        ui.wait_text("#toasts", "alpha now belongs to alice");
        // By her label, from the users the drawer was drawn with.
        ui.wait_text("#drawer dl.facts", "Alice Example");
    })
    .await;
    if !ran {
        return;
    }
    let keys = key_rows::list(&server.db.pool).await.expect("list keys");
    let user = keys
        .iter()
        .find(|key| key.name == "alpha")
        .and_then(|key| key.user.clone());
    assert_eq!(user.as_deref(), Some("alice"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_older_writes_late_table_read_never_redraws_over_a_newer_write() {
    let server = server().await;
    let admin = server
        .key("console-admin", Access::Admin, &["main"], Some("main"))
        .await;
    server.key("alpha", Access::Read, &["main"], None).await;
    alice(&server).await;
    let ran = drive(server.base.clone(), move |ui| {
        ui.login(&admin);
        ui.open("/keys");
        ui.click("[data-test=key-alpha]");
        ui.wait_text("#drawer-title", "alpha");
        // Reporting is enabled; the table read after it answers late.
        ui.hold_answer("GET", "/api/v1/keys", || ui.click("#key-report-toggle"));
        ui.wait_text("#toasts", "alpha may now report");
        // Meanwhile alpha is given to alice in the same drawer, and drawn.
        ui.select("#key-user-select", "alice");
        ui.click("#key-assign");
        ui.wait_text("#toasts", "alpha now belongs to alice");
        // By her label, from the users the drawer was drawn with.
        ui.wait_text("#drawer dl.facts", "Alice Example");
        // The older write's late table answer must not redraw its answer
        // (no user change) over the newer one.
        ui.release_held();
        let facts = ui.text("#drawer dl.facts");
        assert!(
            facts.contains("Alice Example"),
            "the older write redrew the drawer: {facts}"
        );
    })
    .await;
    if !ran {
        return;
    }
    let keys = key_rows::list(&server.db.pool).await.expect("list keys");
    let alpha = keys
        .iter()
        .find(|key| key.name == "alpha")
        .expect("alpha is stored");
    assert_eq!(alpha.user.as_deref(), Some("alice"));
    assert!(alpha.maintenance_report);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_write_settling_for_one_key_never_cancels_opening_another() {
    let server = server().await;
    let admin = server
        .key("console-admin", Access::Admin, &["main"], Some("main"))
        .await;
    server.key("alpha", Access::Read, &["main"], None).await;
    server.key("bravo", Access::Read, &["main"], None).await;
    alice(&server).await;
    let ran = drive(server.base.clone(), move |ui| {
        ui.login(&admin);
        ui.open("/keys");
        ui.click("[data-test=key-alpha]");
        ui.wait_text("#drawer-title", "alpha");
        // alpha's assign is held; its drawer is closed, and bravo's drawer
        // is asked for with its users read held too.
        ui.select("#key-user-select", "alice");
        ui.hold_next("PUT", "/user");
        ui.click("#key-assign");
        ui.eval(
            "() => { window.releaseFirst = window.releaseHeld; }",
            serde_json::json!(null),
        );
        ui.click("#drawer-close");
        ui.wait_hidden("#drawer");
        ui.hold_next("GET", "/api/v1/users");
        ui.click("[data-test=key-bravo]");
        // alpha's assign settles first, then bravo's read answers: bravo
        // opens.
        ui.eval("() => window.releaseFirst()", serde_json::json!(null));
        ui.wait_text("#toasts", "alpha now belongs to alice");
        ui.release_held();
        ui.wait_visible("#drawer");
        ui.wait_text("#drawer-title", "bravo");
    })
    .await;
    if !ran {
        return;
    }
    let keys = key_rows::list(&server.db.pool).await.expect("list keys");
    let alpha = keys
        .iter()
        .find(|key| key.name == "alpha")
        .expect("alpha is stored");
    assert_eq!(alpha.user.as_deref(), Some("alice"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_admin_creates_a_provisioner_key_that_reaches_no_catalog() {
    let server = server().await;
    let admin = server.key("console-admin", Access::Admin, &[], None).await;
    let ran = drive(server.base.clone(), move |ui| {
        ui.login(&admin);
        ui.open("/keys");
        ui.click("#key-new");
        ui.fill("#key-form input[name=name]", "signup");
        ui.select("#key-form select[name=user]", OWNER);
        ui.click("#key-form input[name=provisioner]");
        ui.wait_visible("#key-form fieldset.key-reach[disabled]");
        ui.hold_next("POST", "/api/v1/keys");
        ui.click_twice("#key-form button[type=submit]");
        assert_eq!(ui.held_matches(), 1);
        ui.click("#drawer-close");
        ui.click("#key-new");
        ui.wait_visible("#key-form");
        ui.fill("#key-form input[name=name]", "second-signup");
        ui.select("#key-form select[name=user]", OWNER);
        assert!(ui.click_twice("#key-form button[type=submit]"));
        assert!(ui.click_twice("#key-form input[name=provisioner]"));
        assert_eq!(
            ui.held_matches(),
            1,
            "reopening must not start a second write"
        );
        ui.release_held();
        ui.wait_visible("#key-secret");
        ui.click("#key-secret-done");
        ui.wait_visible("[data-test=key-provisioner-signup]");
    })
    .await;
    if ran {
        let record = key_rows::list(&server.db.pool)
            .await
            .unwrap()
            .into_iter()
            .find(|k| k.name == "signup")
            .unwrap();
        assert!(record.provisioner && !record.all_catalogs && record.catalogs.is_empty());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_keys_page_shows_users_and_catalogs_by_label() {
    let server = server().await;
    // Every catalog: a scoped admin reads only its own user's label.
    let admin = server
        .key("console-admin", Access::Admin, &[], Some("main"))
        .await;
    server
        .key("laptop", Access::Read, &["main"], Some("main"))
        .await;
    alice(&server).await;
    server.set_catalog_label("main", "Work notes").await;
    drive(server.base.clone(), move |ui| {
        ui.login(&admin);
        ui.open("/keys");
        ui.wait_visible("[data-test=key-laptop]");
        ui.click("[data-test=key-laptop]");
        ui.select("#key-user-select", "alice");
        ui.click("#key-assign");
        // The user column is the label, not the slug.
        ui.wait_text("[data-test=key-user-laptop]", "Alice Example");
        // Its drawer lists the catalog grant and the default by label.
        let drawer = ui.text("#drawer");
        assert!(drawer.contains("Work notes"), "{drawer}");
        assert!(drawer.contains("Alice Example"), "{drawer}");
        // The ungrant control keeps the slug.
        ui.wait_visible("[data-ungrant=main]");
        ui.click("#drawer-close");
        let table = ui.text("table");
        assert!(table.contains("Work notes"), "{table}");
    })
    .await;
}

/// Opens the code form on a server whose `oauth.code_max_expiry` is `limit`
/// (the default when `None`), reads what it offers, mints a code with its
/// default and reads that code's lifetime.
///
/// # Returns
/// The option values, the selected value and the minted code's lifetime.
async fn offered_and_minted(
    limit: Option<std::time::Duration>,
) -> (Vec<String>, String, TimeDelta) {
    let mut config = Config::default();
    if let Some(limit) = limit {
        config.oauth.code_max_expiry = limit;
    }
    let server =
        TestServer::start(vec![("main", files(&[("index.md", "# Main\n")]))], config).await;
    let admin = server
        .key("console-admin", Access::Admin, &["main"], Some("main"))
        .await;
    let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
    let sink = std::sync::Arc::clone(&seen);
    let ran = drive(server.base.clone(), move |ui| {
        ui.login(&admin);
        ui.open("/keys");
        ui.wait_visible("#code-new");
        ui.click("#code-new");
        ui.wait_visible("#code-form");
        let offered = ui.eval(
            "() => [[...document.querySelectorAll('#code-form select[name=expires_in] option')].map(o => o.value), \
                    document.querySelector('#code-form select[name=expires_in]').value]",
            json!(null),
        );
        *sink.lock().expect("lock") = Some(offered);
        ui.fill("#code-form input[name=name]", "limit-code");
        ui.select("#code-form select[name=user]", OWNER);
        ui.click("#code-form input[name=catalog][value=main]");
        ui.click("#code-form button[type=submit]");
        ui.wait_visible("#code-secret");
    })
    .await;
    assert!(ran);
    let offered = seen
        .lock()
        .expect("lock")
        .take()
        .expect("the form was read");
    let values = offered[0]
        .as_array()
        .expect("options")
        .iter()
        .map(|value| value.as_str().expect("a string").to_string())
        .collect();
    let selected = offered[1].as_str().expect("selected").to_string();
    let minted = codes::list(&server.db.pool).await.expect("list codes");
    assert_eq!(minted.len(), 1, "the form's default was accepted");
    (
        values,
        selected,
        minted[0].expires_at - minted[0].created_at,
    )
}

/// Issue #270: the code form offers only the expiries `oauth.code_max_expiry`
/// allows, selects 24 hours when allowed (else the longest choice), and the
/// default it selects mints a code of that lifetime. Reviews of #657: the
/// default limit, a limit between two choices, one that leaves only 1 hour,
/// and one shorter than every choice (the limit itself is the choice).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_code_form_offers_only_expiries_within_the_servers_limit() {
    use std::time::Duration;
    let cases: [(Option<Duration>, &[&str], &str, TimeDelta); 4] = [
        // The default limit is 7 days: every choice, 24 hours selected.
        (
            None,
            &["1h", "24h", "3d", "7d"],
            "24h",
            TimeDelta::hours(24),
        ),
        // 3 days: 7d is hidden, 24 hours still selected.
        (
            Some(Duration::from_secs(3 * 86_400)),
            &["1h", "24h", "3d"],
            "24h",
            TimeDelta::hours(24),
        ),
        // 2 hours: only 1h, selected.
        (
            Some(Duration::from_secs(2 * 3600)),
            &["1h"],
            "1h",
            TimeDelta::hours(1),
        ),
        // 90 seconds: shorter than every choice, so the limit itself.
        (
            Some(Duration::from_secs(90)),
            &["90s"],
            "90s",
            TimeDelta::seconds(90),
        ),
    ];
    for (limit, options, selected, lifetime) in cases {
        let (offered, chosen, minted) = offered_and_minted(limit).await;
        assert_eq!(offered, options, "limit {limit:?}");
        assert_eq!(chosen, selected, "limit {limit:?}");
        assert_eq!(minted, lifetime, "limit {limit:?}");
    }
}

/// Registers a client, mints a sign-in code `laptop` and redeems it, as the
/// consent page does.
///
/// # Returns
/// The grant's id and its key name.
async fn redeemed_grant(server: &TestServer) -> (String, String) {
    let client_id = "https://client.example.com/client.json";
    sqlx::query(
        "INSERT INTO oauth_clients \
         (client_id, kind, client_name, redirect_uris, metadata, fetched_at, cache_expires_at, \
          created_at) \
         VALUES ($1, 'cimd', 'Example Client', ARRAY['https://client.example.com/callback'], \
                 '{}'::jsonb, now(), now() + interval '1 hour', now())",
    )
    .bind(client_id)
    .execute(&server.db.pool)
    .await
    .expect("insert a client");
    let client = clients::find(&server.db.pool, client_id)
        .await
        .expect("read the client")
        .expect("the client exists");
    let main = ["main".to_string()];
    let code = codes::create(
        &server.db.pool,
        &NewSignInCode {
            name: "laptop",
            access: Access::Write,
            grants: Grants::Catalogs(&main),
            default_catalog: Some("main"),
            actor: "example-actor",
            expires_at: Utc::now() + TimeDelta::hours(1),
            user: OWNER,
        },
        &AuditActor::cli(),
        Utc::now(),
    )
    .await
    .expect("mint a code");
    let grant_id = codes::redeem(&server.db.pool, &code.id, &client, Utc::now())
        .await
        .expect("redeem the code");
    let name = key_rows::list(&server.db.pool)
        .await
        .expect("list keys")
        .into_iter()
        .find(|key| key.id == grant_id)
        .expect("the grant")
        .name;
    (grant_id, name)
}

/// Review of #657: the code form is not offered with every expiry while the
/// first list (which carries the limit) is pending or failed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_code_form_never_offers_every_expiry_before_the_limit_is_known() {
    let mut config = Config::default();
    config.oauth.code_max_expiry = std::time::Duration::from_secs(2 * 3600);
    let server =
        TestServer::start(vec![("main", files(&[("index.md", "# Main\n")]))], config).await;
    let admin = server
        .key("console-admin", Access::Admin, &["main"], Some("main"))
        .await;
    let offered = |ui: &crate::harness::Ui| {
        ui.eval(
            "() => [...document.querySelectorAll('#code-form select[name=expires_in] option')].map(o => o.value)",
            json!(null),
        )
    };
    let ran = drive(server.base.clone(), move |ui| {
        ui.login(&admin);
        // The first list is held: New must fetch the limit itself.
        ui.hold_next("GET", "/oauth/codes");
        ui.open("/keys");
        ui.wait_visible("#code-new");
        ui.click("#code-new");
        ui.wait_visible("#code-form");
        assert_eq!(offered(ui), json!(["1h"]));
        ui.release_held();
        ui.click("#drawer-close");
        // The first list fails: New fetches it again, and offers only 1h.
        ui.fail_next("GET", "/oauth/codes", 503, "unavailable");
        ui.open("/health");
        ui.open("/keys");
        ui.wait_visible("#code-new");
        ui.click("#code-new");
        ui.wait_visible("#code-form");
        assert_eq!(offered(ui), json!(["1h"]));
    })
    .await;
    assert!(ran);
}

// Installed in the page: every `GET /oauth/codes` fails while `mode` is
// "fail" (so the limit stays unknown whatever number of times the page
// asks for the first list), and is held until `releaseLimit()` while it
// is "hold", and goes through untouched in any other mode.
const LIMIT_READS: &str = "() => {
    const original = window.fetch;
    window.limitMode = 'fail';
    window.limitHeld = [];
    window.releaseLimit = () => window.limitHeld.splice(0).forEach(release => release());
    window.fetch = (input, init) => {
        const method = (init && init.method) || 'GET';
        if (method !== 'GET' || !String(input).endsWith('/oauth/codes')) {
            return original(input, init);
        }
        if (window.limitMode === 'fail') {
            return Promise.resolve(new Response(
                JSON.stringify({ error: 'test', message: 'limit list down' }),
                { status: 503, headers: { 'Content-Type': 'application/json' } }));
        }
        if (window.limitMode === 'pass') return original(input, init);
        return new Promise(resolve => {
            window.limitHeld.push(() => resolve(original(input, init)));
        });
    };
    return true;
}";

/// Review of #657 (round 3): when New's own read of the limit fails, the
/// form stays closed (the failure is toasted) and New tries again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_limit_read_keeps_the_code_form_closed_until_it_succeeds() {
    let server = server().await;
    let admin = server
        .key("console-admin", Access::Admin, &["main"], Some("main"))
        .await;
    let ran = drive(server.base.clone(), move |ui| {
        ui.login(&admin);
        ui.eval(LIMIT_READS, json!(null));
        ui.open("/keys");
        ui.wait_visible("#code-new");
        ui.wait_text("#toasts", "limit list down");
        let toasts = |ui: &crate::harness::Ui| {
            ui.eval(
                "() => document.querySelectorAll('#toasts .toast').length",
                json!(null),
            )
            .as_u64()
            .unwrap_or(0)
        };
        let before = toasts(ui);
        ui.eval(
            "() => document.getElementById('code-new').click()",
            json!(null),
        );
        // New's own read failed too: one more toast, and no form.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while toasts(ui) <= before {
            assert!(std::time::Instant::now() < deadline, "no second failure");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        // A form that did open would do so after the users read; give it time.
        std::thread::sleep(std::time::Duration::from_millis(1000));
        assert!(!ui.page.is_visible("#code-form").expect("visible"));
        // The list answers again: New opens the form, with the real limit.
        ui.eval("() => { window.limitMode = 'pass'; }", json!(null));
        ui.eval(
            "() => document.getElementById('code-new').click()",
            json!(null),
        );
        ui.wait_visible("#code-form");
    })
    .await;
    assert!(ran);
}

/// Review of #657 (round 3): a redeemed code's grant that the key list does
/// not hold (here its read failed) shows as text, not a button.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_grant_missing_from_the_key_list_shows_as_text() {
    let server = server().await;
    let admin = server
        .key("console-admin", Access::Admin, &["main"], Some("main"))
        .await;
    let (grant_id, _) = redeemed_grant(&server).await;
    let ran = drive(server.base.clone(), move |ui| {
        ui.login(&admin);
        ui.fail_next("GET", "/keys", 503, "keys unavailable");
        ui.open("/keys");
        ui.wait_visible("#codes-show-all");
        ui.click("#codes-show-all");
        ui.wait_visible("[data-test=code-laptop]");
        ui.eval(
            "() => document.querySelector('[data-test=code-laptop]').click()",
            json!(null),
        );
        ui.wait_text("#drawer dl.facts", &grant_id);
        assert!(!ui.page.is_visible("#code-open-grant").expect("visible"));
    })
    .await;
    assert!(ran);
}

/// Review of #657 (round 2): the code form does not open over a drawer
/// opened, or after one dismissed, while its limit read was pending.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pending_limit_read_does_not_open_the_code_form_over_a_newer_drawer() {
    let server = server().await;
    let admin = server
        .key("console-admin", Access::Admin, &["main"], Some("main"))
        .await;
    let ran = drive(server.base.clone(), move |ui| {
        ui.login(&admin);
        ui.eval(LIMIT_READS, json!(null));
        let row = "[data-test=key-console-admin]";
        let held = |ui: &crate::harness::Ui| {
            ui.eval("() => window.limitHeld.length", json!(null))
                .as_u64()
                .unwrap_or(0)
        };
        for dismiss in [false, true] {
            ui.eval("() => { window.limitMode = 'fail'; }", json!(null));
            ui.open("/health");
            ui.wait_hidden(row);
            ui.open("/keys");
            ui.wait_visible(row);
            // The failure's toast shows the first list has been answered.
            ui.wait_text("#toasts", "limit list down");
            ui.eval("() => { window.limitMode = 'hold'; }", json!(null));
            // In the page: the toast may sit under a real click.
            ui.eval(
                "() => document.getElementById('code-new').click()",
                json!(null),
            );
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            while held(ui) == 0 {
                assert!(
                    std::time::Instant::now() < deadline,
                    "New never read the limit list (dismiss: {dismiss})"
                );
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            ui.eval(
                "(selector) => document.querySelector(selector).click()",
                json!(row),
            );
            ui.wait_text("#drawer dl.facts", "API key");
            if dismiss {
                ui.click("#drawer-close");
            }
            ui.eval("() => window.releaseLimit()", json!(null));
            std::thread::sleep(std::time::Duration::from_millis(1000));
            assert!(
                !ui.page.is_visible("#code-form").expect("visible"),
                "the code form opened (dismissed: {dismiss})"
            );
            assert_eq!(
                ui.page.is_visible("#drawer").expect("visible"),
                !dismiss,
                "dismissed: {dismiss}"
            );
            if !dismiss {
                ui.click("#drawer-close");
            }
        }
    })
    .await;
    assert!(ran);
}

/// Review of #657: a keys read that answers after the page was left and
/// shown again must not replace the visible page's rows or loader, so a
/// grant change from the code drawer still refreshes the current table.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_late_keys_read_from_a_left_page_does_not_hijack_the_current_one() {
    let server = server().await;
    let admin = server
        .key("console-admin", Access::Admin, &["main"], Some("main"))
        .await;
    let (grant_id, name) = redeemed_grant(&server).await;
    let row = format!("[data-test=\"key-{name}\"]");
    let ran = drive(server.base.clone(), move |ui| {
        ui.login(&admin);
        // The first visit's keys read is answered early and delivered late.
        ui.hold_answer("GET", "/keys", || ui.open("/keys"));
        ui.open("/health");
        ui.open("/keys");
        ui.wait_visible(&row);
        ui.release_held();
        ui.click("#codes-show-all");
        ui.wait_visible("[data-test=code-laptop]");
        ui.click("[data-test=code-laptop]");
        ui.wait_visible("#code-open-grant");
        ui.click("#code-open-grant");
        ui.wait_text("#drawer dl.facts", &grant_id);
        ui.click("#key-revoke");
        ui.click("#confirm-ok");
        // Revoked keys leave the default list: the CURRENT table redrew.
        ui.wait_hidden(&row);
    })
    .await;
    assert!(ran);
}

/// Issue #270: a redeemed code's drawer opens its grant's drawer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_redeemed_codes_grant_opens_from_its_drawer() {
    let server = server().await;
    let admin = server
        .key("console-admin", Access::Admin, &["main"], Some("main"))
        .await;
    let (grant_id, _) = redeemed_grant(&server).await;
    let ran = drive(server.base.clone(), move |ui| {
        ui.login(&admin);
        ui.open("/keys");
        ui.wait_visible("#codes-show-all");
        ui.click("#codes-show-all");
        ui.wait_visible("[data-test=code-laptop]");
        ui.click("[data-test=code-laptop]");
        ui.wait_visible("#code-open-grant");
        ui.click("#code-open-grant");
        // The drawer now shows the grant: its kind and its id.
        ui.wait_text("#drawer dl.facts", "OAuth grant");
        ui.wait_text("#drawer dl.facts", &grant_id);
        ui.wait_visible("#key-revoke");
    })
    .await;
    assert!(ran);
}

/// Issue #270: Pico's light-theme variables must not outrank the console's
/// own mapping, so a primary button is the console's accent in both themes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_console_colours_beat_picos_in_both_themes() {
    let server = server().await;
    let admin = server
        .key("console-admin", Access::Admin, &["main"], Some("main"))
        .await;
    let ran = drive(server.base.clone(), move |ui| {
        ui.login(&admin);
        ui.open("/keys");
        ui.wait_visible("#key-new");
        let colour = |ui: &crate::harness::Ui| {
            ui.eval(
                // Pico fades colours over .2s; no transition reads the end value.
                "() => { const b = document.getElementById('key-new'); \
                   b.style.transition = 'none'; \
                   return [document.documentElement.dataset.theme, \
                           getComputedStyle(b).backgroundColor]; }",
                json!(null),
            )
        };
        let first = colour(ui);
        ui.click("#theme-toggle");
        let second = colour(ui);
        let mut seen = [first, second];
        seen.sort_by_key(|pair| pair[0].as_str().unwrap().to_string());
        // styles.css: --accent #6d28d9 (light), #a78bfa (dark).
        assert_eq!(seen[0], json!(["dark", "rgb(167, 139, 250)"]));
        assert_eq!(seen[1], json!(["light", "rgb(109, 40, 217)"]));
    })
    .await;
    assert!(ran);
}

/// Creates `bob` ("Bob Builder"), a user no key belongs to, and gives the
/// key `laptop` to `alice`, both by SQL.
async fn bob_and_alices_laptop(server: &TestServer) {
    let bob = NewUser {
        name: "bob",
        display_name: &okf_mcp::labels::DisplayName::parse("Bob Builder").unwrap(),
    };
    users::create(&server.db.pool, &bob, &AuditActor::cli(), Utc::now())
        .await
        .expect("create bob");
    sqlx::query(
        "UPDATE api_keys SET user_id = (SELECT id FROM users WHERE name = 'alice') \
         WHERE name = 'laptop'",
    )
    .execute(&server.db.pool)
    .await
    .expect("give laptop to alice");
}

/// Issue #749: the shell no longer holds every user's label for an
/// every-catalog admin; the keys table reads the labels of the users its
/// rows name, and only those.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_keys_table_reads_the_labels_of_the_users_it_shows_only() {
    let server = server().await;
    let admin = server
        .key("console-admin", Access::Admin, &[], Some("main"))
        .await;
    server
        .key("laptop", Access::Read, &["main"], Some("main"))
        .await;
    alice(&server).await;
    bob_and_alices_laptop(&server).await;
    drive(server.base.clone(), move |ui| {
        ui.login(&admin);
        ui.open("/keys");
        ui.wait_text("[data-test=key-user-laptop]", "Alice Example");
        // Nobody shown is bob: his label was never read.
        let bob = ui.eval(
            "async () => (await import('/ui/js/core/labels.js')).userLabel('bob')",
            json!(null),
        );
        assert_eq!(bob, json!("bob"), "a label of a user no row shows");
        // A sort by User redraws the cells from the labels read with the
        // rows (the third column).
        ui.click("#keys-table th:nth-child(3)");
        ui.wait_text("[data-test=key-user-laptop]", "Alice Example");
    })
    .await;
}

/// Issue #749 (`tools/agentic/lessons.md`: a failed side read never
/// discards an answer): when the labels read fails, the keys still draw,
/// their users by slug, with no toast.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_labels_read_still_draws_the_keys_by_slug() {
    let server = server().await;
    let admin = server
        .key("console-admin", Access::Admin, &[], Some("main"))
        .await;
    server
        .key("laptop", Access::Read, &["main"], Some("main"))
        .await;
    alice(&server).await;
    bob_and_alices_laptop(&server).await;
    drive(server.base.clone(), move |ui| {
        ui.login(&admin);
        ui.wait_visible("#app");
        ui.fail_next(
            "GET",
            "/api/v1/labels/named?users=alice",
            503,
            "labels are away",
        );
        ui.open("/keys");
        ui.wait_text("[data-test=key-user-laptop]", "alice");
        assert_eq!(ui.text("[data-test=key-user-laptop]"), "alice");
        assert!(ui
            .page
            .is_visible("[data-test=key-laptop]")
            .expect("visible"));
        assert!(
            !ui.text("#toasts").contains("labels are away"),
            "{}",
            ui.text("#toasts")
        );
        // The next load asks again, and names her.
        ui.open("/teams");
        ui.wait_visible("#teams-table");
        ui.open("/keys");
        ui.wait_text("[data-test=key-user-laptop]", "Alice Example");
    })
    .await;
}

/// Review of PR #754 (`tools/agentic/lessons.md`: overlapping reads draw
/// only the newest answer): a labels read asked for before a refresh, and
/// answering after a read asked for since, never replaces the newer label.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_late_labels_answer_never_replaces_a_newer_one() {
    let server = server().await;
    let admin = server
        .key("console-admin", Access::Admin, &[], Some("main"))
        .await;
    alice(&server).await;
    drive(server.base.clone(), move |ui| {
        ui.login(&admin);
        ui.wait_visible("#app");
        // The first read of alice's label reaches the server now (her old
        // label), and its answer is held.
        ui.hold_answer("GET", "/api/v1/labels/named?users=alice", || {
            ui.eval(
                "async () => { \
                   const labels = await import('/ui/js/core/labels.js'); \
                   window.firstRead = labels.ensureLabels({ users: ['alice'] }); \
                   return true; }",
                json!(null),
            );
        });
        // She is renamed, the shell refreshes, and a newer read settles.
        let newer = ui.eval(
            "async () => { \
               const { api } = await import('/ui/js/core/api.js'); \
               const shell = await import('/ui/js/core/shell.js'); \
               const labels = await import('/ui/js/core/labels.js'); \
               await api.patch('/users/alice', { display_name: 'Alice Renamed' }); \
               await shell.refreshIdentity(); \
               await labels.ensureLabels({ users: ['alice'] }); \
               return labels.userLabel('alice'); }",
            json!(null),
        );
        assert_eq!(newer, json!("Alice Renamed"));
        // The old answer arrives last; the newer label stays.
        ui.release_held();
        let shown = ui.eval(
            "async () => { \
               await window.firstRead; \
               return (await import('/ui/js/core/labels.js')).userLabel('alice'); }",
            json!(null),
        );
        assert_eq!(
            shown,
            json!("Alice Renamed"),
            "the late answer replaced the newer label"
        );
    })
    .await;
}

/// Issue #1086: a revoke that finishes after the person left the Keys page
/// and came back reloads the list shown now, so the revoked key leaves it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_revoke_that_finishes_after_navigating_away_updates_the_list_shown() {
    let server = server().await;
    let admin = server.key("console-admin", Access::Admin, &[], None).await;
    server.key("victim", Access::Read, &["main"], None).await;
    drive(server.base.clone(), move |ui| {
        ui.login(&admin);
        ui.open("/keys");
        ui.click("[data-test=key-victim]");
        ui.wait_visible("#key-revoke");
        ui.hold_next("POST", "/revoke");
        ui.click("#key-revoke");
        ui.click("#confirm-ok");
        // Away from the page and back: the new page lists the key still.
        ui.click("#drawer-close");
        ui.open("/teams");
        ui.wait_visible(&format!("[data-test=team-{OWNER}]"));
        ui.open("/keys");
        ui.wait_visible("[data-test=key-victim]");
        ui.release_held();
        ui.wait_hidden("[data-test=key-victim]");
    })
    .await;
}
