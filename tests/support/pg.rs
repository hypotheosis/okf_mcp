//! A throwaway Postgres database for each test, dropped with its [`TestDb`].
//! The library's unit tests include this same file (`#[path]` in
//! `src/db.rs`), so there is one copy.
//!
//! **The server.** With [`SERVER_URL_VAR`] set, as `scripts/test-pg.sh run`
//! (`just test`) sets it, every test in every process shares that one server
//! and gets a database of its own on it. Unset, each [`TestDb`] starts its own
//! container, removed when the [`TestDb`] is dropped, so a plain `cargo test`
//! needs nothing running. The shared server is started outside the test
//! processes because Rust never drops statics: a container held in one would
//! never be removed, and under `cargo nextest` (a process per test) a
//! per-process server is a server per test.
//!
//! **Migrations run once per server.** [`template_name`] hashes the
//! migrations (okf's and apalis-postgres', which `okf_mcp::db::migrate` runs
//! after okf's), and [`ensure_template`] builds a database of that name under a
//! session advisory lock: concurrent processes race safely, and a server kept
//! running across schema changes gets a new template rather than serving a
//! stale one. A migrated test database ([`TestDb::start_migrated`]) is a
//! `CREATE DATABASE ... TEMPLATE` copy of it, much cheaper than migrating.
//!
//! **Cleanup.** A test's database is dropped `WITH (FORCE)` when its
//! [`TestDb`] is dropped, even when the test panicked. A killed process drops
//! nothing, so each process first drops the `test_*` databases older than
//! [`STALE_AFTER`] ([`drop_stale_databases`]); the name carries its creation
//! time ([`new_database_name`]).
//!
//! **Containers.** A start that fails after Docker created the container
//! (the start times out under load, or Docker refuses it with "endpoint ...
//! already exists") returns no [`ContainerAsync`], so nothing would remove
//! it. Every container therefore carries a label unique to its start, and
//! [`start_labelled`] removes whatever has that label when the start fails.
//! Tests that need a customized container (`tests/tls.rs`) start it through
//! [`start_with_retry`] too.
//!
//! A killed process runs no `Drop` and no start cleanup, so its containers
//! stay. The first start in each process therefore sweeps
//! ([`sweep_stale_containers`]): it removes the labelled containers whose
//! owner process (the pid in the label) no longer exists, and only those.

// Each crate that includes this file uses a different part of it (the CLI
// tests need `url`, the unit tests only `pool`).
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use sqlx::migrate::Migrator;
use sqlx::postgres::{PgConnectOptions, PgConnection, PgPool, PgPoolOptions};
use sqlx::Connection;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::bollard::errors::Error as DockerError;
use testcontainers_modules::testcontainers::bollard::query_parameters::{
    ListContainersOptions, RemoveContainerOptions,
};
use testcontainers_modules::testcontainers::bollard::Docker;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{
    ContainerAsync, ContainerRequest, Image, ImageExt, TestcontainersError,
};
use tokio::sync::OnceCell;

/// The label key whose value identifies one start of a test container.
pub const START_LABEL: &str = "okf-mcp.test-start";

/// The label key that names where the [`START_LABEL`] pid is meaningful: the
/// machine's boot id and the pid namespace ([`owner_scope`]). A pid means
/// nothing in another namespace (a test run inside a container sees its own
/// pids) or on another machine sharing the Docker daemon (two hosts report the
/// same namespace id), so the sweep removes only containers of its own scope.
pub const OWNER_SCOPE_LABEL: &str = "okf-mcp.test-owner-scope";

/// How long one start may take before it counts as failed (testcontainers'
/// own default).
pub const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

/// Attempts per container start: one retry covers a transient Docker
/// refusal or a start that timed out under load.
const START_ATTEMPTS: u32 = 2;

/// The environment variable naming a running server every test shares: a
/// `postgres://user:password@host:port` URL with no database name.
pub const SERVER_URL_VAR: &str = "TEST_PG_URL";

/// The age past which a `test_*` database belongs to a killed run, not to a
/// test still running, and is dropped.
pub const STALE_AFTER: Duration = Duration::from_secs(60 * 60);

/// The first part of every test database's name.
pub const DATABASE_PREFIX: &str = "test_";

/// The session advisory lock key that serializes template builds on a
/// server (any constant every process agrees on: "okf-tpl" in ASCII).
const TEMPLATE_LOCK_KEY: i64 = 0x006f_6b66_2d74_706c;

/// The pool size of each test's own connection to its database: 8, so a
/// mail test's worker (a LISTEN connection, plus fetches, acks and
/// heartbeats) and the test itself both fit (Decision 19).
const POOL_SIZE: u32 = 8;

/// The schema migrations, embedded here as in `okf_mcp::db::MIGRATOR` (this
/// file cannot name that: it is `crate::db` in the library's unit tests and
/// `okf_mcp::db` in the integration tests).
static MIGRATOR: Migrator = sqlx::migrate!("./migrations/accounts");

/// The catalog store migrations, with their history in
/// `catalog._sqlx_migrations`, as in `okf_mcp::db::CATALOG_MIGRATOR`.
fn catalog_migrator() -> Migrator {
    let mut migrator = sqlx::migrate!("./migrations/catalog");
    migrator.dangerous_set_table_name("catalog._sqlx_migrations");
    migrator
}

/// What `okf_mcp::db::migrate` does to the catalog schema after the account
/// migrations: adopts the baseline (records it without running it: 0041
/// and 0042 built the schema) and runs the later catalog migrations. It
/// writes no `catalog.store_identity` row, unlike `migrate`, which writes
/// `main`: a copy of the template is then free to be opened as any store
/// (tests use a second migrated database as a `host` store), and the first
/// store to open it writes its own name.
///
/// # Errors
/// [`sqlx::Error`] if a migration fails.
async fn adopt_catalog(connection: &mut PgConnection) -> Result<(), sqlx::Error> {
    let migrator = catalog_migrator();
    // `run_direct` with `skip = true` up to version 1: the baseline's
    // history row alone. A database with no account migrations has no
    // catalog schema to adopt.
    migrator.run_direct(Some(1), &mut *connection, true).await?;
    migrator.run_direct(None, &mut *connection, false).await?;
    Ok(())
}

/// Runs the account, catalog (adoption) and apalis migrations on `pool`'s
/// database, as `okf_mcp::db::migrate` does.
///
/// # Panics
/// If a migration fails: a test environment problem.
async fn migrate_in_place(pool: &PgPool) {
    MIGRATOR.run(pool).await.expect("migrations apply");
    let mut connection = pool.acquire().await.expect("a connection");
    adopt_catalog(&mut connection)
        .await
        .expect("catalog migrations apply");
    drop(connection);
    apalis_migrator()
        .run(pool)
        .await
        .expect("apalis migrations apply");
}

/// In [`StoreMode::Separate`], removes schema `catalog` (the tables the
/// account migrations built) from the account database, so that a statement
/// sent to the account pool instead of the store's fails with "relation does
/// not exist" instead of reading nothing. Does nothing in
/// [`StoreMode::Same`], where the account database is the store.
///
/// Production keeps the schema in a separated deployment's account database
/// (release 3 removes it in a later migration), but nothing reads it there:
/// every catalog statement goes to the store's pool.
///
/// # Panics
/// If the statement fails: a test environment problem.
async fn drop_account_catalog(connection: &mut PgConnection) {
    if store_mode() == StoreMode::Separate {
        sqlx::query("DROP SCHEMA catalog CASCADE")
            .execute(connection)
            .await
            .expect("drop the account database's catalog schema");
    }
}

/// apalis-postgres' migrations (schema `apalis`, history in
/// `apalis._sqlx_migrations`), which `okf_mcp::db::migrate` runs after
/// [`MIGRATOR`].
fn apalis_migrator() -> Migrator {
    // `::<()>`: the job type a storage carries does not matter for its schema.
    apalis_postgres::PostgresStorage::<()>::migrations()
}

/// The shared server's template, prepared once per process. A
/// `tokio::sync::OnceCell` runs its async initializer once, however many
/// tests ask at the same time (like a lock-guarded lazy global in Python).
static SHARED_TEMPLATE: OnceCell<String> = OnceCell::const_new();

/// A database of a test's own and a pool connected to it, with up to 8
/// connections.
pub struct TestDb {
    /// Connected to the test's database.
    pub pool: PgPool,
    /// The connection URL, for code that connects by itself (the CLI).
    pub url: String,
    // Fields drop in declaration order: the pool closes before the database
    // is dropped (and, without a shared server, its container removed).
    _database: Database,
}

impl TestDb {
    /// A pool on the `main` store whose connections see only schema
    /// `catalog`, as a store's pool does: for tests that call store-layer
    /// functions with a bare pool. Lazy, with the same limit as
    /// [`TestDb::pool`]. In [`StoreMode::Same`] the store is this database;
    /// in [`StoreMode::Separate`] it is the second database ([`store_options`]).
    pub fn store(&self) -> PgPool {
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(self.pool.options().get_max_connections())
            .after_connect(|connection, _meta| {
                Box::pin(async move {
                    sqlx::query("SET search_path = catalog")
                        .execute(connection)
                        .await
                        .map(|_| ())
                })
            })
            .connect_lazy_with(store_options(&self.pool))
    }

    /// Creates one more store database on this test's server, empty and
    /// migrated as a new store is, dropped with this [`TestDb`]. The caller
    /// registers it (a `stores` row with the `env` credential
    /// [`STORE_PASSWORD_VAR`], whose variable this sets).
    ///
    /// # Arguments
    /// * `suffix` - lowercase letters and digits, unique among this test's
    ///   store databases.
    ///
    /// # Returns
    /// The database's name.
    ///
    /// # Panics
    /// As [`TestDb::start`].
    pub async fn extra_store_database(&self, suffix: &str) -> String {
        let database = &self._database;
        let name = format!("{}_x{suffix}", database.name);
        create_store_database(&database.server_url, &name, database.container.is_some()).await;
        set_store_password_once(&database.server_url);
        database
            .extra
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(name.clone());
        name
    }

    /// Creates an empty database (no migrations), for tests of migrating.
    ///
    /// # Panics
    /// If no server can be reached or started (Docker unreachable, or a
    /// container fails to start twice), or the database cannot be created:
    /// a test environment problem, reported at once.
    pub async fn start() -> Self {
        Self::create(false).await
    }

    /// Creates a database migrated as production's release one leaves it:
    /// the account database holds schema `catalog` (the store `main`) in
    /// every [`StoreMode`], with no second database. For tests of the
    /// account migrations' own schema and of the store `main` being the
    /// account database.
    ///
    /// # Panics
    /// As [`TestDb::start`], or if a migration fails.
    pub async fn start_account_layout() -> Self {
        let test_db = Self::create(false).await;
        migrate_in_place(&test_db.pool).await;
        test_db
    }

    /// Creates a migrated database, a copy of the server's template.
    ///
    /// # Panics
    /// As [`TestDb::start`], or if the template cannot be built.
    pub async fn start_migrated() -> Self {
        Self::create(true).await
    }

    /// Creates the test's database, migrated when `migrated`: on the shared
    /// server, a new database (a copy of the template); on a container of
    /// the test's own, its `postgres` database, migrated in place (a
    /// template would be built for one copy).
    async fn create(migrated: bool) -> Self {
        let (server_url, container) = server().await;
        let name = match &container {
            Some(_) => "postgres".to_string(),
            None => {
                // The shared server is prepared (template built, stale
                // databases dropped) once per process, whether or not this
                // test migrates.
                let template = shared_template(&server_url).await;
                let name = new_database_name(SystemTime::now());
                create_database(&server_url, &name, migrated.then_some(template.as_str()))
                    .await
                    .expect("create the test database");
                name
            }
        };
        let url = format!("{server_url}/{name}");
        let pool = PgPoolOptions::new()
            .max_connections(POOL_SIZE)
            .connect(&url)
            .await
            .expect("connect to the test database");
        if migrated && container.is_some() {
            migrate_in_place(&pool).await;
            let mut connection = pool.acquire().await.expect("a connection");
            drop_account_catalog(&mut connection).await;
        }
        let (store_database, registry_key) = match migrated && store_mode() == StoreMode::Separate {
            true => {
                let (store, key) =
                    separate_store(&server_url, &name, container.is_some(), &pool).await;
                (Some(store), Some(key))
            }
            false => (None, None),
        };
        Self {
            pool,
            url,
            _database: Database {
                server_url,
                name,
                store_database,
                registry_key,
                extra: Mutex::new(Vec::new()),
                container,
            },
        }
    }
}

/// Where the catalog tests keep catalog data (`OKF_TEST_STORE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreMode {
    /// The store `main` is the account database (the default).
    Same,
    /// The store `main` is a second database on the same server, registered
    /// as a `host` store with the `env` credential kind, so a query that
    /// reads the wrong database finds an empty table or none.
    Separate,
}

/// The variable that picks the [`StoreMode`]: `same` (the default) or
/// `separate`.
pub const STORE_MODE_VAR: &str = "OKF_TEST_STORE";

/// The variable that holds the second database's password (the `env`
/// credential of the registered store).
pub const STORE_PASSWORD_VAR: &str = "OKF_TEST_STORE_PASSWORD";

/// The environment of a test registry that knows only the harness's own
/// store credential: [`STORE_PASSWORD_VAR`] (set in `separate` mode, unset
/// in `same` mode) and nothing else.
pub fn harness_password(name: &str) -> Option<String> {
    (name == STORE_PASSWORD_VAR)
        .then(|| std::env::var(name).ok())
        .flatten()
}

/// The mode `OKF_TEST_STORE` names.
///
/// # Panics
/// On a value other than `same` or `separate`: a typo must not silently
/// run the default mode.
pub fn store_mode() -> StoreMode {
    match std::env::var(STORE_MODE_VAR).as_deref() {
        Err(_) | Ok("" | "same") => StoreMode::Same,
        Ok("separate") => StoreMode::Separate,
        Ok(other) => panic!("{STORE_MODE_VAR} must be `same` or `separate`, not {other:?}"),
    }
}

/// The second database of each [`StoreMode::Separate`] test, by the account
/// database's server and name ([`registry_key`]), for the helpers that derive
/// a store handle from the account pool alone.
static SEPARATE_STORES: Mutex<Option<HashMap<String, PgConnectOptions>>> = Mutex::new(None);

/// The connection settings of the `main` store of the test whose account
/// database `accounts` is connected to: the second database in
/// [`StoreMode::Separate`], else the account database's own.
///
/// # Arguments
/// * `accounts` - a pool on a test's account database.
///
/// # Returns
/// Options (login included) for the store's database.
pub fn store_options(accounts: &PgPool) -> PgConnectOptions {
    let options = (*accounts.connect_options()).clone();
    let registered = SEPARATE_STORES
        .lock()
        .expect("the store registry lock")
        .as_ref()
        .and_then(|stores| stores.get(&registry_key(&options)).cloned());
    registered.unwrap_or(options)
}

/// The key of an account database in [`SEPARATE_STORES`]: its server (host
/// and port) and its name. The name alone is not unique: every container of
/// its own holds its account database as `postgres`.
pub fn registry_key(accounts: &PgConnectOptions) -> String {
    format!(
        "{}:{}/{}",
        accounts.get_host(),
        accounts.get_port(),
        accounts.get_database().unwrap_or_default()
    )
}

/// Registers `store` as the `main` store of the account database `accounts`
/// connects to.
///
/// # Returns
/// The key to [`unregister_store`] it with.
pub fn register_store(accounts: &PgConnectOptions, store: PgConnectOptions) -> String {
    let key = registry_key(accounts);
    SEPARATE_STORES
        .lock()
        .expect("the store registry lock")
        .get_or_insert_with(HashMap::new)
        .insert(key.clone(), store);
    key
}

/// Removes the registration [`register_store`] made under `key`.
pub fn unregister_store(key: &str) {
    if let Some(stores) = SEPARATE_STORES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_mut()
    {
        stores.remove(key);
    }
}

/// A pool on the store `main` of the test whose account pool is `accounts`:
/// for a test statement on schema `catalog`, which is a database of its own
/// in [`StoreMode::Separate`]. Connects on first use; drop it when done.
///
/// # Arguments
/// * `accounts` - a pool on a test's account database.
pub fn store_pool(accounts: &PgPool) -> PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_lazy_with(store_options(accounts))
}

/// Makes the second database of a [`StoreMode::Separate`] test and registers
/// it as the store `main` in the account database.
///
/// On the shared server the database is a copy of the store template; on a
/// container of the test's own it is built in place. The `stores` row `main`
/// becomes a `host` row naming it, with the `env` credential
/// [`STORE_PASSWORD_VAR`]. The copy carries no identity row: the first open
/// writes `main`.
///
/// # Returns
/// The second database's name, for [`Database`] to drop, and its registry key.
///
/// # Panics
/// If a statement fails: a test environment problem.
async fn separate_store(
    server_url: &str,
    account_database: &str,
    own_container: bool,
    accounts: &PgPool,
) -> (String, String) {
    let store_name = format!("{account_database}_store");
    create_store_database(server_url, &store_name, own_container).await;
    let options = (*accounts.connect_options()).clone();
    // The password of every test database is the server's: one value per
    // process, set before the first registered store opens.
    set_store_password_once(server_url);
    sqlx::query(
        "UPDATE stores SET location_kind = 'host', host = $1, port = $2, database = $3, \
         credential_kind = 'env', credential_ref = $4 WHERE name = 'main'",
    )
    .bind(options.get_host())
    .bind(i32::from(options.get_port()))
    .bind(&store_name)
    .bind(STORE_PASSWORD_VAR)
    .execute(accounts)
    .await
    .expect("register the second database as the store main");
    let key = register_store(&options, options.clone().database(&store_name));
    (store_name, key)
}

/// Creates database `name` holding an empty, migrated store schema: a copy
/// of the store template on the shared server, built in place on a container
/// of the test's own.
///
/// # Panics
/// If a statement fails: a test environment problem.
async fn create_store_database(server_url: &str, name: &str, own_container: bool) {
    if own_container {
        create_database(server_url, name, None)
            .await
            .expect("create the store database");
        let mut connection = PgConnection::connect(&format!("{server_url}/{name}"))
            .await
            .expect("connect to the store database");
        build_store_schema(&mut connection)
            .await
            .expect("catalog migrations apply to the store");
        connection
            .close()
            .await
            .expect("close the store connection");
    } else {
        let template = format!("{}_store", shared_template(server_url).await);
        create_database(server_url, name, Some(&template))
            .await
            .expect("create the store database");
    }
}

/// Sets [`STORE_PASSWORD_VAR`] to the server URL's password, once per
/// process. (Edition 2021: `set_var` is a safe call. It runs before the
/// first store opens, and every test of the process sets the same value.)
fn set_store_password_once(server_url: &str) {
    static SET: std::sync::Once = std::sync::Once::new();
    SET.call_once(|| std::env::set_var(STORE_PASSWORD_VAR, server_password(server_url)));
}

/// The password `server_url` logs in with, as the connection uses it: the
/// URL's userinfo is percent-encoded, so `p%40ss` is the password `p@ss`.
///
/// # Returns
/// The decoded password, or an empty string when the URL has none or does
/// not parse.
pub fn server_password(server_url: &str) -> String {
    url::Url::parse(server_url)
        .ok()
        .and_then(|parsed| {
            parsed.password().map(|encoded| {
                percent_encoding::percent_decode_str(encoded)
                    .decode_utf8_lossy()
                    .into_owned()
            })
        })
        .unwrap_or_default()
}

/// Creates schema `catalog` in an empty database and runs every catalog
/// migration on it, as a new store gets them, without an identity row.
async fn build_store_schema(connection: &mut PgConnection) -> Result<(), sqlx::Error> {
    execute_ddl(connection, "CREATE SCHEMA catalog".to_string()).await?;
    catalog_migrator()
        .run_direct(None, &mut *connection, false)
        .await?;
    // The identity row is written by the first open, under the name the
    // copy is registered as.
    sqlx::query("DELETE FROM catalog.store_identity")
        .execute(connection)
        .await?;
    Ok(())
}

/// A test's database; dropping it drops the database (on the shared server)
/// or removes the container that holds it.
struct Database {
    server_url: String,
    name: String,
    /// The second database of a [`StoreMode::Separate`] test.
    store_database: Option<String>,
    /// The key of that store in [`SEPARATE_STORES`].
    registry_key: Option<String>,
    /// The databases [`TestDb::extra_store_database`] made.
    extra: Mutex<Vec<String>>,
    /// The container of this test's own, when no server is shared.
    container: Option<ContainerAsync<Postgres>>,
}

// `Drop` runs when the value goes out of scope (like `__del__`, but
// deterministic), including while a failed test unwinds.
impl Drop for Database {
    fn drop(&mut self) {
        if let Some(key) = &self.registry_key {
            unregister_store(key);
        }
        if self.container.is_some() {
            // The container's own `Drop` removes the whole server.
            return;
        }
        let (server_url, name) = (self.server_url.clone(), self.name.clone());
        let databases: Vec<String> = self
            .store_database
            .iter()
            .cloned()
            .chain(
                self.extra
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .iter()
                    .cloned(),
            )
            .collect();
        // `drop` cannot `.await`, and blocking on the test's own runtime from
        // inside it would deadlock a single-threaded one, so the DROP runs on
        // a thread with a runtime of its own, joined before `drop` returns.
        let dropped = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| error.to_string())?;
            runtime
                .block_on(async {
                    drop_database(&server_url, &name).await?;
                    for store in &databases {
                        drop_database(&server_url, store).await?;
                    }
                    Ok::<(), sqlx::Error>(())
                })
                .map_err(|error| error.to_string())
        })
        .join();
        // A failure here must not panic (a panic during unwinding aborts the
        // process); a database left behind is dropped by a later run.
        match dropped {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!("drop test database {}: {error}", self.name),
            Err(_) => tracing::warn!("drop test database {}: the thread panicked", self.name),
        }
    }
}

/// The URL of the shared server, if [`SERVER_URL_VAR`] names one.
///
/// # Returns
/// The URL without a trailing `/`, or `None` when the variable is unset or
/// empty.
pub fn shared_server_url() -> Option<String> {
    std::env::var(SERVER_URL_VAR)
        .ok()
        .map(|url| url.trim_end_matches('/').to_string())
        .filter(|url| !url.is_empty())
}

/// A server for one test: the shared one, or a container of its own.
///
/// # Returns
/// The server URL (no database name) and, without a shared server, the
/// container, which removes itself when dropped.
///
/// # Panics
/// As [`start_with_retry`].
pub async fn server() -> (String, Option<ContainerAsync<Postgres>>) {
    if let Some(url) = shared_server_url() {
        return (url, None);
    }
    let container = start_with_retry(postgres_17).await;
    let host = container.get_host().await.expect("container host");
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("container port");
    let url = format!("postgres://postgres:postgres@{host}:{port}");
    (url, Some(container))
}

/// The shared server's template name, after this process's first call has
/// built it if needed and dropped stale databases.
async fn shared_template(server_url: &str) -> String {
    let prepared = SHARED_TEMPLATE.get_or_init(|| async {
        let name = template_name();
        ensure_template(server_url, &name)
            .await
            .expect("build the migrated template database");
        drop_stale_databases(server_url, SystemTime::now())
            .await
            .expect("drop stale test databases");
        name
    });
    prepared.await.clone()
}

/// The migrated template's name: `okf_tpl_` and a hash of every migration's
/// version and checksum, so a schema change gets a template of its own.
pub fn template_name() -> String {
    let mut hasher = Sha256::new();
    // Names the layout of the templates too: a server that kept an older
    // template without a store template builds a new one.
    hasher.update(b"account-and-store-templates");
    // The account template of `separate` mode has no schema `catalog`.
    hasher.update([u8::from(store_mode() == StoreMode::Separate)]);
    let catalog = catalog_migrator();
    for migration in MIGRATOR
        .iter()
        .chain(catalog.iter())
        .chain(apalis_migrator().iter())
    {
        hasher.update(migration.version.to_le_bytes());
        hasher.update(&migration.checksum);
    }
    let digest = hasher.finalize();
    let hex: String = digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("okf_tpl_{hex}")
}

/// How many okf migrations a migrated database has applied (in
/// `public._sqlx_migrations`; apalis keeps its own history).
pub fn migration_count() -> usize {
    MIGRATOR.iter().count()
}

/// Makes `name` a complete migrated template on `server_url`, building it
/// unless another process already has.
///
/// The check and the build run under a session advisory lock, so of several
/// processes racing, one builds and the rest wait, then find it complete. A
/// template is marked complete (`IS_TEMPLATE`) only once migrated, so one
/// whose builder was killed is dropped and built again. Its store template
/// (`{name}_store`) is marked first and the account template last: the
/// account mark is the commit point, and a builder killed between the two
/// leaves a marked store template, which the rebuild unmarks and drops.
///
/// # Arguments
/// * `server_url` - the server, with no database name.
/// * `name` - the template's name ([`template_name`], or a test's own).
///
/// # Returns
/// Whether this call built it.
///
/// # Errors
/// If the server is unreachable or a statement or migration fails. The lock
/// is released either way: it ends with the session.
pub async fn ensure_template(server_url: &str, name: &str) -> Result<bool, sqlx::Error> {
    let mut admin = connect_admin(server_url).await?;
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(TEMPLATE_LOCK_KEY)
        .execute(&mut admin)
        .await?;
    let complete: Option<bool> =
        sqlx::query_scalar("SELECT datistemplate FROM pg_database WHERE datname = $1")
            .bind(name)
            .fetch_optional(&mut admin)
            .await?;
    let built = match complete {
        Some(true) => false,
        Some(false) => {
            drop_template_on(&mut admin, name).await?;
            build_template(&mut admin, server_url, name).await?;
            true
        }
        None => {
            build_template(&mut admin, server_url, name).await?;
            true
        }
    };
    // Closing the session releases the advisory lock.
    admin.close().await?;
    Ok(built)
}

/// Creates `name`, migrates it (okf's migrations, then apalis'), then marks
/// it a template.
async fn build_template(
    admin: &mut PgConnection,
    server_url: &str,
    name: &str,
) -> Result<(), sqlx::Error> {
    // The store template (`{name}_store`: schema `catalog` alone, for the
    // second database of `OKF_TEST_STORE=separate`) is complete before the
    // account template is, so a complete account template has its store one.
    let store_name = format!("{name}_store");
    drop_template_on(admin, &store_name).await?;
    execute_ddl(admin, format!("CREATE DATABASE {}", quoted(&store_name))).await?;
    let mut store = PgConnection::connect(&format!("{server_url}/{store_name}")).await?;
    build_store_schema(&mut store).await?;
    store.close().await?;
    execute_ddl(admin, format!("CREATE DATABASE {}", quoted(name))).await?;
    let mut template = PgConnection::connect(&format!("{server_url}/{name}")).await?;
    MIGRATOR.run(&mut template).await?;
    adopt_catalog(&mut template).await?;
    drop_account_catalog(&mut template).await;
    apalis_migrator().run(&mut template).await?;
    // A copy needs the template to have no connections.
    template.close().await?;
    // Marked only now: the store first, the account last. The account mark
    // is the commit point ([`ensure_template`] trusts nothing else), so a
    // builder killed between the two leaves an unmarked account template and
    // a marked store one, and the next build unmarks the store's to drop it.
    for database in [&store_name, name] {
        execute_ddl(
            admin,
            format!("ALTER DATABASE {} IS_TEMPLATE true", quoted(database)),
        )
        .await?;
    }
    Ok(())
}

/// A new test database name: [`DATABASE_PREFIX`], the creation time in Unix
/// seconds, the process id and a counter (unique across processes).
///
/// # Arguments
/// * `now` - the creation time.
pub fn new_database_name(now: SystemTime) -> String {
    let seconds = unix_seconds(now);
    format!(
        "{DATABASE_PREFIX}{seconds}_{}",
        unique_start_label().replace('-', "_")
    )
}

/// When a test database was created, from its name.
///
/// # Returns
/// Unix seconds, or `None` if `name` is not a [`new_database_name`].
pub fn database_created_at(name: &str) -> Option<u64> {
    name.strip_prefix(DATABASE_PREFIX)?
        .split('_')
        .next()?
        .parse()
        .ok()
}

/// Drops every test database created more than [`STALE_AFTER`] before `now`.
///
/// A drop that fails is logged and skipped, not returned: another process
/// sweeping at the same moment may be dropping the same database.
///
/// # Returns
/// The names this call dropped.
///
/// # Errors
/// If the server is unreachable or the databases cannot be listed.
pub async fn drop_stale_databases(
    server_url: &str,
    now: SystemTime,
) -> Result<Vec<String>, sqlx::Error> {
    let cutoff = unix_seconds(now).saturating_sub(STALE_AFTER.as_secs());
    let mut admin = connect_admin(server_url).await?;
    let names: Vec<String> = sqlx::query_scalar("SELECT datname FROM pg_database")
        .fetch_all(&mut admin)
        .await?;
    let stale: Vec<String> = names
        .into_iter()
        .filter(|name| database_created_at(name).is_some_and(|created| created < cutoff))
        .collect();
    let mut dropped = Vec::with_capacity(stale.len());
    for name in stale {
        match drop_database_on(&mut admin, &name).await {
            Ok(()) => dropped.push(name),
            Err(error) => tracing::warn!("drop stale test database {name}: {error}"),
        }
    }
    admin.close().await?;
    Ok(dropped)
}

/// Creates `name` on `server_url`, a copy of `template` when given (else
/// an empty database).
///
/// # Errors
/// If the server is unreachable or the create fails.
pub async fn create_database(
    server_url: &str,
    name: &str,
    template: Option<&str>,
) -> Result<(), sqlx::Error> {
    let statement = match template {
        Some(template) => format!(
            "CREATE DATABASE {} TEMPLATE {}",
            quoted(name),
            quoted(template)
        ),
        None => format!("CREATE DATABASE {}", quoted(name)),
    };
    let mut admin = connect_admin(server_url).await?;
    execute_ddl(&mut admin, statement).await?;
    admin.close().await
}

/// Drops `name` on `server_url`, ending its sessions (`WITH (FORCE)`); a
/// database already gone is fine.
///
/// # Errors
/// If the server is unreachable or the drop fails.
pub async fn drop_database(server_url: &str, name: &str) -> Result<(), sqlx::Error> {
    let mut admin = connect_admin(server_url).await?;
    drop_database_on(&mut admin, name).await?;
    admin.close().await
}

/// Drops the template `name` and its store template (`{name}_store`) on
/// `server_url`, unmarking each first; either already gone is fine.
///
/// The account template goes first, as its mark is the commit point
/// [`ensure_template`] checks: stopped halfway, this leaves a state the next
/// build recovers from.
///
/// # Errors
/// If the server is unreachable or a statement fails.
pub async fn drop_template(server_url: &str, name: &str) -> Result<(), sqlx::Error> {
    let mut admin = connect_admin(server_url).await?;
    drop_template_on(&mut admin, name).await?;
    drop_template_on(&mut admin, &format!("{name}_store")).await?;
    admin.close().await
}

/// As [`drop_database_on`], but first unmarks `name` if it is a template:
/// PostgreSQL refuses to drop a database whose `IS_TEMPLATE` is true. Every
/// drop in the template build and its recovery goes through here.
///
/// # Errors
/// If a statement fails.
async fn drop_template_on(admin: &mut PgConnection, name: &str) -> Result<(), sqlx::Error> {
    let marked: Option<bool> =
        sqlx::query_scalar("SELECT datistemplate FROM pg_database WHERE datname = $1")
            .bind(name)
            .fetch_optional(&mut *admin)
            .await?;
    if marked == Some(true) {
        let statement = format!("ALTER DATABASE {} IS_TEMPLATE false", quoted(name));
        execute_ddl(admin, statement).await?;
    }
    drop_database_on(admin, name).await
}

/// As [`drop_database`], over an open connection to the server.
async fn drop_database_on(admin: &mut PgConnection, name: &str) -> Result<(), sqlx::Error> {
    let statement = format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", quoted(name));
    execute_ddl(admin, statement).await
}

/// Runs a statement about a whole database (`CREATE`, `ALTER` or `DROP
/// DATABASE`) over `admin`.
///
/// These statements cannot take a bind parameter for the name, so callers
/// format it in through [`quoted`], which accepts only the names this file
/// generates and tests' literals; that is the audit `AssertSqlSafe` asks for.
///
/// # Errors
/// If the statement fails.
pub async fn execute_ddl(admin: &mut PgConnection, statement: String) -> Result<(), sqlx::Error> {
    sqlx::query(sqlx::AssertSqlSafe(statement))
        .execute(admin)
        .await?;
    Ok(())
}

/// `name` as a quoted SQL identifier.
///
/// # Panics
/// If `name` holds anything but lowercase ASCII letters, digits and `_`:
/// every database name here is generated, so another is a bug in a test.
pub fn quoted(name: &str) -> String {
    let generated = name
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_');
    assert!(generated, "not a generated database name: {name:?}");
    format!("\"{name}\"")
}

/// A connection to the server's `postgres` database, from which databases
/// are created and dropped.
///
/// # Errors
/// If the server is unreachable.
pub async fn connect_admin(server_url: &str) -> Result<PgConnection, sqlx::Error> {
    PgConnection::connect(&format!("{server_url}/postgres")).await
}

/// Seconds since the Unix epoch (0 for a time before it).
fn unix_seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// A request for Postgres 17 (Alpine), the image every test database uses;
/// callers add their own settings (files, command) before starting it.
pub fn postgres_17() -> ContainerRequest<Postgres> {
    Postgres::default().with_tag("17-alpine")
}

/// Starts the container `request` builds, through [`start_labelled`] with
/// [`STARTUP_TIMEOUT`], retrying a failed start once with a fresh label.
///
/// # Arguments
/// * `request` - builds the container request; called once per attempt,
///   because a request is consumed by its start and cannot be cloned.
///
/// # Panics
/// If every attempt fails (the last error is reported).
pub async fn start_with_retry<I: Image>(
    request: impl Fn() -> ContainerRequest<I>,
) -> ContainerAsync<I> {
    let mut attempt = 1;
    loop {
        let labelled = request().with_startup_timeout(STARTUP_TIMEOUT);
        let image = labelled.image().name().to_string();
        match start_labelled(labelled, &unique_start_label()).await {
            Ok(container) => return container,
            Err(error) if attempt < START_ATTEMPTS => {
                tracing::warn!("{image} container start failed, retrying: {error}");
                attempt += 1;
            }
            Err(error) => panic!("start a {image} container (is docker reachable?): {error}"),
        }
    }
}

/// A [`START_LABEL`] value no other start uses: the process id keeps
/// concurrent test binaries apart, the counter keeps one binary's tests apart.
pub fn unique_start_label() -> String {
    // A process-wide counter; `fetch_add` hands each caller a distinct value
    // even from parallel threads (like `itertools.count()` behind a lock).
    static NEXT_START: AtomicU64 = AtomicU64::new(0);
    let start = NEXT_START.fetch_add(1, Ordering::Relaxed);
    format!("{}-{start}", std::process::id())
}

/// Starts `request` labelled `START_LABEL=label`; on failure, removes any
/// container the attempt left behind before returning the error.
///
/// # Arguments
/// * `request` - the configured container (image, files, command, startup
///   timeout).
/// * `label` - this start's [`START_LABEL`] value ([`unique_start_label`]).
///
/// # Errors
/// The start's error, or, if the cleanup also failed, that error wrapped as
/// [`TestcontainersError::Other`] (naming both).
pub async fn start_labelled<I: Image>(
    request: ContainerRequest<I>,
    label: &str,
) -> Result<ContainerAsync<I>, TestcontainersError> {
    sweep_stale_containers_once().await;
    let request = request.with_label(START_LABEL, label);
    let request = match owner_scope() {
        Some(namespace) => request.with_label(OWNER_SCOPE_LABEL, namespace),
        None => request,
    };
    let start_error = match request.start().await {
        Ok(container) => return Ok(container),
        Err(error) => error,
    };
    match remove_labelled(label).await {
        Ok(_) => Err(start_error),
        Err(cleanup_error) => Err(TestcontainersError::other(format!(
            "{start_error}; removing its container also failed: {cleanup_error}"
        ))),
    }
}

/// Whether `error` is the kind a start with a zero-length timeout can
/// legitimately end in: the wait stage's own `StartupTimeout`, or, when the
/// machine is loaded enough that an earlier stage (pull, create, copy) runs
/// out of time first, a Docker client, I/O or wrapped error that says it timed
/// out. Any other error (a refusal, a missing image, a bad request) is not.
///
/// # Arguments
/// * `error` - what a start with a zero-length timeout returned.
///
/// # Returns
/// Whether the error is a timeout of some stage of the start.
pub fn is_timeout_class(error: &TestcontainersError) -> bool {
    use testcontainers_modules::testcontainers::core::error::WaitContainerError;

    match error {
        TestcontainersError::WaitContainer(WaitContainerError::StartupTimeout) => true,
        TestcontainersError::Io(io) => io.kind() == std::io::ErrorKind::TimedOut,
        TestcontainersError::Client(_) | TestcontainersError::Other(_) => {
            let text = error.to_string().to_lowercase();
            text.contains("timed out") || text.contains("timeout")
        }
        TestcontainersError::WaitContainer(_)
        | TestcontainersError::PortNotExposed { .. }
        | TestcontainersError::MissingInfo(_)
        | TestcontainersError::Exec(_) => false,
    }
}

/// The ids of every container (running or not) labelled `START_LABEL=label`.
///
/// # Errors
/// If Docker is unreachable or refuses the listing.
pub async fn labelled_containers(label: &str) -> Result<Vec<String>, DockerError> {
    let docker = Docker::connect_with_defaults()?;
    let filters = HashMap::from([("label".to_string(), vec![format!("{START_LABEL}={label}")])]);
    let containers = docker
        .list_containers(Some(ListContainersOptions {
            all: true,
            filters: Some(filters),
            ..Default::default()
        }))
        .await?;
    Ok(containers
        .into_iter()
        .filter_map(|summary| summary.id)
        .collect())
}

/// Force-removes every container labelled `START_LABEL=label`, with its
/// anonymous volumes.
///
/// # Returns
/// How many containers this call removed.
///
/// # Errors
/// If Docker is unreachable, or refuses a removal for a reason other than
/// the container being gone already (404) or already being removed (409),
/// as when testcontainers' own `Drop` got there first.
pub async fn remove_labelled(label: &str) -> Result<usize, DockerError> {
    let docker = Docker::connect_with_defaults()?;
    let mut removed = 0;
    for id in labelled_containers(label).await? {
        let options = RemoveContainerOptions {
            force: true,
            v: true,
            ..Default::default()
        };
        match docker.remove_container(&id, Some(options)).await {
            Ok(()) => removed += 1,
            Err(DockerError::DockerResponseServerError {
                status_code: 404 | 409,
                ..
            }) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(removed)
}

/// Where this process's pid is meaningful (Linux): the kernel's boot id, which
/// differs between machines and between boots, and the pid namespace (the
/// target of `/proc/self/ns/pid`, such as `pid:[4026531836]`), as
/// `<boot id>/<namespace>`.
///
/// # Returns
/// `None` where either cannot be read (not Linux); the container then carries
/// no scope label and no sweep removes it.
pub fn owner_scope() -> Option<String> {
    let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    let namespace = std::fs::read_link("/proc/self/ns/pid").ok()?;
    Some(format!(
        "{}/{}",
        boot_id.trim(),
        namespace.to_string_lossy()
    ))
}

/// Whether the process with this pid might still be alive: `false` only when
/// the kernel says there is no such process (`ESRCH`).
///
/// This sends signal 0 with `kill(2)`, which checks existence and permission
/// without delivering anything. Unlike a look in `/proc`, it does not depend on
/// what the mount hides (`hidepid`) or on a file lookup that can fail. A
/// process of another user answers `EPERM` (alive), and any other answer
/// (an invalid pid, an unexpected error) is uncertain, so it counts as alive:
/// a wrong "alive" keeps a container, a wrong "dead" would remove a live
/// test's. A zombie counts as alive.
pub fn process_may_be_alive(pid: u32) -> bool {
    // Pid 0 and values above `i32::MAX` are not process ids; `kill` would
    // treat 0 (or a negative number) as a process group.
    let Some(pid) = i32::try_from(pid).ok().filter(|pid| *pid > 0) else {
        return true;
    };
    // SAFETY: `kill` takes two integers and reads no memory of ours. Signal 0
    // delivers nothing, so the call cannot affect the target process.
    let result = unsafe { libc::kill(pid, 0) };
    if result == 0 {
        return true;
    }
    signal_error_means_alive(std::io::Error::last_os_error().raw_os_error())
}

/// What a failed `kill(pid, 0)` says: dead only for `ESRCH`; `EPERM`, any other
/// error and an unreadable error code all mean alive or unknown.
pub fn signal_error_means_alive(errno: Option<i32>) -> bool {
    errno != Some(libc::ESRCH)
}

/// Whether `mountinfo` (the text of `/proc/self/mountinfo`) shows `/proc`
/// mounted with restricted process visibility (`hidepid=1`/`2`, or
/// `subset=pid`), or does not show a `/proc` mount at all.
///
/// The liveness probe does not depend on `/proc`, so this is a second guard:
/// where the process table is restricted, nobody can say what a test run is
/// allowed to see, and the sweep stays off.
pub fn proc_mount_is_restricted(mountinfo: &str) -> bool {
    let mut found = false;
    for line in mountinfo.lines() {
        let Some((before, after)) = line.split_once(" - ") else {
            continue;
        };
        let before: Vec<&str> = before.split_whitespace().collect();
        let after: Vec<&str> = after.split_whitespace().collect();
        // before: id parent major:minor root mount-point options ...;
        // after: fstype source super-options.
        if before.get(4) != Some(&"/proc") || after.first() != Some(&"proc") {
            continue;
        }
        found = true;
        let options = before
            .get(5)
            .into_iter()
            .chain(after.get(2))
            .flat_map(|options| options.split(','));
        for option in options {
            match option.split_once('=') {
                Some(("hidepid", "0" | "off")) => {}
                Some(("hidepid", _)) | Some(("subset", "pid")) => return true,
                _ => {}
            }
        }
    }
    !found
}

/// Whether the sweep may run here: the owner scope is known and `/proc` is not
/// restricted. An unreadable `/proc/self/mountinfo` counts as restricted.
pub fn liveness_is_reliable() -> bool {
    owner_scope().is_some()
        && std::fs::read_to_string("/proc/self/mountinfo")
            .is_ok_and(|mountinfo| !proc_mount_is_restricted(&mountinfo))
}

/// One container that carries a [`START_LABEL`], as the sweep sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelledContainer {
    /// The container id.
    pub id: String,
    /// The value of [`START_LABEL`] (`<pid>-<counter>` for a test start).
    pub start_label: String,
    /// The value of [`OWNER_SCOPE_LABEL`], if the container has one.
    pub owner_scope: Option<String>,
}

/// The owner pid in a start label (`<pid>-<counter>`).
///
/// # Returns
/// `None` for any other shape, such as the `server-<name>` label of a server
/// started with `scripts/test-pg.sh up NAME` and no owner pid, which no
/// process owns (a server kept across runs).
pub fn owner_pid(start_label: &str) -> Option<u32> {
    let (pid, counter) = start_label.split_once('-')?;
    counter.parse::<u64>().ok()?;
    pid.parse().ok()
}

/// The ids of the containers that are safe to remove: those labelled with this
/// process's own [`owner_scope`] (same machine boot and pid namespace), whose
/// owner pid is not alive there.
///
/// A container is kept when anything is uncertain: no scope label (an
/// older start, or a platform without one), another scope (namespace or
/// machine), a label that is not `<pid>-<counter>`, or an owner that is alive.
/// A dead pid that was reused by another process keeps its container until
/// that process exits.
///
/// # Arguments
/// * `containers` - every container carrying [`START_LABEL`].
/// * `own_scope` - [`owner_scope`] of the sweeper.
/// * `may_be_alive` - `false` only for a pid known to be dead; anything
///   uncertain is `true`, which keeps the container.
pub fn stale_container_ids(
    containers: &[LabelledContainer],
    own_scope: &str,
    may_be_alive: impl Fn(u32) -> bool,
) -> Vec<String> {
    containers
        .iter()
        .filter(|container| container.owner_scope.as_deref() == Some(own_scope))
        .filter_map(|container| Some((container, owner_pid(&container.start_label)?)))
        .filter(|(_, pid)| !may_be_alive(*pid))
        .map(|(container, _)| container.id.clone())
        .collect()
}

/// Every container (running or not) that carries a [`START_LABEL`].
///
/// # Errors
/// If Docker is unreachable or refuses the listing.
pub async fn all_labelled_containers() -> Result<Vec<LabelledContainer>, DockerError> {
    let docker = Docker::connect_with_defaults()?;
    let filters = HashMap::from([("label".to_string(), vec![START_LABEL.to_string()])]);
    let containers = docker
        .list_containers(Some(ListContainersOptions {
            all: true,
            filters: Some(filters),
            ..Default::default()
        }))
        .await?;
    Ok(containers
        .into_iter()
        .filter_map(|summary| {
            let labels = summary.labels?;
            Some(LabelledContainer {
                id: summary.id?,
                start_label: labels.get(START_LABEL)?.clone(),
                owner_scope: labels.get(OWNER_SCOPE_LABEL).cloned(),
            })
        })
        .collect())
}

/// Force-removes the containers [`stale_container_ids`] picks, with their
/// anonymous volumes.
///
/// # Returns
/// How many containers this call removed; 0 where liveness cannot be trusted
/// ([`liveness_is_reliable`]).
///
/// # Errors
/// If Docker is unreachable, or refuses a removal for a reason other than the
/// container being gone already (404) or already being removed (409), as when
/// another process's sweep got there first.
pub async fn sweep_stale_containers() -> Result<usize, DockerError> {
    let Some(scope) = owner_scope().filter(|_| liveness_is_reliable()) else {
        return Ok(0);
    };
    sweep_stale_containers_of(&scope, process_may_be_alive).await
}

/// [`sweep_stale_containers`] for an explicit scope and liveness check: removes
/// the containers labelled with `own_scope` whose owner pid `may_be_alive` rules out.
/// A test passes a scope of its own, so the sweep can only match its containers.
///
/// # Errors
/// As [`sweep_stale_containers`].
pub async fn sweep_stale_containers_of(
    own_scope: &str,
    may_be_alive: impl Fn(u32) -> bool,
) -> Result<usize, DockerError> {
    let stale = stale_container_ids(&all_labelled_containers().await?, own_scope, may_be_alive);
    let mut removed = 0;
    for id in stale {
        if remove_container_by_id(&id).await? {
            removed += 1;
        }
    }
    Ok(removed)
}

/// Force-removes one container with its anonymous volumes.
///
/// # Returns
/// Whether this call removed it; `false` when it was gone already (404) or
/// already being removed (409).
///
/// # Errors
/// If Docker is unreachable or refuses the removal for another reason.
pub async fn remove_container_by_id(id: &str) -> Result<bool, DockerError> {
    let docker = Docker::connect_with_defaults()?;
    let options = RemoveContainerOptions {
        force: true,
        v: true,
        ..Default::default()
    };
    match docker.remove_container(id, Some(options)).await {
        Ok(()) => Ok(true),
        Err(DockerError::DockerResponseServerError {
            status_code: 404 | 409,
            ..
        }) => Ok(false),
        Err(error) => Err(error),
    }
}

/// Runs [`sweep_stale_containers`] on the first call in this process and
/// reports how many containers it removed. A failure is only logged: the
/// sweep is housekeeping and must not fail the test that happened to start
/// first.
async fn sweep_stale_containers_once() {
    static SWEPT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if SWEPT.swap(true, Ordering::SeqCst) {
        return;
    }
    match sweep_stale_containers().await {
        Ok(0) => {}
        Ok(removed) => {
            // Printed too: the test harness hides a passing test's tracing.
            eprintln!("swept {removed} stale test container(s) left by exited processes");
            tracing::info!(removed, "swept stale test containers");
        }
        Err(error) => tracing::warn!("sweep stale test containers: {error}"),
    }
}
