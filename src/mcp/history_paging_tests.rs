//! Paging `history` past the newest page (issue #949): `before` is a strict
//! upper bound on the revision number, `next` is the page's last revision
//! while an older matching one exists, and a filter or an exact page
//! boundary never yields a `next` that leads to an empty page.

use chrono::{DateTime, Utc};

use crate::config::Config;
use crate::mcp::history::{history, History, HistoryParams, MAX_LIMIT};
use crate::mcp::testing::{app_with, context, principal, TestApp};
use crate::mcp::ToolContext;
use crate::store::history::REVISIONS_SQL;

const NOTE: &str = "---\ntype: Note\ntitle: Note\n---\nBody.\n";

/// How many revisions `a.md` ends up with: one import plus the seeded ones.
const TOTAL: i32 = 261;

/// A catalog whose `a.md` has revisions 1..=261: the import, then 260
/// inserted straight into the store (publishing that many is slow). Revision
/// `n >= 2` was made at `2026-01-01T00:00:00Z` plus `n` minutes.
async fn long_history() -> (TestApp, ToolContext) {
    let test = app_with(&[("pg", &[("a.md", NOTE)])], Config::default()).await;
    sqlx::query(
        "INSERT INTO catalog.revisions \
         (catalog_id, path, revision, op, executable, actor, message, created_at) \
         SELECT catalog_id, path, n, 'update', false, 'seed', 'edit ' || n, \
                '2026-01-01T00:00:00Z'::timestamptz + n * interval '1 minute' \
         FROM catalog.revisions, generate_series(2, $1) AS n \
         WHERE path = 'a.md' AND revision = 1",
    )
    .bind(TOTAL)
    .execute(test.app.main_db().await.executor())
    .await
    .expect("seed revisions");
    let ctx = context(&test, principal(&[], Some("pg")), None);
    (test, ctx)
}

fn ask(limit: u32, before: Option<i32>) -> HistoryParams {
    HistoryParams {
        path: "a.md".to_string(),
        catalog: None,
        limit: Some(limit),
        user: None,
        since: None,
        before,
    }
}

fn numbers(page: &History) -> Vec<i32> {
    page.revisions.iter().map(|row| row.revision).collect()
}

/// Follows `next` from the newest page until it is null, returning every
/// page.
async fn walk(ctx: &ToolContext, first: HistoryParams) -> Vec<History> {
    let mut pages = Vec::new();
    let mut params = first;
    loop {
        let page = history(ctx, params.clone()).await.expect("history");
        let next = page.next;
        pages.push(page);
        match next {
            Some(before) => params.before = Some(before),
            None => return pages,
        }
        assert!(pages.len() < 400, "next never ends");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn next_lists_a_document_past_200_revisions_down_to_revision_1() {
    let (_test, ctx) = long_history().await;

    let pages = walk(&ctx, ask(MAX_LIMIT, None)).await;

    let all: Vec<i32> = pages.iter().flat_map(numbers).collect();
    assert_eq!(all, (1..=TOTAL).rev().collect::<Vec<_>>());
    assert_eq!(pages.len(), 2);
    assert_eq!(pages[0].next, Some(TOTAL - 199));
    assert_eq!(pages[1].next, None);
    assert!(pages.iter().all(|page| page.latest_revision == Some(TOTAL)));
}

#[tokio::test(flavor = "multi_thread")]
async fn exactly_limit_revisions_left_gives_no_next() {
    let (_test, ctx) = long_history().await;

    // 261 revisions: 200, then 61 left. Asking for 61 takes them all.
    let exact = history(&ctx, ask(61, Some(62))).await.unwrap();
    let short = history(&ctx, ask(60, Some(62))).await.unwrap();

    assert_eq!(numbers(&exact), (1..=61).rev().collect::<Vec<_>>());
    assert_eq!(exact.next, None, "nothing older: no page after this one");
    assert_eq!(short.revisions.len(), 60);
    assert_eq!(short.next, Some(2));
}

#[tokio::test(flavor = "multi_thread")]
async fn before_is_a_strict_bound_and_past_the_oldest_is_an_empty_page() {
    let (_test, ctx) = long_history().await;

    let strict = history(&ctx, ask(3, Some(10))).await.unwrap();
    let at_oldest = history(&ctx, ask(3, Some(1))).await.unwrap();
    let below = history(&ctx, ask(3, Some(-5))).await.unwrap();
    let above = history(&ctx, ask(3, Some(i32::MAX))).await.unwrap();

    assert_eq!(numbers(&strict), [9, 8, 7]);
    assert_eq!(strict.next, Some(7));
    assert!(at_oldest.revisions.is_empty());
    assert_eq!(at_oldest.next, None);
    assert_eq!(at_oldest.latest_revision, Some(TOTAL));
    assert!(below.revisions.is_empty() && below.next.is_none());
    assert_eq!(numbers(&above), [261, 260, 259]);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_path_is_still_not_found_with_before() {
    let (_test, ctx) = long_history().await;
    let missing = HistoryParams {
        path: "nowhere.md".to_string(),
        ..ask(3, Some(5))
    };

    let refused = history(&ctx, missing).await;

    assert!(refused.is_err(), "{refused:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_filter_pages_over_the_revisions_it_matches_only() {
    let (_test, ctx) = long_history().await;
    // Matches revisions 100..=261 (made from minute 100 on) and revision 1,
    // an import made now.
    let since: DateTime<Utc> = "2026-01-01T01:40:00Z".parse().unwrap();
    let first = HistoryParams {
        since: Some(since.to_rfc3339()),
        ..ask(100, None)
    };

    let pages = walk(&ctx, first).await;

    let all: Vec<i32> = pages.iter().flat_map(numbers).collect();
    let expected: Vec<i32> = (100..=TOTAL).rev().chain([1]).collect();
    assert_eq!(all, expected);
    assert_eq!(
        pages
            .iter()
            .map(|page| page.revisions.len())
            .collect::<Vec<_>>(),
        [100, 63]
    );
    assert_eq!(pages[0].next, Some(162));
    assert_eq!(pages[1].next, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_page_query_reads_the_primary_key_backwards_without_sorting() {
    let (test, _ctx) = long_history().await;
    let db = test.app.main_db().await;
    let catalog_id: i64 =
        sqlx::query_scalar("SELECT catalog_id FROM catalog.revisions WHERE path = 'a.md' LIMIT 1")
            .fetch_one(db.executor())
            .await
            .unwrap();
    // A table this small is read by a sequential scan whatever the indexes
    // offer, so the planner is told not to: what is asked is whether the
    // key can serve the query.
    let mut tx = db.pool().begin().await.unwrap();
    sqlx::query("SET LOCAL enable_seqscan = off")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("SET LOCAL enable_bitmapscan = off")
        .execute(&mut *tx)
        .await
        .unwrap();

    let plan: Vec<String> = // `AssertSqlSafe`: the text is a constant of ours, not input.
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!("EXPLAIN {REVISIONS_SQL}")))
        .bind(catalog_id)
        .bind("a.md")
        .bind(20_i64)
        .bind(None::<String>)
        .bind(None::<DateTime<Utc>>)
        .bind(Some(100_i32))
        .fetch_all(&mut *tx)
        .await
        .unwrap();

    let plan = plan.join("\n");
    assert!(plan.contains("Index Scan Backward"), "{plan}");
    assert!(
        plan.contains("revisions_catalog_id_path_revision_key"),
        "{plan}"
    );
    assert!(!plan.contains("Sort"), "{plan}");
    assert!(
        plan.contains("revision <"),
        "the bound is an index condition: {plan}"
    );
}
