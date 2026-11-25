//! The configuration file (TOML; the design's "Configuration"). Each
//! milestone reads the fields it uses (M7 added `time_server`). Unknown keys
//! are ignored, so one file serves every version of the server.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::Path;
use std::time::Duration;

// `Uri` and `Authority` are the `http` crate's parsed URL and `host[:port]`
// types, re-exported by axum; the MCP `Host` check parses `allowed_hosts`
// with the same `Authority`.
use axum::http::uri::{Authority, Uri};
use bytesize::ByteSize;
// lettre's parsed `Name <address>`; `mail.from` must parse as one.
use lettre::message::Mailbox;
use serde::{Deserialize, Deserializer};

use crate::store::{catalogs, settings};

use crate::error::OkfError;
use crate::telemetry::config::TelemetryConfig;

/// The default import limit: 64 MiB, uncompressed.
pub const DEFAULT_MAX_IMPORT_BYTES: u64 = 64 * 1024 * 1024;

/// The default listen address: every IPv4 interface, port 8080.
pub const DEFAULT_LISTEN: SocketAddr =
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 8080));

/// rmcp's default `Host` allow-list (loopback only): right for a local run,
/// wrong behind an ingress, where the config must name the public host.
pub const DEFAULT_ALLOWED_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "::1"];

/// How long an MCP session may sit idle before it is closed.
pub const DEFAULT_SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);

/// The default for `worker_shutdown_grace`: how long `serve` waits for the
/// mail worker to finish the mail in hand once it is told to stop. It is the
/// longest a provider may take to answer a send (30 s for Postmark and SES).
pub const DEFAULT_WORKER_SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

/// The default `get_concept` outline threshold: bodies over 20,000 characters.
pub const DEFAULT_CONCEPT_OUTLINE_CHARS: usize = 20_000;

/// The default database pool size.
pub const DEFAULT_DATABASE_MAX_CONNECTIONS: u32 = 16;

/// The default `store_max_connections`: the pool size of each catalog store.
pub const DEFAULT_STORE_MAX_CONNECTIONS: u32 = 8;

/// The default `store_idle_timeout`: how long an idle store connection is
/// kept before it is closed.
pub const DEFAULT_STORE_IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// The default `store_connect_timeout`: how long opening a store's pool, or
/// waiting for one of its connections, may take before the store is
/// reported unavailable.
pub const DEFAULT_STORE_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The default `overlay_cache_bytes`: 256 MiB of estimated overlay memory.
pub const DEFAULT_OVERLAY_CACHE_BYTES: u64 = 256 * 1024 * 1024;

/// The default `catalog_cache_bytes`: 1 GiB of estimated snapshot memory.
pub const DEFAULT_CATALOG_CACHE_BYTES: u64 = 1024 * 1024 * 1024;

/// The default `catalog_directory_entries`: catalog rows kept by name.
pub const DEFAULT_CATALOG_DIRECTORY_ENTRIES: u64 = 100_000;

/// The default `search_fanout_max`: the most catalogs `GET /api/v1/search`
/// searches when the request names none (M18 spec Decision 8).
pub const DEFAULT_SEARCH_FANOUT_MAX: u32 = 25;

/// The default `catalog_ttl`: how long a cached catalog row is served.
pub const DEFAULT_CATALOG_TTL: Duration = Duration::from_secs(10);

/// The default `credential_cache_entries`: keys and OAuth grants kept, by
/// hash and by id.
pub const DEFAULT_CREDENTIAL_CACHE_ENTRIES: u64 = 100_000;

/// The default `user_cache_entries`: users kept, by id.
pub const DEFAULT_USER_CACHE_ENTRIES: u64 = 100_000;

/// The default `credential_ttl`: how long a cached credential or user is
/// served before it is read again.
pub const DEFAULT_CREDENTIAL_TTL: Duration = Duration::from_secs(10);

/// The default lifetime of an OAuth access token. Access tokens stay
/// revocable at once (the access-token cache and its revocation counter),
/// so a long one costs little; each refresh is a chance for a client's
/// processes to race (`oauth.refresh_reuse_grace`).
pub const DEFAULT_ACCESS_TOKEN_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// The default lifetime of an OAuth refresh token (restarted by each rotation).
pub const DEFAULT_REFRESH_TOKEN_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// The default window in which a used refresh token rotates again instead of
/// revoking its family.
pub const DEFAULT_REFRESH_REUSE_GRACE: Duration = Duration::from_secs(60);

/// The longest `oauth.refresh_reuse_grace` may be: past this, a copied
/// refresh token used soon after the real one would go unnoticed for too long.
pub const MAX_REFRESH_REUSE_GRACE: Duration = Duration::from_secs(10 * 60);

/// The default idle period after which housekeeping revokes an OAuth grant
/// that holds no live token.
pub const DEFAULT_IDLE_GRANT_REVOKE_AFTER: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// The default cap on a sign-in code's `--expires`.
pub const DEFAULT_CODE_MAX_EXPIRY: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The default number of live self-issued keys per user.
pub const DEFAULT_MAX_KEYS_PER_USER: u32 = 20;

/// The most `max_keys_per_user` may be set to.
pub const MAX_KEYS_PER_USER_LIMIT: u32 = 1000;

/// The default console session idle timeout.
pub const DEFAULT_CONSOLE_SESSION_IDLE: Duration = Duration::from_secs(12 * 60 * 60);

/// The default console session absolute lifetime.
pub const DEFAULT_CONSOLE_SESSION_ABSOLUTE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The default lifetime of a setup link.
pub const DEFAULT_SETUP_LINK_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// The default lifetime of a password-reset link.
pub const DEFAULT_RESET_LINK_TTL: Duration = Duration::from_secs(60 * 60);

/// The `[oauth]` section: used only when `public_url` is set.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct OAuthConfig {
    /// How long an access token is accepted (`"1h"`).
    #[serde(with = "humantime_serde")]
    pub access_token_ttl: Duration,
    /// How long a refresh token is accepted after it is issued (`"30d"`);
    /// each rotation issues a new one, so the lifetime slides.
    #[serde(with = "humantime_serde")]
    pub refresh_token_ttl: Duration,
    /// How long after its first use a refresh token presented again rotates
    /// again instead of revoking its family (`"60s"`; `"0s"` turns it off).
    /// Processes of one client that share a credential refresh at once;
    /// only one can win, and the rest would otherwise sign it out.
    #[serde(with = "humantime_serde")]
    pub refresh_reuse_grace: Duration,
    /// How long an OAuth grant may go without a token being issued for it
    /// before housekeeping revokes it (`"30d"`). A grant that still holds
    /// a live token is never revoked, whatever this says.
    #[serde(with = "humantime_serde")]
    pub idle_grant_revoke_after: Duration,
    /// The longest `okf-mcp oauth code create --expires` accepts (`"7d"`).
    #[serde(with = "humantime_serde")]
    pub code_max_expiry: Duration,
    /// Whether `POST /oauth/register` (RFC 7591) is offered.
    pub dcr_enabled: bool,
    /// Lets a Client ID Metadata Document live on a private or loopback
    /// address. Only for test rigs: it opens the fetcher to the internal network.
    pub cimd_allow_private: bool,
}

impl Default for OAuthConfig {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "runs before the subscriber exists (configuration, builder or start-up), so a span would be recorded nowhere"
        )
    )]
    fn default() -> Self {
        Self {
            access_token_ttl: DEFAULT_ACCESS_TOKEN_TTL,
            refresh_token_ttl: DEFAULT_REFRESH_TOKEN_TTL,
            refresh_reuse_grace: DEFAULT_REFRESH_REUSE_GRACE,
            idle_grant_revoke_after: DEFAULT_IDLE_GRANT_REVOKE_AFTER,
            code_max_expiry: DEFAULT_CODE_MAX_EXPIRY,
            dcr_enabled: true,
            cimd_allow_private: false,
        }
    }
}

/// The `[auth]` section: the defaults for the console's sign-in policy and
/// branding. The policy and branding keys can be overridden at run time by
/// `server_settings` (`store::settings`); `trusted_proxy_header` and `oidc`
/// are config-only.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct AuthConfig {
    /// Whether `/signin` offers email and password.
    pub password_sign_in: bool,
    /// Live self-issued keys a user may hold.
    pub max_keys_per_user: u32,
    /// The longest expiry a self-issued key may have; `None` is no limit.
    // humantime_serde reads `Option<Duration>` too: a missing key is `None`.
    #[serde(with = "humantime_serde")]
    pub max_key_lifetime: Option<Duration>,
    /// How long a console session may sit idle (`"12h"`).
    #[serde(with = "humantime_serde")]
    pub console_session_idle: Duration,
    /// How long a console session lasts at most (`"7d"`).
    #[serde(with = "humantime_serde")]
    pub console_session_absolute: Duration,
    /// How long a setup link works (`"24h"`).
    #[serde(with = "humantime_serde")]
    pub setup_link_ttl: Duration,
    /// How long a password-reset link works (`"1h"`).
    #[serde(with = "humantime_serde")]
    pub reset_link_ttl: Duration,
    /// The header whose last hop is the client's address behind a proxy
    /// (`"x-forwarded-for"`); unset means the peer address (Decision 7).
    pub trusted_proxy_header: Option<String>,
    /// The OIDC providers (`[[auth.oidc]]`); none by default.
    pub oidc: Vec<OidcProviderConfig>,
    /// The default product name people see in mail, on the server's pages,
    /// in the console and in passkey prompts
    /// ([`settings::DEFAULT_PRODUCT_NAME`] when unset).
    pub product_name: String,
    /// The default support contact (an email address or `https://` URL);
    /// `None` shows no contact line.
    pub support_contact: Option<String>,
    /// The default `https://` URL of the console's "Send feedback" link;
    /// `None` shows no link.
    pub feedback_url: Option<String>,
    /// The default `https://` URL of the console's "Documentation" link;
    /// `None` shows no link.
    pub docs_url: Option<String>,
    /// The default `https://` URL of the console's "Billing" link (a
    /// reusable billing-portal login, never a customer-specific URL);
    /// `None` shows no link.
    pub billing_url: Option<String>,
    /// The default `https://` URL a first password setup redirects to (a
    /// getting-started guide); `None` goes to the console.
    pub onboarding_url: Option<String>,
    /// The name the MCP server reports in `initialize` (`serverInfo.name`;
    /// [`DEFAULT_MCP_SERVER_NAME`] when unset). Config-only: it is an
    /// identifier, not display text (`serverInfo.title` is the product
    /// name). 1 to [`MAX_MCP_SERVER_NAME_CHARS`] of ASCII letters, digits,
    /// `-`, `_` and `.`.
    pub mcp_server_name: String,
}

/// The MCP server's reported name with no `[auth] mcp_server_name`: this
/// crate's name, read at compile time.
pub const DEFAULT_MCP_SERVER_NAME: &str = env!("CARGO_PKG_NAME");

/// The longest `[auth] mcp_server_name`, in characters.
pub const MAX_MCP_SERVER_NAME_CHARS: usize = 64;

/// One `[[auth.oidc]]` provider (spec "OIDC"). The client secret is read
/// from the environment variable `client_secret_env` at start-up, never
/// from this file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
// A misspelt key (or an inline `client_secret`) is an error, not ignored.
#[serde(deny_unknown_fields)]
pub struct OidcProviderConfig {
    /// Stable id: stored in `user_identities.provider` and part of the
    /// redirect URI (`a-z`, `0-9`, `-`; at most 32).
    pub id: String,
    /// The sign-in button's text ("Sign in with `label`").
    pub label: String,
    /// The issuer URL; discovery reads `<issuer>/.well-known/openid-configuration`.
    pub issuer: String,
    /// The client id registered with the provider.
    pub client_id: String,
    /// The environment variable holding the client secret.
    pub client_secret_env: String,
    /// The scopes asked for (`openid` is always sent).
    #[serde(default = "default_oidc_scopes")]
    pub scopes: Vec<String>,
}

/// Whether `id` is a well-formed `[[auth.oidc]]` id: 1 to 32 of `a-z`,
/// `0-9` and `-`. Config validation requires it, and unlinking uses it to
/// answer a malformed provider (a NUL, which Postgres refuses, included)
/// as unknown before any query.
///
/// # Arguments
/// * `id` - the candidate id.
///
/// # Returns
/// `true` when it is well formed.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) fn is_provider_id(id: &str) -> bool {
    (1..=32).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// `["openid", "email", "profile"]`.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "runs before the subscriber exists (configuration, builder or start-up), so a span would be recorded nowhere"
    )
)]
fn default_oidc_scopes() -> Vec<String> {
    Vec::from(["openid", "email", "profile"].map(String::from))
}

impl Default for AuthConfig {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "runs before the subscriber exists (configuration, builder or start-up), so a span would be recorded nowhere"
        )
    )]
    fn default() -> Self {
        Self {
            password_sign_in: true,
            max_keys_per_user: DEFAULT_MAX_KEYS_PER_USER,
            max_key_lifetime: None,
            console_session_idle: DEFAULT_CONSOLE_SESSION_IDLE,
            console_session_absolute: DEFAULT_CONSOLE_SESSION_ABSOLUTE,
            setup_link_ttl: DEFAULT_SETUP_LINK_TTL,
            reset_link_ttl: DEFAULT_RESET_LINK_TTL,
            trusted_proxy_header: None,
            oidc: Vec::new(),
            product_name: settings::DEFAULT_PRODUCT_NAME.to_string(),
            support_contact: None,
            feedback_url: None,
            docs_url: None,
            billing_url: None,
            onboarding_url: None,
            mcp_server_name: DEFAULT_MCP_SERVER_NAME.to_string(),
        }
    }
}

impl AuthConfig {
    /// Checks the values serde cannot.
    ///
    /// # Returns
    /// `Ok(())`, or the problem in words.
    ///
    /// # Errors
    /// A zero or out-of-range duration, `max_keys_per_user` above
    /// [`MAX_KEYS_PER_USER_LIMIT`], a `trusted_proxy_header` that is not
    /// an HTTP header name, a malformed `mcp_server_name`, or a branding
    /// default the matching setting would refuse.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "runs before the subscriber exists (configuration, builder or start-up), so a span would be recorded nowhere"
        )
    )]
    fn validate(&self) -> Result<(), String> {
        for (name, value) in [
            ("auth.console_session_idle", self.console_session_idle),
            (
                "auth.console_session_absolute",
                self.console_session_absolute,
            ),
            ("auth.setup_link_ttl", self.setup_link_ttl),
            ("auth.reset_link_ttl", self.reset_link_ttl),
        ] {
            check_auth_duration(name, value)?;
        }
        if let Some(lifetime) = self.max_key_lifetime {
            check_auth_duration("auth.max_key_lifetime", lifetime)?;
        }
        if self.max_keys_per_user > MAX_KEYS_PER_USER_LIMIT {
            return Err(format!(
                "auth.max_keys_per_user must be at most {MAX_KEYS_PER_USER_LIMIT}"
            ));
        }
        if let Some(header) = self.trusted_proxy_header.as_deref() {
            // The same parser axum uses for request headers; it refuses an
            // empty name too.
            if axum::http::HeaderName::from_bytes(header.as_bytes()).is_err() {
                return Err(format!(
                    "auth.trusted_proxy_header {header:?} is not an HTTP header name"
                ));
            }
        }
        let name = &self.mcp_server_name;
        let well_formed = name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        if name.is_empty() || name.len() > MAX_MCP_SERVER_NAME_CHARS || !well_formed {
            return Err(format!(
                "auth.mcp_server_name takes 1 to {MAX_MCP_SERVER_NAME_CHARS} ASCII letters, \
                 digits, '-', '_' and '.', not {name:?}"
            ));
        }
        // The same checks `okf-mcp settings set` applies, so a default the
        // file gives is one the Settings page could have stored.
        settings::check_branding_defaults(self).map_err(|problem| format!("auth.{problem}"))
    }

    /// The branding this file gives: what people see where no setting
    /// overrides it. The product name is trimmed, as a stored one is.
    ///
    /// # Returns
    /// The default product name, support contact, feedback URL and
    /// documentation URL.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: maps a value, no I/O"
        )
    )]
    pub fn default_brand(&self) -> settings::Brand {
        settings::Brand {
            product_name: self.product_name.trim().to_string(),
            support_contact: self.support_contact.clone(),
            feedback_url: self.feedback_url.clone(),
            docs_url: self.docs_url.clone(),
            billing_url: self.billing_url.clone(),
        }
    }
}

/// The longest `[auth]` or `[oauth]` lifetime the config and settings
/// accept: ten years, in humantime's own year (365.25 days), so
/// `"10years"` is accepted exactly. It is far inside `DateTime<Utc>`'s
/// calendar range: added to any real `now` it is always a valid
/// timestamp, so a lifetime that passed the check can never make
/// `clock::after` fail.
pub const MAX_AUTH_DURATION: Duration = Duration::from_secs(10 * 31_557_600);

/// Checks one lifetime setting: above zero and at most
/// [`MAX_AUTH_DURATION`]. Shared by the config file (`[oauth]` and
/// `[auth]`) and `store::settings` (both when a value is set and when a
/// stored one is loaded), so a value is accepted or refused the same way
/// everywhere.
///
/// # Arguments
/// * `name` - the key, for the message.
/// * `value` - the duration.
///
/// # Returns
/// `Ok(())`, or the problem in words.
///
/// # Errors
/// A zero duration, or one longer than [`MAX_AUTH_DURATION`].
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "runs before the subscriber exists (configuration, builder or start-up), so a span would be recorded nowhere"
    )
)]
pub(crate) fn check_auth_duration(name: &str, value: Duration) -> Result<(), String> {
    if value.is_zero() {
        return Err(format!("{name} must be more than zero"));
    }
    if value > MAX_AUTH_DURATION {
        return Err(format!("{name} must be at most 10 years"));
    }
    Ok(())
}

/// The default From address: a placeholder that is valid, so a server
/// without `[mail]` starts (with the log provider, it sends nothing).
pub const DEFAULT_MAIL_FROM: &str = "okf <okf@localhost>";

/// The default SMTP port: submission with STARTTLS.
pub const DEFAULT_SMTP_PORT: u16 = 587;

/// The default Postmark message stream (its transactional one).
pub const DEFAULT_POSTMARK_STREAM: &str = "outbound";

/// Which [`crate::mail::Mailer`] sends mail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MailProvider {
    /// An SMTP relay (`[mail.smtp]`).
    Smtp,
    /// Postmark's HTTP API (`[mail.postmark]`).
    Postmark,
    /// Amazon SES through the AWS SDK (`[mail.ses]`, optional), with the
    /// standard AWS credential chain (EKS Pod Identity among it).
    Ses,
    /// Writes each message, links included, to the log at `info`. For
    /// development and tests only.
    #[default]
    Log,
}

impl MailProvider {
    /// The config spelling.
    ///
    /// # Returns
    /// `"smtp"`, `"postmark"`, `"ses"` or `"log"`, as `[mail] provider`
    /// takes it.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "a pure match returning a closed value: a span would record nothing new"
        )
    )]
    pub fn as_str(self) -> &'static str {
        match self {
            MailProvider::Smtp => "smtp",
            MailProvider::Postmark => "postmark",
            MailProvider::Ses => "ses",
            MailProvider::Log => "log",
        }
    }
}

/// `[mail.smtp]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SmtpConfig {
    /// The relay's host name.
    pub host: String,
    /// Its port (587 for STARTTLS, 465 for implicit TLS).
    #[serde(default = "default_smtp_port")]
    pub port: u16,
    /// `true`: STARTTLS, required (the session never continues in clear).
    /// `false`: TLS from the first byte. There is no plaintext mode.
    #[serde(default = "default_true")]
    pub starttls: bool,
    /// The user to authenticate as, if the relay wants one.
    #[serde(default)]
    pub username: Option<String>,
    /// The environment variable holding the password (required with
    /// `username`).
    #[serde(default)]
    pub password_env: Option<String>,
}

/// `[mail.postmark]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PostmarkConfig {
    /// The environment variable holding the server token.
    pub token_env: String,
    /// The message stream (`"outbound"`, Postmark's default transactional stream).
    #[serde(default = "default_message_stream")]
    pub message_stream: String,
}

/// `[mail.ses]`. Credentials are never here: the AWS SDK's standard chain
/// finds them (environment, profile, web identity, or the container
/// provider that EKS Pod Identity configures).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct SesConfig {
    /// The AWS region to send in (`"us-east-1"`). Unset: the SDK's own
    /// resolution (`AWS_REGION`, then the profile, then instance metadata).
    pub region: Option<String>,
}

// `serde(default = "...")` takes a function's name, not a value.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "runs before the subscriber exists (configuration, builder or start-up), so a span would be recorded nowhere"
    )
)]
fn default_smtp_port() -> u16 {
    DEFAULT_SMTP_PORT
}

#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "runs before the subscriber exists (configuration, builder or start-up), so a span would be recorded nowhere"
    )
)]
fn default_true() -> bool {
    true
}

#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "runs before the subscriber exists (configuration, builder or start-up), so a span would be recorded nowhere"
    )
)]
fn default_message_stream() -> String {
    DEFAULT_POSTMARK_STREAM.to_string()
}

/// The `[mail]` section. Secrets are not here: only the names of the
/// environment variables that hold them, read when the server starts.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct MailConfig {
    /// Which provider sends.
    pub provider: MailProvider,
    /// The From address (`"okf <okf@example.com>"`).
    pub from: String,
    /// Deprecated and ignored: every `serve` runs the mail worker, so a
    /// Deployment of several replicas needs no per-pod mail setting (issue
    /// #458). The key still parses, so a configuration that has it keeps
    /// starting; [`Config::deprecation_warnings`] names it. The worker's
    /// pool is shared with the housekeeping worker: up to 3 connections
    /// beside `database_max_connections`.
    pub worker: Option<bool>,
    /// The SMTP relay, for `provider = "smtp"`.
    pub smtp: Option<SmtpConfig>,
    /// Postmark, for `provider = "postmark"`.
    pub postmark: Option<PostmarkConfig>,
    /// SES, for `provider = "ses"` (optional: every key has a default).
    pub ses: Option<SesConfig>,
}

impl Default for MailConfig {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "runs before the subscriber exists (configuration, builder or start-up), so a span would be recorded nowhere"
        )
    )]
    fn default() -> Self {
        Self {
            provider: MailProvider::Log,
            from: DEFAULT_MAIL_FROM.to_string(),
            worker: None,
            smtp: None,
            postmark: None,
            ses: None,
        }
    }
}

impl MailConfig {
    /// Checks what serde cannot: that the chosen provider is configured,
    /// and that `from` is one address.
    ///
    /// # Returns
    /// `Ok(())`, or the problem in words.
    ///
    /// # Errors
    /// A `from` that is not one mailbox; `provider = "smtp"` without a
    /// `[mail.smtp]` table, with a blank host, port 0, or a `username`
    /// without `password_env` (or the reverse); `provider = "postmark"`
    /// without a `[mail.postmark]` table, or with a blank `token_env` or
    /// `message_stream`; `provider = "ses"` with a blank `region`.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "runs before the subscriber exists (configuration, builder or start-up), so a span would be recorded nowhere"
        )
    )]
    fn validate(&self) -> Result<(), String> {
        self.from
            .parse::<Mailbox>()
            .map_err(|error| format!("mail.from {:?} is not an address: {error}", self.from))?;
        match self.provider {
            MailProvider::Log => Ok(()),
            MailProvider::Smtp => {
                // `let ... else`: bind the `Some`, or leave the function.
                let Some(smtp) = &self.smtp else {
                    return Err("mail.provider = \"smtp\" needs a [mail.smtp] table".to_string());
                };
                if smtp.host.trim().is_empty() {
                    return Err("mail.smtp.host is blank".to_string());
                }
                if smtp.port == 0 {
                    return Err("mail.smtp.port must be more than zero".to_string());
                }
                if smtp.username.is_some() != smtp.password_env.is_some() {
                    return Err(
                        "mail.smtp.username and mail.smtp.password_env go together".to_string()
                    );
                }
                if smtp
                    .password_env
                    .as_deref()
                    .is_some_and(|name| name.trim().is_empty())
                {
                    return Err("mail.smtp.password_env is blank".to_string());
                }
                Ok(())
            }
            MailProvider::Postmark => {
                let Some(postmark) = &self.postmark else {
                    return Err(
                        "mail.provider = \"postmark\" needs a [mail.postmark] table".to_string()
                    );
                };
                if postmark.token_env.trim().is_empty() {
                    return Err("mail.postmark.token_env is blank".to_string());
                }
                if postmark.message_stream.trim().is_empty() {
                    return Err("mail.postmark.message_stream is blank".to_string());
                }
                Ok(())
            }
            MailProvider::Ses => {
                // `and_then` reaches through both `Option`s: the table, then
                // its key. A missing region is fine (the SDK resolves one at
                // start-up); a blank one is a mistake.
                if self
                    .ses
                    .as_ref()
                    .and_then(|ses| ses.region.as_deref())
                    .is_some_and(|region| region.trim().is_empty())
                {
                    return Err("mail.ses.region is blank".to_string());
                }
                Ok(())
            }
        }
    }
}

/// The default `[feedback] max_per_hour`: submissions one credential may
/// make in any hour.
pub const DEFAULT_FEEDBACK_MAX_PER_HOUR: u32 = 10;

/// The most `[feedback] max_per_hour` may be set to.
pub const MAX_FEEDBACK_MAX_PER_HOUR: u32 = 1000;

/// The default `[feedback] retention`: how long forwarded feedback is kept.
pub const DEFAULT_FEEDBACK_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// The shortest `[feedback] retention`: one day, longer than the hour the
/// rate limit counts back over, so pruning never hides a recent submission.
pub const MIN_FEEDBACK_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

/// The longest `[feedback] retention`: ten years.
pub const MAX_FEEDBACK_RETENTION: Duration = Duration::from_secs(10 * 366 * 24 * 60 * 60);

/// The longest Forminit form id accepted, in bytes.
pub const MAX_FORMINIT_FORM_ID_BYTES: usize = 64;

#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "serde default for a config key, read before the subscriber exists"
    )
)]
fn default_feedback_max_per_hour() -> u32 {
    DEFAULT_FEEDBACK_MAX_PER_HOUR
}

#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "serde default for a config key, read before the subscriber exists"
    )
)]
fn default_feedback_retention() -> Duration {
    DEFAULT_FEEDBACK_RETENTION
}

/// Is `name` usable as an environment variable's name: one or more ASCII
/// letters, digits and `_`?
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "config check run once at start-up, like `Config::validate`"
    )
)]
fn is_env_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// The `[feedback]` table (issue #674). Absent: `send_feedback` is not
/// offered and nothing is sent anywhere. Present: the tool queues each
/// submission and a worker forwards it to the operator's Forminit form,
/// `https://forminit.com/f/<forminit_form_id>`.
// `deny_unknown_fields`: a misspelt key (or an `api_key = ...` that would put
// the key in the file) is a parse error instead of being ignored.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedbackConfig {
    /// The Forminit form's id: the last segment of its endpoint URL. Not a
    /// secret. 1 to 64 ASCII letters, digits, `-` and `_`, so it can only
    /// ever name a path segment under `https://forminit.com/f/`.
    pub forminit_form_id: String,
    /// The environment variable holding the form's API key (sent as
    /// `X-API-KEY`), if any. Without a key Forminit accepts one submission
    /// per 5 seconds per address; with one, 5 per second.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Whether `serve` runs the feedback worker (default `true`). With
    /// several replicas, set it on exactly one, as `[mail] worker`.
    #[serde(default = "default_true")]
    pub worker: bool,
    /// Submissions one credential may make in any hour (default 10).
    #[serde(default = "default_feedback_max_per_hour")]
    pub max_per_hour: u32,
    /// How long forwarded (`sent`) feedback is kept before housekeeping
    /// deletes it (default `"30d"`, at least `"1d"`).
    #[serde(default = "default_feedback_retention", with = "humantime_serde")]
    pub retention: Duration,
    /// Test only: the origin the form's URL is built on instead of
    /// `https://forminit.com` (a mock server). Not readable from TOML, and
    /// not compiled outside tests.
    #[cfg(test)]
    #[serde(skip)]
    pub base_url_for_tests: Option<String>,
}

impl FeedbackConfig {
    /// A `[feedback]` table for `form_id` with every other key at its
    /// default.
    ///
    /// # Arguments
    /// * `form_id` - the Forminit form id.
    ///
    /// # Returns
    /// The table.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "runs before the subscriber exists (configuration, builder or start-up), so a span would be recorded nowhere"
        )
    )]
    #[must_use]
    pub fn for_form(form_id: &str) -> Self {
        Self {
            forminit_form_id: form_id.to_string(),
            api_key_env: None,
            worker: true,
            max_per_hour: DEFAULT_FEEDBACK_MAX_PER_HOUR,
            retention: DEFAULT_FEEDBACK_RETENTION,
            #[cfg(test)]
            base_url_for_tests: None,
        }
    }

    /// Checks what serde cannot.
    ///
    /// # Returns
    /// `Ok(())` when the table is usable.
    ///
    /// # Errors
    /// A message naming the bad key: a `forminit_form_id` that is empty,
    /// over 64 bytes or holds a character outside `[A-Za-z0-9_-]` (so it can
    /// never change the URL's host or path); an `api_key_env` that is not a
    /// variable name; a `max_per_hour` of 0 or over 1000; a `retention`
    /// under a day or over ten years.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "config check run once at start-up, like `Config::validate`"
        )
    )]
    pub fn validate(&self) -> Result<(), String> {
        if !is_forminit_form_id(&self.forminit_form_id) {
            return Err(format!(
                "feedback.forminit_form_id must be 1 to {MAX_FORMINIT_FORM_ID_BYTES} of A-Z, a-z, 0-9, - and _"
            ));
        }
        if self
            .api_key_env
            .as_deref()
            .is_some_and(|name| !is_env_name(name))
        {
            return Err(
                "feedback.api_key_env must name an environment variable (letters, digits and _)"
                    .to_string(),
            );
        }
        if self.max_per_hour == 0 || self.max_per_hour > MAX_FEEDBACK_MAX_PER_HOUR {
            return Err(format!(
                "feedback.max_per_hour must be 1 to {MAX_FEEDBACK_MAX_PER_HOUR}"
            ));
        }
        if self.retention < MIN_FEEDBACK_RETENTION || self.retention > MAX_FEEDBACK_RETENTION {
            return Err(
                "feedback.retention must be at least 1 day and at most 10 years".to_string(),
            );
        }
        Ok(())
    }
}

/// Is `id` a usable Forminit form id: 1 to [`MAX_FORMINIT_FORM_ID_BYTES`]
/// ASCII letters, digits, `-` and `_`? Nothing else can appear, so the id
/// is one path segment and can never add a `/`, `?`, `#`, `@` or `..`.
///
/// # Arguments
/// * `id` - the configured id.
///
/// # Returns
/// Whether it is acceptable.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "config check run once at start-up, like `Config::validate`"
    )
)]
#[must_use]
pub fn is_forminit_form_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_FORMINIT_FORM_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// The longest `[redis] key_prefix` accepted, in bytes.
const MAX_REDIS_KEY_PREFIX_BYTES: usize = 64;

#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "serde default for a config key, read before the subscriber exists"
    )
)]
fn default_key_prefix() -> String {
    "okf:".to_string()
}

/// Which Redis client `[redis]` uses (spec Decision 20).
// `rename_all = "lowercase"` makes the TOML spelling `"standalone"` or
// `"cluster"`; anything else is a parse error (Python: an Enum lookup).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RedisMode {
    /// One Redis or Valkey primary (`ConnectionManager`).
    #[default]
    Standalone,
    /// A cluster, including ElastiCache Serverless (`ClusterClient`).
    Cluster,
}

/// The `[redis]` table (spec Decision 1). Absent: every short-lived state stays
/// in memory and one replica is required. Present: `serve` keeps it in Redis.
// `deny_unknown_fields` makes a misspelt key (or a `url = ...` that would put a
// password in the file) a parse error instead of being ignored silently.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedisConfig {
    /// The environment variable holding the Redis URL (`redis://` or
    /// `rediss://`; it may carry a password, so it never goes in the file).
    pub url_env: String,
    /// Prepended to every key and channel, so one Redis can serve several
    /// deployments. Default `"okf:"`.
    #[serde(default = "default_key_prefix")]
    pub key_prefix: String,
    /// `"standalone"` (default) or `"cluster"` (ElastiCache Serverless and
    /// other clusters). Every key and channel carries one hash tag in both
    /// modes (spec Decision 20).
    #[serde(default)]
    pub mode: RedisMode,
}

impl RedisConfig {
    /// Checks the variable name and the key prefix.
    ///
    /// # Returns
    /// `Ok(())` when both are usable.
    ///
    /// # Errors
    /// A message naming the bad key: `url_env` that is empty or has a
    /// character outside `[A-Za-z0-9_]`; `key_prefix` that is empty, over 64
    /// bytes, or holds whitespace, a control character or a brace (the prefix
    /// goes inside the hash tag `{<prefix>}`).
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "config check run once at start-up, like `Config::validate`"
        )
    )]
    pub fn validate(&self) -> Result<(), String> {
        if self.url_env.is_empty()
            || !self
                .url_env
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return Err(
                "redis.url_env must name an environment variable (letters, digits and _)"
                    .to_string(),
            );
        }
        let prefix = &self.key_prefix;
        if prefix.is_empty()
            || prefix.len() > MAX_REDIS_KEY_PREFIX_BYTES
            || prefix
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || c == '{' || c == '}')
        {
            return Err(format!(
                "redis.key_prefix must be 1 to {MAX_REDIS_KEY_PREFIX_BYTES} bytes with no whitespace, control characters or braces"
            ));
        }
        Ok(())
    }
}

/// Settings read from the configuration file.
// `#[serde(default)]` fills every missing key from `Default::default()`.
// serde ignores keys the struct does not name unless told otherwise
// (`deny_unknown_fields`), which is what lets later milestones' keys through.
// Not `Eq`: `[telemetry.otlp] sample_ratio` is an `f64`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct Config {
    /// The largest uncompressed total an import may expand to (`"64MiB"`, or
    /// a number of bytes). `MiB` is binary; `MB` and `M` are decimal. For a
    /// tarball it counts the whole decompressed tar stream (headers and
    /// 512-byte padding included); for a directory, the files' contents.
    /// The import route caps the request body at this size plus a little
    /// slack for gzip (`body_limit`).
    pub max_import_bytes: ByteSize,
    /// The address `serve` listens on (`"0.0.0.0:8080"`); `--listen` overrides it.
    pub listen: SocketAddr,
    /// The `Host` names (optionally `host:port`) MCP requests may carry, the
    /// DNS-rebinding check. Behind an ingress, the public hostname.
    pub allowed_hosts: Vec<String>,
    /// The catalog a key with no default of its own reads and writes (design
    /// "Catalog selection", step 4).
    pub default_catalog: Option<String>,
    /// Deprecated and ignored (milestone 18, spec Decision 18): the server no
    /// longer polls. Changes reach every replica through the change feed,
    /// with `catalog_ttl` and `credential_ttl` as the backstop. The key
    /// still parses (`"10s"`), so a configuration that has it keeps
    /// starting; [`Config::deprecation_warnings`] names it.
    // `with = "humantime_serde"` parses the field through that module's
    // functions instead of `Duration`'s own serde impl (which wants
    // `{secs, nanos}`); it reads `Option<Duration>` too, a missing key
    // being `None`.
    #[serde(with = "humantime_serde")]
    pub poll_interval: Option<Duration>,
    /// How long an MCP session may be idle before it is closed (`"2h"`).
    #[serde(with = "humantime_serde")]
    pub session_idle_timeout: Duration,
    /// How long `serve` waits, once told to stop, for the mail worker to
    /// finish the mail in hand (`"30s"`). Match the pod's
    /// `terminationGracePeriodSeconds` to it, plus a few seconds. Past it
    /// `serve` logs and returns, and apalis hands the unfinished job to
    /// another replica after the orphan timeout.
    #[serde(with = "humantime_serde")]
    pub worker_shutdown_grace: Duration,
    /// The NTP server the clock check measures against (`host`, `host:port`
    /// or an address); none means no check (design "Time").
    pub time_server: Option<String>,
    /// Review cadence overrides keyed by catalog name.
    #[serde(deserialize_with = "deserialize_maintenance_intervals")]
    pub maintenance_review_intervals: BTreeMap<String, Duration>,
    /// `get_concept` returns an outline instead of a body longer than this
    /// many characters (Unicode code points), unless asked for `full`.
    pub concept_outline_chars: usize,
    /// The most database connections one process opens (every command,
    /// `serve` included). A write holds one for its transaction, and a read
    /// that builds an overlay holds one to read the changeset's entries.
    /// `serve`'s workers do not count against it: they share one pool of up
    /// to `mail::queue::WORKER_POOL_SIZE` (3) connections of their own.
    pub database_max_connections: u32,
    /// The most connections one process opens to each catalog store (`8`),
    /// beside `database_max_connections`, which is the account pool. Every
    /// store has a pool of its own, opened on its first use, and a store
    /// inside the account database still opens one (plan decision P1).
    pub store_max_connections: u32,
    /// How long a catalog store's connection may sit idle before it is
    /// closed (`"10m"`). A store's pool keeps no connection open while
    /// unused.
    #[serde(with = "humantime_serde")]
    pub store_idle_timeout: Duration,
    /// How long opening a store's pool, or waiting for a
    /// connection to begin a transaction on it, may take (`"5s"`). Past it the request fails with
    /// `StoreUnavailable`.
    #[serde(with = "humantime_serde")]
    pub store_connect_timeout: Duration,
    /// The most memory the changeset overlays held in this process may take,
    /// estimated as a catalog snapshot's is, over the overlay's own files
    /// (`"256MiB"`, or a number of bytes). Past it, the least recently used
    /// is dropped (and rebuilt if read again); an overlay larger than the
    /// whole budget is served but not kept.
    pub overlay_cache_bytes: ByteSize,
    /// Deprecated and ignored: use `overlay_cache_bytes`. Still read so an
    /// old configuration starts (`deprecation_warnings` names it).
    pub overlay_cache_size: Option<usize>,
    /// The most memory the catalog snapshots held in this process may take,
    /// as estimated from their stored size (`"1GiB"`, or a number of bytes).
    /// Catalogs load on first use; past the budget, the least recently used
    /// is dropped and loaded again when next read. A catalog larger than the
    /// whole budget is still served, but not kept.
    pub catalog_cache_bytes: ByteSize,
    /// The most catalog rows the catalog directory keeps, by name (`100000`).
    /// A row is a few hundred bytes, so this bounds the directory at tens of
    /// megabytes however many catalogs exist.
    pub catalog_directory_entries: u64,
    /// How long the directory serves a catalog row before it reads it again
    /// (`"10s"`). Invalidations drop a row sooner; this is the backstop for
    /// one that is lost.
    #[serde(with = "humantime_serde")]
    pub catalog_ttl: Duration,
    /// The most keys and OAuth grants the credential cache keeps (`100000`
    /// each, by hash and by id). Only live credentials that were used are
    /// kept; an unknown one is never cached.
    pub credential_cache_entries: u64,
    /// The most users the user cache keeps, by id (`100000`): each one's
    /// admin flag, ceiling, teams, entitlements and live unlocks.
    pub user_cache_entries: u64,
    /// How long a cached credential or user is served before it is read
    /// again (`"10s"`). Invalidations drop one sooner; this is the backstop
    /// for one that is lost, and the bound on a revocation's reach then.
    #[serde(with = "humantime_serde")]
    pub credential_ttl: Duration,
    /// The most catalogs `GET /api/v1/search` searches when the request
    /// names none (`25`, from 1 to 999): a key that reaches more must name
    /// them, so one request cannot search every catalog in the system.
    pub search_fanout_max: u32,
    /// The externally visible origin (`"https://okf.example.com"`). Setting
    /// it turns OAuth on: it is the issuer, and `<public_url>/mcp` is the
    /// resource tokens are issued for. Its host must be in `allowed_hosts`.
    pub public_url: Option<String>,
    /// The OAuth settings (`[oauth]`).
    pub oauth: OAuthConfig,
    /// The console sign-in policy defaults (`[auth]`).
    pub auth: AuthConfig,
    /// Outgoing mail (`[mail]`).
    pub mail: MailConfig,
    /// Logs, traces and metrics (`[telemetry]`).
    pub telemetry: TelemetryConfig,
    /// Shared state for several `serve` replicas (`[redis]`); `None` keeps
    /// everything in memory.
    pub redis: Option<RedisConfig>,
    /// The `send_feedback` tool's destination (`[feedback]`); `None` turns
    /// the tool off.
    pub feedback: Option<FeedbackConfig>,
}

impl Default for Config {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "runs before the subscriber exists (configuration, builder or start-up), so a span would be recorded nowhere"
        )
    )]
    fn default() -> Self {
        Self {
            max_import_bytes: ByteSize::b(DEFAULT_MAX_IMPORT_BYTES),
            listen: DEFAULT_LISTEN,
            // `Vec::from` takes ownership of the array (after `.map` turns each
            // `&str` into an owned `String`) without copying it a second time.
            allowed_hosts: Vec::from(DEFAULT_ALLOWED_HOSTS.map(String::from)),
            default_catalog: None,
            poll_interval: None,
            session_idle_timeout: DEFAULT_SESSION_IDLE_TIMEOUT,
            worker_shutdown_grace: DEFAULT_WORKER_SHUTDOWN_GRACE,
            time_server: None,
            maintenance_review_intervals: BTreeMap::new(),
            concept_outline_chars: DEFAULT_CONCEPT_OUTLINE_CHARS,
            database_max_connections: DEFAULT_DATABASE_MAX_CONNECTIONS,
            store_max_connections: DEFAULT_STORE_MAX_CONNECTIONS,
            store_idle_timeout: DEFAULT_STORE_IDLE_TIMEOUT,
            store_connect_timeout: DEFAULT_STORE_CONNECT_TIMEOUT,
            overlay_cache_bytes: ByteSize::b(DEFAULT_OVERLAY_CACHE_BYTES),
            overlay_cache_size: None,
            catalog_cache_bytes: ByteSize::b(DEFAULT_CATALOG_CACHE_BYTES),
            catalog_directory_entries: DEFAULT_CATALOG_DIRECTORY_ENTRIES,
            catalog_ttl: DEFAULT_CATALOG_TTL,
            credential_cache_entries: DEFAULT_CREDENTIAL_CACHE_ENTRIES,
            user_cache_entries: DEFAULT_USER_CACHE_ENTRIES,
            credential_ttl: DEFAULT_CREDENTIAL_TTL,
            search_fanout_max: DEFAULT_SEARCH_FANOUT_MAX,
            public_url: None,
            oauth: OAuthConfig::default(),
            auth: AuthConfig::default(),
            mail: MailConfig::default(),
            telemetry: TelemetryConfig::default(),
            redis: None,
            feedback: None,
        }
    }
}

#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "runs before the subscriber exists (configuration, builder or start-up), so a span would be recorded nowhere"
    )
)]
fn deserialize_maintenance_intervals<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<String, Duration>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    struct HumanDuration(#[serde(with = "humantime_serde")] Duration);

    let values = BTreeMap::<String, HumanDuration>::deserialize(deserializer)?;
    Ok(values
        .into_iter()
        .map(|(name, value)| (name, value.0))
        .collect())
}

impl Config {
    /// Review cadence for a catalog, defaulting to 30 days.
    #[must_use]
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn maintenance_review_interval(&self, catalog: &str) -> chrono::Duration {
        let interval = self
            .maintenance_review_intervals
            .get(catalog)
            .copied()
            .unwrap_or(Duration::from_secs(2_592_000));
        chrono::Duration::from_std(interval).expect("validated maintenance interval fits chrono")
    }

    /// Parses a configuration file's text. It does not call
    /// [`Config::validate`]; [`Config::load`] does, so a caller that parses
    /// text itself must validate the result before serving with it.
    ///
    /// # Arguments
    /// * `text` - the file's contents.
    ///
    /// # Returns
    /// The settings, with defaults for every key the text leaves out.
    ///
    /// # Errors
    /// The TOML parser's error if the text is not valid TOML or a value has
    /// the wrong type.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "runs before the subscriber exists (configuration, builder or start-up), so a span would be recorded nowhere"
        )
    )]
    pub fn from_toml(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    /// Settings that are read but no longer do anything. `Config::load` runs
    /// before the log subscriber exists, so it cannot log them; `serve` calls
    /// this once the subscriber is up and logs each message as a warning.
    ///
    /// # Returns
    /// One message per deprecated setting present in the file (empty when
    /// there is none).
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "pure check over the parsed settings, called once at start-up"
        )
    )]
    pub fn deprecation_warnings(&self) -> Vec<&'static str> {
        let mut warnings = Vec::new();
        if self.mail.worker.is_some() {
            warnings.push("mail.worker is deprecated and ignored: every serve runs the worker");
        }
        if self.poll_interval.is_some() {
            warnings.push("poll_interval is deprecated and ignored");
        }
        if self.overlay_cache_size.is_some() {
            warnings.push("overlay_cache_size is deprecated and ignored; set overlay_cache_bytes");
        }
        warnings
    }

    /// Checks the values serde cannot: ones that would break the server.
    ///
    /// # Returns
    /// `Ok(())`, or the problem in words.
    ///
    /// # Errors
    /// A zero `session_idle_timeout`, an empty `allowed_hosts` (which rmcp reads as
    /// "accept any `Host`"), or a blank entry in it (which no `Host` matches,
    /// so every request would get 403 with no hint why), or a blank
    /// `time_server` or one with surrounding whitespace (which the lookup
    /// would fail on every check, seen only as a `warn`), or a zero
    /// `database_max_connections` or `overlay_cache_bytes`, a zero OAuth
    /// lifetime, an `[auth]` value out of range (see `AuthConfig`), a
    /// `[mail]` section that cannot send (see `MailConfig`), or a
    /// `public_url` that is not an `https` origin (`http` on
    /// a loopback host only) whose host `allowed_hosts` admits, or an
    /// `[[auth.oidc]]` provider that cannot work (see `validate_oidc`), or a
    /// `[feedback]` table that cannot work (see `FeedbackConfig::validate`).
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "runs before the subscriber exists (configuration, builder or start-up), so a span would be recorded nowhere"
        )
    )]
    pub fn validate(&self) -> Result<(), String> {
        if self.session_idle_timeout.is_zero() {
            return Err("session_idle_timeout must be more than zero".to_string());
        }
        if self.worker_shutdown_grace.is_zero() {
            return Err("worker_shutdown_grace must be more than zero".to_string());
        }
        if self.database_max_connections == 0 {
            return Err("database_max_connections must be more than zero".to_string());
        }
        if self.store_max_connections == 0 {
            return Err("store_max_connections must be more than zero".to_string());
        }
        if self.store_idle_timeout.is_zero() {
            return Err("store_idle_timeout must be more than zero".to_string());
        }
        if self.store_connect_timeout.is_zero() {
            return Err("store_connect_timeout must be more than zero".to_string());
        }
        if self.overlay_cache_bytes.as_u64() == 0 {
            return Err("overlay_cache_bytes must be more than zero".to_string());
        }
        if self.catalog_cache_bytes.as_u64() == 0 {
            return Err("catalog_cache_bytes must be more than zero".to_string());
        }
        if self.catalog_directory_entries == 0 {
            return Err("catalog_directory_entries must be more than zero".to_string());
        }
        if self.catalog_ttl.is_zero() {
            return Err("catalog_ttl must be more than zero".to_string());
        }
        if self.credential_cache_entries == 0 {
            return Err("credential_cache_entries must be more than zero".to_string());
        }
        if self.user_cache_entries == 0 {
            return Err("user_cache_entries must be more than zero".to_string());
        }
        if self.credential_ttl.is_zero() {
            return Err("credential_ttl must be more than zero".to_string());
        }
        // The cap is checked with one listing page of `search_fanout_max + 1`
        // rows, and a page holds at most `MAX_PAGE` (1000).
        if self.search_fanout_max == 0
            || self.search_fanout_max >= crate::cache::directory::MAX_PAGE
        {
            return Err("search_fanout_max must be from 1 to 999".to_string());
        }
        if self.allowed_hosts.is_empty() {
            return Err(
                "allowed_hosts is empty; list the host names clients connect to".to_string(),
            );
        }
        // `trim` returns a borrowed slice (`&str`) without the surrounding
        // whitespace, so the check allocates nothing.
        if self.allowed_hosts.iter().any(|host| host.trim().is_empty()) {
            return Err("allowed_hosts has a blank entry; remove it or name a host".to_string());
        }
        // `if let Some(x) = ...`: Python's `if (x := ...) is not None`.
        // `as_deref` turns `&Option<String>` into `Option<&str>`.
        if let Some(server) = self.time_server.as_deref() {
            let trimmed = server.trim();
            if trimmed.is_empty() {
                return Err("time_server is blank; name the NTP server or leave it out".to_string());
            }
            if trimmed != server {
                return Err("time_server has surrounding whitespace; remove it".to_string());
            }
        }
        for (name, interval) in &self.maintenance_review_intervals {
            if !catalogs::is_valid_name(name) {
                return Err(format!(
                    "maintenance_review_intervals: invalid catalog name {name:?}"
                ));
            }
            if *interval < Duration::from_secs(86_400) {
                return Err(format!(
                    "maintenance_review_intervals.{name} must be at least 1 day"
                ));
            }
            if chrono::Duration::from_std(*interval).is_err() {
                return Err(format!(
                    "maintenance_review_intervals.{name} exceeds supported duration"
                ));
            }
        }
        for (name, value) in [
            ("oauth.access_token_ttl", self.oauth.access_token_ttl),
            ("oauth.refresh_token_ttl", self.oauth.refresh_token_ttl),
            (
                "oauth.idle_grant_revoke_after",
                self.oauth.idle_grant_revoke_after,
            ),
            ("oauth.code_max_expiry", self.oauth.code_max_expiry),
        ] {
            check_auth_duration(name, value)?;
        }
        if self.oauth.refresh_reuse_grace > MAX_REFRESH_REUSE_GRACE {
            return Err("oauth.refresh_reuse_grace must be at most 10 minutes".to_string());
        }
        self.auth.validate()?;
        self.mail.validate()?;
        self.telemetry.validate()?;
        if let Some(redis) = &self.redis {
            redis.validate()?;
        }
        if let Some(feedback) = &self.feedback {
            feedback.validate()?;
        }
        self.validate_metrics_listen()?;
        self.validate_oidc()?;
        self.validate_public_url()
    }

    /// Checks that `[telemetry.prometheus] listen` does not claim the socket
    /// `listen` does (`serve --listen` is checked again where it applies).
    ///
    /// # Errors
    /// The two addresses overlap.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "config check run once at start-up, like `validate`"
        )
    )]
    pub fn validate_metrics_listen(&self) -> Result<(), String> {
        match &self.telemetry.prometheus {
            Some(prometheus) if prometheus.overlaps(self.listen) => {
                Err("telemetry.prometheus.listen overlaps listen".to_string())
            }
            _ => Ok(()),
        }
    }

    /// Checks `[[auth.oidc]]`: providers need `public_url` (the redirect
    /// URI's origin); each has a valid, unique id, a label, a client id and
    /// a secret variable, and an `https` issuer (`http` on loopback only)
    /// with no query or fragment.
    ///
    /// # Returns
    /// `Ok(())` without providers, or when every one is acceptable.
    ///
    /// # Errors
    /// The problem in words, naming the provider and the key.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "runs before the subscriber exists (configuration, builder or start-up), so a span would be recorded nowhere"
        )
    )]
    fn validate_oidc(&self) -> Result<(), String> {
        if self.auth.oidc.is_empty() {
            return Ok(());
        }
        if self.public_url.is_none() {
            return Err("auth.oidc needs public_url, the origin of its redirect URIs".to_string());
        }
        let mut seen = std::collections::HashSet::new();
        for provider in &self.auth.oidc {
            let id = provider.id.as_str();
            if !is_provider_id(id) {
                return Err(format!(
                    "auth.oidc id {id:?} must be 1 to 32 of a-z, 0-9 and -"
                ));
            }
            // `insert` returns `false` for a value already in the set.
            if !seen.insert(id) {
                return Err(format!("auth.oidc id {id:?} is listed twice"));
            }
            for (key, value) in [
                ("label", &provider.label),
                ("client_id", &provider.client_id),
                ("client_secret_env", &provider.client_secret_env),
            ] {
                if value.trim().is_empty() {
                    return Err(format!("auth.oidc.{id}: {key} is empty"));
                }
            }
            let issuer = url::Url::parse(&provider.issuer).map_err(|_| {
                format!("auth.oidc.{id}: issuer {:?} is not a URL", provider.issuer)
            })?;
            let loopback = issuer
                .host_str()
                .is_some_and(|host| is_loopback(&normalize_host(host)));
            let secure = issuer.scheme() == "https" || (issuer.scheme() == "http" && loopback);
            if !secure || issuer.query().is_some() || issuer.fragment().is_some() {
                return Err(format!(
                    "auth.oidc.{id}: issuer {:?} must be an https URL (http only on a \
                     loopback host), with no query or fragment",
                    provider.issuer
                ));
            }
        }
        Ok(())
    }

    /// Checks `public_url`: an `https` origin (`http` only on a loopback
    /// host, for local testing), with no user info, path or query, whose
    /// host an `allowed_hosts` entry admits under the MCP `Host` check's
    /// rule (same host; the entry's port, if it has one, equal to the URL's).
    ///
    /// # Returns
    /// `Ok(())` when `public_url` is unset or acceptable.
    ///
    /// # Errors
    /// The problem in words.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "runs before the subscriber exists (configuration, builder or start-up), so a span would be recorded nowhere"
        )
    )]
    fn validate_public_url(&self) -> Result<(), String> {
        let Some(public_url) = self.public_url.as_deref() else {
            return Ok(());
        };
        let uri: Uri = public_url
            .parse()
            .map_err(|_| format!("public_url {public_url:?} is not a URL"))?;
        // A tuple pattern matches both `Option`s at once.
        let (Some(scheme), Some(authority)) = (uri.scheme_str(), uri.authority()) else {
            return Err(format!(
                "public_url {public_url:?} needs a scheme and a host"
            ));
        };
        // `user@host` would put the user info into the issuer and every URL
        // built from it.
        if authority.as_str().contains('@') {
            return Err(format!(
                "public_url {public_url:?} must be an origin, with no user info"
            ));
        }
        if !matches!(uri.path(), "" | "/") || uri.query().is_some() {
            return Err(format!(
                "public_url {public_url:?} must be an origin, with no path or query"
            ));
        }
        let host = normalize_host(authority.host());
        let secure = scheme.eq_ignore_ascii_case("https")
            || (scheme.eq_ignore_ascii_case("http") && is_loopback(&host));
        if !secure {
            return Err(format!(
                "public_url {public_url:?} must use https (http only on a loopback host)"
            ));
        }
        let port = authority.port_u16();
        let admitted = self.allowed_hosts.iter().any(|entry| {
            // An entry that is not an authority (rmcp's fallback) is a bare host.
            let (entry_host, entry_port) = match entry.trim().parse::<Authority>() {
                Ok(parsed) => (normalize_host(parsed.host()), parsed.port_u16()),
                Err(_) => (normalize_host(entry.trim()), None),
            };
            // `is_none_or`: true for `None`, else the closure's answer.
            entry_host == host && entry_port.is_none_or(|entry_port| Some(entry_port) == port)
        });
        if !admitted {
            return Err(format!(
                "public_url's host {host:?} is not in allowed_hosts, so MCP requests to it \
                 would be refused; add it"
            ));
        }
        Ok(())
    }

    /// Reads the configuration file, or returns the defaults without one.
    ///
    /// # Arguments
    /// * `path` - the file (`--config` or `OKF_MCP_CONFIG`), if any.
    ///
    /// # Returns
    /// The file's settings, or [`Config::default`] when `path` is `None`.
    ///
    /// # Errors
    /// [`OkfError::Config`] if the file can't be read or parsed, or fails
    /// [`Config::validate`].
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "runs before the subscriber exists (configuration, builder or start-up), so a span would be recorded nowhere"
        )
    )]
    pub fn load(path: Option<&Path>) -> Result<Self, OkfError> {
        // `let ... else` is a pattern match that must diverge (here `return`)
        // when the pattern does not match: an early return for `None`.
        let Some(path) = path else {
            return Ok(Self::default());
        };
        // A closure (Python's lambda) that wraps a message with the path.
        let error = |message: String| OkfError::Config {
            path: path.display().to_string(),
            message,
        };
        let text = std::fs::read_to_string(path).map_err(|e| error(e.to_string()))?;
        let config = Self::from_toml(&text).map_err(|e| error(e.to_string()))?;
        // `map_err(error)` passes the closure itself: `validate`'s `String`
        // is already the message it wants.
        config.validate().map_err(error)?;
        Ok(config)
    }
}

/// A host as the MCP `Host` check compares it: lowercase, IPv6 brackets trimmed.
///
/// # Arguments
/// * `host` - a host as parsed from a URL or an `allowed_hosts` entry.
///
/// # Returns
/// The normalized host.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "runs before the subscriber exists (configuration, builder or start-up), so a span would be recorded nowhere"
    )
)]
fn normalize_host(host: &str) -> String {
    host.trim_matches(|c| c == '[' || c == ']')
        .to_ascii_lowercase()
}

/// Is `host` (normalized) this machine?
///
/// # Arguments
/// * `host` - a host from [`normalize_host`].
///
/// # Returns
/// `true` for `localhost` and loopback addresses of either family.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "runs before the subscriber exists (configuration, builder or start-up), so a span would be recorded nowhere"
    )
)]
fn is_loopback(host: &str) -> bool {
    // `parse::<IpAddr>()` reads either address family; `is_ok_and` is
    // Python's `try: ... except: False` around a test.
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

#[cfg(test)]
mod tests {

    #[test]
    fn redis_is_off_by_default_and_parses_with_defaults() {
        assert_eq!(Config::default().redis, None);
        let config = Config::from_toml("[redis]\nurl_env = \"OKF_REDIS_URL\"\n").unwrap();
        let redis = config.redis.clone().expect("a [redis] table");
        assert_eq!(redis.url_env, "OKF_REDIS_URL");
        assert_eq!(redis.key_prefix, "okf:");
        assert_eq!(redis.mode, RedisMode::Standalone);
        config.validate().unwrap();
    }

    #[test]
    fn redis_mode_parses_cluster_and_refuses_anything_else() {
        let config = Config::from_toml("[redis]\nurl_env = \"X\"\nmode = \"cluster\"\n").unwrap();
        assert_eq!(config.redis.expect("a table").mode, RedisMode::Cluster);
        let config =
            Config::from_toml("[redis]\nurl_env = \"X\"\nmode = \"standalone\"\n").unwrap();
        assert_eq!(config.redis.expect("a table").mode, RedisMode::Standalone);
        assert!(Config::from_toml("[redis]\nurl_env = \"X\"\nmode = \"sentinel\"\n").is_err());
    }

    #[test]
    fn redis_rejects_unknown_keys_and_bad_values() {
        assert!(Config::from_toml("[redis]\nurl_env = \"X\"\nurl = \"redis://h\"\n").is_err());
        let long_prefix = format!(
            "[redis]\nurl_env = \"X\"\nkey_prefix = \"{}\"\n",
            "a".repeat(65)
        );
        for (toml, needle) in [
            ("[redis]\nurl_env = \"\"\n".to_string(), "redis.url_env"),
            (
                "[redis]\nurl_env = \"HAS SPACE\"\n".to_string(),
                "redis.url_env",
            ),
            (
                "[redis]\nurl_env = \"X\"\nkey_prefix = \"\"\n".to_string(),
                "redis.key_prefix",
            ),
            (
                "[redis]\nurl_env = \"X\"\nkey_prefix = \"a b\"\n".to_string(),
                "redis.key_prefix",
            ),
            (
                "[redis]\nurl_env = \"X\"\nkey_prefix = \"a{b\"\n".to_string(),
                "redis.key_prefix",
            ),
            (
                "[redis]\nurl_env = \"X\"\nkey_prefix = \"a}\"\n".to_string(),
                "redis.key_prefix",
            ),
            (long_prefix, "redis.key_prefix"),
        ] {
            let message = Config::from_toml(&toml).unwrap().validate().unwrap_err();
            assert!(message.contains(needle), "{toml}: {message}");
        }
    }
    use super::*;

    #[test]
    fn a_mail_providers_name_is_the_spelling_the_config_takes() {
        for provider in [
            MailProvider::Smtp,
            MailProvider::Postmark,
            MailProvider::Ses,
            MailProvider::Log,
        ] {
            // serde reads a bare enum variant from a JSON string.
            let parsed: MailProvider =
                serde_json::from_value(serde_json::Value::from(provider.as_str())).unwrap();

            assert_eq!(parsed, provider);
        }
    }

    #[test]
    fn an_empty_file_gives_the_defaults() {
        let config = Config::from_toml("").unwrap();

        assert_eq!(config, Config::default());
        assert_eq!(config.max_import_bytes.as_u64(), 64 * 1024 * 1024);
        assert_eq!(config.listen.to_string(), "0.0.0.0:8080");
        assert_eq!(config.allowed_hosts, ["localhost", "127.0.0.1", "::1"]);
        assert_eq!(config.default_catalog, None);
        assert_eq!(config.poll_interval, None);
        assert_eq!(
            config.session_idle_timeout,
            Duration::from_secs(2 * 60 * 60)
        );
        assert_eq!(config.concept_outline_chars, 20_000);
        assert_eq!(config.database_max_connections, 16);
        assert_eq!(config.store_max_connections, 8);
        assert_eq!(config.store_idle_timeout, Duration::from_secs(600));
        assert_eq!(config.store_connect_timeout, Duration::from_secs(5));
        assert_eq!(config.overlay_cache_bytes.as_u64(), 256 * 1024 * 1024);
        assert_eq!(config.overlay_cache_size, None);
        assert_eq!(config.catalog_cache_bytes.as_u64(), 1024 * 1024 * 1024);
        assert_eq!(config.catalog_directory_entries, 100_000);
        assert_eq!(config.catalog_ttl, Duration::from_secs(10));
        assert_eq!(config.credential_cache_entries, 100_000);
        assert_eq!(config.user_cache_entries, 100_000);
        assert_eq!(config.credential_ttl, Duration::from_secs(10));
        assert_eq!(config.search_fanout_max, 25);
    }

    #[test]
    fn every_field_reads_from_the_file_and_unknown_keys_are_ignored() {
        let text = "max_import_bytes = \"1MiB\"\n\
                    listen = \"127.0.0.1:9000\"\n\
                    allowed_hosts = [\"okf-mcp.example\"]\n\
                    default_catalog = \"pg\"\n\
                    poll_interval = \"90s\"\n\
                    session_idle_timeout = \"30m\"\n\
                    time_server = \"ntp.example\"\n\
                    concept_outline_chars = 500\n\
                    database_max_connections = 32\n\
                    overlay_cache_bytes = \"8MiB\"\n\
                    a_later_key = true\n";

        let config = Config::from_toml(text).unwrap();

        assert_eq!(config.max_import_bytes.as_u64(), 1024 * 1024);
        assert_eq!(config.listen.to_string(), "127.0.0.1:9000");
        assert_eq!(config.allowed_hosts, ["okf-mcp.example"]);
        assert_eq!(config.default_catalog.as_deref(), Some("pg"));
        assert_eq!(config.poll_interval, Some(Duration::from_secs(90)));
        assert_eq!(config.session_idle_timeout, Duration::from_secs(30 * 60));
        assert_eq!(config.time_server.as_deref(), Some("ntp.example"));
        assert_eq!(config.concept_outline_chars, 500);
        assert_eq!(config.database_max_connections, 32);
        assert_eq!(config.overlay_cache_bytes.as_u64(), 8 * 1024 * 1024);
    }

    #[test]
    fn time_server_is_optional_and_never_blank() {
        let none = Config::from_toml("").unwrap();
        let named = Config::from_toml("time_server = \"time.example\"\n").unwrap();
        let blank = Config::from_toml("time_server = \"  \"\n").unwrap();
        let padded = Config::from_toml("time_server = \" time.example\\t\"\n").unwrap();

        assert_eq!(none.time_server, None);
        assert_eq!(named.time_server.as_deref(), Some("time.example"));
        assert_eq!(named.validate(), Ok(()));
        assert_eq!(
            blank.validate(),
            Err("time_server is blank; name the NTP server or leave it out".to_string())
        );
        assert_eq!(
            padded.validate(),
            Err("time_server has surrounding whitespace; remove it".to_string())
        );
    }

    #[test]
    fn sizes_take_plain_numbers() {
        let config = Config::from_toml("max_import_bytes = 2048\n").unwrap();
        assert_eq!(config.max_import_bytes.as_u64(), 2048);
    }

    #[test]
    fn a_bad_value_names_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("okf.toml");
        std::fs::write(&path, "session_idle_timeout = \"soon\"\n").unwrap();

        let error = Config::load(Some(&path)).unwrap_err();

        assert!(matches!(&error, OkfError::Config { path: p, .. } if p.ends_with("okf.toml")));
    }

    #[test]
    fn values_that_would_break_the_server_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("okf.toml");
        for (text, needle) in [
            ("session_idle_timeout = \"0s\"\n", "session_idle_timeout"),
            ("allowed_hosts = []\n", "allowed_hosts"),
            ("database_max_connections = 0\n", "database_max_connections"),
            ("store_max_connections = 0\n", "store_max_connections"),
            ("store_idle_timeout = \"0s\"\n", "store_idle_timeout"),
            ("store_connect_timeout = \"0s\"\n", "store_connect_timeout"),
            ("overlay_cache_bytes = 0\n", "overlay_cache_bytes"),
            ("catalog_cache_bytes = 0\n", "catalog_cache_bytes"),
            (
                "catalog_directory_entries = 0\n",
                "catalog_directory_entries",
            ),
            ("catalog_ttl = \"0s\"\n", "catalog_ttl"),
            ("credential_cache_entries = 0\n", "credential_cache_entries"),
            ("user_cache_entries = 0\n", "user_cache_entries"),
            ("credential_ttl = \"0s\"\n", "credential_ttl"),
            ("search_fanout_max = 0\n", "search_fanout_max"),
            (
                "search_fanout_max = 1000\n",
                "search_fanout_max must be from 1 to 999",
            ),
            (
                "allowed_hosts = [\"\"]\n",
                "allowed_hosts has a blank entry",
            ),
            (
                "allowed_hosts = [\"localhost\", \"  \"]\n",
                "allowed_hosts has a blank entry",
            ),
        ] {
            std::fs::write(&path, text).unwrap();

            let error = Config::load(Some(&path)).unwrap_err();

            assert!(
                matches!(&error, OkfError::Config { path: p, .. } if p.ends_with("okf.toml")),
                "{text}: {error:?}"
            );
            assert!(error.to_string().contains(needle), "{text}: {error}");
        }
    }

    #[test]
    fn maintenance_review_interval_is_per_catalog() {
        let default = Config::default();
        assert_eq!(
            default.maintenance_review_interval("pg").num_seconds(),
            2_592_000
        );
        let config =
            Config::from_toml("[maintenance_review_intervals]\npg = \"14d\"\nother = \"1d\"\n")
                .unwrap();
        assert_eq!(
            config.maintenance_review_interval("pg").num_seconds(),
            1_209_600
        );
        assert_eq!(
            config.maintenance_review_interval("other").num_seconds(),
            86_400
        );
        assert_eq!(
            config.maintenance_review_interval("missing").num_seconds(),
            2_592_000
        );
        assert_eq!(config.validate(), Ok(()));
        assert!(
            Config::from_toml("[maintenance_review_intervals]\npg = \"not-duration\"\n").is_err()
        );
        let too_short =
            Config::from_toml("[maintenance_review_intervals]\npg = \"86399s\"\n").unwrap();
        assert!(too_short.validate().is_err());
        let invalid_name =
            Config::from_toml("[maintenance_review_intervals]\n\"Bad_Name\" = \"1d\"\n").unwrap();
        assert!(invalid_name.validate().is_err());
    }

    #[test]
    fn no_file_gives_the_defaults() {
        assert_eq!(Config::load(None).unwrap(), Config::default());
    }

    /// The defaults with OAuth on at `public_url`, its host allowed.
    fn oauth_on(public_url: &str, allowed: &[&str]) -> Config {
        Config {
            public_url: Some(public_url.to_string()),
            allowed_hosts: allowed.iter().map(|host| host.to_string()).collect(),
            ..Config::default()
        }
    }

    #[test]
    fn oauth_is_off_by_default_with_sensible_lifetimes() {
        let config = Config::default();

        assert_eq!(config.public_url, None);
        assert_eq!(
            config.oauth.access_token_ttl,
            Duration::from_secs(24 * 60 * 60)
        );
        assert_eq!(
            config.oauth.refresh_token_ttl,
            Duration::from_secs(30 * 24 * 60 * 60)
        );
        assert_eq!(config.oauth.refresh_reuse_grace, Duration::from_secs(60));
        assert_eq!(
            config.oauth.idle_grant_revoke_after,
            Duration::from_secs(30 * 24 * 60 * 60)
        );
        assert_eq!(
            config.oauth.code_max_expiry,
            Duration::from_secs(7 * 24 * 60 * 60)
        );
        assert!(config.oauth.dcr_enabled);
        assert!(!config.oauth.cimd_allow_private);
        assert_eq!(config.validate(), Ok(()));
    }

    #[test]
    fn the_oauth_section_reads_from_the_file() {
        let text = "public_url = \"https://okf.example.com\"\n\
                    allowed_hosts = [\"okf.example.com\"]\n\
                    [oauth]\n\
                    access_token_ttl = \"15m\"\n\
                    refresh_token_ttl = \"7d\"\n\
                    refresh_reuse_grace = \"5s\"\n\
                    idle_grant_revoke_after = \"60d\"\n\
                    code_max_expiry = \"2d\"\n\
                    dcr_enabled = false\n\
                    cimd_allow_private = true\n";

        let config = Config::from_toml(text).unwrap();

        assert_eq!(
            config.public_url.as_deref(),
            Some("https://okf.example.com")
        );
        assert_eq!(config.oauth.access_token_ttl, Duration::from_secs(15 * 60));
        assert_eq!(
            config.oauth.refresh_token_ttl,
            Duration::from_secs(7 * 24 * 60 * 60)
        );
        assert_eq!(config.oauth.refresh_reuse_grace, Duration::from_secs(5));
        assert_eq!(
            config.oauth.idle_grant_revoke_after,
            Duration::from_secs(60 * 24 * 60 * 60)
        );
        assert_eq!(
            config.oauth.code_max_expiry,
            Duration::from_secs(2 * 24 * 60 * 60)
        );
        assert!(!config.oauth.dcr_enabled);
        assert!(config.oauth.cimd_allow_private);
        assert_eq!(config.validate(), Ok(()));
    }

    #[test]
    fn public_url_must_be_an_allowed_https_origin() {
        // Accepted: https on an allowed host, with or without a trailing
        // slash or an explicit port that an entry allows; http on loopback.
        for config in [
            oauth_on("https://okf.example.com", &["okf.example.com"]),
            oauth_on("https://okf.example.com/", &["okf.example.com"]),
            oauth_on("https://OKF.example.com", &["okf.example.com"]),
            oauth_on("https://okf.example.com:8443", &["okf.example.com:8443"]),
            oauth_on("https://okf.example.com:8443", &["okf.example.com"]),
            oauth_on("http://127.0.0.1:8080", &["127.0.0.1"]),
            oauth_on("http://localhost:8080", &["localhost"]),
            oauth_on("http://[::1]:8080", &["::1"]),
        ] {
            assert_eq!(config.validate(), Ok(()), "{:?}", config.public_url);
        }
        // Refused, each with a reason that names the problem.
        for (config, needle) in [
            (
                oauth_on("http://okf.example.com", &["okf.example.com"]),
                "https",
            ),
            (
                oauth_on("ftp://okf.example.com", &["okf.example.com"]),
                "https",
            ),
            (
                oauth_on("https://okf.example.com/okf", &["okf.example.com"]),
                "origin",
            ),
            (
                oauth_on("https://okf.example.com/?a=b", &["okf.example.com"]),
                "origin",
            ),
            (
                oauth_on("https://user@okf.example.com", &["okf.example.com"]),
                "user info",
            ),
            (oauth_on("okf.example.com", &["okf.example.com"]), "scheme"),
            (
                oauth_on("https://other.example.com", &["okf.example.com"]),
                "allowed_hosts",
            ),
            (
                oauth_on("https://okf.example.com", &["okf.example.com:8443"]),
                "allowed_hosts",
            ),
            (oauth_on("not a url", &["okf.example.com"]), "URL"),
        ] {
            let error = config.validate().expect_err("refused");
            assert!(error.contains(needle), "{:?}: {error}", config.public_url);
        }
    }

    #[test]
    fn oauth_lifetimes_must_be_positive() {
        // Each closure is a plain `fn` pointer (it captures nothing), so the
        // four share one type and fit in one array.
        let changes: [fn(&mut OAuthConfig); 4] = [
            |oauth| oauth.access_token_ttl = Duration::ZERO,
            |oauth| oauth.refresh_token_ttl = Duration::ZERO,
            |oauth| oauth.idle_grant_revoke_after = Duration::ZERO,
            |oauth| oauth.code_max_expiry = Duration::ZERO,
        ];
        for change in changes {
            let mut config = Config::default();
            change(&mut config.oauth);

            let error = config.validate().expect_err("refused");

            assert!(error.contains("more than zero"), "{error}");
        }
    }

    /// Issue #884: zero turns the reuse grace window off; past ten minutes
    /// a copied refresh token would go unnoticed too long.
    #[test]
    fn the_refresh_reuse_grace_may_be_zero_and_at_most_ten_minutes() {
        let with = |grace: Duration| {
            let mut config = Config::default();
            config.oauth.refresh_reuse_grace = grace;
            config.validate()
        };

        assert_eq!(with(Duration::ZERO), Ok(()));
        assert_eq!(with(MAX_REFRESH_REUSE_GRACE), Ok(()));
        let error = with(MAX_REFRESH_REUSE_GRACE + Duration::from_secs(1)).expect_err("refused");
        assert!(error.contains("oauth.refresh_reuse_grace"), "{error}");
    }

    #[test]
    fn auth_defaults_match_the_spec() {
        let auth = Config::from_toml("").unwrap().auth;

        assert_eq!(auth, AuthConfig::default());
        assert!(auth.password_sign_in);
        assert_eq!(auth.max_keys_per_user, 20);
        assert_eq!(auth.max_key_lifetime, None);
        assert_eq!(auth.console_session_idle, Duration::from_secs(12 * 60 * 60));
        assert_eq!(
            auth.console_session_absolute,
            Duration::from_secs(7 * 24 * 60 * 60)
        );
        assert_eq!(auth.setup_link_ttl, Duration::from_secs(24 * 60 * 60));
        assert_eq!(auth.reset_link_ttl, Duration::from_secs(60 * 60));
        assert_eq!(auth.trusted_proxy_header, None);
    }

    #[test]
    fn every_auth_key_reads_from_the_file() {
        let text = "[auth]\n\
                    password_sign_in = false\n\
                    max_keys_per_user = 5\n\
                    max_key_lifetime = \"90d\"\n\
                    console_session_idle = \"30m\"\n\
                    console_session_absolute = \"1d\"\n\
                    setup_link_ttl = \"2d\"\n\
                    reset_link_ttl = \"15m\"\n\
                    trusted_proxy_header = \"x-forwarded-for\"\n";

        let config = Config::from_toml(text).unwrap();
        let auth = &config.auth;

        assert_eq!(config.validate(), Ok(()));
        assert!(!auth.password_sign_in);
        assert_eq!(auth.max_keys_per_user, 5);
        assert_eq!(
            auth.max_key_lifetime,
            Some(Duration::from_secs(90 * 24 * 60 * 60))
        );
        assert_eq!(auth.console_session_idle, Duration::from_secs(30 * 60));
        assert_eq!(
            auth.console_session_absolute,
            Duration::from_secs(24 * 60 * 60)
        );
        assert_eq!(auth.setup_link_ttl, Duration::from_secs(2 * 24 * 60 * 60));
        assert_eq!(auth.reset_link_ttl, Duration::from_secs(15 * 60));
        assert_eq!(
            auth.trusted_proxy_header.as_deref(),
            Some("x-forwarded-for")
        );
    }

    #[test]
    fn auth_values_that_would_break_sign_in_are_refused() {
        let refused = [
            "[auth]\nconsole_session_idle = \"0s\"\n",
            "[auth]\nconsole_session_absolute = \"0s\"\n",
            "[auth]\nsetup_link_ttl = \"0s\"\n",
            "[auth]\nreset_link_ttl = \"0s\"\n",
            "[auth]\nmax_key_lifetime = \"0s\"\n",
            "[auth]\nreset_link_ttl = \"300000000years\"\n",
            "[auth]\nconsole_session_absolute = \"1000000years\"\n",
            "[auth]\nsetup_link_ttl = \"10years 1s\"\n",
            "[oauth]\nrefresh_token_ttl = \"1000000years\"\n",
            "[auth]\nmax_keys_per_user = 1001\n",
            "[auth]\ntrusted_proxy_header = \"not a header\"\n",
            "[auth]\ntrusted_proxy_header = \"\"\n",
        ];

        for text in refused {
            let config = Config::from_toml(text).unwrap();
            assert!(config.validate().is_err(), "{text}");
        }
        let at_the_limit = Config::from_toml("[auth]\nmax_keys_per_user = 1000\n").unwrap();
        assert_eq!(at_the_limit.validate(), Ok(()));
        let longest =
            Config::from_toml("[auth]\nconsole_session_absolute = \"10years\"\n").unwrap();
        assert_eq!(longest.validate(), Ok(()));
        let too_long = Config::from_toml("[auth]\nconsole_session_absolute = \"1000000years\"\n")
            .unwrap()
            .validate()
            .unwrap_err();
        assert!(
            too_long.contains("auth.console_session_absolute") && too_long.contains("10 years"),
            "{too_long}"
        );
    }

    #[test]
    fn the_branding_defaults_read_from_the_auth_table() {
        let text = "[auth]\n\
                    product_name = \" Acme Notes \"\n\
                    support_contact = \"help@example.com\"\n\
                    feedback_url = \"https://example.com/feedback\"\n\
                    docs_url = \"https://docs.example.com\"\n\
                    billing_url = \"https://billing.example.com/login\"\n\
                    onboarding_url = \"https://docs.example.com/start/\"\n";

        let config = Config::from_toml(text).unwrap();

        assert_eq!(config.validate(), Ok(()));
        assert_eq!(
            config.auth.default_brand(),
            settings::Brand {
                product_name: "Acme Notes".to_string(),
                support_contact: Some("help@example.com".to_string()),
                feedback_url: Some("https://example.com/feedback".to_string()),
                docs_url: Some("https://docs.example.com".to_string()),
                billing_url: Some("https://billing.example.com/login".to_string()),
            }
        );
        // Not part of the brand: only the setup redirect reads it.
        assert_eq!(
            config.auth.onboarding_url.as_deref(),
            Some("https://docs.example.com/start/")
        );
        assert_eq!(
            Config::from_toml("").unwrap().auth.default_brand(),
            settings::Brand::default()
        );
    }

    #[test]
    fn the_mcp_server_name_reads_from_the_auth_table_and_is_checked() {
        assert_eq!(
            Config::from_toml("").unwrap().auth.mcp_server_name,
            "okf_mcp"
        );
        let named = Config::from_toml("[auth]\nmcp_server_name = \"acme-notes.v1\"\n").unwrap();
        assert_eq!(named.validate(), Ok(()));
        assert_eq!(named.auth.mcp_server_name, "acme-notes.v1");
        let longest = format!("[auth]\nmcp_server_name = \"{}\"\n", "a".repeat(64));
        assert_eq!(Config::from_toml(&longest).unwrap().validate(), Ok(()));

        let too_long = format!("[auth]\nmcp_server_name = \"{}\"\n", "a".repeat(65));
        for text in [
            "[auth]\nmcp_server_name = \"\"\n",
            "[auth]\nmcp_server_name = \"Acme Notes\"\n",
            "[auth]\nmcp_server_name = \"acm\u{e9}\"\n",
            too_long.as_str(),
        ] {
            let problem = Config::from_toml(text).unwrap().validate().unwrap_err();
            assert!(
                problem.starts_with("auth.mcp_server_name"),
                "{text}: {problem}"
            );
        }
    }

    #[test]
    fn branding_defaults_the_settings_would_refuse_are_refused() {
        let refused = [
            "[auth]\nproduct_name = \"\"\n",
            "[auth]\nproduct_name = \"Acme\\u0007\"\n",
            "[auth]\nsupport_contact = \"http://example.com\"\n",
            "[auth]\nfeedback_url = \"help@example.com\"\n",
            "[auth]\ndocs_url = \"http://docs.example.com\"\n",
            "[auth]\nbilling_url = \"http://billing.example.com\"\n",
            "[auth]\nonboarding_url = \"/ui/\"\n",
        ];

        for text in refused {
            let problem = Config::from_toml(text).unwrap().validate().unwrap_err();
            assert!(problem.starts_with("auth."), "{text}: {problem}");
        }
    }

    #[test]
    fn mail_defaults_to_the_log_provider_with_a_worker() {
        let mail = Config::from_toml("").unwrap().mail;

        assert_eq!(mail, MailConfig::default());
        assert_eq!(mail.provider, MailProvider::Log);
        assert_eq!(mail.from, "okf <okf@localhost>");
        assert_eq!(mail.worker, None);
        assert!(Config::default().deprecation_warnings().is_empty());
        assert!(Config::default().validate().is_ok());
    }

    #[test]
    fn mail_worker_false_parses_with_a_warning_and_is_ignored() {
        let config = Config::from_toml("[mail]\nworker = false\n").unwrap();

        assert_eq!(config.mail.worker, Some(false));
        assert!(
            config.validate().is_ok(),
            "an old configuration still starts"
        );
        assert_eq!(
            config.deprecation_warnings(),
            ["mail.worker is deprecated and ignored: every serve runs the worker"]
        );
    }

    #[test]
    fn poll_interval_parses_with_a_warning_and_is_ignored() {
        // A zero interval too: nothing runs on it any more.
        for text in ["poll_interval = \"10s\"\n", "poll_interval = \"0s\"\n"] {
            let config = Config::from_toml(text).unwrap();

            assert!(
                config.validate().is_ok(),
                "an old configuration still starts"
            );
            assert_eq!(
                config.deprecation_warnings(),
                ["poll_interval is deprecated and ignored"]
            );
        }
    }

    #[test]
    fn overlay_cache_size_parses_with_a_warning_and_is_ignored() {
        let config = Config::from_toml("overlay_cache_size = 0\n").unwrap();

        assert!(config.validate().is_ok(), "an old configuration starts");
        assert_eq!(
            config.deprecation_warnings(),
            ["overlay_cache_size is deprecated and ignored; set overlay_cache_bytes"]
        );
    }

    #[test]
    fn worker_shutdown_grace_defaults_reads_and_must_be_positive() {
        assert_eq!(
            Config::default().worker_shutdown_grace,
            Duration::from_secs(30)
        );
        let config = Config::from_toml("worker_shutdown_grace = \"45s\"\n").unwrap();
        assert_eq!(config.worker_shutdown_grace, Duration::from_secs(45));
        let error = Config::from_toml("worker_shutdown_grace = \"0s\"\n")
            .unwrap()
            .validate()
            .unwrap_err();
        assert!(error.contains("worker_shutdown_grace"), "{error}");
    }

    #[test]
    fn every_mail_key_reads_from_the_file() {
        let text = "[mail]\n\
                    provider = \"smtp\"\n\
                    from = \"okf <okf@example.com>\"\n\
                    worker = false\n\
                    [mail.smtp]\n\
                    host = \"smtp.example.com\"\n\
                    port = 465\n\
                    starttls = false\n\
                    username = \"okf\"\n\
                    password_env = \"OKF_SMTP_PASSWORD\"\n\
                    [mail.postmark]\n\
                    token_env = \"OKF_POSTMARK_TOKEN\"\n\
                    [mail.ses]\n\
                    region = \"us-east-1\"\n";

        let config = Config::from_toml(text).unwrap();

        assert_eq!(config.mail.provider, MailProvider::Smtp);
        assert_eq!(config.mail.from, "okf <okf@example.com>");
        assert_eq!(config.mail.worker, Some(false));
        assert_eq!(
            config.mail.smtp,
            Some(SmtpConfig {
                host: "smtp.example.com".to_string(),
                port: 465,
                starttls: false,
                username: Some("okf".to_string()),
                password_env: Some("OKF_SMTP_PASSWORD".to_string()),
            })
        );
        assert_eq!(
            config.mail.postmark,
            Some(PostmarkConfig {
                token_env: "OKF_POSTMARK_TOKEN".to_string(),
                message_stream: "outbound".to_string(),
            })
        );
        assert_eq!(
            config.mail.ses,
            Some(SesConfig {
                region: Some("us-east-1".to_string()),
            })
        );
        assert!(config.validate().is_ok());
    }

    #[test]
    fn ses_needs_no_table_and_takes_an_optional_region() {
        for text in [
            "[mail]\nprovider = \"ses\"\n",
            "[mail]\nprovider = \"ses\"\n[mail.ses]\n",
            "[mail]\nprovider = \"ses\"\n[mail.ses]\nregion = \"eu-west-1\"\n",
        ] {
            let config = Config::from_toml(text).unwrap();

            assert_eq!(config.mail.provider, MailProvider::Ses, "{text}");
            assert!(config.validate().is_ok(), "{text}");
        }
    }

    #[test]
    fn smtp_defaults_to_starttls_on_587() {
        let text = "[mail]\nprovider = \"smtp\"\n[mail.smtp]\nhost = \"smtp.example.com\"\n";

        let smtp = Config::from_toml(text).unwrap().mail.smtp.unwrap();

        assert_eq!((smtp.port, smtp.starttls), (587, true));
    }

    #[test]
    fn mail_settings_that_cannot_send_are_refused() {
        let smtp = "[mail]\nprovider = \"smtp\"\n[mail.smtp]\nhost = \"smtp.example.com\"\n";
        let refused = [
            "[mail]\nprovider = \"smtp\"\n".to_string(),
            "[mail]\nprovider = \"postmark\"\n".to_string(),
            "[mail]\nfrom = \"not an address\"\n".to_string(),
            "[mail]\nprovider = \"smtp\"\n[mail.smtp]\nhost = \" \"\n".to_string(),
            format!("{smtp}username = \"okf\"\n"),
            format!("{smtp}password_env = \"OKF_SMTP_PASSWORD\"\n"),
            format!("{smtp}username = \"okf\"\npassword_env = \" \"\n"),
            format!("{smtp}port = 0\n"),
            "[mail]\nprovider = \"postmark\"\n[mail.postmark]\ntoken_env = \"\"\n".to_string(),
            "[mail]\nprovider = \"postmark\"\n[mail.postmark]\ntoken_env = \"T\"\n\
             message_stream = \" \"\n"
                .to_string(),
            "[mail]\nprovider = \"ses\"\n[mail.ses]\nregion = \" \"\n".to_string(),
        ];

        for text in &refused {
            let config = Config::from_toml(text).unwrap();
            assert!(config.validate().is_err(), "{text}");
        }
        assert!(Config::from_toml("[mail]\nprovider = \"pigeon\"\n").is_err());
    }

    const OIDC: &str = r#"
public_url = "https://okf.example.com"
allowed_hosts = ["okf.example.com"]

[[auth.oidc]]
id = "example"
label = "Example ID"
issuer = "https://id.example.com"
client_id = "okf-console"
client_secret_env = "OKF_OIDC_EXAMPLE_SECRET"
"#;

    #[test]
    fn oidc_providers_parse_from_an_array_of_tables_with_default_scopes() {
        let config = Config::from_toml(OIDC).expect("parses");

        config.validate().expect("valid");
        assert_eq!(config.auth.oidc.len(), 1);
        assert_eq!(config.auth.oidc[0].id, "example");
        assert_eq!(config.auth.oidc[0].scopes, ["openid", "email", "profile"]);
    }

    #[test]
    fn bad_oidc_providers_are_refused() {
        let base = Config::from_toml(OIDC).expect("parses");
        // Each case changes one thing and names a word the refusal must
        // contain. `Change` is a plain function pointer; closures that
        // capture nothing coerce to one.
        type Change = fn(&mut Config);
        let cases: [(&str, Change); 8] = [
            ("public_url", |config| config.public_url = None),
            ("id", |config| {
                config.auth.oidc[0].id = "Example".to_string()
            }),
            ("twice", |config| {
                let again = config.auth.oidc[0].clone();
                config.auth.oidc.push(again);
            }),
            ("label", |config| {
                config.auth.oidc[0].label = " ".to_string()
            }),
            ("client_id", |config| {
                config.auth.oidc[0].client_id = String::new()
            }),
            ("client_secret_env", |config| {
                config.auth.oidc[0].client_secret_env = String::new()
            }),
            ("issuer", |config| {
                config.auth.oidc[0].issuer = "http://id.example.com".to_string()
            }),
            ("issuer", |config| {
                config.auth.oidc[0].issuer = "https://id.example.com/?tenant=1".to_string()
            }),
        ];
        for (word, change) in cases {
            let mut config = base.clone();
            change(&mut config);
            let refused = config.validate().expect_err(word);
            assert!(refused.contains(word), "{word}: {refused}");
        }
    }

    #[test]
    fn an_oidc_issuer_may_be_http_on_loopback_only() {
        let mut config = Config::from_toml(OIDC).expect("parses");
        config.auth.oidc[0].issuer = "http://127.0.0.1:9000".to_string();

        config
            .validate()
            .expect("loopback http is allowed, for tests");
    }

    #[test]
    fn an_unknown_oidc_key_is_refused_at_parse_time() {
        let text = format!("{OIDC}client_secret = \"inline\"\n");

        assert!(
            Config::from_toml(&text).is_err(),
            "secrets come from the environment only"
        );
    }

    #[test]
    fn feedback_is_off_by_default_and_parses_with_defaults() {
        let off = Config::from_toml("").unwrap();
        let on = Config::from_toml(
            "[feedback]\nforminit_form_id = \"form-placeholder_1\"\n\
             api_key_env = \"OKF_MCP_FORMINIT_API_KEY\"\n",
        )
        .unwrap();

        assert_eq!(off.feedback, None);
        let feedback = on.feedback.clone().expect("the table");
        assert_eq!(feedback.forminit_form_id, "form-placeholder_1");
        assert_eq!(
            feedback.api_key_env.as_deref(),
            Some("OKF_MCP_FORMINIT_API_KEY")
        );
        assert!(feedback.worker);
        assert_eq!(feedback.max_per_hour, 10);
        assert_eq!(feedback.retention, Duration::from_secs(30 * 24 * 60 * 60));
        assert_eq!(on.validate(), Ok(()));
    }

    #[test]
    fn feedback_parses_every_key() {
        let config = Config::from_toml(
            "[feedback]\nforminit_form_id = \"abc\"\nworker = false\n\
             max_per_hour = 25\nretention = \"7d\"\n",
        )
        .unwrap();

        let feedback = config.feedback.expect("the table");
        assert!(!feedback.worker);
        assert_eq!(feedback.max_per_hour, 25);
        assert_eq!(feedback.retention, Duration::from_secs(7 * 24 * 60 * 60));
        assert_eq!(feedback.api_key_env, None);
    }

    #[test]
    fn a_form_id_that_could_change_the_url_is_refused() {
        let long = "a".repeat(65);
        for id in [
            "",
            "a/b",
            "../x",
            "a?b=1",
            "a#b",
            "a@evil.example",
            "a b",
            "a.b",
            "a%2Fb",
            &long,
        ] {
            let config = Config {
                feedback: Some(FeedbackConfig::for_form(id)),
                ..Config::default()
            };
            let refused = config.validate().unwrap_err();
            assert!(
                refused.contains("feedback.forminit_form_id"),
                "{id:?}: {refused}"
            );
        }
        for id in ["a", "Abc-123_x", &"z".repeat(64)] {
            assert!(is_forminit_form_id(id), "{id:?}");
        }
    }

    #[test]
    fn feedback_rejects_unknown_keys_and_bad_values() {
        // A key in the file, or a misspelling, is a parse error.
        for text in [
            "[feedback]\nforminit_form_id = \"a\"\napi_key = \"zq-secret\"\n",
            "[feedback]\nforminit_form_id = \"a\"\nmax_per_hr = 3\n",
            "[feedback]\napi_key_env = \"X\"\n",
        ] {
            assert!(Config::from_toml(text).is_err(), "{text}");
        }
        for (change, key) in [
            (
                (|feedback: &mut FeedbackConfig| {
                    feedback.api_key_env = Some("HAS SPACE".to_string())
                }) as fn(&mut FeedbackConfig),
                "feedback.api_key_env",
            ),
            (
                |feedback| feedback.api_key_env = Some(String::new()),
                "feedback.api_key_env",
            ),
            (
                |feedback| feedback.max_per_hour = 0,
                "feedback.max_per_hour",
            ),
            (
                |feedback| feedback.max_per_hour = 1001,
                "feedback.max_per_hour",
            ),
            (
                |feedback| feedback.retention = Duration::from_secs(60 * 60),
                "feedback.retention",
            ),
            (
                |feedback| feedback.retention = Duration::from_secs(11 * 366 * 24 * 60 * 60),
                "feedback.retention",
            ),
        ] {
            let mut feedback = FeedbackConfig::for_form("a");
            change(&mut feedback);
            let config = Config {
                feedback: Some(feedback),
                ..Config::default()
            };
            let refused = config.validate().unwrap_err();
            assert!(refused.contains(key), "{refused}");
        }
    }
}
