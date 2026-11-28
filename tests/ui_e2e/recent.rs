//! Recent changes (issue #261): the selected catalog's published revisions,
//! newest first, each opening its diff in the drawer (a deletion's too);
//! Load older and Refresh; empty and failed reads; and a catalog switch
//! that no late answer can undo.

use chrono::Utc;
use okf_mcp::archive::Files;
use okf_mcp::config::Config;
use okf_mcp::store::import::{import, ImportOptions, ImportScope};
use okf_mcp::store::keys::Access;
use serde_json::json;

use crate::support::server::{files, TestServer};
use crate::ui_e2e::harness::{drive, Ui};

/// `systems/alpha.md` as first imported.
const ALPHA: &str = "---\ntype: system\ntitle: Alpha\n---\n# Alpha\n\nNothing yet.\n";

/// `main` with `alpha` as given, and beta unless `beta` is false.
fn main_catalog(alpha: &str, beta: bool) -> Files {
    let mut entries = vec![
        ("index.md", "# Main\n\n- [Alpha](systems/alpha.md)\n"),
        ("log.md", "# Log\n"),
        ("systems/alpha.md", alpha),
    ];
    if beta {
        entries.push((
            "systems/beta.md",
            "---\ntype: system\ntitle: Beta\n---\n# Beta\n\nBeta.\n",
        ));
    }
    files(&entries)
}

/// Imports `catalog`'s `entries` over what it holds, as `actor`.
async fn publish(server: &TestServer, catalog: &str, entries: Files, actor: &str) {
    let options = ImportOptions {
        replace: true,
        scope: ImportScope::Everything,
        expect_generation: None,
        expect_catalog_id: None,
        actor,
        audit: &okf_mcp::store::audit::AuditActor::cli(),
        now: Utc::now(),
    };
    crate::support::server::applied(&server.app, async {
        import(
            &crate::support::stores::main_store(&server.db.pool),
            catalog,
            entries,
            &options,
        )
        .await
        .expect("import");
    })
    .await;
}

/// A server with `main` (created, imported by `tester`, then by `editor`,
/// who changed alpha and dropped beta), `other` (created and imported by
/// `tester`) and `empty` (with no history at all, as a catalog from before
/// revisions were kept), and a read key on all three, with `main` selected.
async fn server() -> (TestServer, String) {
    let server = TestServer::start(
        vec![
            ("main", main_catalog(ALPHA, true)),
            ("other", files(&[("notes.md", "# Notes\n")])),
            ("empty", files(&[])),
        ],
        Config::default(),
    )
    .await;
    let edited = ALPHA.replace("Nothing yet.", "Two settings.");
    publish(&server, "main", main_catalog(&edited, false), "editor").await;
    // Two statements: the registry and the store are databases of their own
    // in `OKF_TEST_STORE=separate` mode.
    let empty: i64 = sqlx::query_scalar("SELECT id FROM catalogs WHERE name = 'empty'")
        .fetch_one(&server.db.pool)
        .await
        .expect("find `empty`");
    sqlx::query("DELETE FROM catalog.revisions WHERE catalog_id = $1")
        .bind(empty)
        .execute(&server.db.store())
        .await
        .expect("empty the history of `empty`");
    let key = server
        .key(
            "recent-reader",
            Access::Read,
            &["main", "other", "empty"],
            Some("main"),
        )
        .await;
    (server, key)
}

/// Publishes `late.md` into `main` as `latecomer` (an import over what
/// `main` holds after [`server`]).
async fn add_late(pool: &sqlx::PgPool, app: &std::sync::Arc<okf_mcp::app::App>) {
    let edited = ALPHA.replace("Nothing yet.", "Two settings.");
    let mut entries = main_catalog(&edited, false);
    entries.extend(files(&[("late.md", "# Late\n")]));
    let options = ImportOptions {
        replace: true,
        scope: ImportScope::Everything,
        expect_generation: None,
        expect_catalog_id: None,
        actor: "latecomer",
        audit: &okf_mcp::store::audit::AuditActor::cli(),
        now: Utc::now(),
    };
    crate::support::server::applied(app, async {
        import(
            &crate::support::stores::main_store(pool),
            "main",
            entries,
            &options,
        )
        .await
        .expect("import late.md");
    })
    .await;
}

/// Each row of the feed as `path@revision op actor`, in the order shown;
/// `actor` is the actor's id (shown beneath its label when it has one).
fn feed(ui: &Ui) -> Vec<String> {
    ui.eval(
        "() => [...document.querySelectorAll('#recent-table tbody tr.clickable')] \
           .map(tr => [...tr.children].map(td => \
             (td.querySelector('.id-line') ?? td).textContent.trim())) \
           .map(([, path, rev, op, actor]) => `${path}@${rev} ${op} ${actor}`)",
        json!(null),
    )
    .as_array()
    .expect("a list")
    .iter()
    .map(|row| row.as_str().expect("a row").to_string())
    .collect()
}

/// Clicks the feed row for `path` at `revision`.
fn open_row(ui: &Ui, path: &str, revision: u32) {
    ui.eval(
        "([path, rev]) => [...document.querySelectorAll('#recent-table tbody tr.clickable')] \
           .find(tr => tr.children[1].textContent.trim() === path \
                    && tr.children[2].textContent.trim() === String(rev)).click()",
        json!([path, revision]),
    );
}

/// Clicks Load older until it is gone, each time waiting for the rows to
/// grow: a cursor is only given while another row follows it, so every
/// continuation adds one at least, and the button's state is set in the
/// same turn as the rows are drawn.
fn load_all(ui: &Ui) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while ui.page.is_visible("#recent-older").expect("visible") {
        let before = feed(ui).len();
        ui.click("#recent-older");
        while feed(ui).len() == before {
            assert!(
                std::time::Instant::now() < deadline,
                "Load older never settled"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

/// Waits until the feed has at least `count` rows.
fn wait_rows(ui: &Ui, count: usize) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while feed(ui).len() < count {
        assert!(
            std::time::Instant::now() < deadline,
            "{count} rows never came"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Selects `catalog` in the switcher, as a user would.
fn switch_to(ui: &Ui, catalog: &str) {
    ui.eval(
        "(catalog) => { const select = document.getElementById('catalog-select'); \
           select.value = catalog; select.dispatchEvent(new Event('change')); }",
        json!(catalog),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_feed_lists_every_revision_newest_first_and_opens_each_diff() {
    let (server, key) = server().await;
    drive(server.base.clone(), move |ui| {
        ui.login(&key);
        ui.click("a[data-nav=recent]");
        ui.wait_text("#recent-table tbody", "editor");
        ui.wait_text("#recent-scope", "main");
        // The editor's import wrote both of its rows at one time: by path,
        // descending. The older revisions follow, repeats of a path too.
        let rows = feed(ui);
        assert_eq!(
            rows[..2],
            [
                "/systems/beta.md@2 delete editor",
                "/systems/alpha.md@2 import editor",
            ]
        );
        for older in [
            "/systems/beta.md@1 import tester",
            "/systems/alpha.md@1 import tester",
        ] {
            assert!(
                rows[2..].contains(&older.to_string()),
                "{older} in {rows:?}"
            );
        }
        // An update's diff and text.
        open_row(ui, "/systems/alpha.md", 2);
        ui.wait_visible("#drawer");
        ui.wait_text("#drawer-title", "/systems/alpha.md @ 2");
        let diff = ui.wait_text("#revision-diff", "+Two settings.");
        assert!(diff.contains("-Nothing yet."), "{diff}");
        ui.click("#drawer-close");
        ui.wait_hidden("#drawer");
        // A deleted document opens too, with no text at its deletion.
        open_row(ui, "/systems/beta.md", 2);
        ui.wait_visible("#drawer");
        ui.wait_text("#drawer-title", "/systems/beta.md @ 2");
        let diff = ui.wait_text("#revision-diff", "-Beta.");
        assert!(diff.contains("+++ /dev/null"), "{diff}");
        ui.wait_visible("#revision-no-text");
        ui.click("#drawer-close");
        ui.wait_hidden("#drawer");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn load_older_pages_through_ties_and_refresh_shows_a_new_change() {
    let (server, key) = server().await;
    let pool = server.db.pool.clone();
    let app = server.app.clone();
    // The journey runs on a blocking thread, which may wait on the test's
    // runtime to publish a change mid-journey.
    let runtime = tokio::runtime::Handle::current();
    drive(server.base.clone(), move |ui| {
        ui.login(&key);
        ui.open("/recent");
        ui.wait_text("#recent-table tbody", "editor");
        let whole = feed(ui);
        assert!(whole.len() > 4, "three pages of two: {whole:?}");
        ui.eval(
            "async () => { (await import('/ui/js/pages/recent.js')).setRecentPageSize(2); }",
            json!(null),
        );
        ui.open("/browse");
        ui.open("/recent");
        ui.wait_visible("#recent-older");
        assert_eq!(feed(ui), whole[..2]);
        ui.click("#recent-older");
        ui.wait_text("#recent-table tbody", "/systems/alpha.md");
        ui.wait_visible("#recent-older");
        assert_eq!(feed(ui), whole[..4]);
        // A failed continuation keeps the rows and the cursor for a retry.
        ui.fail_next("GET", "", 503, "down for a moment");
        ui.click("#recent-older");
        ui.wait_visible("#recent-status");
        ui.wait_text("#recent-status", "may be out of date");
        ui.wait_visible("#recent-older");
        assert_eq!(feed(ui), whole[..4]);
        load_all(ui);
        assert_eq!(feed(ui), whole, "every row once, in order");
        // A change published meanwhile shows on Refresh, first.
        runtime.block_on(add_late(&pool, &app));
        ui.click("#recent-refresh");
        ui.wait_text("#recent-table tbody", "latecomer");
        assert_eq!(
            feed(ui),
            ["/late.md@1 import latecomer", whole[0].as_str()],
            "Refresh starts again from the newest page"
        );
        ui.wait_visible("#recent-older");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_empty_catalog_and_a_failed_read_say_so() {
    let (server, key) = server().await;
    drive(server.base.clone(), move |ui| {
        ui.login(&key);
        switch_to(ui, "empty");
        ui.open("/recent");
        ui.wait_text("#recent-scope", "empty");
        ui.wait_text("#recent-table", "No changes have been published");
        ui.wait_visible("#recent-table");
        // The first read of `main` fails: a message, and Refresh retries.
        ui.fail_next("GET", "recent-changes?limit=50", 503, "down for a moment");
        switch_to(ui, "main");
        ui.wait_visible("#recent-status");
        ui.wait_text("#recent-status", "could not be loaded");
        assert!(ui.text("#recent-status").contains("down for a moment"));
        ui.click("#recent-refresh");
        ui.wait_text("#recent-table tbody", "editor");
        ui.wait_hidden("#recent-status");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_late_answer_for_the_old_catalog_fills_neither_the_feed_nor_the_drawer() {
    let (server, key) = server().await;
    drive(server.base.clone(), move |ui| {
        ui.login(&key);
        ui.open("/recent");
        ui.wait_text("#recent-table tbody", "editor");
        // A Refresh of `main` answers only after the switch to `other`.
        ui.hold_answer("GET", "/catalogs/main/recent-changes?limit=50", || {
            ui.click("#recent-refresh")
        });
        switch_to(ui, "other");
        ui.wait_text("#recent-scope", "other");
        ui.wait_text("#recent-table tbody", "/notes.md");
        ui.release_held();
        let rows = feed(ui);
        assert!(
            rows.contains(&"/notes.md@1 import tester".to_string()),
            "{rows:?}"
        );
        assert!(
            !rows.iter().any(|row| row.contains("/systems/")),
            "{rows:?}"
        );
        // A revision of `main` asked for before the switch never opens.
        switch_to(ui, "main");
        ui.wait_text("#recent-table tbody", "editor");
        ui.hold_answer("GET", "/catalogs/main/revisions/2/systems/alpha.md", || {
            open_row(ui, "/systems/alpha.md", 2)
        });
        switch_to(ui, "other");
        ui.wait_text("#recent-table tbody", "/notes.md");
        ui.release_held();
        ui.wait_hidden("#drawer");
        let rows = feed(ui);
        assert!(
            !rows.iter().any(|row| row.contains("/systems/")),
            "{rows:?}"
        );
        // Of two revisions opened in turn, the later one's drawer stays,
        // whichever answer comes last.
        switch_to(ui, "main");
        ui.wait_text("#recent-table tbody", "editor");
        ui.hold_answer("GET", "/catalogs/main/revisions/2/systems/alpha.md", || {
            open_row(ui, "/systems/alpha.md", 2)
        });
        open_row(ui, "/systems/alpha.md", 1);
        ui.wait_text("#drawer-title", "/systems/alpha.md @ 1");
        ui.release_held();
        assert_eq!(ui.text("#drawer-title"), "/systems/alpha.md @ 1");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_overtaken_or_failed_refresh_keeps_the_newest_rows_and_their_cursor() {
    let (server, key) = server().await;
    let pool = server.db.pool.clone();
    let app = server.app.clone();
    let runtime = tokio::runtime::Handle::current();
    drive(server.base.clone(), move |ui| {
        ui.login(&key);
        ui.open("/recent");
        ui.wait_text("#recent-table tbody", "editor");
        let before = feed(ui);
        ui.eval(
            "async () => { (await import('/ui/js/pages/recent.js')).setRecentPageSize(2); }",
            json!(null),
        );
        ui.open("/browse");
        ui.open("/recent");
        ui.wait_visible("#recent-older");
        assert_eq!(feed(ui), before[..2]);
        // A Refresh answered before late.md is published arrives only
        // after a newer Refresh that shows it: the older answer is dropped.
        ui.hold_answer("GET", "recent-changes?limit=2", || {
            ui.click("#recent-refresh")
        });
        runtime.block_on(add_late(&pool, &app));
        ui.click("#recent-refresh");
        ui.wait_text("#recent-table tbody", "latecomer");
        ui.release_held();
        let newest = feed(ui);
        assert_eq!(newest, ["/late.md@1 import latecomer", before[0].as_str()]);
        ui.wait_visible("#recent-older");
        // A Refresh overtaken by a newer one that fails: the rows and the
        // cursor stay, marked as possibly out of date, and Load older
        // continues them once.
        ui.hold_answer("GET", "recent-changes?limit=2", || {
            ui.click("#recent-refresh")
        });
        ui.fail_next("GET", "recent-changes?limit=2", 503, "down for a moment");
        ui.click("#recent-refresh");
        ui.wait_visible("#recent-status");
        ui.wait_text("#recent-status", "may be out of date");
        ui.release_held();
        assert_eq!(feed(ui), newest);
        ui.wait_visible("#recent-older");
        ui.click("#recent-older");
        wait_rows(ui, 4);
        let continued = feed(ui);
        assert_eq!(continued.len(), 4, "{continued:?}");
        assert_eq!(continued[..2], newest);
        assert_eq!(
            continued[2..],
            before[1..3],
            "continued from the same cursor, once"
        );
    })
    .await;
}
