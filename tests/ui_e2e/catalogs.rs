//! Catalog administration and import/export in the console (M9 task 5),
//! and a catalog's team member access (M12 Task 15).

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use chrono::Utc;
use okf_mcp::archive::{write_tarball, ExportFile};
use okf_mcp::config::Config;
use okf_mcp::okf::workspace::Workspace as _;
use okf_mcp::store::audit::AuditActor;
use okf_mcp::store::ceiling::{self, MemberAccess, NewEntitlement};
use okf_mcp::store::keys::Access;
use okf_mcp::store::teams::{self, NewTeam, Role};
use okf_mcp::store::users::AdminChange;
use serde_json::json;

use crate::support::server::{files, TestServer, OWNER};
use crate::ui_e2e::harness::{drive, Ui};
use crate::ui_e2e::self_service::{console_base, login_server, EMAIL, PASSWORD};

const ALPHA: &str = "---\ntype: system\ntitle: Alpha\n---\n# Alpha\n\nAlpha.\n";

/// Puts a file into the import page's `#upload-file`, as a user choosing it
/// would: rustwright has no set_input_files, so the page builds the File from
/// base64 and assigns it through a DataTransfer, then fires `change`.
const CHOOSE_FILE: &str = "(arg) => {
    const bytes = Uint8Array.from(atob(arg.data), char => char.charCodeAt(0));
    const transfer = new DataTransfer();
    transfer.items.add(new File([bytes], arg.name, { type: 'application/gzip' }));
    const input = document.querySelector('#upload-file');
    input.files = transfer.files;
    input.dispatchEvent(new Event('change', { bubbles: true }));
    return input.files.length;
}";

async fn server() -> TestServer {
    TestServer::start(
        vec![(
            "main",
            files(&[("index.md", "# Main\n"), ("systems/alpha.md", ALPHA)]),
        )],
        Config::default(),
    )
    .await
}

/// A `.tar.gz` of `(path, text)` pairs.
fn tarball(entries: &[(&str, &str)]) -> Vec<u8> {
    let modified = chrono::Utc::now();
    let entries: Vec<ExportFile<'_>> = entries
        .iter()
        .map(|&(path, text)| ExportFile {
            path,
            content: text.as_bytes(),
            executable: false,
            modified,
        })
        .collect();
    write_tarball(Vec::new(), &entries).expect("tarball")
}

/// The catalog switcher's options.
pub(crate) fn switcher(ui: &Ui) -> Vec<String> {
    let values = ui.eval(
        "() => [...document.querySelectorAll('#catalog-select option')] \
         .map(option => option.value)",
        json!(null),
    );
    serde_json::from_value(values).expect("a list of names")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_admin_creates_and_deletes_a_catalog_and_the_switcher_follows() {
    let server = server().await;
    // No grants: every catalog, the only kind of key that may create one.
    let key = server
        .key("console-root", Access::Admin, &[], Some("main"))
        .await;
    drive(server.base.clone(), move |ui| {
        ui.login(&key);
        ui.open("/catalogs");
        ui.wait_visible("[data-catalog=main]");
        ui.click("#catalog-create");
        ui.wait_visible("#catalog-create-form");
        ui.fill("#catalog-create-form input[name=name]", "scratch");
        ui.fill(
            "#catalog-create-form textarea[name=description]",
            "Scratch space",
        );
        ui.select("#catalog-create-form select[name=owner]", OWNER);
        ui.click("#catalog-create-form button[type=submit]");
        // The page rebuilds the switcher before it redraws the table.
        ui.wait_visible("[data-catalog=scratch]");
        ui.wait_text("[data-test=catalog-owner-scratch]", "'s team");
        assert_eq!(switcher(ui), ["main", "scratch"]);

        ui.click("[data-catalog=scratch]");
        ui.wait_visible("#catalog-delete");
        ui.click("#catalog-delete");
        ui.wait_visible("#confirm-typed");
        ui.fill("#confirm-typed", "scratch");
        ui.click("#confirm-ok");
        ui.wait_hidden("[data-catalog=scratch]");
        assert_eq!(switcher(ui), ["main"]);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_admin_changes_a_catalogs_owner_and_a_scoped_admin_cannot() {
    let server = server().await;
    let root = server.key("console-root", Access::Admin, &[], None).await;
    let scoped = server
        .key("console-scoped", Access::Admin, &["main"], Some("main"))
        .await;
    let family = NewTeam {
        name: "family",
        display_name: &okf_mcp::labels::DisplayName::default(),
        owner: OWNER,
    };
    teams::create(
        &server.db.pool,
        &family,
        &okf_mcp::store::audit::AuditActor::cli(),
        Utc::now(),
    )
    .await
    .expect("create a team");
    let ran = drive(server.base.clone(), move |ui| {
        // A scoped admin sees the owner, but the control is disabled.
        ui.login(&scoped);
        ui.open("/catalogs");
        ui.wait_text("[data-test=catalog-owner-main]", "'s team");
        ui.click("[data-catalog=main]");
        ui.wait_visible("#catalog-owner-select[disabled]");
        ui.wait_visible("#catalog-owner-save[disabled]");
        ui.click("#drawer-close");
        ui.click("#signout");
        // One that reaches every catalog changes it.
        ui.login(&root);
        ui.open("/catalogs");
        ui.click("[data-catalog=main]");
        // The scoped admin's closed drawer still holds its disabled control
        // until this one replaces it.
        ui.wait_visible("#catalog-owner-select:not([disabled])");
        ui.wait_text("#catalog-owner-save", "Change owner");
        ui.select("#catalog-owner-select", "family");
        ui.click("#catalog-owner-save");
        ui.wait_text("[data-test=catalog-owner-main]", "Unnamed team");
    })
    .await;
    if !ran {
        return;
    }
    let owned = teams::find(&server.db.pool, "family")
        .await
        .expect("read the team");
    assert_eq!(owned.catalogs, ["main"]);
}

/// The admin-only navigation links (API keys, users, teams, settings) that
/// are showing. The Catalogs link, which sits in the "Your account" group
/// and shows to a non-admin signed in with an account too, is not one.
fn visible_admin_links(ui: &Ui) -> serde_json::Value {
    ui.eval(
        "() => ['keys', 'users', 'teams', 'settings'] \
         .filter(nav => [...document.querySelectorAll(`a[data-nav=${nav}]`)] \
             .some(link => link.offsetParent !== null))",
        json!(null),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn export_hands_the_browser_the_catalog_tarball() {
    let server = server().await;
    let key = server
        .key("console-exporter", Access::Read, &["main"], Some("main"))
        .await;
    let browser_key = key.clone();
    drive(server.base.clone(), move |ui| {
        ui.login(&browser_key);
        // A read key reaches the page from the navigation (issue #260),
        // while the Administer group stays hidden.
        ui.wait_visible("a[data-nav=transfer]");
        assert!(!ui.page.is_visible("[data-need=admin]").expect("visible"));
        assert_eq!(visible_admin_links(ui), json!([]));
        ui.click("a[data-nav=transfer]");
        ui.wait_visible("#export-btn");
        ui.click("#export-btn");
        ui.wait_visible("#export-link");
        let link = ui.eval(
            "() => { const a = document.querySelector('#export-link'); \
             return { href: a.href, download: a.download, size: Number(a.dataset.size) }; }",
            json!(null),
        );
        assert!(
            link["href"].as_str().expect("an href").starts_with("blob:"),
            "{link}"
        );
        assert_eq!(link["download"], "untitled-catalog-main.tar.gz");
        assert!(link["size"].as_u64().expect("a size") > 0);
        // A read key sees no import form.
        assert!(!ui.page.is_visible("#import-panel").expect("visible"));
        assert!(!ui.page.is_visible("#import-btn").expect("visible"));
    })
    .await;
    // rustwright cannot save downloads (Deviation 5): fetch what the button
    // fetched, with the same bearer key, and check it is gzip.
    let response = reqwest::Client::new()
        .get(format!("{}/api/v1/catalogs/main/export", server.base))
        .bearer_auth(&key)
        .send()
        .await
        .expect("export");
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "application/gzip");
    let bytes = response.bytes().await.expect("body");
    assert_eq!(bytes[..2], [0x1f, 0x8b]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_import_reports_its_counts_and_a_stale_generation_is_refused() {
    let server = server().await;
    // A `write` key: import needs no more (issue #260).
    let key = server
        .key("console-importer", Access::Write, &["main"], Some("main"))
        .await;
    let generation = server
        .app
        .catalog("main")
        .await
        .unwrap()
        .expect("main")
        .current()
        .generation();
    let archive = tarball(&[
        ("index.md", "# Main\n\nImported.\n"),
        ("systems/alpha.md", ALPHA),
        (
            "systems/gamma.md",
            "---\ntype: system\ntitle: Gamma\n---\n# Gamma\n",
        ),
    ]);
    drive(server.base.clone(), move |ui| {
        ui.login(&key);
        ui.wait_visible("a[data-nav=transfer]");
        assert_eq!(visible_admin_links(ui), json!([]));
        ui.click("a[data-nav=transfer]");
        ui.wait_text("#transfer-generation", &format!("generation {generation}"));
        // A writer exports too.
        ui.click("#export-btn");
        ui.wait_visible("#export-link");
        let prefilled = ui.eval(
            "() => document.querySelector('#import-generation').value",
            json!(null),
        );
        assert_eq!(prefilled, json!(generation.to_string()));
        ui.wait_visible("#upload-file");
        let chosen = ui.eval(
            CHOOSE_FILE,
            json!({ "name": "main.tar.gz", "data": STANDARD.encode(&archive) }),
        );
        assert_eq!(chosen, json!(1));
        ui.wait_text("#upload-name", "main.tar.gz");
        ui.click("#import-replace");
        ui.click("#import-btn");
        ui.wait_visible("#import-report");
        assert_eq!(ui.text("#import-added"), "1");
        assert_eq!(ui.text("#import-updated"), "1");
        assert_eq!(ui.text("#import-deleted"), "0");
        assert_eq!(ui.text("#import-unchanged"), "1");
        ui.wait_text("#import-lint", "Lint:");
        // The page re-reads the catalog: the field now holds the new generation.
        ui.wait_text(
            "#transfer-generation",
            &format!("generation {}", generation + 1),
        );

        // The same file against an old generation: refused, and the panel says why.
        ui.fill("#import-generation", &generation.to_string());
        ui.click("#import-btn");
        ui.wait_text("#import-error", "export it again");
        assert!(!ui.page.is_visible("#import-report").expect("visible"));
    })
    .await;
    assert_eq!(
        server
            .app
            .catalog("main")
            .await
            .unwrap()
            .expect("main")
            .current()
            .generation(),
        generation + 1
    );
}

/// Delays every export `fetch` by 1.5 s, so a journey can act while one is
/// in flight.
const DELAY_EXPORTS: &str = "() => {
    const real = window.fetch;
    window.fetch = (url, options) => String(url).includes('/export')
        ? new Promise(done => setTimeout(done, 1500)).then(() => real.call(window, url, options))
        : real.call(window, url, options);
    return true;
}";

/// Delays every `XMLHttpRequest` send (the import upload) by 1.5 s and
/// counts them in `window.importPosts`.
const DELAY_UPLOADS: &str = "() => {
    const send = XMLHttpRequest.prototype.send;
    window.importPosts = 0;
    XMLHttpRequest.prototype.send = function (body) {
        window.importPosts += 1;
        setTimeout(() => send.call(this, body), 1500);
    };
    return true;
}";

/// Whether each of `selectors` is disabled, in order.
fn disabled(ui: &Ui, selectors: &[&str]) -> Vec<bool> {
    let states = ui.eval(
        "(selectors) => selectors.map(selector => document.querySelector(selector).disabled)",
        json!(selectors),
    );
    serde_json::from_value(states).expect("a list of booleans")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_export_or_import_in_flight_locks_the_catalog_and_admits_no_second_import() {
    let server = TestServer::start(
        vec![
            (
                "main",
                files(&[("index.md", "# Main\n"), ("systems/alpha.md", ALPHA)]),
            ),
            ("other", files(&[("index.md", "# Other\n")])),
        ],
        Config::default(),
    )
    .await;
    let key = server
        .key(
            "console-busy",
            Access::Admin,
            &["main", "other"],
            Some("main"),
        )
        .await;
    let generation = server
        .app
        .catalog("main")
        .await
        .unwrap()
        .expect("main")
        .current()
        .generation();
    let first = STANDARD.encode(tarball(&[("index.md", "# Main\n\nFirst.\n")]));
    let second = STANDARD.encode(tarball(&[("index.md", "# Main\n\nSecond.\n")]));
    let controls = ["#transfer-catalog", "#export-btn", "#import-btn"];
    drive(server.base.clone(), move |ui| {
        ui.login(&key);
        ui.open("/transfer/main");
        ui.wait_text("#transfer-generation", &format!("generation {generation}"));

        // An export in flight: the picker cannot move to `other` under it,
        // and the file it hands over is named for the catalog it fetched.
        ui.eval(DELAY_EXPORTS, json!(null));
        ui.click("#export-btn");
        assert_eq!(disabled(ui, &controls), [true, true, true]);
        ui.wait_visible("#export-link");
        let download = ui.eval(
            "() => document.querySelector('#export-link').download",
            json!(null),
        );
        assert_eq!(download, json!("untitled-catalog-main.tar.gz"));
        assert_eq!(disabled(ui, &controls[..2]), [false, false]);

        // An import in flight: choosing another file does not unlock Import,
        // and a click on it sends nothing.
        ui.eval(DELAY_UPLOADS, json!(null));
        ui.eval(
            CHOOSE_FILE,
            json!({ "name": "first.tar.gz", "data": first }),
        );
        ui.click("#import-replace");
        ui.click("#import-btn");
        ui.eval(
            CHOOSE_FILE,
            json!({ "name": "second.tar.gz", "data": second }),
        );
        assert_eq!(disabled(ui, &controls), [true, true, true]);
        let posts = ui.eval(
            "() => { document.querySelector('#import-btn').click(); return window.importPosts; }",
            json!(null),
        );
        assert_eq!(posts, json!(1));
        ui.wait_visible("#import-report");
        ui.wait_text(
            "#transfer-generation",
            &format!("generation {}", generation + 1),
        );
        let posts = ui.eval("() => window.importPosts", json!(null));
        assert_eq!(posts, json!(1));
        // Done: the second file may be imported now.
        assert_eq!(disabled(ui, &controls), [false, false, false]);
    })
    .await;
    assert_eq!(
        server
            .app
            .catalog("main")
            .await
            .unwrap()
            .expect("main")
            .current()
            .generation(),
        generation + 1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_admin_signed_in_with_a_password_imports_an_archive() {
    // `public_url` is the server's own address, so `/signin` exists.
    let server = TestServer::start_with(
        vec![(
            "main",
            files(&[("index.md", "# Main\n"), ("systems/alpha.md", ALPHA)]),
        )],
        |base| Config {
            public_url: Some(base.to_string()),
            ..Config::default()
        },
    )
    .await;
    let password = "violet-lantern-orbit-42";
    server
        .user_with_password("erin", "erin@example.com", password)
        .await;
    crate::support::server::applied(&server.app, async {
        okf_mcp::store::users::set_admin(
            &server.db.pool,
            "erin",
            AdminChange {
                on: true,
                allow_last: false,
            },
            &AuditActor::cli(),
            Utc::now(),
        )
        .await
        .expect("make erin an admin");
    })
    .await;
    let generation = server
        .app
        .catalog("main")
        .await
        .unwrap()
        .expect("main")
        .current()
        .generation();
    let archive = tarball(&[
        ("index.md", "# Main\n\nImported by a session.\n"),
        ("systems/alpha.md", ALPHA),
    ]);

    let ran = drive(server.base.clone(), move |ui| {
        ui.sign_in_password("erin@example.com", password);
        ui.wait_text("#whoami", "erin - admin");
        // The cookie session alone: no key in this browser.
        assert_eq!(
            ui.eval("() => localStorage.getItem('okf.key')", json!(null)),
            serde_json::Value::Null
        );
        // An admin keeps the Administer group, and Import & export sits
        // outside it.
        ui.wait_visible("[data-need=admin]");
        assert_eq!(
            ui.eval(
                "() => !!document.querySelector('[data-need=admin] a[data-nav=transfer]')",
                json!(null)
            ),
            json!(false)
        );
        ui.click("a[data-nav=transfer]");
        ui.wait_text("#transfer-generation", &format!("generation {generation}"));
        ui.wait_visible("#upload-file");
        let chosen = ui.eval(
            CHOOSE_FILE,
            json!({ "name": "main.tar.gz", "data": STANDARD.encode(&archive) }),
        );
        assert_eq!(chosen, json!(1));
        ui.wait_text("#upload-name", "main.tar.gz");
        ui.click("#import-replace");
        // The upload is a write, so it carries the session's CSRF token.
        ui.click("#import-btn");
        ui.wait_visible("#import-report");
        assert_eq!(ui.text("#import-updated"), "1");
        ui.wait_text(
            "#transfer-generation",
            &format!("generation {}", generation + 1),
        );
    })
    .await;
    if !ran {
        return;
    }
    let current = server
        .app
        .catalog("main")
        .await
        .unwrap()
        .expect("main")
        .current();
    assert_eq!(current.generation(), generation + 1);
}

/// Issue #260: a member signed in with an account, entitled to `main` at
/// `write` and `other` at `read`, finds Import & export in the navigation
/// (no Administer group), sees only those two catalogs in the picker (not
/// `private`), gets the import form only on `main`, and imports there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_imports_where_they_write_and_only_exports_where_they_read() {
    let server = TestServer::start_with(
        vec![
            ("main", files(&[("index.md", "# Main\n")])),
            ("other", files(&[("index.md", "# Other\n")])),
            ("private", files(&[("index.md", "# Private\n")])),
        ],
        |base| Config {
            public_url: Some(base.replace("127.0.0.1", "localhost")),
            ..Config::default()
        },
    )
    .await;
    server.user_with_password("alice", EMAIL, PASSWORD).await;
    crate::support::server::applied(&server.app, async {
        for (catalog, access) in [("main", Access::Write), ("other", Access::Read)] {
            ceiling::entitle(
                &server.db.pool,
                &NewEntitlement {
                    user: "alice",
                    catalog,
                    access,
                },
                &AuditActor::cli(),
                Utc::now(),
            )
            .await
            .expect("entitle");
        }
    })
    .await;
    // The generation `app` serves for catalog `name`.
    async fn generation(app: &okf_mcp::app::App, name: &str) -> i64 {
        app.catalog(name)
            .await
            .unwrap()
            .expect(name)
            .current()
            .generation()
    }
    let (main_before, other_before) = (
        generation(&server.app, "main").await,
        generation(&server.app, "other").await,
    );
    let archive = tarball(&[("index.md", "# Main\n\nImported by a member.\n")]);

    let ran = drive(console_base(&server), move |ui| {
        ui.sign_in_password(EMAIL, PASSWORD);
        ui.wait_visible("a[data-nav=transfer]");
        assert!(!ui.page.is_visible("[data-need=admin]").expect("visible"));
        assert_eq!(visible_admin_links(ui), json!([]));
        ui.click("a[data-nav=transfer]");
        ui.select("#transfer-catalog", "main");
        let options = ui.eval(
            "() => [...document.querySelectorAll('#transfer-catalog option')].map(o => o.value)",
            json!(null),
        );
        assert_eq!(options, json!(["main", "other"]), "only reachable catalogs");
        ui.wait_text("#transfer-generation", &format!("generation {main_before}"));
        ui.wait_visible("#import-panel");
        // Read-only on `other`: export, but no import form.
        ui.select("#transfer-catalog", "other");
        ui.wait_text(
            "#transfer-generation",
            &format!("generation {other_before}"),
        );
        ui.wait_hidden("#import-panel");
        ui.click("#export-btn");
        ui.wait_visible("#export-link");
        // Back on `main`, the form returns, and the import goes through.
        ui.select("#transfer-catalog", "main");
        ui.wait_visible("#import-panel");
        let chosen = ui.eval(
            CHOOSE_FILE,
            json!({ "name": "main.tar.gz", "data": STANDARD.encode(&archive) }),
        );
        assert_eq!(chosen, json!(1));
        ui.wait_text("#upload-name", "main.tar.gz");
        ui.click("#import-replace");
        ui.click("#import-btn");
        ui.wait_visible("#import-report");
        assert_eq!(ui.text("#import-updated"), "1");
        ui.wait_text(
            "#transfer-generation",
            &format!("generation {}", main_before + 1),
        );
    })
    .await;
    if !ran {
        return;
    }
    assert_eq!(generation(&server.app, "main").await, main_before + 1);
    assert_eq!(
        generation(&server.app, "other").await,
        other_before,
        "nothing imported there"
    );
    let main = server
        .app
        .catalog("main")
        .await
        .unwrap()
        .expect("main")
        .current();
    assert_eq!(
        main.workspace()
            .read_to_string("index.md")
            .expect("index.md"),
        "# Main\n\nImported by a member.\n"
    );
}

/// Issue #647: while a catalog's owner team is suspended, a writer sees the
/// Import panel disabled with a note saying why (the server would refuse the
/// upload with 403 `Suspended`), export still works, and an admin keeps a
/// working import.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_suspended_catalog_disables_import_for_a_writer_but_not_an_admin() {
    let server = server().await;
    let writer = server
        .key(
            "console-suspended-writer",
            Access::Write,
            &["main"],
            Some("main"),
        )
        .await;
    let admin = server
        .key("console-suspended-admin", Access::Admin, &[], Some("main"))
        .await;
    crate::support::server::applied(&server.app, async {
        teams::suspend(
            &server.db.pool,
            OWNER,
            Some("Paused for review."),
            &AuditActor::cli(),
            Utc::now(),
        )
        .await
        .expect("suspend the owner team");
    })
    .await;
    let generation = server
        .app
        .catalog("main")
        .await
        .unwrap()
        .expect("main")
        .current()
        .generation();
    let archive = tarball(&[("index.md", "# Main\n\nImported by an admin.\n")]);
    // The import controls' state: the note, and whether each control is
    // disabled (the button itself, and the fieldset holding the file input).
    const CONTROLS: &str = "() => ({ \
        note: !document.querySelector('#import-suspended').hidden, \
        button: document.querySelector('#import-btn').disabled, \
        fieldset: document.querySelector('#import-controls').disabled, \
        file: document.querySelector('#upload-file').matches(':disabled') })";

    let writer_archive = archive.clone();
    drive(server.base.clone(), move |ui| {
        ui.login(&writer);
        ui.open("/transfer/main");
        ui.wait_visible("#import-panel");
        ui.wait_visible("#import-suspended");
        let text = ui.text("#import-suspended");
        assert!(
            text.contains("suspended") && text.contains("only an admin"),
            "{text}"
        );
        // A file chosen anyway (the input is disabled, so only a script can
        // do it) must not enable the button.
        let chosen = ui.eval(
            CHOOSE_FILE,
            json!({ "name": "main.tar.gz", "data": STANDARD.encode(&writer_archive) }),
        );
        assert_eq!(chosen, json!(1));
        assert_eq!(
            ui.eval(CONTROLS, json!(null)),
            json!({ "note": true, "button": true, "fieldset": true, "file": true })
        );
        // Export is not refused for a suspended catalog.
        ui.click("#export-btn");
        ui.wait_visible("#export-link");
    })
    .await;

    let ran = drive(server.base.clone(), move |ui| {
        ui.login(&admin);
        ui.open("/transfer/main");
        ui.wait_visible("#import-panel");
        assert_eq!(
            ui.eval(CONTROLS, json!(null)),
            json!({ "note": false, "button": true, "fieldset": false, "file": false }),
            "no file chosen yet: only the button waits for one"
        );
        let chosen = ui.eval(
            CHOOSE_FILE,
            json!({ "name": "main.tar.gz", "data": STANDARD.encode(&archive) }),
        );
        assert_eq!(chosen, json!(1));
        ui.wait_text("#upload-name", "main.tar.gz");
        ui.click("#import-replace");
        ui.click("#import-btn");
        ui.wait_visible("#import-report");
    })
    .await;
    if !ran {
        return;
    }
    assert_eq!(
        server
            .app
            .catalog("main")
            .await
            .unwrap()
            .expect("main")
            .current()
            .generation(),
        generation + 1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn member_access_read_makes_a_team_catalog_read_only_for_its_members() {
    let server = login_server().await;
    server.user_with_password("alice", EMAIL, PASSWORD).await;
    let pool = &server.db.pool;
    let family = NewTeam {
        name: "family",
        display_name: &okf_mcp::labels::DisplayName::default(),
        owner: OWNER,
    };
    teams::create(
        pool,
        &family,
        &okf_mcp::store::audit::AuditActor::cli(),
        Utc::now(),
    )
    .await
    .expect("create a team");
    teams::set_member(
        pool,
        &okf_mcp::store::teams::Membership {
            team: "family",
            user: "alice",
            role: Role::Member,
        },
        &okf_mcp::store::audit::AuditActor::cli(),
        Utc::now(),
    )
    .await
    .expect("add alice");
    crate::support::server::applied(&server.app, async {
        crate::support::stores::create_catalog(
            pool,
            &okf_mcp::store::catalogs::NewCatalog {
                name: "shared",
                display_name: &okf_mcp::labels::DisplayName::default(),
                description: "",
                owner_team: "family",
            },
            &okf_mcp::store::audit::AuditActor::cli(),
            Utc::now(),
        )
        .await
        .expect("create the team's catalog");
    })
    .await;
    let admin = server.key("console-admin", Access::Admin, &[], None).await;
    let ran = drive(console_base(&server), move |ui| {
        ui.login(&admin);
        ui.open("/catalogs");
        ui.click("[data-catalog=shared]");
        ui.wait_text("#catalog-member-access-current", "write");
        // Nothing to save until the choice differs from the server's value.
        ui.wait_visible("#catalog-member-access-save[disabled]");
        // A refused save: its reason stays in the drawer, the value is read
        // again (still write), and the drawer stays usable.
        // The console has no control a full admin can get refused through,
        // so the page's fetch answers this one.
        ui.fail_next("PUT", "/member-access", 409, "refused by the test");
        ui.select("#catalog-member-access-select", "read");
        ui.click("#catalog-member-access-save");
        ui.wait_visible("#catalog-member-access-error");
        ui.wait_text("#catalog-member-access-error", "refused by the test");
        ui.wait_text("#catalog-member-access-current", "write");
        ui.wait_visible("#catalog-member-access-select:not([disabled])");
        // Refused again, and the read after it fails too: the reason stays,
        // the last value is marked out of date, and Save can be tried again.
        ui.fail_next("PUT", "/member-access", 409, "refused again");
        ui.fail_next("GET", "/member-access", 503, "the server is unavailable");
        ui.select("#catalog-member-access-select", "read");
        ui.click("#catalog-member-access-save");
        ui.wait_text("#catalog-member-access-error", "refused again");
        ui.wait_text("#catalog-member-access-current", "may be out of date");
        ui.wait_visible("#catalog-member-access-select:not([disabled])");
        ui.wait_visible("#catalog-member-access-save:not([disabled])");
        // Then a real one.
        ui.select("#catalog-member-access-select", "read");
        ui.click("#catalog-member-access-save");
        ui.wait_text("#toasts", "now reach shared at read");
        ui.wait_hidden("#catalog-member-access-error");
        ui.wait_text_gone("#catalog-member-access-current", "out of date");
        // A save that settles after the drawer was closed and opened again
        // shows in the new drawer.
        ui.hold_next("PUT", "/member-access");
        ui.select("#catalog-member-access-select", "write");
        ui.click("#catalog-member-access-save");
        ui.click("#drawer-close");
        ui.click("[data-catalog=shared]");
        ui.wait_text("#catalog-member-access-current", "read");
        ui.wait_visible("#catalog-member-access-select[disabled]");
        ui.release_held();
        ui.wait_text("#catalog-member-access-current", "write");
        ui.wait_visible("#catalog-member-access-select:not([disabled])");
        // Back to read for alice, below.
        ui.select("#catalog-member-access-select", "read");
        ui.click("#catalog-member-access-save");
        ui.wait_text_gone("#catalog-member-access-current", "write");
        ui.wait_text("#catalog-member-access-current", "read");
        // The drawer stays open and shows the value read again.
        ui.wait_text("#catalog-member-access-current", "read");
        ui.wait_visible("#catalog-member-access-save[disabled]");
        ui.click("#drawer-close");
        ui.click("#signout");
        ui.wait_visible("#login");
        // alice reaches the team's catalog read-only.
        ui.sign_in_password(EMAIL, PASSWORD);
        ui.open("/my-keys");
        ui.click("#my-key-new");
        // The picker reads the ceiling a page at a time (issue #736).
        ui.wait_visible("#my-key-form select[name=level-shared]");
        assert_eq!(
            ui.eval(
                "() => [...document.querySelectorAll('#my-key-form select[name=level-shared] option')] \
                 .map(option => option.value)",
                json!(null)
            ),
            json!(["read"])
        );
    })
    .await;
    if !ran {
        return;
    }
    let access = ceiling::member_access(&server.db.pool, "shared")
        .await
        .expect("read member access");
    assert_eq!(access, MemberAccess::Read);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_scoped_admin_cannot_change_member_access() {
    let server = server().await;
    let scoped = server
        .key("console-scoped", Access::Admin, &["main"], Some("main"))
        .await;
    drive(server.base.clone(), move |ui| {
        ui.login(&scoped);
        ui.open("/catalogs");
        ui.click("[data-catalog=main]");
        // Read, but not changeable.
        ui.wait_text("#catalog-member-access-current", "write");
        ui.wait_visible("#catalog-member-access-select[disabled]");
        ui.wait_visible("#catalog-member-access-save[disabled]");
        ui.wait_text("#catalog-member-access", "every catalog");
        // The page itself carries no stray "null" under the table.
        assert!(!ui.text("#main").contains("null"));
    })
    .await;
}

/// The value of the input `selector`.
fn input_value(ui: &Ui, selector: &str) -> serde_json::Value {
    ui.eval(
        "(selector) => document.querySelector(selector).value",
        json!(selector),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_admin_edits_a_catalogs_display_name_and_description_together() {
    let server = server().await;
    let root = server.key("console-root", Access::Admin, &[], None).await;
    crate::ui_e2e::account::acme_with_owner_and_member(&server).await;
    let ran = drive(server.base.clone(), move |ui| {
        ui.login(&root);
        ui.open("/catalogs");
        // The label heads the cell with the slug beneath; Owner is the team's label.
        let cell = ui.wait_text("[data-catalog=acme-notes]", "acme-notes");
        assert!(cell.contains("acme-notes"), "{cell}");
        ui.wait_text("[data-test=catalog-owner-acme-notes]", "Acme");
        ui.click("[data-catalog=acme-notes]");
        ui.wait_visible("#catalog-display-name");
        ui.fill("#catalog-display-name", "Notes");
        ui.fill("#catalog-description", "Shared notes");
        ui.click("#catalog-save");
        ui.wait_text("#toasts", "Saved Notes");
        ui.wait_text("[data-catalog=acme-notes]", "Notes");
        ui.wait_text("#catalog-select", "Notes");
    })
    .await;
    if !ran {
        return;
    }
    let row = okf_mcp::store::catalogs::find(&server.db.pool, "acme-notes")
        .await
        .expect("the catalog");
    assert_eq!(row.display_name, "Notes");
    assert_eq!(row.description, "Shared notes");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_non_admin_team_owner_renames_their_catalog_and_a_member_sees_no_editor() {
    let server = login_server().await;
    crate::ui_e2e::account::acme_with_owner_and_member(&server).await;
    let ran = drive(console_base(&server), |ui| {
        ui.sign_in_password("carol@example.com", PASSWORD);
        ui.open("/catalogs");
        ui.click("[data-catalog=acme-notes]");
        ui.wait_visible("#catalog-maintenance-section");
        assert!(!ui
            .page
            .is_visible("#catalog-display-name")
            .expect("visible"));
        assert!(!ui.page.is_visible("#catalog-save").expect("visible"));
        // A member who is not an owner gets no access controls either.
        for hidden in ["#catalog-member-access", "#catalog-entitlements"] {
            assert!(!ui.page.is_visible(hidden).expect("visible"), "{hidden}");
        }
    })
    .await;
    if !ran {
        return;
    }
    let ran = drive(console_base(&server), |ui| {
        ui.sign_in_password("bob@example.com", PASSWORD);
        ui.open("/catalogs");
        ui.click("[data-catalog=acme-notes]");
        ui.wait_visible("#catalog-display-name");
        // No team picker or Delete: the admin's alone. The owner of a
        // shared team has the team's member access and entitlements.
        assert!(!ui
            .page
            .is_visible("#catalog-owner-select")
            .expect("visible"));
        ui.wait_visible("#catalog-member-access");
        ui.wait_visible("#catalog-entitlements");
        assert!(!ui.page.is_visible("#catalog-delete").expect("visible"));
        assert_eq!(input_value(ui, "#catalog-display-name"), "");
        ui.fill("#catalog-display-name", "Acme Notes");
        ui.click("#catalog-save");
        ui.wait_text("#toasts", "Saved Acme Notes");
        ui.wait_text("[data-catalog=acme-notes]", "Acme Notes");
    })
    .await;
    if !ran {
        return;
    }
    let row = okf_mcp::store::catalogs::find(&server.db.pool, "acme-notes")
        .await
        .expect("the catalog");
    assert_eq!(row.display_name, "Acme Notes");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_team_owner_sets_member_access_and_entitles_a_member_from_the_drawer() {
    let server = login_server().await;
    crate::ui_e2e::account::acme_with_owner_and_member(&server).await;
    let ran = drive(console_base(&server), |ui| {
        ui.sign_in_password("bob@example.com", PASSWORD);
        ui.open("/catalogs");
        ui.click("[data-catalog=acme-notes]");
        // Member access: lower it to read, and the server's value shows.
        ui.wait_text("#catalog-member-access-current", "write");
        ui.wait_visible("#catalog-member-access-select:not([disabled])");
        ui.select("#catalog-member-access-select", "read");
        ui.click("#catalog-member-access-save");
        ui.wait_text("#catalog-member-access-current", "read");
        // Entitle the team's member above it, from the members only.
        ui.wait_visible("#catalog-entitle-user:not([disabled])");
        ui.select("#catalog-entitle-user", "carol");
        ui.select("#catalog-entitle-level", "write");
        ui.click("#catalog-entitle-save");
        ui.wait_text("#catalog-entitlements-list", "carol: write");
        ui.wait_text("#toasts", "now reaches");
        // Taking it away.
        ui.click("[data-remove=carol]");
        ui.wait_text_gone("#catalog-entitlements-list", "carol: write");
        ui.wait_text("#catalog-entitlements-list", "No one is entitled");
    })
    .await;
    if !ran {
        return;
    }
    // The writes happened and are what the server holds.
    let pool = &server.db.pool;
    assert_eq!(
        ceiling::member_access(pool, "acme-notes")
            .await
            .expect("member access"),
        MemberAccess::Read
    );
    assert!(ceiling::entitlements(pool, "carol")
        .await
        .expect("entitlements")
        .is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_large_team_is_picked_from_a_page_at_a_time_with_load_more() {
    let server = login_server().await;
    crate::ui_e2e::account::acme_with_owner_and_member(&server).await;
    // 102 members in all: bob, carol and 100 more, past the page of 100.
    let pool = &server.db.pool;
    for number in 0..100 {
        let name = format!("m{number:03}");
        okf_mcp::store::users::create(
            pool,
            &okf_mcp::store::users::NewUser {
                name: &name,
                display_name: &okf_mcp::labels::DisplayName::default(),
            },
            &AuditActor::cli(),
            Utc::now(),
        )
        .await
        .expect("create a member");
        teams::set_member(
            pool,
            &teams::Membership {
                team: "acme",
                user: &name,
                role: Role::Member,
            },
            &AuditActor::cli(),
            Utc::now(),
        )
        .await
        .expect("add the member");
    }
    let ran = drive(console_base(&server), |ui| {
        ui.sign_in_password("bob@example.com", PASSWORD);
        ui.open("/catalogs");
        ui.click("[data-catalog=acme-notes]");
        ui.wait_visible("#catalog-entitle-user:not([disabled])");
        // The first page holds 100 of the 102: the last names wait.
        ui.wait_visible("#catalog-members-more");
        ui.click("#catalog-members-more");
        ui.wait_hidden("#catalog-members-more");
        // Picked from the continued page.
        ui.select("#catalog-entitle-user", "m099");
    })
    .await;
    if !ran {
        return;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_entitle_in_flight_is_sent_once_and_a_refusal_shows_its_reason() {
    let server = login_server().await;
    crate::ui_e2e::account::acme_with_owner_and_member(&server).await;
    let ran = drive(console_base(&server), |ui| {
        ui.sign_in_password("bob@example.com", PASSWORD);
        ui.open("/catalogs");
        ui.click("[data-catalog=acme-notes]");
        ui.wait_visible("#catalog-entitle-user:not([disabled])");
        ui.select("#catalog-entitle-user", "carol");
        // Held: a double click sends one request.
        ui.hold_next("PUT", "/entitlements/acme-notes");
        assert!(
            ui.click_twice("#catalog-entitle-save"),
            "Entitle is not disabled while it runs"
        );
        assert_eq!(ui.held_matches(), 1, "a second entitle was sent");
        ui.release_held();
        ui.wait_text("#catalog-entitlements-list", "carol: read");
        // A refused write keeps its reason on the page.
        ui.fail_next(
            "PUT",
            "/entitlements/acme-notes",
            403,
            "refused by the test",
        );
        ui.select("#catalog-entitle-level", "write");
        ui.click("#catalog-entitle-save");
        ui.wait_visible("#catalog-entitlements-error");
        ui.wait_text("#catalog-entitlements-error", "refused by the test");
        ui.wait_text("#catalog-entitlements-list", "carol: read");
        ui.wait_visible("#catalog-entitle-save:not([disabled])");
    })
    .await;
    if !ran {
        return;
    }
    let given = ceiling::entitlements(&server.db.pool, "carol")
        .await
        .expect("entitlements");
    assert_eq!(given.len(), 1);
    assert_eq!(given[0].access, Access::Read);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_delete_dialog_still_asks_for_the_slug_of_a_labelled_catalog() {
    let server = server().await;
    let root = server.key("console-root", Access::Admin, &[], None).await;
    crate::ui_e2e::account::acme_with_owner_and_member(&server).await;
    drive(server.base.clone(), move |ui| {
        ui.login(&root);
        ui.open("/catalogs");
        ui.click("[data-catalog=acme-notes]");
        ui.wait_visible("#catalog-display-name");
        ui.fill("#catalog-display-name", "Notes");
        ui.click("#catalog-save");
        ui.wait_text("[data-catalog=acme-notes]", "Notes");
        ui.click("[data-catalog=acme-notes]");
        ui.click("#catalog-delete");
        ui.wait_text("dialog.confirm", "Delete Notes?");
        ui.wait_text("dialog.confirm", "acme-notes");
        ui.fill("#confirm-typed", "Notes");
        assert_eq!(disabled(ui, &["#confirm-ok"]), [true]);
        ui.fill("#confirm-typed", "acme-notes");
        assert_eq!(disabled(ui, &["#confirm-ok"]), [false]);
    })
    .await;
}

/// The option texts of the `<select>` `selector`, as `[value, text]` pairs.
fn options(ui: &Ui, selector: &str) -> serde_json::Value {
    ui.eval(
        "(selector) => [...document.querySelector(selector).options].map(o => [o.value, o.text])",
        serde_json::json!(selector),
    )
}

/// Whether the options of `selector` hold `value` with exactly the text
/// `text`.
fn has_option(ui: &Ui, selector: &str, value: &str, text: &str) -> bool {
    options(ui, selector).as_array().is_some_and(|all| {
        all.iter()
            .any(|pair| pair == &serde_json::json!([value, text]))
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn team_pickers_say_personal_team_or_team_and_still_send_the_bare_name() {
    let server = server().await;
    let root = server.key("console-root", Access::Admin, &[], None).await;
    let family = NewTeam {
        name: "acme",
        display_name: &okf_mcp::labels::DisplayName::parse("Acme").expect("a display name"),
        owner: OWNER,
    };
    teams::create(
        &server.db.pool,
        &family,
        &okf_mcp::store::audit::AuditActor::cli(),
        Utc::now(),
    )
    .await
    .expect("create a team");
    // Give the fixture user a distinct display name (this also reloads the label
    // table the console reads), so the full option texts are deterministic.
    server.set_user_label(OWNER, "Alice Example").await;
    let personal_text = "Alice Example (personal team)";
    let ran = drive(server.base.clone(), move |ui| {
        ui.login(&root);
        ui.open("/catalogs");
        // The owner column of the existing catalog names its team's kind.
        ui.wait_text("[data-test=catalog-owner-main]", "personal team");
        // The create form's picker: values are bare names, texts say the kind.
        ui.click("#catalog-create");
        ui.wait_visible("#catalog-create-form select[name=owner]");
        ui.wait_text("#catalog-create-form select[name=owner]", "Acme (team)");
        let create_owner = "#catalog-create-form select[name=owner]";
        assert!(
            has_option(ui, create_owner, OWNER, personal_text),
            "{}",
            options(ui, create_owner)
        );
        assert!(has_option(ui, create_owner, "acme", "Acme (team)"));
        ui.fill("#catalog-create-form input[name=name]", "scratch");
        ui.select("#catalog-create-form select[name=owner]", OWNER);
        ui.click("#catalog-create-form button[type=submit]");
        ui.wait_visible("[data-catalog=scratch]");
        // Change owner: the same labels, and the bare name is what is sent.
        ui.click("[data-catalog=scratch]");
        ui.wait_visible("#catalog-owner-select:not([disabled])");
        ui.wait_text("#catalog-owner-select", "Acme (team)");
        assert!(has_option(
            ui,
            "#catalog-owner-select",
            OWNER,
            personal_text
        ));
        assert!(has_option(
            ui,
            "#catalog-owner-select",
            "acme",
            "Acme (team)"
        ));
        ui.select("#catalog-owner-select", "acme");
        ui.click("#catalog-owner-save");
        ui.wait_text("[data-test=catalog-owner-scratch]", "Acme");
    })
    .await;
    if !ran {
        return;
    }
    // Created with the bare personal team's name, then moved to "acme".
    let row = okf_mcp::store::catalogs::find(&server.db.pool, "scratch")
        .await
        .expect("the catalog");
    assert_eq!(row.owner_team.as_deref(), Some("acme"));
    assert!(
        !teams::find(&server.db.pool, OWNER)
            .await
            .expect("team")
            .catalogs
            .contains(&"scratch".to_string()),
        "the first owner was the personal team, by its bare name"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_older_list_read_that_answers_late_never_redraws_the_catalogs_table() {
    let server = server().await;
    let key = server
        .key("console-root", Access::Admin, &[], Some("main"))
        .await;
    drive(server.base.clone(), move |ui| {
        ui.login(&key);
        ui.open("/catalogs");
        ui.wait_visible("[data-catalog=main]");
        // The list read after creating "older" is sent at once but answers
        // late: the server's answer lists "older" but not "newer".
        ui.click("#catalog-create");
        ui.wait_visible("#catalog-create-form select[name=owner]");
        ui.fill("#catalog-create-form input[name=name]", "older");
        ui.select("#catalog-create-form select[name=owner]", OWNER);
        ui.hold_answer("GET", "/api/v1/catalogs", || {
            ui.click("#catalog-create-form button[type=submit]")
        });
        // A newer write's list read answers first and lists both.
        ui.click("#catalog-create");
        ui.wait_visible("#catalog-create-form select[name=owner]");
        ui.fill("#catalog-create-form input[name=name]", "newer");
        ui.select("#catalog-create-form select[name=owner]", OWNER);
        ui.click("#catalog-create-form button[type=submit]");
        ui.wait_visible("[data-catalog=newer]");
        // The older answer, released now, must not replace the newer table.
        ui.release_held();
        assert!(
            ui.present("[data-catalog=newer]") && ui.present("[data-catalog=older]"),
            "the older read redrew the catalogs table"
        );
    })
    .await;
}

/// M18 Task 10: the catalogs page draws the first page (100) and a Load
/// more control; the rows drawn keep their cursor while an older reload
/// answers late and a newer one fails, and Load more then appends the next
/// page once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_catalogs_page_loads_more_and_keeps_its_cursor_when_reloads_fail() {
    // `c000` to `c100` and `main`: 102 catalogs, so the first page ends at
    // `c099` and the second holds `c100` and `main`.
    let names: Vec<String> = (0..=100).map(|index| format!("c{index:03}")).collect();
    let mut catalogs: Vec<(&str, _)> = names
        .iter()
        .map(|name| (name.as_str(), files(&[])))
        .collect();
    catalogs.push(("main", files(&[("index.md", "# Main\n")])));
    let server = TestServer::start(catalogs, Config::default()).await;
    let key = server
        .key("console-root", Access::Admin, &[], Some("main"))
        .await;
    drive(server.base.clone(), move |ui| {
        let count = |selector: &str| {
            ui.eval(
                "(selector) => document.querySelectorAll(selector).length",
                json!(selector),
            )
        };
        let create = |name: &str| {
            ui.click("#catalog-create");
            ui.wait_visible("#catalog-create-form select[name=owner]");
            ui.fill("#catalog-create-form input[name=name]", name);
            ui.select("#catalog-create-form select[name=owner]", OWNER);
        };
        ui.login(&key);
        ui.open("/catalogs");
        ui.wait_visible("[data-catalog=c000]");
        ui.wait_visible("#catalogs-more");
        assert_eq!(count("span[data-catalog]"), json!(100), "one page drawn");
        assert!(!ui.present("[data-catalog=main]"));

        // The list read after creating `n-one` answers late; the one after
        // creating `n-two` fails. Neither may take the rows' cursor away.
        create("n-one");
        ui.hold_answer("GET", "/api/v1/catalogs", || {
            ui.click("#catalog-create-form button[type=submit]")
        });
        create("n-two");
        ui.fail_next("GET", "/api/v1/catalogs", 503, "the list is away");
        ui.click("#catalog-create-form button[type=submit]");
        ui.wait_visible("#catalogs-stale");
        ui.release_held();
        ui.wait_visible("#catalogs-more");
        assert_eq!(
            count("span[data-catalog]"),
            json!(100),
            "the rows drawn stay"
        );

        ui.click("#catalogs-more");
        ui.wait_visible("[data-catalog=n-two]");

        // c100, main, n-one and n-two, appended once.
        assert_eq!(count("span[data-catalog]"), json!(104));
        assert_eq!(count("[data-catalog=c099]"), json!(1));
        assert_eq!(count("[data-catalog=main]"), json!(1));
        ui.wait_hidden("#catalogs-more");
        // The first page still comes from the read before the failed one.
        ui.wait_visible("#catalogs-stale");
    })
    .await;
}

/// Issue #752: a catalog delete that takes a while (its DELETE is held)
/// shows a pending state, cannot be started twice, leaves the console usable,
/// and reports its outcome without touching a drawer opened meanwhile.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_catalog_delete_shows_pending_and_leaves_the_console_usable() {
    let server = server().await;
    let root = server.key("console-root", Access::Admin, &[], None).await;
    crate::ui_e2e::account::acme_with_owner_and_member(&server).await;
    let ran = drive(server.base.clone(), move |ui| {
        ui.login(&root);
        ui.open("/catalogs");
        ui.click("[data-catalog=acme-notes]");
        ui.wait_visible("#catalog-delete:not([disabled])");
        ui.hold_next("DELETE", "/catalogs/acme-notes");
        ui.click("#catalog-delete");
        ui.fill("#confirm-typed", "acme-notes");
        ui.click("#confirm-ok");
        // Pending: the controls are disabled and the drawer says what runs.
        ui.wait_visible("#catalog-delete[disabled]");
        ui.wait_visible("#catalog-force[disabled]");
        ui.wait_visible("#catalog-delete-note");
        ui.wait_text("#catalog-delete-note", "Deleting");
        // Not a success yet: nothing says it was deleted, and the catalog
        // is still listed.
        assert!(!ui.text("#toasts").contains("Deleted catalog"));
        // The same catalog's drawer, closed and opened again, is still
        // pending, and a second click sends nothing.
        ui.click("#drawer-close");
        ui.click("[data-catalog=acme-notes]");
        ui.wait_visible("#catalog-delete[disabled]");
        ui.wait_text("#catalog-delete-note", "Deleting");
        assert!(ui.click_twice("#catalog-delete"), "Delete is disabled");
        assert_eq!(ui.held_matches(), 1, "a second DELETE was sent");
        // Unrelated navigation and reads work meanwhile.
        ui.open("/teams");
        ui.wait_visible("[data-test=team-acme]");
        ui.click("#drawer-close");
        ui.open("/catalogs");
        ui.wait_visible("[data-catalog=main]");
        // A drawer of another catalog, open when the delete settles, stays.
        ui.click("[data-catalog=main]");
        ui.wait_visible("#catalog-display-name");
        ui.release_held();
        ui.wait_hidden("[data-catalog=acme-notes]");
        assert!(ui.text("#toasts").contains("Deleted catalog"));
        assert!(
            ui.present("#catalog-edit-form") && !ui.text("#drawer-title").contains("acme"),
            "another catalog's drawer was closed or replaced"
        );
        assert_eq!(switcher(ui), ["main"]);
    })
    .await;
    if !ran {
        return;
    }
    assert!(
        okf_mcp::store::catalogs::find(&server.db.pool, "acme-notes")
            .await
            .is_err()
    );
}

/// Issue #752: a refused delete (unpublished changes, no force) ends the
/// pending state, keeps the reason in the drawer and leaves Delete usable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_catalog_delete_keeps_its_reason_and_enables_delete_again() {
    let server = server().await;
    let root = server.key("console-root", Access::Admin, &[], None).await;
    let ran = drive(server.base.clone(), move |ui| {
        ui.login(&root);
        ui.open("/catalogs");
        ui.click("[data-catalog=main]");
        ui.wait_visible("#catalog-delete:not([disabled])");
        ui.fail_next("DELETE", "/catalogs/main", 409, "has unpublished changes");
        ui.click("#catalog-delete");
        ui.fill("#confirm-typed", "main");
        ui.click("#confirm-ok");
        ui.wait_text("#catalog-delete-note", "unpublished changes");
        ui.wait_visible("#catalog-delete:not([disabled])");
        ui.wait_visible("#catalog-force:not([disabled])");
        assert!(ui.present("[data-catalog=main]"));
    })
    .await;
    if !ran {
        return;
    }
    assert!(okf_mcp::store::catalogs::find(&server.db.pool, "main")
        .await
        .is_ok());
}

/// Issue #1030: an admin's Catalogs page names each catalog's store, with its
/// location as the tooltip and in the drawer, and filters by store; a key
/// below admin sees no store at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_admin_sees_which_store_each_catalog_is_on() {
    let server = server().await;
    let actor = okf_mcp::store::audit::AuditActor::cli();
    // A second store, in a database of its own, holding catalog `alpha`.
    let database = server.db.extra_store_database("acme").await;
    let options = server.db.pool.connect_options();
    let (host, port) = (options.get_host().to_string(), options.get_port());
    let acme = okf_mcp::stores::StoreName::parse("acme").expect("a store name");
    let row = okf_mcp::store::stores::StoreRow {
        name: acme.clone(),
        location: okf_mcp::store::stores::StoreLocation::Host {
            host: host.clone(),
            port,
            database: database.clone(),
            credential_kind: okf_mcp::store::stores::CredentialKind::Env,
            credential_ref: crate::support::pg::STORE_PASSWORD_VAR.to_string(),
        },
    };
    okf_mcp::store::stores::add(
        &server.db.pool,
        server.app.stores(),
        row,
        &actor,
        Utc::now(),
    )
    .await
    .expect("add the store");
    let team = NewTeam {
        name: "team-a",
        display_name: &okf_mcp::labels::DisplayName::default(),
        owner: OWNER,
    };
    teams::create(&server.db.pool, &team, &actor, Utc::now())
        .await
        .expect("create the team");
    teams::set_store(&server.db.pool, "team-a", Some(&acme), &actor, Utc::now())
        .await
        .expect("pin the team");
    let new = okf_mcp::store::catalogs::NewCatalog {
        name: "alpha",
        display_name: &okf_mcp::labels::DisplayName::default(),
        description: "",
        owner_team: "team-a",
    };
    crate::support::server::applied(&server.app, async {
        okf_mcp::store::catalogs::create_in(server.app.stores(), &new, &actor, Utc::now()).await
    })
    .await
    .expect("create the catalog on the store");
    let location = format!("{host}:{port}/{database}");
    let root = server.key("console-root", Access::Admin, &[], None).await;
    let reader = server
        .key("console-reader", Access::Read, &["main"], Some("main"))
        .await;

    drive(server.base.clone(), move |ui| {
        ui.login(&root);
        ui.open("/catalogs");
        ui.wait_visible("[data-test=catalog-store-alpha]");
        assert_eq!(ui.text("[data-test=catalog-store-main]"), "main");
        assert_eq!(
            ui.attribute("[data-test=catalog-store-main]", "title"),
            "account database"
        );
        assert_eq!(ui.text("[data-test=catalog-store-alpha]"), "acme");
        assert_eq!(
            ui.attribute("[data-test=catalog-store-alpha]", "title"),
            location
        );

        // The filter narrows the table to one store.
        ui.select("#catalogs-store-filter", "acme");
        ui.wait_hidden("[data-catalog=main]");
        assert!(ui.present("[data-catalog=alpha]"));
        ui.select("#catalogs-store-filter", "");
        ui.wait_visible("[data-catalog=main]");

        // The drawer shows the store and where it is.
        ui.click("[data-catalog=alpha]");
        ui.wait_text("#catalog-store-name", "acme");
        assert_eq!(ui.text("#catalog-store-location"), location);
        ui.click("#drawer-close");
        ui.click("#signout");

        // A key below admin is told no store.
        ui.login(&reader);
        ui.open("/catalogs");
        ui.wait_visible("[data-catalog=main]");
        assert!(!ui.present("#catalogs-store-filter"));
        assert!(!ui.present("[data-test=catalog-store-main]"));
    })
    .await;
}
