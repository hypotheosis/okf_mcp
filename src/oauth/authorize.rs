//! `GET` and `POST /oauth/authorize` (spec "Endpoints" and "Consent flow").
//!
//! The GET validates the request in a fixed order. Until the client and its
//! exact redirect URI are known, a failure is an error page, never a
//! redirect (no open redirect). After that, failures go back to the client
//! as `error=...`. A valid request becomes a pending authorization (in
//! memory, or in Redis with `[redis]`, so the POSTs may reach any replica),
//! and the POSTs walk it through the sign-in code and consent. A Redis
//! failure fails closed: the busy page (503), never a fallback to memory.
//! Approve and Deny answer with a page that forwards the browser to the
//! client rather than a 303 (issue #435).
//!
//! Who approves (spec "OAuth consent as the user"): with `signin=code`, the
//! sign-in code path (step 1 and 2), whose grants stay admin-issued. Else a
//! signed-in person (the console session cookie) chooses catalogs within
//! their ceiling, and the approval is a self-issued grant. The page offers
//! one page of the ceiling at a time (`CONSENT_PAGE` catalogs), found by
//! a name prefix (Find) or continued (Next catalogs), and carries the
//! ticked catalogs over (at most `MAX_CARRIED`): it never reads or holds
//! the whole ceiling (issue #736). Without a
//! session, the GET sends the browser to `/signin` first, before anything
//! is stored.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::rejection::{FormRejection, QueryRejection};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Form, Router};
use serde::Deserialize;
use subtle::ConstantTimeEq;

use crate::app::App;
use crate::error::OkfError;
use crate::http::console_session::{cookie, signed_in_user, COOKIE};
use crate::http::signin::cross_site;
use crate::keys;
use crate::labels::directory::{Labels, Wanted};
use crate::labels::user_label;
use crate::maintenance::types::new_id;
use crate::oauth::clients;
use crate::oauth::consent::{self, GrantSummary, PageView, UserPage};
use crate::oauth::error::OAuthError;
use crate::oauth::state::{
    code_key, AcceptedCode, AuthorizationCode, PendingAuthorization, UserConsent,
    MAX_FAILED_ATTEMPTS,
};
use crate::oauth::{pkce, resource, OAuth};
use crate::store::ceiling::{self, Ceiling};
use crate::store::keys::Access;
use crate::store::oauth::clients::{ClientKind, ClientRecord};
use crate::store::oauth::codes::{self as code_rows, SignInCodeRecord};
use crate::store::oauth::grants::{self as grant_rows, NewSelfIssuedGrant};
use crate::store::settings::Brand;
use crate::store::sign_in_events::Outcome;
use crate::store::users::UserAuth;
use crate::telemetry::metrics::Limiter;
use crate::telemetry::subscriber::split_event;

/// The message for every unusable sign-in code, so the page never says
/// whether a code exists, expired, was redeemed or was revoked.
const BAD_CODE: &str = "That sign-in code is not valid. Check it, or ask for a new one.";
/// The message for an Approve from a step-2 page that a later code entry
/// (in another copy of the form) replaced.
const STALE: &str =
    "That approval page was out of date. Approve from the latest page, or enter the code again.";
/// The message for an unknown, expired or forged pending authorization.
const EXPIRED: &str = "This sign-in request has expired or is not valid.";
/// The message for a consent the page could not have produced (a catalog
/// or level it did not offer, or an unknown field).
const TAMPERED: &str =
    "That approval did not match what was offered. Start again from your client.";
/// The message for a consent posted after its user signed out or switched.
const SIGNED_OUT: &str =
    "You are no longer signed in as the person this page was for. Start again from your client.";
/// The message for a consent whose access shrank while the page was open.
const CHANGED: &str = "Your access changed while this page was open. Start again from your client.";
/// The message for a consent with no catalog ticked.
const NOTHING_CHOSEN: &str = "Choose at least one catalog, or Deny.";
/// The message for a default catalog that is not ticked.
const DEFAULT_NOT_CHOSEN: &str = "The default catalog must be one you allow.";
/// A form posted from another site (403).
const CROSS_SITE: &str = "This form can only be sent from this site. Start again from your client.";
/// The message for a Find or Next catalogs with too many catalogs ticked.
const TOO_MANY_CARRIED: &str =
    "You can carry at most 50 ticked catalogs to another search. Approve these first, or untick some.";
/// How many catalogs of the ceiling a signed-in consent page shows at once
/// (issue #736).
pub(crate) const CONSENT_PAGE: u32 = 50;
/// The most ticked catalogs a Find or Next catalogs carries over (named in
/// [`TOO_MANY_CARRIED`]), so a pending authorization holds at most
/// `CONSENT_PAGE + MAX_CARRIED` offered catalogs.
pub(crate) const MAX_CARRIED: usize = 50;
/// The busy page (503): a Redis failure or a database deadlock, or no room
/// for an authorization code.
const BUSY: &str = "The server is busy. Start again from your client in a minute.";

/// The routes: `GET` and `POST /oauth/authorize`.
///
/// # Returns
/// A router for [`crate::oauth::routes`] to merge.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "router builder: runs once at startup, outside any request, so a span would be an orphan root"
    )
)]
pub fn routes() -> Router<Arc<App>> {
    Router::new().route("/oauth/authorize", get(authorize).post(consent_post))
}

/// The authorization request's parameters. Every field is optional, so a
/// missing one is the handler's error, in the spec's order, not serde's.
#[derive(Debug, Default, Deserialize)]
struct AuthorizeQuery {
    response_type: Option<String>,
    client_id: Option<String>,
    redirect_uri: Option<String>,
    state: Option<String>,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
    resource: Option<String>,
    /// `code` asks for the sign-in code path (step 1) even with a session;
    /// the "Use a sign-in code instead" links set it.
    signin: Option<String>,
    // `scope` is accepted and ignored (serde skips unknown fields): the
    // sign-in code or the person's choice decides the grant.
}

/// The consent form's fields.
// `Debug` by hand: the pending id, the CSRF token, the sign-in code and the
// consent nonce never reach a log.
#[derive(Deserialize)]
struct ConsentForm {
    pending: String,
    csrf: String,
    action: String,
    sign_in_code: Option<String>,
    /// The step-2 page's token (see [`AcceptedCode`]), or the signed-in
    /// page's consent nonce (see [`UserConsent`]); absent on step 1.
    summary: Option<String>,
    /// The signed-in page's default catalog select (empty: none).
    default_catalog: Option<String>,
    /// The signed-in page's Find box: the name prefix to find catalogs by.
    find: Option<String>,
    /// Every other field: the signed-in page's `grant.<catalog>` checkboxes
    /// and `level.<catalog>` selects. `flatten` collects the fields not
    /// named above into this map, like `**kwargs`.
    #[serde(flatten)]
    choices: BTreeMap<String, String>,
}

impl std::fmt::Debug for ConsentForm {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "Display/Debug impl: runs while the subscriber formats an event"
        )
    )]
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConsentForm")
            .field("pending", &"<redacted>")
            .field("action", &self.action)
            .finish_non_exhaustive()
    }
}

/// What an Approve finds, decided in the consent POST's one atomic update.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Approval {
    /// No code has been accepted yet: back to step 1.
    NoCode,
    /// The form came from a step-2 page that a later code entry replaced.
    Stale,
    /// The form is the current summary's: redeem this code id.
    Current(String),
}

/// Decides an Approve against the accepted code, comparing the posted
/// summary token in constant time.
///
/// # Arguments
/// * `accepted` - the pending request's accepted code, if any.
/// * `posted` - the form's `summary` field, if any.
///
/// # Returns
/// [`Approval::Current`] only when `posted` is that code's summary token.
#[tracing::instrument(level = "trace", skip_all)]
fn approval(accepted: Option<&AcceptedCode>, posted: Option<&str>) -> Approval {
    let Some(accepted) = accepted else {
        return Approval::NoCode;
    };
    // `is_some_and`: `posted is not None and <predicate>`.
    let current = posted.is_some_and(|posted| {
        accepted
            .summary_nonce
            .as_bytes()
            .ct_eq(posted.as_bytes())
            .into()
    });
    if current {
        Approval::Current(accepted.code_id.clone())
    } else {
        Approval::Stale
    }
}

/// Spends a signed-in page's consent nonce: when `posted` is the current
/// nonce, it is replaced by `fresh`, so the same page (or a replay of its
/// form) cannot approve again. Runs inside the consent POST's one atomic
/// update ([`consent_change`]).
///
/// # Arguments
/// * `consent` - the pending request's consent, if it is on the user path.
/// * `posted` - the form's `summary` field, if any.
/// * `fresh` - an unseen random token to put in its place.
///
/// # Returns
/// Whether the nonce matched and was spent.
#[tracing::instrument(level = "trace", skip_all)]
fn claim_consent(consent: Option<&mut UserConsent>, posted: Option<&str>, fresh: String) -> bool {
    let Some(consent) = consent else {
        return false;
    };
    let current =
        posted.is_some_and(|posted| consent.nonce.as_bytes().ct_eq(posted.as_bytes()).into());
    if current {
        consent.nonce = fresh;
    }
    current
}

/// How an answer travels back to the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hop {
    /// A 303, for an answer to a GET: no form is involved.
    Redirect,
    /// [`consent::forward_page`], for an answer to the consent form's POST:
    /// a 303 there would put the client's own redirects under the page's
    /// `form-action` (issue #435).
    Forward,
}

/// Sends the browser to the client's redirect URI with `pairs`, `state`
/// and `iss` added, by `hop`.
///
/// A 303, never a 307: a 307 would make the browser re-POST the consent
/// form, sign-in code included, to the client.
///
/// # Arguments
/// * `brand` - the product name and support contact, for the pages.
/// * `redirect_uri` - the request's URI, matched against the client's
///   (see [`clients::redirect_uri_matches`]).
/// * `pairs` - `code`, or `error` and `error_description`.
/// * `(state, issuer)` - the client's `state`, echoed if it sent one, and
///   this server's issuer (RFC 9207).
/// * `hop` - a 303 or the forwarding page.
///
/// # Returns
/// The redirect or the forwarding page.
#[tracing::instrument(level = "trace", skip_all)]
fn back_to_client(
    brand: &Brand,
    redirect_uri: &str,
    pairs: &[(&str, &str)],
    (state, issuer): (Option<&str>, &str),
    hop: Hop,
) -> Response {
    // `redirect_uri_matches` parsed the URI before trusting it, so this
    // cannot fail; a failure is answered as an error page rather than a panic.
    let Ok(mut target) = url::Url::parse(redirect_uri) else {
        return consent::error_page(
            StatusCode::BAD_REQUEST,
            "The client's redirect URI is invalid.",
            brand,
        );
    };
    {
        // `query_pairs_mut` borrows `target` mutably until this block ends,
        // and keeps any query the registered URI already has.
        let mut query = target.query_pairs_mut();
        query.extend_pairs(pairs);
        if let Some(state) = state {
            query.append_pair("state", state);
        }
        query.append_pair("iss", issuer);
    }
    match hop {
        Hop::Redirect => Redirect::to(target.as_str()).into_response(),
        Hop::Forward => consent::forward_page(target.as_str(), brand),
    }
}

/// An OAuth error sent back to the client.
///
/// # Arguments
/// * `brand` - the product name and support contact (see
///   [`back_to_client`]).
/// * `redirect_uri` - the request's URI, matched against the client's
///   (see [`clients::redirect_uri_matches`]).
/// * `(state, issuer)` - the client's `state`, echoed if it sent one, and
///   this server's issuer.
/// * `(code, description)` - the OAuth error code and its
///   `error_description`.
/// * `hop` - a 303 or the forwarding page (see [`Hop`]).
///
/// # Returns
/// The redirect or the forwarding page.
#[tracing::instrument(level = "trace", skip_all)]
fn error_to_client(
    brand: &Brand,
    redirect_uri: &str,
    state_and_issuer: (Option<&str>, &str),
    (code, description): (&str, &str),
    hop: Hop,
) -> Response {
    back_to_client(
        brand,
        redirect_uri,
        &[("error", code), ("error_description", description)],
        state_and_issuer,
        hop,
    )
}

/// The page view of a pending authorization.
#[tracing::instrument(level = "trace", skip_all)]
fn view(pending_id: &str, pending: &PendingAuthorization) -> PageView {
    // `redirect_uri_matches` parsed the URI before trusting it, so the
    // fallback never shows.
    let client_host = url::Url::parse(&pending.redirect_uri)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .unwrap_or_default();
    PageView {
        pending_id: pending_id.to_string(),
        csrf: pending.csrf.clone(),
        client_name: pending
            .client
            .client_name
            .clone()
            .unwrap_or_else(|| pending.client.client_id.clone()),
        client_host,
        verified: pending.client.kind == ClientKind::Cimd,
    }
}

/// The labels a sign-in code's summary shows: its catalogs, its default
/// and its user.
///
/// # Arguments
/// * `app` - the application, for its label lookup.
/// * `record` - the code.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "trace", skip_all)]
async fn summary_labels(app: &App, record: &SignInCodeRecord) -> Result<Labels, OkfError> {
    let mut wanted = Wanted::default();
    for slug in record.catalogs.iter().chain(record.default_catalog.iter()) {
        wanted.catalog(slug);
    }
    if let Some(user) = &record.user {
        wanted.user(user);
    }
    app.labels().labels_with_owners(&wanted).await
}

/// The labels a consent page shows: the catalogs it offers.
///
/// # Arguments
/// * `app` - the application, for its label lookup.
/// * `pending` - the request; its `consent` is set (an unset one offers
///   nothing).
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "trace", skip_all)]
async fn consent_labels(app: &App, pending: &PendingAuthorization) -> Result<Labels, OkfError> {
    let mut wanted = Wanted::default();
    for slug in pending
        .consent
        .iter()
        .flat_map(|consent| consent.offered.keys())
    {
        wanted.catalog(slug);
    }
    app.labels().labels_with_owners(&wanted).await
}

/// What a sign-in code grants, as step 2 states it.
#[tracing::instrument(level = "trace", skip_all)]
fn summary(record: &SignInCodeRecord, labels: &Labels) -> GrantSummary {
    // A slug the directory does not hold yet (created since the last
    // load) is shown as itself, as `catalog_choices` does for catalogs.
    let user_shown = |slug: &String| labels.user(slug).unwrap_or(slug).to_string();
    // The texts are built once, over the listed catalogs and the default,
    // so the default is told apart from a same-label catalog exactly as
    // the list does. An all-catalog grant lists none, so the default is
    // shown by its own label alone. A default outside the list (not expected) joins the
    // set, so it is still told apart.
    let listed: &[String] = if record.all_catalogs {
        &[]
    } else {
        &record.catalogs
    };
    let default = record.default_catalog.as_deref();
    let choices = labels.catalog_choices(
        listed
            .iter()
            .map(String::as_str)
            .chain(default.filter(|slug| !listed.iter().any(|listed| listed == slug))),
    );
    let shown = |slug: &str| {
        choices
            .iter()
            .find(|(candidate, _)| candidate == slug)
            .map_or_else(|| slug.to_string(), |(_, text)| text.clone())
    };
    GrantSummary {
        access: record.access.as_str(),
        catalogs: if record.all_catalogs {
            "every catalog".to_string()
        } else {
            listed
                .iter()
                .map(|slug| shown(slug))
                .collect::<Vec<_>>()
                .join(", ")
        },
        actor: record.actor.clone(),
        user: record.user.as_ref().map(user_shown),
        default_catalog: default.map(shown),
    }
}

/// A request's validated parameters as a query string, in a fixed order and
/// without `signin`: what `/signin` returns to and the sign-in code link
/// extends. Rebuilt rather than copied from the request, so it carries only
/// visible ASCII and passes `pages::safe_return_to`.
///
/// # Arguments
/// * `query` - the request, already validated.
///
/// # Returns
/// The encoded query, without the `?`.
#[tracing::instrument(level = "trace", skip_all)]
fn canonical_query(query: &AuthorizeQuery) -> String {
    let fields = [
        ("response_type", &query.response_type),
        ("client_id", &query.client_id),
        ("redirect_uri", &query.redirect_uri),
        ("state", &query.state),
        ("code_challenge", &query.code_challenge),
        ("code_challenge_method", &query.code_challenge_method),
        ("resource", &query.resource),
    ];
    // `filter_map` drops the absent ones (only `state` and `resource` can
    // be, by now).
    form_urlencoded::Serializer::new(String::new())
        .extend_pairs(
            fields
                .iter()
                .filter_map(|(name, value)| value.as_deref().map(|value| (*name, value))),
        )
        .finish()
}

/// The sign-in code path's URL for a request's [`canonical_query`].
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
    )
)]
fn code_href(query: &str) -> String {
    format!("/oauth/authorize?{query}&signin=code")
}

/// The "Use a sign-in code instead" link for an authorize URL.
///
/// # Arguments
/// * `return_to` - a same-origin URL, such as `/signin`'s sanitised
///   `return_to`.
///
/// # Returns
/// The URL with `signin=code` added, or `None` when it is not an authorize
/// URL (so `/signin` shows no link for console sign-ins).
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
    )
)]
pub(crate) fn code_path_href(return_to: &str) -> Option<String> {
    // `strip_prefix` is Python's `removeprefix`, returning `None` when the
    // prefix is absent.
    return_to.strip_prefix("/oauth/authorize?").map(code_href)
}

/// A 303 to `/signin`, which comes back to this authorize URL after
/// sign-in. Nothing is stored: the GET simply runs again.
///
/// # Arguments
/// * `query` - the request's [`canonical_query`].
///
/// # Returns
/// The redirect.
#[tracing::instrument(level = "trace", skip_all)]
fn to_sign_in(query: &str) -> Response {
    let return_to = format!("/oauth/authorize?{query}");
    // `byte_serialize` percent-encodes for a query value, like Python's
    // `urllib.parse.quote_plus`.
    let encoded: String = form_urlencoded::byte_serialize(return_to.as_bytes()).collect();
    Redirect::to(&format!("/signin?return_to={encoded}")).into_response()
}

/// Why a signed-in consent's choices were refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    /// Nothing was ticked: show the form again.
    Empty,
    /// Something the page never offered: refuse the request.
    Tampered,
}

/// The catalogs and levels a signed-in consent chose, checked against what
/// its page offered (the posted form is never trusted).
///
/// # Arguments
/// * `choices` - the form's `grant.<catalog>` and `level.<catalog>` fields.
/// * `offered` - what the page offered.
///
/// # Returns
/// Each ticked catalog's level.
///
/// # Errors
/// [`Choice::Empty`] when nothing is ticked; [`Choice::Tampered`] as for
/// [`ticked_levels`].
#[tracing::instrument(level = "trace", skip_all)]
fn chosen_levels(
    choices: &BTreeMap<String, String>,
    offered: &Ceiling,
) -> Result<BTreeMap<String, Access>, Choice> {
    let chosen = ticked_levels(choices, offered)?;
    if chosen.is_empty() {
        Err(Choice::Empty)
    } else {
        Ok(chosen)
    }
}

/// The catalogs ticked on a signed-in consent page and their levels,
/// checked against what the page offered; none ticked is an empty map (a
/// Find or Next catalogs carries what is ticked, which may be nothing).
///
/// # Arguments
/// * `choices` - the form's `grant.<catalog>` and `level.<catalog>` fields.
/// * `offered` - what the page offered.
///
/// # Returns
/// Each ticked catalog's level.
///
/// # Errors
/// [`Choice::Tampered`] for a catalog not offered, a level above the offer
/// or unknown, a ticked catalog without a level, a checkbox value other
/// than `on`, or any other field.
#[tracing::instrument(level = "trace", skip_all)]
fn ticked_levels(
    choices: &BTreeMap<String, String>,
    offered: &Ceiling,
) -> Result<BTreeMap<String, Access>, Choice> {
    let mut chosen = BTreeMap::new();
    for (field, value) in choices {
        if let Some(catalog) = field.strip_prefix("grant.") {
            let allowed = *offered.get(catalog).ok_or(Choice::Tampered)?;
            let level = choices
                .get(&format!("level.{catalog}"))
                .and_then(|level| Access::parse(level))
                .ok_or(Choice::Tampered)?;
            if value != "on" || level > allowed {
                return Err(Choice::Tampered);
            }
            chosen.insert(catalog.to_string(), level);
        } else if let Some(catalog) = field.strip_prefix("level.") {
            // Selects post even when unticked; they must still be offered
            // ones, with a level the page could show.
            let allowed = *offered.get(catalog).ok_or(Choice::Tampered)?;
            if Access::parse(value).is_none_or(|level| level > allowed) {
                return Err(Choice::Tampered);
            }
        } else {
            return Err(Choice::Tampered);
        }
    }
    Ok(chosen)
}

/// Renders the signed-in consent page for a pending authorization.
///
/// # Arguments
/// * `pending_id`, `pending` - the request; its `consent` is set.
/// * `user` - the signed-in user, and the labels of what it is offered.
/// * `chosen` - the catalogs carried over ticked (a Find or Next
///   catalogs), if any.
/// * `error` - a message, if the page is shown again.
/// * `brand` - the product name and support contact.
///
/// # Returns
/// The page, or the expired page if `pending` is on the code path.
#[tracing::instrument(level = "trace", skip_all)]
fn user_consent_page(
    pending_id: &str,
    pending: &PendingAuthorization,
    (user, labels): (&UserAuth, &Labels),
    chosen: Option<&BTreeMap<String, Access>>,
    error: Option<&str>,
    brand: &Brand,
) -> Response {
    let Some(consent) = pending.consent.as_ref() else {
        return consent::error_page(StatusCode::BAD_REQUEST, EXPIRED, brand);
    };
    // The person sees labels; the slugs stay in the form's ids and values.
    let user_label = user_label(&user.display_name, user.email.as_deref());
    let catalog_labels = labels.catalog_choices(consent.offered.keys().map(String::as_str));
    consent::user_page(
        &view(pending_id, pending),
        &UserPage {
            user_label: &user_label,
            catalog_labels: &catalog_labels,
            ceiling: &consent.offered,
            browse: consent::Browse {
                prefix: &consent.prefix,
                more: consent.next.is_some(),
                whole: consent.whole,
                chosen,
            },
            nonce: &consent.nonce,
            code_href: &code_href(&pending.query),
        },
        error,
        brand,
    )
}

/// The consent a signed-in person's page starts with: the first page of
/// their ceiling now (never the whole ceiling, issue #736) and a fresh
/// nonce.
///
/// # Errors
/// [`OkfError::Io`] if the OS random source fails; [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all)]
async fn new_consent(app: &App, user: &UserAuth) -> Result<UserConsent, OkfError> {
    let page = ceiling::page(app.accounts().executor(), &user.id, "", None, CONSENT_PAGE).await?;
    Ok(UserConsent {
        user_id: user.id.clone(),
        nonce: keys::generate_with("")?,
        whole: page.next.is_none(),
        offered: page.entries,
        prefix: String::new(),
        next: page.next,
    })
}

/// The trusted half of a request: its client and its redirect URI, as
/// [`clients::redirect_uri_matches`] accepts it.
///
/// # Returns
/// The client and the redirect URI.
///
/// # Errors
/// The status and message of the error page to show: never a redirect,
/// since neither the client nor the redirect URI is trusted yet.
#[tracing::instrument(level = "debug", skip_all)]
async fn trusted_client<'q>(
    app: &App,
    oauth: &OAuth,
    query: &'q AuthorizeQuery,
) -> Result<(ClientRecord, &'q str), (StatusCode, &'static str)> {
    // 1. The client.
    let Some(client_id) = query.client_id.as_deref() else {
        return Err((StatusCode::BAD_REQUEST, "The request names no client."));
    };
    let client = match clients::resolve(app, oauth, client_id).await {
        Ok(client) => client,
        Err(OAuthError::Server(error)) => {
            split_event!(
                error,
                { kind = error.kind(), },
                { %error, },
                "resolving an OAuth client failed"
            );
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "The server could not check this client.",
            ));
        }
        Err(error) => {
            // The page stays generic; the log says why, so an operator can
            // tell an unknown DCR id from a document this server refuses
            // (issue #273). A client id is not a secret.
            split_event!(
                warn,
                { reason = error.code(), },
                { client_id, error = %error, },
                "refused an OAuth client"
            );
            return Err((
                StatusCode::BAD_REQUEST,
                "This client is not known to the server.",
            ));
        }
    };
    // 2. Its redirect URI: string equality, no prefix or pattern, except
    // that a loopback `http` URI may name any port (RFC 8252 section 7.3).
    // The request's URI, port included, is what is kept: the redirect goes
    // there, and the token request must repeat it exactly.
    let Some(redirect_uri) = query.redirect_uri.as_deref().filter(|uri| {
        client
            .redirect_uris
            .iter()
            .any(|known| clients::redirect_uri_matches(known, uri))
    }) else {
        return Err((
            StatusCode::BAD_REQUEST,
            "The request's redirect URI is not one the client registered.",
        ));
    };
    Ok((client, redirect_uri))
}

/// `GET /oauth/authorize`: validates the request, then shows step 1 (the
/// sign-in code path), the signed-in consent page, or sends the browser to
/// `/signin`.
///
/// When it refuses a `resource` that is not this server's, its span carries
/// `okf.resource`: how the value differed, a closed set
/// ([`resource::classify`]), never the value.
#[tracing::instrument(
    level = "debug",
    skip_all,
    fields(okf.resource = tracing::field::Empty)
)]
async fn authorize(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    query: Result<Query<AuthorizeQuery>, QueryRejection>,
) -> Response {
    // OAuth off: the routes are not mounted then, so this is only a guard.
    let Some(oauth) = app.oauth() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // An unparsable query is treated like an empty one: the first missing
    // parameter, `client_id`, is then the error page. `map` unwraps the
    // `Query` tuple struct; a rejection becomes the all-`None` default.
    let query = query.map(|Query(query)| query).unwrap_or_default();
    let brand = app.settings().brand();
    let (client, redirect_uri) = match trusted_client(&app, oauth, &query).await {
        Ok(trusted) => trusted,
        Err((status, message)) => return consent::error_page(status, message, &brand),
    };
    // From here on, errors go back to the client.
    let issuer = oauth.settings.issuer.as_str();
    let state = query.state.as_deref();
    let back = |code: &str, description: &str| {
        error_to_client(
            &brand,
            redirect_uri,
            (state, issuer),
            (code, description),
            Hop::Redirect,
        )
    };
    // 3. The rest, in the spec's order.
    if query.response_type.as_deref() != Some("code") {
        return back(
            "unsupported_response_type",
            "only response_type=code is supported",
        );
    }
    let Some(code_challenge) = query
        .code_challenge
        .as_deref()
        .filter(|challenge| pkce::is_valid_challenge(challenge))
    else {
        return back("invalid_request", "a PKCE S256 code_challenge is required");
    };
    if query.code_challenge_method.as_deref() != Some("S256") {
        return back("invalid_request", "code_challenge_method must be S256");
    }
    // RFC 8707 makes `resource` optional here, and some clients (Crush)
    // strip it from the browser URL and send it only at the token endpoint.
    // This server has one resource, so a missing one means `/mcp`; a named
    // one must be it. The token endpoint still requires and checks it.
    if let Some(sent) = query
        .resource
        .as_deref()
        .filter(|sent| *sent != oauth.settings.resource)
    {
        tracing::Span::current().record(
            "okf.resource",
            resource::classify(sent, &oauth.settings.resource).as_str(),
        );
        return back(
            "invalid_target",
            "resource must name this server's MCP endpoint",
        );
    }
    // 4. Who approves (spec "OAuth consent as the user"): the sign-in code
    // path when asked for, else the signed-in person, else /signin. This
    // comes after every check that can fail, and before anything is
    // stored: an anonymous GET leaves nothing behind.
    let query_string = canonical_query(&query);
    let user = if query.signin.as_deref() == Some("code") {
        None
    } else {
        match signed_in_user(&app, &headers).await {
            Ok(Some(user)) => Some(user),
            Ok(None) => return to_sign_in(&query_string),
            Err(error) => return server_error(&error, &brand),
        }
    };
    let consent = match &user {
        Some(user) => match new_consent(&app, user).await {
            Ok(consent) => Some(consent),
            // Deleted since the session was read: as if signed out.
            Err(OkfError::UnknownName { .. }) => return to_sign_in(&query_string),
            Err(error) => return server_error(&error, &brand),
        },
        None => None,
    };
    // 5. A pending authorization, and its first page.
    let (pending_id, csrf) = match (keys::generate_with(""), keys::generate_with("")) {
        (Ok(id), Ok(csrf)) => (id, csrf),
        (Err(error), _) | (_, Err(error)) => return server_error(&error, &brand),
    };
    let pending = PendingAuthorization {
        csrf,
        client,
        redirect_uri: redirect_uri.to_string(),
        state: query.state.clone(),
        code_challenge: code_challenge.to_string(),
        resource: oauth.settings.resource.clone(),
        accepted: None,
        failed_attempts: 0,
        consent,
        query: query_string,
    };
    let page = match &user {
        Some(user) => match consent_labels(&app, &pending).await {
            Ok(labels) => {
                user_consent_page(&pending_id, &pending, (user, &labels), None, None, &brand)
            }
            Err(error) => return server_error(&error, &brand),
        },
        None => consent::code_page(&view(&pending_id, &pending), None, &brand),
    };
    // `insert` is `false` when the bounded memory map is full: an
    // unauthenticated GET must not grow memory without limit. A Redis
    // failure is the busy page (spec Decision 16).
    match oauth
        .pending
        .insert(&pending_id, pending, Instant::now())
        .await
    {
        Ok(true) => page,
        Ok(false) => consent::error_page(
            StatusCode::SERVICE_UNAVAILABLE,
            "Too many sign-ins are in progress. Try again in a few minutes.",
            &brand,
        ),
        Err(error) => server_error(&error, &brand),
    }
}

/// What a consent POST's update reads: a snapshot of the request after the
/// change, the Approve decision, and whether a signed-in page's nonce was
/// spent.
type Decided = (PendingAuthorization, Approval, bool);

/// The change a consent POST makes to its pending authorization, in one
/// atomic update (a compare-and-set in Redis).
///
/// The CSRF token is checked first, and a mismatch changes nothing:
/// someone who learned the id cannot cancel the request. A request whose
/// wrong codes are spent is refused too (it is normally gone already). An
/// Approve's summary token is matched against the code it was rendered for,
/// so the code id and its token come from one snapshot, and a signed-in
/// Approve's nonce is spent, so one page approves once on every replica.
///
/// # Arguments
/// * `form` - the posted form.
/// * `fresh` - the replacement nonce for a signed-in Approve, else `None`.
///
/// # Returns
/// The closure for [`crate::shared::ephemeral::Ephemeral::update`]. It may
/// run more than once, each time on a fresh copy, so it reads only that copy
/// and clones `fresh` (`lessons.md`). It answers `None` when the CSRF token
/// does not match.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "builds a closure and returns it; the update that runs it has the span"
    )
)]
// `impl FnMut(..) + 'f`: an unnamed closure type that may be called several
// times and borrows `form` for `'f` (Python: returning an inner function).
fn consent_change<'f>(
    form: &'f ConsentForm,
    fresh: Option<String>,
) -> impl FnMut(&mut PendingAuthorization) -> Option<Decided> + 'f {
    // `move`: the closure owns `fresh` (and a copy of the `form` reference).
    move |pending| {
        let matches: bool = pending.csrf.as_bytes().ct_eq(form.csrf.as_bytes()).into();
        let live = pending.failed_attempts < MAX_FAILED_ATTEMPTS;
        (matches && live).then(|| {
            let decided = approval(pending.accepted.as_ref(), form.summary.as_deref());
            // Cloned, not taken: in Redis this closure can run again on a
            // fresh copy (`Ephemeral::update`), and that run must see the
            // same token, or it would refuse a valid Approve.
            let claimed = fresh.clone().is_some_and(|fresh| {
                claim_consent(pending.consent.as_mut(), form.summary.as_deref(), fresh)
            });
            (pending.clone(), decided, claimed)
        })
    }
}

/// Drops a pending authorization (denied, spent or refused), then answers
/// `answer`.
///
/// # Arguments
/// * `oauth` - the OAuth state.
/// * `pending_id` - the request's id.
/// * `brand` - for the busy page.
/// * `answer` - the page to answer once it is dropped.
///
/// # Returns
/// `answer`, or the busy page when Redis fails: the request then stays
/// until it expires, and the answer must not say it ended.
#[tracing::instrument(level = "trace", skip_all)]
async fn ending(oauth: &OAuth, pending_id: &str, brand: &Brand, answer: Response) -> Response {
    match oauth.pending.remove(pending_id).await {
        Ok(()) => answer,
        Err(error) => server_error(&error, brand),
    }
}

/// `POST /oauth/authorize`: the consent form (spec "Consent flow").
#[tracing::instrument(level = "debug", skip_all)]
async fn consent_post(
    State(app): State<Arc<App>>,
    parts: axum::http::request::Parts,
    form: Result<Form<ConsentForm>, FormRejection>,
) -> Response {
    // OAuth off: the routes are not mounted then, so this is only a guard.
    let Some(oauth) = app.oauth() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let brand = app.settings().brand();
    // Another site's page posting this form into the person's browser
    // (with its own pending id and token) is refused before anything else.
    if cross_site(&parts.headers) {
        return consent::error_page(StatusCode::FORBIDDEN, CROSS_SITE, &brand);
    }
    let now = Instant::now();
    if !oauth.consent_limit.try_take(now).await {
        app.metrics().rate_limited(Limiter::OauthConsent);
        return consent::error_page(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many sign-in attempts. Wait a minute.",
            &brand,
        );
    }
    let Ok(Form(form)) = form else {
        return consent::error_page(StatusCode::BAD_REQUEST, EXPIRED, &brand);
    };
    // A signed-in Approve spends its page's nonce; the replacement is made
    // here, outside the update, because making it can fail.
    let fresh = if matches!(form.action.as_str(), "consent" | "find" | "more") {
        match keys::generate_with("") {
            Ok(nonce) => Some(nonce),
            Err(error) => return server_error(&error, &brand),
        }
    } else {
        None
    };
    // The CSRF token, the summary token and the nonce, in one atomic update
    // ([`consent_change`]).
    let pending = match oauth
        .pending
        .update(&form.pending, now, consent_change(&form, fresh))
        .await
    {
        Ok(pending) => pending,
        Err(error) => return server_error(&error, &brand),
    };
    // `Some(Some(_))`: the entry is live (outer) and the token matched (inner).
    let Some(Some((pending, decided, claimed))) = pending else {
        return consent::error_page(StatusCode::BAD_REQUEST, EXPIRED, &brand);
    };
    // The sign-in code steps belong to the code path only: on the user
    // path, the code is reached through the link, a fresh GET.
    let code_path = pending.consent.is_none();
    match form.action.as_str() {
        "deny" => {
            let denied = error_to_client(
                &brand,
                &pending.redirect_uri,
                (pending.state.as_deref(), &oauth.settings.issuer),
                ("access_denied", "the person signing in denied access"),
                Hop::Forward,
            );
            ending(oauth, &form.pending, &brand, denied).await
        }
        "code" if code_path => {
            let presented = form.sign_in_code.as_deref().unwrap_or_default();
            check_code(&app, oauth, &form.pending, &pending, presented).await
        }
        "approve" if code_path => match decided {
            Approval::NoCode => consent::code_page(&view(&form.pending, &pending), None, &brand),
            // Nothing changes: the latest summary still approves.
            Approval::Stale => {
                consent::code_page(&view(&form.pending, &pending), Some(STALE), &brand)
            }
            Approval::Current(code_id) => {
                approve(&app, oauth, &form.pending, &pending, &code_id).await
            }
        },
        // `claimed`: on the user path, with this page's current nonce.
        "consent" if claimed => consent_as_user(&app, oauth, &form, &pending, &parts).await,
        "find" | "more" if claimed => {
            consent_browse(&app, oauth, &form, &pending, &parts.headers).await
        }
        _ => consent::error_page(StatusCode::BAD_REQUEST, EXPIRED, &brand),
    }
}

/// Counts a failed attempt and answers step 1 again, or drops the request
/// at [`MAX_FAILED_ATTEMPTS`].
#[tracing::instrument(level = "trace", skip_all)]
async fn failed_attempt(
    oauth: &OAuth,
    pending_id: &str,
    pending: &PendingAuthorization,
    brand: &Brand,
) -> Response {
    // Idempotent on a fresh copy: each run counts once on the value it was
    // given, so wrong codes on every replica add up.
    let attempts = oauth
        .pending
        .update(pending_id, Instant::now(), |pending| {
            pending.failed_attempts = pending.failed_attempts.saturating_add(1);
            pending.accepted = None;
            pending.failed_attempts
        })
        .await;
    match attempts {
        Ok(Some(attempts)) if attempts < MAX_FAILED_ATTEMPTS => {
            consent::code_page(&view(pending_id, pending), Some(BAD_CODE), brand)
        }
        Ok(Some(_) | None) => {
            let refused = consent::error_page(
                StatusCode::BAD_REQUEST,
                "Too many wrong sign-in codes.",
                brand,
            );
            ending(oauth, pending_id, brand, refused).await
        }
        Err(error) => server_error(&error, brand),
    }
}

/// Writes the `outcome` of the span that was current when it was made
/// (`check_code` or `finish`, which declare `method = "oauth_authorize"` and
/// an empty `outcome`) when it is dropped: `failed` on every early return,
/// `ok` only after [`Decision::accept`]. The values are the spellings of
/// `okf_auth_attempts_total`; nothing from the request (a code, a client id,
/// a redirect) is recorded.
struct Decision {
    /// The span to write to.
    span: tracing::Span,
    /// What the decision came to so far.
    outcome: Outcome,
}

impl Decision {
    /// Starts as a failure, on the current span.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "captures the caller's span; a span of its own would be captured instead"
        )
    )]
    fn start() -> Self {
        Self {
            span: tracing::Span::current(),
            outcome: Outcome::Failed,
        }
    }

    /// Marks the decision as accepted.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: sets one field, a span would record nothing new"
        )
    )]
    fn accept(&mut self) {
        self.outcome = Outcome::Ok;
    }
}

// `Drop` runs when the value goes out of scope, like a Python context
// manager's `__exit__`, and on every early `return`.
impl Drop for Decision {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "a drop hook that only records one field on the captured span"
        )
    )]
    fn drop(&mut self) {
        self.span.record("outcome", self.outcome.as_str());
    }
}

/// `action=code`: checks the code without consuming it, and shows step 2
/// with a fresh summary token that replaces any earlier summary's.
#[tracing::instrument(
    level = "debug",
    skip_all,
    fields(method = "oauth_authorize", outcome = tracing::field::Empty)
)]
async fn check_code(
    app: &App,
    oauth: &OAuth,
    pending_id: &str,
    pending: &PendingAuthorization,
    presented: &str,
) -> Response {
    let mut decision = Decision::start();
    let brand = app.settings().brand();
    match code_rows::find_live(app.accounts().executor(), presented, app.clock().now()).await {
        Ok(Some(record)) => {
            let summary_nonce = match keys::generate_with("") {
                Ok(nonce) => nonce,
                Err(error) => return server_error(&error, &brand),
            };
            let accepted = AcceptedCode {
                code_id: record.id.clone(),
                summary_nonce: summary_nonce.clone(),
            };
            let stored = oauth
                .pending
                .update(pending_id, Instant::now(), |pending| {
                    // Cloned, not moved: the closure may run again on a
                    // fresh copy (`Ephemeral::update`).
                    pending.accepted = Some(accepted.clone());
                })
                .await;
            match stored {
                Ok(Some(())) => {}
                Ok(None) => return consent::error_page(StatusCode::BAD_REQUEST, EXPIRED, &brand),
                Err(error) => return server_error(&error, &brand),
            }
            let labels = match summary_labels(app, &record).await {
                Ok(labels) => labels,
                Err(error) => return server_error(&error, &brand),
            };
            decision.accept();
            consent::summary_page(
                &view(pending_id, pending),
                &summary(&record, &labels),
                &summary_nonce,
                &brand,
            )
        }
        Ok(None) => failed_attempt(oauth, pending_id, pending, &brand).await,
        Err(error) => server_error(&error, &brand),
    }
}

/// `action=approve` from the current summary: redeems its code into a
/// grant and [`finish`]es.
#[tracing::instrument(level = "debug", skip_all)]
async fn approve(
    app: &App,
    oauth: &OAuth,
    pending_id: &str,
    pending: &PendingAuthorization,
    code_id: &str,
) -> Response {
    let brand = app.settings().brand();
    let grant_id = match code_rows::redeem(
        app.accounts().executor(),
        code_id,
        &pending.client,
        app.clock().now(),
    )
    .await
    {
        Ok(grant_id) => grant_id,
        // Expired, revoked or redeemed meanwhile (another approve won).
        Err(OkfError::Refused { .. }) => {
            return failed_attempt(oauth, pending_id, pending, &brand).await
        }
        Err(error) => return server_error(&error, &brand),
    };
    finish(app, oauth, pending_id, pending, grant_id).await
}

/// Ends an approved request: drops the pending authorization, loads the
/// new grant into the key cache, mints an authorization code bound to it
/// and sends the browser back to the client. Both paths end here.
///
/// # Arguments
/// * `app`, `oauth` - the application and its OAuth state.
/// * `pending_id`, `pending` - the request.
/// * `grant_id` - the grant the approval created.
///
/// # Returns
/// The forwarding page to the client with `code`, or an error page.
#[tracing::instrument(
    level = "debug",
    skip_all,
    fields(method = "oauth_authorize", outcome = tracing::field::Empty)
)]
async fn finish(
    app: &App,
    oauth: &OAuth,
    pending_id: &str,
    pending: &PendingAuthorization,
    grant_id: String,
) -> Response {
    let mut decision = Decision::start();
    let brand = app.settings().brand();
    // The request is spent once the grant exists, whatever happens next. A
    // Redis failure stops here (spec Decision 16), and no code is minted, so
    // the grant just created can never be used: revoke it (issue #589), so
    // the user's grant list shows no consent that never completed. The
    // person starts again from their client.
    if let Err(error) = oauth.pending.remove(pending_id).await {
        revoke_unused_grant(app, &grant_id).await;
        return server_error(&error, &brand);
    }
    // The credential cache reads the new grant on first use, so nothing
    // needs loading here.
    let (code, family_id) = match mint_ids(oauth) {
        Ok(ids) => ids,
        Err(error) => {
            revoke_unused_grant(app, &grant_id).await;
            return server_error(&error, &brand);
        }
    };
    // A full map refuses the code, and so does a Redis failure (counted and
    // logged by `Ephemeral`; never a fallback to memory, spec Decision 16):
    // `unwrap_or(false)` sends an `Err` to the busy page, as a full map.
    // No code is minted, so the grant just created is revoked (issue #925),
    // as when the pending removal fails, and the person starts again from
    // their client.
    let stored = oauth
        .codes
        .insert(
            &code_key(&code),
            AuthorizationCode {
                // The code takes its own copy: the id is still needed below
                // to revoke the grant if the code is not stored.
                grant_id: grant_id.clone(),
                client_id: pending.client.client_id.clone(),
                redirect_uri: pending.redirect_uri.clone(),
                code_challenge: pending.code_challenge.clone(),
                resource: pending.resource.clone(),
                family_id,
                used: false,
                replayed: false,
            },
            Instant::now(),
        )
        .await
        .unwrap_or(false);
    if !stored {
        revoke_unused_grant(app, &grant_id).await;
        return consent::error_page(StatusCode::SERVICE_UNAVAILABLE, BUSY, &brand);
    }
    decision.accept();
    back_to_client(
        &brand,
        &pending.redirect_uri,
        &[("code", &code)],
        (pending.state.as_deref(), &oauth.settings.issuer),
        Hop::Forward,
    )
}

/// Generates the authorization code and the refresh-token family id bound
/// to it.
///
/// # Arguments
/// * `oauth` - the OAuth state (only a test build reads it, for the hook
///   that forces a failure).
///
/// # Errors
/// [`OkfError::Io`] if the OS random source fails.
#[cfg_attr(
    not(test),
    allow(unused_variables, reason = "only the test hook reads `oauth`")
)]
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "two calls to the instrumented generators: each is its own span"
    )
)]
fn mint_ids(oauth: &OAuth) -> Result<(String, String), OkfError> {
    #[cfg(test)]
    let forced = oauth.take_forced_generation_failure();
    #[cfg(test)]
    let forced_error = || OkfError::Io {
        path: "<forced generation failure>".to_string(),
        source: std::io::Error::other("forced by a test"),
    };
    #[cfg(test)]
    if forced == Some(super::ForcedGenerationFailure::Code) {
        return Err(forced_error());
    }
    let code = keys::generate_with(keys::AUTHORIZATION_CODE_PREFIX)?;
    #[cfg(test)]
    if forced == Some(super::ForcedGenerationFailure::FamilyId) {
        return Err(forced_error());
    }
    Ok((code, new_id()?))
}

/// Revokes the grant this request created and could not finish with a code,
/// by its own id: a grant that existed before the request is never touched,
/// because both ways of creating one (`redeem`, `create_self_issued`) insert
/// a new row. Best effort: a failure is logged, and the grant, which no code
/// leads to, stays until it is revoked by hand or goes idle.
///
/// # Arguments
/// * `app` - the application.
/// * `grant_id` - the grant the approval created.
#[tracing::instrument(level = "debug", skip_all)]
async fn revoke_unused_grant(app: &App, grant_id: &str) {
    let audit = crate::store::audit::AuditActor::system("oauth");
    if let Err(error) = crate::store::keys::revoke_by_id(
        app.accounts().executor(),
        grant_id,
        &audit,
        app.clock().now(),
    )
    .await
    {
        split_event!(
            warn,
            { grant = %grant_id, },
            { %error, },
            "could not revoke a grant whose consent did not complete"
        );
    }
}

/// `action=consent`: a signed-in person's approval, its nonce already spent
/// by [`claim_consent`]. It checks that the cookie is still that person's,
/// refuses any choice the page did not offer, and creates the self-issued
/// grant, which re-checks the session and the ceiling under the user's
/// row lock. Then it [`finish`]es as the code path does.
///
/// # Arguments
/// * `app`, `oauth` - the application and its OAuth state.
/// * `form` - the posted form.
/// * `pending` - the request, holding the replacement nonce.
/// * `headers` - the request's headers (the session cookie).
///
/// # Returns
/// The forwarding page to the client, the page again (nothing or a bad
/// default chosen), or an error page.
#[tracing::instrument(level = "debug", skip_all)]
async fn consent_as_user(
    app: &App,
    oauth: &OAuth,
    form: &ConsentForm,
    pending: &PendingAuthorization,
    parts: &axum::http::request::Parts,
) -> Response {
    let headers = &parts.headers;
    let brand = app.settings().brand();
    let Some(consent) = pending.consent.as_ref() else {
        return consent::error_page(StatusCode::BAD_REQUEST, EXPIRED, &brand);
    };
    // Every refusal below ends the request: the page cannot be trusted again.
    // The closure returns `ending`'s future, which the caller awaits; `move`
    // copies the references it uses into it.
    let brand = &brand;
    let refuse = move |message| {
        let refused = consent::error_page(StatusCode::BAD_REQUEST, message, brand);
        ending(oauth, &form.pending, brand, refused)
    };
    let Some(session_id) = cookie(headers, COOKIE) else {
        return refuse(SIGNED_OUT).await;
    };
    let user = match signed_in_user(app, headers).await {
        Ok(Some(user)) if user.id == consent.user_id => user,
        Ok(_) => return refuse(SIGNED_OUT).await,
        Err(error) => return server_error(&error, brand),
    };
    let chosen = match chosen_levels(&form.choices, &consent.offered) {
        Ok(chosen) => chosen,
        Err(Choice::Empty) => {
            let labels = match consent_labels(app, pending).await {
                Ok(labels) => labels,
                Err(error) => return server_error(&error, brand),
            };
            return user_consent_page(
                &form.pending,
                pending,
                (&user, &labels),
                None,
                Some(NOTHING_CHOSEN),
                brand,
            );
        }
        Err(Choice::Tampered) => {
            split_event!(
                warn,
                {},
                { user = %user.name, client = %pending.client.client_id, },
                "refused a consent outside the offered choices"
            );
            return refuse(TAMPERED).await;
        }
    };
    let default_catalog = form
        .default_catalog
        .as_deref()
        .filter(|name| !name.is_empty());
    if default_catalog.is_some_and(|name| !chosen.contains_key(name)) {
        let labels = match consent_labels(app, pending).await {
            Ok(labels) => labels,
            Err(error) => return server_error(&error, brand),
        };
        return user_consent_page(
            &form.pending,
            pending,
            (&user, &labels),
            None,
            Some(DEFAULT_NOT_CHOSEN),
            brand,
        );
    }
    let new = NewSelfIssuedGrant {
        user_id: &user.id,
        session_id,
        client: &pending.client,
        levels: &chosen,
        default_catalog,
    };
    let audit = crate::store::audit::AuditActor {
        source: crate::store::audit::Source::Console,
        credential: None,
        user_id: Some(user.id.clone()),
        label: user.name.clone(),
        via: crate::store::audit::Via::Session,
        operator: user.is_admin,
        provisioner: false,
        ip: crate::http::console_session::client_ip(parts, &app.config().auth),
    };
    let grant_id = match grant_rows::create_self_issued(
        app.accounts().executor(),
        &new,
        &audit,
        app.clock().now(),
    )
    .await
    {
        Ok(grant_id) => grant_id,
        // Signed out, reset or deleted since the check above.
        Err(OkfError::Unauthenticated { .. }) => return refuse(SIGNED_OUT).await,
        // The ceiling shrank, or a chosen catalog was deleted, meanwhile.
        Err(OkfError::Refused { .. } | OkfError::CatalogNotSelected { .. }) => {
            return refuse(CHANGED).await
        }
        Err(error) => return server_error(&error, brand),
    };
    split_event!(
        info,
        { grant = %grant_id, },
        { user = %user.name, client = %pending.client.client_id, },
        "consent granted as the signed-in user"
    );
    finish(app, oauth, &form.pending, pending, grant_id).await
}

/// The offer a Find or Next catalogs moves to, read from the database now:
/// the ticked catalogs at the user's ceiling on each now (one dropped when
/// the ceiling no longer reaches it, its level lowered with the ceiling),
/// and one page of the ceiling (issue #736). Bounded by [`MAX_CARRIED`]
/// and [`CONSENT_PAGE`]: no read here grows with the ceiling.
///
/// # Arguments
/// * `app` - the application.
/// * `user_id` - the signed-in user's id.
/// * `ticked` - the catalogs ticked on the page, checked against its offer.
/// * `(prefix, after)` - the page to read: its name prefix and the name it
///   continues after (`None` for the first).
///
/// # Returns
/// The new consent (without its nonce, which the caller keeps) and the
/// carried catalogs at their (possibly lowered) chosen levels.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all)]
async fn browse_offer(
    app: &App,
    user_id: &str,
    ticked: &BTreeMap<String, Access>,
    (prefix, after): (&str, Option<&str>),
) -> Result<(UserConsent, BTreeMap<String, Access>), OkfError> {
    let names: Vec<String> = ticked.keys().cloned().collect();
    // `&mut *` borrows the pooled connection, returned when the statement ends.
    let carried_ceiling = ceiling::ceiling_in(
        &mut *app
            .accounts()
            .executor()
            .acquire()
            .await
            .map_err(OkfError::from)?,
        user_id,
        &names,
    )
    .await?;
    let page = ceiling::page(
        app.accounts().executor(),
        user_id,
        prefix,
        after,
        CONSENT_PAGE,
    )
    .await?;
    // Each ticked catalog the ceiling still reaches, at the lower of the
    // level chosen and the ceiling's now.
    let carried: BTreeMap<String, Access> = ticked
        .iter()
        .filter_map(|(catalog, &level)| {
            let allowed = *carried_ceiling.get(catalog)?;
            Some((catalog.clone(), level.min(allowed)))
        })
        .collect();
    let whole = prefix.is_empty() && after.is_none() && page.next.is_none();
    // The page, and the carried catalogs at the ceiling's level (what the
    // page may offer for them); `extend` overwrites a page entry with the
    // same level read a moment apart.
    let mut offered = page.entries;
    offered.extend(
        carried
            .keys()
            .filter_map(|catalog| Some((catalog.clone(), *carried_ceiling.get(catalog)?))),
    );
    Ok((
        UserConsent {
            user_id: user_id.to_string(),
            nonce: String::new(),
            offered,
            prefix: prefix.to_string(),
            next: page.next,
            whole,
        },
        carried,
    ))
}

/// `action=find` or `action=more`: a signed-in page's Find (catalogs whose
/// names start with the Find box) or Next catalogs (the page after the
/// one shown), its nonce already spent by [`claim_consent`] and replaced.
/// The ticked catalogs are carried over (checked against the page's offer,
/// at most [`MAX_CARRIED`]); the new offer is read now ([`browse_offer`])
/// and stored in the pending authorization in one atomic update that
/// checks the replacement nonce is still current, then shown (issue #736).
///
/// # Arguments
/// * `app`, `oauth` - the application and its OAuth state.
/// * `form` - the posted form.
/// * `pending` - the request, holding the replacement nonce.
/// * `headers` - the request's headers (the session cookie).
///
/// # Returns
/// The page with the new offer, the page again (too many ticked), or an
/// error page.
#[tracing::instrument(level = "debug", skip_all)]
async fn consent_browse(
    app: &App,
    oauth: &OAuth,
    form: &ConsentForm,
    pending: &PendingAuthorization,
    headers: &HeaderMap,
) -> Response {
    let brand = app.settings().brand();
    let Some(consent) = pending.consent.as_ref() else {
        return consent::error_page(StatusCode::BAD_REQUEST, EXPIRED, &brand);
    };
    let brand = &brand;
    let refuse = move |message| {
        let refused = consent::error_page(StatusCode::BAD_REQUEST, message, brand);
        ending(oauth, &form.pending, brand, refused)
    };
    let user = match signed_in_user(app, headers).await {
        Ok(Some(user)) if user.id == consent.user_id => user,
        Ok(_) => return refuse(SIGNED_OUT).await,
        Err(error) => return server_error(&error, brand),
    };
    let ticked = match ticked_levels(&form.choices, &consent.offered) {
        Ok(ticked) => ticked,
        Err(Choice::Empty | Choice::Tampered) => {
            split_event!(
                warn,
                {},
                { user = %user.name, client = %pending.client.client_id, },
                "refused a consent search outside the offered choices"
            );
            return refuse(TAMPERED).await;
        }
    };
    // What to show: the page as it was (its replacement nonce is stored)
    // with its ticks and a message, or the new offer.
    let (shown, chosen, error) = if ticked.len() > MAX_CARRIED {
        (pending.clone(), ticked, Some(TOO_MANY_CARRIED))
    } else {
        match browse_to(app, oauth, form, consent, &ticked).await {
            Ok(Some((updated, carried))) => (updated, carried, None),
            // Gone, or another POST of this page spent the nonce meanwhile.
            Ok(None) => return consent::error_page(StatusCode::BAD_REQUEST, EXPIRED, brand),
            Err(error) => return server_error(&error, brand),
        }
    };
    match consent_labels(app, &shown).await {
        Ok(labels) => user_consent_page(
            &form.pending,
            &shown,
            (&user, &labels),
            Some(&chosen),
            error,
            brand,
        ),
        Err(error) => server_error(&error, brand),
    }
}

/// What a stored Find or Next catalogs gives back: the request as stored,
/// and the carried catalogs at their (possibly lowered) chosen levels.
type Browsed = (PendingAuthorization, BTreeMap<String, Access>);

/// Moves a signed-in page to the offer its Find or Next catalogs asks for:
/// reads it ([`browse_offer`]) and stores it in the pending authorization
/// while the page's replacement nonce is still current.
///
/// # Arguments
/// * `app`, `oauth` - the application and its OAuth state.
/// * `form` - the posted form: its action, Find box and pending id.
/// * `consent` - the page's consent, its nonce already replaced.
/// * `ticked` - the ticked catalogs, checked against its offer.
///
/// # Returns
/// The stored request and the carried catalogs at their levels, or `None`
/// when the request is gone or its nonce was spent meanwhile.
///
/// # Errors
/// [`OkfError::Database`]; [`OkfError::Busy`] when Redis fails.
#[tracing::instrument(level = "debug", skip_all)]
async fn browse_to(
    app: &App,
    oauth: &OAuth,
    form: &ConsentForm,
    consent: &UserConsent,
    ticked: &BTreeMap<String, Access>,
) -> Result<Option<Browsed>, OkfError> {
    // Find starts over from the box's prefix; Next continues the shown
    // page's. An unusable prefix finds nothing (`ceiling::page`).
    let found;
    let (prefix, after) = if form.action == "find" {
        found = form
            .find
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        (found.as_str(), None)
    } else {
        (consent.prefix.as_str(), consent.next.as_deref())
    };
    let (mut offer, carried) = browse_offer(app, &consent.user_id, ticked, (prefix, after)).await?;
    offer.nonce.clone_from(&consent.nonce);
    // Stored only while the replacement nonce is still the page's: the
    // closure may run again on a fresh copy (Redis), so it clones `offer`
    // inside and does nothing else (`lessons.md`).
    let stored = oauth
        .pending
        .update(&form.pending, Instant::now(), |entry| {
            let current = entry.consent.as_ref().is_some_and(|now| {
                now.user_id == offer.user_id
                    && bool::from(now.nonce.as_bytes().ct_eq(offer.nonce.as_bytes()))
            });
            current.then(|| {
                entry.consent = Some(offer.clone());
                entry.clone()
            })
        })
        .await?;
    // `flatten`: absent (outer `None`) or not current (inner) alike.
    Ok(stored.flatten().map(|updated| (updated, carried)))
}

/// The page for a failure; the details go to the log only.
///
/// # Arguments
/// * `error` - what failed.
/// * `brand` - for the page.
///
/// # Returns
/// The busy page (503, "try again") for [`OkfError::Busy`]: a Redis failure
/// (spec Decision 16) or a database deadlock, as on the rest of the HTTP
/// API. A 500 page for anything else.
#[tracing::instrument(level = "trace", skip_all)]
fn server_error(error: &OkfError, brand: &Brand) -> Response {
    split_event!(
        error,
        { kind = error.kind(), },
        { %error, },
        "consent failed"
    );
    if matches!(error, OkfError::Busy { .. }) {
        return consent::error_page(StatusCode::SERVICE_UNAVAILABLE, BUSY, brand);
    }
    consent::error_page(
        StatusCode::INTERNAL_SERVER_ERROR,
        "The server could not complete sign-in.",
        brand,
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use axum::body::Body;
    use axum::http::{HeaderMap, Request, StatusCode};
    use chrono::Utc;
    use tokio_util::sync::CancellationToken;
    use tower::ServiceExt;

    use super::{chosen_levels, Choice, CONSENT_PAGE, MAX_CARRIED, TOO_MANY_CARRIED};
    use crate::http::api::testing::Browser;
    use crate::http::console_session::COOKIE;
    use crate::labels::directory::Labels;
    use crate::labels::DisplayName;
    use crate::mcp::testing::TestApp;
    use crate::oauth::pkce;
    use crate::oauth::state::code_key;
    use crate::oauth::testing::{
        authorize_query, authorize_query_for, form, forwarded, hidden, hidden_inputs,
        insert_loopback_client, oauth_app, send, send_as, sign_in_code, user_authorize_query,
        CLIENT_ID, LOOPBACK_CLIENT_ID, LOOPBACK_ON_PORT, PUBLIC_URL, REDIRECT_URI, VERIFIER,
    };
    use crate::store::audit::AuditActor;
    use crate::store::ceiling::NewEntitlement;
    use crate::store::ceiling::{self, Ceiling};
    use crate::store::keys::{self as key_rows, Access, KeyKind};
    use crate::store::oauth::codes as code_rows;
    use crate::store::oauth::codes::SignInCodeRecord;
    use crate::store::settings::{self, SettingKey};
    use crate::store::teams::{self, NewTeam};
    use crate::store::user_sessions;
    use crate::store::users::{self, NewUser};

    /// Task 1b: a refused client's id is caller text, so it reaches stderr
    /// only, and the exported event keeps the error's code.
    #[test]
    fn a_refused_client_id_reaches_stderr_and_not_otlp() {
        use crate::telemetry::config::LogFormat;
        use crate::telemetry::subscriber::capture::observe;

        const SENTINEL: &str = "zq-sentinel-7f3a-client";
        let observed = observe("trace", "trace", "trace", LogFormat::Text, || {
            // A one-thread runtime keeps every task on this thread, under
            // the scoped subscriber.
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            runtime.block_on(async {
                let test = oauth_app().await;
                let query = authorize_query_for(SENTINEL, REDIRECT_URI);
                let (status, _, _) =
                    send(&test, "GET", &format!("/oauth/authorize?{query}"), None).await;
                assert_eq!(status, StatusCode::BAD_REQUEST);
            });
        });
        let exported = observed.exported_text();
        assert!(exported.contains("refused an OAuth client"), "{exported}");
        assert!(!exported.contains(SENTINEL), "{exported}");
        assert!(observed.stderr.contains(SENTINEL), "{}", observed.stderr);
    }

    /// Replaces one parameter of the valid query.
    fn query_with(name: &str, value: &str) -> String {
        authorize_query()
            .split('&')
            .map(|pair| match pair.split_once('=') {
                Some((key, _)) if key == name => form(&[(name, value)]),
                _ => pair.to_string(),
            })
            .collect::<Vec<_>>()
            .join("&")
    }

    /// The valid query without one parameter.
    fn query_without(name: &str) -> String {
        authorize_query()
            .split('&')
            .filter(|pair| !pair.starts_with(&format!("{name}=")))
            .collect::<Vec<_>>()
            .join("&")
    }

    fn location(headers: &HeaderMap) -> url::Url {
        url::Url::parse(headers["location"].to_str().unwrap()).unwrap()
    }

    fn param(url: &url::Url, name: &str) -> Option<String> {
        url.query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    }

    /// Runs step 1 and returns `(pending, csrf)`.
    async fn start(test: &TestApp) -> (String, String) {
        start_from(test, &authorize_query()).await
    }

    /// Runs step 1 for `query` and returns `(pending, csrf)`.
    async fn start_from(test: &TestApp, query: &str) -> (String, String) {
        let (_, _, page) = send(test, "GET", &format!("/oauth/authorize?{query}"), None).await;
        (hidden(&page, "pending"), hidden(&page, "csrf"))
    }

    async fn post(test: &TestApp, pairs: &[(&str, &str)]) -> (StatusCode, HeaderMap, String) {
        send(test, "POST", "/oauth/authorize", Some(form(pairs))).await
    }

    async fn oauth_grants(test: &TestApp) -> usize {
        key_rows::list(test.app.accounts().executor())
            .await
            .unwrap()
            .iter()
            .filter(|row| row.kind == KeyKind::OAuth)
            .count()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_valid_request_shows_step_one_for_the_client() {
        let test = oauth_app().await;
        let (status, headers, page) = send(
            &test,
            "GET",
            &format!("/oauth/authorize?{}", authorize_query()),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(page.contains("Example Client"));
        assert!(page.contains("returns to client.example.com"));
        assert!(page.contains("unverified"), "a DCR client");
        assert!(headers["content-security-policy"]
            .to_str()
            .unwrap()
            .ends_with("form-action 'self';"));
        assert_eq!(headers["cache-control"], "no-store");
        assert_eq!(headers["referrer-policy"], "no-referrer");
        assert_eq!(headers["x-content-type-options"], "nosniff");
        assert!(!hidden(&page, "pending").is_empty());
        assert!(!hidden(&page, "csrf").is_empty());
        assert!(page.contains("id=\"consent-code-form\""));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn before_the_redirect_uri_is_trusted_errors_never_redirect() {
        let test = oauth_app().await;
        for query in [
            query_with("client_id", "no-such-client"),
            query_without("client_id"),
            query_with("redirect_uri", "https://attacker.example.com/callback"),
            query_with("redirect_uri", &format!("{REDIRECT_URI}/extra")),
            query_with("redirect_uri", &format!("{REDIRECT_URI}?x=1")),
            query_without("redirect_uri"),
            // Later errors too: the untrusted redirect URI wins.
            format!(
                "{}&response_type=token",
                query_with("redirect_uri", "https://attacker.example.com/callback")
                    .replace("response_type=code", "")
            ),
            "%zz=%zz&&=".to_string(),
        ] {
            let (status, headers, page) =
                send(&test, "GET", &format!("/oauth/authorize?{query}"), None).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{query}");
            assert!(headers.get("location").is_none(), "{query}");
            assert!(page.contains("Cannot connect"), "{query}");
            assert!(!page.contains("attacker.example.com"), "{query}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_portless_loopback_registration_accepts_any_port_and_redirects_there() {
        let test = oauth_app().await;
        insert_loopback_client(&test).await;
        let code = sign_in_code(&test, Access::Read).await;

        let (status, _, page) = send(
            &test,
            "GET",
            &format!(
                "/oauth/authorize?{}",
                authorize_query_for(LOOPBACK_CLIENT_ID, LOOPBACK_ON_PORT)
            ),
            None,
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{page}");
        let (pending, csrf) = (hidden(&page, "pending"), hidden(&page, "csrf"));
        let (status, _, summary) = post(
            &test,
            &[
                ("pending", &pending),
                ("csrf", &csrf),
                ("action", "code"),
                ("sign_in_code", &code),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, headers, body) = approve_from(&test, &summary).await;
        let target = forwarded(status, &headers, &body);
        assert_eq!(target.port(), Some(54123));
        assert!(target.as_str().starts_with(LOOPBACK_ON_PORT));
        let issued = param(&target, "code").expect("a code");
        let stored = test
            .app
            .oauth()
            .expect("oauth on")
            .codes
            .update(&code_key(&issued), Instant::now(), |code| code.clone())
            .await
            .unwrap()
            .expect("the code is stored");
        assert_eq!(stored.redirect_uri, LOOPBACK_ON_PORT, "kept with its port");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_loopback_registration_refuses_anything_but_another_port() {
        let test = oauth_app().await;
        insert_loopback_client(&test).await;
        for redirect_uri in [
            "http://localhost:54123/other",
            "http://127.0.0.1:54123/callback",
            "https://localhost:54123/callback",
            "http://localhost.example.com:54123/callback",
        ] {
            let query = authorize_query_for(LOOPBACK_CLIENT_ID, redirect_uri);

            let (status, headers, page) =
                send(&test, "GET", &format!("/oauth/authorize?{query}"), None).await;

            assert_eq!(status, StatusCode::BAD_REQUEST, "{redirect_uri}");
            assert!(headers.get("location").is_none(), "{redirect_uri}");
            assert!(page.contains("Cannot connect"), "{redirect_uri}");
        }
        // An `https` registration's port stays exact.
        let query = query_with("redirect_uri", "https://client.example.com:8443/callback");
        let (status, headers, _) =
            send(&test, "GET", &format!("/oauth/authorize?{query}"), None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(headers.get("location").is_none());
    }

    /// Issue #1011: a refused `resource` says how it differed on the
    /// authorize span, and the value is nowhere in what was exported.
    #[tokio::test]
    async fn a_refused_resource_says_how_it_differed_on_the_authorize_span() {
        use crate::telemetry::config::LogFormat;
        use crate::telemetry::subscriber::capture::{attribute, Observer};

        let test = oauth_app().await;
        let marker = "zq-marker-9f3";
        let cases = [
            (format!("{PUBLIC_URL}/mcp/"), "trailing_slash"),
            (PUBLIC_URL.to_string(), "origin"),
            (format!("{PUBLIC_URL}/{marker}"), "other_path"),
            (format!("https://{marker}.example.net/mcp"), "other_host"),
            (marker.to_string(), "unparseable"),
        ];
        for (sent, expected) in cases {
            let observer = Observer::start("off", "debug", "off", LogFormat::Text);
            let query = query_with("resource", &sent);
            let (status, _, _) =
                send(&test, "GET", &format!("/oauth/authorize?{query}"), None).await;
            let observed = observer.finish();

            assert_eq!(status, StatusCode::SEE_OTHER, "{expected}");
            let spans = observed.spans_named("authorize");
            let recorded: Vec<_> = spans
                .iter()
                .filter_map(|span| attribute(span, "okf.resource"))
                .map(ToString::to_string)
                .collect();
            assert_eq!(recorded, [expected], "{expected}");
            assert!(!observed.exported_text().contains(marker), "{expected}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn after_the_redirect_uri_is_trusted_errors_go_back_to_the_client() {
        let test = oauth_app().await;
        for (query, expected) in [
            (
                query_with("response_type", "token"),
                "unsupported_response_type",
            ),
            (query_without("response_type"), "unsupported_response_type"),
            (
                query_with("code_challenge_method", "plain"),
                "invalid_request",
            ),
            (query_without("code_challenge_method"), "invalid_request"),
            (query_with("code_challenge", "short"), "invalid_request"),
            (query_without("code_challenge"), "invalid_request"),
            (
                query_with("resource", "https://other.example.com/mcp"),
                "invalid_target",
            ),
        ] {
            let (status, headers, _) =
                send(&test, "GET", &format!("/oauth/authorize?{query}"), None).await;
            assert_eq!(status, StatusCode::SEE_OTHER, "{query}");
            let target = location(&headers);
            assert!(target.as_str().starts_with(REDIRECT_URI), "{query}");
            assert_eq!(
                param(&target, "error").as_deref(),
                Some(expected),
                "{query}"
            );
            assert_eq!(param(&target, "state").as_deref(), Some("xyz"));
            assert_eq!(param(&target, "iss").as_deref(), Some(PUBLIC_URL));
        }
    }

    /// RFC 8707 makes `resource` optional at authorize, and some clients
    /// (Crush) strip it from the browser URL and send it only at the token
    /// endpoint. A request without it is for this server's one resource:
    /// the code and the tokens are bound to `/mcp` as usual.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_authorize_request_without_resource_is_for_the_mcp_endpoint() {
        let test = oauth_app().await;
        let code = sign_in_code(&test, Access::Write).await;
        let (pending, csrf) = start_from(&test, &query_without("resource")).await;
        let (status, _, summary) = post(
            &test,
            &[
                ("pending", &pending),
                ("csrf", &csrf),
                ("action", "code"),
                ("sign_in_code", &code),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, headers, body) = approve_from(&test, &summary).await;
        let target = forwarded(status, &headers, &body);
        let issued = param(&target, "code").expect("a code");
        let oauth = test.app.oauth().expect("oauth on");
        let waiting = oauth
            .codes
            .update(&code_key(&issued), Instant::now(), |code| code.clone())
            .await
            .unwrap()
            .expect("the code is stored");
        assert_eq!(waiting.resource, oauth.settings.resource);
        let (access, _) = crate::oauth::testing::exchange_for_tokens(&test, &issued).await;
        assert!(access.starts_with("okfat_"), "{access}");
    }

    /// Design, Auth row: the authorize decision spans carry `method` and
    /// `outcome`, and none of what the person typed or was given.
    #[tokio::test]
    async fn the_authorize_spans_carry_method_and_outcome_and_no_code() {
        use crate::telemetry::config::LogFormat;
        use crate::telemetry::subscriber::capture::{attribute, Observer};

        let test = oauth_app().await;
        let code = sign_in_code(&test, Access::Write).await;
        let (pending, csrf) = start(&test).await;
        let observer = Observer::start("off", "debug", "off", LogFormat::Text);

        // A wrong code, then the right one, then the approval.
        let (_, _, _) = post(
            &test,
            &[
                ("pending", &pending),
                ("csrf", &csrf),
                ("action", "code"),
                ("sign_in_code", "zq-wrong-code"),
            ],
        )
        .await;
        let (_, _, summary) = post(
            &test,
            &[
                ("pending", &pending),
                ("csrf", &csrf),
                ("action", "code"),
                ("sign_in_code", &code),
            ],
        )
        .await;
        let (status, headers, body) = approve_from(&test, &summary).await;
        let target = forwarded(status, &headers, &body);
        let issued = param(&target, "code").expect("a code");
        let observed = observer.finish();

        let outcomes = |name: &str| -> Vec<String> {
            observed
                .spans_named(name)
                .iter()
                .map(|span| {
                    assert_eq!(
                        attribute(span, "method")
                            .map(ToString::to_string)
                            .as_deref(),
                        Some("oauth_authorize"),
                        "{name}"
                    );
                    let all: Vec<_> = span
                        .attributes
                        .iter()
                        .filter(|attribute| attribute.key.as_str() == "outcome")
                        .collect();
                    assert_eq!(all.len(), 1, "one outcome on {name}: {all:?}");
                    all[0].value.to_string()
                })
                .collect()
        };
        assert_eq!(outcomes("check_code"), ["failed", "ok"]);
        assert_eq!(outcomes("finish"), ["ok"]);
        let exported = observed.exported_text();
        for secret in [code.as_str(), "zq-wrong-code", issued.as_str()] {
            assert!(!exported.contains(secret), "{secret} in {exported}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_code_then_approve_creates_a_grant_and_forwards_with_a_code() {
        let test = oauth_app().await;
        let code = sign_in_code(&test, Access::Write).await;
        let (pending, csrf) = start(&test).await;

        let (status, headers, summary) = post(
            &test,
            &[
                ("pending", &pending),
                ("csrf", &csrf),
                ("action", "code"),
                ("sign_in_code", &code),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        // The catalog and the user are shown by label (neither has a
        // display name here), never by slug.
        assert!(
            summary.contains("<strong>write</strong> access to <strong>Untitled catalog</strong>")
        );
        assert!(summary
            .contains("as <strong>Unnamed user</strong> (actor <strong>example-actor</strong>)"));
        assert!(headers["content-security-policy"]
            .to_str()
            .unwrap()
            .ends_with("form-action 'self';"));

        let (status, headers, body) = approve_from(&test, &summary).await;
        let target = forwarded(status, &headers, &body);
        let issued = param(&target, "code").expect("a code");
        assert!(issued.starts_with("okfac_"));
        assert_eq!(param(&target, "state").as_deref(), Some("xyz"));
        assert_eq!(param(&target, "iss").as_deref(), Some(PUBLIC_URL));

        // The grant exists, is in the key cache, and the code is waiting.
        let oauth = test.app.oauth().expect("oauth on");
        let waiting = oauth
            .codes
            .update(&code_key(&issued), Instant::now(), |code| code.clone())
            .await
            .unwrap()
            .expect("the code is stored");
        assert!(!waiting.used);
        assert!(pkce::verify(&waiting.code_challenge, VERIFIER));
        assert_eq!(waiting.redirect_uri, REDIRECT_URI);
        assert_eq!(waiting.resource, oauth.settings.resource);
        let principal = test
            .app
            .find_grant(&waiting.grant_id)
            .await
            .unwrap()
            .expect("cached grant");
        assert_eq!(principal.actor, "example-actor");
        assert_eq!(principal.access, Access::Write);
        let rows = key_rows::list(test.app.accounts().executor())
            .await
            .unwrap();
        let grant = rows.iter().find(|row| row.id == waiting.grant_id).unwrap();
        assert_eq!(grant.kind, KeyKind::OAuth);
        assert_eq!(grant.client_id.as_deref(), Some(CLIENT_ID));
        // The sign-in code is spent.
        let live = code_rows::find_live(
            test.app.accounts().executor(),
            &code,
            test.app.clock().now(),
        )
        .await
        .unwrap();
        assert!(live.is_none());

        // The pending request is spent: a second approve finds nothing.
        let (status, _, _) = approve_from(&test, &summary).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_sign_in_code_is_consumed_only_on_approve() {
        let test = oauth_app().await;
        let code = sign_in_code(&test, Access::Read).await;
        let (pending, csrf) = start(&test).await;
        let (status, _, _) = post(
            &test,
            &[
                ("pending", &pending),
                ("csrf", &csrf),
                ("action", "code"),
                ("sign_in_code", &code),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, headers, body) = post(
            &test,
            &[("pending", &pending), ("csrf", &csrf), ("action", "deny")],
        )
        .await;
        assert_eq!(
            param(&forwarded(status, &headers, &body), "error").as_deref(),
            Some("access_denied")
        );

        // Step 2 then Deny left the code live and made no grant.
        let live = code_rows::find_live(
            test.app.accounts().executor(),
            &code,
            test.app.clock().now(),
        )
        .await
        .unwrap();
        assert!(live.is_some(), "the code survives a deny");
        assert_eq!(oauth_grants(&test).await, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn deny_forwards_with_access_denied() {
        let test = oauth_app().await;
        let (pending, csrf) = start(&test).await;
        let (status, headers, body) = post(
            &test,
            &[("pending", &pending), ("csrf", &csrf), ("action", "deny")],
        )
        .await;
        let target = forwarded(status, &headers, &body);
        assert!(target.as_str().starts_with(REDIRECT_URI));
        assert_eq!(param(&target, "error").as_deref(), Some("access_denied"));
        assert_eq!(param(&target, "state").as_deref(), Some("xyz"));
        assert_eq!(param(&target, "iss").as_deref(), Some(PUBLIC_URL));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_wrong_csrf_token_is_refused_and_keeps_the_request() {
        let test = oauth_app().await;
        let code = sign_in_code(&test, Access::Read).await;
        let (pending, csrf) = start(&test).await;
        for forged in ["forged", "", &csrf[..csrf.len() - 1]] {
            for action in ["deny", "code", "approve"] {
                let (status, headers, page) = post(
                    &test,
                    &[
                        ("pending", &pending),
                        ("csrf", forged),
                        ("action", action),
                        ("sign_in_code", &code),
                    ],
                )
                .await;
                assert_eq!(status, StatusCode::BAD_REQUEST, "{action} {forged:?}");
                assert!(headers.get("location").is_none());
                assert!(page.contains("Cannot connect"));
            }
        }
        // The real form still works: a forger cannot cancel someone's request.
        let (status, headers, body) = post(
            &test,
            &[("pending", &pending), ("csrf", &csrf), ("action", "deny")],
        )
        .await;
        forwarded(status, &headers, &body);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn another_requests_csrf_token_does_not_work() {
        let test = oauth_app().await;
        let (first, _) = start(&test).await;
        let (_, second_csrf) = start(&test).await;
        let (status, headers, _) = post(
            &test,
            &[
                ("pending", &first),
                ("csrf", &second_csrf),
                ("action", "deny"),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(headers.get("location").is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn five_bad_codes_end_the_request() {
        let test = oauth_app().await;
        let (pending, csrf) = start(&test).await;
        let bad = [
            ("pending", pending.as_str()),
            ("csrf", csrf.as_str()),
            ("action", "code"),
            ("sign_in_code", "okfsc_wrong"),
        ];
        for attempt in 1..=4 {
            let (status, _, page) = post(&test, &bad).await;
            assert_eq!(status, StatusCode::OK, "attempt {attempt}");
            assert!(page.contains("That sign-in code is not valid"));
        }
        let (status, _, page) = post(&test, &bad).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(page.contains("Too many"));
        // Even a good code no longer helps: the request is gone.
        let code = sign_in_code(&test, Access::Read).await;
        let (status, _, _) = post(
            &test,
            &[
                ("pending", &pending),
                ("csrf", &csrf),
                ("action", "code"),
                ("sign_in_code", &code),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn consent_posts_are_rate_limited() {
        let test = oauth_app().await;
        let (pending, csrf) = start(&test).await;
        let oauth = test.app.oauth().expect("oauth on");
        // Drain the bucket at an instant a minute ahead: the handler's own
        // (earlier) instants then refill nothing, so the test is exact.
        let later = Instant::now() + Duration::from_secs(60);
        while oauth.consent_limit.try_take(later).await {}
        let (status, headers, page) = post(
            &test,
            &[("pending", &pending), ("csrf", &csrf), ("action", "deny")],
        )
        .await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert!(headers.get("location").is_none());
        assert!(page.contains("Too many sign-in attempts"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn approve_without_an_accepted_code_goes_back_to_step_one() {
        let test = oauth_app().await;
        let (pending, csrf) = start(&test).await;
        let (status, _, page) = post(
            &test,
            &[
                ("pending", &pending),
                ("csrf", &csrf),
                ("action", "approve"),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(page.contains(r#"id="sign-in-code""#));
        assert_eq!(oauth_grants(&test).await, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_code_redeemed_elsewhere_meanwhile_is_refused_at_approve() {
        let test = oauth_app().await;
        let code = sign_in_code(&test, Access::Read).await;
        let (first, first_csrf) = start(&test).await;
        let (second, second_csrf) = start(&test).await;
        let mut summaries = Vec::new();
        for (pending, csrf) in [(&first, &first_csrf), (&second, &second_csrf)] {
            let (status, _, summary) = post(
                &test,
                &[
                    ("pending", pending),
                    ("csrf", csrf),
                    ("action", "code"),
                    ("sign_in_code", &code),
                ],
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            summaries.push(summary);
        }
        let (status, headers, body) = approve_from(&test, &summaries[0]).await;
        forwarded(status, &headers, &body);
        let (status, _, page) = approve_from(&test, &summaries[1]).await;
        assert_eq!(status, StatusCode::OK);
        assert!(page.contains("That sign-in code is not valid"));
        assert_eq!(oauth_grants(&test).await, 1);
    }

    /// Posts the Approve button of a step-2 page's form, with exactly the
    /// hidden fields that page carries.
    async fn approve_from(test: &TestApp, summary: &str) -> (StatusCode, HeaderMap, String) {
        let fields = hidden_inputs(summary, "consent-approve-form");
        let mut pairs: Vec<(&str, &str)> = fields
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        pairs.push(("action", "approve"));
        post(test, &pairs).await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_older_summary_cannot_approve_a_code_entered_after_it() {
        let test = oauth_app().await;
        let read_code = sign_in_code(&test, Access::Read).await;
        let write_code = sign_in_code(&test, Access::Write).await;
        let (pending, csrf) = start(&test).await;
        let mut summaries = Vec::new();
        // Two copies of step 1 under one pending request: A, then B.
        for code in [&read_code, &write_code] {
            let (status, _, summary) = post(
                &test,
                &[
                    ("pending", &pending),
                    ("csrf", &csrf),
                    ("action", "code"),
                    ("sign_in_code", code),
                ],
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            summaries.push(summary);
        }
        assert!(summaries[0].contains("<strong>read</strong> access"));
        assert!(summaries[1].contains("<strong>write</strong> access"));

        // A's page said read-only: its Approve must not redeem B (or A).
        let (status, headers, page) = approve_from(&test, &summaries[0]).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert!(headers.get("location").is_none());
        assert!(page.contains(r#"id="sign-in-code""#), "back at step 1");
        assert_eq!(oauth_grants(&test).await, 0);
        let now = test.app.clock().now();
        for code in [&read_code, &write_code] {
            let live = code_rows::find_live(test.app.accounts().executor(), code, now)
                .await
                .unwrap();
            assert!(live.is_some(), "neither code is redeemed");
        }

        // A crafted Approve with no summary token is refused the same way.
        let (status, _, page) = post(
            &test,
            &[
                ("pending", &pending),
                ("csrf", &csrf),
                ("action", "approve"),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(page.contains("out of date"));
        assert_eq!(oauth_grants(&test).await, 0);

        // B's own, current summary still approves, and grants write.
        let (status, headers, body) = approve_from(&test, &summaries[1]).await;
        let issued = param(&forwarded(status, &headers, &body), "code").expect("a code");
        let oauth = test.app.oauth().expect("oauth on");
        let waiting = oauth
            .codes
            .update(&code_key(&issued), Instant::now(), |code| code.clone())
            .await
            .unwrap()
            .expect("the code is stored");
        let principal = test
            .app
            .find_grant(&waiting.grant_id)
            .await
            .unwrap()
            .expect("cached grant");
        assert_eq!(principal.access, Access::Write);
        let read_live = code_rows::find_live(test.app.accounts().executor(), &read_code, now)
            .await
            .unwrap();
        assert!(read_live.is_some(), "A was never redeemed");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_malformed_form_is_an_error_page() {
        let test = oauth_app().await;
        for body in ["action=code", "pending=x&csrf=y&action=dance"] {
            let (status, headers, page) =
                send(&test, "POST", "/oauth/authorize", Some(body.into())).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert!(headers.get("location").is_none());
            assert!(page.contains("Cannot connect"));
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn consent_pages_name_the_product_and_show_the_contact() {
        let test = oauth_app().await;
        crate::invalidate::testing::applied(&test.app, async {
            for (key, value) in [
                (SettingKey::ProductName, "Acme Notes"),
                (SettingKey::SupportContact, "help@example.com"),
            ] {
                settings::set(
                    test.app.accounts().executor(),
                    key,
                    value,
                    &AuditActor::cli(),
                    Utc::now(),
                )
                .await
                .unwrap();
            }
        })
        .await;

        let (_, _, unknown_client) = send(&test, "GET", "/oauth/authorize", None).await;
        let (_, _, malformed) = send(
            &test,
            "POST",
            "/oauth/authorize",
            Some("action=code".into()),
        )
        .await;

        for page in [unknown_client, malformed] {
            assert!(
                page.contains("<title>Cannot connect - Acme Notes</title>"),
                "{page}"
            );
            assert!(
                page.contains("Questions? Contact help@example.com</p>"),
                "{page}"
            );
            assert!(!page.contains("okf"), "{page}");
        }
    }

    // ---- Consent as the signed-in user (Task 11) ----

    /// Creates user `name` with these entitlements and signs it in.
    async fn signed_in(test: &TestApp, name: &str, grants: &[(&str, Access)]) -> Browser {
        users::create(
            test.app.accounts().executor(),
            &NewUser {
                name,
                display_name: &crate::labels::DisplayName::default(),
            },
            &AuditActor::cli(),
            Utc::now(),
        )
        .await
        .expect("create the user");
        for (catalog, access) in grants {
            ceiling::entitle(
                test.app.accounts().executor(),
                &NewEntitlement {
                    user: name,
                    catalog,
                    access: *access,
                },
                &AuditActor::cli(),
                Utc::now(),
            )
            .await
            .expect("entitle");
        }
        Browser::session_for(test, name).await
    }

    /// The signed-in consent page for `session`.
    async fn user_page(test: &TestApp, session: &Browser) -> String {
        let (status, _, page) = send_as(
            test,
            "GET",
            &format!("/oauth/authorize?{}", user_authorize_query()),
            None,
            Some(session),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{page}");
        page
    }

    /// The pending id, CSRF token and nonce of a signed-in page, with
    /// `action=consent`, then `choices`.
    fn consent_form(page: &str, choices: &[(&str, &str)]) -> String {
        let (pending, csrf, summary) = (
            hidden(page, "pending"),
            hidden(page, "csrf"),
            hidden(page, "summary"),
        );
        let mut pairs = vec![
            ("pending", pending.as_str()),
            ("csrf", csrf.as_str()),
            ("summary", summary.as_str()),
            ("action", "consent"),
        ];
        pairs.extend_from_slice(choices);
        form(&pairs)
    }

    /// Posts `choices` from `page`'s consent form as `session`.
    async fn consent_as(
        test: &TestApp,
        session: Option<&Browser>,
        page: &str,
        choices: &[(&str, &str)],
    ) -> (StatusCode, HeaderMap, String) {
        send_as(
            test,
            "POST",
            "/oauth/authorize",
            Some(consent_form(page, choices)),
            session,
        )
        .await
    }

    /// The grant behind the authorization code `target` carries to the
    /// client.
    async fn grant_of(test: &TestApp, target: &url::Url) -> String {
        let issued = param(target, "code").expect("a code");
        test.app
            .oauth()
            .expect("oauth on")
            .codes
            .update(&code_key(&issued), Instant::now(), |code| {
                code.grant_id.clone()
            })
            .await
            .unwrap()
            .expect("the code is stored")
    }

    const READ_MAIN: &[(&str, &str)] = &[("grant.main", "on"), ("level.main", "read")];

    #[tokio::test(flavor = "multi_thread")]
    async fn without_a_session_authorize_sends_the_person_to_sign_in() {
        let test = oauth_app().await;

        let (status, headers, _) = send(
            &test,
            "GET",
            &format!("/oauth/authorize?{}", user_authorize_query()),
            None,
        )
        .await;

        assert_eq!(status, StatusCode::SEE_OTHER);
        let target = headers["location"].to_str().expect("a location");
        let parsed = url::Url::parse(&format!("{PUBLIC_URL}{target}")).expect("a URL");
        assert_eq!(parsed.path(), "/signin");
        let return_to = param(&parsed, "return_to").expect("a return_to");
        assert_eq!(
            return_to,
            format!("/oauth/authorize?{}", user_authorize_query())
        );
        // It survives `/signin`'s check, so sign-in comes back here.
        assert_eq!(
            crate::http::pages::safe_return_to(Some(&return_to)),
            return_to
        );
        assert_eq!(
            test.app.oauth().expect("oauth on").pending.len(),
            0,
            "nothing is stored before sign-in"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_sign_in_redirect_comes_after_the_request_is_validated() {
        let test = oauth_app().await;
        let bad_redirect = user_authorize_query().replace(
            "client.example.com%2Fcallback",
            "attacker.example.com%2Fcallback",
        );
        let bad_type = user_authorize_query().replace("response_type=code", "response_type=token");

        let (untrusted, untrusted_headers, _) = send(
            &test,
            "GET",
            &format!("/oauth/authorize?{bad_redirect}"),
            None,
        )
        .await;
        let (to_client, client_headers, _) =
            send(&test, "GET", &format!("/oauth/authorize?{bad_type}"), None).await;

        assert_eq!(untrusted, StatusCode::BAD_REQUEST);
        assert!(untrusted_headers.get("location").is_none());
        assert_eq!(to_client, StatusCode::SEE_OTHER);
        let target = location(&client_headers);
        assert!(target.as_str().starts_with(REDIRECT_URI));
        assert_eq!(
            param(&target, "error").as_deref(),
            Some("unsupported_response_type")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_sign_in_code_path_needs_no_session_and_ignores_one() {
        let test = oauth_app().await;
        let alice = signed_in(&test, "alice", &[("main", Access::Read)]).await;
        let uri = format!("/oauth/authorize?{}", authorize_query());

        let (anonymous, _, code_page) = send(&test, "GET", &uri, None).await;
        let (signed, _, also_code_page) = send_as(&test, "GET", &uri, None, Some(&alice)).await;

        assert_eq!(anonymous, StatusCode::OK);
        assert!(code_page.contains("id=\"consent-code-form\""));
        assert_eq!(signed, StatusCode::OK);
        assert!(also_code_page.contains("id=\"consent-code-form\""));
        assert!(!also_code_page.contains("consent-user-form"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_signed_in_person_sees_only_their_ceiling() {
        let test = oauth_app().await;
        let alice = signed_in(&test, "alice", &[("main", Access::Read)]).await;

        let page = user_page(&test, &alice).await;

        assert!(page.contains("id=\"consent-user-form\""));
        // No display name and no email: a label, never the slug.
        assert!(page.contains("<strong id=\"consent-user\">Unnamed user</strong>"));
        assert!(!page.contains(">alice<"), "the slug is not shown as a name");
        assert!(page.contains("name=\"grant.main\""));
        assert!(
            !page.contains("grant.other"),
            "other is outside the ceiling"
        );
        assert!(
            !page.contains("<option value=\"write\">"),
            "main is read-only for alice"
        );
        assert!(page.contains("id=\"consent-use-code\""));
        assert!(page.contains(&format!(
            "href=\"/oauth/authorize?{}&amp;signin=code\"",
            user_authorize_query().replace('&', "&amp;")
        )));
        assert!(!hidden(&page, "summary").is_empty());
    }

    /// Sets display names straight in SQL and reloads the key cache the
    /// way any label change does (a bump of `admin_generation`, then a poll).
    async fn relabel(test: &TestApp, user: (&str, &str), catalogs: [(&str, &str); 2]) {
        let pool = test.app.accounts().executor();
        sqlx::query("UPDATE users SET display_name = $1 WHERE name = $2")
            .bind(user.1)
            .bind(user.0)
            .execute(pool)
            .await
            .unwrap();
        for (slug, display_name) in catalogs {
            sqlx::query("UPDATE catalogs SET display_name = $1 WHERE name = $2")
                .bind(display_name)
                .bind(slug)
                .execute(pool)
                .await
                .unwrap();
        }
        // A raw update sends no change message: a catch-up (what a
        // reconnect does) makes the label cache read the labels again.
        test.app.apply_reconnect().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_signed_in_page_shows_labels_but_keeps_slugs_in_ids_names_and_values() {
        let test = oauth_app().await;
        let alice = signed_in(
            &test,
            "alice",
            &[("main", Access::Write), ("other", Access::Read)],
        )
        .await;
        relabel(
            &test,
            ("alice", "Alice <b>Example</b>"),
            [("main", "Acme \"Handbook\""), ("other", "Field Notes")],
        )
        .await;

        let page = user_page(&test, &alice).await;

        assert!(
            page.contains(r#"<strong id="consent-user">Alice &lt;b&gt;Example&lt;/b&gt;</strong>"#),
            "{page}"
        );
        assert!(
            page.contains(
                r#"<input type="checkbox" name="grant.main" value="on" id="consent-catalog-main"> Acme &quot;Handbook&quot;</label>"#
            ),
            "{page}"
        );
        assert!(
            page.contains(r#"aria-label="Access to Acme &quot;Handbook&quot;""#),
            "{page}"
        );
        assert!(page.contains(r#"name="level.other""#), "{page}");
        assert!(
            page.contains(r#"<option value="main">Acme &quot;Handbook&quot;</option>"#),
            "{page}"
        );
        assert!(
            page.contains(r#"<option value="other">Field Notes</option>"#),
            "{page}"
        );
        assert!(!page.contains("<b>Example"), "markup is escaped: {page}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn catalogs_sharing_a_label_are_told_apart_by_their_owner_team() {
        let test = oauth_app().await;
        let alice = signed_in(
            &test,
            "alice",
            &[("main", Access::Read), ("other", Access::Read)],
        )
        .await;
        // `other` moves to a second team, so the shared label gets a suffix.
        teams::create(
            test.app.accounts().executor(),
            &NewTeam {
                name: "acme",
                display_name: &DisplayName::parse("Acme Team").unwrap(),
                owner: crate::db::TEST_USER,
            },
            &AuditActor::cli(),
            Utc::now(),
        )
        .await
        .unwrap();
        sqlx::query(
            "UPDATE catalogs SET owner_team_id = (SELECT id FROM teams WHERE name = 'acme') \
             WHERE name = 'other'",
        )
        .execute(test.app.accounts().executor())
        .await
        .unwrap();
        relabel(
            &test,
            ("alice", "Alice"),
            [("main", "Handbook"), ("other", "Handbook")],
        )
        .await;

        let page = user_page(&test, &alice).await;

        assert!(page.contains("Handbook (Acme Team)</option>"), "{page}");
        let bare = page.matches("Handbook</option>").count();
        assert_eq!(bare, 0, "no option shows the bare shared label: {page}");
        assert!(page.contains(r#"value="main">Handbook ("#), "{page}");
        assert!(page.contains(r#"name="grant.other""#), "{page}");
    }

    /// A live code record granting `catalogs` (or every catalog) with
    /// `default_catalog`, for the pure `summary` tests.
    fn code_record(
        all_catalogs: bool,
        catalogs: &[&str],
        default_catalog: Option<&str>,
    ) -> SignInCodeRecord {
        SignInCodeRecord {
            id: "c-1".to_string(),
            name: "example".to_string(),
            access: Access::Read,
            all_catalogs,
            catalogs: catalogs.iter().map(ToString::to_string).collect(),
            default_catalog: default_catalog.map(ToString::to_string),
            actor: "example-actor".to_string(),
            created_at: Utc::now(),
            expires_at: Utc::now(),
            redeemed_at: None,
            grant_id: None,
            revoked_at: None,
            user: None,
        }
    }

    /// Two catalogs both labelled `Notes`, owned by teams `Acme` and `Other
    /// team`.
    fn notes_labels() -> Labels {
        use crate::labels::directory::{CatalogRow, TeamRow};
        let team = |name: &str, display: &str| TeamRow {
            name: name.to_string(),
            display_name: display.to_string(),
            personal_of: None,
        };
        let catalog = |name: &str, owner: &str| CatalogRow {
            name: name.to_string(),
            display_name: "Notes".to_string(),
            description: String::new(),
            owner_team: Some(owner.to_string()),
        };
        Labels::from_rows(
            Vec::new(),
            vec![team("t-a", "Acme"), team("t-b", "Other team")],
            vec![catalog("notes-a", "t-a"), catalog("notes-b", "t-b")],
        )
    }

    #[test]
    fn the_summarys_default_catalog_is_told_apart_like_the_list() {
        let labels = notes_labels();

        let listed = super::summary(
            &code_record(false, &["notes-a", "notes-b"], Some("notes-b")),
            &labels,
        );
        let every = super::summary(&code_record(true, &[], Some("notes-b")), &labels);

        assert_eq!(listed.catalogs, "Notes (Acme), Notes (Other team)");
        assert_eq!(
            listed.default_catalog.as_deref(),
            Some("Notes (Other team)")
        );
        // An all-catalog grant lists none, so the default is shown alone,
        // by its label (the fallback for a grant that names no catalogs).
        assert_eq!(every.catalogs, "every catalog");
        assert_eq!(every.default_catalog.as_deref(), Some("Notes"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_sign_in_code_summary_names_the_user_and_catalogs_by_label() {
        let test = oauth_app().await;
        relabel(
            &test,
            (crate::db::TEST_USER, "Tess <i>Tester</i>"),
            [("main", "Acme Handbook"), ("other", "Field Notes")],
        )
        .await;
        let code = sign_in_code(&test, Access::Read).await;
        let (_, _, page) = send(
            &test,
            "GET",
            &format!("/oauth/authorize?{}", authorize_query()),
            None,
        )
        .await;

        let (status, _, summary) = post(
            &test,
            &[
                ("pending", &hidden(&page, "pending")),
                ("csrf", &hidden(&page, "csrf")),
                ("action", "code"),
                ("sign_in_code", &code),
            ],
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{summary}");
        assert!(
            summary.contains("<strong>Acme Handbook</strong>"),
            "{summary}"
        );
        assert!(
            summary.contains("as <strong>Tess &lt;i&gt;Tester&lt;/i&gt;</strong>"),
            "{summary}"
        );
        assert!(
            summary.contains("default catalog <strong>Acme Handbook</strong>"),
            "{summary}"
        );
        assert!(!summary.contains("<strong>main</strong>"), "{summary}");
        assert!(!summary.contains("<i>Tester"), "{summary}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn approving_as_the_person_creates_a_capped_self_issued_grant() {
        let test = oauth_app().await;
        let alice = signed_in(
            &test,
            "alice",
            &[("main", Access::Write), ("other", Access::Read)],
        )
        .await;
        let page = user_page(&test, &alice).await;

        let (status, headers, body) = consent_as(
            &test,
            Some(&alice),
            &page,
            &[
                ("grant.main", "on"),
                ("level.main", "write"),
                ("level.other", "read"),
                ("default_catalog", "main"),
            ],
        )
        .await;

        let target = forwarded(status, &headers, &body);
        assert!(target.as_str().starts_with(REDIRECT_URI));
        assert_eq!(param(&target, "state").as_deref(), Some("xyz"));
        let grant = grant_of(&test, &target).await;
        let principal = test
            .app
            .find_grant(&grant)
            .await
            .unwrap()
            .expect("cached grant");
        assert_eq!(principal.user.as_deref(), Some("alice"));
        assert_eq!(principal.actor, "Example Client");
        assert_eq!(principal.level("main"), Some(Access::Write));
        assert_eq!(principal.level("other"), None, "not ticked");
        assert_eq!(principal.default_catalog.as_deref(), Some("main"));
        let record = key_rows::list(test.app.accounts().executor())
            .await
            .unwrap()
            .into_iter()
            .find(|row| row.id == grant)
            .unwrap();
        assert!(record.self_issued);
        assert_eq!(record.kind, KeyKind::OAuth);
        assert_eq!(record.client_id.as_deref(), Some(CLIENT_ID));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn lowering_the_ceiling_lowers_the_grant_after_a_reload() {
        let test = oauth_app().await;
        let alice = signed_in(&test, "alice", &[("main", Access::Write)]).await;
        let page = user_page(&test, &alice).await;
        let (status, headers, body) = consent_as(
            &test,
            Some(&alice),
            &page,
            &[
                ("grant.main", "on"),
                ("level.main", "write"),
                ("default_catalog", ""),
            ],
        )
        .await;
        let grant = grant_of(&test, &forwarded(status, &headers, &body)).await;
        let granted = test
            .app
            .find_grant(&grant)
            .await
            .unwrap()
            .expect("cached")
            .level("main");

        crate::invalidate::testing::applied(&test.app, async {
            ceiling::entitle(
                test.app.accounts().executor(),
                &NewEntitlement {
                    user: "alice",
                    catalog: "main",
                    access: Access::Read,
                },
                &AuditActor::cli(),
                Utc::now(),
            )
            .await
            .expect("lower");
        })
        .await;
        let lowered = test
            .app
            .find_grant(&grant)
            .await
            .unwrap()
            .expect("cached")
            .level("main");
        crate::invalidate::testing::applied(&test.app, async {
            ceiling::unentitle(
                test.app.accounts().executor(),
                "alice",
                "main",
                &AuditActor::cli(),
                Utc::now(),
            )
            .await
            .expect("remove");
        })
        .await;
        let removed = test
            .app
            .find_grant(&grant)
            .await
            .unwrap()
            .map(|principal| principal.level("main"));

        assert_eq!(granted, Some(Access::Write));
        assert_eq!(lowered, Some(Access::Read));
        // Outside the ceiling the grant reaches nothing (it may stay cached
        // with no catalogs, or drop out of the cache).
        assert!(matches!(removed, None | Some(None)), "{removed:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_tampered_consent_is_refused_and_grants_nothing() {
        let test = oauth_app().await;
        let alice = signed_in(&test, "alice", &[("main", Access::Read)]).await;
        let cases: [&[(&str, &str)]; 6] = [
            &[("grant.other", "on"), ("level.other", "read")],
            &[("grant.main", "on"), ("level.main", "write")],
            &[("grant.main", "on"), ("level.main", "admin")],
            &[("grant.main", "yes"), ("level.main", "read")],
            &[
                ("grant.main", "on"),
                ("level.main", "read"),
                ("sneaky", "1"),
            ],
            &[
                ("grant.main", "on"),
                ("level.main", "read"),
                ("level.other", "read"),
            ],
        ];
        for choices in cases {
            let page = user_page(&test, &alice).await;
            let (status, _, body) = consent_as(&test, Some(&alice), &page, choices).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{choices:?}");
            assert!(
                body.contains("did not match what was offered"),
                "{choices:?}"
            );
            // The request is over: the same form cannot be sent again.
            let (again, _, _) = consent_as(&test, Some(&alice), &page, READ_MAIN).await;
            assert_eq!(again, StatusCode::BAD_REQUEST, "{choices:?}");
        }
        // A forged consent nonce.
        let page = user_page(&test, &alice).await;
        let forged = form(&[
            ("pending", &hidden(&page, "pending")),
            ("csrf", &hidden(&page, "csrf")),
            ("summary", "forged"),
            ("action", "consent"),
            ("grant.main", "on"),
            ("level.main", "read"),
        ]);
        let (status, _, _) = send_as(
            &test,
            "POST",
            "/oauth/authorize",
            Some(forged),
            Some(&alice),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(oauth_grants(&test).await, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_consent_posted_by_another_signed_in_user_is_refused() {
        let test = oauth_app().await;
        let alice = signed_in(&test, "alice", &[("main", Access::Read)]).await;
        let bob = signed_in(&test, "bob", &[("main", Access::Read)]).await;
        let page = user_page(&test, &alice).await;

        let (status, _, body) = consent_as(&test, Some(&bob), &page, READ_MAIN).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("no longer signed in"), "{body}");
        assert_eq!(oauth_grants(&test).await, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_session_ended_after_the_page_cannot_approve() {
        let test = oauth_app().await;
        let alice = signed_in(&test, "alice", &[("main", Access::Read)]).await;
        let without_cookie = user_page(&test, &alice).await;
        let signed_out = user_page(&test, &alice).await;

        let (no_cookie, _, no_cookie_body) =
            consent_as(&test, None, &without_cookie, READ_MAIN).await;
        user_sessions::revoke(
            test.app.accounts().executor(),
            alice.cookie(COOKIE).expect("a session"),
        )
        .await
        .expect("sign out");
        let (ended, _, ended_body) = consent_as(&test, Some(&alice), &signed_out, READ_MAIN).await;

        assert_eq!(no_cookie, StatusCode::BAD_REQUEST);
        assert!(no_cookie_body.contains("no longer signed in"));
        assert_eq!(ended, StatusCode::BAD_REQUEST);
        assert!(ended_body.contains("no longer signed in"));
        assert_eq!(oauth_grants(&test).await, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_ceiling_lowered_after_the_page_refuses_the_old_offer() {
        let test = oauth_app().await;
        let alice = signed_in(&test, "alice", &[("main", Access::Write)]).await;
        let page = user_page(&test, &alice).await;
        ceiling::entitle(
            test.app.accounts().executor(),
            &NewEntitlement {
                user: "alice",
                catalog: "main",
                access: Access::Read,
            },
            &AuditActor::cli(),
            Utc::now(),
        )
        .await
        .expect("lower");

        let (status, _, body) = consent_as(
            &test,
            Some(&alice),
            &page,
            &[("grant.main", "on"), ("level.main", "write")],
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("access changed"), "{body}");
        assert_eq!(oauth_grants(&test).await, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_catalog_offered_since_the_page_was_shown_cannot_be_chosen() {
        // The nonce binds Approve to what that page offered, not to the
        // ceiling at POST time.
        let test = oauth_app().await;
        let alice = signed_in(&test, "alice", &[("main", Access::Read)]).await;
        let page = user_page(&test, &alice).await;
        ceiling::entitle(
            test.app.accounts().executor(),
            &NewEntitlement {
                user: "alice",
                catalog: "other",
                access: Access::Read,
            },
            &AuditActor::cli(),
            Utc::now(),
        )
        .await
        .expect("widen");

        let (status, _, _) = consent_as(
            &test,
            Some(&alice),
            &page,
            &[("grant.other", "on"), ("level.other", "read")],
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(oauth_grants(&test).await, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn choosing_nothing_or_an_unchosen_default_shows_the_form_again() {
        let test = oauth_app().await;
        let alice = signed_in(
            &test,
            "alice",
            &[("main", Access::Read), ("other", Access::Read)],
        )
        .await;
        let page = user_page(&test, &alice).await;

        let (nothing, _, again) =
            consent_as(&test, Some(&alice), &page, &[("level.main", "read")]).await;
        let (default, _, again_too) = consent_as(
            &test,
            Some(&alice),
            &again,
            &[
                ("grant.main", "on"),
                ("level.main", "read"),
                ("default_catalog", "other"),
            ],
        )
        .await;

        assert_eq!(nothing, StatusCode::OK);
        assert!(again.contains("id=\"consent-user-form\""));
        assert!(again.contains("Choose at least one catalog"));
        assert_eq!(default, StatusCode::OK);
        assert!(again_too.contains("The default catalog must be one you allow"));
        assert_eq!(oauth_grants(&test).await, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn each_page_approves_at_most_once() {
        let test = oauth_app().await;
        let alice = signed_in(&test, "alice", &[("main", Access::Read)]).await;
        let first = user_page(&test, &alice).await;
        // Choosing nothing shows the page again with a new nonce, spending
        // the first page's.
        let (_, _, again) = consent_as(&test, Some(&alice), &first, &[]).await;

        let (stale, _, _) = consent_as(&test, Some(&alice), &first, READ_MAIN).await;
        // Two Approves of one page racing: one wins.
        let racing = consent_form(&again, READ_MAIN);
        let (one, two) = tokio::join!(
            send_as(
                &test,
                "POST",
                "/oauth/authorize",
                Some(racing.clone()),
                Some(&alice)
            ),
            send_as(
                &test,
                "POST",
                "/oauth/authorize",
                Some(racing),
                Some(&alice)
            ),
        );

        assert_eq!(stale, StatusCode::BAD_REQUEST);
        let mut statuses = [one.0, two.0];
        statuses.sort();
        assert_eq!(statuses, [StatusCode::OK, StatusCode::BAD_REQUEST]);
        assert_eq!(oauth_grants(&test).await, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_consent_posted_from_another_site_is_refused() {
        let test = oauth_app().await;
        let alice = signed_in(&test, "alice", &[("main", Access::Read)]).await;
        let page = user_page(&test, &alice).await;
        let post = |site: &'static str| {
            let request = Request::builder()
                .method("POST")
                .uri("/oauth/authorize")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("sec-fetch-site", site)
                .header(
                    "cookie",
                    format!("{COOKIE}={}", alice.cookie(COOKIE).expect("a session")),
                )
                .body(Body::from(consent_form(&page, READ_MAIN)))
                .expect("a request");
            crate::http::router(Arc::clone(&test.app), &CancellationToken::new()).oneshot(request)
        };

        let cross = post("cross-site").await.expect("a response").status();
        let same_site = post("same-site").await.expect("a response").status();
        let same_origin = post("same-origin").await.expect("a response").status();

        assert_eq!(cross, StatusCode::FORBIDDEN);
        assert_eq!(same_site, StatusCode::FORBIDDEN);
        // Refused before the nonce was looked at: the page still approves.
        assert_eq!(same_origin, StatusCode::OK);
        assert_eq!(oauth_grants(&test).await, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_code_steps_are_refused_on_the_signed_in_path() {
        let test = oauth_app().await;
        let code = sign_in_code(&test, Access::Write).await;
        let alice = signed_in(&test, "alice", &[("main", Access::Read)]).await;
        let page = user_page(&test, &alice).await;
        let (pending, csrf) = (hidden(&page, "pending"), hidden(&page, "csrf"));

        let (entered, _, _) = send_as(
            &test,
            "POST",
            "/oauth/authorize",
            Some(form(&[
                ("pending", &pending),
                ("csrf", &csrf),
                ("action", "code"),
                ("sign_in_code", &code),
            ])),
            Some(&alice),
        )
        .await;

        assert_eq!(entered, StatusCode::BAD_REQUEST);
        // The code path's page cannot reach the user path's action either.
        let (_, _, code_page) = send(
            &test,
            "GET",
            &format!("/oauth/authorize?{}", authorize_query()),
            None,
        )
        .await;
        let (consented, _, _) = send_as(
            &test,
            "POST",
            "/oauth/authorize",
            Some(form(&[
                ("pending", &hidden(&code_page, "pending")),
                ("csrf", &hidden(&code_page, "csrf")),
                ("action", "consent"),
                ("grant.main", "on"),
                ("level.main", "read"),
            ])),
            Some(&alice),
        )
        .await;
        assert_eq!(consented, StatusCode::BAD_REQUEST);
        assert_eq!(oauth_grants(&test).await, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_empty_ceiling_offers_only_deny_and_the_code_path() {
        let test = oauth_app().await;
        let alice = signed_in(&test, "alice", &[]).await;

        let page = user_page(&test, &alice).await;

        assert!(page.contains("id=\"consent-empty\""));
        assert!(!page.contains("id=\"consent-approve\""));
        assert!(page.contains("id=\"consent-deny\""));
        assert!(page.contains("id=\"consent-use-code\""));
    }

    /// What a browser posts from the signed-in form as `page` renders it,
    /// untouched: each ticked checkbox, and each select's `selected` option
    /// (else its first). Hidden inputs and the button come from
    /// [`consent_form`].
    fn initial_choices(page: &str) -> Vec<(String, String)> {
        let start = page.find("id=\"consent-user-form\"").expect("the form");
        let form = &page[start..];
        let form = &form[..form.find("</form>").expect("the form's end")];
        let mut choices = Vec::new();
        for (at, marker) in form.match_indices("<input type=\"checkbox\" name=\"") {
            let rest = &form[at + marker.len()..];
            let tag = &rest[..rest.find('>').expect("the tag's end")];
            if tag.ends_with(" checked") {
                let (name, rest) = tag.split_once("\" value=\"").expect("a value");
                let (value, _) = rest.split_once('"').expect("closing quote");
                choices.push((name.to_string(), value.to_string()));
            }
        }
        for (at, marker) in form.match_indices("<select name=\"") {
            let rest = &form[at + marker.len()..];
            let (name, rest) = rest.split_once('"').expect("closing quote");
            let options = &rest[..rest.find("</select>").expect("the select's end")];
            let value = |option: &str| {
                let rest = &option[option.find("value=\"").expect("a value") + 7..];
                rest[..rest.find('"').expect("closing quote")].to_string()
            };
            let all: Vec<&str> = options.split("<option ").skip(1).collect();
            let chosen = all
                .iter()
                .find(|option| option.contains(" selected>"))
                .or_else(|| all.first())
                .expect("an option");
            choices.push((name.to_string(), value(chosen)));
        }
        choices
    }

    /// Approves `page`'s form as it starts, as `session`.
    async fn approve_as_shown(
        test: &TestApp,
        session: &Browser,
        page: &str,
    ) -> (StatusCode, HeaderMap, String) {
        let choices = initial_choices(page);
        let pairs: Vec<(&str, &str)> = choices
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        consent_as(test, Some(session), page, &pairs).await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn one_write_catalogs_collapsed_defaults_approve_write_with_it_as_default() {
        let test = oauth_app().await;
        let alice = signed_in(&test, "alice", &[("main", Access::Write)]).await;
        let page = user_page(&test, &alice).await;
        assert!(page.contains("<details id=\"consent-advanced\">"), "{page}");
        assert_eq!(
            initial_choices(&page),
            [
                ("grant.main".to_string(), "on".to_string()),
                ("level.main".to_string(), "write".to_string()),
                ("default_catalog".to_string(), "main".to_string()),
            ]
        );

        let (status, headers, body) = approve_as_shown(&test, &alice, &page).await;

        let grant = grant_of(&test, &forwarded(status, &headers, &body)).await;
        let principal = test
            .app
            .find_grant(&grant)
            .await
            .unwrap()
            .expect("cached grant");
        assert_eq!(principal.user.as_deref(), Some("alice"));
        assert_eq!(principal.level("main"), Some(Access::Write));
        assert_eq!(principal.default_catalog.as_deref(), Some("main"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn one_read_only_catalogs_collapsed_defaults_approve_read_with_it_as_default() {
        let test = oauth_app().await;
        let alice = signed_in(&test, "alice", &[("main", Access::Read)]).await;
        let page = user_page(&test, &alice).await;
        assert!(page.contains("<details id=\"consent-advanced\">"), "{page}");
        assert!(!page.contains("value=\"write\""), "write is not offered");

        let (status, headers, body) = approve_as_shown(&test, &alice, &page).await;

        let grant = grant_of(&test, &forwarded(status, &headers, &body)).await;
        let principal = test
            .app
            .find_grant(&grant)
            .await
            .unwrap()
            .expect("cached grant");
        assert_eq!(principal.level("main"), Some(Access::Read));
        assert_eq!(principal.default_catalog.as_deref(), Some("main"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn several_catalogs_start_open_and_preselect_nothing() {
        let test = oauth_app().await;
        let alice = signed_in(
            &test,
            "alice",
            &[("main", Access::Write), ("other", Access::Read)],
        )
        .await;
        let page = user_page(&test, &alice).await;

        assert!(
            page.contains("<details id=\"consent-advanced\" open>"),
            "{page}"
        );
        assert!(!page.contains(" checked>"), "{page}");
        // Untouched, the form chooses nothing, which is refused as before.
        let (status, _, again) = approve_as_shown(&test, &alice, &page).await;
        assert_eq!(status, StatusCode::OK);
        assert!(again.contains("Choose at least one catalog"), "{again}");
        assert_eq!(oauth_grants(&test).await, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn one_catalogs_preselection_can_be_changed_and_is_still_validated() {
        let test = oauth_app().await;
        let alice = signed_in(&test, "alice", &[("main", Access::Write)]).await;

        // Unticked: refused as an empty choice, shown again open.
        let page = user_page(&test, &alice).await;
        let (nothing, _, again) = consent_as(
            &test,
            Some(&alice),
            &page,
            &[("level.main", "write"), ("default_catalog", "main")],
        )
        .await;
        assert_eq!(nothing, StatusCode::OK);
        assert!(again.contains("Choose at least one catalog"), "{again}");
        assert!(
            again.contains("<details id=\"consent-advanced\" open>"),
            "{again}"
        );
        // A default outside the choice: refused.
        let (not_chosen, _, refused) = consent_as(
            &test,
            Some(&alice),
            &again,
            &[
                ("grant.main", "on"),
                ("level.main", "write"),
                ("default_catalog", "other"),
            ],
        )
        .await;
        assert_eq!(not_chosen, StatusCode::OK);
        assert!(
            refused.contains("The default catalog must be one you allow"),
            "{refused}"
        );
        assert_eq!(oauth_grants(&test).await, 0);

        // Lowered to read with no default: honoured as chosen.
        let page = user_page(&test, &alice).await;
        let (status, headers, body) = consent_as(
            &test,
            Some(&alice),
            &page,
            &[
                ("grant.main", "on"),
                ("level.main", "read"),
                ("default_catalog", ""),
            ],
        )
        .await;
        let grant = grant_of(&test, &forwarded(status, &headers, &body)).await;
        let principal = test
            .app
            .find_grant(&grant)
            .await
            .unwrap()
            .expect("cached grant");
        assert_eq!(principal.level("main"), Some(Access::Read));
        assert_eq!(principal.default_catalog, None);
    }

    /// A signed-in Approve's form and a pending authorization it matches,
    /// with `failed_attempts` wrong codes and an accepted code.
    fn matching(failed_attempts: u8) -> (super::ConsentForm, super::PendingAuthorization) {
        use crate::oauth::state::{AcceptedCode, UserConsent};
        use crate::store::oauth::clients::{ClientKind, ClientRecord};
        let form = super::ConsentForm {
            pending: "pending-id".to_string(),
            csrf: "csrf-token".to_string(),
            action: "consent".to_string(),
            sign_in_code: None,
            summary: Some("page-nonce".to_string()),
            default_catalog: None,
            find: None,
            choices: BTreeMap::new(),
        };
        let pending = super::PendingAuthorization {
            csrf: "csrf-token".to_string(),
            client: ClientRecord {
                client_id: CLIENT_ID.to_string(),
                kind: ClientKind::Dcr,
                client_name: None,
                redirect_uris: vec![REDIRECT_URI.to_string()],
                metadata: serde_json::json!({}),
                fetched_at: None,
                cache_expires_at: None,
                created_at: Utc::now(),
            },
            redirect_uri: REDIRECT_URI.to_string(),
            state: None,
            code_challenge: "c".repeat(43),
            resource: "https://okf.example.com/mcp".to_string(),
            accepted: Some(AcceptedCode {
                code_id: "code-id".to_string(),
                summary_nonce: "summary-nonce".to_string(),
            }),
            failed_attempts,
            consent: Some(UserConsent {
                user_id: "user-id".to_string(),
                nonce: "page-nonce".to_string(),
                offered: Ceiling::new(),
                prefix: String::new(),
                next: None,
                whole: true,
            }),
            query: String::new(),
        };
        (form, pending)
    }

    /// A request whose wrong codes are spent (left behind by a failed
    /// removal) is refused even with a matching CSRF token, and nothing in
    /// it changes; one attempt fewer is still allowed.
    #[test]
    fn a_request_with_its_wrong_codes_spent_is_refused_untouched() {
        use crate::oauth::state::MAX_FAILED_ATTEMPTS;
        let (form, mut spent) = matching(MAX_FAILED_ATTEMPTS);
        let before = (spent.accepted.clone(), spent.consent.clone());

        let refused = super::consent_change(&form, Some("fresh".to_string()))(&mut spent);

        assert!(refused.is_none(), "the spent request is refused");
        assert_eq!((spent.accepted, spent.consent), before, "nothing changed");

        let (form, mut live) = matching(MAX_FAILED_ATTEMPTS - 1);
        let accepted = live.accepted.clone();

        let allowed = super::consent_change(&form, Some("fresh".to_string()))(&mut live);

        let (_, decided, claimed) = allowed.expect("one attempt fewer is allowed");
        assert!(claimed, "the page's nonce is spent");
        assert_eq!(
            decided,
            super::Approval::Stale,
            "no summary token was posted"
        );
        assert_eq!(live.consent.expect("user path").nonce, "fresh");
        assert_eq!(live.accepted, accepted);
    }

    #[test]
    fn debug_never_prints_the_forms_secrets() {
        let posted = super::ConsentForm {
            pending: "pending-id-that-must-not-leak".to_string(),
            csrf: "csrf-token-that-must-not-leak".to_string(),
            action: "consent".to_string(),
            sign_in_code: Some("okfsc_code-that-must-not-leak".to_string()),
            summary: Some("consent-nonce-that-must-not-leak".to_string()),
            default_catalog: None,
            find: None,
            choices: BTreeMap::new(),
        };

        let printed = format!("{posted:?}");

        for secret in [
            "pending-id-that-must-not-leak",
            "csrf-token-that-must-not-leak",
            "okfsc_code-that-must-not-leak",
            "consent-nonce-that-must-not-leak",
        ] {
            assert!(!printed.contains(secret), "{printed}");
        }
        assert!(printed.contains("consent"), "{printed}");
    }

    #[test]
    fn chosen_levels_keep_ticked_catalogs_within_the_offer() {
        let offered: Ceiling = [
            ("main".to_string(), Access::Write),
            ("other".to_string(), Access::Read),
        ]
        .into_iter()
        .collect();
        let choices = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };

        let chosen = chosen_levels(
            &choices(&[
                ("grant.main", "on"),
                ("level.main", "read"),
                ("level.other", "read"),
            ]),
            &offered,
        );

        assert_eq!(
            chosen.ok(),
            Some([("main".to_string(), Access::Read)].into_iter().collect())
        );
        assert_eq!(chosen_levels(&choices(&[]), &offered), Err(Choice::Empty));
        for tampered in [
            &[("grant.main", "on")][..],
            &[("grant.other", "on"), ("level.other", "write")],
            &[("grant.main", "yes"), ("level.main", "read")],
            &[("level.nowhere", "read")],
            &[("level.other", "write")],
            &[("grant.main", "on"), ("level.main", "admin")],
            &[("anything", "1")],
        ] {
            assert_eq!(
                chosen_levels(&choices(tampered), &offered),
                Err(Choice::Tampered),
                "{tampered:?}"
            );
        }
    }

    /// Task 8: pending authorizations in Redis, across two replicas.
    mod replicas {
        use std::collections::BTreeMap;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        use axum::http::StatusCode;

        use super::super::{consent_change, ConsentForm, BAD_CODE, BUSY, EXPIRED};
        use super::{oauth_grants, param, signed_in, user_page, TestApp, READ_MAIN};
        use crate::db::redis_support::TestRedis;
        use crate::oauth::state::MAX_FAILED_ATTEMPTS;
        use crate::oauth::testing::{
            authorize_query, exchange_for_tokens, form, forwarded, hidden, oauth_replica_pair_on,
            send, send_on, sign_in_code,
        };
        use crate::oauth::{App, ForcedGenerationFailure};
        use crate::shared::ephemeral::Keyspace;
        use crate::shared::hashed;
        use crate::store::keys::{self as key_rows, Access, KeyKind};

        /// Every body's bound (`lessons.md`: every wait is bounded).
        const BOUND: Duration = Duration::from_secs(120);

        /// The sign-in code step's form for `pending`.
        fn code_form(pending: &str, csrf: &str, code: &str) -> String {
            form(&[
                ("pending", pending),
                ("csrf", csrf),
                ("action", "code"),
                ("sign_in_code", code),
            ])
        }

        async fn consent_started_on_one_replica_is_approved_on_the_other_and_exchanged_on_the_first(
            server: TestRedis,
        ) {
            let (a, b, _server) = oauth_replica_pair_on(server).await;
            let code = sign_in_code(&a, Access::Read).await;
            let (status, _, page) = send(
                &a,
                "GET",
                &format!("/oauth/authorize?{}", authorize_query()),
                None,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{page}");
            let (pending, csrf) = (hidden(&page, "pending"), hidden(&page, "csrf"));

            let (status, _, summary) = send_on(
                &b,
                "POST",
                "/oauth/authorize",
                Some(code_form(&pending, &csrf, &code)),
                None,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "B shows step 2: {summary}");
            let approval = form(&[
                ("pending", &pending),
                ("csrf", &csrf),
                ("summary", &hidden(&summary, "summary")),
                ("action", "approve"),
            ]);
            let (status, headers, answer) =
                send_on(&b, "POST", "/oauth/authorize", Some(approval), None).await;
            let issued = param(&forwarded(status, &headers, &answer), "code").expect("a code");

            // `exchange_for_tokens` asserts the 200.
            let (access, refresh) = exchange_for_tokens(&a, &issued).await;

            assert!(!access.is_empty() && !refresh.is_empty());
        }

        async fn a_consent_page_approves_once_across_replicas(server: TestRedis) {
            let (a, b, _server) = oauth_replica_pair_on(server).await;
            let alice = signed_in(&a, "alice", &[("main", Access::Read)]).await;
            // Several rounds, so a nonce that is not spent atomically (both
            // read it unspent) is caught even if one round serializes.
            for round in 0..4 {
                let page = user_page(&a, &alice).await;
                let posted = super::consent_form(&page, READ_MAIN);

                // `join!` polls both on this task, so the two reads overlap.
                let (on_a, on_b) = tokio::join!(
                    send_on(
                        &a.app,
                        "POST",
                        "/oauth/authorize",
                        Some(posted.clone()),
                        Some(&alice)
                    ),
                    send_on(&b, "POST", "/oauth/authorize", Some(posted), Some(&alice)),
                );

                let mut statuses = [on_a.0, on_b.0];
                statuses.sort();
                assert_eq!(
                    statuses,
                    [StatusCode::OK, StatusCode::BAD_REQUEST],
                    "round {round}: one approves: {} / {}",
                    on_a.2,
                    on_b.2
                );
                let refused = if on_a.0 == StatusCode::OK {
                    on_b.2
                } else {
                    on_a.2
                };
                assert!(refused.contains(EXPIRED), "round {round}: {refused}");
            }
            assert_eq!(oauth_grants(&a).await, 4, "one grant per page");
        }

        async fn wrong_codes_count_across_replicas(server: TestRedis) {
            let (a, b, _server) = oauth_replica_pair_on(server).await;
            let (_, _, page) = send(
                &a,
                "GET",
                &format!("/oauth/authorize?{}", authorize_query()),
                None,
            )
            .await;
            let (pending, csrf) = (hidden(&page, "pending"), hidden(&page, "csrf"));

            for attempt in 1..=MAX_FAILED_ATTEMPTS {
                let replica = if attempt % 2 == 1 { &b } else { &a.app };
                let (status, _, answer) = send_on(
                    replica,
                    "POST",
                    "/oauth/authorize",
                    Some(code_form(&pending, &csrf, "okfsc_wrong")),
                    None,
                )
                .await;
                if attempt < MAX_FAILED_ATTEMPTS {
                    assert_eq!(status, StatusCode::OK, "attempt {attempt}: {answer}");
                    assert!(answer.contains(BAD_CODE), "attempt {attempt}: {answer}");
                } else {
                    assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
                    assert!(answer.contains("Too many wrong sign-in codes."), "{answer}");
                }
            }

            let code = sign_in_code(&a, Access::Read).await;
            for replica in [&a.app, &b] {
                let (status, _, answer) = send_on(
                    replica,
                    "POST",
                    "/oauth/authorize",
                    Some(code_form(&pending, &csrf, &code)),
                    None,
                )
                .await;

                assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
                assert!(answer.contains(EXPIRED), "the request is gone: {answer}");
            }
        }

        async fn an_expired_pending_authorization_is_refused(server: TestRedis) {
            let (a, b, server) = oauth_replica_pair_on(server).await;
            let code = sign_in_code(&a, Access::Read).await;
            let (_, _, page) = send(
                &a,
                "GET",
                &format!("/oauth/authorize?{}", authorize_query()),
                None,
            )
            .await;
            let (pending, csrf) = (hidden(&page, "pending"), hidden(&page, "csrf"));
            let handle = crate::shared::testing::connect(&server).await;
            let space = Keyspace::OauthPending.name();
            // The raw id is never a key name (spec Decision 7): only its hash.
            let raw: bool = redis::cmd("EXISTS")
                .arg(handle.key(space, &pending))
                .query_async(&mut handle.connection())
                .await
                .expect("EXISTS");
            assert!(!raw, "no key is named by the raw pending id");
            let expiring: bool = redis::cmd("PEXPIRE")
                .arg(handle.key(space, &hashed(&pending)))
                .arg(1)
                .query_async(&mut handle.connection())
                .await
                .expect("PEXPIRE");
            assert!(expiring, "the request is stored under the id's hash");
            tokio::time::sleep(Duration::from_millis(50)).await;

            for replica in [&b, &a.app] {
                let (status, _, answer) = send_on(
                    replica,
                    "POST",
                    "/oauth/authorize",
                    Some(code_form(&pending, &csrf, &code)),
                    None,
                )
                .await;

                assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
                assert!(answer.contains(EXPIRED), "{answer}");
            }
        }

        /// The consent POST's change, rerun after another replica wrote
        /// between its read and its write, still spends the page's nonce
        /// (`lessons.md`: a closure passed to `update` may run twice).
        async fn a_lost_race_reruns_the_consent_change_on_a_fresh_copy(server: TestRedis) {
            let (a, b, _server) = oauth_replica_pair_on(server).await;
            let alice = signed_in(&a, "alice", &[("main", Access::Read)]).await;
            let page = user_page(&a, &alice).await;
            let pending_id = hidden(&page, "pending");
            let posted = ConsentForm {
                pending: pending_id.clone(),
                csrf: hidden(&page, "csrf"),
                action: "consent".to_string(),
                sign_in_code: None,
                summary: Some(hidden(&page, "summary")),
                default_catalog: None,
                find: None,
                choices: BTreeMap::new(),
            };
            // Two replicas, two Redis handles: A's update, B's interference.
            let mine = &a.app.oauth().expect("OAuth is on").pending;
            let theirs = &b.oauth().expect("OAuth is on").pending;
            let runs = AtomicUsize::new(0);
            let mut change = consent_change(&posted, Some("fresh-nonce".to_string()));

            let decided = mine
                .update(&pending_id, Instant::now(), |pending| {
                    if runs.fetch_add(1, Ordering::SeqCst) == 0 {
                        // B writes between this run's read and its write.
                        // `block_in_place` lets this sync closure wait on
                        // async code (multi-thread runtime only).
                        tokio::task::block_in_place(|| {
                            tokio::runtime::Handle::current().block_on(theirs.update(
                                &pending_id,
                                Instant::now(),
                                |pending| pending.query.push_str("&changed=on-b"),
                            ))
                        })
                        .expect("B's write")
                        .expect("the request is live on B");
                    }
                    change(pending)
                })
                .await
                .expect("Redis answers")
                .expect("the request is live");

            assert_eq!(runs.load(Ordering::SeqCst), 2, "the change ran again");
            let (snapshot, _, claimed) = decided.expect("the CSRF token matches");
            assert!(claimed, "the rerun still spends the page's nonce");
            assert!(snapshot.query.ends_with("&changed=on-b"), "on B's copy");
            assert_eq!(
                snapshot.consent.expect("the user path").nonce,
                "fresh-nonce",
                "the stored nonce is the replacement"
            );
        }

        async fn a_redis_outage_answers_the_busy_page(server: TestRedis) {
            // A shared server cannot be stopped for everyone.
            let (a, b, server) = oauth_replica_pair_on(server.dedicated().await).await;
            let code = sign_in_code(&a, Access::Read).await;
            let (_, _, page) = send(
                &a,
                "GET",
                &format!("/oauth/authorize?{}", authorize_query()),
                None,
            )
            .await;
            let (pending, csrf) = (hidden(&page, "pending"), hidden(&page, "csrf"));
            server.stop().await;

            let (started, _, start_page) = send(
                &a,
                "GET",
                &format!("/oauth/authorize?{}", authorize_query()),
                None,
            )
            .await;
            let (stepped, _, step_page) = send_on(
                &b,
                "POST",
                "/oauth/authorize",
                Some(code_form(&pending, &csrf, &code)),
                None,
            )
            .await;

            assert_eq!(started, StatusCode::SERVICE_UNAVAILABLE, "{start_page}");
            assert!(start_page.contains(BUSY), "{start_page}");
            assert_eq!(stepped, StatusCode::SERVICE_UNAVAILABLE, "{step_page}");
            assert!(step_page.contains(BUSY), "{step_page}");
        }

        /// Sets the `default` user's command rule (`-del` or `+del`) through
        /// `admin`, a connection of another user.
        async fn default_user_may(admin: &mut redis::aio::MultiplexedConnection, rule: &str) {
            let _: () = redis::cmd("ACL")
                .arg("SETUSER")
                .arg("default")
                .arg(rule)
                .query_async(admin)
                .await
                .expect("change the default user's permissions");
        }

        /// A connection to `server` as an admin user of its own, so a test can
        /// take commands from the `default` user (the apps') and give them
        /// back. `server` must be dedicated: a shared server's other tests
        /// use `default`.
        async fn admin_of(server: &TestRedis) -> redis::aio::MultiplexedConnection {
            // A plain connection: the container has one node in either mode.
            let client = redis::Client::open(server.url.as_str()).expect("the test redis URL");
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
            admin
        }

        /// The ids of the OAuth grants that are not revoked: the user's grant
        /// list.
        async fn live_grants(test: &TestApp) -> Vec<String> {
            key_rows::list(test.app.accounts().executor())
                .await
                .expect("list the grants")
                .into_iter()
                .filter(|row| row.kind == KeyKind::OAuth && row.revoked_at.is_none())
                .map(|row| row.id)
                .collect()
        }

        /// Starts a consent on `a`: its pending id and CSRF token.
        async fn start_consent(a: &TestApp) -> (String, String) {
            let (_, _, page) = send(
                a,
                "GET",
                &format!("/oauth/authorize?{}", authorize_query()),
                None,
            )
            .await;
            (hidden(&page, "pending"), hidden(&page, "csrf"))
        }

        /// Starts a consent on `a`, signs in and submits the code on `b`, and
        /// returns the summary's approval form, ready to post.
        async fn approval_form(a: &TestApp, b: &Arc<App>) -> String {
            let (approved, approved_csrf) = start_consent(a).await;
            let code = sign_in_code(a, Access::Read).await;
            let (_, _, summary) = send_on(
                b,
                "POST",
                "/oauth/authorize",
                Some(code_form(&approved, &approved_csrf, &code)),
                None,
            )
            .await;
            form(&[
                ("pending", &approved),
                ("csrf", &approved_csrf),
                ("summary", &hidden(&summary, "summary")),
                ("action", "approve"),
            ])
        }

        /// How many OAuth grants are revoked.
        async fn revoked_grants(test: &TestApp) -> usize {
            key_rows::list(test.app.accounts().executor())
                .await
                .expect("list the grants")
                .into_iter()
                .filter(|row| row.kind == KeyKind::OAuth && row.revoked_at.is_some())
                .count()
        }

        /// Completes one consent (started on `a`, finished on `b`) so a grant
        /// exists before a failing one, and returns the live grants, which
        /// must be that one grant.
        async fn complete_earlier_consent(a: &TestApp, b: &Arc<App>) -> Vec<String> {
            let (earlier, earlier_csrf) = start_consent(a).await;
            let earlier_code = sign_in_code(a, Access::Read).await;
            let (_, _, earlier_summary) = send_on(
                b,
                "POST",
                "/oauth/authorize",
                Some(code_form(&earlier, &earlier_csrf, &earlier_code)),
                None,
            )
            .await;
            let (earlier_status, earlier_headers, earlier_page) = send_on(
                b,
                "POST",
                "/oauth/authorize",
                Some(form(&[
                    ("pending", &earlier),
                    ("csrf", &earlier_csrf),
                    ("summary", &hidden(&earlier_summary, "summary")),
                    ("action", "approve"),
                ])),
                None,
            )
            .await;
            let earlier_target = forwarded(earlier_status, &earlier_headers, &earlier_page);
            assert!(
                param(&earlier_target, "code").is_some(),
                "the earlier consent completed"
            );
            let before = live_grants(a).await;
            assert_eq!(before.len(), 1, "one grant before the failed approval");
            before
        }

        /// When the removal after a successful update fails (`DEL` refused
        /// with `NOPERM`, `GET` and the compare-and-set still allowed), Deny
        /// and Approve answer the busy page: Deny does not forward the
        /// denial, and Approve mints no authorization code. The grant the
        /// failed approval created is revoked (issue #589), and a grant made
        /// by an earlier, completed consent is left as it was.
        async fn a_failed_removal_answers_the_busy_page(server: TestRedis) {
            // Restricting the default user would break every test sharing it.
            let server = server.dedicated().await;
            let mut admin = admin_of(&server).await;
            let (a, b, _server) = oauth_replica_pair_on(server).await;
            let start = || start_consent(&a);
            let before = complete_earlier_consent(&a, &b).await;

            let (denied, denied_csrf) = start().await;
            let (approved, approved_csrf) = start().await;
            let code = sign_in_code(&a, Access::Read).await;
            let (_, _, summary) = send_on(
                &b,
                "POST",
                "/oauth/authorize",
                Some(code_form(&approved, &approved_csrf, &code)),
                None,
            )
            .await;
            let deny = form(&[
                ("pending", &denied),
                ("csrf", &denied_csrf),
                ("action", "deny"),
            ]);
            let approval = form(&[
                ("pending", &approved),
                ("csrf", &approved_csrf),
                ("summary", &hidden(&summary, "summary")),
                ("action", "approve"),
            ]);
            let stored_codes = async |admin: &mut redis::aio::MultiplexedConnection| {
                let mut keys: Vec<String> = redis::cmd("KEYS")
                    .arg("*oauth:code:*")
                    .query_async(admin)
                    .await
                    .expect("KEYS");
                keys.sort();
                keys
            };
            let codes_before = stored_codes(&mut admin).await;
            default_user_may(&mut admin, "-del").await;

            let refused_deny =
                send_on(&b, "POST", "/oauth/authorize", Some(deny.clone()), None).await;
            let refused_approval =
                send_on(&b, "POST", "/oauth/authorize", Some(approval), None).await;
            let codes = stored_codes(&mut admin).await;
            default_user_may(&mut admin, "+del").await;
            let later_deny = send_on(&b, "POST", "/oauth/authorize", Some(deny), None).await;

            assert_eq!(
                refused_deny.0,
                StatusCode::SERVICE_UNAVAILABLE,
                "{}",
                refused_deny.2
            );
            assert!(refused_deny.2.contains(BUSY), "{}", refused_deny.2);
            assert!(
                !refused_deny.2.contains("access_denied"),
                "no denial is forwarded"
            );
            assert_eq!(
                refused_approval.0,
                StatusCode::SERVICE_UNAVAILABLE,
                "{}",
                refused_approval.2
            );
            assert!(refused_approval.2.contains(BUSY), "{}", refused_approval.2);
            assert!(
                !refused_approval.2.contains("code="),
                "no code is forwarded"
            );
            assert_eq!(
                codes, codes_before,
                "no authorization code was stored by the refused approval"
            );
            assert_eq!(
                live_grants(&a).await,
                before,
                "the failed approval's grant is revoked and the earlier one survives"
            );
            // The denied request stayed, so the Deny goes through once Redis
            // accepts the removal again.
            let target = forwarded(later_deny.0, &later_deny.1, &later_deny.2);
            assert_eq!(param(&target, "error").as_deref(), Some("access_denied"));
        }

        /// When the code cannot be stored (a write to the code key refused with
        /// `NOPERM`, the pending removal still allowed), Approve answers the busy page with
        /// no code, and the grant it created is revoked (issue #925); a grant
        /// made by an earlier, completed consent is left as it was.
        async fn a_refused_code_insert_revokes_the_new_grant(server: TestRedis) {
            let server = server.dedicated().await;
            let mut admin = admin_of(&server).await;
            let (a, b, _server) = oauth_replica_pair_on(server).await;
            let before = complete_earlier_consent(&a, &b).await;

            let approval = approval_form(&a, &b).await;
            // Writes to the code keys are refused (`NOPERM`); every other key
            // stays writable, so only the code insert fails.
            for rule in [
                "resetkeys",
                "~*oauth:pending:*",
                "~*oidc:*",
                "~*passkey:*",
                "~*mcp:*",
                "%R~*oauth:code:*",
            ] {
                default_user_may(&mut admin, rule).await;
            }
            let refused = send_on(&b, "POST", "/oauth/authorize", Some(approval), None).await;
            default_user_may(&mut admin, "~*").await;

            assert_eq!(refused.0, StatusCode::SERVICE_UNAVAILABLE, "{}", refused.2);
            assert!(refused.2.contains(BUSY), "{}", refused.2);
            assert!(!refused.2.contains("code="), "no code is forwarded");
            assert_eq!(
                live_grants(&a).await,
                before,
                "the failed approval's grant is revoked and the earlier one survives"
            );
            assert_eq!(
                revoked_grants(&a).await,
                1,
                "the approval created a grant, then revoked it"
            );
        }

        /// Approving a consent while the generator named by `which` fails
        /// answers the 500 page, forwards no code, and revokes the grant the
        /// approval created; an earlier consent's grant stays live (#925).
        async fn a_failed_generation_revokes_the_new_grant(
            server: TestRedis,
            which: ForcedGenerationFailure,
        ) {
            let (a, b, _server) = oauth_replica_pair_on(server).await;
            let before = complete_earlier_consent(&a, &b).await;
            let approval = approval_form(&a, &b).await;
            b.oauth()
                .expect("OAuth is on")
                .force_generation_failure(which);

            let failed = send_on(&b, "POST", "/oauth/authorize", Some(approval), None).await;

            assert_eq!(
                failed.0,
                StatusCode::INTERNAL_SERVER_ERROR,
                "{which:?}: {}",
                failed.2
            );
            assert!(
                failed.2.contains("could not complete sign-in"),
                "{which:?}: {}",
                failed.2
            );
            assert!(!failed.2.contains("code="), "no code is forwarded");
            assert_eq!(
                live_grants(&a).await,
                before,
                "{which:?}: the new grant is revoked and the earlier one survives"
            );
            assert_eq!(
                revoked_grants(&a).await,
                1,
                "{which:?}: the approval created a grant, then revoked it"
            );
        }

        async fn a_failed_code_generation_revokes_the_new_grant(server: TestRedis) {
            a_failed_generation_revokes_the_new_grant(server, ForcedGenerationFailure::Code).await;
        }

        async fn a_failed_family_id_generation_revokes_the_new_grant(server: TestRedis) {
            a_failed_generation_revokes_the_new_grant(server, ForcedGenerationFailure::FamilyId)
                .await;
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
            consent_started_on_one_replica_is_approved_on_the_other_and_exchanged_on_the_first,
            a_consent_page_approves_once_across_replicas,
            wrong_codes_count_across_replicas,
            an_expired_pending_authorization_is_refused,
            a_lost_race_reruns_the_consent_change_on_a_fresh_copy,
            a_redis_outage_answers_the_busy_page,
            a_failed_removal_answers_the_busy_page,
            a_refused_code_insert_revokes_the_new_grant,
            a_failed_code_generation_revokes_the_new_grant,
            a_failed_family_id_generation_revokes_the_new_grant,
        );
    }

    // Issue #736: the signed-in page offers the ceiling a page at a time.

    /// Gives `count` catalogs (`c00001`...) to `user`'s personal team by
    /// SQL, their `member_access` cycling none, read, write.
    async fn seed_owned(test: &TestApp, user: &str, count: i64) {
        sqlx::query(
            "INSERT INTO catalogs (name, description, created_at, owner_team_id, member_access) \
             SELECT 'c' || lpad(n::text, 5, '0'), '', now(), \
                    (SELECT id FROM teams WHERE name = $1), \
                    (ARRAY['none', 'read', 'write'])[n % 3 + 1] \
             FROM generate_series(1, $2) AS n",
        )
        .bind(user)
        .bind(count)
        .execute(test.app.accounts().executor())
        .await
        .unwrap();
        sqlx::query("ANALYZE catalogs")
            .execute(test.app.accounts().executor())
            .await
            .unwrap();
    }

    /// A Find (`find` set) or Next catalogs (`None`) of a signed-in page,
    /// with `choices` ticked.
    async fn browse_as(
        test: &TestApp,
        session: &Browser,
        page: &str,
        find: Option<&str>,
        choices: &[(&str, &str)],
    ) -> (StatusCode, String) {
        let (pending, csrf, summary) = (
            hidden(page, "pending"),
            hidden(page, "csrf"),
            hidden(page, "summary"),
        );
        let mut pairs = vec![
            ("pending", pending.as_str()),
            ("csrf", csrf.as_str()),
            ("summary", summary.as_str()),
        ];
        match find {
            Some(prefix) => pairs.extend([("action", "find"), ("find", prefix)]),
            None => pairs.push(("action", "more")),
        }
        pairs.extend_from_slice(choices);
        let (status, _, body) = send_as(
            test,
            "POST",
            "/oauth/authorize",
            Some(form(&pairs)),
            Some(session),
        )
        .await;
        (status, body)
    }

    /// What the page's pending authorization holds for its consent.
    async fn consent_of(test: &TestApp, page: &str) -> crate::oauth::state::UserConsent {
        test.app
            .oauth()
            .expect("oauth on")
            .pending
            .update(&hidden(page, "pending"), Instant::now(), |pending| {
                pending.consent.clone()
            })
            .await
            .unwrap()
            .flatten()
            .expect("a signed-in consent")
    }

    /// Equivalence with the whole-ceiling page it replaces: walking Next
    /// catalogs offers exactly `ceiling_of` (every catalog once, at its
    /// level), a page of at most `CONSENT_PAGE` at a time, and a Find
    /// offers exactly the ceiling's catalogs with that prefix. Team
    /// catalogs at each `member_access`, and entitlements raising one,
    /// matching one, reaching a `none` one and one of another team.
    #[tokio::test(flavor = "multi_thread")]
    async fn walking_the_consent_pages_offers_exactly_the_whole_ceiling() {
        let test = oauth_app().await;
        let alice = signed_in(&test, "alice", &[("main", Access::Read)]).await;
        seed_owned(&test, "alice", 160).await;
        for (catalog, access) in [
            ("c00001", Access::Write),
            ("c00002", Access::Write),
            ("c00003", Access::Read),
            ("c00004", Access::Read),
        ] {
            ceiling::entitle(
                test.app.accounts().executor(),
                &NewEntitlement {
                    user: "alice",
                    catalog,
                    access,
                },
                &AuditActor::cli(),
                Utc::now(),
            )
            .await
            .expect("entitle");
        }
        let whole = ceiling::ceiling_of(test.app.accounts().executor(), "alice")
            .await
            .unwrap();

        let mut page = user_page(&test, &alice).await;
        let mut walked = Ceiling::new();
        let mut pages = 0;
        loop {
            let consent = consent_of(&test, &page).await;
            assert!(consent.offered.len() <= CONSENT_PAGE as usize);
            assert!(!consent.whole, "160 catalogs do not fit a page");
            for (catalog, level) in &consent.offered {
                assert!(page.contains(&format!("name=\"grant.{catalog}\"")));
                assert_eq!(
                    walked.insert(catalog.clone(), *level),
                    None,
                    "{catalog} twice"
                );
            }
            pages += 1;
            if consent.next.is_none() {
                assert!(!page.contains("id=\"consent-more\""));
                break;
            }
            assert!(page.contains("id=\"consent-more\""));
            let (status, next) = browse_as(&test, &alice, &page, None, &[]).await;
            assert_eq!(status, StatusCode::OK, "{next}");
            page = next;
        }
        assert_eq!(walked, whole);
        assert!(pages >= 3, "{pages} pages");

        let (status, found) = browse_as(&test, &alice, &page, Some(" C0001"), &[]).await;
        assert_eq!(status, StatusCode::OK, "{found}");
        let wanted: Ceiling = whole
            .iter()
            .filter(|(catalog, _)| catalog.starts_with("c0001"))
            .map(|(catalog, level)| (catalog.clone(), *level))
            .collect();
        let consent = consent_of(&test, &found).await;
        assert_eq!(consent.offered, wanted);
        assert_eq!(consent.prefix, "c0001");
        assert!(
            found.contains("value=\"c0001\""),
            "the box keeps the prefix"
        );
    }

    /// Ticked catalogs are carried across a Find, preselected at their
    /// level, and approved with the found ones in one grant; a Find for a
    /// prefix nothing starts with says so and still offers what is carried.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_find_carries_the_ticked_catalogs_into_one_grant() {
        let test = oauth_app().await;
        let alice = signed_in(
            &test,
            "alice",
            &[("main", Access::Write), ("other", Access::Read)],
        )
        .await;
        let page = user_page(&test, &alice).await;

        let (status, found) = browse_as(
            &test,
            &alice,
            &page,
            Some("ot"),
            &[("grant.main", "on"), ("level.main", "write")],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{found}");
        assert!(
            found.contains("name=\"grant.main\" value=\"on\" id=\"consent-catalog-main\" checked")
        );
        assert!(found.contains("<option value=\"write\" selected>write</option>"));
        assert!(found.contains("name=\"grant.other\" value=\"on\" id=\"consent-catalog-other\">"));
        let (status, none) = browse_as(
            &test,
            &alice,
            &found,
            Some("zz"),
            &[("grant.main", "on"), ("level.main", "write")],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{none}");
        assert!(
            !none.contains("id=\"consent-none-found\""),
            "main is carried"
        );
        assert_eq!(
            consent_of(&test, &none).await.offered,
            [("main".to_string(), Access::Write)]
                .into_iter()
                .collect::<Ceiling>()
        );
        let (status, empty) = browse_as(&test, &alice, &none, Some("zz"), &[]).await;
        assert_eq!(status, StatusCode::OK, "{empty}");
        assert!(empty.contains("id=\"consent-none-found\""), "{empty}");
        assert!(!empty.contains("id=\"consent-empty\""), "she has catalogs");

        let (status, again) = browse_as(&test, &alice, &empty, Some("ot"), &[]).await;
        assert_eq!(status, StatusCode::OK);
        let (status, carried) = browse_as(
            &test,
            &alice,
            &again,
            Some("ma"),
            &[("grant.other", "on"), ("level.other", "read")],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, headers, body) = consent_as(
            &test,
            Some(&alice),
            &carried,
            &[
                ("grant.main", "on"),
                ("level.main", "write"),
                ("grant.other", "on"),
                ("level.other", "read"),
                ("default_catalog", "main"),
            ],
        )
        .await;
        let grant = grant_of(&test, &forwarded(status, &headers, &body)).await;
        let principal = test
            .app
            .find_grant(&grant)
            .await
            .unwrap()
            .expect("cached grant");
        assert_eq!(principal.level("main"), Some(Access::Write));
        assert_eq!(principal.level("other"), Some(Access::Read));
        assert_eq!(principal.default_catalog.as_deref(), Some("main"));
    }

    /// A carried catalog is offered at the ceiling's level now: lowered
    /// since the page, it is carried at the lower level, and a write
    /// chosen from the old page is refused; one the ceiling no longer
    /// reaches is dropped. A tick the page never offered is refused, and so
    /// is any later use of a page whose nonce a Find spent.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_find_rechecks_what_it_carries_and_spends_the_page() {
        let test = oauth_app().await;
        let alice = signed_in(
            &test,
            "alice",
            &[("main", Access::Write), ("other", Access::Read)],
        )
        .await;
        let page = user_page(&test, &alice).await;
        ceiling::entitle(
            test.app.accounts().executor(),
            &NewEntitlement {
                user: "alice",
                catalog: "main",
                access: Access::Read,
            },
            &AuditActor::cli(),
            Utc::now(),
        )
        .await
        .expect("lower");
        ceiling::unentitle(
            test.app.accounts().executor(),
            "alice",
            "other",
            &AuditActor::cli(),
            Utc::now(),
        )
        .await
        .expect("remove");

        let (status, found) = browse_as(
            &test,
            &alice,
            &page,
            Some("zz"),
            &[
                ("grant.main", "on"),
                ("level.main", "write"),
                ("grant.other", "on"),
                ("level.other", "read"),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{found}");
        assert_eq!(
            consent_of(&test, &found).await.offered,
            [("main".to_string(), Access::Read)]
                .into_iter()
                .collect::<Ceiling>()
        );
        assert!(!found.contains("value=\"write\""), "{found}");
        // The old page's nonce was spent by the Find.
        let (stale, _, _) = consent_as(&test, Some(&alice), &page, READ_MAIN).await;
        assert_eq!(stale, StatusCode::BAD_REQUEST);
        let (write, _, body) = consent_as(
            &test,
            Some(&alice),
            &found,
            &[("grant.main", "on"), ("level.main", "write")],
        )
        .await;
        assert_eq!(write, StatusCode::BAD_REQUEST);
        assert!(body.contains("did not match what was offered"), "{body}");
        assert_eq!(oauth_grants(&test).await, 0);

        let page = user_page(&test, &alice).await;
        let (tampered, body) = browse_as(
            &test,
            &alice,
            &page,
            Some("ma"),
            &[("grant.other", "on"), ("level.other", "read")],
        )
        .await;
        assert_eq!(tampered, StatusCode::BAD_REQUEST);
        assert!(body.contains("did not match what was offered"), "{body}");
    }

    /// The offer stays bounded: more than `MAX_CARRIED` ticked catalogs are
    /// not carried; the page comes back with the message and its ticks.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_find_carries_at_most_max_carried_catalogs() {
        let test = oauth_app().await;
        let alice = signed_in(&test, "alice", &[]).await;
        // 160 catalogs, two thirds reachable: over two pages.
        seed_owned(&test, "alice", 160).await;
        let first = user_page(&test, &alice).await;
        let ticks = |consent: &crate::oauth::state::UserConsent| -> Vec<(String, String)> {
            consent
                .offered
                .keys()
                .flat_map(|catalog| {
                    [
                        (format!("grant.{catalog}"), "on".to_string()),
                        (format!("level.{catalog}"), "read".to_string()),
                    ]
                })
                .collect()
        };
        let all = ticks(&consent_of(&test, &first).await);
        let pairs: Vec<(&str, &str)> = all.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        let (status, second) = browse_as(&test, &alice, &first, None, &pairs).await;
        assert_eq!(status, StatusCode::OK, "{second}");
        let offered = consent_of(&test, &second).await;
        assert_eq!(offered.offered.len(), 2 * CONSENT_PAGE as usize);
        let all = ticks(&offered);
        let pairs: Vec<(&str, &str)> = all.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();

        let (status, refused) = browse_as(&test, &alice, &second, None, &pairs).await;

        assert_eq!(status, StatusCode::OK, "{refused}");
        assert!(refused.contains("at most 50 ticked catalogs"), "{refused}");
        assert_eq!(consent_of(&test, &refused).await.offered, offered.offered);
        assert!(TOO_MANY_CARRIED.contains(&MAX_CARRIED.to_string()));
    }

    /// Issue #736's size bound: with a team owning 50,000 catalogs, `GET
    /// /me`, `GET /session`, `GET /me/ceiling`, the consent page and a Find
    /// each read a bounded number of rows (sqlx's `rows_returned`), not one
    /// per catalog.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_team_owning_many_catalogs_keeps_each_page_small() {
        use crate::rows_returned::counting;
        let test = oauth_app().await;
        let alice = signed_in(&test, "alice", &[]).await;
        sqlx::query(
            "INSERT INTO catalogs (name, description, created_at, owner_team_id) \
             SELECT 'c' || lpad(n::text, 6, '0'), '', now(), \
                    (SELECT id FROM teams WHERE name = 'alice') \
             FROM generate_series(1, 50000) AS n",
        )
        .execute(test.app.accounts().executor())
        .await
        .unwrap();
        sqlx::query("ANALYZE catalogs")
            .execute(test.app.accounts().executor())
            .await
            .unwrap();
        let (test, alice) = (&test, &alice);
        let get = |path: &'static str| async move {
            let (status, _, body) = send_as(test, "GET", path, None, Some(alice)).await;
            assert_eq!(status, StatusCode::OK, "{path}: {body}");
            body
        };
        // Warm the session and the caches, so only the page's own reads count.
        get("/api/v1/session").await;

        let (me, me_rows, _) = counting(get("/api/v1/me")).await;
        let (_, session_rows, _) = counting(get("/api/v1/session")).await;
        let (_, listing_rows, _) = counting(get("/api/v1/me/ceiling")).await;
        let authorize = format!("/oauth/authorize?{}", user_authorize_query());
        let (page, page_rows, _) =
            counting(async { send_as(test, "GET", &authorize, None, Some(alice)).await.2 }).await;
        // The page's offer is one page (the Find below replaces it).
        assert_eq!(
            consent_of(test, &page).await.offered.len(),
            CONSENT_PAGE as usize
        );
        let ((status, found), find_rows, _) =
            counting(browse_as(test, alice, &page, Some("c04999"), &[])).await;
        assert_eq!(consent_of(test, &found).await.offered.len(), 10);

        assert!(me.contains("\"ceiling_access\":\"write\""), "{me}");
        assert_eq!(status, StatusCode::OK, "{found}");
        assert!(found.contains("name=\"grant.c049999\""), "{found}");
        for (what, rows) in [
            ("GET /me", me_rows),
            ("GET /session", session_rows),
            ("GET /me/ceiling", listing_rows),
            ("the consent page", page_rows),
            ("a Find", find_rows),
        ] {
            assert!(rows < 1_000, "{what} returned {rows} rows");
        }
        assert!(page_rows > 0, "the page's reads are counted");
    }
}
