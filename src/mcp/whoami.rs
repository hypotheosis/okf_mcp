//! `whoami` (issue #666): who the calling credential acts for, what it may
//! reach, and the server's clock. It needs no catalog and works for any
//! credential, `read` included.
//!
//! It shows the credential's own kind, access, reach and default catalog,
//! plus the user and their label, and no key id, key name, token or grant
//! id (`list_changesets` shows the key id and name; this does not). The
//! actor is the one that tool and `history` already name, with its label
//! from the label directory.

use axum::http::request::Parts;
use rmcp::handler::server::tool::Extension;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router, Json};
use schemars::JsonSchema;
use serde::Serialize;

use crate::auth::{CredentialKind, Reach};
use crate::clock::format_timestamp;
use crate::error::OkfError;
use crate::labels::directory::Wanted;
use crate::mcp::context::ToolContext;
use crate::mcp::{reply, OkfServer};

/// The tool's description: short, since every client loads it up front.
const DESCRIPTION: &str = "Who this connection acts for and the server's current time: the \
    user (id and label), the actor writes are recorded as (what `by` defaults to), the \
    credential (`api_key`, or `oauth` with its client's name), its access, the catalogs it \
    reaches with each level, its default catalog, and `now` (UTC, as stamps are written). \
    For the connection's selection, see `list_catalogs`.";

/// One catalog a credential is granted, and its level there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct GrantedCatalog {
    /// The catalog's id.
    pub name: String,
    /// The credential's level there: `read`, `write` or `admin`.
    pub level: String,
}

/// `whoami`'s result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct WhoAmI {
    /// The user's id; null for a credential no user owns yet.
    pub user: Option<String>,
    /// The user's label: say this to people. Null with no user.
    pub user_label: Option<String>,
    /// What writes are recorded as, and what `by` defaults to.
    pub actor: String,
    /// The label of the actor when the actor is itself a user (a console session); null for a key or OAuth actor. The person behind a credential is `user_label`.
    pub actor_label: Option<String>,
    /// `api_key` or `oauth`.
    pub credential: String,
    /// The OAuth client's name, as it names itself; null for an API key.
    pub client_name: Option<String>,
    /// The credential's highest access level: `read`, `write` or `admin`.
    pub access: String,
    /// True when it reaches every catalog, present and future, at `access`
    /// (then `catalogs` is empty).
    pub all_catalogs: bool,
    /// The catalogs it reaches, with its level on each; empty when `all_catalogs`.
    pub catalogs: Vec<GrantedCatalog>,
    /// The credential's default catalog, if it has one.
    pub default_catalog: Option<String>,
    /// The server's current UTC time (`2026-11-02T21:00:00Z`), as stamps are written.
    pub now: String,
}

/// Describes the calling credential and reads the server's clock.
///
/// # Arguments
/// * `ctx` - the call's context.
///
/// # Returns
/// The credential's user, actor, kind, access and reach, and `now`.
///
/// # Errors
/// [`OkfError::Forbidden`] for a console session, which is no MCP
/// credential (as `send_feedback` refuses it); [`OkfError::Database`] if
/// the labels cannot be read.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn whoami(ctx: &ToolContext) -> Result<WhoAmI, OkfError> {
    let principal = ctx.principal();
    let (credential, client_name) = match &principal.kind {
        CredentialKind::ApiKey => ("api_key", None),
        CredentialKind::OAuth { client_name, .. } => ("oauth", client_name.clone()),
        // A console session never reaches the MCP route.
        CredentialKind::Session { .. } => {
            return Err(OkfError::Forbidden {
                reason: "whoami takes an API key or an OAuth token".to_string(),
            })
        }
    };
    let mut wanted = Wanted::default();
    wanted.user(&principal.actor);
    if let Some(user) = &principal.user {
        wanted.user(user);
    }
    let labels = ctx.labels_for(&wanted).await?;
    let (all_catalogs, catalogs) = match &principal.reach {
        Reach::All => (true, Vec::new()),
        Reach::Catalogs(levels) => (
            false,
            levels
                .iter()
                .map(|(name, level)| GrantedCatalog {
                    name: name.clone(),
                    level: level.as_str().to_string(),
                })
                .collect(),
        ),
        // Only a console session has a ceiling, refused above.
        Reach::Ceiling => (false, Vec::new()),
    };
    Ok(WhoAmI {
        user: principal.user.clone(),
        user_label: principal
            .user
            .as_deref()
            .and_then(|user| labels.user(user))
            .map(str::to_string),
        actor: principal.actor.clone(),
        actor_label: labels.actor(&principal.actor).map(str::to_string),
        credential: credential.to_string(),
        client_name,
        access: principal.access.as_str().to_string(),
        all_catalogs,
        catalogs,
        default_catalog: principal.default_catalog.clone(),
        now: format_timestamp(ctx.app().clock().now()),
    })
}

// The router for this module's tool, joined in `OkfServer::tools`.
#[tool_router(router = whoami_router, vis = "pub(crate)")]
impl OkfServer {
    /// The `whoami` tool.
    #[tool(
        title = "Who Am I",
        description = DESCRIPTION,
        annotations(
            title = "Who Am I",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn whoami(
        &self,
        Extension(parts): Extension<Parts>,
    ) -> Result<Json<WhoAmI>, CallToolResult> {
        let ctx = self.context(&parts)?;
        reply(whoami(&ctx).await)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::auth::Principal;
    use crate::clock::FixedClock;
    use crate::config::Config;
    use crate::mcp::testing::{app_at, app_with, context, principal, principal_as};
    use crate::store::keys::Access;

    /// A `read` principal owned by `test-user`, as an API key or an OAuth grant.
    fn owned(oauth: bool) -> Arc<Principal> {
        let mut made = (*principal(&["pg"], Some("pg"))).clone();
        made.user = Some(crate::db::TEST_USER.to_string());
        if oauth {
            made.actor = "Example Client".to_string();
            made.kind = CredentialKind::OAuth {
                client_id: Some("client-1".to_string()),
                client_name: Some("Example Client".to_string()),
            };
        }
        Arc::new(made)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_api_key_names_its_user_actor_reach_and_the_injected_clock() {
        let at = Utc.with_ymd_and_hms(2026, 11, 2, 21, 30, 5).unwrap();
        let test = app_at(
            &[("pg", &[])],
            Config::default(),
            at,
            Arc::new(FixedClock(at)),
        )
        .await;

        let me = whoami(&context(&test, owned(false), None)).await.unwrap();

        assert_eq!(me.user.as_deref(), Some(crate::db::TEST_USER));
        assert!(me.user_label.is_some());
        assert_eq!(me.actor, "tester");
        assert_eq!(me.credential, "api_key");
        assert_eq!(me.client_name, None);
        assert_eq!(me.access, "read");
        assert!(!me.all_catalogs);
        assert_eq!(
            me.catalogs,
            [GrantedCatalog {
                name: "pg".to_string(),
                level: "read".to_string()
            }]
        );
        assert_eq!(me.default_catalog.as_deref(), Some("pg"));
        assert_eq!(me.now, "2026-11-02T21:30:05Z");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_oauth_grant_names_its_client_and_its_user() {
        let test = app_with(&[("pg", &[])], Config::default()).await;

        let me = whoami(&context(&test, owned(true), None)).await.unwrap();

        assert_eq!(me.credential, "oauth");
        assert_eq!(me.client_name.as_deref(), Some("Example Client"));
        assert_eq!(me.actor, "Example Client");
        assert_eq!(me.actor_label, None, "an OAuth actor is no user");
        assert_eq!(me.user.as_deref(), Some(crate::db::TEST_USER));
        assert!(me.user_label.is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unassigned_key_with_no_selection_still_answers() {
        let test = app_with(&[("pg", &[])], Config::default()).await;
        let key = principal_as("legacy", Access::Read, &[], None);

        let me = whoami(&context(&test, key, None)).await.unwrap();

        assert_eq!(me.user, None);
        assert_eq!(me.user_label, None);
        assert_eq!(me.actor, "legacy");
        assert!(me.all_catalogs);
        assert!(me.catalogs.is_empty());
        assert_eq!(me.default_catalog, None);
        assert_eq!(me.access, "read");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_console_session_is_refused() {
        let test = app_with(&[("pg", &[])], Config::default()).await;
        let mut made = (*principal(&[], None)).clone();
        made.kind = CredentialKind::Session {
            user_id: "u".to_string(),
        };

        let refused = whoami(&context(&test, Arc::new(made), None))
            .await
            .unwrap_err();

        assert_eq!(refused.kind(), "Forbidden");
    }

    #[test]
    fn the_tool_is_read_only_and_takes_no_arguments() {
        let listed = OkfServer::tools()
            .list_all()
            .into_iter()
            .find(|tool| tool.name == "whoami")
            .expect("whoami is registered");

        let hints = listed.annotations.expect("annotated");
        assert_eq!(hints.read_only_hint, Some(true));
        assert_eq!(hints.idempotent_hint, Some(true));
        assert_eq!(listed.input_schema["properties"], serde_json::json!({}));
        assert!(DESCRIPTION.len() < 500);
    }
}
