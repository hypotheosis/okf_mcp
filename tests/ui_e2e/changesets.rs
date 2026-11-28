//! The Changesets page: an admin finds a changeset an agent left open,
//! reads its diff, and publishes or discards it.

use chrono::Utc;
use okf_mcp::config::Config;
use okf_mcp::labels::DisplayName;
use okf_mcp::store::audit::AuditActor;
use okf_mcp::store::keys::Access;
use okf_mcp::store::users::{self, NewUser};
use serde_json::json;

use crate::support::server::{call, files, open_changeset, structured, Client, TestServer};
use crate::ui_e2e::harness::drive;

const ALPHA: &str = "---\ntype: system\ntitle: Alpha\n---\n# Alpha\n\nAlpha runs.\n";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_admin_discards_a_changeset_from_the_console() {
    let server = TestServer::start(
        vec![(
            "main",
            files(&[
                ("index.md", "# Main\n\n- [Alpha](systems/alpha.md)\n"),
                ("systems/alpha.md", ALPHA),
            ]),
        )],
        Config::default(),
    )
    .await;
    let key = server
        .key("console-admin", Access::Admin, &["main"], Some("main"))
        .await;
    // An agent's unfinished work: one edit in an open changeset.
    let client = server.client(&key, None).await.expect("connect");
    let handle = open_changeset(&client, Some("main")).await;
    let edited = call(
        &client,
        "edit_document",
        json!({
            "path": "/systems/alpha.md", "old_str": "Alpha runs.", "new_str": "Alpha runs well.",
            "changeset": handle,
        }),
    )
    .await;
    structured(&edited);

    let shown = handle.clone();
    let ran = drive(server.base.clone(), move |ui| {
        ui.login(&key);
        ui.open("/changesets");
        ui.wait_text("#changesets-table", &shown);
        ui.click("[data-test=changeset-handle]");
        ui.wait_text("#changeset-diff", "+Alpha runs well.");
        // Cancelling the confirmation sends nothing: the drawer stays open,
        // and the discard below succeeds (a discarded handle would be 409).
        ui.click("#changeset-discard");
        ui.click("#confirm-cancel");
        ui.wait_hidden("#confirm-cancel");
        ui.wait_visible("#changeset-discard");
        assert!(ui.text("#changeset-diff").contains("+Alpha runs well."));
        ui.click("#changeset-discard");
        ui.click("#confirm-ok");
        ui.wait_text("#changesets-table", "No open changesets");
        ui.wait_hidden("#drawer");
    })
    .await;
    if !ran {
        // Skipped (no Chromium): the changeset is still open, as it should be.
        return;
    }

    // The server agrees: nothing is open any more.
    let listed = call(&client, "list_changesets", json!({})).await;
    assert_eq!(structured(&listed)["changesets"], json!([]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_changesets_table_shows_the_catalog_and_actor_labels() {
    let server = TestServer::start(
        vec![(
            "main",
            files(&[
                ("index.md", "# Main\n\n- [Alpha](systems/alpha.md)\n"),
                ("systems/alpha.md", ALPHA),
            ]),
        )],
        Config::default(),
    )
    .await;
    // A key whose actor is a user's slug: its changesets carry that user's label.
    let key = server
        .key("alice", Access::Admin, &["main"], Some("main"))
        .await;
    users::create(
        &server.db.pool,
        &NewUser {
            name: "alice",
            display_name: &DisplayName::parse("Alice Example").expect("a display name"),
        },
        &AuditActor::cli(),
        Utc::now(),
    )
    .await
    .expect("create alice");
    server.set_catalog_label("main", "Work notes").await;
    let client = server.client(&key, None).await.expect("connect");
    let handle = open_changeset(&client, Some("main")).await;
    let edited = call(
        &client,
        "edit_document",
        json!({
            "path": "/systems/alpha.md", "old_str": "Alpha runs.", "new_str": "Alpha runs well.",
            "changeset": handle,
        }),
    )
    .await;
    structured(&edited);

    drive(server.base.clone(), move |ui| {
        ui.login(&key);
        ui.open("/changesets");
        ui.wait_text("#changesets-table", "Work notes");
        let table = ui.text("#changesets-table");
        // The label first, the slug beneath it; the actor by its label.
        assert!(table.contains("Alice Example"), "{table}");
        assert!(table.contains("main"), "{table}");
        ui.click("[data-test=changeset-handle]");
        ui.wait_text("#drawer", "Work notes - alice (Alice Example)");
    })
    .await;
}

/// An agent's key opens a changeset in `main` and edits Alpha in it.
///
/// # Returns
/// The agent's MCP client and the handle.
async fn agent_edit(server: &TestServer, agent: &str, new: &str) -> (Client, String) {
    let client = server.client(agent, None).await.expect("connect");
    let handle = open_changeset(&client, Some("main")).await;
    let edited = call(
        &client,
        "edit_document",
        json!({
            "path": "/systems/alpha.md", "old_str": "Alpha runs.", "new_str": new,
            "changeset": handle,
        }),
    )
    .await;
    structured(&edited);
    (client, handle)
}

/// A server with `main` (an index and Alpha), an agent's write key and the
/// console's admin key.
async fn publish_server() -> (TestServer, String, String) {
    let server = TestServer::start(
        vec![(
            "main",
            files(&[
                ("index.md", "# Main\n\n- [Alpha](systems/alpha.md)\n"),
                ("systems/alpha.md", ALPHA),
            ]),
        )],
        Config::default(),
    )
    .await;
    let agent = server
        .key("agent", Access::Write, &["main"], Some("main"))
        .await;
    let admin = server
        .key("console-admin", Access::Admin, &["main"], Some("main"))
        .await;
    (server, agent, admin)
}

/// Opens the drawer of the changeset `handle` from the table.
fn click_handle(ui: &crate::ui_e2e::harness::Ui, handle: &str) {
    let clicked = ui.eval(
        "(handle) => { const cell = [...document.querySelectorAll('[data-test=changeset-handle]')] \
         .find(el => el.textContent === handle); if (cell) cell.click(); return !!cell; }",
        json!(handle),
    );
    assert_eq!(clicked, json!(true), "no row for {handle}");
}

/// The (actor, message) of every revision of Alpha, oldest first.
async fn alpha_revisions(server: &TestServer) -> Vec<(String, String)> {
    sqlx::query_as(
        "SELECT actor, COALESCE(message, '') FROM catalog.revisions \
         WHERE path = 'systems/alpha.md' ORDER BY revision",
    )
    .fetch_all(&server.db.store())
    .await
    .expect("read Alpha's revisions")
}

/// An admin reviews an agent's changeset and publishes it from the
/// console: a blank message cannot be sent, Cancel sends nothing, a double
/// click (and a drawer reopened meanwhile) sends one publish, and the
/// revision carries the message and the admin as its actor.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_admin_reviews_and_publishes_an_agents_changeset() {
    let (server, agent, admin) = publish_server().await;
    let (client, handle) = agent_edit(&server, &agent, "Alpha runs well.").await;

    let shown = handle.clone();
    let ran = drive(server.base.clone(), move |ui| {
        ui.login(&admin);
        ui.open("/changesets");
        ui.wait_text("#changesets-table", &shown);
        ui.click("[data-test=changeset-handle]");
        ui.wait_text("#changeset-diff", "+Alpha runs well.");
        // No message, or only whitespace: Publish stays disabled, and a
        // submit anyway says why and sends nothing.
        let disabled = "(selector) => document.querySelector(selector).disabled";
        assert_eq!(ui.eval(disabled, json!("#changeset-publish")), json!(true));
        ui.fill("#changeset-message", "   ");
        assert_eq!(ui.eval(disabled, json!("#changeset-publish")), json!(true));
        ui.eval(
            "() => document.querySelector('#changeset-publish-form').requestSubmit()",
            json!(null),
        );
        ui.wait_visible("#changeset-publish-error");
        ui.wait_text("#changeset-publish-error", "Enter a message");
        // Cancelling the confirmation sends nothing and keeps the message.
        ui.fill("#changeset-message", "Reviewed: Alpha runs well");
        ui.click("#changeset-publish");
        ui.wait_text("dialog.confirm", "Reviewed: Alpha runs well");
        ui.click("#confirm-cancel");
        ui.wait_hidden("#confirm-cancel");
        ui.wait_visible("#changeset-publish:not([disabled])");
        let kept = ui.eval(
            "() => document.querySelector('#changeset-message').value",
            json!(null),
        );
        assert_eq!(kept, json!("Reviewed: Alpha runs well"));
        assert!(ui.text("#changeset-diff").contains("+Alpha runs well."));
        // Confirmed, the publish is held: a double click sends nothing more,
        // and the drawer opened again meanwhile draws Publish and Discard
        // disabled.
        ui.hold_next("POST", "/publish");
        ui.click("#changeset-publish");
        ui.click("#confirm-ok");
        assert!(
            ui.click_twice("#changeset-publish"),
            "Publish stayed enabled while its write ran"
        );
        ui.mark("#changeset-publish");
        ui.click("#drawer-close");
        ui.click("[data-test=changeset-handle]");
        ui.wait_redrawn("#changeset-publish");
        ui.fill("#changeset-message", "A second publish");
        ui.wait_visible("#changeset-publish[disabled]");
        ui.wait_visible("#changeset-discard[disabled]");
        assert_eq!(ui.held_matches(), 1, "a double click sent two publishes");
        ui.release_held();
        // The publish closes the drawer showing its handle, even the
        // reopened one, and the list no longer has it.
        ui.wait_hidden("#drawer");
        ui.wait_text("#changesets-table", "No open changesets");
        ui.wait_visible("#changesets-result");
        ui.wait_text("#changesets-result", "1 revision");
        ui.wait_text(
            "#changesets-result",
            "update /systems/alpha.md (revision 2)",
        );
    })
    .await;
    if !ran {
        return;
    }

    let listed = call(&client, "list_changesets", json!({})).await;
    assert_eq!(structured(&listed)["changesets"], json!([]));
    let read = call(&client, "get_concept", json!({"path": "/systems/alpha.md"})).await;
    let body = structured(&read)["body"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(body.contains("Alpha runs well."), "{body}");
    let revisions = alpha_revisions(&server).await;
    assert_eq!(
        revisions.last(),
        Some(&(
            "console-admin".to_string(),
            "Reviewed: Alpha runs well".to_string()
        )),
        "{revisions:?}"
    );
    assert_eq!(revisions.len(), 2, "one publish: {revisions:?}");
}

/// A publish that conflicts shows the conflicting file and keeps the
/// message, the drawer and the changeset; an empty changeset reports
/// nothing to publish and stays open.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_publish_keeps_the_message_and_the_changeset() {
    let (server, agent, admin) = publish_server().await;
    let (client, stale) = agent_edit(&server, &agent, "Alpha runs well.").await;
    // Another changeset publishes Alpha first, so `stale` conflicts.
    let (_, first) = agent_edit(&server, &agent, "Alpha runs fast.").await;
    let published = call(
        &client,
        "publish",
        json!({"message": "Faster", "changeset": first}),
    )
    .await;
    assert_eq!(structured(&published)["published"], true);
    let empty = open_changeset(&client, Some("main")).await;

    let (stale_shown, empty_shown) = (stale.clone(), empty.clone());
    let ran = drive(server.base.clone(), move |ui| {
        ui.login(&admin);
        ui.open("/changesets");
        ui.wait_text("#changesets-table", &stale_shown);
        ui.wait_text("#changesets-table", &empty_shown);
        click_handle(ui, &stale_shown);
        ui.wait_text("#changeset-diff", "+Alpha runs well.");
        ui.fill("#changeset-message", "Reviewed: Alpha runs well");
        ui.click("#changeset-publish");
        ui.click("#confirm-ok");
        ui.wait_visible("#changeset-publish-error");
        ui.wait_text("#changeset-publish-error", "Not published");
        ui.wait_text(
            "[data-test=publish-refusal-details]",
            "/systems/alpha.md: changed since this changeset read it",
        );
        ui.wait_visible("#drawer");
        ui.wait_visible("#changeset-publish:not([disabled])");
        let kept = ui.eval(
            "() => document.querySelector('#changeset-message').value",
            json!(null),
        );
        assert_eq!(kept, json!("Reviewed: Alpha runs well"));
        assert!(ui.text("#changesets-table").contains(&stale_shown));
        // The empty changeset: nothing to publish, and it stays open.
        ui.click("#drawer-close");
        click_handle(ui, &empty_shown);
        ui.wait_text("#changeset-diff", "No pending files");
        ui.fill("#changeset-message", "Nothing");
        ui.click("#changeset-publish");
        ui.click("#confirm-ok");
        ui.wait_visible("#changeset-publish-error");
        ui.wait_text("#changeset-publish-error", "Nothing to publish");
        ui.wait_visible("#drawer");
        assert!(ui.text("#changesets-table").contains(&empty_shown));
        assert!(!ui.text("#changesets-result").contains("Published"));
    })
    .await;
    if !ran {
        return;
    }

    // Both are still open, and Alpha has only the agent's publish.
    let listed = call(&client, "list_changesets", json!({})).await;
    let open: Vec<&str> = structured(&listed)["changesets"]
        .as_array()
        .expect("a list")
        .iter()
        .filter_map(|row| row["changeset"].as_str())
        .collect();
    assert!(open.contains(&stale.as_str()), "{open:?}");
    assert!(open.contains(&empty.as_str()), "{open:?}");
    let revisions = alpha_revisions(&server).await;
    assert_eq!(
        revisions.last(),
        Some(&("agent".to_string(), "Faster".to_string()))
    );
}

/// Issue #665: the changesets list read after a Discard is sent at once but
/// answers late, listing the second changeset; the read after the second
/// Discard answers first and lists none. The older answer, released now,
/// must not bring the discarded changeset back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_older_list_read_that_answers_late_never_redraws_the_changesets_table() {
    let (server, agent, admin) = publish_server().await;
    let (_first_client, first) = agent_edit(&server, &agent, "Alpha runs first.").await;
    let (_second_client, second) = agent_edit(&server, &agent, "Alpha runs second.").await;
    drive(server.base.clone(), move |ui| {
        ui.login(&admin);
        ui.open("/changesets");
        ui.wait_text("#changesets-table", &first);
        ui.wait_text("#changesets-table", &second);
        click_handle(ui, &first);
        ui.wait_text("#changeset-diff", "+Alpha runs first.");
        // The read after discarding the first answers late: the server's
        // answer still lists the second changeset.
        ui.hold_answer("GET", "/api/v1/changesets?catalog=main", || {
            ui.click("#changeset-discard");
            ui.click("#confirm-ok");
        });
        // The second Discard's read answers first and lists nothing.
        click_handle(ui, &second);
        ui.wait_text("#changeset-diff", "+Alpha runs second.");
        ui.click("#changeset-discard");
        ui.click("#confirm-ok");
        ui.wait_text("#changesets-table", "No open changesets");
        // The older answer must not replace the newer table.
        ui.release_held();
        let table = ui.text("#changesets-table");
        assert!(
            table.contains("No open changesets") && !table.contains(&second),
            "the older read redrew the changesets table: {table}"
        );
    })
    .await;
}
