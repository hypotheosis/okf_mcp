//! The web console's static files under `/ui/` (web console design, "Front
//! end"), embedded at build time by `build.rs`, plus the security headers
//! and the page manifest the client router reads.

use axum::extract::Path;
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Json, Router};

// Pastes build.rs's generated `const ASSETS: &[(&str, &[u8])]` here, as if
// written in this file (like a C `#include`).
include!(concat!(env!("OUT_DIR"), "/ui_assets.rs"));

/// The console's Content-Security-Policy, shared by the sign-in pages and
/// the reader: scripts and styles only from this origin (Google Fonts'
/// stylesheet excepted), fonts from this origin (the reader's) or Google
/// Fonts (the console's), no framing.
pub(crate) const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; script-src 'self'; \
    style-src 'self' https://fonts.googleapis.com; font-src 'self' https://fonts.gstatic.com; \
    img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; \
    form-action 'self'";

/// The console's routes. They need no key: the files hold no catalog data.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "router builder: runs once at startup, outside any request, so a span would be an orphan root"
    )
)]
pub(crate) fn routes<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .route("/", get(|| async { Redirect::to("/ui/") }))
        .route("/ui", get(|| async { Redirect::permanent("/ui/") }))
        .route("/ui/", get(|| async { asset(ASSETS, "index.html") }))
        .route("/ui/manifest.json", get(manifest))
        .route(
            "/ui/{*path}",
            get(|Path(path): Path<String>| async move { asset(ASSETS, &path) }),
        )
        .layer(middleware::map_response(security_headers))
}

/// The pages this build embeds (`js/pages/<name>.js`), by name.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
    )
)]
fn page_names() -> Vec<&'static str> {
    ASSETS
        .iter()
        .filter_map(|(path, _)| path.strip_prefix("js/pages/")?.strip_suffix(".js"))
        .collect()
}

/// `GET /ui/manifest.json`: `{"pages": [...]}`.
#[tracing::instrument(level = "debug", skip_all)]
async fn manifest() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "pages": page_names() }))
}

/// The media type for an embedded file, by extension.
#[tracing::instrument(level = "trace", skip_all)]
pub(crate) fn content_type(path: &str) -> &'static str {
    match path.rsplit_once('.').map(|(_, extension)| extension) {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("woff2") => "font/woff2",
        Some("png") => "image/png",
        Some("webmanifest") => "application/manifest+json",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// One embedded file from `table`, revalidated on every load (`no-cache`),
/// or 404.
///
/// # Arguments
/// * `table` - the embedded files to look in ([`ASSETS`] or the reader's).
///   The bytes are `'static` (they live in the binary), which axum needs to
///   send them without copying.
/// * `path` - the file's path below its tree.
#[tracing::instrument(level = "trace", skip_all)]
pub(crate) fn asset(table: &[(&str, &'static [u8])], path: &str) -> Response {
    match table.iter().find(|(name, _)| *name == path) {
        Some((name, body)) => static_file(
            (
                [
                    (header::CONTENT_TYPE, content_type(name)),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                *body,
            )
                .into_response(),
        ),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Marks `response` as an embedded static file, which carries no
/// `X-Trace-Id` (see [`crate::http::StaticFile`]).
#[tracing::instrument(level = "trace", skip_all)]
pub(crate) fn static_file(mut response: Response) -> Response {
    response.extensions_mut().insert(crate::http::StaticFile);
    response
}

/// One console file's bytes, by its path below `src/http/ui`.
///
/// # Returns
/// `None` when the console embeds no such file.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
    )
)]
pub(crate) fn console_file(path: &str) -> Option<&'static [u8]> {
    ASSETS
        .iter()
        .find(|(name, _)| *name == path)
        .map(|(_, body)| *body)
}

/// Adds the CSP, `nosniff` and `no-referrer` to every console response.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn security_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::api::testing::app;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

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

    #[tokio::test]
    async fn the_shell_is_served_without_a_key_with_security_headers() {
        let response = get("/ui/").await;
        assert_eq!(response.status(), StatusCode::OK);
        let headers = response.headers();
        assert_eq!(headers["content-type"], "text/html; charset=utf-8");
        assert_eq!(headers["content-security-policy"], CONTENT_SECURITY_POLICY);
        assert_eq!(headers["x-content-type-options"], "nosniff");
        assert_eq!(headers["referrer-policy"], "no-referrer");
    }

    #[tokio::test]
    async fn assets_have_their_content_type_and_unknown_ones_are_404() {
        assert_eq!(
            get("/ui/js/main.js").await.headers()["content-type"],
            "text/javascript; charset=utf-8"
        );
        assert_eq!(
            get("/ui/styles.css").await.headers()["content-type"],
            "text/css; charset=utf-8"
        );
        assert_eq!(get("/ui/nope.js").await.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn root_and_bare_ui_redirect_to_the_shell() {
        let root = get("/").await;
        assert_eq!(root.status(), StatusCode::SEE_OTHER);
        assert_eq!(root.headers()["location"], "/ui/");
        let bare = get("/ui").await;
        assert_eq!(bare.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(bare.headers()["location"], "/ui/");
    }

    #[test]
    fn the_manifest_lists_embedded_pages_only() {
        let pages = page_names();
        assert!(pages
            .iter()
            .all(|name| !name.contains('/') && !name.ends_with(".js")));
        // Task 1 embeds no page; later tasks add theirs (keys, health, ...).
        assert_eq!(
            pages.len(),
            ASSETS
                .iter()
                .filter(|(p, _)| p.starts_with("js/pages/"))
                .count()
        );
    }

    #[test]
    fn content_types_follow_the_extension() {
        assert_eq!(content_type("a/b.css"), "text/css; charset=utf-8");
        assert_eq!(content_type("x.svg"), "image/svg+xml");
        assert_eq!(content_type("README"), "application/octet-stream");
        assert_eq!(content_type("icons/a.png"), "image/png");
        assert_eq!(
            content_type("manifest.webmanifest"),
            "application/manifest+json"
        );
        assert_eq!(content_type("fonts/OFL.txt"), "text/plain; charset=utf-8");
    }

    /// The reader's self-hosted fonts load under the shared policy, which
    /// still allows Google Fonts for the console.
    #[tokio::test]
    async fn the_policy_lets_pages_load_fonts_from_their_own_origin() {
        for path in ["/ui/", "/read/"] {
            let response = get(path).await;
            let policy = response.headers()["content-security-policy"]
                .to_str()
                .expect("an ASCII header");
            assert!(
                policy.contains("font-src 'self' https://fonts.gstatic.com;"),
                "{path}: {policy}"
            );
        }
    }
}
