//! OIDC providers from `[[auth.oidc]]` (spec "OIDC", plan Decision 12).
//! Each provider's discovery document is cached for an hour, and a failed
//! refresh keeps the cached one. The registry builds the authorize URL
//! (PKCE S256, `state`, `nonce`) and exchanges the code, verifying the ID
//! token. HTTP goes through the server's own reqwest 0.13 by a small
//! adapter, so no second reqwest is linked.
//!
//! Secrets stay out of logs: the client secret is an `openidconnect`
//! `ClientSecret` (its `Debug` is redacted), and the flow's `state`, `nonce`
//! and PKCE verifier live in [`FlowSecrets`], whose `Debug` is redacted too.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use openidconnect::core::{
    CoreAuthPrompt, CoreAuthenticationFlow, CoreClient, CoreProviderMetadata,
};
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointMaybeSet, EndpointNotSet,
    EndpointSet, HttpRequest, HttpResponse, IssuerUrl, Nonce, PkceCodeChallenge, PkceCodeVerifier,
    RedirectUrl, Scope, TokenResponse,
};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::config::{Config, OidcProviderConfig};
use crate::error::OkfError;
use crate::keys;
use crate::shared::ephemeral::{Ephemeral, Index, IndexSpace, Keyspace, Swap};
use crate::shared::limits::KeyedLimit;
use crate::shared::Redis;
use crate::telemetry::metrics::Limiter;
use crate::telemetry::subscriber::split_event;

/// A client configured from a discovery document: an authorization
/// endpoint, and maybe a token and a userinfo endpoint (the typestate the
/// crate gives `from_provider_metadata`).
pub type ConfiguredClient = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

/// How long a discovery document is used before it is fetched again.
pub const DISCOVERY_TTL: Duration = Duration::from_secs(60 * 60);
/// After a failed refresh, how long the cached document is used before
/// the next try.
pub const FAILED_REFRESH_RETRY: Duration = Duration::from_secs(60);
/// How long a flow may take (spec: a ten-minute cookie).
pub const FLOW_TTL: Duration = Duration::from_secs(10 * 60);
/// The most flows in progress at once (they start unauthenticated, so the
/// start route also takes from the per-address sign-in limit).
pub const MAX_FLOWS: usize = 10_000;
/// The per-user link allowance: a token bucket with a burst of this many
/// starts, refilled one per minute (this many per [`FLOW_TTL`]). Each start
/// holds a flow-map entry for ten minutes, so one signed-in user holds at
/// most about twice this many (the burst plus ten minutes of refill) of
/// the map every sign-in shares.
pub const LINK_STARTS_PER_USER: u32 = 10;
/// The most users the per-user link limiter tracks at once.
const MAX_LINKING_USERS: usize = 10_000;
/// How long before the callback the person may last have authenticated at
/// the provider, for an ID token to count as the fresh proof that deletes an
/// account (issue #775). The `auth_time` claim must be present and at most
/// this old: a provider that ignores `max_age=0` is refused unless the
/// person really did authenticate recently. Documented in
/// `docs/operations.md`.
pub const REAUTH_AUTH_TIME_WINDOW: Duration = Duration::from_secs(5 * 60);
/// How far ahead of the server's clock an `auth_time` may be (clock skew
/// between the provider and this server); anything later is refused.
const REAUTH_CLOCK_SKEW: Duration = Duration::from_secs(60);
/// How long a finished re-authentication waits to be used by
/// `DELETE /api/v1/me`: short, and single use.
pub const REAUTH_PROOF_TTL: Duration = Duration::from_secs(5 * 60);
/// The longest any provider request may take.
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest `sub` OIDC Core allows (section 2: "MUST NOT exceed 255
/// ASCII characters").
const MAX_SUBJECT_LENGTH: usize = 255;

/// Why an OIDC step failed.
#[derive(Debug, thiserror::Error)]
pub enum OidcError {
    /// No provider has this id.
    #[error("unknown OIDC provider {0:?}")]
    UnknownProvider(String),
    /// Discovery failed and nothing is cached.
    #[error("OIDC discovery for {provider} failed: {message}")]
    Discovery {
        /// The provider's id.
        provider: String,
        /// What went wrong.
        message: String,
    },
    /// The code exchange failed.
    #[error("the OIDC code exchange failed: {0}")]
    Exchange(String),
    /// The ID token is missing or did not verify.
    #[error("the ID token was refused: {0}")]
    IdToken(String),
}

impl From<OidcError> for OkfError {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    fn from(error: OidcError) -> Self {
        let message = error.to_string();
        match error {
            OidcError::UnknownProvider(id) => OkfError::NotFound {
                path: format!("auth/oidc/{id}"),
            },
            OidcError::Discovery { .. } => OkfError::Busy { reason: message },
            OidcError::Exchange(_) | OidcError::IdToken(_) => {
                OkfError::Unauthenticated { reason: message }
            }
        }
    }
}

/// A provider request that failed: the adapter's error type.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct HttpError(String);

/// The adapter: sends one openidconnect request with reqwest 0.13. A
/// closure calling it is an `AsyncHttpClient` (oauth2's blanket impl for
/// any `Fn(HttpRequest) -> impl Future<Output = Result<HttpResponse, E>>`).
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "it injects the caller's trace context: its own span would replace the span the request is made under"
    )
)]
async fn send(client: reqwest::Client, request: HttpRequest) -> Result<HttpResponse, HttpError> {
    // The annotation names the conversion's error type for `map_err`.
    let mut request: reqwest::Request = request
        .try_into()
        .map_err(|error: reqwest::Error| HttpError(error.to_string()))?;
    // The provider's logs can join the trace of the sign-in that asked.
    crate::telemetry::propagation::inject(request.headers_mut());
    let response = client
        .execute(request)
        .await
        .map_err(|error| HttpError(error.to_string()))?;
    let mut builder = openidconnect::http::Response::builder().status(response.status());
    for (name, value) in response.headers() {
        builder = builder.header(name, value);
    }
    let body = response
        .bytes()
        .await
        .map_err(|error| HttpError(error.to_string()))?
        .to_vec();
    builder
        .body(body)
        .map_err(|error| HttpError(error.to_string()))
}

/// A flow's secrets: compared or sent back once, never logged. With
/// `[redis]` they are stored there as JSON (server-side, like Postgres).
#[derive(Serialize, Deserialize)]
pub struct FlowSecrets {
    /// The `state` sent to the provider.
    pub state: String,
    /// The `nonce` the ID token must carry.
    pub nonce: String,
    /// The PKCE verifier.
    pub verifier: String,
}

// Written by hand, not derived (Python's `__repr__`), so a stray `{:?}`
// prints no secret.
impl std::fmt::Debug for FlowSecrets {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "Display/Debug impl: runs while the subscriber formats an event"
        )
    )]
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("FlowSecrets(<redacted>)")
    }
}

/// A signed-in user linking a provider from the account page.
#[derive(Serialize, Deserialize)]
pub struct LinkRequest {
    /// The user's id.
    pub user_id: String,
    /// [`session_hash`] of the session that asked, never its raw id (spec
    /// Decision 7): the callback finishes only when its own session cookie
    /// hashes to it, and then keeps that request's session when the others
    /// are ended.
    pub session_hash: String,
    /// Keep the user's other sessions.
    pub keep_sessions: bool,
}

/// The form a console session id is stored in by a link flow or a passkey
/// registration (`http::passkeys`), and the key of its
/// [`IndexSpace::OidcLink`] or `IndexSpace::PasskeySession` entry: the
/// unpadded URL-safe base64 of its SHA-256.
///
/// # Arguments
/// * `session_id` - the raw console session id (the cookie's value).
///
/// # Returns
/// 43 base64 characters.
#[tracing::instrument(level = "trace", skip_all)]
pub fn session_hash(session_id: &str) -> String {
    URL_SAFE_NO_PAD.encode(keys::hash(session_id))
}

// Hand-written: even the session's hash stays out of logs.
impl std::fmt::Debug for LinkRequest {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "Display/Debug impl: runs while the subscriber formats an event"
        )
    )]
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LinkRequest")
            .field("user_id", &self.user_id)
            .field("session_hash", &"<redacted>")
            .field("keep_sessions", &self.keep_sessions)
            .finish()
    }
}

/// A signed-in user re-authenticating at the provider, to delete their own
/// account (issue #775). It never signs in or links anything.
#[derive(Serialize, Deserialize)]
pub struct ReauthRequest {
    /// The user's id.
    pub user_id: String,
    /// [`session_hash`] of the session that asked: the callback finishes
    /// only in the browser whose live session hashes to it.
    pub session_hash: String,
    /// The `sub` of the user's identity at this provider when the flow
    /// started: the returned ID token must carry exactly this subject.
    pub subject: String,
}

// Hand-written: neither the session's hash nor the subject reaches a log.
impl std::fmt::Debug for ReauthRequest {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "Display/Debug impl: runs while the subscriber formats an event"
        )
    )]
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReauthRequest")
            .field("user_id", &self.user_id)
            .field("session_hash", &"<redacted>")
            .field("subject", &"<redacted>")
            .finish()
    }
}

/// A finished re-authentication, waiting for `DELETE /api/v1/me`: the
/// provider proved, moments ago, that the person behind this session's
/// linked identity is here. Stored under the session's [`session_hash`],
/// taken (single use) by the deletion, which then rechecks under the user's
/// row lock that the identity is still linked to the user.
#[derive(Serialize, Deserialize)]
pub struct ReauthProof {
    /// The user's id.
    pub user_id: String,
    /// The provider's id.
    pub provider: String,
    /// The verified ID token's `sub`; empty for a stale outcome.
    pub subject: String,
    /// [`session_hash`] of the session it was made for (also the key).
    pub session_hash: String,
    /// The provider answered, but its `auth_time` was not fresh: the
    /// deletion is refused with `ReauthStale`. Holds no identity.
    #[serde(default)]
    pub stale: bool,
}

// Hand-written: it names a session and an identity.
impl std::fmt::Debug for ReauthProof {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "Display/Debug impl: runs while the subscriber formats an event"
        )
    )]
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReauthProof")
            .field("user_id", &self.user_id)
            .field("provider", &self.provider)
            .finish_non_exhaustive()
    }
}

/// A flow in progress, keyed by the `okf_oidc` cookie's random id. Its
/// `Debug` shows no secret ([`FlowSecrets`], [`LinkRequest`] and
/// [`ReauthRequest`] redact). At most one of `link` and `reauth` is set; a
/// flow with neither is a sign-in.
#[derive(Debug, Serialize, Deserialize)]
pub struct Flow {
    /// The provider it was started with.
    pub provider: String,
    /// Its secrets.
    pub secrets: FlowSecrets,
    /// Where to go after signing in.
    pub return_to: String,
    /// Set when a signed-in user is linking, not signing in.
    pub link: Option<LinkRequest>,
    /// Set when a signed-in user is re-authenticating to delete their
    /// account (issue #775). `default`: a flow stored before this field
    /// existed reads as a sign-in or link, never a re-authentication.
    #[serde(default)]
    pub reauth: Option<ReauthRequest>,
}

/// What a verified ID token says about the person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// The provider's `sub` (1 to 255 characters, no control character).
    pub subject: String,
    /// Its `email`, if sent.
    pub email: Option<String>,
    /// Whether `email_verified` was `true` (or the string `"true"`, which
    /// some providers send: the crate's `accept-string-booleans`).
    pub email_verified: bool,
}

/// Whether a `sub` may be stored and looked up: non-empty, at most 255
/// bytes, and free of control characters (Postgres refuses a NUL in text,
/// which would turn a hostile token into a database error).
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
    )
)]
fn valid_subject(subject: &str) -> bool {
    (1..=MAX_SUBJECT_LENGTH).contains(&subject.len()) && !subject.chars().any(char::is_control)
}

/// One configured provider.
struct Provider {
    config: OidcProviderConfig,
    secret: ClientSecret,
    redirect: RedirectUrl,
    // tokio's mutex, held across a discovery fetch: concurrent sign-ins
    // wait for one fetch instead of each making their own.
    cached: tokio::sync::Mutex<Option<Cached>>,
}

/// A configured client and when to refresh it.
struct Cached {
    client: ConfiguredClient,
    refresh_at: Instant,
}

/// The configured providers and the flows in progress, held by `App`.
pub struct OidcProviders {
    http: reqwest::Client,
    providers: BTreeMap<String, Provider>,
    /// Flows in progress (ten minutes, single use), keyed by the flow
    /// cookie's random id (chosen by the server, never by the client), in
    /// memory or, with `[redis]`, shared by every instance. Add to it with
    /// [`Self::store_flow`], which bounds each actor's share.
    pub flows: Ephemeral<Flow>,
    /// Each console session's live link flow, by [`session_hash`]: a new
    /// link flow replaces (and removes) the session's previous one.
    link_flows: Index,
    /// Finished re-authentications by [`session_hash`] (one per session, a
    /// new one replacing the old), each valid for [`REAUTH_PROOF_TTL`] and
    /// taken once.
    reauth_proofs: Ephemeral<ReauthProof>,
    /// Link starts by user id: a burst of [`LINK_STARTS_PER_USER`], refilled
    /// one per minute; in Redis with `[redis]`, shared by every instance.
    link_starts: KeyedLimit,
}

// Hand-written: the providers hold client secrets, and the flows theirs.
impl std::fmt::Debug for OidcProviders {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "Display/Debug impl: runs while the subscriber formats an event"
        )
    )]
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OidcProviders")
            .field("providers", &self.providers.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl OidcProviders {
    /// The registry for `[[auth.oidc]]`.
    ///
    /// # Arguments
    /// * `config` - the server's settings (validated).
    /// * `secret_of` - reads an environment variable (`std::env::var` in
    ///   production; a closure in tests).
    /// * `redis` - the shared Redis (`[redis]`): flows and link flows live
    ///   there, so a callback may land on any instance; `None` keeps them in
    ///   this process's memory.
    ///
    /// # Returns
    /// `None` without `public_url` or providers.
    ///
    /// # Errors
    /// [`OkfError::Config`] when a provider's secret variable is unset or
    /// empty, or the HTTP client cannot be built.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn new(
        config: &Config,
        secret_of: impl Fn(&str) -> Option<String>,
        redis: Option<&Redis>,
    ) -> Result<Option<Self>, OkfError> {
        let Some(public_url) = config.public_url.as_deref() else {
            return Ok(None);
        };
        if config.auth.oidc.is_empty() {
            return Ok(None);
        }
        let config_error = |path: String, message: String| OkfError::Config { path, message };
        let http = reqwest::Client::builder()
            // Discovery and the token endpoint answer directly; following a
            // redirect could reach an internal address.
            .redirect(reqwest::redirect::Policy::none())
            .timeout(HTTP_TIMEOUT)
            .user_agent(concat!("okf-mcp/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| {
                config_error(
                    "auth.oidc".to_string(),
                    format!("building its HTTP client: {error}"),
                )
            })?;
        let mut providers = BTreeMap::new();
        for provider in &config.auth.oidc {
            let path = format!("auth.oidc.{}", provider.id);
            // The message names the variable, never its value.
            let secret = secret_of(&provider.client_secret_env)
                .filter(|secret| !secret.is_empty())
                .ok_or_else(|| {
                    config_error(
                        path.clone(),
                        format!(
                            "the environment variable {} is not set",
                            provider.client_secret_env
                        ),
                    )
                })?;
            let redirect = RedirectUrl::new(format!(
                "{}/auth/oidc/{}/callback",
                public_url.trim_end_matches('/'),
                provider.id
            ))
            .map_err(|error| config_error(path.clone(), error.to_string()))?;
            providers.insert(
                provider.id.clone(),
                Provider {
                    config: provider.clone(),
                    secret: ClientSecret::new(secret),
                    redirect,
                    cached: tokio::sync::Mutex::new(None),
                },
            );
        }
        Ok(Some(Self {
            http,
            providers,
            flows: Ephemeral::new(redis, Keyspace::OidcFlow, FLOW_TTL, MAX_FLOWS),
            link_flows: Index::new(redis, IndexSpace::OidcLink, FLOW_TTL, MAX_FLOWS),
            reauth_proofs: Ephemeral::new(redis, Keyspace::OidcReauth, REAUTH_PROOF_TTL, MAX_FLOWS),
            link_starts: KeyedLimit::new(
                Limiter::LinkStarts,
                LINK_STARTS_PER_USER,
                FLOW_TTL,
                MAX_LINKING_USERS,
                redis,
            ),
        }))
    }

    /// Stores a new flow so that no actor holds more than its share of
    /// [`Self::flows`]: the flow this browser held before (its old cookie)
    /// is removed, and a link flow replaces the one its session started
    /// before. The new flow is stored before the old one is removed, so
    /// two concurrent starts of one session leave one flow, never a
    /// stranded one.
    ///
    /// # Arguments
    /// * `id` - the new flow cookie's random id.
    /// * `flow` - the flow.
    /// * `previous` - the id in the browser's current flow cookie, if any.
    /// * `now` - the current instant.
    ///
    /// # Returns
    /// `false`, storing nothing, when the memory map or index is full.
    ///
    /// # Errors
    /// [`OkfError::Busy`] when Redis fails: the start fails closed.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn store_flow(
        &self,
        id: &str,
        flow: Flow,
        previous: Option<&str>,
        now: Instant,
    ) -> Result<bool, OkfError> {
        // The session the link belongs to, as the key of its index entry:
        // the flow already holds only the session's hash.
        // A re-authentication shares the link's slot: a session holds one
        // of the two at a time.
        let session_key = flow
            .link
            .as_ref()
            .map(|link| link.session_hash.clone())
            .or_else(|| {
                flow.reauth
                    .as_ref()
                    .map(|reauth| reauth.session_hash.clone())
            });
        if let Some(previous) = previous.filter(|previous| *previous != id) {
            self.flows.remove(previous).await?;
        }
        if !self.flows.insert(id, flow, now).await? {
            return Ok(false);
        }
        if let Some(key) = session_key {
            // One atomic swap of the session's pointer, in memory (under the
            // map's lock) or in Redis (`SET ... GET`).
            let swap = match self.link_flows.replace(&key, id, now).await {
                Ok(swap) => swap,
                Err(error) => {
                    // The flow just stored has no index entry and its cookie
                    // is never sent (the start fails closed): remove it now
                    // rather than leave it until its TTL. Best effort: a
                    // failed removal is already counted as a Redis `remove`
                    // error, and the swap's error is the one returned.
                    let _ = self.flows.remove(id).await;
                    return Err(error);
                }
            };
            match swap {
                Swap::Swapped(Some(stale)) => self.flows.remove_stale(&stale).await?,
                Swap::Swapped(None) => {}
                // The index is full: keep the map bounded by refusing.
                Swap::Full => {
                    self.flows.remove(id).await?;
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    /// Keeps a finished re-authentication for the session it was made for,
    /// replacing that session's previous one.
    ///
    /// # Arguments
    /// * `proof` - the proof, keyed by its `session_hash`.
    /// * `now` - the current instant.
    ///
    /// # Returns
    /// `false`, storing nothing, when the memory store is full.
    ///
    /// # Errors
    /// [`OkfError::Busy`] when Redis fails.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn store_reauth_proof(
        &self,
        proof: ReauthProof,
        now: Instant,
    ) -> Result<bool, OkfError> {
        let key = proof.session_hash.clone();
        self.reauth_proofs.insert(&key, proof, now).await
    }

    /// Takes (single use) the session's finished re-authentication.
    ///
    /// # Arguments
    /// * `session_hash` - [`session_hash`] of the asking session.
    /// * `now` - the current instant.
    ///
    /// # Returns
    /// The proof, or `None` when there is none, it expired or it was used.
    ///
    /// # Errors
    /// [`OkfError::Busy`] when Redis fails.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn take_reauth_proof(
        &self,
        session_hash: &str,
        now: Instant,
    ) -> Result<Option<ReauthProof>, OkfError> {
        self.reauth_proofs.take(session_hash, now).await
    }

    /// Takes one link start from the user's allowance.
    ///
    /// # Arguments
    /// * `user_id` - the signed-in user.
    /// * `now` - the current instant.
    ///
    /// # Returns
    /// `false` once the user's bucket is empty: a burst of
    /// [`LINK_STARTS_PER_USER`] starts, then one more per minute.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub async fn may_start_link(&self, user_id: &str, now: Instant) -> bool {
        self.link_starts.try_take(user_id, now).await
    }

    /// Every provider's id and label, by id.
    ///
    /// # Returns
    /// `(id, label)` pairs.
    // `+ '_`: the iterator borrows `self`.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn labels(&self) -> impl Iterator<Item = (&str, &str)> + '_ {
        self.providers
            .values()
            .map(|provider| (provider.config.id.as_str(), provider.config.label.as_str()))
    }

    /// A provider's label.
    ///
    /// # Arguments
    /// * `id` - the provider's id.
    ///
    /// # Returns
    /// Its label, or `None` for an unknown id.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn label(&self, id: &str) -> Option<&str> {
        self.providers
            .get(id)
            .map(|provider| provider.config.label.as_str())
    }

    /// The provider called `id`.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    fn provider(&self, id: &str) -> Result<&Provider, OidcError> {
        self.providers
            .get(id)
            .ok_or_else(|| OidcError::UnknownProvider(id.to_string()))
    }

    /// Fetches the discovery document and configures a client from it.
    #[tracing::instrument(level = "debug", skip_all)]
    async fn discover(&self, provider: &Provider) -> Result<ConfiguredClient, OidcError> {
        let failed = |message: String| OidcError::Discovery {
            provider: provider.config.id.clone(),
            message,
        };
        let issuer = IssuerUrl::new(provider.config.issuer.clone())
            .map_err(|error| failed(error.to_string()))?;
        // The closure is the `AsyncHttpClient`; each call gets its own
        // handle to the pooled client (a cheap `Arc` clone).
        let http = |request: HttpRequest| send(self.http.clone(), request);
        let metadata = CoreProviderMetadata::discover_async(issuer, &http)
            .await
            .map_err(|error| failed(error.to_string()))?;
        Ok(CoreClient::from_provider_metadata(
            metadata,
            ClientId::new(provider.config.client_id.clone()),
            Some(provider.secret.clone()),
        )
        .set_redirect_uri(provider.redirect.clone()))
    }

    /// A provider's client, from the cache while it is fresh.
    ///
    /// # Arguments
    /// * `id` - the provider's id.
    /// * `now` - the current instant (passed in, so tests control time).
    ///
    /// # Returns
    /// The client: freshly discovered, cached, or (after a failed refresh)
    /// the stale cached one, retried after [`FAILED_REFRESH_RETRY`].
    ///
    /// # Errors
    /// [`OidcError::UnknownProvider`]; [`OidcError::Discovery`] when
    /// nothing is cached and discovery fails.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn client(&self, id: &str, now: Instant) -> Result<ConfiguredClient, OidcError> {
        let provider = self.provider(id)?;
        let mut cached = provider.cached.lock().await;
        if let Some(entry) = cached.as_ref().filter(|entry| now < entry.refresh_at) {
            return Ok(entry.client.clone());
        }
        match self.discover(provider).await {
            Ok(client) => {
                *cached = Some(Cached {
                    client: client.clone(),
                    refresh_at: now + DISCOVERY_TTL,
                });
                Ok(client)
            }
            Err(error) => match cached.as_mut() {
                Some(entry) => {
                    split_event!(
                        warn,
                        { provider = id, },
                        { %error, },
                        "OIDC discovery failed; keeping the cached document"
                    );
                    entry.refresh_at = now + FAILED_REFRESH_RETRY;
                    Ok(entry.client.clone())
                }
                None => Err(error),
            },
        }
    }

    /// The provider's authorize URL for a new flow.
    ///
    /// # Arguments
    /// * `id` - the provider's id.
    /// * `now` - the current instant.
    /// * `fresh` - ask the provider to authenticate the person again
    ///   (`prompt=login` and `max_age=0`), for a re-authentication.
    ///
    /// # Returns
    /// The URL and the flow's secrets (for the caller to keep).
    ///
    /// # Errors
    /// As [`Self::client`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn authorize_url(
        &self,
        id: &str,
        now: Instant,
        fresh: bool,
    ) -> Result<(String, FlowSecrets), OidcError> {
        let client = self.client(id, now).await?;
        let provider = self.provider(id)?;
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let mut request = client
            .authorize_url(
                CoreAuthenticationFlow::AuthorizationCode,
                CsrfToken::new_random,
                Nonce::new_random,
            )
            .set_pkce_challenge(challenge);
        if fresh {
            // The provider is asked to show its login again; whether it does
            // is checked from `auth_time` ([`Self::identify_fresh`]).
            request = request
                .add_prompt(CoreAuthPrompt::Login)
                .set_max_age(Duration::ZERO);
        }
        // The crate always sends `openid`; adding it again would repeat it.
        for scope in provider
            .config
            .scopes
            .iter()
            .filter(|scope| scope.as_str() != "openid")
        {
            request = request.add_scope(Scope::new(scope.clone()));
        }
        let (url, state, nonce) = request.url();
        let secrets = FlowSecrets {
            state: state.into_secret(),
            nonce: nonce.secret().clone(),
            verifier: verifier.into_secret(),
        };
        Ok((url.to_string(), secrets))
    }

    /// Exchanges the code and verifies the ID token (issuer, audience,
    /// expiry, signature and nonce). The caller has already checked
    /// `state`: the crate does not.
    ///
    /// # Arguments
    /// * `id` - the provider's id.
    /// * `code` - the callback's `code`.
    /// * `secrets` - the flow's secrets (consumed: they are single use).
    /// * `now` - the current instant.
    ///
    /// # Returns
    /// What the token says about the person.
    ///
    /// # Errors
    /// [`OidcError::Exchange`] or [`OidcError::IdToken`] (also for a `sub`
    /// that is empty, too long or holds a control character); as
    /// [`Self::client`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn identify(
        &self,
        id: &str,
        code: String,
        secrets: FlowSecrets,
        now: Instant,
    ) -> Result<Identity, OidcError> {
        self.identify_checked(id, code, secrets, now)
            .await
            .map(|(identity, _)| identity)
    }

    /// As [`Self::identify`], for a re-authentication: the same verification
    /// (signature, issuer, audience, nonce, expiry), then the verified
    /// token's `auth_time` is judged: the answer is `true` only when it is
    /// at most [`REAUTH_AUTH_TIME_WINDOW`] before `clock_now` (and not in the
    /// future beyond a minute of skew). A token without `auth_time` is not
    /// fresh: a provider that ignored `max_age=0` proves nothing unless it
    /// says when the person last authenticated.
    ///
    /// The identity comes back whether or not the token is fresh, so the
    /// caller can check WHO answered before it says anything about
    /// freshness.
    ///
    /// # Arguments
    /// As [`Self::identify`]; `clock_now` - the server clock's present.
    ///
    /// # Returns
    /// The verified identity, and whether its `auth_time` is fresh.
    ///
    /// # Errors
    /// As [`Self::identify`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn identify_fresh(
        &self,
        id: &str,
        code: String,
        secrets: FlowSecrets,
        now: Instant,
        clock_now: DateTime<Utc>,
    ) -> Result<(Identity, bool), OidcError> {
        let (identity, auth_time) = self.identify_checked(id, code, secrets, now).await?;
        Ok((identity, check_auth_time(auth_time, clock_now).is_ok()))
    }

    /// The code exchange and ID-token verification both public methods
    /// share. Returns the verified token's `auth_time` beside the identity;
    /// nothing is read from the token before its signature has passed.
    #[tracing::instrument(level = "debug", skip_all)]
    async fn identify_checked(
        &self,
        id: &str,
        code: String,
        secrets: FlowSecrets,
        now: Instant,
    ) -> Result<(Identity, Option<DateTime<Utc>>), OidcError> {
        let client = self.client(id, now).await?;
        let http = |request: HttpRequest| send(self.http.clone(), request);
        let tokens = client
            .exchange_code(AuthorizationCode::new(code))
            .map_err(|error| OidcError::Exchange(error.to_string()))?
            .set_pkce_verifier(PkceCodeVerifier::new(secrets.verifier))
            .request_async(&http)
            .await
            .map_err(|error| OidcError::Exchange(error.to_string()))?;
        // `id_token` comes from the `TokenResponse` trait imported above.
        let id_token = tokens
            .id_token()
            .ok_or_else(|| OidcError::IdToken("the provider sent no ID token".to_string()))?;
        let nonce = Nonce::new(secrets.nonce);
        let claims = id_token
            .claims(&client.id_token_verifier(), &nonce)
            .map_err(|error| OidcError::IdToken(error.to_string()))?;
        let subject = claims.subject().as_str();
        if !valid_subject(subject) {
            return Err(OidcError::IdToken(
                "its subject is empty, too long or holds a control character".to_string(),
            ));
        }
        let identity = Identity {
            subject: subject.to_string(),
            email: claims.email().map(|email| email.as_str().to_string()),
            email_verified: claims.email_verified() == Some(true),
        };
        Ok((identity, claims.auth_time()))
    }
}

/// Whether an ID token's `auth_time` shows a fresh authentication: present,
/// at most [`REAUTH_AUTH_TIME_WINDOW`] before `now` and no later than
/// [`REAUTH_CLOCK_SKEW`] after it.
///
/// # Errors
/// The reason it is not fresh.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
    )
)]
fn check_auth_time(auth_time: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Result<(), String> {
    let Some(auth_time) = auth_time else {
        return Err("the ID token has no auth_time claim".to_string());
    };
    // The constants are small, so the conversions cannot fail; `unwrap_or`
    // keeps the check closed (a zero window refuses everything) if one did.
    let window = chrono::Duration::from_std(REAUTH_AUTH_TIME_WINDOW).unwrap_or_default();
    let skew = chrono::Duration::from_std(REAUTH_CLOCK_SKEW).unwrap_or_default();
    if auth_time < now - window {
        return Err("the person authenticated too long ago".to_string());
    }
    if auth_time > now + skew {
        return Err("the authentication time is in the future".to_string());
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod testing;

#[cfg(test)]
mod tests {

    #[test]
    fn auth_time_must_be_present_recent_and_not_from_the_future() {
        use super::{check_auth_time, REAUTH_AUTH_TIME_WINDOW, REAUTH_CLOCK_SKEW};
        let now = Utc::now();
        let window = chrono::Duration::from_std(REAUTH_AUTH_TIME_WINDOW).unwrap();
        let skew = chrono::Duration::from_std(REAUTH_CLOCK_SKEW).unwrap();
        let second = chrono::Duration::seconds(1);

        assert!(check_auth_time(None, now).is_err(), "absent");
        assert!(check_auth_time(Some(now), now).is_ok(), "just now");
        assert!(
            check_auth_time(Some(now - window), now).is_ok(),
            "window edge"
        );
        assert!(check_auth_time(Some(now - window - second), now).is_err());
        assert!(check_auth_time(Some(now + skew), now).is_ok(), "skew edge");
        assert!(check_auth_time(Some(now + skew + second), now).is_err());
    }
    use std::sync::atomic::Ordering;

    use openidconnect::core::CoreIdTokenClaims;

    use super::testing::{self, SECRET};
    use super::*;
    use crate::config::AuthConfig;

    fn config(idp: &testing::MockIdp) -> Config {
        Config {
            public_url: Some("https://okf.example.com".to_string()),
            auth: AuthConfig {
                oidc: vec![testing::provider(idp, "UNUSED")],
                ..AuthConfig::default()
            },
            ..Config::default()
        }
    }

    fn providers(idp: &testing::MockIdp) -> OidcProviders {
        OidcProviders::new(&config(idp), |_| Some(SECRET.to_string()), None)
            .expect("built")
            .expect("on")
    }

    #[tokio::test]
    async fn outbound_requests_carry_traceparent() {
        use crate::telemetry::propagation::testing::{assert_joins_the_callers_trace, echo_stub};
        let (url, seen) = echo_stub().await;
        let client = reqwest::Client::new();

        assert_joins_the_callers_trace(&seen, || async {
            let request = openidconnect::http::Request::builder()
                .uri(format!("{url}/.well-known/openid-configuration"))
                .body(Vec::new())
                .expect("a request");
            send(client.clone(), request).await.expect("answered");
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn discovery_is_cached_for_an_hour_and_kept_when_a_refresh_fails() {
        let idp = testing::start().await;
        let providers = providers(&idp);
        let start = Instant::now();

        providers
            .client("example", start)
            .await
            .expect("discovered");
        providers
            .client("example", start + Duration::from_secs(30 * 60))
            .await
            .expect("cached");
        assert_eq!(idp.discoveries(), 1);
        idp.discovery_down.store(true, Ordering::SeqCst);
        let later = start + Duration::from_secs(2 * 60 * 60);
        providers
            .client("example", later)
            .await
            .expect("the cached document is kept");
        assert_eq!(idp.discoveries(), 2);
        // After a failed refresh it waits a minute before asking again.
        providers
            .client("example", later + Duration::from_secs(30))
            .await
            .expect("still kept");
        assert_eq!(idp.discoveries(), 2);
        let fresh = self::providers(&idp).client("example", start).await;
        assert!(
            matches!(fresh, Err(OidcError::Discovery { .. })),
            "nothing cached yet"
        );
        let unknown = providers.client("nope", start).await;
        assert!(matches!(unknown, Err(OidcError::UnknownProvider(_))));
    }

    #[test]
    fn a_missing_client_secret_fails_start_up() {
        let config = Config {
            public_url: Some("https://okf.example.com".to_string()),
            auth: AuthConfig {
                oidc: vec![OidcProviderConfig {
                    id: "example".to_string(),
                    label: "Example ID".to_string(),
                    issuer: "https://id.example.com".to_string(),
                    client_id: "okf-console".to_string(),
                    client_secret_env: "OKF_OIDC_EXAMPLE_SECRET".to_string(),
                    scopes: vec!["openid".to_string()],
                }],
                ..AuthConfig::default()
            },
            ..Config::default()
        };

        let refused = OidcProviders::new(&config, |_| None, None);
        let empty = OidcProviders::new(&config, |_| Some(String::new()), None);

        assert!(matches!(refused, Err(OkfError::Config { .. })));
        assert!(matches!(empty, Err(OkfError::Config { .. })));
        assert!(OidcProviders::new(&Config::default(), |_| None, None)
            .expect("no providers")
            .is_none());
    }

    #[test]
    fn no_secret_reaches_debug_output() {
        let secrets = FlowSecrets {
            state: "state-value".to_string(),
            nonce: "nonce-value".to_string(),
            verifier: "verifier-value".to_string(),
        };
        let flow = Flow {
            provider: "example".to_string(),
            secrets,
            return_to: "/ui/".to_string(),
            link: Some(LinkRequest {
                user_id: "user-1".to_string(),
                session_hash: "session-value".to_string(),
                keep_sessions: false,
            }),
            reauth: Some(ReauthRequest {
                user_id: "user-1".to_string(),
                session_hash: "reauth-session-value".to_string(),
                subject: "subject-value".to_string(),
            }),
        };
        let proof = ReauthProof {
            user_id: "user-1".to_string(),
            provider: "example".to_string(),
            subject: "subject-value".to_string(),
            session_hash: "reauth-session-value".to_string(),
            stale: false,
        };
        let providers = OidcProviders::new(
            &Config {
                public_url: Some("https://okf.example.com".to_string()),
                auth: AuthConfig {
                    oidc: vec![OidcProviderConfig {
                        id: "example".to_string(),
                        label: "Example ID".to_string(),
                        issuer: "https://id.example.com".to_string(),
                        client_id: "okf-console".to_string(),
                        client_secret_env: "UNUSED".to_string(),
                        scopes: vec!["openid".to_string()],
                    }],
                    ..AuthConfig::default()
                },
                ..Config::default()
            },
            |_| Some("client-secret-value".to_string()),
            None,
        )
        .expect("built")
        .expect("on");

        let printed = format!("{flow:?} {proof:?} {providers:?}");

        for secret in [
            "state-value",
            "nonce-value",
            "verifier-value",
            "session-value",
            "subject-value",
            "client-secret-value",
        ] {
            assert!(!printed.contains(secret), "{secret} in {printed}");
        }
        assert!(printed.contains("example"), "{printed}");
    }

    #[test]
    fn email_verified_may_be_the_string_true() {
        // Some providers send `"email_verified": "true"`; the crate's
        // `accept-string-booleans` feature reads it as `true`.
        let claims = |verified: serde_json::Value| {
            serde_json::from_value::<CoreIdTokenClaims>(serde_json::json!({
                "iss": "https://id.example.com",
                "aud": "okf-console",
                "exp": 4_000_000_000_i64,
                "iat": 1_700_000_000_i64,
                "sub": "sub-1",
                "email": "alice@example.com",
                "email_verified": verified,
            }))
            .expect("claims parse")
        };

        assert_eq!(
            claims(serde_json::json!("true")).email_verified(),
            Some(true)
        );
        assert_eq!(
            claims(serde_json::json!("false")).email_verified(),
            Some(false)
        );
        assert_eq!(claims(serde_json::json!(true)).email_verified(), Some(true));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn link_starts_are_a_burst_of_ten_refilled_one_per_minute() {
        let idp = testing::start().await;
        let providers = providers(&idp);
        // A fixed starting instant; later ones are offsets from it, so the
        // test controls time exactly.
        let start = Instant::now();
        // `&providers`: the closures below copy this reference into each
        // future they return, instead of moving the providers.
        let providers = &providers;
        let take = |at: Instant| providers.may_start_link("user-1", at);
        // How many of `count` starts at `at` are allowed.
        let starts = |at: Instant, count: u32| async move {
            let mut allowed = 0;
            for _ in 0..count {
                if providers.may_start_link("user-1", at).await {
                    allowed += 1;
                }
            }
            allowed
        };

        let burst = starts(start, LINK_STARTS_PER_USER).await;
        let eleventh = take(start).await;
        let after_a_minute = start + Duration::from_secs(61);
        let one_more = take(after_a_minute).await;
        let not_two = take(after_a_minute).await;
        let refilled = after_a_minute + FLOW_TTL;
        let again = starts(refilled, LINK_STARTS_PER_USER).await;
        let past_the_burst = take(refilled).await;

        assert_eq!(burst, 10);
        assert!(!eleventh, "the eleventh start in a burst is refused");
        assert!(one_more, "a minute refills one start");
        assert!(!not_two, "and only one");
        assert_eq!(again, 10, "a full refill allows a whole burst again");
        assert!(!past_the_burst);
        assert!(
            providers.may_start_link("user-2", start).await,
            "users are apart"
        );
    }

    #[test]
    fn a_subject_postgres_would_refuse_or_too_long_is_invalid() {
        assert!(valid_subject("sub-1"));
        assert!(valid_subject(&"a".repeat(MAX_SUBJECT_LENGTH)));
        assert!(!valid_subject(""));
        assert!(!valid_subject("sub\0one"));
        assert!(!valid_subject("sub\none"));
        assert!(!valid_subject(&"a".repeat(MAX_SUBJECT_LENGTH + 1)));
    }
}
