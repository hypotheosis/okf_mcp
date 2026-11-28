//! Tests for creating a catalog in two steps and for recovering one that
//! stopped half-way (plan Task 10b, decision P9).

use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};
use serde_json::json;
use sqlx::PgPool;
use tokio::sync::Notify;

use super::*;
use crate::db::{test_db, TEST_TEAM, TEST_USER};
use crate::labels::DisplayName;
use crate::store::audit::testing::entries;
use crate::store::audit::AuditActor;

/// The database-side bound the tests give recovery: long enough never to
/// fire in a test that expects a wait.
const WAIT: std::time::Duration = std::time::Duration::from_secs(60);

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 23, 12, 0, 0).unwrap()
}

/// The display name every test catalog has: none.
static NO_DISPLAY_NAME: std::sync::LazyLock<DisplayName> =
    std::sync::LazyLock::new(DisplayName::default);

fn new_catalog(name: &str) -> NewCatalog<'_> {
    NewCatalog {
        name,
        display_name: &NO_DISPLAY_NAME,
        description: "about it",
        owner_team: TEST_TEAM,
    }
}

/// Step 1 only: the registry row exists, unprovisioned, with no store side.
async fn half_made(pool: &PgPool, name: &str) -> (Catalog, Pending) {
    let (catalog, store) = register(pool, &new_catalog(name), &AuditActor::cli(), now())
        .await
        .unwrap();
    let pending = Pending {
        id: catalog.id,
        name: catalog.name.clone(),
        store: store.to_string(),
        created_at: catalog.created_at,
    };
    (catalog, pending)
}

async fn document_count(pool: &PgPool, id: i64) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM catalog.documents WHERE catalog_id = $1")
        .bind(id)
        .fetch_one(&crate::db::store_pool(pool))
        .await
        .unwrap()
}

async fn row_exists(pool: &PgPool, id: i64) -> bool {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM catalogs WHERE id = $1)")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Waits until `task` ends or Postgres reports a session waiting for any
/// lock: the condition the concurrency tests need, with no sleep. Returns
/// whether the waiter waits for an advisory lock, the catalog's lock (a
/// session blocked on anything else means the lock did not do its job).
async fn wait_for_waiter_or_end<T>(pool: &PgPool, task: &tokio::task::JoinHandle<T>) -> bool {
    loop {
        if task.is_finished() {
            return false;
        }
        let waiting: Option<String> = sqlx::query_scalar(
            // This test's databases only (the account database and, in
            // `separate` store mode, its store): the server is shared.
            concat!(
                "SELECT l.locktype FROM pg_locks l JOIN pg_stat_activity a ON a.pid = l.pid \
                 WHERE NOT l.granted AND ",
                crate::test_locks::own_databases!("a.datname"),
                " LIMIT 1"
            ),
        )
        .fetch_optional(pool)
        .await
        .unwrap();
        if let Some(locktype) = waiting {
            return locktype == "advisory";
        }
        tokio::task::yield_now().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_half_created_catalog_is_not_visible() {
    let db = test_db().await;
    let (catalog, _) = half_made(&db.pool, "half").await;
    let actor = AuditActor::cli();

    assert!(matches!(
        find(&db.pool, "half").await,
        Err(OkfError::CatalogNotSelected { .. })
    ));
    assert!(list(&db.pool).await.unwrap().is_empty());
    assert_eq!(row_by_name(&db.pool, "half").await.unwrap(), None);
    assert_eq!(row_by_id(&db.pool, catalog.id).await.unwrap(), None);
    // Nothing that finds a catalog by name can act on it either.
    assert!(matches!(
        delete(&db.pool, "half", true, &actor, now()).await,
        Err(OkfError::CatalogNotSelected { .. })
    ));
    assert!(matches!(
        set_owner(&db.pool, "half", TEST_TEAM, &actor, now()).await,
        Err(OkfError::CatalogNotSelected { .. })
    ));
    let mut conn = db.pool.acquire().await.unwrap();
    let mut store = db.store().acquire().await.unwrap();
    assert!(matches!(
        lock_by_name(&mut conn, &mut store, "half").await,
        Err(OkfError::CatalogNotSelected { .. })
    ));
    // The name is taken, though: a second create is refused.
    drop((conn, store));
    let again = create(&db.pool, &new_catalog("half"), &actor, now()).await;
    assert!(matches!(again, Err(OkfError::Refused { .. })), "{again:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn create_leaves_a_whole_catalog_and_announces_it_once() {
    let db = test_db().await;
    let made = create(&db.pool, &new_catalog("whole"), &AuditActor::cli(), now())
        .await
        .unwrap();

    assert_eq!(find(&db.pool, "whole").await.unwrap(), made);
    assert_eq!(document_count(&db.store(), made.id).await, 2);
    let created = entries(&db.pool, "catalog.created").await;
    assert_eq!(created.len(), 1, "{created:?}");
    let provisioned: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT provisioned_at FROM catalogs WHERE id = $1")
            .bind(made.id)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(provisioned, Some(now()));
}

#[tokio::test(flavor = "multi_thread")]
async fn provisioning_job_finishes_a_catalog_whose_store_side_exists() {
    let db = test_db().await;
    let (catalog, pending) = half_made(&db.pool, "stopped").await;
    // The create got as far as committing its store side.
    let store = CatalogDb::main_of(&db.pool);
    provision_store_side(&store, &catalog, "cli", now(), std::future::ready(()))
        .await
        .unwrap();

    let outcome = recover(&store, &pending, now(), WAIT).await.unwrap();

    assert_eq!(outcome, Recovery::Finished);
    assert_eq!(find(&db.pool, "stopped").await.unwrap().id, catalog.id);
    assert_eq!(document_count(&db.store(), catalog.id).await, 2);
    assert!(entries(&db.pool, "catalog.deleted").await.is_empty());
    // Nothing is left to recover, and a second look changes nothing.
    assert!(pending_rows(&db.pool).await.is_empty());
    assert_eq!(
        recover(&store, &pending, now(), WAIT).await.unwrap(),
        Recovery::Settled
    );
}

async fn pending_rows(pool: &PgPool) -> Vec<Pending> {
    pending(pool, now() + chrono::TimeDelta::days(1), None, &[])
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn provisioning_job_removes_a_catalog_whose_store_side_never_happened() {
    let db = test_db().await;
    let (catalog, pending) = half_made(&db.pool, "lost").await;
    let store = CatalogDb::main_of(&db.pool);

    let outcome = recover(&store, &pending, now(), WAIT).await.unwrap();

    assert_eq!(outcome, Recovery::Removed);
    assert!(!row_exists(&db.pool, catalog.id).await);
    assert_eq!(document_count(&db.store(), catalog.id).await, 0);
    // The failed create is in the audit details, for the owner team.
    let deleted = entries(&db.pool, "catalog.deleted").await;
    assert_eq!(deleted.len(), 1, "{deleted:?}");
    assert_eq!(deleted[0].target.as_deref(), Some("lost"));
    assert_eq!(deleted[0].teams, [TEST_USER]);
    assert_eq!(
        deleted[0].details,
        json!({"forced": false, "generation": 0, "failed_create": true})
    );
    // The name is free again.
    create(&db.pool, &new_catalog("lost"), &AuditActor::cli(), now())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn pending_lists_only_old_unprovisioned_rows_oldest_first() {
    let db = test_db().await;
    let early = now() - chrono::TimeDelta::minutes(5);
    for (name, at) in [
        ("second", now() - chrono::TimeDelta::minutes(2)),
        ("first", early),
    ] {
        register(&db.pool, &new_catalog(name), &AuditActor::cli(), at)
            .await
            .unwrap();
    }
    create(&db.pool, &new_catalog("whole"), &AuditActor::cli(), early)
        .await
        .unwrap();
    register(&db.pool, &new_catalog("young"), &AuditActor::cli(), now())
        .await
        .unwrap();

    let found = pending(&db.pool, now() - chrono::TimeDelta::minutes(1), None, &[])
        .await
        .unwrap();

    let names: Vec<&str> = found.iter().map(|row| row.name.as_str()).collect();
    assert_eq!(names, ["first", "second"]);
    assert_eq!(found[0].store, "main");
}

#[tokio::test(flavor = "multi_thread")]
async fn pending_pages_after_a_cursor_and_leaves_out_skipped_stores() {
    let db = test_db().await;
    let mut rows = Vec::new();
    for (name, minutes) in [("a", 4), ("b", 3), ("c", 2)] {
        let at = now() - chrono::TimeDelta::minutes(minutes);
        let (catalog, _) = register(&db.pool, &new_catalog(name), &AuditActor::cli(), at)
            .await
            .unwrap();
        rows.push(catalog.id);
    }
    sqlx::query(
        "INSERT INTO stores (name, location_kind, host, port, database, credential_kind, \
         credential_ref, created_at) \
         VALUES ('elsewhere', 'host', 'db.example.internal', 5432, 'okf', 'env', 'UNSET', now())",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE catalogs SET store = 'elsewhere' WHERE id = $1")
        .bind(rows[1])
        .execute(&db.pool)
        .await
        .unwrap();
    let cutoff = now();

    let after_first = pending(
        &db.pool,
        cutoff,
        Some((now() - chrono::TimeDelta::minutes(4), rows[0])),
        &[],
    )
    .await
    .unwrap();
    let skipping = pending(&db.pool, cutoff, None, &["elsewhere".to_string()])
        .await
        .unwrap();

    let ids = |found: &[Pending]| found.iter().map(|row| row.id).collect::<Vec<_>>();
    assert_eq!(ids(&after_first), [rows[1], rows[2]]);
    assert_eq!(ids(&skipping), [rows[0], rows[2]]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_create_stalled_past_the_interval_is_not_removed_while_it_holds_the_lock() {
    let db = test_db().await;
    let (catalog, pending) = half_made(&db.pool, "slow").await;
    let store = CatalogDb::main_of(&db.pool);
    let (entered, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));

    // The create has the lock and its files written, and stalls before commit.
    let create_task = {
        let (store, catalog) = (store.clone(), catalog.clone());
        let (entered, release) = (entered.clone(), release.clone());
        tokio::spawn(async move {
            provision_store_side(&store, &catalog, "cli", now(), async move {
                entered.notify_one();
                release.notified().await;
            })
            .await
        })
    };
    entered.notified().await;
    // Recovery looks at it now, as the housekeeping task would after the interval.
    let recovery_task = {
        let (store, pending) = (store.clone(), pending.clone());
        tokio::spawn(async move { recover(&store, &pending, now(), WAIT).await })
    };
    let waited = wait_for_waiter_or_end(&db.pool, &recovery_task).await;
    release.notify_one();

    create_task.await.unwrap().unwrap();
    let outcome = recovery_task.await.unwrap().unwrap();
    assert!(waited, "recovery should wait for the create's lock");
    assert_eq!(outcome, Recovery::Finished, "it saw the create's data");
    assert_eq!(find(&db.pool, "slow").await.unwrap().id, catalog.id);
    assert_eq!(document_count(&db.store(), catalog.id).await, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_create_that_waited_behind_recovery_writes_nothing() {
    let db = test_db().await;
    let (catalog, pending) = half_made(&db.pool, "late").await;
    let store = CatalogDb::main_of(&db.pool);
    let (entered, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));

    // Recovery has the lock and has deleted the row, and stalls before commit.
    let recovery_task = {
        let (store, pending) = (store.clone(), pending.clone());
        let (entered, release) = (entered.clone(), release.clone());
        tokio::spawn(async move {
            recover_holding(&store, &pending, now(), WAIT, async move {
                entered.notify_one();
                release.notified().await;
            })
            .await
        })
    };
    entered.notified().await;
    // The stalled create wakes up and runs its store step.
    let create_task = {
        let (store, catalog) = (store.clone(), catalog.clone());
        tokio::spawn(async move {
            provision_store_side(&store, &catalog, "cli", now(), std::future::ready(())).await
        })
    };
    let waited = wait_for_waiter_or_end(&db.pool, &create_task).await;
    release.notify_one();

    assert_eq!(recovery_task.await.unwrap().unwrap(), Recovery::Removed);
    let aborted = create_task.await.unwrap();
    assert!(waited, "the create should wait for recovery's lock");
    assert!(matches!(aborted, Err(OkfError::Busy { .. })), "{aborted:?}");
    assert!(!row_exists(&db.pool, catalog.id).await);
    assert_eq!(document_count(&db.store(), catalog.id).await, 0);
}

/// The pool rule: a create and a recovery each need one connection at a
/// time, so a pool of one is enough (`database_max_connections = 1`). The
/// timeout turns a regression into a failure instead of a hang.
#[tokio::test(flavor = "multi_thread")]
async fn create_and_recovery_work_on_a_one_connection_pool() {
    let db = test_db().await;
    let one = crate::db::connect(&db.url, 1)
        .await
        .expect("a one-connection pool");
    let limit = std::time::Duration::from_secs(20);

    let made = tokio::time::timeout(
        limit,
        create(&one, &new_catalog("solo"), &AuditActor::cli(), now()),
    )
    .await
    .expect("create finished")
    .unwrap();
    assert_eq!(document_count(&one, made.id).await, 2);

    let (_, lost) = half_made(&one, "solo-lost").await;
    let (_, stopped_catalog) = {
        let (catalog, pending) = half_made(&one, "solo-stopped").await;
        (catalog, pending)
    };
    let store = CatalogDb::main_of(&one);
    let removed = tokio::time::timeout(limit, recover(&store, &lost, now(), WAIT))
        .await
        .expect("recovery finished")
        .unwrap();
    assert_eq!(removed, Recovery::Removed);
    let finished = tokio::time::timeout(limit, async {
        let catalog = find_half(&one, stopped_catalog.id).await;
        provision_store_side(&store, &catalog, "cli", now(), std::future::ready(())).await?;
        recover(&store, &stopped_catalog, now(), WAIT).await
    })
    .await
    .expect("recovery finished")
    .unwrap();
    assert_eq!(finished, Recovery::Finished);
}

/// A half-made row as the `Catalog` step 2 takes, read without the
/// provisioned filter.
async fn find_half(pool: &PgPool, id: i64) -> Catalog {
    sqlx::query_as::<_, Catalog>(
        "SELECT id, name, display_name, description, 0::bigint AS generation, created_at, \
                NULL::text AS owner_team, store FROM catalogs WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}
