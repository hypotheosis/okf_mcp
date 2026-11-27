//! OIDC sign-in and account linking (spec "OIDC", plan Decision 12).
//!
//! A sign-in starts at `GET /auth/oidc/{provider}/start` (the sign-in
//! page's button), which keeps the flow's `state`, `nonce` and PKCE
//! verifier on the server under a random id and puts that id in a
//! ten-minute `SameSite=Lax` cookie: the flow is bound to the browser that
//! started it, and the callback takes it out (single use). The callback
//! checks `state` itself (the crate does not), exchanges the code and
//! verifies the ID token, then finds the user by `(provider, sub)`, else by
//! a verified email (linking it), else refuses.
//!
//! Linking from the account page starts with `POST /api/v1/me/identities/
//! {provider}/link` (a console session and its CSRF header), which records
//! the session's hash in the flow (never its raw id); the callback links
//! only for a browser whose own live session hashes to it, so neither
//! another site nor another person's callback can link an identity to
//! someone's account.
//!
//! Deleting one's own account without a password or passkey (issue #775)
//! starts with `POST /api/v1/me/identities/{provider}/reauth`, a flow of its
//! own kind (`Flow::reauth`, `prompt=login` and `max_age=0`), bound to the
//! console session and to the identity the user has linked at that provider.
//! The same callback route finishes it, but only to keep a short-lived,
//! single-use [`ReauthProof`] for that session: it never signs in, creates a
//! session or links anything, and a sign-in callback never makes one.
//! `DELETE /api/v1/me` takes the proof (`take_reauth`) and
//! `users::delete_own` rechecks it under the user's row lock.
//!
//! With `[redis]` the flows live there (spec Decisions 3-8): a callback may
//! land on any instance, and the take is atomic, so a flow finishes once.
//! A Redis failure fails closed with a 503 page, never a fallback to memory.
//!
//! No secret reaches a log line: the callback's query has no `Debug`, the
//! request span records the path only (`http::router`), and the flow's
//! secrets redact themselves.

use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Path, Query, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::Deserialize;
use subtle::ConstantTimeEq;

use crate::app::App;
use crate::auth::{CredentialKind, Principal};
use crate::error::OkfError;
use crate::http::console_session::{
    append_all, client_ip, cookie, secure_cookies, session_user, set_cookie, ConsoleSession,
    SameSite, COOKIE,
};
use crate::http::limits::ip_key;
use crate::http::pages::safe_return_to;
use crate::http::signin::{
    audit, message_page, notify_method_added, start_in, ProviderButton, SignIn,
};
use crate::http::{timeout_layer, AppError};
use crate::keys;
use crate::oidc::{
    session_hash, Flow, Identity, LinkRequest, OidcError, OidcProviders, ReauthProof,
    ReauthRequest, FLOW_TTL,
};
use crate::store::identities::{self, OidcOutcome, OidcSignIn, SessionLink};
use crate::store::settings::Brand;
use crate::store::sign_in_events::{Method, Outcome};
use crate::store::user_sessions;
use crate::store::users::UserAuth;
use crate::telemetry::metrics::Limiter;
use crate::telemetry::subscriber::split_event;

/// The flow cookie: `SameSite=Lax`, so the provider's redirect carries it.
pub const FLOW_COOKIE: &str = "okf_oidc";
/// No user for this identity or verified email (403, spec).
pub const NO_ACCOUNT: &str = "No account for this email. Ask an admin to invite you.";
/// A bad state, nonce or token, or a replayed callback (400, spec).
pub const SIGN_IN_RETRY: &str = "Sign-in failed; try again";
/// A link callback without the session that started it (400).
pub const LINK_RETRY: &str = "Linking failed: sign in again and start it from your account page.";
/// A re-authentication callback that did not verify, or came from a browser
/// other than the one that started it (400).
pub const REAUTH_RETRY: &str =
    "Re-authentication failed: start it again from your account page, and sign in at your \
     provider when it asks.";
/// Discovery failed with nothing cached (503).
const UNAVAILABLE: &str = "The sign-in provider cannot be reached right now; try again later.";
/// The shared flow store failed (503): the flow is not finished, and its
/// cookie is kept so a reload can finish it.
const TRY_LATER: &str = "Sign-in is unavailable right now; try again shortly.";
/// The identity cannot be linked (409): another user holds it, or the user
/// already links another account at this provider.
pub const LINKED_ELSEWHERE: &str = "This account cannot be linked: it is linked to another user, \
     or another account at this provider is already linked to yours.";
/// Over the per-address sign-in limit (429).
const TOO_MANY: &str = "Too many attempts from this address. Wait a few minutes and try again.";
/// Over the per-user link limit (429).
const TOO_MANY_LINKS: &str = "Too many links started; wait a few minutes and try again.";
/// Where linking returns to.
const ACCOUNT_PAGE: &str = "/ui/#/account";
/// The pages' title.
const TITLE: &str = "Sign in";

/// The message for a provider id no configured provider has.
const NO_SUCH_PROVIDER: &str = "No such sign-in provider.";

/// The open routes, merged only when providers are configured.
///
/// # Returns
/// The router, with the request timeout.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "router builder: runs once at startup, outside any request, so a span would be an orphan root"
    )
)]
pub(crate) fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/auth/oidc/{provider}/start", get(start))
        .route("/auth/oidc/{provider}/callback", get(callback))
        .layer(timeout_layer())
}

/// The linking route, merged into `/api/v1` (behind `require_principal`,
/// which checks the CSRF header of a cookie write).
///
/// # Returns
/// The router.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "router builder: runs once at startup, outside any request, so a span would be an orphan root"
    )
)]
pub(crate) fn api_routes() -> Router<Arc<App>> {
    // `{provider}`: Task 10b's `/me/identities/{provider}` must use the same
    // parameter name at this position, or the router refuses to build.
    Router::new()
        .route("/me/identities/{provider}/link", post(link))
        .route(
            "/me/identities/{provider}/reauth",
            post(reauth_start).get(reauth_status),
        )
}

/// The sign-in page's provider buttons.
///
/// # Arguments
/// * `app` - the shared state.
///
/// # Returns
/// One button per configured provider, by id.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) fn buttons(app: &App) -> Vec<ProviderButton> {
    app.oidc().map_or_else(Vec::new, |oidc| {
        oidc.labels()
            .map(|(id, label)| ProviderButton {
                id: id.to_string(),
                label: label.to_string(),
            })
            .collect()
    })
}

/// The `Set-Cookie` header that clears the flow cookie.
#[tracing::instrument(level = "trace", skip_all)]
fn clear_flow(app: &App) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::SET_COOKIE,
        set_cookie(
            FLOW_COOKIE,
            "",
            std::time::Duration::ZERO,
            SameSite::Lax,
            secure_cookies(app),
        ),
    );
    headers
}

/// A flow to start.
struct NewFlow<'a> {
    /// The provider's id.
    provider: &'a str,
    /// Where to go afterwards.
    return_to: String,
    /// Set for a link from the account page.
    link: Option<LinkRequest>,
    /// Set for a re-authentication from the account page.
    reauth: Option<ReauthRequest>,
}

/// Starts a flow: the provider's authorize URL and the flow cookie. The
/// flow this browser held before is dropped, and a link replaces the one
/// its session started before ([`OidcProviders::store_flow`]).
///
/// # Errors
/// [`OkfError::NotFound`] for an unknown provider; [`OkfError::Busy`] when
/// discovery fails with nothing cached, too many flows are in progress or
/// Redis fails; [`OkfError::Io`] from the random source.
#[tracing::instrument(level = "debug", skip_all)]
async fn begin(
    app: &App,
    oidc: &OidcProviders,
    headers: &HeaderMap,
    new: NewFlow<'_>,
) -> Result<(String, HeaderMap), OkfError> {
    let fresh = new.reauth.is_some();
    let (url, secrets) = oidc
        .authorize_url(new.provider, Instant::now(), fresh)
        .await?;
    let id = keys::generate_with("")?;
    let flow = Flow {
        provider: new.provider.to_string(),
        secrets,
        return_to: new.return_to,
        link: new.link,
        reauth: new.reauth,
    };
    let previous = cookie(headers, FLOW_COOKIE);
    if !oidc.store_flow(&id, flow, previous, Instant::now()).await? {
        return Err(OkfError::Busy {
            reason: "too many sign-ins in progress; try again shortly".to_string(),
        });
    }
    let mut headers = HeaderMap::new();
    headers.insert(
        header::SET_COOKIE,
        set_cookie(
            FLOW_COOKIE,
            &id,
            FLOW_TTL,
            SameSite::Lax,
            secure_cookies(app),
        ),
    );
    Ok((url, headers))
}

/// `GET /auth/oidc/{provider}/start`'s query.
#[derive(Debug, Default, Deserialize)]
struct StartQuery {
    return_to: Option<String>,
}

/// `GET /auth/oidc/{provider}/start`: 303 to the provider. Each start
/// takes from the per-address sign-in limit, so no one address can fill
/// the flow map.
#[tracing::instrument(level = "debug", skip_all)]
async fn start(
    State(app): State<Arc<App>>,
    Path(provider): Path<String>,
    parts: Parts,
    Query(query): Query<StartQuery>,
) -> Response {
    let Some(oidc) = app.oidc() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let brand = app.settings().brand();
    if oidc.label(&provider).is_none() {
        return message_page(StatusCode::NOT_FOUND, TITLE, NO_SUCH_PROVIDER, &brand);
    }
    let ip = client_ip(&parts, &app.config().auth);
    if !app
        .sign_in_limits()
        .sign_in_per_ip
        .try_take(&ip_key(ip), Instant::now())
        .await
    {
        app.metrics().rate_limited(Limiter::SignInPerIp);
        return message_page(StatusCode::TOO_MANY_REQUESTS, TITLE, TOO_MANY, &brand);
    }
    let return_to = safe_return_to(query.return_to.as_deref());
    let new = NewFlow {
        provider: &provider,
        return_to,
        link: None,
        reauth: None,
    };
    match begin(&app, oidc, &parts.headers, new).await {
        Ok((url, headers)) => (headers, Redirect::to(&url)).into_response(),
        Err(OkfError::NotFound { .. }) => {
            message_page(StatusCode::NOT_FOUND, TITLE, NO_SUCH_PROVIDER, &brand)
        }
        Err(OkfError::Busy { .. }) => {
            message_page(StatusCode::SERVICE_UNAVAILABLE, TITLE, UNAVAILABLE, &brand)
        }
        Err(error) => AppError(error).into_response(),
    }
}

/// The provider's redirect back. No `Debug`: it holds the code.
#[derive(Deserialize)]
struct CallbackQuery {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

/// How a callback ends.
enum Done {
    /// Go to `to`, with the session cookie (empty when linking).
    Redirect {
        session_cookie: HeaderMap,
        to: String,
    },
    /// A page with this status and message.
    Refused(StatusCode, &'static str),
}

/// A message page that also clears the flow cookie.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
    )
)]
fn page(brand: &Brand, headers: &HeaderMap, status: StatusCode, message: &str) -> Response {
    let mut response = message_page(status, TITLE, message, brand);
    append_all(response.headers_mut(), headers);
    response
}

/// `GET /auth/oidc/{provider}/callback`.
#[tracing::instrument(level = "debug", skip_all)]
async fn callback(
    State(app): State<Arc<App>>,
    Path(provider): Path<String>,
    parts: Parts,
    Query(query): Query<CallbackQuery>,
) -> Response {
    let Some(oidc) = app.oidc() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let brand = app.settings().brand();
    let flow = match cookie(&parts.headers, FLOW_COOKIE) {
        Some(id) => oidc.flows.take(id, Instant::now()).await,
        None => Ok(None),
    };
    // A Redis failure fails closed. The take may never have run, so the flow
    // cookie is kept (no `Set-Cookie`): reloading this page once Redis is
    // back can still finish the flow.
    let Ok(flow) = flow else {
        return page(
            &brand,
            &HeaderMap::new(),
            StatusCode::SERVICE_UNAVAILABLE,
            TRY_LATER,
        );
    };
    // Every answer after the take clears the flow cookie: the flow is
    // single use, and it is gone now.
    let mut headers = clear_flow(&app);
    let Some(Flow {
        provider: started_with,
        secrets,
        return_to,
        link,
        reauth,
    }) = flow
    else {
        return page(&brand, &headers, StatusCode::BAD_REQUEST, SIGN_IN_RETRY);
    };
    // The `state` check is this redirect's CSRF guard; openidconnect leaves
    // it to the app. Constant time, like every other secret comparison.
    let state_ok = started_with == provider
        && query
            .state
            .as_deref()
            .is_some_and(|state| bool::from(state.as_bytes().ct_eq(secrets.state.as_bytes())));
    // A tuple pattern: the state matched, the provider sent no error, and a code.
    let (true, None, Some(code)) = (state_ok, query.error.as_deref(), query.code) else {
        return page(&brand, &headers, StatusCode::BAD_REQUEST, SIGN_IN_RETRY);
    };
    let ip = client_ip(&parts, &app.config().auth);
    // A re-authentication also needs a recent `auth_time` in the verified
    // ID token; who answered is checked first, in `finish_reauth`.
    let identified = match &reauth {
        Some(_) => {
            oidc.identify_fresh(&provider, code, secrets, Instant::now(), app.clock().now())
                .await
        }
        None => oidc
            .identify(&provider, code, secrets, Instant::now())
            .await
            .map(|identity| (identity, true)),
    };
    let (identity, fresh) = match identified {
        Ok(answer) => answer,
        Err(OidcError::Discovery { .. }) => {
            return page(
                &brand,
                &headers,
                StatusCode::SERVICE_UNAVAILABLE,
                UNAVAILABLE,
            )
        }
        Err(error) => {
            split_event!(
                info,
                { provider = %provider, },
                { %error, },
                "OIDC sign-in refused"
            );
            // A refused re-authentication is not a failed sign-in: it is
            // not recorded as one.
            if reauth.is_some() {
                return page(&brand, &headers, StatusCode::BAD_REQUEST, REAUTH_RETRY);
            }
            audit(&app, None, Method::Oidc, Outcome::Failed, ip).await;
            return page(&brand, &headers, StatusCode::BAD_REQUEST, SIGN_IN_RETRY);
        }
    };
    let done = match (link, reauth) {
        (Some(link), _) => finish_link(&app, &parts, &provider, &identity, link).await,
        (None, Some(reauth)) => {
            finish_reauth(&app, &parts, &provider, &identity, fresh, reauth).await
        }
        (None, None) => finish_sign_in(&app, &parts, &provider, &identity, return_to).await,
    };
    match done {
        Ok(Done::Redirect { session_cookie, to }) => {
            append_all(&mut headers, &session_cookie);
            (headers, Redirect::to(&to)).into_response()
        }
        Ok(Done::Refused(status, message)) => page(&brand, &headers, status, message),
        Err(error) => {
            let mut response = AppError(error).into_response();
            append_all(response.headers_mut(), &headers);
            response
        }
    }
}

/// What to call a provider in the method-added notice.
#[tracing::instrument(level = "trace", skip_all)]
fn method_text(app: &App, provider: &str) -> String {
    let label = app
        .oidc()
        .and_then(|oidc| oidc.label(provider))
        .unwrap_or(provider);
    format!("your {label} account")
}

/// The identity's email, trimmed, when it may be used to find an account:
/// sent, and verified by the provider.
#[tracing::instrument(level = "trace", skip_all)]
fn verified_email(identity: &Identity) -> Option<&str> {
    identity
        .email
        .as_deref()
        .map(str::trim)
        .filter(|_| identity.email_verified)
}

/// Signs in by `(provider, sub)`, else links by verified email and signs
/// in, else refuses with 403 (spec, in that order). An identity already
/// linked always signs in as its own user, whatever email it now carries,
/// so a changed email at the provider never reaches another account.
/// Everything is one transaction ([`complete_sign_in`]); after its commit
/// only the response is built.
#[tracing::instrument(level = "debug", skip_all)]
async fn finish_sign_in(
    app: &App,
    parts: &Parts,
    provider: &str,
    identity: &Identity,
    return_to: String,
) -> Result<Done, OkfError> {
    let ip = client_ip(parts, &app.config().auth);
    let request = OidcSignIn {
        provider,
        subject: &identity.subject,
        // Only a verified email may claim an account.
        verified_email: verified_email(identity),
    };
    let session_cookie = match complete_sign_in(app, parts, &request).await {
        Ok(Some(cookie)) => cookie,
        // Nothing was committed on these paths, so the audit may wait.
        Ok(None) => {
            audit(app, None, Method::Oidc, Outcome::Failed, ip).await;
            return Ok(Done::Refused(StatusCode::FORBIDDEN, NO_ACCOUNT));
        }
        Err(OkfError::Unauthenticated { .. }) => {
            audit(app, None, Method::Oidc, Outcome::Failed, ip).await;
            return Ok(Done::Refused(StatusCode::BAD_REQUEST, SIGN_IN_RETRY));
        }
        Err(OkfError::Refused { .. }) => {
            return Ok(Done::Refused(StatusCode::CONFLICT, LINKED_ELSEWHERE))
        }
        Err(error) => return Err(error),
    };
    Ok(Done::Redirect {
        session_cookie,
        to: return_to,
    })
}

/// One OIDC sign-in transaction: `identities::sign_in_in` (the user locked,
/// the link re-read or made, the identity stamped), then the session, the
/// end of the browser's old one and the event (`signin::start_in`), then
/// the notice of a link made just now, then the commit.
///
/// # Returns
/// The session's `Set-Cookie` header, or `None` for no account (nothing
/// changed).
///
/// # Errors
/// [`OkfError::Unauthenticated`] for an identity unlinked (or a user
/// deleted) meanwhile; [`OkfError::Refused`] as `identities::sign_in_in`;
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all)]
async fn complete_sign_in(
    app: &App,
    parts: &Parts,
    request: &OidcSignIn<'_>,
) -> Result<Option<HeaderMap>, OkfError> {
    let mut tx = app.accounts().executor().begin().await?;
    // Returning early drops `tx`, which rolls back.
    let (user, linked) = match identities::sign_in_in(&mut tx, request, app.clock().now()).await? {
        OidcOutcome::SignedIn { user, linked } => (user, linked),
        OidcOutcome::NoAccount => return Ok(None),
        OidcOutcome::Unlinked => {
            return Err(OkfError::Unauthenticated {
                reason: "the identity was unlinked meanwhile".to_string(),
            })
        }
    };
    let signing_in = SignIn {
        user: &user,
        method: Method::Oidc,
        password: None,
    };
    let cookie = start_in(app, &mut tx, parts, &signing_in).await?;
    if linked {
        tracing::info!(user = %user.id, provider = request.provider, "OIDC identity linked by verified email");
        let method = method_text(app, request.provider);
        notify_method_added(app, &mut tx, &user, &method).await?;
    }
    tx.commit().await?;
    Ok(Some(cookie))
}

/// The user of this browser's console session and that session's raw id,
/// when it is the live session that started a flow: its id hashes to the
/// flow's `session_hash` (compared in constant time), and it is the same
/// user. The id returned is this request's own cookie, never a stored one.
///
/// # Arguments
/// * `app` - the application.
/// * `parts` - the callback request.
/// * `user_id` - the user the flow was started for.
/// * `started_in` - the flow's [`session_hash`].
#[tracing::instrument(level = "debug", skip_all)]
async fn flow_user<'a>(
    app: &App,
    parts: &'a Parts,
    user_id: &str,
    started_in: &str,
) -> Result<Option<(UserAuth, &'a str)>, OkfError> {
    let Some(id) = cookie(&parts.headers, COOKIE) else {
        return Ok(None);
    };
    if !bool::from(session_hash(id).as_bytes().ct_eq(started_in.as_bytes())) {
        return Ok(None);
    }
    let idle = app.settings().console_session_idle;
    match user_sessions::touch(app.accounts().executor(), id, idle, app.clock().now()).await? {
        Some(row) if row.user_id == user_id => Ok(session_user(app, &row.user_id)
            .await?
            .map(|user| (user, id))),
        _ => Ok(None),
    }
}

/// The user of this browser's console session and its raw id, when it is
/// the live session that started the link ([`flow_user`]).
#[tracing::instrument(level = "debug", skip_all)]
async fn linking_user<'a>(
    app: &App,
    parts: &'a Parts,
    link: &LinkRequest,
) -> Result<Option<(UserAuth, &'a str)>, OkfError> {
    flow_user(app, parts, &link.user_id, &link.session_hash).await
}

/// Finishes a re-authentication: keeps a [`ReauthProof`] for the session
/// that started it, and goes back to the account page. Nothing else: no
/// session is created, no identity linked, no sign-in recorded. It keeps the
/// proof only when:
/// - this browser's own live session is the one that started the flow
///   ([`flow_user`]); and
/// - the verified ID token's subject is exactly the identity the flow was
///   started for (another identity, even one linked to the same user,
///   proves nothing), whether or not its `auth_time` is fresh.
///
/// A right identity with a stale `auth_time` keeps a stale marker instead,
/// which makes the deletion answer `ReauthStale`.
///
/// What the deletion needs is checked again, under the user's row lock, by
/// `users::delete_own`.
#[tracing::instrument(level = "debug", skip_all)]
async fn finish_reauth(
    app: &Arc<App>,
    parts: &Parts,
    provider: &str,
    identity: &Identity,
    fresh: bool,
    reauth: ReauthRequest,
) -> Result<Done, OkfError> {
    let refused = || Ok(Done::Refused(StatusCode::BAD_REQUEST, REAUTH_RETRY));
    let Some(oidc) = app.oidc() else {
        return refused();
    };
    let Some((_, _)) = flow_user(app, parts, &reauth.user_id, &reauth.session_hash).await? else {
        return refused();
    };
    // Who answered comes before how fresh it is: another identity gets the
    // generic refusal and leaves nothing behind, so a wrong account is never
    // told that signing in again would help.
    let same_identity = bool::from(identity.subject.as_bytes().ct_eq(reauth.subject.as_bytes()));
    if !same_identity {
        split_event!(
            info,
            { provider = %provider, },
            { user = %reauth.user_id, },
            "OIDC re-authentication refused: another identity answered"
        );
        return refused();
    }
    let proof = ReauthProof {
        user_id: reauth.user_id,
        provider: provider.to_string(),
        // A stale outcome keeps no identity, only the fact that it was stale.
        subject: if fresh { reauth.subject } else { String::new() },
        session_hash: reauth.session_hash,
        stale: !fresh,
    };
    if !oidc.store_reauth_proof(proof, Instant::now()).await? {
        return Err(OkfError::Busy {
            reason: "too many sign-ins in progress; try again shortly".to_string(),
        });
    }
    Ok(Done::Redirect {
        session_cookie: HeaderMap::new(),
        to: ACCOUNT_PAGE.to_string(),
    })
}

/// Takes this session's finished re-authentication for `DELETE /api/v1/me`
/// (single use). The caller passes it to `users::delete_own` as
/// `Proof::Oidc`, which checks again, under the user's row lock, that the
/// identity is still linked to the user.
///
/// # Arguments
/// * `app` - the application.
/// * `session` - the request's console session.
/// * `user_id` - the session's user.
///
/// # Errors
/// [`OkfError::NotFound`] with OIDC off; [`OkfError::Gone`] when this
/// session has no finished re-authentication, or it expired or was used;
/// [`OkfError::Forbidden`] when it belongs to another user or session;
/// [`OkfError::ReauthStale`] when the provider did not vouch for a fresh
/// sign-in (no `auth_time`, or an old one);
/// [`OkfError::Busy`] when Redis fails.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn take_reauth(
    app: &App,
    session: &ConsoleSession,
    user_id: &str,
) -> Result<ReauthProof, OkfError> {
    let oidc = app.oidc().ok_or_else(|| OkfError::NotFound {
        path: "auth/oidc".to_string(),
    })?;
    let hash = session_hash(&session.id);
    let proof = oidc
        .take_reauth_proof(&hash, Instant::now())
        .await?
        .ok_or_else(|| OkfError::Gone {
            reason: "no provider re-authentication is waiting for this session, or it expired; \
                     re-authenticate again"
                .to_string(),
        })?;
    judge_reauth(oidc, proof, user_id, &hash)
}

/// What a finished re-authentication is worth to this session: the one
/// rule `DELETE /api/v1/me` ([`take_reauth`]) and the read-only status call
/// ([`reauth_status`]) both apply, so they can never disagree. (The
/// `auth_time` window itself is judged once, when the provider answers, in
/// `Oidc::identify_fresh`; its verdict is the proof's `stale` flag.)
///
/// # Errors
/// [`OkfError::Forbidden`] when the proof belongs to another user or
/// session; [`OkfError::ReauthStale`] when the provider did not vouch for a
/// fresh sign-in (no `auth_time`, an old one, or one in the future).
#[tracing::instrument(level = "debug", skip_all)]
fn judge_reauth(
    oidc: &OidcProviders,
    proof: ReauthProof,
    user_id: &str,
    hash: &str,
) -> Result<ReauthProof, OkfError> {
    // The key was this session's hash already; both are checked anyway, in
    // constant time, so a proof stored under the wrong key could not pass.
    let same_session = bool::from(proof.session_hash.as_bytes().ct_eq(hash.as_bytes()));
    if proof.user_id != user_id || !same_session {
        return Err(OkfError::Forbidden {
            reason: "this re-authentication belongs to another session".to_string(),
        });
    }
    if proof.stale {
        // Only the label: never the issuer, the subject or token details.
        let provider = oidc
            .label(&proof.provider)
            .unwrap_or("Your identity provider");
        return Err(OkfError::ReauthStale {
            provider: provider.to_string(),
        });
    }
    Ok(proof)
}

/// Links the identity to the signed-in user who started the flow, whatever
/// its email (spec), and ends their other sessions unless kept, in one
/// transaction. Only the browser still holding the session that started
/// the link may finish it. After the commit only the notice runs, detached.
#[tracing::instrument(level = "debug", skip_all)]
async fn finish_link(
    app: &Arc<App>,
    parts: &Parts,
    provider: &str,
    identity: &Identity,
    link: LinkRequest,
) -> Result<Done, OkfError> {
    let Some((user, session_id)) = linking_user(app, parts, &link).await? else {
        return Ok(Done::Refused(StatusCode::BAD_REQUEST, LINK_RETRY));
    };
    let email = identity.email.as_deref().map(str::trim).unwrap_or_default();
    let request = SessionLink {
        user_id: &link.user_id,
        // This request's own session, which hashed to the flow's.
        session_id,
        provider,
        subject: &identity.subject,
        email,
        keep_sessions: link.keep_sessions,
    };
    let ended = match link_in_one_transaction(app, &user, &request).await {
        Ok(ended) => ended,
        Err(OkfError::Refused { .. }) => {
            return Ok(Done::Refused(StatusCode::CONFLICT, LINKED_ELSEWHERE))
        }
        // The session ended between the check above and the link.
        Err(OkfError::Unauthenticated { .. }) => {
            return Ok(Done::Refused(StatusCode::BAD_REQUEST, LINK_RETRY))
        }
        Err(error) => return Err(error),
    };
    tracing::info!(user = %link.user_id, provider, ended, "OIDC identity linked from the account page");
    // The notice was queued in the link's transaction: nothing runs after
    // its commit but building the response.
    Ok(Done::Redirect {
        session_cookie: HeaderMap::new(),
        to: ACCOUNT_PAGE.to_string(),
    })
}

/// Links the identity to the session's user and queues the notice, in one
/// transaction (`identities::link_for_session_in`).
///
/// # Returns
/// How many other sessions ended.
///
/// # Errors
/// As `identities::link_for_session_in`.
#[tracing::instrument(level = "debug", skip_all)]
async fn link_in_one_transaction(
    app: &App,
    user: &UserAuth,
    request: &SessionLink<'_>,
) -> Result<u64, OkfError> {
    let mut tx = app.accounts().executor().begin().await?;
    let ended = identities::link_for_session_in(&mut tx, request, app.clock().now()).await?;
    let method = method_text(app, request.provider);
    notify_method_added(app, &mut tx, user, &method).await?;
    tx.commit().await?;
    Ok(ended)
}

/// `POST /me/identities/{provider}/link`'s body.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct LinkBody {
    /// Keep the user's other sessions (the account page's checkbox).
    #[serde(default)]
    keep_sessions: bool,
}

/// `POST /api/v1/me/identities/{provider}/link`: `{"url": <authorize URL>}`
/// and the flow cookie. The console navigates to the URL itself, so the
/// console CSP's `form-action 'self'` never meets the provider's origin.
///
/// # Errors
/// 403 for a key or grant; 404 for an unknown provider or with OIDC off;
/// 429 once the user's allowance is spent (a burst of
/// [`crate::oidc::LINK_STARTS_PER_USER`], refilled one per minute);
/// 503 when the provider cannot be reached.
#[tracing::instrument(level = "debug", skip_all)]
async fn link(
    State(app): State<Arc<App>>,
    Path(provider): Path<String>,
    Extension(principal): Extension<Arc<Principal>>,
    session: Option<Extension<ConsoleSession>>,
    headers: HeaderMap,
    Json(body): Json<LinkBody>,
) -> Result<Response, AppError> {
    let (user_id, session) = match (&principal.kind, session) {
        (CredentialKind::Session { user_id }, Some(Extension(session))) => {
            (user_id.clone(), session)
        }
        _ => {
            return Err(OkfError::Forbidden {
                reason: "linking an account needs a console session".to_string(),
            }
            .into())
        }
    };
    let oidc = app.oidc().ok_or_else(|| OkfError::NotFound {
        path: format!("me/identities/{provider}/link"),
    })?;
    if oidc.label(&provider).is_none() {
        return Err(OkfError::NotFound {
            path: format!("me/identities/{provider}/link"),
        }
        .into());
    }
    // A signed-in user is not limited by address, so their own allowance
    // keeps them from filling the flow map every sign-in shares.
    if !oidc.may_start_link(&user_id, Instant::now()).await {
        app.metrics().rate_limited(Limiter::LinkStarts);
        let body = serde_json::json!({
            "error": "TooManyRequests",
            "message": TOO_MANY_LINKS,
        });
        return Ok((StatusCode::TOO_MANY_REQUESTS, Json(body)).into_response());
    }
    let new = NewFlow {
        provider: &provider,
        return_to: ACCOUNT_PAGE.to_string(),
        link: Some(LinkRequest {
            user_id,
            session_hash: session_hash(&session.id),
            keep_sessions: body.keep_sessions,
        }),
        reauth: None,
    };
    let (url, flow_cookie) = begin(&app, oidc, &headers, new).await?;
    Ok((flow_cookie, Json(serde_json::json!({ "url": url }))).into_response())
}

/// The user and console session a re-authentication call is bound to.
///
/// # Errors
/// [`OkfError::Forbidden`] for a key, a grant, or no session at all.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
    )
)]
fn reauth_session(
    principal: &Principal,
    session: Option<Extension<ConsoleSession>>,
) -> Result<(String, ConsoleSession), OkfError> {
    match (&principal.kind, session) {
        (CredentialKind::Session { user_id }, Some(Extension(session))) => {
            Ok((user_id.clone(), session))
        }
        _ => Err(OkfError::Forbidden {
            reason: "re-authenticating needs a console session".to_string(),
        }),
    }
}

/// `POST /api/v1/me/identities/{provider}/reauth`: `{"url": <authorize
/// URL>}` and the flow cookie, to re-authenticate at the provider the user
/// has linked (`prompt=login`, `max_age=0`) before deleting their account
/// with `DELETE /api/v1/me` and `{"oidc_reauth": true}`. The flow is bound
/// to this console session and to the identity linked there now; its
/// callback keeps a proof for the session and does nothing else.
///
/// # Errors
/// 403 for a key or grant; 404 for an unknown provider, with OIDC off, or
/// when the user has no identity linked at that provider; 429 once the
/// user's allowance is spent (shared with linking); 503 when the provider
/// cannot be reached.
#[tracing::instrument(level = "debug", skip_all)]
async fn reauth_start(
    State(app): State<Arc<App>>,
    Path(provider): Path<String>,
    Extension(principal): Extension<Arc<Principal>>,
    session: Option<Extension<ConsoleSession>>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let (user_id, session) = reauth_session(&principal, session)?;
    let not_found = || OkfError::NotFound {
        path: format!("me/identities/{provider}/reauth"),
    };
    let oidc = app.oidc().ok_or_else(not_found)?;
    if oidc.label(&provider).is_none() {
        return Err(not_found().into());
    }
    // The identity this person linked at the provider: the one whose
    // return the callback accepts.
    let subject = identities::subject_of(app.accounts().executor(), &user_id, &provider)
        .await?
        .ok_or_else(not_found)?;
    if !oidc.may_start_link(&user_id, Instant::now()).await {
        app.metrics().rate_limited(Limiter::LinkStarts);
        let body = serde_json::json!({
            "error": "TooManyRequests",
            "message": TOO_MANY_LINKS,
        });
        return Ok((StatusCode::TOO_MANY_REQUESTS, Json(body)).into_response());
    }
    let new = NewFlow {
        provider: &provider,
        return_to: ACCOUNT_PAGE.to_string(),
        link: None,
        reauth: Some(ReauthRequest {
            user_id,
            session_hash: session_hash(&session.id),
            subject,
        }),
    };
    let (url, flow_cookie) = begin(&app, oidc, &headers, new).await?;
    Ok((flow_cookie, Json(serde_json::json!({ "url": url }))).into_response())
}

/// `GET /api/v1/me/identities/{provider}/reauth`: `{"state": ...}`, what
/// `DELETE /api/v1/me` with `{"oidc_reauth": true}` would find now, so the
/// console can tell the person at once, on their return from the provider,
/// that signing in again is needed. One of:
/// - `none`: no finished re-authentication waits for this session at this
///   provider (never started, expired, already used, or another session's);
/// - `ready`: a fresh one waits;
/// - `stale`: one waits, but the provider did not vouch for a fresh sign-in
///   (no `auth_time`, an old one, or one in the future), the very case the
///   deletion answers `ReauthStale` for ([`judge_reauth`] is the one rule).
///
/// Read-only: the proof is not taken, nothing is written and no lifetime
/// is renewed. The body holds the state and nothing else: no subject,
/// issuer or time.
///
/// # Errors
/// 403 for a key, a grant or no console session; 404 for an unknown
/// provider or with OIDC off; 503 when Redis fails.
#[tracing::instrument(level = "debug", skip_all)]
async fn reauth_status(
    State(app): State<Arc<App>>,
    Path(provider): Path<String>,
    Extension(principal): Extension<Arc<Principal>>,
    session: Option<Extension<ConsoleSession>>,
) -> Result<Response, AppError> {
    let (user_id, session) = reauth_session(&principal, session)?;
    let not_found = || OkfError::NotFound {
        path: format!("me/identities/{provider}/reauth"),
    };
    let oidc = app.oidc().ok_or_else(not_found)?;
    if oidc.label(&provider).is_none() {
        return Err(not_found().into());
    }
    let hash = session_hash(&session.id);
    let proof = oidc
        .peek_reauth_proof(&hash, Instant::now())
        .await?
        // A proof for another provider is not an answer about this one.
        .filter(|proof| proof.provider == provider);
    let state = match proof {
        None => "none",
        Some(proof) => match judge_reauth(oidc, proof, &user_id, &hash) {
            Ok(_) => "ready",
            Err(OkfError::ReauthStale { .. }) => "stale",
            Err(other) => return Err(other.into()),
        },
    };
    Ok(Json(serde_json::json!({ "state": state })).into_response())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use axum::http::StatusCode;
    use chrono::Utc;
    use serde_json::json;
    use sqlx::PgPool;

    use super::{
        FLOW_COOKIE, LINKED_ELSEWHERE, LINK_RETRY, NO_ACCOUNT, REAUTH_RETRY, SIGN_IN_RETRY,
        TRY_LATER,
    };
    use crate::app::App;
    use crate::config::{AuthConfig, Config};
    use crate::db::redis_support::TestRedis;
    use crate::http::api::testing::{
        keys, login_config, user_with_password, Browser, Payload, Reply, MAIN, PASSWORD,
    };
    use crate::http::console_session::COOKIE;
    use crate::http::limits::SIGN_IN_PER_IP;
    use crate::mcp::testing::{app_with, replica_pair_on, TestApp};
    use crate::oidc::testing::{self as idp, AuthTime, MockIdp, CLIENT_ID, SECRET};
    use crate::oidc::{LINK_STARTS_PER_USER, REAUTH_AUTH_TIME_WINDOW, REAUTH_PROOF_TTL};
    use crate::store::audit::AuditActor;
    use crate::store::identities;
    use crate::store::sign_in_events::{self, Method, Outcome};
    use crate::store::users::AdminChange;
    use crate::store::{passwords, users};

    /// An app with the mock provider as `example`. The client secret comes
    /// from the environment, as in production; each test names its own
    /// variable, so tests running at once never share one.
    async fn oidc_app(idp: &MockIdp, secret_env: &str) -> TestApp {
        app_with(
            &[("main", MAIN), ("other", &[("index.md", "# Other\n")])],
            oidc_config(idp, secret_env),
        )
        .await
    }

    /// The settings with the mock provider as `example`, its secret set in
    /// `secret_env`.
    fn oidc_config(idp: &MockIdp, secret_env: &str) -> Config {
        // Edition 2021: `set_var` is a safe call.
        std::env::set_var(secret_env, SECRET);
        Config {
            auth: AuthConfig {
                oidc: vec![idp::provider(idp, secret_env)],
                ..AuthConfig::default()
            },
            ..login_config()
        }
    }

    /// Two replicas over one database and `redis`, with the mock provider.
    async fn oidc_pair(
        idp: &MockIdp,
        secret_env: &str,
        redis: TestRedis,
    ) -> (TestApp, Arc<App>, TestRedis) {
        replica_pair_on(&[], oidc_config(idp, secret_env), redis).await
    }

    fn query_of(url: &str) -> HashMap<String, String> {
        url::Url::parse(url)
            .expect("an absolute URL")
            .query_pairs()
            .into_owned()
            .collect()
    }

    /// Starts a sign-in and returns the authorize URL's query. The mock is
    /// told the nonce, as its authorize page would learn it.
    async fn start(
        test: &TestApp,
        idp: &MockIdp,
        browser: &mut Browser,
    ) -> HashMap<String, String> {
        start_on(&test.app, idp, browser).await
    }

    /// [`start`] on one replica.
    async fn start_on(
        app: &Arc<App>,
        idp: &MockIdp,
        browser: &mut Browser,
    ) -> HashMap<String, String> {
        let reply = browser
            .send_to(
                app,
                "GET",
                "/auth/oidc/example/start?return_to=%2Fui%2F%23%2Fkeys",
                Payload::Empty,
            )
            .await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
        let cookie = reply.set_cookie(FLOW_COOKIE).expect("a flow cookie");
        assert!(
            cookie.contains("SameSite=Lax") && cookie.contains("Max-Age=600"),
            "{cookie}"
        );
        let query = query_of(reply.location());
        idp.set_nonce(&query["nonce"]);
        query
    }

    async fn callback(test: &TestApp, browser: &mut Browser, state: &str) -> Reply {
        callback_on(&test.app, browser, state).await
    }

    /// [`callback`] on one replica.
    async fn callback_on(app: &Arc<App>, browser: &mut Browser, state: &str) -> Reply {
        let uri = format!("/auth/oidc/example/callback?code=c&state={state}");
        browser.send_to(app, "GET", &uri, Payload::Empty).await
    }

    /// Starts linking from `browser`'s session and returns the authorize
    /// URL's query.
    async fn start_link(
        test: &TestApp,
        idp: &MockIdp,
        browser: &mut Browser,
        body: serde_json::Value,
    ) -> HashMap<String, String> {
        start_link_on(&test.app, idp, browser, body).await
    }

    /// [`start_link`] on one replica.
    async fn start_link_on(
        app: &Arc<App>,
        idp: &MockIdp,
        browser: &mut Browser,
        body: serde_json::Value,
    ) -> HashMap<String, String> {
        let started = browser
            .send_to(
                app,
                "POST",
                "/api/v1/me/identities/example/link",
                Payload::Json(body),
            )
            .await;
        assert_eq!(started.status, StatusCode::OK, "{}", started.text());
        let query = query_of(started.json()["url"].as_str().expect("a URL"));
        idp.set_nonce(&query["nonce"]);
        query
    }

    async fn queued_mail(test: &TestApp) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM apalis.jobs")
            .fetch_one(test.app.accounts().executor())
            .await
            .expect("count")
    }

    async fn user_id(test: &TestApp) -> String {
        sqlx::query_scalar("SELECT id FROM users WHERE name = 'alice'")
            .fetch_one(test.app.accounts().executor())
            .await
            .expect("alice's id")
    }

    /// Waits (up to 10 s) until `want` mails are queued: the method-added
    /// notice is queued on a task of its own, after the response.
    async fn wait_for_mail(test: &TestApp, want: i64) {
        for _ in 0..200 {
            if queued_mail(test).await == want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("{want} mails were never queued");
    }

    async fn sessions_of(pool: &PgPool, user_id: &str) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM user_sessions WHERE user_id = $1")
            .bind(user_id)
            .fetch_one(pool)
            .await
            .expect("count the sessions")
    }

    /// Waits (up to 10 s) until `want` backends of this database wait on a
    /// lock.
    async fn wait_for_lock_waiters(pool: &PgPool, want: i64) {
        for _ in 0..200 {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity \
                  WHERE datname = current_database() AND wait_event_type = 'Lock'",
            )
            .fetch_one(pool)
            .await
            .expect("count the lock waiters");
            if waiting >= want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("{want} backends never waited on a lock");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_start_redirect_carries_pkce_state_nonce_and_the_redirect_uri() {
        let idp = idp::start().await;
        let test = oidc_app(&idp, "OKF_TEST_OIDC_SECRET_START").await;

        let query = start(&test, &idp, &mut Browser::default()).await;

        assert_eq!(query["client_id"], CLIENT_ID);
        assert_eq!(query["response_type"], "code");
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(
            query["redirect_uri"],
            "https://okf.example.com/auth/oidc/example/callback"
        );
        assert_eq!(query["scope"], "openid email");
        assert!(query["state"].len() >= 22 && query["nonce"].len() >= 22);
        assert!(query["code_challenge"].len() >= 43);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_verified_email_links_the_identity_and_signs_in() {
        let idp = idp::start().await;
        let test = oidc_app(&idp, "OKF_TEST_OIDC_SECRET_LINK").await;
        let alice = user_with_password(&test, "alice", "alice@example.com", Some(PASSWORD)).await;
        idp.will_issue("sub-1", Some("Alice@Example.com"), Some(true));
        let mut browser = Browser::default();

        let query = start(&test, &idp, &mut browser).await;
        let reply = callback(&test, &mut browser, &query["state"]).await;

        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
        assert_eq!(reply.location(), "/ui/#/keys");
        assert!(browser.cookie(COOKIE).is_some());
        assert_eq!(browser.cookie(FLOW_COOKIE), None);
        let linked = identities::list(test.app.accounts().executor(), &alice.id)
            .await
            .expect("list");
        assert_eq!(linked.len(), 1);
        assert_eq!(linked[0].email_at_link, "Alice@Example.com");
        assert!(linked[0].last_used_at.is_some());
        wait_for_mail(&test, 1).await;
        let events = sign_in_events::recent(test.app.accounts().executor(), "alice", 5)
            .await
            .expect("events");
        assert!(matches!(
            (&events[0].method, &events[0].outcome),
            (Method::Oidc, Outcome::Ok)
        ));
        let session = browser
            .send(&test, "GET", "/api/v1/session", Payload::Empty)
            .await;
        assert_eq!(session.json()["user"], "alice");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unverified_or_unknown_email_is_refused_with_403() {
        let idp = idp::start().await;
        let test = oidc_app(&idp, "OKF_TEST_OIDC_SECRET_REFUSE").await;
        let alice = user_with_password(&test, "alice", "alice@example.com", Some(PASSWORD)).await;

        for (subject, email, verified) in [
            ("sub-2", Some("alice@example.com"), Some(false)),
            ("sub-3", Some("alice@example.com"), None),
            ("sub-4", Some("nobody@example.com"), Some(true)),
            ("sub-5", None, Some(true)),
            ("sub-6", Some("alice\0@example.com"), Some(true)),
        ] {
            idp.will_issue(subject, email, verified);
            let mut browser = Browser::default();
            let query = start(&test, &idp, &mut browser).await;
            let reply = callback(&test, &mut browser, &query["state"]).await;
            assert_eq!(reply.status, StatusCode::FORBIDDEN, "{subject}");
            assert!(reply.text().contains(NO_ACCOUNT), "{subject}");
            assert_eq!(browser.cookie(COOKIE), None);
        }
        assert!(identities::list(test.app.accounts().executor(), &alice.id)
            .await
            .expect("list")
            .is_empty());
        let events = sign_in_events::recent(test.app.accounts().executor(), "alice", 5)
            .await
            .expect("events");
        assert!(events.is_empty(), "refusals name no user");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_returning_subject_signs_in_after_its_email_changed() {
        let idp = idp::start().await;
        let test = oidc_app(&idp, "OKF_TEST_OIDC_SECRET_RETURN").await;
        user_with_password(&test, "alice", "alice@example.com", Some(PASSWORD)).await;
        let bob = user_with_password(&test, "bob", "bob@example.com", Some(PASSWORD)).await;
        idp.will_issue("sub-1", Some("alice@example.com"), Some(true));
        let mut first = Browser::default();
        let query = start(&test, &idp, &mut first).await;
        callback(&test, &mut first, &query["state"]).await;

        for email in ["alice@elsewhere.example", "bob@example.com"] {
            // Now verified as bob's address: `(provider, sub)` still wins.
            idp.will_issue("sub-1", Some(email), Some(email.starts_with("bob")));
            let mut browser = Browser::default();
            let query = start(&test, &idp, &mut browser).await;
            let reply = callback(&test, &mut browser, &query["state"]).await;

            assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
            let session = browser
                .send(&test, "GET", "/api/v1/session", Payload::Empty)
                .await;
            assert_eq!(session.json()["user"], "alice", "{email}");
        }
        assert!(identities::list(test.app.accounts().executor(), &bob.id)
            .await
            .expect("list")
            .is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_wrong_state_a_wrong_nonce_a_provider_error_or_a_replay_is_400() {
        let idp = idp::start().await;
        let test = oidc_app(&idp, "OKF_TEST_OIDC_SECRET_BAD").await;
        user_with_password(&test, "alice", "alice@example.com", Some(PASSWORD)).await;
        idp.will_issue("sub-1", Some("alice@example.com"), Some(true));

        let mut wrong_state = Browser::default();
        start(&test, &idp, &mut wrong_state).await;
        let wrong_state = callback(&test, &mut wrong_state, "not-the-state").await;

        let mut wrong_nonce = Browser::default();
        let query = start(&test, &idp, &mut wrong_nonce).await;
        idp.set_nonce("not-the-nonce");
        let wrong_nonce = callback(&test, &mut wrong_nonce, &query["state"]).await;

        let mut denied = Browser::default();
        let query = start(&test, &idp, &mut denied).await;
        let uri = format!(
            "/auth/oidc/example/callback?error=access_denied&state={}",
            query["state"]
        );
        let denied = denied.send(&test, "GET", &uri, Payload::Empty).await;

        // Another browser presenting a valid state, without the flow cookie.
        let mut started = Browser::default();
        let query = start(&test, &idp, &mut started).await;
        let elsewhere = callback(&test, &mut Browser::default(), &query["state"]).await;

        let mut replayed = Browser::default();
        let query = start(&test, &idp, &mut replayed).await;
        let flow = replayed.cookie(FLOW_COOKIE).expect("a flow").to_string();
        let first = callback(&test, &mut replayed, &query["state"]).await;
        let again = callback(
            &test,
            &mut Browser::with_cookie(FLOW_COOKIE, &flow),
            &query["state"],
        )
        .await;

        for (what, reply) in [
            ("state", &wrong_state),
            ("nonce", &wrong_nonce),
            ("error", &denied),
            ("another browser", &elsewhere),
            ("replay", &again),
        ] {
            assert_eq!(
                reply.status,
                StatusCode::BAD_REQUEST,
                "{what}: {}",
                reply.text()
            );
            assert!(reply.text().contains(SIGN_IN_RETRY), "{what}");
            assert!(reply.set_cookie(COOKIE).is_none(), "{what}");
        }
        assert_eq!(first.status, StatusCode::SEE_OTHER);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_token_from_another_issuer_for_another_client_or_expired_is_400() {
        let idp = idp::start().await;
        let test = oidc_app(&idp, "OKF_TEST_OIDC_SECRET_CLAIMS").await;
        user_with_password(&test, "alice", "alice@example.com", Some(PASSWORD)).await;
        idp.will_issue("sub-1", Some("alice@example.com"), Some(true));

        for (what, issuer, audience, expired) in [
            ("issuer", Some("https://id.example.com"), None, false),
            ("audience", None, Some("another-client"), false),
            ("expiry", None, None, true),
        ] {
            idp.will_sign_badly(issuer, audience, expired);
            let mut browser = Browser::default();
            let query = start(&test, &idp, &mut browser).await;
            let reply = callback(&test, &mut browser, &query["state"]).await;

            assert_eq!(
                reply.status,
                StatusCode::BAD_REQUEST,
                "{what}: {}",
                reply.text()
            );
            assert!(reply.set_cookie(COOKIE).is_none(), "{what}");
        }
        idp.will_sign_badly(None, None, false);
        let mut browser = Browser::default();
        let query = start(&test, &idp, &mut browser).await;
        let good = callback(&test, &mut browser, &query["state"]).await;
        assert_eq!(
            good.status,
            StatusCode::SEE_OTHER,
            "a good token still works"
        );
        assert_eq!(
            sessions_of(test.app.accounts().executor(), &user_id(&test).await).await,
            1
        );
    }

    /// `POST /api/v1/me/identities/example/link` with an empty body.
    async fn post_link(test: &TestApp, browser: &mut Browser) -> Reply {
        browser
            .send(
                test,
                "POST",
                "/api/v1/me/identities/example/link",
                Payload::Json(json!({})),
            )
            .await
    }

    /// The number of flows in progress.
    fn flows(test: &TestApp) -> usize {
        test.app.oidc().expect("OIDC is on").flows.len()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn past_the_address_limit_a_start_is_429_without_a_flow() {
        let idp = idp::start().await;
        let test = oidc_app(&idp, "OKF_TEST_OIDC_SECRET_LIMIT").await;

        // `oneshot` requests carry no address, so they share one bucket.
        for _ in 0..SIGN_IN_PER_IP {
            start(&test, &idp, &mut Browser::default()).await;
        }
        let mut browser = Browser::default();
        let limited = browser
            .send(&test, "GET", "/auth/oidc/example/start", Payload::Empty)
            .await;
        let unknown = browser
            .send(&test, "GET", "/auth/oidc/nope/start", Payload::Empty)
            .await;

        assert_eq!(
            limited.status,
            StatusCode::TOO_MANY_REQUESTS,
            "{}",
            limited.text()
        );
        assert!(limited.set_cookie(FLOW_COOKIE).is_none());
        assert_eq!(
            flows(&test),
            usize::try_from(SIGN_IN_PER_IP).expect("small")
        );
        assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_browser_starting_again_replaces_its_previous_flow() {
        let idp = idp::start().await;
        let test = oidc_app(&idp, "OKF_TEST_OIDC_SECRET_RESTART").await;
        user_with_password(&test, "alice", "alice@example.com", Some(PASSWORD)).await;
        idp.will_issue("sub-1", Some("alice@example.com"), Some(true));
        let mut browser = Browser::default();

        for _ in 0..3 {
            start(&test, &idp, &mut browser).await;
        }
        let query = start(&test, &idp, &mut browser).await;
        let done = callback(&test, &mut browser, &query["state"]).await;

        assert_eq!(done.status, StatusCode::SEE_OTHER, "{}", done.text());
        assert_eq!(
            flows(&test),
            0,
            "the earlier flows were dropped, not stranded"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn repeated_link_starts_never_grow_the_flow_map_and_others_still_start() {
        let idp = idp::start().await;
        let test = oidc_app(&idp, "OKF_TEST_OIDC_SECRET_FLOOD").await;
        user_with_password(&test, "alice", "alice@example.com", Some(PASSWORD)).await;
        user_with_password(&test, "bob", "bob@example.com", Some(PASSWORD)).await;
        let alice = Browser::signed_in(&test, "alice@example.com", PASSWORD).await;
        let session = alice.cookie(COOKIE).expect("a session").to_string();
        let csrf = alice.csrf.clone();

        // A script that never keeps the flow cookie, on one session: each
        // start replaces the session's previous flow.
        for _ in 0..5 {
            let mut scripted = Browser::with_cookie(COOKIE, &session);
            scripted.csrf = csrf.clone();
            assert_eq!(post_link(&test, &mut scripted).await.status, StatusCode::OK);
        }
        assert_eq!(flows(&test), 1, "one live link flow per session");

        // New sessions each time: the per-user allowance runs out.
        let mut refused = None;
        for _ in 5..=LINK_STARTS_PER_USER {
            let mut fresh = Browser::signed_in(&test, "alice@example.com", PASSWORD).await;
            let reply = post_link(&test, &mut fresh).await;
            if reply.status != StatusCode::OK {
                refused = Some(reply);
                break;
            }
        }
        let refused = refused.expect("the allowance ran out");
        assert_eq!(
            refused.status,
            StatusCode::TOO_MANY_REQUESTS,
            "{}",
            refused.text()
        );
        assert!(refused.set_cookie(FLOW_COOKIE).is_none());
        assert!(flows(&test) <= usize::try_from(LINK_STARTS_PER_USER).expect("small"));

        let mut bob = Browser::signed_in(&test, "bob@example.com", PASSWORD).await;
        let anyone = Browser::default()
            .send(&test, "GET", "/auth/oidc/example/start", Payload::Empty)
            .await;
        assert_eq!(
            post_link(&test, &mut bob).await.status,
            StatusCode::OK,
            "another user"
        );
        assert_eq!(anyone.status, StatusCode::SEE_OTHER, "a sign-in");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_subject_postgres_would_refuse_is_400_not_500() {
        let idp = idp::start().await;
        let test = oidc_app(&idp, "OKF_TEST_OIDC_SECRET_NUL").await;
        user_with_password(&test, "alice", "alice@example.com", Some(PASSWORD)).await;
        idp.will_issue("sub\0one", Some("alice@example.com"), Some(true));
        let mut browser = Browser::default();

        let query = start(&test, &idp, &mut browser).await;
        let reply = callback(&test, &mut browser, &query["state"]).await;

        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text());
        assert_eq!(browser.cookie(COOKIE), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_signed_in_user_links_a_provider_whatever_its_email() {
        let idp = idp::start().await;
        let test = oidc_app(&idp, "OKF_TEST_OIDC_SECRET_ACCOUNT").await;
        let alice = user_with_password(&test, "alice", "alice@example.com", Some(PASSWORD)).await;
        let mut here = Browser::signed_in(&test, "alice@example.com", PASSWORD).await;
        let mut elsewhere = Browser::signed_in(&test, "alice@example.com", PASSWORD).await;
        idp.will_issue("sub-9", Some("someone@elsewhere.example"), Some(false));

        let query = start_link(&test, &idp, &mut here, json!({})).await;
        let done = callback(&test, &mut here, &query["state"]).await;

        assert_eq!(done.status, StatusCode::SEE_OTHER, "{}", done.text());
        assert_eq!(done.location(), "/ui/#/account");
        let linked = identities::list(test.app.accounts().executor(), &alice.id)
            .await
            .expect("list");
        assert_eq!(linked[0].email_at_link, "someone@elsewhere.example");
        let kept = here
            .send(&test, "GET", "/api/v1/session", Payload::Empty)
            .await;
        let ended = elsewhere
            .send(&test, "GET", "/api/v1/session", Payload::Empty)
            .await;
        assert_eq!(kept.status, StatusCode::OK);
        assert_eq!(ended.status, StatusCode::UNAUTHORIZED);
        wait_for_mail(&test, 1).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn keep_sessions_keeps_the_other_sessions_when_linking() {
        let idp = idp::start().await;
        let test = oidc_app(&idp, "OKF_TEST_OIDC_SECRET_KEEP").await;
        user_with_password(&test, "alice", "alice@example.com", Some(PASSWORD)).await;
        let mut here = Browser::signed_in(&test, "alice@example.com", PASSWORD).await;
        let mut elsewhere = Browser::signed_in(&test, "alice@example.com", PASSWORD).await;
        idp.will_issue("sub-9", None, None);

        let query = start_link(&test, &idp, &mut here, json!({ "keep_sessions": true })).await;
        let done = callback(&test, &mut here, &query["state"]).await;

        assert_eq!(done.status, StatusCode::SEE_OTHER, "{}", done.text());
        let kept = elsewhere
            .send(&test, "GET", "/api/v1/session", Payload::Empty)
            .await;
        assert_eq!(kept.status, StatusCode::OK);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_link_callback_needs_the_live_session_that_started_it() {
        let idp = idp::start().await;
        let test = oidc_app(&idp, "OKF_TEST_OIDC_SECRET_BOUND").await;
        let alice = user_with_password(&test, "alice", "alice@example.com", Some(PASSWORD)).await;
        user_with_password(&test, "bob", "bob@example.com", Some(PASSWORD)).await;
        idp.will_issue("sub-9", Some("mallory@example.com"), Some(true));

        // The flow cookie alone, without alice's session (another browser
        // handed the callback, or alice signed out meanwhile).
        let mut started = Browser::signed_in(&test, "alice@example.com", PASSWORD).await;
        let query = start_link(&test, &idp, &mut started, json!({})).await;
        let flow = started.cookie(FLOW_COOKIE).expect("a flow").to_string();
        let mut bare = Browser::with_cookie(FLOW_COOKIE, &flow);
        let without_session = callback(&test, &mut bare, &query["state"]).await;

        // The flow cookie beside another user's session.
        let mut started = Browser::signed_in(&test, "alice@example.com", PASSWORD).await;
        let query = start_link(&test, &idp, &mut started, json!({})).await;
        let flow = started.cookie(FLOW_COOKIE).expect("a flow").to_string();
        let mut as_bob = Browser::signed_in(&test, "bob@example.com", PASSWORD).await;
        as_bob.add_cookie(FLOW_COOKIE, &flow);
        let with_bob = callback(&test, &mut as_bob, &query["state"]).await;

        // Alice's session, signed out before the provider sent her back.
        let mut started = Browser::signed_in(&test, "alice@example.com", PASSWORD).await;
        let alice_session = started.cookie(COOKIE).expect("a session").to_string();
        let query = start_link(&test, &idp, &mut started, json!({})).await;
        let signed_out = started
            .send(&test, "DELETE", "/api/v1/session", Payload::Empty)
            .await;
        assert_eq!(signed_out.status, StatusCode::NO_CONTENT);
        // A second tab still sends the session cookie the first one cleared.
        let mut stale = Browser::with_cookie(COOKIE, &alice_session);
        stale.add_cookie(FLOW_COOKIE, started.cookie(FLOW_COOKIE).expect("a flow"));
        let after_sign_out = callback(&test, &mut stale, &query["state"]).await;

        for (what, reply) in [
            ("no session", &without_session),
            ("bob's session", &with_bob),
            ("signed out", &after_sign_out),
        ] {
            assert_eq!(
                reply.status,
                StatusCode::BAD_REQUEST,
                "{what}: {}",
                reply.text()
            );
            assert!(reply.text().contains(LINK_RETRY), "{what}");
        }
        assert!(identities::list(test.app.accounts().executor(), &alice.id)
            .await
            .expect("list")
            .is_empty());
        let bob = users::find_by_email(test.app.accounts().executor(), "bob@example.com")
            .await
            .expect("find")
            .expect("bob");
        assert!(identities::list(test.app.accounts().executor(), &bob.id)
            .await
            .expect("list")
            .is_empty());
    }

    // ----- Re-authentication for account deletion (issue #775) -----

    /// How long ago a person authenticated, as the mock's `auth_time`.
    fn ago(seconds: i64) -> AuthTime {
        AuthTime::Ago(chrono::Duration::seconds(seconds))
    }

    /// alice with no password and `sub-1` linked at the provider, signed in
    /// to the console (a session made directly, as for any signed-in test).
    async fn oidc_only_alice(test: &TestApp, idp: &MockIdp) -> (String, Browser) {
        let alice = user_with_password(test, "alice", "alice@example.com", None).await;
        idp.will_issue("sub-1", Some("alice@example.com"), Some(true));
        identities::link(
            test.app.accounts().executor(),
            &alice.id,
            "example",
            "sub-1",
            "alice@example.com",
            Utc::now(),
        )
        .await
        .expect("link alice's identity");
        (alice.id, Browser::session_for(test, "alice").await)
    }

    /// Starts a re-authentication from `browser`'s session on `app` and
    /// returns the authorize URL's query; the mock is told the nonce.
    async fn start_reauth_on(
        app: &Arc<App>,
        idp: &MockIdp,
        browser: &mut Browser,
    ) -> HashMap<String, String> {
        let started = browser
            .send_to(
                app,
                "POST",
                "/api/v1/me/identities/example/reauth",
                Payload::Json(json!({})),
            )
            .await;
        assert_eq!(started.status, StatusCode::OK, "{}", started.text());
        let query = query_of(started.json()["url"].as_str().expect("a URL"));
        idp.set_nonce(&query["nonce"]);
        query
    }

    /// What the browser does with the authorize URL: asks the mock provider
    /// for it (without following its redirect back).
    async fn visit_authorize(idp: &MockIdp, query: &HashMap<String, String>) {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("an HTTP client");
        let mut url = url::Url::parse(&format!("{}/authorize", idp.issuer)).expect("a URL");
        url.query_pairs_mut().extend_pairs(query);
        let answer = client.get(url).send().await.expect("visit the provider");
        assert!(answer.status().is_redirection(), "{}", answer.status());
    }

    async fn start_reauth(
        test: &TestApp,
        idp: &MockIdp,
        browser: &mut Browser,
    ) -> HashMap<String, String> {
        start_reauth_on(&test.app, idp, browser).await
    }

    /// `DELETE /api/v1/me` with the provider re-authentication as proof.
    async fn delete_with_reauth_on(app: &Arc<App>, browser: &mut Browser) -> Reply {
        browser
            .send_to(
                app,
                "DELETE",
                "/api/v1/me",
                Payload::Json(json!({"oidc_reauth": true})),
            )
            .await
    }

    async fn delete_with_reauth(test: &TestApp, browser: &mut Browser) -> Reply {
        delete_with_reauth_on(&test.app, browser).await
    }

    async fn user_count(test: &TestApp, id: &str) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM users WHERE id = $1")
            .bind(id)
            .fetch_one(test.app.accounts().executor())
            .await
            .expect("count")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_fresh_provider_reauthentication_deletes_an_oidc_only_account() {
        // Arrange
        let idp = idp::start().await;
        let test = oidc_app(&idp, "OKF_TEST_OIDC_SECRET_REAUTH_OK").await;
        let (id, mut browser) = oidc_only_alice(&test, &idp).await;
        let session = browser.cookie(COOKIE).expect("a session").to_string();

        // Act: the browser follows the authorize URL to the mock, an honest
        // provider that re-authenticates because `max_age=0` asked it to and
        // says so in `auth_time`.
        let query = start_reauth(&test, &idp, &mut browser).await;
        visit_authorize(&idp, &query).await;
        let back = callback(&test, &mut browser, &query["state"]).await;
        let kept = user_count(&test, &id).await;
        let session_after = browser.cookie(COOKIE).map(str::to_string);
        let deleted = delete_with_reauth(&test, &mut browser).await;

        // Assert: the provider was asked to authenticate again, the
        // callback only came back to the account page (no session, no
        // link), and the deletion went through.
        assert_eq!(query["prompt"], "login");
        assert_eq!(query["max_age"], "0");
        let asked = idp.authorizations();
        assert_eq!(asked.len(), 1, "one visit to the provider");
        assert_eq!(asked[0]["prompt"], "login");
        assert_eq!(asked[0]["max_age"], "0");
        assert_eq!(back.status, StatusCode::SEE_OTHER, "{}", back.text());
        assert_eq!(back.location(), "/ui/#/account");
        assert!(back.set_cookie(COOKIE).is_none(), "no session cookie");
        assert_eq!(session_after.as_deref(), Some(session.as_str()));
        assert_eq!(kept, 1, "the callback deleted nothing");
        assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{}", deleted.text());
        assert_eq!(user_count(&test, &id).await, 0);
        let left: i64 = sqlx::query_scalar(
            "SELECT (SELECT count(*) FROM user_identities WHERE user_id = $1) \
                  + (SELECT count(*) FROM user_sessions WHERE user_id = $1)",
        )
        .bind(&id)
        .fetch_one(test.app.accounts().executor())
        .await
        .expect("count");
        assert_eq!(left, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_auth_time_four_minutes_old_is_inside_the_window_and_deletes() {
        let idp = idp::start().await;
        let test = oidc_app(&idp, "OKF_TEST_OIDC_SECRET_REAUTH_FOUR").await;
        let (id, mut browser) = oidc_only_alice(&test, &idp).await;
        idp.will_authenticate(ago(240));

        let query = start_reauth(&test, &idp, &mut browser).await;
        let back = callback(&test, &mut browser, &query["state"]).await;
        let deleted = delete_with_reauth(&test, &mut browser).await;

        assert_eq!(back.status, StatusCode::SEE_OTHER, "{}", back.text());
        assert_eq!(back.location(), "/ui/#/account");
        assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{}", deleted.text());
        assert_eq!(user_count(&test, &id).await, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_sign_in_start_never_asks_for_a_fresh_login() {
        let idp = idp::start().await;
        let test = oidc_app(&idp, "OKF_TEST_OIDC_SECRET_REAUTH_PLAIN").await;
        let mut browser = Browser::default();

        let query = start(&test, &idp, &mut browser).await;

        assert!(!query.contains_key("prompt"), "{query:?}");
        assert!(!query.contains_key("max_age"), "{query:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_missing_stale_or_future_auth_time_is_refused_and_keeps_the_account() {
        let idp = idp::start().await;
        let test = oidc_app(&idp, "OKF_TEST_OIDC_SECRET_REAUTH_TIME").await;
        let (id, mut browser) = oidc_only_alice(&test, &idp).await;
        let window = i64::try_from(REAUTH_AUTH_TIME_WINDOW.as_secs()).expect("seconds");

        for (what, auth_time) in [
            (
                "no auth_time (a provider that ignored max_age)",
                AuthTime::Absent,
            ),
            ("just past the window", ago(window + 30)),
            ("an hour old", ago(3600)),
            ("in the future", ago(-600)),
        ] {
            idp.will_authenticate(auth_time);
            let query = start_reauth(&test, &idp, &mut browser).await;
            let back = callback(&test, &mut browser, &query["state"]).await;
            let deleted = delete_with_reauth(&test, &mut browser).await;

            // The provider's answer was genuine but not fresh: back to the
            // account page, with nothing signed in or linked.
            assert_eq!(
                back.status,
                StatusCode::SEE_OTHER,
                "{what}: {}",
                back.text()
            );
            assert!(back.set_cookie(COOKIE).is_none(), "{what}");
            // The deletion says why, with its own kind, and names the
            // provider by label only.
            assert_eq!(
                deleted.status,
                StatusCode::FORBIDDEN,
                "{what}: {}",
                deleted.text()
            );
            let body = deleted.json();
            assert_eq!(body["error"], "ReauthStale", "{what}");
            let message = body["message"].as_str().expect("a message");
            assert!(message.contains("Example ID"), "{what}: {message}");
            assert!(message.contains("sign out of Example ID"), "{what}");
            for secret in [idp.issuer.as_str(), "sub-1"] {
                assert!(!message.contains(secret), "{what}: {message}");
            }
            assert_eq!(user_count(&test, &id).await, 1, "{what}");
            // One use: the next attempt has nothing waiting.
            let again = delete_with_reauth(&test, &mut browser).await;
            assert_eq!(again.status, StatusCode::GONE, "{what}");
        }
        // A refused re-authentication is not a failed sign-in.
        let events: i64 = sqlx::query_scalar("SELECT count(*) FROM sign_in_events")
            .fetch_one(test.app.accounts().executor())
            .await
            .expect("count");
        assert_eq!(events, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_token_for_another_identity_is_refused_even_a_linked_one() {
        let idp = idp::start().await;
        let test = oidc_app(&idp, "OKF_TEST_OIDC_SECRET_REAUTH_OTHER").await;
        let (id, mut browser) = oidc_only_alice(&test, &idp).await;
        let bob = user_with_password(&test, "bob", "bob@example.com", None).await;
        identities::link(
            test.app.accounts().executor(),
            &bob.id,
            "example",
            "sub-2",
            "bob@example.com",
            Utc::now(),
        )
        .await
        .expect("link bob's identity");

        // bob's identity (linked to a user), and one nobody has linked, each
        // with a fresh, an absent and a stale `auth_time`: who answered is
        // judged before how fresh it was, so the wrong account is never told
        // that signing in again would help.
        for subject in ["sub-2", "sub-9"] {
            for (what, auth_time) in [
                ("fresh", ago(5)),
                ("absent", AuthTime::Absent),
                ("stale", ago(3600)),
            ] {
                idp.will_issue(subject, Some("alice@example.com"), Some(true));
                idp.will_authenticate(auth_time);
                let query = start_reauth(&test, &idp, &mut browser).await;
                let back = callback(&test, &mut browser, &query["state"]).await;
                let deleted = delete_with_reauth(&test, &mut browser).await;

                assert_eq!(
                    back.status,
                    StatusCode::BAD_REQUEST,
                    "{subject} {what}: {}",
                    back.text()
                );
                assert!(back.text().contains(REAUTH_RETRY), "{subject} {what}");
                // No marker either: nothing is waiting, and it is not
                // `ReauthStale`.
                assert_eq!(deleted.status, StatusCode::GONE, "{subject} {what}");
                assert_ne!(deleted.json()["error"], "ReauthStale", "{subject} {what}");
            }
        }
        assert_eq!(user_count(&test, &id).await, 1);
        assert_eq!(user_count(&test, &bob.id).await, 1);
    }

    /// `GET /api/v1/me/identities/example/reauth` from `browser`'s session.
    async fn reauth_status_of(test: &TestApp, browser: &mut Browser) -> Reply {
        browser
            .send_to(
                &test.app,
                "GET",
                "/api/v1/me/identities/example/reauth",
                Payload::Empty,
            )
            .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_status_call_never_spends_the_proof() {
        let idp = idp::start().await;
        let test = oidc_app(&idp, "OKF_TEST_OIDC_SECRET_REAUTH_PEEK").await;
        let (id, mut browser) = oidc_only_alice(&test, &idp).await;
        idp.will_authenticate(ago(5));

        // Nothing waits before the person returns from the provider.
        let before = reauth_status_of(&test, &mut browser).await;
        let query = start_reauth(&test, &idp, &mut browser).await;
        let started = reauth_status_of(&test, &mut browser).await;
        callback(&test, &mut browser, &query["state"]).await;
        let first = reauth_status_of(&test, &mut browser).await;
        let second = reauth_status_of(&test, &mut browser).await;
        let deleted = delete_with_reauth(&test, &mut browser).await;
        let after = reauth_status_of(&test, &mut browser).await;

        // The answer holds the state and nothing else, and asking twice
        // left the proof for the deletion.
        assert_eq!(before.status, StatusCode::OK, "{}", before.text());
        assert_eq!(before.json(), json!({"state": "none"}));
        assert_eq!(started.json(), json!({"state": "none"}));
        assert_eq!(first.json(), json!({"state": "ready"}));
        assert_eq!(second.json(), json!({"state": "ready"}));
        assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{}", deleted.text());
        assert_eq!(user_count(&test, &id).await, 0);
        // Her session ended with the account: nothing to ask as.
        assert_ne!(after.status, StatusCode::OK);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_status_call_says_stale_for_every_stale_cause_and_leaves_the_marker() {
        let idp = idp::start().await;
        let test = oidc_app(&idp, "OKF_TEST_OIDC_SECRET_REAUTH_PEEKSTALE").await;
        let (id, mut browser) = oidc_only_alice(&test, &idp).await;
        let window = i64::try_from(REAUTH_AUTH_TIME_WINDOW.as_secs()).expect("seconds");

        for (what, auth_time) in [
            ("no auth_time", AuthTime::Absent),
            ("just past the window", ago(window + 30)),
            ("an hour old", ago(3600)),
            ("in the future", ago(-600)),
        ] {
            idp.will_authenticate(auth_time);
            let query = start_reauth(&test, &idp, &mut browser).await;
            callback(&test, &mut browser, &query["state"]).await;

            let first = reauth_status_of(&test, &mut browser).await;
            let second = reauth_status_of(&test, &mut browser).await;
            let deleted = delete_with_reauth(&test, &mut browser).await;
            let after = reauth_status_of(&test, &mut browser).await;

            // Only the state: no provider label, issuer, subject or time.
            assert_eq!(first.status, StatusCode::OK, "{what}: {}", first.text());
            assert_eq!(first.json(), json!({"state": "stale"}), "{what}");
            assert_eq!(second.json(), json!({"state": "stale"}), "{what}");
            // The deletion agrees with what the status said, and still finds
            // the marker after two reads.
            assert_eq!(deleted.status, StatusCode::FORBIDDEN, "{what}");
            assert_eq!(deleted.json()["error"], "ReauthStale", "{what}");
            assert_eq!(after.json(), json!({"state": "none"}), "{what}");
        }
        assert_eq!(user_count(&test, &id).await, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_status_call_refuses_other_callers_and_shows_nothing_of_another_session() {
        let idp = idp::start().await;
        let test = oidc_app(&idp, "OKF_TEST_OIDC_SECRET_REAUTH_PEEKOTHER").await;
        let (id, mut browser) = oidc_only_alice(&test, &idp).await;
        idp.will_authenticate(ago(3600));
        let query = start_reauth(&test, &idp, &mut browser).await;
        callback(&test, &mut browser, &query["state"]).await;
        user_with_password(&test, "bob", "bob@example.com", Some(PASSWORD)).await;
        let mut bob = Browser::session_for(&test, "bob").await;
        let mut nobody = Browser::default();

        let by_bob = reauth_status_of(&test, &mut bob).await;
        let by_nobody = reauth_status_of(&test, &mut nobody).await;
        let unknown = browser
            .send_to(
                &test.app,
                "GET",
                "/api/v1/me/identities/nowhere/reauth",
                Payload::Empty,
            )
            .await;
        let by_alice = reauth_status_of(&test, &mut browser).await;

        // Another signed-in session sees only its own (empty) state, never
        // alice's stale marker; no session is refused outright; an unknown
        // provider is a 404.
        assert_eq!(by_bob.json(), json!({"state": "none"}));
        assert!(
            by_nobody.status == StatusCode::UNAUTHORIZED
                || by_nobody.status == StatusCode::FORBIDDEN,
            "{}",
            by_nobody.status
        );
        assert_eq!(unknown.status, StatusCode::NOT_FOUND);
        assert_eq!(by_alice.json(), json!({"state": "stale"}));
        assert_eq!(user_count(&test, &id).await, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_used_proof_cannot_be_replayed_and_an_expired_one_is_gone() {
        let idp = idp::start().await;
        let test = oidc_app(&idp, "OKF_TEST_OIDC_SECRET_REAUTH_REPLAY").await;
        let (id, mut browser) = oidc_only_alice(&test, &idp).await;
        idp.will_authenticate(ago(5));
        // A rule of the deletion refuses it once: her personal team owns a
        // catalog.
        sqlx::query(
            "UPDATE catalogs SET owner_team_id = (SELECT id FROM teams WHERE name = 'alice') \
             WHERE name = 'main'",
        )
        .execute(test.app.accounts().executor())
        .await
        .expect("give alice's team main");
        let query = start_reauth(&test, &idp, &mut browser).await;
        callback(&test, &mut browser, &query["state"]).await;

        let refused = delete_with_reauth(&test, &mut browser).await;
        sqlx::query("UPDATE catalogs SET owner_team_id = NULL WHERE name = 'main'")
            .execute(test.app.accounts().executor())
            .await
            .expect("take it back");
        let replayed = delete_with_reauth(&test, &mut browser).await;

        // The refusal used the proof up: the replay finds none.
        assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.text());
        assert_eq!(replayed.status, StatusCode::GONE, "{}", replayed.text());
        assert_eq!(user_count(&test, &id).await, 1);

        // A new proof works; one that waited past its lifetime does not.
        let query = start_reauth(&test, &idp, &mut browser).await;
        callback(&test, &mut browser, &query["state"]).await;
        let oidc = test.app.oidc().expect("OIDC is on");
        let hash = crate::oidc::session_hash(browser.cookie(COOKIE).expect("a session"));
        let late = Instant::now() + REAUTH_PROOF_TTL + Duration::from_secs(1);
        assert!(oidc
            .take_reauth_proof(&hash, late)
            .await
            .expect("take")
            .is_none());
        let query = start_reauth(&test, &idp, &mut browser).await;
        callback(&test, &mut browser, &query["state"]).await;
        let deleted = delete_with_reauth(&test, &mut browser).await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{}", deleted.text());
        assert_eq!(user_count(&test, &id).await, 0);
    }

    /// Re-authenticates in S1 and S2 of one user on `start_on` and
    /// `finish_on`: S2 can neither finish S1's flow nor spend S1's proof.
    async fn a_reauthentication_belongs_to_the_session_that_started_it(
        test: &TestApp,
        start_on: &Arc<App>,
        finish_on: &Arc<App>,
        idp: &MockIdp,
    ) {
        let (id, mut s1) = oidc_only_alice(test, idp).await;
        let mut s2 = Browser::session_for(test, "alice").await;
        idp.will_authenticate(ago(5));

        // S1's flow cookie beside S2's session.
        let query = start_reauth_on(start_on, idp, &mut s1).await;
        let flow = s1.cookie(FLOW_COOKIE).expect("a flow").to_string();
        let mut crossed = Browser::with_cookie(COOKIE, s2.cookie(COOKIE).expect("S2"));
        crossed.add_cookie(FLOW_COOKIE, &flow);
        let refused = callback_on(finish_on, &mut crossed, &query["state"]).await;
        // S1 finishes it; S2 then tries to spend S1's proof.
        let query = start_reauth_on(start_on, idp, &mut s1).await;
        let done = callback_on(finish_on, &mut s1, &query["state"]).await;
        let from_s2 = delete_with_reauth_on(finish_on, &mut s2).await;
        let from_s1 = delete_with_reauth_on(start_on, &mut s1).await;

        assert_eq!(
            refused.status,
            StatusCode::BAD_REQUEST,
            "{}",
            refused.text()
        );
        assert!(refused.text().contains(REAUTH_RETRY));
        assert_eq!(done.status, StatusCode::SEE_OTHER, "{}", done.text());
        assert_eq!(from_s2.status, StatusCode::GONE, "{}", from_s2.text());
        // S1's proof was still there for S1.
        assert_eq!(from_s1.status, StatusCode::NO_CONTENT, "{}", from_s1.text());
        assert_eq!(user_count(test, &id).await, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_reauthentication_belongs_to_the_session_that_started_it_in_memory() {
        let idp = idp::start().await;
        let test = oidc_app(&idp, "OKF_TEST_OIDC_SECRET_REAUTH_SESSION").await;
        a_reauthentication_belongs_to_the_session_that_started_it(
            &test, &test.app, &test.app, &idp,
        )
        .await;
    }

    async fn a_reauthentication_belongs_to_its_session_across_replicas(redis: TestRedis) {
        let idp = idp::start().await;
        let (a, b, _redis) = oidc_pair(&idp, "OKF_TEST_OIDC_SECRET_REAUTH_REPLICAS", redis).await;
        a_reauthentication_belongs_to_the_session_that_started_it(&a, &a.app, &b, &idp).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_sign_in_callback_never_satisfies_the_deletion() {
        let idp = idp::start().await;
        let test = oidc_app(&idp, "OKF_TEST_OIDC_SECRET_REAUTH_LOGIN").await;
        let (id, mut browser) = oidc_only_alice(&test, &idp).await;
        // The sign-in's token even carries a brand new auth_time.
        idp.will_authenticate(ago(0));

        let query = start(&test, &idp, &mut browser).await;
        let signed_in = callback(&test, &mut browser, &query["state"]).await;
        // The console reads its new session's CSRF token from the session.
        let session = browser
            .send(&test, "GET", "/api/v1/session", Payload::Empty)
            .await;
        browser.csrf = Some(
            session.json()["csrf"]
                .as_str()
                .expect("a token")
                .to_string(),
        );
        let deleted = delete_with_reauth(&test, &mut browser).await;

        assert_eq!(
            signed_in.status,
            StatusCode::SEE_OTHER,
            "{}",
            signed_in.text()
        );
        assert!(signed_in.set_cookie(COOKIE).is_some(), "a sign-in");
        assert_eq!(deleted.status, StatusCode::GONE, "{}", deleted.text());
        assert_eq!(user_count(&test, &id).await, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_reauthentication_callback_never_signs_in_or_links() {
        let idp = idp::start().await;
        let test = oidc_app(&idp, "OKF_TEST_OIDC_SECRET_REAUTH_NOLOGIN").await;
        let (id, mut browser) = oidc_only_alice(&test, &idp).await;
        idp.will_authenticate(ago(5));
        let query = start_reauth(&test, &idp, &mut browser).await;
        let flow = browser.cookie(FLOW_COOKIE).expect("a flow").to_string();

        // The flow cookie without alice's session: not a sign-in either.
        let mut bare = Browser::with_cookie(FLOW_COOKIE, &flow);
        let refused = callback(&test, &mut bare, &query["state"]).await;

        assert_eq!(
            refused.status,
            StatusCode::BAD_REQUEST,
            "{}",
            refused.text()
        );
        assert!(refused.set_cookie(COOKIE).is_none());
        assert_eq!(bare.cookie(COOKIE), None);
        assert_eq!(sessions_of(test.app.accounts().executor(), &id).await, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn starting_a_reauthentication_needs_a_console_session_and_a_linked_identity() {
        let idp = idp::start().await;
        let test = oidc_app(&idp, "OKF_TEST_OIDC_SECRET_REAUTH_START").await;
        let keys = keys(&test).await;
        let uri = "/api/v1/me/identities/example/reauth";
        user_with_password(&test, "bob", "bob@example.com", Some(PASSWORD)).await;
        let mut bob = Browser::signed_in(&test, "bob@example.com", PASSWORD).await;
        let (_, mut alice) = oidc_only_alice(&test, &idp).await;

        let mut anonymous = Browser::default();
        let anonymous = anonymous
            .send(&test, "POST", uri, Payload::Json(json!({})))
            .await;
        let (bearer, _) =
            crate::http::api::testing::send(&test, "POST", uri, Some(&keys.admin), Some(json!({})))
                .await;
        // bob links nothing at the provider; no provider is called "nope".
        let unlinked = bob.send(&test, "POST", uri, Payload::Json(json!({}))).await;
        let unknown = alice
            .send(
                &test,
                "POST",
                "/api/v1/me/identities/nope/reauth",
                Payload::Json(json!({})),
            )
            .await;

        assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
        assert_eq!(bearer.as_u16(), 403);
        assert_eq!(
            unlinked.status,
            StatusCode::NOT_FOUND,
            "{}",
            unlinked.text()
        );
        assert_eq!(unknown.status, StatusCode::NOT_FOUND, "{}", unknown.text());
    }

    #[test]
    fn the_auth_time_window_is_five_minutes_and_a_reauth_proof_lives_that_long() {
        assert_eq!(REAUTH_AUTH_TIME_WINDOW, Duration::from_secs(300));
        assert_eq!(REAUTH_PROOF_TTL, Duration::from_secs(300));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn linking_a_subject_another_user_holds_is_409() {
        let idp = idp::start().await;
        let test = oidc_app(&idp, "OKF_TEST_OIDC_SECRET_TAKEN").await;
        user_with_password(&test, "alice", "alice@example.com", Some(PASSWORD)).await;
        let bob = user_with_password(&test, "bob", "bob@example.com", Some(PASSWORD)).await;
        identities::link(
            test.app.accounts().executor(),
            &bob.id,
            "example",
            "sub-1",
            "bob@example.com",
            chrono::Utc::now(),
        )
        .await
        .expect("bob holds sub-1");
        let mut alice = Browser::signed_in(&test, "alice@example.com", PASSWORD).await;
        idp.will_issue("sub-1", Some("bob@example.com"), Some(true));

        let query = start_link(&test, &idp, &mut alice, json!({ "keep_sessions": true })).await;
        let done = callback(&test, &mut alice, &query["state"]).await;

        assert_eq!(done.status, StatusCode::CONFLICT);
        assert!(done.text().contains("linked to another user"));
        assert!(LINKED_ELSEWHERE.contains("linked to another user"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_subject_claiming_a_linked_users_email_is_409() {
        let idp = idp::start().await;
        let test = oidc_app(&idp, "OKF_TEST_OIDC_SECRET_SECOND").await;
        let alice = user_with_password(&test, "alice", "alice@example.com", Some(PASSWORD)).await;
        identities::link(
            test.app.accounts().executor(),
            &alice.id,
            "example",
            "sub-1",
            "alice@example.com",
            chrono::Utc::now(),
        )
        .await
        .expect("alice holds sub-1");
        idp.will_issue("sub-2", Some("alice@example.com"), Some(true));
        let mut browser = Browser::default();

        let query = start(&test, &idp, &mut browser).await;
        let reply = callback(&test, &mut browser, &query["state"]).await;

        assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.text());
        assert_eq!(browser.cookie(COOKIE), None);
        assert_eq!(
            identities::list(test.app.accounts().executor(), &alice.id)
                .await
                .expect("list")
                .len(),
            1
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_sign_in_waiting_on_an_unlink_gets_no_session() {
        let idp = idp::start().await;
        let test = oidc_app(&idp, "OKF_TEST_OIDC_SECRET_UNLINK").await;
        let alice = user_with_password(&test, "alice", "alice@example.com", Some(PASSWORD)).await;
        let pool = test.app.accounts().executor();
        identities::link(
            pool,
            &alice.id,
            "example",
            "sub-1",
            "alice@example.com",
            chrono::Utc::now(),
        )
        .await
        .expect("link");
        idp.will_issue("sub-1", Some("alice@example.com"), Some(true));
        let mut browser = Browser::default();
        let query = start(&test, &idp, &mut browser).await;

        // An unlink in progress: it holds alice's row, as
        // `sign_in_methods::count` takes it. The callback finds alice by
        // `(provider, sub)` (a read that takes no lock) and then queues on
        // her row to insert the session; only then is the identity deleted
        // and the unlink committed.
        let mut holder = pool.begin().await.expect("begin");
        sqlx::query("SELECT 1 FROM users WHERE id = $1 FOR NO KEY UPDATE")
            .bind(&alice.id)
            .execute(&mut *holder)
            .await
            .expect("lock alice");
        let unlink = async {
            wait_for_lock_waiters(pool, 1).await;
            sqlx::query("DELETE FROM user_identities WHERE user_id = $1")
                .bind(&alice.id)
                .execute(&mut *holder)
                .await
                .expect("unlink");
            holder.commit().await.expect("commit the unlink");
        };
        // `join!` polls both futures on this task until both finish, like
        // `asyncio.gather`: the callback waits on the lock while `unlink`
        // runs.
        let (reply, ()) = tokio::join!(callback(&test, &mut browser, &query["state"]), unlink);

        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text());
        assert!(reply.set_cookie(COOKIE).is_none());
        assert_eq!(sessions_of(pool, &alice.id).await, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_sign_in_commits_nothing_until_its_event_and_notice_are_written() {
        let idp = idp::start().await;
        let test = oidc_app(&idp, "OKF_TEST_OIDC_SECRET_COMMIT").await;
        let alice = user_with_password(&test, "alice", "alice@example.com", Some(PASSWORD)).await;
        let bob = user_with_password(&test, "bob", "bob@example.com", Some(PASSWORD)).await;
        let pool = test.app.accounts().executor();

        // The event table is locked: the sign-in waits inside its
        // transaction, with nothing committed, and finishes once it can.
        idp.will_issue("sub-1", Some("alice@example.com"), Some(true));
        let mut browser = Browser::default();
        let query = start(&test, &idp, &mut browser).await;
        let mut events = pool.begin().await.expect("begin");
        sqlx::query("LOCK TABLE sign_in_events IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *events)
            .await
            .expect("lock the events");
        let release = async {
            wait_for_lock_waiters(pool, 1).await;
            assert_eq!(sessions_of(pool, &alice.id).await, 0, "no session yet");
            assert!(
                identities::list(pool, &alice.id)
                    .await
                    .expect("list")
                    .is_empty(),
                "no link yet"
            );
            events.rollback().await.expect("release the events");
        };
        let (reply, ()) = tokio::join!(callback(&test, &mut browser, &query["state"]), release);

        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
        assert!(reply.set_cookie(COOKIE).is_some());
        assert_eq!(sessions_of(pool, &alice.id).await, 1);
        let recorded = sign_in_events::recent(pool, "alice", 5)
            .await
            .expect("events");
        assert!(matches!(
            (&recorded[0].method, &recorded[0].outcome),
            (Method::Oidc, Outcome::Ok)
        ));

        // The mail queue is locked: the sign-in that links bob queues its
        // notice inside its transaction, so it waits there with nothing
        // committed, then answers with its cookie once the queue is free.
        // After its commit it waits on nothing.
        idp.will_issue("sub-2", Some("bob@example.com"), Some(true));
        let mut browser = Browser::default();
        let query = start(&test, &idp, &mut browser).await;
        let mut jobs = pool.begin().await.expect("begin");
        sqlx::query("LOCK TABLE apalis.jobs IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *jobs)
            .await
            .expect("lock the mail queue");
        let release = async {
            wait_for_lock_waiters(pool, 1).await;
            assert_eq!(sessions_of(pool, &bob.id).await, 0, "no session yet");
            jobs.rollback().await.expect("release the mail queue");
        };
        let (reply, ()) = tokio::join!(callback(&test, &mut browser, &query["state"]), release);
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
        assert!(reply.set_cookie(COOKIE).is_some());
        assert_eq!(sessions_of(pool, &bob.id).await, 1);
        assert_eq!(
            queued_mail(&test).await,
            2,
            "both notices committed with their links"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_oidc_session_ends_with_a_reset_or_an_admin_change() {
        let idp = idp::start().await;
        let test = oidc_app(&idp, "OKF_TEST_OIDC_SECRET_RESET").await;
        let alice = user_with_password(&test, "alice", "alice@example.com", Some(PASSWORD)).await;
        idp.will_issue("sub-1", Some("alice@example.com"), Some(true));
        let pool = test.app.accounts().executor();

        let mut first = Browser::default();
        let query = start(&test, &idp, &mut first).await;
        callback(&test, &mut first, &query["state"]).await;
        passwords::reset(
            pool,
            &alice.id,
            "amber-quarry-signal-77",
            chrono::Utc::now(),
        )
        .await
        .expect("reset");
        let after_reset = first
            .send(&test, "GET", "/api/v1/session", Payload::Empty)
            .await;

        let mut second = Browser::default();
        let query = start(&test, &idp, &mut second).await;
        callback(&test, &mut second, &query["state"]).await;
        users::set_admin(
            pool,
            "alice",
            AdminChange {
                on: true,
                allow_last: false,
            },
            &AuditActor::cli(),
            Utc::now(),
        )
        .await
        .expect("make alice an admin");
        let after_admin = second
            .send(&test, "GET", "/api/v1/session", Payload::Empty)
            .await;

        assert_eq!(after_reset.status, StatusCode::UNAUTHORIZED);
        assert_eq!(after_admin.status, StatusCode::UNAUTHORIZED);
        assert_eq!(sessions_of(pool, &alice.id).await, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_sign_in_page_shows_each_provider_and_unknown_ones_are_404() {
        let idp = idp::start().await;
        let test = oidc_app(&idp, "OKF_TEST_OIDC_SECRET_PAGE").await;
        let mut browser = Browser::default();

        let page = browser
            .send(&test, "GET", "/signin", Payload::Empty)
            .await
            .text();
        let unknown = browser
            .send(&test, "GET", "/auth/oidc/nope/start", Payload::Empty)
            .await;

        assert!(page.contains(r#"id="signin-oidc-example""#));
        assert!(page.contains("Sign in with Example ID"));
        assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn linking_needs_a_console_session_and_its_csrf_header() {
        let idp = idp::start().await;
        let test = oidc_app(&idp, "OKF_TEST_OIDC_SECRET_KEY").await;
        user_with_password(&test, "alice", "alice@example.com", Some(PASSWORD)).await;
        let mut keyed = Browser::default();
        keyed.bearer = Some(keys(&test).await.write);
        let mut forged = Browser::signed_in(&test, "alice@example.com", PASSWORD).await;
        forged.csrf = None;

        let by_key = keyed
            .send(
                &test,
                "POST",
                "/api/v1/me/identities/example/link",
                Payload::Json(json!({})),
            )
            .await;
        let without_csrf = forged
            .send(
                &test,
                "POST",
                "/api/v1/me/identities/example/link",
                Payload::Json(json!({})),
            )
            .await;

        assert_eq!(by_key.status, StatusCode::FORBIDDEN);
        assert_eq!(without_csrf.status, StatusCode::FORBIDDEN);
        assert!(without_csrf.set_cookie(FLOW_COOKIE).is_none());
    }

    // Two replicas (spec Decisions 3-8, plan Task 9). Each body runs on a
    // standalone Redis and on a one-node cluster (`in_both_modes!` below).

    /// The bound on every replica test (containers start inside it).
    const BOUND: Duration = Duration::from_secs(180);

    /// Every key under the test's prefix.
    async fn redis_keys(app: &App, prefix: &str) -> Vec<String> {
        let redis = app.redis().expect("the app's redis");
        redis::cmd("KEYS")
            .arg(format!("{{{prefix}}}*"))
            .query_async(&mut redis.connection())
            .await
            .expect("list the keys")
    }

    /// How many of `keys` are in the key space `space` (`oidc:flow`).
    fn in_space(keys: &[String], prefix: &str, space: &str) -> usize {
        let start = format!("{{{prefix}}}{space}:");
        keys.iter().filter(|key| key.starts_with(&start)).count()
    }

    async fn a_sign_in_started_on_one_replica_completes_on_the_other_once(redis: TestRedis) {
        let idp = idp::start().await;
        let (a, b, _redis) = oidc_pair(&idp, "OKF_TEST_OIDC_SECRET_REPLICA", redis).await;
        user_with_password(&a, "alice", "alice@example.com", Some(PASSWORD)).await;
        idp.will_issue("sub-1", Some("alice@example.com"), Some(true));
        let mut browser = Browser::default();

        let query = start_on(&a.app, &idp, &mut browser).await;
        let flow = browser.cookie(FLOW_COOKIE).expect("a flow").to_string();
        let done = callback_on(&b, &mut browser, &query["state"]).await;
        let mut replay = Browser::with_cookie(FLOW_COOKIE, &flow);
        let replayed = callback_on(&a.app, &mut replay, &query["state"]).await;

        assert_eq!(done.status, StatusCode::SEE_OTHER, "{}", done.text());
        assert_eq!(done.location(), "/ui/#/keys");
        let session = browser
            .send_to(&b, "GET", "/api/v1/session", Payload::Empty)
            .await;
        assert_eq!(session.json()["user"], "alice");
        assert_eq!(
            replayed.status,
            StatusCode::BAD_REQUEST,
            "{}",
            replayed.text()
        );
        assert!(replayed.text().contains(SIGN_IN_RETRY));
        assert_eq!(replay.cookie(COOKIE), None, "the replay signed nobody in");
    }

    async fn a_link_flow_replaces_the_sessions_previous_one_across_replicas(redis: TestRedis) {
        let idp = idp::start().await;
        let (a, b, redis) = oidc_pair(&idp, "OKF_TEST_OIDC_SECRET_REPLACE", redis).await;
        let alice = user_with_password(&a, "alice", "alice@example.com", Some(PASSWORD)).await;
        let signed_in = Browser::signed_in(&a, "alice@example.com", PASSWORD).await;
        let session = signed_in.cookie(COOKIE).expect("a session").to_string();
        idp.will_issue("sub-9", None, None);

        // A script that never keeps the flow cookie: only the session's
        // index can drop the first flow.
        let mut first = Browser::with_cookie(COOKIE, &session);
        first.csrf = signed_in.csrf.clone();
        let first_query = start_link_on(&a.app, &idp, &mut first, json!({})).await;
        let mut second = Browser::with_cookie(COOKIE, &session);
        second.csrf = signed_in.csrf.clone();
        let second_query = start_link_on(&b, &idp, &mut second, json!({})).await;

        let keys = redis_keys(&a.app, &redis.prefix).await;
        assert_eq!(in_space(&keys, &redis.prefix, "oidc:link"), 1, "{keys:?}");
        assert_eq!(in_space(&keys, &redis.prefix, "oidc:flow"), 1, "{keys:?}");
        let stale = callback_on(&a.app, &mut first, &first_query["state"]).await;
        assert_eq!(stale.status, StatusCode::BAD_REQUEST, "{}", stale.text());
        assert!(identities::list(a.app.accounts().executor(), &alice.id)
            .await
            .expect("list")
            .is_empty());
        let done = callback_on(&a.app, &mut second, &second_query["state"]).await;
        assert_eq!(done.status, StatusCode::SEE_OTHER, "{}", done.text());
        assert_eq!(
            identities::list(a.app.accounts().executor(), &alice.id)
                .await
                .expect("list")
                .len(),
            1
        );
    }

    async fn no_raw_session_id_reaches_redis(redis: TestRedis) {
        let idp = idp::start().await;
        let (a, _b, redis) = oidc_pair(&idp, "OKF_TEST_OIDC_SECRET_RAW", redis).await;
        user_with_password(&a, "alice", "alice@example.com", Some(PASSWORD)).await;
        let mut browser = Browser::signed_in(&a, "alice@example.com", PASSWORD).await;
        let session = browser.cookie(COOKIE).expect("a session").to_string();

        start_link_on(&a.app, &idp, &mut browser, json!({})).await;
        let flow = browser.cookie(FLOW_COOKIE).expect("a flow").to_string();

        let keys = redis_keys(&a.app, &redis.prefix).await;
        assert_eq!(
            keys.len(),
            3,
            "a flow, its session's pointer and the user's link-start bucket: {keys:?}"
        );
        assert_eq!(
            in_space(&keys, &redis.prefix, "ratelimit:link_starts"),
            1,
            "{keys:?}"
        );
        let shared = a.app.redis().expect("redis");
        for key in &keys {
            let value = crate::shared::testing::value_text(shared, key).await;
            for secret in [session.as_str(), flow.as_str()] {
                assert!(!key.contains(secret), "{key}");
                assert!(!value.contains(secret), "{key} holds a raw id");
            }
            if in_space(std::slice::from_ref(key), &redis.prefix, "oidc:flow") == 1 {
                assert!(value.contains(&super::session_hash(&session)), "{value}");
            }
        }
    }

    /// Links from session S1 on `start_on`, finishing on `finish_on`: with
    /// S1's flow cookie but S2's session it is refused and links nothing;
    /// with S1's own session it links and S1 is the session kept.
    async fn a_link_flow_finishes_only_in_the_session_that_started_it(
        test: &TestApp,
        start_on: &Arc<App>,
        finish_on: &Arc<App>,
        idp: &MockIdp,
    ) {
        let alice = user_with_password(test, "alice", "alice@example.com", Some(PASSWORD)).await;
        let mut s1 = Browser::signed_in(test, "alice@example.com", PASSWORD).await;
        let mut s2 = Browser::signed_in(test, "alice@example.com", PASSWORD).await;
        idp.will_issue("sub-9", None, None);

        let query = start_link_on(start_on, idp, &mut s1, json!({})).await;
        let flow = s1.cookie(FLOW_COOKIE).expect("a flow").to_string();
        let mut crossed = Browser::with_cookie(COOKIE, s2.cookie(COOKIE).expect("S2"));
        crossed.add_cookie(FLOW_COOKIE, &flow);
        let refused = callback_on(finish_on, &mut crossed, &query["state"]).await;

        assert_eq!(
            refused.status,
            StatusCode::BAD_REQUEST,
            "{}",
            refused.text()
        );
        assert!(refused.text().contains(LINK_RETRY));
        assert!(identities::list(test.app.accounts().executor(), &alice.id)
            .await
            .expect("list")
            .is_empty());

        let query = start_link_on(start_on, idp, &mut s1, json!({})).await;
        let done = callback_on(finish_on, &mut s1, &query["state"]).await;

        assert_eq!(done.status, StatusCode::SEE_OTHER, "{}", done.text());
        assert_eq!(done.location(), "/ui/#/account");
        assert_eq!(
            identities::list(test.app.accounts().executor(), &alice.id)
                .await
                .expect("list")
                .len(),
            1
        );
        let kept = s1
            .send_to(finish_on, "GET", "/api/v1/session", Payload::Empty)
            .await;
        let ended = s2
            .send_to(finish_on, "GET", "/api/v1/session", Payload::Empty)
            .await;
        assert_eq!(kept.status, StatusCode::OK, "S1 is the session kept");
        assert_eq!(ended.status, StatusCode::UNAUTHORIZED, "S2 ended");
        assert_eq!(
            sessions_of(test.app.accounts().executor(), &alice.id).await,
            1
        );
    }

    async fn a_link_flow_finishes_only_in_its_session_across_replicas(redis: TestRedis) {
        let idp = idp::start().await;
        let (a, b, _redis) = oidc_pair(&idp, "OKF_TEST_OIDC_SECRET_OWN", redis).await;
        a_link_flow_finishes_only_in_the_session_that_started_it(&a, &a.app, &b, &idp).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_link_flow_finishes_only_in_its_session_in_memory() {
        let idp = idp::start().await;
        let test = oidc_app(&idp, "OKF_TEST_OIDC_SECRET_OWN_MEMORY").await;
        let app = Arc::clone(&test.app);
        a_link_flow_finishes_only_in_the_session_that_started_it(&test, &app, &app, &idp).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn link_starts_are_one_allowance_across_replicas_with_redis() {
        tokio::time::timeout(Duration::from_secs(120), async {
            let idp = idp::start().await;
            let (a, b, _redis) = oidc_pair(
                &idp,
                "OKF_TEST_OIDC_SECRET_SHARED_LINKS",
                TestRedis::start().await,
            )
            .await;
            let (one, two) = (
                a.app.oidc().expect("oidc on the first replica"),
                b.oidc().expect("oidc on the second replica"),
            );
            let now = std::time::Instant::now();
            let mut allowed = 0;
            for n in 0..LINK_STARTS_PER_USER {
                let replica = if n % 2 == 0 { one } else { two };
                if replica.may_start_link("user-1", now).await {
                    allowed += 1;
                }
            }

            assert_eq!(allowed, LINK_STARTS_PER_USER);
            assert!(!one.may_start_link("user-1", now).await);
            assert!(
                !two.may_start_link("user-1", now).await,
                "the second replica counts the first one's starts"
            );
        })
        .await
        .expect("the shared link allowance test within 120 s");
    }

    async fn a_redis_outage_fails_closed_and_keeps_the_flow_cookie(redis: TestRedis) {
        let idp = idp::start().await;
        // A shared server cannot be stopped for everyone.
        let redis = redis.dedicated().await;
        let (a, b, redis) = oidc_pair(&idp, "OKF_TEST_OIDC_SECRET_OUTAGE", redis).await;
        let mut browser = Browser::default();
        let query = start_on(&a.app, &idp, &mut browser).await;
        let flow = browser.cookie(FLOW_COOKIE).expect("a flow").to_string();
        redis.stop().await;

        let callback = callback_on(&b, &mut browser, &query["state"]).await;
        let started = Browser::default()
            .send_to(&a.app, "GET", "/auth/oidc/example/start", Payload::Empty)
            .await;

        assert_eq!(
            callback.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{}",
            callback.text()
        );
        assert!(callback.text().contains(TRY_LATER));
        assert!(
            callback.set_cookie(FLOW_COOKIE).is_none(),
            "the 503 keeps the flow cookie"
        );
        assert_eq!(browser.cookie(FLOW_COOKIE), Some(flow.as_str()));
        assert_eq!(browser.cookie(COOKIE), None);
        assert_eq!(started.status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(started.set_cookie(FLOW_COOKIE).is_none());
    }

    /// Sets the `default` user's command permissions (`+@all` or `-@all`)
    /// through `admin`, a connection of another user: with `-@all` every
    /// command the apps send is refused (`NOPERM`) before it runs.
    async fn default_user_may(admin: &mut redis::aio::MultiplexedConnection, rule: &str) {
        let _: () = redis::cmd("ACL")
            .arg("SETUSER")
            .arg("default")
            .arg(rule)
            .query_async(admin)
            .await
            .expect("change the default user's permissions");
    }

    async fn a_flow_refused_by_redis_finishes_once_redis_answers_again(redis: TestRedis) {
        // Revoking the default user would break every test sharing it.
        let redis = redis.dedicated().await;
        let idp = idp::start().await;
        let (a, b, redis) = oidc_pair(&idp, "OKF_TEST_OIDC_SECRET_RECOVER", redis).await;
        user_with_password(&a, "alice", "alice@example.com", Some(PASSWORD)).await;
        idp.will_issue("sub-1", Some("alice@example.com"), Some(true));
        let mut browser = Browser::default();
        let query = start_on(&a.app, &idp, &mut browser).await;
        let flow = browser.cookie(FLOW_COOKIE).expect("a flow").to_string();
        // An admin user on its own connection (a plain one: the container
        // has one node), so it can give the permissions back.
        let client = redis::Client::open(redis.url.as_str()).expect("the test redis URL");
        let mut admin = client
            .get_multiplexed_async_connection()
            .await
            .expect("an admin connection");
        let _: () = redis::cmd("ACL")
            .arg("SETUSER")
            .arg("okf-test-admin")
            .arg("on")
            .arg("nopass")
            .arg("+@all")
            .arg("~*")
            .arg("&*")
            .query_async(&mut admin)
            .await
            .expect("create the admin user");
        let _: () = redis::cmd("AUTH")
            .arg("okf-test-admin")
            .arg("unused")
            .query_async(&mut admin)
            .await
            .expect("authenticate as the admin user");
        default_user_may(&mut admin, "-@all").await;

        let refused = callback_on(&b, &mut browser, &query["state"]).await;
        let kept = browser.cookie(FLOW_COOKIE).map(str::to_string);
        default_user_may(&mut admin, "+@all").await;
        let finished = callback_on(&b, &mut browser, &query["state"]).await;

        assert_eq!(
            refused.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{}",
            refused.text()
        );
        assert!(
            refused.set_cookie(FLOW_COOKIE).is_none(),
            "the cookie is kept"
        );
        assert_eq!(kept, Some(flow));
        assert_eq!(
            finished.status,
            StatusCode::SEE_OTHER,
            "{}",
            finished.text()
        );
        assert_eq!(finished.location(), "/ui/#/keys");
        assert!(
            browser.cookie(COOKIE).is_some(),
            "signed in after the refusal"
        );
        assert_eq!(browser.cookie(FLOW_COOKIE), None);
    }

    /// When the swap of the session's link index fails (the index space's
    /// keys refused with `NOPERM`, the flow's allowed), the link start
    /// answers 503 and the flow it just stored is removed at once (issue
    /// #588): no flow key waits out its TTL.
    async fn a_failed_link_index_swap_removes_the_stored_flow(redis: TestRedis) {
        // Restricting the default user would break every test sharing it.
        let redis = redis.dedicated().await;
        let idp = idp::start().await;
        let (a, _b, redis) = oidc_pair(&idp, "OKF_TEST_OIDC_SECRET_ORPHAN", redis).await;
        user_with_password(&a, "alice", "alice@example.com", Some(PASSWORD)).await;
        let mut browser = Browser::signed_in(&a, "alice@example.com", PASSWORD).await;
        let client = redis::Client::open(redis.url.as_str()).expect("the test redis URL");
        let mut admin = client
            .get_multiplexed_async_connection()
            .await
            .expect("an admin connection");
        let _: () = redis::cmd("ACL")
            .arg("SETUSER")
            .arg("okf-test-admin")
            .arg("on")
            .arg("nopass")
            .arg("+@all")
            .arg("~*")
            .arg("&*")
            .query_async(&mut admin)
            .await
            .expect("create the admin user");
        let _: () = redis::cmd("AUTH")
            .arg("okf-test-admin")
            .arg("unused")
            .query_async(&mut admin)
            .await
            .expect("authenticate as the admin user");
        // The apps may touch only the flow and rate-limit keys: the link
        // index's `SET` is refused.
        let _: () = redis::cmd("ACL")
            .arg("SETUSER")
            .arg("default")
            .arg("resetkeys")
            .arg("~*oidc:flow:*")
            .arg("~*ratelimit:*")
            .query_async(&mut admin)
            .await
            .expect("restrict the default user's keys");

        let refused = browser
            .send_to(
                &a.app,
                "POST",
                "/api/v1/me/identities/example/link",
                Payload::Json(json!({})),
            )
            .await;
        let keys = redis_keys(&a.app, &redis.prefix).await;

        assert_eq!(
            refused.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{}",
            refused.text()
        );
        assert_eq!(
            in_space(&keys, &redis.prefix, "oidc:flow"),
            0,
            "no orphan flow stays: {keys:?}"
        );
        assert_eq!(in_space(&keys, &redis.prefix, "oidc:link"), 0, "{keys:?}");
    }

    // `macro_rules!` writes a standalone and a cluster `#[tokio::test]` for
    // every body named (Python: a parametrized test).
    macro_rules! in_both_modes {
        ($($name:ident),* $(,)?) => {
            mod standalone {
                $(
                    #[tokio::test(flavor = "multi_thread")]
                    async fn $name() {
                        let server = crate::db::redis_support::TestRedis::start().await;
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
                        let server = crate::db::redis_support::TestRedis::start_cluster().await;
                        tokio::time::timeout(super::BOUND, super::$name(server))
                            .await
                            .expect(concat!(stringify!($name), " finished within the bound"));
                    }
                )*
            }
        };
    }

    in_both_modes!(
        a_sign_in_started_on_one_replica_completes_on_the_other_once,
        a_link_flow_replaces_the_sessions_previous_one_across_replicas,
        no_raw_session_id_reaches_redis,
        a_link_flow_finishes_only_in_its_session_across_replicas,
        a_reauthentication_belongs_to_its_session_across_replicas,
        a_redis_outage_fails_closed_and_keeps_the_flow_cookie,
        a_flow_refused_by_redis_finishes_once_redis_answers_again,
        a_failed_link_index_swap_removes_the_stored_flow,
    );
}
