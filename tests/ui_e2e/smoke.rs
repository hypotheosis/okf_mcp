//! The whole console (M9 Task 8): an admin visits every page in turn from
//! the sidebar, each page renders its content or its empty state, and no
//! page logs a console error (`drive` fails the journey on any).

use okf_mcp::config::Config;
use okf_mcp::store::keys::Access;

use crate::support::server::{files, TestServer};
use crate::ui_e2e::harness::drive;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_admin_visits_every_page_without_a_console_error() {
    let server = TestServer::start(
        vec![(
            "main",
            files(&[
                (
                    "index.md",
                    "# Main\n\n- [Alpha](systems/alpha.md): the first system\n",
                ),
                ("systems/index.md", "# Systems\n\n- [Alpha](alpha.md)\n"),
                (
                    "systems/alpha.md",
                    "---\ntype: system\ntitle: Alpha\n---\n# Alpha\n\nAlpha runs.\n",
                ),
            ]),
        )],
        Config::default(),
    )
    .await;
    let key = server
        .key("console-smoke", Access::Admin, &["main"], Some("main"))
        .await;
    drive(server.base.clone(), move |ui| {
        ui.login(&key);

        // Health is part of Recent changes, which comes first here.
        ui.click("a[data-nav=recent]");
        ui.wait_visible("#n-lint");
        ui.wait_visible("#n-oldest");

        ui.click("a[data-nav=browse]");
        ui.wait_text("#concept-title", "/index.md");
        ui.open("/browse/main/systems/alpha.md");
        ui.wait_text("#concept-title", "Alpha");
        ui.wait_text("#markdown-body", "Alpha runs.");

        // The sidebar's search box: Enter shows the results in the panel.
        ui.fill("#sidebar-search-input", "alpha");
        ui.page
            .press_key(Some("#sidebar-search-input"), "Enter")
            .expect("press Enter");
        ui.wait_text("#search-count", "hit");
        assert!(ui
            .text("#search-results")
            .contains("main /systems/alpha.md"));

        ui.click("a[data-nav=findings]");
        ui.wait_text("#findings-table tbody", "No findings here.");

        ui.click("a[data-nav=runs]");
        ui.wait_text("#runs-table tbody", "No maintenance runs yet.");

        ui.click("a[data-nav=changesets]");
        ui.wait_text("#changesets-table", "No open changesets.");

        // The Your account group's, for admins and owners alike.
        ui.click("[data-need=account] a[data-nav=catalogs]");
        ui.wait_visible("[data-catalog=main]");

        ui.click("a[data-nav=transfer]");
        ui.click("#export-btn");
        ui.wait_visible("#export-link");

        ui.click("a[data-nav=keys]");
        ui.wait_visible("[data-test=key-console-smoke]");

        ui.click("a[data-nav=settings]");
        ui.wait_visible("#settings-table");
        ui.wait_visible("#mail-queue-table");

        ui.click("a[data-nav=unlocks]");
        ui.wait_text("#unlocks-table", "No unlocks.");
    })
    .await;
}

/// Another process can listen on the IPv6 loopback at the very port a test
/// server got on `127.0.0.1` (a container engine's port proxy listens on
/// `[::]` for the ports it publishes, and the OS picks an IPv4 port without
/// looking at IPv6). Chromium tries `::1` first for `localhost`, so without
/// the harness's host-resolver rule the page below would be answered by the
/// squatter, with nothing, instead of by the server (issue #500, seen as
/// `ERR_EMPTY_RESPONSE` on the sign-in page).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn localhost_reaches_the_ipv4_server_when_another_process_holds_the_ipv6_port() {
    // The squatter first, on whatever IPv6 port is free; then the server on
    // the same number over IPv4. Without an IPv6 loopback, or when that
    // number is taken over IPv4, there is nothing to prove.
    let Ok(squatter) = tokio::net::TcpListener::bind("[::1]:0").await else {
        eprintln!("skipping: no IPv6 loopback");
        return;
    };
    let port = squatter.local_addr().expect("address").port();
    let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", port)).await else {
        eprintln!("skipping: port {port} is taken over IPv4");
        return;
    };
    // Accepts and closes at once, as a proxy to a container that does not
    // speak HTTP does; runs until the test's runtime shuts down.
    tokio::spawn(async move {
        loop {
            drop(squatter.accept().await);
        }
    });
    let router = axum::Router::new().route(
        "/",
        axum::routing::get(|| async {
            axum::response::Html("<!DOCTYPE html><title>ipv4</title><p id=\"here\">served</p>")
        }),
    );
    tokio::spawn(async move { axum::serve(listener, router).await });

    // `drive` runs nothing when there is no Chromium; then there is nothing to check.
    drive(format!("http://localhost:{port}"), move |ui| {
        ui.page
            .goto(&format!("{}/", ui.base), rustwright::GotoOptions::default())
            .expect("open the page through localhost");
        ui.wait_text("#here", "served");
    })
    .await;
}
