//! The operator's index repair (`okf-mcp catalog repair-indexes`, issue
//! #426): [`crate::okf::ops::repair_catalog`] run on a catalog's live
//! documents and written at once, in one transaction under the catalog's
//! lock, as the command-line import writes.
//!
//! Agents repair through a changeset instead (the `repair_indexes` tool).
//! This path exists for catalogs created before new catalogs got starter
//! files, whose owners may never connect an agent that could.
//!
//! Lock order: as import's. `audit::lock_people` first (the catalog), then
//! the catalog's advisory lock (`catalogs::lock_by_name`), then the
//! documents. The advisory lock excludes publishes and imports. A changeset
//! write may run meanwhile; its entries keep their base revisions, and every
//! path this writes gets a new revision, so a later publish of such an
//! entry replays its bookkeeping or reports a conflict, never overwrites.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::error::OkfError;
use crate::invalidate::{invalidate, Invalidation};
use crate::okf::ops::{repair_catalog, OpContext, OpOutcome};
use crate::okf::rules;
use crate::okf::snapshot::Snapshot;
use crate::okf::workspace::{MemWorkspace, StoredFile};
use crate::store::audit::{self, Action, AuditActor, CatalogEntry, People};
use crate::store::catalogs::{self, CatalogGeneration};
use crate::store::documents::{self, Derived, DocumentWrite, RevisionOp, RevisionRecord};
use crate::store::import::{existing_rows, ExistingRow};
use crate::stores::{AccountDb, CatalogDb, StoreName};

/// The `revisions.message` of a repair.
pub const REPAIR_MESSAGE: &str = "repair indexes";

/// How to repair.
#[derive(Debug, Clone, Copy)]
pub struct RepairOptions<'a> {
    /// Compute and report the repair, but write nothing.
    pub dry_run: bool,
    /// Recorded as `updated_by` and the revisions' `actor`.
    pub actor: &'a str,
    /// Who, for the audit log's `catalog.repaired` entry.
    pub audit: &'a AuditActor,
    /// The time of every row written, and of the log entry (server clock).
    pub now: DateTime<Utc>,
}

/// What a repair did (or, in a dry run, would do).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RepairReport {
    /// The catalog.
    pub catalog: String,
    /// `/`-prefixed paths written, the root index first.
    pub written: Vec<String>,
    /// The op's summary line.
    pub message: String,
    /// One per created file and per TODO placeholder.
    pub notes: Vec<String>,
    /// The catalog's generation after the repair; `None` for a dry run or a
    /// repair that wrote nothing.
    pub generation: Option<i64>,
    /// The catalog and [`RepairReport::generation`], for the command line to
    /// announce to the serving replicas (issue #659); `None` exactly when
    /// `generation` is. Not serialized.
    #[serde(skip)]
    pub changed: Option<CatalogGeneration>,
}

/// Repairs a catalog's indexes and log in place (see the module docs).
///
/// # Arguments
/// * `db` - the store holding the catalog.
/// * `catalog` - the catalog's name.
/// * `options` - dry run, actor and time.
///
/// # Returns
/// What was (or would be) written. A catalog with nothing missing writes
/// nothing, bumps nothing and records nothing.
///
/// # Errors
/// [`OkfError::CatalogNotSelected`] for an unknown catalog, or one deleted
/// while this waited for its lock; [`OkfError::InvalidPath`] when its stored
/// paths cannot form a workspace, or a written path is machinery;
/// [`OkfError::SecretDetected`] or [`OkfError::FrontmatterParse`] from the
/// content rules (only text the catalog already held can trigger them, in a
/// listing's title or summary); [`OkfError::Database`], a failed audit write
/// included. Every error rolls back, leaving the catalog as it was.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "repair.repair"))]
pub async fn repair(
    db: &CatalogDb,
    catalog: &str,
    options: &RepairOptions<'_>,
) -> Result<RepairReport, OkfError> {
    // Dropping `tx` without `commit()` (any `?`, or a dry run) rolls back.
    // The account transaction wraps the store's; the store commits first.
    let mut account_tx = db.accounts().begin().await?;
    audit::lock_people(
        &mut account_tx,
        options.audit,
        &People {
            catalogs: &[catalog],
            ..People::default()
        },
    )
    .await?;
    // Acquisition order (plan Task 11, issue #970): the account transaction
    // and every account row lock (`lock_people`) come BEFORE the store
    // connection. A change that held the store's connection while it waited
    // for a user row would block a user deletion (which holds those rows and
    // then needs a store connection) in a cycle PostgreSQL cannot see,
    // because one edge is a pool wait.
    let mut tx = db.begin().await?;
    let found = catalogs::lock_by_name(&mut account_tx, &mut tx, catalog).await?;
    // The entry's account ids, resolved on the account transaction (the
    // rows `lock_people` holds) for the store transaction to write.
    let resolved = audit::resolve_actor(&mut account_tx, options.audit, found.id).await?;
    let existing = existing_rows(&mut tx, found.id).await?;
    let live: Vec<(String, StoredFile)> = existing
        .values()
        .filter(|row| row.is_live())
        .map(|row| {
            let file = StoredFile {
                content: row.content.clone(),
                executable: row.executable,
            };
            (row.path.clone(), file)
        })
        .collect();
    let (name, description) = (found.name.clone(), found.description.clone());
    let (actor, now) = (options.actor.to_string(), options.now);
    // The op reads every document and parses concepts: CPU work, so it runs
    // on a blocking thread. `move` hands it owned copies of what it needs.
    let caller = crate::okf::caller::Caller::capture();
    let computed = tokio::task::spawn_blocking(move || {
        let _entered = caller.enter();
        compute(live, &name, &description, &actor, now)
    })
    .await
    // A panic in the task is re-raised here, as if it ran inline.
    .unwrap_or_else(|error| std::panic::resume_unwind(error.into_panic()))?;
    let Computed { outcome, files } = computed;
    let mut report = RepairReport {
        catalog: found.name.clone(),
        written: outcome
            .touched
            .iter()
            .map(|path| format!("/{path}"))
            .collect(),
        message: outcome.message,
        notes: outcome.notes,
        generation: None,
        changed: None,
    };
    if options.dry_run || files.is_empty() {
        return Ok(report);
    }
    write_files(&mut tx, found.id, &existing, &files, options).await?;
    let generation = catalogs::bump_generation(&mut tx, found.id).await?;
    // In the store transaction: the entry commits with the repair.
    audit::record_catalog(
        &mut tx,
        &resolved,
        &CatalogEntry {
            action: Action::CatalogRepaired,
            catalog_id: found.id,
            catalog,
            details: serde_json::json!({
                "files": report.written,
                "generation": generation,
            }),
        },
        options.now,
    )
    .await?;
    // On the store's change feed: sent when the generation commits.
    invalidate(
        &mut tx,
        &[Invalidation::Catalog {
            id: found.id,
            generation,
        }],
    )
    .await?;
    tx.commit().await?;
    account_tx.commit().await?;
    report.generation = Some(generation);
    report.changed = Some(CatalogGeneration {
        catalog_id: found.id,
        generation,
    });
    Ok(report)
}

/// The op's outcome and the files it wrote, checked by the write rules.
struct Computed {
    /// The op's outcome.
    outcome: OpOutcome,
    /// `(path, file, derived columns)` per written path, in write order.
    files: Vec<(String, StoredFile, Derived)>,
}

/// Runs the repair on a copy of the live documents and checks every file
/// it wrote against the write rules (runs on a blocking thread).
#[tracing::instrument(level = "trace", skip_all)]
fn compute(
    live: Vec<(String, StoredFile)>,
    name: &str,
    description: &str,
    actor: &str,
    now: DateTime<Utc>,
) -> Result<Computed, OkfError> {
    let workspace = MemWorkspace::from_stored(live)?;
    let ctx = OpContext { now, by: actor };
    let outcome = repair_catalog(&workspace, &ctx, name, description)?;
    let mut written = Vec::with_capacity(outcome.touched.len());
    for path in &outcome.touched {
        rules::check_path(path)?;
        let file = workspace
            .stored(path)
            .expect("the repair wrote every path it touched");
        let text = String::from_utf8_lossy(&file.content);
        rules::check_content(path, &text)?;
        written.push((path.clone(), file));
    }
    // The derived columns (kind, title) of the written files only: they are
    // indexes and the log, whose columns do not depend on other documents.
    let snapshot = Snapshot::build(&MemWorkspace::from_stored(written.clone())?)?;
    let files = written
        .into_iter()
        .map(|(path, file)| {
            let derived = Derived::of(snapshot.get(&path));
            (path, file, derived)
        })
        .collect();
    Ok(Computed { outcome, files })
}

/// Writes each repaired file as the next revision of its path (1 for a new
/// path; a tombstone's numbers continue), with a `create` or `update`
/// revision carrying [`REPAIR_MESSAGE`].
#[tracing::instrument(level = "trace", skip_all, fields(db.system.name = "postgresql", db.operation.name = "repair.write_files", catalog_id = catalog_id))]
async fn write_files(
    conn: &mut sqlx::PgConnection,
    catalog_id: i64,
    existing: &HashMap<String, ExistingRow>,
    files: &[(String, StoredFile, Derived)],
    options: &RepairOptions<'_>,
) -> Result<(), OkfError> {
    for (path, file, derived) in files {
        let previous = existing.get(path);
        let write = DocumentWrite {
            catalog_id,
            path,
            revision: previous.map_or(1, |row| row.revision + 1),
            actor: options.actor,
            now: options.now,
        };
        let record = RevisionRecord {
            op: if previous.is_some_and(ExistingRow::is_live) {
                RevisionOp::Update
            } else {
                RevisionOp::Create
            },
            file: Some(file),
            changeset_id: None,
            message: REPAIR_MESSAGE,
            user_id: None,
        };
        documents::write_document(conn, &write, file, derived).await?;
        documents::record_revision(conn, &write, &record).await?;
    }
    Ok(())
}

/// The catalogs with no live root `index.md`, by name: what
/// `repair-indexes --missing-only` repairs.
///
/// Two reads, joined here by catalog id (plan decision P1: no statement
/// names tables from both sides): the account database lists the catalogs
/// in name order, and the store lists which of them have a live root
/// `index.md`.
///
/// # Arguments
/// * `accounts` - the account database (the catalogs).
/// * `db` - the store holding their documents; only the catalogs placed on
///   it are listed.
///
/// # Returns
/// Their names, sorted (in the database's order, as the one joined statement
/// gave).
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "repair.missing_root_index"))]
pub async fn missing_root_index(
    accounts: &AccountDb,
    db: &CatalogDb,
) -> Result<Vec<String>, OkfError> {
    let catalogs: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, name FROM catalogs \
         WHERE provisioned_at IS NOT NULL AND deleted_at IS NULL AND store = $1 ORDER BY name",
    )
    .bind(db.store().as_str())
    .fetch_all(accounts.executor())
    .await?;
    if catalogs.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<i64> = catalogs.iter().map(|(id, _)| *id).collect();
    // `DISTINCT`: one row per catalog that has a root index, however many
    // revisions its rows carry.
    let with_index: HashSet<i64> = sqlx::query_scalar(
        "SELECT DISTINCT catalog_id FROM documents \
         WHERE catalog_id = ANY($1) AND path = 'index.md' AND deleted_at IS NULL",
    )
    .bind(&ids)
    .fetch_all(db.executor())
    .await?
    .into_iter()
    .collect();
    Ok(catalogs
        .into_iter()
        .filter(|(id, _)| !with_index.contains(id))
        .map(|(_, name)| name)
        .collect())
}

/// Which of `names` live on `store`: the catalogs of that name placed on it,
/// in the order given. A name that is no live catalog counts as `main`'s, so
/// the repair of it fails on one store, with the error it always gave.
///
/// # Arguments
/// * `accounts` - the account database (the catalogs).
/// * `store` - the store.
/// * `names` - the catalogs named on the command line.
///
/// # Returns
/// The names placed on `store`.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "repair.placed_on"))]
pub async fn placed_on(
    accounts: &AccountDb,
    store: &StoreName,
    names: &[String],
) -> Result<Vec<String>, OkfError> {
    Ok(sqlx::query_scalar(
        "SELECT t.name FROM unnest($2::text[]) WITH ORDINALITY AS t(name, position) \
         LEFT JOIN catalogs c ON c.name = t.name AND c.deleted_at IS NULL \
         WHERE COALESCE(c.store, 'main') = $1 ORDER BY t.position",
    )
    .bind(store.as_str())
    .bind(names)
    .fetch_all(accounts.executor())
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{test_db, TEST_TEAM};
    use crate::store::audit::testing::entries;
    use crate::store::import::{import, ImportOptions, ImportScope};
    use chrono::TimeZone;
    use sqlx::PgPool;

    const NOTE: &str = "---\ntype: Note\ntitle: A\ndescription: A note.\n---\n# A\n";

    fn at(minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 15, 12, minute, 0).unwrap()
    }

    /// Creates `name`, then imports `files` into it: a catalog as one
    /// created before starter files and imported without indexes is (the
    /// import drops the starter files it lacks).
    async fn legacy(pool: &PgPool, name: &str, description: &str, files: &[(&str, &str)]) {
        let new = catalogs::NewCatalog {
            name,
            display_name: &crate::labels::DisplayName::default(),
            description,
            owner_team: TEST_TEAM,
        };
        catalogs::create(pool, &new, &AuditActor::cli(), at(0))
            .await
            .unwrap();
        let options = ImportOptions {
            replace: false,
            scope: ImportScope::Everything,
            expect_generation: None,
            expect_catalog_id: None,
            actor: "test/import",
            audit: &AuditActor::cli(),
            now: at(1),
        };
        import(
            &crate::stores::CatalogDb::main_in(pool),
            name,
            crate::archive::files(files),
            &options,
        )
        .await
        .unwrap();
    }

    fn options(audit: &AuditActor, dry_run: bool) -> RepairOptions<'_> {
        RepairOptions {
            dry_run,
            actor: "okf-mcp/repair-indexes",
            audit,
            now: at(2),
        }
    }

    async fn live_text(pool: &PgPool, path: &str) -> Option<String> {
        sqlx::query_scalar::<_, Vec<u8>>(
            "SELECT content FROM catalog.documents WHERE path = $1 AND deleted_at IS NULL",
        )
        .bind(path)
        .fetch_optional(&crate::db::store_pool(pool))
        .await
        .unwrap()
        .map(|bytes| String::from_utf8(bytes).unwrap())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_repair_writes_the_missing_files_once_and_records_it() {
        let db = test_db().await;
        legacy(
            &db.pool,
            "pg",
            "Team notes.",
            &[("a.md", NOTE), ("infra/b.md", NOTE)],
        )
        .await;
        let audit = AuditActor::cli();

        let report = repair(
            &crate::stores::CatalogDb::main_in(&db.pool),
            "pg",
            &options(&audit, false),
        )
        .await
        .unwrap();
        let again = repair(
            &crate::stores::CatalogDb::main_in(&db.pool),
            "pg",
            &options(&audit, false),
        )
        .await
        .unwrap();

        assert_eq!(
            report.written,
            ["/index.md", "/log.md", "/infra/index.md"],
            "{report:?}"
        );
        assert_eq!(report.generation, Some(2));
        let root = live_text(&db.pool, "index.md").await.unwrap();
        // The root a new catalog would get, plus the listings.
        assert!(
            root.starts_with("---\nokf_version: \"0.2\"\n---\n\n# pg\n\nTeam notes.\n"),
            "{root}"
        );
        assert!(root.contains("* [A](a.md) - A note."), "{root}");
        assert!(
            root.contains("* [Infra](infra/) - TODO: describe this folder"),
            "{root}"
        );
        let log = live_text(&db.pool, "log.md").await.unwrap();
        assert!(
            log.contains("* **Update:** Repaired the catalog's indexes (okf-mcp catalog repair-indexes): wrote index.md, log.md, infra/index.md."),
            "{log}"
        );
        // The starter files' tombstones (revision 2) continue their numbers.
        let history: Vec<(String, i32, String, String, String)> = sqlx::query_as(
            "SELECT path, revision, op, actor, message FROM catalog.revisions \
             WHERE message = $1 ORDER BY path",
        )
        .bind(REPAIR_MESSAGE)
        .fetch_all(&crate::db::store_pool(&db.pool))
        .await
        .unwrap();
        let row = |path: &str, revision| {
            (
                path.to_string(),
                revision,
                "create".to_string(),
                "okf-mcp/repair-indexes".to_string(),
                REPAIR_MESSAGE.to_string(),
            )
        };
        assert_eq!(
            history,
            [
                row("index.md", 3),
                row("infra/index.md", 1),
                row("log.md", 3)
            ]
        );
        let kind: Option<String> =
            sqlx::query_scalar("SELECT kind FROM catalog.documents WHERE path = 'infra/index.md'")
                .fetch_one(&crate::db::store_pool(&db.pool))
                .await
                .unwrap();
        assert_eq!(kind.as_deref(), Some("index"));
        // A second run finds nothing missing: no write, bump or entry.
        assert!(
            again.written.is_empty() && again.generation.is_none(),
            "{again:?}"
        );
        assert_eq!(again.message, "indexes complete: nothing to repair");
        let recorded = entries(&db.pool, "catalog.repaired").await;
        assert_eq!(recorded.len(), 1, "{recorded:?}");
        assert_eq!(recorded[0].target.as_deref(), Some("pg"));
        assert_eq!(
            recorded[0].details,
            serde_json::json!({"files": ["/index.md", "/log.md", "/infra/index.md"], "generation": 2})
        );
        assert_eq!(catalogs::test_generation(&db.pool, "pg").await, 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_repair_that_waited_for_a_publish_builds_on_what_it_wrote() {
        let db = test_db().await;
        legacy(&db.pool, "pg", "", &[("a.md", NOTE)]).await;
        let catalog = catalogs::find(&db.pool, "pg").await.unwrap();
        // Hold the catalog's lock, as a publish in progress does, and write
        // a curated root index.md under it.
        let mut holder = db.store().begin().await.unwrap();
        catalogs::lock(&mut holder, catalog.id).await.unwrap();
        let pool = db.pool.clone();
        let waiting = tokio::spawn(async move {
            let audit = AuditActor::cli();
            repair(
                &crate::stores::CatalogDb::main_in(&pool),
                "pg",
                &options(&audit, false),
            )
            .await
        });
        catalogs::tests::until_a_lock_is_awaited(&db.pool).await;
        let curated = "---\nokf_version: \"0.2\"\n---\n# Ours\n\nCurated words.\n";
        let file = StoredFile {
            content: curated.as_bytes().to_vec(),
            executable: false,
        };
        // The starter index.md's tombstone is revision 2.
        let write = DocumentWrite {
            catalog_id: catalog.id,
            path: "index.md",
            revision: 3,
            actor: "publisher",
            now: at(1),
        };
        documents::write_document(&mut holder, &write, &file, &Derived::default())
            .await
            .unwrap();
        holder.commit().await.unwrap();
        let repaired = tokio::time::timeout(std::time::Duration::from_secs(30), waiting)
            .await
            .expect("the repair waited for the held catalog lock and never finished")
            .unwrap()
            .unwrap();

        // The repair saw the published root: it kept it, listed the concept
        // in it, and continued its revision.
        // The root existed, so only the log was created; the listing came after.
        assert_eq!(repaired.written, ["/log.md", "/index.md"], "{repaired:?}");
        let root = live_text(&db.pool, "index.md").await.unwrap();
        assert!(root.starts_with(curated), "{root}");
        assert!(root.contains("* [A](a.md) - A note."), "{root}");
        let revision: i32 =
            sqlx::query_scalar("SELECT revision FROM catalog.documents WHERE path = 'index.md'")
                .fetch_one(&crate::db::store_pool(&db.pool))
                .await
                .unwrap();
        assert_eq!(revision, 4);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_dry_run_reports_the_repair_and_writes_nothing() {
        let db = test_db().await;
        legacy(&db.pool, "pg", "", &[("a.md", NOTE)]).await;
        let audit = AuditActor::cli();

        let dry = repair(
            &crate::stores::CatalogDb::main_in(&db.pool),
            "pg",
            &options(&audit, true),
        )
        .await
        .unwrap();

        assert_eq!(dry.written, ["/index.md", "/log.md"]);
        assert_eq!(dry.generation, None);
        // No description: the root's TODO line is a placeholder to fill in.
        assert!(dry
            .notes
            .iter()
            .any(|note| note.contains("no description, so replace its TODO line")));
        assert_eq!(live_text(&db.pool, "index.md").await, None);
        assert!(entries(&db.pool, "catalog.repaired").await.is_empty());
        assert_eq!(catalogs::test_generation(&db.pool, "pg").await, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn missing_root_index_lists_only_catalogs_without_one() {
        let db = test_db().await;
        legacy(&db.pool, "old", "", &[("a.md", NOTE)]).await;
        let new = catalogs::NewCatalog {
            name: "fresh",
            display_name: &crate::labels::DisplayName::default(),
            description: "",
            owner_team: TEST_TEAM,
        };
        catalogs::create(&db.pool, &new, &AuditActor::cli(), at(0))
            .await
            .unwrap();
        let audit = AuditActor::cli();

        let before = missing_root_index(&AccountDb::of(&db.pool), &CatalogDb::main_of(&db.pool))
            .await
            .unwrap();
        repair(
            &crate::stores::CatalogDb::main_in(&db.pool),
            "old",
            &options(&audit, false),
        )
        .await
        .unwrap();
        let after = missing_root_index(&AccountDb::of(&db.pool), &CatalogDb::main_of(&db.pool))
            .await
            .unwrap();
        let unknown = repair(
            &crate::stores::CatalogDb::main_in(&db.pool),
            "nope",
            &options(&audit, false),
        )
        .await;

        assert_eq!(before, ["old"]);
        assert!(after.is_empty(), "{after:?}");
        assert!(matches!(unknown, Err(OkfError::CatalogNotSelected { .. })));
    }

    /// Issue #963 (M21 Task 5): the join of `catalogs` and `documents` is two
    /// reads joined by id. It lists what the joined statement listed, in
    /// name order: a catalog whose root index is soft-deleted or only nested
    /// (`sub/index.md`) counts as missing it, a catalog with one does not.
    #[tokio::test(flavor = "multi_thread")]
    async fn repair_lists_the_same_catalogs_after_the_split() {
        let db = test_db().await;
        // Created after `alpha` would be by id, listed by name.
        legacy(&db.pool, "nested", "", &[("sub/index.md", NOTE)]).await;
        legacy(&db.pool, "alpha", "", &[("a.md", NOTE)]).await;
        for name in ["zulu", "mid"] {
            let new = catalogs::NewCatalog {
                name,
                display_name: &crate::labels::DisplayName::default(),
                description: "",
                owner_team: TEST_TEAM,
            };
            catalogs::create(&db.pool, &new, &AuditActor::cli(), at(0))
                .await
                .unwrap();
        }
        let accounts = AccountDb::of(&db.pool);
        let store = CatalogDb::main_of(&db.pool);
        let before = missing_root_index(&accounts, &store).await.unwrap();
        // `mid`'s root index is deleted (a tombstone row stays).
        let deleted = sqlx::query(
            "UPDATE catalog.documents SET deleted_at = now() \
             WHERE path = 'index.md' AND catalog_id = $1",
        )
        .bind(crate::db::catalog_id(&db.pool, "mid").await)
        .execute(&crate::db::store_pool(&db.pool))
        .await
        .unwrap()
        .rows_affected();
        let after = missing_root_index(&accounts, &store).await.unwrap();

        assert_eq!(deleted, 1);
        assert_eq!(before, ["alpha", "nested"]);
        assert_eq!(after, ["alpha", "mid", "nested"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_audit_write_fails_the_repair() {
        let db = test_db().await;
        legacy(&db.pool, "pg", "", &[("a.md", NOTE)]).await;
        crate::http::api::testing::fail_on(
            &db.pool,
            "BEFORE INSERT ON catalog.catalog_audit_entries FOR EACH ROW",
        )
        .await;
        let audit = AuditActor::cli();

        let refused = repair(
            &crate::stores::CatalogDb::main_in(&db.pool),
            "pg",
            &options(&audit, false),
        )
        .await;

        assert!(refused.is_err());
        assert_eq!(live_text(&db.pool, "index.md").await, None);
        assert_eq!(catalogs::test_generation(&db.pool, "pg").await, 1);
    }
}
