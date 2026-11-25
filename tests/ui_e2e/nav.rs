//! The sidebar (issue #989): search first, then Catalogs (with the catalog
//! selector), Records, Administer and Your account, in that order, each group
//! showing what the identity may use; the search box sends nothing when blank
//! and shows results in the main panel otherwise.

use okf_mcp::config::Config;
use okf_mcp::store::keys::Access;
use serde_json::json;

use crate::harness::{drive, Ui};
use crate::support::server::{files, TestServer};

/// The sidebar's visible parts in document order: the search box, each group
/// label and each link (a link's own text only, without its badge; hidden entries left out).
const SIDEBAR: &str = "() => [...document.querySelectorAll(\
    '.sidebar #sidebar-search-input, .sidebar .nav-group-label, .sidebar .nav-link')] \
    .filter(el => el.offsetParent !== null) \
    .map(el => el.id === 'sidebar-search-input' ? '[search]' \
        : [...el.childNodes].filter(node => node.nodeType === 3) \
            .map(node => node.textContent).join('').trim())";

/// The number of `/api/v1/search` requests the page has made.
const SEARCH_REQUESTS: &str = "() => performance.getEntriesByType('resource') \
    .filter(entry => entry.name.includes('/api/v1/search')).length";

async fn server() -> TestServer {
    TestServer::start(
        vec![
            (
                "main",
                files(&[
                    ("index.md", "# Main\n\n- [Alpha](alpha.md)\n"),
                    (
                        "alpha.md",
                        "---\ntype: system\ntitle: Alpha\n---\n# Alpha\n\nAlpha runs.\n",
                    ),
                ]),
            ),
            ("other", files(&[("index.md", "# Other\n")])),
        ],
        Config::default(),
    )
    .await
}

fn press_enter_in_search(ui: &Ui) {
    ui.page
        .press_key(Some("#sidebar-search-input"), "Enter")
        .expect("press Enter");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_admin_key_sees_search_then_catalogs_records_administer_and_account() {
    let server = server().await;
    let key = server
        .key("nav-admin", Access::Admin, &["main", "other"], Some("main"))
        .await;
    drive(server.base.clone(), move |ui| {
        ui.login(&key);
        ui.wait_visible("[data-need=account]");
        assert_eq!(
            ui.eval(SIDEBAR, json!(null)),
            json!([
                "[search]",
                "Catalogs",
                "Browse",
                "Recent changes",
                "Import & export",
                "Records",
                "Findings",
                "Runs",
                "Changesets",
                "Unlocks",
                "Administer",
                "API keys",
                "Users",
                "Teams",
                "Settings",
                "Your account",
                "Catalogs",
            ])
        );
        // No standalone Search, Health, Maintenance or Your teams entries.
        assert_eq!(
            ui.eval(
                "() => ['search', 'health'].filter(nav => document.querySelector(`a[data-nav=${nav}]`))",
                json!(null)
            ),
            json!([])
        );
        let text = ui.text(".sidebar");
        assert!(!text.contains("Maintenance") && !text.contains("Your teams"), "{text}");
        // The selector sits inside the Catalogs group, after its links.
        assert_eq!(
            ui.eval(
                "() => { const group = document.querySelector('.nav-group'); \
                 const select = group.querySelector('#catalog-select'); \
                 const last = group.querySelector('a[data-nav=transfer]'); \
                 return !!select && !!(last.compareDocumentPosition(select) \
                     & Node.DOCUMENT_POSITION_FOLLOWING); }",
                json!(null)
            ),
            json!(true)
        );
        // The default route is Browse.
        ui.open("");
        ui.wait_text("#concept-title", "/index.md");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_read_key_sees_no_administer_or_account_group_and_keeps_the_maintenance_links() {
    let server = server().await;
    let key = server
        .key("nav-reader", Access::Read, &["main"], Some("main"))
        .await;
    drive(server.base.clone(), move |ui| {
        ui.login(&key);
        ui.wait_visible("a[data-nav=findings]");
        assert_eq!(
            ui.eval(SIDEBAR, json!(null)),
            json!([
                "[search]",
                "Catalogs",
                "Browse",
                "Recent changes",
                "Import & export",
                "Records",
                "Findings",
                "Runs",
                "Changesets",
            ])
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_sidebar_search_shows_results_in_the_panel_and_sends_nothing_when_blank() {
    let server = server().await;
    let key = server
        .key("nav-search", Access::Read, &["main", "other"], Some("main"))
        .await;
    drive(server.base.clone(), move |ui| {
        ui.login(&key);
        ui.wait_visible("#sidebar-search-input");
        assert_eq!(
            ui.eval(
                "() => document.querySelector('#sidebar-search-input').value",
                json!(null)
            ),
            json!("")
        );
        // Blank (and spaces only): no navigation, no request.
        press_enter_in_search(ui);
        ui.fill("#sidebar-search-input", "   ");
        press_enter_in_search(ui);
        assert_eq!(ui.eval(SEARCH_REQUESTS, json!(null)), json!(0));
        assert!(!ui.text("#main").contains("hit"));

        ui.fill("#sidebar-search-input", "alpha");
        press_enter_in_search(ui);
        ui.wait_text("#search-count", "1 hit");
        assert!(ui.text("#search-results").contains("main /alpha.md"));
        assert_eq!(
            ui.eval("() => location.hash", json!(null)),
            json!("#/search/alpha")
        );
        // A repeat of the same query reads again.
        let before = ui.eval(SEARCH_REQUESTS, json!(null)).as_u64().unwrap_or(0);
        press_enter_in_search(ui);
        ui.poll_eval(&format!(
            "() => performance.getEntriesByType('resource') \
             .filter(entry => entry.name.includes('/api/v1/search')).length > {before}"
        ));
        // A result opens its concept in Browse.
        ui.click("#search-results a.hit-title");
        ui.wait_text("#concept-title", "Alpha");
        // A linked search shows its query in the box.
        ui.open("/search/alpha");
        ui.wait_text("#search-count", "hit");
        assert_eq!(
            ui.eval(
                "() => document.querySelector('#sidebar-search-input').value",
                json!(null)
            ),
            json!("alpha")
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_catalog_selector_in_the_sidebar_still_switches_catalogs() {
    let server = server().await;
    let key = server
        .key("nav-switch", Access::Read, &["main", "other"], Some("main"))
        .await;
    drive(server.base.clone(), move |ui| {
        ui.login(&key);
        ui.click("a[data-nav=recent]");
        ui.wait_text("#recent-scope", "main");
        ui.select(".nav-group #catalog-select", "other");
        ui.wait_text("#recent-scope", "other");
    })
    .await;
}
