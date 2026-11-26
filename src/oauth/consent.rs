//! The consent page (spec "Consent flow"): server-rendered HTML, a plain
//! form, no script. Step 1 asks for the sign-in code, step 2 states what will
//! be granted, and an error page ends a request that cannot go on. A
//! signed-in person instead sees [`user_page`] (spec "OAuth consent as the
//! user"): their ceiling's catalogs to choose from.
//!
//! Client names come from client metadata, which anyone can write, so every
//! piece of text goes through `escape`. There is no template engine; the
//! document shell and the response headers come from `crate::http::pages`.

use std::collections::BTreeMap;

use axum::http::StatusCode;
use axum::response::Response;

use crate::http::pages;
/// Moved to `crate::http::pages`; kept here so `mail::templates` (Task 6)
/// compiles whichever of Tasks 6 and 7a merges first (plan Decision 14).
pub(crate) use crate::http::pages::escape;
use crate::store::ceiling::Ceiling;
use crate::store::keys::Access;
use crate::store::settings::Brand;

/// What every page shows about the request.
// `Debug` by hand: the pending id and the CSRF token never reach a log.
#[derive(Clone, PartialEq, Eq)]
pub struct PageView {
    /// The pending authorization's id (a hidden field).
    pub pending_id: String,
    /// Its CSRF token (a hidden field).
    pub csrf: String,
    /// The client's name, or its id when it gave none.
    pub client_name: String,
    /// The host of the redirect URI the browser will return to.
    pub client_host: String,
    /// The client came from a CIMD URL (its name is that site's own).
    pub verified: bool,
}

impl std::fmt::Debug for PageView {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "Display/Debug impl: runs while the subscriber formats an event"
        )
    )]
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PageView")
            .field("pending_id", &"<redacted>")
            .field("csrf", &"<redacted>")
            .field("client_name", &self.client_name)
            .field("client_host", &self.client_host)
            .field("verified", &self.verified)
            .finish()
    }
}

/// What a sign-in code grants, for step 2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantSummary {
    /// `read` or `write`.
    pub access: &'static str,
    /// `every catalog`, or the names joined with ", ".
    pub catalogs: String,
    /// The actor its writes are recorded as.
    pub actor: String,
    /// The user the grant will belong to (`None` for an unassigned code).
    pub user: Option<String>,
    /// Its default catalog.
    pub default_catalog: Option<String>,
}

/// The pages' Content-Security-Policy.
///
/// `form-action 'self'`: every form posts to this server, and Approve and
/// Deny answer with [`forward_page`] rather than a redirect, so no other
/// origin is ever a form's destination (issue #435). No `script-src` and
/// no `'unsafe-inline'`: `default-src 'none'` forbids every script and
/// inline style.
pub(crate) const CONTENT_SECURITY_POLICY: &str =
    "default-src 'none'; style-src 'self' https://fonts.googleapis.com; \
     font-src https://fonts.gstatic.com; img-src 'self' data:; \
     frame-ancestors 'none'; base-uri 'none'; form-action 'self';";

/// A consent page: [`pages::page`] with [`CONTENT_SECURITY_POLICY`],
/// branded with `brand`.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
    )
)]
fn page(status: StatusCode, title: &str, body: &str, brand: &Brand) -> Response {
    pages::page(status, title, body, CONTENT_SECURITY_POLICY, brand)
}

/// The client block shared by both steps.
#[tracing::instrument(level = "trace", skip_all)]
fn client_block(view: &PageView) -> String {
    let badge = if view.verified {
        ""
    } else {
        r#"<span class="consent-badge" title="This client registered itself; its name is not checked.">unverified</span>"#
    };
    format!(
        r#"        <p><span class="consent-client" id="consent-client">{name}</span>{badge}<br>
        <span class="muted consent-host">returns to {host}</span></p>"#,
        name = escape(&view.client_name),
        host = escape(&view.client_host),
    )
}

/// The hidden fields every form carries.
#[tracing::instrument(level = "trace", skip_all)]
fn hidden_fields(view: &PageView) -> String {
    format!(
        r#"<input type="hidden" name="pending" value="{pending}">
            <input type="hidden" name="csrf" value="{csrf}">"#,
        pending = escape(&view.pending_id),
        csrf = escape(&view.csrf),
    )
}

/// A refusal's line above a form, or nothing.
#[tracing::instrument(level = "trace", skip_all)]
fn error_line(error: Option<&str>) -> String {
    error.map_or_else(String::new, |message| {
        format!(
            r#"        <p class="error consent-error" id="consent-error" role="alert">{}</p>
"#,
            escape(message)
        )
    })
}

/// Step 1's HTML.
///
/// # Arguments
/// * `view` - the request.
/// * `error` - a message from a failed attempt, if any.
///
/// # Returns
/// The page body.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) fn code_page_html(view: &PageView, error: Option<&str>) -> String {
    let error = error_line(error);
    format!(
        r#"        <h1>Connect a client</h1>
{client}
        <p class="muted">Enter the sign-in code your administrator gave you. It decides which catalogs this client may use.</p>
{error}        <form method="post" action="/oauth/authorize" id="consent-code-form">
            {hidden}
            <label>Sign-in code
                <input type="password" name="sign_in_code" id="sign-in-code" autocomplete="off" required>
            </label>
            <div class="consent-actions">
                <button type="submit" name="action" value="code" id="consent-continue">Continue</button>
                <button type="submit" name="action" value="deny" id="consent-deny" class="secondary" formnovalidate>Deny</button>
            </div>
        </form>"#,
        client = client_block(view),
        hidden = hidden_fields(view),
    )
}

/// Step 1: the sign-in code form.
///
/// # Arguments
/// * `view` - the request.
/// * `error` - a message from a failed attempt, if any.
/// * `brand` - the product name and support contact.
///
/// # Returns
/// 200 with the page.
#[tracing::instrument(level = "debug", skip_all)]
pub fn code_page(view: &PageView, error: Option<&str>, brand: &Brand) -> Response {
    page(
        StatusCode::OK,
        "Connect a client",
        &code_page_html(view, error),
        brand,
    )
}

/// Step 2's HTML.
///
/// # Arguments
/// * `view` - the request.
/// * `grant` - what the sign-in code grants.
/// * `summary_nonce` - this summary's token (a hidden field), which binds
///   its Approve to the code it shows.
///
/// # Returns
/// The page body.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) fn summary_page_html(
    view: &PageView,
    grant: &GrantSummary,
    summary_nonce: &str,
) -> String {
    let default = grant
        .default_catalog
        .as_deref()
        .map_or_else(String::new, |name| {
            format!(", default catalog <strong>{}</strong>", escape(name))
        });
    // An unassigned code keeps the wording it had before codes had users.
    let who = match &grant.user {
        Some(user) => format!(
            "as <strong>{}</strong> (actor <strong>{}</strong>)",
            escape(user),
            escape(&grant.actor)
        ),
        None => format!("as actor <strong>{}</strong>", escape(&grant.actor)),
    };
    format!(
        r#"        <h1>Allow access?</h1>
{client}
        <p class="consent-summary" id="consent-summary"><strong>{name}</strong> ({host}) will get <strong>{access}</strong> access to <strong>{catalogs}</strong> {who}{default}.</p>
        <form method="post" action="/oauth/authorize" id="consent-approve-form">
            {hidden}
            <input type="hidden" name="summary" value="{summary}">
            <div class="consent-actions">
                <button type="submit" name="action" value="approve" id="consent-approve">Approve</button>
                <button type="submit" name="action" value="deny" id="consent-deny" class="secondary">Deny</button>
            </div>
        </form>"#,
        client = client_block(view),
        name = escape(&view.client_name),
        host = escape(&view.client_host),
        // A `&'static str` from `Access::as_str`: never client text.
        access = grant.access,
        catalogs = escape(&grant.catalogs),
        hidden = hidden_fields(view),
        summary = escape(summary_nonce),
    )
}

/// Step 2: what will be granted, with Approve and Deny.
///
/// # Arguments
/// * `view` - the request.
/// * `grant` - what the sign-in code grants.
/// * `summary_nonce` - this summary's token, rendered as the hidden
///   `summary` input that binds its Approve to the code it shows.
/// * `brand` - the product name and support contact.
///
/// # Returns
/// 200 with the page.
#[tracing::instrument(level = "debug", skip_all)]
pub fn summary_page(
    view: &PageView,
    grant: &GrantSummary,
    summary_nonce: &str,
    brand: &Brand,
) -> Response {
    page(
        StatusCode::OK,
        "Allow access?",
        &summary_page_html(view, grant, summary_nonce),
        brand,
    )
}

/// How a signed-in consent page's offer was found (issue #736): the page
/// shows one page of the ceiling at a time, found by a name prefix, and
/// carries over the catalogs ticked before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Browse<'a> {
    /// The prefix the page's catalogs start with; empty for none.
    pub prefix: &'a str,
    /// Whether another page of the ceiling follows ("Next catalogs").
    pub more: bool,
    /// Whether the offer is the user's whole ceiling: only then may a
    /// lone catalog start preselected, and an empty offer say the user
    /// has nothing to share.
    pub whole: bool,
    /// The catalogs carried over ticked, at the level chosen; `None` for
    /// none.
    pub chosen: Option<&'a BTreeMap<String, Access>>,
}

impl Browse<'_> {
    /// An offer that is the whole ceiling, with nothing ticked.
    pub const WHOLE: Browse<'static> = Browse {
        prefix: "",
        more: false,
        whole: true,
        chosen: None,
    };
}

/// What the signed-in consent page shows beyond [`PageView`].
// `Debug` by hand: the consent nonce never reaches a log.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct UserPage<'a> {
    /// The signed-in user's label (never its slug).
    pub user_label: &'a str,
    /// What it offers: the catalogs and levels the user may grant, one page
    /// of the ceiling and the catalogs carried over ticked (issue #736),
    /// never the whole ceiling.
    pub ceiling: &'a Ceiling,
    /// How the offer was found: its prefix, whether more follow, whether
    /// it is the whole ceiling, and what stays ticked.
    pub browse: Browse<'a>,
    /// `(slug, shown text)` for each catalog of the ceiling, in order, every
    /// text distinct (`Labels::catalog_choices`). The slug stays in ids,
    /// names and values; only the text is shown to the person.
    pub catalog_labels: &'a [(String, String)],
    /// The page's consent nonce (the hidden `summary` input).
    pub nonce: &'a str,
    /// The "Use a sign-in code instead" link (same-origin, unescaped).
    pub code_href: &'a str,
}

impl std::fmt::Debug for UserPage<'_> {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "Display/Debug impl: runs while the subscriber formats an event"
        )
    )]
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UserPage")
            .field("user_label", &self.user_label)
            .field("ceiling", &self.ceiling)
            .field("browse", &self.browse)
            .field("nonce", &"<redacted>")
            .field("code_href", &self.code_href)
            .finish()
    }
}

/// The signed-in consent page's HTML: a checkbox and a level select per
/// ceiling catalog (only the levels the ceiling allows), a default catalog
/// select, Approve and Deny, and the sign-in code link. With an empty
/// ceiling there is nothing to approve: Deny and the link only.
///
/// The catalog controls and the default select sit in a native `<details>`
/// disclosure labelled "Advanced Options" (issue #718), so the page stays
/// script-free. With several catalogs offered it starts open and nothing is
/// preselected. With exactly one it starts closed and that catalog is
/// preselected: ticked, at its ceiling's level (write only when the ceiling
/// allows write), and as the default. A closed disclosure's controls still
/// submit, and the preselection is only the form's starting state: the
/// approval is checked exactly as any other choice is. When `error` is set
/// (a refused choice shown again) it starts open, so the controls the
/// message is about are in view.
///
/// # Arguments
/// * `view` - the request.
/// * `user` - the user, ceiling, nonce and link.
/// * `error` - a message from a refused choice, if any.
///
/// # Returns
/// The page body.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) fn user_page_html(view: &PageView, user: &UserPage<'_>, error: Option<&str>) -> String {
    let link = format!(
        r#"        <p class="muted"><a href="{}" id="consent-use-code">Use a sign-in code instead</a></p>"#,
        escape(user.code_href)
    );
    let browse = user.browse;
    if user.ceiling.is_empty() && browse.whole {
        return format!(
            r#"        <h1>Allow access?</h1>
{client}
        <p class="error consent-error" id="consent-empty">You have no catalogs to share yet. Ask an admin for access, or use a sign-in code.</p>
        <form method="post" action="/oauth/authorize" id="consent-user-form">
            {hidden}
            <div class="consent-actions">
                <button type="submit" name="action" value="deny" id="consent-deny" class="secondary">Deny</button>
            </div>
        </form>
{link}"#,
            client = client_block(view),
            hidden = hidden_fields(view),
        );
    }
    // The sole offered catalog, when there is exactly one: it starts
    // preselected. Several catalogs keep the old defaults (nothing ticked,
    // read, no default), since guessing among them would be a choice the
    // person did not make.
    // Only when the offer is the whole ceiling: one catalog found by a
    // prefix is not the only one the person could mean.
    let sole = match (user.ceiling.len(), browse.whole) {
        (1, true) => user.ceiling.keys().next().map(String::as_str),
        _ => None,
    };
    // A catalog carried over ticked, and its chosen level.
    let carried = |catalog: &str| {
        browse
            .chosen
            .and_then(|chosen| chosen.get(catalog))
            .copied()
    };
    // Catalog names are `[a-z0-9-]+` (a schema CHECK), so they are safe in
    // ids and names; they are escaped anyway, as everything is. The label
    // is free text, so it is escaped as visible text and as an attribute
    // value (`escape` covers both).
    let rows: String = user
        .catalog_labels
        .iter()
        // `filter_map` skips a slug the ceiling no longer holds (never, as
        // the caller builds both from one ceiling).
        .filter_map(|(catalog, shown)| user.ceiling.get(catalog).map(|&allowed| (catalog, shown, allowed)))
        .map(|(catalog, shown, allowed)| {
            let name = escape(catalog);
            let label = escape(shown);
            let kept = carried(catalog);
            let preselected = sole == Some(catalog.as_str());
            let checked = if preselected || kept.is_some() {
                " checked"
            } else {
                ""
            };
            // Write is offered only within the ceiling, and preselected only
            // where it is offered (or was chosen); a read-only catalog
            // starts at read, its only option.
            let write_chosen = kept.map_or(preselected, |level| level >= Access::Write);
            let write = match (allowed >= Access::Write, write_chosen) {
                (true, true) => {
                    r#"
                            <option value="write" selected>write</option>"#
                }
                (true, false) => {
                    r#"
                            <option value="write">write</option>"#
                }
                (false, _) => "",
            };
            format!(
                r#"                    <div class="consent-catalog">
                        <label><input type="checkbox" name="grant.{name}" value="on" id="consent-catalog-{name}"{checked}> {label}</label>
                        <select name="level.{name}" id="consent-level-{name}" aria-label="Access to {label}">
                            <option value="read">read</option>{write}
                        </select>
                    </div>
"#
            )
        })
        .collect();
    // Found by a prefix and nothing matched: say so where the rows go.
    let rows = if rows.is_empty() {
        format!(
            r#"                    <p class="muted" id="consent-none-found">No catalog of yours starts with <strong>{}</strong>.</p>
"#,
            escape(browse.prefix)
        )
    } else {
        rows
    };
    // The finder: a prefix and Find, which keep the ticked catalogs, and
    // Next catalogs when the ceiling has more after this page. Plain
    // submit buttons, so the page stays script-free.
    let more = if browse.more {
        r#"
                <button type="submit" name="action" value="more" id="consent-more" class="secondary" formnovalidate>Next catalogs</button>"#
    } else {
        ""
    };
    let finder = format!(
        r#"                <div class="consent-find">
                    <label>Find catalogs whose name starts with
                        <input type="search" name="find" id="consent-find" value="{prefix}" autocomplete="off">
                    </label>
                    <button type="submit" name="action" value="find" id="consent-find-go" class="secondary" formnovalidate>Find</button>{more}
                    <p class="muted small">Catalogs you tick stay chosen when you find others.</p>
                </div>
"#,
        prefix = escape(browse.prefix),
    );
    let defaults: String = user
        .catalog_labels
        .iter()
        .map(|(catalog, shown)| {
            let name = escape(catalog);
            let label = escape(shown);
            let selected = if sole == Some(catalog.as_str()) {
                " selected"
            } else {
                ""
            };
            format!(
                r#"                        <option value="{name}"{selected}>{label}</option>
"#
            )
        })
        .collect();
    // Open with several catalogs to choose among, or to show what a refusal
    // is about; closed for the one-catalog default.
    let open = if sole.is_none() || error.is_some() {
        " open"
    } else {
        ""
    };
    // With the controls closed, say in plain view what Approve grants as
    // the page starts, so consent never rests on hidden settings.
    let preset = sole
        .and_then(|catalog| {
            let allowed = user.ceiling.get(catalog)?;
            let shown = user
                .catalog_labels
                .iter()
                .find(|(slug, _)| slug == catalog)
                .map_or(catalog, |(_, shown)| shown.as_str());
            Some(format!(
                r#"        <p class="muted" id="consent-preset">Unless you change it under Advanced Options, Approve grants <strong>{level}</strong> access to <strong>{label}</strong> and makes it the default catalog.</p>
"#,
                // The level the row preselects, from the same rule.
                level = if *allowed >= Access::Write { "write" } else { "read" },
                label = escape(shown),
            ))
        })
        .unwrap_or_default();
    format!(
        r#"        <h1>Allow access?</h1>
{client}
        <p class="muted">Signed in as <strong id="consent-user">{user}</strong>. Choose what <strong>{name}</strong> may use. You can grant up to your own access.</p>
{preset}{error}        <form method="post" action="/oauth/authorize" id="consent-user-form">
            {hidden}
            <input type="hidden" name="summary" value="{nonce}">
            <details id="consent-advanced"{open}>
                <summary id="consent-advanced-toggle">Advanced Options</summary>
{finder}                <fieldset id="consent-catalogs">
                    <legend>Catalogs</legend>
{rows}                </fieldset>
                <label>Default catalog
                    <select name="default_catalog" id="consent-default">
                        <option value="">None</option>
{defaults}                    </select>
                </label>
            </details>
            <div class="consent-actions">
                <button type="submit" name="action" value="consent" id="consent-approve">Approve</button>
                <button type="submit" name="action" value="deny" id="consent-deny" class="secondary" formnovalidate>Deny</button>
            </div>
        </form>
{link}"#,
        client = client_block(view),
        user = escape(user.user_label),
        name = escape(&view.client_name),
        error = error_line(error),
        hidden = hidden_fields(view),
        nonce = escape(user.nonce),
    )
}

/// The signed-in consent page.
///
/// # Arguments
/// * `view` - the request.
/// * `user` - the user, ceiling, nonce and link.
/// * `error` - a message from a refused choice, if any.
/// * `brand` - the product name and support contact.
///
/// # Returns
/// 200 with the page.
#[tracing::instrument(level = "debug", skip_all)]
pub fn user_page(
    view: &PageView,
    user: &UserPage<'_>,
    error: Option<&str>,
    brand: &Brand,
) -> Response {
    page(
        StatusCode::OK,
        "Allow access?",
        &user_page_html(view, user, error),
        brand,
    )
}

/// The error page's HTML.
///
/// # Arguments
/// * `message` - what went wrong, for a person.
/// * `trace_id` - the request's trace id, for a person to quote when the
///   failure is the server's (issue #1104).
///
/// # Returns
/// The page body (no form).
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) fn error_page_html(message: &str, trace_id: Option<&str>) -> String {
    let reference = trace_id.map_or_else(String::new, |trace_id| {
        format!(
            "\n        <p class=\"muted\">Quote this reference when you report it: \
             <code id=\"consent-trace-id\">{}</code></p>",
            escape(trace_id)
        )
    });
    format!(
        r#"        <h1>Cannot connect</h1>
        <p class="error consent-error" id="consent-error" role="alert">{}</p>{reference}
        <p class="muted">Close this window and start again from your client.</p>"#,
        escape(message)
    )
}

/// A page that ends the request (no form).
///
/// # Arguments
/// * `status` - the HTTP status.
/// * `message` - what went wrong, for a person.
/// * `brand` - the product name and support contact.
///
/// # Returns
/// The page with `status`; a 500 also shows the request's trace id when it
/// has one (issue #1104).
#[tracing::instrument(level = "debug", skip_all)]
pub fn error_page(status: StatusCode, message: &str, brand: &Brand) -> Response {
    let trace_id = (status == StatusCode::INTERNAL_SERVER_ERROR)
        .then(crate::http::request_trace_id)
        .flatten();
    page(
        status,
        "Cannot connect",
        &error_page_html(message, trace_id.as_deref()),
        brand,
    )
}

/// The forwarding page's HTML ([`pages::forward_body`]).
///
/// # Arguments
/// * `target` - the client's redirect URI with the answer added.
///
/// # Returns
/// The page body (no form).
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) fn forward_page_html(target: &str) -> String {
    pages::forward_body("Returning to your client", "consent-forward", target)
}

/// The answer to a consent form's Approve or Deny: a page that sends the
/// browser on to the client, in place of a 303.
///
/// Browsers apply `form-action` to every redirect that follows a form
/// POST, the client's own included, so a redirect URI that itself
/// redirects to another origin (`www.` to the bare domain, say) would be
/// blocked after a 303. A refresh is not a form submission, so neither it
/// nor any redirect after it is subject to `form-action`. With no form
/// here, the policy names only `'self'`.
///
/// # Arguments
/// * `target` - the client's redirect URI with the answer added.
/// * `brand` - the product name and support contact.
///
/// # Returns
/// A 200 page.
#[tracing::instrument(level = "debug", skip_all)]
pub fn forward_page(target: &str, brand: &Brand) -> Response {
    page(
        StatusCode::OK,
        "Returning to your client",
        &forward_page_html(target),
        brand,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> PageView {
        PageView {
            pending_id: "p1".to_string(),
            csrf: "c1".to_string(),
            client_name: "<Example & Co>".to_string(),
            client_host: "client.example.com".to_string(),
            verified: false,
        }
    }

    #[test]
    fn debug_never_prints_the_pending_id_or_the_csrf_token() {
        let page = PageView {
            pending_id: "pending-id-that-must-not-leak".to_string(),
            csrf: "csrf-token-that-must-not-leak".to_string(),
            ..view()
        };

        let printed = format!("{page:?}");

        assert!(
            !printed.contains("pending-id-that-must-not-leak"),
            "{printed}"
        );
        assert!(
            !printed.contains("csrf-token-that-must-not-leak"),
            "{printed}"
        );
        assert!(printed.contains("client.example.com"), "{printed}");
    }

    #[test]
    fn debug_never_prints_the_user_pages_nonce() {
        let ceiling = Ceiling::new();
        let user = UserPage {
            user_label: "alice",
            catalog_labels: &[],
            ceiling: &ceiling,
            nonce: "consent-nonce-that-must-not-leak",
            code_href: "/oauth/authorize?a=1&signin=code",
            browse: Browse::WHOLE,
        };

        let printed = format!("{user:?}");

        assert!(
            !printed.contains("consent-nonce-that-must-not-leak"),
            "{printed}"
        );
        assert!(printed.contains("alice"), "{printed}");
    }

    #[test]
    fn the_csp_allows_forms_to_post_only_to_this_server() {
        let csp = CONTENT_SECURITY_POLICY;
        assert!(csp.ends_with("form-action 'self';"));
        assert!(csp.starts_with("default-src 'none';"));
        assert!(csp.contains("frame-ancestors 'none'"));
        assert!(!csp.contains("script-src"), "the page runs no script");
        assert!(!csp.contains("unsafe-inline"), "no inline style or script");
    }

    #[test]
    fn pages_carry_the_security_headers() {
        let response = code_page(&view(), None, &Brand::default());
        let headers = response.headers();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(headers["cache-control"], "no-store");
        assert_eq!(headers["referrer-policy"], "no-referrer");
        assert_eq!(headers["x-content-type-options"], "nosniff");
        assert_eq!(headers["content-type"], "text/html; charset=utf-8");
        assert_eq!(headers["content-security-policy"], CONTENT_SECURITY_POLICY);
    }

    #[test]
    fn client_text_is_escaped_and_unverified_clients_say_so() {
        let html = code_page_html(&view(), Some("bad <code>"));
        assert!(html.contains("&lt;Example &amp; Co&gt;"));
        assert!(!html.contains("<Example"));
        assert!(html.contains("unverified"));
        assert!(html.contains("bad &lt;code&gt;"));
        assert!(html.contains(r#"name="pending" value="p1""#));
        assert!(html.contains(r#"name="csrf" value="c1""#));
    }

    #[test]
    fn a_hostile_host_cannot_break_out_of_its_markup() {
        let hostile = PageView {
            client_host: r#"x"><script>alert(1)</script>"#.to_string(),
            verified: true,
            ..view()
        };
        let grant = GrantSummary {
            access: "read",
            catalogs: "main".to_string(),
            actor: "<b>a</b>".to_string(),
            user: Some("<i>u</i>".to_string()),
            default_catalog: None,
        };
        for html in [
            code_page_html(&hostile, None),
            summary_page_html(&hostile, &grant, "n1"),
        ] {
            assert!(!html.contains("<script>"), "{html}");
            assert!(!html.contains("<b>a</b>"), "{html}");
            assert!(!html.contains("<i>u</i>"), "{html}");
            assert!(!html.contains("unverified"), "a CIMD client is verified");
        }
    }

    #[test]
    fn pages_have_no_inline_script_or_style() {
        let grant = GrantSummary {
            access: "read",
            catalogs: "main".to_string(),
            actor: "example-actor".to_string(),
            user: Some("alice".to_string()),
            default_catalog: None,
        };
        for html in [
            pages::document(
                "t",
                &code_page_html(&view(), Some("e")),
                &[],
                &Brand::default(),
            ),
            pages::document(
                "t",
                &summary_page_html(&view(), &grant, "n1"),
                &[],
                &Brand::default(),
            ),
            pages::document("t", &error_page_html("e", None), &[], &Brand::default()),
        ] {
            assert!(!html.contains("<script"), "{html}");
            assert!(!html.contains("<style"), "{html}");
            assert!(!html.contains("style="), "{html}");
        }
    }

    #[test]
    fn the_summary_names_access_catalogs_actor_and_default() {
        let grant = GrantSummary {
            access: "write",
            catalogs: "main, other".to_string(),
            actor: "example-actor".to_string(),
            user: Some("alice".to_string()),
            default_catalog: Some("main".to_string()),
        };
        let html = summary_page_html(&view(), &grant, "n1");
        assert!(html.contains("<strong>write</strong>"));
        assert!(html.contains("<strong>main, other</strong>"));
        assert!(html.contains("as <strong>alice</strong> (actor <strong>example-actor</strong>)"));
        assert!(html.contains("default catalog <strong>main</strong>"));
        assert!(html.contains(r#"id="consent-approve""#));
        assert!(html.contains(r#"id="consent-deny""#));
        assert!(html.contains(r#"name="summary" value="n1""#));
    }

    #[test]
    fn the_summary_of_an_unassigned_code_names_only_the_actor() {
        let grant = GrantSummary {
            access: "read",
            catalogs: "main".to_string(),
            actor: "example-actor".to_string(),
            user: None,
            default_catalog: None,
        };
        let html = summary_page_html(&view(), &grant, "n1");
        assert!(html.contains("as actor <strong>example-actor</strong>."));
        assert!(!html.contains("(actor"));
    }

    fn ceiling() -> Ceiling {
        [
            ("main".to_string(), Access::Write),
            ("other".to_string(), Access::Read),
        ]
        .into_iter()
        .collect()
    }

    #[test]
    fn the_user_page_offers_each_catalog_at_most_at_its_ceiling() {
        let ceiling = ceiling();
        let labels = [
            ("main".to_string(), "Main <b>book</b>".to_string()),
            ("other".to_string(), "Other".to_string()),
        ];
        let user = UserPage {
            user_label: "<alice>",
            catalog_labels: &labels,
            ceiling: &ceiling,
            nonce: "n1",
            code_href: "/oauth/authorize?a=1&signin=code",
            browse: Browse::WHOLE,
        };

        let html = user_page_html(&view(), &user, Some("bad <choice>"));

        assert!(html.contains(r#"<strong id="consent-user">&lt;alice&gt;</strong>"#));
        assert!(html.contains(r#"name="grant.main""#));
        assert!(html.contains(r#"name="grant.other""#));
        // The visible text is the escaped label; ids, names and values keep
        // the slug.
        assert!(html.contains(r#"id="consent-catalog-main"> Main &lt;b&gt;book&lt;/b&gt;</label>"#));
        assert!(html.contains(r#"aria-label="Access to Main &lt;b&gt;book&lt;/b&gt;""#));
        assert!(html.contains(r#"<option value="main">Main &lt;b&gt;book&lt;/b&gt;</option>"#));
        assert!(!html.contains("<b>book"), "{html}");
        // `main` offers write, `other` only read.
        let other = &html[html.find(r#"name="level.other""#).expect("other's select")..];
        let other = &other[..other.find("</select>").expect("its end")];
        assert!(!other.contains("write"), "{other}");
        assert!(html.contains(r#"<option value="write">write</option>"#));
        assert!(html.contains(r#"name="summary" value="n1""#));
        assert!(html.contains(r#"href="/oauth/authorize?a=1&amp;signin=code""#));
        assert!(html.contains("bad &lt;choice&gt;"));
        assert!(html.contains(r#"id="consent-approve""#));
        let document = pages::document("t", &html, &[], &Brand::default());
        assert!(!document.contains("<script"));
        assert!(!document.contains("style="));
    }

    /// A signed-in page offering `ceiling`, labelled by slug, with `error`.
    fn user_html(ceiling: &Ceiling, error: Option<&str>) -> String {
        let labels: Vec<(String, String)> = ceiling
            .keys()
            .map(|slug| (slug.clone(), format!("{slug} <i>x</i>")))
            .collect();
        let user = UserPage {
            user_label: "alice",
            catalog_labels: &labels,
            ceiling,
            nonce: "n1",
            code_href: "/oauth/authorize?a=1&signin=code",
            browse: Browse::WHOLE,
        };
        user_page_html(&view(), &user, error)
    }

    /// The text from `start` up to the next `end` in `html`.
    fn between<'h>(html: &'h str, start: &str, end: &str) -> &'h str {
        let from = &html[html.find(start).expect(start)..];
        &from[..from.find(end).expect(end)]
    }

    /// The catalog controls and the default select sit inside the
    /// "Advanced Options" disclosure; the error line, Approve, Deny and the
    /// code link sit outside it.
    fn assert_advanced_options_hold_only_the_choices(html: &str) {
        let advanced = between(html, r#"<details id="consent-advanced""#, "</details>");
        assert!(
            advanced
                .contains(r#"<summary id="consent-advanced-toggle">Advanced Options</summary>"#),
            "{html}"
        );
        assert!(advanced.contains(r#"id="consent-catalogs""#), "{html}");
        assert!(advanced.contains(r#"name="default_catalog""#), "{html}");
        for outside in [
            r#"id="consent-approve""#,
            r#"id="consent-deny""#,
            r#"id="consent-use-code""#,
            r#"id="consent-error""#,
            r#"id="consent-client""#,
            r#"name="summary""#,
        ] {
            assert!(!advanced.contains(outside), "{outside} is outside: {html}");
        }
        // Nothing in it is disabled: a closed disclosure still submits.
        assert!(!advanced.contains("disabled"), "{html}");
    }

    #[test]
    fn several_catalogs_open_advanced_options_with_nothing_preselected() {
        let html = user_html(&ceiling(), None);

        assert_advanced_options_hold_only_the_choices(&html);
        assert!(
            html.contains(r#"<details id="consent-advanced" open>"#),
            "{html}"
        );
        assert!(!html.contains(" checked>"), "nothing is ticked: {html}");
        assert!(
            !html.contains(" selected>"),
            "read and None stay first: {html}"
        );
        assert!(!html.contains(r#"id="consent-preset""#), "{html}");
    }

    #[test]
    fn one_write_catalog_starts_closed_ticked_at_write_and_as_the_default() {
        let ceiling: Ceiling = [("main".to_string(), Access::Write)].into_iter().collect();

        let html = user_html(&ceiling, None);

        assert_advanced_options_hold_only_the_choices(&html);
        assert!(
            html.contains(r#"<details id="consent-advanced">"#),
            "{html}"
        );
        assert!(!html.contains(" open"), "{html}");
        assert!(
            html.contains(r#"name="grant.main" value="on" id="consent-catalog-main" checked>"#),
            "{html}"
        );
        let level = between(&html, r#"name="level.main""#, "</select>");
        assert!(
            level.contains(r#"<option value="write" selected>write</option>"#),
            "{level}"
        );
        let default = between(&html, r#"name="default_catalog""#, "</select>");
        assert!(
            default.contains(r#"<option value="main" selected>main &lt;i&gt;x&lt;/i&gt;</option>"#),
            "{default}"
        );
        assert!(
            default.contains(r#"<option value="">None</option>"#),
            "{default}"
        );
        // What Approve grants is said outside the closed disclosure, escaped.
        assert!(
            html.contains(
                "Approve grants <strong>write</strong> access to <strong>main &lt;i&gt;x&lt;/i&gt;</strong> and makes it the default catalog."
            ),
            "{html}"
        );
        assert!(!html.contains("<i>x</i>"), "labels are escaped: {html}");
    }

    #[test]
    fn one_read_only_catalog_starts_closed_ticked_at_read_with_no_write() {
        let ceiling: Ceiling = [("main".to_string(), Access::Read)].into_iter().collect();

        let html = user_html(&ceiling, None);

        assert_advanced_options_hold_only_the_choices(&html);
        assert!(
            html.contains(r#"<details id="consent-advanced">"#),
            "{html}"
        );
        assert!(
            html.contains(r#"id="consent-catalog-main" checked>"#),
            "{html}"
        );
        let level = between(&html, r#"name="level.main""#, "</select>");
        assert!(!level.contains("write"), "write is not offered: {level}");
        assert!(
            level.contains(r#"<option value="read">read</option>"#),
            "{level}"
        );
        let default = between(&html, r#"name="default_catalog""#, "</select>");
        assert!(
            default.contains(r#"<option value="main" selected>"#),
            "{default}"
        );
        assert!(
            html.contains("Approve grants <strong>read</strong> access to"),
            "{html}"
        );
        assert!(!html.contains("<strong>write</strong>"), "{html}");
    }

    #[test]
    fn a_refused_choice_shows_one_catalogs_page_with_advanced_options_open() {
        let ceiling: Ceiling = [("main".to_string(), Access::Write)].into_iter().collect();

        let html = user_html(&ceiling, Some("Choose at least one catalog, or Deny."));

        assert_advanced_options_hold_only_the_choices(&html);
        assert!(
            html.contains(r#"<details id="consent-advanced" open>"#),
            "{html}"
        );
        assert!(html.contains(r#"id="consent-error""#), "{html}");
        // The form starts from the same defaults again.
        assert!(
            html.contains(r#"id="consent-catalog-main" checked>"#),
            "{html}"
        );
    }

    #[test]
    fn an_empty_ceiling_offers_only_deny_and_the_code_link() {
        let empty = Ceiling::new();
        let user = UserPage {
            user_label: "alice",
            catalog_labels: &[],
            ceiling: &empty,
            nonce: "n1",
            code_href: "/oauth/authorize?a=1&signin=code",
            browse: Browse::WHOLE,
        };

        let html = user_page_html(&view(), &user, None);

        assert!(html.contains(r#"id="consent-empty""#));
        assert!(html.contains(r#"id="consent-deny""#));
        assert!(html.contains(r#"id="consent-use-code""#));
        assert!(!html.contains(r#"id="consent-approve""#));
        assert!(!html.contains(r#"name="summary""#), "nothing to approve");
        assert!(!html.contains("Advanced Options"), "nothing to choose");
        assert!(!html.contains("<details"), "{html}");
        assert!(!html.contains(r#"id="consent-preset""#), "{html}");
    }

    #[test]
    fn the_error_page_has_no_form_and_its_status() {
        let response = error_page(
            StatusCode::BAD_REQUEST,
            "Unknown <client>.",
            &Brand::default(),
        );
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(response.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("form-action 'self';"));
        let html = error_page_html("Unknown <client>.", None);
        assert!(html.contains("Unknown &lt;client&gt;."));
        assert!(!html.contains("<form"));
        assert!(!html.contains("consent-trace-id"), "no id, no reference");
    }

    #[test]
    fn the_error_page_shows_the_trace_id_it_is_given() {
        let html = error_page_html(
            "The server failed.",
            Some("0af7651916cd43dd8448eb211c80319c"),
        );

        assert!(
            html.contains(r#"<code id="consent-trace-id">0af7651916cd43dd8448eb211c80319c</code>"#),
            "{html}"
        );
    }

    #[test]
    fn the_forward_page_refreshes_and_links_to_the_escaped_target() {
        let target = "https://client.example.com/cb?code=c1&state=a\"b";

        let response = forward_page(target, &Brand::default());
        let html = forward_page_html(target);

        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().get("location").is_none());
        assert!(response.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("form-action 'self';"));
        let escaped = "https://client.example.com/cb?code=c1&amp;state=a&quot;b";
        assert!(html.contains(&format!(
            r#"<meta http-equiv="refresh" content="0;url={escaped}">"#
        )));
        assert!(html.contains(&format!(r#"id="consent-forward" href="{escaped}""#)));
        assert!(!html.contains("<form"));
    }
}
