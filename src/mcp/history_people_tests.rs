//! The person behind each revision (issue #986): recorded at publish time
//! from the publishing credential, kept when a key is reassigned or the user
//! deleted, shown by `history`, `recent_changes`, `list_changesets` and
//! `pending_changes`, and filtered by `user` and `since`.

use std::sync::Arc;

use chrono::{DateTime, TimeDelta, Utc};

use crate::auth::Principal;
use crate::config::Config;
use crate::error::OkfError;
use crate::mcp::changesets::{
    list_changesets, pending_changes, publish, ListParams, PendingParams, PublishParams,
};
use crate::mcp::history::{
    history, recent_changes, HistoryParams, RecentChanges, RecentParams, Revision,
};
use crate::mcp::testing::{
    app_with, cached_principal, context, open, pending_entry, principal_as, store_key,
    stored_secret, PendingEdit, TestApp,
};
use crate::mcp::ToolContext;
use crate::store::audit::AuditActor;
use crate::store::keys::Access;
use crate::store::users::{self, DeleteOptions, NewUser};

const NOTE: &str = "---\ntype: Note\ntitle: Note\n---\nBody.\n";

async fn pg() -> TestApp {
    app_with(&[("pg", &[("a.md", NOTE)])], Config::default()).await
}

/// Creates user `name`, returning its `users.id`.
async fn user(test: &TestApp, name: &str) -> String {
    let record = users::create(
        test.app.accounts().executor(),
        &NewUser {
            name,
            display_name: &crate::labels::DisplayName::parse("Carol Example").unwrap(),
        },
        &AuditActor::cli(),
        Utc::now(),
    )
    .await
    .expect("create the user");
    record.id
}

/// A write context for key `key_id` owned by user `name` (`id`), whose key
/// row is stored.
async fn writer_of(test: &TestApp, key_id: &str, name: &str, id: &str) -> ToolContext {
    store_key(test, key_id).await;
    let base = principal_as(key_id, Access::Write, &[], Some("pg"));
    let owned = Principal {
        user: Some(name.to_string()),
        user_id: Some(id.to_string()),
        ..(*base).clone()
    };
    context(test, Arc::new(owned), None)
}

/// Publishes a new file `path` through `ctx`.
async fn publish_file(test: &TestApp, ctx: &ToolContext, path: &str) {
    let handle = open(ctx).await;
    pending_entry(
        test,
        &handle,
        &PendingEdit {
            path,
            text: Some(NOTE),
            at: Utc::now(),
        },
    )
    .await;
    publish(
        ctx,
        PublishParams {
            message: format!("add {path}"),
            changeset: handle,
        },
    )
    .await
    .expect("publish");
}

fn history_of(path: &str) -> HistoryParams {
    HistoryParams {
        path: path.to_string(),
        catalog: None,
        limit: None,
        user: None,
        since: None,
    }
}

async fn revisions_of(ctx: &ToolContext, path: &str) -> Vec<Revision> {
    history(ctx, history_of(path)).await.unwrap().revisions
}

fn recent_of(user: Option<&str>) -> RecentParams {
    RecentParams {
        catalog: Some("pg".to_string()),
        user: user.map(str::to_string),
        ..RecentParams::default()
    }
}

fn paths(changes: &RecentChanges) -> Vec<&str> {
    changes
        .changes
        .iter()
        .map(|change| change.path.as_str())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_publish_records_the_publishing_users_beside_the_actor() {
    let test = pg().await;
    let secret = stored_secret(&test, "claude-code", Access::Write, &["pg"], false).await;
    let ctx = context(&test, cached_principal(&test, &secret).await, None);

    publish_file(&test, &ctx, "n.md").await;

    let shown = revisions_of(&ctx, "n.md").await;
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].actor, "claude-code");
    assert_eq!(shown[0].actor_label, None, "a key's actor is no user");
    assert_eq!(shown[0].user.as_deref(), Some(crate::db::TEST_USER));
    assert!(shown[0].user_label.is_some(), "{:?}", shown[0]);
    let stored: Option<String> =
        sqlx::query_scalar("SELECT user_id FROM catalog.revisions WHERE path = 'n.md'")
            .fetch_one(&crate::db::store_pool(test.app.accounts().executor()))
            .await
            .unwrap();
    assert!(stored.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_key_with_no_user_and_an_import_record_no_person() {
    let test = pg().await;
    let ctx = crate::mcp::testing::writer(&test, "legacy", "pg").await;

    publish_file(&test, &ctx, "n.md").await;

    let published = revisions_of(&ctx, "n.md").await;
    let imported = revisions_of(&ctx, "a.md").await;
    assert_eq!(
        (&published[0].user, &published[0].user_label),
        (&None, &None)
    );
    assert_eq!((&imported[0].user, &imported[0].user_label), (&None, &None));
}

#[tokio::test(flavor = "multi_thread")]
async fn reassigning_the_key_does_not_change_who_earlier_revisions_credit() {
    let test = pg().await;
    let secret = stored_secret(&test, "claude-code", Access::Write, &["pg"], false).await;
    let ctx = context(&test, cached_principal(&test, &secret).await, None);
    publish_file(&test, &ctx, "n.md").await;
    let other = user(&test, "dave").await;

    sqlx::query("UPDATE api_keys SET user_id = $1 WHERE name = 'claude-code'")
        .bind(&other)
        .execute(test.app.accounts().executor())
        .await
        .unwrap();

    let shown = revisions_of(&ctx, "n.md").await;
    assert_eq!(shown[0].user.as_deref(), Some(crate::db::TEST_USER));
}

#[tokio::test(flavor = "multi_thread")]
async fn deleting_the_user_keeps_the_record_and_history_shows_the_id() {
    let test = pg().await;
    let carol = user(&test, "carol").await;
    let ctx = writer_of(&test, "carol-key", "carol", &carol).await;
    publish_file(&test, &ctx, "n.md").await;
    let before = revisions_of(&ctx, "n.md").await;
    assert_eq!(before[0].user.as_deref(), Some("carol"));
    assert!(before[0].user_label.is_some());

    users::delete(
        test.app.accounts().executor(),
        "carol",
        DeleteOptions::default(),
        &AuditActor::cli(),
        Utc::now(),
    )
    .await
    .expect("delete the user");

    let reader = context(
        &test,
        principal_as("reader", Access::Read, &[], Some("pg")),
        None,
    );
    let after = revisions_of(&reader, "n.md").await;
    assert_eq!(after[0].user.as_deref(), Some(carol.as_str()), "the id");
    assert_eq!(after[0].user_label, None, "no label resolves any more");
    let stored: Option<String> =
        sqlx::query_scalar("SELECT user_id FROM catalog.revisions WHERE path = 'n.md'")
            .fetch_one(&crate::db::store_pool(test.app.accounts().executor()))
            .await
            .unwrap();
    assert_eq!(stored.as_deref(), Some(carol.as_str()));
    // The id the revision shows filters as it reads; the gone name does not.
    let by_id = recent_changes(&reader, recent_of(Some(&carol)))
        .await
        .unwrap();
    let by_name = recent_changes(&reader, recent_of(Some("carol")))
        .await
        .unwrap();
    assert_eq!(paths(&by_id), ["/n.md"]);
    assert!(by_name.changes.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn history_and_recent_changes_filter_by_user_and_since_newest_first() {
    let test = pg().await;
    let (ann, bob) = (user(&test, "ann").await, user(&test, "bob").await);
    let ann_ctx = writer_of(&test, "ann-key", "ann", &ann).await;
    let bob_ctx = writer_of(&test, "bob-key", "bob", &bob).await;
    publish_file(&test, &ann_ctx, "one.md").await;
    let middle = Utc::now();
    publish_file(&test, &bob_ctx, "two.md").await;
    publish_file(&test, &ann_ctx, "three.md").await;
    let reader = context(
        &test,
        principal_as("reader", Access::Read, &[], Some("pg")),
        None,
    );

    let all = recent_changes(&reader, recent_of(None)).await.unwrap();
    let anns = recent_changes(&reader, recent_of(Some("ann")))
        .await
        .unwrap();
    let since = RecentParams {
        since: Some(middle.to_rfc3339()),
        ..recent_of(None)
    };
    let since = recent_changes(&reader, since).await.unwrap();
    let ann_since = RecentParams {
        since: Some(middle.to_rfc3339()),
        ..recent_of(Some("ann"))
    };
    let ann_since = recent_changes(&reader, ann_since).await.unwrap();

    // Every revision, the starter and imported ones too, newest first.
    assert_eq!(&paths(&all)[..3], ["/three.md", "/two.md", "/one.md"]);
    assert_eq!(paths(&anns), ["/three.md", "/one.md"]);
    assert_eq!(paths(&since), ["/three.md", "/two.md"]);
    assert_eq!(paths(&ann_since), ["/three.md"]);
    assert_eq!(anns.changes[0].user.as_deref(), Some("ann"));
    assert_eq!(anns.changes[0].actor, "ann-key");
    // `history` takes the same filters, and answers an empty list, not "no
    // such document", for a path whose revisions the filters leave out.
    let one = |user: Option<&str>, since: Option<DateTime<Utc>>| HistoryParams {
        user: user.map(str::to_string),
        since: since.map(|at| at.to_rfc3339()),
        ..history_of("one.md")
    };
    let mine = history(&reader, one(Some("ann"), None)).await.unwrap();
    let bobs = history(&reader, one(Some("bob"), None)).await.unwrap();
    let later = history(&reader, one(None, Some(middle))).await.unwrap();
    let missing = HistoryParams {
        user: Some("ann".to_string()),
        ..history_of("nowhere.md")
    };
    let missing = history(&reader, missing).await.unwrap_err();
    assert_eq!(mine.revisions.len(), 1);
    assert!(bobs.revisions.is_empty());
    assert!(later.revisions.is_empty());
    assert!(matches!(missing, OkfError::NotFound { .. }));
}

#[tokio::test(flavor = "multi_thread")]
async fn recent_changes_pages_by_cursor_within_the_caps_and_refuses_a_bad_since() {
    let test = pg().await;
    let ann = user(&test, "ann").await;
    let ctx = writer_of(&test, "ann-key", "ann", &ann).await;
    for path in ["a1.md", "a2.md", "a3.md"] {
        publish_file(&test, &ctx, path).await;
    }
    let page = |limit, after: Option<String>| RecentParams {
        limit: Some(limit),
        after,
        ..recent_of(Some("ann"))
    };

    let first = recent_changes(&ctx, page(2, None)).await.unwrap();
    let second = recent_changes(&ctx, page(2, first.next_after.clone()))
        .await
        .unwrap();
    let zero = recent_changes(&ctx, page(0, None)).await.unwrap();
    let huge = recent_changes(&ctx, page(u32::MAX, None)).await.unwrap();
    let bad_since = RecentParams {
        since: Some("yesterday".to_string()),
        ..recent_of(None)
    };
    let bad_since = recent_changes(&ctx, bad_since).await.unwrap_err();
    let bad_after = RecentParams {
        after: Some("nonsense".to_string()),
        ..recent_of(None)
    };
    let bad_after = recent_changes(&ctx, bad_after).await.unwrap_err();

    assert_eq!(paths(&first), ["/a3.md", "/a2.md"]);
    assert!(first.next_after.is_some());
    assert_eq!(paths(&second), ["/a1.md"]);
    assert_eq!(second.next_after, None);
    assert_eq!(zero.changes.len(), 1, "0 counts as 1");
    assert_eq!(huge.changes.len(), 3, "capped at 200, here by the rows");
    assert!(
        matches!(bad_since, OkfError::Refused { .. }),
        "{bad_since:?}"
    );
    assert!(
        matches!(bad_after, OkfError::NotFound { .. }),
        "{bad_after:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn open_changesets_show_the_person_who_opened_them() {
    let test = pg().await;
    let secret = stored_secret(&test, "claude-code", Access::Write, &["pg"], false).await;
    let ctx = context(&test, cached_principal(&test, &secret).await, None);
    let handle = open(&ctx).await;
    pending_entry(
        &test,
        &handle,
        &PendingEdit {
            path: "n.md",
            text: Some(NOTE),
            at: Utc::now() + TimeDelta::seconds(1),
        },
    )
    .await;

    let listed = list_changesets(&ctx, ListParams { catalog: None })
        .await
        .unwrap();
    let pending = pending_changes(
        &ctx,
        PendingParams {
            changeset: handle.clone(),
        },
    )
    .await
    .unwrap();

    assert_eq!(listed.changesets.len(), 1);
    assert_eq!(
        listed.changesets[0].user.as_deref(),
        Some(crate::db::TEST_USER)
    );
    assert!(listed.changesets[0].user_label.is_some());
    assert_eq!(pending.user.as_deref(), Some(crate::db::TEST_USER));
    assert!(pending.user_label.is_some());
}
