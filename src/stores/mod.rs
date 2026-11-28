//! The two kinds of database handle: [`AccountDb`] (users, teams, keys,
//! sessions, the catalog registry) and [`CatalogDb`] (one store's catalog
//! data), and [`StoreName`], a store's name.
//!
//! Both handles wrap a `PgPool`. They are separate types so a function that
//! takes one cannot be handed the other: the compiler, not a code review,
//! catches a query sent to the wrong database. Release 1 of the catalog
//! stores milestone only introduces the types; in it both reach the same
//! database. (A Python reader: this is the "newtype" pattern, two classes
//! that wrap the same object so the type checker can tell them apart.)

use std::future::Future;
use std::time::Duration;

use futures_util::FutureExt;
use sqlx::pool::PoolConnection;
use sqlx::postgres::PgPool;
use sqlx::{Postgres, Transaction};
use tokio::sync::watch;

use crate::telemetry::subscriber::split_event;

pub mod bounded;
mod credentials;
mod executor;
pub mod placement;
mod registry;

pub(crate) use credentials::{is_auth_failure, signal_auth_refused};

pub use executor::StoreExecutor;
#[cfg(test)]
pub(crate) use registry::same_database;
pub use registry::{PoolSettings, Stores, STORE_SEARCH_PATH};

use crate::error::OkfError;

/// The most stores a read across stores talks to at once, so a listing or a
/// search over many catalogs never opens more than this many connections in
/// parallel (each store's own pool is bounded by `store_max_connections`).
pub const READ_CONCURRENCY: usize = 8;

/// How many times `store_connect_timeout` loading a catalog's state from a
/// store may take (30 s by default) before the store is reported
/// unavailable: a connection that stalls after it was opened (a lock, a
/// network that dropped packets) is as unavailable as one that refuses, but
/// a large catalog legitimately takes longer than one connect.
pub const STATE_LOAD_TIMEOUT_FACTOR: u32 = 6;

/// Reads a driver error from `store`: a failure to get a connection, a
/// broken connection or a refused login is [`OkfError::StoreUnavailable`],
/// anything else the usual conversion. [`CatalogDb::classify`] for a caller
/// that holds only the store's name.
///
/// # Arguments
/// * `store` - the store the operation ran on.
/// * `error` - what the driver returned.
///
/// # Returns
/// The error to hand back.
#[tracing::instrument(level = "trace", skip_all)]
pub fn classify_store_error(store: &StoreName, error: sqlx::Error) -> OkfError {
    if !is_store_unreachable(&error) {
        return OkfError::from(error);
    }
    // A refused login is the store being unreachable to this server (a
    // rotated secret, an expired token), not a fault in the request;
    // the credential tasks are told to fetch a fresh login.
    if credentials::is_auth_failure(&error) {
        credentials::signal_auth_refused(&error);
    }
    OkfError::store_unavailable(store)
}

/// `true` for a driver error that means the store cannot be reached: a
/// refused login, a pool that timed out or is closed, a transport or TLS
/// failure (at connect or mid-query), a crashed pool worker.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "a pure match over the error's variants: a closed-value classifier"
    )
)]
fn is_store_unreachable(error: &sqlx::Error) -> bool {
    credentials::is_auth_failure(error)
        || matches!(
            error,
            sqlx::Error::PoolTimedOut
                | sqlx::Error::PoolClosed
                | sqlx::Error::Io(_)
                | sqlx::Error::Tls(_)
                | sqlx::Error::WorkerCrashed
        )
}

/// [`classify_store_error`] for an error already converted with `?`: a
/// [`OkfError::Database`] from a transport failure becomes
/// [`OkfError::StoreUnavailable`]; every other error is returned as it is.
///
/// # Arguments
/// * `store` - the store the operation ran on.
/// * `error` - what the operation returned.
///
/// # Returns
/// The error to hand back.
#[tracing::instrument(level = "trace", skip_all)]
pub fn classify_store_failure(store: &StoreName, error: OkfError) -> OkfError {
    match error {
        OkfError::Database { source } => classify_store_error(store, source),
        other => other,
    }
}

/// The name of the store every existing catalog is on after migration 0036.
const MAIN_STORE: &str = "main";

/// The longest store name accepted: a bound so a name fits a metric label
/// and a log line.
const MAX_STORE_NAME_LEN: usize = 63;

/// A store's name, as on its `stores` row: lowercase letters, digits and
/// hyphens (`^[a-z0-9-]+$`), 1 to 63 characters.
// The field is private (no `pub`), so the only way to make one is `parse`
// or `main`: a `StoreName` in hand is always valid.
// `try_from = "String"` makes serde read a string and then call `TryFrom<String>`
// (below), so deserializing goes through `parse` like any other constructor.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct StoreName(String);

impl StoreName {
    /// Checks a store name.
    ///
    /// # Arguments
    /// * `name` - the text to check.
    ///
    /// # Returns
    /// The name, if it matches `^[a-z0-9-]+$` and is at most 63 characters.
    ///
    /// # Errors
    /// [`OkfError::Refused`] naming the rule (never the rejected text, which
    /// is caller input) when it does not.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn parse(name: &str) -> Result<Self, OkfError> {
        let valid = !name.is_empty()
            && name.len() <= MAX_STORE_NAME_LEN
            && name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
        if valid {
            Ok(Self(name.to_string()))
        } else {
            Err(OkfError::Refused {
                reason: format!(
                    "a store name is 1 to {MAX_STORE_NAME_LEN} characters: lowercase letters, \
                     digits and hyphens"
                ),
            })
        }
    }

    /// The name as text.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "one-line accessor")
    )]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `main`, the store every existing catalog is on after migration 0036.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "returns a constant")
    )]
    pub fn main() -> Self {
        Self(MAIN_STORE.to_string())
    }
}

impl TryFrom<String> for StoreName {
    type Error = OkfError;

    /// [`StoreName::parse`] for an owned string, so deserialization validates.
    ///
    /// # Arguments
    /// * `name` - the text to check.
    ///
    /// # Returns
    /// The name, if valid.
    ///
    /// # Errors
    /// [`OkfError::Refused`], as [`StoreName::parse`].
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "delegates to the instrumented `parse`")
    )]
    fn try_from(name: String) -> Result<Self, Self::Error> {
        Self::parse(&name)
    }
}

impl std::fmt::Display for StoreName {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The account database. Cloning is cheap (the pool is reference-counted).
#[derive(Debug, Clone)]
pub struct AccountDb {
    pool: PgPool,
}

impl AccountDb {
    /// Wraps a pool as the account database.
    ///
    /// # Arguments
    /// * `pool` - a pool on the account database.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "one-line constructor")
    )]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Begins a transaction on the account database.
    ///
    /// # Returns
    /// The transaction, which owns its connection (`'static`).
    ///
    /// # Errors
    /// [`OkfError::Database`] (or `Busy`) when no connection is available.
    #[tracing::instrument(level = "trace", skip_all)]
    pub async fn begin(&self) -> Result<Transaction<'static, Postgres>, OkfError> {
        Ok(self.pool.begin().await?)
    }

    /// The pool, for store-layer functions that take an executor. Routes and
    /// tools pass the [`AccountDb`] itself, not this.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "one-line accessor")
    )]
    pub fn executor(&self) -> &PgPool {
        &self.pool
    }

    /// The account database over a test pool: the one-line handle the
    /// tests build around the pool they hold (the account side of
    /// [`CatalogDb::main_of`]).
    #[cfg(test)]
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "one-line test constructor")
    )]
    pub(crate) fn of(pool: &PgPool) -> Self {
        Self::new(pool.clone())
    }
}

/// One store's database: the catalog data of the catalogs placed on it.
#[derive(Debug, Clone)]
pub struct CatalogDb {
    store: StoreName,
    pool: PgPool,
    // The account database this store belongs to. Until Task 12 moves the
    // audit entry into the store transaction, a change that touches both
    // (the audit entry, the users it locks, the catalog's registry row)
    // runs an account transaction beside the store's, and takes it from here.
    accounts: AccountDb,
    // How a connection refused its login waits for a fresh one (issue
    // #998): `None` for a store whose login never changes.
    renewal: Option<Renewal>,
}

/// What a store handle needs to wait out a refused login: the credential
/// task's refresh count, and how long to wait for it.
#[derive(Debug, Clone)]
struct Renewal {
    // Counts the refreshes the credential task has applied; closes when the
    // task ends (the store was forgotten).
    refreshed: watch::Receiver<u64>,
    wait: Duration,
}

impl CatalogDb {
    /// Wraps a pool as a store's database.
    ///
    /// # Arguments
    /// * `store` - the store's name.
    /// * `pool` - a pool on the store's database.
    /// * `accounts` - the account database the store serves.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "one-line constructor")
    )]
    pub fn new(store: StoreName, pool: PgPool, accounts: AccountDb) -> Self {
        Self {
            store,
            pool,
            accounts,
            renewal: None,
        }
    }

    /// This handle, made to wait for the credential task's next refresh when
    /// a connection is refused its login (see `CatalogDb::retrying`).
    ///
    /// # Arguments
    /// * `refreshed` - the credential task's refresh count.
    /// * `wait` - the longest to wait for a refresh (`store_connect_timeout`).
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "one-line builder")
    )]
    pub(super) fn with_renewal(mut self, refreshed: watch::Receiver<u64>, wait: Duration) -> Self {
        self.renewal = Some(Renewal { refreshed, wait });
        self
    }

    /// Runs `attempt` (an acquire, or a begin, on this store's pool) and,
    /// when the pool is refused its login, waits for the credential task's
    /// next refresh and runs it once more (issue #998).
    ///
    /// A pool that needs a new physical connection after the secret
    /// rotated or the token expired is refused (SQLSTATE class 28). The
    /// credential task refreshes the pool's login when told, so this tells
    /// it, waits for the refresh count to pass the value read before the
    /// first attempt, and tries again. Every caller refused at once waits on
    /// the same refresh: the task takes one fetch for all of them, and a
    /// refresh that landed while the attempt was in flight counts, so
    /// nobody waits for one that already happened.
    ///
    /// Only a refused login retries; a timeout or a transport error is
    /// returned at once, so a real outage costs no extra time. There is no
    /// second retry: a login still refused after the refresh is returned.
    /// The wait is bounded by `store_connect_timeout` counted from the first
    /// attempt; when the task ends or the time runs out, the first error
    /// is returned. A store whose login never changes has no renewal and
    /// runs `attempt` once.
    ///
    /// # Arguments
    /// * `attempt` - the pool call; run at most twice.
    ///
    /// # Returns
    /// What `attempt` returned the last time it ran.
    #[tracing::instrument(level = "trace", skip_all)]
    async fn retrying<T, F, Fut>(&self, mut attempt: F) -> Result<T, sqlx::Error>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, sqlx::Error>>,
    {
        let Some(renewal) = &self.renewal else {
            return attempt().await;
        };
        let started = tokio::time::Instant::now();
        // Read before the attempt: a refresh that lands during it still
        // counts, because it is newer than this value.
        let seen = *renewal.refreshed.borrow();
        let refused = match attempt().await {
            Err(error) if credentials::is_auth_failure(&error) => error,
            done => return done,
        };
        credentials::signal_auth_refused(&refused);
        let mut refreshed = renewal.refreshed.clone();
        let wait = renewal.wait.saturating_sub(started.elapsed());
        // `Ok(Ok(_))` is a refresh; the other two are the time running out
        // and the task ending (the store was forgotten). The guard `wait_for`
        // returns is dropped inside `map`, before `refreshed` goes.
        let renewed = tokio::time::timeout(
            wait,
            refreshed
                .wait_for(|count| *count > seen)
                .map(|seen| seen.is_ok()),
        )
        .await;
        if !matches!(renewed, Ok(true)) {
            return Err(refused);
        }
        split_event!(
            debug,
            {},
            { store = %self.store, },
            "a store's connection was refused its login; retrying after a refresh"
        );
        attempt().await
    }

    /// A connection from the store's pool, trying once more after a refused
    /// login ((`CatalogDb::retrying`)).
    ///
    /// # Returns
    /// The connection, which goes back to the pool when dropped.
    ///
    /// # Errors
    /// [`OkfError::StoreUnavailable`] when no connection can be had within
    /// `store_connect_timeout`, or the login stays refused;
    /// [`OkfError::Database`] or `Busy` otherwise.
    #[tracing::instrument(level = "trace", skip_all)]
    pub async fn acquire(&self) -> Result<PoolConnection<Postgres>, OkfError> {
        self.acquire_raw()
            .await
            .map_err(|error| self.classify(error))
    }

    /// [`CatalogDb::acquire`] with the driver's own error, for the executor,
    /// which classifies on its way out.
    #[tracing::instrument(level = "trace", skip_all)]
    async fn acquire_raw(&self) -> Result<PoolConnection<Postgres>, sqlx::Error> {
        self.retrying(|| self.pool.acquire()).await
    }

    /// Begins a transaction with `statement` (for example
    /// `BEGIN ISOLATION LEVEL REPEATABLE READ, READ ONLY`), trying once more
    /// after a refused login. See [`CatalogDb::begin`] for the errors and
    /// the order a transaction must be taken in.
    ///
    /// # Arguments
    /// * `statement` - the `BEGIN` statement, from the code, never from input.
    #[tracing::instrument(level = "trace", skip_all)]
    pub async fn begin_with(
        &self,
        statement: &'static str,
    ) -> Result<Transaction<'static, Postgres>, OkfError> {
        self.retrying(|| self.pool.begin_with(statement))
            .await
            .map_err(|error| self.classify(error))
    }

    /// The account database this store serves: the other side of a change
    /// that writes both (an audit entry and the rows it locks, a catalog's
    /// registry row). The store's own pool is never the account's, even when
    /// the store is inside the account database (plan decision P1).
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "one-line accessor")
    )]
    pub fn accounts(&self) -> &AccountDb {
        &self.accounts
    }

    /// The store's name.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "one-line accessor")
    )]
    pub fn store(&self) -> &StoreName {
        &self.store
    }

    /// Begins a transaction on the store's database.
    ///
    /// # Acquisition order
    /// A change that also holds an account transaction takes the account
    /// side first: the account connection, then every account row lock
    /// (`lock_people`, the user and key rows), and only then this. A
    /// transaction that holds the store's connection must never wait for an
    /// account row, because a user deletion holds those rows and then needs
    /// a store connection: with one store connection that is a cycle
    /// PostgreSQL cannot detect (one edge is a pool wait). The store
    /// commits first, then the account transaction (issue #970).
    ///
    /// # Returns
    /// The transaction, which owns its connection (`'static`).
    ///
    /// # Errors
    /// [`OkfError::StoreUnavailable`] when no connection can be had within
    /// `store_connect_timeout` (the pool is exhausted, or the store cannot be
    /// reached to open another, or refuses the login); [`OkfError::Database`] or `Busy` otherwise.
    #[tracing::instrument(level = "trace", skip_all)]
    pub async fn begin(&self) -> Result<Transaction<'static, Postgres>, OkfError> {
        self.retrying(|| self.pool.begin())
            .await
            .map_err(|error| self.classify(error))
    }

    /// Reads a driver error from this store's pool: a failure to get a
    /// connection, or a refused login, is [`OkfError::StoreUnavailable`] (which never names the
    /// store to the client), anything else the usual conversion. Account
    /// database errors never pass through here.
    ///
    /// # Arguments
    /// * `error` - what the driver returned for an operation on this store.
    ///
    /// # Returns
    /// The error to hand back.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn classify(&self, error: sqlx::Error) -> OkfError {
        classify_store_error(&self.store, error)
    }

    /// The pool as an executor, for store-layer functions that take one.
    /// Routes and tools pass the [`CatalogDb`] itself, not this.
    ///
    /// A driver error that means the store cannot be reached (a pool
    /// timeout, a closed pool, a transport or TLS failure, a refused login)
    /// leaves a query on it already as [`OkfError::StoreUnavailable`]
    /// once `?` converts it, so no call site classifies (issue #995). Any
    /// other error, a constraint violation or a bad statement, is the
    /// driver's, as before. For a pool-level call (`begin`, `acquire`,
    /// `close`, its options) use [`CatalogDb::pool`].
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "one-line accessor")
    )]
    pub fn executor(&self) -> StoreExecutor<'_> {
        StoreExecutor::new(self)
    }

    /// The raw pool, for what an executor cannot do: close it, read its
    /// options, take a connection. An error from the pool itself is the
    /// caller's to classify with [`CatalogDb::classify`]; queries go
    /// through [`CatalogDb::executor`], which does it for them.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "one-line accessor")
    )]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// The `main` store when it is the account database, for a caller that
    /// holds only the account pool (the tests, and tools that run outside
    /// the server): a pool of its own on the same database and login, with
    /// the store's `search_path` ([`STORE_SEARCH_PATH`]), so a query for an
    /// account table sent to it fails as it does in production. The pool is
    /// lazy (it connects on first use) and as large as `accounts`.
    ///
    /// # Arguments
    /// * `accounts` - the account pool.
    ///
    /// # Returns
    /// A handle for the store `main`. The server itself never calls this: it
    /// gets its handles from [`Stores`].
    #[cfg(test)]
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn main_in(accounts: &PgPool) -> Self {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(accounts.options().get_max_connections())
            .after_connect(|connection, _meta| Box::pin(registry::set_search_path(connection)))
            .connect_lazy_with(crate::db::pg::store_options(accounts));
        Self::new(StoreName::main(), pool, AccountDb::new(accounts.clone()))
    }

    #[cfg(test)]
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "one-line test constructor")
    )]
    pub(crate) fn main_of(pool: &PgPool) -> Self {
        Self::main_in(pool)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[tokio::test]
    async fn classify_turns_connection_failures_into_store_unavailable() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://u:p@127.0.0.1:1/x")
            .unwrap();
        let db = CatalogDb::new(
            StoreName::parse("shared-1").unwrap(),
            pool.clone(),
            AccountDb::new(pool),
        );

        for failure in [
            sqlx::Error::PoolTimedOut,
            sqlx::Error::PoolClosed,
            sqlx::Error::WorkerCrashed,
            sqlx::Error::Io(std::io::Error::other("connection refused")),
        ] {
            let error = db.classify(failure);
            assert!(
                matches!(error, OkfError::StoreUnavailable { .. }),
                "{error:?}"
            );
        }
        let other = db.classify(sqlx::Error::RowNotFound);
        assert!(matches!(other, OkfError::Database { .. }), "{other:?}");
    }

    /// A driver error with SQLSTATE 28P01 (a refused password) whose
    /// message names a role no credential task has, so signalling it wakes
    /// none of the tests' tasks running beside this one.
    #[derive(Debug)]
    struct Refused;

    impl std::fmt::Display for Refused {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str(sqlx::error::DatabaseError::message(self))
        }
    }

    impl std::error::Error for Refused {}

    impl sqlx::error::DatabaseError for Refused {
        fn message(&self) -> &str {
            "password authentication failed for user \"no-such-role-998\""
        }
        fn code(&self) -> Option<std::borrow::Cow<'_, str>> {
            Some(std::borrow::Cow::Borrowed("28P01"))
        }
        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }
        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::Other
        }
    }

    fn refused() -> sqlx::Error {
        sqlx::Error::Database(Box::new(Refused))
    }

    /// A store handle on a lazy pool, waiting up to `wait` for a refresh
    /// counted by the returned sender.
    fn renewing_db(wait: Duration) -> (CatalogDb, watch::Sender<u64>) {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://u:p@127.0.0.1:1/x")
            .unwrap();
        let (refreshes, refreshed) = watch::channel(0_u64);
        let db = CatalogDb::new(
            StoreName::parse("shared-1").unwrap(),
            pool.clone(),
            AccountDb::new(pool),
        )
        .with_renewal(refreshed, wait);
        (db, refreshes)
    }

    /// An attempt that is refused its login `refusals` times, then works,
    /// and counts how often it ran.
    fn attempts_refused(
        refusals: usize,
        ran: &std::sync::atomic::AtomicUsize,
    ) -> impl Future<Output = Result<(), sqlx::Error>> + '_ {
        let nth = ran.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::future::ready(if nth < refusals {
            Err(refused())
        } else {
            Ok(())
        })
    }

    /// Issue #998: a refused login waits for the refresh count to move, then
    /// runs the attempt once more.
    #[tokio::test]
    async fn a_refused_login_waits_for_a_refresh_and_retries_once() {
        let (db, refreshes) = renewing_db(Duration::from_secs(30));
        let ran = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let worker = {
            let (db, ran) = (db.clone(), Arc::clone(&ran));
            tokio::spawn(async move { db.retrying(|| attempts_refused(1, &ran)).await })
        };

        // The retry waits: the attempt ran once and has not run again.
        tokio::time::timeout(Duration::from_secs(10), async {
            while ran.load(std::sync::atomic::Ordering::SeqCst) < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the first attempt ran");
        assert!(!worker.is_finished());
        refreshes.send_modify(|count| *count += 1);
        let done = tokio::time::timeout(Duration::from_secs(10), worker)
            .await
            .expect("the retry ran after the refresh")
            .unwrap();

        assert!(done.is_ok());
        assert_eq!(ran.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    /// A login still refused after the refresh is returned, after one retry
    /// and no more.
    #[tokio::test]
    async fn a_login_that_stays_refused_is_tried_twice_and_no_more() {
        let (db, refreshes) = renewing_db(Duration::from_secs(30));
        let ran = std::sync::atomic::AtomicUsize::new(0);
        refreshes.send_modify(|count| *count += 1);
        // The attempt itself sees a newer count than the one read before it.
        let result = db
            .retrying(|| {
                refreshes.send_modify(|count| *count += 1);
                attempts_refused(usize::MAX, &ran)
            })
            .await;

        assert!(result.is_err());
        assert_eq!(ran.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    /// With no refresh the wait ends at `store_connect_timeout` with the
    /// refusal, without a second attempt.
    #[tokio::test(start_paused = true)]
    async fn no_refresh_within_the_timeout_returns_the_refusal() {
        let wait = Duration::from_secs(5);
        let (db, _refreshes) = renewing_db(wait);
        let ran = std::sync::atomic::AtomicUsize::new(0);
        let started = tokio::time::Instant::now();

        let error = db
            .retrying(|| attempts_refused(usize::MAX, &ran))
            .await
            .unwrap_err();

        assert!(is_auth_failure(&error));
        assert_eq!(ran.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(started.elapsed(), wait);
        assert!(matches!(
            db.classify(error),
            OkfError::StoreUnavailable { .. }
        ));
    }

    /// A credential task that has ended (the store was forgotten) cannot
    /// refresh: the refusal returns at once.
    #[tokio::test(start_paused = true)]
    async fn an_ended_credential_task_returns_the_refusal_at_once() {
        let (db, refreshes) = renewing_db(Duration::from_secs(5));
        drop(refreshes);
        let ran = std::sync::atomic::AtomicUsize::new(0);
        let started = tokio::time::Instant::now();

        let result = db.retrying(|| attempts_refused(usize::MAX, &ran)).await;

        assert!(result.is_err());
        assert_eq!(ran.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(started.elapsed(), Duration::ZERO);
    }

    /// Only a refused login retries: a timeout or a transport error is
    /// returned at once, so a real outage costs no extra time.
    #[tokio::test(start_paused = true)]
    async fn other_failures_are_not_retried() {
        let (db, _refreshes) = renewing_db(Duration::from_secs(5));
        for failure in [
            || sqlx::Error::PoolTimedOut,
            || sqlx::Error::PoolClosed,
            || sqlx::Error::Io(std::io::Error::other("connection refused")),
            || sqlx::Error::WorkerCrashed,
            || sqlx::Error::RowNotFound,
        ] {
            let ran = std::sync::atomic::AtomicUsize::new(0);
            let started = tokio::time::Instant::now();

            let result: Result<(), _> = db
                .retrying(|| {
                    ran.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    std::future::ready(Err(failure()))
                })
                .await;

            assert!(result.is_err());
            assert_eq!(ran.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert_eq!(started.elapsed(), Duration::ZERO);
        }
    }

    /// A store whose login never changes has no renewal: one attempt.
    #[tokio::test]
    async fn a_store_without_a_credential_task_does_not_retry() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://u:p@127.0.0.1:1/x")
            .unwrap();
        let db = CatalogDb::new(StoreName::main(), pool.clone(), AccountDb::new(pool));
        let ran = std::sync::atomic::AtomicUsize::new(0);

        let result = db.retrying(|| attempts_refused(usize::MAX, &ran)).await;

        assert!(result.is_err());
        assert_eq!(ran.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// Callers refused together wait on the same refresh: one bump of the
    /// count releases all of them.
    #[tokio::test]
    async fn every_caller_refused_at_once_is_released_by_one_refresh() {
        let (db, refreshes) = renewing_db(Duration::from_secs(30));
        let ran = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let callers = 8;
        let workers: Vec<_> = (0..callers)
            .map(|_| {
                let (db, ran) = (db.clone(), Arc::clone(&ran));
                tokio::spawn(async move {
                    // Refused the first time this caller runs it, then works.
                    let mut first = true;
                    db.retrying(|| {
                        let refuse = std::mem::replace(&mut first, false);
                        ran.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        std::future::ready(if refuse { Err(refused()) } else { Ok(()) })
                    })
                    .await
                })
            })
            .collect();
        tokio::time::timeout(Duration::from_secs(10), async {
            while ran.load(std::sync::atomic::Ordering::SeqCst) < callers {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("every caller was refused");

        refreshes.send_modify(|count| *count += 1);

        for worker in workers {
            let done = tokio::time::timeout(Duration::from_secs(10), worker)
                .await
                .expect("released by the one refresh")
                .unwrap();
            assert!(done.is_ok());
        }
        assert_eq!(ran.load(std::sync::atomic::Ordering::SeqCst), 2 * callers);
    }

    /// A refresh that landed while the attempt was in flight is the one
    /// the retry needs: it does not wait for another.
    #[tokio::test(start_paused = true)]
    async fn a_refresh_during_the_attempt_is_not_waited_for_again() {
        let (db, refreshes) = renewing_db(Duration::from_secs(5));
        let ran = std::sync::atomic::AtomicUsize::new(0);
        let started = tokio::time::Instant::now();

        let result = db
            .retrying(|| {
                if ran.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                    refreshes.send_modify(|count| *count += 1);
                }
                attempts_refused(1, &ran)
            })
            .await;

        assert!(result.is_ok());
        assert_eq!(ran.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(started.elapsed(), Duration::ZERO);
    }

    #[test]
    fn store_name_parse_accepts_slugs_and_refuses_the_rest() {
        for good in [
            "main",
            "shared-1",
            "acme",
            "0",
            "a-b-c",
            "-",
            &"a".repeat(63),
        ] {
            assert_eq!(StoreName::parse(good).unwrap().as_str(), good);
        }
        let too_long = "a".repeat(64);
        for bad in [
            "",
            "Main",
            "shared_1",
            "a b",
            "a.b",
            "a/b",
            "caf\u{e9}",
            "nul\0",
            &too_long,
        ] {
            let error = StoreName::parse(bad).unwrap_err();
            assert_eq!(error.kind(), "Refused", "{bad:?}");
            // The rejected text is caller input: the message never echoes it.
            if !bad.is_empty() {
                assert!(!error.to_string().contains(bad), "{bad:?}");
            }
        }
        assert_eq!(StoreName::main().as_str(), "main");
        assert_eq!(StoreName::main().to_string(), "main");
    }

    #[test]
    fn store_name_deserialize_validates_like_parse() {
        let too_long = format!("\"{}\"", "a".repeat(64));
        for bad in [
            r#""Main/invalid""#,
            r#""""#,
            r#""shared_1""#,
            too_long.as_str(),
            "5",
            "null",
        ] {
            assert!(serde_json::from_str::<StoreName>(bad).is_err(), "{bad}");
        }
        // The refusal is the fixed message: it does not echo the input.
        let error = serde_json::from_str::<StoreName>(r#""Main/invalid""#).unwrap_err();
        assert!(!error.to_string().contains("Main/invalid"));

        let name = StoreName::parse("shared-1").unwrap();
        let json = serde_json::to_string(&name).unwrap();
        assert_eq!(json, r#""shared-1""#);
        assert_eq!(serde_json::from_str::<StoreName>(&json).unwrap(), name);
    }

    #[tokio::test]
    async fn main_store_and_accounts_reach_the_same_database_in_release_one() {
        let db = crate::db::test_db().await;
        crate::db::main_in_the_account_database(&db.pool).await;
        let app = crate::app::App::load(
            db.pool.clone(),
            crate::config::Config::default(),
            std::sync::Arc::new(crate::clock::SystemClock),
        )
        .await
        .unwrap();

        let from_accounts: String = sqlx::query_scalar("SELECT current_database()")
            .fetch_one(app.accounts().executor())
            .await
            .unwrap();
        let from_store: String = sqlx::query_scalar("SELECT current_database()")
            .fetch_one(app.main_db().await.executor())
            .await
            .unwrap();
        assert_eq!(from_accounts, from_store);
        assert_eq!(app.main_db().await.store(), &StoreName::main());
    }
    /// The Postgres error code for "undefined table".
    const UNDEFINED_TABLE: &str = "42P01";

    /// Whether `error` is Postgres's "the relation does not exist".
    fn is_undefined_table(error: &sqlx::Error) -> bool {
        error
            .as_database_error()
            .and_then(|database| database.code())
            .is_some_and(|code| code == UNDEFINED_TABLE)
    }

    /// Issue #970 review: with `database_max_connections = 1` the process
    /// holds one connection per pool, and every path reaches the `main`
    /// store through the registry's one pool for it, never a pool of its
    /// own: a create, an import (the publish write path), the directory
    /// load and a deletion all run, within a time limit, and the registry
    /// opened exactly one store pool.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_one_connection_configuration_never_opens_a_second_pool_for_main() {
        use crate::archive::files;
        use crate::cache::directory::CatalogDirectory;
        use crate::labels::DisplayName;
        use crate::store::audit::AuditActor;
        use crate::store::catalogs::{self, NewCatalog};
        use crate::store::import::{import, ImportOptions, ImportScope};
        use crate::telemetry::metrics::Metrics;
        use std::time::Duration;

        let db = crate::db::test_db().await;
        let one = crate::db::connect(&db.url, 1).await.unwrap();
        let stores = Stores::new(
            one.clone(),
            PoolSettings {
                max_connections: 1,
                idle_timeout: Duration::from_secs(60),
                connect_timeout: Duration::from_secs(5),
            },
        );
        let limit = Duration::from_secs(30);
        let cli = AuditActor::cli();
        let name = DisplayName::default();
        let new = NewCatalog {
            name: "solo",
            display_name: &name,
            description: "",
            owner_team: crate::db::TEST_TEAM,
        };

        let work = async {
            catalogs::create_in(&stores, &new, &cli, chrono::Utc::now()).await?;
            let main = stores.get(&StoreName::main()).await?;
            let options = ImportOptions {
                replace: true,
                scope: ImportScope::Everything,
                expect_generation: None,
                expect_catalog_id: None,
                actor: "t",
                audit: &cli,
                now: chrono::Utc::now(),
            };
            import(&main, "solo", files(&[("a.md", "# A\n")]), &options).await?;
            let directory = CatalogDirectory::new(
                one.clone(),
                stores.clone(),
                10,
                Duration::from_secs(60),
                Metrics::global(),
            );
            let row = directory.resolve("solo").await?.expect("the catalog");
            assert_eq!(row.generation, 1);
            catalogs::delete_in(&stores, "solo", true, &cli, chrono::Utc::now()).await
        };
        tokio::time::timeout(limit, work)
            .await
            .expect("finished on one connection")
            .unwrap();

        assert_eq!(stores.pools_opened(), 1);
        assert!(one.size() <= 1);
        let main = stores.get(&StoreName::main()).await.unwrap();
        assert!(main.pool().size() <= 1);
        assert_eq!(stores.pools_opened(), 1);
    }

    /// Migration 0041 (issue #970): a store's connections see schema
    /// `catalog` only, so a query for an account table fails there, and the
    /// store's own tables answer.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_store_pool_cannot_see_account_tables() {
        let db = crate::db::test_db().await;
        let store = CatalogDb::main_in(&db.pool);

        let account = sqlx::query("SELECT 1 FROM users")
            .fetch_all(store.executor())
            .await
            .unwrap_err();
        let registry = sqlx::query("SELECT 1 FROM catalogs")
            .fetch_all(store.executor())
            .await
            .unwrap_err();
        let own: i64 = sqlx::query_scalar("SELECT count(*) FROM documents")
            .fetch_one(store.executor())
            .await
            .unwrap();

        assert!(is_undefined_table(&account), "{account:?}");
        assert!(is_undefined_table(&registry), "{registry:?}");
        assert_eq!(own, 0);
    }

    /// The reverse: an account pool sees `public` only, so a query for a
    /// catalog table by its bare name fails there (it must be qualified).
    #[tokio::test(flavor = "multi_thread")]
    async fn an_account_pool_cannot_see_catalog_tables() {
        let db = crate::db::test_db().await;

        let bare = sqlx::query("SELECT 1 FROM documents")
            .fetch_all(&db.pool)
            .await
            .unwrap_err();
        let state = sqlx::query("SELECT 1 FROM catalog_state")
            .fetch_all(&db.pool)
            .await
            .unwrap_err();
        let qualified: i64 = sqlx::query_scalar("SELECT count(*) FROM catalog.documents")
            .fetch_one(&crate::db::store_pool(&db.pool))
            .await
            .unwrap();

        assert!(is_undefined_table(&bare), "{bare:?}");
        assert!(is_undefined_table(&state), "{state:?}");
        assert_eq!(qualified, 0);
    }
}
