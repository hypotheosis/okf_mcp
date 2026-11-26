//! The mobile reader's static files under `/read/` (reader design,
//! "Serving"), embedded at build time by `build.rs`, served with the
//! console's security headers. They hold no catalog data, so they need no
//! credential; the page reads `/api/v1` with the session cookie.
//!
//! Two files carry the product name (`index.html` and the manifest), so
//! they are rendered per request from the settings, and the shell version
//! and the precache listing follow the name: a rename reaches installed
//! copies on their next launch.

use std::collections::hash_map::DefaultHasher;
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, LazyLock};

use sha2::{Digest, Sha256};

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Json, Router};

use crate::app::App;
use crate::http::pages::escape;
use crate::http::ui::{asset, console_file, content_type, security_headers, static_file};

// Pastes build.rs's generated `READ_ASSETS` here, as if written in this
// file (like a C `#include`).
include!(concat!(env!("OUT_DIR"), "/read_assets.rs"));

/// The console files the reader's page loads, precached with its own.
pub(crate) const SHARED_CONSOLE_FILES: &[&str] = &[
    "/ui/vendor/pico.min.css",
    "/ui/js/core/dom.js",
    "/ui/js/core/format.js",
];

/// A hash of `files`' names and bytes, as 16 hex digits.
///
/// # Arguments
/// * `files` - `(name, bytes)` pairs, in a fixed order.
///
/// # Returns
/// The same digits for the same files on every build with this toolchain
/// (`DefaultHasher::new` is SipHash with fixed keys).
#[tracing::instrument(level = "trace", skip_all)]
fn shell_version<'a>(files: impl IntoIterator<Item = (&'a str, &'a [u8])>) -> String {
    let mut hasher = DefaultHasher::new();
    for (name, bytes) in files {
        name.hash(&mut hasher);
        bytes.hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

/// A shared console file's bytes, by its URL (`/ui/...`); empty when the
/// console does not embed it (a test catches that).
#[tracing::instrument(level = "trace", skip_all)]
fn shared_bytes(url: &str) -> &'static [u8] {
    url.strip_prefix("/ui/")
        .and_then(console_file)
        .unwrap_or_default()
}

/// The placeholder in `index.html` and the manifest that the product name
/// replaces, escaped for the file's own syntax.
const PRODUCT_PLACEHOLDER: &str = "__PRODUCT__";

/// A hash of every file the worker precaches, the reader's own (templates
/// as written) and the shared console files, so a change to any of them
/// changes the worker's bytes and installed apps update. Computed once on
/// first use (`LazyLock` is like a module-level value computed on first
/// access). The served version also covers the product name
/// ([`shell_version_for`]).
static BUILD_VERSION: LazyLock<String> = LazyLock::new(|| {
    shell_version(
        READ_ASSETS.iter().copied().chain(
            SHARED_CONSOLE_FILES
                .iter()
                .map(|url| (*url, shared_bytes(url))),
        ),
    )
});

/// One embedded reader file's bytes by name, as UTF-8 text (empty when the
/// build lacks it, which a test catches).
#[tracing::instrument(level = "trace", skip_all)]
fn template(name: &str) -> String {
    READ_ASSETS
        .iter()
        .find(|(file, _)| *file == name)
        .map(|(_, body)| String::from_utf8_lossy(body).into_owned())
        .unwrap_or_default()
}

/// The shell's version for `product`: the build's hash and the name, so
/// renaming the product changes it.
#[tracing::instrument(level = "trace", skip_all)]
fn shell_version_for(product: &str) -> String {
    shell_version([
        ("build", BUILD_VERSION.as_bytes()),
        ("product", product.as_bytes()),
    ])
}

/// `index.html` for `product`: the tab title, and the label under the home
/// screen icon on iOS.
#[tracing::instrument(level = "trace", skip_all)]
fn render_index(product: &str) -> String {
    template("index.html").replace(PRODUCT_PLACEHOLDER, &escape(product))
}

/// The manifest for `product`: its name is "<product> Reader", its short
/// name the product alone.
#[tracing::instrument(level = "trace", skip_all)]
fn render_manifest(product: &str) -> String {
    // A JSON string's content, without its two delimiting quotes: the
    // placeholder sits inside the template's quotes. Exactly one quote is
    // removed from each end (`trim_matches` would also eat an escaped quote
    // that ends the name, leaving a dangling backslash).
    let quoted = serde_json::Value::String(product.to_string()).to_string();
    let content = quoted
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .expect("a JSON string is wrapped in quotes");
    template("manifest.webmanifest").replace(PRODUCT_PLACEHOLDER, content)
}

/// The worker's source with `version` filled in.
#[tracing::instrument(level = "trace", skip_all)]
fn render_worker(version: &str) -> String {
    template("sw.js").replace("__SHELL_VERSION__", version)
}

/// The reader's routes.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "router builder: runs once at startup, outside any request, so a span would be an orphan root"
    )
)]
pub(crate) fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/read", get(|| async { Redirect::permanent("/read/") }))
        .route("/read/", get(index))
        .route("/read/shell.json", get(shell))
        .route("/read/sw.js", get(worker))
        .route("/read/{*path}", get(file))
        .layer(middleware::map_response(security_headers))
}

/// The product name now (the settings reload it, so a rename shows
/// without a restart).
#[tracing::instrument(level = "trace", skip_all)]
fn product_name(app: &App) -> String {
    app.settings().brand().product_name
}

/// `body` served with the media type of the file `name`, revalidated on every
/// load.
#[tracing::instrument(level = "trace", skip_all)]
fn rendered(name: &str, body: String) -> Response {
    static_file(
        (
            [
                (header::CONTENT_TYPE, content_type(name)),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            body,
        )
            .into_response(),
    )
}

/// `GET /read/`: the page, named for the product.
#[tracing::instrument(level = "debug", skip_all)]
async fn index(State(app): State<Arc<App>>) -> Response {
    rendered("index.html", render_index(&product_name(&app)))
}

/// `GET /read/{*path}`: an embedded file; the two templated ones are
/// rendered for the product.
#[tracing::instrument(level = "debug", skip_all)]
async fn file(State(app): State<Arc<App>>, Path(path): Path<String>) -> Response {
    match path.as_str() {
        "index.html" => rendered(&path, render_index(&product_name(&app))),
        "manifest.webmanifest" => rendered(&path, render_manifest(&product_name(&app))),
        _ => asset(READ_ASSETS, &path),
    }
}

/// `bytes`' SHA-256 as 64 lowercase hex digits, as the worker computes it
/// with `crypto.subtle.digest`.
#[tracing::instrument(level = "trace", skip_all)]
fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            // `write!` into a String cannot fail; `let _` drops the Ok.
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// The worker's precache listing for `product`: the shell version and each
/// file's URL with the SHA-256 of the bytes this build serves for it. The
/// worker installs only when the listing's version is its own and every file
/// it fetched hashes as listed, so a deploy or a rename between its requests
/// (or a mix of replicas on two builds) fails the install instead of caching
/// a mix. `index.html` is listed as `/read/`, the URL the page is opened at.
#[tracing::instrument(level = "trace", skip_all)]
fn shell_listing(product: &str) -> serde_json::Value {
    let version = shell_version_for(product);
    let reader = READ_ASSETS.iter().map(|(name, body)| {
        let url = match *name {
            "index.html" => "/read/".to_string(),
            other => format!("/read/{other}"),
        };
        // The bytes as served: the templated files and the worker rendered.
        let rendered_bytes = match *name {
            "index.html" => Some(render_index(product)),
            "manifest.webmanifest" => Some(render_manifest(product)),
            "sw.js" => Some(render_worker(&version)),
            _ => None,
        };
        let sha256 =
            rendered_bytes.map_or_else(|| sha256_hex(body), |text| sha256_hex(text.as_bytes()));
        serde_json::json!({ "url": url, "sha256": sha256 })
    });
    let shared = SHARED_CONSOLE_FILES
        .iter()
        .map(|url| serde_json::json!({ "url": url, "sha256": sha256_hex(shared_bytes(url)) }));
    serde_json::json!({ "version": version, "files": reader.chain(shared).collect::<Vec<_>>() })
}

/// `GET /read/shell.json`: `{"version": ..., "files": [{"url", "sha256"}]}`,
/// what the worker precaches and how to check it.
#[tracing::instrument(level = "debug", skip_all)]
async fn shell(State(app): State<Arc<App>>) -> Json<serde_json::Value> {
    Json(shell_listing(&product_name(&app)))
}

/// `GET /read/sw.js`: the worker, with its version, revalidated on every
/// load so a deploy or a rename reaches installed apps on their next launch.
#[tracing::instrument(level = "debug", skip_all)]
async fn worker(State(app): State<Arc<App>>) -> Response {
    let source = render_worker(&shell_version_for(&product_name(&app)));
    if source.is_empty() {
        return StatusCode::NOT_FOUND.into_response();
    }
    static_file(
        (
            [
                (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            source,
        )
            .into_response(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::api::testing::app;
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    /// One GET through the whole router, as a browser would send it.
    async fn get(path: &str) -> Response {
        let test = app().await;
        crate::http::router(
            std::sync::Arc::clone(&test.app),
            &tokio_util::sync::CancellationToken::new(),
        )
        .oneshot(
            Request::builder()
                .uri(path)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response")
    }

    /// The body as text.
    async fn text(response: Response) -> String {
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        String::from_utf8(bytes.to_vec()).expect("utf-8")
    }

    #[tokio::test]
    async fn the_reader_page_is_served_with_the_console_security_headers() {
        let response = get("/read/").await;
        assert_eq!(response.status(), StatusCode::OK);
        let headers = response.headers();
        assert_eq!(headers["content-type"], "text/html; charset=utf-8");
        assert_eq!(
            headers["content-security-policy"],
            crate::http::ui::CONTENT_SECURITY_POLICY
        );
        assert_eq!(headers["x-content-type-options"], "nosniff");
        assert_eq!(headers["referrer-policy"], "no-referrer");
        assert!(text(response).await.contains("/read/manifest.webmanifest"));
    }

    #[tokio::test]
    async fn bare_read_redirects_to_the_reader() {
        let response = get("/read").await;
        assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(response.headers()["location"], "/read/");
    }

    #[tokio::test]
    async fn the_manifest_and_icons_have_their_content_types() {
        assert_eq!(
            get("/read/manifest.webmanifest").await.headers()["content-type"],
            "application/manifest+json"
        );
        assert_eq!(
            get("/read/icons/icon-192.png").await.headers()["content-type"],
            "image/png"
        );
    }

    #[tokio::test]
    async fn the_shell_precaches_the_fonts_the_sprite_and_their_licenses() {
        let response = get("/read/shell.json").await;
        let listing: serde_json::Value = serde_json::from_str(&text(response).await).expect("json");
        let files: Vec<&str> = listing["files"]
            .as_array()
            .expect("files")
            .iter()
            .filter_map(|entry| entry["url"].as_str())
            .collect();
        for url in [
            "/read/icons.svg",
            "/read/fonts/inter-latin.woff2",
            "/read/fonts/literata-latin.woff2",
            "/read/fonts/literata-italic-latin.woff2",
            "/read/fonts/OFL-Inter.txt",
            "/read/fonts/OFL-Literata.txt",
        ] {
            assert!(files.contains(&url), "{url} in {files:?}");
        }
        assert_eq!(
            get("/read/fonts/inter-latin.woff2").await.headers()["content-type"],
            "font/woff2"
        );
        assert_eq!(
            get("/read/icons.svg").await.headers()["content-type"],
            "image/svg+xml"
        );
        assert_eq!(
            get("/read/fonts/OFL-Inter.txt").await.headers()["content-type"],
            "text/plain; charset=utf-8"
        );
    }

    #[tokio::test]
    async fn the_system_bars_follow_the_theme() {
        let page = text(get("/read/").await).await;
        assert!(page.contains(
            r##"<meta name="theme-color" content="#ffffff" media="(prefers-color-scheme: light)">"##
        ));
        assert!(page.contains(
            r##"<meta name="theme-color" content="#191919" media="(prefers-color-scheme: dark)">"##
        ));
        let manifest: serde_json::Value =
            serde_json::from_str(&text(get("/read/manifest.webmanifest").await).await)
                .expect("json");
        // Neutral, so no colour of the old accent shows in a system bar.
        assert_eq!(manifest["theme_color"], "#ffffff");
        assert_eq!(manifest["background_color"], "#ffffff");
    }

    #[tokio::test]
    async fn the_reader_and_the_console_never_serve_each_others_files() {
        assert_eq!(
            get("/read/styles.css").await.status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            get("/ui/manifest.webmanifest").await.status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(get("/read/nope.js").await.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_shell_listing_names_every_reader_file_and_the_shared_ones() {
        let response = get("/read/shell.json").await;
        assert_eq!(response.status(), StatusCode::OK);
        let listing: serde_json::Value = serde_json::from_str(&text(response).await).expect("json");
        assert_eq!(
            listing["version"],
            shell_version_for(crate::store::settings::DEFAULT_PRODUCT_NAME)
        );
        let entries = listing["files"].as_array().expect("files");
        let files: Vec<&str> = entries
            .iter()
            .filter_map(|entry| entry["url"].as_str())
            .collect();
        assert_eq!(files.len(), entries.len(), "every entry has a url");
        assert!(files.contains(&"/read/"));
        assert!(!files.contains(&"/read/index.html"));
        assert!(files.contains(&"/read/manifest.webmanifest"));
        for shared in SHARED_CONSOLE_FILES {
            assert!(files.contains(shared), "{shared}");
        }
        // Each listed file is served, with exactly the bytes its hash
        // names, so a worker that verifies them installs.
        for entry in entries {
            let url = entry["url"].as_str().expect("url");
            let response = get(url).await;
            assert_eq!(response.status(), StatusCode::OK, "{url}");
            let bytes = to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("body");
            assert_eq!(entry["sha256"], sha256_hex(&bytes), "{url}");
        }
    }

    #[tokio::test]
    async fn the_worker_is_revalidated_and_carries_the_shell_version() {
        let response = get("/read/sw.js").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["cache-control"], "no-cache");
        assert_eq!(
            response.headers()["content-type"],
            "text/javascript; charset=utf-8"
        );
        let source = text(response).await;
        assert!(source.contains(&shell_version_for(
            crate::store::settings::DEFAULT_PRODUCT_NAME
        )));
        assert!(!source.contains("__SHELL_VERSION__"));
    }

    #[tokio::test]
    async fn the_console_does_not_serve_the_reader_worker() {
        assert_eq!(get("/ui/sw.js").await.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn the_manifest_is_named_after_the_product() {
        let manifest: serde_json::Value =
            serde_json::from_str(&render_manifest("Acme Notes")).expect("json");
        assert_eq!(manifest["name"], "Acme Notes Reader");
        assert_eq!(manifest["short_name"], "Acme Notes");
        assert_eq!(manifest["start_url"], "/read/");
        let default: serde_json::Value = serde_json::from_str(&render_manifest(
            crate::store::settings::DEFAULT_PRODUCT_NAME,
        ))
        .expect("json");
        assert_eq!(default["name"], "okf Reader");
        assert_eq!(default["short_name"], "okf");
    }

    #[test]
    fn the_manifest_escapes_the_name_for_json() {
        let product = "Acme \"Notes\" \\ <b>";
        let manifest: serde_json::Value =
            serde_json::from_str(&render_manifest(product)).expect("still valid json");
        assert_eq!(manifest["name"], format!("{product} Reader"));
        assert_eq!(manifest["short_name"], product);
    }

    #[test]
    fn a_name_that_starts_or_ends_with_a_quote_round_trips_in_the_manifest() {
        for product in ["Acme \"Notes\"", "\"Acme\"", "Acme\\", "\\\"", "\""] {
            let manifest: serde_json::Value = serde_json::from_str(&render_manifest(product))
                .unwrap_or_else(|error| panic!("{product:?}: {error}"));
            assert_eq!(manifest["name"], format!("{product} Reader"), "{product:?}");
            assert_eq!(manifest["short_name"], product, "{product:?}");
        }
    }

    #[test]
    fn quotes_angle_brackets_and_ampersands_stay_correct_in_the_page() {
        let page = render_index(r#"Q "x" <b> & 'y'"#);
        let escaped = "Q &quot;x&quot; &lt;b&gt; &amp; &#39;y&#39;";
        assert!(page.contains(&format!("<title>{escaped} Reader</title>")));
        assert!(page.contains(&format!(
            r#"name="apple-mobile-web-app-title" content="{escaped}""#
        )));
    }

    #[test]
    fn the_page_is_named_after_the_product_and_escapes_it() {
        let page = render_index("Acme Notes");
        assert!(page.contains("<title>Acme Notes Reader</title>"));
        assert!(page.contains(r#"name="apple-mobile-web-app-title" content="Acme Notes""#));
        assert!(!page.contains(PRODUCT_PLACEHOLDER));
        let hostile = render_index(r#""><script>alert(1)</script>"#);
        assert!(!hostile.contains("<script>alert"));
        assert!(hostile.contains("&quot;&gt;&lt;script&gt;alert(1)&lt;/script&gt;"));
        // The page keeps its manifest link and no inline script.
        assert!(page.contains(r#"<link rel="manifest" href="/read/manifest.webmanifest">"#));
        assert!(!page.contains("<script>"));
    }

    #[test]
    fn a_rename_changes_the_shell_version_and_the_listing() {
        assert_ne!(shell_version_for("Acme Notes"), shell_version_for("Other"));
        assert_eq!(
            shell_version_for("Acme Notes"),
            shell_version_for("Acme Notes")
        );
        let before = shell_listing("Acme Notes");
        let after = shell_listing("Other");
        assert_ne!(before["version"], after["version"]);
        let hash = |listing: &serde_json::Value, url: &str| {
            listing["files"]
                .as_array()
                .expect("files")
                .iter()
                .find(|entry| entry["url"] == url)
                .map(|entry| entry["sha256"].clone())
        };
        for url in ["/read/", "/read/manifest.webmanifest", "/read/sw.js"] {
            assert_ne!(hash(&before, url), hash(&after, url), "{url}");
        }
        // A file that does not carry the name keeps its hash.
        assert_eq!(
            hash(&before, "/read/icons.svg"),
            hash(&after, "/read/icons.svg")
        );
    }

    #[tokio::test]
    async fn the_served_files_follow_a_rename_made_at_run_time() {
        use crate::store::audit::AuditActor;
        use crate::store::settings::{self, SettingKey};
        // A short `credential_ttl` makes each request recheck the settings.
        let test = crate::mcp::testing::app_with(
            &[],
            crate::config::Config {
                credential_ttl: std::time::Duration::from_millis(100),
                ..crate::config::Config::default()
            },
        )
        .await;
        let served = || async {
            let router = crate::http::router(
                std::sync::Arc::clone(&test.app),
                &tokio_util::sync::CancellationToken::new(),
            );
            let mut bodies = Vec::new();
            for path in ["/read/", "/read/manifest.webmanifest", "/read/shell.json"] {
                let response = router
                    .clone()
                    .oneshot(
                        Request::builder()
                            .uri(path)
                            .body(Body::empty())
                            .expect("request"),
                    )
                    .await
                    .expect("response");
                bodies.push(text(response).await);
            }
            bodies
        };
        let before = served().await;
        assert!(before[1].contains("okf Reader"));
        settings::set(
            test.app.accounts().executor(),
            SettingKey::ProductName,
            "Acme Notes",
            &AuditActor::cli(),
            chrono::Utc::now(),
        )
        .await
        .expect("rename");
        // The settings reload on their own clock; wait for the new name.
        let mut after = served().await;
        for _ in 0..50 {
            if after[1].contains("Acme Notes Reader") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            after = served().await;
        }
        assert!(after[0].contains("<title>Acme Notes Reader</title>"));
        assert!(after[1].contains("Acme Notes Reader"));
        assert_ne!(before[2], after[2], "the listing's version moved");
    }

    #[test]
    fn the_shell_version_changes_when_only_a_shared_file_does() {
        let old: [(&str, &[u8]); 2] = [("index.html", b"page"), ("/ui/js/core/dom.js", b"old")];
        let new: [(&str, &[u8]); 2] = [("index.html", b"page"), ("/ui/js/core/dom.js", b"new")];
        assert_ne!(shell_version(old), shell_version(new));
        assert_eq!(shell_version(old), shell_version(old));
    }

    #[test]
    fn the_served_version_covers_the_shared_console_files() {
        let reader_only = shell_version(READ_ASSETS.iter().copied());
        assert_ne!(*BUILD_VERSION, reader_only);
        for url in SHARED_CONSOLE_FILES {
            assert!(!shared_bytes(url).is_empty(), "{url} is embedded");
        }
    }

    /// Every icon the reader's page and scripts name by a literal
    /// (`icons.svg#i-NAME` in markup, `icon('NAME'` in a script) is a
    /// symbol of the sprite, so no icon draws empty.
    #[test]
    fn every_icon_the_reader_names_is_in_its_sprite() {
        let sprite = READ_ASSETS
            .iter()
            .find(|(name, _)| *name == "icons.svg")
            .map(|(_, body)| String::from_utf8_lossy(body).into_owned())
            .expect("icons.svg is embedded");
        let named =
            regex::Regex::new(r"icons\.svg#i-([a-z-]+)|icon\('([a-z-]+)'").expect("valid regex");
        let mut seen = 0;
        for (name, body) in READ_ASSETS {
            if !(name.ends_with(".js") || name.ends_with(".html")) {
                continue;
            }
            let text = String::from_utf8_lossy(body);
            for found in named.captures_iter(&text) {
                let icon = found
                    .get(1)
                    .or_else(|| found.get(2))
                    .map(|group| group.as_str())
                    .expect("one group matched");
                seen += 1;
                assert!(
                    sprite.contains(&format!("id=\"i-{icon}\"")),
                    "{name} names i-{icon}, which icons.svg lacks"
                );
            }
        }
        assert!(seen > 0, "the reader names no icon");
    }
}
