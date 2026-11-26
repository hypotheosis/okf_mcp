//! `POST /oauth/token` and `POST /oauth/revoke` (spec "Endpoints").
//!
//! Tokens are opaque and stored as SHA-256 only. One authorization code
//! starts a token family; each refresh rotates the refresh token within the
//! family. Presenting a spent code or a used refresh token revokes the
//! family (RFC 6749 section 4.1.2; OAuth 2.1 refresh token rotation).

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::rejection::FormRejection;
use axum::extract::State;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Form, Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;

use crate::app::App;
use crate::error::OkfError;
use crate::http::REQUEST_TIMEOUT;
use crate::keys;
use crate::oauth::error::OAuthError;
use crate::oauth::state::{code_key, AuthorizationCode};
use crate::oauth::{pkce, OAuth};
use crate::store::oauth::tokens::{
    self as token_rows, LiveAccessToken, NewToken, RefreshUse, RevokedToken, TokenKind,
};
use crate::store::sign_in_events::Outcome;
use crate::telemetry::metrics::{Grant, Limiter};

/// The routes: `POST /oauth/token` and `POST /oauth/revoke`.
///
/// # Returns
/// A router for [`crate::oauth::routes`] to merge (which adds
/// `Cache-Control: no-store` to every answer).
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "router builder: runs once at startup, outside any request, so a span would be an orphan root"
    )
)]
pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/oauth/token", post(token))
        .route("/oauth/revoke", post(revoke))
}

/// A token request's fields (RFC 6749 sections 4.1.3 and 6). All optional,
/// so a missing one is `invalid_request` from the handler, not a 422 from
/// the extractor.
// `Debug` by hand: codes, verifiers and tokens never reach a log.
#[derive(Default, Deserialize)]
struct TokenForm {
    grant_type: Option<String>,
    code: Option<String>,
    redirect_uri: Option<String>,
    client_id: Option<String>,
    code_verifier: Option<String>,
    resource: Option<String>,
    refresh_token: Option<String>,
}

impl std::fmt::Debug for TokenForm {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "Display/Debug impl: runs while the subscriber formats an event"
        )
    )]
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `finish_non_exhaustive` prints `..` for the fields left out.
        formatter
            .debug_struct("TokenForm")
            .field("grant_type", &self.grant_type)
            .field("client_id", &self.client_id)
            .field("redirect_uri", &self.redirect_uri)
            .field("resource", &self.resource)
            .finish_non_exhaustive()
    }
}

/// A revocation request (RFC 7009 section 2.1). `client_id` and
/// `token_type_hint` may be sent and are ignored: the stored row says the
/// token's kind, and clients are public.
#[derive(Default, Deserialize)]
struct RevokeForm {
    token: Option<String>,
}

/// A successful token response (RFC 6749 section 5.1).
// `Debug` by hand, for the same reason as `TokenForm`.
#[derive(Serialize)]
struct TokenResponse {
    access_token: String,
    token_type: &'static str,
    expires_in: u64,
    refresh_token: String,
}

impl std::fmt::Debug for TokenResponse {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "Display/Debug impl: runs while the subscriber formats an event"
        )
    )]
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TokenResponse")
            .field("token_type", &self.token_type)
            .field("expires_in", &self.expires_in)
            .finish_non_exhaustive()
    }
}

impl IntoResponse for TokenResponse {
    #[tracing::instrument(level = "trace", skip_all)]
    fn into_response(self) -> Response {
        // RFC 6749 section 5.1: a response carrying tokens must not be
        // cached, by HTTP/1.1 (`Cache-Control`) or HTTP/1.0 (`Pragma`) caches.
        // `Cache-Control: no-store` comes from the `/oauth/*` route layer
        // (`oauth::routes`), so it is not set here as well; only `Pragma` is.
        (
            [(header::PRAGMA, HeaderValue::from_static("no-cache"))],
            Json(self),
        )
            .into_response()
    }
}

/// A required, non-empty field.
///
/// # Arguments
/// * `value` - the field as parsed.
/// * `name` - its name, for the error.
///
/// # Returns
/// The text, borrowed from the form (the `'a` says the result lives as long
/// as `value`, not `name`).
///
/// # Errors
/// [`OAuthError::InvalidRequest`] naming the field.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
    )
)]
fn required<'a>(value: &'a Option<String>, name: &str) -> Result<&'a str, OAuthError> {
    value
        .as_deref()
        .filter(|text| !text.is_empty())
        .ok_or_else(|| OAuthError::InvalidRequest(format!("{name} is required")))
}

/// What a pair of tokens is bound to.
#[derive(Debug, Clone, Copy)]
struct Binding<'a> {
    grant_id: &'a str,
    family_id: &'a str,
    resource: &'a str,
}

/// A minted pair, written but not yet committed or cached.
struct Minted {
    response: TokenResponse,
    access_hash: [u8; 32],
    access: LiveAccessToken,
    /// When it was minted, for the cache's pruning.
    now: DateTime<Utc>,
    /// The cache's revocation generation, read before the tokens were
    /// written: a family revocation that lands before `remember` keeps the
    /// token out of the cache.
    generation: u64,
}

/// Mints an access and a refresh token in `conn`'s transaction.
///
/// # Arguments
/// * `conn` - the transaction (the caller commits, then calls [`remember`]).
/// * `oauth` - the lifetimes and the access-token cache.
/// * `binding` - the grant, family and resource the tokens are bound to.
/// * `now` - the server clock.
///
/// # Returns
/// The pair and what caching its access token needs.
///
/// # Errors
/// [`OkfError::Io`] if the random source fails; [`OkfError::Database`];
/// [`OkfError::Config`] for an impossible lifetime.
#[tracing::instrument(level = "debug", skip_all)]
async fn mint(
    conn: &mut PgConnection,
    oauth: &OAuth,
    binding: Binding<'_>,
    now: DateTime<Utc>,
) -> Result<Minted, OkfError> {
    // Read before any write: see `Minted::generation`.
    let generation = oauth.access.generation();
    let access_token = keys::generate_with(keys::ACCESS_TOKEN_PREFIX)?;
    let refresh_token = keys::generate_with(keys::REFRESH_TOKEN_PREFIX)?;
    // `clock::after` returns an error past the calendar's range, where
    // `now + delta` would panic.
    let access_expires = crate::clock::after(now, oauth.settings.access_token_ttl)?;
    let refresh_expires = crate::clock::after(now, oauth.settings.refresh_token_ttl)?;
    let access_hash = keys::hash(&access_token);
    for (sha256, kind, expires_at) in [
        (access_hash, TokenKind::Access, access_expires),
        (
            keys::hash(&refresh_token),
            TokenKind::Refresh,
            refresh_expires,
        ),
    ] {
        let new = NewToken {
            sha256,
            grant_id: binding.grant_id,
            kind,
            family_id: binding.family_id,
            resource: binding.resource,
            expires_at,
        };
        // `&mut *conn` reborrows the connection for one call, so the loop
        // can use it again (Python has no equivalent: a borrow ends here).
        token_rows::insert(&mut *conn, &new, now).await?;
    }
    Ok(Minted {
        response: TokenResponse {
            access_token,
            token_type: "Bearer",
            expires_in: oauth.settings.access_token_ttl.as_secs(),
            refresh_token,
        },
        access_hash,
        access: LiveAccessToken {
            grant_id: binding.grant_id.to_string(),
            resource: binding.resource.to_string(),
            expires_at: access_expires,
        },
        now,
        generation,
    })
}

/// After commit: caches the new access token and hands back the response.
#[tracing::instrument(level = "trace", skip_all)]
fn remember(oauth: &OAuth, minted: Minted) -> TokenResponse {
    oauth.access.remember(
        minted.access_hash,
        minted.access,
        minted.now,
        minted.generation,
    );
    minted.response
}

/// Revokes a family in the database, then drops its access tokens from the
/// cache.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all)]
async fn revoke_family(app: &App, oauth: &OAuth, family_id: &str) -> Result<(), OkfError> {
    let revoked =
        token_rows::revoke_family(app.accounts().executor(), family_id, app.clock().now()).await?;
    oauth.access.forget(&revoked).await;
    Ok(())
}

/// `POST /oauth/token`. One `okf.oauth.token` span per request; its
/// `okf.grant` is one of the closed set, set once the form is read (an
/// unsupported type stays empty: a client chose it). The span also carries
/// `method` (`oauth_token`), `outcome` (`ok` or `failed`, the spellings of
/// `okf_auth_attempts_total`), `okf.error` (the RFC 6749 error code, on
/// failure) and, for a refresh, `okf.refresh` (what presenting the token
/// found), and never a code, token or client id. No attribute key holds
/// `auth` or `token`, which trace backends' default scrubbers hide.
#[tracing::instrument(
    level = "info",
    name = "okf.oauth.token",
    skip_all,
    fields(
        method = "oauth_token",
        outcome = tracing::field::Empty,
        okf.grant = tracing::field::Empty,
        okf.error = tracing::field::Empty,
        okf.refresh = tracing::field::Empty
    )
)]
async fn token(
    State(app): State<Arc<App>>,
    form: Result<Form<TokenForm>, FormRejection>,
) -> Result<TokenResponse, OAuthError> {
    // Held so the inner function records the grant on this span, not on its own.
    let span = tracing::Span::current();
    let result = issue_token(&span, app, form).await;
    let outcome = match &result {
        Ok(_) => Outcome::Ok,
        Err(error) => {
            span.record("okf.error", error.code());
            Outcome::Failed
        }
    };
    span.record("outcome", outcome.as_str());
    result
}

/// The body of [`token`]: rate limit, read the form, run the grant. It
/// records the grant on `token`'s span.
#[tracing::instrument(level = "debug", skip_all)]
async fn issue_token(
    span: &tracing::Span,
    app: Arc<App>,
    form: Result<Form<TokenForm>, FormRejection>,
) -> Result<TokenResponse, OAuthError> {
    let oauth = app
        .oauth()
        .ok_or_else(|| OAuthError::InvalidRequest("OAuth is off".to_string()))?;
    if !oauth.token_limit.try_take(Instant::now()).await {
        app.metrics().rate_limited(Limiter::OauthToken);
        return Err(OAuthError::SlowDown);
    }
    let Form(form) = form.map_err(|_| {
        OAuthError::InvalidRequest("the body must be application/x-www-form-urlencoded".to_string())
    })?;
    match required(&form.grant_type, "grant_type")? {
        "authorization_code" => {
            span.record("okf.grant", Grant::AuthorizationCode.as_str());
            exchange_code(&app, oauth, &form).await
        }
        "refresh_token" => {
            span.record("okf.grant", Grant::RefreshToken.as_str());
            refresh(span, &app, oauth, &form).await
        }
        other => Err(OAuthError::UnsupportedGrantType(format!(
            "{other:?} is not supported"
        ))),
    }
}

/// How long a code stays in the store at least after an exchange marked it
/// used. The OAuth routes are cut off at [`REQUEST_TIMEOUT`], so an exchange
/// cannot reach its replay check later than that after the mark; the floor is
/// that deadline plus a margin, so the check always finds the spent code even
/// if the code's own 60 s lifetime ended meanwhile. A code gone then would be
/// indistinguishable from a replayed one and would revoke a legitimate
/// exchange's tokens (issue #590). Derived from the deadline so the two
/// cannot drift apart.
const USED_CODE_FLOOR: Duration = Duration::from_secs(REQUEST_TIMEOUT.as_secs() + 30);

/// `grant_type=authorization_code`.
#[tracing::instrument(level = "debug", skip_all)]
async fn exchange_code(
    app: &App,
    oauth: &OAuth,
    form: &TokenForm,
) -> Result<TokenResponse, OAuthError> {
    let code = required(&form.code, "code")?;
    let client_id = required(&form.client_id, "client_id")?;
    let redirect_uri = required(&form.redirect_uri, "redirect_uri")?;
    let verifier = required(&form.code_verifier, "code_verifier")?;
    let resource = required(&form.resource, "resource")?;
    let key = code_key(code);
    // The code was live at this instant; the replay check after commit asks
    // the map as of the same instant, so the code expiring meanwhile does not
    // hide a replay.
    let started = Instant::now();
    // Marking the code used atomically makes the exchange single-use even
    // for two racing requests, on one replica (the map's lock) or on two
    // (Redis's compare-and-set: only one write of `used` from `false` lands,
    // and the loser reruns this closure on the stored copy, sees `used` and
    // is the replay). A used code stays until it expires, so a replay is
    // recognized and revokes its family. The closure reads and changes only
    // `stored`, so a rerun on a fresh copy answers the same (`lessons.md`).
    // A Redis failure (`?`) answers 503 and never falls back to memory
    // (spec Decision 16): the code is neither spent nor exchanged.
    //
    // The same script that marks the code used also keeps it at least
    // `USED_CODE_FLOOR` longer, so the check after the commit finds it even
    // when the code's own lifetime ran out meanwhile (a code is only ever
    // extended once it is spent, never while it could still be exchanged).
    let taken = oauth
        .codes
        .update_with_ttl_floor(&key, started, USED_CODE_FLOOR, |stored| {
            if stored.used {
                stored.replayed = true;
                Err(stored.family_id.clone())
            } else {
                stored.used = true;
                Ok(stored.clone())
            }
        })
        .await?;
    let stored: AuthorizationCode = match taken {
        None => {
            return Err(OAuthError::InvalidGrant(
                "unknown or expired code".to_string(),
            ))
        }
        Some(Err(family_id)) => {
            revoke_family(app, oauth, &family_id).await?;
            return Err(OAuthError::InvalidGrant(
                "the code was already used".to_string(),
            ));
        }
        Some(Ok(stored)) => stored,
    };
    // Each refusal below leaves the code spent: a code is good for one
    // attempt, right or wrong.
    if stored.client_id != client_id || stored.redirect_uri != redirect_uri {
        return Err(OAuthError::InvalidGrant(
            "the code was issued to another client or redirect URI".to_string(),
        ));
    }
    if stored.resource != resource {
        return Err(OAuthError::InvalidTarget(
            "resource differs from the authorization request".to_string(),
        ));
    }
    if !pkce::verify(&stored.code_challenge, verifier) {
        return Err(OAuthError::InvalidGrant(
            "code_verifier does not match".to_string(),
        ));
    }
    // The credential cache reads a grant it does not hold, so one made on
    // another replica is found at once.
    if app.find_grant(&stored.grant_id).await?.is_none() {
        return Err(OAuthError::InvalidGrant(
            "the grant was revoked".to_string(),
        ));
    }
    let binding = Binding {
        grant_id: &stored.grant_id,
        family_id: &stored.family_id,
        resource: &stored.resource,
    };
    let mut tx = app
        .accounts()
        .executor()
        .begin()
        .await
        .map_err(OkfError::from)?;
    // The grant's share lock, then the family's, as a rotation takes them
    // (`use_refresh`): a grant revoked since the key-cache check above is
    // refused here, and a revocation that starts now waits for this commit.
    if !token_rows::lock_grant(&mut tx, &stored.grant_id).await? {
        return Err(OAuthError::InvalidGrant(
            "the grant was revoked".to_string(),
        ));
    }
    // A family revocation waits for this commit and then sees these tokens.
    token_rows::lock_family(&mut tx, &stored.family_id).await?;
    let minted = mint(&mut tx, oauth, binding, app.clock().now()).await?;
    tx.commit().await.map_err(OkfError::from)?;
    // The lock covers a revocation that starts while this transaction
    // holds it. A replay can also revoke the family before this exchange
    // took the lock, before these tokens existed. The flag is set atomically
    // (the map's lock, or Redis's compare-and-set) before that revocation,
    // so either this sees it, or the replay's revocation ran after the
    // commit and caught these tokens. The read is an `update` that changes
    // nothing, so it sees the latest stored copy. Only a stored code with
    // `replayed` unset keeps the tokens. A code gone from the store
    // (Redis lost it, or it outlived `USED_CODE_FLOOR`) cannot vouch for itself, and neither can
    // a Redis failure: an exchange that cannot prove it was not replayed
    // must not keep its tokens, so both revoke (spec Decision 16). The
    // failure is still answered as what it is, a 503 to retry, not a replay.
    match oauth
        .codes
        .update(&key, started, |stored| stored.replayed)
        .await
    {
        Ok(Some(false)) => {}
        Ok(Some(true) | None) => {
            revoke_family(app, oauth, &stored.family_id).await?;
            return Err(OAuthError::InvalidGrant(
                "the code was used twice".to_string(),
            ));
        }
        Err(error) => {
            revoke_family(app, oauth, &stored.family_id).await?;
            return Err(OAuthError::Server(error));
        }
    }
    app.metrics().oauth_token_issued(Grant::AuthorizationCode);
    Ok(remember(oauth, minted))
}

/// `grant_type=refresh_token`. Records what presenting the token found on
/// `span` (the request's `okf.oauth.token` span) as `okf.refresh`:
/// `rotated`, `grace` (a used token rotated again within
/// `oauth.refresh_reuse_grace`), `reused` (the family is revoked) or
/// `unknown`.
#[tracing::instrument(level = "debug", skip_all)]
async fn refresh(
    span: &tracing::Span,
    app: &App,
    oauth: &OAuth,
    form: &TokenForm,
) -> Result<TokenResponse, OAuthError> {
    let presented = required(&form.refresh_token, "refresh_token")?;
    let client_id = required(&form.client_id, "client_id")?;
    let now = app.clock().now();
    let mut tx = app
        .accounts()
        .executor()
        .begin()
        .await
        .map_err(OkfError::from)?;
    let grace = oauth.settings.refresh_reuse_grace;
    let found = token_rows::use_refresh(&mut tx, &keys::hash(presented), now, grace).await?;
    span.record("okf.refresh", refresh_label(&found));
    match found {
        RefreshUse::Unknown => Err(OAuthError::InvalidGrant(
            "unknown, expired or revoked refresh token".to_string(),
        )),
        RefreshUse::Reused { family_id } => {
            // Nothing was written; dropping `tx` rolls back and releases the
            // row lock before the revocation updates the row.
            drop(tx);
            revoke_family(app, oauth, &family_id).await?;
            Err(OAuthError::InvalidGrant(
                "the refresh token was already used".to_string(),
            ))
        }
        RefreshUse::Rotated {
            grant_id,
            family_id,
            resource,
            client_id: grant_client,
            // A rotation within the grace window mints a successor like a
            // first one: the family then holds one live pair per process
            // that raced, each rotating on its own from here.
            again: _,
        } => {
            // Each refusal returns before `commit`, so dropping `tx` rolls
            // back the `used_at` mark and the rightful client can still use
            // the token. The grant's client was read under its share lock
            // on this transaction's connection: nothing before the commit
            // loads a cache, which would take a second pooled connection
            // while these locks are held.
            if grant_client.as_deref() != Some(client_id) {
                return Err(OAuthError::InvalidGrant(
                    "the refresh token belongs to another client".to_string(),
                ));
            }
            if form
                .resource
                .as_deref()
                .is_some_and(|asked| asked != resource)
            {
                return Err(OAuthError::InvalidTarget(
                    "resource differs from the original grant".to_string(),
                ));
            }
            let binding = Binding {
                grant_id: &grant_id,
                family_id: &family_id,
                resource: &resource,
            };
            let minted = mint(&mut tx, oauth, binding, now).await?;
            tx.commit().await.map_err(OkfError::from)?;
            app.metrics().oauth_token_issued(Grant::RefreshToken);
            Ok(remember(oauth, minted))
        }
    }
}

/// The `okf.refresh` span value for what presenting a refresh token found.
///
/// # Arguments
/// * `found` - [`token_rows::use_refresh`]'s answer.
///
/// # Returns
/// One of `rotated`, `grace`, `reused` or `unknown`.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
    )
)]
fn refresh_label(found: &RefreshUse) -> &'static str {
    match found {
        RefreshUse::Rotated { again: false, .. } => "rotated",
        RefreshUse::Rotated { again: true, .. } => "grace",
        RefreshUse::Reused { .. } => "reused",
        RefreshUse::Unknown => "unknown",
    }
}

/// `POST /oauth/revoke` (RFC 7009): 200 for any token, an unknown one
/// included. A refresh token takes its whole family with it.
#[tracing::instrument(level = "debug", skip_all)]
async fn revoke(
    State(app): State<Arc<App>>,
    form: Result<Form<RevokeForm>, FormRejection>,
) -> Result<StatusCode, OAuthError> {
    let oauth = app
        .oauth()
        .ok_or_else(|| OAuthError::InvalidRequest("OAuth is off".to_string()))?;
    if !oauth.token_limit.try_take(Instant::now()).await {
        app.metrics().rate_limited(Limiter::OauthToken);
        return Err(OAuthError::SlowDown);
    }
    let Form(form) = form.map_err(|_| {
        OAuthError::InvalidRequest("the body must be application/x-www-form-urlencoded".to_string())
    })?;
    let token = required(&form.token, "token")?;
    let hash = keys::hash(token);
    match token_rows::revoke(app.accounts().executor(), &hash, app.clock().now()).await? {
        // Revoking twice is harmless, so a family already revoked is fine.
        Some(RevokedToken {
            family_id,
            kind: TokenKind::Refresh,
        }) => revoke_family(&app, oauth, &family_id).await?,
        Some(RevokedToken {
            kind: TokenKind::Access,
            ..
        }) => oauth.access.forget(&[hash]).await,
        None => {}
    }
    Ok(StatusCode::OK)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use axum::http::{HeaderMap, StatusCode};

    use super::{TokenForm, TokenResponse};
    use crate::mcp::testing::{render, sample_of, TestApp};
    use crate::oauth::state::code_key;
    use crate::oauth::testing::{
        authorization_code, authorization_code_for, authorize_query_for, form,
        insert_loopback_client, oauth_app, oauth_app_with, oauth_app_with_metrics, send, CLIENT_ID,
        LOOPBACK_CLIENT_ID, LOOPBACK_ON_PORT, LOOPBACK_REDIRECT_URI, REDIRECT_URI, RESOURCE,
        VERIFIER,
    };

    fn exchange(code: &str) -> String {
        exchange_with(code, "", "")
    }

    /// [`oauth_app`] with `oauth.refresh_reuse_grace` off, so a second use
    /// of a refresh token revokes its family at once.
    async fn oauth_app_without_grace() -> TestApp {
        let mut config =
            crate::store::oauth::testing::oauth_config(crate::oauth::testing::PUBLIC_URL);
        config.oauth.refresh_reuse_grace = std::time::Duration::ZERO;
        oauth_app_with(config).await
    }

    /// An exchange request with the field `name` set to `value` instead
    /// (an empty `name` changes nothing).
    fn exchange_with(code: &str, name: &str, value: &str) -> String {
        let fields = [
            ("grant_type", "authorization_code"),
            ("code", code),
            ("client_id", CLIENT_ID),
            ("redirect_uri", REDIRECT_URI),
            ("code_verifier", VERIFIER),
            ("resource", RESOURCE),
        ];
        let pairs: Vec<(&str, &str)> = fields
            .iter()
            .map(|&(key, original)| (key, if key == name { value } else { original }))
            .collect();
        form(&pairs)
    }

    fn refresh(token: &str, client_id: &str) -> String {
        form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", token),
            ("client_id", client_id),
        ])
    }

    async fn token(test: &TestApp, body: String) -> (StatusCode, HeaderMap, serde_json::Value) {
        let (status, headers, text) = send(test, "POST", "/oauth/token", Some(body)).await;
        (status, headers, serde_json::from_str(&text).expect("JSON"))
    }

    /// Whether `access` authenticates on this server's resource.
    async fn works(test: &TestApp, access: &str) -> bool {
        let oauth = test.app.oauth().expect("OAuth is on");
        oauth
            .access
            .verify(&test.app, &oauth.settings.resource, access)
            .await
            .unwrap()
            .is_some()
    }

    fn text(body: &serde_json::Value, field: &str) -> String {
        body[field].as_str().expect(field).to_string()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_code_exchanges_once_for_a_working_token_pair() {
        let test = oauth_app().await;
        let code = authorization_code(&test).await;

        let (status, headers, body) = token(&test, exchange(&code)).await;

        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(headers["cache-control"], "no-store");
        // Exactly one `Cache-Control`, from the route layer alone (issue #270).
        assert_eq!(headers.get_all("cache-control").iter().count(), 1);
        assert_eq!(headers["pragma"], "no-cache");
        assert_eq!(body["token_type"], "Bearer");
        assert_eq!(body["expires_in"], 24 * 60 * 60);
        let access = text(&body, "access_token");
        assert!(access.starts_with("okfat_"));
        assert!(text(&body, "refresh_token").starts_with("okfrt_"));
        let oauth = test.app.oauth().unwrap();
        let principal = oauth
            .access
            .verify(&test.app, &oauth.settings.resource, &access)
            .await
            .unwrap()
            .expect("the access token authenticates as the grant");
        assert_eq!(principal.actor, "example-actor");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_replayed_code_is_refused_and_revokes_what_it_minted() {
        let test = oauth_app().await;
        let code = authorization_code(&test).await;
        let (_, _, first) = token(&test, exchange(&code)).await;
        let access = text(&first, "access_token");
        let refresh_token = text(&first, "refresh_token");
        assert!(works(&test, &access).await);

        let (status, _, body) = token(&test, exchange(&code)).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid_grant");
        assert!(!works(&test, &access).await, "the access token is revoked");
        let (status, _, _) = token(&test, refresh(&refresh_token, CLIENT_ID)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "so is the refresh token");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_exchange_that_a_replay_overtakes_is_revoked() {
        // The race the `replayed` flag closes: the replay is seen while the
        // first exchange is still minting. Flag the stored code as replayed
        // by hand, as a replay arriving then would, and the exchange revokes
        // what it minted instead of handing it out.
        let test = oauth_app().await;
        let code = authorization_code(&test).await;
        let oauth = test.app.oauth().unwrap();
        oauth
            .codes
            .update(&code_key(&code), Instant::now(), |stored| {
                stored.replayed = true;
            })
            .await
            .unwrap()
            .expect("the code is stored");

        let (status, _, body) = token(&test, exchange(&code)).await;

        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"], "invalid_grant");
        let live: i64 =
            sqlx::query_scalar("SELECT count(*) FROM oauth_tokens WHERE revoked_at IS NULL")
                .fetch_one(test.app.accounts().executor())
                .await
                .unwrap();
        assert_eq!(live, 0, "the minted pair is revoked");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn each_binding_of_the_code_is_checked() {
        for (name, value, expected) in [
            ("code_verifier", "x".repeat(43), "invalid_grant"),
            ("client_id", "another-client".to_string(), "invalid_grant"),
            (
                "redirect_uri",
                format!("{REDIRECT_URI}/other"),
                "invalid_grant",
            ),
            (
                "resource",
                "https://other.example.com/mcp".to_string(),
                "invalid_target",
            ),
        ] {
            let test = oauth_app().await;
            let code = authorization_code(&test).await;

            let (status, _, json) = token(&test, exchange_with(&code, name, &value)).await;

            assert_eq!(status, StatusCode::BAD_REQUEST, "{name}");
            assert_eq!(json["error"], expected, "{name}");
            // The code is spent by the failed attempt.
            let (status, _, _) = token(&test, exchange(&code)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{name}: single use");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_loopback_code_exchanges_only_with_the_port_it_was_issued_for() {
        // Authorize may pick any loopback port (RFC 8252 section 7.3), but
        // the token request repeats that exact URI (RFC 6749 section 4.1.3).
        let test = oauth_app().await;
        insert_loopback_client(&test).await;
        let query = authorize_query_for(LOOPBACK_CLIENT_ID, LOOPBACK_ON_PORT);
        let exchange_at = |code: &str, redirect_uri: &str| {
            form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("client_id", LOOPBACK_CLIENT_ID),
                ("redirect_uri", redirect_uri),
                ("code_verifier", VERIFIER),
                ("resource", RESOURCE),
            ])
        };

        let code = authorization_code_for(&test, &query).await;
        let (status, _, body) = token(&test, exchange_at(&code, LOOPBACK_ON_PORT)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(works(&test, &text(&body, "access_token")).await);

        for other in ["http://localhost:54124/callback", LOOPBACK_REDIRECT_URI] {
            let code = authorization_code_for(&test, &query).await;
            let (status, _, body) = token(&test, exchange_at(&code, other)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{other}: {body}");
            assert_eq!(body["error"], "invalid_grant", "{other}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_code_whose_grant_was_revoked_is_refused() {
        let test = oauth_app().await;
        let code = authorization_code(&test).await;
        let grant: String = sqlx::query_scalar("SELECT id FROM api_keys WHERE kind = 'oauth'")
            .fetch_one(test.app.accounts().executor())
            .await
            .unwrap();
        crate::invalidate::testing::applied(&test.app, async {
            crate::store::keys::revoke_by_id(
                test.app.accounts().executor(),
                &grant,
                &crate::store::audit::AuditActor::cli(),
                test.app.clock().now(),
            )
            .await
            .unwrap();
        })
        .await;

        let (status, _, body) = token(&test, exchange(&code)).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid_grant");
    }

    /// A code exchange racing a revocation of its grant (issue #652): the
    /// revocation holds the grant's `api_keys` row `FOR UPDATE`, which the
    /// exchange's `lock_grant` (`FOR SHARE`) waits behind. The credential
    /// cache check before it still sees the grant live (the revocation is
    /// uncommitted), so only the lock protocol can refuse the exchange.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_code_exchange_waiting_on_a_revocation_is_refused_and_mints_nothing() {
        let test = oauth_app().await;
        let code = authorization_code(&test).await;
        let pool = test.app.accounts().executor();
        let grant: String = sqlx::query_scalar("SELECT id FROM api_keys WHERE kind = 'oauth'")
            .fetch_one(pool)
            .await
            .unwrap();
        // The revocation, as `revoke_by_id` takes it: the row `FOR UPDATE`,
        // then the update, committed later.
        let mut revocation = pool.begin().await.unwrap();
        let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *revocation)
            .await
            .unwrap();
        sqlx::query("SELECT 1 FROM api_keys WHERE id = $1 FOR UPDATE")
            .bind(&grant)
            .execute(&mut *revocation)
            .await
            .unwrap();
        sqlx::query("UPDATE api_keys SET revoked_at = $2 WHERE id = $1")
            .bind(&grant)
            .bind(test.app.clock().now())
            .execute(&mut *revocation)
            .await
            .unwrap();

        // The exchange runs beside the driver, which commits the revocation
        // only once the exchange is confirmed waiting on the grant's row.
        let exchanging = token(&test, exchange(&code));
        let driver = async {
            let waiter = crate::test_locks::expect_row_lock_waiter(pool, holder, "api_keys").await;
            assert_ne!(waiter, holder);
            revocation.commit().await.unwrap();
        };
        let ((status, _, body), ()) = tokio::time::timeout(Duration::from_secs(30), async {
            tokio::join!(exchanging, driver)
        })
        .await
        .expect("the exchange finished within 30 s of the revocation");

        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"], "invalid_grant");
        assert_eq!(body["error_description"], "the grant was revoked");
        let minted = |pool| {
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM oauth_tokens WHERE grant_id = $1")
                .bind(grant.clone())
                .fetch_one(pool)
        };
        assert_eq!(minted(pool).await.unwrap(), 0, "no token row was minted");
        // The code was spent by the refused attempt: it cannot be tried again.
        let (status, _, body) = token(&test, exchange(&code)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"], "invalid_grant");
        assert_eq!(minted(pool).await.unwrap(), 0, "the replay minted nothing");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oauth_tokens_count_by_grant() {
        use crate::telemetry::config::LogFormat;
        use crate::telemetry::subscriber::capture::observe;

        let (test, registry) = oauth_app_with_metrics().await;
        let code = authorization_code(&test).await;
        let (_, _, first) = token(&test, exchange(&code)).await;
        let refresh_token = text(&first, "refresh_token");
        let counted = render(&registry);
        let series = |grant: &str, text: &str| {
            sample_of(
                text,
                &format!("okf_oauth_tokens_issued_total{{grant=\"{grant}\"}}"),
            )
        };
        assert_eq!(
            series("authorization_code", &counted),
            Some(1.0),
            "{counted}"
        );
        assert_eq!(series("refresh_token", &counted), None, "{counted}");

        // The refresh runs under an in-memory span exporter, on this thread.
        let observed = tokio::task::block_in_place(|| {
            observe("trace", "trace", "trace", LogFormat::Text, || {
                tokio::runtime::Handle::current()
                    .block_on(token(&test, refresh(&refresh_token, CLIENT_ID)));
            })
        });

        let counted = render(&registry);
        assert_eq!(
            series("authorization_code", &counted),
            Some(1.0),
            "{counted}"
        );
        assert_eq!(series("refresh_token", &counted), Some(1.0), "{counted}");
        let span = observed
            .spans
            .iter()
            .find(|span| span.name == "okf.oauth.token")
            .expect("an okf.oauth.token span");
        let attribute = |key: &str| {
            span.attributes
                .iter()
                .find(|attribute| attribute.key.as_str() == key)
                .map(|attribute| attribute.value.to_string())
        };
        assert_eq!(attribute("okf.grant").as_deref(), Some("refresh_token"));
        assert_eq!(attribute("okf.refresh").as_deref(), Some("rotated"));
        assert_eq!(attribute("okf.error"), None, "it succeeded");
        let exported = observed.exported_text();
        for secret in [refresh_token.as_str(), CLIENT_ID] {
            assert!(!exported.contains(secret), "{secret} in {exported}");
        }
    }

    /// Design, Auth row: the `okf.oauth.token` span says which method ran
    /// and how it ended, and never a code or token.
    #[tokio::test]
    async fn the_token_span_carries_method_and_outcome() {
        use crate::telemetry::config::LogFormat;
        use crate::telemetry::subscriber::capture::{attribute, Observer};

        let test = oauth_app().await;
        let code = authorization_code(&test).await;
        let observer = Observer::start("off", "info", "off", LogFormat::Text);

        let (ok, _, _) = token(&test, exchange(&code)).await;
        // The same code again is a replay.
        let (replay, _, _) = token(&test, exchange(&code)).await;
        let observed = observer.finish();

        assert_eq!(ok, StatusCode::OK);
        assert_ne!(replay, StatusCode::OK);
        let seen: Vec<_> = observed
            .spans_named("okf.oauth.token")
            .iter()
            .map(|span| {
                (
                    attribute(span, "method").map(ToString::to_string),
                    attribute(span, "outcome").map(ToString::to_string),
                )
            })
            .collect();
        let want = |outcome: &str| (Some("oauth_token".to_string()), Some(outcome.to_string()));
        assert_eq!(seen, [want("ok"), want("failed")]);
        let exported = observed.exported_text();
        assert!(!exported.contains(&code), "{exported}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_exhausted_token_bucket_counts_as_the_oauth_token_limiter() {
        let (test, registry) = oauth_app_with_metrics().await;
        let oauth = test.app.oauth().expect("OAuth is on");
        while oauth.token_limit.try_take(std::time::Instant::now()).await {}

        let (status, _, body) = token(&test, exchange("okfac_unused")).await;

        assert_ne!(status, StatusCode::OK, "{body}");
        let counted = render(&registry);
        assert_eq!(
            sample_of(
                &counted,
                "okf_ratelimit_rejections_total{limiter=\"oauth_token\"}"
            ),
            Some(1.0),
            "{counted}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn refresh_rotates_and_a_reused_refresh_token_revokes_the_family() {
        let test = oauth_app_without_grace().await;
        let code = authorization_code(&test).await;
        let (_, _, first) = token(&test, exchange(&code)).await;
        let old_refresh = text(&first, "refresh_token");

        let (status, headers, second) = token(&test, refresh(&old_refresh, CLIENT_ID)).await;
        assert_eq!(status, StatusCode::OK, "{second}");
        assert_eq!(headers["pragma"], "no-cache");
        let new_access = text(&second, "access_token");
        let new_refresh = text(&second, "refresh_token");
        assert_ne!(new_refresh, old_refresh);
        // Cached when minted: only `forget` can stop it now.
        assert!(works(&test, &new_access).await);

        let (status, _, reused) = token(&test, refresh(&old_refresh, CLIENT_ID)).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(reused["error"], "invalid_grant");
        // The whole family is gone: the new pair and the first access token.
        assert!(!works(&test, &new_access).await);
        assert!(!works(&test, &text(&first, "access_token")).await);
        let (status, _, _) = token(&test, refresh(&new_refresh, CLIENT_ID)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    /// Issue #884: processes of one client sharing a credential refresh
    /// with the same token at once. Within `oauth.refresh_reuse_grace` the
    /// loser is rotated too, and nobody is signed out: both new pairs work.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_racing_refresh_within_the_grace_window_keeps_the_family() {
        let test = oauth_app().await;
        let code = authorization_code(&test).await;
        let (_, _, first) = token(&test, exchange(&code)).await;
        let shared = text(&first, "refresh_token");

        let (won, _, winner) = token(&test, refresh(&shared, CLIENT_ID)).await;
        let (lost, _, loser) = token(&test, refresh(&shared, CLIENT_ID)).await;

        assert_eq!(won, StatusCode::OK, "{winner}");
        assert_eq!(lost, StatusCode::OK, "{loser}");
        assert_ne!(
            text(&winner, "refresh_token"),
            text(&loser, "refresh_token")
        );
        assert!(works(&test, &text(&winner, "access_token")).await);
        assert!(works(&test, &text(&loser, "access_token")).await);
        for pair in [&winner, &loser] {
            let (status, _, next) =
                token(&test, refresh(&text(pair, "refresh_token"), CLIENT_ID)).await;
            assert_eq!(status, StatusCode::OK, "each successor rotates: {next}");
        }
    }

    /// Within the window, a used token presented by another client is
    /// refused as an unused one is, and spends nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_grace_window_still_refuses_another_client() {
        let test = oauth_app().await;
        let code = authorization_code(&test).await;
        let (_, _, first) = token(&test, exchange(&code)).await;
        let shared = text(&first, "refresh_token");
        let (status, _, winner) = token(&test, refresh(&shared, CLIENT_ID)).await;
        assert_eq!(status, StatusCode::OK, "{winner}");

        let (status, _, refused) = token(&test, refresh(&shared, "another-client")).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(refused["error"], "invalid_grant");
        assert!(works(&test, &text(&winner, "access_token")).await);
    }

    /// The span says why a refresh failed: a reuse past the window is
    /// `okf.refresh = reused` with `okf.error = invalid_grant`.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_reused_refresh_says_so_on_its_span() {
        use crate::telemetry::config::LogFormat;
        use crate::telemetry::subscriber::capture::observe;

        let test = oauth_app_without_grace().await;
        let code = authorization_code(&test).await;
        let (_, _, first) = token(&test, exchange(&code)).await;
        let shared = text(&first, "refresh_token");
        let (status, _, _) = token(&test, refresh(&shared, CLIENT_ID)).await;
        assert_eq!(status, StatusCode::OK);

        let observed = tokio::task::block_in_place(|| {
            observe("trace", "trace", "trace", LogFormat::Text, || {
                tokio::runtime::Handle::current()
                    .block_on(token(&test, refresh(&shared, CLIENT_ID)));
            })
        });

        let span = observed
            .spans
            .iter()
            .find(|span| span.name == "okf.oauth.token")
            .expect("an okf.oauth.token span");
        let attribute = |key: &str| {
            span.attributes
                .iter()
                .find(|attribute| attribute.key.as_str() == key)
                .map(|attribute| attribute.value.to_string())
        };
        assert_eq!(attribute("okf.refresh").as_deref(), Some("reused"));
        assert_eq!(attribute("okf.error").as_deref(), Some("invalid_grant"));
        assert_eq!(attribute("outcome").as_deref(), Some("failed"));
        for attribute in &span.attributes {
            let key = attribute.key.as_str();
            assert!(
                !key.contains("auth") && !key.contains("token"),
                "{key} would be scrubbed by a trace backend"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_refresh_on_the_other_replica_catches_up_on_its_grant() {
        // The code is exchanged on A; the refresh token lives in Postgres,
        // so B can rotate it, once it has caught up on the grant A's consent
        // made (spec Decision 14).
        let (a, b, _redis) = crate::oauth::testing::oauth_replica_pair().await;
        let code = authorization_code(&a).await;
        let (_, refresh_token) = crate::oauth::testing::exchange_for_tokens(&a, &code).await;
        let request = TokenForm {
            grant_type: Some("refresh_token".to_string()),
            refresh_token: Some(refresh_token),
            client_id: Some(CLIENT_ID.to_string()),
            ..TokenForm::default()
        };

        let answer = super::refresh(
            &tracing::Span::none(),
            &b,
            b.oauth().expect("OAuth is on"),
            &request,
        )
        .await;

        assert!(
            answer.is_ok(),
            "B rotates the refresh token: {:?}",
            answer.err()
        );
    }

    /// The refresh transaction holds a connection and the grant's, family's
    /// and token's locks until it commits: nothing in it may take a second
    /// pooled connection (review of PR #713). With one connection in the
    /// pool and every cache cold, a second connection would never come.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_refresh_with_cold_caches_on_a_one_connection_pool_rotates() {
        let test = oauth_app().await;
        let code = authorization_code(&test).await;
        let (_, refresh_token) = crate::oauth::testing::exchange_for_tokens(&test, &code).await;
        let config = crate::config::Config {
            database_max_connections: 1,
            ..crate::store::oauth::testing::oauth_config(crate::oauth::testing::PUBLIC_URL)
        };
        let pool = crate::db::connect(test.db_url(), 1)
            .await
            .expect("a one-connection pool");
        // A fresh app: its credential and user caches have read nothing.
        let cold = crate::app::App::load_with_redis(
            pool,
            config,
            std::sync::Arc::new(crate::clock::SystemClock),
            crate::telemetry::metrics::Metrics::global(),
            None,
        )
        .await
        .expect("load the app");
        let request = TokenForm {
            grant_type: Some("refresh_token".to_string()),
            refresh_token: Some(refresh_token),
            client_id: Some(CLIENT_ID.to_string()),
            ..TokenForm::default()
        };

        let answer = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            super::refresh(
                &tracing::Span::none(),
                &cold,
                cold.oauth().expect("OAuth is on"),
                &request,
            ),
        )
        .await
        .expect("the refresh never waits for a second connection");

        assert!(answer.is_ok(), "rotated: {:?}", answer.err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_refused_refresh_does_not_spend_the_token() {
        let test = oauth_app().await;
        let code = authorization_code(&test).await;
        let (_, _, first) = token(&test, exchange(&code)).await;
        let refresh_token = text(&first, "refresh_token");

        let (status, _, by_another) = token(&test, refresh(&refresh_token, "another-client")).await;
        let other_resource = form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", &refresh_token),
            ("client_id", CLIENT_ID),
            ("resource", "https://other.example.com/mcp"),
        ]);
        let (target_status, _, for_another) = token(&test, other_resource).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(by_another["error"], "invalid_grant");
        assert_eq!(target_status, StatusCode::BAD_REQUEST);
        assert_eq!(for_another["error"], "invalid_target");
        // Rolled back: the rightful client can still use it, once.
        let (status, _, body) = token(&test, refresh(&refresh_token, CLIENT_ID)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(works(&test, &text(&first, "access_token")).await);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bad_requests_get_rfc_6749_errors() {
        let test = oauth_app().await;
        for (body, expected) in [
            (
                form(&[("grant_type", "password")]),
                "unsupported_grant_type",
            ),
            (form(&[("code", "x")]), "invalid_request"),
            (
                form(&[("grant_type", "authorization_code"), ("code", "okfac_x")]),
                "invalid_request",
            ),
            (exchange("okfac_unknown"), "invalid_grant"),
            (refresh("okfrt_unknown", CLIENT_ID), "invalid_grant"),
            (
                form(&[("grant_type", "refresh_token"), ("client_id", CLIENT_ID)]),
                "invalid_request",
            ),
        ] {
            let (status, headers, json) = token(&test, body.clone()).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert_eq!(json["error"], expected, "{body}");
            assert_eq!(headers["cache-control"], "no-store");
        }
        // Not a form at all.
        let (status, _, text) = send(&test, "POST", "/oauth/token", None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn revoking_a_refresh_token_revokes_its_family_and_unknown_tokens_are_fine() {
        let test = oauth_app().await;
        let code = authorization_code(&test).await;
        let (_, _, pair) = token(&test, exchange(&code)).await;
        let access = text(&pair, "access_token");
        let refresh_token = text(&pair, "refresh_token");
        assert!(works(&test, &access).await);

        let (unknown, headers, _) = send(
            &test,
            "POST",
            "/oauth/revoke",
            Some(form(&[("token", "okfat_nothing")])),
        )
        .await;
        let (status, _, _) = send(
            &test,
            "POST",
            "/oauth/revoke",
            Some(form(&[
                ("token", &refresh_token),
                ("token_type_hint", "refresh_token"),
                ("client_id", CLIENT_ID),
            ])),
        )
        .await;
        let (again, _, _) = send(
            &test,
            "POST",
            "/oauth/revoke",
            Some(form(&[("token", &refresh_token)])),
        )
        .await;

        assert_eq!(unknown, StatusCode::OK);
        assert_eq!(headers["cache-control"], "no-store");
        assert_eq!(status, StatusCode::OK);
        assert_eq!(again, StatusCode::OK);
        assert!(!works(&test, &access).await, "the family's access token");
        let (status, _, _) = token(&test, refresh(&refresh_token, CLIENT_ID)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn revoking_an_access_token_leaves_the_refresh_token() {
        let test = oauth_app().await;
        let code = authorization_code(&test).await;
        let (_, _, pair) = token(&test, exchange(&code)).await;
        let access = text(&pair, "access_token");
        assert!(works(&test, &access).await);

        let (status, _, _) = send(
            &test,
            "POST",
            "/oauth/revoke",
            Some(form(&[("token", &access)])),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(!works(&test, &access).await);
        let (status, _, _) = token(&test, refresh(&text(&pair, "refresh_token"), CLIENT_ID)).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn revoke_without_a_token_is_invalid_request() {
        let test = oauth_app().await;

        let (status, _, body) = send(&test, "POST", "/oauth/revoke", Some(String::new())).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("invalid_request"), "{body}");
    }

    #[test]
    fn the_used_code_floor_outlasts_the_request_deadline() {
        assert!(super::USED_CODE_FLOOR > crate::http::REQUEST_TIMEOUT);
    }

    #[test]
    fn debug_never_prints_codes_verifiers_or_tokens() {
        let secret = "okfac_do-not-print";
        let request = TokenForm {
            grant_type: Some("authorization_code".into()),
            code: Some(secret.into()),
            code_verifier: Some(secret.into()),
            refresh_token: Some(secret.into()),
            ..TokenForm::default()
        };
        let response = TokenResponse {
            access_token: secret.into(),
            token_type: "Bearer",
            expires_in: 3600,
            refresh_token: secret.into(),
        };

        let printed = format!("{request:?} {response:?}");

        assert!(!printed.contains(secret), "{printed}");
        assert!(printed.contains("authorization_code"));
    }

    /// Codes across two replicas over one Redis (spec Decision 5). Each
    /// body runs on standalone Redis and on a one-node cluster.
    mod replicas {
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        use axum::http::StatusCode;

        use super::{exchange, text};
        use crate::app::App;
        use crate::db::redis_support::TestRedis;
        use crate::oauth::state::{code_key, AuthorizationCode};
        use crate::oauth::testing::{
            approval_form, authorization_code, authorize_query, oauth_replica_pair_on, send_on,
        };
        use crate::shared::ephemeral::Keyspace;
        use crate::shared::hashed;

        /// Every body's bound (`lessons.md`: every wait is bounded).
        const BOUND: Duration = Duration::from_secs(120);

        /// Exchanges `code` on `app` through its router.
        async fn exchange_on(app: &Arc<App>, code: &str) -> (StatusCode, serde_json::Value) {
            let (status, _, body) =
                send_on(app, "POST", "/oauth/token", Some(exchange(code)), None).await;
            (status, serde_json::from_str(&body).expect("JSON"))
        }

        /// Whether `access` authenticates on `app`.
        async fn works_on(app: &App, access: &str) -> bool {
            let oauth = app.oauth().expect("OAuth is on");
            oauth
                .access
                .verify(app, &oauth.settings.resource, access)
                .await
                .expect("verify answers")
                .is_some()
        }

        /// The stored code, read through `app`'s store.
        async fn stored_on(app: &App, code: &str) -> Option<AuthorizationCode> {
            app.oauth()
                .expect("OAuth is on")
                .codes
                .update(&code_key(code), std::time::Instant::now(), |stored| {
                    stored.clone()
                })
                .await
                .expect("Redis answers")
        }

        async fn a_code_minted_on_one_replica_is_exchanged_on_the_other_once(server: TestRedis) {
            let (a, b, _server) = oauth_replica_pair_on(server).await;
            let code = authorization_code(&a).await;
            // B runs no subscriber in a lib pair and has never read the
            // grant: its credential cache reads it on first use.
            stored_on(&a.app, &code).await.expect("A stored the code");

            let (status, first) = exchange_on(&b, &code).await;

            assert_eq!(status, StatusCode::OK, "B exchanges A's code: {first}");
            let access = text(&first, "access_token");
            assert!(works_on(&b, &access).await, "the token works on B");
            assert!(works_on(&a.app, &access).await, "and on A");
            let spent = stored_on(&a.app, &code).await.expect("still stored");
            assert!(spent.used && !spent.replayed, "A sees it used");

            let (status, again) = exchange_on(&a.app, &code).await;

            assert_eq!(status, StatusCode::BAD_REQUEST, "{again}");
            assert_eq!(again["error"], "invalid_grant");
            assert_eq!(again["error_description"], "the code was already used");
            assert!(
                !works_on(&a.app, &access).await,
                "the family is revoked on A"
            );
            assert!(!works_on(&b, &access).await, "and on B, which cached it");
        }

        async fn two_replicas_racing_one_code_leave_no_usable_token(server: TestRedis) {
            let (a, b, _server) = oauth_replica_pair_on(server).await;
            // Several rounds, so a store that is not compare-and-set (both
            // read `used = false`) is caught even if one round serializes.
            for round in 0..4 {
                let code = authorization_code(&a).await;

                // `join!` polls both on this task: each sends its read before
                // either answer is handled, so the two reads overlap.
                let (on_a, on_b) = tokio::join!(exchange_on(&a.app, &code), exchange_on(&b, &code));

                let answers = [on_a, on_b];
                let succeeded = answers
                    .iter()
                    .filter(|(status, _)| *status == StatusCode::OK)
                    .count();
                assert!(
                    succeeded <= 1,
                    "round {round}: at most one 200: {answers:?}"
                );
                for (status, body) in answers
                    .iter()
                    .filter(|(status, _)| *status != StatusCode::OK)
                {
                    assert_eq!(*status, StatusCode::BAD_REQUEST, "round {round}: {body}");
                    assert_eq!(body["error"], "invalid_grant", "round {round}");
                }
                // One exchange marked it used; the other found it used and
                // is the one replay.
                let replays = answers
                    .iter()
                    .filter(|(_, body)| body["error_description"] == "the code was already used")
                    .count();
                assert_eq!(replays, 1, "round {round}: exactly one replay: {answers:?}");
                let stored = stored_on(&a.app, &code).await.expect("still stored");
                assert!(stored.used && stored.replayed, "round {round}: {stored:?}");
                for (_, body) in answers
                    .iter()
                    .filter(|(status, _)| *status == StatusCode::OK)
                {
                    let access = text(body, "access_token");
                    assert!(
                        !works_on(&a.app, &access).await,
                        "round {round}: refused on A"
                    );
                    assert!(!works_on(&b, &access).await, "round {round}: refused on B");
                }
            }
            let live: i64 =
                sqlx::query_scalar("SELECT count(*) FROM oauth_tokens WHERE revoked_at IS NULL")
                    .fetch_one(a.app.accounts().executor())
                    .await
                    .expect("count live tokens");
            assert_eq!(live, 0, "no minted token is live");
        }

        async fn an_expired_code_is_refused_on_either_replica(server: TestRedis) {
            let (a, b, server) = oauth_replica_pair_on(server).await;
            let code = authorization_code(&a).await;
            let handle = crate::shared::testing::connect(&server).await;
            // `Ephemeral` hashes the key it is given, and `code_key` is a
            // hash already: the name is `oauth:code:<hashed(code_key)>`.
            let key = handle.key(Keyspace::OauthCode.name(), &hashed(&code_key(&code)));
            let expiring: bool = redis::cmd("PEXPIRE")
                .arg(&key)
                .arg(1)
                .query_async(&mut handle.connection())
                .await
                .expect("PEXPIRE");
            assert!(expiring, "the code is stored under that key");
            tokio::time::sleep(Duration::from_millis(50)).await;

            for app in [&b, &a.app] {
                let (status, body) = exchange_on(app, &code).await;

                assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
                assert_eq!(body["error"], "invalid_grant");
                assert_eq!(body["error_description"], "unknown or expired code");
            }
        }

        /// Issue #590: a code whose own lifetime runs out while its exchange
        /// is minting is still a legitimate exchange, not a replay.
        async fn a_code_expiring_mid_exchange_keeps_its_tokens(server: TestRedis) {
            let (a, _b, server) = oauth_replica_pair_on(server).await;
            let code = authorization_code(&a).await;
            let family = stored_on(&a.app, &code)
                .await
                .expect("A stored the code")
                .family_id;
            let handle = crate::shared::testing::connect(&server).await;
            let key = handle.key(Keyspace::OauthCode.name(), &hashed(&code_key(&code)));
            // The code has this long left when the exchange starts.
            const LEFT: Duration = Duration::from_millis(2_000);
            let shortened = Instant::now();
            let expiring: bool = redis::cmd("PEXPIRE")
                .arg(&key)
                .arg(LEFT.as_millis() as u64)
                .query_async(&mut handle.connection())
                .await
                .expect("PEXPIRE");
            assert!(expiring, "the code is stored under that key");
            // Hold the family's lock, so the exchange stops after it marked
            // the code used and before its post-commit replay check.
            let mut holder = a
                .app
                .accounts()
                .executor()
                .begin()
                .await
                .expect("a transaction");
            crate::store::oauth::tokens::lock_family(&mut holder, &family)
                .await
                .expect("hold the family's lock");
            let exchanging = {
                let app = Arc::clone(&a.app);
                let code = code.clone();
                tokio::spawn(async move { exchange_on(&app, &code).await })
            };
            crate::http::api::testing::wait_for_lock_waiters(a.app.accounts().executor(), 1).await;
            let marked = stored_on(&a.app, &code).await.expect("still stored");
            assert!(marked.used, "the exchange marked the code used");
            // The marking write kept the code longer than it had left.
            let left: i64 = redis::cmd("PTTL")
                .arg(&key)
                .query_async(&mut handle.connection())
                .await
                .expect("PTTL");
            assert!(
                left > i64::try_from(LEFT.as_millis()).expect("fits"),
                "the used code outlives its old lifetime: {left} ms left"
            );
            // And it outlives the longest an exchange may take (the OAuth
            // routes are cut off at `REQUEST_TIMEOUT`), so a stall of any
            // allowed length still finds it; a stall that long is not slept.
            assert!(
                left > i64::try_from(crate::http::REQUEST_TIMEOUT.as_millis()).expect("fits"),
                "the used code outlives the request deadline: {left} ms left"
            );
            // Let what the code had left pass, as it does for an exchange
            // that is slow in its last moments.
            tokio::time::sleep(
                LEFT.saturating_sub(shortened.elapsed()) + Duration::from_millis(250),
            )
            .await;
            assert!(
                stored_on(&a.app, &code).await.is_some(),
                "the used code is still there for the check"
            );

            holder.rollback().await.expect("release the family's lock");
            let (status, body) = tokio::time::timeout(BOUND, exchanging)
                .await
                .expect("the exchange finished after the lock was released")
                .expect("the exchange task did not panic");

            assert_eq!(status, StatusCode::OK, "{body}");
            let access = text(&body, "access_token");
            assert!(works_on(&a.app, &access).await, "the tokens stay live");
        }

        /// A genuine replay on the other replica still answers `invalid_grant`
        /// and revokes the family, as before the floor.
        async fn a_replay_of_a_used_code_still_revokes_on_redis(server: TestRedis) {
            let (a, b, _server) = oauth_replica_pair_on(server).await;
            let code = authorization_code(&a).await;
            let (first, first_body) = exchange_on(&a.app, &code).await;
            let access = text(&first_body, "access_token");
            let (second, second_body) = exchange_on(&b, &code).await;

            assert_eq!(first, StatusCode::OK, "{first_body}");
            assert_eq!(second, StatusCode::BAD_REQUEST, "{second_body}");
            assert_eq!(second_body["error"], "invalid_grant");
            assert_eq!(
                second_body["error_description"],
                "the code was already used"
            );
            assert!(
                !works_on(&a.app, &access).await,
                "the replay revoked the family"
            );
        }

        async fn a_redis_outage_fails_closed(server: TestRedis) {
            // A shared server cannot be stopped for everyone.
            let (a, b, server) = oauth_replica_pair_on(server.dedicated().await).await;
            let code = authorization_code(&a).await;
            // Pending authorizations live in Redis too (Task 8), so the
            // consent is walked up to Approve before Redis stops.
            let approval = approval_form(&a, &authorize_query()).await;
            server.stop().await;

            let (status, body) = exchange_on(&b, &code).await;
            let (approve_status, _, page) =
                send_on(&a.app, "POST", "/oauth/authorize", Some(approval), None).await;

            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
            assert_eq!(body["error"], "server_error");
            assert_eq!(approve_status, StatusCode::SERVICE_UNAVAILABLE, "{page}");
            assert!(page.contains("The server is busy"), "the busy page: {page}");
            let live: i64 = sqlx::query_scalar("SELECT count(*) FROM oauth_tokens")
                .fetch_one(a.app.accounts().executor())
                .await
                .expect("count tokens");
            assert_eq!(live, 0, "nothing was minted, from memory or otherwise");
        }

        async fn a_redis_failure_after_the_commit_revokes_and_answers_503(server: TestRedis) {
            // A shared server cannot be stopped for everyone.
            let (a, b, server) = oauth_replica_pair_on(server.dedicated().await).await;
            let code = authorization_code(&a).await;
            let family = stored_on(&a.app, &code)
                .await
                .expect("A stored the code")
                .family_id;
            // Hold the family's lock, so B's exchange stops after it marked
            // the code used and before it mints; Redis fails while it waits,
            // so only the check after the commit meets the failure.
            let mut holder = a
                .app
                .accounts()
                .executor()
                .begin()
                .await
                .expect("a transaction");
            crate::store::oauth::tokens::lock_family(&mut holder, &family)
                .await
                .expect("hold the family's lock");
            let exchanging = {
                let b = Arc::clone(&b);
                let code = code.clone();
                // `tokio::spawn` runs the exchange as its own task (Python:
                // `asyncio.create_task`); `async move` gives it owned copies.
                tokio::spawn(async move { exchange_on(&b, &code).await })
            };
            crate::http::api::testing::wait_for_lock_waiters(a.app.accounts().executor(), 1).await;
            let marked = stored_on(&a.app, &code).await.expect("still stored");
            assert!(marked.used, "B marked the code used before the lock");
            server.stop().await;

            holder.rollback().await.expect("release the family's lock");
            let (status, body) = tokio::time::timeout(BOUND, exchanging)
                .await
                .expect("the exchange finished after the lock was released")
                .expect("the exchange task did not panic");

            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
            assert_eq!(body["error"], "server_error");
            let (minted, live): (i64, i64) = sqlx::query_as(
                "SELECT count(*), count(*) FILTER (WHERE revoked_at IS NULL) \
                   FROM oauth_tokens WHERE family_id = $1",
            )
            .bind(&family)
            .fetch_one(a.app.accounts().executor())
            .await
            .expect("count the family's tokens");
            assert!(minted > 0, "the exchange committed its tokens");
            assert_eq!(live, 0, "and the family was revoked");
        }

        // `macro_rules!` writes a standalone and a cluster `#[tokio::test]`
        // for every body named (Python: a parametrized test).
        macro_rules! in_both_modes {
            ($($name:ident),* $(,)?) => {
                mod standalone {
                    $(
                        #[tokio::test(flavor = "multi_thread")]
                        async fn $name() {
                            let server = super::TestRedis::start().await;
                            tokio::time::timeout(super::BOUND, super::$name(server))
                                .await
                                .expect(concat!(stringify!($name), " finished within the bound"));
                        }
                    )*
                }
                mod cluster {
                    $(
                        #[tokio::test(flavor = "multi_thread")]
                        async fn $name() {
                            let server = super::TestRedis::start_cluster().await;
                            tokio::time::timeout(super::BOUND, super::$name(server))
                                .await
                                .expect(concat!(stringify!($name), " finished within the bound"));
                        }
                    )*
                }
            };
        }

        in_both_modes!(
            a_code_minted_on_one_replica_is_exchanged_on_the_other_once,
            a_redis_failure_after_the_commit_revokes_and_answers_503,
            two_replicas_racing_one_code_leave_no_usable_token,
            an_expired_code_is_refused_on_either_replica,
            a_code_expiring_mid_exchange_keeps_its_tokens,
            a_replay_of_a_used_code_still_revokes_on_redis,
            a_redis_outage_fails_closed,
        );
    }
}
