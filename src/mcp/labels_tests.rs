//! Labels beside ids in the tool outputs (milestone 17, Task 7): each
//! output that names a catalog or an actor says what to call it.

use crate::config::Config;
use crate::error::OkfError;
use crate::mcp::catalogs::list;
use crate::mcp::changesets::{list_changesets, open_changeset, ListParams, OpenParams};
use crate::mcp::documents::{get_concept, read_document, GetConceptParams, ReadDocumentParams};
use crate::mcp::history::{history, HistoryParams};
use crate::mcp::search::{self, SearchParams};
use crate::mcp::testing::{
    app_with, cached_principal, context, principal, stored_secret, suspend_catalogs, writer,
    TestApp,
};
use crate::store::keys::Access;

const NOTE: &str = "---\ntype: Note\ntitle: Note\n---\nBody.\n";

/// Runs `statements`, then catches up (what a change-feed reconnect does),
/// so the label cache reads the labels again: a raw update sends no change
/// message, as the store's own writers do.
async fn relabel(test: &TestApp, statements: &[&'static str]) {
    for statement in statements {
        // A statement on schema `catalog` runs on the store, a database of
        // its own in `OKF_TEST_STORE=separate` mode.
        let pool = if statement.contains(" catalog.") {
            crate::db::store_pool(test.app.accounts().executor())
        } else {
            test.app.accounts().executor().clone()
        };
        sqlx::query(*statement)
            .execute(&pool)
            .await
            .expect("relabel");
    }
    test.app.apply_reconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn list_catalogs_carries_display_name_and_label() {
    let test = app_with(&[("main", &[]), ("other", &[])], Config::default()).await;
    relabel(
        &test,
        &[
            "UPDATE catalogs SET display_name = 'Work notes' WHERE name = 'main'",
            "UPDATE catalogs SET display_name = '', description = 'Other things' \
             WHERE name = 'other'",
        ],
    )
    .await;

    let listed = list(&context(&test, principal(&[], None), None), None)
        .await
        .unwrap();

    let main = listed.catalogs.iter().find(|c| c.name == "main").unwrap();
    assert_eq!(main.display_name, "Work notes");
    assert_eq!(main.label, "Work notes");
    let other = listed.catalogs.iter().find(|c| c.name == "other").unwrap();
    assert_eq!(other.display_name, "");
    assert_eq!(other.label, "Other things");
}

#[tokio::test(flavor = "multi_thread")]
async fn list_catalogs_names_a_suspended_team_by_label() {
    let test = app_with(&[("main", &[])], Config::default()).await;
    let reader = stored_secret(&test, "reader", Access::Read, &[], false).await;
    suspend_catalogs(&test, &["main"]).await;
    relabel(
        &test,
        &["UPDATE teams SET display_name = 'Acme' WHERE name = 'family'"],
    )
    .await;

    let listed = list(
        &context(&test, cached_principal(&test, &reader).await, None),
        None,
    )
    .await
    .unwrap();

    let suspended = listed.catalogs[0].suspended.as_ref().expect("suspended");
    assert_eq!(suspended.team, "family");
    assert_eq!(suspended.team_label, "Acme");
}

#[tokio::test(flavor = "multi_thread")]
async fn history_resolves_a_user_actor_to_a_label_and_others_to_null() {
    let test = app_with(
        &[("pg", &[("a.md", NOTE), ("b.md", NOTE)])],
        Config::default(),
    )
    .await;
    // The test principal's actor is "tester": make it a user slug.
    relabel(
        &test,
        &[
            "INSERT INTO users (name, display_name, created_at) \
             VALUES ('tester', 'Alice Example', now())",
            "UPDATE catalog.revisions SET actor = 'claude-code/x' WHERE path = 'b.md'",
        ],
    )
    .await;
    let ctx = context(&test, principal(&[], Some("pg")), None);
    let params = |path: &str| HistoryParams {
        path: path.to_string(),
        catalog: None,
        limit: None,
        user: None,
        since: None,
        before: None,
    };

    let by_user = history(&ctx, params("a.md")).await.unwrap();
    let by_client = history(&ctx, params("b.md")).await.unwrap();

    assert_eq!(by_user.revisions[0].actor, "tester");
    assert_eq!(
        by_user.revisions[0].actor_label.as_deref(),
        Some("Alice Example")
    );
    assert_eq!(by_client.revisions[0].actor, "claude-code/x");
    assert_eq!(by_client.revisions[0].actor_label, None);
    let json = serde_json::to_value(&by_client).unwrap();
    assert_eq!(
        json["revisions"][0].get("actor_label"),
        Some(&serde_json::Value::Null)
    );
    // No display name and no description: the label rule's placeholder.
    assert_eq!(by_user.catalog_label, "Untitled catalog");
}

#[tokio::test(flavor = "multi_thread")]
async fn reads_searches_and_changesets_carry_the_catalog_label() {
    let test = app_with(
        &[("pg", &[("a.md", NOTE), ("index.md", "# Index\n")])],
        Config::default(),
    )
    .await;
    relabel(
        &test,
        &["UPDATE catalogs SET display_name = 'Work notes' WHERE name = 'pg'"],
    )
    .await;
    let ctx = writer(&test, "tester", "pg").await;

    let concept = get_concept(
        &ctx,
        GetConceptParams {
            path: "a".to_string(),
            catalog: None,
            links: false,
            raw: false,
            section: None,
            full: false,
            changeset: None,
        },
    )
    .await
    .unwrap();
    let document = read_document(
        &ctx,
        ReadDocumentParams {
            path: Some("/index.md".to_string()),
            catalog: None,
            changeset: None,
        },
    )
    .await
    .unwrap();
    let found = search::run(
        &ctx,
        SearchParams {
            query: "body".to_string(),
            catalogs: None,
            concept_type: None,
            tags: Vec::new(),
            status: None,
            limit: None,
            changeset: None,
        },
    )
    .await
    .unwrap();
    let opened = open_changeset(&ctx, OpenParams { catalog: None })
        .await
        .unwrap();
    let listed = list_changesets(&ctx, ListParams { catalog: None })
        .await
        .unwrap();

    assert_eq!(concept.catalog, "pg");
    assert_eq!(concept.catalog_label, "Work notes");
    assert_eq!(document.catalog_label, "Work notes");
    assert_eq!(found.hits[0].hit.catalog, "pg");
    assert_eq!(found.hits[0].catalog_label, "Work notes");
    assert_eq!(opened.catalog, "pg");
    assert_eq!(opened.catalog_label, "Work notes");
    assert_eq!(listed.changesets[0].catalog_label, "Work notes");
    let json = serde_json::to_value(&found).unwrap();
    assert_eq!(json["hits"][0]["catalog_label"], "Work notes");
    assert_eq!(json["hits"][0]["catalog"], "pg");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_changeset_actor_that_is_a_user_slug_gets_a_label() {
    let test = app_with(&[("pg", &[])], Config::default()).await;
    let ctx = writer(&test, "alice", "pg").await;
    open_changeset(&ctx, OpenParams { catalog: None })
        .await
        .unwrap();
    relabel(
        &test,
        &["INSERT INTO users (name, display_name, created_at) \
           VALUES ('alice', 'Alice Example', now())"],
    )
    .await;

    let listed = list_changesets(&ctx, ListParams { catalog: None })
        .await
        .unwrap();

    let first = &listed.changesets[0];
    assert_eq!(first.actor_label.as_deref(), Some("Alice Example"));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_suspended_error_names_label_and_id() {
    let test = app_with(&[("m-1", &[("a.md", NOTE)])], Config::default()).await;
    let reader = stored_secret(&test, "reader", Access::Read, &["m-1"], false).await;
    suspend_catalogs(&test, &["m-1"]).await;
    relabel(
        &test,
        &[
            "UPDATE catalogs SET display_name = 'Work notes' WHERE name = 'm-1'",
            "UPDATE teams SET display_name = 'Acme' WHERE name = 'family'",
        ],
    )
    .await;

    let refused = cached_principal(&test, &reader)
        .await
        .require_in(test.app.directory(), Access::Read, "m-1")
        .await
        .unwrap_err();

    assert!(matches!(refused, OkfError::Suspended { .. }));
    assert_eq!(
        refused.to_string(),
        "catalog \"Work notes\" (m-1) is unavailable: its team \"Acme\" (family) is suspended. \
         Paused for billing."
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_catalog_error_is_unchanged() {
    let test = app_with(&[("pg", &[])], Config::default()).await;
    let ctx = context(&test, principal(&[], Some("pg")), None);

    let error = ctx.one(Some("nope")).await.unwrap_err();

    assert_eq!(
        error.to_string(),
        "catalog \"nope\" does not exist or is not available"
    );
}
