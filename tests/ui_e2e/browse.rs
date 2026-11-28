//! Browse and Search (M9 Task 4): the tree, a rendered concept and a link
//! in its body, raw HTML shown as text, the Links and History tabs with a
//! revision's diff, and search.

use chrono::Utc;
use okf_mcp::archive::Files;
use okf_mcp::config::Config;
use okf_mcp::store::import::{import, ImportOptions, ImportScope};
use okf_mcp::store::keys::Access;
use serde_json::json;

use crate::support::server::{files, TestServer};
use crate::ui_e2e::harness::drive;

/// `systems/alpha.md` as first imported.
const ALPHA: &str = "---\ntype: system\ntitle: Alpha\n---\n# Alpha\n\n\
                     Alpha runs [Beta](beta.md).\n\n## Config\n\nNothing yet.\n";

/// A concept that tries to run script three ways.
const HOSTILE: &str = "---\ntype: note\ntitle: Hostile\n---\n# Hostile\n\n\
                       <script>window.owned = true</script>\n\n\
                       Inline <img src=x onerror=\"window.owned = true\"> and \
                       [a link](javascript:window.owned=true).\n";

/// The synthetic `main` catalog, with `alpha` as given.
fn main_catalog(alpha: &str) -> Files {
    files(&[
        (
            "index.md",
            "# Main\n\n- [Alpha](systems/alpha.md): the first system\n",
        ),
        ("log.md", "# Log\n"),
        ("AGENTS.md", "# Agents\n"),
        (
            "systems/index.md",
            "# Systems\n\n- [Alpha](alpha.md)\n- [Beta](beta.md)\n",
        ),
        ("systems/alpha.md", alpha),
        (
            "systems/beta.md",
            "---\ntype: system\ntitle: Beta\n---\n# Beta\n\nBeta.\n",
        ),
        ("notes/hostile.md", HOSTILE),
    ])
}

/// A server holding `main` and a read key on it.
async fn server() -> (TestServer, String) {
    let server = TestServer::start(vec![("main", main_catalog(ALPHA))], Config::default()).await;
    let key = server
        .key("browse-reader", Access::Read, &["main"], Some("main"))
        .await;
    (server, key)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_tree_opens_a_concept_and_a_link_in_its_body_opens_the_next() {
    let (server, key) = server().await;
    drive(server.base.clone(), move |ui| {
        ui.login(&key);
        ui.open("/browse/main");
        ui.wait_text("#concept-title", "/index.md");
        ui.wait_visible("#markdown-body");
        let folder_open = |ui: &crate::ui_e2e::harness::Ui| {
            ui.eval(
                "() => document.querySelector(\"[data-folder='systems']\").open",
                json!(null),
            )
        };
        assert_eq!(folder_open(ui), json!(false), "folders start closed");
        ui.click("[data-folder='systems'] > summary");
        assert_eq!(folder_open(ui), json!(true));
        ui.click("[data-path='/systems/alpha.md']");
        ui.wait_text("#concept-title", "Alpha");
        ui.wait_visible("#markdown-body h2[data-slug='config']");
        assert_eq!(ui.text("#markdown-body h1"), "Alpha");
        assert!(ui.text("#provenance").contains("0 sources"));
        assert!(
            !ui.text("#markdown-body").contains("type: system"),
            "no frontmatter"
        );
        ui.click("#markdown-body a[href='#/browse/main/systems/beta.md']");
        ui.wait_text("#concept-title", "Beta");
        assert_eq!(
            ui.eval("() => location.hash", json!(null)),
            json!("#/browse/main/systems/beta.md")
        );
        // A section route opens the concept at that heading.
        ui.open("/browse/main/systems/alpha.md?s=config");
        ui.wait_text("#concept-title", "Alpha");
        ui.wait_visible("#outline a[href='#/browse/main/systems/alpha.md?s=config']");
        // A link written before the parameter ('<path>.md/<slug>') still opens it.
        ui.open("/browse/main/systems/alpha.md/config");
        ui.wait_text("#concept-title", "Alpha");
        ui.wait_visible("#outline a[href='#/browse/main/systems/alpha.md?s=config']");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn raw_html_and_script_links_in_a_concept_render_as_text() {
    let (server, key) = server().await;
    drive(server.base.clone(), move |ui| {
        ui.login(&key);
        ui.open("/browse/main/notes/hostile.md");
        ui.wait_text("#markdown-body", "<script>window.owned = true</script>");
        assert!(ui.text("#markdown-body").contains("<img src=x"));
        let injected = ui.eval(
            "() => document.querySelectorAll('#markdown-body script, #markdown-body img, \
             #markdown-body [onerror], #markdown-body a[href^=\"javascript\"]').length",
            json!(null),
        );
        assert_eq!(injected, json!(0));
        assert_eq!(
            ui.eval("() => window.owned === undefined", json!(null)),
            json!(true)
        );
        // drive() then fails the journey on any console error, CSP
        // violations included.
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_links_tab_shows_backlinks_and_a_revision_opens_its_diff() {
    let (server, key) = server().await;
    // A second revision of alpha, as a replacing import writes it.
    let options = ImportOptions {
        replace: true,
        scope: ImportScope::Everything,
        expect_generation: None,
        expect_catalog_id: None,
        actor: "editor",
        audit: &okf_mcp::store::audit::AuditActor::cli(),
        now: Utc::now(),
    };
    let edited = ALPHA.replace("Nothing yet.", "Two settings.");
    crate::support::server::applied(&server.app, async {
        import(
            &crate::support::stores::main_store(&server.db.pool),
            "main",
            main_catalog(&edited),
            &options,
        )
        .await
        .expect("import the edit");
    })
    .await;
    drive(server.base.clone(), move |ui| {
        ui.login(&key);
        ui.open("/browse/main/systems/beta.md");
        ui.wait_text("#concept-title", "Beta");
        ui.click("#concept-tabs [data-value='links']");
        ui.wait_text("#backlinks", "/systems/alpha.md");
        // The row from alpha (CSS cannot select by text, so a script clicks it).
        ui.eval(
            "() => [...document.querySelectorAll('#backlinks tbody tr')]\
             .find(row => row.textContent.includes('/systems/alpha.md')).click()",
            json!(null),
        );
        ui.wait_text("#concept-title", "Alpha");
        ui.wait_text("#markdown-body", "Two settings.");
        ui.click("#concept-tabs [data-value='history']");
        ui.wait_text("#history", "editor");
        ui.click("#history tbody tr:first-child");
        ui.wait_visible("#drawer");
        let diff = ui.wait_text("#revision-diff", "+Two settings.");
        assert!(diff.contains("-Nothing yet."), "{diff}");
        ui.click("#drawer-close");
        ui.wait_hidden("#drawer");
    })
    .await;
}

/// The revision numbers in the History tab's rows, in the order shown.
fn history_rows(ui: &crate::ui_e2e::harness::Ui) -> Vec<String> {
    serde_json::from_value(ui.eval(
        "() => [...document.querySelectorAll('#history tbody tr td:first-child')]\
         .map(cell => cell.textContent)",
        json!(null),
    ))
    .expect("row numbers")
}

/// The `server()` with revisions 2..=12 of alpha added, so its history has 12.
async fn server_with_long_history() -> (TestServer, String) {
    let (server, key) = server().await;
    // Revisions 2..=12 of alpha, written straight into the store: publishing
    // them one by one would be slow, and the page is shrunk to 5 below.
    sqlx::query(
        "INSERT INTO catalog.revisions \
         (catalog_id, path, revision, op, executable, actor, message, created_at) \
         SELECT catalog_id, path, n, 'update', false, 'seed', 'edit ' || n, now() \
         FROM catalog.revisions, generate_series(2, 12) AS n \
         WHERE path = 'systems/alpha.md' AND revision = 1",
    )
    .execute(&server.db.store())
    .await
    .expect("seed revisions");
    (server, key)
}

/// The revision numbers `from` down to `to`, as the History tab shows them.
fn numbers(from: i32, to: i32) -> Vec<String> {
    (to..=from).rev().map(|number| number.to_string()).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_load_older_keeps_the_rows_and_a_retry_asks_for_the_same_page() {
    let (server, key) = server_with_long_history().await;
    drive(server.base.clone(), move |ui| {
        ui.login(&key);
        ui.eval(
            "async () => { (await import('/ui/js/pages/browse.js')).setHistoryPageSize(5); }",
            json!(null),
        );
        ui.open("/browse/main/systems/alpha.md");
        ui.wait_text("#concept-title", "Alpha");
        ui.click("#concept-tabs [data-value='history']");
        ui.wait_text("#history", "edit 12");
        ui.wait_visible("#history-older");
        // The page below 8 fails: the rows stay, the message shows, and
        // Load older comes back.
        ui.fail_next("GET", "limit=5&before=8", 503, "down for a moment");
        ui.click("#history-older");
        ui.wait_visible("#history-status");
        assert!(ui.text("#history-status").contains("down for a moment"));
        ui.wait_visible("#history-older");
        assert_eq!(history_rows(ui), numbers(12, 8));
        // The retry asks for the same cursor once, appends the page once and
        // clears the message.
        ui.hold_answer("GET", "limit=5&before=8", || ui.click("#history-older"));
        assert_eq!(ui.held_matches(), 1, "the same cursor, asked once");
        ui.release_held();
        ui.wait_text("#history", "edit 3");
        ui.wait_hidden("#history-status");
        assert_eq!(history_rows(ui), numbers(12, 3));
        ui.wait_visible("#history-older");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn history_loads_older_revisions_once_and_a_late_page_draws_nowhere() {
    let (server, key) = server_with_long_history().await;
    drive(server.base.clone(), move |ui| {
        ui.login(&key);
        ui.eval(
            "async () => { (await import('/ui/js/pages/browse.js')).setHistoryPageSize(5); }",
            json!(null),
        );
        ui.open("/browse/main/systems/alpha.md");
        ui.wait_text("#concept-title", "Alpha");
        ui.click("#concept-tabs [data-value='history']");
        ui.wait_text("#history", "edit 12");
        assert_eq!(history_rows(ui), numbers(12, 8));
        ui.wait_visible("#history-older");
        // Two quick clicks ask for the page below 8 once and append it once.
        ui.hold_answer("GET", "limit=5&before=8", || {
            ui.click_twice("#history-older");
        });
        assert_eq!(ui.held_matches(), 1, "one request for one page");
        ui.release_held();
        ui.wait_text("#history", "edit 3");
        assert_eq!(history_rows(ui), numbers(12, 3));
        // A page that answers after the tab was left draws nothing, and the
        // tab opened again starts from the newest page.
        ui.hold_answer("GET", "limit=5&before=3", || ui.click("#history-older"));
        ui.click("#concept-tabs [data-value='content']");
        ui.wait_visible("#markdown-body");
        ui.release_held();
        ui.click("#concept-tabs [data-value='history']");
        ui.wait_text("#history", "edit 12");
        assert_eq!(history_rows(ui), numbers(12, 8));
        // The last page ends the list and takes the button away.
        ui.click("#history-older");
        ui.wait_text("#history", "edit 3");
        ui.click("#history-older");
        ui.poll_eval("() => document.querySelectorAll('#history tbody tr').length === 12");
        ui.wait_hidden("#history-older");
        assert_eq!(history_rows(ui), numbers(12, 1));
        ui.assert_no_console_errors();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn search_finds_alpha_and_opens_it() {
    let (server, key) = server().await;
    drive(server.base.clone(), move |ui| {
        ui.login(&key);
        ui.open("/search");
        ui.fill("#search-query", "alpha");
        ui.click("#search-form button[type=submit]");
        ui.wait_text("#search-count", "hit");
        assert!(ui
            .text("#search-results")
            .contains("main /systems/alpha.md"));
        ui.click("#search-results a[data-path='/systems/alpha.md']");
        ui.wait_text("#concept-title", "Alpha");
    })
    .await;
}

/// Three catalogs, each holding one concept that says "needle", and a key
/// on all three, with the search cap at `cap`.
async fn needle_server(cap: u32) -> (TestServer, String) {
    let note = |name: &str| files(&[("index.md", "# Index\n"), (name, "# Needle\n\nneedle\n")]);
    let server = TestServer::start(
        vec![
            ("one", note("one.md")),
            ("two", note("two.md")),
            ("three", note("three.md")),
        ],
        Config {
            search_fanout_max: cap,
            ..Config::default()
        },
    )
    .await;
    let key = server
        .key(
            "browse-needle",
            Access::Read,
            &["one", "two", "three"],
            Some("one"),
        )
        .await;
    (server, key)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn search_within_the_cap_names_no_catalogs_and_shows_no_chooser() {
    let (server, key) = needle_server(3).await;
    drive(server.base.clone(), move |ui| {
        ui.login(&key);
        ui.open("/search/needle");
        ui.wait_text("#search-count", "3 hits");
        assert!(!ui.present("#search-chooser"));
        // The request names no catalogs: the server's own every-reachable path applies.
        let urls = ui.eval(
            "() => performance.getEntriesByType('resource').map(entry => entry.name).filter(name => name.includes('/api/v1/search'))",
            json!(null),
        );
        let urls = urls.as_array().expect("urls");
        assert_eq!(urls.len(), 1, "{urls:?}");
        assert!(!urls[0].as_str().unwrap_or_default().contains("catalogs="), "{urls:?}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn search_past_the_cap_asks_for_a_choice_and_limits_it() {
    let (server, key) = needle_server(2).await;
    drive(server.base.clone(), move |ui| {
        ui.login(&key);
        ui.open("/search/needle");
        ui.wait_visible("#search-chooser");
        // The current catalog and the first other one are chosen to start.
        ui.wait_text("#search-cap", "2 of at most 2");
        ui.wait_text("#search-count", "2 hits");
        let enabled = ui.eval(
            "() => [...document.querySelectorAll('#search-chooser input:not(:disabled)')].map(box => box.value).sort()",
            json!(null),
        );
        assert_eq!(enabled, json!(["one", "three"]), "the unchosen box is disabled at the cap");
        // Unchecking one frees the third box; search only the one left.
        ui.click("#search-chooser input[value=one]");
        ui.wait_text("#search-cap", "1 of at most 2");
        ui.click("#search-form button[type=submit]");
        ui.wait_text("#search-count", "1 hit");
        let shown = ui.text("#search-results");
        assert!(shown.contains("three /three.md"), "{shown}");
    })
    .await;
}

/// The number of `/api/v1/search` requests the page has made.
const SEARCH_REQUESTS: &str = "() => performance.getEntriesByType('resource') \
    .filter(entry => entry.name.includes('/api/v1/search')).length";

/// The values of the checked boxes in the chooser, sorted.
const CHECKED: &str = "() => [...document.querySelectorAll('#search-chooser input:checked')] \
    .map(box => box.value).sort()";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_empty_choice_is_kept_and_searches_nothing() {
    let (server, key) = needle_server(2).await;
    drive(server.base.clone(), move |ui| {
        ui.login(&key);
        ui.open("/search/needle");
        ui.wait_text("#search-count", "2 hits");
        assert_eq!(ui.eval(SEARCH_REQUESTS, json!(null)), json!(1));
        ui.click("#search-chooser input[value=one]");
        ui.click("#search-chooser input[value=three]");
        ui.wait_text("#search-cap", "Choose at least one catalog");
        assert_eq!(ui.eval(CHECKED, json!(null)), json!([]));
        // Search is off, and a submit anyway (Enter in the box) does nothing.
        assert_eq!(
            ui.eval(
                "() => document.querySelector('#search-form button').disabled",
                json!(null)
            ),
            json!(true)
        );
        ui.eval(
            "() => document.querySelector('#search-form').requestSubmit()",
            json!(null),
        );
        // Opening the search again (a reload of the link) keeps the empty choice
        // instead of restoring the defaults, and sends no request.
        ui.eval("() => { location.hash = '#/search/Needle'; }", json!(null));
        ui.wait_text("#search-none", "Choose at least one catalog");
        assert_eq!(
            ui.eval(CHECKED, json!(null)),
            json!([]),
            "no default is restored"
        );
        assert_eq!(
            ui.eval(SEARCH_REQUESTS, json!(null)),
            json!(1),
            "no request was sent"
        );
        // Choosing one again enables Search and searches only that one.
        ui.click("#search-chooser input[value=two]");
        ui.click("#search-form button[type=submit]");
        ui.wait_text("#search-count", "1 hit");
        assert!(ui.text("#search-results").contains("two /two.md"));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chosen_catalog_that_is_no_longer_reachable_drops_out_of_the_choice() {
    let (server, key) = needle_server(1).await;
    drive(server.base.clone(), move |ui| {
        ui.login(&key);
        ui.open("/search");
        ui.wait_visible("#search-chooser");
        assert_eq!(ui.eval(CHECKED, json!(null)), json!(["one"]));
        // The identity is read again and `one` is gone from it; two remain,
        // still more than the cap of 1.
        ui.eval(
            "async () => { const { store } = await import('/ui/js/core/store.js'); \
               store.whoami.catalogs = store.whoami.catalogs.filter(name => name !== 'one'); \
               location.hash = '#/search/needle'; }",
            json!(null),
        );
        ui.wait_text("#search-none", "Choose at least one catalog");
        assert_eq!(ui.eval(CHECKED, json!(null)), json!([]));
        assert_eq!(ui.eval(SEARCH_REQUESTS, json!(null)), json!(0));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn switching_catalogs_while_browsing_opens_the_new_root_once() {
    let server = TestServer::start(
        vec![
            ("main", main_catalog(ALPHA)),
            ("other", files(&[("index.md", "# Other\n")])),
        ],
        Config::default(),
    )
    .await;
    let key = server
        .key(
            "browse-both",
            Access::Read,
            &["main", "other"],
            Some("main"),
        )
        .await;
    drive(server.base.clone(), move |ui| {
        ui.login(&key);
        // Several Browse renders: the page must not add a switcher listener
        // on each, or one switch would render the new catalog several times.
        for route in [
            "/browse/main",
            "/browse/main/systems/alpha.md",
            "/browse/main/systems/beta.md",
        ] {
            ui.open(route);
            ui.wait_visible("#markdown-body");
        }
        ui.wait_text("#concept-title", "Beta");
        // Count the tree requests for `other`, then switch to it.
        ui.eval(
            "() => { window.otherTrees = 0; const real = window.fetch; \
             window.fetch = (url, opts) => { \
             if (String(url).endsWith('/catalogs/other/tree')) window.otherTrees += 1; \
             return real(url, opts); }; \
             const select = document.querySelector('#catalog-select'); \
             select.value = 'other'; \
             select.dispatchEvent(new Event('change')); }",
            json!(null),
        );
        ui.wait_text("#concept-title", "/index.md");
        ui.wait_text("#markdown-body", "Other");
        assert_eq!(
            ui.eval("() => location.hash", json!(null)),
            json!("#/browse/other")
        );
        assert_eq!(ui.eval("() => window.otherTrees", json!(null)), json!(1));
    })
    .await;
}

/// Where the `deep` heading's top is in the viewport, polled for up to 120
/// frames until it is in view (the page renders after its request and
/// scrolls in a `requestAnimationFrame`); `top` is `null` while it is missing.
const DEEP_TOP: &str = "() => new Promise(done => { let frames = 0; const look = () => { \
    const heading = document.querySelector(\"#markdown-body [data-slug='deep']\"); \
    const top = heading ? heading.getBoundingClientRect().top : null; \
    const truncated = document.querySelector('#concept-truncated') !== null; \
    const inView = top !== null && top >= -2 && top < innerHeight; \
    if ((inView && !truncated) || ++frames > 120) done({ top, truncated, height: innerHeight }); \
    else requestAnimationFrame(look); }; look(); })";

/// Asserts the `deep` heading is rendered, with the whole body, and scrolled
/// into view.
fn assert_deep_in_view(ui: &crate::ui_e2e::harness::Ui) {
    ui.wait_visible("#markdown-body h2[data-slug='deep']");
    let place = ui.eval(DEEP_TOP, json!(null));
    assert_eq!(place["truncated"], json!(false), "the whole body: {place}");
    let top = place["top"].as_f64().expect("the heading's top");
    let height = place["height"].as_f64().expect("the viewport height");
    assert!((-2.0..height).contains(&top), "deep is in view: {place}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_section_link_on_a_long_concept_loads_the_body_and_reaches_the_heading() {
    // Enough paragraphs to push `## Deep` well below the first screen.
    let filler = "A paragraph of filler text.\n\n".repeat(120);
    let long = format!(
        "---\ntype: note\ntitle: Long\n---\n# Long\n\n## Top\n\n{filler}## Deep\n\nFound.\n"
    );
    let config = Config {
        concept_outline_chars: 200,
        ..Config::default()
    };
    let server = TestServer::start(
        vec![(
            "main",
            files(&[("index.md", "# Main\n"), ("notes/long.md", long.as_str())]),
        )],
        config,
    )
    .await;
    let key = server
        .key("browse-long", Access::Read, &["main"], Some("main"))
        .await;
    drive(server.base.clone(), move |ui| {
        let deep_link = "#outline a[href='#/browse/main/notes/long.md?s=deep']";
        ui.login(&key);
        // Over the limit: only the outline, until a section link is followed.
        ui.open("/browse/main/notes/long.md");
        ui.wait_visible("#concept-truncated");
        ui.click(deep_link);
        assert_deep_in_view(ui);
        // After "Load the whole body", a section link keeps the whole body.
        ui.open("/browse/main/notes/long.md");
        ui.wait_visible("#concept-full");
        ui.click("#concept-full");
        ui.wait_visible("#markdown-body");
        ui.click(deep_link);
        assert_deep_in_view(ui);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_named_dot_md_is_not_mistaken_for_a_document_with_a_section() {
    let server = TestServer::start(
        vec![(
            "main",
            files(&[
                (
                    "index.md",
                    "# Main\n\n[Topic](archive.md/topic.md) and \
                     [its details](archive.md/topic.md#details).\n",
                ),
                ("archive.md/index.md", "# Archive\n"),
                (
                    "archive.md/topic.md",
                    "---\ntype: note\ntitle: Topic\n---\n# Topic\n\n## Details\n\nHere.\n",
                ),
            ]),
        )],
        Config::default(),
    )
    .await;
    let key = server
        .key("browse-dotmd", Access::Read, &["main"], Some("main"))
        .await;
    drive(server.base.clone(), move |ui| {
        ui.login(&key);
        ui.open("/browse/main");
        ui.wait_visible("#markdown-body");
        // A body link into the folder opens the document inside it.
        ui.click("#markdown-body a[href='#/browse/main/archive.md/topic.md']");
        ui.wait_text("#concept-title", "Topic");
        // A section link names the heading in a parameter.
        ui.open("/browse/main");
        ui.wait_visible("#markdown-body");
        ui.click("#markdown-body a[href='#/browse/main/archive.md/topic.md?s=details']");
        ui.wait_text("#concept-title", "Topic");
        ui.wait_visible("#outline a[href='#/browse/main/archive.md/topic.md?s=details']");
        // The old form ('<path>.md/<slug>') reads the last .md segment as the document.
        ui.open("/browse/main/archive.md/topic.md/details");
        ui.wait_text("#concept-title", "Topic");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_suspended_view_names_the_catalog_and_its_team_by_label() {
    let (server, key) = server().await;
    server.set_catalog_label("main", "Work notes").await;
    server.set_user_label("tester", "Tess").await;
    crate::support::server::applied(&server.app, async {
        okf_mcp::store::teams::suspend(
            &server.db.pool,
            "tester",
            Some("Paused for review."),
            &okf_mcp::store::audit::AuditActor::cli(),
            Utc::now(),
        )
        .await
        .expect("suspend the owner team");
    })
    .await;
    drive(server.base.clone(), move |ui| {
        ui.login(&key);
        ui.open("/browse/main");
        ui.wait_text("#browse-suspended", "Work notes is unavailable");
        let shown = ui.text("#browse-suspended");
        assert!(
            shown.contains("Its team Tess's team is suspended."),
            "{shown}"
        );
        // URLs keep slugs.
        ui.wait_visible("#browse-suspended a[href='#/transfer/main']");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_export_link_uses_the_filename_the_server_chose() {
    let (server, key) = server().await;
    server.set_catalog_label("main", "Work notes").await;
    drive(server.base.clone(), move |ui| {
        ui.login(&key);
        ui.open("/transfer");
        // The picker offers the label; its value stays the slug.
        ui.wait_text("#transfer-catalog", "Work notes");
        ui.click("#export-btn");
        ui.wait_visible("#export-link");
        let link = ui.eval(
            "() => { const a = document.querySelector('#export-link'); \
             return { download: a.download, text: a.textContent }; }",
            json!(null),
        );
        assert_eq!(link["download"], "work-notes-main.tar.gz", "{link}");
        let text = link["text"].as_str().expect("a link text");
        assert!(text.contains("Save work-notes-main.tar.gz again"), "{link}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_export_filename_falls_back_to_the_slug_without_a_usable_header() {
    let (server, key) = server().await;
    drive(server.base.clone(), move |ui| {
        ui.login(&key);
        let names = ui.eval(
            "async () => { const { exportFilename } = await import('/ui/js/pages/transfer.js'); \
             return [exportFilename('attachment; filename=\"a-main.tar.gz\"', 'main'), \
             exportFilename(null, 'main'), exportFilename('attachment', 'main'), \
             exportFilename('attachment; filename=\"../x.tar.gz\"', 'main')]; }",
            json!(null),
        );
        assert_eq!(
            names,
            json!(["a-main.tar.gz", "main.tar.gz", "main.tar.gz", "main.tar.gz"])
        );
    })
    .await;
}
