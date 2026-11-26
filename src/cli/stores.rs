//! `okf-mcp stores ...`: the catalog stores, the default store and team pins
//! (catalog stores design, "Placement and the admin surface"). Each handler
//! returns the text to print and calls the same store function as its REST
//! route (`src/http/api/stores.rs`).

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use super::StoresCommand;
use crate::clock::format_timestamp;
use crate::store::audit::AuditActor;
use crate::store::stores::{self, StoreLocation, StoreRow, StoreSummary};
use crate::store::teams;
use crate::stores::Stores;

/// Runs `okf-mcp stores ...`.
///
/// # Arguments
/// * `pool` - the migrated account database.
/// * `stores` - the registry, whose open path `add` checks a store with.
/// * `command` - which subcommand.
/// * `now` - the time a change is recorded at.
///
/// # Returns
/// The text to print.
///
/// # Errors
/// A store that cannot be reached, is unknown, or a
/// database error.
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn stores(
    pool: &PgPool,
    stores: &Stores,
    command: StoresCommand,
    now: DateTime<Utc>,
) -> anyhow::Result<String> {
    // The command line has no principal: the operating-system user is the
    // actor (`AuditActor::cli`).
    let actor = AuditActor::cli();
    Ok(match command {
        StoresCommand::List => stores::summaries(pool)
            .await?
            .iter()
            .map(|summary| format_store(summary) + "\n")
            .collect(),
        StoresCommand::Add {
            name,
            host,
            port,
            database,
            credential_kind,
            credential_ref,
        } => {
            let row = StoreRow {
                name: name.clone(),
                location: StoreLocation::Host {
                    host,
                    port,
                    database,
                    credential_kind,
                    credential_ref,
                },
            };
            stores::add(pool, stores, row, &actor, now).await?;
            format!("added store {name}\n")
        }
        StoresCommand::SetDefault { store } => {
            stores::set_default(pool, &store, &actor, now).await?;
            format!(
                "new catalogs now go on store {store} (existing catalogs stay where they are)\n"
            )
        }
        StoresCommand::Pin { team, store } => {
            teams::set_store(pool, &team, Some(&store), &actor, now).await?;
            format!("pinned team {team} to store {store}\n")
        }
        StoresCommand::Unpin { team } => {
            teams::set_store(pool, &team, None, &actor, now).await?;
            format!("unpinned team {team}\n")
        }
        StoresCommand::Migrate => migrate_all(stores).await?,
    })
}

/// `stores migrate`: migrates every store in name order, one at a time, and
/// reports each one on its own line. A failure is reported and the next
/// store still runs.
///
/// # Arguments
/// * `stores` - the registry (fresh in this process, so each store's first
///   open migrates it).
///
/// # Returns
/// The report, when every store migrated.
///
/// # Errors
/// The report and how many stores failed, when any did; the registry's
/// error if the store names cannot be read.
#[tracing::instrument(level = "debug", skip_all)]
async fn migrate_all(stores: &Stores) -> anyhow::Result<String> {
    let names = stores.all().await?;
    let mut report = String::new();
    let mut failed = 0_usize;
    for name in &names {
        // The detail of a failure (host, driver error) is in the local log;
        // the line says only what the error variant says.
        let line = match stores.migration(name).await {
            Ok(migration) => format!("{name}\t{migration}\n"),
            Err(error) => {
                failed += 1;
                format!("{name}\tfailed: {error} (the log has the cause)\n")
            }
        };
        report.push_str(&line);
    }
    if failed > 0 {
        anyhow::bail!(
            "{report}{failed} of {} stores could not be migrated",
            names.len()
        );
    }
    Ok(report)
}

/// One `stores list` line, tab-separated: name, `default` or `-`, where it
/// is, the credential's kind and reference (never the credential), how many
/// teams are pinned to it, and when it was added.
///
/// # Arguments
/// * `summary` - the store.
///
/// # Returns
/// The line, without a newline.
#[tracing::instrument(level = "trace", skip_all)]
fn format_store(summary: &StoreSummary) -> String {
    let (place, credential) = match &summary.row.location {
        StoreLocation::AccountDatabase => ("account database".to_string(), "-".to_string()),
        StoreLocation::Host {
            host,
            port,
            database,
            credential_kind,
            credential_ref,
        } => (
            format!("{host}:{port}/{database}"),
            format!("{} {credential_ref}", credential_kind.as_str()),
        ),
    };
    [
        summary.row.name.to_string(),
        if summary.is_default { "default" } else { "-" }.to_string(),
        place,
        credential,
        format!("{} pinned", summary.pinned_teams),
        format_timestamp(summary.created_at),
    ]
    .join("\t")
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::cli::{Cli, Command};
    use crate::db::{test_db, TEST_TEAM};
    use crate::stores::PoolSettings;

    fn command(args: &[&str]) -> StoresCommand {
        let cli = Cli::try_parse_from(
            ["okf-mcp", "stores"]
                .into_iter()
                .chain(args.iter().copied()),
        )
        .expect("the arguments parse");
        match cli.command {
            Some(Command::Stores(command)) => command,
            other => panic!("not a stores command: {other:?}"),
        }
    }

    fn registry(pool: &PgPool) -> Stores {
        Stores::new(
            pool.clone(),
            PoolSettings {
                max_connections: 2,
                idle_timeout: std::time::Duration::from_secs(60),
                connect_timeout: std::time::Duration::from_secs(2),
            },
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_commands_list_set_default_pin_and_unpin() {
        let db = test_db().await;
        crate::db::main_in_the_account_database(&db.pool).await;
        let stores = registry(&db.pool);
        let now = Utc::now();
        let run = |args: &'static [&'static str]| {
            let (pool, stores) = (db.pool.clone(), stores.clone());
            async move { super::stores(&pool, &stores, command(args), now).await }
        };

        let listed = run(&["list"]).await.unwrap();
        let set = run(&["set-default", "main"]).await.unwrap();
        let pinned = run(&["pin", TEAM_ARG, "main"]).await.unwrap();
        let unpinned = run(&["unpin", TEAM_ARG]).await.unwrap();
        let unknown = run(&["set-default", "nowhere"]).await.unwrap_err();

        assert!(listed.starts_with("main\tdefault\taccount database\t-\t0 pinned\t"));
        assert!(set.contains("existing catalogs stay where they are"));
        assert_eq!(pinned, format!("pinned team {TEST_TEAM} to store main\n"));
        assert_eq!(unpinned, format!("unpinned team {TEST_TEAM}\n"));
        assert!(unknown.to_string().contains("store"), "{unknown}");
    }

    /// [`TEST_TEAM`] as a `'static` argument.
    const TEAM_ARG: &str = TEST_TEAM;

    #[tokio::test(flavor = "multi_thread")]
    async fn adding_a_store_that_cannot_be_reached_is_refused_and_names_no_host() {
        let db = test_db().await;
        let stores = registry(&db.pool);
        let refusing = crate::test_net::Refused::new();
        let closed = refusing.port().to_string();

        let refused = crate::test_net::bounded(
            "adding a store at a refusing address",
            super::stores(
                &db.pool,
                &stores,
                command(&[
                    "add",
                    "shared-1",
                    "--host",
                    "127.0.0.1",
                    "--port",
                    &closed,
                    "--database",
                    "okf",
                    "--credential-kind",
                    "env",
                    "--credential-ref",
                    "NO_SUCH_VARIABLE",
                ]),
                Utc::now(),
            ),
        )
        .await
        .unwrap_err();

        let text = format!("{refused:#}");
        for private in ["127.0.0.1", closed.as_str(), "NO_SUCH_VARIABLE"] {
            assert!(!text.contains(private), "{private} leaked into {text}");
        }
        assert!(Cli::try_parse_from([
            "okf-mcp",
            "stores",
            "add",
            "x",
            "--host",
            "h",
            "--database",
            "d",
            "--credential-kind",
            "password",
            "--credential-ref",
            "r",
        ])
        .is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn migrate_stores_reports_an_unreachable_store_and_continues() {
        let db = crate::db::test_db_account_layout().await;
        crate::db::main_in_the_account_database(&db.pool).await;
        // An empty database for a new store, on the same server and login.
        let fresh = crate::db::pg_support::TestDb::start().await;
        let refusing = crate::test_net::Refused::new();
        let closed = refusing.port();
        let options = fresh.pool.connect_options();
        // Names in order: `a-down` (unreachable) is migrated first, so the
        // two after it show that the command went on.
        for (name, host, port, database) in [
            ("a-down", "127.0.0.1", closed, "okf"),
            (
                "shared-2",
                options.get_host(),
                options.get_port(),
                options.get_database().unwrap(),
            ),
        ] {
            sqlx::query(
                "INSERT INTO stores (name, location_kind, host, port, database, \
                 credential_kind, credential_ref, created_at) \
                 VALUES ($1, 'host', $2, $3, $4, 'env', 'STORE_PASSWORD', now())",
            )
            .bind(name)
            .bind(host)
            .bind(i32::from(port))
            .bind(database)
            .execute(&db.pool)
            .await
            .unwrap();
        }
        let password = fresh
            .url
            .split("://")
            .nth(1)
            .and_then(|rest| rest.split('@').next())
            .and_then(|credentials| credentials.split_once(':'))
            .map(|(_, password)| password.to_string())
            .unwrap_or_default();
        let stores = Stores::with_environment(
            db.pool.clone(),
            PoolSettings {
                max_connections: 1,
                idle_timeout: std::time::Duration::from_secs(60),
                connect_timeout: std::time::Duration::from_secs(2),
            },
            std::sync::Arc::new(move |name| (name == "STORE_PASSWORD").then(|| password.clone())),
        );

        let failed = crate::test_net::bounded(
            "migrating a store at a refusing address",
            super::stores(&db.pool, &stores, command(&["migrate"]), Utc::now()),
        )
        .await
        .unwrap_err();

        let report = failed.to_string();
        let applied = crate::db::CATALOG_MIGRATOR.iter().count();
        let lines: Vec<&str> = report.lines().collect();
        assert_eq!(lines.len(), 4, "{report}");
        assert!(lines[0].starts_with("a-down\tfailed: "), "{report}");
        assert_eq!(lines[1], "main\tup to date");
        assert_eq!(lines[2], format!("shared-2\t{applied} applied"));
        assert_eq!(lines[3], "1 of 3 stores could not be migrated");
        // No host, port or credential in the report.
        for private in [closed.to_string().as_str(), "STORE_PASSWORD"] {
            assert!(!report.contains(private), "{private} leaked into {report}");
        }
        let mut connection = fresh.pool.acquire().await.unwrap();
        assert_eq!(
            crate::db::store_owner(&mut connection)
                .await
                .unwrap()
                .as_deref(),
            Some("shared-2")
        );
    }
}
