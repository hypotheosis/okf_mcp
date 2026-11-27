//! `guide` (issue #1174): the server's instructions, on request.
//!
//! Some clients drop the `instructions` a server sends in `initialize` and show the model only
//! the tool list. This tool hands the model the same text, so it can read how to use the server
//! before it decides what to do. It returns exactly what `get_info` sends: the session's
//! composed instructions are held once on [`OkfServer`], and both read that one field.
//! It needs no catalog and works for any credential, `read` included.

use axum::http::request::Parts;
use rmcp::handler::server::tool::Extension;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router, Json};
use schemars::JsonSchema;
use serde::Serialize;

use crate::mcp::{reply, OkfServer};

/// The tool's name.
pub(crate) const TOOL: &str = "guide";

/// Where [`DESCRIPTION`] names the product: the words [`description`] replaces with the
/// server's product name.
const PRODUCT: &str = "this server";

/// The tool's description as written on the tool, naming no product (the public tool reference
/// shows it so); a session's tool list names the product instead ([`description`]). It opens
/// with a label that fits the 31 characters a client that defers tools shows (issue #1174).
pub(crate) const DESCRIPTION: &str = "Read first: how to use this server to remember and recall \
    what the person shares (their notes, facts and preferences), and how saving works. \
    Returns the server's instructions, the same text it sends when a session starts, for \
    clients that do not show them to you.";

/// The tool's description for a session: [`DESCRIPTION`] naming the product.
///
/// # Arguments
/// * `product_name` - the effective `product_name` setting.
///
/// # Returns
/// The description, "Read first: how to use Acme Notes to remember ...".
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial conversion: pure arithmetic or formatting on its arguments, no I/O"
    )
)]
pub(crate) fn description(product_name: &str) -> String {
    DESCRIPTION.replace(PRODUCT, product_name)
}

/// `guide`'s result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Guide {
    /// The server's instructions, exactly as `initialize` sends them.
    pub instructions: String,
}

// The router for this module's tool, joined in `OkfServer::tools`.
#[tool_router(router = guide_router, vis = "pub(crate)")]
impl OkfServer {
    /// The `guide` tool.
    #[tool(
        title = "Guide",
        description = DESCRIPTION,
        annotations(
            title = "Guide",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn guide(
        &self,
        Extension(parts): Extension<Parts>,
    ) -> Result<Json<Guide>, CallToolResult> {
        // The credential is checked as for every tool; the text needs no catalog.
        self.context(&parts)?;
        reply(Ok(Guide {
            instructions: self.instructions().to_string(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rmcp::ServerHandler;

    use super::*;
    use crate::config::{AuthConfig, Config, FeedbackConfig};
    use crate::mail::feedback::tests::FORM;
    use crate::mcp::testing::app_with;
    use crate::mcp::{compose_instructions, INSTRUCTIONS};

    async fn server_for(config: Config) -> OkfServer {
        let test = app_with(&[("pg", &[])], config).await;
        OkfServer::new(Arc::clone(&test.app))
    }

    fn branded(feedback: bool) -> Config {
        Config {
            auth: AuthConfig {
                product_name: "Telekin".to_string(),
                ..AuthConfig::default()
            },
            feedback: feedback.then(|| FeedbackConfig::for_form(FORM)),
            ..Config::default()
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_guide_text_is_what_get_info_sends_without_feedback() {
        let server = server_for(branded(false)).await;

        assert_eq!(server.instructions(), INSTRUCTIONS);
        assert_eq!(
            server.get_info().instructions.as_deref(),
            Some(server.instructions())
        );
        assert_eq!(server.instructions(), compose_instructions(None));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_guide_text_is_what_get_info_sends_with_feedback() {
        let server = server_for(branded(true)).await;

        assert!(server.instructions().len() > INSTRUCTIONS.len());
        assert_eq!(
            server.get_info().instructions.as_deref(),
            Some(server.instructions())
        );
        assert_eq!(server.instructions(), compose_instructions(Some("Telekin")));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_guide_opens_with_a_read_first_label_naming_the_product() {
        let server = server_for(branded(false)).await;

        let tool = server.tool_router.get(TOOL).expect("guide is registered");
        let shown = tool.description.as_deref().unwrap_or_default();

        assert_eq!(&shown[..30], "Read first: how to use Telekin");
        assert!(!shown.contains(PRODUCT), "{shown}");
    }
}
