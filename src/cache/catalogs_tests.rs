//! Tests for the catalog cache (M18 Task 8), through a real `App`: loads on
//! demand, refreshes in place, broken generations, budgets and the
//! per-request snapshot.

use std::time::Duration;

use bytesize::ByteSize;
use chrono::Utc;
use sqlx::PgPool;
use tokio::time::timeout;

use super::*;
use crate::app::App;
use crate::archive::Files;
use crate::config::Config;
use crate::invalidate::{Invalidation, Message};
use crate::mcp::testing::{
    app_with_metrics, context, open, pending_entry, principal, render, sample_of, writer,
    PendingEdit, TestApp,
};
use crate::okf::workspace::StoredFile;
use crate::store::audit::AuditActor;
use crate::store::import::{import, ImportOptions, ImportScope};
use crate::store::publish::{publish, PublishInput};

/// Every wait in these tests is bounded by this.
const BOUND: Duration = Duration::from_secs(30);

/// A note of `size` bytes or a little more.
fn note(size: usize) -> String {
    let mut text = "---\ntype: Note\ntitle: Big\ndescription: A note.\n---\n# Big\n".to_string();
    while text.len() < size {
        text.push_str("Lorem ipsum dolor sit amet.\n");
    }
    text
}

/// An app whose catalog cache holds `budget` estimated bytes, with
/// `catalogs` holding the files given.
async fn app(catalogs: &[(&str, &[(&str, &str)])], budget: u64) -> (TestApp, prometheus::Registry) {
    let config = Config {
        catalog_cache_bytes: ByteSize::b(budget),
        ..Config::default()
    };
    app_with_metrics(catalogs, config).await
}

/// A change-feed message as the listener would parse it off the wire.
fn message(invalidation: Invalidation) -> Message {
    Message {
        invalidation,
        sent_at_ms: Utc::now().timestamp_millis(),
    }
}

/// The row of catalog `name`, as a request resolves it.
async fn row(app: &App, name: &str) -> Arc<CatalogRow> {
    app.catalog_row(name)
        .await
        .unwrap()
        .expect("the catalog exists")
}

/// Imports `files` into `catalog` on the pool, as another replica or the
/// command line would (nothing tells this app), and returns the generation.
async fn import_behind(pool: &PgPool, catalog: &str, files: &[(&str, &str)]) -> i64 {
    let files: Files = files
        .iter()
        .map(|(path, text)| {
            let file = StoredFile {
                content: text.as_bytes().to_vec(),
                executable: false,
            };
            (path.to_string(), file)
        })
        .collect();
    let options = ImportOptions {
        replace: true,
        scope: ImportScope::Everything,
        expect_generation: None,
        expect_catalog_id: None,
        actor: "tester",
        audit: &AuditActor::cli(),
        now: Utc::now(),
    };
    import(
        &crate::stores::CatalogDb::main_in(pool),
        catalog,
        files,
        &options,
    )
    .await
    .unwrap()
    .changed
    .expect("the import changed the catalog")
    .generation
}

/// Stores a row no workspace accepts (a path escaping the catalog) and
/// moves the generation, so that generation's snapshot fails to build.
async fn break_catalog(pool: &PgPool, catalog_id: i64) {
    sqlx::query(
        "INSERT INTO catalog.documents (catalog_id, path, content, executable, revision, \
                                updated_at, updated_by) \
         VALUES ($1, '../escape.md', '', false, 1, now(), 'test')",
    )
    .bind(catalog_id)
    .execute(&crate::db::store_pool(pool))
    .await
    .unwrap();
    bump_generation(pool, catalog_id).await;
}

/// Moves a catalog's generation, as an import or publish does.
async fn bump_generation(pool: &PgPool, catalog_id: i64) {
    sqlx::query("UPDATE catalog.catalog_state SET generation = generation + 1 WHERE id = $1")
        .bind(catalog_id)
        .execute(&crate::db::store_pool(pool))
        .await
        .unwrap();
}

/// A value of the rendered metrics, or 0 when the series is absent.
fn sample(registry: &prometheus::Registry, series: &str) -> f64 {
    sample_of(&render(registry), series).unwrap_or(0.0)
}

#[tokio::test(flavor = "multi_thread")]
async fn start_up_loads_no_catalog() {
    let three: &[(&str, &[(&str, &str)])] = &[
        ("a", &[("a.md", "# A\n")]),
        ("b", &[("b.md", "# B\n")]),
        ("c", &[("c.md", "# C\n")]),
    ];

    let (test, registry) = app(three, ByteSize::gib(1).as_u64()).await;
    test.app.catalog_cache().settle().await;

    let text = render(&registry);
    assert_eq!(
        sample_of(&text, r#"okf_cache_entries{cache="catalog"}"#),
        Some(0.0),
        "{text}"
    );
    for name in ["a", "b", "c"] {
        let id = row(&test.app, name).await.id;
        assert!(test.app.catalog_cache().resident(id).await.is_none());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_first_read_loads_and_the_second_hits() {
    let (test, registry) = app(&[("pg", &[("a.md", "# A\n")])], ByteSize::gib(1).as_u64()).await;
    let pg = row(&test.app, "pg").await;

    let first = test.app.catalog_state(&pg).await.unwrap();
    let second = test.app.catalog_state(&pg).await.unwrap();

    assert!(Arc::ptr_eq(&first, &second), "the cached state is served");
    assert_eq!(
        sample(
            &registry,
            r#"okf_cache_requests_total{cache="catalog",result="miss"}"#
        ),
        1.0
    );
    assert_eq!(
        sample(
            &registry,
            r#"okf_cache_requests_total{cache="catalog",result="hit"}"#
        ),
        1.0
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_catalog_invalidation_refreshes_in_place() {
    let (test, _registry) = app(&[("pg", &[("a.md", "# A\n")])], ByteSize::gib(1).as_u64()).await;
    let pg = row(&test.app, "pg").await;
    let state = test.app.catalog_state(&pg).await.unwrap();
    let pinned = state.current();

    let generation =
        import_behind(test.app.accounts().executor(), "pg", &[("a.md", "# A2\n")]).await;
    test.app
        .apply(&message(Invalidation::Catalog {
            id: pg.id,
            generation,
        }))
        .await;

    // A reader that pinned the old snapshot keeps it, whole.
    assert_eq!(pinned.snapshot().get("a.md").unwrap().text(), "# A\n");
    // The same state, refreshed in place.
    assert_eq!(state.current().generation(), generation);
    let fresh = row(&test.app, "pg").await;
    assert_eq!(fresh.generation, generation);
    let again = test.app.catalog_state(&fresh).await.unwrap();
    assert!(Arc::ptr_eq(&state, &again));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stale_row_is_caught_up_on_use_without_a_message() {
    let (test, _registry) = app(&[("pg", &[("a.md", "# A\n")])], ByteSize::gib(1).as_u64()).await;
    let state = test.app.catalog("pg").await.unwrap().unwrap();
    let generation =
        import_behind(test.app.accounts().executor(), "pg", &[("a.md", "# A2\n")]).await;

    // A lost change feed marks the directory stale; the next use reads the
    // row again and refreshes the snapshot to it.
    test.app.apply_reconnect().await;
    let again = test.app.catalog("pg").await.unwrap().unwrap();

    assert!(Arc::ptr_eq(&state, &again));
    assert_eq!(again.current().generation(), generation);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_broken_generation_is_not_rebuilt_on_every_request() {
    let (test, registry) = app(&[("pg", &[("a.md", "# A\n")])], ByteSize::gib(1).as_u64()).await;
    let pool = test.app.accounts().executor().clone();
    let id = row(&test.app, "pg").await.id;
    break_catalog(&pool, id).await;
    // Nothing told the directory: as after a lost change feed, it reads the
    // row again.
    test.app.apply_reconnect().await;
    let broken = row(&test.app, "pg").await;
    let attempts = |outcome: &str| {
        sample(
            &registry,
            &format!(
                r#"okf_cache_load_duration_seconds_count{{cache="catalog",outcome="{outcome}"}}"#
            ),
        )
    };

    let first = test.app.catalog_state(&broken).await.unwrap_err();
    let second = test.app.catalog_state(&broken).await.unwrap_err();

    assert_eq!(first.kind(), "CatalogNotSelected");
    assert_eq!(second.kind(), "CatalogNotSelected");
    assert_eq!(attempts("error"), 1.0, "one build attempt for two reads");
    assert!(test.app.catalog("pg").await.unwrap().is_none());

    // Repaired, and the generation moves (as an import would move it).
    sqlx::query("DELETE FROM catalog.documents WHERE catalog_id = $1 AND path = '../escape.md'")
        .bind(id)
        .execute(&crate::db::store_pool(&pool))
        .await
        .unwrap();
    bump_generation(&pool, id).await;
    test.app
        .apply(&message(Invalidation::Catalog {
            id,
            generation: broken.generation + 1,
        }))
        .await;
    let repaired = row(&test.app, "pg").await;
    let state = test.app.catalog_state(&repaired).await.unwrap();

    assert_eq!(state.current().generation(), repaired.generation);
    assert_eq!(attempts("error"), 1.0);
    assert_eq!(
        attempts("ok"),
        1.0,
        "one more attempt, at the new generation"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_refresh_keeps_serving_the_old_snapshot_until_the_catalog_changes() {
    let (test, _registry) = app(&[("pg", &[("a.md", "# A\n")])], ByteSize::gib(1).as_u64()).await;
    let pool = test.app.accounts().executor().clone();
    let state = test.app.catalog("pg").await.unwrap().unwrap();
    let loaded_at = state.current().generation();
    break_catalog(&pool, state.id()).await;
    test.app.apply_reconnect().await;

    let served = test.app.catalog("pg").await.unwrap().unwrap();

    assert!(Arc::ptr_eq(&served, &state));
    assert_eq!(served.current().generation(), loaded_at, "the old snapshot");
}

/// Requests that all find a resident catalog behind a generation that does
/// not build rebuild it once between them: the first records it as broken
/// under the catalog's catch-up lock, and the others, queued on that lock,
/// see the record and serve the old snapshot.
#[test]
fn concurrent_requests_behind_a_broken_generation_build_it_once() {
    use crate::telemetry::subscriber::capture::SpanRecorder;

    // Arranged on its own runtime: the recorder runs the act on another.
    let setup = tokio::runtime::Runtime::new().unwrap();
    let (test, _registry, broken, loaded_at) = setup.block_on(async {
        let (test, registry) =
            app(&[("pg", &[("a.md", "# A\n")])], ByteSize::gib(1).as_u64()).await;
        let state = test.app.catalog("pg").await.unwrap().unwrap();
        let loaded_at = state.current().generation();
        break_catalog(test.app.accounts().executor(), state.id()).await;
        test.app.apply_reconnect().await;
        let broken = row(&test.app, "pg").await;
        assert!(broken.generation > loaded_at);
        (test, registry, broken, loaded_at)
    });
    let recorder = SpanRecorder::new();

    let served = recorder.run(async {
        let get = || test.app.catalog_state(&broken);
        tokio::time::timeout(BOUND, async {
            tokio::join!(get(), get(), get(), get(), get(), get())
        })
        .await
        .expect("the requests finish")
    });

    let builds = recorder
        .finished()
        .iter()
        .filter(|span| span.name == "build")
        .count();
    assert_eq!(builds, 1, "one build for six requests");
    for state in [served.0, served.1, served.2, served.3, served.4, served.5] {
        let state = state.expect("the old snapshot is served");
        assert_eq!(state.current().generation(), loaded_at);
    }
    setup.block_on(async move { drop(test) });
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_keeps_its_snapshot_while_the_entry_is_evicted() {
    // Two catalogs of one kilobyte each; the budget holds one.
    let text = note(1024);
    let files: &[(&str, &str)] = &[("a.md", &text)];
    let budget = weight(1024 * 3 / 2, 1);
    let (test, _registry) = app(&[("a", files), ("b", files)], budget).await;
    let ctx = context(&test, principal(&[], None), None);

    let a = ctx.state("a").await.unwrap();
    let a_snapshot = ctx.snapshot(&a);
    let b = ctx.state("b").await.unwrap();
    let b_snapshot = ctx.snapshot(&b);
    test.app.catalog_cache().settle().await;

    let a_resident = test.app.catalog_cache().resident(a.id()).await.is_some();
    let b_resident = test.app.catalog_cache().resident(b.id()).await.is_some();
    assert!(!(a_resident && b_resident), "the budget holds one catalog");
    // The request still reads both, from the snapshots it took.
    assert!(Arc::ptr_eq(&ctx.state("a").await.unwrap(), &a));
    assert!(Arc::ptr_eq(&ctx.snapshot(&a), &a_snapshot));
    assert!(Arc::ptr_eq(&ctx.snapshot(&b), &b_snapshot));
    assert!(a_snapshot.snapshot().get("a.md").is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_sees_one_snapshot_when_the_catalog_refreshes_between_reads() {
    let (test, _registry) = app(&[("a", &[("a.md", "# A\n")])], ByteSize::gib(1).as_u64()).await;
    let ctx = context(&test, principal(&[], None), None);
    let first = ctx.read_set(Some("a")).await.unwrap();
    let first_generation = ctx.snapshot(&first[0]).generation();

    let generation =
        import_behind(test.app.accounts().executor(), "a", &[("a.md", "# A2\n")]).await;
    test.app
        .apply(&message(Invalidation::Catalog {
            id: first[0].id(),
            generation,
        }))
        .await;
    let second = ctx.read_set(Some("a")).await.unwrap();

    assert_eq!(first[0].current().generation(), generation, "refreshed");
    assert_eq!(ctx.snapshot(&second[0]).generation(), first_generation);
    let next = context(&test, principal(&[], None), None);
    let read = next.read_set(Some("a")).await.unwrap();
    assert_eq!(next.snapshot(&read[0]).generation(), generation);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resident_catalog_that_grows_past_the_budget_is_not_kept() {
    // Small enough to be kept at first; the documents added take each past
    // the budget (64 KiB of estimated bytes, 6.4 KiB stored).
    let small: &[(&str, &str)] = &[("a.md", "---\ntype: Note\n---\n# A\n")];
    let budget = 64 * 1024;
    let (test, registry) = app(&[("pg", small), ("work", small)], budget).await;
    let cache = test.app.catalog_cache();
    let pg = test.app.catalog("pg").await.unwrap().unwrap();
    let work = test.app.catalog("work").await.unwrap().unwrap();
    cache.settle().await;
    assert!(cache.resident(pg.id()).await.is_some());
    assert!(cache.resident(work.id()).await.is_some());
    let big = note(8 * 1024);

    // Through the change feed: `work` grows behind this app, then its
    // message is applied (an in-place refresh).
    let generation = import_behind(test.app.accounts().executor(), "work", &[("a.md", &big)]).await;
    test.app
        .apply(&message(Invalidation::Catalog {
            id: work.id(),
            generation,
        }))
        .await;
    // Through the publisher's own refresh: `pg` grows by a publish here.
    let ctx = writer(&test, "writer", "pg").await;
    let handle = open(&ctx).await;
    let edit = PendingEdit {
        path: "b.md",
        text: Some(&big),
        at: Utc::now(),
    };
    let changeset = pending_entry(&test, &handle, &edit).await;
    let input = PublishInput {
        changeset: &changeset,
        actor: "writer",
        message: "grow",
    };
    let published = publish(&test.app, &pg, &input, &AuditActor::cli())
        .await
        .unwrap();
    cache.settle().await;

    assert!(published.published);
    assert_eq!(
        work.current().generation(),
        generation,
        "refreshed in place"
    );
    assert!(
        cache.resident(work.id()).await.is_none(),
        "work is not kept"
    );
    assert!(cache.resident(pg.id()).await.is_none(), "pg is not kept");
    // Not reported at the weight it was first stored with.
    assert_eq!(
        sample(&registry, r#"okf_cache_bytes{cache="catalog"}"#),
        0.0
    );
}

/// A publish that refreshes its state in place (the snapshot is behind the
/// database: an import elsewhere) and is then refused (lint) still has the
/// cache weigh the grown snapshot, so an entry past the budget is dropped.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_publish_after_a_growing_refresh_still_reweighs() {
    let small: &[(&str, &str)] = &[("a.md", "---\ntype: Note\n---\n# A\n")];
    let (test, registry) = app(&[("pg", small)], 64 * 1024).await;
    let cache = test.app.catalog_cache();
    let pg = test.app.catalog("pg").await.unwrap().unwrap();
    cache.settle().await;
    assert!(cache.resident(pg.id()).await.is_some());
    let ctx = writer(&test, "writer", "pg").await;
    let handle = open(&ctx).await;
    // A new file whose link goes nowhere: the publish's lint refuses it.
    let edit = PendingEdit {
        path: "broken.md",
        text: Some("---\ntype: Note\ntitle: B\ndescription: B.\n---\nSee [gone](/gone.md).\n"),
        at: Utc::now(),
    };
    let changeset = pending_entry(&test, &handle, &edit).await;
    // The catalog grows past the budget behind this app's back.
    let big = note(8 * 1024);
    let generation = import_behind(test.app.accounts().executor(), "pg", &[("a.md", &big)]).await;

    let input = PublishInput {
        changeset: &changeset,
        actor: "writer",
        message: "refused",
    };
    let refused = publish(&test.app, &pg, &input, &AuditActor::cli()).await;
    cache.settle().await;

    assert_eq!(refused.unwrap_err().kind(), "LintFailed");
    assert_eq!(pg.current().generation(), generation, "refreshed in place");
    assert!(cache.resident(pg.id()).await.is_none(), "not kept");
    assert_eq!(
        sample(&registry, r#"okf_cache_bytes{cache="catalog"}"#),
        0.0
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_load_overtaken_by_a_catalog_message_is_served_but_not_kept() {
    let (test, _registry) = app(&[("pg", &[("a.md", "# A\n")])], ByteSize::gib(1).as_u64()).await;
    let pool = crate::db::store_pool(test.app.accounts().executor());
    let pg = row(&test.app, "pg").await;
    // Hold the documents table, so the load reads the generation and then
    // waits; release it on every path (dropping the transaction rolls back).
    let mut hold = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE catalog.documents IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *hold)
        .await
        .unwrap();
    let loading = {
        let app = Arc::clone(&test.app);
        let pg = Arc::clone(&pg);
        tokio::spawn(async move { app.catalog_state(&pg).await })
    };
    timeout(BOUND, async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity \
                 WHERE wait_event_type = 'Lock' AND query LIKE '%documents%'",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            if waiting > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the load waits on the documents table");

    // A publish committed elsewhere: its message arrives while the load,
    // which read the older generation, is still running.
    test.app
        .apply(&message(Invalidation::Catalog {
            id: pg.id,
            generation: pg.generation + 1,
        }))
        .await;
    hold.rollback().await.unwrap();
    let served = timeout(BOUND, loading)
        .await
        .expect("the load finishes once the table is released")
        .unwrap()
        .unwrap();

    assert_eq!(served.current().generation(), pg.generation, "served");
    assert!(
        test.app.catalog_cache().resident(pg.id).await.is_none(),
        "but not kept"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deleted_catalog_leaves_the_cache_on_its_message() {
    let (test, _registry) = app(&[("pg", &[("a.md", "# A\n")])], ByteSize::gib(1).as_u64()).await;
    let state = test.app.catalog("pg").await.unwrap().unwrap();
    let sent =
        crate::invalidate::testing::invalidations_of(test.app.accounts().executor(), async {
            crate::store::catalogs::delete_settled(
                test.app.accounts().executor(),
                "pg",
                false,
                &AuditActor::cli(),
                Utc::now(),
            )
            .await
            .unwrap();
        })
        .await;

    for invalidation in sent {
        test.app.apply(&message(invalidation)).await;
    }

    assert!(test
        .app
        .catalog_cache()
        .resident(state.id())
        .await
        .is_none());
    assert!(test.app.catalog("pg").await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_description_change_keeps_the_snapshot() {
    let (test, _registry) = app(&[("pg", &[("a.md", "# A\n")])], ByteSize::gib(1).as_u64()).await;
    let state = test.app.catalog("pg").await.unwrap().unwrap();
    let before = state.current();
    let change = crate::store::catalogs::DetailsChange {
        catalog: "pg",
        display_name: None,
        description: Some("source of truth"),
        owner: None,
    };
    let sent =
        crate::invalidate::testing::invalidations_of(test.app.accounts().executor(), async {
            crate::store::catalogs::update_details(
                test.app.accounts().executor(),
                &change,
                &AuditActor::cli(),
                Utc::now(),
            )
            .await
            .unwrap();
        })
        .await;

    for invalidation in sent {
        test.app.apply(&message(invalidation)).await;
    }

    // The row carries the new description; the snapshot was not rebuilt.
    assert_eq!(row(&test.app, "pg").await.description, "source of truth");
    let again = test.app.catalog("pg").await.unwrap().unwrap();
    assert!(Arc::ptr_eq(&again, &state));
    assert!(Arc::ptr_eq(&before, &again.current()));
}

#[test]
fn the_weight_charges_bytes_files_and_the_catalog_and_saturates_at_u64() {
    let fixed = PER_CATALOG_BYTES;
    let per_file = PER_FILE_BYTES;
    let factor = WEIGHT_FACTOR;

    assert_eq!(weight(0, 0), fixed, "an empty catalog is not free");
    assert_eq!(
        weight(0, 5_000),
        fixed + 5_000 * per_file,
        "nor are empty files"
    );
    assert_eq!(weight(1000, 1), fixed + per_file + 1000 * factor);
    assert_eq!(weight(u64::MAX, 0), u64::MAX);
    assert_eq!(weight(0, u64::MAX), u64::MAX);
}

/// Catalogs of empty files are charged by their files: a budget of a few
/// such catalogs keeps no more than it holds, however many are read.
#[tokio::test(flavor = "multi_thread")]
async fn catalogs_of_empty_files_are_charged_and_evicted() {
    let empty: Vec<(String, String)> = (0..200)
        .map(|index| (format!("notes/empty-{index:03}.md"), String::new()))
        .collect();
    let files: Vec<(&str, &str)> = empty
        .iter()
        .map(|(path, text)| (path.as_str(), text.as_str()))
        .collect();
    let names = ["e0", "e1", "e2", "e3", "e4", "e5"];
    let catalogs: Vec<(&str, &[(&str, &str)])> =
        names.iter().map(|name| (*name, files.as_slice())).collect();
    // Room for two of them: each weighs about 200 KiB.
    let each = weight(0, 200);
    let (test, registry) = app(&catalogs, each * 5 / 2).await;
    let cache = test.app.catalog_cache();

    let mut states = Vec::new();
    for name in names {
        let state = test.app.catalog(name).await.unwrap().unwrap();
        assert_eq!(state.current().stored_bytes(), 0, "{name} holds no bytes");
        states.push(state);
    }
    cache.settle().await;

    let mut resident = 0;
    for state in &states {
        if cache.resident(state.id()).await.is_some() {
            resident += 1;
        }
    }
    assert!(
        resident <= 2,
        "{resident} of 6 kept within a two-catalog budget"
    );
    assert!(
        sample(&registry, r#"okf_cache_bytes{cache="catalog"}"#) <= (each * 5 / 2) as f64,
        "the byte gauge stays within the budget"
    );
    assert!(cache.cached() >= 1, "the budget still holds what fits");
}

#[test]
fn only_data_failures_are_remembered_as_broken() {
    let deleted = OkfError::CatalogNotSelected {
        catalog: "pg".to_string(),
    };
    let database = OkfError::Database {
        source: sqlx::Error::PoolTimedOut,
    };
    let busy = OkfError::Busy {
        reason: "shared".to_string(),
    };
    let bad_data = OkfError::InvalidPath {
        path: "../escape.md".to_string(),
        reason: "escapes the catalog".to_string(),
    };

    assert!(!from_stored_data(&deleted));
    assert!(!from_stored_data(&database));
    assert!(!from_stored_data(&busy));
    assert!(from_stored_data(&bad_data));
}
