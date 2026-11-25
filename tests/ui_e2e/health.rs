//! Health, inside Recent changes: its tiles equal the API's counts, the Links
//! report lists an orphan, a report row opens its concept in Browse, the old
//! `#/health` link lands on Recent changes, Health comes before the revision
//! list, and a failed Health read leaves the list working.

use okf_mcp::config::Config;
use okf_mcp::store::keys::Access;
use serde_json::{json, Value};

use crate::harness::drive;
use crate::support::server::{files, TestServer};

const INDEX: &str = "# Main\n\n- [Alpha](systems/alpha.md): the first system\n";
const ALPHA: &str = "---\ntype: system\ntitle: Alpha\ndescription: The first system.\n---\n# Alpha\n\nAlpha runs [Beta](beta.md) and [Gone](gone.md).\n";
const BETA: &str = "---\ntype: system\ntitle: Beta\ndescription: The second system.\nopen_questions:\n  - Who owns Beta?\n---\n# Beta\n\nBeta serves [Alpha](alpha.md).\n";
// No `type` (a lint error), and nothing links here (an orphan).
const GAMMA: &str = "---\ntitle: Gamma\n---\n# Gamma\n\nNothing links here.\n";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn health_tiles_match_the_api_and_a_row_opens_its_concept() {
    let server = TestServer::start(
        vec![(
            "main",
            files(&[
                ("index.md", INDEX),
                ("systems/alpha.md", ALPHA),
                ("systems/beta.md", BETA),
                ("systems/gamma.md", GAMMA),
            ]),
        )],
        Config::default(),
    )
    .await;
    let key = server
        .key("console-health", Access::Read, &["main"], Some("main"))
        .await;
    let health: Value = reqwest::Client::new()
        .get(format!("{}/api/v1/catalogs/main/health", server.base))
        .bearer_auth(&key)
        .send()
        .await
        .expect("GET health")
        .json()
        .await
        .expect("health as JSON");
    assert_eq!(health["orphans"], 1, "{health}");
    assert!(health["lint_errors"].as_u64().unwrap_or(0) >= 1, "{health}");
    assert!(
        health["oldest"]["age_days"].is_null(),
        "no concept is stamped: {health}"
    );
    let broken = health["broken_links"].as_u64().expect("broken_links");
    let broken_line = if broken == 1 {
        "1 broken link".to_string()
    } else {
        format!("{broken} broken links")
    };
    drive(server.base.clone(), move |ui| {
        ui.login(&key);
        // The old standalone address lands on Recent changes.
        ui.open("/health");
        ui.wait_visible("#n-lint");
        assert_eq!(
            ui.eval("() => location.hash", json!(null)),
            json!("#/recent")
        );
        assert_eq!(
            ui.eval(
                "() => document.querySelector('a[data-nav=recent]').classList.contains('active')",
                json!(null)
            ),
            json!(true)
        );
        // Health comes before the revision history.
        assert_eq!(
            ui.eval(
                "() => !!(document.getElementById('health').compareDocumentPosition(\
                 document.getElementById('recent-table')) & Node.DOCUMENT_POSITION_FOLLOWING)",
                json!(null)
            ),
            json!(true)
        );
        ui.wait_text("#recent-table tbody", "/systems/");
        for (tile, field) in [
            ("lint", "lint_errors"),
            ("sprawl", "sprawl"),
            ("orphans", "orphans"),
            ("questions", "open_questions"),
        ] {
            // The tiles are visible before the page's fetch fills them, so
            // wait for each value rather than reading it at once.
            let selector = format!("#n-{tile}");
            let shown = ui.wait_text(&selector, &health[field].to_string());
            assert_eq!(shown, health[field].to_string(), "tile {tile}");
        }
        assert_eq!(ui.text("#d-orphans"), broken_line);
        assert_eq!(ui.text("#n-oldest"), "never");

        ui.click(".segments .seg[data-value=links]");
        ui.wait_text("#report-table", "systems/gamma.md");
        let clicked = ui.eval(
            "(path) => { const row = [...document.querySelectorAll('#report-table tbody tr')]\
             .find(r => r.textContent.includes(path) && r.textContent.includes('orphan')); \
             if (!row) return false; row.click(); return true; }",
            json!("systems/gamma.md"),
        );
        assert_eq!(clicked, json!(true));
        assert_eq!(
            ui.eval("() => location.hash", json!(null)),
            json!("#/browse/main/systems/gamma.md")
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_health_read_leaves_the_revision_list_working_and_refresh_recovers() {
    let server = TestServer::start(
        vec![(
            "main",
            files(&[("index.md", INDEX), ("systems/alpha.md", ALPHA)]),
        )],
        Config::default(),
    )
    .await;
    let key = server
        .key("console-health-fail", Access::Read, &["main"], Some("main"))
        .await;
    drive(server.base.clone(), move |ui| {
        ui.login(&key);
        ui.open("");
        ui.fail_next("GET", "/health", 503, "unavailable");
        ui.click("a[data-nav=recent]");
        ui.wait_text("#health-tiles", "The counts could not be loaded.");
        // The revisions are listed all the same.
        ui.wait_text("#recent-table tbody", "/systems/alpha.md");
        // Refresh on the Health section reads the counts again.
        ui.click("#health-refresh");
        ui.wait_visible("#n-lint");
        ui.wait_visible("#recent-table");
    })
    .await;
}
