//! [`StoreExecutor`]: a store's pool as an executor whose driver errors are
//! already classified, so a read through it cannot report an unreachable
//! store as [`OkfError::Database`] (issue #995).
//!
//! A store-layer function takes `impl PgExecutor` and returns the driver's
//! error converted by `?`. That conversion knows nothing about which
//! database the executor was on, so a pool timeout on a store came back as
//! `Database` (an internal error) instead of `StoreUnavailable`. The type
//! here closes that at the one place every such query passes: it wraps the
//! driver error that means "the store cannot be reached" as a
//! [`StoreUnavailable`](OkfError::StoreUnavailable), carried inside
//! `sqlx::Error::Configuration`, and `From<sqlx::Error> for OkfError`
//! unwraps it. A call site needs no change, and a new one cannot forget.
//! (A Python reader: this is a proxy object that forwards each call to the
//! real pool and rewrites the exception on the way out.)

use futures_util::future::BoxFuture;
use futures_util::stream::BoxStream;
use futures_util::{StreamExt, TryStreamExt};
use sqlx::postgres::{PgPool, PgQueryResult, PgRow, PgStatement, PgTypeInfo};
use sqlx::{Either, Execute, Executor, Postgres};

use super::{classify_store_error, is_store_unreachable, StoreName};

/// A store's pool, for store-layer functions that take an executor.
///
/// It is `Copy`, like the `&PgPool` it stands in for, so one value can run
/// several queries. It has no `begin` or `acquire`: a transaction comes from
/// [`CatalogDb::begin`](super::CatalogDb::begin), which classifies its own
/// errors.
#[derive(Debug, Clone, Copy)]
pub struct StoreExecutor<'a> {
    store: &'a StoreName,
    pool: &'a PgPool,
}

impl<'a> StoreExecutor<'a> {
    /// Wraps `pool`, the pool of `store`.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "one-line constructor")
    )]
    pub(super) fn new(store: &'a StoreName, pool: &'a PgPool) -> Self {
        Self { store, pool }
    }
}

/// Rewrites a driver error from `store`: one that
/// [`classify_store_error`] reads as the store being unreachable becomes
/// `sqlx::Error::Configuration` holding the [`OkfError::StoreUnavailable`]
/// (logged once, here); any other error is returned as it is. The same
/// errors count wherever they happen, a failed connect or a connection
/// reset mid-query, as in [`CatalogDb::classify`](super::CatalogDb::classify).
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "a forwarding shim on every store query: a span per query would be noise, and the caller's span records the error"
    )
)]
fn mark_unavailable(store: &StoreName, error: sqlx::Error) -> sqlx::Error {
    if is_store_unreachable(&error) {
        sqlx::Error::Configuration(Box::new(classify_store_error(store, error)))
    } else {
        error
    }
}

impl<'c> Executor<'c> for StoreExecutor<'c> {
    type Database = Postgres;

    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "a forwarding shim on every store query: a span per query would be noise, and the caller's span records the error"
        )
    )]
    fn fetch_many<'e, 'q: 'e, E>(
        self,
        query: E,
    ) -> BoxStream<'e, Result<Either<PgQueryResult, PgRow>, sqlx::Error>>
    where
        'c: 'e,
        E: 'q + Execute<'q, Postgres>,
    {
        let store = self.store;
        self.pool
            .fetch_many(query)
            .map_err(move |error| mark_unavailable(store, error))
            .boxed()
    }

    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "a forwarding shim on every store query: a span per query would be noise, and the caller's span records the error"
        )
    )]
    fn fetch_optional<'e, 'q: 'e, E>(
        self,
        query: E,
    ) -> BoxFuture<'e, Result<Option<PgRow>, sqlx::Error>>
    where
        'c: 'e,
        E: 'q + Execute<'q, Postgres>,
    {
        let store = self.store;
        let rows = self.pool.fetch_optional(query);
        Box::pin(async move { rows.await.map_err(|error| mark_unavailable(store, error)) })
    }

    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "a forwarding shim on every store query: a span per query would be noise, and the caller's span records the error"
        )
    )]
    fn prepare_with<'e>(
        self,
        sql: sqlx::SqlStr,
        parameters: &'e [PgTypeInfo],
    ) -> BoxFuture<'e, Result<PgStatement, sqlx::Error>>
    where
        'c: 'e,
    {
        let store = self.store;
        let prepared = self.pool.prepare_with(sql, parameters);
        Box::pin(async move {
            prepared
                .await
                .map_err(|error| mark_unavailable(store, error))
        })
    }

    #[doc(hidden)]
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "a forwarding shim on every store query: a span per query would be noise, and the caller's span records the error"
        )
    )]
    fn describe<'e>(
        self,
        sql: sqlx::SqlStr,
    ) -> BoxFuture<'e, Result<sqlx::Describe<Postgres>, sqlx::Error>>
    where
        'c: 'e,
    {
        let store = self.store;
        let described = self.pool.describe(sql);
        Box::pin(async move {
            described
                .await
                .map_err(|error| mark_unavailable(store, error))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::OkfError;
    use crate::stores::CatalogDb;

    /// A store over a pool that has been closed: every query on it fails
    /// the way an exhausted or lost store does (`PoolClosed`).
    async fn closed_store() -> CatalogDb {
        let db = crate::db::test_db().await;
        let store = CatalogDb::main_in(&db.pool);
        store.pool().close().await;
        store
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn every_query_kind_on_a_lost_store_converts_to_store_unavailable() {
        let store = closed_store().await;

        let fetched: Result<i64, OkfError> = sqlx::query_scalar("SELECT 1")
            .fetch_one(store.executor())
            .await
            .map_err(OkfError::from);
        let listed: Result<Vec<i64>, OkfError> = sqlx::query_scalar("SELECT 1")
            .fetch_all(store.executor())
            .await
            .map_err(OkfError::from);
        let maybe: Result<Option<i64>, OkfError> = sqlx::query_scalar("SELECT 1")
            .fetch_optional(store.executor())
            .await
            .map_err(OkfError::from);
        let ran = sqlx::query("SELECT 1")
            .execute(store.executor())
            .await
            .map_err(OkfError::from);
        let prepared = store
            .executor()
            .prepare(sqlx::SqlSafeStr::into_sql_str("SELECT 1"))
            .await
            .map_err(OkfError::from);

        for error in [
            fetched.unwrap_err(),
            listed.unwrap_err(),
            maybe.unwrap_err(),
            ran.unwrap_err(),
            prepared.unwrap_err(),
        ] {
            assert!(
                matches!(error, OkfError::StoreUnavailable { .. }),
                "{error:?}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_statement_on_a_live_store_stays_a_database_error() {
        let db = crate::db::test_db().await;
        let store = CatalogDb::main_in(&db.pool);

        let bad_sql = sqlx::query("SELECT FROM no_such_table_995")
            .fetch_all(store.executor())
            .await
            .map_err(OkfError::from)
            .unwrap_err();
        let no_row = sqlx::query_scalar::<_, i64>("SELECT 1 WHERE false")
            .fetch_one(store.executor())
            .await
            .map_err(OkfError::from)
            .unwrap_err();

        assert!(matches!(bad_sql, OkfError::Database { .. }), "{bad_sql:?}");
        assert!(matches!(no_row, OkfError::Database { .. }), "{no_row:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_account_pool_is_not_classified_as_a_store() {
        let db = crate::db::test_db().await;
        db.pool.close().await;

        let error = sqlx::query("SELECT 1")
            .execute(&db.pool)
            .await
            .map_err(OkfError::from)
            .unwrap_err();

        assert!(matches!(error, OkfError::Database { .. }), "{error:?}");
    }
}
