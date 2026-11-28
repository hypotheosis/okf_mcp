//! Per-store housekeeping (catalog stores plan Task 16, issue #976): the
//! jobs keyed `"<task>:<store>:<slot>"`, scheduled for every store in the
//! `stores` table, each touching its own store only, with every wait
//! bounded.
//!
//! The stores are registered as `host` stores on databases of their own
//! (`TestDb::extra_store_database`), so the tests mean the same in both
//! `OKF_TEST_STORE` modes. Nothing here sleeps to wait: a test waits on a
//! job's status, with a bound.

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, TimeDelta, Utc};

use super::tests::{
    changeset_on, fixed_now, jobs, runs, start, stop, wait_for, writer_key, PATIENCE,
};
use super::*;
use crate::clock::SystemClock;
use crate::config::Config;
use crate::db::pg_support::TestDb;
use crate::db::test_db;
use crate::invalidate::store_feed_tests::{add_store, catalog_on};
use crate::mcp::testing::local_metrics;
use crate::store::changesets::ChangesetState;

/// A replica: an app on a pool of its own over the test's account
/// database, with its own instruments.
struct Replica {
    app: Arc<App>,
    _provider: opentelemetry_sdk::metrics::SdkMeterProvider,
    registry: prometheus::Registry,
}

async fn replica(db: &TestDb) -> Replica {
    replica_sized(db, 5, Config::default()).await
}

/// [`replica`] with an account pool of `connections` and `config`.
async fn replica_sized(db: &TestDb, connections: u32, config: Config) -> Replica {
    let (provider, metrics, registry) = local_metrics();
    let pool = crate::db::connect(&db.url, connections).await.unwrap();
    let app = App::load_with_metrics(pool, config, Arc::new(SystemClock), metrics)
        .await
        .unwrap();
    Replica {
        app,
        _provider: provider,
        registry,
    }
}

fn store(name: &str) -> StoreName {
    StoreName::parse(name).unwrap()
}

/// Every job key of the slots containing `now`, for `stores`, sorted.
fn keys_for(stores: &[StoreName], now: DateTime<Utc>) -> Vec<String> {
    let mut keys: Vec<String> = Task::ALL
        .into_iter()
        .flat_map(|task| {
            let slot = task.slot_at(now);
            let cluster = task
                .runs_on_cluster()
                .then(|| Housekeeping::cluster(task, slot).key());
            let per_store = stores
                .iter()
                .filter(move |_| task.runs_per_store())
                .map(move |store| Housekeeping::on_store(task, store.clone(), slot).key());
            cluster.into_iter().chain(per_store)
        })
        .collect();
    keys.sort();
    keys
}

/// The scheduled jobs' keys, sorted.
async fn scheduled_keys(pool: &sqlx::PgPool) -> Vec<String> {
    let mut keys: Vec<String> = jobs(pool).await.into_iter().map(|(key, ..)| key).collect();
    keys.sort();
    keys
}

/// Creates catalog `name` on `on` (`None`: the default store) with an empty
/// changeset of key `writer` last changed at `at`.
///
/// # Returns
/// The store's handle and the changeset's id.
async fn changeset_in_new_catalog(
    replica: &Replica,
    db: &TestDb,
    name: &str,
    on: Option<&StoreName>,
    at: DateTime<Utc>,
) -> (CatalogDb, i64) {
    let catalog = catalog_on(replica.app.stores(), &db.pool, name, on).await;
    let store_db = replica
        .app
        .stores()
        .get(on.unwrap_or(&StoreName::main()))
        .await
        .unwrap();
    let id = changeset_on(&store_db, catalog.id, at, None).await;
    (store_db, id)
}

async fn state_on(db: &CatalogDb, id: i64) -> ChangesetState {
    changesets::by_id(db.executor(), id)
        .await
        .unwrap()
        .unwrap()
        .state
}

/// The `cluster_store_stats` rows, by store.
async fn store_counts(pool: &sqlx::PgPool) -> Vec<(String, i64)> {
    sqlx::query_as("SELECT store, open_changesets FROM cluster_store_stats ORDER BY store")
        .fetch_all(pool)
        .await
        .unwrap()
}

/// A day and an hour ago: older than the empty-changeset limit.
fn day_old() -> DateTime<Utc> {
    Utc::now() - TimeDelta::hours(25)
}

#[tokio::test(flavor = "multi_thread")]
async fn per_store_housekeeping_runs_once_per_store_and_slot_across_replicas() {
    // Arrange: two separate stores, each with a catalog holding an empty
    // changeset a day old, and two replicas on one account database.
    let db = test_db().await;
    writer_key(&db.pool).await;
    let shared = [store("shared-1"), store("shared-2")];
    for name in &shared {
        add_store(&db, name.as_str()).await;
    }
    let a = replica(&db).await;
    let b = replica(&db).await;
    let mut stale = Vec::new();
    for (name, catalog) in shared.iter().zip(["one", "two"]) {
        stale.push(changeset_in_new_catalog(&a, &db, catalog, Some(name), day_old()).await);
    }
    let now = fixed_now();
    let every_store = [StoreName::main(), shared[0].clone(), shared[1].clone()];
    let expected = keys_for(&every_store, now);

    // Act: both replicas push the slots of `now`, twice each, all at once;
    // then both workers run whatever was pushed.
    tokio::time::timeout(PATIENCE, async {
        tokio::join!(
            schedule_slots(&a.app, now),
            schedule_slots(&b.app, now),
            schedule_slots(&a.app, now),
            schedule_slots(&b.app, now),
        )
    })
    .await
    .expect("every push returns");
    let scheduled = scheduled_keys(&db.pool).await;
    let worker_a = start(&a.app, "replica-a");
    let worker_b = start(&b.app, "replica-b");
    for key in &expected {
        wait_for(&db.pool, key, "Done").await;
    }
    stop(worker_a).await;
    stop(worker_b).await;

    // Assert: one job per task, store and slot, each run once in the
    // cluster, and each store's work done on that store.
    assert_eq!(scheduled, expected);
    let finished = jobs(&db.pool).await;
    assert!(
        finished
            .iter()
            .all(|(_, status, attempts)| status == "Done" && *attempts == 1),
        "{finished:?}"
    );
    for task in Task::ALL {
        let prefix = format!("{}:", task.as_str());
        let pushed = expected
            .iter()
            .filter(|key| key.starts_with(&prefix))
            .count();
        let ran = runs(&a.registry, task, "ok") + runs(&b.registry, task, "ok");
        assert_eq!(ran, pushed as f64, "{task:?} ran once per job");
    }
    for (store_db, id) in &stale {
        assert_eq!(
            state_on(store_db, *id).await,
            ChangesetState::Discarded,
            "pruned on {}",
            store_db.store()
        );
    }
    let counted: Vec<String> = store_counts(&db.pool)
        .await
        .into_iter()
        .map(|(store, _)| store)
        .collect();
    assert_eq!(counted, ["main", "shared-1", "shared-2"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_store_added_after_start_gets_its_jobs_at_the_next_pass() {
    // Arrange: a running replica that has scheduled one pass.
    let db = test_db().await;
    let replica = replica(&db).await;
    let now = fixed_now();
    schedule_slots(&replica.app, now).await;
    let before = scheduled_keys(&db.pool).await;

    // Act: a store is added, and the next pass runs (same slots).
    add_store(&db, "shared-1").await;
    schedule_slots(&replica.app, now).await;
    let after = scheduled_keys(&db.pool).await;

    // Assert: the new store has its jobs, without a restart or an open.
    assert_eq!(before, keys_for(&[StoreName::main()], now));
    assert_eq!(
        after,
        keys_for(&[StoreName::main(), store("shared-1")], now)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn one_store_failing_does_not_hold_up_the_others() {
    // Arrange: `main` and two separate stores with an empty changeset a day
    // old each; then the database of `shared-2` is dropped.
    let db = test_db().await;
    writer_key(&db.pool).await;
    let up = store("shared-1");
    let down = store("shared-2");
    add_store(&db, up.as_str()).await;
    let down_database = add_store(&db, down.as_str()).await;
    let replica = replica(&db).await;
    let on_main = changeset_in_new_catalog(&replica, &db, "zero", None, day_old()).await;
    let on_up = changeset_in_new_catalog(&replica, &db, "one", Some(&up), day_old()).await;
    changeset_in_new_catalog(&replica, &db, "two", Some(&down), day_old()).await;
    // `AssertSqlSafe`: the name is the harness's own, quoted.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE \"{down_database}\" WITH (FORCE)"
    )))
    .execute(&db.pool)
    .await
    .unwrap();
    let now = fixed_now();

    // Act: the slots are pushed and the worker runs them.
    schedule_slots(&replica.app, now).await;
    let worker = start(&replica.app, "replica-a");
    for key in keys_for(&[StoreName::main(), up.clone()], now) {
        wait_for(&db.pool, &key, "Done").await;
    }
    let failed: Vec<String> = Task::ALL
        .into_iter()
        .filter(|task| task.runs_per_store())
        .map(|task| Housekeeping::on_store(task, down.clone(), task.slot_at(now)).key())
        .collect();
    for key in &failed {
        wait_for(&db.pool, key, "Failed").await;
    }
    stop(worker).await;

    // Assert: every other store's work is done; the dropped store's jobs
    // failed (to be retried) and it has no count.
    assert_eq!(
        state_on(&on_main.0, on_main.1).await,
        ChangesetState::Discarded
    );
    assert_eq!(state_on(&on_up.0, on_up.1).await, ChangesetState::Discarded);
    let counted: Vec<String> = store_counts(&db.pool)
        .await
        .into_iter()
        .map(|(store, _)| store)
        .collect();
    assert_eq!(counted, ["main", "shared-1"]);
    let down_jobs: Vec<(String, String, i32)> = jobs(&db.pool)
        .await
        .into_iter()
        .filter(|(key, ..)| failed.contains(key))
        .collect();
    assert_eq!(down_jobs.len(), failed.len(), "{down_jobs:?}");
    assert!(
        down_jobs
            .iter()
            .all(|(_, status, attempts)| status == "Failed" && *attempts >= 1),
        "tried, and put back for a retry: {down_jobs:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cluster_stats_sums_open_changesets_across_stores() {
    // Arrange: one open changeset on `main`, two on `shared-1`, three on
    // `shared-2`.
    let db = test_db().await;
    writer_key(&db.pool).await;
    let shared = [store("shared-1"), store("shared-2")];
    let mut databases = Vec::new();
    for name in &shared {
        databases.push(add_store(&db, name.as_str()).await);
    }
    let replica = replica(&db).await;
    let now = Utc::now();
    let mut handles = Vec::new();
    for (open, (name, on)) in [
        (1, ("zero", None)),
        (2, ("one", Some(&shared[0]))),
        (3, ("two", Some(&shared[1]))),
    ] {
        let catalog = catalog_on(replica.app.stores(), &db.pool, name, on).await;
        let store_db = replica
            .app
            .stores()
            .get(on.unwrap_or(&StoreName::main()))
            .await
            .unwrap();
        for _ in 0..open {
            changeset_on(&store_db, catalog.id, now, None).await;
        }
        handles.push((store_db, catalog.id));
    }
    let every_store = [StoreName::main(), shared[0].clone(), shared[1].clone()];
    let count_all = || async {
        for name in &every_store {
            // A failure is that store's alone; the rest are still counted.
            let _ = run_on_store(Task::ClusterStats, name, &replica.app).await;
        }
        run(Task::ClusterStats, &replica.app).await.unwrap();
        read_stats(&db.pool).await.unwrap().unwrap().open_changesets
    };

    // Act
    let summed = count_all().await;
    let rows = store_counts(&db.pool).await;
    // Then `shared-1` gains a changeset and `shared-2` goes away: its count
    // fails, and the sum keeps its last known one.
    changeset_on(&handles[1].0, handles[1].1, now, None).await;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE \"{}\" WITH (FORCE)",
        databases[1]
    )))
    .execute(&db.pool)
    .await
    .unwrap();
    let after_loss = count_all().await;

    // Assert
    assert_eq!(summed, 6);
    assert_eq!(
        rows,
        [
            ("main".to_string(), 1),
            ("shared-1".to_string(), 2),
            ("shared-2".to_string(), 3),
        ]
    );
    assert_eq!(after_loss, 7, "1 + 3 counted now, plus shared-2's last 3");
}

/// Every per-store step is bounded at the database: a lock held on the
/// table it needs fails the step with the database's lock timeout (not the
/// process's backstop), no statement is left waiting, and the same job
/// succeeds once the lock is gone.
#[tokio::test(flavor = "multi_thread")]
async fn a_store_job_blocked_on_a_lock_gives_up_at_the_database() {
    let db = test_db().await;
    let shared = store("shared-1");
    add_store(&db, shared.as_str()).await;
    let replica = replica(&db).await;
    catalog_on(replica.app.stores(), &db.pool, "one", Some(&shared)).await;
    let store_db = replica.app.stores().get(&shared).await.unwrap();
    let limits = Limits {
        attempt: Duration::from_secs(30),
        wait: Duration::from_millis(500),
    };

    for (task, table) in [
        (Task::PruneChangesets, "changesets"),
        (Task::ClusterStats, "changesets"),
        (Task::PruneAudit, "catalog_audit_entries"),
    ] {
        // Arrange: another connection to the store holds the table.
        let mut holder = store_db.pool().begin().await.unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "LOCK TABLE {table} IN ACCESS EXCLUSIVE MODE"
        )))
        .execute(&mut *holder)
        .await
        .unwrap();
        let holder_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *holder)
            .await
            .unwrap();

        // Act
        let started = Instant::now();
        let blocked = tokio::time::timeout(
            PATIENCE,
            run_on_store_bounded(task, &shared, &replica.app, limits),
        )
        .await
        .expect("the job returns while the lock is held");
        let elapsed = started.elapsed();
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity WHERE pg_blocking_pids(pid) @> ARRAY[$1]::int[]",
        )
        .bind(holder_pid)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        holder.rollback().await.unwrap();
        let after = run_on_store_bounded(task, &shared, &replica.app, limits).await;

        // Assert
        assert!(
            matches!(blocked, Err(OkfError::Database { .. })),
            "{task:?}: the database's lock timeout, not the backstop: {blocked:?}"
        );
        assert!(elapsed < Duration::from_secs(10), "{task:?}: {elapsed:?}");
        assert_eq!(waiting, 0, "{task:?}: nothing still waits on the lock");
        assert!(after.is_ok(), "{task:?}: {after:?}");
    }
}

/// No store job holds a connection of one database while it waits for one
/// of the other, or a second of the same: with one account connection and
/// one connection per store, every per-store job and the cluster sum finish.
#[tokio::test(flavor = "multi_thread")]
async fn store_jobs_finish_on_pools_of_one_connection() {
    let db = test_db().await;
    writer_key(&db.pool).await;
    let shared = store("shared-1");
    add_store(&db, shared.as_str()).await;
    let config = Config {
        store_max_connections: 1,
        ..Config::default()
    };
    let replica = replica_sized(&db, 1, config).await;
    let (store_db, id) =
        changeset_in_new_catalog(&replica, &db, "one", Some(&shared), day_old()).await;

    let finished = tokio::time::timeout(PATIENCE, async {
        for task in Task::ALL.into_iter().filter(|task| task.runs_per_store()) {
            for name in [StoreName::main(), shared.clone()] {
                run_on_store(task, &name, &replica.app)
                    .await
                    .unwrap_or_else(|error| panic!("{task:?} on {name}: {error:?}"));
            }
        }
        run(Task::ClusterStats, &replica.app).await.unwrap();
    })
    .await;

    assert!(finished.is_ok(), "a job waited on a connection it holds");
    assert_eq!(state_on(&store_db, id).await, ChangesetState::Discarded);
    assert_eq!(
        store_counts(&db.pool).await,
        [("main".to_string(), 0), ("shared-1".to_string(), 0)]
    );
}

/// What a recovery test needs: a replica whose bounds are short, an older
/// half-made catalog on `shared-1` (the store the test stalls), and a newer
/// one on `shared-2`, which is healthy. Both have their store side, so a
/// recovery that gets to them finishes them.
struct Pending {
    db: TestDb,
    replica: Replica,
    /// The older catalog's id, on the store the test stalls.
    stalled_id: i64,
    /// A connection to the stalled store, to hold a lock on.
    stalled_store: CatalogDb,
}

async fn two_half_made_catalogs() -> Pending {
    let db = test_db().await;
    let (stalled, healthy) = (store("shared-1"), store("shared-2"));
    add_store(&db, stalled.as_str()).await;
    add_store(&db, healthy.as_str()).await;
    let config = Config {
        store_connect_timeout: Duration::from_millis(500),
        ..Config::default()
    };
    let replica = replica_sized(&db, 5, config).await;
    let older = catalog_on(replica.app.stores(), &db.pool, "older", Some(&stalled)).await;
    catalog_on(replica.app.stores(), &db.pool, "newer", Some(&healthy)).await;
    let now = replica.app.clock().now();
    for (name, minutes) in [("older", 90), ("newer", 60)] {
        sqlx::query("UPDATE catalogs SET provisioned_at = NULL, created_at = $2 WHERE name = $1")
            .bind(name)
            .bind(now - TimeDelta::minutes(minutes))
            .execute(&db.pool)
            .await
            .unwrap();
    }
    let stalled_store = replica.app.stores().get(&stalled).await.unwrap();
    Pending {
        db,
        replica,
        stalled_id: older.id,
        stalled_store,
    }
}

/// Whether catalog `name` is provisioned.
async fn is_provisioned(db: &TestDb, name: &str) -> bool {
    sqlx::query_scalar("SELECT provisioned_at IS NOT NULL FROM catalogs WHERE name = $1")
        .bind(name)
        .fetch_one(&db.pool)
        .await
        .unwrap()
}

/// Runs the recovery with `limits` under [`PATIENCE`], so a recovery that
/// waits on the held lock fails the test instead of hanging it.
async fn recover_within_patience(
    test: &Pending,
    limits: Limits,
) -> Result<Result<(), OkfError>, tokio::time::error::Elapsed> {
    tokio::time::timeout(
        PATIENCE,
        provision_catalogs_bounded(&test.replica.app, limits),
    )
    .await
}

/// Asserts the failure is PostgreSQL's own `lock_timeout` (`55P03`) or
/// `statement_timeout` (`57014`), not the process backstop's `Busy`: without
/// the database-side bounds the attempt would end as `Busy` or not at all.
fn assert_database_timeout(outcome: &Result<(), OkfError>) {
    let code = match outcome {
        Err(OkfError::Database { source }) => source
            .as_database_error()
            .and_then(|error| error.code())
            .map(|code| code.into_owned()),
        _ => None,
    };
    assert!(
        matches!(code.as_deref(), Some("55P03" | "57014")),
        "expected the database's own timeout, got {outcome:?}"
    );
}

/// The catalog lock of the older catalog is held by another connection, so
/// recovery of it waits. It gives up at the database, the store is skipped
/// for the run, the first failure is the task's result, and the newer
/// catalog on the healthy store settles while the lock is still held.
#[tokio::test(flavor = "multi_thread")]
async fn a_held_catalog_lock_does_not_starve_a_healthy_store() {
    let test = two_half_made_catalogs().await;
    let mut holder = test.stalled_store.pool().begin().await.unwrap();
    catalogs::lock(&mut holder, test.stalled_id).await.unwrap();
    let limits = provision_limits(&test.replica.app);

    let outcome = recover_within_patience(&test, limits).await;

    let outcome = outcome.expect("the recovery waited on the held lock");
    assert_database_timeout(&outcome);
    assert!(
        is_provisioned(&test.db, "newer").await,
        "the healthy store settled"
    );
    assert!(
        !is_provisioned(&test.db, "older").await,
        "the row stays pending"
    );
    // The lock is released only now: the next run finishes the older one.
    holder.rollback().await.unwrap();
    provision_catalogs_bounded(&test.replica.app, limits)
        .await
        .unwrap();
    assert!(is_provisioned(&test.db, "older").await);
}

/// A store query stalls (a table lock another connection holds): the same
/// bounds apply, whatever the statement.
#[tokio::test(flavor = "multi_thread")]
async fn a_stalled_store_query_does_not_starve_a_healthy_store() {
    let test = two_half_made_catalogs().await;
    let mut holder = test.stalled_store.pool().begin().await.unwrap();
    sqlx::query("LOCK TABLE catalog_state IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *holder)
        .await
        .unwrap();
    let limits = provision_limits(&test.replica.app);

    let outcome = recover_within_patience(&test, limits).await;

    let outcome = outcome.expect("the recovery waited on the held table");
    assert_database_timeout(&outcome);
    assert!(is_provisioned(&test.db, "newer").await);
    assert!(!is_provisioned(&test.db, "older").await);
    holder.rollback().await.unwrap();
}

/// The whole-attempt backstop, with a database bound too long to fire: the
/// attempt is dropped as `Busy`, the store skipped, the healthy one settled.
#[tokio::test(flavor = "multi_thread")]
async fn the_attempt_backstop_contains_a_wait_the_database_does_not_bound() {
    let test = two_half_made_catalogs().await;
    let mut holder = test.stalled_store.pool().begin().await.unwrap();
    catalogs::lock(&mut holder, test.stalled_id).await.unwrap();
    let limits = Limits {
        attempt: Duration::from_millis(500),
        wait: Duration::from_secs(120),
    };

    let outcome = recover_within_patience(&test, limits).await;

    let outcome = outcome.expect("the backstop gave up");
    assert!(matches!(outcome, Err(OkfError::Busy { .. })), "{outcome:?}");
    assert!(is_provisioned(&test.db, "newer").await);
    assert!(!is_provisioned(&test.db, "older").await);
    holder.rollback().await.unwrap();
}
