//! The `stores` table (migration `0036_stores.sql`): where each catalog
//! store's database is, read by [`crate::stores::Stores`] when it opens a
//! store's pool.
//!
//! A row holds a *reference* to a credential, never the credential itself
//! (plan Conventions, "Privacy rules"), so a row is safe to log apart from
//! the host, which the callers keep out of any reply.

use chrono::{DateTime, Utc};
use sqlx::{PgExecutor, PgPool};

use crate::error::OkfError;
use crate::invalidate::{invalidate, Invalidation};
use crate::store::audit::{self, Action, AuditActor, Entry, People, Target};
use crate::store::decode_column;
use crate::stores::{placement, StoreName, Stores};

/// How a store's credential is found (`stores.credential_kind`).
// `rename_all = "kebab-case"` makes serde read and write the spellings the
// table stores (`iam`, `secrets-manager`, `env`), for the admin API's body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CredentialKind {
    /// An RDS IAM authentication token, signed from the server's AWS
    /// credentials.
    Iam,
    /// A secret in a secrets manager, named by its ARN.
    SecretsManager,
    /// An environment variable holding the password.
    Env,
}

impl CredentialKind {
    /// The kind as stored.
    ///
    /// # Returns
    /// `"iam"`, `"secrets-manager"` or `"env"`.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "a pure match over the variants")
    )]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Iam => "iam",
            Self::SecretsManager => "secrets-manager",
            Self::Env => "env",
        }
    }

    /// Reads a stored spelling.
    ///
    /// # Arguments
    /// * `text` - the column's value.
    ///
    /// # Returns
    /// The kind, or `None` for a spelling the table's CHECK does not allow.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "a pure match over the spellings")
    )]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "iam" => Some(Self::Iam),
            "secrets-manager" => Some(Self::SecretsManager),
            "env" => Some(Self::Env),
            _ => None,
        }
    }
}

/// Where a store's database is (`stores.location_kind`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreLocation {
    /// The account database itself.
    AccountDatabase,
    /// A database on a host of its own.
    Host {
        /// The server's host name.
        host: String,
        /// Its port.
        port: u16,
        /// The database's name.
        database: String,
        /// How the credential is found.
        credential_kind: CredentialKind,
        /// What names the credential: a variable name, a secret ARN or an
        /// IAM identity. Never the credential.
        credential_ref: String,
    },
}

/// One row of `stores`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreRow {
    /// The store's name.
    pub name: StoreName,
    /// Where its database is.
    pub location: StoreLocation,
}

/// [`StoreRow`] as the query returns it: every column of the location,
/// nullable.
#[derive(sqlx::FromRow)]
struct StoreRowColumns {
    name: String,
    location_kind: String,
    host: Option<String>,
    port: Option<i32>,
    database: Option<String>,
    credential_kind: Option<String>,
    credential_ref: Option<String>,
}

impl TryFrom<StoreRowColumns> for StoreRow {
    type Error = OkfError;

    /// Checks the columns the table's CHECKs already constrain, so a
    /// hand-edited row is an error instead of a guess.
    ///
    /// # Errors
    /// [`OkfError::Database`] (a `ColumnDecode`, naming the column) for a
    /// value the table would not have accepted.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "maps a value, no I/O")
    )]
    fn try_from(columns: StoreRowColumns) -> Result<Self, Self::Error> {
        let name = decode_column(StoreName::parse(&columns.name).ok(), "stores.name")?;
        let location = match columns.location_kind.as_str() {
            "account-database" => StoreLocation::AccountDatabase,
            _ => {
                let host = decode_column(columns.host, "stores.host")?;
                let port = decode_column(
                    columns.port.and_then(|port| u16::try_from(port).ok()),
                    "stores.port",
                )?;
                let database = decode_column(columns.database, "stores.database")?;
                let credential_kind = decode_column(
                    columns
                        .credential_kind
                        .as_deref()
                        .and_then(CredentialKind::parse),
                    "stores.credential_kind",
                )?;
                let credential_ref =
                    decode_column(columns.credential_ref, "stores.credential_ref")?;
                StoreLocation::Host {
                    host,
                    port,
                    database,
                    credential_kind,
                    credential_ref,
                }
            }
        };
        Ok(Self { name, location })
    }
}

/// The row of the store called `name`.
///
/// # Arguments
/// * `executor` - the account database.
/// * `name` - the store's name.
///
/// # Returns
/// Its row, or `None` if there is no such store.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "stores.by_name"))]
pub async fn by_name<'e>(
    executor: impl PgExecutor<'e>,
    name: &StoreName,
) -> Result<Option<StoreRow>, OkfError> {
    let columns = sqlx::query_as::<_, StoreRowColumns>(
        "SELECT name, location_kind, host, port, database, credential_kind, \
             credential_ref FROM stores WHERE name = $1",
    )
    .bind(name.as_str())
    .fetch_optional(executor)
    .await?;
    columns.map(StoreRow::try_from).transpose()
}

/// The name of every store, in name order.
///
/// # Arguments
/// * `executor` - the account database.
///
/// # Returns
/// The names.
///
/// # Errors
/// [`OkfError::Database`], or a `ColumnDecode` for a stored name that is not
/// a valid store name.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "stores.names"))]
pub async fn names<'e>(executor: impl PgExecutor<'e>) -> Result<Vec<StoreName>, OkfError> {
    let names: Vec<String> = sqlx::query_scalar("SELECT name FROM stores ORDER BY name")
        .fetch_all(executor)
        .await?;
    names
        .into_iter()
        .map(|name| decode_column(StoreName::parse(&name).ok(), "stores.name"))
        .collect()
}

/// One store as the admin surface lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreSummary {
    /// The row.
    pub row: StoreRow,
    /// When it was added (server clock).
    pub created_at: DateTime<Utc>,
    /// Whether new catalogs go here unless their team is pinned elsewhere.
    pub is_default: bool,
    /// How many teams are pinned to it.
    pub pinned_teams: i64,
    /// How many catalogs it holds: the provisioned, undeleted ones, as the
    /// catalog listings count them.
    pub catalogs: i64,
}

/// [`StoreSummary`] as the query returns it.
#[derive(sqlx::FromRow)]
struct SummaryColumns {
    #[sqlx(flatten)]
    row: StoreRowColumns,
    created_at: DateTime<Utc>,
    pinned_teams: i64,
    catalogs: i64,
}

/// Every store with its creation time and pin count, in name order, and
/// which one is the default.
///
/// # Arguments
/// * `executor` - the account database.
///
/// # Returns
/// The summaries.
///
/// # Errors
/// [`OkfError::Database`], or a `ColumnDecode` for a hand-edited row.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "stores.summaries"))]
pub async fn summaries(executor: &PgPool) -> Result<Vec<StoreSummary>, OkfError> {
    let default = placement::default_store(executor).await?;
    let columns = sqlx::query_as::<_, SummaryColumns>(
        "SELECT s.name, s.location_kind, s.host, s.port, s.database, s.credential_kind, \
                s.credential_ref, s.created_at, \
                (SELECT count(*) FROM teams t WHERE t.store = s.name) AS pinned_teams, \
                (SELECT count(*) FROM catalogs c WHERE c.store = s.name \
                   AND c.provisioned_at IS NOT NULL AND c.deleted_at IS NULL) AS catalogs \
         FROM stores s ORDER BY s.name",
    )
    .fetch_all(executor)
    .await?;
    columns
        .into_iter()
        .map(|columns| {
            let row = StoreRow::try_from(columns.row)?;
            Ok(StoreSummary {
                is_default: row.name == default,
                row,
                created_at: columns.created_at,
                pinned_teams: columns.pinned_teams,
                catalogs: columns.catalogs,
            })
        })
        .collect()
}

/// Adds a store: refuses a taken name or location, checks that it can be
/// opened and is a database of its own, migrates it, then writes its row
/// (`insert`, below).
///
/// One account transaction (one connection, so a pool of one is enough) lasts
/// to the end. It takes two advisory locks (the name; the host, port and
/// database) and refuses a name or location that already has a row, so a
/// request that is bound to fail never touches the target database.
///
/// Then [`Stores::claim`] opens the target database and takes an advisory lock
/// inside it, which is held until after the registry insert commits (or
/// fails). Under that lock it refuses the account database and a database
/// whose identity names another registered store (a second host name for it),
/// replaces an identity naming a store with no row, and migrates the schema,
/// which writes the identity row. The target lock orders every add that
/// reaches one physical database, under any host spelling or name, so none
/// decides ownership while another is between its migration and its insert.
/// If the add still fails after the migration (the process dies, say), the
/// identity names a store with no row; the next `add` of that database,
/// under any name, replaces it. So a retry, or an add under a corrected name,
/// succeeds and no manual repair is needed.
///
/// # Arguments
/// * `pool` - the account database.
/// * `stores` - the registry, whose open path does the connection check
///   and the target-database claim.
/// * `row` - the store to add.
/// * `actor` - who, for the audit log.
/// * `now` - the server clock.
///
/// # Returns
/// The new store.
///
/// # Errors
/// [`OkfError::Refused`] when the store cannot be reached with these
/// settings (the reply names no host, port or credential; the cause is
/// logged locally), when it is the account database or another store's
/// database, when its schema cannot be
/// migrated, or when the name or the host, port and database is already
/// used; [`OkfError::Database`], a failed audit write included.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "stores.add"))]
pub async fn add(
    pool: &PgPool,
    stores: &Stores,
    row: StoreRow,
    actor: &AuditActor,
    now: DateTime<Utc>,
) -> Result<StoreSummary, OkfError> {
    // One account connection, for the whole add: the advisory locks, the
    // registry reads and the insert all use it, so a pool of one is enough
    // and concurrent adds never wait on each other's second connection.
    let mut tx = pool.begin().await?;
    refuse_known_conflicts(&mut tx, &row).await?;
    let claim = stores
        .claim(row.clone(), &mut tx)
        .await
        .map_err(unreachable_refused)?;
    let added = insert_in(&mut tx, row, actor, now).await;
    let committed = match added {
        Ok(summary) => tx.commit().await.map(|()| summary).map_err(OkfError::from),
        Err(error) => Err(error),
    };
    // After the commit (or the rollback, as `tx` drops): the next add of
    // this database sees the registration.
    claim.release().await;
    committed
}

/// Takes this add's two advisory locks on `reservation` (one on the name,
/// one on the host, port and database, always in that order) and refuses a
/// name or a location that already has a row, before the target database is
/// touched. Another add of the same name or the same database waits here,
/// then finds the winner's row and is refused without a side effect.
///
/// # Errors
/// [`OkfError::Refused`] for a name or location already in use;
/// [`OkfError::Database`] for a failed read.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "stores.refuse_known_conflicts"))]
async fn refuse_known_conflicts(
    reservation: &mut sqlx::PgConnection,
    row: &StoreRow,
) -> Result<(), OkfError> {
    let StoreLocation::Host {
        host,
        port,
        database,
        ..
    } = &row.location
    else {
        // `insert` refuses it.
        return Ok(());
    };
    for key in [
        format!("okf.store-add.name:{}", row.name),
        format!(
            "okf.store-add.location:{}:{port}/{database}",
            host.to_lowercase()
        ),
    ] {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(key)
            .execute(&mut *reservation)
            .await?;
    }
    let (name_taken, location_taken): (bool, bool) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM stores WHERE name = $1), \
                EXISTS (SELECT 1 FROM stores WHERE lower(host) = lower($2) AND port = $3 \
                        AND database = $4)",
    )
    .bind(row.name.as_str())
    .bind(host)
    .bind(i32::from(*port))
    .bind(database)
    .fetch_one(&mut *reservation)
    .await?;
    if name_taken {
        return Err(OkfError::Refused {
            reason: format!("store {} already exists", row.name),
        });
    }
    if location_taken {
        return Err(OkfError::Refused {
            reason: "another store already uses that host, port and database".to_string(),
        });
    }
    Ok(())
}

/// A store that could not be opened, as [`add`] refuses it: without the
/// host, port or credential.
///
/// # Arguments
/// * `error` - what the open returned.
///
/// # Returns
/// [`OkfError::Refused`] for `StoreUnavailable`, anything else unchanged.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(uninstrumented_fn, reason = "a pure mapping of one error variant")
)]
fn unreachable_refused(error: OkfError) -> OkfError {
    match error {
        OkfError::StoreUnavailable { .. } => OkfError::Refused {
            reason: "the store cannot be reached with these settings; check the host, port, \
                     database and credential"
                .to_string(),
        },
        other => other,
    }
}

/// Writes a store's row, records `store.added` and sends the store
/// invalidation, in one transaction: [`add`] without its checks.
///
/// # Arguments
/// * `pool` - the account database.
/// * `row` - the store, which must be a `host` store.
/// * `actor` - who, for the audit log.
/// * `now` - the server clock.
///
/// # Errors
/// [`OkfError::Refused`] for the account-database location or a name or
/// host, port and database already in use; [`OkfError::Database`], a failed
/// audit write included.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "stores.insert"))]
async fn insert_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    row: StoreRow,
    actor: &AuditActor,
    now: DateTime<Utc>,
) -> Result<StoreSummary, OkfError> {
    let StoreLocation::Host {
        host,
        port,
        database,
        credential_kind,
        credential_ref,
    } = &row.location
    else {
        // Only `main` is inside the account database, and it exists.
        return Err(OkfError::Refused {
            reason: "the account database already has its store, main".to_string(),
        });
    };
    audit::lock_people(&mut *tx, actor, &People::default()).await?;
    let inserted = sqlx::query(
        "INSERT INTO stores (name, location_kind, host, port, database, credential_kind, \
                             credential_ref, created_at) \
         VALUES ($1, 'host', $2, $3, $4, $5, $6, $7)",
    )
    .bind(row.name.as_str())
    .bind(host)
    .bind(i32::from(*port))
    .bind(database)
    .bind(credential_kind.as_str())
    .bind(credential_ref)
    .bind(now)
    .execute(&mut **tx)
    .await;
    if let Err(error) = inserted {
        return Err(
            match error.as_database_error().and_then(|db| db.constraint()) {
                Some("stores_pkey") => OkfError::Refused {
                    reason: format!("store {} already exists", row.name),
                },
                Some("stores_one_row_per_database") => OkfError::Refused {
                    reason: "another store already uses that host, port and database".to_string(),
                },
                _ => error.into(),
            },
        );
    }
    audit::record(
        &mut *tx,
        actor,
        &Entry {
            action: Action::StoreAdded,
            target: Target::Store(row.name.as_str()),
            person: None,
            teams: &[],
            // Never the host or the credential reference.
            details: serde_json::json!({
                "location": "host",
                "credential_kind": credential_kind.as_str(),
            }),
        },
        now,
    )
    .await?;
    invalidate(&mut *tx, &[Invalidation::store(&row.name)]).await?;
    Ok(StoreSummary {
        is_default: false,
        row,
        created_at: now,
        pinned_teams: 0,
        catalogs: 0,
    })
}

/// [`insert_in`] in a transaction of its own: for tests of the row alone.
///
/// # Errors
/// As for [`insert_in`].
#[cfg(test)]
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(uninstrumented_fn, reason = "a test wrapper")
)]
async fn insert(
    pool: &PgPool,
    row: StoreRow,
    actor: &AuditActor,
    now: DateTime<Utc>,
) -> Result<StoreSummary, OkfError> {
    let mut tx = pool.begin().await?;
    let summary = insert_in(&mut tx, row, actor, now).await?;
    tx.commit().await?;
    Ok(summary)
}

/// Makes `name` the store new catalogs go on (unless their team is pinned),
/// records `setting.default_store_changed` and sends the settings
/// invalidation, in one transaction. Catalogs that exist stay where they
/// are. The same store again is no change: nothing is written or recorded.
///
/// Lock order (`tools/agentic/lessons.md`): the actor's user rows, the
/// store's row (`FOR SHARE`, so it cannot go while it is the default), the
/// generation row, then the setting.
///
/// # Arguments
/// * `pool` - the account database.
/// * `name` - the store.
/// * `actor` - who, for the audit log.
/// * `now` - the server clock.
///
/// # Errors
/// [`OkfError::UnknownName`] for a store with no row; [`OkfError::Database`],
/// a failed audit write included.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "stores.set_default"))]
pub async fn set_default(
    pool: &PgPool,
    name: &StoreName,
    actor: &AuditActor,
    now: DateTime<Utc>,
) -> Result<(), OkfError> {
    let mut tx = pool.begin().await?;
    audit::lock_people(&mut tx, actor, &People::default()).await?;
    lock_store(&mut tx, name).await?;
    invalidate(&mut tx, &[Invalidation::Settings]).await?;
    let previous: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT value FROM server_settings WHERE key = $1 FOR UPDATE")
            .bind(placement::DEFAULT_STORE_KEY)
            .fetch_optional(&mut *tx)
            .await?;
    let json = serde_json::json!(name.as_str());
    if previous.as_ref() == Some(&json) {
        // Unchanged: dropping `tx` rolls the generation bump back, and its
        // `NOTIFY` is never sent.
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO server_settings (key, value, updated_at, updated_by) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, \
             updated_at = EXCLUDED.updated_at, updated_by = EXCLUDED.updated_by",
    )
    .bind(placement::DEFAULT_STORE_KEY)
    .bind(&json)
    .bind(now)
    .bind(crate::store::settings::updated_by(actor))
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        actor,
        &Entry {
            action: Action::SettingDefaultStoreChanged,
            target: Target::Setting(placement::DEFAULT_STORE_KEY),
            person: None,
            teams: &[],
            details: serde_json::json!({"from": previous, "to": json}),
        },
        now,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Locks a store's row `FOR SHARE`, so a change that names it cannot race
/// the row's removal.
///
/// # Errors
/// [`OkfError::UnknownName`] for a store with no row; [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "stores.lock_store"))]
pub(crate) async fn lock_store(
    conn: &mut sqlx::PgConnection,
    name: &StoreName,
) -> Result<(), OkfError> {
    let found: Option<i32> = sqlx::query_scalar("SELECT 1 FROM stores WHERE name = $1 FOR SHARE")
        .bind(name.as_str())
        .fetch_optional(&mut *conn)
        .await?;
    found.map(|_| ()).ok_or_else(|| OkfError::UnknownName {
        noun: "store",
        name: name.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::invalidate::testing::invalidations_of;

    #[tokio::test]
    async fn main_is_the_account_database_row() {
        let db = crate::db::test_db().await;
        crate::db::main_in_the_account_database(&db.pool).await;

        let row = by_name(&db.pool, &StoreName::main())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(row.name, StoreName::main());
        assert_eq!(row.location, StoreLocation::AccountDatabase);
        assert_eq!(names(&db.pool).await.unwrap(), [StoreName::main()]);
        let unknown = StoreName::parse("shared-1").unwrap();
        assert_eq!(by_name(&db.pool, &unknown).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_host_row_reads_back_with_its_credential_reference() {
        let db = crate::db::test_db().await;
        sqlx::query(
            "INSERT INTO stores (name, location_kind, host, port, database, \
             credential_kind, credential_ref, created_at) \
             VALUES ('shared-1', 'host', 'db.example.internal', 5432, 'okf', 'env', \
             'SHARED_1_PASSWORD', now())",
        )
        .execute(&db.pool)
        .await
        .unwrap();

        let name = StoreName::parse("shared-1").unwrap();
        let row = by_name(&db.pool, &name).await.unwrap().unwrap();

        assert_eq!(
            row.location,
            StoreLocation::Host {
                host: "db.example.internal".to_string(),
                port: 5432,
                database: "okf".to_string(),
                credential_kind: CredentialKind::Env,
                credential_ref: "SHARED_1_PASSWORD".to_string(),
            }
        );
        assert_eq!(names(&db.pool).await.unwrap(), [StoreName::main(), name]);
    }

    /// Registry settings for these tests: a short connect timeout.
    fn pool_settings() -> crate::stores::PoolSettings {
        crate::stores::PoolSettings {
            max_connections: 2,
            idle_timeout: std::time::Duration::from_secs(60),
            connect_timeout: std::time::Duration::from_secs(2),
        }
    }

    /// A `host` row for store `name` that points at `host:port/database`,
    /// with an `env` credential in `STORE_PASSWORD`.
    fn host_row(name: &str, host: &str, port: u16, database: &str) -> StoreRow {
        StoreRow {
            name: StoreName::parse(name).unwrap(),
            location: StoreLocation::Host {
                host: host.to_string(),
                port,
                database: database.to_string(),
                credential_kind: CredentialKind::Env,
                credential_ref: "STORE_PASSWORD".to_string(),
            },
        }
    }

    /// A registry whose environment holds the test database's password, and
    /// a row for store `name` that reaches the test database itself.
    fn reachable(db: &crate::db::pg_support::TestDb, name: &str) -> (Stores, StoreRow) {
        reachable_on(db, db, name)
    }

    /// A registry on `db`'s account database whose environment holds
    /// `target`'s password, and a row for store `name` that reaches `target`
    /// as `host`.
    fn reachable_as(
        db: &crate::db::pg_support::TestDb,
        target: &crate::db::pg_support::TestDb,
        name: &str,
        host: &str,
    ) -> (Stores, StoreRow) {
        let (stores, row) = reachable_on(db, target, name);
        let StoreLocation::Host { port, database, .. } = &row.location else {
            unreachable!("a host row")
        };
        let row = host_row(name, host, *port, database);
        (stores, row)
    }

    /// A registry on `db`'s account database whose environment holds
    /// `target`'s password, and a row for store `name` that reaches `target`.
    fn reachable_on(
        db: &crate::db::pg_support::TestDb,
        target: &crate::db::pg_support::TestDb,
        name: &str,
    ) -> (Stores, StoreRow) {
        let after_scheme = target.url.split("://").nth(1).expect("a URL");
        let password = after_scheme
            .split('@')
            .next()
            .and_then(|credentials| credentials.split(':').nth(1))
            .unwrap_or_default()
            .to_string();
        let stores = Stores::with_environment(
            db.pool.clone(),
            pool_settings(),
            std::sync::Arc::new(move |variable| {
                (variable == "STORE_PASSWORD").then(|| password.clone())
            }),
        );
        let options = target.pool.connect_options();
        let row = host_row(
            name,
            options.get_host(),
            options.get_port(),
            options.get_database().expect("a database"),
        );
        (stores, row)
    }

    async fn store_names(pool: &PgPool) -> Vec<String> {
        names(pool)
            .await
            .unwrap()
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn adding_an_unreachable_store_is_refused() {
        let db = crate::db::test_db().await;
        // A port held but never listened on: every connect is refused.
        let refusing = crate::test_net::Refused::new();
        let closed = refusing.port();
        let (stores, _) = reachable(&db, "shared-1");
        let row = host_row("shared-1", "127.0.0.1", closed, "okf");

        let refused = crate::test_net::bounded(
            "adding a store at a refusing address",
            add(&db.pool, &stores, row, &AuditActor::cli(), Utc::now()),
        )
        .await
        .unwrap_err();

        assert!(matches!(refused, OkfError::Refused { .. }), "{refused:?}");
        // The reply names no host, port or credential.
        let reply = refused.to_json().to_string();
        for private in ["127.0.0.1", &closed.to_string(), "STORE_PASSWORD"] {
            assert!(!reply.contains(private), "{private} leaked into {reply}");
        }
        assert_eq!(store_names(&db.pool).await, ["main"]);
        assert!(audit::testing::entries(&db.pool, "store.added")
            .await
            .is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_host_store_can_be_added() {
        let db = crate::db::test_db().await;
        // An empty database of its own: adding the store migrates it.
        let target = crate::db::pg_support::TestDb::start().await;
        let (stores, row) = reachable_on(&db, &target, "shared-1");

        let added = add(&db.pool, &stores, row, &AuditActor::cli(), Utc::now())
            .await
            .unwrap();

        assert_eq!(added.row.name.as_str(), "shared-1");
        assert_eq!(store_names(&db.pool).await, ["main", "shared-1"]);
        // The new store's database was migrated to the embedded versions and
        // answers to its name.
        let mut connection = target.pool.acquire().await.unwrap();
        assert_eq!(
            crate::db::store_owner(&mut connection).await.unwrap(),
            Some("shared-1".to_string())
        );
        let applied: Vec<i64> =
            sqlx::query_scalar("SELECT version FROM catalog._sqlx_migrations ORDER BY version")
                .fetch_all(&mut *connection)
                .await
                .unwrap();
        let embedded: Vec<i64> = crate::db::CATALOG_MIGRATOR
            .iter()
            .filter(|migration| !migration.migration_type.is_down_migration())
            .map(|migration| migration.version)
            .collect();
        assert_eq!(applied, embedded);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_store_whose_schema_cannot_be_migrated_leaves_no_row() {
        let db = crate::db::test_db().await;
        let target = crate::db::pg_support::TestDb::start().await;
        // A table the baseline migration creates (not `catalog_state`, whose
        // presence would be taken for an adoptable schema), already there with another
        // shape: the migration fails part way.
        sqlx::query("CREATE SCHEMA catalog")
            .execute(&target.pool)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE catalog.documents (unrelated int)")
            .execute(&target.pool)
            .await
            .unwrap();
        let (stores, row) = reachable_on(&db, &target, "shared-1");

        let refused = add(&db.pool, &stores, row, &AuditActor::cli(), Utc::now())
            .await
            .unwrap_err();

        assert!(
            matches!(&refused, OkfError::Refused { reason } if reason.contains("migrated")),
            "{refused:?}"
        );
        assert_eq!(store_names(&db.pool).await, ["main"]);
    }

    /// The identity row of `target`'s database, if it has one.
    async fn owner_of(target: &crate::db::pg_support::TestDb) -> Option<String> {
        let mut connection = target.pool.acquire().await.unwrap();
        crate::db::store_owner(&mut connection).await.unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_taken_name_is_refused_before_the_database_is_claimed() {
        let db = crate::db::test_db().await;
        let fresh = crate::db::pg_support::TestDb::start().await;
        let (_, taken) = reachable(&db, "shared-1");
        insert(&db.pool, taken, &AuditActor::cli(), Utc::now())
            .await
            .unwrap();
        let (stores, row) = reachable_on(&db, &fresh, "shared-1");

        let refused = add(&db.pool, &stores, row, &AuditActor::cli(), Utc::now())
            .await
            .unwrap_err();

        assert!(
            matches!(&refused, OkfError::Refused { reason } if reason.contains("already exists")),
            "{refused:?}"
        );
        assert_eq!(owner_of(&fresh).await, None, "the database was claimed");
        // The corrected name takes the database.
        let (stores, row) = reachable_on(&db, &fresh, "shared-2");
        add(&db.pool, &stores, row, &AuditActor::cli(), Utc::now())
            .await
            .unwrap();
        assert_eq!(owner_of(&fresh).await.as_deref(), Some("shared-2"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_taken_location_is_refused_before_the_database_is_claimed() {
        let db = crate::db::test_db().await;
        let target = crate::db::pg_support::TestDb::start().await;
        // Registered, but never opened: its identity row does not exist yet.
        let (stores, first) = reachable_on(&db, &target, "shared-1");
        insert(&db.pool, first, &AuditActor::cli(), Utc::now())
            .await
            .unwrap();
        let (_, second) = reachable_on(&db, &target, "shared-2");

        let refused = add(&db.pool, &stores, second, &AuditActor::cli(), Utc::now())
            .await
            .unwrap_err();

        assert!(
            matches!(&refused, OkfError::Refused { reason } if reason.contains("host, port")),
            "{refused:?}"
        );
        assert_eq!(owner_of(&target).await, None, "the database was claimed");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_database_claimed_by_an_unregistered_store_can_be_added_again() {
        let db = crate::db::test_db().await;
        let target = crate::db::pg_support::TestDb::start().await;
        // An add that died after its migration: the identity names a store
        // with no row.
        let (stores, ghost) = reachable_on(&db, &target, "ghost");
        let mut registry = db.pool.acquire().await.unwrap();
        stores
            .claim(ghost, &mut registry)
            .await
            .unwrap()
            .release()
            .await;
        assert_eq!(owner_of(&target).await.as_deref(), Some("ghost"));
        let (_, row) = reachable_on(&db, &target, "shared-1");

        add(&db.pool, &stores, row, &AuditActor::cli(), Utc::now())
            .await
            .unwrap();

        assert_eq!(store_names(&db.pool).await, ["main", "shared-1"]);
        assert_eq!(owner_of(&target).await.as_deref(), Some("shared-1"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_adds_of_one_name_leave_the_loser_database_unclaimed() {
        let db = crate::db::test_db().await;
        let first = crate::db::pg_support::TestDb::start().await;
        let second = crate::db::pg_support::TestDb::start().await;
        let (stores, one) = reachable_on(&db, &first, "shared-1");
        let (_, two) = reachable_on(&db, &second, "shared-1");
        let actor = AuditActor::cli();

        let (one, two) = tokio::join!(
            add(&db.pool, &stores, one, &actor, Utc::now()),
            add(&db.pool, &stores, two, &actor, Utc::now()),
        );

        // Exactly one wins, whichever the scheduler ran first.
        let (winner, loser) = match (one, two) {
            (Ok(_), Err(_)) => (&first, &second),
            (Err(_), Ok(_)) => (&second, &first),
            (one, two) => panic!("expected one winner: {one:?} {two:?}"),
        };
        assert_eq!(owner_of(winner).await.as_deref(), Some("shared-1"));
        assert_eq!(owner_of(loser).await, None, "the loser was claimed");
        assert_eq!(store_names(&db.pool).await, ["main", "shared-1"]);
    }

    /// A registry on a pool of `max` account connections (a short acquire
    /// timeout, so starvation fails the test instead of hanging it), whose
    /// environment holds the test database's password.
    async fn bounded(db: &crate::db::pg_support::TestDb, max: u32) -> (PgPool, Stores) {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(max)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect(&db.url)
            .await
            .unwrap();
        let after_scheme = db.url.split("://").nth(1).expect("a URL");
        let password = after_scheme
            .split('@')
            .next()
            .and_then(|credentials| credentials.split(':').nth(1))
            .unwrap_or_default()
            .to_string();
        let stores = Stores::with_environment(
            pool.clone(),
            pool_settings(),
            std::sync::Arc::new(move |variable| {
                (variable == "STORE_PASSWORD").then(|| password.clone())
            }),
        );
        (pool, stores)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_store_can_be_added_on_a_one_connection_account_pool() {
        let db = crate::db::test_db().await;
        let target = crate::db::pg_support::TestDb::start().await;
        let (pool, stores) = bounded(&db, 1).await;
        let (_, row) = reachable_on(&db, &target, "shared-1");

        add(&pool, &stores, row, &AuditActor::cli(), Utc::now())
            .await
            .unwrap();

        assert_eq!(store_names(&db.pool).await, ["main", "shared-1"]);
        assert_eq!(owner_of(&target).await.as_deref(), Some("shared-1"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_adds_on_a_bounded_account_pool_all_succeed() {
        let db = crate::db::test_db().await;
        let (pool, stores) = bounded(&db, 2).await;
        let mut targets = Vec::new();
        let mut adds = tokio::task::JoinSet::new();
        for index in 0..4 {
            let target = crate::db::pg_support::TestDb::start().await;
            let (_, row) = reachable_on(&db, &target, &format!("shared-{index}"));
            targets.push(target);
            let (pool, stores) = (pool.clone(), stores.clone());
            adds.spawn(
                async move { add(&pool, &stores, row, &AuditActor::cli(), Utc::now()).await },
            );
        }

        while let Some(added) = adds.join_next().await {
            added.unwrap().unwrap();
        }

        assert_eq!(store_names(&db.pool).await.len(), 5);
        for (index, target) in targets.iter().enumerate() {
            assert_eq!(owner_of(target).await, Some(format!("shared-{index}")),);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_add_waits_while_another_holds_the_target_database() {
        let db = crate::db::test_db().await;
        let target = crate::db::pg_support::TestDb::start().await;
        let (stores, row) = reachable_on(&db, &target, "shared-1");
        // Another add of this database is mid-way: it holds the target lock.
        let mut holder = target.pool.acquire().await.unwrap();
        sqlx::query("SELECT pg_advisory_lock(hashtextextended($1, 0))")
            .bind(crate::db::STORE_ADD_LOCK_KEY)
            .execute(&mut *holder)
            .await
            .unwrap();
        let actor = AuditActor::cli();
        let pending = add(&db.pool, &stores, row.clone(), &actor, Utc::now());
        tokio::pin!(pending);

        let blocked =
            tokio::time::timeout(std::time::Duration::from_millis(500), &mut pending).await;

        assert!(blocked.is_err(), "the add did not wait for the lock");
        assert_eq!(owner_of(&target).await, None);
        sqlx::query("SELECT pg_advisory_unlock(hashtextextended($1, 0))")
            .bind(crate::db::STORE_ADD_LOCK_KEY)
            .execute(&mut *holder)
            .await
            .unwrap();
        pending.await.unwrap();
        assert_eq!(owner_of(&target).await.as_deref(), Some("shared-1"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_alias_adds_of_one_database_register_exactly_one() {
        let db = crate::db::test_db().await;
        for round in 0..5 {
            let target = crate::db::pg_support::TestDb::start().await;
            let (stores, one) = reachable_as(&db, &target, "shared-1", "localhost");
            let (_, two) = reachable_as(&db, &target, "shared-2", "127.0.0.1");
            let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
            let run = |row: StoreRow| {
                let (pool, stores, barrier) = (db.pool.clone(), stores.clone(), barrier.clone());
                tokio::spawn(async move {
                    barrier.wait().await;
                    add(&pool, &stores, row, &AuditActor::cli(), Utc::now()).await
                })
            };
            let (first, second) = (run(one), run(two));
            let (first, second) = (first.await.unwrap(), second.await.unwrap());

            let winners = [&first, &second]
                .iter()
                .filter(|added| added.is_ok())
                .count();
            assert_eq!(winners, 1, "round {round}: {first:?} {second:?}");
            let registered: Vec<String> = store_names(&db.pool)
                .await
                .into_iter()
                .filter(|name| name != "main")
                .collect();
            let [name] = registered.as_slice() else {
                panic!("round {round}: {registered:?}")
            };
            assert_eq!(owner_of(&target).await.as_deref(), Some(name.as_str()));
            sqlx::query("DELETE FROM stores WHERE name <> 'main'")
                .execute(&db.pool)
                .await
                .unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn adding_the_account_database_as_a_host_store_is_refused() {
        let db = crate::db::test_db().await;
        // A `host` row that reaches the account database itself.
        let (stores, row) = reachable(&db, "shared-1");

        let refused = add(&db.pool, &stores, row, &AuditActor::cli(), Utc::now())
            .await
            .unwrap_err();

        assert!(
            matches!(&refused, OkfError::Refused { reason } if reason.contains("account database")),
            "{refused:?}"
        );
        assert_eq!(store_names(&db.pool).await, ["main"]);
        assert!(audit::testing::entries(&db.pool, "store.added")
            .await
            .is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn adding_a_store_that_aliases_another_is_refused() {
        let db = crate::db::test_db().await;
        let target = crate::db::pg_support::TestDb::start().await;
        // Store `shared-1` is registered on the target and has been opened,
        // so the target's identity row names it.
        let (stores, first) = reachable_on(&db, &target, "shared-1");
        insert(&db.pool, first.clone(), &AuditActor::cli(), Utc::now())
            .await
            .unwrap();
        stores.get(&first.name).await.unwrap();
        // The same server under a second host name, which the unique index on
        // host, port and database cannot see.
        let StoreLocation::Host { host, .. } = &first.location else {
            unreachable!("a host row")
        };
        let alias = match host.as_str() {
            "localhost" => "127.0.0.1",
            _ => "localhost",
        };
        let (_, second) = reachable_as(&db, &target, "shared-2", alias);

        let refused = add(&db.pool, &stores, second, &AuditActor::cli(), Utc::now())
            .await
            .unwrap_err();

        assert!(
            matches!(&refused, OkfError::Refused { reason }
                if reason.contains("already belongs to store shared-1")),
            "{refused:?}"
        );
        assert_eq!(store_names(&db.pool).await, ["main", "shared-1"]);
        let mut connection = target.pool.acquire().await.unwrap();
        assert_eq!(
            crate::db::store_owner(&mut connection)
                .await
                .unwrap()
                .as_deref(),
            Some("shared-1")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_store_row_is_refused_for_a_taken_name_or_database() {
        let db = crate::db::test_db().await;
        let (_, row) = reachable(&db, "shared-1");
        insert(&db.pool, row.clone(), &AuditActor::cli(), Utc::now())
            .await
            .unwrap();

        let same_database = insert(
            &db.pool,
            StoreRow {
                name: StoreName::parse("shared-2").unwrap(),
                ..row.clone()
            },
            &AuditActor::cli(),
            Utc::now(),
        )
        .await
        .unwrap_err();
        let same_name = insert(
            &db.pool,
            host_row("shared-1", "db.example.internal", 5432, "okf"),
            &AuditActor::cli(),
            Utc::now(),
        )
        .await
        .unwrap_err();
        let main = insert(
            &db.pool,
            StoreRow {
                name: StoreName::parse("elsewhere").unwrap(),
                location: StoreLocation::AccountDatabase,
            },
            &AuditActor::cli(),
            Utc::now(),
        )
        .await
        .unwrap_err();

        for refused in [same_database, same_name, main] {
            assert!(matches!(refused, OkfError::Refused { .. }), "{refused:?}");
        }
        assert_eq!(store_names(&db.pool).await, ["main", "shared-1"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn every_store_change_is_audited() {
        let db = crate::db::test_db().await;
        let (_, row) = reachable(&db, "shared-1");
        let name = row.name.clone();
        let actor = AuditActor::cli();

        insert(&db.pool, row, &actor, Utc::now()).await.unwrap();
        set_default(&db.pool, &name, &actor, Utc::now())
            .await
            .unwrap();
        crate::store::teams::set_store(
            &db.pool,
            crate::db::TEST_TEAM,
            Some(&name),
            &actor,
            Utc::now(),
        )
        .await
        .unwrap();
        crate::store::teams::set_store(&db.pool, crate::db::TEST_TEAM, None, &actor, Utc::now())
            .await
            .unwrap();

        let added = audit::testing::entries(&db.pool, "store.added").await;
        let default = audit::testing::entries(&db.pool, "setting.default_store_changed").await;
        let pinned = audit::testing::entries(&db.pool, "team.store_pinned").await;
        let unpinned = audit::testing::entries(&db.pool, "team.store_unpinned").await;
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].target_kind, "store");
        assert_eq!(added[0].target.as_deref(), Some("shared-1"));
        // The details name neither the host nor the credential reference.
        assert_eq!(
            added[0].details,
            serde_json::json!({"location": "host", "credential_kind": "env"})
        );
        assert_eq!(default.len(), 1);
        assert_eq!(default[0].target.as_deref(), Some("default_store"));
        assert_eq!(
            default[0].details,
            serde_json::json!({"from": "main", "to": "shared-1"})
        );
        assert_eq!(pinned.len(), 1);
        assert_eq!(
            pinned[0].details,
            serde_json::json!({"from": null, "to": "shared-1"})
        );
        assert_eq!(unpinned.len(), 1);
        assert_eq!(
            unpinned[0].details,
            serde_json::json!({"from": "shared-1", "to": null})
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_store_change_reaches_every_replica_through_the_change_feed() {
        let db = crate::db::test_db().await;
        let (_, row) = reachable(&db, "shared-1");
        let name = row.name.clone();
        let actor = AuditActor::cli();

        let added = invalidations_of(&db.pool, async {
            insert(&db.pool, row, &actor, Utc::now()).await.unwrap();
        })
        .await;
        let default = invalidations_of(&db.pool, async {
            set_default(&db.pool, &name, &actor, Utc::now())
                .await
                .unwrap();
        })
        .await;
        let pinned = invalidations_of(&db.pool, async {
            crate::store::teams::set_store(
                &db.pool,
                crate::db::TEST_TEAM,
                Some(&name),
                &actor,
                Utc::now(),
            )
            .await
            .unwrap();
        })
        .await;

        assert_eq!(added, [Invalidation::store(&name)]);
        assert_eq!(default, [Invalidation::Settings]);
        assert_eq!(pinned.len(), 1);
        assert!(matches!(pinned[0], Invalidation::Team { .. }), "{pinned:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unchanged_default_or_pin_records_and_sends_nothing() {
        let db = crate::db::test_db().await;
        let actor = AuditActor::cli();

        let sent = invalidations_of(&db.pool, async {
            set_default(&db.pool, &StoreName::main(), &actor, Utc::now())
                .await
                .unwrap();
            let changed = crate::store::teams::set_store(
                &db.pool,
                crate::db::TEST_TEAM,
                None,
                &actor,
                Utc::now(),
            )
            .await
            .unwrap();
            assert!(!changed);
        })
        .await;

        assert!(sent.is_empty(), "{sent:?}");
        for action in [
            "setting.default_store_changed",
            "team.store_unpinned",
            "team.store_pinned",
        ] {
            assert!(audit::testing::entries(&db.pool, action).await.is_empty());
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unknown_store_or_team_is_refused_and_changes_nothing() {
        let db = crate::db::test_db().await;
        let actor = AuditActor::cli();
        let nowhere = StoreName::parse("nowhere").unwrap();

        let default = set_default(&db.pool, &nowhere, &actor, Utc::now())
            .await
            .unwrap_err();
        let pin = crate::store::teams::set_store(
            &db.pool,
            crate::db::TEST_TEAM,
            Some(&nowhere),
            &actor,
            Utc::now(),
        )
        .await
        .unwrap_err();
        let team = crate::store::teams::set_store(
            &db.pool,
            "no-such-team",
            Some(&StoreName::main()),
            &actor,
            Utc::now(),
        )
        .await
        .unwrap_err();

        for refused in [default, pin, team] {
            assert!(
                matches!(refused, OkfError::UnknownName { .. }),
                "{refused:?}"
            );
        }
        assert_eq!(
            placement::default_store(&db.pool).await.unwrap(),
            StoreName::main()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_listing_marks_the_default_and_counts_pins() {
        let db = crate::db::test_db().await;
        let (_, row) = reachable(&db, "shared-1");
        let name = row.name.clone();
        let actor = AuditActor::cli();
        insert(&db.pool, row, &actor, Utc::now()).await.unwrap();
        set_default(&db.pool, &name, &actor, Utc::now())
            .await
            .unwrap();
        crate::store::teams::set_store(
            &db.pool,
            crate::db::TEST_TEAM,
            Some(&name),
            &actor,
            Utc::now(),
        )
        .await
        .unwrap();

        let listed = summaries(&db.pool).await.unwrap();

        let shape: Vec<_> = listed
            .iter()
            .map(|summary| {
                (
                    summary.row.name.to_string(),
                    summary.is_default,
                    summary.pinned_teams,
                )
            })
            .collect();
        assert_eq!(
            shape,
            [
                ("main".to_string(), false, 0),
                ("shared-1".to_string(), true, 1)
            ]
        );
    }

    #[test]
    fn credential_kinds_round_trip_through_their_spelling() {
        for kind in [
            CredentialKind::Iam,
            CredentialKind::SecretsManager,
            CredentialKind::Env,
        ] {
            assert_eq!(CredentialKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(CredentialKind::parse("password"), None);
    }
}
