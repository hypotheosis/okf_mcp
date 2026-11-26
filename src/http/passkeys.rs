//! Passkeys (spec "Passkeys", plan Decision 13): WebAuthn registration from
//! an emailed setup link or a signed-in console session, and discoverable
//! ("usernameless") sign-in. The relying party id is `public_url`'s host
//! and the origin is `public_url`; webauthn-rs always requires user
//! verification.
//!
//! A ceremony's server-side state waits for at most five minutes, keyed by
//! a random id in the `okf_passkey` cookie, and is taken (single use) by the
//! request that finishes it. Without `[redis]` it is held in this process's
//! memory; with it, in Redis (`shared::Ephemeral`, M16 Task 10), under the
//! cookie's hash, so a ceremony begun on one replica finishes on another
//! and is taken once across all of them (`GETDEL`). A Redis failure answers
//! 503; nothing falls back to memory.
//!
//! No raw console session id or setup token is stored, in memory or in
//! Redis (spec Decision 7). A registration keeps only the hash of what
//! started it: the console session id, or the emailed setup token. It
//! finishes only in a request whose own session, or whose presented token,
//! hashes to that value (compared in constant time), and then uses that
//! request's own id or token.
//!
//! A sign-in checks the assertion and starts the session in one
//! transaction that holds the user's row and the passkey's row
//! (`store::passkeys::lock_for_sign_in`). So the stored counter only moves
//! forward, and no removal of the passkey or password reset can commit
//! between the check and the insert: one that commits first is seen by the
//! check, and a reset that waits ends the session.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Extension, Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use webauthn_rs::prelude::{
    CredentialID, DiscoverableAuthentication, Passkey, PasskeyAuthentication, PasskeyRegistration,
    PublicKeyCredential, RegisterPublicKeyCredential, Url, Uuid, Webauthn, WebauthnBuilder,
    WebauthnError,
};
use webauthn_rs_proto::ResidentKeyRequirement;

use crate::app::App;
use crate::auth::{CredentialKind, Principal};
use crate::config::Config;
use crate::error::OkfError;
use crate::http::api::api_headers;
use crate::http::console_session::{
    append_all, client_ip, cookie, secure_cookies, session_user, set_cookie, ConsoleSession,
    SameSite,
};
use crate::http::limits::ip_key;
use crate::http::pages::safe_return_to;
use crate::http::signin::{
    after_setup, audit, cross_site, notify_method_added, start_in, SignIn, LINK_EXPIRED,
};
use crate::http::{timeout_layer, AppError};
use crate::keys;
use crate::labels::user_label;
// The one form a console session id takes when stored (OIDC link flows use
// it too).
use crate::oidc::session_hash;
use crate::shared::ephemeral::{Ephemeral, Index, IndexSpace, Keyspace, Swap};
use crate::shared::Redis;
use crate::store::email_tokens::{self, Purpose};
use crate::store::passkeys::{self as passkey_rows, SessionRegistration};
use crate::store::sign_in_events::{Method, Outcome};
use crate::store::users::UserAuth;
use crate::telemetry::metrics::Limiter;
use crate::telemetry::subscriber::split_event;

/// The ceremony cookie: `SameSite=Strict`, five minutes.
pub const CEREMONY_COOKIE: &str = "okf_passkey";
/// How long a ceremony may take (spec: a five-minute cookie).
pub const CEREMONY_TTL: Duration = Duration::from_secs(5 * 60);
/// The most ceremonies in progress at once (they start unauthenticated).
pub const MAX_CEREMONIES: usize = 10_000;
/// The longest passkey name, in characters.
pub const MAX_NAME_CHARS: usize = 64;

/// Every refused passkey sign-in says this, whatever the reason.
const SIGN_IN_FAILED: &str = "Passkey sign-in failed; try again";
/// A registration webauthn-rs refused.
const REGISTRATION_FAILED: &str = "The passkey could not be registered; try again";
/// No ceremony, an expired one, or one of another kind (410).
const CEREMONY_EXPIRED: &str = "The passkey request expired; start again";
/// A request another site's page sent (403).
const CROSS_SITE: &str = "passkey requests can only come from this site";

/// The WebAuthn relying party and the ceremonies in progress, held by `App`.
pub struct Passkeys {
    webauthn: Webauthn,
    // In memory without `[redis]`, else in Redis under the cookie's hash.
    ceremonies: Ephemeral<Ceremony>,
    // The ceremony each console session's registration holds, keyed by the
    // hash of the session id: a session has at most one at a time.
    by_session: Index,
}

impl Passkeys {
    /// The relying party for `public_url`.
    ///
    /// # Arguments
    /// * `config` - the server's settings.
    /// * `redis` - the instance's Redis, if `[redis]` is set: the ceremonies
    ///   are then kept there, shared by every replica.
    ///
    /// # Returns
    /// `None` without `public_url`, or when its host is an address (WebAuthn
    /// needs a domain; logged at `info`).
    ///
    /// # Errors
    /// [`OkfError::Config`] if `public_url` does not parse or webauthn-rs
    /// refuses it.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn new(config: &Config, redis: Option<&Redis>) -> Result<Option<Self>, OkfError> {
        let Some(public_url) = config.public_url.as_deref() else {
            return Ok(None);
        };
        // A closure that builds the error, so the two failures below share it.
        let error = |message: String| OkfError::Config {
            path: "public_url".to_string(),
            message,
        };
        let origin = Url::parse(public_url).map_err(|e| error(format!("{public_url:?}: {e}")))?;
        // `domain()` is `None` for an address host such as 127.0.0.1.
        let Some(rp_id) = origin.domain() else {
            tracing::info!(
                "passkeys are off: public_url's host is an address, and WebAuthn needs a domain"
            );
            return Ok(None);
        };
        // The name a browser or password manager shows ("Save a passkey for
        // ..."): the config's product name. A WebAuthn relying party is fixed
        // at start-up, so a `product_name` setting saved later is not used.
        let rp_name = config.auth.default_brand().product_name;
        let webauthn = WebauthnBuilder::new(rp_id, &origin)
            .and_then(|builder| builder.rp_name(&rp_name).build())
            .map_err(|e| error(format!("WebAuthn refuses {public_url:?}: {e}")))?;
        Ok(Some(Self {
            webauthn,
            ceremonies: Ephemeral::new(
                redis,
                Keyspace::PasskeyCeremony,
                CEREMONY_TTL,
                MAX_CEREMONIES,
            ),
            by_session: Index::new(
                redis,
                IndexSpace::PasskeySession,
                CEREMONY_TTL,
                MAX_CEREMONIES,
            ),
        }))
    }
}

/// A ceremony in progress. No `Debug`: it holds the challenges.
/// `Serialize`/`Deserialize` (webauthn-rs' `danger-allow-state-serialisation`)
/// let the Redis store keep it as JSON, server-side; it is never sent to a
/// client, logged or put in a span.
#[derive(Serialize, Deserialize)]
enum Ceremony {
    /// Registering a passkey (boxed: much larger than a sign-in).
    Register(Box<Registration>),
    /// A discoverable sign-in.
    SignIn {
        state: DiscoverableAuthentication,
        return_to: String,
    },
    /// A signed-in session confirming it is its user with one of the user's
    /// passkeys, before deleting the account (issue #355). Only the session
    /// id's hash is kept, as for a registration.
    Confirm {
        state: PasskeyAuthentication,
        user_id: String,
        session_hash: String,
    },
}

/// A registration's state and what to do once it finishes.
#[derive(Serialize, Deserialize)]
struct Registration {
    state: PasskeyRegistration,
    user_id: String,
    pending: Pending,
}

/// What a registration was started with.
#[derive(Serialize, Deserialize)]
struct Pending {
    name: String,
    via: RegisterVia,
    return_to: String,
}

/// Who asked for a registration.
#[derive(Serialize, Deserialize)]
enum RegisterVia {
    /// The setup page. Only the emailed token's SHA-256 ([`keys::hash`], the
    /// form Postgres stores) is kept, never the token (spec Decision 7): the
    /// finish must present the token again, and redeems the presented one.
    Setup { token_hash: [u8; 32] },
    /// A signed-in session, whose other sessions end unless kept. Only the
    /// session id's hash ([`crate::oidc::session_hash`]) is kept, never the id (spec
    /// Decision 7): the finish compares it with the finishing request's own
    /// session and then uses that request's id.
    Session {
        session_hash: String,
        keep_sessions: bool,
    },
}

/// The open routes (no credential), merged only when passkeys are on.
///
/// # Returns
/// The router; every response is `no-store` and `nosniff`.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "router builder: runs once at startup, outside any request, so a span would be an orphan root"
    )
)]
pub(crate) fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/auth/passkey/setup/start", post(setup_start))
        .route("/auth/passkey/setup/finish", post(setup_finish))
        .route("/auth/passkey/signin/start", post(sign_in_start))
        .route("/auth/passkey/signin/finish", post(sign_in_finish))
        .layer(timeout_layer())
        .layer(middleware::map_response(api_headers))
}

/// The signed-in registration routes, merged into `/api/v1` (behind
/// `require_principal`, so a cookie write needs its CSRF header); 404
/// while passkeys are off.
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
    Router::new()
        .route("/me/passkeys/register/start", post(register_start))
        .route("/me/passkeys/register/finish", post(register_finish))
        .route("/me/passkeys/confirm/start", post(confirm_start))
}

/// The passkey state, or 404 while passkeys are off.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
    )
)]
fn enabled(app: &App) -> Result<&Passkeys, OkfError> {
    app.passkeys().ok_or_else(|| OkfError::NotFound {
        path: "passkeys".to_string(),
    })
}

/// The 410 for a missing, expired, used or mismatched ceremony.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
    )
)]
fn expired() -> OkfError {
    OkfError::Gone {
        reason: CEREMONY_EXPIRED.to_string(),
    }
}

/// Logs why webauthn-rs refused a registration, and answers 422 with one
/// message, as a bad passkey name is answered. A registration that fails is
/// bad input about the credential, not a failed sign-in: from a console
/// session a 401 would read as "signed out", and the session is still live
/// (issue #325).
#[tracing::instrument(level = "trace", skip_all)]
fn registration_refused(error: &WebauthnError) -> OkfError {
    split_event!(
        debug,
        {},
        { %error, },
        "WebAuthn refused a registration"
    );
    OkfError::LimitExceeded {
        limit: "passkey",
        reason: REGISTRATION_FAILED.to_string(),
    }
}

/// Logs why webauthn-rs refused, and answers with one message.
#[tracing::instrument(level = "trace", skip_all)]
fn refused(error: &WebauthnError, message: &str) -> OkfError {
    split_event!(
        debug,
        {},
        { %error, },
        "WebAuthn refused"
    );
    OkfError::Unauthenticated {
        reason: message.to_string(),
    }
}

/// A passkey name: trimmed, 1 to 64 characters, no control characters
/// (a NUL, which Postgres refuses, included). Registration and the
/// account page's rename (`PATCH /me/passkeys/{id}`) both check it.
///
/// # Arguments
/// * `raw` - the name as sent.
///
/// # Returns
/// The trimmed name.
///
/// # Errors
/// [`OkfError::LimitExceeded`] with `limit: "passkey_name"`.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) fn passkey_name(raw: &str) -> Result<String, OkfError> {
    let name = raw.trim();
    let length = name.chars().count();
    if length == 0 || length > MAX_NAME_CHARS || name.chars().any(char::is_control) {
        return Err(OkfError::LimitExceeded {
            limit: "passkey_name",
            reason: format!(
                "a passkey name is 1 to {MAX_NAME_CHARS} characters, with no control characters"
            ),
        });
    }
    Ok(name.to_string())
}

/// The WebAuthn user handle: the user's UUID (`users.id` is
/// `gen_random_uuid()::text`).
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
    )
)]
fn user_handle(user_id: &str) -> Result<Uuid, OkfError> {
    Uuid::parse_str(user_id).map_err(|_| OkfError::Refused {
        reason: "this user's id is not a UUID, which a passkey needs".to_string(),
    })
}

/// 429 for the per-address limit, as JSON.
#[tracing::instrument(level = "trace", skip_all)]
fn too_many() -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        Json(serde_json::json!({
            "error": "TooManyRequests",
            "message": "Too many attempts from this address; wait a few minutes.",
        })),
    )
        .into_response()
}

/// Screens an open route's request: 403 when another site's page sent it
/// (login CSRF, as the sign-in forms check), 429 past the per-address
/// sign-in limit (shared with password sign-in).
///
/// # Returns
/// The refusal to send, if any.
#[tracing::instrument(level = "trace", skip_all)]
async fn screen(app: &App, parts: &Parts, limited: bool) -> Option<Response> {
    if cross_site(&parts.headers) {
        let error = OkfError::Forbidden {
            reason: CROSS_SITE.to_string(),
        };
        return Some(AppError(error).into_response());
    }
    let ip = client_ip(parts, &app.config().auth);
    let admitted = !limited
        || app
            .sign_in_limits()
            .sign_in_per_ip
            .try_take(&ip_key(ip), Instant::now())
            .await;
    if !admitted {
        app.metrics().rate_limited(Limiter::SignInPerIp);
    }
    (!admitted).then(too_many)
}

/// The 503 for a full ceremony map (memory only; a Redis failure answers
/// its own 503).
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
    )
)]
fn busy() -> OkfError {
    OkfError::Busy {
        reason: "too many passkey requests in progress; try again shortly".to_string(),
    }
}

/// The console session a registration or confirmation ceremony belongs
/// to, as the key of `Passkeys::by_session`: the stored hash (the ceremony
/// holds no raw id). A session holds one such ceremony at a time, so a
/// confirmation started while a registration is in progress replaces it.
#[tracing::instrument(level = "trace", skip_all)]
fn session_key(ceremony: &Ceremony) -> Option<String> {
    match ceremony {
        Ceremony::Register(registration) => match &registration.pending.via {
            RegisterVia::Session { session_hash, .. } => Some(session_hash.clone()),
            RegisterVia::Setup { .. } => None,
        },
        Ceremony::Confirm { session_hash, .. } => Some(session_hash.clone()),
        Ceremony::SignIn { .. } => None,
    }
}

/// Stores a ceremony and returns the cookie that names it. Each actor
/// holds at most one ceremony at a time, so no one can grow the map
/// beyond the per-address limit on the open starts:
/// - the ceremony the request's old cookie names is dropped (a browser
///   that starts again replaces its previous one);
/// - a console session's new registration replaces that session's previous
///   one, cookie or not.
///
/// # Arguments
/// * `app` - the shared state (for `Secure`).
/// * `passkeys` - the ceremonies.
/// * `headers` - the request's headers (its old ceremony cookie).
/// * `ceremony` - the state to keep.
/// * `now` - the current instant.
///
/// # Errors
/// [`OkfError::Busy`] when [`MAX_CEREMONIES`] are in progress (memory) or
/// Redis fails; [`OkfError::Io`] from the random source.
#[tracing::instrument(level = "trace", skip_all)]
async fn remember(
    app: &App,
    passkeys: &Passkeys,
    headers: &HeaderMap,
    ceremony: Ceremony,
    now: Instant,
) -> Result<HeaderMap, OkfError> {
    if let Some(old) = cookie(headers, CEREMONY_COOKIE) {
        passkeys.ceremonies.remove(old).await?;
    }
    let session = session_key(&ceremony);
    // A server-chosen random id, so no client can pick (or replace) another
    // browser's entry.
    let id = keys::generate_with("")?;
    if !passkeys.ceremonies.insert(&id, ceremony, now).await? {
        return Err(busy());
    }
    if let Some(session) = session {
        // One atomic swap (in memory under the map's lock, in Redis one
        // `SET .. GET`) that hands back what the session held before, so of
        // two concurrent starts by one session, on any replicas, each drops
        // what it replaced and one ceremony is left.
        let swapped = match passkeys.by_session.replace(&session, &id, now).await {
            Ok(swapped) => swapped,
            Err(error) => {
                // Unreachable without its cookie, but not left behind.
                let _ = passkeys.ceremonies.remove(&id).await;
                return Err(error);
            }
        };
        match swapped {
            Swap::Swapped(Some(previous)) => passkeys.ceremonies.remove_stale(&previous).await?,
            Swap::Swapped(None) => {}
            Swap::Full => {
                passkeys.ceremonies.remove(&id).await?;
                return Err(busy());
            }
        }
    }
    let mut headers = HeaderMap::new();
    headers.insert(
        header::SET_COOKIE,
        set_cookie(
            CEREMONY_COOKIE,
            &id,
            CEREMONY_TTL,
            SameSite::Strict,
            secure_cookies(app),
        ),
    );
    Ok(headers)
}

/// Takes (single use) the ceremony the cookie names.
///
/// # Returns
/// The ceremony and the header that clears its cookie.
///
/// # Errors
/// [`OkfError::Gone`] without a cookie or a live ceremony;
/// [`OkfError::Busy`] when Redis fails.
#[tracing::instrument(level = "trace", skip_all)]
async fn take_ceremony(
    app: &App,
    passkeys: &Passkeys,
    headers: &HeaderMap,
    now: Instant,
) -> Result<(Ceremony, HeaderMap), OkfError> {
    let id = cookie(headers, CEREMONY_COOKIE).ok_or_else(expired)?;
    // `GETDEL` in Redis: of two finishes racing on two replicas, one gets it.
    let ceremony = passkeys
        .ceremonies
        .take(id, now)
        .await?
        .ok_or_else(expired)?;
    Ok((ceremony, clear_ceremony_cookie(app)))
}

/// The `Set-Cookie` header that removes the ceremony cookie: sent when a
/// ceremony is taken, and by every successful account deletion
/// (`DELETE /api/v1/me`), whichever way it re-authenticated.
///
/// # Arguments
/// * `app` - the shared state (for `Secure`).
///
/// # Returns
/// The header.
#[tracing::instrument(level = "trace", skip_all)]
pub(crate) fn clear_ceremony_cookie(app: &App) -> HeaderMap {
    let mut clear = HeaderMap::new();
    clear.insert(
        header::SET_COOKIE,
        set_cookie(
            CEREMONY_COOKIE,
            "",
            Duration::ZERO,
            SameSite::Strict,
            secure_cookies(app),
        ),
    );
    clear
}

/// Starts a registration for `user`: the challenge (asking for a
/// discoverable credential) and the ceremony cookie.
#[tracing::instrument(level = "debug", skip_all)]
async fn begin_registration(
    app: &App,
    passkeys: &Passkeys,
    headers: &HeaderMap,
    user: &UserAuth,
    pending: Pending,
) -> Result<Response, OkfError> {
    let handle = user_handle(&user.id)?;
    let existing: Vec<CredentialID> = passkey_rows::for_user(app.accounts().executor(), &user.id)
        .await?
        .into_iter()
        .map(|(_, passkey)| passkey.cred_id().clone())
        .collect();
    let account = user.email.as_deref().unwrap_or(&user.name);
    // `user.name` stays the account; the display name is the label.
    let display = user_label(&user.display_name, user.email.as_deref());
    let (mut challenge, state) = passkeys
        .webauthn
        .start_passkey_registration(
            handle,
            account,
            &display,
            (!existing.is_empty()).then_some(existing),
        )
        .map_err(|error| registration_refused(&error))?;
    // webauthn-rs asks for `residentKey: "discouraged"`; discoverable
    // sign-in needs the credential kept on the authenticator.
    if let Some(selection) = challenge.public_key.authenticator_selection.as_mut() {
        selection.resident_key = Some(ResidentKeyRequirement::Required);
    }
    let ceremony = Ceremony::Register(Box::new(Registration {
        state,
        user_id: user.id.clone(),
        pending,
    }));
    let cookie = remember(app, passkeys, headers, ceremony, Instant::now()).await?;
    Ok((cookie, Json(challenge)).into_response())
}

/// A verified registration: the new passkey, what it was started with, its
/// owner and the header that clears the ceremony cookie.
struct Registered {
    passkey: Passkey,
    pending: Pending,
    user_id: String,
    clear: HeaderMap,
}

/// Takes the registration the cookie names and verifies the credential
/// (no database work, so no transaction is open meanwhile).
///
/// # Errors
/// [`OkfError::Gone`] without a live registration ceremony;
/// [`OkfError::LimitExceeded`] (422) for a credential that does not verify;
/// [`OkfError::Busy`] when Redis fails.
#[tracing::instrument(level = "trace", skip_all)]
async fn finish_registration(
    app: &App,
    passkeys: &Passkeys,
    headers: &HeaderMap,
    credential: &RegisterPublicKeyCredential,
) -> Result<Registered, OkfError> {
    let (ceremony, clear) = take_ceremony(app, passkeys, headers, Instant::now()).await?;
    let Ceremony::Register(registration) = ceremony else {
        return Err(expired());
    };
    // `*registration` moves the struct out of its `Box`.
    let Registration {
        state,
        user_id,
        pending,
    } = *registration;
    let passkey = passkeys
        .webauthn
        .finish_passkey_registration(credential, &state)
        .map_err(|error| registration_refused(&error))?;
    Ok(Registered {
        passkey,
        pending,
        user_id,
        clear,
    })
}

/// `POST /auth/passkey/setup/start`'s body. No `Debug`: it holds the token.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetupStart {
    token: String,
    name: String,
    #[serde(default)]
    return_to: Option<String>,
}

/// `POST /auth/passkey/setup/start`: a registration challenge for the
/// setup link's user. The link is checked, not used.
///
/// # Errors
/// 403 from another site; 404 with passkeys off; 410 for a bad link; 422
/// for a bad name; 429 past the per-address limit.
#[tracing::instrument(level = "debug", skip_all)]
async fn setup_start(
    State(app): State<Arc<App>>,
    parts: Parts,
    Json(body): Json<SetupStart>,
) -> Result<Response, AppError> {
    let passkeys = enabled(&app)?;
    if let Some(refusal) = screen(&app, &parts, true).await {
        return Ok(refusal);
    }
    let name = passkey_name(&body.name)?;
    let user = email_tokens::peek(
        app.accounts().executor(),
        &body.token,
        Purpose::Setup,
        app.clock().now(),
    )
    .await?;
    let pending = Pending {
        name,
        via: RegisterVia::Setup {
            token_hash: keys::hash(&body.token),
        },
        return_to: safe_return_to(body.return_to.as_deref()),
    };
    Ok(begin_registration(&app, passkeys, &parts.headers, &user, pending).await?)
}

/// `POST /auth/passkey/setup/finish`'s body: the setup token again (the
/// ceremony keeps only its hash) and the new credential. No `Debug`: it
/// holds the token.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetupFinish {
    token: String,
    credential: RegisterPublicKeyCredential,
}

/// `POST /auth/passkey/setup/finish`: stores the passkey, uses the link and
/// signs in, all in one transaction, so a failure anywhere leaves the link
/// usable and no passkey or session behind. Answers
/// `{"redirect": <return_to>}`.
///
/// # Errors
/// 403 from another site; 410 for an expired ceremony, a used link, or a
/// token other than the one the ceremony started with; 422 for a credential
/// that does not verify; 409 for a passkey already registered.
#[tracing::instrument(level = "debug", skip_all)]
async fn setup_finish(
    State(app): State<Arc<App>>,
    parts: Parts,
    Json(body): Json<SetupFinish>,
) -> Result<Response, AppError> {
    let passkeys = enabled(&app)?;
    if let Some(refusal) = screen(&app, &parts, false).await {
        return Ok(refusal);
    }
    let Registered {
        passkey,
        pending,
        user_id,
        clear: mut headers,
    } = finish_registration(&app, passkeys, &parts.headers, &body.credential).await?;
    let RegisterVia::Setup { token_hash } = pending.via else {
        return Err(expired().into());
    };
    // The ceremony holds only the token's hash: the presented token must hash
    // to it (constant time), and only the presented token is redeemed below.
    if !keys::matches(&token_hash, &body.token) {
        return Err(OkfError::Gone {
            reason: LINK_EXPIRED.to_string(),
        }
        .into());
    }
    let set_up = SetUp {
        token: &body.token,
        user_id: &user_id,
        passkey: &passkey,
        name: &pending.name,
    };
    // The event is recorded in the link's transaction: nothing runs after
    // its commit but building the response.
    let cookie = use_setup_link(&app, &parts, &set_up).await?;
    append_all(&mut headers, &cookie);
    // The same rule as a password setup: the onboarding guide, unless an
    // OAuth authorization is being continued. `passkey.js` navigates to it.
    let settings = app.settings();
    let redirect = after_setup(&pending.return_to, settings.onboarding_url.as_deref());
    Ok((
        headers,
        Json(serde_json::json!({ "redirect": redirect.target() })),
    )
        .into_response())
}

/// A verified setup-link registration, for [`use_setup_link`]. No `Debug`:
/// it holds the token.
struct SetUp<'a> {
    /// The emailed token.
    token: &'a str,
    /// The user the ceremony was started for.
    user_id: &'a str,
    /// The verified passkey.
    passkey: &'a Passkey,
    /// Its name.
    name: &'a str,
}

/// Uses the setup link, stores the passkey and starts the session in one
/// transaction. `redeem_in` locks the user's row first, then the token
/// (lock order), and the passkey and the session follow under that lock;
/// any error drops `tx`, which rolls everything back.
///
/// # Returns
/// The session's `Set-Cookie` header.
///
/// # Errors
/// [`OkfError::Gone`] for a link used, expired or now naming another user;
/// [`OkfError::Refused`] for a passkey already registered;
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all)]
async fn use_setup_link(
    app: &App,
    parts: &Parts,
    set_up: &SetUp<'_>,
) -> Result<HeaderMap, OkfError> {
    let now = app.clock().now();
    let mut tx = app.accounts().executor().begin().await?;
    let user = email_tokens::redeem_in(&mut tx, set_up.token, Purpose::Setup, now).await?;
    if user.id != set_up.user_id {
        return Err(OkfError::Gone {
            reason: LINK_EXPIRED.to_string(),
        });
    }
    passkey_rows::add_in(&mut tx, &user.id, set_up.passkey, set_up.name, now).await?;
    let signing_in = SignIn {
        user: &user,
        method: Method::Setup,
        password: None,
    };
    let cookie = start_in(app, &mut tx, parts, &signing_in).await?;
    tx.commit().await?;
    tracing::info!(user = %user.id, "passkey added from a setup link");
    Ok(cookie)
}

/// `POST /auth/passkey/signin/start`'s body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SignInStart {
    #[serde(default)]
    return_to: Option<String>,
}

/// `POST /auth/passkey/signin/start`: a discoverable challenge.
///
/// # Errors
/// 403 from another site; 404 with passkeys off; 429 past the per-address
/// limit; 503 when too many ceremonies are in progress.
#[tracing::instrument(level = "debug", skip_all)]
async fn sign_in_start(
    State(app): State<Arc<App>>,
    parts: Parts,
    Json(body): Json<SignInStart>,
) -> Result<Response, AppError> {
    let passkeys = enabled(&app)?;
    if let Some(refusal) = screen(&app, &parts, true).await {
        return Ok(refusal);
    }
    let (mut challenge, state) = passkeys
        .webauthn
        .start_discoverable_authentication()
        .map_err(|error| refused(&error, SIGN_IN_FAILED))?;
    // A button press, not autofill: without conditional mediation the
    // browser shows its modal prompt.
    challenge.mediation = None;
    let return_to = safe_return_to(body.return_to.as_deref());
    let ceremony = Ceremony::SignIn { state, return_to };
    let headers = remember(&app, passkeys, &parts.headers, ceremony, Instant::now()).await?;
    Ok((headers, Json(challenge)).into_response())
}

/// What an assertion proved.
enum Verdict {
    /// Signed in: the session (and its event) is committed.
    SignedIn { cookie: HeaderMap },
    /// Refused, naming the credential's owner when it is known.
    Refused { user_id: Option<String> },
}

/// Checks an assertion and, if it holds, stores the raised counter and
/// starts the session, in one transaction that holds the user's row and
/// the passkey's (`passkey_rows::lock_for_sign_in`). The signature check
/// runs under those locks: it is one public-key verification (well under a
/// millisecond), and it is what makes the check and the insert one step.
///
/// # Errors
/// [`OkfError::Database`]; [`OkfError::Io`] from the random source.
#[tracing::instrument(level = "debug", skip_all)]
async fn verify_and_start(
    app: &App,
    passkeys: &Passkeys,
    parts: &Parts,
    credential: &PublicKeyCredential,
    state: DiscoverableAuthentication,
) -> Result<Verdict, OkfError> {
    let Ok((handle, credential_id)) = passkeys
        .webauthn
        .identify_discoverable_authentication(credential)
    else {
        return Ok(Verdict::Refused { user_id: None });
    };
    // Every early return below drops `tx`, which rolls back and releases
    // the locks; only a sign-in that holds commits.
    let mut tx = app.accounts().executor().begin().await?;
    let Some((user, mut passkey)) = passkey_rows::lock_for_sign_in(&mut tx, credential_id).await?
    else {
        return Ok(Verdict::Refused { user_id: None });
    };
    // webauthn-rs does not check that the user handle names the
    // credential's owner; the handle must be exactly that existing user.
    if Uuid::parse_str(&user.id).ok() != Some(handle) {
        return Ok(Verdict::Refused {
            user_id: Some(user.id),
        });
    }
    let result = match passkeys
        .webauthn
        .finish_discoverable_authentication_v2(credential, state, &passkey)
    {
        Ok(result) => result,
        // The authenticator's counter did not move past the stored one:
        // a replayed assertion, or a cloned authenticator.
        Err(WebauthnError::CredentialPossibleCompromise) => {
            tracing::warn!(user = %user.id, "passkey refused: its counter did not move");
            return Ok(Verdict::Refused {
                user_id: Some(user.id),
            });
        }
        Err(error) => {
            split_event!(
                debug,
                { user = %user.id, },
                { %error, },
                "passkey assertion refused"
            );
            return Ok(Verdict::Refused {
                user_id: Some(user.id),
            });
        }
    };
    // Raises the stored counter and syncs the backup flags; `None` only if
    // the result is for another credential, which `_v2` already refused.
    if passkey.update_credential(&result).is_none() {
        return Ok(Verdict::Refused {
            user_id: Some(user.id),
        });
    }
    passkey_rows::update_in(&mut tx, credential_id, &passkey, app.clock().now()).await?;
    let signing_in = SignIn {
        user: &user,
        method: Method::Passkey,
        password: None,
    };
    let cookie = start_in(app, &mut tx, parts, &signing_in).await?;
    tx.commit().await?;
    Ok(Verdict::SignedIn { cookie })
}

/// `POST /auth/passkey/signin/finish`: signs in. Answers
/// `{"redirect": <return_to>}` with the session cookie.
///
/// # Errors
/// 403 from another site; 410 for an expired or used ceremony; 401 for an
/// assertion that does not verify (recorded as a failed sign-in); 429 past
/// the per-address limit.
#[tracing::instrument(level = "debug", skip_all)]
async fn sign_in_finish(
    State(app): State<Arc<App>>,
    parts: Parts,
    Json(credential): Json<PublicKeyCredential>,
) -> Result<Response, AppError> {
    let passkeys = enabled(&app)?;
    if let Some(refusal) = screen(&app, &parts, true).await {
        return Ok(refusal);
    }
    let (ceremony, mut headers) =
        take_ceremony(&app, passkeys, &parts.headers, Instant::now()).await?;
    let Ceremony::SignIn { state, return_to } = ceremony else {
        return Err(expired().into());
    };
    match verify_and_start(&app, passkeys, &parts, &credential, state).await? {
        // The event is recorded in the sign-in's transaction: nothing runs
        // after its commit but building the response.
        Verdict::SignedIn { cookie } => {
            append_all(&mut headers, &cookie);
            Ok((headers, Json(serde_json::json!({ "redirect": return_to }))).into_response())
        }
        Verdict::Refused { user_id } => {
            let ip = client_ip(&parts, &app.config().auth);
            audit(
                &app,
                user_id.as_deref(),
                Method::Passkey,
                Outcome::Failed,
                ip,
            )
            .await;
            let error = OkfError::Unauthenticated {
                reason: SIGN_IN_FAILED.to_string(),
            };
            // The ceremony was used either way, so its cookie goes too.
            Ok((headers, AppError(error)).into_response())
        }
    }
}

/// The calling session's user id and session, or 403 for a key or grant.
#[tracing::instrument(level = "trace", skip_all)]
fn signed_in(
    principal: &Principal,
    session: Option<Extension<ConsoleSession>>,
) -> Result<(String, ConsoleSession), OkfError> {
    match (&principal.kind, session) {
        (CredentialKind::Session { user_id }, Some(Extension(session))) => {
            Ok((user_id.clone(), session))
        }
        _ => Err(OkfError::Forbidden {
            reason: "adding a passkey needs a console session".to_string(),
        }),
    }
}

/// `POST /me/passkeys/register/start`'s body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegisterStart {
    name: String,
    /// Keep the user's other sessions (the account page's checkbox).
    #[serde(default)]
    keep_sessions: bool,
}

/// `POST /api/v1/me/passkeys/register/start`.
///
/// # Errors
/// 403 for a key or grant; 404 with passkeys off; 422 for a bad name.
#[tracing::instrument(level = "debug", skip_all)]
async fn register_start(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    session: Option<Extension<ConsoleSession>>,
    headers: HeaderMap,
    Json(body): Json<RegisterStart>,
) -> Result<Response, AppError> {
    let (user_id, session) = signed_in(&principal, session)?;
    let passkeys = enabled(&app)?;
    let name = passkey_name(&body.name)?;
    let user = session_user(&app, &user_id)
        .await?
        .ok_or_else(|| OkfError::Unauthenticated {
            reason: "this session's user no longer exists".to_string(),
        })?;
    let pending = Pending {
        name,
        via: RegisterVia::Session {
            session_hash: session_hash(&session.id),
            keep_sessions: body.keep_sessions,
        },
        return_to: "/ui/#/account".to_string(),
    };
    Ok(begin_registration(&app, passkeys, &headers, &user, pending).await?)
}

/// `POST /api/v1/me/passkeys/register/finish`: stores the passkey, ends the
/// user's other sessions unless kept (in the same transaction, which
/// refuses a session ended meanwhile), and mails the notice. 201 with
/// `{"id", "name"}`.
///
/// # Errors
/// 403 for a key, a grant or another session's ceremony; 410 for an expired
/// ceremony; 422 for a credential that does not verify; 401 only for a
/// session that ended meanwhile; 409 for a passkey already registered.
#[tracing::instrument(level = "debug", skip_all)]
async fn register_finish(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    session: Option<Extension<ConsoleSession>>,
    headers: HeaderMap,
    Json(credential): Json<RegisterPublicKeyCredential>,
) -> Result<Response, AppError> {
    let (user_id, session) = signed_in(&principal, session)?;
    let passkeys = enabled(&app)?;
    let Registered {
        passkey,
        pending,
        user_id: owner,
        clear,
    } = finish_registration(&app, passkeys, &headers, &credential).await?;
    let RegisterVia::Session {
        session_hash: started_in,
        keep_sessions,
    } = pending.via
    else {
        return Err(expired().into());
    };
    // The ceremony holds only its session's hash: this request's own session
    // must hash to it (constant time: `ct_eq` gives a `Choice`, which
    // `bool::from` reads once the comparison is done), and only this
    // request's own id is used below.
    let same_session = bool::from(
        started_in
            .as_bytes()
            .ct_eq(session_hash(&session.id).as_bytes()),
    );
    if owner != user_id || !same_session {
        return Err(OkfError::Forbidden {
            reason: "this passkey request belongs to another session".to_string(),
        }
        .into());
    }
    let registration = SessionRegistration {
        user_id: &user_id,
        session_id: &session.id,
        passkey: &passkey,
        name: &pending.name,
        keep_sessions,
        now: app.clock().now(),
    };
    let ended = store_registration(&app, &registration).await?;
    tracing::info!(user = %user_id, ended, "passkey added; other sessions ended unless kept");
    let body = serde_json::json!({
        "id": URL_SAFE_NO_PAD.encode(passkey.cred_id().as_slice()),
        "name": pending.name,
    });
    Ok((StatusCode::CREATED, clear, Json(body)).into_response())
}

/// The refusal of a passkey confirmation that does not verify: a 403, never
/// a 401, which the console would read as "signed out" while the session
/// is still live.
const CONFIRM_FAILED: &str = "the passkey could not confirm it is you; try again";

/// `POST /api/v1/me/passkeys/confirm/start`: a challenge that only the
/// session user's own passkeys can answer, for re-authenticating before a
/// sensitive action (deleting the account, `DELETE /api/v1/me`). The
/// ceremony is bound to the `okf_passkey` cookie and to this session's
/// hash, and replaces any ceremony this session already holds.
///
/// # Errors
/// 403 for a key or grant; 404 with passkeys off; 409 when the user has no
/// passkey; 503 when too many ceremonies are in progress.
#[tracing::instrument(level = "debug", skip_all)]
async fn confirm_start(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    session: Option<Extension<ConsoleSession>>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let (user_id, session) = confirming(&principal, session)?;
    let passkeys = enabled(&app)?;
    // `for_user` gives (name, passkey) pairs; only the passkeys are needed.
    let own: Vec<Passkey> = passkey_rows::for_user(app.accounts().executor(), &user_id)
        .await?
        .into_iter()
        .map(|(_, passkey)| passkey)
        .collect();
    if own.is_empty() {
        return Err(OkfError::Refused {
            reason: "you have no passkey; confirm with your password instead".to_string(),
        }
        .into());
    }
    let (mut challenge, state) = passkeys
        .webauthn
        .start_passkey_authentication(&own)
        .map_err(|error| refused(&error, CONFIRM_FAILED))?;
    // A button press: the browser's modal prompt, not autofill.
    challenge.mediation = None;
    let ceremony = Ceremony::Confirm {
        state,
        user_id,
        session_hash: session_hash(&session.id),
    };
    let headers = remember(&app, passkeys, &headers, ceremony, Instant::now()).await?;
    Ok((headers, Json(challenge)).into_response())
}

/// The calling session's user id and session for a confirmation, or 403
/// for a key or grant.
#[tracing::instrument(level = "trace", skip_all)]
fn confirming(
    principal: &Principal,
    session: Option<Extension<ConsoleSession>>,
) -> Result<(String, ConsoleSession), OkfError> {
    signed_in(principal, session).map_err(|_| OkfError::Forbidden {
        reason: "confirming with a passkey needs a console session".to_string(),
    })
}

/// Checks a passkey assertion against the confirmation this session started
/// ([`confirm_start`]), taking (single use) the ceremony the cookie names.
/// Used by `DELETE /api/v1/me`, which then checks again, under the user's
/// row lock, that the passkey is still the user's.
///
/// # Arguments
/// * `app` - the application.
/// * `headers` - the request's headers (its ceremony cookie).
/// * `session` - the request's console session.
/// * `user_id` - the session's user.
/// * `credential` - the browser's assertion.
///
/// # Returns
/// The asserted passkey's raw credential id. (The caller clears the
/// ceremony cookie when it answers, [`clear_ceremony_cookie`].)
///
/// # Errors
/// [`OkfError::NotFound`] with passkeys off; [`OkfError::Gone`] for a
/// missing, expired, used or other kind of ceremony;
/// [`OkfError::Forbidden`] for another session's or user's ceremony, or an
/// assertion that does not verify (logged at `debug`);
/// [`OkfError::Busy`] when Redis fails.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn confirm(
    app: &App,
    headers: &HeaderMap,
    session: &ConsoleSession,
    user_id: &str,
    credential: &PublicKeyCredential,
) -> Result<Vec<u8>, OkfError> {
    let passkeys = enabled(app)?;
    let (ceremony, _clear) = take_ceremony(app, passkeys, headers, Instant::now()).await?;
    let Ceremony::Confirm {
        state,
        user_id: owner,
        session_hash: started_in,
    } = ceremony
    else {
        return Err(expired());
    };
    // As a registration's finish: this request's own session must hash to
    // the stored one, compared in constant time.
    let same_session = bool::from(
        started_in
            .as_bytes()
            .ct_eq(session_hash(&session.id).as_bytes()),
    );
    if owner != user_id || !same_session {
        return Err(OkfError::Forbidden {
            reason: "this passkey request belongs to another session".to_string(),
        });
    }
    // Checks the signature against the user's passkeys as the start read
    // them, the challenge, the origin, user verification and the counter.
    let result = passkeys
        .webauthn
        .finish_passkey_authentication(credential, &state)
        .map_err(|error| {
            split_event!(
                debug,
                { user = %user_id, },
                { %error, },
                "passkey confirmation refused"
            );
            OkfError::Forbidden {
                reason: CONFIRM_FAILED.to_string(),
            }
        })?;
    Ok(result.cred_id().as_slice().to_vec())
}

/// Stores a session's passkey, ends the other sessions unless kept and
/// queues the notice, in one transaction; nothing runs after its commit
/// but building the response.
///
/// # Returns
/// How many other sessions ended.
///
/// # Errors
/// As `passkey_rows::add_from_session_in`; [`OkfError::Unauthenticated`]
/// once the user is gone.
#[tracing::instrument(level = "debug", skip_all)]
async fn store_registration(
    app: &App,
    registration: &SessionRegistration<'_>,
) -> Result<u64, OkfError> {
    // Read before the transaction (the key cache, else the database), for
    // the notice's address.
    let user = session_user(app, registration.user_id)
        .await?
        .ok_or_else(|| OkfError::Unauthenticated {
            reason: "this session's user no longer exists".to_string(),
        })?;
    let mut tx = app.accounts().executor().begin().await?;
    let ended = passkey_rows::add_from_session_in(&mut tx, registration).await?;
    notify_method_added(app, &mut tx, &user, "a passkey").await?;
    tx.commit().await?;
    Ok(ended)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use chrono::Utc;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;
    use tower::ServiceExt;
    use webauthn_authenticator_rs::prelude::WebauthnAuthenticator;
    use webauthn_authenticator_rs::softpasskey::SoftPasskey;
    use webauthn_rs::prelude::{CreationChallengeResponse, RequestChallengeResponse};

    use super::*;
    use crate::http::api::testing::{
        allow, fail_on, keys as test_keys, login_app, login_config, user_with_password,
        wait_for_lock_waiters, Browser, Payload, Reply, MAIN, PASSWORD,
    };
    use crate::http::console_session::COOKIE;
    use crate::http::router;
    use crate::mcp::testing::{app_with, TestApp};
    use crate::store::passwords::{self, NewPassword};
    use crate::store::sign_in_events::{self, Method, Outcome};
    use crate::store::sign_in_methods;

    /// `login_config`'s origin, which the software authenticator signs for.
    const ORIGIN: &str = "https://okf.example.com";

    fn origin() -> Url {
        Url::parse(ORIGIN).expect("the origin")
    }

    async fn setup_token(test: &TestApp, user: &UserAuth) -> String {
        email_tokens::issue(
            test.app.accounts().executor(),
            &user.id,
            Purpose::Setup,
            Duration::from_secs(3600),
            Utc::now(),
        )
        .await
        .expect("issue a setup token")
    }

    /// Registers `soft` through the setup link, as `passkey.js` does.
    async fn register_by_setup(
        test: &TestApp,
        browser: &mut Browser,
        soft: &mut SoftPasskey,
        token: &str,
    ) -> Reply {
        register_across(&test.app, &test.app, browser, soft, token).await
    }

    /// [`register_by_setup`], started on `start_on` and finished on `finish_on`
    /// (two replicas, or one app twice).
    async fn register_across(
        start_on: &Arc<App>,
        finish_on: &Arc<App>,
        browser: &mut Browser,
        soft: &mut SoftPasskey,
        token: &str,
    ) -> Reply {
        let credential = setup_registration(start_on, browser, soft, token).await;
        finish_setup(finish_on, browser, token, credential).await
    }

    /// Starts a setup-link registration on `start_on` and has `soft` answer
    /// it; the credential is returned unsent.
    async fn setup_registration(
        start_on: &Arc<App>,
        browser: &mut Browser,
        soft: &mut SoftPasskey,
        token: &str,
    ) -> serde_json::Value {
        let body = json!({ "token": token, "name": "Laptop" });
        let start = browser
            .send_to(
                start_on,
                "POST",
                "/auth/passkey/setup/start",
                Payload::Json(body),
            )
            .await;
        assert_eq!(start.status, StatusCode::OK, "{}", start.text());
        let line = start
            .set_cookie(CEREMONY_COOKIE)
            .expect("a ceremony cookie");
        assert!(line.contains("SameSite=Strict"), "{line}");
        assert!(line.contains("Max-Age=300"), "{line}");
        let challenge = start.json();
        assert_eq!(
            challenge["publicKey"]["authenticatorSelection"]["residentKey"],
            "required"
        );
        let challenge: CreationChallengeResponse =
            serde_json::from_value(challenge).expect("a challenge");
        let credential = soft
            .do_registration(origin(), challenge)
            .expect("the authenticator registers");
        serde_json::to_value(&credential).expect("the credential as JSON")
    }

    /// Finishes a setup-link registration on `app`, presenting `token` with
    /// the credential, as `passkey.js` does.
    async fn finish_setup(
        app: &Arc<App>,
        browser: &mut Browser,
        token: &str,
        credential: serde_json::Value,
    ) -> Reply {
        browser
            .send_to(
                app,
                "POST",
                "/auth/passkey/setup/finish",
                Payload::Json(json!({ "token": token, "credential": credential })),
            )
            .await
    }

    /// Alice with a setup-registered passkey, and the authenticator.
    async fn alice_with_passkey(test: &TestApp, password: Option<&str>) -> (UserAuth, SoftPasskey) {
        let alice = user_with_password(test, "alice", "alice@example.com", password).await;
        let token = setup_token(test, &alice).await;
        let mut soft = SoftPasskey::new(true);
        let registered = register_by_setup(test, &mut Browser::default(), &mut soft, &token).await;
        assert_eq!(registered.status, StatusCode::OK, "{}", registered.text());
        (alice, soft)
    }

    /// The assertion for a discoverable sign-in, signed as `handle`'s user
    /// and patched as a browser's own discovery would fill it (the
    /// software authenticator can neither discover nor send a handle).
    async fn assertion_as(
        test: &TestApp,
        browser: &mut Browser,
        soft: &mut SoftPasskey,
        owner: &UserAuth,
        handle: &str,
        return_to: &str,
    ) -> serde_json::Value {
        assertion_on(&test.app, browser, soft, owner, handle, return_to).await
    }

    /// [`assertion_as`], with the sign-in started on `app`.
    async fn assertion_on(
        app: &Arc<App>,
        browser: &mut Browser,
        soft: &mut SoftPasskey,
        owner: &UserAuth,
        handle: &str,
        return_to: &str,
    ) -> serde_json::Value {
        let start = browser
            .send_to(
                app,
                "POST",
                "/auth/passkey/signin/start",
                Payload::Json(json!({ "return_to": return_to })),
            )
            .await;
        assert_eq!(start.status, StatusCode::OK, "{}", start.text());
        let mut challenge = start.json();
        assert!(
            challenge.get("mediation").is_none(),
            "a modal prompt, not autofill"
        );
        let stored = passkey_rows::list(app.accounts().executor(), &owner.id)
            .await
            .expect("list");
        challenge["publicKey"]["allowCredentials"] =
            json!([{ "type": "public-key", "id": stored[0].id }]);
        let challenge: RequestChallengeResponse =
            serde_json::from_value(challenge).expect("a challenge");
        let signed = soft
            .do_authentication(origin(), challenge)
            .expect("the authenticator signs");
        let mut signed = serde_json::to_value(&signed).expect("the assertion as JSON");
        let handle = Uuid::parse_str(handle).expect("user ids are UUIDs");
        signed["response"]["userHandle"] = json!(URL_SAFE_NO_PAD.encode(handle.as_bytes()));
        signed
    }

    async fn assertion(
        test: &TestApp,
        browser: &mut Browser,
        soft: &mut SoftPasskey,
        user: &UserAuth,
        return_to: &str,
    ) -> serde_json::Value {
        assertion_as(test, browser, soft, user, &user.id, return_to).await
    }

    async fn finish(test: &TestApp, browser: &mut Browser, assertion: serde_json::Value) -> Reply {
        finish_on(&test.app, browser, assertion).await
    }

    /// [`finish`] on `app`.
    async fn finish_on(
        app: &Arc<App>,
        browser: &mut Browser,
        assertion: serde_json::Value,
    ) -> Reply {
        browser
            .send_to(
                app,
                "POST",
                "/auth/passkey/signin/finish",
                Payload::Json(assertion),
            )
            .await
    }

    /// The stored signature counter of alice's only passkey.
    async fn stored_counter(test: &TestApp, user: &UserAuth) -> serde_json::Value {
        let listed = passkey_rows::list(test.app.accounts().executor(), &user.id)
            .await
            .expect("list");
        let credential_id = URL_SAFE_NO_PAD.decode(&listed[0].id).expect("base64url");
        let (_, stored) = passkey_rows::find(test.app.accounts().executor(), &credential_id)
            .await
            .expect("find")
            .expect("the passkey");
        serde_json::to_value(&stored).expect("json")["cred"]["counter"].clone()
    }

    async fn sessions_of(test: &TestApp, user: &UserAuth) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM user_sessions WHERE user_id = $1")
            .bind(&user.id)
            .fetch_one(test.app.accounts().executor())
            .await
            .expect("count the sessions")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_setup_link_registers_a_passkey_that_then_signs_in_without_an_email() {
        let test = login_app().await;
        let alice = user_with_password(&test, "alice", "alice@example.com", None).await;
        let token = setup_token(&test, &alice).await;
        let mut soft = SoftPasskey::new(true);
        let mut setup = Browser::default();

        let registered = register_by_setup(&test, &mut setup, &mut soft, &token).await;

        assert_eq!(registered.status, StatusCode::OK, "{}", registered.text());
        assert_eq!(registered.json()["redirect"], "/ui/");
        assert!(setup.cookie(COOKIE).is_some(), "setting up signs in");
        assert_eq!(
            setup.cookie(CEREMONY_COOKIE),
            None,
            "the ceremony cookie is cleared"
        );
        let used = email_tokens::peek(
            test.app.accounts().executor(),
            &token,
            Purpose::Setup,
            Utc::now(),
        )
        .await;
        assert!(used.is_err(), "the setup link is used");
        let listed = passkey_rows::list(test.app.accounts().executor(), &alice.id)
            .await
            .expect("list");
        assert_eq!(listed[0].name, "Laptop");

        for round in 1..=2 {
            let mut browser = Browser::default();
            let signed = assertion(&test, &mut browser, &mut soft, &alice, "/ui/#/keys").await;
            let reply = finish(&test, &mut browser, signed).await;
            assert_eq!(
                reply.status,
                StatusCode::OK,
                "round {round}: {}",
                reply.text()
            );
            assert_eq!(reply.json()["redirect"], "/ui/#/keys");
            let session = browser
                .send(&test, "GET", "/api/v1/session", Payload::Empty)
                .await;
            assert_eq!(session.json()["user"], "alice");
            // The stored counter follows the authenticator's (1, then 2).
            assert_eq!(stored_counter(&test, &alice).await, round);
        }
        let events = sign_in_events::recent(test.app.accounts().executor(), "alice", 5)
            .await
            .expect("events");
        assert!(matches!(
            (&events[0].method, &events[0].outcome),
            (Method::Passkey, Outcome::Ok)
        ));
        assert!(matches!(
            (&events[2].method, &events[2].outcome),
            (Method::Setup, Outcome::Ok)
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_counter_that_did_not_move_is_refused_and_recorded() {
        let test = login_app().await;
        let (alice, mut soft) = alice_with_passkey(&test, None).await;
        sqlx::query(
            "UPDATE user_passkeys SET passkey = jsonb_set(passkey, '{cred,counter}', '1000')",
        )
        .execute(test.app.accounts().executor())
        .await
        .expect("inflate the stored counter");
        let mut browser = Browser::default();

        let signed = assertion(&test, &mut browser, &mut soft, &alice, "/ui/").await;
        let reply = finish(&test, &mut browser, signed).await;

        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{}", reply.text());
        assert_eq!(reply.json()["message"], SIGN_IN_FAILED);
        assert_eq!(browser.cookie(COOKIE), None);
        assert_eq!(stored_counter(&test, &alice).await, 1000, "never lowered");
        let events = sign_in_events::recent(test.app.accounts().executor(), "alice", 5)
            .await
            .expect("events");
        assert!(matches!(
            (&events[0].method, &events[0].outcome),
            (Method::Passkey, Outcome::Failed)
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_user_handle_naming_another_user_is_refused() {
        let test = login_app().await;
        let (alice, mut soft) = alice_with_passkey(&test, None).await;
        let bob = user_with_password(&test, "bob", "bob@example.com", Some(PASSWORD)).await;
        let mut browser = Browser::default();

        let signed = assertion_as(&test, &mut browser, &mut soft, &alice, &bob.id, "/ui/").await;
        let reply = finish(&test, &mut browser, signed).await;

        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{}", reply.text());
        assert_eq!(browser.cookie(COOKIE), None);
        assert_eq!(sessions_of(&test, &bob).await, 0);
        let events = sign_in_events::recent(test.app.accounts().executor(), "alice", 5)
            .await
            .expect("events");
        assert!(matches!(
            (&events[0].method, &events[0].outcome),
            (Method::Passkey, Outcome::Failed)
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_ceremony_works_once_and_only_with_its_cookie() {
        let test = login_app().await;
        let (alice, mut soft) = alice_with_passkey(&test, None).await;
        let mut browser = Browser::default();
        let signed = assertion(&test, &mut browser, &mut soft, &alice, "/ui/").await;
        let ceremony = browser
            .cookie(CEREMONY_COOKIE)
            .expect("a ceremony")
            .to_string();

        let without = finish(&test, &mut Browser::default(), signed.clone()).await;
        let first = finish(&test, &mut browser, signed.clone()).await;
        let replayed = finish(
            &test,
            &mut Browser::with_cookie(CEREMONY_COOKIE, &ceremony),
            signed,
        )
        .await;

        assert_eq!(without.status, StatusCode::GONE);
        assert_eq!(first.status, StatusCode::OK, "{}", first.text());
        assert_eq!(replayed.status, StatusCode::GONE);
        assert_eq!(browser.cookie(CEREMONY_COOKIE), None, "cleared once used");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_ceremony_expires_after_five_minutes_and_is_of_one_kind() {
        let test = login_app().await;
        let passkeys = test.app.passkeys().expect("passkeys are on");
        let start = Instant::now();
        let ceremony = |passkeys: &Passkeys| {
            let (_, state) = passkeys
                .webauthn
                .start_discoverable_authentication()
                .expect("a challenge");
            Ceremony::SignIn {
                state,
                return_to: "/ui/".to_string(),
            }
        };
        // The request a browser sends back with the cookie `remember` set.
        let presenting = |set: HeaderMap| {
            let line = set[header::SET_COOKIE].to_str().expect("text").to_string();
            let pair = line.split(';').next().expect("a pair").to_string();
            let mut headers = HeaderMap::new();
            headers.insert(header::COOKIE, pair.parse().expect("a header"));
            headers
        };
        let late = presenting(
            remember(
                &test.app,
                passkeys,
                &HeaderMap::new(),
                ceremony(passkeys),
                start,
            )
            .await
            .expect("remember"),
        );
        let on_time = presenting(
            remember(
                &test.app,
                passkeys,
                &HeaderMap::new(),
                ceremony(passkeys),
                start,
            )
            .await
            .expect("remember"),
        );

        let after = take_ceremony(&test.app, passkeys, &late, start + CEREMONY_TTL).await;
        let within = take_ceremony(&test.app, passkeys, &on_time, start + CEREMONY_TTL / 2).await;

        assert!(matches!(after, Err(OkfError::Gone { .. })));
        let (taken, clear) = within.expect("a live ceremony");
        assert!(matches!(taken, Ceremony::SignIn { .. }));
        assert!(clear[header::SET_COOKIE]
            .to_str()
            .expect("text")
            .contains("Max-Age=0"));
        // A sign-in ceremony does not finish a registration.
        let mut browser = Browser::default();
        let signed = browser
            .send(
                &test,
                "POST",
                "/auth/passkey/signin/start",
                Payload::Json(json!({})),
            )
            .await;
        assert_eq!(signed.status, StatusCode::OK);
        let credential = json!({
            "id": "AAAA", "rawId": "AAAA", "type": "public-key", "extensions": {},
            "response": { "attestationObject": "AAAA", "clientDataJSON": "AAAA" },
        });
        let crossed = browser
            .send(
                &test,
                "POST",
                "/auth/passkey/setup/finish",
                Payload::Json(json!({ "token": "any", "credential": credential })),
            )
            .await;
        assert_eq!(crossed.status, StatusCode::GONE, "{}", crossed.text());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_hostile_return_to_becomes_the_console() {
        let test = login_app().await;
        let (alice, mut soft) = alice_with_passkey(&test, None).await;

        for hostile in [
            "//evil.example",
            "https://evil.example",
            "/\\evil.example",
            "javascript:alert(1)",
            "%2F%2Fevil.example",
        ] {
            let mut browser = Browser::default();
            let signed = assertion(&test, &mut browser, &mut soft, &alice, hostile).await;
            let reply = finish(&test, &mut browser, signed).await;
            assert_eq!(reply.status, StatusCode::OK, "{hostile}: {}", reply.text());
            assert_eq!(reply.json()["redirect"], "/ui/", "{hostile}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_hostile_return_to_on_the_setup_page_becomes_the_console() {
        let test = login_app().await;
        let alice = user_with_password(&test, "alice", "alice@example.com", None).await;
        let token = setup_token(&test, &alice).await;
        let mut soft = SoftPasskey::new(true);
        let mut browser = Browser::default();
        let body = json!({ "token": token, "name": "Laptop", "return_to": "//evil.example" });

        let start = browser
            .send(
                &test,
                "POST",
                "/auth/passkey/setup/start",
                Payload::Json(body),
            )
            .await;
        let challenge: CreationChallengeResponse =
            serde_json::from_value(start.json()).expect("a challenge");
        let credential = soft
            .do_registration(origin(), challenge)
            .expect("the authenticator registers");
        let done = browser
            .send(
                &test,
                "POST",
                "/auth/passkey/setup/finish",
                Payload::Json(json!({
                    "token": token,
                    "credential": serde_json::to_value(&credential).expect("json"),
                })),
            )
            .await;

        assert_eq!(done.status, StatusCode::OK, "{}", done.text());
        assert_eq!(done.json()["redirect"], "/ui/");
    }

    /// Sets alice up by passkey on an app with `onboarding_url` as given,
    /// starting the ceremony with `return_to`, and returns the finish's
    /// `redirect`.
    async fn setup_redirect(onboarding_url: Option<&str>, return_to: Option<&str>) -> String {
        let mut config = login_config();
        config.auth.onboarding_url = onboarding_url.map(str::to_string);
        let test = app_with(&[("main", MAIN)], config).await;
        let alice = user_with_password(&test, "alice", "alice@example.com", None).await;
        let token = setup_token(&test, &alice).await;
        let mut soft = SoftPasskey::new(true);
        let mut browser = Browser::default();
        let mut body = json!({ "token": token, "name": "Laptop" });
        if let Some(return_to) = return_to {
            body["return_to"] = json!(return_to);
        }
        let start = browser
            .send(
                &test,
                "POST",
                "/auth/passkey/setup/start",
                Payload::Json(body),
            )
            .await;
        let challenge: CreationChallengeResponse =
            serde_json::from_value(start.json()).expect("a challenge");
        let credential = soft
            .do_registration(origin(), challenge)
            .expect("the authenticator registers");
        let credential = serde_json::to_value(&credential).expect("json");
        let done = finish_setup(&test.app, &mut browser, &token, credential).await;
        assert_eq!(done.status, StatusCode::OK, "{}", done.text());
        done.json()["redirect"]
            .as_str()
            .expect("a redirect string")
            .to_string()
    }

    const GUIDE: &str = "https://docs.example.com/start/first-15-minutes/";

    #[tokio::test(flavor = "multi_thread")]
    async fn a_passkey_setup_goes_to_the_onboarding_guide_when_one_is_set() {
        assert_eq!(setup_redirect(Some(GUIDE), None).await, GUIDE);
        assert_eq!(setup_redirect(Some(GUIDE), Some("/ui/#/keys")).await, GUIDE);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_passkey_setup_keeps_an_oauth_continuation_over_the_guide() {
        let oauth = "/oauth/authorize?client_id=abc&state=1";
        assert_eq!(setup_redirect(Some(GUIDE), Some(oauth)).await, oauth);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_passkey_setup_without_a_guide_goes_where_it_was_going() {
        assert_eq!(setup_redirect(None, None).await, "/ui/");
        assert_eq!(setup_redirect(None, Some("/ui/#/keys")).await, "/ui/#/keys");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_passkey_routes_share_the_per_address_limit() {
        let test = login_app().await;
        let alice = user_with_password(&test, "alice", "alice@example.com", None).await;
        let token = setup_token(&test, &alice).await;
        let mut browser = Browser::default();
        let start = || Payload::Json(json!({}));
        // oneshot requests carry no address, so they share one bucket.
        for attempt in 1..=30 {
            let reply = browser
                .send(&test, "POST", "/auth/passkey/signin/start", start())
                .await;
            assert_eq!(reply.status, StatusCode::OK, "attempt {attempt}");
        }

        let sign_in = browser
            .send(&test, "POST", "/auth/passkey/signin/start", start())
            .await;
        let finish = browser
            .send(
                &test,
                "POST",
                "/auth/passkey/signin/finish",
                Payload::Json(json!({
                    "id": "AAAA", "rawId": "AAAA", "type": "public-key", "extensions": {},
                    "response": {
                        "authenticatorData": "AAAA", "clientDataJSON": "AAAA",
                        "signature": "AAAA", "userHandle": null,
                    },
                })),
            )
            .await;
        let setup = browser
            .send(
                &test,
                "POST",
                "/auth/passkey/setup/start",
                Payload::Json(json!({ "token": token, "name": "Laptop" })),
            )
            .await;

        assert_eq!(sign_in.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(finish.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(setup.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(sign_in.json()["error"], "TooManyRequests");
    }

    /// Sends `uri` with the `Sec-Fetch-Site` another site's page would
    /// carry.
    async fn cross_site_post(test: &TestApp, uri: &str, body: serde_json::Value) -> StatusCode {
        let request = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .header("sec-fetch-site", "cross-site")
            .body(Body::from(body.to_string()))
            .expect("a request");
        router(Arc::clone(&test.app), &CancellationToken::new())
            .oneshot(request)
            .await
            .expect("the router answers")
            .status()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_passkey_request_from_another_site_is_refused() {
        let test = login_app().await;
        let alice = user_with_password(&test, "alice", "alice@example.com", None).await;
        let token = setup_token(&test, &alice).await;
        let credential = json!({
            "id": "AAAA", "rawId": "AAAA", "type": "public-key", "extensions": {},
            "response": { "attestationObject": "AAAA", "clientDataJSON": "AAAA" },
        });
        let assertion = json!({
            "id": "AAAA", "rawId": "AAAA", "type": "public-key", "extensions": {},
            "response": {
                "authenticatorData": "AAAA", "clientDataJSON": "AAAA",
                "signature": "AAAA", "userHandle": null,
            },
        });

        let statuses = [
            cross_site_post(
                &test,
                "/auth/passkey/setup/start",
                json!({ "token": token, "name": "Laptop" }),
            )
            .await,
            cross_site_post(
                &test,
                "/auth/passkey/setup/finish",
                json!({ "token": token, "credential": credential }),
            )
            .await,
            cross_site_post(&test, "/auth/passkey/signin/start", json!({})).await,
            cross_site_post(&test, "/auth/passkey/signin/finish", assertion).await,
        ];

        assert_eq!(statuses, [StatusCode::FORBIDDEN; 4]);
        let link = email_tokens::peek(
            test.app.accounts().executor(),
            &token,
            Purpose::Setup,
            Utc::now(),
        )
        .await;
        assert!(link.is_ok(), "the link is untouched");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_setup_that_fails_stores_no_passkey_and_keeps_the_link() {
        let test = login_app().await;
        let alice = user_with_password(&test, "alice", "alice@example.com", None).await;
        let token = setup_token(&test, &alice).await;
        let pool = test.app.accounts().executor();
        // The session insert fails, after the link was used and the passkey
        // stored in the same transaction.
        fail_on(pool, "BEFORE INSERT ON user_sessions FOR EACH ROW").await;
        let mut soft = SoftPasskey::new(true);

        let failed = register_by_setup(&test, &mut Browser::default(), &mut soft, &token).await;
        let link = email_tokens::peek(pool, &token, Purpose::Setup, Utc::now()).await;
        let stored = passkey_rows::list(pool, &alice.id).await.expect("list");
        allow(pool, "user_sessions").await;
        let retried = register_by_setup(&test, &mut Browser::default(), &mut soft, &token).await;

        assert_eq!(failed.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(link.is_ok(), "the link was not used");
        assert!(stored.is_empty(), "no passkey was stored");
        assert_eq!(retried.status, StatusCode::OK, "{}", retried.text());
    }

    /// The trigger that makes ending the browser's old session fail.
    const FAIL_SESSION_DELETE: &str = "BEFORE DELETE ON user_sessions FOR EACH STATEMENT";

    #[tokio::test(flavor = "multi_thread")]
    async fn a_passkey_sign_in_whose_old_session_cannot_end_changes_nothing() {
        let test = login_app().await;
        let (alice, mut soft) = alice_with_passkey(&test, Some(PASSWORD)).await;
        let pool = test.app.accounts().executor();
        let mut browser = Browser::signed_in(&test, "alice@example.com", PASSWORD).await;
        let old = browser.cookie(COOKIE).expect("a session").to_string();
        let before = sessions_of(&test, &alice).await;
        fail_on(pool, FAIL_SESSION_DELETE).await;

        let signed = assertion(&test, &mut browser, &mut soft, &alice, "/ui/").await;
        let failed = finish(&test, &mut browser, signed).await;
        let counter = stored_counter(&test, &alice).await;
        let after_failure = sessions_of(&test, &alice).await;
        allow(pool, "user_sessions").await;
        let signed = assertion(&test, &mut browser, &mut soft, &alice, "/ui/").await;
        let retried = finish(&test, &mut browser, signed).await;
        let session = browser
            .send(&test, "GET", "/api/v1/session", Payload::Empty)
            .await;

        // All or nothing: the counter, the new session and the old one's
        // end commit together.
        assert_eq!(failed.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(failed.set_cookie(COOKIE).is_none());
        assert_eq!(
            counter, 0,
            "the registration's counter, not the failed sign-in's"
        );
        assert_eq!(after_failure, before);
        assert_eq!(retried.status, StatusCode::OK, "{}", retried.text());
        assert_ne!(browser.cookie(COOKIE), Some(old.as_str()));
        assert_eq!(session.json()["user"], "alice");
        assert_eq!(
            sessions_of(&test, &alice).await,
            before,
            "the old one ended"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_passkey_setup_whose_old_session_cannot_end_keeps_the_link() {
        let test = login_app().await;
        let alice = user_with_password(&test, "alice", "alice@example.com", None).await;
        user_with_password(&test, "bob", "bob@example.com", Some(PASSWORD)).await;
        let token = setup_token(&test, &alice).await;
        let pool = test.app.accounts().executor();
        // The browser is still signed in as someone else.
        let mut browser = Browser::signed_in(&test, "bob@example.com", PASSWORD).await;
        let mut soft = SoftPasskey::new(true);
        fail_on(pool, FAIL_SESSION_DELETE).await;

        let failed = register_by_setup(&test, &mut browser, &mut soft, &token).await;
        let link = email_tokens::peek(pool, &token, Purpose::Setup, Utc::now()).await;
        let stored = passkey_rows::list(pool, &alice.id).await.expect("list");
        allow(pool, "user_sessions").await;
        let retried = register_by_setup(&test, &mut browser, &mut soft, &token).await;
        let session = browser
            .send(&test, "GET", "/api/v1/session", Payload::Empty)
            .await;

        assert_eq!(failed.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(failed.set_cookie(COOKIE).is_none());
        assert!(link.is_ok(), "the link was not used");
        assert!(stored.is_empty(), "no passkey was stored");
        assert_eq!(retried.status, StatusCode::OK, "{}", retried.text());
        assert_eq!(session.json()["user"], "alice");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn one_actors_repeated_starts_hold_one_ceremony() {
        let test = login_app().await;
        user_with_password(&test, "alice", "alice@example.com", Some(PASSWORD)).await;
        let passkeys = test.app.passkeys().expect("passkeys are on");
        let mut browser = Browser::default();
        let anonymous = || Payload::Json(json!({}));

        // A browser that starts again (its cookie kept) replaces its ceremony.
        for _ in 0..5 {
            let reply = browser
                .send(&test, "POST", "/auth/passkey/signin/start", anonymous())
                .await;
            assert_eq!(reply.status, StatusCode::OK);
        }
        let after_one_browser = passkeys.ceremonies.len();
        let other = Browser::default()
            .send(&test, "POST", "/auth/passkey/signin/start", anonymous())
            .await;
        let after_another = passkeys.ceremonies.len();
        // A session holds one registration, even when it drops the cookie.
        let session = Browser::signed_in(&test, "alice@example.com", PASSWORD).await;
        for _ in 0..5 {
            let mut cookieless = Browser::with_cookie(COOKIE, session.cookie(COOKIE).expect("id"));
            cookieless.csrf.clone_from(&session.csrf);
            let reply = cookieless
                .send(
                    &test,
                    "POST",
                    "/api/v1/me/passkeys/register/start",
                    Payload::Json(json!({ "name": "Phone" })),
                )
                .await;
            assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
        }
        let after_one_session = passkeys.ceremonies.len();
        let mut second = Browser::signed_in(&test, "alice@example.com", PASSWORD).await;
        let started = second
            .send(
                &test,
                "POST",
                "/api/v1/me/passkeys/register/start",
                Payload::Json(json!({ "name": "Laptop" })),
            )
            .await;

        assert_eq!(after_one_browser, 1);
        assert_eq!(other.status, StatusCode::OK, "others can still start");
        assert_eq!(after_another, 2);
        assert_eq!(after_one_session, 3);
        assert_eq!(started.status, StatusCode::OK, "others can still start");
        assert_eq!(passkeys.ceremonies.len(), 4);
    }

    /// Holds `table` in `ACCESS EXCLUSIVE` mode until the returned
    /// transaction ends, so every statement that touches it waits.
    async fn lock_table(
        pool: &sqlx::PgPool,
        table: &str,
    ) -> sqlx::Transaction<'static, sqlx::Postgres> {
        let mut holder = pool.begin().await.expect("begin");
        // `AssertSqlSafe`: `table` is a literal of the calling test.
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "LOCK TABLE {table} IN ACCESS EXCLUSIVE MODE"
        )))
        .execute(&mut *holder)
        .await
        .expect("lock the table");
        holder
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_sign_in_waiting_on_the_event_table_commits_nothing_until_it_returns_its_cookie() {
        let test = Arc::new(login_app().await);
        let (alice, mut soft) = alice_with_passkey(&test, None).await;
        let before = sessions_of(&test, &alice).await;
        let mut browser = Browser::default();
        let signed = assertion(&test, &mut browser, &mut soft, &alice, "/ui/").await;
        let pool = test.app.accounts().executor().clone();
        let holder = lock_table(&pool, "sign_in_events").await;
        let sign_in = tokio::spawn({
            let test = Arc::clone(&test);
            async move {
                let reply = finish(&test, &mut browser, signed).await;
                (reply, browser)
            }
        });
        wait_for_lock_waiters(&pool, 1).await;
        // Blocked on the event, inside its transaction: nothing committed.
        let while_blocked = sessions_of(&test, &alice).await;

        holder.commit().await.expect("release the table");
        let (reply, mut browser) = sign_in.await.expect("the sign-in task");
        let session = browser
            .send(&test, "GET", "/api/v1/session", Payload::Empty)
            .await;

        assert_eq!(while_blocked, before);
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
        assert!(reply.set_cookie(COOKIE).is_some());
        assert_eq!(session.json()["user"], "alice");
        let events = sign_in_events::recent(test.app.accounts().executor(), "alice", 1)
            .await
            .expect("events");
        assert!(matches!(
            (&events[0].method, &events[0].outcome),
            (Method::Passkey, Outcome::Ok)
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_registration_waiting_on_the_mail_queue_commits_nothing_until_it_answers() {
        let test = Arc::new(login_app().await);
        let alice = user_with_password(&test, "alice", "alice@example.com", Some(PASSWORD)).await;
        let mut browser = Browser::signed_in(&test, "alice@example.com", PASSWORD).await;
        let mut soft = SoftPasskey::new(true);
        let credential =
            registration_from_session(&test, &mut browser, &mut soft, json!({ "name": "Phone" }))
                .await;
        let pool = test.app.accounts().executor().clone();
        let holder = lock_table(&pool, "apalis.jobs").await;
        let finish = tokio::spawn({
            let test = Arc::clone(&test);
            async move {
                browser
                    .send(
                        &test,
                        "POST",
                        "/api/v1/me/passkeys/register/finish",
                        Payload::Json(credential),
                    )
                    .await
            }
        });
        wait_for_lock_waiters(&pool, 1).await;
        let while_blocked = passkey_rows::list(&pool, &alice.id).await.expect("list");

        holder.commit().await.expect("release the queue");
        let reply = finish.await.expect("the registration task");

        assert!(
            while_blocked.is_empty(),
            "nothing committed while the notice waited"
        );
        assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text());
        assert_eq!(
            passkey_rows::list(&pool, &alice.id)
                .await
                .expect("list")
                .len(),
            1
        );
        let queued: i64 = sqlx::query_scalar("SELECT count(*) FROM apalis.jobs")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(queued, 1, "the notice committed with the passkey");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_notice_that_cannot_be_queued_does_not_undo_the_passkey() {
        let test = login_app().await;
        let alice = user_with_password(&test, "alice", "alice@example.com", Some(PASSWORD)).await;
        let mut browser = Browser::signed_in(&test, "alice@example.com", PASSWORD).await;
        let mut soft = SoftPasskey::new(true);
        let pool = test.app.accounts().executor();
        fail_on(pool, "BEFORE INSERT ON apalis.jobs FOR EACH STATEMENT").await;

        let done =
            register_from_session(&test, &mut browser, &mut soft, json!({ "name": "Phone" })).await;
        allow(pool, "apalis.jobs").await;

        assert_eq!(done.status, StatusCode::CREATED, "{}", done.text());
        assert_eq!(
            passkey_rows::list(pool, &alice.id)
                .await
                .expect("list")
                .len(),
            1
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_sign_in_queued_on_a_passkey_removal_gets_no_session() {
        let test = Arc::new(login_app().await);
        let (alice, mut soft) = alice_with_passkey(&test, Some(PASSWORD)).await;
        let before = sessions_of(&test, &alice).await;
        let mut browser = Browser::default();
        let signed = assertion(&test, &mut browser, &mut soft, &alice, "/ui/").await;
        let pool = test.app.accounts().executor().clone();
        // A removal in progress, as `passkeys::remove` runs it: the user's
        // row locked first, then the passkey deleted, not yet committed.
        let mut removal = pool.begin().await.expect("begin");
        sign_in_methods::count(&mut removal, &alice.id)
            .await
            .expect("lock alice");
        sqlx::query("DELETE FROM user_passkeys WHERE user_id = $1")
            .bind(&alice.id)
            .execute(&mut *removal)
            .await
            .expect("delete the passkey");
        // `tokio::spawn` runs the sign-in on its own task, like
        // `asyncio.create_task`; `move` hands it owned copies.
        let sign_in = tokio::spawn({
            let test = Arc::clone(&test);
            async move {
                let reply = finish(&test, &mut browser, signed).await;
                (reply, browser)
            }
        });
        wait_for_lock_waiters(&pool, 1).await;

        removal.commit().await.expect("remove the passkey");
        let (reply, browser) = sign_in.await.expect("the sign-in task");

        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{}", reply.text());
        assert_eq!(browser.cookie(COOKIE), None);
        assert_eq!(sessions_of(&test, &alice).await, before, "no new session");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_sign_in_ahead_of_a_reset_has_its_session_ended_by_it() {
        let test = Arc::new(login_app().await);
        let (alice, mut soft) = alice_with_passkey(&test, Some(PASSWORD)).await;
        let mut browser = Browser::default();
        let signed = assertion(&test, &mut browser, &mut soft, &alice, "/ui/").await;
        let pool = test.app.accounts().executor().clone();
        // Hold the passkey's row (as another sign-in with it would), so the
        // sign-in stops holding alice's row, and the reset queues behind it.
        let mut holder = pool.begin().await.expect("begin");
        sqlx::query("SELECT 1 FROM user_passkeys WHERE user_id = $1 FOR UPDATE")
            .bind(&alice.id)
            .execute(&mut *holder)
            .await
            .expect("hold the passkey");
        let sign_in = tokio::spawn({
            let test = Arc::clone(&test);
            async move {
                let reply = finish(&test, &mut browser, signed).await;
                (reply, browser)
            }
        });
        wait_for_lock_waiters(&pool, 1).await;
        let reset = tokio::spawn({
            let pool = pool.clone();
            let user_id = alice.id.clone();
            async move { passwords::reset(&pool, &user_id, "amber-quarry-signal-77", Utc::now()).await }
        });
        wait_for_lock_waiters(&pool, 2).await;

        holder.commit().await.expect("release the passkey");
        let (reply, mut browser) = sign_in.await.expect("the sign-in task");
        reset.await.expect("the reset task").expect("reset");
        let session = browser
            .send(&test, "GET", "/api/v1/session", Payload::Empty)
            .await;

        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
        assert_eq!(
            session.status,
            StatusCode::UNAUTHORIZED,
            "the reset ended it"
        );
        assert_eq!(sessions_of(&test, &alice).await, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_signed_in_user_adds_a_passkey_and_their_other_sessions_end() {
        let test = login_app().await;
        user_with_password(&test, "alice", "alice@example.com", Some(PASSWORD)).await;
        let mut here = Browser::signed_in(&test, "alice@example.com", PASSWORD).await;
        let mut elsewhere = Browser::signed_in(&test, "alice@example.com", PASSWORD).await;
        let mut soft = SoftPasskey::new(true);

        let done =
            register_from_session(&test, &mut here, &mut soft, json!({ "name": "Phone" })).await;

        assert_eq!(done.status, StatusCode::CREATED, "{}", done.text());
        assert_eq!(done.json()["name"], "Phone");
        assert_eq!(
            here.cookie(CEREMONY_COOKIE),
            None,
            "the ceremony cookie is cleared"
        );
        let still = here
            .send(&test, "GET", "/api/v1/session", Payload::Empty)
            .await;
        let ended = elsewhere
            .send(&test, "GET", "/api/v1/session", Payload::Empty)
            .await;
        assert_eq!(still.status, StatusCode::OK);
        assert_eq!(ended.status, StatusCode::UNAUTHORIZED);
        let queued: i64 = sqlx::query_scalar("SELECT count(*) FROM apalis.jobs")
            .fetch_one(test.app.accounts().executor())
            .await
            .expect("count");
        assert_eq!(queued, 1, "the method-added notice");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn keep_sessions_keeps_the_other_sessions() {
        let test = login_app().await;
        user_with_password(&test, "alice", "alice@example.com", Some(PASSWORD)).await;
        let mut here = Browser::signed_in(&test, "alice@example.com", PASSWORD).await;
        let mut elsewhere = Browser::signed_in(&test, "alice@example.com", PASSWORD).await;
        let mut soft = SoftPasskey::new(true);
        let body = json!({ "name": "Phone", "keep_sessions": true });

        let done = register_from_session(&test, &mut here, &mut soft, body).await;

        assert_eq!(done.status, StatusCode::CREATED, "{}", done.text());
        let kept = elsewhere
            .send(&test, "GET", "/api/v1/session", Payload::Empty)
            .await;
        assert_eq!(kept.status, StatusCode::OK);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_registration_options_show_the_label_and_keep_the_email_as_the_account() {
        let test = login_app().await;
        user_with_password(&test, "alice", "alice@example.com", Some(PASSWORD)).await;
        let mut browser = Browser::signed_in(&test, "alice@example.com", PASSWORD).await;
        let mut options = Vec::new();
        for display_name in ["Alice Example", "", "<b>x</b>"] {
            sqlx::query("UPDATE users SET display_name = $1 WHERE name = 'alice'")
                .bind(display_name)
                .execute(test.app.accounts().executor())
                .await
                .expect("display name");
            // The signed-in user comes from the user cache; a raw update
            // sends no change message, so a catch-up (what a reconnect
            // does) makes it read the user again.
            test.app.apply_reconnect().await;

            let start = browser
                .send(
                    &test,
                    "POST",
                    "/api/v1/me/passkeys/register/start",
                    Payload::Json(json!({ "name": "Phone" })),
                )
                .await;

            assert_eq!(start.status, StatusCode::OK, "{}", start.text());
            options.push(start.json()["publicKey"]["user"].clone());
        }

        let names: Vec<_> = options.iter().map(|user| user["name"].clone()).collect();
        let displays: Vec<_> = options
            .iter()
            .map(|user| user["displayName"].clone())
            .collect();
        assert_eq!(
            names, ["alice@example.com"; 3],
            "the account stays the email"
        );
        // The label, the email's local part when unset (never the slug as a
        // name), and hostile text as plain data (it is JSON, not markup).
        assert_eq!(displays, ["Alice Example", "alice", "<b>x</b>"]);
    }

    /// Starts a registration from `browser`'s session and has `soft` answer
    /// it; the finish request is returned unsent.
    async fn registration_from_session(
        test: &TestApp,
        browser: &mut Browser,
        soft: &mut SoftPasskey,
        body: serde_json::Value,
    ) -> serde_json::Value {
        registration_on(&test.app, browser, soft, body).await
    }

    /// [`registration_from_session`], started on `app`.
    async fn registration_on(
        app: &Arc<App>,
        browser: &mut Browser,
        soft: &mut SoftPasskey,
        body: serde_json::Value,
    ) -> serde_json::Value {
        let start = browser
            .send_to(
                app,
                "POST",
                "/api/v1/me/passkeys/register/start",
                Payload::Json(body),
            )
            .await;
        assert_eq!(start.status, StatusCode::OK, "{}", start.text());
        let challenge: CreationChallengeResponse =
            serde_json::from_value(start.json()).expect("a challenge");
        let credential = soft
            .do_registration(origin(), challenge)
            .expect("the authenticator registers");
        serde_json::to_value(&credential).expect("json")
    }

    async fn register_from_session(
        test: &TestApp,
        browser: &mut Browser,
        soft: &mut SoftPasskey,
        body: serde_json::Value,
    ) -> Reply {
        let credential = registration_from_session(test, browser, soft, body).await;
        browser
            .send(
                test,
                "POST",
                "/api/v1/me/passkeys/register/finish",
                Payload::Json(credential),
            )
            .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_passkey_added_by_a_session_a_reset_ended_is_refused() {
        let test = Arc::new(login_app().await);
        let alice = user_with_password(&test, "alice", "alice@example.com", Some(PASSWORD)).await;
        let mut browser = Browser::signed_in(&test, "alice@example.com", PASSWORD).await;
        let mut soft = SoftPasskey::new(true);
        let credential =
            registration_from_session(&test, &mut browser, &mut soft, json!({ "name": "Phone" }))
                .await;
        let pool = test.app.accounts().executor().clone();
        let new_password = NewPassword::hash("amber-quarry-signal-77")
            .await
            .expect("hash");
        // A reset that has locked alice's row but not yet ended her
        // sessions: the registration passes the session check of the
        // middleware, then queues on her row.
        let mut reset = pool.begin().await.expect("begin");
        sqlx::query("SELECT 1 FROM users WHERE id = $1 FOR UPDATE")
            .bind(&alice.id)
            .execute(&mut *reset)
            .await
            .expect("lock alice");
        let finish = tokio::spawn({
            let test = Arc::clone(&test);
            async move {
                browser
                    .send(
                        &test,
                        "POST",
                        "/api/v1/me/passkeys/register/finish",
                        Payload::Json(credential),
                    )
                    .await
            }
        });
        wait_for_lock_waiters(&pool, 1).await;

        passwords::reset_in(&mut reset, &alice.id, &new_password, Utc::now())
            .await
            .expect("reset");
        reset.commit().await.expect("commit the reset");
        let reply = finish.await.expect("the registration task");

        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{}", reply.text());
        let stored = passkey_rows::list(&pool, &alice.id).await.expect("list");
        assert!(stored.is_empty(), "no passkey outlives the reset");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn registering_from_the_api_needs_a_session_and_its_csrf_token() {
        let test = login_app().await;
        user_with_password(&test, "alice", "alice@example.com", Some(PASSWORD)).await;
        let mut browser = Browser::signed_in(&test, "alice@example.com", PASSWORD).await;
        browser.csrf = None;
        let mut keyed = Browser::default();
        keyed.bearer = Some(test_keys(&test).await.write);
        let body = || Payload::Json(json!({ "name": "Phone" }));

        let no_csrf = browser
            .send(&test, "POST", "/api/v1/me/passkeys/register/start", body())
            .await;
        let key = keyed
            .send(&test, "POST", "/api/v1/me/passkeys/register/start", body())
            .await;
        let key_finish = keyed
            .send(
                &test,
                "POST",
                "/api/v1/me/passkeys/register/finish",
                Payload::Json(json!({})),
            )
            .await;

        assert_eq!(no_csrf.status, StatusCode::FORBIDDEN);
        assert_eq!(key.status, StatusCode::FORBIDDEN);
        assert_ne!(key_finish.status, StatusCode::CREATED);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_credential_that_does_not_verify_is_422_and_the_session_lives_on() {
        let test = login_app().await;
        let alice = user_with_password(&test, "alice", "alice@example.com", Some(PASSWORD)).await;
        let mut browser = Browser::signed_in(&test, "alice@example.com", PASSWORD).await;
        let mut soft = SoftPasskey::new(true);
        let mut credential =
            registration_from_session(&test, &mut browser, &mut soft, json!({ "name": "Phone" }))
                .await;
        // The client data claims a sign-in (`webauthn.get`), not a
        // registration, so webauthn-rs refuses it.
        let field = &mut credential["response"]["clientDataJSON"];
        let client_data = URL_SAFE_NO_PAD
            .decode(field.as_str().expect("client data"))
            .expect("base64url");
        let forged = String::from_utf8(client_data)
            .expect("client data is JSON text")
            .replace("webauthn.create", "webauthn.get");
        *field = json!(URL_SAFE_NO_PAD.encode(forged));

        let reply = browser
            .send(
                &test,
                "POST",
                "/api/v1/me/passkeys/register/finish",
                Payload::Json(credential),
            )
            .await;

        // A bad credential, never a 401: the console reads 401 as "signed
        // out", and this session is still live.
        assert_eq!(
            reply.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{}",
            reply.text()
        );
        assert_eq!(reply.json()["message"], REGISTRATION_FAILED);
        let still = browser
            .send(&test, "GET", "/api/v1/session", Payload::Empty)
            .await;
        assert_eq!(still.status, StatusCode::OK);
        let stored = passkey_rows::list(test.app.accounts().executor(), &alice.id)
            .await
            .expect("list");
        assert!(stored.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_ceremony_of_another_session_is_refused() {
        let test = login_app().await;
        let alice = user_with_password(&test, "alice", "alice@example.com", Some(PASSWORD)).await;
        let mut first = Browser::signed_in(&test, "alice@example.com", PASSWORD).await;
        let second = Browser::signed_in(&test, "alice@example.com", PASSWORD).await;
        let mut soft = SoftPasskey::new(true);
        let credential =
            registration_from_session(&test, &mut first, &mut soft, json!({ "name": "Phone" }))
                .await;
        // The second session presents the first one's ceremony cookie.
        let cookies = format!(
            "{COOKIE}={}; {CEREMONY_COOKIE}={}",
            second.cookie(COOKIE).expect("a session"),
            first.cookie(CEREMONY_COOKIE).expect("a ceremony"),
        );
        let request = Request::builder()
            .method("POST")
            .uri("/api/v1/me/passkeys/register/finish")
            .header("content-type", "application/json")
            .header("cookie", cookies)
            .header(
                crate::http::console_session::CSRF_HEADER,
                second.csrf.as_deref().expect("a token"),
            )
            .body(Body::from(credential.to_string()))
            .expect("a request");

        let reply = router(Arc::clone(&test.app), &CancellationToken::new())
            .oneshot(request)
            .await
            .expect("the router answers");

        assert_eq!(reply.status(), StatusCode::FORBIDDEN);
        let stored = passkey_rows::list(test.app.accounts().executor(), &alice.id)
            .await
            .expect("list");
        assert!(stored.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bad_name_is_422() {
        let test = login_app().await;
        let alice = user_with_password(&test, "alice", "alice@example.com", None).await;
        let token = setup_token(&test, &alice).await;

        for name in ["  ".to_string(), "a\u{0}b".to_string(), "x".repeat(65)] {
            let reply = Browser::default()
                .send(
                    &test,
                    "POST",
                    "/auth/passkey/setup/start",
                    Payload::Json(json!({ "token": token, "name": name })),
                )
                .await;
            assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "{name:?}");
            assert_eq!(reply.json()["error"], "LimitExceeded", "{name:?}");
        }
        assert_eq!(passkey_name(&"x".repeat(64)).expect("64 is fine").len(), 64);
        assert_eq!(passkey_name(" Laptop ").expect("trimmed"), "Laptop");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn passkeys_are_off_for_an_address_public_url() {
        let test = app_with(
            &[("main", MAIN)],
            Config {
                public_url: Some("http://127.0.0.1:8080".to_string()),
                ..Config::default()
            },
        )
        .await;
        let mut browser = Browser::default();

        let start = browser
            .send(
                &test,
                "POST",
                "/auth/passkey/signin/start",
                Payload::Json(json!({})),
            )
            .await;
        let page = browser.send(&test, "GET", "/signin", Payload::Empty).await;

        assert!(test.app.passkeys().is_none());
        assert_eq!(start.status, StatusCode::NOT_FOUND);
        assert!(!page.text().contains("signin-passkey"));
        assert!(!page.text().contains("<script"));
        assert!(page.text().contains("signin-form"), "passwords still work");
        assert!(Passkeys::new(&login_config(), None)
            .expect("a domain")
            .is_some());
        assert!(Passkeys::new(&Config::default(), None)
            .expect("no url")
            .is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_sign_in_and_setup_pages_offer_a_passkey_and_load_its_script() {
        let test = login_app().await;
        let alice = user_with_password(&test, "alice", "alice@example.com", None).await;
        let token = setup_token(&test, &alice).await;
        let mut browser = Browser::default();

        let signin = browser
            .send(&test, "GET", "/signin", Payload::Empty)
            .await
            .text();
        let setup = browser
            .send(
                &test,
                "GET",
                &format!("/account/setup?token={token}"),
                Payload::Empty,
            )
            .await
            .text();
        let script = browser
            .send(&test, "GET", "/ui/js/signin/passkey.js", Payload::Empty)
            .await;

        for html in [&signin, &setup] {
            assert!(
                html.contains(r#"<script type="module" src="/ui/js/signin/passkey.js"></script>"#)
            );
        }
        assert!(signin.contains(r#"id="signin-passkey""#));
        assert!(setup.contains(r#"id="setup-passkey""#));
        assert_eq!(script.status, StatusCode::OK);
        assert!(script.text().contains("./webauthn.js"));
    }

    // Two replicas (M16 Task 10): two `App`s over one database and one
    // Redis, and no change-feed listener (a lib test), so what passes was
    // carried by Redis.

    /// Two replicas with `login_config` over `redis`, both knowing alice.
    async fn replicas(redis: crate::db::redis_support::TestRedis) -> Replicas {
        let (a, b, redis) = crate::mcp::testing::replica_pair_on(&[], login_config(), redis).await;
        Replicas { a, b, redis }
    }

    /// What [`replicas`] starts; the test holds it until the end.
    struct Replicas {
        /// The first replica (it owns the database).
        a: TestApp,
        /// The second replica.
        b: Arc<App>,
        /// The Redis both share.
        redis: crate::db::redis_support::TestRedis,
    }

    /// Alice with a password, known to both replicas' key caches.
    async fn alice_on_both(pair: &Replicas) -> UserAuth {
        let alice = user_with_password(&pair.a, "alice", "alice@example.com", Some(PASSWORD)).await;
        alice
    }

    /// Every key under the test's prefix with its value as text.
    async fn redis_contents(app: &App) -> Vec<(String, String)> {
        let redis = app.redis().expect("redis is on");
        let mut connection = redis.connection();
        // `{<prefix>}*`: every key of this test's prefix.
        let pattern = redis.channel("*");
        let names: Vec<String> = redis::cmd("KEYS")
            .arg(&pattern)
            .query_async(&mut connection)
            .await
            .expect("list the keys");
        let mut contents = Vec::with_capacity(names.len());
        for name in names {
            // A rate-limit bucket is a hash; the rest are strings.
            let value = crate::shared::testing::value_text(redis, &name).await;
            contents.push((name, value));
        }
        contents
    }

    /// The live keys of one space (`passkey:ceremony`, `passkey:session`).
    fn keys_of<'a>(contents: &'a [(String, String)], app: &App, space: &str) -> Vec<&'a str> {
        let prefix = app.redis().expect("redis is on").key(space, "");
        contents
            .iter()
            .map(|(name, _)| name.as_str())
            .filter(|name| name.starts_with(&prefix))
            .collect()
    }

    async fn a_registration_begun_on_one_replica_finishes_on_the_other(
        redis: crate::db::redis_support::TestRedis,
    ) {
        let pair = replicas(redis).await;
        let alice = alice_on_both(&pair).await;
        let token = setup_token(&pair.a, &alice).await;
        let mut soft = SoftPasskey::new(true);

        let registered = register_across(
            &pair.a.app,
            &pair.b,
            &mut Browser::default(),
            &mut soft,
            &token,
        )
        .await;

        assert_eq!(registered.status, StatusCode::OK, "{}", registered.text());
        let listed = passkey_rows::list(pair.b.accounts().executor(), &alice.id)
            .await
            .expect("list");
        assert_eq!(listed.len(), 1, "the passkey is stored");
        // And it signs in, here on the second replica alone.
        let mut browser = Browser::default();
        let signed =
            assertion_on(&pair.b, &mut browser, &mut soft, &alice, &alice.id, "/ui/").await;
        let reply = finish_on(&pair.b, &mut browser, signed).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
        assert!(browser.cookie(COOKIE).is_some());
    }

    async fn a_sign_in_begun_on_one_replica_finishes_on_the_other(
        redis: crate::db::redis_support::TestRedis,
    ) {
        let pair = replicas(redis).await;
        let (alice, mut soft) = alice_with_passkey(&pair.a, None).await;
        let mut browser = Browser::default();

        let signed = assertion_on(
            &pair.a.app,
            &mut browser,
            &mut soft,
            &alice,
            &alice.id,
            "/ui/#/keys",
        )
        .await;
        let reply = finish_on(&pair.b, &mut browser, signed).await;

        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
        assert_eq!(
            reply.json()["redirect"],
            "/ui/#/keys",
            "the start's return_to"
        );
        let session = browser
            .send_to(&pair.b, "GET", "/api/v1/session", Payload::Empty)
            .await;
        assert_eq!(session.json()["user"], "alice");
        assert_eq!(stored_counter(&pair.a, &alice).await, 1);
    }

    async fn a_ceremony_cannot_be_finished_twice_across_replicas(
        redis: crate::db::redis_support::TestRedis,
    ) {
        let pair = replicas(redis).await;
        let (alice, mut soft) = alice_with_passkey(&pair.a, None).await;
        let mut browser = Browser::default();
        let signed = assertion_on(
            &pair.a.app,
            &mut browser,
            &mut soft,
            &alice,
            &alice.id,
            "/ui/",
        )
        .await;
        let ceremony = browser
            .cookie(CEREMONY_COOKIE)
            .expect("a ceremony")
            .to_string();
        let mut on_a = Browser::with_cookie(CEREMONY_COOKIE, &ceremony);
        let mut on_b = Browser::with_cookie(CEREMONY_COOKIE, &ceremony);

        // Both replicas race the one ceremony: one takes it.
        let (first, second) = tokio::join!(
            finish_on(&pair.a.app, &mut on_a, signed.clone()),
            finish_on(&pair.b, &mut on_b, signed.clone()),
        );
        let mut statuses = [first.status, second.status];
        statuses.sort();
        // Then the same assertion and cookie, sent again to the first.
        let replayed = finish_on(
            &pair.a.app,
            &mut Browser::with_cookie(CEREMONY_COOKIE, &ceremony),
            signed,
        )
        .await;

        assert_eq!(statuses, [StatusCode::OK, StatusCode::GONE]);
        assert_eq!(replayed.status, StatusCode::GONE, "{}", replayed.text());
        assert_eq!(replayed.json()["message"], CEREMONY_EXPIRED);
        // The setup link's session, and one passkey sign-in.
        assert_eq!(sessions_of(&pair.a, &alice).await, 2, "one sign-in");
    }

    /// Registration from a console session, started on `start_on` and
    /// finished on `finish_on`: it finishes only in the session that started
    /// it, and then uses that request's own session.
    async fn a_session_registration_finishes_only_in_its_own_session(
        test: &TestApp,
        start_on: &Arc<App>,
        finish_on: &Arc<App>,
    ) {
        let alice = user_with_password(test, "alice", "alice@example.com", Some(PASSWORD)).await;
        let mut one = Browser::signed_in(test, "alice@example.com", PASSWORD).await;
        let mut two = Browser::signed_in(test, "alice@example.com", PASSWORD).await;
        let (mut soft_one, mut soft_two) = (SoftPasskey::new(true), SoftPasskey::new(true));
        let two_credential = registration_on(
            start_on,
            &mut two,
            &mut soft_two,
            json!({ "name": "Tablet", "keep_sessions": true }),
        )
        .await;
        let one_credential = registration_on(
            start_on,
            &mut one,
            &mut soft_one,
            json!({ "name": "Phone" }),
        )
        .await;

        // Session two presents session one's ceremony cookie.
        let mut crossed = Browser::with_cookie(COOKIE, two.cookie(COOKIE).expect("a session"));
        crossed.add_cookie(
            CEREMONY_COOKIE,
            one.cookie(CEREMONY_COOKIE).expect("a ceremony"),
        );
        crossed.csrf.clone_from(&two.csrf);
        let refused = crossed
            .send_to(
                finish_on,
                "POST",
                "/api/v1/me/passkeys/register/finish",
                Payload::Json(one_credential),
            )
            .await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.text());
        assert_eq!(
            refused.json()["message"],
            "forbidden: this passkey request belongs to another session"
        );
        let stored = passkey_rows::list(test.app.accounts().executor(), &alice.id)
            .await
            .expect("list");
        assert!(stored.is_empty(), "no passkey stored");
        // Session two's own ceremony was not consumed by the attempt.
        let own = two
            .send_to(
                finish_on,
                "POST",
                "/api/v1/me/passkeys/register/finish",
                Payload::Json(two_credential),
            )
            .await;
        assert_eq!(own.status, StatusCode::CREATED, "{}", own.text());

        // Session one starts again and finishes in its own session.
        let again = registration_on(
            start_on,
            &mut one,
            &mut soft_one,
            json!({ "name": "Phone" }),
        )
        .await;
        let done = one
            .send_to(
                finish_on,
                "POST",
                "/api/v1/me/passkeys/register/finish",
                Payload::Json(again),
            )
            .await;

        assert_eq!(done.status, StatusCode::CREATED, "{}", done.text());
        assert_eq!(done.json()["name"], "Phone");
        let kept = one
            .send_to(finish_on, "GET", "/api/v1/session", Payload::Empty)
            .await;
        let ended = two
            .send_to(finish_on, "GET", "/api/v1/session", Payload::Empty)
            .await;
        assert_eq!(kept.status, StatusCode::OK, "its own session is kept");
        assert_eq!(ended.status, StatusCode::UNAUTHORIZED, "the others end");
        assert_eq!(sessions_of(test, &alice).await, 1);
    }

    async fn a_session_registration_finishes_only_in_the_session_that_started_it(
        redis: crate::db::redis_support::TestRedis,
    ) {
        let pair = replicas(redis).await;
        a_session_registration_finishes_only_in_its_own_session(&pair.a, &pair.a.app, &pair.b)
            .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_session_registration_finishes_only_in_the_session_that_started_it_in_memory() {
        let test = login_app().await;
        let app = Arc::clone(&test.app);
        a_session_registration_finishes_only_in_its_own_session(&test, &app, &app).await;
    }

    async fn no_raw_session_id_reaches_redis(redis: crate::db::redis_support::TestRedis) {
        let pair = replicas(redis).await;
        alice_on_both(&pair).await;
        let mut browser = Browser::signed_in(&pair.a, "alice@example.com", PASSWORD).await;
        let session_id = browser.cookie(COOKIE).expect("a session").to_string();

        registration_on(
            &pair.a.app,
            &mut browser,
            &mut SoftPasskey::new(true),
            json!({ "name": "Phone" }),
        )
        .await;

        let ceremony = browser.cookie(CEREMONY_COOKIE).expect("a ceremony");
        let contents = redis_contents(&pair.a.app).await;
        let ceremonies = keys_of(&contents, &pair.a.app, "passkey:ceremony");
        assert_eq!(ceremonies.len(), 1, "{contents:?}");
        for (name, value) in &contents {
            assert!(!name.contains(&session_id), "a key holds the session id");
            assert!(!value.contains(&session_id), "a value holds the session id");
            assert!(!name.contains(ceremony), "a key holds the ceremony cookie");
        }
        // The ceremony keeps the session's hash, which keys the index too.
        let hash = session_hash(&session_id);
        assert!(contents
            .iter()
            .any(|(name, value)| name.starts_with(ceremonies[0]) && value.contains(&hash)));
        let index = keys_of(&contents, &pair.a.app, "passkey:session");
        assert_eq!(
            index,
            [pair
                .a
                .app
                .redis()
                .expect("redis")
                .key("passkey:session", &hash)]
        );
    }

    async fn no_raw_setup_token_reaches_redis(redis: crate::db::redis_support::TestRedis) {
        let pair = replicas(redis).await;
        let alice = alice_on_both(&pair).await;
        let token = setup_token(&pair.a, &alice).await;
        let mut browser = Browser::default();

        let credential = setup_registration(
            &pair.a.app,
            &mut browser,
            &mut SoftPasskey::new(true),
            &token,
        )
        .await;

        let contents = redis_contents(&pair.a.app).await;
        assert_eq!(
            keys_of(&contents, &pair.a.app, "passkey:ceremony").len(),
            1,
            "{contents:?}"
        );
        for (name, value) in &contents {
            assert!(!name.contains(&token), "a key holds the setup token");
            assert!(!value.contains(&token), "a value holds the setup token");
        }
        // The hash it keeps still finishes, on the other replica.
        let done = finish_setup(&pair.b, &mut browser, &token, credential).await;
        assert_eq!(done.status, StatusCode::OK, "{}", done.text());
    }

    /// A setup-link registration finishes only with the token it started
    /// with: a finish presenting the user's newer link, or another user's,
    /// is refused, stores nothing and leaves that link usable.
    async fn a_setup_finish_presenting_another_token_is_refused_on(
        test: &TestApp,
        start_on: &Arc<App>,
        finish_on: &Arc<App>,
    ) {
        let alice = user_with_password(test, "alice", "alice@example.com", None).await;
        let bob = user_with_password(test, "bob", "bob@example.com", None).await;
        let mut soft = SoftPasskey::new(true);
        let old_link = setup_token(test, &alice).await;
        let mut browser = Browser::default();
        let credential = setup_registration(start_on, &mut browser, &mut soft, &old_link).await;
        // The link is sent again meanwhile, which deletes the old one.
        let new_link = setup_token(test, &alice).await;
        let bobs_link = setup_token(test, &bob).await;
        let mut bobs_browser = Browser::default();
        let bobs_credential =
            setup_registration(start_on, &mut bobs_browser, &mut soft, &bobs_link).await;

        let newer = finish_setup(finish_on, &mut browser, &new_link, credential).await;
        let crossed = finish_setup(finish_on, &mut bobs_browser, &old_link, bobs_credential).await;

        for (refused, what) in [
            (&newer, "the newer link"),
            (&crossed, "another user's link"),
        ] {
            assert_eq!(
                refused.status,
                StatusCode::GONE,
                "{what}: {}",
                refused.text()
            );
            assert_eq!(refused.json()["message"], LINK_EXPIRED, "{what}");
        }
        for user in [&alice, &bob] {
            let stored = passkey_rows::list(test.app.accounts().executor(), &user.id)
                .await
                .expect("list");
            assert!(stored.is_empty(), "no passkey stored");
            assert_eq!(sessions_of(test, user).await, 0, "no session");
        }
        for link in [&new_link, &bobs_link] {
            let usable = email_tokens::peek(
                test.app.accounts().executor(),
                link,
                Purpose::Setup,
                Utc::now(),
            )
            .await;
            assert!(usable.is_ok(), "the presented link is untouched");
        }
    }

    async fn a_setup_finish_presenting_another_token_is_refused(
        redis: crate::db::redis_support::TestRedis,
    ) {
        let pair = replicas(redis).await;
        a_setup_finish_presenting_another_token_is_refused_on(&pair.a, &pair.a.app, &pair.b).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_setup_finish_presenting_another_token_is_refused_in_memory() {
        let test = login_app().await;
        let app = Arc::clone(&test.app);
        a_setup_finish_presenting_another_token_is_refused_on(&test, &app, &app).await;
    }

    async fn one_sessions_registrations_hold_one_ceremony_across_replicas(
        redis: crate::db::redis_support::TestRedis,
    ) {
        let pair = replicas(redis).await;
        alice_on_both(&pair).await;
        let mut first = Browser::signed_in(&pair.a, "alice@example.com", PASSWORD).await;
        let mut soft = SoftPasskey::new(true);
        let stale = registration_on(
            &pair.a.app,
            &mut first,
            &mut soft,
            json!({ "name": "Phone" }),
        )
        .await;
        let stale_cookie = first
            .cookie(CEREMONY_COOKIE)
            .expect("a ceremony")
            .to_string();
        // The same session, without its ceremony cookie, starts on the other.
        let mut second = Browser::with_cookie(COOKIE, first.cookie(COOKIE).expect("a session"));
        second.csrf.clone_from(&first.csrf);
        let fresh =
            registration_on(&pair.b, &mut second, &mut soft, json!({ "name": "Phone" })).await;

        let contents = redis_contents(&pair.a.app).await;
        let mut old = Browser::with_cookie(COOKIE, first.cookie(COOKIE).expect("a session"));
        old.add_cookie(CEREMONY_COOKIE, &stale_cookie);
        old.csrf.clone_from(&first.csrf);
        let replaced = old
            .send_to(
                &pair.a.app,
                "POST",
                "/api/v1/me/passkeys/register/finish",
                Payload::Json(stale),
            )
            .await;
        let done = second
            .send_to(
                &pair.a.app,
                "POST",
                "/api/v1/me/passkeys/register/finish",
                Payload::Json(fresh),
            )
            .await;

        assert_eq!(
            keys_of(&contents, &pair.a.app, "passkey:ceremony").len(),
            1,
            "{contents:?}"
        );
        assert_eq!(
            keys_of(&contents, &pair.a.app, "passkey:session").len(),
            1,
            "{contents:?}"
        );
        assert_eq!(replaced.status, StatusCode::GONE, "{}", replaced.text());
        assert_eq!(done.status, StatusCode::CREATED, "{}", done.text());
    }

    async fn a_redis_outage_fails_passkey_steps_closed(redis: crate::db::redis_support::TestRedis) {
        // A shared server cannot be stopped for everyone.
        let pair = replicas(redis.dedicated().await).await;
        let (alice, mut soft) = alice_with_passkey(&pair.a, None).await;
        let mut browser = Browser::default();
        let signed = assertion_on(
            &pair.a.app,
            &mut browser,
            &mut soft,
            &alice,
            &alice.id,
            "/ui/",
        )
        .await;
        pair.redis.stop().await;

        let finished = finish_on(&pair.b, &mut browser, signed).await;
        let started = Browser::default()
            .send_to(
                &pair.a.app,
                "POST",
                "/auth/passkey/signin/start",
                Payload::Json(json!({})),
            )
            .await;

        assert_eq!(
            finished.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{}",
            finished.text()
        );
        assert_eq!(
            started.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{}",
            started.text()
        );
        // Only the setup link's session.
        assert_eq!(sessions_of(&pair.a, &alice).await, 1, "no sign-in");
        // Nothing fell back to memory.
        let Some(passkeys) = pair.a.app.passkeys() else {
            panic!("passkeys are on");
        };
        assert!(matches!(passkeys.ceremonies, Ephemeral::Redis { .. }));
    }

    /// Starts a passkey confirmation from `browser`'s session and has `soft`
    /// answer it, as the Account page's "Confirm with a passkey" does.
    ///
    /// # Returns
    /// The assertion, unsent.
    async fn confirmation(
        test: &TestApp,
        browser: &mut Browser,
        soft: &mut SoftPasskey,
    ) -> serde_json::Value {
        let start = browser
            .send(
                test,
                "POST",
                "/api/v1/me/passkeys/confirm/start",
                Payload::Empty,
            )
            .await;
        assert_eq!(start.status, StatusCode::OK, "{}", start.text());
        let line = start
            .set_cookie(CEREMONY_COOKIE)
            .expect("a ceremony cookie");
        assert!(line.contains("SameSite=Strict"), "{line}");
        let challenge = start.json();
        assert_eq!(
            challenge["publicKey"]["allowCredentials"]
                .as_array()
                .map(Vec::len),
            Some(1),
            "only the user's own passkey may answer"
        );
        let challenge: RequestChallengeResponse =
            serde_json::from_value(challenge).expect("a challenge");
        let signed = soft
            .do_authentication(origin(), challenge)
            .expect("the authenticator signs");
        serde_json::to_value(&signed).expect("the assertion as JSON")
    }

    /// `DELETE /api/v1/me` with a passkey assertion.
    async fn delete_with(
        test: &TestApp,
        browser: &mut Browser,
        assertion: serde_json::Value,
    ) -> Reply {
        browser
            .send(
                test,
                "DELETE",
                "/api/v1/me",
                Payload::Json(json!({ "passkey": assertion })),
            )
            .await
    }

    async fn users_named(test: &TestApp, name: &str) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM users WHERE name = $1")
            .bind(name)
            .fetch_one(test.app.accounts().executor())
            .await
            .expect("count")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_fresh_passkey_assertion_deletes_the_account() {
        // Arrange: alice has no password, only a passkey.
        let test = login_app().await;
        let (alice, mut soft) = alice_with_passkey(&test, None).await;
        let mut browser = Browser::session_for(&test, "alice").await;

        // Act
        let assertion = confirmation(&test, &mut browser, &mut soft).await;
        let deleted = delete_with(&test, &mut browser, assertion).await;

        // Assert: deleted, both cookies cleared, every passkey gone.
        assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{}", deleted.text());
        assert!(deleted
            .set_cookie(COOKIE)
            .is_some_and(|line| line.contains("Max-Age=0")));
        assert!(deleted
            .set_cookie(CEREMONY_COOKIE)
            .is_some_and(|line| line.contains("Max-Age=0")));
        assert_eq!(users_named(&test, "alice").await, 0);
        let passkeys: i64 =
            sqlx::query_scalar("SELECT count(*) FROM user_passkeys WHERE user_id = $1")
                .bind(&alice.id)
                .fetch_one(test.app.accounts().executor())
                .await
                .expect("count");
        assert_eq!(passkeys, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_passkey_confirmation_is_single_use_and_bound_to_its_session() {
        let test = login_app().await;
        let (_, mut soft) = alice_with_passkey(&test, Some(PASSWORD)).await;
        let mut here = Browser::signed_in(&test, "alice@example.com", PASSWORD).await;
        let mut elsewhere = Browser::signed_in(&test, "alice@example.com", PASSWORD).await;

        // Another session of alice's presents this session's ceremony.
        let assertion = confirmation(&test, &mut here, &mut soft).await;
        let ceremony = here
            .cookie(CEREMONY_COOKIE)
            .expect("the cookie")
            .to_string();
        elsewhere.add_cookie(CEREMONY_COOKIE, &ceremony);
        let stolen = delete_with(&test, &mut elsewhere, assertion.clone()).await;
        // The same ceremony again, from its own session: it was used.
        here.add_cookie(CEREMONY_COOKIE, &ceremony);
        let replayed = delete_with(&test, &mut here, assertion).await;
        // A ceremony that was never started.
        let mut fresh = Browser::signed_in(&test, "alice@example.com", PASSWORD).await;
        let unstarted = delete_with(
            &test,
            &mut fresh,
            json!({
                "id": "AAAA", "rawId": "AAAA", "type": "public-key", "extensions": {},
                "response": {"authenticatorData": "AAAA", "clientDataJSON": "AAAA",
                             "signature": "AAAA", "userHandle": null},
            }),
        )
        .await;

        assert_eq!(stolen.status, StatusCode::FORBIDDEN, "{}", stolen.text());
        assert_eq!(replayed.status, StatusCode::GONE, "{}", replayed.text());
        assert_eq!(unstarted.status, StatusCode::GONE, "{}", unstarted.text());
        assert_eq!(users_named(&test, "alice").await, 1, "nothing was deleted");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_assertion_that_does_not_verify_is_a_403_and_keeps_the_account() {
        let test = login_app().await;
        let (_, mut soft) = alice_with_passkey(&test, Some(PASSWORD)).await;
        let mut browser = Browser::signed_in(&test, "alice@example.com", PASSWORD).await;
        let mut assertion = confirmation(&test, &mut browser, &mut soft).await;
        // A signature over something else.
        assertion["response"]["signature"] = json!("AAAA");

        let refused = delete_with(&test, &mut browser, assertion).await;
        let still = browser
            .send(&test, "GET", "/api/v1/session", Payload::Empty)
            .await;

        assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.text());
        assert!(
            refused.text().contains("could not confirm"),
            "{}",
            refused.text()
        );
        assert_eq!(still.status, StatusCode::OK, "still signed in");
        assert_eq!(users_named(&test, "alice").await, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_user_without_a_passkey_cannot_start_a_confirmation_and_a_key_never_can() {
        let test = login_app().await;
        user_with_password(&test, "alice", "alice@example.com", Some(PASSWORD)).await;
        let mut browser = Browser::signed_in(&test, "alice@example.com", PASSWORD).await;
        let keys = test_keys(&test).await;
        let mut bearer = Browser::default();
        bearer.bearer = Some(keys.admin.clone());

        let none = browser
            .send(
                &test,
                "POST",
                "/api/v1/me/passkeys/confirm/start",
                Payload::Empty,
            )
            .await;
        let by_key = bearer
            .send(
                &test,
                "POST",
                "/api/v1/me/passkeys/confirm/start",
                Payload::Empty,
            )
            .await;

        assert_eq!(none.status, StatusCode::CONFLICT, "{}", none.text());
        assert!(none.text().contains("no passkey"), "{}", none.text());
        assert_eq!(by_key.status, StatusCode::FORBIDDEN, "{}", by_key.text());
    }

    /// The bound on every two-replica test (containers start inside it).
    const REPLICA_BOUND: Duration = Duration::from_secs(180);

    // Each two-replica body above runs twice: on a standalone Redis and on a
    // one-node cluster. `macro_rules!` writes the two `#[tokio::test]`
    // wrappers for every name (Python: a parametrized test).
    macro_rules! in_both_modes {
        ($($name:ident),* $(,)?) => {
            mod standalone {
                $(
                    #[tokio::test(flavor = "multi_thread")]
                    async fn $name() {
                        let server = crate::db::redis_support::TestRedis::start().await;
                        tokio::time::timeout(super::REPLICA_BOUND, super::$name(server))
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
                        tokio::time::timeout(super::REPLICA_BOUND, super::$name(server))
                            .await
                            .expect(concat!(stringify!($name), " finished within the bound"));
                    }
                )*
            }
        };
    }

    in_both_modes!(
        a_registration_begun_on_one_replica_finishes_on_the_other,
        a_sign_in_begun_on_one_replica_finishes_on_the_other,
        a_ceremony_cannot_be_finished_twice_across_replicas,
        a_session_registration_finishes_only_in_the_session_that_started_it,
        no_raw_session_id_reaches_redis,
        no_raw_setup_token_reaches_redis,
        a_setup_finish_presenting_another_token_is_refused,
        one_sessions_registrations_hold_one_ceremony_across_replicas,
        a_redis_outage_fails_passkey_steps_closed,
    );
}
