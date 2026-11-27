//! Test helpers for the OAuth routes (`#[cfg(test)]`): an app with OAuth on,
//! a registered DCR client, a live sign-in code, and a request helper over
//! the whole router. Tasks 5 and 6 share them.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{header, HeaderMap, Request, StatusCode};
use chrono::{TimeDelta, Utc};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use crate::app::App;
use crate::config::Config;
use crate::db::redis_support::TestRedis;
use crate::http::api::testing::{Browser, MAIN};
use crate::http::console_session::COOKIE;
use crate::http::router;
use crate::mcp::testing::{app_with, app_with_metrics, replica_pair_on, TestApp};
use crate::store::keys::{Access, Grants};
use crate::store::oauth::clients::ClientKind;
use crate::store::oauth::codes::{self as code_rows, insert_test_client, NewSignInCode};
use crate::store::oauth::testing::oauth_config;

/// The public URL the test apps run under.
pub(crate) const PUBLIC_URL: &str = "https://okf.example.com";
/// The resource every valid request names.
pub(crate) const RESOURCE: &str = "https://okf.example.com/mcp";
/// The test DCR client's id.
pub(crate) const CLIENT_ID: &str = "test-client";
/// The test client's one redirect URI.
pub(crate) const REDIRECT_URI: &str = "https://client.example.com/callback";
/// A second DCR client, registered by [`insert_loopback_client`].
pub(crate) const LOOPBACK_CLIENT_ID: &str = "loopback-client";
/// Its one redirect URI: loopback `http` with no port, as Claude Code
/// registers it.
pub(crate) const LOOPBACK_REDIRECT_URI: &str = "http://localhost/callback";
/// [`LOOPBACK_REDIRECT_URI`] on the ephemeral port a native client sends.
pub(crate) const LOOPBACK_ON_PORT: &str = "http://localhost:54123/callback";
/// A valid S256 challenge (RFC 7636 appendix B, a published vector).
pub(crate) const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
/// The verifier of [`CHALLENGE`].
pub(crate) const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";

/// An app holding `main` and `other` with OAuth on at [`PUBLIC_URL`], and
/// the DCR client [`CLIENT_ID`] (named "Example Client") registered.
///
/// # Returns
/// The app and its database.
///
/// # Panics
/// If the database or the insert fails: a broken test setup.
pub(crate) async fn oauth_app() -> TestApp {
    oauth_app_with(oauth_config(PUBLIC_URL)).await
}

/// [`oauth_app`] on `config` instead of [`oauth_config`]'s defaults, for a
/// test that needs another OAuth setting (no refresh reuse grace window).
///
/// # Arguments
/// * `config` - the settings; start from `oauth_config(PUBLIC_URL)`.
///
/// # Returns
/// The app and its database.
///
/// # Panics
/// As [`oauth_app`].
pub(crate) async fn oauth_app_with(config: Config) -> TestApp {
    let test = app_with(
        &[("main", MAIN), ("other", &[("index.md", "# Other\n")])],
        config,
    )
    .await;
    // Corrected (issue #247): `insert_dcr` lets Postgres pick a UUID, so the
    // fixed test id goes in through the store's plain-SQL helper.
    insert_test_client(
        test.app.accounts().executor(),
        CLIENT_ID,
        ClientKind::Dcr,
        REDIRECT_URI,
    )
    .await;
    test
}

/// [`oauth_app`] recording on a local meter provider, and the registry it
/// renders into.
///
/// # Panics
/// As [`oauth_app`].
pub(crate) async fn oauth_app_with_metrics() -> (TestApp, prometheus::Registry) {
    oauth_app_with_metrics_config(oauth_config(PUBLIC_URL)).await
}

/// [`oauth_app_with_metrics`] on `config`.
///
/// # Arguments
/// * `config` - the configuration (OAuth on).
///
/// # Panics
/// If the app or the client cannot be set up: a broken test setup.
pub(crate) async fn oauth_app_with_metrics_config(
    config: crate::config::Config,
) -> (TestApp, prometheus::Registry) {
    let (test, registry) = app_with_metrics(
        &[("main", MAIN), ("other", &[("index.md", "# Other\n")])],
        config,
    )
    .await;
    insert_test_client(
        test.app.accounts().executor(),
        CLIENT_ID,
        ClientKind::Dcr,
        REDIRECT_URI,
    )
    .await;
    (test, registry)
}

/// Registers the DCR client [`LOOPBACK_CLIENT_ID`] with the portless
/// [`LOOPBACK_REDIRECT_URI`].
///
/// # Arguments
/// * `test` - the app ([`oauth_app`]).
///
/// # Panics
/// If the insert fails: a broken test setup.
pub(crate) async fn insert_loopback_client(test: &TestApp) {
    insert_test_client(
        test.app.accounts().executor(),
        LOOPBACK_CLIENT_ID,
        ClientKind::Dcr,
        LOOPBACK_REDIRECT_URI,
    )
    .await;
}

/// A live sign-in code: `access` on `main`, actor `example-actor`.
///
/// # Arguments
/// * `test` - the app.
/// * `access` - `Read` or `Write`.
///
/// # Returns
/// The `okfsc_` code.
///
/// # Panics
/// If the store refuses the code.
pub(crate) async fn sign_in_code(test: &TestApp, access: Access) -> String {
    let catalogs = ["main".to_string()];
    let new = NewSignInCode {
        name: "example",
        access,
        grants: Grants::Catalogs(&catalogs),
        default_catalog: Some("main"),
        actor: "example-actor",
        expires_at: Utc::now() + TimeDelta::hours(24),
        user: crate::db::TEST_USER,
    };
    code_rows::create(
        test.app.accounts().executor(),
        &new,
        &crate::store::audit::AuditActor::cli(),
        Utc::now(),
    )
    .await
    .expect("create a sign-in code")
    .code
}

/// Runs authorize and consent on the sign-in code path (code, then Approve
/// from the summary), as the DCR client [`CLIENT_ID`].
///
/// # Arguments
/// * `test` - the app ([`oauth_app`] or [`oauth_replica_pair`]'s first).
///
/// # Returns
/// The `okfac_` code.
///
/// # Panics
/// If a step does not answer as expected.
pub(crate) async fn authorization_code(test: &TestApp) -> String {
    authorization_code_for(test, &authorize_query()).await
}

/// [`authorization_code`] for the authorize request `query`.
///
/// # Arguments
/// * `test` - the app.
/// * `query` - the authorize request's query string, without the `?`.
///
/// # Returns
/// The `okfac_` code.
///
/// # Panics
/// If a step does not answer as expected.
pub(crate) async fn authorization_code_for(test: &TestApp, query: &str) -> String {
    let (status, headers, page) = approve(test, query).await;
    forwarded(status, &headers, &page)
        .query_pairs()
        .find(|(key, _)| key == "code")
        .map(|(_, value)| value.into_owned())
        .expect("a code")
}

/// Runs authorize and consent on the sign-in code path up to Approve, and
/// returns Approve's answer unread (a forwarding page with the code, or an
/// error page).
///
/// # Arguments
/// * `test` - the app.
/// * `query` - the authorize request's query string, without the `?`.
///
/// # Returns
/// Approve's status, headers and body.
///
/// # Panics
/// If a step before Approve does not answer as expected.
pub(crate) async fn approve(test: &TestApp, query: &str) -> (StatusCode, HeaderMap, String) {
    let approval = approval_form(test, query).await;
    send(test, "POST", "/oauth/authorize", Some(approval)).await
}

/// Runs authorize and the sign-in code step, and returns the summary
/// page's Approve form, unsent (to post it later, or on another replica).
///
/// # Arguments
/// * `test` - the app.
/// * `query` - the authorize request's query string, without the `?`.
///
/// # Returns
/// The form body.
///
/// # Panics
/// If a step does not answer as expected.
pub(crate) async fn approval_form(test: &TestApp, query: &str) -> String {
    let code = sign_in_code(test, Access::Read).await;
    let (_, _, page) = send(test, "GET", &format!("/oauth/authorize?{query}"), None).await;
    let (pending, csrf) = (hidden(&page, "pending"), hidden(&page, "csrf"));
    let (_, _, summary) = send(
        test,
        "POST",
        "/oauth/authorize",
        Some(form(&[
            ("pending", &pending),
            ("csrf", &csrf),
            ("action", "code"),
            ("sign_in_code", &code),
        ])),
    )
    .await;
    form(&[
        ("pending", &pending),
        ("csrf", &csrf),
        ("summary", &hidden(&summary, "summary")),
        ("action", "approve"),
    ])
}

/// [`oauth_app`] as two replicas over one database and one Redis
/// (`mcp::testing::replica_pair`; no change-feed listener in a lib test). The client is
/// registered after both loaded, which needs no key cache.
///
/// # Returns
/// The first replica (it owns the database), the second, and their Redis.
///
/// # Panics
/// If the database, Redis or the insert fails: a broken test setup.
pub(crate) async fn oauth_replica_pair() -> (TestApp, Arc<App>, TestRedis) {
    oauth_replica_pair_on(TestRedis::start().await).await
}

/// [`oauth_replica_pair`] over a Redis the test already started (a
/// cluster, for one).
///
/// # Arguments
/// * `redis` - the server both replicas connect to.
///
/// # Returns
/// As [`oauth_replica_pair`].
///
/// # Panics
/// As [`oauth_replica_pair`].
pub(crate) async fn oauth_replica_pair_on(redis: TestRedis) -> (TestApp, Arc<App>, TestRedis) {
    let (first, second, redis) = replica_pair_on(
        &[("main", MAIN), ("other", &[("index.md", "# Other\n")])],
        oauth_config(PUBLIC_URL),
        redis,
    )
    .await;
    insert_test_client(
        first.app.accounts().executor(),
        CLIENT_ID,
        ClientKind::Dcr,
        REDIRECT_URI,
    )
    .await;
    (first, second, redis)
}

/// Exchanges `code` (from [`authorization_code`]) on `test` for tokens.
///
/// # Arguments
/// * `test` - the app that issued the code.
/// * `code` - the `okfac_` code.
///
/// # Returns
/// The access token and the refresh token.
///
/// # Panics
/// If the exchange does not answer 200 with both tokens.
pub(crate) async fn exchange_for_tokens(test: &TestApp, code: &str) -> (String, String) {
    let body = form(&[
        ("grant_type", "authorization_code"),
        ("code", code),
        ("client_id", CLIENT_ID),
        ("redirect_uri", REDIRECT_URI),
        ("code_verifier", VERIFIER),
        ("resource", RESOURCE),
    ]);
    let (status, _, text) = send(test, "POST", "/oauth/token", Some(body)).await;
    assert_eq!(status, StatusCode::OK, "the exchange answers 200: {text}");
    let tokens: serde_json::Value = serde_json::from_str(&text).expect("a JSON token response");
    let field = |name: &str| {
        tokens[name]
            .as_str()
            .unwrap_or_else(|| panic!("the response has {name}"))
            .to_string()
    };
    (field("access_token"), field("refresh_token"))
}

/// A valid authorization request's query string on the sign-in code path
/// (`signin=code`; the client's `state` is `xyz`).
///
/// # Returns
/// The encoded query, without the `?`.
pub(crate) fn authorize_query() -> String {
    authorize_query_for(CLIENT_ID, REDIRECT_URI)
}

/// [`authorize_query`] for another client or redirect URI.
///
/// # Arguments
/// * `client_id` - the request's `client_id`.
/// * `redirect_uri` - the request's `redirect_uri`.
///
/// # Returns
/// The encoded query, without the `?`.
pub(crate) fn authorize_query_for(client_id: &str, redirect_uri: &str) -> String {
    format!(
        "{}&signin=code",
        user_authorize_query_for(client_id, redirect_uri)
    )
}

/// A valid authorization request's query string for a signed-in person:
/// [`authorize_query`] without `signin=code`. Its pairs are in the order
/// the server rebuilds a request in, so it is also `/signin`'s `return_to`.
///
/// # Returns
/// The encoded query, without the `?`.
pub(crate) fn user_authorize_query() -> String {
    user_authorize_query_for(CLIENT_ID, REDIRECT_URI)
}

/// [`user_authorize_query`] for another client or redirect URI.
///
/// # Arguments
/// * `client_id` - the request's `client_id`.
/// * `redirect_uri` - the request's `redirect_uri`.
///
/// # Returns
/// The encoded query, without the `?`.
fn user_authorize_query_for(client_id: &str, redirect_uri: &str) -> String {
    form(&[
        ("response_type", "code"),
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
        ("state", "xyz"),
        ("code_challenge", CHALLENGE),
        ("code_challenge_method", "S256"),
        ("resource", RESOURCE),
    ])
}

/// `application/x-www-form-urlencoded` text of `pairs`.
///
/// # Arguments
/// * `pairs` - `(name, value)` pairs, in order.
///
/// # Returns
/// The encoded text.
pub(crate) fn form(pairs: &[(&str, &str)]) -> String {
    form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish()
}

/// Sends one request through the whole router.
///
/// # Arguments
/// * `test` - the app.
/// * `method` - `GET` or `POST`.
/// * `uri` - the path and query.
/// * `body` - a form body (sent as `application/x-www-form-urlencoded`), if any.
///
/// # Returns
/// The status, the headers and the body as text.
///
/// # Panics
/// If the request cannot be built or the body cannot be read.
pub(crate) async fn send(
    test: &TestApp,
    method: &str,
    uri: &str,
    body: Option<String>,
) -> (StatusCode, HeaderMap, String) {
    send_as(test, method, uri, body, None).await
}

/// As [`send`], with a console session's cookie when `session` is given.
///
/// # Arguments
/// * `test` - the app.
/// * `method` - `GET` or `POST`.
/// * `uri` - the path and query.
/// * `body` - a form body, if any.
/// * `session` - a signed-in browser (Task 7a's `Browser`) whose session
///   cookie to send, if any.
///
/// # Returns
/// The status, the headers and the body as text.
///
/// # Panics
/// If the request cannot be built or the body cannot be read.
pub(crate) async fn send_as(
    test: &TestApp,
    method: &str,
    uri: &str,
    body: Option<String>,
    session: Option<&Browser>,
) -> (StatusCode, HeaderMap, String) {
    send_on(&test.app, method, uri, body, session).await
}

/// As [`send_as`], to any app (the second of [`oauth_replica_pair`], for
/// one).
///
/// # Arguments
/// * `app` - the app whose router answers.
/// * `method` - `GET` or `POST`.
/// * `uri` - the path and query.
/// * `body` - a form body, if any.
/// * `session` - a signed-in browser whose session cookie to send, if any.
///
/// # Returns
/// The status, the headers and the body as text.
///
/// # Panics
/// If the request cannot be built or the body cannot be read.
pub(crate) async fn send_on(
    app: &Arc<App>,
    method: &str,
    uri: &str,
    body: Option<String>,
    session: Option<&Browser>,
) -> (StatusCode, HeaderMap, String) {
    let mut request = Request::builder().method(method).uri(uri);
    if body.is_some() {
        request = request.header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    }
    // Only the cookie: these are top-level navigations and form posts,
    // which carry no CSRF header (the consent form has its own token).
    if let Some(id) = session.and_then(|browser| browser.cookie(COOKIE)) {
        request = request.header(header::COOKIE, format!("{COOKIE}={id}"));
    }
    let request = request
        .body(body.map_or_else(Body::empty, Body::from))
        .expect("request");
    let response = router(Arc::clone(app), &CancellationToken::new())
        .oneshot(request)
        .await
        .expect("response");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.expect("body");
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

/// Where a consent POST's answer forwards the browser
/// ([`crate::oauth::consent::forward_page`]): the Continue link's target,
/// unescaped.
///
/// # Arguments
/// * `status` - the answer's status.
/// * `headers` - its headers.
/// * `page` - its HTML.
///
/// # Returns
/// The client URL, answer included.
///
/// # Panics
/// If the answer is not a 200 forwarding page.
pub(crate) fn forwarded(status: StatusCode, headers: &HeaderMap, page: &str) -> url::Url {
    assert_eq!(status, StatusCode::OK, "a forwarding page: {page}");
    assert!(headers.get(header::LOCATION).is_none(), "not a redirect");
    let marker = "id=\"consent-forward\" href=\"";
    let start = page.find(marker).expect("the Continue link") + marker.len();
    let end = start + page[start..].find('"').expect("closing quote");
    // `escape` wrote `&amp;` between the query's pairs. The test redirect
    // URIs hold no `"`, `'`, `<` or `>`, and the query is percent-encoded,
    // so `&amp;` is the only entity to undo.
    url::Url::parse(&page[start..end].replace("&amp;", "&")).expect("an absolute URL")
}

/// The `value` of the hidden input `name` in a consent page.
///
/// # Arguments
/// * `page` - the page's HTML.
/// * `name` - the input's name.
///
/// # Returns
/// The value, as written (escaped).
///
/// # Panics
/// If the page has no such input.
pub(crate) fn hidden(page: &str, name: &str) -> String {
    let marker = format!("name=\"{name}\" value=\"");
    let start = page.find(&marker).expect("hidden input") + marker.len();
    let end = start + page[start..].find('"').expect("closing quote");
    page[start..end].to_string()
}

/// Every hidden input of the form `form_id` in a consent page, in order:
/// what a browser would post from that form, minus its buttons.
///
/// # Arguments
/// * `page` - the page's HTML.
/// * `form_id` - the form's `id` attribute.
///
/// # Returns
/// `(name, value)` pairs, as written (escaped).
///
/// # Panics
/// If the page has no such form.
pub(crate) fn hidden_inputs(page: &str, form_id: &str) -> Vec<(String, String)> {
    let start = page.find(&format!("id=\"{form_id}\"")).expect("the form");
    let form = &page[start..];
    let form = &form[..form.find("</form>").expect("the form's end")];
    form.match_indices("<input type=\"hidden\" name=\"")
        .map(|(at, marker)| {
            let rest = &form[at + marker.len()..];
            let (name, rest) = rest.split_once("\" value=\"").expect("a value");
            let (value, _) = rest.split_once('"').expect("closing quote");
            (name.to_string(), value.to_string())
        })
        .collect()
}
