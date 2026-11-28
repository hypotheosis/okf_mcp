//! The OAuth consent page in a real browser, with no console errors or CSP
//! violations: the sign-in code path (step 1, step 2, Approve), and a
//! person who signs in on the way and approves catalogs from their
//! ceiling. Both end with the hand-off to the client, on another port. A
//! third journey's redirect URI redirects again, to yet another origin (as
//! `www.` to the bare domain does): Approve must still get there, which a
//! 303 under the consent page's `form-action` could not (issue #435).
//! With only one catalog offered, Advanced Options starts closed: Approve
//! as shown grants that catalog at write as the default, and opening the
//! disclosure lets the person change that (issue #718).

use axum::extract::RawQuery;
use axum::response::{Html, Redirect};
use axum::routing::get;
use axum::Router;
use chrono::{TimeDelta, Utc};
use okf_mcp::config::Config;
use okf_mcp::oauth::clients::MAX_UNBOUND_DCR;
use okf_mcp::store::audit::AuditActor;
use okf_mcp::store::ceiling;
use okf_mcp::store::ceiling::NewEntitlement;
use okf_mcp::store::keys::{self as key_rows, Access, Grants, KeyKind, KeyRecord};
use okf_mcp::store::oauth::clients::{self as client_rows, NewDcrClient};
use okf_mcp::store::oauth::codes::{self as code_rows, NewSignInCode};
use rustwright::GotoOptions;

use crate::support::server::{files, TestServer};
use crate::ui_e2e::harness::{drive, Ui};

const PASSWORD: &str = "violet-lantern-orbit-42";

/// Serves a page at `/callback` standing in for the client, on its own port.
///
/// # Returns
/// Its base URL, `http://127.0.0.1:<port>`.
async fn callback_host() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("address"));
    let router = Router::new().route(
        "/callback",
        get(|| async {
            Html("<!DOCTYPE html><title>client</title><p id=\"done\">back at the client</p>")
        }),
    );
    // Runs until the test's runtime shuts down.
    tokio::spawn(async move { axum::serve(listener, router).await });
    base
}

/// Serves `/callback` on its own port as a 308 to `to`, query kept: a
/// redirect URI that redirects again, to another origin.
///
/// # Returns
/// Its base URL, `http://127.0.0.1:<port>`.
async fn hop_host(to: String) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("address"));
    let router = Router::new().route(
        "/callback",
        // `move` hands the closure its own copy of `to`; each request clones
        // it again for its `async` block.
        get(move |RawQuery(query): RawQuery| {
            let to = to.clone();
            async move { Redirect::permanent(&format!("{to}?{}", query.unwrap_or_default())) }
        }),
    );
    tokio::spawn(async move { axum::serve(listener, router).await });
    base
}

/// A server whose `public_url` is its own address, a registered DCR client
/// whose redirect URI is [`callback_host`]'s (or a [`hop_host`]'s in front
/// of it), and the authorize URL for it.
struct Journey {
    server: TestServer,
    client_id: String,
    redirect_uri: String,
    /// The client page the browser lands on: `redirect_uri`, or where its
    /// hop sends it.
    landing: String,
}

impl Journey {
    async fn start() -> Self {
        Self::start_via(false).await
    }

    /// [`Journey::start`], with the redirect URI a [`hop_host`] when `hop`.
    async fn start_via(hop: bool) -> Self {
        let server = TestServer::start_with(
            vec![
                ("main", files(&[("index.md", "# Main\n")])),
                ("other", files(&[("index.md", "# Other\n")])),
            ],
            |base| Config {
                public_url: Some(base.to_string()),
                ..Config::default()
            },
        )
        .await;
        let landing = format!("{}/callback", callback_host().await);
        let redirect_uri = if hop {
            format!("{}/callback", hop_host(landing.clone()).await)
        } else {
            landing.clone()
        };
        // `insert_dcr` takes the cap of unbound clients and returns the stored
        // client (whose id Postgres picks), or `None` at the cap.
        let redirect_uris = [redirect_uri.clone()];
        let metadata = serde_json::json!({ "redirect_uris": [redirect_uri] });
        let client = client_rows::insert_dcr(
            &server.db.pool,
            &NewDcrClient {
                client_name: Some("Example Client"),
                redirect_uris: &redirect_uris,
                metadata: &metadata,
            },
            Utc::now(),
            MAX_UNBOUND_DCR,
        )
        .await
        .expect("register the client")
        .expect("under the cap");
        Self {
            server,
            client_id: client.client_id,
            redirect_uri,
            landing,
        }
    }

    /// The authorize URL, with `extra` pairs after the standard ones.
    fn authorize(&self, extra: &[(&str, &str)]) -> String {
        let resource = format!("{}/mcp", self.server.base);
        let query = form_urlencoded::Serializer::new(String::new())
            .extend_pairs([
                ("response_type", "code"),
                ("client_id", self.client_id.as_str()),
                ("redirect_uri", self.redirect_uri.as_str()),
                ("state", "xyz"),
                // RFC 7636 appendix B's challenge: a published vector.
                (
                    "code_challenge",
                    "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM",
                ),
                ("code_challenge_method", "S256"),
                ("resource", resource.as_str()),
            ])
            .extend_pairs(extra)
            .finish();
        format!("{}/oauth/authorize?{query}", self.server.base)
    }

    /// The OAuth grants, as stored.
    async fn grants(&self) -> Vec<KeyRecord> {
        key_rows::list(&self.server.db.pool)
            .await
            .expect("list keys")
            .into_iter()
            .filter(|row| row.kind == KeyKind::OAuth)
            .collect()
    }
}

/// Waits for the client's page, `landing`, and checks the hand-off carried
/// a code and the state.
fn assert_back_at_the_client(ui: &Ui, landing: &str) {
    ui.wait_visible("#done");
    let href = ui.eval("() => location.href", serde_json::json!(null));
    let href = href.as_str().unwrap_or_default();
    assert!(href.starts_with(landing), "back at the client: {href}");
    assert!(href.contains("code=okfac_"), "with a code: {href}");
    assert!(href.contains("state=xyz"), "with the state: {href}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_person_enters_a_code_approves_and_lands_back_at_the_client() {
    approve_with_a_code(Journey::start().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn approve_reaches_a_client_whose_redirect_uri_redirects_to_another_origin() {
    let journey = Journey::start_via(true).await;
    assert_ne!(journey.redirect_uri, journey.landing, "a hop in between");
    approve_with_a_code(journey).await;
}

/// The sign-in code path through Approve, then the grant it made.
async fn approve_with_a_code(journey: Journey) {
    let catalogs = ["main".to_string()];
    let code = code_rows::create(
        &journey.server.db.pool,
        &NewSignInCode {
            name: "journey",
            access: Access::Write,
            grants: Grants::Catalogs(&catalogs),
            default_catalog: Some("main"),
            actor: "example-actor",
            expires_at: Utc::now() + TimeDelta::hours(1),
            user: crate::support::server::OWNER,
        },
        &okf_mcp::store::audit::AuditActor::cli(),
        Utc::now(),
    )
    .await
    .expect("create a sign-in code")
    .code;
    // The admin-minted sign-in code path, with no session.
    let authorize = journey.authorize(&[("signin", "code")]);
    let landing = journey.landing.clone();

    let ran = drive(journey.server.base.clone(), move |ui| {
        ui.page
            .goto(&authorize, GotoOptions::default())
            .expect("open the consent page");
        ui.wait_text("#consent-client", "Example Client");
        ui.fill("#sign-in-code", &code);
        ui.click("#consent-continue");
        ui.wait_text("#consent-summary", "write");
        assert!(ui.text("#consent-summary").contains("example-actor"));
        ui.click("#consent-approve");
        assert_back_at_the_client(ui, &landing);
    })
    .await;
    if !ran {
        return;
    }
    // The approval made one OAuth grant, now in the server's key cache.
    let grants = journey.grants().await;
    assert_eq!(grants.len(), 1);
    assert_eq!(
        grants[0].client_id.as_deref(),
        Some(journey.client_id.as_str())
    );
    assert!(!grants[0].self_issued, "a code's grant is admin-issued");
    assert!(journey
        .server
        .app
        .find_grant(&grants[0].id)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_person_signs_in_on_the_way_and_approves_from_their_ceiling() {
    let journey = Journey::start().await;
    let pool = &journey.server.db.pool;
    journey
        .server
        .user_with_password("alice", "alice@example.com", PASSWORD)
        .await;
    for (catalog, access) in [("main", Access::Write), ("other", Access::Read)] {
        ceiling::entitle(
            pool,
            &NewEntitlement {
                user: "alice",
                catalog,
                access,
            },
            &AuditActor::cli(),
            Utc::now(),
        )
        .await
        .expect("entitle");
    }
    // Labels: a display name for alice and for `main`; the page must show
    // them, not the slugs. A bump of the generation reloads the directory.
    sqlx::query("UPDATE users SET display_name = 'Alice Example' WHERE name = 'alice'")
        .execute(pool)
        .await
        .expect("user display name");
    sqlx::query("UPDATE catalogs SET display_name = 'Acme Handbook' WHERE name = 'main'")
        .execute(pool)
        .await
        .expect("catalog display name");
    // Raw updates send no change message: catch up as a reconnect does.
    journey.server.app.apply_reconnect().await;
    let authorize = journey.authorize(&[]);
    let landing = journey.landing.clone();

    let ran = drive(journey.server.base.clone(), move |ui| {
        ui.page
            .goto(&authorize, GotoOptions::default())
            .expect("open the authorize URL");
        // No session yet: the sign-in page, offering the code path too.
        ui.wait_visible("#signin-form");
        ui.wait_visible("#signin-use-code");
        ui.fill("#signin-email", "alice@example.com");
        ui.fill("#signin-password", PASSWORD);
        ui.click("#signin-submit");
        // Signed in, back at the request: alice's own ceiling.
        ui.wait_text("#consent-user", "Alice Example");
        ui.wait_text("#consent-client", "Example Client");
        // The catalog shows its label; the slug stays in the control's id.
        assert!(ui.text("#consent-catalogs").contains("Acme Handbook"));
        ui.click("#consent-catalog-main");
        ui.select("#consent-level-main", "write");
        ui.select("#consent-default", "main");
        ui.click("#consent-approve");
        assert_back_at_the_client(ui, &landing);
    })
    .await;
    if !ran {
        return;
    }
    // One self-issued grant of alice's: write on main only, now cached.
    let grants = journey.grants().await;
    assert_eq!(grants.len(), 1);
    let grant = &grants[0];
    assert!(grant.self_issued);
    assert_eq!(grant.user.as_deref(), Some("alice"));
    assert_eq!(grant.client_id.as_deref(), Some(journey.client_id.as_str()));
    assert_eq!(
        grant.catalog_access,
        [("main".to_string(), Access::Write)].into_iter().collect()
    );
    assert_eq!(grant.default_catalog.as_deref(), Some("main"));
    let principal = journey
        .server
        .app
        .find_grant(&grant.id)
        .await
        .unwrap()
        .expect("cached grant");
    assert_eq!(principal.level("main"), Some(Access::Write));
    assert_eq!(principal.level("other"), None);
}

/// Whether the consent page's Advanced Options disclosure is open now.
fn disclosure_open(ui: &Ui) -> bool {
    ui.eval(
        "() => document.querySelector('#consent-advanced').open",
        serde_json::json!(null),
    ) == serde_json::json!(true)
}

/// Whether `selector` is rendered now, by `Element.checkVisibility`, which
/// counts content a closed `<details>` hides (unlike a bounding box).
fn rendered(ui: &Ui, selector: &str) -> bool {
    ui.eval(
        "(selector) => document.querySelector(selector).checkVisibility()",
        serde_json::json!(selector),
    ) == serde_json::json!(true)
}

/// Signs alice in on the way to a consent page that offers only `main`, at
/// `access`, runs `choose` there, approves, and returns the grant (or
/// `None` when the browser could not start).
async fn approve_the_only_catalog(
    access: Access,
    choose: impl FnOnce(&Ui) + Send + 'static,
) -> Option<KeyRecord> {
    let journey = Journey::start().await;
    journey
        .server
        .user_with_password("alice", "alice@example.com", PASSWORD)
        .await;
    ceiling::entitle(
        &journey.server.db.pool,
        &NewEntitlement {
            user: "alice",
            catalog: "main",
            access,
        },
        &AuditActor::cli(),
        Utc::now(),
    )
    .await
    .expect("entitle");
    let authorize = journey.authorize(&[]);
    let landing = journey.landing.clone();

    let ran = drive(journey.server.base.clone(), move |ui| {
        ui.page
            .goto(&authorize, GotoOptions::default())
            .expect("open the authorize URL");
        ui.wait_visible("#signin-form");
        ui.fill("#signin-email", "alice@example.com");
        ui.fill("#signin-password", PASSWORD);
        ui.click("#signin-submit");
        ui.wait_visible("#consent-approve");
        // One catalog: Advanced Options starts closed, its controls not
        // rendered. `wait_hidden` cannot tell: Chromium hides a closed
        // `<details>`'s content with `content-visibility`, and a forced
        // layout still gives it a box, so ask `checkVisibility` instead.
        ui.wait_visible("#consent-advanced-toggle");
        ui.wait_visible("#consent-preset");
        assert!(!disclosure_open(ui), "Advanced Options starts closed");
        assert!(!rendered(ui, "#consent-catalog-main"));
        assert!(!rendered(ui, "#consent-default"));
        choose(ui);
        ui.click("#consent-approve");
        assert_back_at_the_client(ui, &landing);
    })
    .await;
    if !ran {
        return None;
    }
    let grants = journey.grants().await;
    assert_eq!(grants.len(), 1);
    Some(grants[0].clone())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_only_catalogs_collapsed_defaults_approve_write_as_the_default() {
    let Some(grant) = approve_the_only_catalog(Access::Write, |_| {}).await else {
        return;
    };
    assert!(grant.self_issued);
    assert_eq!(
        grant.catalog_access,
        [("main".to_string(), Access::Write)].into_iter().collect()
    );
    assert_eq!(grant.default_catalog.as_deref(), Some("main"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn advanced_options_open_to_change_the_only_catalogs_defaults() {
    let Some(grant) = approve_the_only_catalog(Access::Write, |ui| {
        ui.click("#consent-advanced-toggle");
        assert!(disclosure_open(ui), "the summary opens Advanced Options");
        assert!(rendered(ui, "#consent-catalog-main"));
        assert!(rendered(ui, "#consent-default"));
        ui.select("#consent-level-main", "read");
        ui.select("#consent-default", "");
    })
    .await
    else {
        return;
    };
    assert_eq!(
        grant.catalog_access,
        [("main".to_string(), Access::Read)].into_iter().collect()
    );
    assert_eq!(grant.default_catalog, None);
}

/// Issue #736: the signed-in page offers the ceiling a page at a time. A
/// person ticks `main`, finds `other` by its name's start (Find, a plain
/// form post), sees `main` still ticked at write, ticks `other` too, and
/// approves one grant with both.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_person_finds_another_catalog_and_approves_both() {
    let journey = Journey::start().await;
    let pool = &journey.server.db.pool;
    journey
        .server
        .user_with_password("alice", "alice@example.com", PASSWORD)
        .await;
    for (catalog, access) in [("main", Access::Write), ("other", Access::Read)] {
        ceiling::entitle(
            pool,
            &NewEntitlement {
                user: "alice",
                catalog,
                access,
            },
            &AuditActor::cli(),
            Utc::now(),
        )
        .await
        .expect("entitle");
    }
    let authorize = journey.authorize(&[]);
    let landing = journey.landing.clone();

    let ran = drive(journey.server.base.clone(), move |ui| {
        ui.page
            .goto(&authorize, GotoOptions::default())
            .expect("open the authorize URL");
        ui.wait_visible("#signin-form");
        ui.fill("#signin-email", "alice@example.com");
        ui.fill("#signin-password", PASSWORD);
        ui.click("#signin-submit");
        ui.wait_visible("#consent-find");
        ui.click("#consent-catalog-main");
        ui.select("#consent-level-main", "write");
        ui.fill("#consent-find", "OT");
        ui.click("#consent-find-go");
        // The found page (its Find box holds the prefix, lowercased):
        // `other`, and `main` carried over, still ticked.
        ui.wait_visible("#consent-find[value=ot]");
        assert_eq!(
            ui.eval(
                "() => [document.querySelector('#consent-find').value, \
                 document.querySelector('#consent-catalog-main').checked, \
                 document.querySelector('#consent-level-main').value, \
                 document.querySelector('#consent-catalog-other').checked]",
                serde_json::json!(null)
            ),
            serde_json::json!(["ot", true, "write", false])
        );
        ui.click("#consent-catalog-other");
        ui.select("#consent-default", "other");
        ui.click("#consent-approve");
        assert_back_at_the_client(ui, &landing);
    })
    .await;
    if !ran {
        return;
    }
    let grants = journey.grants().await;
    assert_eq!(grants.len(), 1);
    assert_eq!(
        grants[0].catalog_access,
        [
            ("main".to_string(), Access::Write),
            ("other".to_string(), Access::Read),
        ]
        .into_iter()
        .collect()
    );
    assert_eq!(grants[0].default_catalog.as_deref(), Some("other"));
}
