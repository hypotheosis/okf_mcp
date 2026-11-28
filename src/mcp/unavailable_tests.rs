//! Reads across stores when one store cannot be reached (plan Task 18, P3).
//!
//! Every test puts a catalog on a store of its own (a second database) and
//! then makes that store really unreachable: its row points at a closed port
//! and its pool is dropped, so a connect is refused. Nothing here sleeps; each
//! call is bounded by a timeout.

use std::future::Future;
use std::time::Duration;

use crate::config::Config;
use crate::error::OkfError;
use crate::mcp::changesets::{list_changesets, open_changeset, ListParams, OpenParams};
use crate::mcp::documents::{get_concept, read_document, GetConceptParams, ReadDocumentParams};
use crate::mcp::history::{history, HistoryParams};
use crate::mcp::search::{run, SearchParams};
use crate::mcp::testing::{app_with, context, open, principal, principal_as, writer, TestApp};
use crate::store::keys::Access;

/// Longest any call in these tests may take.
const BOUND: Duration = Duration::from_secs(30);

const HOME_NOTE: &str = "---\ntype: Note\ntitle: Home\n---\nAlpha at home.\n";
const REMOTE_NOTE: &str = "---\ntype: Note\ntitle: Remote\n---\nAlpha far away.\n";

/// An app with catalog `home` on `main` and catalog `remote` on a store of
/// its own that cannot be reached.
async fn app_with_a_lost_store() -> TestApp {
    let test = app_with(&[("home", &[("note.md", HOME_NOTE)])], Config::default()).await;
    let store = test
        .catalog_on_new_store(
            "store-b",
            "remote",
            &[("note.md", REMOTE_NOTE), ("far.md", REMOTE_NOTE)],
        )
        .await;
    test.make_unreachable(&store).await;
    test
}

/// Runs `call` under the test bound.
async fn bounded<T>(call: impl Future<Output = T>) -> T {
    tokio::time::timeout(BOUND, call)
        .await
        .expect("the call finished within the bound, not hanging on the lost store")
}

fn search_for(query: &str, catalogs: &[&str]) -> SearchParams {
    SearchParams {
        query: query.to_string(),
        catalogs: Some(catalogs.iter().map(|name| name.to_string()).collect()),
        concept_type: None,
        tags: Vec::new(),
        status: None,
        limit: None,
        changeset: None,
    }
}

fn concept(path: &str) -> GetConceptParams {
    GetConceptParams {
        path: path.to_string(),
        catalog: None,
        links: false,
        raw: false,
        section: None,
        full: false,
        changeset: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn search_returns_other_stores_results_and_names_the_unavailable_catalog() {
    let test = app_with_a_lost_store().await;
    let ctx = context(&test, principal(&[], None), None);

    let result = bounded(run(&ctx, search_for("alpha", &["home", "remote"])))
        .await
        .unwrap();

    let found: Vec<&str> = result
        .hits
        .iter()
        .map(|hit| hit.hit.catalog.as_str())
        .collect();
    assert_eq!(found, ["home"]);
    assert_eq!(result.unavailable, ["remote"]);
    // Two lost catalogs on one store are named once each, sorted.
    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["unavailable"], serde_json::json!(["remote"]));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_unavailable_field_is_left_out_when_every_store_answers() {
    let test = app_with_a_lost_store().await;
    let ctx = context(&test, principal(&[], None), None);

    let result = bounded(run(&ctx, search_for("alpha", &["home"])))
        .await
        .unwrap();

    assert!(result.unavailable.is_empty());
    let json = serde_json::to_value(&result).unwrap();
    assert!(json.get("unavailable").is_none(), "{json}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_catalog_the_key_does_not_reach_is_never_named_unavailable() {
    let test = app_with_a_lost_store().await;
    // The key reaches `home` only: `remote` is on the lost store.
    let ctx = context(&test, principal(&["home"], None), None);

    let named = bounded(run(&ctx, search_for("alpha", &["home", "remote"]))).await;
    let default = bounded(run(&ctx, search_for("alpha", &["home"])))
        .await
        .unwrap();

    // The refusal is the one for a catalog that does not exist.
    assert!(
        matches!(named, Err(OkfError::CatalogNotSelected { .. })),
        "{named:?}"
    );
    assert!(default.unavailable.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn list_catalogs_leaves_out_and_names_the_unavailable_catalog() {
    let test = app_with_a_lost_store().await;
    let ctx = context(&test, principal(&[], None), None);

    let listed = bounded(crate::mcp::catalogs::list(&ctx, None))
        .await
        .unwrap();

    let names: Vec<&str> = listed
        .catalogs
        .iter()
        .map(|info| info.name.as_str())
        .collect();
    assert_eq!(names, ["home"]);
    assert_eq!(listed.unavailable, ["remote"]);
    let reach_home = context(&test, principal(&["home"], None), None);
    let narrow = bounded(crate::mcp::catalogs::list(&reach_home, None))
        .await
        .unwrap();
    assert!(narrow.unavailable.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn get_concept_fails_when_an_earlier_catalogs_store_is_unavailable() {
    let test = app_with_a_lost_store().await;
    // `home` holds `note.md`, but `remote` comes first and might have held it.
    let remote_first = context(&test, principal(&[], None), Some(&["remote", "home"]));
    let home_first = context(&test, principal(&[], None), Some(&["home", "remote"]));

    let blocked = bounded(get_concept(&remote_first, concept("note"))).await;
    let blocked_history = bounded(history(
        &remote_first,
        HistoryParams {
            path: "note".to_string(),
            catalog: None,
            limit: None,
            user: None,
            since: None,
            before: None,
        },
    ))
    .await;
    let blocked_document = bounded(read_document(
        &remote_first,
        ReadDocumentParams {
            path: Some("/index.md".to_string()),
            catalog: None,
            changeset: None,
        },
    ))
    .await;
    // An earlier catalog that has it answers; the lost one is never asked.
    let answered = bounded(get_concept(&home_first, concept("note")))
        .await
        .unwrap();
    // A path no earlier catalog has: the lost one might, so not "not found".
    let missing = bounded(get_concept(&home_first, concept("nowhere"))).await;

    assert!(
        matches!(blocked, Err(OkfError::StoreUnavailable { .. })),
        "{blocked:?}"
    );
    assert!(matches!(
        blocked_history,
        Err(OkfError::StoreUnavailable { .. })
    ));
    assert!(matches!(
        blocked_document,
        Err(OkfError::StoreUnavailable { .. })
    ));
    assert_eq!(answered.catalog, "home");
    assert!(
        matches!(missing, Err(OkfError::StoreUnavailable { .. })),
        "{missing:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_write_to_an_unavailable_store_fails_with_store_unavailable() {
    let test = app_with_a_lost_store().await;
    let ctx = writer(&test, "writer", "remote").await;

    let opened = bounded(open_changeset(
        &ctx,
        OpenParams {
            catalog: Some("remote".to_string()),
        },
    ))
    .await;

    assert!(
        matches!(opened, Err(OkfError::StoreUnavailable { .. })),
        "{opened:?}"
    );
    // The message never names the store.
    assert!(!opened.unwrap_err().to_string().contains("store-b"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_changeset_handle_on_a_lost_store_is_unavailable_and_others_still_resolve() {
    let test = app_with(&[("home", &[("note.md", HOME_NOTE)])], Config::default()).await;
    let ctx = writer(&test, "writer", "home").await;
    let on_main = open(&ctx).await;
    let store = test
        .catalog_on_new_store("store-b", "remote", &[("note.md", REMOTE_NOTE)])
        .await;
    let remote_ctx = context(
        &test,
        crate::mcp::testing::principal_as("writer", Access::Write, &[], Some("remote")),
        None,
    );
    let on_remote = open(&remote_ctx).await;
    test.make_unreachable(&store).await;
    let fresh = ctx.fresh();

    // `main` is asked first, so its handle resolves without touching the lost store.
    let found = bounded(fresh.changeset(&on_main, Access::Write)).await;
    // The other handle is on the lost store: a failure, not "unknown".
    let lost = bounded(ctx.fresh().changeset(&on_remote, Access::Write)).await;
    // A handle that is nowhere cannot be told from one on the lost store.
    let unknown = bounded(
        ctx.fresh()
            .changeset("00000000-0000-0000-0000-000000000000", Access::Write),
    )
    .await;
    let listed = bounded(list_changesets(&ctx.fresh(), ListParams { catalog: None }))
        .await
        .unwrap();

    assert!(found.is_ok(), "{:?}", found.err());
    assert!(
        matches!(lost, Err(OkfError::StoreUnavailable { .. })),
        "{:?}",
        lost.err()
    );
    assert!(
        matches!(unknown, Err(OkfError::StoreUnavailable { .. })),
        "{:?}",
        unknown.err()
    );
    assert_eq!(listed.unavailable, ["remote"]);
    assert_eq!(
        listed.changesets.len(),
        1,
        "main's changeset is still listed"
    );
}

/// A config whose connects give up quickly, so a stalled store costs a test
/// about a second per probe.
fn quick_config(default_catalog: Option<&str>) -> Config {
    Config {
        store_connect_timeout: Duration::from_secs(1),
        default_catalog: default_catalog.map(str::to_string),
        ..Config::default()
    }
}

/// An app with catalog `main` on the default store and `remote` on a store that accepts
/// connections and never answers, and how many connections a single failed
/// attempt to open that store makes (the cost of one probe).
async fn app_with_a_stalled_store(
    config: Config,
) -> (TestApp, crate::mcp::testing::StalledStore, usize) {
    let test = app_with(&[("main", &[("note.md", HOME_NOTE)])], config).await;
    let store = test
        .catalog_on_new_store("store-b", "remote", &[("note.md", REMOTE_NOTE)])
        .await;
    let stalled = test.make_stalled(&store).await;
    let before = stalled.accepted();
    let opened = bounded(test.app.stores().get(&store)).await;
    assert!(
        matches!(opened, Err(OkfError::StoreUnavailable { .. })),
        "{:?}",
        opened.err()
    );
    let one_probe = stalled.accepted() - before;
    assert!(one_probe >= 1, "a probe reaches the stalled server");
    (test, stalled, one_probe)
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unnamed_console_search_probes_a_lost_store_once() {
    use crate::http::api::testing::{every_catalog_admin, send};
    let (test, stalled, one_probe) = app_with_a_stalled_store(quick_config(None)).await;
    let key = every_catalog_admin(&test).await;
    let before = stalled.accepted();

    let (status, body) = bounded(send(
        &test,
        "GET",
        "/api/v1/search?q=alpha",
        Some(&key),
        None,
    ))
    .await;

    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    assert_eq!(body["unavailable"], serde_json::json!(["remote"]), "{body}");
    assert_eq!(
        stalled.accepted() - before,
        one_probe,
        "the page's probe is the only one: selection and loads reuse it"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn list_catalogs_probes_a_lost_selected_store_once() {
    // `remote` is the default catalog, so the selection names it.
    let (test, stalled, one_probe) = app_with_a_stalled_store(quick_config(Some("remote"))).await;
    let ctx = context(&test, principal(&[], None), None);
    let before = stalled.accepted();

    let listed = bounded(crate::mcp::catalogs::list(&ctx, None))
        .await
        .unwrap();

    assert_eq!(listed.unavailable, ["remote"]);
    assert_eq!(
        stalled.accepted() - before,
        one_probe,
        "the page's probe is the only one: the selection reuses it"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn list_catalogs_with_two_granted_catalogs_probes_a_lost_store_once() {
    // No key or server default and no connection list: the selection falls
    // back to the key's grants, which the page already read (issue #1017).
    let (test, stalled, one_probe) = app_with_a_stalled_store(quick_config(None)).await;
    let ctx = context(&test, principal(&["main", "remote"], None), None);
    let before = stalled.accepted();

    let listed = bounded(crate::mcp::catalogs::list(&ctx, None))
        .await
        .unwrap();

    let names: Vec<&str> = listed.catalogs.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["main"]);
    assert_eq!(listed.unavailable, ["remote"]);
    assert_eq!(
        stalled.accepted() - before,
        one_probe,
        "the page's probe is the only one: the grant fallback reuses it"
    );
    // The selection is the same as without the saving: both granted, so no
    // single catalog is chosen.
    assert!(
        listed.selection_error.is_some(),
        "{:?}",
        listed.selection_error
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn list_changesets_with_a_connection_list_probes_a_lost_store_once() {
    let (test, stalled, one_probe) = app_with_a_stalled_store(quick_config(None)).await;
    let ctx = context(
        &test,
        principal_as("writer", Access::Write, &[], None),
        Some(&["main", "remote"]),
    );
    let open_on_main = open(&writer(&test, "writer", "main").await).await;
    let before = stalled.accepted();

    let listed = bounded(list_changesets(&ctx, ListParams { catalog: None }))
        .await
        .unwrap();

    assert_eq!(listed.unavailable, ["remote"]);
    let handles: Vec<&str> = listed
        .changesets
        .iter()
        .map(|c| c.changeset.as_str())
        .collect();
    assert_eq!(handles, [open_on_main.as_str()]);
    assert_eq!(
        stalled.accepted() - before,
        one_probe,
        "check_connection's probe is the only one: the fan-out reuses it"
    );
}

/// A handle no store holds: the lookup has to ask every store.
const UNHELD_HANDLE: &str = "00000000-0000-4000-8000-000000000000";

#[tokio::test(flavor = "multi_thread")]
async fn changeset_by_handle_probes_a_lost_store_once() {
    let (test, stalled, one_probe) = app_with_a_stalled_store(quick_config(None)).await;
    let ctx = context(
        &test,
        principal_as("writer", Access::Write, &[], None),
        Some(&["main", "remote"]),
    );
    let before = stalled.accepted();

    let result = bounded(ctx.changeset(UNHELD_HANDLE, Access::Read)).await;

    // The handle might be on the lost store, so the answer is unavailable.
    assert!(
        matches!(result, Err(OkfError::StoreUnavailable { .. })),
        "{:?}",
        result.err()
    );
    assert_eq!(
        stalled.accepted() - before,
        one_probe,
        "check_connection's probe is the only one: the handle lookup reuses it"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lost_store_is_reached_again_by_a_later_call_once_it_is_back() {
    let test = app_with(&[("main", &[("note.md", HOME_NOTE)])], quick_config(None)).await;
    let store = test
        .catalog_on_new_store("store-b", "remote", &[("note.md", REMOTE_NOTE)])
        .await;
    let (_host, port): (String, i32) =
        sqlx::query_as("SELECT host, port FROM stores WHERE name = $1")
            .bind(store.as_str())
            .fetch_one(test.app.accounts().executor())
            .await
            .expect("read the store's address");
    let on_remote = open(&writer(&test, "writer", "remote").await).await;
    let call = || {
        context(
            &test,
            principal_as("writer", Access::Write, &[], None),
            Some(&["main", "remote"]),
        )
    };
    // The store goes away.
    let stalled = test.make_stalled(&store).await;
    let before = stalled.accepted();

    let during = bounded(call().changeset(&on_remote, Access::Read)).await;

    assert!(
        matches!(during, Err(OkfError::StoreUnavailable { .. })),
        "{:?}",
        during.err()
    );
    assert!(stalled.accepted() > before, "the lost store was probed");

    // It comes back; nothing else changes. A new call finds the handle, so
    // the first call's finding was not remembered.
    test.repoint(&store, u16::try_from(port).expect("a port"))
        .await;
    let after = bounded(call().changeset(&on_remote, Access::Read))
        .await
        .expect("the store is reached again");
    assert_eq!(after.row.handle, on_remote);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_store_that_goes_away_after_its_pool_opened_is_unavailable_not_a_database_error() {
    let test = app_with(&[("home", &[("note.md", HOME_NOTE)])], quick_config(None)).await;
    let store = test
        .catalog_on_new_store("store-b", "remote", &[("note.md", REMOTE_NOTE)])
        .await;
    let proxy = test.front_with_a_proxy(&store).await;
    // The directory row is cached and the pool is open; only the state is
    // not loaded. Then the store goes away.
    test.app
        .catalog_row("remote")
        .await
        .unwrap()
        .expect("the row");
    proxy.stop().await;
    let ctx = context(&test, principal(&[], None), None);

    let result = bounded(run(&ctx, search_for("alpha", &["home", "remote"])))
        .await
        .unwrap();

    let found: Vec<&str> = result
        .hits
        .iter()
        .map(|hit| hit.hit.catalog.as_str())
        .collect();
    assert_eq!(found, ["home"]);
    assert_eq!(result.unavailable, ["remote"]);
}

/// A catalog `remote` on its own store whose state is loaded and cached, and
/// the proxy that, once stopped, takes the store away: what a read does that
/// asks the store again after the state is in hand (issue #995).
async fn app_with_a_loaded_store_behind_a_proxy() -> (TestApp, crate::mcp::testing::StoreProxy) {
    let test = app_with(&[("main", &[("note.md", HOME_NOTE)])], quick_config(None)).await;
    let store = test
        .catalog_on_new_store("store-b", "remote", &[("note.md", REMOTE_NOTE)])
        .await;
    let proxy = test.front_with_a_proxy(&store).await;
    let ctx = context(&test, principal(&[], None), Some(&["remote"]));
    // Loads and caches the catalog's state, so the next calls need the
    // store only for what the state does not hold.
    bounded(get_concept(&ctx, concept("note"))).await.unwrap();
    (test, proxy)
}

#[tokio::test(flavor = "multi_thread")]
async fn history_on_a_store_that_went_away_is_unavailable_not_a_database_error() {
    let (test, proxy) = app_with_a_loaded_store_behind_a_proxy().await;
    let ctx = context(&test, principal(&[], None), Some(&["remote"]));
    proxy.stop().await;

    let result = bounded(history(
        &ctx,
        HistoryParams {
            path: "note".to_string(),
            catalog: Some("remote".to_string()),
            limit: None,
            user: None,
            since: None,
            before: None,
        },
    ))
    .await;

    assert!(
        matches!(result, Err(OkfError::StoreUnavailable { .. })),
        "{result:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_history_route_on_a_store_that_went_away_answers_unavailable() {
    use crate::http::api::testing::{every_catalog_admin, send};
    let (test, proxy) = app_with_a_loaded_store_behind_a_proxy().await;
    let key = every_catalog_admin(&test).await;
    proxy.stop().await;

    let (status, body) = bounded(send(
        &test,
        "GET",
        "/api/v1/catalogs/remote/history/note.md",
        Some(&key),
        None,
    ))
    .await;

    assert_eq!(
        status,
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "{body}"
    );
    assert_eq!(body["error"], "StoreUnavailable", "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_store_whose_query_stalls_is_unavailable_within_the_bound() {
    let test = app_with(
        &[("home", &[("note.md", HOME_NOTE)])],
        Config {
            store_connect_timeout: Duration::from_millis(300),
            ..Config::default()
        },
    )
    .await;
    let store = test
        .catalog_on_new_store("store-b", "remote", &[("note.md", REMOTE_NOTE)])
        .await;
    test.app
        .catalog_row("remote")
        .await
        .unwrap()
        .expect("the row");
    // A lock on the catalog's documents that nobody releases: the load's
    // query waits for it, on a connection that is up.
    let db = test.app.stores().get(&store).await.unwrap();
    let mut lock = db.pool().begin().await.unwrap();
    sqlx::query("LOCK TABLE documents IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .unwrap();
    let ctx = context(&test, principal(&[], None), None);

    let result = bounded(run(&ctx, search_for("alpha", &["home", "remote"])))
        .await
        .unwrap();

    assert_eq!(result.unavailable, ["remote"]);
    assert_eq!(result.hits.len(), 1);
    lock.rollback().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn list_catalogs_spends_one_bound_on_a_store_that_fails_at_the_state_load() {
    let connect = Duration::from_millis(300);
    let test = app_with(
        &[("home", &[("note.md", HOME_NOTE)])],
        Config {
            store_connect_timeout: connect,
            ..Config::default()
        },
    )
    .await;
    let store = test
        .catalog_on_new_store("store-b", "remote-1", &[("note.md", REMOTE_NOTE)])
        .await;
    test.catalog_on_store(&store, "remote-2", &[("note.md", REMOTE_NOTE)])
        .await;
    // The store answers the directory's probe but its documents are locked:
    // each catalog's state load would stall for the whole load bound.
    let db = test.app.stores().get(&store).await.unwrap();
    let mut lock = db.pool().begin().await.unwrap();
    sqlx::query("LOCK TABLE documents IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .unwrap();
    let ctx = context(&test, principal(&[], None), None);
    let load_bound = test.app.stores().state_load_timeout();
    let started = std::time::Instant::now();

    let listed = bounded(crate::mcp::catalogs::list(&ctx, None))
        .await
        .unwrap();

    assert_eq!(listed.unavailable, ["remote-1", "remote-2"]);
    // One stalled load, not one per catalog on the store.
    assert!(
        started.elapsed() < load_bound * 3 / 2,
        "took {:?} for a load bound of {load_bound:?}",
        started.elapsed()
    );
    lock.rollback().await.unwrap();
}

/// Takes an exclusive lock on the store's documents, so a query on them
/// waits on a connection that is up. Roll the returned transaction back to
/// release it.
async fn lock_documents(
    test: &TestApp,
    store: &crate::stores::StoreName,
) -> sqlx::Transaction<'static, sqlx::Postgres> {
    let db = test.app.stores().get(store).await.unwrap();
    let mut lock = db.pool().begin().await.unwrap();
    sqlx::query("LOCK TABLE documents IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .unwrap();
    lock
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refresh_that_stalls_is_unavailable_not_a_stale_snapshot() {
    let test = app_with(
        &[("home", &[("note.md", HOME_NOTE)])],
        Config {
            store_connect_timeout: Duration::from_millis(300),
            ..Config::default()
        },
    )
    .await;
    let store = test
        .catalog_on_new_store("store-b", "remote", &[("note.md", REMOTE_NOTE)])
        .await;
    let ctx = context(&test, principal(&[], None), Some(&["remote", "home"]));
    // Load the state while the store answers.
    let before = bounded(run(&ctx, search_for("alpha", &["remote"])))
        .await
        .unwrap();
    assert!(before.unavailable.is_empty() && !before.hits.is_empty());
    // The catalog moves on, then the store stops answering its queries.
    let id = test.app.catalog_row("remote").await.unwrap().unwrap().id;
    let db = test.app.stores().get(&store).await.unwrap();
    sqlx::query("UPDATE catalog_state SET generation = generation + 1 WHERE id = $1")
        .bind(id)
        .execute(db.executor())
        .await
        .unwrap();
    test.app.directory().mark_all_stale().await;
    let lock = lock_documents(&test, &store).await;
    let fresh = context(&test, principal(&[], None), Some(&["remote", "home"]));

    let searched = bounded(run(&fresh, search_for("alpha", &["home", "remote"])))
        .await
        .unwrap();
    let blocked = bounded(get_concept(&fresh, concept("nowhere"))).await;

    assert_eq!(searched.unavailable, ["remote"]);
    assert_eq!(searched.hits.len(), 1, "only home answers");
    assert!(
        matches!(blocked, Err(OkfError::StoreUnavailable { .. })),
        "{blocked:?}"
    );
    lock.rollback().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_searches_on_an_uncached_row_of_a_stalled_store_both_get_partial_results() {
    let (test, _stalled, _) = app_with_a_stalled_store(quick_config(None)).await;
    // The probe above did not cache the row: both searches must resolve it.
    let first = context(&test, principal(&[], None), None);
    let second = context(&test, principal(&[], None), None);

    let (one, two) = bounded(async {
        tokio::join!(
            run(&first, search_for("alpha", &["main", "remote"])),
            run(&second, search_for("alpha", &["main", "remote"]))
        )
    })
    .await;

    for result in [one, two] {
        let result = result.expect("a lost store is named, not a failure");
        assert_eq!(result.unavailable, ["remote"]);
        assert_eq!(result.hits.len(), 1);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_searches_on_a_cached_row_with_a_stalled_state_both_get_partial_results() {
    let test = app_with(
        &[("home", &[("note.md", HOME_NOTE)])],
        Config {
            store_connect_timeout: Duration::from_millis(300),
            ..Config::default()
        },
    )
    .await;
    let store = test
        .catalog_on_new_store("store-b", "remote", &[("note.md", REMOTE_NOTE)])
        .await;
    // The row is cached; its state is not loaded, and its load will stall.
    test.app
        .catalog_row("remote")
        .await
        .unwrap()
        .expect("the row");
    let lock = lock_documents(&test, &store).await;
    let first = context(&test, principal(&[], None), None);
    let second = context(&test, principal(&[], None), None);

    let (one, two) = bounded(async {
        tokio::join!(
            run(&first, search_for("alpha", &["home", "remote"])),
            run(&second, search_for("alpha", &["home", "remote"]))
        )
    })
    .await;

    for result in [one, two] {
        let result = result.expect("a stalled load is named, not a failure");
        assert_eq!(result.unavailable, ["remote"]);
        assert_eq!(result.hits.len(), 1);
    }
    lock.rollback().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_catalog_listing_route_spends_one_load_bound_on_a_store_that_stalls() {
    use crate::http::api::testing::{every_catalog_admin, send};
    let connect = Duration::from_millis(300);
    let test = app_with(
        &[("main", &[("note.md", HOME_NOTE)])],
        Config {
            store_connect_timeout: connect,
            ..Config::default()
        },
    )
    .await;
    let store = test
        .catalog_on_new_store("store-b", "remote-1", &[("note.md", REMOTE_NOTE)])
        .await;
    test.catalog_on_store(&store, "remote-2", &[("note.md", REMOTE_NOTE)])
        .await;
    let key = every_catalog_admin(&test).await;
    let lock = lock_documents(&test, &store).await;
    let load_bound = test.app.stores().state_load_timeout();
    let started = std::time::Instant::now();

    let (status, body) = bounded(send(&test, "GET", "/api/v1/catalogs", Some(&key), None)).await;

    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    assert_eq!(
        body["unavailable"],
        serde_json::json!(["remote-1", "remote-2"]),
        "{body}"
    );
    assert!(
        started.elapsed() < load_bound * 3 / 2,
        "took {:?} for a load bound of {load_bound:?}",
        started.elapsed()
    );
    lock.rollback().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_readable_listing_over_several_scan_pages_spends_one_load_bound_on_a_stalled_store() {
    let connect = Duration::from_millis(300);
    let test = app_with(
        &[("main", &[("note.md", HOME_NOTE)])],
        Config {
            store_connect_timeout: connect,
            ..Config::default()
        },
    )
    .await;
    let store = test
        .catalog_on_new_store("store-b", "remote-1", &[("note.md", REMOTE_NOTE)])
        .await;
    for name in ["remote-2", "remote-3"] {
        test.catalog_on_store(&store, name, &[("note.md", REMOTE_NOTE)])
            .await;
    }
    // The generations query is the probe a listing makes: lock its table.
    let db = test.app.stores().get(&store).await.unwrap();
    let mut lock = db.pool().begin().await.unwrap();
    sqlx::query("LOCK TABLE catalog_state IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .unwrap();
    let started = std::time::Instant::now();

    // One catalog per scan page: four scan pages, three of them on the store
    // that stalls, and every catalog readable so the scan reaches the end.
    let page = bounded(test.app.directory().readable_page_by(
        &principal(&[], None),
        None,
        10,
        chrono::Utc::now(),
        1,
    ))
    .await
    .expect("a stalled store is named, not a failure");

    assert_eq!(page.items.len(), 4);
    assert_eq!(page.unavailable, ["remote-1", "remote-2", "remote-3"]);
    assert!(
        started.elapsed() < connect * 2,
        "took {:?} for a store-load bound of {connect:?}",
        started.elapsed()
    );
    lock.rollback().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn listed_states_answers_in_page_order_across_stores() {
    // Arrange: rows on two stores, interleaved, so the first row of each store
    // loads in a different wave from the rest.
    let test = app_with(&[("home", &[("note.md", HOME_NOTE)])], Config::default()).await;
    let store = test
        .catalog_on_new_store("store-b", "remote-1", &[("note.md", REMOTE_NOTE)])
        .await;
    test.catalog_on_store(&store, "remote-2", &[("note.md", REMOTE_NOTE)])
        .await;
    let ctx = context(&test, principal(&[], None), None);
    let mut rows = Vec::new();
    for name in ["remote-1", "home", "remote-2"] {
        rows.push(test.app.catalog_row(name).await.unwrap().unwrap());
    }
    let page: Vec<_> = rows.iter().collect();

    // Act
    let states = bounded(ctx.listed_states(&page)).await;

    // Assert
    let names: Vec<_> = states
        .into_iter()
        .map(|state| state.unwrap().unwrap().name().to_string())
        .collect();
    assert_eq!(names, ["remote-1", "home", "remote-2"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cached_first_row_does_not_let_the_cold_rows_stall_together() {
    // Arrange: three catalogs on one store, only the first cached; the store
    // answers the directory's probe but its documents are locked, so each
    // cold load waits on the lock.
    let test = app_with(
        &[("home", &[("note.md", HOME_NOTE)])],
        Config {
            store_connect_timeout: Duration::from_secs(2),
            ..Config::default()
        },
    )
    .await;
    let store = test
        .catalog_on_new_store("store-b", "remote-1", &[("note.md", REMOTE_NOTE)])
        .await;
    for name in ["remote-2", "remote-3"] {
        test.catalog_on_store(&store, name, &[("note.md", REMOTE_NOTE)])
            .await;
    }
    let mut rows = Vec::new();
    for name in ["remote-1", "remote-2", "remote-3"] {
        rows.push(test.app.catalog_row(name).await.unwrap().unwrap());
    }
    test.app.catalog_state(&rows[0]).await.unwrap();
    let db = test.app.stores().get(&store).await.unwrap();
    let mut lock = db.pool().begin().await.unwrap();
    sqlx::query("LOCK TABLE documents IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .unwrap();
    let ctx = std::sync::Arc::new(context(&test, principal(&[], None), None));
    let loading = {
        let ctx = std::sync::Arc::clone(&ctx);
        let rows = rows.clone();
        tokio::spawn(async move {
            let page: Vec<_> = rows.iter().collect();
            ctx.listed_states(&page).await
        })
    };

    // Act: count the queries waiting on the lock once loading has begun.
    let waiting = || async {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE wait_event_type = 'Lock' AND datname = current_database() \
             AND pid <> pg_backend_pid()",
        )
        .fetch_one(db.executor())
        .await
        .unwrap()
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while waiting().await == 0 {
        assert!(std::time::Instant::now() < deadline, "no load ever waited");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Long enough for a second, overlapping load to have started.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let attempts = waiting().await;
    let results = bounded(loading).await.unwrap();
    lock.rollback().await.unwrap();

    // Assert: one cold load attempted the store; the other was refused from
    // the down set.
    assert_eq!(attempts, 1, "cold rows on a stalled store overlapped");
    assert!(matches!(results[0], Ok(Some(_))), "the cached row answers");
    assert!(
        results[1..]
            .iter()
            .all(|result| matches!(result, Err(OkfError::StoreUnavailable { .. }))),
        "{results:?}"
    );
}
