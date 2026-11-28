//! The console's help entries (issue #793): the "Documentation" link
//! (`docs_url`) for everyone signed in, admin or not, by key or account;
//! and the feedback entry, which is the in-console form (with `[feedback]`)
//! for someone signed in with an account, else the external "Send
//! feedback" link (`feedback_url`), and never both. The "Billing" link
//! (`billing_url`, issue #988) is for people signed in with an account only.

use chrono::Utc;
use okf_mcp::config::{AuthConfig, Config, FeedbackConfig};
use okf_mcp::store::audit::AuditActor;
use okf_mcp::store::keys::Access;
use okf_mcp::store::users::{self, AdminChange};
use serde_json::json;

use crate::support::server::{files, TestServer};
use crate::ui_e2e::harness::{drive, Ui};
use crate::ui_e2e::self_service::{console_base, login_server_with, member, EMAIL, PASSWORD};

/// The documentation site every journey here configures.
const DOCS: &str = "https://docs.example.com/";

/// The external feedback page the journeys with both entries configure.
const FEEDBACK_PAGE: &str = "https://example.com/feedback";

/// The billing service start page every journey with the billing link configures.
const BILLING: &str = "https://billing.example.com/login";

/// The `[auth]` branding these journeys use: a documentation link, a
/// billing link and an external feedback page.
fn branded() -> AuthConfig {
    AuthConfig {
        docs_url: Some(DOCS.to_string()),
        billing_url: Some(BILLING.to_string()),
        feedback_url: Some(FEEDBACK_PAGE.to_string()),
        ..AuthConfig::default()
    }
}

/// A login server (as `login_server`) with [`branded`] and a `[feedback]`
/// table. Nothing runs a feedback worker, so submissions stay queued.
async fn feedback_server() -> TestServer {
    TestServer::start_with(
        vec![
            ("main", files(&[("index.md", "# Main\n")])),
            ("other", files(&[("index.md", "# Other\n")])),
        ],
        |base| Config {
            public_url: Some(base.replace("127.0.0.1", "localhost")),
            auth: branded(),
            feedback: Some(FeedbackConfig::for_form("form-placeholder_1")),
            ..Config::default()
        },
    )
    .await
}

/// Waits until the shell knows both who is signed in and the brand, so a
/// hidden entry is hidden for good, not only not drawn yet.
fn wait_settled(ui: &Ui) {
    ui.wait_text("#whoami", " - ");
    ui.wait_visible("#app[data-brand-loaded=true]");
}

/// The documentation link's `[href, target, rel]`.
fn docs_link(ui: &Ui) -> serde_json::Value {
    ui.eval(
        "() => { const a = document.getElementById('docs-link'); \
         return [a.getAttribute('href'), a.target, a.rel]; }",
        json!(null),
    )
}

/// The billing link's `[href, target, rel]`.
fn billing_link(ui: &Ui) -> serde_json::Value {
    ui.eval(
        "() => { const a = document.getElementById('billing-link'); \
         return [a.getAttribute('href'), a.target, a.rel]; }",
        json!(null),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_billing_link_shows_for_a_member_and_not_for_a_key() {
    let server = feedback_server().await;
    member(&server, Access::Read).await;
    let key = server
        .key("console-reader", Access::Read, &["main"], Some("main"))
        .await;

    drive(console_base(&server), |ui| {
        // An ordinary member: no Administer group, but the link, safe to open.
        ui.sign_in_password(EMAIL, PASSWORD);
        wait_settled(ui);
        ui.wait_hidden("[data-need=admin]");
        ui.wait_visible("#billing-link");
        ui.wait_text("#billing-link", "Billing");
        assert_eq!(
            billing_link(ui),
            json!([BILLING, "_blank", "noopener noreferrer"])
        );
        // It sits in "Your account" and stays on every page.
        ui.click("a[data-nav=account]");
        ui.wait_visible("#main .view[data-loaded=true]");
        ui.wait_visible("#billing-link");
        ui.click("#signout");
        ui.wait_visible("#login");
        // Signed out: no stale link.
        ui.wait_hidden("#billing-link");
    })
    .await;
    drive(console_base(&server), move |ui| {
        // An API key alone has no account: no link, and no href kept.
        ui.login(&key);
        wait_settled(ui);
        ui.wait_visible("#docs-link");
        ui.wait_hidden("#billing-link");
        assert_eq!(
            ui.eval(
                "() => document.getElementById('billing-link').hasAttribute('href')",
                json!(null)
            ),
            json!(false)
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_documentation_link_shows_for_members_admins_and_keys() {
    let server = feedback_server().await;
    member(&server, Access::Read).await;
    server
        .user_with_password("bob", "bob@example.com", PASSWORD)
        .await;
    crate::support::server::applied(&server.app, async {
        users::set_admin(
            &server.db.pool,
            "bob",
            AdminChange {
                on: true,
                allow_last: false,
            },
            &AuditActor::cli(),
            Utc::now(),
        )
        .await
        .expect("make bob an admin");
    })
    .await;
    let key = server
        .key("console-reader", Access::Read, &["main"], Some("main"))
        .await;

    drive(console_base(&server), |ui| {
        // A member: no Administer group, but the link.
        ui.sign_in_password(EMAIL, PASSWORD);
        wait_settled(ui);
        ui.wait_hidden("[data-need=admin]");
        ui.wait_visible("#docs-link");
        ui.wait_text("#docs-link", "Documentation");
        assert_eq!(docs_link(ui), json!([DOCS, "_blank", "noopener"]));
        // It is in the shell, so it stays on every page.
        ui.click("a[data-nav=account]");
        ui.wait_visible("#main .view[data-loaded=true]");
        ui.wait_visible("#docs-link");
        ui.click("#signout");
        ui.wait_visible("#login");
    })
    .await;
    drive(console_base(&server), |ui| {
        // An admin user.
        ui.sign_in_password("bob@example.com", PASSWORD);
        wait_settled(ui);
        ui.wait_visible("[data-need=admin]");
        ui.wait_visible("#docs-link");
        assert_eq!(docs_link(ui), json!([DOCS, "_blank", "noopener"]));
    })
    .await;
    drive(console_base(&server), move |ui| {
        // A key.
        ui.login(&key);
        wait_settled(ui);
        ui.wait_visible("#docs-link");
        assert_eq!(docs_link(ui), json!([DOCS, "_blank", "noopener"]));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_no_help_configured_neither_entry_shows() {
    let server = login_server_with(AuthConfig::default()).await;
    member(&server, Access::Read).await;

    drive(console_base(&server), |ui| {
        ui.sign_in_password(EMAIL, PASSWORD);
        wait_settled(ui);
        ui.wait_hidden("#docs-link");
        ui.wait_hidden("#billing-link");
        ui.wait_hidden("#feedback-open");
        ui.wait_hidden("#feedback-link");
        assert_eq!(
            ui.eval(
                "() => ['docs-link', 'billing-link'] \
                 .map(id => document.getElementById(id).hasAttribute('href'))",
                json!(null)
            ),
            json!([false, false])
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_key_gets_the_feedback_link_since_the_form_needs_an_account() {
    let server = feedback_server().await;
    let key = server.key("console-admin", Access::Admin, &[], None).await;

    drive(console_base(&server), move |ui| {
        ui.login(&key);
        wait_settled(ui);
        ui.wait_visible("#feedback-link");
        ui.wait_hidden("#feedback-open");
        assert_eq!(
            ui.eval(
                "() => document.getElementById('feedback-link').getAttribute('href')",
                json!(null)
            ),
            json!(FEEDBACK_PAGE)
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_feedback_form_sends_once_and_shows_success_and_errors() {
    let server = feedback_server().await;
    member(&server, Access::Read).await;

    let ran = drive(console_base(&server), |ui| {
        ui.sign_in_password(EMAIL, PASSWORD);
        wait_settled(ui);
        // The form wins over the external link: one entry only.
        ui.wait_visible("#feedback-open");
        ui.wait_hidden("#feedback-link");
        ui.click("#feedback-open");
        ui.wait_visible("#feedback-form");
        ui.wait_text(
            "#feedback-disclosure",
            "The feedback goes to okf's team through a third-party form service (Forminit).",
        );
        ui.fill("#feedback-message", "The search page is great");
        ui.select("#feedback-kind", "praise");
        ui.select("#feedback-rating", "5");

        // Held: two clicks send one request, and a reopened form's Send
        // button is disabled until it settles.
        ui.hold_next("POST", "/api/v1/feedback");
        assert!(
            ui.click_twice("#feedback-send"),
            "Send is disabled while sending"
        );
        ui.mark("#feedback-send");
        // In the page: the drawer's scrim covers the sidebar.
        ui.eval(
            "() => document.getElementById('feedback-open').click()",
            json!(null),
        );
        ui.wait_redrawn("#feedback-send");
        assert!(
            ui.click_twice("#feedback-send"),
            "a reopened Send is disabled too"
        );
        assert_eq!(ui.held_matches(), 1);
        ui.release_held();
        // The reopened form was not the one that sent, so the toast says so.
        ui.wait_text("#toasts", "Your feedback was sent.");

        // From a settled form: the confirmation shows in the form.
        ui.fill("#feedback-message", "Second note");
        ui.click("#feedback-send");
        ui.wait_visible("#feedback-status[data-state=ok]");
        ui.wait_text("#feedback-status", "Thank you. Your feedback was sent.");
        assert_eq!(
            ui.eval(
                "() => document.getElementById('feedback-message').value",
                json!(null)
            ),
            json!("")
        );

        // A refusal shows its reason in the form, keeps the text, and leaves
        // Send usable.
        ui.fill("#feedback-message", "Third note");
        ui.fail_next(
            "POST",
            "/api/v1/feedback",
            422,
            "this credential has sent 10 feedback messages in the last hour",
        );
        ui.click("#feedback-send");
        ui.wait_visible("#feedback-status[data-state=error]");
        ui.wait_text("#feedback-status", "Not sent: this credential has sent 10");
        assert_eq!(
            ui.eval(
                "() => [document.getElementById('feedback-message').value, \
                 document.getElementById('feedback-send').disabled]",
                json!(null)
            ),
            json!(["Third note", false])
        );
        ui.assert_no_console_errors();
    })
    .await;
    if !ran {
        return;
    }
    let stored: Vec<(String, String, String, Option<i16>, String)> = sqlx::query_as(
        "SELECT message, kind, origin, rating, credential_kind FROM feedback \
         ORDER BY created_at, id",
    )
    .fetch_all(&server.db.pool)
    .await
    .expect("read the feedback");
    assert_eq!(
        stored,
        [
            (
                "The search page is great".to_string(),
                "praise".to_string(),
                "user".to_string(),
                Some(5),
                "session".to_string()
            ),
            // From the reopened form, at its defaults.
            (
                "Second note".to_string(),
                "other".to_string(),
                "user".to_string(),
                None,
                "session".to_string()
            ),
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_held_send_locks_the_fields_so_no_unsent_edit_is_lost() {
    let server = feedback_server().await;
    member(&server, Access::Read).await;
    // Whether each field is disabled, and whether the message can take the
    // focus (a person can only type into a focused field).
    const LOCKED: &str = "() => { \
        const fields = ['feedback-message', 'feedback-kind', 'feedback-rating'] \
            .map(id => document.getElementById(id)); \
        fields[0].focus(); \
        return [...fields.map(field => field.disabled), \
            document.activeElement === fields[0]]; }";

    let ran = drive(console_base(&server), |ui| {
        ui.sign_in_password(EMAIL, PASSWORD);
        wait_settled(ui);
        ui.wait_visible("#feedback-open");
        ui.click("#feedback-open");
        ui.wait_visible("#feedback-form");
        ui.fill("#feedback-message", "First note");

        ui.hold_next("POST", "/api/v1/feedback");
        ui.click("#feedback-send");
        // While the send is held, nothing can be edited (`once` disables
        // the guarded fields before its request starts).
        assert_eq!(
            ui.eval(LOCKED, json!(null)),
            json!([true, true, true, false])
        );
        assert_eq!(ui.held_matches(), 1);
        ui.release_held();

        // Settled: the sent text is cleared, and the fields are usable again.
        ui.wait_visible("#feedback-status[data-state=ok]");
        assert_eq!(
            ui.eval(LOCKED, json!(null)),
            json!([false, false, false, true])
        );
        assert_eq!(
            ui.eval(
                "() => document.getElementById('feedback-message').value",
                json!(null)
            ),
            json!("")
        );
        ui.assert_no_console_errors();
    })
    .await;
    if !ran {
        return;
    }
    let sent: Vec<String> = sqlx::query_scalar("SELECT message FROM feedback")
        .fetch_all(&server.db.pool)
        .await
        .expect("read the feedback");
    assert_eq!(sent, ["First note"]);
}
