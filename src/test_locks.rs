//! Test helpers that decide which heavyweight lock a backend waits on.
//!
//! They read `pg_locks`, never `pg_stat_activity.query`. `pg_stat_activity`
//! takes its query text and state from a snapshot made once per transaction,
//! while `wait_event_type` and `pg_blocking_pids` are read live, so a backend
//! that was idle when the snapshot was taken and is blocked now shows the
//! text of the statement it ran before (issues #372 and #639). `pg_locks`
//! lists the lock each waiter has requested and not been granted, read when
//! the query runs.

use std::time::Duration;

use sqlx::{PgConnection, PgPool};

/// A SQL condition on the column `$column` (a literal such as `"datname"`):
/// true for this test's own databases and no other test's. These are the
/// account database (`current_database()`, or the SQL expression `$database`),
/// its store `{name}_store` and the databases `{name}_x...` that
/// `TestDb::extra_store_database` makes in `separate` store mode.
///
/// A prefix match is not enough: every test database name ends in a counter
/// without padding, so `..._1` would match `..._10` and `..._11` of another
/// test made in the same second. The conditions here are an exact name, an
/// exact `_store` suffix and an `_x` child (account names end in digits).
macro_rules! own_databases {
    ($column:literal) => {
        $crate::test_locks::own_databases!($column, "current_database()")
    };
    ($column:literal, $database:literal) => {
        concat!(
            "(",
            $column,
            " = ",
            $database,
            " OR ",
            $column,
            " = ",
            $database,
            " || '_store'",
            " OR left(",
            $column,
            ", length(",
            $database,
            ") + 2) = ",
            $database,
            " || '_x')"
        )
    };
}
pub(crate) use own_databases;

/// How long a helper waits for the lock wait it looks for.
const WAIT_LIMIT: Duration = Duration::from_secs(10);

/// A session queued for a heavyweight lock that another session holds, as
/// `pg_locks` shows it.
#[derive(Debug)]
pub(crate) struct Waiter {
    /// The waiting session's backend pid.
    pub(crate) pid: i32,
    /// `pg_locks.locktype` of the lock it waits for (`advisory`,
    /// `transactionid`, `tuple`, `relation`, ...).
    pub(crate) locktype: String,
    /// For an advisory lock taken with the two-key form, the first key
    /// (`classid`); `None` for other lock types.
    pub(crate) key1: Option<i64>,
    /// For an advisory lock taken with the two-key form, the second key
    /// (`objid`); `None` for other lock types.
    pub(crate) key2: Option<i64>,
}

/// Waits (up to 10 s) until some session is queued for a lock that `holder`
/// holds, and returns that session and the lock it waits for.
///
/// # Arguments
/// * `conn` - the connection to poll from.
/// * `holder` - the backend pid holding the lock.
///
/// # Returns
/// The first waiter (lowest pid) queued behind `holder`.
///
/// # Panics
/// When no session waits on `holder` within 10 s, or a query fails.
pub(crate) async fn wait_waiter(conn: &mut PgConnection, holder: i32) -> Waiter {
    tokio::time::timeout(WAIT_LIMIT, async {
        loop {
            // `classid`/`objid` are `oid`s, cast so they decode as `i64`.
            let row: Option<(i32, String, Option<i64>, Option<i64>)> = sqlx::query_as(
                "SELECT pid, locktype, classid::int8, objid::int8 FROM pg_locks \
                 WHERE NOT granted AND $1 = ANY(pg_blocking_pids(pid)) ORDER BY pid LIMIT 1",
            )
            .bind(holder)
            .fetch_optional(&mut *conn)
            .await
            .unwrap();
            if let Some((pid, locktype, key1, key2)) = row {
                return Waiter {
                    pid,
                    locktype,
                    key1,
                    key2,
                };
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("no session queued for a lock the holder holds")
}

/// Waits until some session is queued for a row lock on `table` that
/// `holder` holds, and returns that session's pid.
///
/// See [`wait_row_lock_waiters`] for what counts as a wait on a row.
///
/// # Arguments
/// * `pool` - the pool to poll from; the pids and `pg_locks` are read from
///   the database the pool connects to.
/// * `holder` - the backend pid holding the row.
/// * `table` - the table whose row is locked (any name `regclass` accepts).
/// * `limit` - how long to wait.
///
/// # Returns
/// The pid of the session that waits for `holder` itself (the lowest such
/// pid), or `None` when none did within `limit`.
///
/// # Panics
/// When a query fails.
pub(crate) async fn wait_row_lock_waiter(
    pool: &PgPool,
    holder: i32,
    table: &str,
    limit: Duration,
) -> Option<i32> {
    wait_row_lock_waiters(pool, holder, table, 1, limit)
        .await
        .map(|pids| pids[0])
}

/// Waits until at least `count` sessions are queued for the row lock on
/// `table` that `holder` holds, and returns their pids.
///
/// A session that must wait for a row lock first takes the row's `tuple`
/// lock (granted, on the table's relation), then waits for the holder's
/// `transactionid` lock (not granted). So the wait on a row of `table` is a
/// not-granted `transactionid` lock together with a granted `tuple` lock on
/// `table`, both held up by `holder`. The `tuple` lock is the only part of
/// the wait that names the table.
///
/// A second session after the same row queues for the first one's `tuple`
/// lock instead (a not-granted `tuple` lock on `table`), so it is behind
/// `holder` only through the first. It counts once some session waits for
/// `holder` itself.
///
/// # Arguments
/// * `pool` - the pool to poll from; the pids and `pg_locks` are read from
///   the database the pool connects to.
/// * `holder` - the backend pid holding the row.
/// * `table` - the table whose row is locked (any name `regclass` accepts).
/// * `count` - how many waiting sessions to wait for.
/// * `limit` - how long to wait.
///
/// # Returns
/// The waiting pids, those waiting for `holder` itself first (each group
/// in pid order), or `None` when fewer than `count` waited within `limit`.
///
/// # Panics
/// When a query fails.
pub(crate) async fn wait_row_lock_waiters(
    pool: &PgPool,
    holder: i32,
    table: &str,
    count: usize,
    limit: Duration,
) -> Option<Vec<i32>> {
    // A table of schema `catalog` lives in the store's database, which is a
    // database of its own in `OKF_TEST_STORE=separate` mode.
    let pool = &if table.starts_with("catalog.") {
        crate::db::store_pool(pool)
    } else {
        pool.clone()
    };
    tokio::time::timeout(limit, async {
        loop {
            let pids: Vec<i32> = sqlx::query_scalar(
                "WITH direct AS ( \
                   SELECT w.pid FROM pg_locks w \
                   JOIN pg_locks t ON t.pid = w.pid AND t.locktype = 'tuple' AND t.granted \
                    AND t.relation = $2::regclass \
                    AND t.database = (SELECT oid FROM pg_database WHERE datname = current_database()) \
                   WHERE w.locktype = 'transactionid' AND NOT w.granted \
                    AND $1 = ANY(pg_blocking_pids(w.pid))), \
                 queued AS ( \
                   SELECT q.pid FROM pg_locks q \
                   WHERE q.locktype = 'tuple' AND NOT q.granted AND q.relation = $2::regclass \
                    AND q.database = (SELECT oid FROM pg_database WHERE datname = current_database()) \
                    AND EXISTS (SELECT 1 FROM direct)) \
                 SELECT pid FROM (SELECT pid, 0 AS rank FROM direct \
                                  UNION ALL SELECT pid, 1 FROM queued) AS waiters \
                 ORDER BY rank, pid",
            )
            .bind(holder)
            .bind(table)
            .fetch_all(pool)
            .await
            .unwrap();
            if pids.len() >= count.max(1) {
                return pids;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .ok()
}

/// [`wait_row_lock_waiter`] for a test that needs the wait: waits up to 10 s.
///
/// # Panics
/// When no session waits for a row lock on `table` held by `holder` within
/// 10 s; the message names the table and the holder.
pub(crate) async fn expect_row_lock_waiter(pool: &PgPool, holder: i32, table: &str) -> i32 {
    wait_row_lock_waiter(pool, holder, table, WAIT_LIMIT)
        .await
        .unwrap_or_else(|| {
            panic!("no session waited for a row lock on {table} held by backend {holder}")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;

    /// Which of `names` the condition keeps for the database `database`.
    async fn kept_by_own_databases(pool: &PgPool, database: &str, names: &[&str]) -> Vec<String> {
        let sql = concat!(
            "SELECT n FROM unnest($1::text[]) AS n WHERE ",
            own_databases!("n", "$2::text"),
            " ORDER BY n"
        );
        sqlx::query_scalar(sql)
            .bind(names)
            .bind(database)
            .fetch_all(pool)
            .await
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn own_databases_are_the_name_its_store_and_its_extras_and_no_longer_counter() {
        let db = test_db().await;
        let names = [
            "r_test_5_9_1",
            "r_test_5_9_1_store",
            "r_test_5_9_1_xa",
            "r_test_5_9_10",
            "r_test_5_9_10_store",
            "r_test_5_9_10_xa",
            "r_test_5_9_11",
            "r_test_5_9",
        ];

        let kept_for_1 = kept_by_own_databases(&db.pool, "r_test_5_9_1", &names).await;
        let kept_for_10 = kept_by_own_databases(&db.pool, "r_test_5_9_10", &names).await;

        assert_eq!(
            kept_for_1,
            ["r_test_5_9_1", "r_test_5_9_1_store", "r_test_5_9_1_xa"]
        );
        assert_eq!(
            kept_for_10,
            ["r_test_5_9_10", "r_test_5_9_10_store", "r_test_5_9_10_xa"]
        );
    }

    /// Starts a session that waits for a `FOR SHARE` lock on `lock_probe_a`'s
    /// row while the returned transaction holds it `FOR UPDATE`.
    async fn hold_and_wait(
        pool: &PgPool,
    ) -> (
        sqlx::Transaction<'static, sqlx::Postgres>,
        i32,
        tokio::task::JoinHandle<()>,
    ) {
        sqlx::query("CREATE TABLE lock_probe_a (id int PRIMARY KEY)")
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE lock_probe_b (id int PRIMARY KEY)")
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO lock_probe_a VALUES (1)")
            .execute(pool)
            .await
            .unwrap();
        let mut holder = pool.begin().await.unwrap();
        let holder_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *holder)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM lock_probe_a WHERE id = 1 FOR UPDATE")
            .execute(&mut *holder)
            .await
            .unwrap();
        let waiting_pool = pool.clone();
        let waiter = tokio::spawn(async move {
            sqlx::query("SELECT id FROM lock_probe_a WHERE id = 1 FOR SHARE")
                .fetch_all(&waiting_pool)
                .await
                .unwrap();
        });
        (holder, holder_pid, waiter)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_row_lock_waiter_is_found_by_its_table() {
        let db = test_db().await;
        let (holder, holder_pid, waiter) = hold_and_wait(&db.pool).await;
        let pid = expect_row_lock_waiter(&db.pool, holder_pid, "lock_probe_a").await;
        assert_ne!(pid, holder_pid);
        holder.commit().await.unwrap();
        waiter.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_row_lock_waiter_on_another_table_does_not_match() {
        let db = test_db().await;
        let (holder, holder_pid, waiter) = hold_and_wait(&db.pool).await;
        // The waiter is queued (found by its own table), yet is no waiter
        // for the other table's rows.
        expect_row_lock_waiter(&db.pool, holder_pid, "lock_probe_a").await;
        let found = wait_row_lock_waiter(
            &db.pool,
            holder_pid,
            "lock_probe_b",
            Duration::from_millis(300),
        )
        .await;
        assert_eq!(found, None);
        holder.commit().await.unwrap();
        waiter.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_row_lock_waiter_behind_another_holder_does_not_match() {
        let db = test_db().await;
        let (holder, holder_pid, waiter) = hold_and_wait(&db.pool).await;
        expect_row_lock_waiter(&db.pool, holder_pid, "lock_probe_a").await;
        let other = holder_pid + 1;
        let found =
            wait_row_lock_waiter(&db.pool, other, "lock_probe_a", Duration::from_millis(300)).await;
        assert_eq!(found, None);
        holder.commit().await.unwrap();
        waiter.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn two_sessions_after_one_row_are_both_found_but_not_a_third() {
        let db = test_db().await;
        let (holder, holder_pid, first) = hold_and_wait(&db.pool).await;
        let waiting_pool = db.pool.clone();
        let second = tokio::spawn(async move {
            sqlx::query("SELECT id FROM lock_probe_a WHERE id = 1 FOR UPDATE")
                .fetch_all(&waiting_pool)
                .await
                .unwrap();
        });
        let both = wait_row_lock_waiters(
            &db.pool,
            holder_pid,
            "lock_probe_a",
            2,
            Duration::from_secs(10),
        )
        .await
        .expect("two sessions waited");
        let third = wait_row_lock_waiters(
            &db.pool,
            holder_pid,
            "lock_probe_a",
            3,
            Duration::from_millis(300),
        )
        .await;
        assert_eq!(both.len(), 2);
        assert_eq!(third, None);
        holder.commit().await.unwrap();
        first.await.unwrap();
        second.await.unwrap();
    }
}
