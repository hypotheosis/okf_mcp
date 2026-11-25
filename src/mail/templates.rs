//! The mail templates (spec "Mail"): plain text with a minimal HTML twin.
//! No template engine: `format!` over fixed text, as the consent pages do,
//! with every interpolated value HTML-escaped in the twin. Every template
//! names the product as the `product_name` setting says, and ends with the
//! `support_contact` line when one is set ([`Brand`]).

use crate::oauth::consent::escape;
use crate::store::settings::{Brand, DEFAULT_PRODUCT_NAME};

/// The setup page's path; the link adds `?token=...`.
pub const SETUP_PATH: &str = "/account/setup";

/// The set-password page a reset link opens.
pub const RESET_PATH: &str = "/account/reset";

/// The closing line of the mails nobody may have asked for.
const IGNORE: &str = "If you did not expect this email, you can ignore it.";

/// A message to render. `link`s carry a single-use token, so `Debug` hides them.
#[derive(Clone, PartialEq, Eq)]
pub enum Template {
    /// An admin's invitation: choose a password or register a passkey.
    Setup {
        /// The setup link ([`link`] with [`SETUP_PATH`]).
        link: String,
        /// The user's name, for the greeting.
        user: String,
    },
    /// A forgotten password.
    Reset {
        /// The reset link ([`link`] with [`RESET_PATH`]).
        link: String,
    },
    /// A notice after a password change, a new passkey or a linked identity.
    MethodAdded {
        /// What was added, in words ("a passkey", "a password", "Example ID").
        method: String,
    },
    /// The Settings page's test email.
    Test,
    /// An operator unlocked a catalog of the recipient's team (spec Part 6).
    /// No staff name and no reason: owners see only the category.
    Unlocked {
        /// The catalog's name.
        catalog: String,
        /// Why, as `unlocks::Category::label` words ("a deletion request").
        category: String,
        /// When it expires (`clock::format_timestamp`, UTC).
        until: String,
    },
}

// A hand-written `Debug` (Python's `__repr__`) instead of `#[derive(Debug)]`,
// so `{:?}` never prints a link's token.
impl std::fmt::Debug for Template {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "Debug impl: runs while an event is formatted, and a span there re-enters the subscriber"
        )
    )]
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Template::Setup { user, .. } => formatter
                .debug_struct("Setup")
                .field("link", &"<redacted>")
                .field("user", user)
                .finish(),
            Template::Reset { .. } => formatter
                .debug_struct("Reset")
                .field("link", &"<redacted>")
                .finish(),
            Template::MethodAdded { method } => formatter
                .debug_struct("MethodAdded")
                .field("method", method)
                .finish(),
            Template::Test => formatter.write_str("Test"),
            Template::Unlocked {
                catalog,
                category,
                until,
            } => formatter
                .debug_struct("Unlocked")
                .field("catalog", catalog)
                .field("category", category)
                .field("until", until)
                .finish(),
        }
    }
}

impl Template {
    /// The template's name, as the mail queue lists it.
    ///
    /// # Returns
    /// `"setup"`, `"reset"`, `"method_added"`, `"test"` or `"unlocked"`.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn name(&self) -> &'static str {
        match self {
            Template::Setup { .. } => "setup",
            Template::Reset { .. } => "reset",
            Template::MethodAdded { .. } => "method_added",
            Template::Test => "test",
            Template::Unlocked { .. } => "unlocked",
        }
    }
}

/// Builds an emailed link from `public_url` (already validated as an
/// origin) and a raw token (base64url, so it needs no escaping). The one
/// link builder (Decision 21).
///
/// # Arguments
/// * `public_url` - the configured origin, with or without a trailing `/`.
/// * `path` - [`SETUP_PATH`] or [`RESET_PATH`].
/// * `token` - the raw token from `store::email_tokens::issue`.
///
/// # Returns
/// The absolute URL, `{public_url}{path}?token={token}`.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
    )
)]
pub fn link(public_url: &str, path: &str, token: &str) -> String {
    format!("{}{path}?token={token}", public_url.trim_end_matches('/'))
}

/// The closing contact line, when the operator set one (unescaped).
#[tracing::instrument(level = "trace", skip_all)]
fn contact_line(brand: &Brand) -> Option<String> {
    brand
        .support_contact
        .as_deref()
        .map(|contact| format!("Questions? Contact {contact}"))
}

/// The words that name the product in a subject that did not name it
/// before branding: empty for the default name, so the default renders
/// exactly as it always has ("Reset your password"), else the name and a
/// space ("Reset your Acme Notes password").
#[tracing::instrument(level = "trace", skip_all)]
fn product_words(brand: &Brand) -> String {
    if brand.product_name == DEFAULT_PRODUCT_NAME {
        String::new()
    } else {
        format!("{} ", brand.product_name)
    }
}

/// A plain-text body with the contact line, if any, as its last paragraph.
#[tracing::instrument(level = "trace", skip_all)]
fn with_contact(text: String, brand: &Brand) -> String {
    match contact_line(brand) {
        Some(line) => format!("{text}\n{line}\n"),
        None => text,
    }
}

/// Wraps paragraphs of already-escaped HTML in a minimal document, with the
/// contact line, if any, as the last paragraph.
#[tracing::instrument(level = "trace", skip_all)]
fn html_document(subject: &str, paragraphs: &[String], brand: &Brand) -> String {
    let contact = contact_line(brand).map(|line| escape(&line));
    // `chain` appends the optional paragraph (an `Option` iterates as zero
    // or one items) without copying the others into a new list.
    let body: String = paragraphs
        .iter()
        .chain(contact.as_ref())
        .map(|paragraph| format!("<p>{paragraph}</p>\n"))
        .collect();
    format!(
        "<!doctype html>\n<html><head><meta charset=\"utf-8\"><title>{}</title></head>\n\
         <body>\n{body}</body></html>\n",
        escape(subject)
    )
}

/// An escaped link, shown as its own text.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
    )
)]
fn anchor(link: &str) -> String {
    let link = escape(link);
    format!("<a href=\"{link}\">{link}</a>")
}

/// Renders a template.
///
/// # Arguments
/// * `template` - what to send.
/// * `brand` - the product name and support contact
///   (`settings::brand_in`).
///
/// # Returns
/// `(subject, plain text, HTML)`.
#[tracing::instrument(level = "debug", skip_all)]
pub fn render(template: &Template, brand: &Brand) -> (String, String, String) {
    let name = brand.product_name.as_str();
    let (subject, text, html) = match template {
        Template::Setup { link, user } => {
            let subject = format!("Set up your {name} account");
            let invited = format!(
                "An administrator invited you to {name}. Open this link to choose a \
                 password or register a passkey:"
            );
            let expiry = "The link works once and expires soon. If it has expired, ask an \
                          administrator for a new one.";
            let text = format!("Hello {user},\n\n{invited}\n\n{link}\n\n{expiry}\n\n{IGNORE}\n");
            let html = html_document(
                &subject,
                &[
                    format!("Hello {},", escape(user)),
                    escape(&invited),
                    anchor(link),
                    expiry.to_string(),
                    IGNORE.to_string(),
                ],
                brand,
            );
            (subject, text, html)
        }
        Template::Reset { link } => {
            let subject = format!("Reset your {}password", product_words(brand));
            let asked = format!(
                "Someone asked to reset the password of your {name} account. Open this \
                 link to choose a new one:"
            );
            let expiry = "The link works once and expires soon. Using it signs you out everywhere.";
            let unasked = "If you did not ask for this, ignore this email; your password is \
                           unchanged.";
            let text = format!("{asked}\n\n{link}\n\n{expiry}\n\n{unasked}\n");
            let html = html_document(
                &subject,
                &[
                    escape(&asked),
                    anchor(link),
                    expiry.to_string(),
                    unasked.to_string(),
                ],
                brand,
            );
            (subject, text, html)
        }
        Template::MethodAdded { method } => {
            let subject = format!(
                "A sign-in method was added to your {}account",
                product_words(brand)
            );
            let notice = "If this was not you, reset your password and ask an administrator \
                          to check your account.";
            let added = format!("{method} was added to your {name} account.");
            let text = format!("{added}\n\n{notice}\n");
            let html = html_document(&subject, &[escape(&added), notice.to_string()], brand);
            (subject, text, html)
        }
        Template::Test => {
            let subject = format!("{name} test email");
            let line = format!("This is a test email from {name}. Mail delivery works.");
            let html = html_document(&subject, &[escape(&line)], brand);
            (subject, format!("{line}\n"), html)
        }
        Template::Unlocked {
            catalog,
            category,
            until,
        } => {
            // The subject names the product, as every other one does, so an
            // inbox tells the products apart.
            // A label is free text and a subject is one header line: a line
            // break (or any control character) in it would start a second
            // header, so it becomes a space here, whatever the stored row says.
            let one_line: String = catalog
                .chars()
                .map(|c| {
                    if c.is_control() || c == '\u{2028}' || c == '\u{2029}' {
                        ' '
                    } else {
                        c
                    }
                })
                .collect();
            let subject = format!("An operator opened {one_line} in {name}");
            let opened = format!(
                "An operator of {name} opened your team's catalog {catalog} for {category} \
                 until {until} (UTC)."
            );
            let counted = "The operator's reads and writes are counted and shown in the audit \
                           log of your team.";
            // With a support contact, `render`'s closing "Questions? Contact
            // ..." line says whom to ask, so the template does not repeat it.
            let unexpected = brand
                .support_contact
                .is_none()
                .then_some("If you did not expect this, contact the operator.");
            // `chain` adds the optional sentence (zero or one items).
            let lines: Vec<&str> = [opened.as_str(), counted]
                .into_iter()
                .chain(unexpected)
                .collect();
            let text = format!("{}\n", lines.join("\n\n"));
            let paragraphs: Vec<String> = lines.iter().map(|line| escape(line)).collect();
            let html = html_document(&subject, &paragraphs, brand);
            (subject, text, html)
        }
    };
    (subject, with_contact(text, brand), html)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_link_joins_the_public_url_a_path_and_the_token() {
        assert_eq!(
            link("https://okf.example.com/", SETUP_PATH, "tok_en-1"),
            "https://okf.example.com/account/setup?token=tok_en-1"
        );
        assert_eq!(
            link("http://127.0.0.1:8080", RESET_PATH, "abc"),
            "http://127.0.0.1:8080/account/reset?token=abc"
        );
    }

    #[test]
    fn a_line_break_in_the_unlocked_catalog_never_reaches_the_subject_header() {
        let template = Template::Unlocked {
            catalog: "Acme\r\nBcc: evil@example.com\u{2028}x".to_string(),
            category: "support".to_string(),
            until: "2026-11-13 14:00".to_string(),
        };

        let (subject, _, _) = render(&template, &Brand::default());

        assert!(
            !subject.chars().any(|c| c.is_control() || c == '\u{2028}'),
            "{subject:?}"
        );
        assert!(
            subject.contains("Acme  Bcc: evil@example.com x"),
            "{subject:?}"
        );
    }

    #[test]
    fn every_template_has_a_subject_a_text_and_an_html_twin() {
        let setup_link = link("https://okf.example.com", SETUP_PATH, "s3cr3t");
        let templates = [
            Template::Setup {
                link: setup_link.clone(),
                user: "alice".to_string(),
            },
            Template::Reset {
                link: link("https://okf.example.com", RESET_PATH, "r3s3t"),
            },
            Template::MethodAdded {
                method: "a passkey".to_string(),
            },
            Template::Test,
        ];
        let subjects = [
            "Set up your okf account",
            "Reset your password",
            "A sign-in method was added to your account",
            "okf test email",
        ];

        for (template, subject) in templates.iter().zip(subjects) {
            let (rendered_subject, text, html) = render(template, &Brand::default());
            assert_eq!(rendered_subject, subject);
            assert!(
                !text.is_empty() && html.starts_with("<!doctype html>"),
                "{template:?}"
            );
        }
        let (_, text, html) = render(&templates[0], &Brand::default());
        assert!(text.contains(&setup_link) && text.contains("alice"));
        assert!(html.contains(&format!("href=\"{setup_link}\"")));
        let (_, text, html) = render(&templates[1], &Brand::default());
        assert!(text.contains("token=r3s3t") && html.contains("token=r3s3t"));
    }

    #[test]
    fn html_escapes_what_it_interpolates() {
        let template = Template::MethodAdded {
            method: "<b>passkey</b> & co".to_string(),
        };
        let greeting = Template::Setup {
            link: "https://okf.example.com/account/setup?token=a&b=\"c\"".to_string(),
            user: "<alice>".to_string(),
        };

        let (_, text, html) = render(&template, &Brand::default());
        let (_, _, setup_html) = render(&greeting, &Brand::default());

        assert!(
            text.contains("<b>passkey</b> & co"),
            "plain text is not escaped"
        );
        assert!(html.contains("&lt;b&gt;passkey&lt;/b&gt; &amp; co"));
        assert!(!html.contains("<b>"));
        assert!(setup_html.contains("Hello &lt;alice&gt;,"));
        assert!(setup_html.contains(
            "href=\"https://okf.example.com/account/setup?token=a&amp;b=&quot;c&quot;\""
        ));
    }

    fn acme() -> Brand {
        Brand {
            product_name: "Acme <Notes>".to_string(),
            support_contact: Some("help@example.com".to_string()),
            feedback_url: None,
            docs_url: None,
            billing_url: None,
        }
    }

    #[test]
    fn every_template_uses_the_product_name_and_ends_with_the_contact() {
        let templates = [
            Template::Setup {
                link: "https://example.com/l".into(),
                user: "alice".into(),
            },
            Template::Reset {
                link: "https://example.com/l".into(),
            },
            Template::MethodAdded {
                method: "a passkey".into(),
            },
            Template::Test,
            unlocked(),
        ];
        let subjects = [
            "Set up your Acme <Notes> account",
            "Reset your Acme <Notes> password",
            "A sign-in method was added to your Acme <Notes> account",
            "Acme <Notes> test email",
            "An operator opened main in Acme <Notes>",
        ];
        for (template, want) in templates.iter().zip(subjects) {
            let (subject, text, html) = render(template, &acme());
            // Every subject names the product on its own, so an inbox tells
            // the products apart.
            assert_eq!(subject, want, "{template:?}");
            assert!(
                html.contains("<title>") && html.contains(&escape(want)),
                "{template:?}: {html}"
            );
            assert!(
                !format!("{subject}{text}{html}").contains("okf"),
                "{template:?}: {text}"
            );
            assert!(text.contains("Acme <Notes>"), "{template:?}: {text}");
            // Escaped in the HTML twin, never raw.
            assert!(html.contains("Acme &lt;Notes&gt;"), "{template:?}: {html}");
            assert!(!html.contains("<Notes>"), "{template:?}: {html}");
            assert!(
                text.ends_with("\n\nQuestions? Contact help@example.com\n"),
                "{text}"
            );
            assert!(
                html.contains("<p>Questions? Contact help@example.com</p>\n</body>"),
                "{html}"
            );
        }
    }

    #[test]
    fn the_default_brand_renders_as_before() {
        let (subject, text, html) = render(&Template::Test, &Brand::default());

        assert_eq!(subject, "okf test email");
        assert_eq!(
            text,
            "This is a test email from okf. Mail delivery works.\n"
        );
        assert!(!html.contains("Questions?"), "{html}");
    }

    fn unlocked() -> Template {
        Template::Unlocked {
            catalog: "main".to_string(),
            category: "a deletion request".to_string(),
            until: "2026-11-13T14:00:00Z".to_string(),
        }
    }

    #[test]
    fn the_unlocked_mail_names_the_catalog_category_and_end_and_no_staff() {
        let (subject, text, html) = render(&unlocked(), &Brand::default());
        let (_, branded, branded_html) = render(&unlocked(), &acme());

        assert_eq!(subject, "An operator opened main in okf");
        assert_eq!(
            text,
            "An operator of okf opened your team's catalog main for a deletion request until \
             2026-11-13T14:00:00Z (UTC).\n\nThe operator's reads and writes are counted and \
             shown in the audit log of your team.\n\nIf you did not expect this, contact the \
             operator.\n"
        );
        assert!(html.contains("<p>If you did not expect this, contact the operator.</p>"));
        // With a contact, the closing line names it once, and the template's
        // own "contact the operator" sentence is left out.
        assert!(!branded.contains("contact the operator"), "{branded}");
        assert_eq!(branded.matches("Contact help@example.com").count(), 1);
        assert!(branded.contains("An operator of Acme <Notes> opened"));
        assert!(branded_html.contains("An operator of Acme &lt;Notes&gt; opened"));
        assert!(!branded_html.contains("<Notes>"), "{branded_html}");
    }

    #[test]
    fn names_are_stable_and_debug_hides_links() {
        let setup = Template::Setup {
            link: "https://okf.example.com/x?token=s3cr3t".to_string(),
            user: "alice".to_string(),
        };
        let reset = Template::Reset {
            link: "https://okf.example.com/x?token=r3s3t".to_string(),
        };

        assert_eq!(
            [
                setup.name(),
                reset.name(),
                Template::MethodAdded {
                    method: String::new()
                }
                .name(),
                Template::Test.name(),
                unlocked().name()
            ],
            ["setup", "reset", "method_added", "test", "unlocked"]
        );
        let shown = format!("{setup:?} {reset:?}");
        assert!(
            !shown.contains("s3cr3t") && !shown.contains("r3s3t") && shown.contains("alice"),
            "{shown}"
        );
    }
}
