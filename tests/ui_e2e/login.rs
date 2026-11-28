//! Signing in and out, a refused key, the theme toggle, what a read key
//! sees, and a start that fails with a stored key.

use okf_mcp::config::Config;
use okf_mcp::store::keys::Access;
use rustwright::GotoOptions;

use crate::support::server::{files, TestServer};
use crate::ui_e2e::harness::drive;
use crate::ui_e2e::self_service::{console_base, login_server, member, EMAIL, PASSWORD};

async fn server() -> TestServer {
    TestServer::start(
        vec![("main", files(&[("index.md", "# Main\n")]))],
        Config::default(),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_admin_signs_in_sees_every_group_and_signs_out() {
    let server = server().await;
    let key = server
        .key("console-admin", Access::Admin, &["main"], Some("main"))
        .await;
    drive(server.base.clone(), move |ui| {
        ui.login(&key);
        ui.wait_text("#whoami", "admin");
        assert_eq!(
            ui.eval(
                "() => document.querySelector('#catalog-select').value",
                serde_json::json!(null)
            ),
            "main"
        );
        assert!(ui.page.is_visible("[data-need=admin]").expect("visible"));
        ui.click("#signout");
        ui.wait_visible("#login");
        // Signed out means the key is gone: a reload stays on the login card.
        ui.open("");
        ui.wait_visible("#login");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wrong_key_is_refused_on_the_login_card() {
    let server = server().await;
    drive(server.base.clone(), |ui| {
        ui.open("");
        ui.wait_visible("#login");
        ui.fill("#login-form input[name=key]", "okf_not-a-real-key");
        ui.click("#login-form button[type=submit]");
        ui.wait_text("#login-error", "not accepted");
        assert!(!ui.page.is_visible("#app").expect("visible"));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_read_key_sees_no_administer_group_and_stays_signed_in() {
    let server = server().await;
    let key = server
        .key("console-reader", Access::Read, &["main"], Some("main"))
        .await;
    drive(server.base.clone(), move |ui| {
        ui.login(&key);
        ui.wait_text("#whoami", "read");
        assert!(!ui.page.is_visible("[data-need=admin]").expect("visible"));
        ui.open("");
        ui.wait_visible("#app"); // localStorage kept the key across the reload
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sign_in_works_when_storage_throws() {
    let server = server().await;
    let key = server
        .key("console-nostorage", Access::Read, &["main"], Some("main"))
        .await;
    drive(server.base.clone(), move |ui| {
        ui.open("");
        ui.wait_visible("#login");
        // Every storage call now throws, as with blocked site data.
        ui.eval(
            "() => { for (const m of ['getItem', 'setItem', 'removeItem']) \
             Storage.prototype[m] = () => { throw new DOMException('denied', 'SecurityError'); }; }",
            serde_json::json!(null),
        );
        ui.fill("#login-form input[name=key]", &key);
        ui.click("#login-form button[type=submit]");
        ui.wait_visible("#app");
        ui.wait_text("#whoami", "read");
        ui.click("#theme-toggle"); // a preference write must not throw either
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_page_render_never_lands_in_a_newer_page() {
    let server = server().await;
    let key = server
        .key("console-router", Access::Read, &["main"], Some("main"))
        .await;
    drive(server.base.clone(), move |ui| {
        ui.login(&key);
        // Two navigations through the real router: the first render is
        // still awaiting when the second runs, then appends late.
        let result = ui.eval(
            "async () => { \
               const router = await import('/ui/js/core/router.js'); \
               let release; \
               const slow = router.mount(async view => { \
                 await new Promise(resolve => { release = resolve; }); \
                 view.append(Object.assign(document.createElement('p'), { id: 'stale' })); }, []); \
               await router.mount(async view => { \
                 view.append(Object.assign(document.createElement('p'), { id: 'fresh' })); }, []); \
               release(); await slow; \
               return [!!document.getElementById('stale'), !!document.getElementById('fresh')]; }",
            serde_json::json!(null),
        );
        assert_eq!(result, serde_json::json!([false, true]));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_theme_toggle_flips_and_persists() {
    let server = server().await;
    let key = server
        .key("console-theme", Access::Read, &["main"], Some("main"))
        .await;
    drive(server.base.clone(), move |ui| {
        ui.login(&key);
        let theme = |ui: &crate::ui_e2e::harness::Ui| {
            ui.eval(
                "() => document.documentElement.dataset.theme",
                serde_json::json!(null),
            )
        };
        let before = theme(ui);
        ui.click("#theme-toggle");
        let after = theme(ui);
        assert_ne!(before, after);
        ui.open("");
        ui.wait_visible("#app");
        assert_eq!(theme(ui), after);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_start_with_a_stored_key_shows_the_login_card() {
    let server = server().await;
    let key = server
        .key("console-boot", Access::Read, &["main"], Some("main"))
        .await;
    let app = std::sync::Arc::clone(&server.app);
    // The journey runs on a blocking thread; this handle lets it run one
    // async call (closing the pool) on the test's runtime.
    let runtime = tokio::runtime::Handle::current();
    drive(server.base.clone(), move |ui| {
        ui.login(&key);
        ui.wait_text("#whoami", "read");
        // The key stays stored, but the next /whoami fails with a 500: the
        // key cache is in memory, and listing key names needs the database.
        runtime.block_on(app.accounts().executor().close());
        ui.page.reload(GotoOptions::default()).expect("reload");
        ui.wait_visible("#login");
        ui.wait_text("#login-error", "could not load");
        assert!(!ui.page.is_visible("#app").expect("visible"));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_header_shows_labels_and_the_catalog_option_keeps_the_slug() {
    use chrono::Utc;
    use okf_mcp::labels::DisplayName;
    use okf_mcp::store::audit::AuditActor;
    use okf_mcp::store::catalogs::NewCatalog;
    use okf_mcp::store::users;

    let server = login_server().await;
    member(&server, Access::Write).await;
    let pool = &server.db.pool;
    users::set_display_name(
        pool,
        "alice",
        &DisplayName::parse("Alice Example").expect("a display name"),
        &AuditActor::cli(),
        Utc::now(),
    )
    .await
    .expect("name alice");
    // alice's personal team owns it, so her ceiling reaches it.
    crate::support::server::applied(&server.app, async {
        crate::support::stores::create_catalog(
            pool,
            &NewCatalog {
                name: "notes",
                display_name: &DisplayName::parse("Work Notes").expect("a display name"),
                description: "",
                owner_team: "alice",
            },
            &AuditActor::cli(),
            Utc::now(),
        )
        .await
        .expect("create the catalog");
    })
    .await;
    drive(console_base(&server), |ui| {
        ui.sign_in_password(EMAIL, PASSWORD);
        ui.wait_text("#whoami", "Alice Example - write");
        ui.wait_text("#catalog-select", "Work Notes");
        // Option text is the label; its value is the slug.
        let options = ui.eval(
            "() => [...document.querySelectorAll('#catalog-select option')] \
             .map(option => [option.textContent, option.value])",
            serde_json::json!(null),
        );
        assert!(
            options
                .as_array()
                .expect("an array")
                .contains(&serde_json::json!(["Work Notes", "notes"])),
            "options were {options}"
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_labels_read_keeps_the_console_open_and_clears_stale_labels() {
    use chrono::Utc;
    use okf_mcp::labels::DisplayName;
    use okf_mcp::store::audit::AuditActor;
    use okf_mcp::store::catalogs::NewCatalog;
    use okf_mcp::store::users;

    let server = login_server().await;
    member(&server, Access::Write).await;
    let pool = &server.db.pool;
    users::set_display_name(
        pool,
        "alice",
        &DisplayName::parse("Alice Example").expect("a display name"),
        &AuditActor::cli(),
        Utc::now(),
    )
    .await
    .expect("name alice");
    // alice's personal team owns it, so her ceiling reaches it.
    crate::support::server::applied(&server.app, async {
        crate::support::stores::create_catalog(
            pool,
            &NewCatalog {
                name: "notes",
                display_name: &DisplayName::parse("Work Notes").expect("a display name"),
                description: "",
                owner_team: "alice",
            },
            &AuditActor::cli(),
            Utc::now(),
        )
        .await
        .expect("create the catalog");
    })
    .await;
    drive(console_base(&server), |ui| {
        ui.sign_in_password(EMAIL, PASSWORD);
        ui.wait_text("#catalog-select", "Work Notes");
        // The next /labels read fails with a 503; /session and /whoami
        // still answer. The identity refresh must still finish, quietly.
        // The shell reads every page, the first with the largest page size.
        ui.fail_next("GET", "/api/v1/labels?limit=1000", 503, "labels are away");
        let done = ui.eval(
            "async () => { \
               const shell = await import('/ui/js/core/shell.js'); \
               await shell.refreshIdentity(); \
               return [...document.querySelectorAll('#catalog-select option')] \
                 .map(option => [option.textContent, option.value]); }",
            serde_json::json!(null),
        );
        // Stale labels are cleared, not kept: text is the slug, as is the value.
        let options = done.as_array().expect("an array");
        assert!(
            options.contains(&serde_json::json!(["notes", "notes"])),
            "options were {done}"
        );
        assert!(
            !options.iter().any(|option| option[0] == "Work Notes"),
            "the old label was kept: {done}"
        );
        // The header still names the person (whoami carries its own label).
        assert!(ui.text("#whoami").contains("Alice Example - write"));
        assert!(ui.page.is_visible("#app").expect("visible"));
        // No error toast for the failed read.
        assert!(
            !ui.text("#toasts").contains("labels are away"),
            "{}",
            ui.text("#toasts")
        );
    })
    .await;
}

/// Answers the page's `GET /api/v1/labels` pages from `arg.pages` (by the
/// `after` they were asked with, `""` for the first), without reaching the
/// server: a caller that reaches more catalogs than one page holds.
const TWO_LABEL_PAGES: &str = "(arg) => {
    const original = window.fetch;
    window.fetch = (input, init) => {
        const url = new URL(String(input), window.location.origin);
        if (((init && init.method) || 'GET') === 'GET' && url.pathname === '/api/v1/labels') {
            const page = arg.pages[url.searchParams.get('after') ?? ''];
            return Promise.resolve(new Response(JSON.stringify(page),
                { status: 200, headers: { 'Content-Type': 'application/json' } }));
        }
        return original(input, init);
    };
    return true;
}";

/// M18 Task 10 (review of PR #712): `GET /labels` scopes `teams` to the
/// owner teams of the catalogs on its page, so the shell keeps every page's
/// teams as well as its catalogs, and a team named only on page two keeps
/// its label.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_shell_keeps_the_owner_team_labels_of_every_labels_page() {
    let server = login_server().await;
    member(&server, Access::Write).await;
    drive(console_base(&server), |ui| {
        ui.sign_in_password(EMAIL, PASSWORD);
        ui.wait_visible("#app");
        ui.eval(
            TWO_LABEL_PAGES,
            serde_json::json!({ "pages": {
                "": { "users": { "alice": "Alice" }, "teams": { "first-team": "First Team" },
                      "catalogs": { "a": "Catalog A" }, "next": "YQ" },
                "YQ": { "users": { "alice": "Alice" }, "teams": { "second-team": "Second Team" },
                        "catalogs": { "b": "Catalog B" } },
            } }),
        );
        let shown = ui.eval(
            "async () => { \
               const shell = await import('/ui/js/core/shell.js'); \
               const labels = await import('/ui/js/core/labels.js'); \
               await shell.refreshIdentity(); \
               return [labels.teamLabel('first-team'), labels.teamLabel('second-team'), \
                       labels.catalogLabel('a'), labels.catalogLabel('b'), \
                       labels.userLabel('alice')]; }",
            serde_json::json!(null),
        );
        assert_eq!(
            shown,
            serde_json::json!([
                "First Team",
                "Second Team",
                "Catalog A",
                "Catalog B",
                "Alice"
            ]),
            "every page's labels are kept"
        );
    })
    .await;
}
