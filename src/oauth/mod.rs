//! The built-in OAuth 2.1 authorization server and the OAuth side of the
//! resource server (M10; spec `docs/superpowers/specs/2026-11-11-okf-oauth-design.md`).
//! On only when `public_url` is set ([`OAuth::new`]).

pub mod access;
pub mod authorize;
pub mod clients;
pub mod consent;
pub mod error;
pub mod expiring;
pub mod metadata;
pub mod pkce;
pub mod ratelimit;
pub mod state;
#[cfg(test)]
pub(crate) mod testing;
pub mod token;

use std::sync::Arc;
use std::time::Duration;

use axum::http::{header, HeaderValue};
use axum::middleware;
use axum::response::Response;
use axum::Router;
use chrono::TimeDelta;

use crate::app::App;
use crate::config::Config;
use crate::error::OkfError;
use crate::shared::ephemeral::{Ephemeral, Keyspace};
use crate::shared::limits::GlobalLimit;
use crate::shared::Redis;
use crate::telemetry::metrics::Limiter;

/// Where the Protected Resource Metadata is served (RFC 9728), also with
/// the resource's path appended (`.../mcp`).
pub const PROTECTED_RESOURCE_PATH: &str = "/.well-known/oauth-protected-resource";

/// Where the Authorization Server Metadata is served (RFC 8414).
pub const AUTHORIZATION_SERVER_PATH: &str = "/.well-known/oauth-authorization-server";

/// Registration: a burst of 10, then one every 6 seconds.
const REGISTER_BURST: u32 = 10;
const REGISTER_PER_SEC: f64 = 1.0 / 6.0;
/// Token exchange: a burst of 60, then 2 a second.
const TOKEN_BURST: u32 = 60;
const TOKEN_PER_SEC: f64 = 2.0;
/// Consent posts: a burst of 20, then 1 a second (a person and a browser
/// journey post a few times per sign-in).
const CONSENT_BURST: u32 = 20;
const CONSENT_PER_SEC: f64 = 1.0;

/// The OAuth settings, derived once from the configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthSettings {
    /// The issuer: `public_url` without a trailing slash.
    pub issuer: String,
    /// The resource tokens are issued for: `<issuer>/mcp`.
    pub resource: String,
    /// The Protected Resource Metadata URL a 401 names.
    pub resource_metadata_url: String,
    /// `oauth.access_token_ttl`.
    pub access_token_ttl: Duration,
    /// `oauth.refresh_token_ttl`.
    pub refresh_token_ttl: Duration,
    /// `oauth.refresh_reuse_grace`, as the `TimeDelta` the store compares
    /// with the token's first use (zero turns the window off).
    pub refresh_reuse_grace: TimeDelta,
    /// `oauth.code_max_expiry`.
    pub code_max_expiry: Duration,
    /// `oauth.dcr_enabled`.
    pub dcr_enabled: bool,
    /// `oauth.cimd_allow_private`.
    pub cimd_allow_private: bool,
}

/// The authorization server's state, held by `App` when OAuth is on.
pub struct OAuth {
    /// The settings.
    pub settings: OAuthSettings,
    /// The 401 `WWW-Authenticate` value, built once.
    pub challenge: HeaderValue,
    /// Live access tokens (the resource side).
    pub access: access::AccessTokens,
    /// Consent pages in progress, by pending id: in memory, or in Redis
    /// with `[redis]` (under the id's hash), so a consent started on one
    /// replica can finish on another.
    pub pending: Ephemeral<state::PendingAuthorization>,
    /// Authorization codes, by the hex SHA-256 of the code ([`state::code_key`]):
    /// in memory, or in Redis with `[redis]` so any replica can exchange
    /// them. A used code stays until it expires, so a replay is seen.
    pub codes: Ephemeral<state::AuthorizationCode>,
    /// The Client ID Metadata Document fetcher.
    pub cimd: clients::CimdFetcher,
    /// `POST /oauth/register`'s limit: in Redis with `[redis]`, shared by
    /// every replica (issue #357), else this process's bucket.
    pub register_limit: GlobalLimit,
    /// `POST /oauth/token`'s (and `/oauth/revoke`'s) limit, likewise.
    pub token_limit: GlobalLimit,
    /// `POST /oauth/authorize`'s limit, likewise.
    pub consent_limit: GlobalLimit,
    // Test hook: the next authorization-code generation fails the way a
    // failed OS random source would (see `ForcedGenerationFailure`).
    #[cfg(test)]
    forced_generation_failure: std::sync::atomic::AtomicU8,
}

/// Which id [`OAuth::force_generation_failure`] makes fail (tests only).
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ForcedGenerationFailure {
    /// The authorization code itself.
    Code = 1,
    /// The refresh-token family id minted with it.
    FamilyId = 2,
}

impl OAuth {
    /// The OAuth state for a configuration that `Config::validate` accepted.
    ///
    /// # Arguments
    /// * `config` - the server's settings.
    /// * `redis` - the shared Redis, with `[redis]`: access-token revocations
    ///   are then counted there for every replica (spec Decision 15), and
    ///   pending authorizations and authorization codes are kept there (spec
    ///   Decision 5).
    ///
    /// # Returns
    /// `None` without `public_url` (OAuth off), else the state.
    ///
    /// # Errors
    /// [`OkfError::Config`] if `public_url` cannot appear in a header, and
    /// [`OkfError::LimitExceeded`] if `oauth.refresh_reuse_grace` is past
    /// chrono's range (`Config::validate` refuses both first, so only a
    /// caller that skipped validation sees these).
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn new(config: &Config, redis: Option<&Redis>) -> Result<Option<Self>, OkfError> {
        let Some(public_url) = config.public_url.as_deref() else {
            return Ok(None);
        };
        let issuer = public_url.trim_end_matches('/').to_string();
        let allow_private = config.oauth.cimd_allow_private;
        // Plain-http documents only on a test rig: private addresses allowed
        // and the server itself on loopback http.
        let allow_http = allow_private && issuer.starts_with("http://");
        let settings = OAuthSettings {
            resource: format!("{issuer}/mcp"),
            resource_metadata_url: format!("{issuer}{PROTECTED_RESOURCE_PATH}"),
            issuer,
            access_token_ttl: config.oauth.access_token_ttl,
            refresh_token_ttl: config.oauth.refresh_token_ttl,
            // `Config::validate` caps the window at ten minutes, well inside
            // chrono's range; only a caller that skipped validation fails.
            refresh_reuse_grace: TimeDelta::from_std(config.oauth.refresh_reuse_grace).map_err(
                |_| OkfError::LimitExceeded {
                    limit: "duration",
                    reason: "oauth.refresh_reuse_grace is too long".to_string(),
                },
            )?,
            code_max_expiry: config.oauth.code_max_expiry,
            dcr_enabled: config.oauth.dcr_enabled,
            cimd_allow_private: config.oauth.cimd_allow_private,
        };
        let challenge = HeaderValue::from_str(&format!(
            "Bearer resource_metadata=\"{}\"",
            settings.resource_metadata_url
        ))
        .map_err(|_| OkfError::Config {
            path: "public_url".to_string(),
            message: format!("{public_url:?} cannot appear in a header"),
        })?;
        Ok(Some(Self {
            settings,
            challenge,
            access: access::AccessTokens::new(redis.cloned()),
            // In Redis the key is `oauth:pending:<hashed(pending id)>`
            // (spec Decision 7), and `MAX_PENDING` applies in memory only
            // (spec Decision 8).
            pending: Ephemeral::new(
                redis,
                Keyspace::OauthPending,
                state::PENDING_TTL,
                state::MAX_PENDING,
            ),
            // In Redis the key is `oauth:code:<hashed(code_key(code))>`:
            // `Ephemeral` hashes every key it is given, and `code_key` is
            // already a hash. Hashing twice is harmless, so it is not
            // special-cased.
            codes: Ephemeral::new(
                redis,
                Keyspace::OauthCode,
                state::AUTHORIZATION_CODE_TTL,
                state::MAX_AUTHORIZATION_CODES,
            ),
            cimd: clients::CimdFetcher::new(allow_private, allow_http)?,
            register_limit: GlobalLimit::new(
                Limiter::OauthRegister,
                REGISTER_BURST,
                REGISTER_PER_SEC,
                redis,
            ),
            token_limit: GlobalLimit::new(Limiter::OauthToken, TOKEN_BURST, TOKEN_PER_SEC, redis),
            consent_limit: GlobalLimit::new(
                Limiter::OauthConsent,
                CONSENT_BURST,
                CONSENT_PER_SEC,
                redis,
            ),
            #[cfg(test)]
            forced_generation_failure: std::sync::atomic::AtomicU8::new(0),
        }))
    }

    /// Makes the next authorization-code generation fail on `which` (tests
    /// only: the OS random source cannot be made to fail).
    #[cfg(test)]
    pub(crate) fn force_generation_failure(&self, which: ForcedGenerationFailure) {
        self.forced_generation_failure
            .store(which as u8, std::sync::atomic::Ordering::SeqCst);
    }

    /// The failure [`OAuth::force_generation_failure`] armed, cleared as it
    /// is read, if any.
    #[cfg(test)]
    pub(crate) fn take_forced_generation_failure(&self) -> Option<ForcedGenerationFailure> {
        match self
            .forced_generation_failure
            .swap(0, std::sync::atomic::Ordering::SeqCst)
        {
            1 => Some(ForcedGenerationFailure::Code),
            2 => Some(ForcedGenerationFailure::FamilyId),
            _ => None,
        }
    }
}

/// Every OAuth route (`/.well-known/*`, `/oauth/*`), for `http::router` to
/// merge when OAuth is on. No route needs a bearer token.
///
/// # Returns
/// A router whose responses all carry `Cache-Control: no-store`.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "router builder: runs once at startup, outside any request, so a span would be an orphan root"
    )
)]
pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .merge(metadata::routes())
        .merge(clients::routes())
        .merge(authorize::routes())
        .merge(token::routes())
        .layer(middleware::map_response(no_store))
}

/// Marks a response uncacheable: tokens, codes and consent pages must never
/// be stored by a cache.
#[tracing::instrument(level = "debug", skip_all)]
async fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_is_off_without_public_url() {
        assert!(OAuth::new(&Config::default(), None).unwrap().is_none());
    }

    #[test]
    fn settings_derive_from_public_url() {
        let config = crate::store::oauth::testing::oauth_config("https://okf.example.com/");

        let oauth = OAuth::new(&config, None).unwrap().expect("on");

        assert_eq!(oauth.settings.issuer, "https://okf.example.com");
        assert_eq!(oauth.settings.resource, "https://okf.example.com/mcp");
        assert_eq!(
            oauth.settings.resource_metadata_url,
            "https://okf.example.com/.well-known/oauth-protected-resource"
        );
        assert_eq!(
            oauth.challenge,
            "Bearer resource_metadata=\"https://okf.example.com/.well-known/oauth-protected-resource\""
        );
        assert_eq!(
            oauth.settings.access_token_ttl,
            config.oauth.access_token_ttl
        );
    }
}
