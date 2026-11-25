//! The Postgres connection pool, the account schema migrations
//! (`migrations/accounts/`) and the catalog store migrations
//! (`migrations/catalog/`).

use std::sync::LazyLock;

use sqlx::migrate::Migrator;
use sqlx::postgres::{PgConnection, PgPool, PgPoolOptions};

use crate::error::OkfError;
use crate::stores::StoreName;

// The library's unit tests share the integration tests' container helper:
// `#[path]` compiles that file here as a module (relative to `src/`).
#[cfg(test)]
#[path = "../tests/support/pg.rs"]
pub(crate) mod pg_support;
// The Redis helper's only sibling is `pg`, so alias it for `super::pg`.
#[cfg(test)]
pub(crate) use pg_support as pg;
#[cfg(test)]
#[path = "../tests/support/redis.rs"]
pub(crate) mod redis_support;

/// The account schema migrations in `migrations/accounts/`, embedded at compile time.
// `migrate!` reads the directory while compiling (`build.rs` makes cargo
// notice new files), so the binary needs no SQL files at runtime.
pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations/accounts");

/// Where a store's migration history is kept: in its own schema, apart from
/// the account database's `public._sqlx_migrations`, so the two migrators
/// never see each other's rows even when the store is the account database.
pub const CATALOG_HISTORY: &str = "catalog._sqlx_migrations";

/// The catalog store migrations in `migrations/catalog/`, embedded at
/// compile time, with their history in [`CATALOG_HISTORY`].
// `LazyLock` builds the value on first use (like a module-level value
// computed once in Python): `dangerous_set_table_name` is a method, so the
// migrator cannot be a plain `static` as `MIGRATOR` is.
pub static CATALOG_MIGRATOR: LazyLock<Migrator> = LazyLock::new(|| {
    let mut migrator = sqlx::migrate!("./migrations/catalog");
    migrator.dangerous_set_table_name(CATALOG_HISTORY);
    migrator
});

/// The version of `migrations/catalog/0001_baseline.sql`, the one adoption
/// records without running.
const BASELINE_VERSION: i64 = 1;

/// Connects a pool to the database.
///
/// # Arguments
/// * `url` - a `postgres://` or `postgresql://` URL (CNPG's generated secret).
/// * `max_connections` - the pool size (`database_max_connections`).
///
/// # Returns
/// A pool of up to `max_connections` connections. `async fn` returns a
/// future: the caller `.await`s it, like a Python coroutine.
///
/// # Errors
/// [`OkfError::Database`] if the URL is malformed or the server is unreachable.
#[tracing::instrument(level = "debug", skip_all)]
pub async fn connect(url: &str, max_connections: u32) -> Result<PgPool, OkfError> {
    let pool = PgPoolOptions::new()
        .max_connections(max_connections)
        .connect(url)
        .await?;
    Ok(pool)
}

/// The indexes okf's migrations build `CONCURRENTLY` (in `no-transaction`
/// migrations). A build that fails part-way leaves the index behind,
/// marked INVALID, and its migration unrecorded; `CREATE INDEX ... IF NOT
/// EXISTS` on the retry would then keep the invalid index for good.
const CONCURRENT_INDEXES: [&str; 6] = [
    "catalogs_member_reach",
    "catalogs_member_write",
    "catalogs_member_reach_bytes",
    "feedback_finished",
    "feedback_undelivered_user",
    "catalogs_unprovisioned",
];

/// The indexes the catalog migrations (`migrations/catalog/`) build
/// `CONCURRENTLY`, in schema `catalog`, with the same failure mode as
/// [`CONCURRENT_INDEXES`].
const CATALOG_CONCURRENT_INDEXES: [&str; 1] = ["revisions_catalog_user_recent"];

/// The advisory lock [`migrate`] holds around the cleanup and the
/// migrations (an arbitrary constant, distinct from sqlx's own lock key).
const CONCURRENT_INDEX_LOCK: i64 = 0x6f6b_665f_6964_7831;

/// The advisory lock, taken inside a new store's own database, that orders the
/// adds that reach one physical database (under any host spelling or name):
/// it is held from the ownership decision through the account registration.
/// Advisory locks are per database, so every add of that database meets it.
pub const STORE_ADD_LOCK_KEY: &str = "okf.store-add.database";

/// Takes [`CONCURRENT_INDEX_LOCK`], waiting for another holder by polling
/// `pg_try_advisory_lock` rather than blocking in `pg_advisory_lock`: a
/// blocked statement is an open transaction, and the holder's
/// `CREATE INDEX CONCURRENTLY` waits for every transaction open when it
/// starts, so the two would wait on each other until the deadlock detector
/// aborted the build.
///
/// # Errors
/// [`OkfError::Database`] (or `Busy`) for a driver error.
#[tracing::instrument(level = "debug", skip_all)]
async fn lock_migrations(conn: &mut PgConnection) -> Result<(), OkfError> {
    loop {
        let taken: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(CONCURRENT_INDEX_LOCK)
            .fetch_one(&mut *conn)
            .await?;
        if taken {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Drops each of `names` in `schema` that exists but is INVALID (a failed
/// concurrent build), so its migration builds it again.
///
/// # Arguments
/// * `conn` - a connection outside any transaction (`DROP INDEX
///   CONCURRENTLY` cannot run in one).
/// * `schema` - the schema holding the indexes (`public` or `catalog`); a
///   schema that does not exist holds none.
/// * `names` - the index names ([`CONCURRENT_INDEXES`] or
///   [`CATALOG_CONCURRENT_INDEXES`]).
///
/// # Returns
/// How many it dropped.
///
/// # Errors
/// [`sqlx::Error`] if a read or a drop fails.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "db.drop_invalid_indexes"))]
async fn drop_invalid_indexes(
    conn: &mut sqlx::PgConnection,
    schema: &str,
    names: &[&str],
) -> Result<usize, sqlx::Error> {
    // Joined by namespace name, not cast with `::regnamespace`, which fails
    // for a schema that does not exist yet (a new store's `catalog`).
    let invalid: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname::text FROM pg_index i \
         JOIN pg_class c ON c.oid = i.indexrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = $1 \
           AND c.relname = ANY($2) AND NOT i.indisvalid",
    )
    .bind(schema)
    .bind(names)
    .fetch_all(&mut *conn)
    .await?;
    for name in &invalid {
        tracing::warn!(index = %name, "dropping an invalid index left by a failed build");
        // The name is one of the constants above, never input: safe to
        // splice (an identifier cannot be a bound parameter).
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP INDEX CONCURRENTLY IF EXISTS {schema}.{name}"
        )))
        .execute(&mut *conn)
        .await?;
    }
    Ok(invalid.len())
}

/// Applies every okf migration not yet applied, then the account database's
/// own catalog store (`main`: adoption the first time, see
/// [`migrate_store`]), then apalis' own (each a no-op when up to date).
///
/// apalis-postgres keeps its tables in schema `apalis` and its history in
/// `apalis._sqlx_migrations`, apart from okf's `public._sqlx_migrations`, so
/// neither migrator ever sees the other's rows. Its migrations create the
/// `pgcrypto` and `hstore` extensions, which (like `citext`) are trusted:
/// the database owner may create them.
///
/// # Errors
/// [`OkfError::Migrate`] if a migration fails, or the database has a
/// migration this binary does not know (it is older than the schema);
/// [`OkfError::Database`] if apalis' setup cannot reach the database.
#[tracing::instrument(level = "debug", skip_all)]
pub async fn migrate(pool: &PgPool) -> Result<(), OkfError> {
    // One connection holds the lock for the cleanup and the migrations, so
    // two replicas starting together never drop an index the other is
    // still building (one being built CONCURRENTLY is INVALID until done).
    // The migrator runs on that same connection: a pool of one
    // (`database_max_connections = 1`) has no second to give it.
    let mut conn = pool.acquire().await?;
    lock_migrations(&mut conn).await?;
    // The lock is released below whatever this returns, so no `?` here.
    let migrated = match drop_invalid_indexes(&mut conn, "public", &CONCURRENT_INDEXES).await {
        // `&mut *conn`: the pooled connection itself (sqlx's migrator takes
        // its own advisory lock on it, and runs `no-transaction` files on
        // it outside any transaction, as on a pool).
        Ok(_) => match MIGRATOR.run(&mut *conn).await {
            // Then the catalog schema the account migrations built (0041,
            // 0042): the account database is store `main`, adopted the
            // first time and brought up to date after, inside the same hold.
            Ok(()) => migrate_store_locked(&mut conn, &StoreName::main())
                .await
                .map(|_| ()),
            Err(error) => Err(OkfError::from(error)),
        },
        Err(error) => Err(OkfError::from(error)),
    };
    // Released on every path; a lost connection releases it too.
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(CONCURRENT_INDEX_LOCK)
        .execute(&mut *conn)
        .await?;
    // Back to the pool before apalis' setup, which takes one of its own.
    drop(conn);
    migrated?;
    // `::<()>`: `setup` is defined on `PostgresStorage<()>`; the job type
    // a storage carries does not matter for creating the schema.
    apalis_postgres::PostgresStorage::<()>::setup(pool)
        .await
        .map_err(apalis_error)?;
    Ok(())
}

/// What [`migrate_store`] did to a store's database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StoreMigration {
    /// The baseline was recorded as applied without running it: the
    /// database already held the catalog schema (the account database
    /// after migrations 0041 and 0042).
    pub adopted: bool,
    /// How many catalog migrations ran (an adopted baseline not counted).
    pub applied: usize,
}

impl std::fmt::Display for StoreMigration {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "a Display impl")
    )]
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.adopted, self.applied) {
            (false, 0) => formatter.write_str("up to date"),
            (true, applied) => write!(formatter, "adopted, {applied} applied"),
            (false, applied) => write!(formatter, "{applied} applied"),
        }
    }
}

/// Brings a store's `catalog` schema up to date: the catalog migrations,
/// adoption of a schema the account migrations built, and the store's
/// identity row.
///
/// A store that is already current (every embedded migration recorded with
/// its checksum, and its identity row naming `store`) is found so with
/// reads alone, without the lock and without any privilege beyond reading
/// the two tables. Otherwise the work runs under the advisory lock
/// [`migrate`] uses, so two replicas opening one store together migrate it
/// once (the lock is per database: it never waits on another store).
///
/// # Arguments
/// * `conn` - a connection to the store's database, outside any
///   transaction (sqlx runs each migration in a transaction of its own). It
///   may have any `search_path`: every name here is schema-qualified.
/// * `store` - the store the database is opened as.
///
/// # Returns
/// What was done.
///
/// # Errors
/// [`OkfError::Refused`] if the database's identity row names another store
/// (it was reached under a second name); [`OkfError::Migrate`] if a
/// migration fails or the database has one this binary does not know;
/// [`OkfError::Database`] (or `Busy`) for a driver error.
#[tracing::instrument(level = "debug", skip_all, fields(store = %store))]
pub async fn migrate_store(
    conn: &mut PgConnection,
    store: &StoreName,
) -> Result<StoreMigration, OkfError> {
    if store_is_current(conn, store).await? {
        return Ok(StoreMigration::default());
    }
    lock_migrations(conn).await?;
    // The lock is released below whatever this returns, so no `?` here.
    let migrated = migrate_store_locked(conn, store).await;
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(CONCURRENT_INDEX_LOCK)
        .execute(&mut *conn)
        .await?;
    migrated
}

/// Whether a store needs nothing from [`migrate_store`]: its history holds
/// exactly the embedded migrations, each succeeded with the same checksum,
/// and its identity row names `store`. Reads only.
///
/// # Errors
/// [`OkfError::Database`] (or `Busy`) for a driver error.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "db.store_is_current"))]
async fn store_is_current(conn: &mut PgConnection, store: &StoreName) -> Result<bool, OkfError> {
    // `to_regclass` reads no table: NULL when the table (or the schema) is
    // missing, where a SELECT from it would fail.
    let (history, identity): (bool, bool) = sqlx::query_as(
        "SELECT to_regclass('catalog._sqlx_migrations') IS NOT NULL, \
                to_regclass('catalog.store_identity') IS NOT NULL",
    )
    .fetch_one(&mut *conn)
    .await?;
    if !(history && identity) {
        return Ok(false);
    }
    let applied: Vec<(i64, Vec<u8>, bool)> = sqlx::query_as(
        "SELECT version, checksum, success FROM catalog._sqlx_migrations ORDER BY version",
    )
    .fetch_all(&mut *conn)
    .await?;
    // `migrate!` lists the files in version order, as the query sorts.
    let embedded = CATALOG_MIGRATOR
        .iter()
        .filter(|migration| !migration.migration_type.is_down_migration());
    let same_history = applied.len() == embedded.clone().count()
        && applied
            .iter()
            .zip(embedded)
            .all(|((version, checksum, success), migration)| {
                *success && *version == migration.version && **checksum == *migration.checksum
            });
    if !same_history {
        return Ok(false);
    }
    let owner = store_owner(conn).await?;
    Ok(owner.as_deref() == Some(store.as_str()))
}

/// [`migrate_store`]'s work, on a connection that already holds
/// `CONCURRENT_INDEX_LOCK` ([`migrate`] calls it for `main` inside its own
/// hold, so a pool of one connection is enough).
///
/// 1. Creates schema `catalog` if absent (a new store), so the history table
///    has somewhere to go.
/// 2. Adoption: when `catalog.catalog_state` exists and the history has no
///    rows, the account migrations built the schema, so the baseline is
///    recorded as applied (version, description and the checksum embedded
///    in this binary) instead of run.
/// 3. Runs every catalog migration not yet applied.
/// 4. Writes the identity row if there is none, then refuses one naming
///    another store.
///
/// # Errors
/// As for [`migrate_store`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "db.migrate_store"))]
async fn migrate_store_locked(
    conn: &mut PgConnection,
    store: &StoreName,
) -> Result<StoreMigration, OkfError> {
    let has_schema: bool = sqlx::query_scalar("SELECT to_regnamespace('catalog') IS NOT NULL")
        .fetch_one(&mut *conn)
        .await?;
    if !has_schema {
        sqlx::query("CREATE SCHEMA catalog")
            .execute(&mut *conn)
            .await?;
    }
    let has_state: bool =
        sqlx::query_scalar("SELECT to_regclass('catalog.catalog_state') IS NOT NULL")
            .fetch_one(&mut *conn)
            .await?;
    // A failed concurrent build of a catalog index, before the migrator
    // would find its name taken (`CREATE INDEX IF NOT EXISTS`).
    drop_invalid_indexes(conn, "catalog", &CATALOG_CONCURRENT_INDEXES).await?;
    let before = history_len(conn).await?;
    let adopted = has_state && before == 0;
    if adopted {
        // `skip = true` writes the history row of every migration up to the
        // target without running its SQL: here, the baseline alone.
        // `run_direct` is what `skip` and `run` call; it takes the connection
        // itself, where their `Acquire` bound is "not general enough" for a
        // future that must be `Send` (sqlx documents it as the way around).
        CATALOG_MIGRATOR
            .run_direct(Some(BASELINE_VERSION), &mut *conn, true)
            .await?;
        tracing::info!(store = %store, "adopted the catalog schema: baseline recorded as applied");
    }
    let skipped = usize::from(adopted);
    CATALOG_MIGRATOR.run_direct(None, &mut *conn, false).await?;
    let after = history_len(conn).await?;
    // A store with no identity row takes this store's name; one that has a
    // row keeps it.
    sqlx::query(
        "INSERT INTO catalog.store_identity (name) SELECT $1 \
         WHERE NOT EXISTS (SELECT 1 FROM catalog.store_identity)",
    )
    .bind(store.as_str())
    .execute(&mut *conn)
    .await?;
    let owner = store_owner(conn).await?;
    if owner.as_deref() != Some(store.as_str()) {
        return Err(OkfError::Refused {
            reason: format!(
                "this database belongs to store {}, not {store}",
                owner.unwrap_or_default()
            ),
        });
    }
    // `try_from`: the counts are row counts, never negative.
    let applied = usize::try_from(after - before)
        .unwrap_or_default()
        .saturating_sub(skipped);
    Ok(StoreMigration { adopted, applied })
}

/// How many rows the store's migration history has (0 when the table does
/// not exist yet).
///
/// # Errors
/// [`OkfError::Database`] (or `Busy`) for a driver error.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "db.history_len"))]
async fn history_len(conn: &mut PgConnection) -> Result<i64, OkfError> {
    let exists: bool =
        sqlx::query_scalar("SELECT to_regclass('catalog._sqlx_migrations') IS NOT NULL")
            .fetch_one(&mut *conn)
            .await?;
    if !exists {
        return Ok(0);
    }
    Ok(
        sqlx::query_scalar("SELECT count(*) FROM catalog._sqlx_migrations")
            .fetch_one(&mut *conn)
            .await?,
    )
}

/// The store a database says it belongs to: its `catalog.store_identity`
/// row.
///
/// # Arguments
/// * `conn` - a connection to the database (any `search_path`).
///
/// # Returns
/// The name, or `None` when the table or its row does not exist yet (a
/// database no store has migrated).
///
/// # Errors
/// [`OkfError::Database`] (or `Busy`) for a driver error.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "db.store_owner"))]
pub async fn store_owner(conn: &mut PgConnection) -> Result<Option<String>, OkfError> {
    let exists: bool =
        sqlx::query_scalar("SELECT to_regclass('catalog.store_identity') IS NOT NULL")
            .fetch_one(&mut *conn)
            .await?;
    if !exists {
        return Ok(None);
    }
    Ok(
        sqlx::query_scalar("SELECT name FROM catalog.store_identity")
            .fetch_optional(&mut *conn)
            .await?,
    )
}

/// Replaces a database's identity row with `store`: a reclaim of a database
/// whose previous claimant was never registered. The caller has checked that
/// the old name has no row and holds the add's location lock.
///
/// # Errors
/// [`OkfError::Database`] (or `Busy`) for a driver error.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "db.replace_store_identity"))]
pub async fn replace_store_identity(
    conn: &mut PgConnection,
    store: &StoreName,
) -> Result<(), OkfError> {
    sqlx::query("UPDATE catalog.store_identity SET name = $1")
        .bind(store.as_str())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Whether a database holds the account tables (`public.stores`): it is the
/// account database, whatever name reached it.
///
/// # Arguments
/// * `conn` - a connection to the database (any `search_path`).
///
/// # Errors
/// [`OkfError::Database`] (or `Busy`) for a driver error.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "db.holds_account_tables"))]
pub async fn holds_account_tables(conn: &mut PgConnection) -> Result<bool, OkfError> {
    Ok(
        sqlx::query_scalar("SELECT to_regclass('public.stores') IS NOT NULL")
            .fetch_one(&mut *conn)
            .await?,
    )
}

/// An apalis setup failure as an [`OkfError`].
///
/// # Arguments
/// * `error` - what `PostgresStorage::setup` returned.
///
/// # Returns
/// [`OkfError::Migrate`] for a migration failure (including apalis' own
/// "upgrade required" refusal), [`OkfError::Database`] (or `Busy`) for a
/// driver error.
#[tracing::instrument(level = "trace", skip_all)]
fn apalis_error(error: apalis_postgres::Error) -> OkfError {
    match error {
        // apalis wraps its migrator's error in a driver error; unwrap it.
        // `*source` moves the `MigrateError` out of its `Box`.
        apalis_postgres::Error::Database(sqlx::Error::Migrate(source)) => {
            OkfError::Migrate { source: *source }
        }
        apalis_postgres::Error::Database(source) => OkfError::from(source),
        // `setup` returns `UpgradeRequired` for a pre-1.0 apalis history; the
        // other variants are about running jobs, not setup. Each keeps its
        // text. Listed rather than `_`, so a new apalis variant is a compile
        // error here until someone decides what it means. `other @ (...)`
        // binds the whole matched value to `other`.
        other @ (apalis_postgres::Error::UpgradeRequired { .. }
        | apalis_postgres::Error::JsonError(_)
        | apalis_postgres::Error::ReenqueueMismatch { .. }
        | apalis_postgres::Error::TaskIdError(_)
        | apalis_postgres::Error::StatusError(_)
        | apalis_postgres::Error::WorkerOutOfSync
        | apalis_postgres::Error::WorkerAlreadyExists(_)) => OkfError::Migrate {
            source: sqlx::migrate::MigrateError::Source(Box::new(std::io::Error::other(
                other.to_string(),
            ))),
        },
    }
}

/// The user (and its personal team) every unit-test database starts with,
/// so tests that create keys, codes and catalogs have an owner to name.
#[cfg(test)]
pub(crate) const TEST_USER: &str = "test-user";
/// [`TEST_USER`]'s personal team.
#[cfg(test)]
pub(crate) const TEST_TEAM: &str = TEST_USER;

/// A pool on the store `main` of the test whose account pool is `accounts`:
/// the account database in the default store mode, the second database in
/// `OKF_TEST_STORE=separate`. For test code that reads or writes catalog
/// data (schema `catalog`) with a statement of its own.
#[cfg(test)]
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(uninstrumented_fn, reason = "a test helper")
)]
pub(crate) fn store_pool(accounts: &PgPool) -> PgPool {
    crate::stores::CatalogDb::main_of(accounts)
        .executor()
        .clone()
}

/// Puts the store `main` back on the account database: for the tests of the
/// release-one layout, which `OKF_TEST_STORE=separate` mode moves to a
/// second database.
#[cfg(test)]
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(uninstrumented_fn, reason = "a test helper")
)]
pub(crate) async fn main_in_the_account_database(accounts: &PgPool) {
    sqlx::query(
        "UPDATE stores SET location_kind = 'account-database', host = NULL, port = NULL, \
         database = NULL, credential_kind = NULL, credential_ref = NULL WHERE name = 'main'",
    )
    .execute(accounts)
    .await
    .expect("put the store main on the account database");
}

/// The id of the catalog called `name`, from the registry in the account
/// database: for test statements that name a catalog's rows in its store,
/// which a subquery on `catalogs` cannot reach once the two are separate
/// databases.
#[cfg(test)]
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(uninstrumented_fn, reason = "a test helper")
)]
pub(crate) async fn catalog_id(accounts: &PgPool, name: &str) -> i64 {
    sqlx::query_scalar("SELECT id FROM catalogs WHERE name = $1")
        .bind(name)
        .fetch_one(accounts)
        .await
        .expect("the catalog is registered")
}

/// A migrated throwaway database for one test, with [`TEST_USER`].
#[cfg(test)]
pub(crate) async fn test_db() -> pg_support::TestDb {
    with_fixture_user(pg_support::TestDb::start_migrated().await).await
}

/// As [`test_db`], with schema `catalog` in the account database in every
/// `OKF_TEST_STORE` mode ([`pg_support::TestDb::start_account_layout`]): for
/// a test of what the account migrations build, or of the store `main` being
/// the account database.
#[cfg(test)]
pub(crate) async fn test_db_account_layout() -> pg_support::TestDb {
    with_fixture_user(pg_support::TestDb::start_account_layout().await).await
}

/// Creates [`TEST_USER`] in `db`.
#[cfg(test)]
async fn with_fixture_user(db: pg_support::TestDb) -> pg_support::TestDb {
    let fixture = crate::store::users::NewUser {
        name: TEST_USER,
        display_name: &crate::labels::DisplayName::default(),
    };
    let actor = crate::store::audit::AuditActor::cli();
    crate::store::users::create(&db.pool, &fixture, &actor, chrono::Utc::now())
        .await
        .expect("create the fixture user");
    db
}

#[cfg(test)]
mod tests {
    use super::*;

    use apalis_postgres::PostgresStorage;

    /// How many migrations `apalis._sqlx_migrations` records.
    async fn apalis_history(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM apalis._sqlx_migrations")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// How many migrations okf's own `_sqlx_migrations` records.
    async fn okf_history(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM public._sqlx_migrations")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn migrations_create_the_schema_and_rerun_as_a_no_op() {
        // A bare database, not `test_db()`: its fixture user would bump the
        // admin generation this test expects migrations to leave at zero.
        let db = pg_support::TestDb::start().await;
        migrate(&db.pool).await.unwrap();
        let apalis_once = apalis_history(&db.pool).await;

        migrate(&db.pool).await.unwrap();

        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT table_name::text FROM information_schema.tables \
             WHERE table_schema = 'public' AND table_name <> '_sqlx_migrations' \
             ORDER BY table_name COLLATE \"C\"",
        )
        .fetch_all(&db.pool)
        .await
        .unwrap();
        assert_eq!(
            tables,
            [
                "api_keys",
                "audit_access_counts",
                "audit_credentials",
                "audit_entries",
                "audit_people",
                "audit_teams",
                "catalogs",
                "cluster_stats",
                "cluster_store_stats",
                "email_tokens",
                "feedback",
                "key_catalogs",
                "oauth_clients",
                "oauth_tokens",
                "server_keys",
                "server_settings",
                "server_state",
                "sign_in_code_catalogs",
                "sign_in_codes",
                "sign_in_events",
                "store_cleanups",
                "stores",
                "team_members",
                "teams",
                "unlocks",
                "user_catalogs",
                "user_identities",
                "user_passkeys",
                "user_sessions",
                "users",
            ]
        );
        // The catalog's own tables live in schema `catalog` (0041; 0042 adds
        // its audit), beside the catalog migrator's history and the store's
        // identity (catalog migration 0002).
        let catalog_tables: Vec<String> = sqlx::query_scalar(
            "SELECT table_name::text FROM information_schema.tables \
             WHERE table_schema = 'catalog' ORDER BY table_name COLLATE \"C\"",
        )
        .fetch_all(&db.pool)
        .await
        .unwrap();
        assert_eq!(
            catalog_tables,
            [
                "_sqlx_migrations",
                "catalog_audit_entries",
                "catalog_state",
                "changeset_entries",
                "changesets",
                "documents",
                "maintenance_dispositions",
                "maintenance_evidence",
                "maintenance_findings",
                "maintenance_requests",
                "maintenance_runs",
                "maintenance_state",
                "maintenance_work",
                "revisions",
                "store_identity",
            ]
        );
        // apalis keeps everything in its own schema, with its own history.
        let apalis_tables: Vec<String> = sqlx::query_scalar(
            "SELECT table_name::text FROM information_schema.tables \
             WHERE table_schema = 'apalis' ORDER BY table_name COLLATE \"C\"",
        )
        .fetch_all(&db.pool)
        .await
        .unwrap();
        for table in ["_sqlx_migrations", "jobs", "workers"] {
            assert!(
                apalis_tables.iter().any(|name| name == table),
                "{apalis_tables:?}"
            );
        }
        let embedded = i64::try_from(PostgresStorage::<()>::migrations().iter().count()).unwrap();
        assert_eq!(apalis_once, embedded, "every apalis migration applied once");
        assert_eq!(
            apalis_history(&db.pool).await,
            apalis_once,
            "the rerun is a no-op"
        );
        assert_eq!(
            okf_history(&db.pool).await,
            i64::try_from(MIGRATOR.iter().count()).unwrap(),
            "okf's history holds only okf's migrations"
        );
        let admin_generation: i64 = sqlx::query_scalar("SELECT admin_generation FROM server_state")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(admin_generation, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn migration_0010_keeps_every_row_and_both_migrators_rerun_as_no_ops() {
        // Arrange: a database the real migrator took to 0009 (so its history
        // is what a v0.14 server left), with a user in her personal team,
        // a catalog that team owns, a key with one grant and an OAuth grant.
        let db = pg_support::TestDb::start().await;
        MIGRATOR.run_to(9, &db.pool).await.unwrap();
        sqlx::raw_sql(
            "INSERT INTO users (name, created_at) VALUES ('alice', now());
             INSERT INTO teams (name, personal_of, created_at)
                 SELECT 'alice', id, now() FROM users WHERE name = 'alice';
             INSERT INTO team_members (team_id, user_id, role, created_at)
                 SELECT t.id, u.id, 'owner', now() FROM teams t, users u
                 WHERE t.name = 'alice' AND u.name = 'alice';
             INSERT INTO catalogs (name, created_at, owner_team_id)
                 SELECT 'pg', now(), id FROM teams WHERE name = 'alice';
             INSERT INTO api_keys (name, sha256, access, all_catalogs, actor, created_at, user_id)
                 SELECT 'laptop', decode(repeat('ab', 32), 'hex'), 'write', false, 'alice',
                        now(), id
                 FROM users WHERE name = 'alice';
             INSERT INTO key_catalogs (key_id, catalog_id)
                 SELECT k.id, c.id FROM api_keys k, catalogs c
                 WHERE k.name = 'laptop' AND c.name = 'pg';
             INSERT INTO oauth_clients (client_id, kind, redirect_uris, metadata, created_at)
                 VALUES ('c1', 'dcr', '{https://client.example.com/cb}', '{}', now());
             INSERT INTO api_keys (name, kind, client_id, access, all_catalogs, actor, created_at)
                 VALUES ('laptop.grant', 'oauth', 'c1', 'read', true, 'alice', now());",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        assert_eq!(okf_history(&db.pool).await, 9);

        // Act: what a v0.15 server does on start, twice.
        migrate(&db.pool).await.unwrap();
        migrate(&db.pool).await.unwrap();

        // Assert: nothing existing changed meaning.
        // `email::text`: decode citext as plain text.
        let user: (
            Option<String>,
            bool,
            Option<String>,
            i32,
            Option<chrono::DateTime<chrono::Utc>>,
        ) = sqlx::query_as(
            "SELECT email::text, is_admin, password_hash, failed_sign_ins, locked_until \
                 FROM users WHERE name = 'alice'",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(user, (None, false, None, 0, None));
        let keys: Vec<(String, bool, Option<chrono::DateTime<chrono::Utc>>)> =
            sqlx::query_as("SELECT name, self_issued, expires_at FROM api_keys ORDER BY name")
                .fetch_all(&db.pool)
                .await
                .unwrap();
        assert_eq!(
            keys,
            [
                ("laptop".to_string(), false, None),
                ("laptop.grant".to_string(), false, None)
            ]
        );
        let level: Option<String> = sqlx::query_scalar("SELECT access FROM key_catalogs")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(level, None, "an existing grant keeps the key's own level");
        let member_access: String =
            sqlx::query_scalar("SELECT member_access FROM catalogs WHERE name = 'pg'")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(member_access, "write");
        let new_rows: i64 = sqlx::query_scalar(
            "SELECT (SELECT count(*) FROM user_identities) + (SELECT count(*) FROM user_passkeys) \
                  + (SELECT count(*) FROM user_sessions) + (SELECT count(*) FROM email_tokens) \
                  + (SELECT count(*) FROM sign_in_events) + (SELECT count(*) FROM user_catalogs) \
                  + (SELECT count(*) FROM server_settings WHERE key <> 'default_store')",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(
            new_rows, 0,
            "the migrations create no rows but default_store"
        );
        assert_eq!(
            okf_history(&db.pool).await,
            i64::try_from(MIGRATOR.iter().count()).unwrap()
        );
        assert_eq!(
            apalis_history(&db.pool).await,
            i64::try_from(PostgresStorage::<()>::migrations().iter().count()).unwrap()
        );
    }

    /// Inserts an API key row with these columns, named `name` (its hash is
    /// derived from the name, so each call is distinct).
    async fn insert_key(
        pool: &PgPool,
        name: &str,
        flags: (&str, bool, bool, bool),
        user_id: Option<&str>,
    ) -> Result<sqlx::postgres::PgQueryResult, sqlx::Error> {
        // A tuple keeps this at five parameters: (access, maintenance_report,
        // all_catalogs, self_issued).
        let (access, report, all, self_issued) = flags;
        sqlx::query(
            "INSERT INTO api_keys \
             (name, sha256, access, maintenance_report, all_catalogs, actor, created_at, \
              user_id, self_issued) \
             VALUES ($1, sha256($1::bytea), $2, $3, $4, $1, now(), $5, $6)",
        )
        .bind(name)
        .bind(access)
        .bind(report)
        .bind(all)
        .bind(user_id)
        .bind(self_issued)
        .execute(pool)
        .await
    }

    /// The constraint a refused statement broke.
    fn broken<T>(result: Result<T, sqlx::Error>) -> Option<String> {
        result.err().and_then(|error| {
            error
                .as_database_error()
                .and_then(|e| e.constraint().map(str::to_string))
        })
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn migration_0019_normalises_legacy_display_names_and_adds_the_checks() {
        // Arrange: a database at 0018 (what a v0.21 server left), holding the
        // dirty values 0009 never refused. Raw strings (`r#"..."#`): Rust
        // would read `\u2028` as a malformed escape; `E'..'` makes Postgres
        // read `\t`, `\n` and `\uXXXX`.
        let db = pg_support::TestDb::start().await;
        MIGRATOR.run_to(18, &db.pool).await.unwrap();
        sqlx::raw_sql(
            r#"INSERT INTO users (name, display_name, created_at) VALUES
                   ('long', repeat('x', 85), now()),
                   ('ctrl', E'  Alice\tExample\u2028Smith  ', now()),
                   ('wide', E'\u00a0Carol\u3000', now()),
                   ('c1', E'Dan\u009fEve', now()),
                   ('cut', repeat('y', 79) || E' \u0085z', now()),
                   ('clean', 'Bob  Lee', now()),
                   ('unset', '', now());
               INSERT INTO teams (name, display_name, created_at) VALUES
                   ('acme', E'Acme\nLtd', now()),
                   ('zeta', 'Zeta', now());
               INSERT INTO catalogs (name, created_at) VALUES ('pg', now());"#,
        )
        .execute(&db.pool)
        .await
        .unwrap();
        // `xmin` is the id of the transaction that wrote a row's current
        // version: unchanged afterwards means the migration left the row be.
        let untouched = "SELECT xmin::text FROM users WHERE name = 'clean' \
                         UNION ALL SELECT xmin::text FROM teams WHERE name = 'zeta'";
        let before: Vec<String> = sqlx::query_scalar(untouched)
            .fetch_all(&db.pool)
            .await
            .unwrap();

        // Act: the rest of the migrations, 0019 included.
        MIGRATOR.run(&db.pool).await.unwrap();

        // Assert: exact normalised values. Control characters (U+0009,
        // U+0085, U+009F, U+2028) become one space each, Unicode whitespace
        // is trimmed (U+00A0, U+3000), the first 80 characters stay, and the
        // space the cut leaves at the end goes. Inner runs of spaces stay.
        let users: Vec<(String, String)> =
            sqlx::query_as("SELECT name, display_name FROM users ORDER BY name")
                .fetch_all(&db.pool)
                .await
                .unwrap();
        let expected_users = [
            ("c1", "Dan Eve".to_string()),
            ("clean", "Bob  Lee".to_string()),
            ("ctrl", "Alice Example Smith".to_string()),
            ("cut", "y".repeat(79)),
            ("long", "x".repeat(80)),
            ("unset", String::new()),
            ("wide", "Carol".to_string()),
        ]
        .map(|(name, display)| (name.to_string(), display));
        assert_eq!(users, expected_users);
        let teams: Vec<(String, String)> =
            sqlx::query_as("SELECT name, display_name FROM teams ORDER BY name")
                .fetch_all(&db.pool)
                .await
                .unwrap();
        assert_eq!(
            teams,
            [
                ("acme".to_string(), "Acme Ltd".to_string()),
                ("zeta".to_string(), "Zeta".to_string()),
            ]
        );
        // Every value is one the server's own rule accepts unchanged.
        for (_, display) in users.iter().chain(&teams) {
            let parsed = crate::labels::DisplayName::parse(display).unwrap();
            assert_eq!(parsed.as_str(), display);
        }
        let after: Vec<String> = sqlx::query_scalar(untouched)
            .fetch_all(&db.pool)
            .await
            .unwrap();
        assert_eq!(after, before, "clean rows are not rewritten");
        // The existing catalog gets the column, empty.
        let catalog: String = sqlx::query_scalar("SELECT display_name FROM catalogs")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(catalog, "");
        // The CHECKs are in force on all three tables.
        // `AssertSqlSafe`: `table` is one of the literals below.
        for table in ["users", "teams", "catalogs"] {
            let refused = sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE {table} SET display_name = repeat('x', 81)"
            )))
            .execute(&db.pool)
            .await;
            assert_eq!(
                broken(refused).as_deref(),
                Some(format!("{table}_display_name_length").as_str()),
                "{table}: the CHECK should refuse 81 characters"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn migration_0019_makes_a_concurrent_legacy_create_wait_for_the_checks() {
        use crate::store::catalogs::tests::until_locks_are_awaited;
        use std::time::Duration;
        // How long each wait below may take before the test fails.
        const WAIT: Duration = Duration::from_secs(30);

        // Arrange: a database at 0018 with one dirty user, whose row another
        // transaction holds (`FOR UPDATE`), so the migration stops inside
        // its cleanup `UPDATE`: after taking its table locks, before the
        // CHECKs.
        let db = pg_support::TestDb::start().await;
        MIGRATOR.run_to(18, &db.pool).await.unwrap();
        sqlx::raw_sql(
            "INSERT INTO users (name, display_name, created_at) VALUES ('ctrl', E'a\\tb', now());",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        let mut holder = db.pool.begin().await.unwrap();
        sqlx::query("SELECT 1 FROM users WHERE name = 'ctrl' FOR UPDATE")
            .execute(&mut *holder)
            .await
            .unwrap();
        // `tokio::spawn` runs each future concurrently, like a thread.
        let pool = db.pool.clone();
        let migrating = tokio::spawn(async move { MIGRATOR.run(&pool).await });
        until_locks_are_awaited(&db.pool, 1).await;

        // Act: what an older replica's `users::create` writes, with a name
        // 0009 never refused. Without the migration's `LOCK TABLE` it
        // commits at once, the cleanup's snapshot misses it, and the CHECK
        // then fails the migration.
        let pool = db.pool.clone();
        let creating = tokio::spawn(async move {
            sqlx::query(
                "INSERT INTO users (name, display_name, created_at) \
                 VALUES ('late', repeat('x', 81), now())",
            )
            .execute(&pool)
            .await
        });
        // Two waiters: the migration on the held row, the create on the
        // migration's table lock.
        until_locks_are_awaited(&db.pool, 2).await;
        holder.rollback().await.unwrap();

        // Assert: the migration commits; the create, run after it, is
        // refused by the CHECK rather than stored.
        let migrated = tokio::time::timeout(WAIT, migrating)
            .await
            .expect("the migration should finish once the row is released")
            .unwrap();
        assert!(migrated.is_ok(), "{migrated:?}");
        let created = tokio::time::timeout(WAIT, creating)
            .await
            .expect("the create should finish once the migration commits")
            .unwrap();
        assert_eq!(
            broken(created).as_deref(),
            Some("users_display_name_length")
        );
        let cleaned: String =
            sqlx::query_scalar("SELECT display_name FROM users WHERE name = 'ctrl'")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(cleaned, "a b");
    }

    /// Whether `feedback_credential_kind_check` is validated, and its text.
    async fn credential_kind_check(pool: &PgPool) -> (bool, String) {
        sqlx::query_as(
            "SELECT convalidated, pg_get_constraintdef(oid) FROM pg_constraint \
             WHERE conname = 'feedback_credential_kind_check'",
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// Inserts one feedback row with this `credential_kind`.
    async fn insert_feedback(pool: &PgPool, id: &str, kind: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO feedback (id, credential, user_slug, message, kind, origin, \
                 server_version, client, credential_kind, created_at) \
             VALUES ($1, 'k', 'alice', 'hi', 'bug', 'user', '0', 'probe', $2, now())",
        )
        .bind(id)
        .bind(kind)
        .execute(pool)
        .await
        .map(|_| ())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn migrations_0029_and_0030_widen_the_credential_kind_check_and_validate_it() {
        // Arrange: a database at 0028 holding feedback from both MCP kinds.
        let db = pg_support::TestDb::start().await;
        MIGRATOR.run_to(28, &db.pool).await.unwrap();
        insert_feedback(&db.pool, "a", "api_key").await.unwrap();
        insert_feedback(&db.pool, "b", "oauth").await.unwrap();

        // Act and assert: 0029 adds the wider check without scanning the
        // old rows (not yet validated), but checks new ones at once.
        MIGRATOR.run_to(29, &db.pool).await.unwrap();
        let (validated, definition) = credential_kind_check(&db.pool).await;
        assert!(
            !validated,
            "0029 must not validate under its exclusive lock"
        );
        assert!(definition.contains("'session'"), "{definition}");
        insert_feedback(&db.pool, "c", "session").await.unwrap();
        assert!(insert_feedback(&db.pool, "d", "bogus").await.is_err());

        // 0030, and the server's own migrate, leave it validated.
        migrate(&db.pool).await.unwrap();
        assert!(credential_kind_check(&db.pool).await.0, "0030 validates it");
        assert!(insert_feedback(&db.pool, "e", "bogus").await.is_err());
        let kept: i64 = sqlx::query_scalar("SELECT count(*) FROM feedback")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(kept, 3);

        // A fresh database ends in the same state.
        let fresh = pg_support::TestDb::start().await;
        migrate(&fresh.pool).await.unwrap();
        assert_eq!(
            credential_kind_check(&fresh.pool).await,
            credential_kind_check(&db.pool).await
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn migration_0010_constraints_hold() {
        let db = test_db().await;
        sqlx::raw_sql("INSERT INTO catalogs (name, created_at) VALUES ('pg', now());")
            .execute(&db.pool)
            .await
            .unwrap();
        let user_id: String = sqlx::query_scalar("SELECT id FROM users WHERE name = $1")
            .bind(TEST_USER)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        let user = Some(user_id.as_str());

        // A self-issued key: read or write, never a reporter, never every
        // catalog, always a user's.
        insert_key(&db.pool, "mine", ("write", false, false, true), user)
            .await
            .unwrap();
        for (name, flags) in [
            ("admin", ("admin", false, false, true)),
            ("reporter", ("read", true, false, true)),
            ("everything", ("read", false, true, true)),
        ] {
            assert_eq!(
                broken(insert_key(&db.pool, name, flags, user).await).as_deref(),
                Some("api_keys_self_issued_caps"),
                "{name}"
            );
        }
        assert_eq!(
            broken(insert_key(&db.pool, "orphan", ("read", false, false, true), None).await)
                .as_deref(),
            Some("api_keys_self_issued_user")
        );
        // Admin-issued keys are as before.
        insert_key(&db.pool, "root", ("admin", true, true, false), None)
            .await
            .unwrap();

        // Per-catalog levels are read or write; NULL is the key's own.
        let grant = |access: Option<&'static str>| {
            let pool = db.pool.clone();
            async move {
                sqlx::query(
                    "INSERT INTO key_catalogs (key_id, catalog_id, access) \
                     SELECT k.id, c.id, $1 FROM api_keys k, catalogs c \
                     WHERE k.name = 'root' AND c.name = 'pg' \
                     ON CONFLICT (key_id, catalog_id) DO UPDATE SET access = EXCLUDED.access",
                )
                .bind(access)
                .execute(&pool)
                .await
            }
        };
        grant(None).await.unwrap();
        grant(Some("read")).await.unwrap();
        assert_eq!(
            broken(grant(Some("admin")).await).as_deref(),
            Some("key_catalogs_access_level")
        );

        // Entitlements, member access, tokens and events take only their values.
        let checks = [
            (
                "INSERT INTO user_catalogs (user_id, catalog_id, access, created_at) \
                 SELECT $1, id, 'admin', now() FROM catalogs WHERE name = 'pg'",
                "user_catalogs_access_check",
            ),
            (
                "UPDATE catalogs SET member_access = 'owner' WHERE name = 'pg' AND $1 <> ''",
                "catalogs_member_access_check",
            ),
            (
                "INSERT INTO email_tokens (hash, user_id, purpose, created_at, expires_at) \
                 VALUES (sha256('t'::bytea), $1, 'login', now(), now())",
                "email_tokens_purpose_check",
            ),
            (
                "INSERT INTO sign_in_events (user_id, method, outcome, at) \
                 VALUES ($1, 'password', 'maybe', now())",
                "sign_in_events_outcome_check",
            ),
        ];
        for (statement, constraint) in checks {
            let result = sqlx::query(statement)
                .bind(&user_id)
                .execute(&db.pool)
                .await;
            assert_eq!(broken(result).as_deref(), Some(constraint), "{statement}");
        }

        // Email addresses are unique ignoring case (citext).
        sqlx::query("UPDATE users SET email = 'Alice@Example.com' WHERE id = $1")
            .bind(&user_id)
            .execute(&db.pool)
            .await
            .unwrap();
        let clash = sqlx::query(
            "INSERT INTO users (name, email, created_at) VALUES ('bob', 'alice@example.com', now())",
        )
        .execute(&db.pool)
        .await;
        assert_eq!(broken(clash).as_deref(), Some("users_email_key"));
    }

    #[test]
    fn an_apalis_setup_error_is_a_migrate_or_database_error() {
        let upgrade = apalis_error(apalis_postgres::Error::UpgradeRequired {
            current: "0.7.4".into(),
            required: "1.0.0".into(),
            link: None,
        });
        let database = apalis_error(apalis_postgres::Error::Database(sqlx::Error::PoolClosed));
        let migrate = apalis_error(apalis_postgres::Error::Database(sqlx::Error::Migrate(
            Box::new(sqlx::migrate::MigrateError::VersionMismatch(1)),
        )));

        assert!(matches!(upgrade, OkfError::Migrate { .. }), "{upgrade:?}");
        assert!(
            matches!(database, OkfError::Database { .. }),
            "{database:?}"
        );
        assert!(
            matches!(
                migrate,
                OkfError::Migrate {
                    source: sqlx::migrate::MigrateError::VersionMismatch(1)
                }
            ),
            "{migrate:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oauth_schema_keeps_existing_keys_and_guards_grant_rows() {
        // Arrange: the schema as of 0007, with one key, then 0008 on top.
        let db = pg_support::TestDb::start().await;
        for migration in [
            include_str!("../migrations/accounts/0001_schema.sql"),
            include_str!("../migrations/accounts/0002_changeset_identity.sql"),
            include_str!("../migrations/accounts/0003_changeset_handles.sql"),
            include_str!("../migrations/accounts/0004_drop_changeset_labels.sql"),
            include_str!("../migrations/accounts/0005_key_names.sql"),
            include_str!("../migrations/accounts/0006_maintenance.sql"),
            include_str!("../migrations/accounts/0007_maintenance_assessment_evidence.sql"),
        ] {
            sqlx::raw_sql(migration).execute(&db.pool).await.unwrap();
        }
        let now = chrono::Utc::now();
        sqlx::query(
            "INSERT INTO api_keys (name, sha256, access, all_catalogs, actor, created_at) \
             VALUES ('existing', $1, 'write', true, 'example-actor', $2)",
        )
        .bind(vec![7u8; 32])
        .bind(now)
        .execute(&db.pool)
        .await
        .unwrap();

        // Act
        sqlx::raw_sql(include_str!("../migrations/accounts/0008_oauth.sql"))
            .execute(&db.pool)
            .await
            .unwrap();

        // Assert: the old key is a `key` row with its hash and no client.
        let (kind, sha256, client_id): (String, Option<Vec<u8>>, Option<String>) =
            sqlx::query_as("SELECT kind, sha256, client_id FROM api_keys WHERE name = 'existing'")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(kind, "key");
        assert_eq!(sha256, Some(vec![7u8; 32]));
        assert_eq!(client_id, None);

        // A grant row needs no hash, may not have one, and may not be admin
        // or report maintenance.
        let grant = |sha256: Option<Vec<u8>>, access: &'static str, report: bool| {
            let pool = db.pool.clone();
            async move {
                sqlx::query(
                    "INSERT INTO api_keys \
                     (name, sha256, access, maintenance_report, all_catalogs, actor, created_at, kind) \
                     VALUES ('laptop', $1, $2, $3, true, 'example-actor', $4, 'oauth')",
                )
                .bind(sha256)
                .bind(access)
                .bind(report)
                .bind(now)
                .execute(&pool)
                .await
            }
        };
        let constraint = |result: Result<_, sqlx::Error>| {
            result
                .expect_err("the row must be refused")
                .as_database_error()
                .and_then(|error| error.constraint().map(str::to_string))
        };
        assert!(grant(None, "write", false).await.is_ok());
        // Two live grants with one name and actor: allowed (the unique index
        // covers `kind = 'key'` rows only).
        assert!(grant(None, "read", false).await.is_ok());
        assert_eq!(
            constraint(grant(Some(vec![8u8; 32]), "write", false).await).as_deref(),
            Some("api_keys_kind_hash")
        );
        assert_eq!(
            constraint(grant(None, "admin", false).await).as_deref(),
            Some("api_keys_oauth_caps")
        );
        assert_eq!(
            constraint(grant(None, "read", true).await).as_deref(),
            Some("api_keys_oauth_caps")
        );
        // A sign-in code names a grant only once redeemed, and keeps its
        // redemption when that grant row is deleted (`ON DELETE SET NULL`).
        let grant_id: String = sqlx::query_scalar(
            "INSERT INTO api_keys (name, access, all_catalogs, actor, created_at, kind) \
             VALUES ('doomed', 'read', true, 'example-actor', $1, 'oauth') RETURNING id",
        )
        .bind(now)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        let code = |byte: u8, redeemed: bool| {
            let pool = db.pool.clone();
            let grant_id = grant_id.clone();
            async move {
                sqlx::query(
                    "INSERT INTO sign_in_codes \
                     (name, sha256, access, all_catalogs, actor, created_at, expires_at, \
                      redeemed_at, grant_id) \
                     VALUES ('laptop', $1, 'read', true, 'example-actor', $2, $3, $4, $5)",
                )
                .bind(vec![byte; 32])
                .bind(now)
                .bind(now + chrono::TimeDelta::hours(1))
                .bind(redeemed.then_some(now))
                .bind(grant_id)
                .execute(&pool)
                .await
            }
        };
        assert_eq!(
            constraint(code(1, false).await).as_deref(),
            Some("sign_in_codes_redeemed_grant")
        );
        code(2, true).await.unwrap();
        sqlx::query("DELETE FROM api_keys WHERE id = $1")
            .bind(&grant_id)
            .execute(&db.pool)
            .await
            .expect("deleting a redeemed code's grant clears the code's grant_id");
        let (redeemed_at, kept_grant): (Option<chrono::DateTime<chrono::Utc>>, Option<String>) =
            sqlx::query_as("SELECT redeemed_at, grant_id FROM sign_in_codes")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!((redeemed_at.is_some(), kept_grant), (true, None));
        // A key row still needs its hash, and a second live key of one
        // name and actor is still refused.
        let key = |sha256: Option<Vec<u8>>| {
            let pool = db.pool.clone();
            async move {
                sqlx::query(
                    "INSERT INTO api_keys (name, sha256, access, all_catalogs, actor, created_at) \
                     VALUES ('existing', $1, 'read', true, 'example-actor', $2)",
                )
                .bind(sha256)
                .bind(now)
                .execute(&pool)
                .await
            }
        };
        assert_eq!(
            constraint(key(None).await).as_deref(),
            Some("api_keys_kind_hash")
        );
        assert_eq!(
            constraint(key(Some(vec![9u8; 32])).await).as_deref(),
            Some("api_keys_live_name_actor")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn migration_0009_keeps_every_credential_and_catalog_unassigned() {
        // Arrange: the schema as of 0008, with one catalog, one key, one grant
        // and one sign-in code, written as the pre-0009 server wrote them.
        let db = pg_support::TestDb::start().await;
        for migration in [
            include_str!("../migrations/accounts/0001_schema.sql"),
            include_str!("../migrations/accounts/0002_changeset_identity.sql"),
            include_str!("../migrations/accounts/0003_changeset_handles.sql"),
            include_str!("../migrations/accounts/0004_drop_changeset_labels.sql"),
            include_str!("../migrations/accounts/0005_key_names.sql"),
            include_str!("../migrations/accounts/0006_maintenance.sql"),
            include_str!("../migrations/accounts/0007_maintenance_assessment_evidence.sql"),
            include_str!("../migrations/accounts/0008_oauth.sql"),
        ] {
            sqlx::raw_sql(migration).execute(&db.pool).await.unwrap();
        }
        sqlx::raw_sql(
            "INSERT INTO catalogs (name, created_at) VALUES ('legacy', now());
             INSERT INTO api_keys (name, sha256, access, all_catalogs, actor, created_at)
                 VALUES ('old', decode(repeat('ab', 32), 'hex'), 'write', true, 'old', now());
             INSERT INTO oauth_clients (client_id, kind, redirect_uris, metadata, created_at)
                 VALUES ('c1', 'dcr', '{https://client.example.com/cb}', '{}', now());
             INSERT INTO api_keys (name, kind, client_id, access, all_catalogs, actor, created_at)
                 VALUES ('old.grant', 'oauth', 'c1', 'read', true, 'old', now());
             INSERT INTO sign_in_codes (name, sha256, access, all_catalogs, actor, created_at, expires_at)
                 VALUES ('old-code', decode(repeat('cd', 32), 'hex'), 'read', true, 'old',
                         now(), now() + interval '1 day');",
        )
        .execute(&db.pool)
        .await
        .unwrap();

        // Act
        sqlx::raw_sql(include_str!("../migrations/accounts/0009_users_teams.sql"))
            .execute(&db.pool)
            .await
            .unwrap();

        // Assert: every row survived, none was attributed to anyone.
        let unassigned: (i64, i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM api_keys WHERE user_id IS NULL), \
                    (SELECT count(*) FROM sign_in_codes WHERE user_id IS NULL), \
                    (SELECT count(*) FROM catalogs WHERE owner_team_id IS NULL)",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(unassigned, (2, 1, 1));
        let users: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(users, 0, "the migration creates no user");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_users_credentials_go_with_it_but_its_personal_team_restricts() {
        // Arrange: alice holds a key, a sign-in code and a membership, and
        // bob has a personal team (migration 0013).
        let db = test_db().await;
        sqlx::raw_sql(
            "INSERT INTO users (name, created_at) VALUES ('alice', now()), ('bob', now());
             INSERT INTO api_keys (name, sha256, access, all_catalogs, actor, created_at, user_id)
                 SELECT 'k', decode(repeat('ef', 32), 'hex'), 'read', true, 'k', now(), id
                 FROM users WHERE name = 'alice';
             INSERT INTO sign_in_codes
                     (name, sha256, access, all_catalogs, actor, created_at, expires_at, user_id)
                 SELECT 'c', decode(repeat('cd', 32), 'hex'), 'read', true, 'c', now(),
                        now() + interval '1 hour', id
                 FROM users WHERE name = 'alice';
             INSERT INTO teams (name, created_at) VALUES ('family', now());
             INSERT INTO team_members (team_id, user_id, role, created_at)
                 SELECT t.id, u.id, 'member', now() FROM teams t, users u
                 WHERE t.name = 'family' AND u.name = 'alice';
             INSERT INTO teams (name, personal_of, created_at)
                 SELECT 'bob', id, now() FROM users WHERE name = 'bob';",
        )
        .execute(&db.pool)
        .await
        .unwrap();

        // Act
        sqlx::query("DELETE FROM users WHERE name = 'alice'")
            .execute(&db.pool)
            .await
            .unwrap();
        let bob = sqlx::query("DELETE FROM users WHERE name = 'bob'")
            .execute(&db.pool)
            .await;

        // Assert: the key, code and membership cascaded; `teams.personal_of`
        // stays `ON DELETE RESTRICT`.
        let left: i64 = sqlx::query_scalar(
            "SELECT (SELECT count(*) FROM api_keys) + (SELECT count(*) FROM sign_in_codes) \
                  + (SELECT count(*) FROM team_members m JOIN teams t ON t.id = m.team_id \
                     WHERE t.name = 'test-team' AND m.user_id NOT IN (SELECT id FROM users))",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(left, 0);
        let error = bob.expect_err("RESTRICT refuses");
        assert_eq!(
            error.as_database_error().and_then(|e| e.code()).as_deref(),
            Some("23503"),
            "a foreign-key violation"
        );
    }

    /// Every audit row, as `(table, entity id, name or label, person's user)`,
    /// sorted: what the backfill test compares.
    async fn audit_rows(pool: &PgPool) -> Vec<(String, String, Option<String>, Option<String>)> {
        let mut rows: Vec<(String, String, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT 'person', user_id, name, user_id FROM audit_people \
             UNION ALL SELECT 'team', a.team_id, a.name, p.user_id FROM audit_teams a \
                 LEFT JOIN audit_people p ON p.id = a.person_id \
             UNION ALL SELECT 'credential', a.credential_id, a.label, p.user_id \
                 FROM audit_credentials a LEFT JOIN audit_people p ON p.id = a.person_id",
        )
        .fetch_all(pool)
        .await
        .unwrap();
        // Sorted here, not by `ORDER BY`, whose collation may order `-`
        // differently from Rust's byte order.
        rows.sort();
        rows
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_backfill_creates_missing_rows_and_is_idempotent() {
        // Arrange: rows written directly, as before the audit log, so none
        // has its audit row: bob with a personal team, a shared team, a key
        // of bob's, a legacy key with no user, an OAuth grant and a revoked
        // key. The fixture user's rows exist already, and one is stale (a
        // renamed user): the backfill must leave it as it is.
        let db = test_db().await;
        sqlx::raw_sql(
            "INSERT INTO users (id, name, display_name, created_at)
                 VALUES ('u-bob', 'bob', 'Bob', now());
             INSERT INTO teams (id, name, personal_of, created_at)
                 VALUES ('t-bob', 'bob', 'u-bob', now());
             INSERT INTO teams (id, name, created_at) VALUES ('t-family', 'family', now());
             INSERT INTO api_keys (id, name, sha256, access, all_catalogs, actor, created_at,
                                   user_id)
                 VALUES ('k-bob', 'k-bob', sha256('k-bob'::bytea), 'read', true, 'laptop',
                         now(), 'u-bob');
             INSERT INTO api_keys (id, name, sha256, access, all_catalogs, actor, created_at)
                 VALUES ('k-legacy', 'k-legacy', sha256('k-legacy'::bytea), 'read', true,
                         'ci', now());
             INSERT INTO oauth_clients (client_id, kind, redirect_uris, metadata, created_at)
                 VALUES ('client', 'dcr', '{https://client.example.com/cb}', '{}', now());
             INSERT INTO api_keys (id, name, kind, client_id, access, all_catalogs, actor,
                                   created_at, user_id)
                 VALUES ('g-bob', 'g-bob', 'oauth', 'client', 'read', true, 'Example Client',
                         now(), 'u-bob');
             INSERT INTO api_keys (id, name, sha256, access, all_catalogs, actor, created_at,
                                   revoked_at)
                 VALUES ('k-revoked', 'k-revoked', sha256('k-revoked'::bytea), 'read', true,
                         'old', now(), now());
             UPDATE audit_people SET name = 'stale' WHERE name = 'test-user';",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        let fixture_team: String = sqlx::query_scalar(
            "SELECT id FROM teams WHERE personal_of IS NOT NULL AND id <> 't-bob'",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        let fixture_user: String = sqlx::query_scalar("SELECT id FROM users WHERE id <> 'u-bob'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        let row = |table: &str, id: &str, name: Option<&str>, user: Option<&str>| {
            (
                table.to_string(),
                id.to_string(),
                name.map(str::to_string),
                user.map(str::to_string),
            )
        };

        // Act
        sqlx::raw_sql(include_str!(
            "../migrations/accounts/0023_audit_backfill.sql"
        ))
        .execute(&db.pool)
        .await
        .unwrap();
        let once = audit_rows(&db.pool).await;
        sqlx::raw_sql(include_str!(
            "../migrations/accounts/0023_audit_backfill.sql"
        ))
        .execute(&db.pool)
        .await
        .unwrap();

        // Assert: the rows the resolvers would have made, nothing for the
        // revoked key, the stale row untouched, and a rerun adds nothing.
        let mut expected = vec![
            row("credential", "g-bob", Some("Example Client"), Some("u-bob")),
            row("credential", "k-bob", Some("laptop"), Some("u-bob")),
            row("credential", "k-legacy", Some("ci"), None),
            row("person", &fixture_user, Some("stale"), Some(&fixture_user)),
            row("person", "u-bob", Some("bob"), Some("u-bob")),
            row("team", &fixture_team, None, Some(&fixture_user)),
            row("team", "t-bob", None, Some("u-bob")),
            row("team", "t-family", Some("family"), None),
        ];
        expected.sort();
        assert_eq!(once, expected);
        assert_eq!(audit_rows(&db.pool).await, once, "a rerun changes nothing");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_backfill_waits_for_a_deletion_and_recreates_nothing_it_forgot() {
        use std::time::Duration;
        // Arrange: bob, with a key and a grant that never acted (no audit
        // rows), is being deleted by an old replica during a rolling deploy:
        // `users::delete` holds him `FOR UPDATE` and has run `forget_person`,
        // but has not yet deleted the row.
        let db = test_db().await;
        sqlx::raw_sql(
            "INSERT INTO users (id, name, display_name, created_at)
                 VALUES ('u-bob', 'bob', 'Bob', now());
             INSERT INTO api_keys (id, name, sha256, access, all_catalogs, actor, created_at,
                                   user_id)
                 VALUES ('k-bob', 'k-bob', sha256('k-bob'::bytea), 'read', true, 'laptop',
                         now(), 'u-bob');",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        let mut deletion = db.pool.begin().await.unwrap();
        sqlx::query("SELECT 1 FROM users WHERE id = 'u-bob' FOR UPDATE")
            .execute(&mut *deletion)
            .await
            .unwrap();
        crate::store::audit::forget_person(&mut deletion, "u-bob")
            .await
            .unwrap();

        // Act: the backfill starts while the deletion is in progress, which
        // then commits once the backfill waits on bob's row (or, without the
        // owner lock, once the backfill has already finished).
        let pool = db.pool.clone();
        let backfill = tokio::spawn(async move {
            sqlx::raw_sql(include_str!(
                "../migrations/accounts/0023_audit_backfill.sql"
            ))
            .execute(&pool)
            .await
        });
        let waited = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let waiting: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM pg_stat_activity \
                     WHERE datname = current_database() AND wait_event_type = 'Lock'",
                )
                .fetch_one(&db.pool)
                .await
                .unwrap();
                if waiting > 0 || backfill.is_finished() {
                    return waiting > 0;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("the backfill neither waited on a lock nor finished within 30 s");
        sqlx::query("DELETE FROM users WHERE id = 'u-bob'")
            .execute(&mut *deletion)
            .await
            .unwrap();
        deletion.commit().await.unwrap();
        tokio::time::timeout(Duration::from_secs(30), backfill)
            .await
            .expect("the backfill finished within 30 s of the deletion")
            .unwrap()
            .unwrap();

        // Assert: nothing names bob or his key again.
        let left: Vec<(String, String, Option<String>, Option<String>)> = audit_rows(&db.pool)
            .await
            .into_iter()
            .filter(|(_, id, name, user)| {
                id == "u-bob"
                    || id == "k-bob"
                    || name.as_deref() == Some("bob")
                    || name.as_deref() == Some("laptop")
                    || user.as_deref() == Some("u-bob")
            })
            .collect();
        assert_eq!(
            left,
            [],
            "the backfill re-created what forget_person cleared"
        );
        assert!(waited, "the backfill waited for the deletion's lock");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn maintenance_schema_preserves_existing_rows_and_defaults_to_opt_out() {
        let db = pg_support::TestDb::start().await;
        for migration in [
            include_str!("../migrations/accounts/0001_schema.sql"),
            include_str!("../migrations/accounts/0002_changeset_identity.sql"),
            include_str!("../migrations/accounts/0003_changeset_handles.sql"),
            include_str!("../migrations/accounts/0004_drop_changeset_labels.sql"),
            include_str!("../migrations/accounts/0005_key_names.sql"),
        ] {
            sqlx::raw_sql(migration).execute(&db.pool).await.unwrap();
        }
        let now = chrono::Utc::now();
        let catalog: i64 = sqlx::query_scalar(
            "INSERT INTO catalogs (name, created_at) VALUES ('pg', $1) RETURNING id",
        )
        .bind(now)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO api_keys (id, name, sha256, access, all_catalogs, actor, created_at) \
             VALUES ('existing', 'existing', $1, 'write', true, 'author', $2)",
        )
        .bind(vec![1u8; 32])
        .bind(now)
        .execute(&db.pool)
        .await
        .unwrap();
        for (path, content) in [("a.md", b"first".to_vec()), ("b.md", b"second".to_vec())] {
            sqlx::query(
                "INSERT INTO revisions (catalog_id, path, revision, op, content, executable, actor, message, created_at) \
                 VALUES ($1, $2, 1, 'import', $3, false, 'author', 'initial', $4)",
            )
            .bind(catalog)
            .bind(path)
            .bind(&content)
            .bind(now)
            .execute(&db.pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO documents (catalog_id, path, content, executable, revision, updated_at, updated_by) \
                 VALUES ($1, $2, $3, false, 1, $4, 'author')",
            )
            .bind(catalog)
            .bind(path)
            .bind(content)
            .bind(now)
            .execute(&db.pool)
            .await
            .unwrap();
        }
        let documents =
            "SELECT to_jsonb(d) FROM (SELECT * FROM documents ORDER BY catalog_id, path) d";
        let revisions = "SELECT to_jsonb(r) FROM (SELECT * FROM revisions ORDER BY catalog_id, path, revision) r";
        let documents_before: Vec<serde_json::Value> = sqlx::query_scalar(documents)
            .fetch_all(&db.pool)
            .await
            .unwrap();
        let revisions_before: Vec<serde_json::Value> = sqlx::query_scalar(revisions)
            .fetch_all(&db.pool)
            .await
            .unwrap();
        let credentials = "SELECT to_jsonb(k) FROM (SELECT * FROM api_keys ORDER BY id) k";
        let credential_before: serde_json::Value = sqlx::query_scalar(credentials)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("../migrations/accounts/0006_maintenance.sql"))
            .execute(&db.pool)
            .await
            .unwrap();
        let existing_key_maintenance_report: bool =
            sqlx::query_scalar("SELECT maintenance_report FROM api_keys WHERE id = 'existing'")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert!(!existing_key_maintenance_report);
        let credential_after: serde_json::Value = sqlx::query_scalar(credentials)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        let mut expected_credential = credential_before;
        expected_credential["maintenance_report"] = serde_json::json!(false);
        assert_eq!(credential_after, expected_credential);
        let documents_after: Vec<serde_json::Value> = sqlx::query_scalar(documents)
            .fetch_all(&db.pool)
            .await
            .unwrap();
        let revisions_after: Vec<serde_json::Value> = sqlx::query_scalar(revisions)
            .fetch_all(&db.pool)
            .await
            .unwrap();
        assert_eq!(documents_before, documents_after);
        assert_eq!(revisions_before, revisions_after);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn maintenance_schema_rejects_cross_catalog_and_duplicate_live_claims() {
        let db = test_db_account_layout().await;
        let now = chrono::Utc::now();
        let first: i64 = sqlx::query_scalar(
            "INSERT INTO catalogs (name, created_at) VALUES ('first', $1) RETURNING id",
        )
        .bind(now)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        let second: i64 = sqlx::query_scalar(
            "INSERT INTO catalogs (name, created_at) VALUES ('second', $1) RETURNING id",
        )
        .bind(now)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        for catalog in [first, second] {
            sqlx::query("INSERT INTO catalog.catalog_state (id) VALUES ($1)")
                .bind(catalog)
                .execute(&db.pool)
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO catalog.revisions (catalog_id, path, revision, op, content, executable, actor, message, created_at) \
                 VALUES ($1, 'a.md', 1, 'import', $2, false, 'a', 'm', $3)",
            )
            .bind(catalog)
            .bind(b"a".to_vec())
            .bind(now)
            .execute(&db.pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO catalog.maintenance_runs (id, catalog_id, credential_id, actor, state, deadline, created_at, primary_count) \
                 VALUES ($1, $2, 'holder', 'a', 'running', $3, $3, 1)",
            )
            .bind(format!("run-{catalog}"))
            .bind(catalog)
            .bind(now)
            .execute(&db.pool)
            .await
            .unwrap();
        }
        let insert_work = "INSERT INTO catalog.maintenance_work (id, run_id, catalog_id, path, revision, lanes, lease_token, lease_expires_at, attempted_at) \
                           VALUES ($1, $2, $3, 'a.md', 1, ARRAY['changed'], $4, $5, $5)";
        sqlx::query(insert_work)
            .bind("work-one")
            .bind(format!("run-{first}"))
            .bind(first)
            .bind("token-one")
            .bind(now)
            .execute(&db.pool)
            .await
            .unwrap();
        let duplicate_active_claim_insert = sqlx::query(insert_work)
            .bind("work-two")
            .bind(format!("run-{first}"))
            .bind(first)
            .bind("token-two")
            .bind(now)
            .execute(&db.pool)
            .await;
        assert!(duplicate_active_claim_insert.is_err());
        let cross_catalog_work_insert = sqlx::query(insert_work)
            .bind("bad-work")
            .bind(format!("run-{second}"))
            .bind(first)
            .bind("token-three")
            .bind(now)
            .execute(&db.pool)
            .await;
        assert!(cross_catalog_work_insert.is_err());
        sqlx::query(insert_work)
            .bind("work-other")
            .bind(format!("run-{second}"))
            .bind(second)
            .bind("token-other")
            .bind(now)
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO catalog.maintenance_findings (id, run_id, work_id, catalog_id, category, assertion, follow_up, uncertainty, relationship, external, created_at) \
             VALUES ('finding-one', $1, 'work-one', $2, 'suggested_link', 'a', 'follow', 'unknown', 'direct_statement', '[]', $3)",
        )
        .bind(format!("run-{first}"))
        .bind(first)
        .bind(now)
        .execute(&db.pool)
        .await
        .unwrap();
        let cross_catalog_evidence_insert = sqlx::query(
            "INSERT INTO catalog.maintenance_evidence (catalog_id, work_id, finding_id, ordinal, path, revision, excerpt) \
             VALUES ($1, 'work-one', 'finding-one', 0, 'a.md', 1, 'a')",
        )
        .bind(second)
        .execute(&db.pool)
        .await;
        assert!(cross_catalog_evidence_insert.is_err());
        sqlx::query(
            "INSERT INTO catalog.revisions (catalog_id, path, revision, op, content, executable, actor, message, created_at) \
             VALUES ($1, 'only-second.md', 1, 'import', $2, false, 'a', 'm', $3)",
        )
        .bind(second)
        .bind(b"a".to_vec())
        .bind(now)
        .execute(&db.pool)
        .await
        .unwrap();
        let cross_catalog_pin_insert = sqlx::query(
            "INSERT INTO catalog.maintenance_evidence (catalog_id, work_id, ordinal, path, revision, excerpt) \
             VALUES ($1, 'work-one', 0, 'only-second.md', 1, 'a')",
        )
        .bind(first)
        .execute(&db.pool)
        .await;
        assert!(cross_catalog_pin_insert.is_err());
        let cross_work_finding_insert = sqlx::query(
            "INSERT INTO catalog.maintenance_evidence (catalog_id, work_id, finding_id, ordinal, path, revision, excerpt) \
             VALUES ($1, $2, 'finding-one', 0, 'a.md', 1, 'a')",
        )
        .bind(second)
        .bind("work-other")
        .execute(&db.pool)
        .await;
        assert!(cross_work_finding_insert.is_err());
        let cross_catalog_finding_insert = sqlx::query(
            "INSERT INTO catalog.maintenance_findings (id, run_id, work_id, catalog_id, category, assertion, follow_up, uncertainty, relationship, external, created_at) \
             VALUES ('bad-finding', $1, 'work-one', $2, 'suggested_link', 'a', 'follow', 'unknown', 'direct_statement', '[]', $3)",
        )
        .bind(format!("run-{second}"))
        .bind(second)
        .bind(now)
        .execute(&db.pool)
        .await;
        assert!(cross_catalog_finding_insert.is_err());
        let cross_catalog_disposition_insert = sqlx::query(
            "INSERT INTO catalog.maintenance_dispositions (catalog_id, finding_id, disposition, rationale, credential_id, actor, created_at) \
             VALUES ($1, 'finding-one', 'dismissed', 'reason', 'holder', 'a', $2)",
        )
        .bind(second)
        .bind(now)
        .execute(&db.pool)
        .await;
        assert!(cross_catalog_disposition_insert.is_err());
        sqlx::query("UPDATE catalog.maintenance_work SET outcome = 'completed', ended_at = $1 WHERE id = 'work-one'")
            .bind(now)
            .execute(&db.pool)
            .await
            .unwrap();
        let second_attempt_after_terminal_insert = sqlx::query(insert_work)
            .bind("work-two")
            .bind(format!("run-{first}"))
            .bind(first)
            .bind("token-two")
            .bind(now)
            .execute(&db.pool)
            .await;
        assert!(second_attempt_after_terminal_insert.is_ok());
        sqlx::query(
            "INSERT INTO catalog.maintenance_evidence (catalog_id, work_id, finding_id, ordinal, path, revision, excerpt) \
             VALUES ($1, 'work-one', 'finding-one', 0, 'a.md', 1, 'a')",
        )
        .bind(first)
        .execute(&db.pool)
        .await
        .unwrap();
        let duplicate_ordinal = sqlx::query(
            "INSERT INTO catalog.maintenance_evidence (catalog_id, work_id, finding_id, ordinal, path, revision, excerpt) \
             VALUES ($1, 'work-one', 'finding-one', 0, 'a.md', 1, 'a')",
        )
        .bind(first)
        .execute(&db.pool)
        .await;
        assert!(duplicate_ordinal.is_err());
        sqlx::query(
            "INSERT INTO catalog.maintenance_state (catalog_id, updated_at) VALUES ($1, $2)",
        )
        .bind(first)
        .bind(now)
        .execute(&db.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO catalog.maintenance_requests (catalog_id, credential_id, operation, idempotency_key, request, response) \
             VALUES ($1, 'holder', 'submit', 'request-one', $2, $3)",
        )
        .bind(first)
        .bind(serde_json::json!({"work_id": "work-one"}))
        .bind(serde_json::json!({"outcome": "completed"}))
        .execute(&db.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO catalog.maintenance_dispositions (catalog_id, finding_id, disposition, rationale, credential_id, actor, created_at) \
             VALUES ($1, 'finding-one', 'dismissed', 'reason', 'holder', 'a', $2)",
        )
        .bind(first)
        .bind(now)
        .execute(&db.pool)
        .await
        .unwrap();
        let populated: (i64, i64, i64, i64, i64, i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM catalog.maintenance_runs WHERE catalog_id = $1), \
             (SELECT count(*) FROM catalog.maintenance_work WHERE catalog_id = $1), \
             (SELECT count(*) FROM catalog.maintenance_findings WHERE catalog_id = $1), \
             (SELECT count(*) FROM catalog.maintenance_evidence WHERE catalog_id = $1), \
             (SELECT count(*) FROM catalog.maintenance_dispositions WHERE catalog_id = $1), \
             (SELECT count(*) FROM catalog.maintenance_requests WHERE catalog_id = $1), \
             (SELECT count(*) FROM catalog.maintenance_state WHERE catalog_id = $1)",
        )
        .bind(first)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(populated, (1, 2, 1, 1, 1, 1, 1));
        let revision_delete = sqlx::query(
            "DELETE FROM catalog.revisions WHERE catalog_id = $1 AND path = 'a.md' AND revision = 1",
        )
        .bind(first)
        .execute(&db.pool)
        .await;
        assert!(
            revision_delete.is_err(),
            "referenced revision must be retained"
        );
        let retained: (i64, i64, i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM catalog.revisions WHERE catalog_id = $1 AND path = 'a.md'), \
             (SELECT count(*) FROM catalog.maintenance_work WHERE catalog_id = $1), \
             (SELECT count(*) FROM catalog.maintenance_findings WHERE catalog_id = $1), \
             (SELECT count(*) FROM catalog.maintenance_evidence WHERE catalog_id = $1)",
        )
        .bind(first)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(retained, (1, 2, 1, 1));
        let retained_history: (i64, i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM catalog.maintenance_dispositions WHERE catalog_id = $1), \
             (SELECT count(*) FROM catalog.maintenance_requests WHERE catalog_id = $1), \
             (SELECT count(*) FROM catalog.maintenance_state WHERE catalog_id = $1)",
        )
        .bind(first)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(retained_history, (1, 1, 1));
        // The registry row, then the catalog's own state row (and with it
        // the data): the account side and the store side delete separately.
        sqlx::query("DELETE FROM catalog.catalog_state WHERE id = $1")
            .bind(first)
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM catalogs WHERE id = $1")
            .bind(first)
            .execute(&db.pool)
            .await
            .unwrap();
        let remaining: (i64, i64, i64, i64, i64, i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM catalog.maintenance_runs WHERE catalog_id = $1), \
             (SELECT count(*) FROM catalog.maintenance_work WHERE catalog_id = $1), \
             (SELECT count(*) FROM catalog.maintenance_findings WHERE catalog_id = $1), \
             (SELECT count(*) FROM catalog.maintenance_evidence WHERE catalog_id = $1), \
             (SELECT count(*) FROM catalog.maintenance_dispositions WHERE catalog_id = $1), \
             (SELECT count(*) FROM catalog.maintenance_requests WHERE catalog_id = $1), \
             (SELECT count(*) FROM catalog.maintenance_state WHERE catalog_id = $1)",
        )
        .bind(first)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(remaining, (0, 0, 0, 0, 0, 0, 0));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn constraints_hold() {
        let db = test_db_account_layout().await;
        let catalog: i64 = sqlx::query_scalar(
            "INSERT INTO catalogs (name, created_at) VALUES ('pg', $1) RETURNING id",
        )
        .bind(chrono::Utc::now())
        .fetch_one(&db.pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO catalog.catalog_state (id) VALUES ($1)")
            .bind(catalog)
            .execute(&db.pool)
            .await
            .unwrap();

        let bad_name =
            sqlx::query("INSERT INTO catalogs (name, created_at) VALUES ('Bad Name', $1)")
                .bind(chrono::Utc::now())
                .execute(&db.pool)
                .await
                .unwrap_err();
        assert!(bad_name.as_database_error().unwrap().is_check_violation());

        // A key, for the changesets below and `key_catalogs` further down.
        sqlx::query(
            "INSERT INTO api_keys (id, name, sha256, access, all_catalogs, default_catalog_id, \
             actor, created_at) VALUES ('k1', 'k1', $1, 'read', false, $2, 'a', $3)",
        )
        .bind(vec![0u8; 32])
        .bind(catalog)
        .bind(chrono::Utc::now())
        .execute(&db.pool)
        .await
        .unwrap();
        // One key may hold several open changesets in a catalog (M8
        // Decision 7), but no two changesets share a handle.
        let open = "INSERT INTO catalog.changesets (catalog_id, handle, key_id, actor, state, created_at, \
                    updated_at) VALUES ($1, $2, 'k1', 'a', 'open', $3, $3)";
        for handle in ["h1", "h2"] {
            sqlx::query(open)
                .bind(catalog)
                .bind(handle)
                .bind(chrono::Utc::now())
                .execute(&db.pool)
                .await
                .unwrap();
        }
        let second = sqlx::query(open)
            .bind(catalog)
            .bind("h1")
            .bind(chrono::Utc::now())
            .execute(&db.pool)
            .await
            .unwrap_err();
        let second = second.as_database_error().unwrap();
        assert!(second.is_unique_violation());
        assert_eq!(second.constraint(), Some("changesets_handle_key"));

        let revision = "INSERT INTO catalog.revisions (catalog_id, path, revision, op, content, \
                        executable, actor, message, created_at) \
                        VALUES ($1, 'a.md', 1, 'import', $2, false, 'a', 'm', $3)";
        sqlx::query(revision)
            .bind(catalog)
            .bind(b"x".to_vec())
            .bind(chrono::Utc::now())
            .execute(&db.pool)
            .await
            .unwrap();
        let duplicate = sqlx::query(revision)
            .bind(catalog)
            .bind(b"y".to_vec())
            .bind(chrono::Utc::now())
            .execute(&db.pool)
            .await
            .unwrap_err();
        let duplicate = duplicate.as_database_error().unwrap();
        assert!(duplicate.is_unique_violation());
        assert_eq!(
            duplicate.constraint(),
            Some("revisions_catalog_id_path_revision_key")
        );

        // One row in every other table that refers to the catalog.
        sqlx::query(
            "INSERT INTO catalog.documents (catalog_id, path, content, executable, revision, \
             updated_at, updated_by) VALUES ($1, 'a.md', $2, false, 1, $3, 'a')",
        )
        .bind(catalog)
        .bind(b"x".to_vec())
        .bind(chrono::Utc::now())
        .execute(&db.pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO key_catalogs (key_id, catalog_id) VALUES ('k1', $1)")
            .bind(catalog)
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO catalog.changeset_entries (changeset_id, path, content) \
             SELECT id, 'a.md', $1 FROM catalog.changesets",
        )
        .bind(b"z".to_vec())
        .execute(&db.pool)
        .await
        .unwrap();

        // Deleting the catalog cascades to every row that refers to it, and
        // clears the key's default instead of deleting the key.
        // The registry row, then the catalog's own state row (and with it
        // the data): the account side and the store side delete separately.
        sqlx::query("DELETE FROM catalog.catalog_state WHERE id = $1")
            .bind(catalog)
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM catalogs WHERE id = $1")
            .bind(catalog)
            .execute(&db.pool)
            .await
            .unwrap();
        let left: (i64, i64, i64, i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM catalog.documents), (SELECT count(*) FROM key_catalogs), \
             (SELECT count(*) FROM catalog.changesets), (SELECT count(*) FROM catalog.changeset_entries), \
             (SELECT count(*) FROM catalog.revisions)",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(left, (0, 0, 0, 0, 0));
        // `Option<i64>` decodes a nullable column: `None` is SQL NULL.
        let default_catalog: Option<i64> =
            sqlx::query_scalar("SELECT default_catalog_id FROM api_keys WHERE id = 'k1'")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(default_catalog, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn key_name_migration_preserves_existing_credentials_and_history() {
        let db = pg_support::TestDb::start().await;
        for migration in [
            include_str!("../migrations/accounts/0001_schema.sql"),
            include_str!("../migrations/accounts/0002_changeset_identity.sql"),
            include_str!("../migrations/accounts/0003_changeset_handles.sql"),
            include_str!("../migrations/accounts/0004_drop_changeset_labels.sql"),
        ] {
            sqlx::raw_sql(migration).execute(&db.pool).await.unwrap();
        }
        let now = chrono::Utc::now();
        let catalog: i64 = sqlx::query_scalar(
            "INSERT INTO catalogs (name, created_at) VALUES ('pg', $1) RETURNING id",
        )
        .bind(now)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        let secret = "okf_existing-holder-secret";
        sqlx::query(
            "INSERT INTO api_keys \
             (id, sha256, access, all_catalogs, actor, created_at) \
             VALUES ('legacy', $1, 'write', true, 'legacy-actor', $2)",
        )
        .bind(crate::keys::hash(secret).as_slice())
        .bind(now)
        .execute(&db.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO changesets \
             (catalog_id, handle, key_id, actor, state, created_at, updated_at) \
             VALUES ($1, 'existing-handle', 'legacy', 'legacy-actor', 'open', $2, $2)",
        )
        .bind(catalog)
        .bind(now)
        .execute(&db.pool)
        .await
        .unwrap();

        sqlx::raw_sql(include_str!("../migrations/accounts/0005_key_names.sql"))
            .execute(&db.pool)
            .await
            .unwrap();

        // This fixture stops at 0005; current key readers require later columns.
        let (id, name, sha256): (String, String, Vec<u8>) =
            sqlx::query_as("SELECT id, name, sha256 FROM api_keys")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!((id.as_str(), name.as_str()), ("legacy", "legacy"));
        assert!(crate::keys::matches(&sha256, secret));
        let owner: (Option<String>, String) =
            sqlx::query_as("SELECT key_id, actor FROM changesets WHERE handle = 'existing-handle'")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(
            owner,
            (Some("legacy".to_string()), "legacy-actor".to_string())
        );
    }

    /// The leak behind issue #187: a zero startup timeout expires after
    /// Docker created the container but before testcontainers returned a
    /// handle, so testcontainers alone leaves it behind.
    ///
    /// Whether the container exists when the budget runs out depends on how
    /// fast Docker is: on a loaded machine an earlier stage (pull, create)
    /// can run the zero budget out first, and nothing leaks (issue #500).
    /// So the start is tried again, under a fresh label, until one leaks; an
    /// attempt that failed for any reason but a timeout still fails the test.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_timed_out_start_leaks_without_cleanup_and_remove_labelled_reaps_it() {
        use testcontainers_modules::testcontainers::runners::AsyncRunner;
        use testcontainers_modules::testcontainers::ImageExt;

        const ATTEMPTS: usize = 10;
        let mut leaked = None;
        for _ in 0..ATTEMPTS {
            let label = pg_support::unique_start_label();
            let started = pg_support::postgres_17()
                .with_label(pg_support::START_LABEL, &label)
                .with_startup_timeout(std::time::Duration::ZERO)
                .start()
                .await;

            // `.err()` keeps the error and drops the container, which has
            // no `Debug`.
            let error = started.err();
            assert!(
                error.as_ref().is_some_and(pg_support::is_timeout_class),
                "a zero startup timeout must fail with a timeout: {error:?}"
            );
            let found = pg_support::labelled_containers(&label).await.unwrap();
            if found.is_empty() {
                continue;
            }
            leaked = Some((label, found.len()));
            break;
        }

        let (label, count) =
            leaked.unwrap_or_else(|| panic!("no start of {ATTEMPTS} left a container behind"));
        assert_eq!(count, 1);
        assert_eq!(pg_support::remove_labelled(&label).await.unwrap(), 1);
        assert!(pg_support::labelled_containers(&label)
            .await
            .unwrap()
            .is_empty());
    }

    fn labelled(
        id: &str,
        start_label: &str,
        namespace: Option<&str>,
    ) -> pg_support::LabelledContainer {
        pg_support::LabelledContainer {
            id: id.to_string(),
            start_label: start_label.to_string(),
            owner_scope: namespace.map(str::to_string),
        }
    }

    /// Issue #784: only a dead owner in our own scope (boot and pid namespace)
    /// gets swept. Two machines report the same namespace id, so the same
    /// namespace on another boot or machine is kept.
    #[test]
    fn the_sweep_picks_only_containers_of_dead_owners_in_this_scope() {
        let ours = "boot-a/pid:[1]";
        let containers = [
            labelled("dead", "100-3", Some(ours)),
            labelled("alive", "200-0", Some(ours)),
            labelled("other-namespace", "100-4", Some("boot-a/pid:[2]")),
            labelled("other-machine", "100-6", Some("boot-b/pid:[1]")),
            labelled("no-scope", "100-5", None),
            labelled("shared-server", "server-okf-mcp-test-pg-100", Some(ours)),
            labelled("malformed", "100", Some(ours)),
            labelled("bad-counter", "100-x", Some(ours)),
            labelled("dead-too", "300-12", Some(ours)),
        ];

        let stale = pg_support::stale_container_ids(&containers, ours, |pid| pid == 200);

        assert_eq!(stale, ["dead", "dead-too"]);
    }

    /// The scope names the boot as well as the namespace.
    #[test]
    fn the_owner_scope_names_the_boot_and_the_namespace() {
        let Some(scope) = pg_support::owner_scope() else {
            return;
        };
        let (boot, namespace) = scope.split_once('/').unwrap();
        assert!(!boot.is_empty() && namespace.starts_with("pid:["));
    }

    /// Issue #784, review of PR #1043: the whole path, with Docker: labelled
    /// containers are listed and exactly the dead owner's is removed. The
    /// sweep gets a scope of its own and a stub liveness check, so it can
    /// match nothing but the containers this test creates (other sessions'
    /// real containers carry the real scope).
    #[tokio::test(flavor = "multi_thread")]
    async fn the_sweep_removes_only_the_dead_owners_container() {
        use testcontainers_modules::testcontainers::runners::AsyncRunner;
        use testcontainers_modules::testcontainers::ImageExt;

        let scope = format!("test-scope-{}", pg_support::unique_start_label());
        let dead_pid = 4_000_001_u32;
        let live_pid = 4_000_002_u32;
        let request = |start: String, owner_scope: Option<&str>| {
            let request = pg_support::postgres_17().with_label(pg_support::START_LABEL, start);
            match owner_scope {
                Some(scope) => request.with_label(pg_support::OWNER_SCOPE_LABEL, scope),
                None => request,
            }
        };
        let mut started = Vec::new();
        for (name, start, owner_scope) in [
            ("dead", format!("{dead_pid}-0"), Some(scope.as_str())),
            ("live", format!("{live_pid}-0"), Some(scope.as_str())),
            ("no-scope", format!("{dead_pid}-1"), None),
            (
                "shared-server",
                format!("server-test-{live_pid}"),
                Some(scope.as_str()),
            ),
            (
                "other-machine",
                format!("{dead_pid}-2"),
                Some("another-boot/pid:[1]"),
            ),
        ] {
            let container = request(start, owner_scope)
                .with_startup_timeout(pg_support::STARTUP_TIMEOUT)
                .start()
                .await
                .unwrap_or_else(|error| panic!("start the {name} container: {error}"));
            started.push((name, container));
        }

        let removed = pg_support::sweep_stale_containers_of(&scope, |pid| pid == live_pid).await;

        let remaining = pg_support::all_labelled_containers().await;
        // Remove every container this test created before any assertion.
        for (_, container) in &started {
            let _ = pg_support::remove_container_by_id(container.id()).await;
        }
        assert_eq!(removed.unwrap(), 1);
        let remaining = remaining.unwrap();
        for (name, container) in &started {
            let present = remaining.iter().any(|found| found.id == container.id());
            assert_eq!(present, *name != "dead", "{name} container");
        }
    }

    /// With every owner alive nothing is picked, and an empty list is fine.
    #[test]
    fn the_sweep_keeps_everything_when_all_owners_are_alive() {
        let containers = [
            labelled("a", "1-0", Some("ns")),
            labelled("b", "2-0", Some("ns")),
        ];

        assert!(pg_support::stale_container_ids(&containers, "ns", |_| true).is_empty());
        assert!(pg_support::stale_container_ids(&[], "ns", |_| false).is_empty());
    }

    #[test]
    fn owner_pid_reads_only_pid_and_counter_labels() {
        assert_eq!(pg_support::owner_pid("4242-17"), Some(4242));
        assert_eq!(pg_support::owner_pid("server-x"), None);
        assert_eq!(pg_support::owner_pid("4242"), None);
        assert_eq!(pg_support::owner_pid("-1"), None);
    }

    /// The liveness probe is `kill(pid, 0)`, which `hidepid` does not affect:
    /// this process and pid 1 (visible or not, a live process of another user
    /// answers `EPERM`) are alive, and values that are no process id are
    /// uncertain, which counts as alive.
    #[test]
    fn the_liveness_probe_calls_live_and_unknown_pids_alive() {
        assert!(pg_support::process_may_be_alive(std::process::id()));
        assert!(pg_support::process_may_be_alive(1));
        assert!(pg_support::process_may_be_alive(0));
        assert!(pg_support::process_may_be_alive(u32::MAX));
    }

    /// Only `ESRCH` permits a removal. `EPERM` is a live process of another
    /// user, which a restricted `/proc` would hide: the case where pid 1 is
    /// visible and the owner is not.
    #[test]
    fn only_esrch_means_dead() {
        assert!(!pg_support::signal_error_means_alive(Some(libc::ESRCH)));
        assert!(pg_support::signal_error_means_alive(Some(libc::EPERM)));
        assert!(pg_support::signal_error_means_alive(Some(libc::EINVAL)));
        assert!(pg_support::signal_error_means_alive(None));
    }

    /// With pid 1 visible but the live owner (pid 500, another user's) not
    /// listed, a `/proc` look would call the owner dead. The probe answers
    /// `EPERM` for it, and the sweep keeps its container.
    #[test]
    fn a_live_owner_hidden_from_proc_is_kept() {
        let ours = "boot-a/pid:[1]";
        let containers = [labelled("hidden-owner", "500-0", Some(ours))];
        let visible_in_proc = |pid: u32| pid == 1;
        let signal_answer = |pid: u32| {
            // pid 500 answers EPERM, anything else ESRCH.
            let errno = if pid == 500 { libc::EPERM } else { libc::ESRCH };
            pg_support::signal_error_means_alive(Some(errno))
        };

        assert_eq!(
            pg_support::stale_container_ids(&containers, ours, visible_in_proc),
            ["hidden-owner"],
            "the /proc look this replaced would have removed it"
        );
        assert!(pg_support::stale_container_ids(&containers, ours, signal_answer).is_empty());
    }

    #[test]
    fn a_restricted_or_missing_proc_mount_turns_the_sweep_off() {
        let line = |options: &str, super_options: &str| {
            format!("30 22 0:25 / /proc {options} shared:5 - proc proc {super_options}\n")
        };
        assert!(!pg_support::proc_mount_is_restricted(&line(
            "rw,nosuid",
            "rw"
        )));
        assert!(!pg_support::proc_mount_is_restricted(&line(
            "rw",
            "rw,hidepid=off"
        )));
        assert!(!pg_support::proc_mount_is_restricted(&line(
            "rw",
            "rw,hidepid=0"
        )));
        assert!(pg_support::proc_mount_is_restricted(&line(
            "rw",
            "rw,hidepid=2"
        )));
        assert!(pg_support::proc_mount_is_restricted(&line(
            "rw,hidepid=1",
            "rw"
        )));
        assert!(pg_support::proc_mount_is_restricted(&line(
            "rw",
            "rw,subset=pid"
        )));
        assert!(pg_support::proc_mount_is_restricted(
            "36 35 98:0 / /mnt rw - ext4 /dev/root rw\n"
        ));
        assert!(pg_support::proc_mount_is_restricted(""));
    }

    /// A zero timeout can end the start in any stage; whichever it was, the
    /// labelled start leaves no container (issue #500: it asserted the wait
    /// stage's `StartupTimeout` alone, which a loaded Docker does not reach).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_timed_out_start_labelled_leaves_no_container() {
        use testcontainers_modules::testcontainers::ImageExt;

        let label = pg_support::unique_start_label();
        let request = pg_support::postgres_17().with_startup_timeout(std::time::Duration::ZERO);

        let started = pg_support::start_labelled(request, &label).await;

        // `.err()` keeps the error and drops the container, which has no `Debug`.
        let error = started.err();
        assert!(
            error.as_ref().is_some_and(pg_support::is_timeout_class),
            "the start did not fail with a timeout-class error: {error:?}"
        );
        assert!(pg_support::labelled_containers(&label)
            .await
            .unwrap()
            .is_empty());
    }

    /// Review of PR #739: a concurrent index build that failed part-way
    /// leaves an INVALID index and no record of its migration; the next
    /// `migrate` drops it and builds it again, valid.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_concurrent_build_is_rebuilt_by_the_next_migrate() {
        let db = super::test_db().await;
        let valid = |name: &'static str| {
            sqlx::query_scalar::<_, bool>(
                "SELECT i.indisvalid FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid \
                 WHERE c.relname = $1",
            )
            .bind(name)
        };
        // As a failed `0025` leaves it: the index there but invalid (the
        // test database's user is a superuser), the migration unrecorded.
        sqlx::query(
            "UPDATE pg_index SET indisvalid = false \
             WHERE indexrelid = 'catalogs_member_reach'::regclass",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        sqlx::query("DELETE FROM _sqlx_migrations WHERE version = 25")
            .execute(&db.pool)
            .await
            .unwrap();
        assert!(!valid("catalogs_member_reach")
            .fetch_one(&db.pool)
            .await
            .unwrap());

        super::migrate(&db.pool).await.unwrap();

        assert!(valid("catalogs_member_reach")
            .fetch_one(&db.pool)
            .await
            .unwrap());
        let recorded: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM _sqlx_migrations WHERE version = 25)")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert!(recorded, "the rebuilt migration is recorded");
    }

    /// Issue #986: a failed concurrent build of the catalog's user index
    /// leaves it INVALID; the next `migrate` drops it and builds it again.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_build_of_the_catalog_user_index_is_rebuilt_by_the_next_migrate() {
        let db = test_db_account_layout().await;
        let valid = || {
            sqlx::query_scalar::<_, bool>(
                "SELECT i.indisvalid FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid \
                 WHERE c.relname = 'revisions_catalog_user_recent'",
            )
        };
        sqlx::query(
            "UPDATE pg_index SET indisvalid = false \
             WHERE indexrelid = 'catalog.revisions_catalog_user_recent'::regclass",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        sqlx::query("DELETE FROM catalog._sqlx_migrations WHERE version = 5")
            .execute(&db.pool)
            .await
            .unwrap();
        assert!(!valid().fetch_one(&db.pool).await.unwrap());

        super::migrate(&db.pool).await.unwrap();

        assert!(valid().fetch_one(&db.pool).await.unwrap());
        let recorded: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM catalog._sqlx_migrations WHERE version = 5)",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert!(recorded, "the rebuilt migration is recorded");
    }

    /// Issue #986, catalog migration 0004: revision -> changeset -> key ->
    /// user, where each link resolves; everything else stays null, and a
    /// revision that already has a person keeps it.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_user_backfill_follows_revision_changeset_key_to_user_where_it_resolves() {
        let db = test_db_account_layout().await;
        let owner: String = sqlx::query_scalar("SELECT id FROM users WHERE name = $1")
            .bind(TEST_USER)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        let catalog = crate::store::catalogs::create(
            &db.pool,
            &crate::store::catalogs::NewCatalog {
                name: "pg",
                display_name: &crate::labels::DisplayName::default(),
                description: "",
                owner_team: TEST_TEAM,
            },
            &crate::store::audit::AuditActor::cli(),
            chrono::Utc::now(),
        )
        .await
        .unwrap();
        for key in ["k-owned", "k-legacy"] {
            crate::store::keys::insert_legacy_test_key(
                &db.pool,
                key,
                crate::store::keys::Access::Write,
                key,
                chrono::Utc::now(),
            )
            .await;
        }
        sqlx::query("UPDATE api_keys SET user_id = $1 WHERE id = 'k-owned'")
            .bind(&owner)
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM catalog.revisions WHERE catalog_id = $1")
            .bind(catalog.id)
            .execute(&db.pool)
            .await
            .unwrap();
        // One changeset per key, and one whose key is gone.
        let mut changesets = Vec::new();
        for key in ["k-owned", "k-legacy", "k-deleted"] {
            let id: i64 = sqlx::query_scalar(
                "INSERT INTO catalog.changesets (catalog_id, key_id, actor, state, created_at, \
                 updated_at) VALUES ($1, $2, 'a', 'published', now(), now()) RETURNING id",
            )
            .bind(catalog.id)
            .bind(key)
            .fetch_one(&db.pool)
            .await
            .unwrap();
            changesets.push(id);
        }
        // path, changeset, a person already recorded
        let rows: [(&str, Option<i64>, Option<&str>); 5] = [
            ("owned.md", Some(changesets[0]), None),
            ("legacy.md", Some(changesets[1]), None),
            ("deleted.md", Some(changesets[2]), None),
            ("import.md", None, None),
            ("kept.md", Some(changesets[0]), Some("someone-else")),
        ];
        for (path, changeset, person) in rows {
            sqlx::query(
                "INSERT INTO catalog.revisions (catalog_id, path, revision, op, executable, \
                 actor, changeset_id, message, created_at, user_id) \
                 VALUES ($1, $2, 1, 'create', false, 'a', $3, 'm', now(), $4)",
            )
            .bind(catalog.id)
            .bind(path)
            .bind(changeset)
            .bind(person)
            .execute(&db.pool)
            .await
            .unwrap();
        }
        let users = || async {
            sqlx::query_as::<_, (String, Option<String>)>(
                "SELECT path, user_id FROM catalog.revisions WHERE catalog_id = $1 ORDER BY path",
            )
            .bind(catalog.id)
            .fetch_all(&db.pool)
            .await
            .unwrap()
        };

        sqlx::raw_sql(include_str!(
            "../migrations/catalog/0004_revisions_user_backfill.sql"
        ))
        .execute(&db.pool)
        .await
        .unwrap();
        let once = users().await;
        sqlx::raw_sql(include_str!(
            "../migrations/catalog/0004_revisions_user_backfill.sql"
        ))
        .execute(&db.pool)
        .await
        .unwrap();

        assert_eq!(
            once,
            [
                ("deleted.md".to_string(), None),
                ("import.md".to_string(), None),
                ("kept.md".to_string(), Some("someone-else".to_string())),
                ("legacy.md".to_string(), None),
                ("owned.md".to_string(), Some(owner)),
            ]
        );
        assert_eq!(users().await, once, "a rerun changes nothing");
    }

    /// Review of PR #739: with a pool of one connection (the smallest
    /// `database_max_connections`), `migrate` finishes on a fresh database
    /// and on one already migrated, instead of waiting for a second
    /// connection the pool cannot give.
    #[tokio::test(flavor = "multi_thread")]
    async fn migrate_needs_only_one_connection() {
        let db = pg_support::TestDb::start().await;
        let one = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect(&db.url)
            .await
            .unwrap();
        for state in ["fresh", "already migrated"] {
            tokio::time::timeout(std::time::Duration::from_secs(120), super::migrate(&one))
                .await
                .unwrap_or_else(|_| panic!("migrating a {state} database timed out"))
                .unwrap_or_else(|error| panic!("migrating a {state} database: {error}"));
        }
        let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations")
            .fetch_one(&one)
            .await
            .unwrap();
        assert_eq!(
            applied,
            i64::try_from(super::MIGRATOR.iter().count()).unwrap()
        );
    }

    /// Every object of schema `catalog` that a schema-only dump would show,
    /// one line each, sorted: tables and columns (in column order, with
    /// type, nullability, default and identity), constraints, indexes,
    /// sequences, triggers and functions, each as Postgres prints its
    /// definition. Two databases with equal lists have the same `catalog`
    /// schema whatever path built it.
    async fn catalog_schema(pool: &PgPool) -> Vec<String> {
        let queries = [
            // Columns by position among the live ones: a column dropped in
            // some history leaves a gap in `attnum` that a dump never shows.
            "SELECT format('column %s %s %s %s %s %s', c.relname, \
                    row_number() OVER (PARTITION BY c.oid ORDER BY a.attnum), a.attname, \
                    format_type(a.atttypid, a.atttypmod), a.attnotnull, \
                    coalesce(pg_get_expr(d.adbin, d.adrelid), '-') || ' ' || a.attidentity::text) \
             FROM pg_class c JOIN pg_attribute a ON a.attrelid = c.oid \
             LEFT JOIN pg_attrdef d ON d.adrelid = c.oid AND d.adnum = a.attnum \
             WHERE c.relnamespace = 'catalog'::regnamespace AND c.relkind = 'r' \
               AND a.attnum > 0 AND NOT a.attisdropped",
            "SELECT format('constraint %s %s %s', c.relname, k.conname, pg_get_constraintdef(k.oid)) \
             FROM pg_constraint k JOIN pg_class c ON c.oid = k.conrelid \
             WHERE k.connamespace = 'catalog'::regnamespace",
            "SELECT format('index %s', indexdef) FROM pg_indexes WHERE schemaname = 'catalog'",
            "SELECT format('sequence %s %s %s %s %s', sequencename, data_type, start_value, \
                    increment_by, cache_size) \
             FROM pg_sequences WHERE schemaname = 'catalog'",
            "SELECT format('trigger %s', pg_get_triggerdef(t.oid)) \
             FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid \
             WHERE c.relnamespace = 'catalog'::regnamespace AND NOT t.tgisinternal",
            "SELECT format('function %s', pg_get_functiondef(p.oid)) \
             FROM pg_proc p WHERE p.pronamespace = 'catalog'::regnamespace",
            "SELECT format('relation %s %s %s', relname, relkind, relpersistence) \
             FROM pg_class WHERE relnamespace = 'catalog'::regnamespace",
        ];
        let mut lines = Vec::new();
        for query in queries {
            let rows: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(query))
                .fetch_all(pool)
                .await
                .unwrap();
            lines.extend(rows);
        }
        lines.sort();
        lines
    }

    /// Puts an adopted test database back to the state migrations 0041 and
    /// 0042 leave: the catalog schema without its migrator's history or
    /// the identity table (catalog migration 0002).
    async fn undo_adoption(pool: &PgPool) {
        sqlx::query("DROP TABLE catalog._sqlx_migrations, catalog.store_identity")
            .execute(pool)
            .await
            .unwrap();
        // And without what the later catalog migrations add to `revisions`
        // (0003 and 0005), as an account database that has not run them.
        sqlx::raw_sql(
            "DROP INDEX catalog.revisions_catalog_user_recent; \
             ALTER TABLE catalog.revisions DROP COLUMN user_id",
        )
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn adopted_schema_equals_a_fresh_store_schema() {
        // The account database as 0041 and 0042 leave it, then adopted by
        // `migrate` (what `okf-mcp migrate` runs).
        let adopted = test_db_account_layout().await;
        undo_adoption(&adopted.pool).await;
        migrate(&adopted.pool).await.unwrap();
        // An empty database migrated as a new store.
        let fresh = pg_support::TestDb::start().await;
        let mut connection = fresh.pool.acquire().await.unwrap();
        let shared = StoreName::parse("shared-1").unwrap();
        let migration = migrate_store(&mut connection, &shared).await.unwrap();
        drop(connection);

        let adopted_schema = catalog_schema(&adopted.pool).await;
        let fresh_schema = catalog_schema(&fresh.pool).await;

        assert_eq!(
            migration,
            StoreMigration {
                adopted: false,
                applied: CATALOG_MIGRATOR.iter().count()
            }
        );
        // Not trivially equal: both hold every catalog table.
        assert!(adopted_schema
            .iter()
            .any(|line| line.starts_with("relation catalog_audit_entries ")));
        assert_eq!(adopted_schema, fresh_schema);
        // Each database answers to its own store.
        let mut connection = adopted.pool.acquire().await.unwrap();
        assert_eq!(
            store_owner(&mut connection).await.unwrap().as_deref(),
            Some("main")
        );
        // The adopted baseline was recorded with the embedded checksum, not
        // run (sqlx writes -1 for a migration it only records).
        let (checksum, took): (Vec<u8>, i64) = sqlx::query_as(
            "SELECT checksum, execution_time FROM catalog._sqlx_migrations WHERE version = $1",
        )
        .bind(BASELINE_VERSION)
        .fetch_one(&mut *connection)
        .await
        .unwrap();
        let baseline = CATALOG_MIGRATOR
            .iter()
            .find(|migration| migration.version == BASELINE_VERSION)
            .unwrap();
        assert_eq!(checksum, *baseline.checksum);
        assert_eq!(took, -1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn adoption_runs_once_under_concurrent_starts() {
        let db = test_db_account_layout().await;
        undo_adoption(&db.pool).await;
        const STARTS: usize = 4;

        // Each start on a connection of its own, all at once: the advisory
        // lock serialises them, and only the first finds nothing recorded.
        let starts = (0..STARTS).map(|_| {
            let pool = db.pool.clone();
            tokio::spawn(async move {
                let mut connection = pool.acquire().await.unwrap();
                migrate_store(&mut connection, &StoreName::main()).await
            })
        });
        let mut outcomes = Vec::with_capacity(STARTS);
        for start in starts.collect::<Vec<_>>() {
            outcomes.push(start.await.unwrap().unwrap());
        }
        // A server start after them is a no-op too.
        migrate(&db.pool).await.unwrap();

        assert_eq!(outcomes.iter().filter(|outcome| outcome.adopted).count(), 1);
        let versions: Vec<i64> =
            sqlx::query_scalar("SELECT version FROM catalog._sqlx_migrations ORDER BY version")
                .fetch_all(&db.pool)
                .await
                .unwrap();
        let embedded: Vec<i64> = CATALOG_MIGRATOR
            .iter()
            .map(|migration| migration.version)
            .collect();
        assert_eq!(versions, embedded);
        let owners: Vec<String> = sqlx::query_scalar("SELECT name FROM catalog.store_identity")
            .fetch_all(&db.pool)
            .await
            .unwrap();
        assert_eq!(owners, ["main"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_store_opened_under_another_name_is_refused_and_keeps_its_identity() {
        let db = pg_support::TestDb::start().await;
        let mut connection = db.pool.acquire().await.unwrap();
        let first = StoreName::parse("shared-1").unwrap();
        let second = StoreName::parse("shared-2").unwrap();
        migrate_store(&mut connection, &first).await.unwrap();

        let refused = migrate_store(&mut connection, &second).await.unwrap_err();

        assert!(
            matches!(&refused, OkfError::Refused { reason }
                if reason.contains("belongs to store shared-1")),
            "{refused:?}"
        );
        assert_eq!(
            store_owner(&mut connection).await.unwrap().as_deref(),
            Some("shared-1")
        );
        // Its own name still opens it (a no-op).
        assert_eq!(
            migrate_store(&mut connection, &first).await.unwrap(),
            StoreMigration::default()
        );
    }

    /// Inserts one `stores` row with the given location columns; the
    /// credential columns are `credential` as (kind, reference).
    async fn insert_store(
        pool: &PgPool,
        name: &str,
        location: (&str, Option<(&str, i32, &str)>),
        credential: Option<(&str, &str)>,
    ) -> Result<(), sqlx::Error> {
        let (kind, address) = location;
        sqlx::query(
            "INSERT INTO stores (name, location_kind, host, port, database, \
                 credential_kind, credential_ref, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, now())",
        )
        .bind(name)
        .bind(kind)
        .bind(address.map(|(host, _, _)| host))
        .bind(address.map(|(_, port, _)| port))
        .bind(address.map(|(_, _, database)| database))
        .bind(credential.map(|(kind, _)| kind))
        .bind(credential.map(|(_, reference)| reference))
        .execute(pool)
        .await
        .map(|_| ())
    }

    /// One `host` store row at `db.example.internal`.
    async fn insert_host_store(
        pool: &PgPool,
        name: &str,
        database: &str,
        credential: Option<(&str, &str)>,
    ) -> Result<(), sqlx::Error> {
        insert_store(
            pool,
            name,
            ("host", Some(("db.example.internal", 5432, database))),
            credential,
        )
        .await
    }

    /// Review of the folder move (issue #965): a database migrated by the
    /// old `migrations/` folder sees no change after the files moved to
    /// `migrations/accounts/`. The fixture is each file's version,
    /// description and SHA-384 checksum as taken before the move (the three
    /// things sqlx records and compares), so the embedded history must match
    /// it, and so must a database's own record once the new migrations ran.
    #[tokio::test(flavor = "multi_thread")]
    async fn moving_the_migrations_folder_keeps_their_history() {
        use sha2::{Digest, Sha384};
        // Arrange: the history as it stood at 0035, from the fixture.
        let expected: Vec<(i64, String, String)> =
            include_str!("../tests/fixtures/migration_history_0035.txt")
                .lines()
                .map(|line| {
                    let mut parts = line.split(' ');
                    let version = parts.next().unwrap().parse().unwrap();
                    let checksum = parts.next_back().unwrap().to_string();
                    let description = parts.collect::<Vec<_>>().join(" ");
                    (version, description.replace('_', " "), checksum)
                })
                .collect();
        assert_eq!(expected.len(), 35);

        // Act: a database migrated up to 0035, then by the server's migrate.
        let db = pg_support::TestDb::start().await;
        MIGRATOR.run_to(35, &db.pool).await.unwrap();
        let recorded = |pool: PgPool| async move {
            let rows: Vec<(i64, String, Vec<u8>)> = sqlx::query_as(
                "SELECT version, description, checksum FROM _sqlx_migrations \
                 WHERE version <= 35 ORDER BY version",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            rows.into_iter()
                .map(|(version, description, checksum)| {
                    let hex: String = checksum.iter().map(|b| format!("{b:02x}")).collect();
                    (version, description, hex)
                })
                .collect::<Vec<_>>()
        };
        let before = recorded(db.pool.clone()).await;
        migrate(&db.pool)
            .await
            .expect("no pending or unknown migration");
        let after = recorded(db.pool.clone()).await;

        // Assert: every record matches the pre-move fixture, before and after.
        assert_eq!(before, expected);
        assert_eq!(after, expected);
        // And the checksum is what the file holds: SHA-384 of its text.
        for migration in MIGRATOR.iter().filter(|m| m.version <= 35) {
            assert_eq!(
                migration.checksum.as_ref(),
                Sha384::digest(migration.sql.as_str().as_bytes()).as_slice(),
                "{}",
                migration.description
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn migration_0036_puts_every_catalog_on_main_provisioned() {
        // Arrange: a database at 0035 with catalogs, one team and one admin
        // setting already in place.
        let db = pg_support::TestDb::start().await;
        MIGRATOR.run_to(35, &db.pool).await.unwrap();
        sqlx::raw_sql(
            "INSERT INTO catalogs (name, created_at) VALUES \
                 ('old', '2026-01-02T03:04:05Z'), ('newer', '2026-06-07T08:09:10Z');",
        )
        .execute(&db.pool)
        .await
        .unwrap();

        // Act: 0036 creates the stores, 0037 places the catalogs (the two
        // migrations of the registry change that existing rows see).
        MIGRATOR.run_to(37, &db.pool).await.unwrap();

        // Assert: each catalog is on main and provisioned. The column's
        // default gives existing rows the time the migration ran, with no
        // row rewritten.
        let rows: Vec<(String, String, bool)> = sqlx::query_as(
            "SELECT name, store, provisioned_at IS NOT NULL FROM catalogs ORDER BY name",
        )
        .fetch_all(&db.pool)
        .await
        .unwrap();
        assert_eq!(
            rows,
            vec![
                ("newer".to_string(), "main".to_string(), true),
                ("old".to_string(), "main".to_string(), true),
            ]
        );
        // main is the one account-database store, and the default.
        let stores: Vec<(String, String)> =
            sqlx::query_as("SELECT name, location_kind FROM stores")
                .fetch_all(&db.pool)
                .await
                .unwrap();
        assert_eq!(
            stores,
            vec![("main".to_string(), "account-database".to_string())]
        );
        let default: serde_json::Value =
            sqlx::query_scalar("SELECT value FROM server_settings WHERE key = 'default_store'")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(default, serde_json::json!("main"));
        // No team is pinned.
        let pinned: i64 = sqlx::query_scalar("SELECT count(*) FROM teams WHERE store IS NOT NULL")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(pinned, 0);
    }

    /// No foreign key joins schema `catalog` to `public` in either direction
    /// (plan Task 11): the two halves can move to separate databases. It
    /// reads `pg_constraint`, so a constraint the migration missed fails it.
    #[tokio::test(flavor = "multi_thread")]
    async fn no_foreign_key_crosses_between_schemas() {
        let db = pg_support::TestDb::start_account_layout().await;
        let foreign_keys = |crossing: bool| {
            let pool = db.pool.clone();
            async move {
                let sql = format!(
                    "SELECT c.conname::text, src.nspname::text, dst.nspname::text \
                     FROM pg_constraint c \
                     JOIN pg_class sc ON sc.oid = c.conrelid \
                     JOIN pg_namespace src ON src.oid = sc.relnamespace \
                     JOIN pg_class dc ON dc.oid = c.confrelid \
                     JOIN pg_namespace dst ON dst.oid = dc.relnamespace \
                     WHERE c.contype = 'f' AND src.nspname IN ('public', 'catalog') \
                       AND dst.nspname IN ('public', 'catalog') AND (src.nspname {} dst.nspname) \
                     ORDER BY 1",
                    if crossing { "<>" } else { "=" }
                );
                // `AssertSqlSafe`: the text is assembled from constants above.
                sqlx::query_as::<_, (String, String, String)>(sqlx::AssertSqlSafe(sql))
                    .fetch_all(&pool)
                    .await
                    .unwrap()
            }
        };

        let crossing = foreign_keys(true).await;
        let within = foreign_keys(false).await;

        assert_eq!(crossing, [], "a foreign key joins the two schemas");
        // The query sees the catalog schema's own keys, so an empty answer
        // above is not an empty database.
        assert!(
            within.iter().any(
                |(name, schema, _)| name == "changesets_catalog_id_fkey" && schema == "catalog"
            ),
            "{within:?}"
        );
    }

    /// Migration 0041 on a populated database (it runs against production
    /// data, not an empty one): every row of the moved tables survives, the
    /// state rows are seeded as the plan's rules say, and a key id left on
    /// a changeset after its key is deleted stays (no foreign key to null it).
    #[tokio::test(flavor = "multi_thread")]
    async fn migration_0041_keeps_every_row_and_seeds_the_state_rows() {
        // Arrange: a database at 0040 with a published catalog, a half-made
        // one that has starter files, a half-made one without, and an empty
        // provisioned one; the first has a document, a revision, a changeset
        // and a run.
        let db = pg_support::TestDb::start().await;
        MIGRATOR.run_to(40, &db.pool).await.unwrap();
        let now = chrono::Utc::now();
        let mut ids = std::collections::HashMap::new();
        for (name, generation, provisioned) in [
            ("whole", 7_i64, true),
            ("half", 0, false),
            ("lost", 0, false),
            ("empty", 3, true),
        ] {
            let id: i64 = sqlx::query_scalar(
                "INSERT INTO catalogs (name, created_at, generation, provisioned_at) \
                 VALUES ($1, $2, $3, CASE WHEN $4 THEN $2 END) RETURNING id",
            )
            .bind(name)
            .bind(now)
            .bind(generation)
            .bind(provisioned)
            .fetch_one(&db.pool)
            .await
            .unwrap();
            ids.insert(name, id);
        }
        for name in ["whole", "half"] {
            sqlx::query(
                "INSERT INTO documents (catalog_id, path, content, executable, revision, \
                 updated_at, updated_by) VALUES ($1, 'a.md', 'a', false, 1, $2, 'author')",
            )
            .bind(ids[name])
            .bind(now)
            .execute(&db.pool)
            .await
            .unwrap();
        }
        sqlx::query(
            "INSERT INTO revisions (catalog_id, path, revision, op, content, executable, \
             actor, message, created_at) VALUES ($1, 'a.md', 1, 'import', 'a', false, 'a', 'm', $2)",
        )
        .bind(ids["whole"])
        .bind(now)
        .execute(&db.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO api_keys (id, name, sha256, access, all_catalogs, actor, created_at) \
             VALUES ('k1', 'k1', $1, 'read', true, 'a', $2)",
        )
        .bind(vec![0u8; 32])
        .bind(now)
        .execute(&db.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO changesets (catalog_id, handle, key_id, actor, state, created_at, \
             updated_at) VALUES ($1, 'h1', 'k1', 'a', 'open', $2, $2)",
        )
        .bind(ids["whole"])
        .bind(now)
        .execute(&db.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO maintenance_runs (id, catalog_id, key_id, credential_id, actor, state, \
             deadline, created_at, primary_count) \
             VALUES ('run', $1, 'k1', 'k1', 'a', 'running', $2, $2, 1)",
        )
        .bind(ids["whole"])
        .bind(now)
        .execute(&db.pool)
        .await
        .unwrap();

        // Act
        MIGRATOR.run_to(41, &db.pool).await.unwrap();

        // Assert: the state rows are every provisioned catalog and every
        // half-made one with documents, at their generation; `lost` has none.
        let states: Vec<(String, i64)> = sqlx::query_as(
            "SELECT c.name, s.generation FROM catalog.catalog_state s \
             JOIN catalogs c ON c.id = s.id ORDER BY c.name",
        )
        .fetch_all(&db.pool)
        .await
        .unwrap();
        assert_eq!(
            states,
            [
                ("empty".to_string(), 3),
                ("half".to_string(), 0),
                ("whole".to_string(), 7),
            ]
        );
        // Every row moved: nothing is left in `public`, and the counts match.
        let left_behind: Option<String> =
            sqlx::query_scalar("SELECT to_regclass('public.documents')::text")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(left_behind, None);
        for (table, expected) in [
            ("documents", 2_i64),
            ("revisions", 1),
            ("changesets", 1),
            ("maintenance_runs", 1),
        ] {
            let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT count(*) FROM catalog.{table}"
            )))
            .fetch_one(&db.pool)
            .await
            .unwrap();
            assert_eq!(count, expected, "{table}");
        }
        // The registry lost its generation and gained `deleted_at`.
        let columns: Vec<String> = sqlx::query_scalar(
            "SELECT column_name::text FROM information_schema.columns \
             WHERE table_schema = 'public' AND table_name = 'catalogs' \
               AND column_name IN ('generation', 'deleted_at') ORDER BY 1",
        )
        .fetch_all(&db.pool)
        .await
        .unwrap();
        assert_eq!(columns, ["deleted_at"]);
        // The key's deletion no longer nulls the ids kept beside it.
        sqlx::query("DELETE FROM api_keys WHERE id = 'k1'")
            .execute(&db.pool)
            .await
            .unwrap();
        let kept: (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT (SELECT key_id FROM catalog.changesets), \
                    (SELECT key_id FROM catalog.maintenance_runs)",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(kept, (Some("k1".to_string()), Some("k1".to_string())));
        // Deleting a state row removes the catalog's data with it.
        sqlx::query("DELETE FROM catalog.catalog_state WHERE id = $1")
            .bind(ids["whole"])
            .execute(&db.pool)
            .await
            .unwrap();
        let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM catalog.changesets")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(remaining, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_release_one_create_after_0036_lands_on_main_provisioned() {
        // Arrange: a fully migrated database.
        let db = pg_support::TestDb::start_migrated().await;

        // Act: the insert a release-1 replica runs, naming only the
        // columns that existed before 0036.
        sqlx::query(
            "INSERT INTO catalogs (name, description, created_at) \
             VALUES ('legacy', '', now())",
        )
        .execute(&db.pool)
        .await
        .unwrap();

        // Assert
        let (store, provisioned): (String, bool) = sqlx::query_as(
            "SELECT store, provisioned_at IS NOT NULL FROM catalogs WHERE name = 'legacy'",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(store, "main");
        assert!(provisioned);
        // New code may leave a catalog unprovisioned, and the index finds it.
        sqlx::query(
            "INSERT INTO catalogs (name, created_at, store, provisioned_at) \
             VALUES ('half', now(), 'main', NULL)",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        let waiting: Vec<String> =
            sqlx::query_scalar("SELECT name FROM catalogs WHERE provisioned_at IS NULL")
                .fetch_all(&db.pool)
                .await
                .unwrap();
        assert_eq!(waiting, vec!["half".to_string()]);
        // A catalog cannot name a store that does not exist.
        let missing = sqlx::query(
            "INSERT INTO catalogs (name, created_at, store) VALUES ('lost', now(), 'nowhere')",
        )
        .execute(&db.pool)
        .await;
        assert_eq!(broken(missing).as_deref(), Some("catalogs_store_fkey"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stores_refuses_a_host_row_without_a_credential_reference() {
        let db = pg_support::TestDb::start_migrated().await;

        let with_null_reference = sqlx::query(
            "INSERT INTO stores (name, location_kind, host, port, database, credential_kind, \
                 created_at) \
             VALUES ('shared-2', 'host', 'db.example.internal', 5432, 'catalogs', 'env', now())",
        )
        .execute(&db.pool)
        .await;
        let without_kind = insert_host_store(&db.pool, "shared-3", "catalogs", None).await;

        // A missing reference, or a missing kind, is refused by the table.
        assert_eq!(
            broken(with_null_reference).as_deref(),
            Some("stores_check1")
        );
        assert_eq!(broken(without_kind).as_deref(), Some("stores_check1"));
        let complete = insert_host_store(
            &db.pool,
            "shared-4",
            "catalogs-4",
            Some(("secrets-manager", "ref-4")),
        )
        .await;
        assert!(complete.is_ok());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stores_refuses_an_account_database_row_with_a_host() {
        let db = pg_support::TestDb::start_migrated().await;
        // main is the account-database row, so test against the shape
        // alone by trying a host column on a row of that kind.
        let with_host = insert_store(
            &db.pool,
            "second",
            ("account-database", Some(("db.example.internal", 5432, "x"))),
            None,
        )
        .await;
        let with_credential = insert_store(
            &db.pool,
            "third",
            ("account-database", None),
            Some(("env", "X")),
        )
        .await;

        assert_eq!(broken(with_host).as_deref(), Some("stores_check"));
        assert_eq!(broken(with_credential).as_deref(), Some("stores_check"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_account_database_store_is_refused() {
        let db = pg_support::TestDb::start_migrated().await;
        main_in_the_account_database(&db.pool).await;

        let second = insert_store(&db.pool, "also-main", ("account-database", None), None).await;

        assert_eq!(
            broken(second).as_deref(),
            Some("stores_one_account_database")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_row_for_the_same_host_port_and_database_is_refused() {
        let db = pg_support::TestDb::start_migrated().await;
        let credential = Some(("env", "SHARED_PASSWORD"));
        insert_host_store(&db.pool, "shared-1", "catalogs", credential)
            .await
            .unwrap();

        // The same server and database under another name, and with the
        // host in another case, are refused; another database is not.
        let same = insert_host_store(&db.pool, "shared-2", "catalogs", credential).await;
        let upper_case = insert_store(
            &db.pool,
            "shared-3",
            ("host", Some(("DB.Example.Internal", 5432, "catalogs"))),
            credential,
        )
        .await;
        let other_database =
            insert_host_store(&db.pool, "shared-4", "other-catalogs", credential).await;

        assert_eq!(broken(same).as_deref(), Some("stores_one_row_per_database"));
        assert_eq!(
            broken(upper_case).as_deref(),
            Some("stores_one_row_per_database")
        );
        assert!(other_database.is_ok());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_store_cannot_be_removed_while_a_catalog_or_team_uses_it() {
        let db = pg_support::TestDb::start_migrated().await;
        insert_host_store(&db.pool, "shared-1", "catalogs", Some(("env", "X")))
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO catalogs (name, created_at, store, provisioned_at) \
             VALUES ('acme', now(), 'shared-1', now())",
        )
        .execute(&db.pool)
        .await
        .unwrap();

        let removal = sqlx::query("DELETE FROM stores WHERE name = 'shared-1'")
            .execute(&db.pool)
            .await;

        assert_eq!(broken(removal).as_deref(), Some("catalogs_store_fkey"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_store_cannot_be_removed_while_only_a_team_pins_it() {
        // Arrange: a second store, a team pinned to it, no catalog on it.
        let db = pg_support::TestDb::start_migrated().await;
        insert_host_store(&db.pool, "shared-1", "catalogs", Some(("env", "X")))
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO teams (id, name, display_name, created_at, store) \
             VALUES ('team-1', 'acme', '', now(), 'shared-1')",
        )
        .execute(&db.pool)
        .await
        .unwrap();

        // Act and assert: the pin alone refuses the removal.
        let removal = sqlx::query("DELETE FROM stores WHERE name = 'shared-1'")
            .execute(&db.pool)
            .await;
        assert_eq!(broken(removal).as_deref(), Some("teams_store_fkey"));

        // Clearing the pin lets the store go.
        sqlx::query("UPDATE teams SET store = NULL WHERE id = 'team-1'")
            .execute(&db.pool)
            .await
            .unwrap();
        let removed = sqlx::query("DELETE FROM stores WHERE name = 'shared-1'")
            .execute(&db.pool)
            .await
            .unwrap();
        assert_eq!(removed.rows_affected(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_registry_constraints_are_validated_and_the_index_is_built_concurrently() {
        let db = pg_support::TestDb::start_migrated().await;

        let constraints: Vec<(String, bool)> = sqlx::query_as(
            "SELECT conname, convalidated FROM pg_constraint \
             WHERE conname IN ('teams_store_fkey', 'catalogs_store_fkey') ORDER BY 1",
        )
        .fetch_all(&db.pool)
        .await
        .unwrap();
        let index: Vec<(bool, bool)> = sqlx::query_as(
            "SELECT i.indisvalid, i.indpred IS NOT NULL FROM pg_index i \
             JOIN pg_class c ON c.oid = i.indexrelid WHERE c.relname = 'catalogs_unprovisioned'",
        )
        .fetch_all(&db.pool)
        .await
        .unwrap();

        assert_eq!(
            constraints,
            vec![
                ("catalogs_store_fkey".to_string(), true),
                ("teams_store_fkey".to_string(), true),
            ]
        );
        assert_eq!(index, vec![(true, true)], "valid and partial");
    }

    /// Inserts a system audit entry about `kind` with the given target
    /// columns (a name, and a person reference).
    async fn insert_audit(
        pool: &PgPool,
        kind: &str,
        name: Option<&str>,
        person: Option<i64>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO audit_entries (at, action, source, actor_label, actor_operator, \
                 target_kind, target_name, target_person_id) \
             VALUES (now(), 'store.added', 'system', 'system:test', true, $1, $2, $3)",
        )
        .bind(kind)
        .bind(name)
        .bind(person)
        .execute(pool)
        .await
        .map(|_| ())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn audit_entries_accept_a_store_target_by_name_only() {
        let db = pg_support::TestDb::start_migrated().await;
        let person: i64 = sqlx::query_scalar(
            "INSERT INTO audit_people (user_id, name, display_name) \
             VALUES (NULL, 'alice', '') RETURNING id",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();

        let by_name = insert_audit(&db.pool, "store", Some("shared-1"), None).await;
        let nameless = insert_audit(&db.pool, "store", None, None).await;
        let with_person = insert_audit(&db.pool, "store", Some("shared-1"), Some(person)).await;
        let unknown_kind = insert_audit(&db.pool, "volume", Some("x"), None).await;

        assert!(by_name.is_ok());
        assert_eq!(
            broken(nameless).as_deref(),
            Some("audit_entries_target_shape")
        );
        assert_eq!(
            broken(with_person).as_deref(),
            Some("audit_entries_target_shape")
        );
        assert_eq!(
            broken(unknown_kind).as_deref(),
            Some("audit_entries_target_kind_check")
        );
        // 0038 adds the checks NOT VALID and 0039 validates them.
        let validated: Vec<(String, bool)> = sqlx::query_as(
            "SELECT conname, convalidated FROM pg_constraint WHERE conrelid = \
             'audit_entries'::regclass AND conname IN \
             ('audit_entries_target_kind_check', 'audit_entries_target_shape') ORDER BY 1",
        )
        .fetch_all(&db.pool)
        .await
        .unwrap();
        assert_eq!(
            validated,
            vec![
                ("audit_entries_target_kind_check".to_string(), true),
                ("audit_entries_target_shape".to_string(), true),
            ]
        );
    }
}
