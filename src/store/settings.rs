//! Policy settings (spec "Settings"): the `[auth]` config supplies the
//! defaults, and `server_settings` rows override them. Every change bumps
//! `admin_generation`, so running servers reload the effective settings
//! with the key cache (`App::reload_keys_if_changed`); request handlers
//! read them from `App::settings()`, never from here.
//!
//! | Key | Stored as (`server_settings.value`) | Accepts ([`set`]) |
//! |---|---|---|
//! | `password_sign_in` | JSON boolean | `true`, `false`, `on`, `off` |
//! | `max_keys_per_user` | JSON number | `0` to `1000` |
//! | `max_key_lifetime` | JSON string (humantime) or `null` | a duration such as `90d`, or `none` |
//! | the four other lifetimes | JSON string (humantime) | a duration above zero |
//! | `product_name` | JSON string | 1 to 60 characters, no control characters (trimmed) |
//! | `support_contact` | JSON string or `null` | an email address, an `https://` URL, or `none` |
//! | `feedback_url` | JSON string or `null` | an `https://` URL, or `none` |
//! | `docs_url` | JSON string or `null` | an `https://` URL, or `none` |
//! | `billing_url` | JSON string or `null` | an `https://` URL, or `none` |
//! | `onboarding_url` | JSON string or `null` | an `https://` URL, or `none` |
//! | `audit_retention_days` | JSON number | `30` to `3650` |
//! | `require_unlock` | JSON boolean | `true`, `false`, `on`, `off` |
//! | `unlock_max_hours` | JSON number | `1` to `72` |
//! | `unlock_notify_owners` | JSON boolean | `true`, `false`, `on`, `off` |
//!
//! The branding settings (`product_name`, `support_contact`,
//! `feedback_url`, `docs_url`, `billing_url`, `onboarding_url`) take their defaults from
//! `[auth]` like the policy keys, falling back to [`DEFAULT_PRODUCT_NAME`],
//! no contact, no feedback link, no documentation link, no billing link and no onboarding
//! redirect; the config file is checked as [`set`] checks a value
//! (`check_branding_defaults`). `audit_retention_days` has no config key
//! (default [`audit::DEFAULT_RETENTION_DAYS`]), since the audit log's
//! append-only trigger reads it from the same row
//! (`migrations/accounts/0012_audit.sql`), and neither have the three unlock
//! settings (spec Part 6; defaults off, [`unlocks::DEFAULT_MAX_HOURS`] and
//! on).
//!
//! [`set`] and [`reset`] record each change in the audit log
//! (`setting.changed`, `setting.cleared`) in their own transaction.
//!
//! Lock order, the same in [`set`] and [`reset`]: first the users the audit
//! entry locks (the actor and its credential's owner, through
//! [`audit::lock_people`], admins first), then the `server_state` row (bumping
//! `admin_generation`), then the `server_settings` row. Users before the
//! generation row is `users::set_admin`'s order too, and the generation row
//! before the setting serializes settings writes.
//!
//! One function, `apply`, checks a value both when it is set (a bad one
//! is refused with 422) and when it is loaded (a row this server cannot
//! read, from a newer version or a hand edit, is logged at `warn` and the
//! config default used), so a stored value can never stop the server.

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
// humantime is the crate humantime_serde parses config durations with;
// `re` re-exports it, so the formats match without another dependency.
use humantime_serde::re::humantime;
use serde::Serialize;
use serde_json::Value;
use sqlx::{PgConnection, PgExecutor, PgPool};

use crate::config::{check_auth_duration, AuthConfig, MAX_KEYS_PER_USER_LIMIT};
use crate::error::OkfError;
use crate::invalidate::{invalidate, Invalidation};
use crate::store::audit::{self, Action, AuditActor, Entry, Target, Via};
use crate::store::unlocks;
use crate::telemetry::subscriber::split_event;

/// The product name with neither an `[auth]` default nor an override.
pub const DEFAULT_PRODUCT_NAME: &str = "okf";

/// The longest product name, in characters.
pub const MAX_PRODUCT_NAME_CHARS: usize = 60;

/// The longest support contact or feedback URL, in characters (an email
/// address is held to `users::check_email`'s 254).
const MAX_SUPPORT_URL_CHARS: usize = 500;

/// What people see the server as: the branding settings people see on
/// every page. Serialized as `GET /ui/brand.json` answers it
/// (`support_contact`, `feedback_url`, `docs_url` and `billing_url` are
/// `null` when unset). `onboarding_url` is not here: only the setup page's redirect
/// reads it ([`Settings::onboarding_url`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Brand {
    /// The product name.
    pub product_name: String,
    /// The support contact (an email address or `https://` URL), if set.
    pub support_contact: Option<String>,
    /// The `https://` URL the console's "Send feedback" link opens, if set.
    pub feedback_url: Option<String>,
    /// The `https://` URL the console's "Documentation" link opens, if set.
    pub docs_url: Option<String>,
    /// The `https://` start page of an external billing service the
    /// console's "Billing" link opens, if set.
    pub billing_url: Option<String>,
}

// `Default` by hand: a derived one would make the product name empty.
impl Default for Brand {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: maps a value, no I/O"
        )
    )]
    fn default() -> Self {
        Self {
            product_name: DEFAULT_PRODUCT_NAME.to_string(),
            support_contact: None,
            feedback_url: None,
            docs_url: None,
            billing_url: None,
        }
    }
}

/// The effective settings: config defaults with overrides applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Whether `/signin` offers email and password.
    pub password_sign_in: bool,
    /// Live self-issued keys a user may hold.
    pub max_keys_per_user: u32,
    /// The longest self-issued key expiry; `None` is no limit.
    pub max_key_lifetime: Option<Duration>,
    /// Console session idle timeout.
    pub console_session_idle: Duration,
    /// Console session absolute lifetime.
    pub console_session_absolute: Duration,
    /// Setup link lifetime.
    pub setup_link_ttl: Duration,
    /// Password-reset link lifetime.
    pub reset_link_ttl: Duration,
    /// The product name in mail, server pages and the console (`[auth]`
    /// `product_name`, else [`DEFAULT_PRODUCT_NAME`]).
    pub product_name: String,
    /// An email address or `https://` URL people are pointed to; `None`
    /// shows no contact line.
    pub support_contact: Option<String>,
    /// An `https://` URL the console's "Send feedback" link opens; `None`
    /// shows no link.
    pub feedback_url: Option<String>,
    /// An `https://` URL the console's "Documentation" link opens; `None`
    /// shows no link.
    pub docs_url: Option<String>,
    /// An `https://` start page of an external billing service the
    /// console's "Billing" link opens (it signs the person in through
    /// this server's OAuth); `None` shows no link.
    pub billing_url: Option<String>,
    /// An `https://` URL a successful first password setup redirects to
    /// (a getting-started guide); `None` goes to the console as before.
    pub onboarding_url: Option<String>,
    /// How many days audit entries are kept before the daily prune deletes
    /// them ([`audit::DEFAULT_RETENTION_DAYS`] by default).
    pub audit_retention_days: u32,
    /// Whether operator credentials need an unlock to open a customer's
    /// catalog contents (off by default).
    pub require_unlock: bool,
    /// The longest unlock, in hours ([`unlocks::DEFAULT_MAX_HOURS`] by
    /// default).
    pub unlock_max_hours: u32,
    /// Whether the owner team's owners are emailed when one of its
    /// catalogs is unlocked (on by default).
    pub unlock_notify_owners: bool,
}

impl Settings {
    /// The settings with no overrides.
    ///
    /// # Arguments
    /// * `defaults` - the `[auth]` section.
    ///
    /// # Returns
    /// Its policy values.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: maps a value, no I/O"
        )
    )]
    pub fn from_config(defaults: &AuthConfig) -> Self {
        let Brand {
            product_name,
            support_contact,
            feedback_url,
            docs_url,
            billing_url,
        } = defaults.default_brand();
        Self {
            password_sign_in: defaults.password_sign_in,
            max_keys_per_user: defaults.max_keys_per_user,
            max_key_lifetime: defaults.max_key_lifetime,
            console_session_idle: defaults.console_session_idle,
            console_session_absolute: defaults.console_session_absolute,
            setup_link_ttl: defaults.setup_link_ttl,
            reset_link_ttl: defaults.reset_link_ttl,
            // `validate` checked these as `set` would.
            product_name,
            support_contact,
            feedback_url,
            docs_url,
            billing_url,
            onboarding_url: defaults.onboarding_url.clone(),
            // Runtime settings only, with no `[auth]` key (spec "My calls").
            audit_retention_days: audit::DEFAULT_RETENTION_DAYS,
            require_unlock: false,
            unlock_max_hours: unlocks::DEFAULT_MAX_HOURS,
            unlock_notify_owners: true,
        }
    }

    /// The branding settings.
    ///
    /// # Returns
    /// A copy of the product name, support contact, feedback URL and
    /// documentation URL.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: maps a value, no I/O"
        )
    )]
    pub fn brand(&self) -> Brand {
        Brand {
            product_name: self.product_name.clone(),
            support_contact: self.support_contact.clone(),
            feedback_url: self.feedback_url.clone(),
            docs_url: self.docs_url.clone(),
            billing_url: self.billing_url.clone(),
        }
    }
}

/// One setting's name. clap spells each exactly as stored
/// (`#[value(name = ...)]`; its default would be kebab-case).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, clap::ValueEnum)]
pub enum SettingKey {
    /// `password_sign_in`.
    #[value(name = "password_sign_in")]
    PasswordSignIn,
    /// `max_keys_per_user`.
    #[value(name = "max_keys_per_user")]
    MaxKeysPerUser,
    /// `max_key_lifetime`.
    #[value(name = "max_key_lifetime")]
    MaxKeyLifetime,
    /// `console_session_idle`.
    #[value(name = "console_session_idle")]
    ConsoleSessionIdle,
    /// `console_session_absolute`.
    #[value(name = "console_session_absolute")]
    ConsoleSessionAbsolute,
    /// `setup_link_ttl`.
    #[value(name = "setup_link_ttl")]
    SetupLinkTtl,
    /// `reset_link_ttl`.
    #[value(name = "reset_link_ttl")]
    ResetLinkTtl,
    /// `product_name`.
    #[value(name = "product_name")]
    ProductName,
    /// `support_contact`.
    #[value(name = "support_contact")]
    SupportContact,
    /// `feedback_url`.
    #[value(name = "feedback_url")]
    FeedbackUrl,
    /// `docs_url`.
    #[value(name = "docs_url")]
    DocsUrl,
    /// `billing_url`.
    #[value(name = "billing_url")]
    BillingUrl,
    /// `onboarding_url`.
    #[value(name = "onboarding_url")]
    OnboardingUrl,
    /// `audit_retention_days`.
    #[value(name = "audit_retention_days")]
    AuditRetentionDays,
    /// `require_unlock`.
    #[value(name = "require_unlock")]
    RequireUnlock,
    /// `unlock_max_hours`.
    #[value(name = "unlock_max_hours")]
    UnlockMaxHours,
    /// `unlock_notify_owners`.
    #[value(name = "unlock_notify_owners")]
    UnlockNotifyOwners,
}

impl SettingKey {
    /// Every key, in the order `settings list` shows them.
    pub const ALL: [SettingKey; 17] = [
        SettingKey::PasswordSignIn,
        SettingKey::MaxKeysPerUser,
        SettingKey::MaxKeyLifetime,
        SettingKey::ConsoleSessionIdle,
        SettingKey::ConsoleSessionAbsolute,
        SettingKey::SetupLinkTtl,
        SettingKey::ResetLinkTtl,
        SettingKey::ProductName,
        SettingKey::SupportContact,
        SettingKey::AuditRetentionDays,
        SettingKey::RequireUnlock,
        SettingKey::UnlockMaxHours,
        SettingKey::UnlockNotifyOwners,
        SettingKey::FeedbackUrl,
        SettingKey::DocsUrl,
        SettingKey::BillingUrl,
        SettingKey::OnboardingUrl,
    ];

    /// The stored name.
    ///
    /// # Returns
    /// The `server_settings.key` value.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: maps a value, no I/O"
        )
    )]
    pub fn as_str(self) -> &'static str {
        match self {
            SettingKey::PasswordSignIn => "password_sign_in",
            SettingKey::MaxKeysPerUser => "max_keys_per_user",
            SettingKey::MaxKeyLifetime => "max_key_lifetime",
            SettingKey::ConsoleSessionIdle => "console_session_idle",
            SettingKey::ConsoleSessionAbsolute => "console_session_absolute",
            SettingKey::SetupLinkTtl => "setup_link_ttl",
            SettingKey::ResetLinkTtl => "reset_link_ttl",
            SettingKey::ProductName => "product_name",
            SettingKey::SupportContact => "support_contact",
            SettingKey::FeedbackUrl => "feedback_url",
            SettingKey::DocsUrl => "docs_url",
            SettingKey::BillingUrl => "billing_url",
            SettingKey::OnboardingUrl => "onboarding_url",
            SettingKey::AuditRetentionDays => "audit_retention_days",
            SettingKey::RequireUnlock => "require_unlock",
            SettingKey::UnlockMaxHours => "unlock_max_hours",
            SettingKey::UnlockNotifyOwners => "unlock_notify_owners",
        }
    }

    /// Reads a stored name.
    ///
    /// # Arguments
    /// * `name` - a `server_settings.key` value.
    ///
    /// # Returns
    /// The key, or `None` for a name this server does not know.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: maps a value, no I/O"
        )
    )]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|key| key.as_str() == name)
    }
}

/// One row of `settings list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingRow {
    /// Which setting.
    pub key: SettingKey,
    /// Its effective value, as [`set`] accepts it (`"true"`, `"20"`,
    /// `"12h"`, `"none"`).
    pub value: String,
    /// Whether a `server_settings` row overrides the config default.
    pub overridden: bool,
    /// When the override was written.
    pub updated_at: Option<DateTime<Utc>>,
    /// Who wrote it (an actor or user name).
    pub updated_by: Option<String>,
}

/// The refusal for a value a setting cannot take.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial conversion: builds an error value, no I/O"
    )
)]
fn invalid(reason: String) -> OkfError {
    OkfError::LimitExceeded {
        limit: "setting",
        reason,
    }
}

/// Parses a duration in the config file's format and checks it as the
/// config file does.
#[tracing::instrument(level = "trace", skip_all)]
fn duration(key: SettingKey, text: &str) -> Result<Duration, String> {
    let parsed = humantime::parse_duration(text)
        .map_err(|error| format!("{}: {text:?} is not a duration ({error})", key.as_str()))?;
    check_auth_duration(key.as_str(), parsed)?;
    Ok(parsed)
}

/// Checks the `[auth]` branding defaults as [`set`] checks the settings,
/// so the file can give no default the Settings page would refuse.
///
/// # Arguments
/// * `defaults` - the `[auth]` section.
///
/// # Returns
/// `Ok(())`, or the refusal, naming the key.
///
/// # Errors
/// A `product_name`, `support_contact`, `feedback_url`, `docs_url`,
/// `billing_url` or `onboarding_url` its setting refuses.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "runs before the subscriber exists (configuration, builder or start-up), so a span would be recorded nowhere"
    )
)]
pub(crate) fn check_branding_defaults(defaults: &AuthConfig) -> Result<(), String> {
    check_product_name(&defaults.product_name)?;
    if let Some(contact) = defaults.support_contact.as_deref() {
        check_support_contact(contact)?;
    }
    for (key, url) in [
        (SettingKey::FeedbackUrl, &defaults.feedback_url),
        (SettingKey::DocsUrl, &defaults.docs_url),
        (SettingKey::BillingUrl, &defaults.billing_url),
        (SettingKey::OnboardingUrl, &defaults.onboarding_url),
    ] {
        if let Some(url) = url.as_deref() {
            check_url_setting(key, url)?;
        }
    }
    Ok(())
}

/// A product name as stored: trimmed, 1 to [`MAX_PRODUCT_NAME_CHARS`]
/// characters, no control characters (a NUL, which Postgres refuses, or a
/// line break in a mail subject).
#[tracing::instrument(level = "trace", skip_all)]
fn check_product_name(text: &str) -> Result<&str, String> {
    let name = text.trim();
    // `chars().count()`: characters, not bytes (`len()` counts UTF-8 bytes).
    let length = name.chars().count();
    if length == 0 || length > MAX_PRODUCT_NAME_CHARS || name.chars().any(char::is_control) {
        return Err(format!(
            "product_name takes 1 to {MAX_PRODUCT_NAME_CHARS} characters with no control \
             characters"
        ));
    }
    Ok(name)
}

/// A URL setting: `https://` with a host, at most [`MAX_SUPPORT_URL_CHARS`]
/// characters, with no whitespace or control characters (the URL parser
/// would silently drop a tab or line break, which would then be stored).
///
/// # Arguments
/// * `text` - the candidate URL.
///
/// # Returns
/// `true` when it is acceptable.
#[tracing::instrument(level = "trace", skip_all)]
fn is_https_url(text: &str) -> bool {
    if !text.starts_with("https://") {
        return false;
    }
    let clean = text.chars().count() <= MAX_SUPPORT_URL_CHARS
        && !text.chars().any(|c| c.is_whitespace() || c.is_control());
    // `is_some_and`: the URL parses and has a non-empty host.
    let has_host = url::Url::parse(text)
        .ok()
        .is_some_and(|url| url.host_str().is_some_and(|host| !host.is_empty()));
    clean && has_host
}

/// A support contact: one plain email address, or an `https://` URL (see
/// [`is_https_url`]).
#[tracing::instrument(level = "trace", skip_all)]
fn check_support_contact(text: &str) -> Result<(), String> {
    let refuse =
        || format!("support_contact takes an email address, an https:// URL or none, not {text:?}");
    if text.starts_with("https://") {
        return if is_https_url(text) {
            Ok(())
        } else {
            Err(refuse())
        };
    }
    crate::store::users::check_email(text).map_err(|_| refuse())
}

/// A link setting (`feedback_url`, `docs_url`, `billing_url`, `onboarding_url`): an
/// `https://` URL only (see [`is_https_url`]); an email address is refused.
#[tracing::instrument(level = "trace", skip_all)]
fn check_url_setting(key: SettingKey, text: &str) -> Result<(), String> {
    if is_https_url(text) {
        Ok(())
    } else {
        Err(format!(
            "{} takes an https:// URL or none, not {text:?}",
            key.as_str()
        ))
    }
}

/// Reads the text of a boolean setting: `true`/`on` or `false`/`off`.
#[tracing::instrument(level = "trace", skip_all)]
fn parse_boolean(key: SettingKey, text: &str) -> Result<Value, String> {
    match text {
        "true" | "on" => Ok(Value::Bool(true)),
        "false" | "off" => Ok(Value::Bool(false)),
        _ => Err(format!(
            "{} takes true or false, not {text:?}",
            key.as_str()
        )),
    }
}

/// The refusal text for an `unlock_max_hours` outside its range.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial conversion: pure arithmetic or formatting on its arguments, no I/O"
    )
)]
fn unlock_hours_range() -> String {
    format!(
        "unlock_max_hours takes a whole number from 1 to {}",
        unlocks::MAX_HOURS_LIMIT
    )
}

/// Turns the text [`set`] was given into the JSON to store.
#[tracing::instrument(level = "trace", skip_all)]
fn parse_value(key: SettingKey, text: &str) -> Result<Value, String> {
    let text = text.trim();
    match key {
        SettingKey::PasswordSignIn | SettingKey::RequireUnlock | SettingKey::UnlockNotifyOwners => {
            parse_boolean(key, text)
        }
        // `u32` parsing refuses a sign, so "-1" fails here; the upper
        // bound is `apply`'s.
        SettingKey::MaxKeysPerUser => text.parse::<u32>().map(Value::from).map_err(|_| {
            format!("max_keys_per_user takes a whole number from 0 to {MAX_KEYS_PER_USER_LIMIT}")
        }),
        SettingKey::MaxKeyLifetime if text == "none" => Ok(Value::Null),
        // Stored in humantime's own spelling, so `list` shows what `load` reads.
        SettingKey::MaxKeyLifetime
        | SettingKey::ConsoleSessionIdle
        | SettingKey::ConsoleSessionAbsolute
        | SettingKey::SetupLinkTtl
        | SettingKey::ResetLinkTtl => duration(key, text)
            .map(|parsed| Value::String(humantime::format_duration(parsed).to_string())),
        SettingKey::ProductName => {
            check_product_name(text).map(|name| Value::String(name.to_string()))
        }
        SettingKey::SupportContact if text == "none" => Ok(Value::Null),
        SettingKey::SupportContact => {
            check_support_contact(text).map(|()| Value::String(text.to_string()))
        }
        SettingKey::FeedbackUrl
        | SettingKey::DocsUrl
        | SettingKey::BillingUrl
        | SettingKey::OnboardingUrl
            if text == "none" =>
        {
            Ok(Value::Null)
        }
        SettingKey::FeedbackUrl
        | SettingKey::DocsUrl
        | SettingKey::BillingUrl
        | SettingKey::OnboardingUrl => {
            check_url_setting(key, text).map(|()| Value::String(text.to_string()))
        }
        // A JSON number, which the append-only trigger reads too; the range
        // is `apply`'s.
        SettingKey::AuditRetentionDays => text
            .parse::<u32>()
            .map(Value::from)
            .map_err(|_| retention_range()),
        // A JSON number; the range is `apply`'s.
        SettingKey::UnlockMaxHours => text
            .parse::<u32>()
            .map(Value::from)
            .map_err(|_| unlock_hours_range()),
    }
}

/// The refusal text for an `audit_retention_days` outside its range.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial conversion: pure arithmetic or formatting on its arguments, no I/O"
    )
)]
fn retention_range() -> String {
    format!(
        "audit_retention_days takes a whole number from {} to {}",
        audit::MIN_RETENTION_DAYS,
        audit::MAX_RETENTION_DAYS
    )
}

/// Applies one stored value to `settings`, after checking it. Used when
/// setting a value (to refuse a bad one) and when loading (to skip one).
/// On an error `settings` is unchanged.
#[tracing::instrument(level = "trace", skip_all)]
fn apply(settings: &mut Settings, key: SettingKey, value: &Value) -> Result<(), String> {
    // A closure (Python's lambda, but it may span lines) shared by the
    // five duration keys.
    let as_duration = |value: &Value| -> Result<Duration, String> {
        let text = value
            .as_str()
            .ok_or_else(|| format!("{} must be a duration string", key.as_str()))?;
        duration(key, text)
    };
    // And the three link keys: `null` for none.
    let as_url = |value: &Value| -> Result<Option<String>, String> {
        match value {
            Value::Null => Ok(None),
            other => {
                let text = other
                    .as_str()
                    .ok_or_else(|| format!("{} must be a string or null", key.as_str()))?;
                check_url_setting(key, text)?;
                Ok(Some(text.to_string()))
            }
        }
    };
    // And the three boolean keys.
    let as_bool = |value: &Value| -> Result<bool, String> {
        value
            .as_bool()
            .ok_or_else(|| format!("{} must be true or false", key.as_str()))
    };
    match key {
        SettingKey::PasswordSignIn => settings.password_sign_in = as_bool(value)?,
        SettingKey::RequireUnlock => settings.require_unlock = as_bool(value)?,
        SettingKey::UnlockNotifyOwners => settings.unlock_notify_owners = as_bool(value)?,
        SettingKey::UnlockMaxHours => {
            settings.unlock_max_hours = value
                .as_u64()
                .and_then(|hours| u32::try_from(hours).ok())
                .filter(|hours| (1..=unlocks::MAX_HOURS_LIMIT).contains(hours))
                .ok_or_else(unlock_hours_range)?;
        }
        SettingKey::MaxKeysPerUser => {
            // `as_u64` is `None` for negatives, fractions and non-numbers;
            // `try_from` refuses what does not fit a `u32`.
            settings.max_keys_per_user = value
                .as_u64()
                .and_then(|count| u32::try_from(count).ok())
                .filter(|count| *count <= MAX_KEYS_PER_USER_LIMIT)
                .ok_or_else(|| {
                    format!("max_keys_per_user must be from 0 to {MAX_KEYS_PER_USER_LIMIT}")
                })?;
        }
        SettingKey::MaxKeyLifetime => {
            settings.max_key_lifetime = match value {
                Value::Null => None,
                other => Some(as_duration(other)?),
            };
        }
        SettingKey::ConsoleSessionIdle => settings.console_session_idle = as_duration(value)?,
        SettingKey::ConsoleSessionAbsolute => {
            settings.console_session_absolute = as_duration(value)?;
        }
        SettingKey::SetupLinkTtl => settings.setup_link_ttl = as_duration(value)?,
        SettingKey::ResetLinkTtl => settings.reset_link_ttl = as_duration(value)?,
        SettingKey::ProductName => {
            let text = value
                .as_str()
                .ok_or_else(|| "product_name must be a string".to_string())?;
            settings.product_name = check_product_name(text)?.to_string();
        }
        SettingKey::SupportContact => {
            settings.support_contact = match value {
                Value::Null => None,
                other => {
                    let text = other
                        .as_str()
                        .ok_or_else(|| "support_contact must be a string or null".to_string())?;
                    check_support_contact(text)?;
                    Some(text.to_string())
                }
            };
        }
        SettingKey::FeedbackUrl => settings.feedback_url = as_url(value)?,
        SettingKey::DocsUrl => settings.docs_url = as_url(value)?,
        SettingKey::BillingUrl => settings.billing_url = as_url(value)?,
        SettingKey::OnboardingUrl => settings.onboarding_url = as_url(value)?,
        SettingKey::AuditRetentionDays => {
            // `RangeInclusive::contains`: `MIN..=MAX` includes both ends.
            settings.audit_retention_days = value
                .as_u64()
                .and_then(|days| u32::try_from(days).ok())
                .filter(|days| {
                    (audit::MIN_RETENTION_DAYS..=audit::MAX_RETENTION_DAYS).contains(days)
                })
                .ok_or_else(retention_range)?;
        }
    }
    Ok(())
}

/// A setting's effective value as [`set`] would accept it.
#[tracing::instrument(level = "trace", skip_all)]
fn display(settings: &Settings, key: SettingKey) -> String {
    let format = |value: Duration| humantime::format_duration(value).to_string();
    match key {
        SettingKey::PasswordSignIn => settings.password_sign_in.to_string(),
        SettingKey::MaxKeysPerUser => settings.max_keys_per_user.to_string(),
        SettingKey::MaxKeyLifetime => settings
            .max_key_lifetime
            .map_or_else(|| "none".to_string(), format),
        SettingKey::ConsoleSessionIdle => format(settings.console_session_idle),
        SettingKey::ConsoleSessionAbsolute => format(settings.console_session_absolute),
        SettingKey::SetupLinkTtl => format(settings.setup_link_ttl),
        SettingKey::ResetLinkTtl => format(settings.reset_link_ttl),
        SettingKey::ProductName => settings.product_name.clone(),
        SettingKey::SupportContact => settings
            .support_contact
            .clone()
            .unwrap_or_else(|| "none".to_string()),
        SettingKey::FeedbackUrl => settings
            .feedback_url
            .clone()
            .unwrap_or_else(|| "none".to_string()),
        SettingKey::DocsUrl => settings
            .docs_url
            .clone()
            .unwrap_or_else(|| "none".to_string()),
        SettingKey::BillingUrl => settings
            .billing_url
            .clone()
            .unwrap_or_else(|| "none".to_string()),
        SettingKey::OnboardingUrl => settings
            .onboarding_url
            .clone()
            .unwrap_or_else(|| "none".to_string()),
        SettingKey::AuditRetentionDays => settings.audit_retention_days.to_string(),
        SettingKey::RequireUnlock => settings.require_unlock.to_string(),
        SettingKey::UnlockMaxHours => settings.unlock_max_hours.to_string(),
        SettingKey::UnlockNotifyOwners => settings.unlock_notify_owners.to_string(),
    }
}

/// One stored override.
#[derive(sqlx::FromRow)]
struct StoredSetting {
    key: String,
    value: Value,
    updated_at: DateTime<Utc>,
    updated_by: String,
}

/// Overlays stored rows on the defaults, skipping (with a log line) rows
/// this server cannot read.
#[tracing::instrument(level = "trace", skip_all)]
fn overlay(defaults: &AuthConfig, rows: &[StoredSetting]) -> Settings {
    let mut settings = Settings::from_config(defaults);
    apply_rows(&mut settings, rows);
    settings
}

/// Applies stored rows to `settings`, skipping (with a log line) rows this
/// server cannot read.
#[tracing::instrument(level = "trace", skip_all)]
fn apply_rows(settings: &mut Settings, rows: &[StoredSetting]) {
    for row in rows {
        let Some(key) = SettingKey::parse(&row.key) else {
            split_event!(
                debug,
                {},
                { setting = %row.key, },
                "ignoring a setting this server does not know"
            );
            continue;
        };
        if let Err(problem) = apply(settings, key, &row.value) {
            split_event!(
                warn,
                {},
                { setting = %row.key, %problem, },
                "stored setting is invalid; using the default"
            );
        }
    }
}

/// The branding settings, read inside the caller's connection or
/// transaction, so a mail queued from the CLI or from any server has the
/// brand stored now. A stored value this server cannot read falls back to
/// the default, as in [`load`].
///
/// # Arguments
/// * `conn` - the caller's connection or transaction (`&mut tx`).
/// * `defaults` - the config's branding (`AuthConfig::default_brand`):
///   what a key with no stored row takes.
///
/// # Returns
/// The product name, support contact, feedback URL, documentation URL and
/// billing URL.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "settings.brand_in"))]
pub async fn brand_in(conn: &mut PgConnection, defaults: &Brand) -> Result<Brand, OkfError> {
    let rows = sqlx::query_as::<_, StoredSetting>(
        "SELECT key, value, updated_at, updated_by FROM server_settings \
         WHERE key IN ('product_name', 'support_contact', 'feedback_url', 'docs_url', 'billing_url')",
    )
    .fetch_all(&mut *conn)
    .await?;
    // Only the brand's rows were read, so the policy fields keep
    // their built-in defaults and are dropped by `brand()`.
    let mut settings = Settings {
        product_name: defaults.product_name.clone(),
        support_contact: defaults.support_contact.clone(),
        feedback_url: defaults.feedback_url.clone(),
        docs_url: defaults.docs_url.clone(),
        billing_url: defaults.billing_url.clone(),
        ..Settings::from_config(&AuthConfig::default())
    };
    apply_rows(&mut settings, &rows);
    Ok(settings.brand())
}

/// Every stored override.
// `impl PgExecutor<'_>` takes a `&PgPool` or a `&mut PgConnection` (a
// transaction), like a Python function typed against a protocol.
#[tracing::instrument(level = "trace", skip_all, fields(db.system.name = "postgresql", db.operation.name = "settings.stored"))]
async fn stored(executor: impl PgExecutor<'_>) -> Result<Vec<StoredSetting>, OkfError> {
    Ok(sqlx::query_as::<_, StoredSetting>(
        "SELECT key, value, updated_at, updated_by FROM server_settings",
    )
    .fetch_all(executor)
    .await?)
}

/// The effective settings. Only code without an `App` (the CLI, and
/// `App` itself when it loads or reloads) calls this.
///
/// # Arguments
/// * `executor` - the database: the pool, or a transaction to read in
///   (the key cache's gates read `require_unlock` in their snapshot).
/// * `defaults` - the `[auth]` section.
///
/// # Returns
/// The defaults with every readable override applied.
///
/// # Errors
/// [`OkfError::Database`]. An unreadable stored value is not an error.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "settings.load"))]
pub async fn load(
    executor: impl PgExecutor<'_>,
    defaults: &AuthConfig,
) -> Result<Settings, OkfError> {
    Ok(overlay(defaults, &stored(executor).await?))
}

/// What `settings list` shows as who wrote an override: the credential's
/// actor (a session's user name), or `cli:<os user>` for the command line,
/// as the audit entry's actor label reads.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial conversion: pure arithmetic or formatting on its arguments, no I/O"
    )
)]
pub(crate) fn updated_by(actor: &AuditActor) -> String {
    match actor.via {
        Via::Cli | Via::System => actor.stored_label(),
        Via::Session | Via::Key | Via::Grant => actor.label.clone(),
    }
}

/// An entry's details for a setting's value: the stored JSON, except for
/// `support_contact`, which can be an email address and is never written
/// into an entry (spec "People in entries"). Every key is listed, so a new
/// setting must decide which arm it takes. `feedback_url`, `docs_url`,
/// `billing_url` and `onboarding_url` are `https://` URLs, never a person, so they are
/// recorded. The unlock settings are policy
/// (a flag and a number of hours), never a person, so they are recorded.
#[tracing::instrument(level = "trace", skip_all)]
fn audited_value(key: SettingKey, value: &Value) -> Option<&Value> {
    match key {
        SettingKey::SupportContact => None,
        SettingKey::PasswordSignIn
        | SettingKey::MaxKeysPerUser
        | SettingKey::MaxKeyLifetime
        | SettingKey::ConsoleSessionIdle
        | SettingKey::ConsoleSessionAbsolute
        | SettingKey::SetupLinkTtl
        | SettingKey::ResetLinkTtl
        | SettingKey::ProductName
        | SettingKey::FeedbackUrl
        | SettingKey::DocsUrl
        | SettingKey::BillingUrl
        | SettingKey::OnboardingUrl
        | SettingKey::AuditRetentionDays
        | SettingKey::RequireUnlock
        | SettingKey::UnlockMaxHours
        | SettingKey::UnlockNotifyOwners => Some(value),
    }
}

/// Overrides one setting, records `setting.changed`, and bumps the
/// administrative generation, all in one transaction. Setting the value
/// already stored writes nothing and records nothing.
///
/// # Arguments
/// * `pool` - the database.
/// * `key` - which setting.
/// * `value` - the new value, as text (see the module's table).
/// * `actor` - who is setting it: the audit entry's actor, and
///   `updated_by` for `settings list`.
/// * `now` - when (server clock).
///
/// # Returns
/// `Ok(())` once the override is stored (or was already this value).
///
/// # Errors
/// [`OkfError::LimitExceeded`] with `limit: "setting"` for a value the
/// setting cannot take (nothing is written); [`OkfError::Database`],
/// including a failed audit write, which stores nothing.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "settings.set"))]
pub async fn set(
    pool: &PgPool,
    key: SettingKey,
    value: &str,
    actor: &AuditActor,
    now: DateTime<Utc>,
) -> Result<(), OkfError> {
    let json = parse_value(key, value).map_err(invalid)?;
    // Apply to a scratch copy only to run the same checks `load` will.
    apply(
        &mut Settings::from_config(&AuthConfig::default()),
        key,
        &json,
    )
    .map_err(invalid)?;
    let mut tx = pool.begin().await?;
    // Lock order (module docs): the actor's user rows, then the generation
    // row, then the setting.
    audit::lock_people(&mut tx, actor, &audit::People::default()).await?;
    // Every settings write takes the generation row's lock, so concurrent
    // writes queue here, and the read below (a new statement, under READ
    // COMMITTED) sees the one before, even when it inserted the row
    // `FOR UPDATE` alone could not lock.
    invalidate(&mut tx, &[Invalidation::Settings]).await?;
    let previous: Option<Value> =
        sqlx::query_scalar("SELECT value FROM server_settings WHERE key = $1 FOR UPDATE")
            .bind(key.as_str())
            .fetch_optional(&mut *tx)
            .await?;
    if previous.as_ref() == Some(&json) {
        // Unchanged: dropping `tx` rolls the generation bump back, and
        // its `NOTIFY` is never sent.
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO server_settings (key, value, updated_at, updated_by) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, \
             updated_at = EXCLUDED.updated_at, updated_by = EXCLUDED.updated_by",
    )
    .bind(key.as_str())
    .bind(&json)
    .bind(now)
    .bind(updated_by(actor))
    .execute(&mut *tx)
    .await?;
    let details = match audited_value(key, &json) {
        Some(to) => serde_json::json!({"from": previous, "to": to}),
        // Only whether a contact is set now.
        None => serde_json::json!({"set": !json.is_null()}),
    };
    audit::record(
        &mut tx,
        actor,
        &Entry {
            action: Action::SettingChanged,
            target: Target::Setting(key.as_str()),
            person: None,
            teams: &[],
            details,
        },
        now,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Removes a setting's override, so the config default applies again.
/// When there was one, records `setting.cleared` and bumps the
/// administrative generation in the same transaction.
///
/// # Arguments
/// * `pool` - the database.
/// * `key` - which setting.
/// * `actor` - who is resetting it, for the audit entry.
/// * `now` - when (server clock).
///
/// # Returns
/// `Ok(())`, whether or not there was an override.
///
/// # Errors
/// [`OkfError::Database`], including a failed audit write, which keeps
/// the override.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "settings.reset"))]
pub async fn reset(
    pool: &PgPool,
    key: SettingKey,
    actor: &AuditActor,
    now: DateTime<Utc>,
) -> Result<(), OkfError> {
    let mut tx = pool.begin().await?;
    // The same lock order as `set` (module docs): the actor's user rows,
    // then the generation row, then the setting, so a concurrent set, reset
    // or `users::set_admin` queue instead of each holding a row the other
    // waits on (a deadlock).
    audit::lock_people(&mut tx, actor, &audit::People::default()).await?;
    invalidate(&mut tx, &[Invalidation::Settings]).await?;
    // `RETURNING value`: the deleted override, for the entry.
    let deleted: Option<Value> =
        sqlx::query_scalar("DELETE FROM server_settings WHERE key = $1 RETURNING value")
            .bind(key.as_str())
            .fetch_optional(&mut *tx)
            .await?;
    // `let ... else`: no override, nothing to record; dropping `tx` rolls
    // the generation bump back, and its `NOTIFY` is never sent.
    let Some(from) = deleted else {
        return Ok(());
    };
    let details = match audited_value(key, &from) {
        Some(from) => serde_json::json!({ "from": from }),
        None => serde_json::json!({}),
    };
    audit::record(
        &mut tx,
        actor,
        &Entry {
            action: Action::SettingCleared,
            target: Target::Setting(key.as_str()),
            person: None,
            teams: &[],
            details,
        },
        now,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Every setting with its effective value and override, in
/// [`SettingKey::ALL`] order.
///
/// # Arguments
/// * `pool` - the database.
/// * `defaults` - the `[auth]` section.
///
/// # Returns
/// One row per key. Rows for unknown keys are left out; an unreadable
/// override shows the default as the value, with `overridden: true`.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "settings.list"))]
pub async fn list(pool: &PgPool, defaults: &AuthConfig) -> Result<Vec<SettingRow>, OkfError> {
    let rows = stored(pool).await?;
    let settings = overlay(defaults, &rows);
    // Index the rows by name once instead of scanning them per key.
    let by_name: HashMap<&str, &StoredSetting> =
        rows.iter().map(|row| (row.key.as_str(), row)).collect();
    Ok(SettingKey::ALL
        .into_iter()
        .map(|key| {
            let row = by_name.get(key.as_str());
            SettingRow {
                key,
                value: display(&settings, key),
                overridden: row.is_some(),
                updated_at: row.map(|row| row.updated_at),
                updated_by: row.map(|row| row.updated_by.clone()),
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;
    use crate::store::catalogs::admin_generation;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-12T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    /// An API key's actor labelled `label`, as `AuditActor::of` builds one.
    fn by(label: &str) -> AuditActor {
        AuditActor {
            source: audit::Source::Api,
            credential: Some("k1".to_string()),
            label: label.to_string(),
            via: Via::Key,
            ..AuditActor::cli()
        }
    }

    fn defaults() -> AuthConfig {
        AuthConfig {
            max_keys_per_user: 7,
            ..AuthConfig::default()
        }
    }

    #[test]
    fn every_key_round_trips_and_spells_with_underscores() {
        for key in SettingKey::ALL {
            assert_eq!(SettingKey::parse(key.as_str()), Some(key));
            // clap's spelling (`settings set KEY`) is the stored one.
            let clap = clap::ValueEnum::to_possible_value(&key).unwrap();
            assert_eq!(clap.get_name(), key.as_str());
        }
        assert_eq!(SettingKey::parse("session_idle"), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn without_overrides_the_settings_are_the_config_defaults() {
        let db = test_db().await;

        let settings = load(&db.pool, &defaults()).await.unwrap();

        assert_eq!(settings, Settings::from_config(&defaults()));
        assert_eq!(settings.max_keys_per_user, 7);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_overrides_a_default_records_who_and_bumps_the_generation() {
        let db = test_db().await;
        let start = admin_generation(&db.pool).await.unwrap();

        set(
            &db.pool,
            SettingKey::MaxKeysPerUser,
            "3",
            &by("alice"),
            now(),
        )
        .await
        .unwrap();
        set(
            &db.pool,
            SettingKey::PasswordSignIn,
            "off",
            &by("alice"),
            now(),
        )
        .await
        .unwrap();
        set(
            &db.pool,
            SettingKey::ConsoleSessionIdle,
            "30m",
            &by("bob"),
            now(),
        )
        .await
        .unwrap();
        set(
            &db.pool,
            SettingKey::MaxKeyLifetime,
            "90d",
            &by("bob"),
            now(),
        )
        .await
        .unwrap();

        let settings = load(&db.pool, &defaults()).await.unwrap();
        assert_eq!(settings.max_keys_per_user, 3);
        assert!(!settings.password_sign_in);
        assert_eq!(settings.console_session_idle, Duration::from_secs(30 * 60));
        assert_eq!(
            settings.max_key_lifetime,
            Some(Duration::from_secs(90 * 86_400))
        );
        assert_eq!(admin_generation(&db.pool).await.unwrap(), start + 4);
        let rows = list(&db.pool, &defaults()).await.unwrap();
        let idle = rows
            .iter()
            .find(|r| r.key == SettingKey::ConsoleSessionIdle)
            .unwrap();
        assert_eq!(
            (
                idle.value.as_str(),
                idle.overridden,
                idle.updated_by.as_deref()
            ),
            ("30m", true, Some("bob"))
        );
        assert_eq!(idle.updated_at, Some(now()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_lifetime_limit_can_be_lifted_with_none() {
        let db = test_db().await;
        let limited = AuthConfig {
            max_key_lifetime: Some(Duration::from_secs(86_400)),
            ..AuthConfig::default()
        };

        set(
            &db.pool,
            SettingKey::MaxKeyLifetime,
            "none",
            &by("alice"),
            now(),
        )
        .await
        .unwrap();

        assert_eq!(
            load(&db.pool, &limited).await.unwrap().max_key_lifetime,
            None
        );
        let rows = list(&db.pool, &limited).await.unwrap();
        let lifetime = rows
            .iter()
            .find(|r| r.key == SettingKey::MaxKeyLifetime)
            .unwrap();
        assert_eq!(lifetime.value, "none");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bad_value_is_refused_and_changes_nothing() {
        let db = test_db().await;
        let start = admin_generation(&db.pool).await.unwrap();

        let refused = [
            set(
                &db.pool,
                SettingKey::MaxKeysPerUser,
                "1001",
                &by("alice"),
                now(),
            )
            .await,
            set(
                &db.pool,
                SettingKey::MaxKeysPerUser,
                "-1",
                &by("alice"),
                now(),
            )
            .await,
            set(
                &db.pool,
                SettingKey::PasswordSignIn,
                "maybe",
                &by("alice"),
                now(),
            )
            .await,
            set(
                &db.pool,
                SettingKey::ResetLinkTtl,
                "0s",
                &by("alice"),
                now(),
            )
            .await,
            set(
                &db.pool,
                SettingKey::SetupLinkTtl,
                "soon",
                &by("alice"),
                now(),
            )
            .await,
            set(
                &db.pool,
                SettingKey::ConsoleSessionAbsolute,
                "none",
                &by("alice"),
                now(),
            )
            .await,
            // Fits chrono's `TimeDelta` but not a `DateTime` once added.
            set(
                &db.pool,
                SettingKey::ConsoleSessionAbsolute,
                "1000000years",
                &by("alice"),
                now(),
            )
            .await,
        ];

        for result in refused {
            assert!(
                matches!(
                    result,
                    Err(OkfError::LimitExceeded {
                        limit: "setting",
                        ..
                    })
                ),
                "{result:?}"
            );
        }
        assert_eq!(admin_generation(&db.pool).await.unwrap(), start);
        assert!(list(&db.pool, &defaults())
            .await
            .unwrap()
            .iter()
            .all(|r| !r.overridden));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reset_returns_to_the_default_and_bumps_only_when_there_was_an_override() {
        let db = test_db().await;
        set(
            &db.pool,
            SettingKey::MaxKeysPerUser,
            "3",
            &by("alice"),
            now(),
        )
        .await
        .unwrap();
        let before = admin_generation(&db.pool).await.unwrap();

        reset(&db.pool, SettingKey::MaxKeysPerUser, &by("alice"), now())
            .await
            .unwrap();
        reset(&db.pool, SettingKey::MaxKeysPerUser, &by("alice"), now())
            .await
            .unwrap();

        assert_eq!(
            load(&db.pool, &defaults()).await.unwrap().max_keys_per_user,
            7
        );
        assert_eq!(admin_generation(&db.pool).await.unwrap(), before + 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stored_lifetime_too_long_for_a_timestamp_falls_back_and_sessions_still_start() {
        let db = test_db().await;
        // Written before the ten-year cap existed, or by hand.
        sqlx::query(
            "INSERT INTO server_settings (key, value, updated_at, updated_by) \
             VALUES ('console_session_absolute', '\"1000000years\"', now(), 'hand')",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        let alice = crate::store::users::tests::seed_user(&db.pool, "alice", None).await;

        let settings = load(&db.pool, &defaults()).await.unwrap();
        let session = crate::store::user_sessions::create(
            &db.pool,
            &alice.id,
            None,
            "",
            settings.console_session_absolute,
            now(),
        )
        .await;

        assert_eq!(
            settings.console_session_absolute,
            crate::config::DEFAULT_CONSOLE_SESSION_ABSOLUTE
        );
        assert!(session.is_ok(), "{session:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn product_name_and_support_contact_are_validated_and_listed() {
        let db = test_db().await;
        let defaults = AuthConfig::default();
        let start = admin_generation(&db.pool).await.unwrap();
        let set_text = |key, value: String| {
            let pool = db.pool.clone();
            async move { set(&pool, key, &value, &by("cli"), now()).await }
        };

        let mut refused = Vec::new();
        for (key, value) in [
            (SettingKey::ProductName, "  ".to_string()),
            (SettingKey::ProductName, "x".repeat(61)),
            (SettingKey::ProductName, "Acme\u{7}".to_string()),
            (SettingKey::ProductName, "Acme\0Notes".to_string()),
            (SettingKey::SupportContact, "http://example.com".to_string()),
            (SettingKey::SupportContact, "https://".to_string()),
            (
                SettingKey::SupportContact,
                "https://exa\tmple.com".to_string(),
            ),
            (
                SettingKey::SupportContact,
                format!("https://example.com/{}", "x".repeat(500)),
            ),
            (SettingKey::SupportContact, "help@".to_string()),
            (
                SettingKey::SupportContact,
                "help desk@example.com".to_string(),
            ),
        ] {
            refused.push((value.clone(), set_text(key, value).await));
        }
        // Sixty characters, multibyte ones counted as one each.
        set_text(SettingKey::ProductName, "\u{e9}".repeat(60))
            .await
            .unwrap();
        set_text(SettingKey::ProductName, " Acme Notes ".to_string())
            .await
            .unwrap();
        set_text(
            SettingKey::SupportContact,
            "https://example.com/help".to_string(),
        )
        .await
        .unwrap();

        for (value, result) in refused {
            assert!(
                matches!(
                    result,
                    Err(OkfError::LimitExceeded {
                        limit: "setting",
                        ..
                    })
                ),
                "{value:?}: {result:?}"
            );
        }
        assert_eq!(admin_generation(&db.pool).await.unwrap(), start + 3);
        let loaded = load(&db.pool, &defaults).await.unwrap();
        assert_eq!(
            loaded.brand(),
            Brand {
                product_name: "Acme Notes".to_string(),
                support_contact: Some("https://example.com/help".to_string()),
                feedback_url: None,
                docs_url: None,
                billing_url: None,
            }
        );
        let mut conn = db.pool.acquire().await.unwrap();
        assert_eq!(
            brand_in(&mut conn, &defaults.default_brand())
                .await
                .unwrap(),
            loaded.brand()
        );
        let rows = list(&db.pool, &defaults).await.unwrap();
        let shown = |key| {
            rows.iter()
                .find(|row| row.key == key)
                .map(|row| row.value.clone())
        };
        assert_eq!(
            shown(SettingKey::ProductName).as_deref(),
            Some("Acme Notes")
        );
        assert_eq!(
            shown(SettingKey::SupportContact).as_deref(),
            Some("https://example.com/help")
        );

        set_text(SettingKey::SupportContact, "help@example.com".to_string())
            .await
            .unwrap();
        assert_eq!(
            brand_in(&mut conn, &defaults.default_brand())
                .await
                .unwrap()
                .support_contact
                .as_deref(),
            Some("help@example.com")
        );
        set_text(SettingKey::SupportContact, "none".to_string())
            .await
            .unwrap();
        assert_eq!(
            load(&db.pool, &defaults).await.unwrap().support_contact,
            None
        );
    }

    #[test]
    fn the_defaults_are_okf_and_no_contact() {
        let settings = Settings::from_config(&AuthConfig::default());

        assert_eq!(settings.product_name, "okf");
        assert_eq!(settings.support_contact, None);
        assert_eq!(settings.brand(), Brand::default());
        assert_eq!(display(&settings, SettingKey::SupportContact), "none");
    }

    /// An `[auth]` section that brands the server.
    fn branded() -> AuthConfig {
        AuthConfig {
            product_name: "  Acme Notes ".to_string(),
            support_contact: Some("help@example.com".to_string()),
            feedback_url: Some("https://example.com/feedback".to_string()),
            docs_url: Some("https://docs.example.com".to_string()),
            billing_url: Some("https://billing.example.com/login".to_string()),
            onboarding_url: Some("https://docs.example.com/start/".to_string()),
            ..AuthConfig::default()
        }
    }

    #[test]
    fn the_auth_branding_defaults_are_used_trimmed() {
        let settings = Settings::from_config(&branded());

        assert_eq!(
            settings.brand(),
            Brand {
                product_name: "Acme Notes".to_string(),
                support_contact: Some("help@example.com".to_string()),
                feedback_url: Some("https://example.com/feedback".to_string()),
                docs_url: Some("https://docs.example.com".to_string()),
                billing_url: Some("https://billing.example.com/login".to_string()),
            }
        );
        assert_eq!(
            settings.onboarding_url.as_deref(),
            Some("https://docs.example.com/start/")
        );
    }

    #[test]
    fn branding_defaults_the_settings_would_refuse_are_refused() {
        assert_eq!(check_branding_defaults(&AuthConfig::default()), Ok(()));
        assert_eq!(check_branding_defaults(&branded()), Ok(()));
        let refused = [
            AuthConfig {
                product_name: "  ".to_string(),
                ..AuthConfig::default()
            },
            AuthConfig {
                product_name: "Acme\u{7}".to_string(),
                ..AuthConfig::default()
            },
            AuthConfig {
                support_contact: Some("http://example.com".to_string()),
                ..AuthConfig::default()
            },
            AuthConfig {
                feedback_url: Some("help@example.com".to_string()),
                ..AuthConfig::default()
            },
            AuthConfig {
                docs_url: Some("http://docs.example.com".to_string()),
                ..AuthConfig::default()
            },
            AuthConfig {
                billing_url: Some("http://billing.example.com".to_string()),
                ..AuthConfig::default()
            },
            AuthConfig {
                onboarding_url: Some("docs.example.com".to_string()),
                ..AuthConfig::default()
            },
        ];
        for defaults in refused {
            assert!(check_branding_defaults(&defaults).is_err(), "{defaults:?}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stored_brand_overrides_the_auth_defaults_until_reset() {
        let db = test_db().await;
        let defaults = branded();
        let mut conn = db.pool.acquire().await.unwrap();
        assert_eq!(
            brand_in(&mut conn, &defaults.default_brand())
                .await
                .unwrap(),
            Settings::from_config(&defaults).brand()
        );

        set(
            &db.pool,
            SettingKey::ProductName,
            "Other",
            &AuditActor::cli(),
            Utc::now(),
        )
        .await
        .unwrap();
        let overridden = brand_in(&mut conn, &defaults.default_brand())
            .await
            .unwrap();
        assert_eq!(overridden.product_name, "Other");
        assert_eq!(
            overridden.support_contact.as_deref(),
            Some("help@example.com")
        );

        reset(
            &db.pool,
            SettingKey::ProductName,
            &AuditActor::cli(),
            Utc::now(),
        )
        .await
        .unwrap();
        assert_eq!(
            load(&db.pool, &defaults).await.unwrap().product_name,
            "Acme Notes"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unreadable_stored_brand_falls_back_to_the_default() {
        let db = test_db().await;
        // A hand edit: a control character and a plain-http contact.
        sqlx::query(
            "INSERT INTO server_settings (key, value, updated_at, updated_by) VALUES \
             ('product_name', '\"Acme\\u0007\"', now(), 'hand'), \
             ('support_contact', '\"http://example.com\"', now(), 'hand')",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        let mut conn = db.pool.acquire().await.unwrap();

        assert_eq!(
            brand_in(&mut conn, &Brand::default()).await.unwrap(),
            Brand::default()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unreadable_and_unknown_rows_fall_back_to_the_defaults() {
        let db = test_db().await;
        sqlx::query(
            "INSERT INTO server_settings (key, value, updated_at, updated_by) VALUES \
             ('max_keys_per_user', '\"lots\"', now(), 'hand'), \
             ('console_session_idle', '\"0s\"', now(), 'hand'), \
             ('password_sign_in', '1', now(), 'hand'), \
             ('a_later_setting', 'true', now(), 'newer-server')",
        )
        .execute(&db.pool)
        .await
        .unwrap();

        let settings = load(&db.pool, &defaults()).await.unwrap();
        let rows = list(&db.pool, &defaults()).await.unwrap();

        assert_eq!(settings, Settings::from_config(&defaults()));
        assert_eq!(rows.len(), SettingKey::ALL.len());
        assert_eq!(
            rows.iter().map(|r| r.key).collect::<Vec<_>>(),
            SettingKey::ALL
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn audit_retention_is_at_least_30_days_and_defaults_to_400() {
        let db = test_db().await;
        let start = admin_generation(&db.pool).await.unwrap();

        let refused = [
            set(
                &db.pool,
                SettingKey::AuditRetentionDays,
                "29",
                &by("alice"),
                now(),
            )
            .await,
            set(
                &db.pool,
                SettingKey::AuditRetentionDays,
                "3651",
                &by("alice"),
                now(),
            )
            .await,
            set(
                &db.pool,
                SettingKey::AuditRetentionDays,
                "-1",
                &by("alice"),
                now(),
            )
            .await,
            set(
                &db.pool,
                SettingKey::AuditRetentionDays,
                "90d",
                &by("alice"),
                now(),
            )
            .await,
        ];
        let default = load(&db.pool, &defaults())
            .await
            .unwrap()
            .audit_retention_days;
        set(
            &db.pool,
            SettingKey::AuditRetentionDays,
            "30",
            &by("alice"),
            now(),
        )
        .await
        .unwrap();
        set(
            &db.pool,
            SettingKey::AuditRetentionDays,
            "3650",
            &by("alice"),
            now(),
        )
        .await
        .unwrap();
        set(
            &db.pool,
            SettingKey::AuditRetentionDays,
            "90",
            &by("alice"),
            now(),
        )
        .await
        .unwrap();

        for result in refused {
            assert!(
                matches!(
                    result,
                    Err(OkfError::LimitExceeded {
                        limit: "setting",
                        ..
                    })
                ),
                "{result:?}"
            );
        }
        assert_eq!(default, 400);
        assert_eq!(
            Settings::from_config(&AuthConfig::default()).audit_retention_days,
            audit::DEFAULT_RETENTION_DAYS
        );
        assert_eq!(
            load(&db.pool, &defaults())
                .await
                .unwrap()
                .audit_retention_days,
            90
        );
        assert_eq!(admin_generation(&db.pool).await.unwrap(), start + 3);
        // The append-only trigger reads the row as a JSON number.
        let stored: Value = sqlx::query_scalar(
            "SELECT value FROM server_settings WHERE key = 'audit_retention_days'",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(stored, serde_json::json!(90));
        let rows = list(&db.pool, &defaults()).await.unwrap();
        let shown = rows
            .iter()
            .find(|row| row.key == SettingKey::AuditRetentionDays)
            .unwrap();
        assert_eq!(shown.value, "90");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_unlock_settings_default_off_four_hours_and_on_and_are_recorded() {
        let db = test_db().await;
        let defaults_now = load(&db.pool, &defaults()).await.unwrap();
        let refused = [
            (SettingKey::UnlockMaxHours, "0"),
            (SettingKey::UnlockMaxHours, "73"),
            (SettingKey::UnlockMaxHours, "-1"),
            (SettingKey::UnlockMaxHours, "4h"),
            (SettingKey::RequireUnlock, "yes"),
            (SettingKey::UnlockNotifyOwners, "1"),
        ];
        let mut refusals = Vec::with_capacity(refused.len());
        for (key, value) in refused {
            refusals.push(set(&db.pool, key, value, &by("alice"), now()).await);
        }

        for (key, value) in [
            (SettingKey::RequireUnlock, "on"),
            (SettingKey::UnlockMaxHours, "1"),
            (SettingKey::UnlockMaxHours, "72"),
            (SettingKey::UnlockNotifyOwners, "off"),
        ] {
            set(&db.pool, key, value, &by("alice"), now())
                .await
                .unwrap();
        }
        let loaded = load(&db.pool, &defaults()).await.unwrap();
        let rows = list(&db.pool, &defaults()).await.unwrap();
        reset(&db.pool, SettingKey::UnlockMaxHours, &by("alice"), now())
            .await
            .unwrap();

        assert_eq!(
            (
                defaults_now.require_unlock,
                defaults_now.unlock_max_hours,
                defaults_now.unlock_notify_owners
            ),
            (false, 4, true)
        );
        for result in refusals {
            assert!(
                matches!(
                    result,
                    Err(OkfError::LimitExceeded {
                        limit: "setting",
                        ..
                    })
                ),
                "{result:?}"
            );
        }
        assert_eq!(
            (
                loaded.require_unlock,
                loaded.unlock_max_hours,
                loaded.unlock_notify_owners
            ),
            (true, 72, false)
        );
        let shown: Vec<(&str, &str)> = rows
            .iter()
            .filter(|row| row.key.as_str().contains("unlock"))
            .map(|row| (row.key.as_str(), row.value.as_str()))
            .collect();
        assert_eq!(
            shown,
            [
                ("require_unlock", "true"),
                ("unlock_max_hours", "72"),
                ("unlock_notify_owners", "false")
            ]
        );
        // None is sensitive: the entries hold the values themselves.
        let changed = audit::testing::entries(&db.pool, "setting.changed").await;
        let details: Vec<(Option<&str>, &Value)> = changed
            .iter()
            .map(|entry| (entry.target.as_deref(), &entry.details))
            .collect();
        assert_eq!(
            details,
            [
                (
                    Some("require_unlock"),
                    &serde_json::json!({"from": null, "to": true})
                ),
                (
                    Some("unlock_max_hours"),
                    &serde_json::json!({"from": null, "to": 1})
                ),
                (
                    Some("unlock_max_hours"),
                    &serde_json::json!({"from": 1, "to": 72})
                ),
                (
                    Some("unlock_notify_owners"),
                    &serde_json::json!({"from": null, "to": false})
                ),
            ]
        );
        let cleared = audit::testing::entries(&db.pool, "setting.cleared").await;
        assert_eq!(cleared[0].details, serde_json::json!({"from": 72}));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unreadable_stored_retention_falls_back_to_the_default() {
        let db = test_db().await;
        sqlx::query(
            "INSERT INTO server_settings (key, value, updated_at, updated_by) \
             VALUES ('audit_retention_days', '10', now(), 'hand')",
        )
        .execute(&db.pool)
        .await
        .unwrap();

        let settings = load(&db.pool, &defaults()).await.unwrap();

        assert_eq!(settings.audit_retention_days, audit::DEFAULT_RETENTION_DAYS);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_and_reset_record_one_entry_each_and_nothing_without_a_change() {
        let db = test_db().await;
        set(
            &db.pool,
            SettingKey::MaxKeysPerUser,
            "3",
            &by("alice"),
            now(),
        )
        .await
        .unwrap();
        let before_same = admin_generation(&db.pool).await.unwrap();
        // The same value again, spelt the same once stored: no change.
        set(
            &db.pool,
            SettingKey::MaxKeysPerUser,
            " 3 ",
            &by("bob"),
            now(),
        )
        .await
        .unwrap();
        let after_same = admin_generation(&db.pool).await.unwrap();
        set(
            &db.pool,
            SettingKey::MaxKeysPerUser,
            "4",
            &by("alice"),
            now(),
        )
        .await
        .unwrap();
        reset(&db.pool, SettingKey::MaxKeysPerUser, &by("alice"), now())
            .await
            .unwrap();
        // No override left: nothing to clear or record.
        reset(&db.pool, SettingKey::MaxKeysPerUser, &by("alice"), now())
            .await
            .unwrap();

        let changed = audit::testing::entries(&db.pool, "setting.changed").await;
        let cleared = audit::testing::entries(&db.pool, "setting.cleared").await;
        assert_eq!(after_same, before_same, "an unchanged value bumps nothing");
        assert_eq!(changed.len(), 2, "{changed:?}");
        assert_eq!(
            changed[0].details,
            serde_json::json!({"from": null, "to": 3})
        );
        assert_eq!(changed[1].details, serde_json::json!({"from": 3, "to": 4}));
        assert_eq!(
            (
                changed[0].target_kind.as_str(),
                changed[0].target.as_deref()
            ),
            ("setting", Some("max_keys_per_user"))
        );
        assert_eq!(
            changed[0].actor_label, "api key",
            "never the key's own label"
        );
        assert!(changed[0].teams.is_empty(), "settings belong to no team");
        assert_eq!(changed[0].person, None);
        assert_eq!(cleared.len(), 1, "{cleared:?}");
        assert_eq!(cleared[0].details, serde_json::json!({"from": 4}));
        assert_eq!(cleared[0].target.as_deref(), Some("max_keys_per_user"));
        let rows = list(&db.pool, &defaults()).await.unwrap();
        assert!(rows.iter().all(|row| !row.overridden));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_support_contact_entry_never_holds_the_contact() {
        let db = test_db().await;
        let contact = |value: &'static str| {
            let pool = db.pool.clone();
            async move {
                set(
                    &pool,
                    SettingKey::SupportContact,
                    value,
                    &by("alice"),
                    now(),
                )
                .await
                .unwrap();
            }
        };

        contact("help@example.com").await;
        contact("none").await;
        contact("https://example.com/help").await;
        reset(&db.pool, SettingKey::SupportContact, &by("alice"), now())
            .await
            .unwrap();

        let changed = audit::testing::entries(&db.pool, "setting.changed").await;
        let cleared = audit::testing::entries(&db.pool, "setting.cleared").await;
        assert_eq!(
            changed
                .iter()
                .map(|e| e.details.clone())
                .collect::<Vec<_>>(),
            [
                serde_json::json!({"set": true}),
                serde_json::json!({"set": false}),
                serde_json::json!({"set": true}),
            ]
        );
        assert_eq!(cleared.len(), 1);
        assert_eq!(cleared[0].details, serde_json::json!({}));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn feedback_url_is_validated_shown_in_the_brand_and_cleared_by_none() {
        let db = test_db().await;
        let defaults = AuthConfig::default();
        let set_text = |value: String| {
            let pool = db.pool.clone();
            async move { set(&pool, SettingKey::FeedbackUrl, &value, &by("cli"), now()).await }
        };
        assert_eq!(
            display(&Settings::from_config(&defaults), SettingKey::FeedbackUrl),
            "none"
        );

        for value in [
            "http://example.com/feedback".to_string(),
            "help@example.com".to_string(),
            "https://".to_string(),
            "https://example.com/a b".to_string(),
            "https://exa\tmple.com".to_string(),
            format!("https://example.com/{}", "x".repeat(500)),
        ] {
            let result = set_text(value.clone()).await;
            assert!(
                matches!(
                    result,
                    Err(OkfError::LimitExceeded {
                        limit: "setting",
                        ..
                    })
                ),
                "{value:?}: {result:?}"
            );
        }
        assert_eq!(
            load(&db.pool, &defaults).await.unwrap().brand(),
            Brand::default()
        );

        set_text("https://example.com/feedback".to_string())
            .await
            .unwrap();
        let loaded = load(&db.pool, &defaults).await.unwrap();
        let mut conn = db.pool.acquire().await.unwrap();
        assert_eq!(
            loaded.brand().feedback_url.as_deref(),
            Some("https://example.com/feedback")
        );
        assert_eq!(
            brand_in(&mut conn, &defaults.default_brand())
                .await
                .unwrap(),
            loaded.brand()
        );
        assert_eq!(
            display(&loaded, SettingKey::FeedbackUrl),
            "https://example.com/feedback"
        );

        set_text("none".to_string()).await.unwrap();
        assert_eq!(load(&db.pool, &defaults).await.unwrap().feedback_url, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn docs_and_onboarding_urls_are_validated_stored_and_cleared_by_none() {
        let db = test_db().await;
        let defaults = AuthConfig::default();
        let settings = Settings::from_config(&defaults);
        assert_eq!(display(&settings, SettingKey::DocsUrl), "none");
        assert_eq!(display(&settings, SettingKey::OnboardingUrl), "none");

        for key in [SettingKey::DocsUrl, SettingKey::OnboardingUrl] {
            for value in [
                "http://docs.example.com",
                "/ui/",
                "//docs.example.com",
                "javascript:alert(1)",
                "https://",
                "https://docs.example.com/a b",
            ] {
                let result = set(&db.pool, key, value, &by("cli"), now()).await;
                assert!(
                    matches!(
                        &result,
                        Err(OkfError::LimitExceeded { limit: "setting", reason })
                            if reason.starts_with(key.as_str())
                    ),
                    "{key:?} {value:?}: {result:?}"
                );
            }
        }
        set(
            &db.pool,
            SettingKey::DocsUrl,
            "https://docs.example.com",
            &by("cli"),
            now(),
        )
        .await
        .unwrap();
        set(
            &db.pool,
            SettingKey::OnboardingUrl,
            "https://docs.example.com/start/",
            &by("cli"),
            now(),
        )
        .await
        .unwrap();
        let loaded = load(&db.pool, &defaults).await.unwrap();
        assert_eq!(
            loaded.brand().docs_url.as_deref(),
            Some("https://docs.example.com")
        );
        assert_eq!(
            loaded.onboarding_url.as_deref(),
            Some("https://docs.example.com/start/")
        );
        // A queued mail's brand reads the documentation URL too.
        let mut conn = db.pool.acquire().await.unwrap();
        assert_eq!(
            brand_in(&mut conn, &defaults.default_brand())
                .await
                .unwrap(),
            loaded.brand()
        );
        drop(conn);

        for key in [SettingKey::DocsUrl, SettingKey::OnboardingUrl] {
            set(&db.pool, key, "none", &by("cli"), now()).await.unwrap();
        }
        let cleared = load(&db.pool, &defaults).await.unwrap();
        assert_eq!((cleared.docs_url, cleared.onboarding_url), (None, None));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_billing_url_is_validated_stored_cleared_by_none_and_reset() {
        let db = test_db().await;
        let defaults = AuthConfig::default();
        assert_eq!(
            display(&Settings::from_config(&defaults), SettingKey::BillingUrl),
            "none"
        );

        for value in [
            "http://billing.example.com",
            "/ui/",
            "//billing.example.com",
            "javascript:alert(1)",
            "billing@example.com",
            "https://billing.example.com/a b",
        ] {
            let result = set(&db.pool, SettingKey::BillingUrl, value, &by("cli"), now()).await;
            assert!(
                matches!(
                    &result,
                    Err(OkfError::LimitExceeded { limit: "setting", reason })
                        if reason.starts_with("billing_url")
                ),
                "{value:?}: {result:?}"
            );
        }
        set(
            &db.pool,
            SettingKey::BillingUrl,
            "https://billing.example.com/login",
            &by("cli"),
            now(),
        )
        .await
        .unwrap();
        let loaded = load(&db.pool, &defaults).await.unwrap();
        assert_eq!(
            loaded.brand().billing_url.as_deref(),
            Some("https://billing.example.com/login")
        );
        // A queued mail's brand reads it too.
        let mut conn = db.pool.acquire().await.unwrap();
        assert_eq!(
            brand_in(&mut conn, &defaults.default_brand())
                .await
                .unwrap(),
            loaded.brand()
        );
        drop(conn);

        set(&db.pool, SettingKey::BillingUrl, "none", &by("cli"), now())
            .await
            .unwrap();
        assert_eq!(load(&db.pool, &defaults).await.unwrap().billing_url, None);

        // A stored override beats the config default until reset.
        let configured = AuthConfig {
            billing_url: Some("https://billing.example.com/default".to_string()),
            ..AuthConfig::default()
        };
        assert_eq!(
            load(&db.pool, &configured).await.unwrap().billing_url,
            None,
            "an explicit none overrides the config default"
        );
        reset(&db.pool, SettingKey::BillingUrl, &by("cli"), now())
            .await
            .unwrap();
        assert_eq!(
            load(&db.pool, &configured)
                .await
                .unwrap()
                .billing_url
                .as_deref(),
            Some("https://billing.example.com/default")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_feedback_url_entry_records_the_url() {
        let db = test_db().await;
        set(
            &db.pool,
            SettingKey::FeedbackUrl,
            "https://example.com/feedback",
            &by("alice"),
            now(),
        )
        .await
        .unwrap();

        let changed = audit::testing::entries(&db.pool, "setting.changed").await;

        assert_eq!(changed.len(), 1, "{changed:?}");
        assert_eq!(
            changed[0].details,
            serde_json::json!({"from": null, "to": "https://example.com/feedback"})
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_audit_write_fails_the_change() {
        let db = test_db().await;
        set(
            &db.pool,
            SettingKey::MaxKeysPerUser,
            "3",
            &by("alice"),
            now(),
        )
        .await
        .unwrap();
        let start = admin_generation(&db.pool).await.unwrap();
        crate::http::api::testing::fail_on(&db.pool, "BEFORE INSERT ON audit_entries FOR EACH ROW")
            .await;

        let set_refused = set(
            &db.pool,
            SettingKey::MaxKeysPerUser,
            "4",
            &by("alice"),
            now(),
        )
        .await;
        let reset_refused = reset(&db.pool, SettingKey::MaxKeysPerUser, &by("alice"), now()).await;

        assert!(set_refused.is_err());
        assert!(reset_refused.is_err());
        assert_eq!(
            load(&db.pool, &defaults()).await.unwrap().max_keys_per_user,
            3,
            "both changes rolled back"
        );
        assert_eq!(admin_generation(&db.pool).await.unwrap(), start);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_command_line_is_listed_and_recorded_as_its_os_user() {
        let db = test_db().await;
        let cli = AuditActor::cli();

        set(&db.pool, SettingKey::MaxKeysPerUser, "3", &cli, now())
            .await
            .unwrap();

        let rows = list(&db.pool, &defaults()).await.unwrap();
        let row = rows
            .iter()
            .find(|row| row.key == SettingKey::MaxKeysPerUser)
            .unwrap();
        let changed = audit::testing::entries(&db.pool, "setting.changed").await;
        let expected = format!("cli:{}", cli.label);
        assert_eq!(row.updated_by.as_deref(), Some(expected.as_str()));
        assert_eq!(
            (changed[0].source.as_str(), changed[0].actor_label.as_str()),
            ("cli", expected.as_str())
        );
    }

    /// Runs `set` to 4 and `reset` of `max_keys_per_user` at once, with an
    /// override of 3 stored, and answers both results. Both are queued on a
    /// held `server_state` row (`set` first, then `reset`), and the hold is
    /// released only once both wait. Postgres does not promise to wake
    /// row-lock waiters in the order they queued: the holder's `UPDATE`
    /// leaves a new row version, and both waiters race for it, so either
    /// may win. Taken in the old order (`reset` deleting before it bumps),
    /// a `set` that won the generation row would wait on the row `reset`
    /// deleted, while `reset` waited on the generation row: a deadlock
    /// Postgres aborts.
    async fn set_and_reset_queued_on_the_generation_row(
        pool: &sqlx::PgPool,
    ) -> (Result<(), OkfError>, Result<(), OkfError>) {
        use crate::http::api::testing::wait_for_lock_waiters;
        use std::time::Duration;
        set(pool, SettingKey::MaxKeysPerUser, "3", &by("alice"), now())
            .await
            .unwrap();
        let mut holder = pool.begin().await.unwrap();
        sqlx::query("UPDATE server_state SET admin_generation = admin_generation")
            .execute(&mut *holder)
            .await
            .unwrap();
        // `tokio::spawn` runs each call on its own task (and connection).
        let setter = tokio::spawn({
            let pool = pool.clone();
            async move { set(&pool, SettingKey::MaxKeysPerUser, "4", &by("alice"), now()).await }
        });
        wait_for_lock_waiters(pool, 1).await;
        let resetter = tokio::spawn({
            let pool = pool.clone();
            async move { reset(&pool, SettingKey::MaxKeysPerUser, &by("bob"), now()).await }
        });
        wait_for_lock_waiters(pool, 2).await;
        holder.commit().await.unwrap();
        let wait = Duration::from_secs(30);
        let set_result = tokio::time::timeout(wait, setter)
            .await
            .expect("set finished within 30 s")
            .expect("the set task did not panic");
        let reset_result = tokio::time::timeout(wait, resetter)
            .await
            .expect("reset finished within 30 s")
            .expect("the reset task did not panic");
        (set_result, reset_result)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_concurrent_set_and_reset_of_one_key_both_succeed_in_either_order() {
        let db = test_db().await;

        let (set_result, reset_result) = set_and_reset_queued_on_the_generation_row(&db.pool).await;

        assert!(set_result.is_ok(), "{set_result:?}");
        assert!(reset_result.is_ok(), "{reset_result:?}");
        // Every settings entry in id order: ids are taken under the
        // generation row's lock, so this is the order the writes ran in.
        let history: Vec<(String, Value)> = sqlx::query_as(
            "SELECT action, details FROM audit_entries \
             WHERE action IN ('setting.changed', 'setting.cleared') ORDER BY id",
        )
        .fetch_all(&db.pool)
        .await
        .unwrap();
        let stored: Option<Value> =
            sqlx::query_scalar("SELECT value FROM server_settings WHERE key = $1")
                .bind(SettingKey::MaxKeysPerUser.as_str())
                .fetch_optional(&db.pool)
                .await
                .unwrap();
        // The waiters may wake in either order (see the helper), so the
        // stored state says which ran last, and the entries must be that
        // serial history: each `from` is the value stored when it ran.
        let entry = |action: &str, details: Value| (action.to_string(), details);
        let stored_three = entry(
            "setting.changed",
            serde_json::json!({"from": null, "to": 3}),
        );
        let (expected, order) = match stored {
            None => (
                vec![
                    stored_three,
                    entry("setting.changed", serde_json::json!({"from": 3, "to": 4})),
                    entry("setting.cleared", serde_json::json!({"from": 4})),
                ],
                "set ran first, and reset cleared what set stored",
            ),
            Some(value) => {
                assert_eq!(value, serde_json::json!(4), "only set's 4 can be left");
                (
                    vec![
                        stored_three,
                        entry("setting.cleared", serde_json::json!({"from": 3})),
                        entry(
                            "setting.changed",
                            serde_json::json!({"from": null, "to": 4}),
                        ),
                    ],
                    "reset ran first, and set found no override",
                )
            }
        };
        assert_eq!(history, expected, "{order}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_set_queued_behind_a_new_override_records_it_as_from() {
        use crate::http::api::testing::wait_for_lock_waiters;
        let db = test_db().await;
        // A held generation row whose transaction also inserts an override
        // of 3, as a `set` that won the race would. Only a read taken after
        // the generation lock sees it: a read before it, even `FOR UPDATE`,
        // can neither see nor wait on a row inserted by an uncommitted
        // transaction, and would record `from: null`.
        let mut holder = db.pool.begin().await.unwrap();
        invalidate(&mut holder, &[Invalidation::Settings])
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO server_settings (key, value, updated_at, updated_by) \
             VALUES ($1, $2, $3, 'carol')",
        )
        .bind(SettingKey::MaxKeysPerUser.as_str())
        .bind(serde_json::json!(3))
        .bind(now())
        .execute(&mut *holder)
        .await
        .unwrap();
        let setter = tokio::spawn({
            let pool = db.pool.clone();
            async move { set(&pool, SettingKey::MaxKeysPerUser, "4", &by("alice"), now()).await }
        });
        wait_for_lock_waiters(&db.pool, 1).await;

        holder.commit().await.unwrap();
        let set_result = tokio::time::timeout(std::time::Duration::from_secs(30), setter)
            .await
            .expect("set finished within 30 s")
            .expect("the set task did not panic");

        assert!(set_result.is_ok(), "{set_result:?}");
        let changed = audit::testing::entries(&db.pool, "setting.changed").await;
        assert_eq!(
            changed
                .iter()
                .map(|e| e.details.clone())
                .collect::<Vec<_>>(),
            [serde_json::json!({"from": 3, "to": 4})],
            "set read the override after it waited, and found the 3 stored meanwhile"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn many_racing_sets_and_resets_of_one_key_all_succeed() {
        let db = test_db().await;

        let (alice, bob) = (by("alice"), by("bob"));
        for round in 0..20 {
            let value = format!("{}", round + 1);
            let wait = std::time::Duration::from_secs(30);
            let (set_result, reset_result) = tokio::time::timeout(wait, async {
                tokio::join!(
                    set(&db.pool, SettingKey::MaxKeysPerUser, &value, &alice, now()),
                    reset(&db.pool, SettingKey::MaxKeysPerUser, &bob, now()),
                )
            })
            .await
            .expect("both finished within 30 s");

            assert!(set_result.is_ok(), "round {round}: {set_result:?}");
            assert!(reset_result.is_ok(), "round {round}: {reset_result:?}");
        }
        let changed = audit::testing::entries(&db.pool, "setting.changed").await;
        let cleared = audit::testing::entries(&db.pool, "setting.cleared").await;
        // Each clear removed exactly the value a set stored just before it.
        for entry in &cleared {
            assert!(
                changed
                    .iter()
                    .any(|c| c.details["to"] == entry.details["from"]),
                "{entry:?}"
            );
        }
        assert_eq!(changed.len(), 20, "every round's set changed the value");
    }

    /// Which settings write [`a_settings_write_and_a_promotion_queue_on_the_admin_row`] races.
    #[derive(Debug, Clone, Copy)]
    enum Write {
        Set,
        Reset,
    }

    /// Races a settings write by admin `boss`'s console session against
    /// `users::set_admin` promoting `alice`, with the interleaving forced:
    /// a held session row of `alice` stops the promotion after it has
    /// locked every admin row (`boss`'s included) and before it bumps the
    /// generation; the settings write is started once the promotion waits,
    /// and the hold released once both wait. Locking the generation row
    /// before the actor's user row (as first shipped), the write would hold
    /// `server_state` and wait on `boss`, the promotion hold `boss` and wait
    /// on `server_state`: a deadlock Postgres aborts.
    ///
    /// # Returns
    /// The settings write's result and the promotion's.
    async fn a_settings_write_and_a_promotion_queue_on_the_admin_row(
        pool: &sqlx::PgPool,
        write: Write,
    ) -> (Result<(), OkfError>, Result<bool, OkfError>) {
        use crate::http::api::testing::wait_for_lock_waiters;
        use crate::store::users::{self, tests::seed_user};
        use std::time::Duration;
        let boss = seed_user(pool, "boss", None).await;
        let alice = seed_user(pool, "alice", None).await;
        let on = users::AdminChange {
            on: true,
            allow_last: false,
        };
        users::set_admin(pool, "boss", on, &AuditActor::cli(), now())
            .await
            .unwrap();
        set(
            pool,
            SettingKey::MaxKeysPerUser,
            "3",
            &AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();
        crate::store::user_sessions::create(
            pool,
            &alice.id,
            None,
            "",
            Duration::from_secs(3600),
            now(),
        )
        .await
        .unwrap();
        let session = AuditActor {
            source: audit::Source::Console,
            credential: None,
            user_id: Some(boss.id.clone()),
            label: "boss".to_string(),
            via: Via::Session,
            operator: true,
            provisioner: false,
            ip: None,
        };
        let mut holder = pool.begin().await.unwrap();
        sqlx::query("SELECT 1 FROM user_sessions WHERE user_id = $1 FOR UPDATE")
            .bind(&alice.id)
            .execute(&mut *holder)
            .await
            .unwrap();
        // `tokio::spawn` runs each on its own task (and connection).
        let promotion = tokio::spawn({
            let pool = pool.clone();
            async move { users::set_admin(&pool, "alice", on, &AuditActor::cli(), now()).await }
        });
        wait_for_lock_waiters(pool, 1).await;
        let settings_write = tokio::spawn({
            let pool = pool.clone();
            async move {
                match write {
                    Write::Set => {
                        set(&pool, SettingKey::MaxKeysPerUser, "4", &session, now()).await
                    }
                    Write::Reset => reset(&pool, SettingKey::MaxKeysPerUser, &session, now()).await,
                }
            }
        });
        wait_for_lock_waiters(pool, 2).await;
        holder.commit().await.unwrap();
        let wait = Duration::from_secs(30);
        let write_result = tokio::time::timeout(wait, settings_write)
            .await
            .expect("the settings write finished within 30 s")
            .expect("the settings task did not panic");
        let promotion_result = tokio::time::timeout(wait, promotion)
            .await
            .expect("the promotion finished within 30 s")
            .expect("the promotion task did not panic");
        (write_result, promotion_result)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_admins_setting_change_and_a_concurrent_promotion_both_succeed() {
        let db = test_db().await;

        let (written, promoted) =
            a_settings_write_and_a_promotion_queue_on_the_admin_row(&db.pool, Write::Set).await;

        assert!(written.is_ok(), "{written:?}");
        assert!(matches!(promoted, Ok(false)), "{promoted:?}");
        let changed = audit::testing::entries(&db.pool, "setting.changed").await;
        assert_eq!(
            changed.last().unwrap().actor_person.as_deref(),
            Some("boss")
        );
        assert_eq!(
            load(&db.pool, &defaults()).await.unwrap().max_keys_per_user,
            4
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_admins_setting_reset_and_a_concurrent_promotion_both_succeed() {
        let db = test_db().await;

        let (written, promoted) =
            a_settings_write_and_a_promotion_queue_on_the_admin_row(&db.pool, Write::Reset).await;

        assert!(written.is_ok(), "{written:?}");
        assert!(matches!(promoted, Ok(false)), "{promoted:?}");
        let cleared = audit::testing::entries(&db.pool, "setting.cleared").await;
        assert_eq!(cleared.len(), 1);
        assert_eq!(cleared[0].actor_person.as_deref(), Some("boss"));
    }
}
