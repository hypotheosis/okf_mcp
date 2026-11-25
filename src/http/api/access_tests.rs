//! Access accounting over REST (M14 Task 7b): an operator's content reads
//! become hourly `catalog.admin_read` entries with identities and scope
//! fixed at admission, and every export is a `catalog.exported` entry.
//! Each privacy test holds the background write (`App::access_hold`),
//! changes something, then lets the write run.

use axum::http::StatusCode;
use chrono::Utc;

use crate::db::{TEST_TEAM, TEST_USER};
use crate::http::api::testing::{app, keys, send};
use crate::mcp::testing::{require_unlock, settle_access, staff_key, staff_root, TestApp};
use crate::store::audit::{self, AuditActor};
use crate::store::catalogs::{self, NewCatalog};
use crate::store::keys::Access;
use crate::store::teams::{self, NewTeam};

/// Reads `main`'s tree as `key` with the background write held.
///
/// # Returns
/// The hold: the write runs once it is dropped.
async fn held_read(test: &TestApp, key: &str) -> tokio::sync::OwnedMutexGuard<()> {
    let hold = std::sync::Arc::clone(&test.app.access_hold)
        .lock_owned()
        .await;
    let (status, _) = send(test, "GET", "/api/v1/catalogs/main/tree", Some(key), None).await;
    assert_eq!(status, StatusCode::OK);
    hold
}

/// Every `catalog.admin_read` entry's teams, details and target name.
async fn admin_reads(test: &TestApp) -> Vec<(Vec<i64>, serde_json::Value, String)> {
    sqlx::query_as(
        "SELECT teams, details, target_name FROM audit_entries \
         WHERE action = 'catalog.admin_read' ORDER BY id",
    )
    .fetch_all(test.app.accounts().executor())
    .await
    .unwrap()
}

/// A team's `audit_teams` id.
async fn audit_team(test: &TestApp, team: &str) -> i64 {
    let mut conn = test.app.accounts().executor().acquire().await.unwrap();
    audit::team_ref(&mut conn, team).await.unwrap()
}

/// A catalog's `catalogs.id`.
async fn catalog_id(test: &TestApp, name: &str) -> i64 {
    sqlx::query_scalar("SELECT id FROM catalogs WHERE name = $1")
        .bind(name)
        .fetch_one(test.app.accounts().executor())
        .await
        .unwrap()
}

/// Creates team `family`, owned by the test user.
async fn family(test: &TestApp) {
    let team = NewTeam {
        name: "family",
        display_name: &crate::labels::DisplayName::default(),
        owner: TEST_USER,
    };
    teams::create(
        test.app.accounts().executor(),
        &team,
        &AuditActor::cli(),
        Utc::now(),
    )
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_operator_read_is_bucketed_and_a_members_read_is_not() {
    let test = app().await;
    let scoped = keys(&test).await;
    let staff = staff_root(&test).await;

    send(
        &test,
        "GET",
        "/api/v1/catalogs/main/tree",
        Some(&staff),
        None,
    )
    .await;
    let document = "/api/v1/catalogs/main/documents/systems/index.md";
    send(&test, "GET", document, Some(&staff), None).await;
    // A member's read (the test user owns main), and an admin key of the
    // member: neither is an admin read.
    send(
        &test,
        "GET",
        "/api/v1/catalogs/main/tree",
        Some(&scoped.read),
        None,
    )
    .await;
    send(
        &test,
        "GET",
        "/api/v1/catalogs/main/tree",
        Some(&scoped.admin),
        None,
    )
    .await;
    settle_access(&test).await;

    let entries =
        audit::testing::entries(test.app.accounts().executor(), "catalog.admin_read").await;
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0].requests, Some(2), "one request, one count");
    assert_eq!(entries[0].target.as_deref(), Some("main"));
    assert_eq!(entries[0].actor_person.as_deref(), Some("staff"));
    assert_eq!(entries[0].actor_label, "api key");
    assert!(entries[0].actor_operator);
    assert_eq!(entries[0].teams, [TEST_TEAM]);
    assert_eq!(entries[0].source, "api");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_write_delayed_past_the_users_deletion_stores_nothing_that_identifies_them() {
    let test = app().await;
    let staff = staff_root(&test).await;
    let hold = held_read(&test, &staff).await;

    crate::store::users::delete(
        test.app.accounts().executor(),
        "staff",
        crate::store::users::DeleteOptions::OPERATOR,
        &AuditActor::cli(),
        Utc::now(),
    )
    .await
    .unwrap();
    drop(hold);
    settle_access(&test).await;

    // Every audit row, as text.
    let stored: String = sqlx::query_scalar(
        "SELECT coalesce((SELECT string_agg(row_to_json(e)::text, '') FROM audit_entries e), '') \
             || coalesce((SELECT string_agg(row_to_json(c)::text, '') \
                          FROM audit_credentials c), '') \
             || coalesce((SELECT string_agg(row_to_json(p)::text, '') FROM audit_people p), '') \
             || coalesce((SELECT string_agg(bucket, '') FROM audit_access_counts), '')",
    )
    .fetch_one(test.app.accounts().executor())
    .await
    .unwrap();
    assert_eq!(
        admin_reads(&test).await.len(),
        1,
        "the read is still recorded"
    );
    assert!(
        !stored.contains("staff") && !stored.contains("Staff"),
        "{stored}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_admitted_before_a_transfer_stays_with_the_old_owner() {
    let test = app().await;
    let staff = staff_root(&test).await;
    family(&test).await;
    let personal = audit_team(&test, TEST_TEAM).await;
    let hold = held_read(&test, &staff).await;

    catalogs::set_owner(
        test.app.accounts().executor(),
        "main",
        "family",
        &AuditActor::cli(),
        Utc::now(),
    )
    .await
    .unwrap();
    drop(hold);
    settle_access(&test).await;

    let reads = admin_reads(&test).await;
    assert_eq!(reads.len(), 1);
    assert_eq!(reads[0].0, [personal]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_of_a_catalog_deleted_or_replaced_before_it_is_written_keeps_its_identity() {
    let test = app().await;
    let staff = staff_root(&test).await;
    family(&test).await;
    let personal = audit_team(&test, TEST_TEAM).await;
    let original = catalog_id(&test, "main").await;
    let hold = held_read(&test, &staff).await;

    // Deleted, then a new `main` for another team, before the write runs.
    catalogs::delete_settled(
        test.app.accounts().executor(),
        "main",
        true,
        &AuditActor::cli(),
        Utc::now(),
    )
    .await
    .unwrap();
    let replacement = NewCatalog {
        name: "main",
        display_name: &crate::labels::DisplayName::default(),
        description: "",
        owner_team: "family",
    };
    catalogs::create(
        test.app.accounts().executor(),
        &replacement,
        &AuditActor::cli(),
        Utc::now(),
    )
    .await
    .unwrap();
    drop(hold);
    settle_access(&test).await;
    // Everything the replacement changed is read again, as after a
    // change-feed reconnect.
    test.app.apply_reconnect().await;
    // A read of the replacement is another entry, in its own scope.
    let (status, _) = send(
        &test,
        "GET",
        "/api/v1/catalogs/main/tree",
        Some(&staff),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    settle_access(&test).await;

    let reads = admin_reads(&test).await;
    let family = audit_team(&test, "family").await;
    let replacement = catalog_id(&test, "main").await;
    assert_eq!(reads.len(), 2, "neither dropped nor merged");
    assert_eq!(
        (reads[0].0.clone(), reads[0].1["catalog_id"].as_i64()),
        (vec![personal], Some(original))
    );
    assert_eq!(
        (reads[1].0.clone(), reads[1].1["catalog_id"].as_i64()),
        (vec![family], Some(replacement))
    );
    assert_eq!(reads[0].2, "main", "the name as it was");
}

#[tokio::test(flavor = "multi_thread")]
async fn every_export_is_recorded_whoever_asks() {
    let test = app().await;
    let scoped = keys(&test).await;
    let staff = staff_root(&test).await;

    let (member, _) = send(
        &test,
        "GET",
        "/api/v1/catalogs/main/export",
        Some(&scoped.read),
        None,
    )
    .await;
    let (operator, _) = send(
        &test,
        "GET",
        "/api/v1/catalogs/main/export",
        Some(&staff),
        None,
    )
    .await;

    assert_eq!((member, operator), (StatusCode::OK, StatusCode::OK));
    let entries = audit::testing::entries(test.app.accounts().executor(), "catalog.exported").await;
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].source, "api");
    assert_eq!(entries[0].actor_person.as_deref(), Some(TEST_USER));
    assert!(!entries[0].actor_operator);
    assert_eq!(entries[1].actor_person.as_deref(), Some("staff"));
    assert!(entries[1].actor_operator);
    assert_eq!(entries[0].teams, [TEST_TEAM]);
    assert!(entries[0].details["files"].as_i64().unwrap() > 0);
    // An export is its own entry, not an admin read.
    settle_access(&test).await;
    assert!(
        audit::testing::entries(test.app.accounts().executor(), "catalog.admin_read")
            .await
            .is_empty()
    );
}

/// Applies the change message `catalog`'s create sent to the app's catalog
/// directory, as the change feed would (lib tests run no listener).
async fn tell_directory(test: &TestApp, catalog: &str) {
    let invalidation = crate::invalidate::Invalidation::CatalogMeta {
        id: catalog_id(test, catalog).await,
        name: catalog.to_string(),
    };
    test.app.directory().apply(&invalidation).await;
}

/// Deletes `main` and creates it again for a new team `acme` of a new user
/// `bob` (so the test user is not a member of the replacement).
async fn replace_main_for_acme(test: &TestApp) {
    let pool = test.app.accounts().executor();
    let cli = AuditActor::cli();
    let bob = crate::store::users::NewUser {
        name: "bob",
        display_name: &crate::labels::DisplayName::default(),
    };
    crate::store::users::create(pool, &bob, &cli, Utc::now())
        .await
        .unwrap();
    let acme = NewTeam {
        name: "acme",
        display_name: &crate::labels::DisplayName::default(),
        owner: "bob",
    };
    teams::create(pool, &acme, &cli, Utc::now()).await.unwrap();
    catalogs::delete_settled(pool, "main", true, &cli, Utc::now())
        .await
        .unwrap();
    let main = NewCatalog {
        name: "main",
        display_name: &crate::labels::DisplayName::default(),
        description: "",
        owner_team: "acme",
    };
    catalogs::create(pool, &main, &cli, Utc::now())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_of_an_old_catalog_map_under_newer_gates_is_refused_not_misrecorded() {
    let test = app().await;
    let staff = staff_root(&test).await;
    let original = catalog_id(&test, "main").await;
    // `main` is cached (its row and its snapshot) before it is replaced.
    test.app.catalog("main").await.unwrap().expect("main");
    replace_main_for_acme(&test).await;
    // This call resolves the old `main` (its row and snapshot are cached)
    // before the replacement's change message reaches the directory.
    let ctx = crate::mcp::testing::context(
        &test,
        crate::mcp::testing::cached_principal(&test, &staff).await,
        None,
    );
    ctx.state("main").await.unwrap();
    tell_directory(&test, "main").await;
    assert_eq!(
        ctx.state("main").await.unwrap().id(),
        original,
        "the old map"
    );

    // `one` checks the incarnation (PR #442 review, round 2), and `admit`
    // on the map's state refuses it the same way.
    let state = ctx.state("main").await.unwrap();
    let selected = ctx.one(Some("main")).await.map(drop);
    let refused = ctx.admit(&state, audit::AccessKind::Read).await;
    assert!(
        matches!(selected, Err(crate::error::OkfError::Busy { .. })),
        "{selected:?}"
    );
    settle_access(&test).await;

    assert!(
        matches!(refused, Err(crate::error::OkfError::Busy { .. })),
        "{refused:?}"
    );
    assert!(admin_reads(&test).await.is_empty(), "nothing misrecorded");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_old_principal_reading_a_new_catalog_map_is_admitted_as_the_new_catalog() {
    let test = app().await;
    // The test user's every-catalog admin key: a member of the old `main`'s
    // owner team, not of the replacement's.
    let root = crate::http::api::testing::every_catalog_admin(&test).await;
    let old_principal = crate::mcp::testing::cached_principal(&test, &root).await;
    crate::invalidate::testing::applied(&test.app, async {
        replace_main_for_acme(&test).await;
    })
    .await;
    let ctx = crate::mcp::testing::context(&test, old_principal, None);

    let state = ctx.one(Some("main")).await.unwrap();
    ctx.admit(&state, audit::AccessKind::Read).await.unwrap();
    settle_access(&test).await;

    let reads = admin_reads(&test).await;
    let acme = audit_team(&test, "acme").await;
    let replacement = catalog_id(&test, "main").await;
    assert_eq!(
        reads.len(),
        1,
        "an admin read of the replacement, not a member's"
    );
    assert_eq!(
        (reads[0].0.clone(), reads[0].1["catalog_id"].as_i64()),
        (vec![acme], Some(replacement))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_export_whose_entry_cannot_be_written_answers_500_without_an_archive() {
    let test = app().await;
    let scoped = keys(&test).await;
    crate::http::api::testing::fail_on(
        test.app.accounts().executor(),
        "BEFORE INSERT ON audit_entries FOR EACH ROW",
    )
    .await;

    let (status, body) = send(
        &test,
        "GET",
        "/api/v1/catalogs/main/export",
        Some(&scoped.read),
        None,
    )
    .await;

    crate::http::api::testing::allow(test.app.accounts().executor(), "audit_entries").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["error"], "Database", "an error body, not an archive");
}

/// A tool context for `secret` on a connection of `main` then `other`.
async fn on_main_and_other(test: &TestApp, secret: &str) -> crate::mcp::context::ToolContext {
    crate::mcp::testing::context(
        test,
        crate::mcp::testing::cached_principal(test, secret).await,
        Some(&["main", "other"]),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_that_answers_from_the_first_catalog_records_only_that_one() {
    use crate::mcp::documents::{get_concept, GetConceptParams};
    use crate::mcp::history::{history, HistoryParams};
    let test = app().await;
    let staff = staff_root(&test).await;
    // `main`, listed first, answers each: both catalogs have `index.md`'s
    // history, and only `main` has the concept.
    let history_ctx = on_main_and_other(&test, &staff).await;
    let concept_ctx = on_main_and_other(&test, &staff).await;

    let found = history(
        &history_ctx,
        HistoryParams {
            path: "index.md".to_string(),
            catalog: None,
            limit: None,
            user: None,
            since: None,
        },
    )
    .await
    .unwrap();
    let concept = get_concept(
        &concept_ctx,
        GetConceptParams {
            path: "systems/alpha.md".to_string(),
            catalog: None,
            links: false,
            raw: false,
            section: None,
            full: false,
            changeset: None,
        },
    )
    .await;
    settle_access(&test).await;

    assert_eq!(found.catalog, "main");
    assert!(concept.is_ok(), "{concept:?}");
    let entries =
        audit::testing::entries(test.app.accounts().executor(), "catalog.admin_read").await;
    assert_eq!(
        entries.len(),
        1,
        "never `other`, which was not read: {entries:?}"
    );
    assert_eq!(entries[0].target.as_deref(), Some("main"));
    assert_eq!(entries[0].requests, Some(2), "one per call");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_search_refused_after_one_catalog_passes_queues_no_read() {
    use crate::mcp::search::{run, SearchParams};
    let test = app().await;
    let staff = staff_root(&test).await;
    // `other` is replaced after this call resolved it: `main` passes its
    // check, `other` (the call still holds the old one) cannot.
    let ctx = on_main_and_other(&test, &staff).await;
    ctx.state("other").await.unwrap();
    let cli = AuditActor::cli();
    catalogs::delete_settled(
        test.app.accounts().executor(),
        "other",
        true,
        &cli,
        Utc::now(),
    )
    .await
    .unwrap();
    let other = NewCatalog {
        name: "other",
        display_name: &crate::labels::DisplayName::default(),
        description: "",
        owner_team: TEST_TEAM,
    };
    catalogs::create(test.app.accounts().executor(), &other, &cli, Utc::now())
        .await
        .unwrap();
    tell_directory(&test, "other").await;

    let refused = run(
        &ctx,
        SearchParams {
            query: "Main".to_string(),
            catalogs: None,
            concept_type: None,
            tags: Vec::new(),
            status: None,
            limit: None,
            changeset: None,
        },
    )
    .await
    .map(drop);
    settle_access(&test).await;

    assert!(
        matches!(refused, Err(crate::error::OkfError::Busy { .. })),
        "{refused:?}"
    );
    assert!(
        admin_reads(&test).await.is_empty(),
        "nothing queued for `main`"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_maintenance_mutation_records_no_read_and_a_revision_read_records_one() {
    use crate::mcp::maintenance::work::{
        maintenance_claim, maintenance_read_revision, MaintenanceClaimParams,
        MaintenanceReadRevisionParams,
    };
    let test = app().await;
    // An admin key with reporting off: its claim is refused, after the
    // target catalog is resolved.
    let staff = staff_root(&test).await;
    let claim_ctx = crate::mcp::testing::context(
        &test,
        crate::mcp::testing::cached_principal(&test, &staff).await,
        None,
    );
    let read_ctx = crate::mcp::testing::context(
        &test,
        crate::mcp::testing::cached_principal(&test, &staff).await,
        None,
    );

    let claimed = maintenance_claim(
        &claim_ctx,
        MaintenanceClaimParams {
            catalog: "main".to_string(),
            idempotency_key: "claim-1".to_string(),
            batch_size: None,
        },
    )
    .await
    .map(drop);
    settle_access(&test).await;
    let after_claim = admin_reads(&test).await.len();
    let read = maintenance_read_revision(
        &read_ctx,
        MaintenanceReadRevisionParams {
            catalog: "main".to_string(),
            path: "systems/alpha.md".to_string(),
            revision: 1,
        },
    )
    .await
    .map(drop);
    settle_access(&test).await;

    assert!(
        matches!(claimed, Err(crate::error::OkfError::Forbidden { .. })),
        "{claimed:?}"
    );
    assert_eq!(after_claim, 0, "a refused claim reads nothing");
    assert!(read.is_ok(), "{read:?}");
    assert_eq!(
        admin_reads(&test).await.len(),
        1,
        "the document text was read"
    );
}

// ---- M14 Task 9b: the unlock check and the maintenance exemption ----

/// Makes user `staff` (from [`staff_root`]) an admin user, as unlocks need.
pub(super) async fn make_staff_admin(test: &TestApp) {
    let change = crate::store::users::AdminChange {
        on: true,
        allow_last: false,
    };
    crate::store::users::set_admin(
        test.app.accounts().executor(),
        "staff",
        change,
        &AuditActor::cli(),
        Utc::now(),
    )
    .await
    .unwrap();
}

/// A one-hour unlock of `catalog` for `staff`, its change message applied.
///
/// # Returns
/// The unlock's id.
pub(super) async fn unlock(test: &TestApp, catalog: &str) -> i64 {
    crate::invalidate::testing::applied(&test.app, create_unlock(test, catalog)).await
}

/// [`unlock`] without the poll.
async fn create_unlock(test: &TestApp, catalog: &str) -> i64 {
    create_unlock_at(test, catalog, Utc::now()).await
}

/// [`create_unlock`] made at `now` (an unlock made two hours ago has
/// expired), its change message not applied.
pub(super) async fn create_unlock_at(
    test: &TestApp,
    catalog: &str,
    now: chrono::DateTime<Utc>,
) -> i64 {
    use crate::store::unlocks::{self, Category, NewUnlock, UnlockPolicy};
    let new = NewUnlock {
        catalog,
        user: "staff",
        category: Category::Support,
        reason: "ticket 12",
        hours: 1,
    };
    let policy = UnlockPolicy {
        max_hours: 4,
        notify_owners: false,
        brand_defaults: crate::store::settings::Brand::default(),
    };
    let created = unlocks::create(
        test.app.accounts().executor(),
        &new,
        &policy,
        &AuditActor::cli(),
        now,
    )
    .await
    .unwrap();
    created.id
}

/// An unlock's (reads, writes, exports).
async fn tallies(test: &TestApp, id: i64) -> (i64, i64, i64) {
    let found = crate::store::unlocks::find(test.app.accounts().executor(), id)
        .await
        .unwrap();
    (found.reads, found.writes, found.exports)
}

/// `POST /api/v1/catalogs/main/import?replace=true` of `archive` as `key`.
async fn import_main(
    test: &TestApp,
    key: &str,
    archive: Vec<u8>,
) -> (StatusCode, serde_json::Value) {
    use tower::ServiceExt;
    let request = axum::http::Request::post("/api/v1/catalogs/main/import?replace=true")
        .header("authorization", format!("Bearer {key}"))
        .body(axum::body::Body::from(archive))
        .unwrap();
    let response = crate::http::router(
        std::sync::Arc::clone(&test.app),
        &tokio_util::sync::CancellationToken::new(),
    )
    .oneshot(request)
    .await
    .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}

/// The staff key's MCP context, as a tool call would build it.
async fn staff_context(test: &TestApp, secret: &str) -> crate::mcp::context::ToolContext {
    crate::mcp::testing::context(
        test,
        crate::mcp::testing::cached_principal(test, secret).await,
        None,
    )
}

/// Opens a changeset on `main` as `ctx`'s key.
async fn open_main(
    ctx: &crate::mcp::context::ToolContext,
) -> Result<String, crate::error::OkfError> {
    let params = crate::mcp::changesets::OpenParams {
        catalog: Some("main".to_string()),
    };
    crate::mcp::changesets::open_changeset(ctx, params)
        .await
        .map(|opened| opened.changeset)
}

#[tokio::test(flavor = "multi_thread")]
async fn with_require_unlock_staff_need_an_unlock_for_reads_writes_and_exports() {
    let test = app().await;
    let scoped = keys(&test).await;
    let staff = staff_root(&test).await;
    make_staff_admin(&test).await;
    require_unlock(&test).await;
    let tree = "/api/v1/catalogs/main/tree";
    let export = "/api/v1/catalogs/main/export";
    let archive = || crate::http::archives::tarball_of(&[("index.md", "# Main\n\nImported.\n")]);

    let (locked, refused) = send(&test, "GET", tree, Some(&staff), None).await;
    let (locked_export, _) = send(&test, "GET", export, Some(&staff), None).await;
    let (locked_import, import_refused) = import_main(&test, &staff, archive()).await;
    let locked_open = open_main(&staff_context(&test, &staff).await).await;
    let (member, _) = send(&test, "GET", tree, Some(&scoped.read), None).await;
    let (member_export, _) = send(&test, "GET", export, Some(&scoped.read), None).await;
    let (listing, _) = send(&test, "GET", "/api/v1/catalogs", Some(&staff), None).await;
    let (users, _) = send(&test, "GET", "/api/v1/users", Some(&staff), None).await;
    let exported_before =
        audit::testing::entries(test.app.accounts().executor(), "catalog.exported").await;

    assert_eq!(locked, StatusCode::FORBIDDEN);
    assert_eq!(refused["error"], "UnlockRequired");
    assert_eq!(refused["catalog"], "main");
    assert_eq!(
        locked_export,
        StatusCode::FORBIDDEN,
        "export needs an unlock too"
    );
    assert_eq!(
        (locked_import, import_refused["error"].as_str()),
        (StatusCode::FORBIDDEN, Some("UnlockRequired")),
        "so does import"
    );
    assert!(
        matches!(
            locked_open,
            Err(crate::error::OkfError::UnlockRequired { .. })
        ),
        "and opening a changeset: {locked_open:?}"
    );
    assert_eq!((member, member_export), (StatusCode::OK, StatusCode::OK));
    assert_eq!(listing, StatusCode::OK, "metadata needs none");
    assert_eq!(users, StatusCode::OK, "administration needs none");
    assert_eq!(
        exported_before.len(),
        1,
        "only the member's export is recorded"
    );

    let id = unlock(&test, "main").await;
    let (open, _) = send(&test, "GET", tree, Some(&staff), None).await;
    send(&test, "GET", tree, Some(&staff), None).await;
    let (exported, _) = send(&test, "GET", export, Some(&staff), None).await;
    let ctx = staff_context(&test, &staff).await;
    let handle = open_main(&ctx)
        .await
        .expect("an unlocked admin opens a changeset");
    let write = staff_context(&test, &staff)
        .await
        .changeset(&handle, Access::Write)
        .await
        .map(drop);
    let (imported, report) = import_main(&test, &staff, archive()).await;
    settle_access(&test).await;

    assert_eq!((open, exported), (StatusCode::OK, StatusCode::OK));
    assert!(write.is_ok(), "{write:?}");
    assert_eq!(imported, StatusCode::OK, "{report}");
    assert_eq!(
        tallies(&test, id).await,
        (2, 2, 1),
        "two reads; a changeset write and an import; one export"
    );
    assert!(
        admin_reads(&test).await.is_empty(),
        "the unlock's tallies replace the hourly record"
    );

    // Ended, the unlock lets nothing in, an open changeset included.
    crate::invalidate::testing::applied(&test.app, async {
        crate::store::unlocks::end(
            test.app.accounts().executor(),
            id,
            &AuditActor::cli(),
            Utc::now(),
        )
        .await
        .unwrap();
    })
    .await;
    let (ended, _) = send(&test, "GET", tree, Some(&staff), None).await;
    let ended_write = staff_context(&test, &staff)
        .await
        .changeset(&handle, Access::Write)
        .await
        .map(drop);
    settle_access(&test).await;

    assert_eq!(ended, StatusCode::FORBIDDEN);
    assert!(
        matches!(
            ended_write,
            Err(crate::error::OkfError::UnlockRequired { .. })
        ),
        "{ended_write:?}"
    );
    assert_eq!(
        tallies(&test, id).await,
        (2, 2, 1),
        "a refusal counts nothing"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unlock_of_one_catalog_opens_only_that_one_and_a_refused_call_queues_nothing() {
    let test = app().await;
    let staff = staff_root(&test).await;
    make_staff_admin(&test).await;
    require_unlock(&test).await;
    let id = unlock(&test, "main").await;

    let (both, refused) = send(
        &test,
        "GET",
        "/api/v1/search?q=alpha&catalogs=main,other",
        Some(&staff),
        None,
    )
    .await;
    let (other_tree, _) = send(
        &test,
        "GET",
        "/api/v1/catalogs/other/tree",
        Some(&staff),
        None,
    )
    .await;
    settle_access(&test).await;
    let after_refusals = tallies(&test, id).await;
    // Unnamed, the search leaves the locked catalog out and reads the other.
    let (unnamed, found) = send(&test, "GET", "/api/v1/search?q=alpha", Some(&staff), None).await;
    settle_access(&test).await;

    assert_eq!(both, StatusCode::FORBIDDEN);
    assert_eq!(
        (refused["error"].as_str(), refused["catalog"].as_str()),
        (Some("UnlockRequired"), Some("other"))
    );
    assert_eq!(
        other_tree,
        StatusCode::FORBIDDEN,
        "the unlock is of main only"
    );
    assert_eq!(
        after_refusals,
        (0, 0, 0),
        "a refused search reads, and counts, nothing, main's unlock included"
    );
    assert_eq!(unnamed, StatusCode::OK, "{found}");
    let hits = found["hits"].as_array().unwrap();
    assert!(
        !hits.is_empty() && hits.iter().all(|hit| hit["catalog"] == "main"),
        "{found}"
    );
    assert_eq!(tallies(&test, id).await, (1, 0, 0));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_maintenance_runner_reads_without_an_unlock_until_the_catalog_opts_out() {
    let test = app().await;
    staff_root(&test).await;
    // A reporting key of a user outside main's owner team.
    let runner = staff_key(&test, "runner", Access::Read, true).await;
    // A reporting key with write access: exempt for reads, not for writes.
    let writing_runner = staff_key(&test, "writing-runner", Access::Write, true).await;
    require_unlock(&test).await;
    let tree = "/api/v1/catalogs/main/tree";

    let export = "/api/v1/catalogs/main/export";
    let (exempt, _) = send(&test, "GET", tree, Some(&runner), None).await;
    let (exempt_export, _) = send(&test, "GET", export, Some(&runner), None).await;
    let (writer_reads, _) = send(&test, "GET", tree, Some(&writing_runner), None).await;
    let writer_opens = open_main(&staff_context(&test, &writing_runner).await).await;
    crate::invalidate::testing::applied(&test.app, async {
        let mut tx = test.app.accounts().executor().begin().await.unwrap();
        let id: i64 = sqlx::query_scalar(
            "UPDATE catalogs SET maintenance = false WHERE name = 'main' RETURNING id",
        )
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        let main = crate::invalidate::Invalidation::CatalogMeta {
            id,
            name: "main".to_owned(),
        };
        crate::invalidate::invalidate(&mut tx, &[main])
            .await
            .unwrap();
        tx.commit().await.unwrap();
    })
    .await;
    let (opted_out, refused) = send(&test, "GET", tree, Some(&runner), None).await;
    let (opted_out_export, _) = send(&test, "GET", export, Some(&runner), None).await;
    let (other, _) = send(
        &test,
        "GET",
        "/api/v1/catalogs/other/tree",
        Some(&runner),
        None,
    )
    .await;

    assert_eq!(exempt, StatusCode::OK, "reads need no unlock");
    assert_eq!(exempt_export, StatusCode::OK, "an export is a read");
    assert_eq!(writer_reads, StatusCode::OK);
    assert!(
        matches!(
            writer_opens,
            Err(crate::error::OkfError::UnlockRequired { .. })
        ),
        "writes get no exemption: {writer_opens:?}"
    );
    assert_eq!(opted_out, StatusCode::FORBIDDEN);
    assert_eq!(refused["error"], "Forbidden");
    assert_eq!(refused["maintenance"], "off");
    assert_eq!(opted_out_export, StatusCode::FORBIDDEN, "nor may it export");
    assert_eq!(other, StatusCode::OK, "only the catalog that opted out");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unlocked_read_of_an_old_catalog_map_is_refused_not_left_out() {
    let test = app().await;
    staff_root(&test).await;
    make_staff_admin(&test).await;
    // A non-admin key of the admin user: under `require_unlock` its reads
    // are let in, and counted, only by an unlock.
    let reader = staff_key(&test, "staff-reader", Access::Read, false).await;
    require_unlock(&test).await;
    let original = catalog_id(&test, "main").await;
    // `main` is cached (its row and its snapshot) before it is replaced.
    test.app.catalog("main").await.unwrap().expect("main");
    replace_main_for_acme(&test).await;
    let id = create_unlock(&test, "main").await;
    // This call resolves the old `main` (its row and snapshot are cached)
    // before the replacement's change message reaches the directory.
    let ctx = crate::mcp::testing::context(
        &test,
        crate::mcp::testing::cached_principal(&test, &reader).await,
        None,
    );
    ctx.state("main").await.unwrap();
    tell_directory(&test, "main").await;
    assert_eq!(
        ctx.state("main").await.unwrap().id(),
        original,
        "the old map"
    );

    // `one` checks the incarnation (PR #442 review, round 2), and `admit`
    // on the map's state refuses it the same way.
    let state = ctx.state("main").await.unwrap();
    let selected = ctx.one(Some("main")).await.map(drop);
    let refused = ctx.admit(&state, audit::AccessKind::Read).await;
    assert!(
        matches!(selected, Err(crate::error::OkfError::Busy { .. })),
        "{selected:?}"
    );
    settle_access(&test).await;

    assert!(
        matches!(refused, Err(crate::error::OkfError::Busy { .. })),
        "{refused:?}"
    );
    assert_eq!(
        tallies(&test, id).await,
        (0, 0, 0),
        "nothing counted against the wrong catalog"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_changeset_list_leaves_out_a_catalog_whose_unlock_ended() {
    let test = app().await;
    let staff = staff_root(&test).await;
    make_staff_admin(&test).await;
    require_unlock(&test).await;
    unlock(&test, "main").await;
    let other = unlock(&test, "other").await;
    let ctx = staff_context(&test, &staff).await;
    for catalog in ["main", "other"] {
        let params = crate::mcp::changesets::OpenParams {
            catalog: Some(catalog.to_string()),
        };
        crate::mcp::changesets::open_changeset(&ctx, params)
            .await
            .unwrap();
    }
    crate::invalidate::testing::applied(&test.app, async {
        crate::store::unlocks::end(
            test.app.accounts().executor(),
            other,
            &AuditActor::cli(),
            Utc::now(),
        )
        .await
        .unwrap();
    })
    .await;
    let listed = async |catalog: Option<&str>| {
        let ctx = staff_context(&test, &staff).await;
        let params = crate::mcp::changesets::ListParams {
            catalog: catalog.map(str::to_string),
        };
        crate::mcp::changesets::list_changesets(&ctx, params).await
    };

    let unnamed = listed(None).await.unwrap();
    let named = listed(Some("other")).await;

    let catalogs: Vec<&str> = unnamed
        .changesets
        .iter()
        .map(|changeset| changeset.catalog.as_str())
        .collect();
    assert_eq!(catalogs, ["main"], "its handles and paths stay locked");
    assert!(
        matches!(named, Err(crate::error::OkfError::UnlockRequired { .. })),
        "{named:?}"
    );
}

/// Unlocks `main` for an admin `staff`, then replaces `main` with a
/// one-document catalog of team `acme` without polling or applying any
/// change message, so the caches still admit through the old catalog's
/// unlock (PR #442 review).
///
/// # Returns
/// The staff key's secret, the old unlock's id and the replacement's id.
async fn stale_unlock_of_a_replaced_main(test: &TestApp) -> (String, i64, i64) {
    let staff = staff_root(test).await;
    make_staff_admin(test).await;
    require_unlock(test).await;
    let id = unlock(test, "main").await;
    // The caches hold the old `main`'s row and the user's unlock of it; no
    // change message reaches them after the replacement (lib tests run no
    // listener), as on a replica whose messages are late.
    test.app.directory().resolve("main").await.unwrap();
    crate::mcp::testing::cached_principal(test, &staff).await;
    replace_main_for_acme(test).await;
    let replacement = catalog_id(test, "main").await;
    import_private_index(test).await;
    (staff, id, replacement)
}

/// The replacement's (generation, live document paths and contents).
async fn replacement_contents(test: &TestApp, id: i64) -> (i64, Vec<(String, Vec<u8>)>) {
    let generation =
        sqlx::query_scalar("SELECT generation FROM catalog.catalog_state WHERE id = $1")
            .bind(id)
            .fetch_one(&crate::db::store_pool(test.app.accounts().executor()))
            .await
            .unwrap();
    let documents = sqlx::query_as(
        "SELECT path, content FROM catalog.documents \
         WHERE catalog_id = $1 AND deleted_at IS NULL ORDER BY path",
    )
    .bind(id)
    .fetch_all(&crate::db::store_pool(test.app.accounts().executor()))
    .await
    .unwrap();
    (generation, documents)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stale_unlock_exports_nothing_of_the_catalog_that_replaced_its_own() {
    let test = app().await;
    let (staff, id, replacement) = stale_unlock_of_a_replaced_main(&test).await;
    // Still the old row: the directory was told nothing.
    let cached = test.app.directory().resolve("main").await.unwrap().unwrap();
    assert_ne!(cached.id, replacement);

    let (status, body) = send(
        &test,
        "GET",
        "/api/v1/catalogs/main/export",
        Some(&staff),
        None,
    )
    .await;
    settle_access(&test).await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"], "Busy");
    let exported =
        audit::testing::entries(test.app.accounts().executor(), "catalog.exported").await;
    assert!(
        exported.is_empty(),
        "nothing of the replacement left: {exported:?}"
    );
    assert_eq!(
        tallies_or_gone(&test, id).await,
        None,
        "the old unlock went with its catalog"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stale_unlock_cannot_import_over_the_catalog_that_replaced_its_own() {
    let test = app().await;
    let (staff, _, replacement) = stale_unlock_of_a_replaced_main(&test).await;
    let before = replacement_contents(&test, replacement).await;

    let overwrite = crate::http::archives::tarball_of(&[("index.md", "# Overwritten\n")]);
    let (status, body) = import_main(&test, &staff, overwrite).await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"], "Busy");
    assert_eq!(
        replacement_contents(&test, replacement).await,
        before,
        "the replacement is untouched"
    );
}

/// An unlock's tallies, or `None` once it is gone (deleted with its catalog).
async fn tallies_or_gone(test: &TestApp, id: i64) -> Option<(i64, i64, i64)> {
    sqlx::query_as("SELECT reads, writes, exports FROM unlocks WHERE id = $1")
        .bind(id)
        .fetch_optional(test.app.accounts().executor())
        .await
        .unwrap()
}

/// An unlocked admin `staff` whose principal was captured before `main`
/// was replaced by `acme`'s one-document catalog holding an open changeset,
/// then polled: the old principal (its gates hold the old `main`'s unlock)
/// against the new catalog map, as a request in flight across a poll sees
/// them (PR #442 review, round 2).
///
/// # Returns
/// The old principal, the replacement's id and its changeset's handle.
async fn old_unlock_against_a_new_map(
    test: &TestApp,
) -> (std::sync::Arc<crate::auth::Principal>, i64, String) {
    let staff = staff_root(test).await;
    make_staff_admin(test).await;
    require_unlock(test).await;
    unlock(test, "main").await;
    let old_principal = crate::mcp::testing::cached_principal(test, &staff).await;
    // The replacement and its document, without a poll in between.
    replace_main_for_acme(test).await;
    import_private_index(test).await;
    let replacement = catalog_id(test, "main").await;
    crate::mcp::testing::store_key(test, "acme-writer").await;
    let mut conn = test.app.main_db().await.executor().acquire().await.unwrap();
    let opened = crate::store::changesets::open(
        &mut conn,
        replacement,
        "acme-writer",
        "acme-writer",
        Utc::now(),
    )
    .await
    .unwrap();
    drop(conn);
    // Everything the replacement changed is read again, as after a
    // change-feed reconnect.
    test.app.apply_reconnect().await;
    (old_principal, replacement, opened.handle)
}

#[tokio::test(flavor = "multi_thread")]
async fn an_old_unlocked_principal_opens_no_changeset_in_the_replacement() {
    let test = app().await;
    let (old_principal, replacement, _) = old_unlock_against_a_new_map(&test).await;
    let ctx = crate::mcp::testing::context(&test, old_principal, None);
    assert_eq!(
        ctx.state("main").await.unwrap().id(),
        replacement,
        "the new map"
    );

    let refused = open_main(&ctx).await;
    let opened: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM catalog.changesets WHERE catalog_id = $1 AND key_id <> 'acme-writer'",
    )
    .bind(replacement)
    .fetch_one(&crate::db::store_pool(test.app.accounts().executor()))
    .await
    .unwrap();

    assert!(
        matches!(refused, Err(crate::error::OkfError::UnlockRequired { .. })),
        "{refused:?}"
    );
    assert_eq!(opened, 0, "nothing opened in the replacement");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_old_unlocked_principal_lists_none_of_the_replacements_changesets() {
    let test = app().await;
    let (old_principal, _, handle) = old_unlock_against_a_new_map(&test).await;
    let list = |catalog: Option<&str>| {
        let ctx = crate::mcp::testing::context(&test, std::sync::Arc::clone(&old_principal), None);
        let params = crate::mcp::changesets::ListParams {
            catalog: catalog.map(str::to_string),
        };
        async move { crate::mcp::changesets::list_changesets(&ctx, params).await }
    };

    let unnamed = list(None).await.unwrap();
    let named = list(Some("main")).await;

    assert!(
        unnamed
            .changesets
            .iter()
            .all(|listed| listed.changeset != handle),
        "unnamed: {unnamed:?}"
    );
    assert!(
        matches!(named, Err(crate::error::OkfError::UnlockRequired { .. })),
        "named: {named:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_old_unlocked_principal_reads_none_of_the_replacements_contents() {
    use axum::Extension;
    use tower::ServiceExt;
    let test = app().await;
    let (old_principal, _, _) = old_unlock_against_a_new_map(&test).await;
    let ctx = crate::mcp::testing::context(&test, std::sync::Arc::clone(&old_principal), None);

    let one = ctx.one(Some("main")).await.map(drop);
    // `view` on the new map's state, as a tool that resolved it otherwise
    // would: refused by the gates that know it, not read unrecorded.
    let state = ctx.state("main").await.unwrap();
    let viewed = ctx.view(&state, None).await.map(drop);
    let read = crate::mcp::documents::read_document(
        &ctx,
        crate::mcp::documents::ReadDocumentParams {
            path: Some("/index.md".to_string()),
            catalog: Some("main".to_string()),
            changeset: None,
        },
    )
    .await;
    // The REST browse routes, with the old principal as the auth layer
    // would have put it in the request.
    let routes = super::browse::routes()
        .layer(Extension(std::sync::Arc::clone(&old_principal)))
        .with_state(std::sync::Arc::clone(&test.app));
    let mut rest = Vec::new();
    for uri in ["/catalogs/main/tree", "/catalogs/main/documents/index.md"] {
        let request = axum::http::Request::get(uri)
            .body(axum::body::Body::empty())
            .unwrap();
        let response = routes.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        rest.push((uri, status, String::from_utf8_lossy(&bytes).into_owned()));
    }
    settle_access(&test).await;

    let locked = |result: &Result<(), crate::error::OkfError>| {
        matches!(result, Err(crate::error::OkfError::UnlockRequired { .. }))
    };
    assert!(locked(&one), "one: {one:?}");
    assert!(locked(&viewed), "view: {viewed:?}");
    assert!(
        matches!(read, Err(crate::error::OkfError::UnlockRequired { .. })),
        "read_document: {read:?}"
    );
    for (uri, status, body) in &rest {
        assert_eq!(*status, StatusCode::FORBIDDEN, "{uri}: {body}");
        assert!(!body.contains("Private"), "{uri} leaked: {body}");
    }
}

/// Imports one private `index.md` into the current `main` (the CLI's way:
/// no admission), so a test can tell the replacement's contents apart.
async fn import_private_index(test: &TestApp) {
    let archive = crate::http::archives::tarball_of(&[("index.md", "# Acme\n\nPrivate.\n")]);
    let options = crate::store::import::ImportOptions {
        replace: true,
        scope: crate::store::import::ImportScope::Everything,
        expect_generation: None,
        expect_catalog_id: None,
        actor: "cli",
        audit: &AuditActor::cli(),
        now: Utc::now(),
    };
    let files = crate::archive::read_tarball(archive.as_slice(), 1 << 20).unwrap();
    crate::store::import::import(&test.app.main_db().await, "main", files, &options)
        .await
        .unwrap();
}
