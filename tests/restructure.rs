//! End-to-end tests of the restructuring tools (design "Testing"), over real
//! HTTP with rmcp's client:
//! - a move, published and exported;
//! - a move against a concurrent edit of a linking file (a conflict);
//! - a move against a concurrent `index` of the same folder (a replay);
//! - a delete refused while linked, then allowed;
//! - a delete that unlinks `log.md` (issue #658), alone and against a
//!   concurrent `log` entry in either order, and still refused for links
//!   outside `log.md`;
//! - a delete refused by a racing log link (issue #661), then retried.

use crate::support;

use okf_mcp::archive::{read_tarball, Files};
use okf_mcp::config::Config;
use okf_mcp::store::keys::Access;
use serde_json::{json, Value};
use support::server::{call, files, open_changeset, structured, Client, TestServer};

const TIME: &str = "---\ntype: System\ntitle: Time server\ndescription: The GPS clock.\n---\n\
                    # Time server\n\n## Ports\n\nUDP 123.\n";
const HUB: &str = "---\ntype: Note\ntitle: Hub\ndescription: Links.\n---\n\
                   See the [time server](/infra/time.md#ports).\n";

/// One catalog, `pg`: a root index, a folder with one concept, a hub linking
/// it, a log.
fn catalog() -> Vec<(&'static str, Files)> {
    vec![(
        "pg",
        files(&[
            (
                "index.md",
                "# PG\n\n# Sections\n\n* [Infra](/infra/index.md) - systems\n",
            ),
            (
                "infra/index.md",
                "# Infra\n\n* [Time server](time.md) - the GPS clock\n",
            ),
            ("infra/time.md", TIME),
            ("hub.md", HUB),
            ("log.md", "# Log\n"),
        ]),
    )]
}

/// A write key with `pg` as its default.
async fn writer(server: &TestServer, id: &str) -> String {
    server.key(id, Access::Write, &[], Some("pg")).await
}

/// Calls a tool that must succeed and returns its JSON.
async fn ok(client: &Client, tool: &'static str, arguments: Value) -> Value {
    structured(&call(client, tool, arguments).await).clone()
}

/// The `(path, op)` pairs of a publish's revisions, sorted.
fn revisions(published: &Value) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = published["revisions"]
        .as_array()
        .expect("a list of revisions")
        .iter()
        .map(|row| {
            let path = row["path"].as_str().expect("a path").to_string();
            (path, row["op"].as_str().expect("an op").to_string())
        })
        .collect();
    // Sorting makes the comparison independent of the order publish lists
    // them in. `sort_unstable` is `list.sort()` without keeping equal items'
    // order, which distinct paths do not need.
    pairs.sort_unstable();
    pairs
}

/// The move every test but the last makes, in the changeset `handle` names.
fn move_time(handle: &str) -> Value {
    json!({"old": "/infra/time", "new": "/infra/clock.md", "changeset": handle})
}

#[tokio::test(flavor = "multi_thread")]
async fn a_move_is_published_and_exported() {
    let server = TestServer::start(catalog(), Config::default()).await;
    let key = writer(&server, "writer").await;
    let client = server.client(&key, None).await.unwrap();
    let handle = open_changeset(&client, None).await;

    let moved = ok(&client, "move_concept", move_time(&handle)).await;
    let seen = ok(
        &client,
        "get_concept",
        json!({"path": "/infra/clock.md", "changeset": handle}),
    )
    .await;
    let published = ok(
        &client,
        "publish",
        json!({"message": "rename the time server", "changeset": handle}),
    )
    .await;
    let new_history = ok(&client, "history", json!({"path": "/infra/clock.md"})).await;
    let old_history = ok(&client, "history", json!({"path": "/infra/time.md"})).await;
    let response = reqwest::Client::new()
        .get(format!("{}/catalogs/pg/export", server.base))
        .bearer_auth(&key)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let tarball = response.bytes().await.unwrap();
    client.cancel().await.unwrap();

    assert_eq!(
        moved["touched"],
        json!([
            "/infra/time.md",
            "/infra/clock.md",
            "/hub.md",
            "/infra/index.md"
        ])
    );
    assert_eq!(seen["title"], "Time server");
    let expected = [
        ("/hub.md", "update"),
        ("/infra/clock.md", "create"),
        ("/infra/index.md", "update"),
        ("/infra/time.md", "delete"),
    ]
    // `[T; N]::map` builds a new fixed-size array (no `Vec` needed), owning
    // `String`s to compare with what `revisions` returns.
    .map(|(path, op)| (path.to_string(), op.to_string()));
    assert_eq!(revisions(&published), expected);
    assert_eq!(new_history["revisions"][0]["op"], "create");
    assert_eq!(old_history["revisions"][0]["op"], "delete");
    assert_eq!(status, reqwest::StatusCode::OK);
    let exported = read_tarball(&tarball[..], u64::MAX).unwrap();
    let text = |path: &str| {
        // `get` returns an `Option` (like `dict.get`), so a missing entry
        // fails with its path rather than a bare map-index panic.
        let file = exported
            .get(path)
            .unwrap_or_else(|| panic!("{path} missing from the export"));
        String::from_utf8(file.content.clone()).unwrap()
    };
    assert!(!exported.contains_key("infra/time.md"));
    assert_eq!(text("infra/clock.md"), TIME);
    assert!(text("hub.md").contains("[time server](/infra/clock.md#ports)"));
    assert!(text("infra/index.md").contains("[Time server](clock.md)"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_concurrent_edit_of_a_linking_file_conflicts() {
    let server = TestServer::start(catalog(), Config::default()).await;
    let mover_key = writer(&server, "writer-a").await;
    let editor_key = writer(&server, "writer-b").await;
    let mover = server.client(&mover_key, None).await.unwrap();
    let editor = server.client(&editor_key, None).await.unwrap();
    let (moving, editing) = (
        open_changeset(&mover, None).await,
        open_changeset(&editor, None).await,
    );
    ok(&mover, "move_concept", move_time(&moving)).await;
    let body = json!({
        "path": "/hub.md",
        "body": "See the [time server](/infra/time.md).\n",
        "changeset": editing,
    });
    ok(&editor, "replace_body", body).await;
    ok(
        &editor,
        "publish",
        json!({"message": "reword the hub", "changeset": editing}),
    )
    .await;

    let refused = call(
        &mover,
        "publish",
        json!({"message": "rename", "changeset": moving}),
    )
    .await;
    let pending = ok(&mover, "pending_changes", json!({"changeset": moving})).await;
    // Without a changeset, the editor reads the published catalog.
    let old = ok(&editor, "get_concept", json!({"path": "/infra/time.md"})).await;
    let new = call(&editor, "get_concept", json!({"path": "/infra/clock.md"})).await;
    mover.cancel().await.unwrap();
    editor.cancel().await.unwrap();

    assert_eq!(refused.is_error, Some(true));
    let body = refused.structured_content.expect("the error's JSON");
    assert_eq!(body["error"], "Conflict");
    assert_eq!(
        body["files"],
        json!([{"path": "/hub.md", "base_revision": 1, "current_revision": 2}])
    );
    // The whole move is still pending, its hub entry marked stale.
    let pending_files = pending["files"].as_array().unwrap();
    let mut pending_paths: Vec<&str> = pending_files
        .iter()
        .map(|file| file["path"].as_str().expect("a string path"))
        .collect();
    pending_paths.sort_unstable();
    assert_eq!(
        pending_paths,
        [
            "/hub.md",
            "/infra/clock.md",
            "/infra/index.md",
            "/infra/time.md"
        ]
    );
    let hub = pending_files
        .iter()
        .find(|file| file["path"] == "/hub.md")
        .expect("the hub is still pending");
    assert_eq!(hub["stale"], true);
    // Nothing of the refused publish reached the published catalog.
    assert_eq!(old["title"], "Time server");
    assert_eq!(new.is_error, Some(true));
    let new = new.structured_content.expect("the error's JSON");
    assert_eq!(new["error"], "NotFound");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_concurrent_index_of_the_same_folder_replays() {
    let server = TestServer::start(catalog(), Config::default()).await;
    let mover_key = writer(&server, "writer-a").await;
    let lister_key = writer(&server, "writer-b").await;
    let mover = server.client(&mover_key, None).await.unwrap();
    let lister = server.client(&lister_key, None).await.unwrap();
    let (moving, listing) = (
        open_changeset(&mover, None).await,
        open_changeset(&lister, None).await,
    );
    ok(&mover, "move_concept", move_time(&moving)).await;
    let concept = json!({
        "path": "/infra/ntp.md",
        "frontmatter": {"type": "System", "title": "NTP", "description": "How hosts sync."},
        "body": "# NTP\n",
        "index_summary": "how hosts sync",
        "changeset": listing,
    });
    ok(&lister, "create_concept", concept).await;
    ok(
        &lister,
        "publish",
        json!({"message": "add NTP", "changeset": listing}),
    )
    .await;

    let published = ok(
        &mover,
        "publish",
        json!({"message": "rename", "changeset": moving}),
    )
    .await;
    let index = ok(&mover, "read_document", json!({"path": "/infra/index.md"})).await;
    mover.cancel().await.unwrap();
    lister.cancel().await.unwrap();

    assert_eq!(published["published"], true);
    assert_eq!(published["replayed"], json!(["/infra/index.md"]));
    let expected = [
        ("/hub.md", "update"),
        ("/infra/clock.md", "create"),
        ("/infra/index.md", "update"),
        ("/infra/time.md", "delete"),
    ]
    .map(|(path, op)| (path.to_string(), op.to_string()));
    assert_eq!(revisions(&published), expected);
    let text = index["text"].as_str().unwrap();
    assert!(
        text.contains("* [Time server](clock.md) - the GPS clock"),
        "{text}"
    );
    assert!(text.contains("* [NTP](ntp.md) - how hosts sync"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_delete_is_refused_while_linked_then_published() {
    let server = TestServer::start(catalog(), Config::default()).await;
    let key = writer(&server, "writer").await;
    let client = server.client(&key, None).await.unwrap();
    let handle = open_changeset(&client, None).await;
    // A closure taking no arguments (a `lambda: ...`): `json!` builds a new
    // value per call, since `call` takes its arguments by value.
    let delete = || json!({"path": "/infra/time.md", "changeset": handle});

    let refused = call(&client, "delete_concept", delete()).await;
    let unlink = json!({
        "path": "/hub.md",
        "old_str": "See the [time server](/infra/time.md#ports).",
        "new_str": "See the time server.",
        "changeset": handle,
    });
    ok(&client, "edit_document", unlink).await;
    let deleted = ok(&client, "delete_concept", delete()).await;
    let published = ok(
        &client,
        "publish",
        json!({"message": "retire the time server", "changeset": handle}),
    )
    .await;
    let index = ok(&client, "read_document", json!({"path": "/infra/index.md"})).await;
    client.cancel().await.unwrap();

    assert_eq!(refused.is_error, Some(true));
    let body = refused.structured_content.expect("the error's JSON");
    assert_eq!(body["error"], "StillLinked");
    assert_eq!(body["path"], "/infra/time.md");
    assert_eq!(body["links"], json!([{"source": "/hub.md", "line": 6}]));
    assert_eq!(
        deleted["touched"],
        json!(["/infra/time.md", "/infra/index.md"])
    );
    let expected = [
        ("/hub.md", "update"),
        ("/infra/index.md", "update"),
        ("/infra/time.md", "delete"),
    ]
    .map(|(path, op)| (path.to_string(), op.to_string()));
    assert_eq!(revisions(&published), expected);
    assert_eq!(index["text"], "# Infra\n\n");
}

/// Issue #658's repro, step 1: a concept, listed and created in its own
/// changeset, whose `log.md` entry links it; published.
async fn logged_concept(client: &Client) {
    let handle = open_changeset(client, None).await;
    let concept = json!({
        "path": "/infra/x",
        "frontmatter": {"type": "System", "title": "X", "description": "A test system."},
        "body": "# X\n",
        "index_summary": "a test system",
        "changeset": handle,
    });
    ok(client, "create_concept", concept).await;
    let entry = json!({
        "text": "Added [X](/infra/x.md)",
        "kind": "Creation",
        "changeset": handle,
    });
    ok(client, "log", entry).await;
    let published = ok(
        client,
        "publish",
        json!({"message": "add x", "changeset": handle}),
    )
    .await;
    assert_eq!(published["published"], true, "{published}");
}

/// The rules of the lint errors in the published catalog (an empty list
/// when it is clean).
async fn lint_errors(client: &Client) -> Vec<String> {
    let report = ok(client, "lint", json!({})).await;
    report["errors"]
        .as_array()
        .unwrap_or_else(|| panic!("lint has an errors list: {report}"))
        .iter()
        .map(|finding| finding.to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_delete_unlinks_log_md_and_later_log_entries_still_publish() {
    let server = TestServer::start(catalog(), Config::default()).await;
    let key = writer(&server, "writer").await;
    let client = server.client(&key, None).await.unwrap();
    logged_concept(&client).await;

    let handle = open_changeset(&client, None).await;
    let deleted = ok(
        &client,
        "delete_concept",
        json!({"path": "/infra/x", "changeset": handle}),
    )
    .await;
    let published = ok(
        &client,
        "publish",
        json!({"message": "retire x", "changeset": handle}),
    )
    .await;
    let after_delete = lint_errors(&client).await;
    let later = open_changeset(&client, None).await;
    let entry = json!({"text": "Checked the catalog", "changeset": later});
    ok(&client, "log", entry).await;
    let later_published = ok(
        &client,
        "publish",
        json!({"message": "a later entry", "changeset": later}),
    )
    .await;
    let after_log = lint_errors(&client).await;
    let log = ok(&client, "read_document", json!({"path": "/log.md"})).await;
    client.cancel().await.unwrap();

    assert_eq!(
        deleted["touched"],
        json!(["/infra/x.md", "/infra/index.md", "/log.md"])
    );
    assert_eq!(published["published"], true, "{published}");
    let expected = [
        ("/infra/index.md", "update"),
        ("/infra/x.md", "delete"),
        ("/log.md", "update"),
    ]
    .map(|(path, op)| (path.to_string(), op.to_string()));
    assert_eq!(revisions(&published), expected);
    assert!(after_delete.is_empty(), "{after_delete:?}");
    assert_eq!(later_published["published"], true, "{later_published}");
    assert!(after_log.is_empty(), "{after_log:?}");
    let text = log["text"].as_str().unwrap();
    assert!(text.contains("* **Creation:** Added X\n"), "{text}");
    assert!(
        text.contains("* **Update:** Checked the catalog\n"),
        "{text}"
    );
    assert!(!text.contains("/infra/x.md"), "{text}");
}

/// A delete pending while another changeset logs an entry: both publish in
/// either order, and `log.md` ends with both entries and no link to the
/// deleted concept.
async fn a_concurrent_log_entry_replays(delete_first: bool) {
    let server = TestServer::start(catalog(), Config::default()).await;
    let deleter_key = writer(&server, "writer-a").await;
    let logger_key = writer(&server, "writer-b").await;
    let deleter = server.client(&deleter_key, None).await.unwrap();
    let logger = server.client(&logger_key, None).await.unwrap();
    logged_concept(&deleter).await;
    let (deleting, logging) = (
        open_changeset(&deleter, None).await,
        open_changeset(&logger, None).await,
    );
    ok(
        &deleter,
        "delete_concept",
        json!({"path": "/infra/x", "changeset": deleting}),
    )
    .await;
    let entry = json!({"text": "Synced the [hub](/hub.md)", "changeset": logging});
    ok(&logger, "log", entry).await;
    let publish = |handle: &str| json!({"message": "publish", "changeset": handle});

    let (first, second) = if delete_first {
        let first = ok(&deleter, "publish", publish(&deleting)).await;
        (first, ok(&logger, "publish", publish(&logging)).await)
    } else {
        let first = ok(&logger, "publish", publish(&logging)).await;
        (first, ok(&deleter, "publish", publish(&deleting)).await)
    };
    let errors = lint_errors(&deleter).await;
    let log = ok(&deleter, "read_document", json!({"path": "/log.md"})).await;
    deleter.cancel().await.unwrap();
    logger.cancel().await.unwrap();

    assert_eq!(first["published"], true, "{first}");
    assert_eq!(second["published"], true, "{second}");
    assert_eq!(second["replayed"], json!(["/log.md"]), "{second}");
    assert!(errors.is_empty(), "{errors:?}");
    let text = log["text"].as_str().unwrap();
    assert!(text.contains("* **Creation:** Added X\n"), "{text}");
    assert!(
        text.contains("* **Update:** Synced the [hub](/hub.md)\n"),
        "{text}"
    );
    assert!(!text.contains("/infra/x.md"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_log_entry_published_after_a_delete_replays() {
    a_concurrent_log_entry_replays(true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_delete_published_after_a_log_entry_replays() {
    a_concurrent_log_entry_replays(false).await;
}

/// Issue #661, the documented rare case: the delete's snapshot has no
/// `log.md` link, so it records no unlink, and a log entry linking the
/// concept publishes first. The delete's publish is refused (a link into a
/// deleted path), and the same delete in a fresh changeset succeeds.
#[tokio::test(flavor = "multi_thread")]
async fn a_delete_refused_by_a_racing_log_link_succeeds_when_retried() {
    let server = TestServer::start(catalog(), Config::default()).await;
    let deleter_key = writer(&server, "writer-a").await;
    let logger_key = writer(&server, "writer-b").await;
    let deleter = server.client(&deleter_key, None).await.unwrap();
    let logger = server.client(&logger_key, None).await.unwrap();
    let publish = |handle: &str| json!({"message": "publish", "changeset": handle});
    // A concept nothing links to (no log entry, unlike `logged_concept`).
    let creating = open_changeset(&deleter, None).await;
    let concept = json!({
        "path": "/infra/x",
        "frontmatter": {"type": "System", "title": "X", "description": "A test system."},
        "body": "# X\n",
        "index_summary": "a test system",
        "changeset": creating,
    });
    ok(&deleter, "create_concept", concept).await;
    ok(&deleter, "publish", publish(&creating)).await;
    let (deleting, logging) = (
        open_changeset(&deleter, None).await,
        open_changeset(&logger, None).await,
    );
    let delete = |handle: &str| json!({"path": "/infra/x", "changeset": handle});
    let deleted = ok(&deleter, "delete_concept", delete(&deleting)).await;
    let entry = json!({"text": "Added [X](/infra/x.md)", "changeset": logging});
    ok(&logger, "log", entry).await;
    ok(&logger, "publish", publish(&logging)).await;

    let refused = call(&deleter, "publish", publish(&deleting)).await;
    let retrying = open_changeset(&deleter, None).await;
    let retried = ok(&deleter, "delete_concept", delete(&retrying)).await;
    let published = ok(&deleter, "publish", publish(&retrying)).await;
    let errors = lint_errors(&deleter).await;
    let log = ok(&deleter, "read_document", json!({"path": "/log.md"})).await;
    deleter.cancel().await.unwrap();
    logger.cancel().await.unwrap();

    // The first delete's snapshot had no log link, so it noted none.
    assert!(
        deleted["notes"].as_array().is_none_or(Vec::is_empty),
        "{deleted}"
    );
    assert_eq!(refused.is_error, Some(true), "{refused:?}");
    let body = refused.structured_content.expect("the error's JSON");
    assert_eq!(body["error"], "LintFailed", "{body}");
    assert_eq!(body["errors"][0]["rule"], "link-into-deleted", "{body}");
    assert_eq!(body["errors"][0]["file"], "/log.md", "{body}");
    // The fresh changeset sees the log link and unlinks it.
    assert!(
        retried["notes"].to_string().contains("/log.md"),
        "{retried}"
    );
    assert_eq!(published["published"], true, "{published}");
    assert!(errors.is_empty(), "{errors:?}");
    let text = log["text"].as_str().unwrap();
    assert!(text.contains("Added X\n"), "{text}");
    assert!(!text.contains("/infra/x.md"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn links_outside_log_md_still_refuse_a_delete() {
    let server = TestServer::start(catalog(), Config::default()).await;
    let key = writer(&server, "writer").await;
    let client = server.client(&key, None).await.unwrap();
    logged_concept(&client).await;
    let handle = open_changeset(&client, None).await;
    // One link from a concept, one from another folder's index.md.
    let hub = json!({
        "path": "/hub.md",
        "old_str": "See the [time server](/infra/time.md#ports).",
        "new_str": "See the [time server](/infra/time.md#ports) and [x](/infra/x.md).",
        "changeset": handle,
    });
    ok(&client, "edit_document", hub).await;
    let root = json!({
        "path": "/index.md",
        "old_str": "* [Infra](/infra/index.md) - systems",
        "new_str": "* [Infra](/infra/index.md) - systems, such as [X](infra/x.md)",
        "changeset": handle,
    });
    ok(&client, "edit_document", root).await;

    let refused = call(
        &client,
        "delete_concept",
        json!({"path": "/infra/x", "changeset": handle}),
    )
    .await;
    let pending = ok(&client, "pending_changes", json!({"changeset": handle})).await;
    client.cancel().await.unwrap();

    assert_eq!(refused.is_error, Some(true));
    let body = refused.structured_content.expect("the error's JSON");
    assert_eq!(body["error"], "StillLinked");
    // log.md's link is not listed: a delete unlinks it.
    assert_eq!(
        body["links"],
        json!([
            {"source": "/hub.md", "line": 6},
            {"source": "/index.md", "line": 5},
        ])
    );
    let mut paths: Vec<&str> = pending["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| file["path"].as_str().unwrap())
        .collect();
    paths.sort_unstable();
    assert_eq!(paths, ["/hub.md", "/index.md"], "the refusal wrote nothing");
}
