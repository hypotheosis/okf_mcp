//! The Audit page (M14 Task 8c): an admin reads and filters everything and
//! exports it; a team owner picks one of its teams and sees operators as
//! "an operator"; and, per `tools/agentic/lessons.md`' read rule, only the
//! newest table load draws, "Load more" appends only to it, and a failed
//! load keeps its rows and its reason on screen.

use okf_mcp::config::Config;
use okf_mcp::store::keys::Access;
use serde_json::{json, Value};

use crate::support::server::{files, TestServer, OWNER};
use crate::ui_e2e::harness::{drive, Ui};
use crate::ui_e2e::self_service::{console_base, login_server, member, EMAIL, PASSWORD};

async fn server() -> TestServer {
    TestServer::start(
        vec![("main", files(&[("index.md", "# Main\n")]))],
        Config::default(),
    )
    .await
}

/// Sends one API request as `key` and fails the test unless it succeeds.
async fn call(server: &TestServer, key: &str, method: &str, path: &str, body: Value) {
    let response = reqwest::Client::new()
        .request(
            method.parse().expect("an HTTP method"),
            format!("{}{path}", server.base),
        )
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .expect("send the request");
    assert!(
        response.status().is_success(),
        "{method} {path}: {}",
        response.status()
    );
}

/// The records of a JSON Lines download, one per line.
fn json_lines(text: &str) -> Vec<Value> {
    let entries: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("each line is JSON"))
        .collect();
    assert!(!entries.is_empty(), "the export is empty");
    entries
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_admin_filters_the_log_by_action_and_exports_it() {
    let server = server().await;
    let admin = server.key("console-admin", Access::Admin, &[], None).await;
    drive(server.base.clone(), move |ui| {
        ui.login(&admin);
        ui.open("/users");
        ui.click("#user-new");
        ui.fill("#user-form input[name=name]", "alice");
        ui.fill("#user-form input[name=display_name]", "Alice Example");
        ui.click("#user-form button[type=submit]");
        ui.wait_visible("[data-test=user-alice]");
        ui.open("/audit");
        ui.wait_text("#audit-table", "user.created");
        ui.select("#audit-action", "user.created");
        // The person column shows the label, not the slug; the link keeps
        // the slug, which the filters match.
        ui.wait_text("#audit-table", "Alice Example");
        ui.wait_text_gone("#audit-table", "catalog.created");
        // The user links to its drawer, which lists its sign-ins.
        // A long label wraps in the narrow column, so the link's centre can
        // fall between its lines: click it from the page, not by a point.
        let clicked = ui.eval(
            "() => { const link = document.querySelector(\"#audit-table a[href='#/users/detail/alice']\"); link?.click(); return !!link; }",
            Value::Null,
        );
        assert_eq!(clicked, json!(true), "no link to alice");
        ui.wait_visible("#drawer");
        ui.wait_text("#drawer", "alice");
        ui.click("#drawer-close");
        ui.open("/audit");
        ui.select("#audit-action", "user.created");
        ui.wait_text_gone("#audit-table", "catalog.created");
        // The export downloads `audit.jsonl`, from a blob the page made of
        // its own fetch (and so with its credential), holding the entries
        // the filter selects.
        let (name, text) = ui.download("#audit-export");
        assert_eq!(name, "audit.jsonl");
        let entries = json_lines(&text);
        assert!(
            entries
                .iter()
                .all(|entry| entry["action"] == "user.created"),
            "the export ignored the action filter: {text}"
        );
        assert!(
            entries
                .iter()
                .any(|entry| entry["target"]["person"] == "alice"
                    && entry["target"]["person_label"] == "Alice Example"),
            "alice's user.created is not in the export, labelled: {text}"
        );
        ui.wait_hidden("#audit-error");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_user_named_unassigned_opens_its_own_drawer_from_the_log() {
    let server = server().await;
    let admin = server.key("console-admin", Access::Admin, &[], None).await;
    call(
        &server,
        &admin,
        "POST",
        "/api/v1/users",
        json!({"name": "unassigned", "display_name": "Unassigned Person"}),
    )
    .await;
    drive(server.base.clone(), move |ui| {
        ui.login(&admin);
        ui.open("/audit");
        ui.select("#audit-action", "user.created");
        ui.wait_text("#audit-table", "user Unassigned Person");
        // The link by its text (the label), whatever route it names.
        let clicked = ui.eval(
            "() => { const link = [...document.querySelectorAll('#audit-table a')] \
             .find(a => a.textContent === 'Unassigned Person'); link?.click(); return !!link; }",
            Value::Null,
        );
        assert_eq!(clicked, json!(true), "no link to the user unassigned");
        ui.wait_visible("#drawer");
        ui.wait_text("#drawer", "unassigned");
        ui.wait_visible("#users-table");
        // Not the view of what has no user or owner.
        assert_eq!(ui.text("#unassigned"), "");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_label_with_markup_is_drawn_as_text() {
    let server = server().await;
    let admin = server.key("console-admin", Access::Admin, &[], None).await;
    let hostile = "<img src=x id=pwned>";
    call(
        &server,
        &admin,
        "POST",
        "/api/v1/users",
        json!({"name": "mallory", "display_name": hostile}),
    )
    .await;
    drive(server.base.clone(), move |ui| {
        ui.login(&admin);
        ui.open("/audit");
        ui.select("#audit-action", "user.created");
        ui.wait_text("#audit-table", hostile);
        let injected = ui.eval(
            "() => document.querySelectorAll('#audit-table img, #pwned').length",
            Value::Null,
        );
        assert_eq!(injected, json!(0), "the label became markup");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_owner_reads_its_team_and_sees_an_operator() {
    // Signing in with a password needs the login server (a `localhost`
    // origin); alice is a member there with read on `main`.
    let server = login_server().await;
    member(&server, Access::Read).await;
    let root = server.key("console-admin", Access::Admin, &[], None).await;
    // An operator (an admin key) makes alice's team and adds a member.
    call(
        &server,
        &root,
        "POST",
        "/api/v1/teams",
        json!({"name": "family", "owner": "alice"}),
    )
    .await;
    call(
        &server,
        &root,
        "PUT",
        &format!("/api/v1/teams/family/members/{OWNER}"),
        json!({"role": "member"}),
    )
    .await;
    drive(console_base(&server), move |ui| {
        ui.sign_in_password(EMAIL, PASSWORD);
        ui.wait_visible("[data-nav=audit]");
        ui.open("/audit");
        ui.select("#audit-team", "family");
        ui.wait_text("#audit-table", "team.member_added");
        ui.wait_text("#audit-table", "an operator");
        ui.wait_text_gone("#audit-table", "console-admin");
        // The export downloads the owner's team's entries, as the owner
        // sees them.
        let (name, text) = ui.download("#audit-export");
        assert_eq!(name, "audit.jsonl");
        let entries = json_lines(&text);
        assert!(
            entries
                .iter()
                .any(|entry| entry["action"] == "team.member_added"),
            "the export misses the member added: {text}"
        );
        assert!(
            entries
                .iter()
                .all(|entry| entry["teams"] == json!(["family"])),
            "the export holds another team's entries: {text}"
        );
        assert!(
            !text.contains("console-admin"),
            "the export names staff: {text}"
        );
        ui.wait_hidden("#audit-error");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_owner_loads_more_through_its_sealed_cursor_and_sees_no_entry_id() {
    let server = login_server().await;
    member(&server, Access::Read).await;
    let root = server.key("console-admin", Access::Admin, &[], None).await;
    call(
        &server,
        &root,
        "POST",
        "/api/v1/teams",
        json!({"name": "family", "owner": "alice"}),
    )
    .await;
    // 101 members: 102 entries of family's, more than one page (100), with
    // each user's own creation between two of them.
    for index in 0..101 {
        let name = format!("kin-{index}");
        call(
            &server,
            &root,
            "POST",
            "/api/v1/users",
            json!({"name": name}),
        )
        .await;
        call(
            &server,
            &root,
            "PUT",
            &format!("/api/v1/teams/family/members/{name}"),
            json!({"role": "member"}),
        )
        .await;
    }
    drive(console_base(&server), move |ui| {
        ui.sign_in_password(EMAIL, PASSWORD);
        ui.wait_visible("[data-nav=audit]");
        ui.open("/audit");
        ui.select("#audit-team", "family");
        ui.wait_visible("#audit-more");
        assert_eq!(rows(ui), 100);
        // The owner's cursor is a sealed string, and the server takes no
        // other from an owner: the second page draws only if the console
        // passed it back as it came.
        ui.click("#audit-more");
        ui.wait_text("#audit-table", "team.created");
        ui.wait_hidden("#audit-more");
        assert_eq!(rows(ui), 102);
        ui.wait_hidden("#audit-error");
        let (_, text) = ui.download("#audit-export");
        let entries = json_lines(&text);
        assert_eq!(entries.len(), 102, "{text}");
        assert!(
            entries.iter().all(|entry| entry.get("id").is_none()),
            "an owner's export carries entry ids: {text}"
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_late_table_load_never_draws_over_a_newer_filter() {
    let server = server().await;
    let admin = server.key("console-admin", Access::Admin, &[], None).await;
    call(
        &server,
        &admin,
        "POST",
        "/api/v1/users",
        json!({"name": "alice"}),
    )
    .await;
    call(
        &server,
        &admin,
        "POST",
        "/api/v1/teams",
        json!({"name": "family", "owner": "alice"}),
    )
    .await;
    drive(server.base.clone(), move |ui| {
        ui.login(&admin);
        ui.open("/audit");
        ui.wait_text("#audit-table", "team.created");
        ui.wait_text("#audit-table", "user.created");
        // The first filter's answer is held; the second's arrives first,
        // and is drawn (the unfiltered rows' user.created is gone).
        ui.hold_answer("GET", "/api/v1/audit?action=user.created", || {
            ui.select("#audit-action", "user.created")
        });
        ui.select("#audit-action", "team.created");
        ui.wait_text_gone("#audit-table", "user.created");
        ui.wait_text("#audit-table", "family");
        ui.release_held();
        // release_held gave the page time to draw the late answer.
        let table = ui.text("#audit-table");
        assert!(
            table.contains("team.created") && !table.contains("user.created"),
            "the older filter's answer drew over the newer one: {table}"
        );
        // A failed load keeps the rows, marked, with its reason on screen,
        // and Refresh still works.
        ui.fail_next(
            "GET",
            "/api/v1/audit?action=team.created",
            503,
            "the log is away",
        );
        ui.click("#audit-refresh");
        ui.wait_visible("#audit-error");
        ui.wait_text("#audit-error", "the log is away");
        ui.wait_visible("#audit-stale");
        assert!(ui.text("#audit-table").contains("family"));
        ui.click("#audit-refresh");
        ui.wait_hidden("#audit-error");
        ui.wait_hidden("#audit-stale");
        // A refused export shows its reason and keeps the rows, which are
        // not out of date.
        ui.fail_next(
            "GET",
            "/api/v1/audit/export?action=team.created",
            403,
            "no export for you",
        );
        ui.click("#audit-export");
        ui.wait_visible("#audit-error");
        ui.wait_text("#audit-error", "no export for you");
        assert!(ui.text("#audit-table").contains("family"));
        assert_eq!(ui.text("#audit-stale"), "");
    })
    .await;
}

/// A server whose log holds 102 `user.created` entries, more than one page
/// (100): the fixture user (by the CLI, the oldest) and `user-0` to
/// `user-100` (by the admin key), plus a team `family`.
///
/// # Returns
/// The server and the admin key.
async fn two_pages() -> (TestServer, String) {
    let server = server().await;
    let admin = server.key("console-admin", Access::Admin, &[], None).await;
    for index in 0..101 {
        call(
            &server,
            &admin,
            "POST",
            "/api/v1/users",
            json!({"name": format!("user-{index}")}),
        )
        .await;
    }
    call(
        &server,
        &admin,
        "POST",
        "/api/v1/teams",
        json!({"name": "family", "owner": "user-0"}),
    )
    .await;
    (server, admin)
}

/// The number of rows `#audit-table` draws.
fn rows(ui: &Ui) -> u64 {
    ui.eval(
        "() => document.querySelectorAll('#audit-table tbody tr').length",
        Value::Null,
    )
    .as_u64()
    .unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_rows_drawn_keep_their_load_more_through_failed_and_overtaken_reads() {
    let (server, admin) = two_pages().await;
    drive(server.base.clone(), move |ui| {
        ui.login(&admin);
        ui.open("/audit");
        ui.select("#audit-action", "user.created");
        ui.wait_visible("#audit-more");
        assert_eq!(rows(ui), 100);
        // A failed Load more keeps the rows, its reason, the stale mark and
        // the button; the retry appends the page once and clears both.
        ui.fail_next("GET", "&action=user.created", 503, "page two is away");
        ui.click("#audit-more");
        ui.wait_text("#audit-error", "page two is away");
        ui.wait_visible("#audit-error");
        ui.wait_visible("#audit-stale");
        ui.wait_visible("#audit-more");
        assert_eq!(rows(ui), 100);
        ui.click("#audit-more");
        // The oldest entry, the fixture user's (by the CLI), is on page two.
        ui.wait_text("#audit-table", "cli:");
        ui.wait_hidden("#audit-error");
        ui.wait_hidden("#audit-stale");
        ui.wait_hidden("#audit-more");
        assert_eq!(rows(ui), 102);
        ui.click("#audit-refresh");
        ui.wait_text_gone("#audit-table", "cli:");
        ui.wait_visible("#audit-more");
        // Two Refreshes overlap: the newer fails, the older answers late
        // (and is dropped). The rows drawn keep their cursor.
        ui.hold_answer("GET", "/api/v1/audit?action=user.created", || {
            ui.click("#audit-refresh")
        });
        ui.fail_next("GET", "/api/v1/audit?action=user.created", 503, "away");
        ui.click("#audit-refresh");
        ui.wait_text("#audit-error", "away");
        ui.release_held();
        ui.wait_visible("#audit-more");
        // A Refresh overtakes a pending Load more and fails: the late page
        // is dropped, and Load more is still there.
        ui.hold_answer("GET", "&action=user.created", || ui.click("#audit-more"));
        ui.fail_next(
            "GET",
            "/api/v1/audit?action=user.created",
            503,
            "away again",
        );
        ui.click("#audit-refresh");
        ui.wait_text("#audit-error", "away again");
        ui.release_held();
        assert_eq!(rows(ui), 100, "an overtaken page was appended");
        ui.wait_visible("#audit-more");
        // And it still works: the next page, once.
        ui.click("#audit-more");
        ui.wait_text("#audit-table", "cli:");
        assert_eq!(rows(ui), 102);
        ui.wait_hidden("#audit-more");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn load_more_appends_only_to_the_newest_table_load() {
    let (server, admin) = two_pages().await;
    drive(server.base.clone(), move |ui| {
        ui.login(&admin);
        ui.open("/audit");
        ui.select("#audit-action", "user.created");
        ui.wait_visible("#audit-more");
        // A failed Refresh of the same filters keeps their Load more.
        ui.fail_next("GET", "/api/v1/audit?action=user.created", 503, "away");
        ui.click("#audit-refresh");
        ui.wait_visible("#audit-error");
        ui.wait_visible("#audit-stale");
        ui.wait_visible("#audit-more");
        // The second page's answer is held while the filter changes.
        ui.hold_answer("GET", "&action=user.created", || ui.click("#audit-more"));
        ui.select("#audit-action", "team.created");
        ui.wait_text("#audit-table", "family");
        ui.release_held();
        let table = ui.text("#audit-table");
        assert!(
            !table.contains("user.created"),
            "a page of the previous filter was appended: {table}"
        );
    })
    .await;
}
