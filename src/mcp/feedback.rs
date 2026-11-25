//! `send_feedback` (issue #674): feedback from a user or an agent about using
//! the server, queued for the operator's Forminit form. Offered only with a
//! `[feedback]` table ([`crate::mcp::OkfServer::new`] drops it otherwise).
//! It needs no catalog and works for any credential, `read` included.
//! Delivery is the worker's job ([`crate::mail::feedback`]).

use axum::http::request::Parts;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router, Json, Peer, RoleServer};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::auth::CredentialKind;
use crate::error::OkfError;
use crate::mail::feedback::{self, Draft, FeedbackOrigin, Sender, MAX_CLIENT_CHARS};
use crate::mcp::context::ToolContext;
use crate::mcp::{reply, OkfServer};

/// The tool's name.
pub(crate) const TOOL: &str = "send_feedback";

/// The `userId` sent for a credential no user owns yet (a key from before
/// users existed that no admin has assigned).
pub(crate) const UNASSIGNED_USER: &str = "unassigned";

/// The client name sent when the session's `initialize` named none.
pub(crate) const UNKNOWN_CLIENT: &str = "unknown";

/// Where [`DESCRIPTION`] names the product: the words [`description`]
/// replaces with the server's product name.
const PRODUCT: &str = "this server";

/// The tool's description as written on the tool, naming no product (the
/// public tool reference shows it so); a session's tool list names the
/// product instead ([`description`]). A test checks the third-party
/// disclosure and the "ask the user first" guidance are in it.
pub(crate) const DESCRIPTION: &str = concat!(
    "Send feedback about using this server (something confusing, broken, missing or good) to \
     this server's team. ",
    // The same disclosure the console's feedback form shows.
    feedback::third_party_disclosure!(),
    " Ask the user before including catalog \
    content, personal details or anything the user hasn't agreed to share. When the user asks \
    you to send feedback, send the user's own words and set `from` to `user`; when you report \
    your own experience, set `from` to `agent`. What is sent: message, kind, from, tool, \
    rating, the server version, your MCP client's name, the credential kind (api_key or oauth), \
    the feedback's id and your user name as the sender's userId. No email, display name, key name, token or \
    catalog content is sent unless it is in the message. A message is at most 4,000 characters, \
    and each credential may send a limited number per hour (10 by default). Returns the \
    feedback's id at once; delivery happens in the background."
);

/// The tool's description for a session: [`DESCRIPTION`] naming the
/// product.
///
/// # Arguments
/// * `product_name` - the effective `product_name` setting.
///
/// # Returns
/// The description, "Send feedback about using Acme Notes ... to Acme
/// Notes's team ...".
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

/// `send_feedback`'s arguments.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct SendFeedbackParams {
    /// The feedback, in the user's or the agent's own words (at most 4,000 characters).
    pub message: String,
    /// What it is about: `bug`, `confusing`, `missing`, `praise` or `other` (default `other`).
    pub kind: Option<String>,
    /// `user` (the person asked for it to be sent) or `agent` (the agent reports its own experience); default `agent`.
    pub from: Option<String>,
    /// The tool the feedback is about, if any (`publish`).
    pub tool: Option<String>,
    /// A rating from 1 to 5, if any.
    pub rating: Option<i64>,
}

/// `send_feedback`'s result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct FeedbackSent {
    /// Always true: the feedback is stored and will be forwarded.
    pub sent: bool,
    /// The feedback's id.
    pub id: String,
}

/// The MCP client's name, safe to store and forward: control characters
/// dropped, cut to [`MAX_CLIENT_CHARS`], [`UNKNOWN_CLIENT`] when empty.
///
/// # Arguments
/// * `name` - what the client called itself in `initialize`, if anything.
///
/// # Returns
/// The name to send.
#[tracing::instrument(level = "trace", skip_all)]
pub(crate) fn client_name(name: Option<&str>) -> String {
    let cleaned: String = name
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_CLIENT_CHARS)
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        UNKNOWN_CLIENT.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Checks the arguments, then stores the feedback and queues it, through
/// [`feedback::check_and_submit`] (which the console's
/// `POST /api/v1/feedback` calls too). `from` defaults to `agent`.
///
/// # Arguments
/// * `ctx` - the call's context (its principal and app).
/// * `client` - the MCP client's name from `initialize`, if known.
/// * `params` - the arguments.
///
/// # Returns
/// `{sent: true, id}`.
///
/// # Errors
/// [`OkfError::Forbidden`] for a console session (which never reaches the
/// MCP route); otherwise as [`feedback::check_and_submit`], whose
/// rate limit counts this credential.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn send(
    ctx: &ToolContext,
    client: Option<&str>,
    params: &SendFeedbackParams,
) -> Result<FeedbackSent, OkfError> {
    let principal = ctx.principal();
    let credential_kind = match principal.kind {
        CredentialKind::ApiKey => "api_key",
        CredentialKind::OAuth { .. } => "oauth",
        // A console session never reaches the MCP route.
        CredentialKind::Session { .. } => {
            return Err(OkfError::Forbidden {
                reason: "send_feedback takes an API key or an OAuth token".to_string(),
            })
        }
    };
    let client = client_name(client);
    let sender = Sender {
        credential: &principal.key_id,
        user_slug: principal.user.as_deref().unwrap_or(UNASSIGNED_USER),
        registered: principal.user.is_some(),
        client: &client,
        credential_kind,
    };
    let draft = Draft {
        message: &params.message,
        kind: params.kind.as_deref(),
        from: params.from.as_deref(),
        default_origin: FeedbackOrigin::Agent,
        tool: params.tool.as_deref(),
        rating: params.rating,
    };
    let id = feedback::check_and_submit(ctx.app(), &sender, &draft).await?;
    Ok(FeedbackSent { sent: true, id })
}

// The router for this module's tool, joined in `OkfServer::tools`.
#[tool_router(router = feedback_router, vis = "pub(crate)")]
impl OkfServer {
    /// The `send_feedback` tool.
    // `peer: Peer<RoleServer>`: rmcp fills it with the session's client,
    // whose `initialize` named it (`peer_info`).
    #[tool(
        title = "Send Feedback",
        description = DESCRIPTION,
        annotations(
            title = "Send Feedback",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn send_feedback(
        &self,
        Extension(parts): Extension<Parts>,
        peer: Peer<RoleServer>,
        Parameters(params): Parameters<SendFeedbackParams>,
    ) -> Result<Json<FeedbackSent>, CallToolResult> {
        let ctx = self.context(&parts)?;
        let info = peer.peer_info();
        // `as_deref` reaches through the `Option<Arc<..>>` to the params.
        let client = info.as_deref().map(|info| info.client_info.name.as_str());
        reply(send(&ctx, client, &params).await)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::auth::Principal;
    use crate::config::AuthConfig;
    use crate::config::{Config, FeedbackConfig};
    use crate::mail::feedback::tests::{config_on, mock_forminit, API_KEY, FORM};
    use crate::mail::feedback::MAX_MESSAGE_CHARS;
    use crate::mcp::testing::{app_with, context, principal, McpProbe, TestApp};
    use crate::mcp::{feedback_instructions, OkfServer, INSTRUCTIONS, TOOL_REFERENCE_URL};
    use crate::store::keys::{self, Access, Grants, NewKey};

    /// A `[feedback]` config whose form is never reached (nothing runs a
    /// worker in these tests).
    fn configured() -> Config {
        Config {
            feedback: Some(FeedbackConfig::for_form(FORM)),
            ..Config::default()
        }
    }

    fn params(message: &str) -> SendFeedbackParams {
        SendFeedbackParams {
            message: message.to_string(),
            kind: None,
            from: None,
            tool: None,
            rating: None,
        }
    }

    /// A `read` principal owned by `alice`, as an API key or an OAuth grant.
    fn reader(oauth: bool) -> Arc<Principal> {
        let mut made = (*principal(&["pg"], None)).clone();
        made.user = Some("alice".to_string());
        if oauth {
            made.kind = CredentialKind::OAuth {
                client_id: Some("client-1".to_string()),
                client_name: Some("Example Client".to_string()),
            };
        }
        Arc::new(made)
    }

    /// Creates the user `alice` that [`reader`] names: a submission from a
    /// user takes that user's row lock and is refused when it is gone.
    async fn seed_alice(test: &TestApp) {
        crate::store::users::create(
            test.app.accounts().executor(),
            &crate::store::users::NewUser {
                name: "alice",
                display_name: &crate::labels::DisplayName::default(),
            },
            &crate::store::audit::AuditActor::cli(),
            chrono::Utc::now(),
        )
        .await
        .unwrap();
    }

    /// Every stored row's forwarded fields, oldest first.
    async fn rows(test: &TestApp) -> Vec<(String, String, String, String, String)> {
        sqlx::query_as(
            "SELECT credential, user_slug, kind, origin, credential_kind FROM feedback \
             ORDER BY created_at, id",
        )
        .fetch_all(test.app.accounts().executor())
        .await
        .unwrap()
    }

    #[test]
    fn the_description_discloses_the_third_party_and_asks_before_sharing() {
        let tool = OkfServer::tools()
            .list_all()
            .into_iter()
            .find(|tool| tool.name == TOOL)
            .expect("send_feedback is registered");
        let description = tool.description.as_deref().unwrap_or_default();

        for required in [
            "goes to this server's team through a third-party form service (Forminit)",
            "Ask the user before including catalog content, personal details or anything the \
             user hasn't agreed to share",
            "send the user's own words",
            "No email, display name, key name, token or catalog content is sent",
            "message, kind, from, tool, rating, the server version, your MCP client's name, \
             the credential kind (api_key or oauth), the feedback's id and your user name as the \
             sender's userId",
        ] {
            assert!(description.contains(required), "missing {required:?}");
        }
        let hints = tool.annotations.expect("annotated");
        assert_eq!(
            (
                hints.read_only_hint,
                hints.destructive_hint,
                hints.idempotent_hint,
                hints.open_world_hint
            ),
            (Some(false), Some(true), Some(false), Some(true))
        );
    }

    #[test]
    fn the_client_name_is_cleaned_and_bounded() {
        assert_eq!(client_name(Some("MCPJam")), "MCPJam");
        assert_eq!(client_name(Some(" a\nb\u{0}c ")), "abc");
        assert_eq!(client_name(Some("")), UNKNOWN_CLIENT);
        assert_eq!(client_name(None), UNKNOWN_CLIENT);
        assert_eq!(client_name(Some(&"x".repeat(500))).chars().count(), 100);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn without_feedback_config_the_tool_is_not_offered_or_mentioned() {
        let off = app_with(&[("pg", &[])], Config::default()).await;
        let on = app_with(&[("pg", &[])], configured()).await;

        let off_server = OkfServer::new(Arc::clone(&off.app));
        let on_server = OkfServer::new(Arc::clone(&on.app));

        assert!(!off_server.tool_router.has_route(TOOL));
        assert!(off_server.tool_router.has_route("search"));
        assert!(on_server.tool_router.has_route(TOOL));
        use rmcp::ServerHandler;
        assert_eq!(
            off_server.get_info().instructions.as_deref(),
            Some(INSTRUCTIONS)
        );
        assert_eq!(
            on_server.get_info().instructions,
            Some(format!("{INSTRUCTIONS} {}", feedback_instructions("okf")))
        );
        // The function refuses too, and stores nothing.
        let refused = send(&context(&off, reader(false), None), None, &params("hi")).await;
        assert!(
            matches!(refused, Err(OkfError::Refused { .. })),
            "{refused:?}"
        );
        assert!(rows(&off).await.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_session_names_the_configured_product_in_the_tool_and_instructions() {
        let branded = Config {
            auth: AuthConfig {
                product_name: "Acme Notes".to_string(),
                ..AuthConfig::default()
            },
            ..configured()
        };
        let test = app_with(&[("pg", &[])], branded).await;

        let server = OkfServer::new(Arc::clone(&test.app));

        use rmcp::ServerHandler;
        let instructions = server.get_info().instructions.unwrap_or_default();
        assert!(
            instructions.ends_with(&feedback_instructions("Acme Notes")),
            "{instructions}"
        );
        assert!(
            instructions.contains("To tell Acme Notes's team"),
            "{instructions}"
        );
        let tool = server.tool_router.get(TOOL).expect("offered");
        let shown = tool.description.as_deref().unwrap_or_default();
        assert!(shown.starts_with(&description("Acme Notes")), "{shown}");
        assert!(
            shown.contains("goes to Acme Notes's team through a third-party form service"),
            "{shown}"
        );
        assert!(!shown.contains("this server"), "{shown}");
        assert!(
            shown.ends_with(&format!("{TOOL_REFERENCE_URL}#{TOOL}")),
            "{shown}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_read_key_and_an_oauth_grant_can_both_send_without_a_catalog() {
        let test = app_with(&[], configured()).await;
        seed_alice(&test).await;
        let mut full = params("  the search results were confusing  ");
        full.kind = Some("confusing".to_string());
        full.from = Some("user".to_string());
        full.tool = Some("search".to_string());
        full.rating = Some(2);
        let mut unassigned = (*principal(&[], None)).clone();
        unassigned.user = None;

        let by_key = send(&context(&test, reader(false), None), Some("MCPJam"), &full)
            .await
            .unwrap();
        let by_grant = send(&context(&test, reader(true), None), None, &params("works"))
            .await
            .unwrap();
        let by_unassigned = send(
            &context(&test, Arc::new(unassigned), None),
            None,
            &params("hello"),
        )
        .await;

        assert!(by_key.sent && by_grant.sent);
        assert_ne!(by_key.id, by_grant.id);
        assert!(by_unassigned.is_ok());
        let stored: (String, Option<String>, Option<i16>, String) =
            sqlx::query_as("SELECT message, tool, rating, client FROM feedback WHERE id = $1")
                .bind(&by_key.id)
                .fetch_one(test.app.accounts().executor())
                .await
                .unwrap();
        assert_eq!(
            stored,
            (
                "the search results were confusing".to_string(),
                Some("search".to_string()),
                Some(2),
                "MCPJam".to_string()
            )
        );
        let all = rows(&test).await;
        let summary: Vec<(&str, &str, &str, &str)> = all
            .iter()
            .map(|(_, user, kind, origin, credential)| {
                (
                    user.as_str(),
                    kind.as_str(),
                    origin.as_str(),
                    credential.as_str(),
                )
            })
            .collect();
        assert!(summary.contains(&("alice", "confusing", "user", "api_key")));
        assert!(summary.contains(&("alice", "other", "agent", "oauth")));
        assert!(summary.contains(&(UNASSIGNED_USER, "other", "agent", "api_key")));
        assert!(all.iter().all(|row| row.0 == "tester"), "{all:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bad_arguments_and_long_messages_are_refused_with_clear_errors() {
        let test = app_with(&[], configured()).await;
        seed_alice(&test).await;
        let ctx = context(&test, reader(false), None);
        let with = |change: fn(&mut SendFeedbackParams)| {
            let mut made = params("fine");
            change(&mut made);
            made
        };

        let longest = send(&ctx, None, &params(&"x".repeat(MAX_MESSAGE_CHARS))).await;
        let too_long = send(&ctx, None, &params(&"é".repeat(MAX_MESSAGE_CHARS + 1))).await;
        let refusals = [
            (params("   "), "message is empty"),
            (params("a\u{0}b"), "NUL"),
            (
                with(|p| p.kind = Some("rant".to_string())),
                "kind must be one of",
            ),
            (
                with(|p| p.from = Some("robot".to_string())),
                "from must be user or agent",
            ),
            (with(|p| p.tool = Some("a/b".to_string())), "tool must be"),
            (with(|p| p.tool = Some(String::new())), "tool must be"),
            (with(|p| p.rating = Some(0)), "rating must be 1 to 5"),
            (with(|p| p.rating = Some(6)), "rating must be 1 to 5"),
        ];

        assert!(longest.is_ok(), "{longest:?}");
        match too_long {
            Err(OkfError::LimitExceeded {
                limit: "feedback_message",
                reason,
            }) => assert!(
                reason.contains("4001 characters; the most is 4000"),
                "{reason}"
            ),
            other => panic!("not refused: {other:?}"),
        }
        for (arguments, expected) in refusals {
            let refused = send(&ctx, None, &arguments).await;
            match refused {
                Err(OkfError::Refused { reason }) => {
                    assert!(reason.contains(expected), "{reason}");
                }
                other => panic!("{expected}: {other:?}"),
            }
        }
        assert_eq!(rows(&test).await.len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_hourly_limit_refuses_with_a_clear_error() {
        let mut config = configured();
        config.feedback.as_mut().unwrap().max_per_hour = 2;
        let test = app_with(&[], config).await;
        seed_alice(&test).await;
        let ctx = context(&test, reader(false), None);

        for _ in 0..2 {
            send(&ctx, None, &params("ok")).await.unwrap();
        }
        let refused = send(&ctx, None, &params("one more")).await;

        let error = refused.unwrap_err();
        assert_eq!(error.kind(), "LimitExceeded");
        assert_eq!(error.to_json()["limit"], "feedback_rate");
        assert!(error.to_string().contains("try again later"), "{error}");
    }

    /// Serves `test`'s app on loopback with a `read` key of the fixture
    /// user, whose email is set.
    async fn serve(
        test: &TestApp,
    ) -> (
        String,
        String,
        CancellationToken,
        tokio::task::JoinHandle<std::io::Result<()>>,
    ) {
        let audit = crate::store::audit::AuditActor::cli();
        crate::store::users::set_email(
            test.app.accounts().executor(),
            crate::db::TEST_USER,
            Some(EMAIL),
            &audit,
            chrono::Utc::now(),
        )
        .await
        .expect("set the user's email");
        let key = keys::create(
            test.app.accounts().executor(),
            &NewKey {
                name: KEY_NAME,
                access: Access::Read,
                maintenance_report: false,
                grants: Grants::Catalogs(&["pg".into()]),
                default_catalog: None,
                actor: KEY_NAME,
                user: crate::db::TEST_USER,
                self_issued: false,
                expires_at: None,
                levels: &[],
            },
            &audit,
            chrono::Utc::now(),
        )
        .await
        .expect("create a key");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let endpoint = format!("http://{}/mcp", listener.local_addr().expect("address"));
        let shutdown = CancellationToken::new();
        let server = tokio::spawn(crate::http::serve(
            Arc::clone(&test.app),
            listener,
            shutdown.clone(),
        ));
        (endpoint, key, shutdown, server)
    }

    /// The fixture user's email: must never be forwarded or logged.
    const EMAIL: &str = "zq-sentinel-674@example.com";

    /// The key's name: must never be forwarded.
    const KEY_NAME: &str = "zq-keyname-674";

    async fn listed_tools(probe: &McpProbe) -> Vec<String> {
        let answer = probe.request("tools/list", json!({})).await;
        answer["result"]["tools"]
            .as_array()
            .unwrap_or_else(|| panic!("{answer}"))
            .iter()
            .filter_map(|tool| tool["name"].as_str().map(str::to_string))
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn without_config_tools_list_leaves_it_out_and_a_call_sends_nothing() {
        let (origin, mock) = mock_forminit(vec![]).await;
        let test = app_with(&[("pg", &[])], Config::default()).await;
        let (endpoint, key, shutdown, server) = serve(&test).await;
        let probe = McpProbe::open(&endpoint, &key).await;

        let listed = listed_tools(&probe).await;
        let called = probe.call(TOOL, json!({"message": "hello"})).await;
        probe.close().await;
        shutdown.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(30), server)
            .await
            .expect("the server stops");

        assert!(listed.iter().any(|name| name == "search"), "{listed:?}");
        assert!(!listed.iter().any(|name| name == TOOL), "{listed:?}");
        assert!(
            called.get("error").is_some() || called["result"]["isError"] == true,
            "{called}"
        );
        assert!(rows(&test).await.is_empty());
        assert!(mock.seen().is_empty(), "{origin}");
    }

    /// The whole path through a real session: `tools/list` offers the tool,
    /// the call is stored, the worker posts exactly the documented body to
    /// the form (with the key), and the row ends `sent` with the hashId. A
    /// log capture over the call and the delivery holds neither the API key
    /// nor the user's email. `#[tokio::test]` is current-thread, so the
    /// server and the worker run on this thread and log to the observer.
    #[tokio::test]
    async fn a_call_is_forwarded_with_the_documented_body_and_no_secret_is_logged() {
        use crate::telemetry::config::LogFormat;
        use crate::telemetry::subscriber::capture::Observer;

        let key_env = "OKF_TEST_FEEDBACK_API_KEY_674";
        // Edition 2021: `set_var` is a safe call; the name is this test's own.
        std::env::set_var(key_env, API_KEY);
        let (origin, mock) = mock_forminit(vec![]).await;
        let config = Config {
            feedback: Some(config_on(&origin, Some(key_env))),
            ..Config::default()
        };
        let test = app_with(&[("pg", &[])], config).await;
        let (endpoint, key, shutdown, server) = serve(&test).await;

        let observer = Observer::start("trace", "trace", "trace", LogFormat::Text);
        let probe = McpProbe::open(&endpoint, &key).await;
        let listed = listed_tools(&probe).await;
        let answer = probe
            .call(
                TOOL,
                json!({"message": "publish refused my link", "kind": "bug", "tool": "publish",
                       "rating": 4}),
            )
            .await;
        let seen = mock.wait_for(1).await;
        let id = answer["result"]["structuredContent"]["id"]
            .as_str()
            .unwrap_or_else(|| panic!("{answer}"))
            .to_string();
        let mut state = (String::new(), None::<String>);
        for _ in 0..100 {
            state = sqlx::query_as("SELECT status, submission_id FROM feedback WHERE id = $1")
                .bind(&id)
                .fetch_one(test.app.accounts().executor())
                .await
                .unwrap();
            if state.0 == "sent" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        probe.close().await;
        let observed = observer.finish();
        shutdown.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(30), server)
            .await
            .expect("the server stops");

        assert!(listed.iter().any(|name| name == TOOL), "{listed:?}");
        assert_eq!(
            answer["result"]["structuredContent"]["sent"], true,
            "{answer}"
        );
        assert_eq!(state, ("sent".to_string(), Some("hash0001".to_string())));
        assert_eq!(seen.len(), 1);
        let (form, headers, body) = &seen[0];
        assert_eq!(form, FORM);
        assert_eq!(headers["x-api-key"], API_KEY);
        assert_eq!(
            body,
            &json!({"blocks": [
                {"type": "text", "name": "message", "value": "publish refused my link"},
                {"type": "text", "name": "kind", "value": "bug"},
                {"type": "text", "name": "from", "value": "agent"},
                {"type": "text", "name": "tool", "value": "publish"},
                {"type": "text", "name": "server_version", "value": env!("CARGO_PKG_VERSION")},
                {"type": "text", "name": "client", "value": "probe"},
                {"type": "text", "name": "credential_kind", "value": "api_key"},
                {"type": "text", "name": "feedback_id", "value": id},
                {"type": "rating", "name": "rating", "value": 4},
                {"type": "sender", "properties": {"userId": crate::db::TEST_USER}},
            ]})
        );
        let sent = body.to_string();
        for private in [EMAIL, KEY_NAME, &key, API_KEY] {
            assert!(!sent.contains(private), "{private} was forwarded");
        }
        let logged = format!("{}|{}", observed.stderr, observed.exported_text());
        assert!(
            logged.contains("feedback forwarded"),
            "the capture saw the delivery"
        );
        for secret in [API_KEY, EMAIL, key.as_str()] {
            assert!(!logged.contains(secret), "{secret} reached a log");
        }
    }
}
