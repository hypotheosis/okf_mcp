//! How a `resource` parameter that this server refused differs from the one
//! it expects (issue #1011).
//!
//! A refused `resource` is a client's spelling of this server's MCP endpoint
//! (`<public_url>/mcp`) or of something else. The spans of the refusals
//! carry the answer from [`classify`] as `okf.resource`, a closed set, so an
//! operator can tell which it was without the value ever reaching a trace,
//! log or error body.

use url::Url;

/// How a refused `resource` differs from the expected one.
///
/// The set is closed: [`ResourceMismatch::as_str`] is the only spelling that
/// reaches telemetry, and no spelling contains a substring (`auth`, `token`,
/// `secret`, `password`, `key`, `cookie`, `session`) that trace backends'
/// default scrubbers hide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceMismatch {
    /// The endpoint's own URL with a `/` added.
    TrailingSlash,
    /// This server's origin and nothing more: no path (or just `/`).
    Origin,
    /// This server's origin, but a path that is neither the endpoint's, the
    /// endpoint's with a trailing slash, nor empty.
    OtherPath,
    /// A different origin: another host, or the same host with another
    /// scheme (`http` for `https`) or port, or a URL with no host at all.
    OtherHost,
    /// Not an absolute URL: empty, relative or malformed.
    Unparseable,
    /// The endpoint, written another way: the same URL after parsing
    /// (letter case in the scheme or host, the scheme's default port, dot
    /// segments), or with a query, fragment or user info added.
    OtherForm,
}

impl ResourceMismatch {
    /// The `okf.resource` span value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TrailingSlash => "trailing_slash",
            Self::Origin => "origin",
            Self::OtherPath => "other_path",
            Self::OtherHost => "other_host",
            Self::Unparseable => "unparseable",
            Self::OtherForm => "other_form",
        }
    }
}

/// Classifies how `sent` differs from `expected`.
///
/// Meant for a `sent` already known not to equal `expected` byte for byte;
/// an equal one comes back as [`ResourceMismatch::OtherForm`]. The value of
/// `sent` is read, never kept or returned.
///
/// # Arguments
/// * `sent` - the `resource` the client sent.
/// * `expected` - the server's own resource, an absolute URL.
///
/// # Returns
/// The first of these that fits, in this order: `Unparseable` (either side
/// is not an absolute URL), `OtherHost` (the scheme, host or port differs),
/// `TrailingSlash`, `Origin` (an empty path), `OtherPath`, else `OtherForm`.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "pure classifier: parses two strings and returns a value, a span would record nothing new"
    )
)]
pub fn classify(sent: &str, expected: &str) -> ResourceMismatch {
    let (Ok(sent), Ok(expected)) = (Url::parse(sent), Url::parse(expected)) else {
        return ResourceMismatch::Unparseable;
    };
    // `Url` lowercases the scheme and host and drops a default port, so
    // letter case alone never makes an origin differ.
    if sent.origin() != expected.origin() {
        return ResourceMismatch::OtherHost;
    }
    let (sent_path, expected_path) = (sent.path(), expected.path());
    if sent_path == expected_path {
        ResourceMismatch::OtherForm
    } else if sent_path.strip_suffix('/') == Some(expected_path) {
        ResourceMismatch::TrailingSlash
    } else if sent_path == "/" {
        ResourceMismatch::Origin
    } else {
        ResourceMismatch::OtherPath
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXPECTED: &str = "https://okf.example.com/mcp";

    fn of(sent: &str) -> &'static str {
        classify(sent, EXPECTED).as_str()
    }

    #[test]
    fn a_trailing_slash_is_named() {
        assert_eq!(of("https://okf.example.com/mcp/"), "trailing_slash");
        // A query on top does not hide it.
        assert_eq!(of("https://okf.example.com/mcp/?a=b"), "trailing_slash");
    }

    #[test]
    fn the_bare_origin_is_named() {
        assert_eq!(of("https://okf.example.com"), "origin");
        assert_eq!(of("https://okf.example.com/"), "origin");
    }

    #[test]
    fn another_path_on_this_host_is_named() {
        assert_eq!(of("https://okf.example.com/other"), "other_path");
        assert_eq!(of("https://okf.example.com/MCP"), "other_path");
        assert_eq!(of("https://okf.example.com/mcp/extra"), "other_path");
    }

    #[test]
    fn another_scheme_host_or_port_is_another_host() {
        assert_eq!(of("https://other.example.com/mcp"), "other_host");
        assert_eq!(of("http://okf.example.com/mcp"), "other_host");
        assert_eq!(of("https://okf.example.com:8443/mcp"), "other_host");
        assert_eq!(of("urn:example:mcp"), "other_host");
    }

    #[test]
    fn a_default_port_and_letter_case_are_the_same_endpoint_in_another_form() {
        assert_eq!(of("https://OKF.Example.com/mcp"), "other_form");
        assert_eq!(of("HTTPS://okf.example.com:443/mcp"), "other_form");
        assert_eq!(of("https://okf.example.com/a/../mcp"), "other_form");
    }

    #[test]
    fn a_query_fragment_or_user_info_is_another_form() {
        assert_eq!(of("https://okf.example.com/mcp?x=1"), "other_form");
        assert_eq!(of("https://okf.example.com/mcp#frag"), "other_form");
        assert_eq!(of("https://user@okf.example.com/mcp"), "other_form");
    }

    #[test]
    fn empty_relative_and_malformed_values_are_unparseable() {
        for sent in [
            "",
            "/mcp",
            "mcp",
            "https://",
            "not a url",
            "https://exa mple.com/mcp",
        ] {
            assert_eq!(of(sent), "unparseable", "{sent:?}");
        }
        assert_eq!(
            classify(EXPECTED, "not a url"),
            ResourceMismatch::Unparseable
        );
    }

    #[test]
    fn an_equal_value_is_another_form() {
        assert_eq!(of(EXPECTED), "other_form");
    }

    /// Whatever is sent, the answer is one of the six values, none of which
    /// holds a substring a trace backend scrubs, and the value is not in it.
    #[hegel::test]
    fn no_input_makes_the_answer_leave_the_closed_set(tc: hegel::TestCase) {
        use hegel::generators;
        const SET: [&str; 6] = [
            "trailing_slash",
            "origin",
            "other_path",
            "other_host",
            "unparseable",
            "other_form",
        ];
        let sent: String = tc.draw(generators::text());
        let answer = classify(&sent, EXPECTED).as_str();
        assert!(SET.contains(&answer), "{answer}");
        for scrubbed in [
            "auth", "token", "secret", "password", "key", "cookie", "session",
        ] {
            assert!(!answer.contains(scrubbed), "{answer}");
        }
    }
}
