//! `POST /api/v1/feedback` (issue #793): the console's "Send feedback"
//! form. It takes the same submissions as the `send_feedback` MCP tool and
//! goes through the same checks, rate limit and queue
//! ([`feedback::check_and_submit`]), so both entry points refuse and limit
//! alike; delivery is the feedback worker's ([`crate::mail::feedback`]).
//!
//! Access:
//! - offered only with a `[feedback]` table: without one the route answers
//!   404, like an unknown path, and `GET /ui/brand.json` has no
//!   `feedback_form`, so the console draws no form;
//! - a console session only (the cookie, with `X-OKF-CSRF`, which
//!   `require_principal` checks on every cookie write); a bearer key or
//!   OAuth token is a 403, since those send feedback with the MCP tool;
//! - the rate limit (`[feedback] max_per_hour`) counts the signed-in user's
//!   console submissions, whichever session sent them
//!   ([`feedback::console_credential`]).
//!
//! `from` is always `user` (a person typed it), and `tool` is never set.
//! The Forminit API key stays on the server: only the worker reads it.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};

use crate::app::App;
use crate::auth::{CredentialKind, Principal};
use crate::error::OkfError;
use crate::http::AppError;
use crate::mail::feedback::{
    self, Caller, Draft, FeedbackKind, FeedbackOrigin, Sender, MAX_MESSAGE_CHARS,
};

/// The `client` a console submission is forwarded with.
pub(crate) const CONSOLE_CLIENT: &str = "web console";

/// The routes, for [`super::routes`].
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "router builder: runs once at startup, outside any request, so a span would be an orphan root"
    )
)]
pub(crate) fn routes() -> Router<Arc<App>> {
    Router::new().route("/feedback", post(send))
}

/// `POST /feedback`'s body. A `from` or `tool` field is refused (422), as
/// is any other unknown field.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FeedbackBody {
    /// The feedback, in the person's own words.
    message: String,
    /// `bug`, `confusing`, `missing`, `praise` or `other` (default `other`).
    #[serde(default)]
    kind: Option<String>,
    /// A rating from 1 to 5, if any.
    #[serde(default)]
    rating: Option<i64>,
}

/// `POST /feedback`'s answer: `201 {"sent": true, "id": ...}`.
#[derive(Debug, Serialize)]
struct FeedbackSent {
    /// Always true: the feedback is stored and will be forwarded.
    sent: bool,
    /// The feedback's id.
    id: String,
}

/// What the console's feedback form needs to draw itself, served as
/// `GET /ui/brand.json`'s `feedback_form` (`null` without `[feedback]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct FeedbackForm {
    /// The third-party disclosure the form shows
    /// ([`feedback::console_disclosure`]).
    pub disclosure: String,
    /// The longest message, in characters.
    pub max_message_chars: usize,
    /// The kinds a submission may name, in the order to offer them.
    pub kinds: [&'static str; 5],
}

/// The console's feedback form for this server, if feedback is configured.
///
/// # Arguments
/// * `app` - the shared state (its `[feedback]` config and product name).
///
/// # Returns
/// The form, or `None` without a `[feedback]` table.
#[tracing::instrument(level = "trace", skip_all)]
pub(crate) fn form(app: &App) -> Option<FeedbackForm> {
    app.config().feedback.as_ref()?;
    Some(FeedbackForm {
        disclosure: feedback::console_disclosure(&app.settings().product_name),
        max_message_chars: MAX_MESSAGE_CHARS,
        kinds: FeedbackKind::NAMES,
    })
}

/// `POST /feedback`: checks the submission and queues it for the operator's
/// form.
///
/// # Arguments
/// * `app` - the shared state.
/// * `principal` - the caller, from `require_principal`.
/// * `body` - the submission.
///
/// # Returns
/// `201 {"sent": true, "id"}`, or 404 without `[feedback]`.
///
/// # Errors
/// 403 for a bearer key or OAuth token; otherwise as
/// [`feedback::check_and_submit`]: 409 (`Refused`) for a blank message, an
/// unknown kind, a rating outside 1-5 or a NUL; 422 (`LimitExceeded`) for a
/// message over 4,000 characters (`feedback_message`) or past the hourly
/// limit (`feedback_rate`).
#[tracing::instrument(level = "debug", skip_all)]
async fn send(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    Json(body): Json<FeedbackBody>,
) -> Result<Response, AppError> {
    if app.config().feedback.is_none() {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    // Both checked at once; every combination is listed, so a new
    // credential kind fails to compile here until someone decides.
    let (user_id, user_slug) = match (&principal.kind, principal.user.as_deref()) {
        (CredentialKind::Session { user_id }, Some(name)) => (user_id.as_str(), name),
        (
            CredentialKind::ApiKey | CredentialKind::OAuth { .. } | CredentialKind::Session { .. },
            _,
        ) => {
            return Err(OkfError::Forbidden {
                reason: "the console's feedback form needs a console session; \
                         a key or an OAuth token sends feedback with the send_feedback tool"
                    .to_string(),
            }
            .into())
        }
    };
    let credential = feedback::console_credential(user_id);
    let sender = Sender {
        credential: &credential,
        user_slug,
        registered: true,
        client: CONSOLE_CLIENT,
        credential_kind: "session",
        caller: Caller::User,
    };
    let draft = Draft {
        message: &body.message,
        kind: body.kind.as_deref(),
        // A person typed it: never `agent`.
        from: None,
        default_origin: FeedbackOrigin::User,
        tool: None,
        rating: body.rating,
    };
    let id = feedback::check_and_submit(&app, &sender, &draft).await?;
    Ok((StatusCode::CREATED, Json(FeedbackSent { sent: true, id })).into_response())
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::*;
    use crate::config::{Config, FeedbackConfig};
    use crate::http::api::testing::{
        keys, login_config, send as send_with_key, user_with_password, Browser, Payload, Reply,
        MAIN,
    };
    use crate::mail::feedback::tests::FORM;
    use crate::mcp::testing::{app_with, TestApp};

    /// A login app with a `[feedback]` table allowing `max_per_hour`
    /// submissions (nothing runs a worker here, so the form is never
    /// reached).
    async fn feedback_app(max_per_hour: u32) -> TestApp {
        let config = Config {
            feedback: Some(FeedbackConfig {
                max_per_hour,
                ..FeedbackConfig::for_form(FORM)
            }),
            ..login_config()
        };
        // `other` too: `keys` makes a key on it.
        app_with(
            &[("main", MAIN), ("other", &[("index.md", "# Other\n")])],
            config,
        )
        .await
    }

    /// A browser signed in as a new user `name`.
    async fn person(test: &TestApp, name: &str) -> Browser {
        user_with_password(test, name, &format!("{name}@example.com"), None).await;
        Browser::session_for(test, name).await
    }

    async fn post(browser: &mut Browser, test: &TestApp, body: Value) -> Reply {
        browser
            .send(test, "POST", "/api/v1/feedback", Payload::Json(body))
            .await
    }

    /// Every stored row's sender fields, oldest first.
    async fn rows(test: &TestApp) -> Vec<(String, String, String, String, String, String)> {
        sqlx::query_as(
            "SELECT credential, user_slug, origin, credential_kind, client, kind FROM feedback \
             ORDER BY created_at, id",
        )
        .fetch_all(test.app.accounts().executor())
        .await
        .expect("read the feedback rows")
    }

    /// The `users.id` of `name`.
    async fn user_id(test: &TestApp, name: &str) -> String {
        sqlx::query_scalar("SELECT id FROM users WHERE name = $1")
            .bind(name)
            .fetch_one(test.app.accounts().executor())
            .await
            .expect("the user exists")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_signed_in_person_sends_feedback_as_the_user_through_the_queue() {
        let test = feedback_app(10).await;
        let mut alice = person(&test, "alice").await;

        let sent = post(
            &mut alice,
            &test,
            json!({"message": "  The search page is great  ", "kind": "praise", "rating": 5}),
        )
        .await;

        assert_eq!(sent.status, StatusCode::CREATED, "{:?}", sent.json());
        assert_eq!(sent.json()["sent"], true);
        let id = sent.json()["id"].as_str().expect("an id").to_string();
        assert_eq!(id.len(), 32);
        let alice_id = user_id(&test, "alice").await;
        assert_eq!(
            rows(&test).await,
            [(
                format!("user:{alice_id}"),
                "alice".to_string(),
                "user".to_string(),
                "session".to_string(),
                CONSOLE_CLIENT.to_string(),
                "praise".to_string(),
            )]
        );
        let (message, rating, status): (String, Option<i16>, String) =
            sqlx::query_as("SELECT message, rating, status FROM feedback WHERE id = $1")
                .bind(&id)
                .fetch_one(test.app.accounts().executor())
                .await
                .expect("the row");
        assert_eq!(
            (message.as_str(), rating, status.as_str()),
            ("The search page is great", Some(5), "pending")
        );
        let queued: i64 =
            sqlx::query_scalar("SELECT count(*) FROM apalis.jobs WHERE job_type = 'send_feedback'")
                .fetch_one(test.app.accounts().executor())
                .await
                .expect("count the jobs");
        assert_eq!(queued, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_key_or_no_credential_cannot_use_the_console_form() {
        let test = feedback_app(10).await;
        let keys = keys(&test).await;

        let (keyed, body) = send_with_key(
            &test,
            "POST",
            "/api/v1/feedback",
            Some(&keys.admin),
            Some(json!({"message": "hello"})),
        )
        .await;
        let (anonymous, _) = send_with_key(
            &test,
            "POST",
            "/api/v1/feedback",
            None,
            Some(json!({"message": "hello"})),
        )
        .await;

        assert_eq!(keyed, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(body["error"], "Forbidden");
        assert_eq!(anonymous, StatusCode::UNAUTHORIZED);
        assert!(rows(&test).await.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_session_write_without_its_csrf_token_is_refused() {
        let test = feedback_app(10).await;
        let mut alice = person(&test, "alice").await;
        let token = alice.csrf.take();

        let missing = post(&mut alice, &test, json!({"message": "hello"})).await;
        alice.csrf = Some("not-the-token".to_string());
        let wrong = post(&mut alice, &test, json!({"message": "hello"})).await;
        alice.csrf = token;
        let right = post(&mut alice, &test, json!({"message": "hello"})).await;

        assert_eq!(missing.status, StatusCode::FORBIDDEN);
        assert_eq!(wrong.status, StatusCode::FORBIDDEN);
        assert_eq!(right.status, StatusCode::CREATED);
        assert_eq!(rows(&test).await.len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn without_feedback_configured_the_route_is_not_there() {
        let test = app_with(&[("main", MAIN)], login_config()).await;
        let mut alice = person(&test, "alice").await;

        let answer = post(&mut alice, &test, json!({"message": "hello"})).await;

        assert_eq!(answer.status, StatusCode::NOT_FOUND);
        assert!(rows(&test).await.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn submissions_are_checked_as_the_tool_checks_them() {
        let test = feedback_app(100).await;
        let mut alice = person(&test, "alice").await;
        let longest = "x".repeat(MAX_MESSAGE_CHARS);
        let too_long = "\u{e9}".repeat(MAX_MESSAGE_CHARS + 1);

        let mut answers = Vec::new();
        for body in [
            json!({"message": "   "}),
            json!({"message": "a\u{0}b"}),
            json!({"message": too_long}),
            json!({"message": "hi", "kind": "rant"}),
            json!({"message": "hi", "rating": 0}),
            json!({"message": "hi", "rating": 6}),
            // `from` and `tool` are the tool's alone: a person is the user.
            json!({"message": "hi", "from": "agent"}),
            json!({"message": "hi", "tool": "publish"}),
            json!({"kind": "bug"}),
        ] {
            let reply = post(&mut alice, &test, body.clone()).await;
            answers.push((
                reply.status,
                reply.json()["error"].clone(),
                reply.json()["limit"].clone(),
            ));
        }
        let fine = post(&mut alice, &test, json!({"message": longest})).await;

        let refused = || (StatusCode::CONFLICT, json!("Refused"), Value::Null);
        assert_eq!(
            answers[..6],
            [
                refused(),
                refused(),
                (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    json!("LimitExceeded"),
                    json!("feedback_message")
                ),
                refused(),
                refused(),
                refused(),
            ]
        );
        for (status, _, _) in &answers[6..] {
            assert_eq!(*status, StatusCode::UNPROCESSABLE_ENTITY);
        }
        assert_eq!(fine.status, StatusCode::CREATED);
        let stored = rows(&test).await;
        assert_eq!(stored.len(), 1, "only the valid one is stored");
        assert_eq!(stored[0].2, "user");
        assert_eq!(stored[0].5, "other");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_hourly_limit_counts_each_user_across_their_sessions() {
        let test = feedback_app(2).await;
        let mut first = person(&test, "alice").await;
        let mut second = Browser::session_for(&test, "alice").await;
        let mut bob = person(&test, "bob").await;

        let one = post(&mut first, &test, json!({"message": "one"})).await;
        let two = post(&mut second, &test, json!({"message": "two"})).await;
        let three = post(&mut first, &test, json!({"message": "three"})).await;
        let other_session = post(&mut second, &test, json!({"message": "four"})).await;
        let other_user = post(&mut bob, &test, json!({"message": "bob's"})).await;

        assert_eq!(one.status, StatusCode::CREATED);
        assert_eq!(two.status, StatusCode::CREATED);
        for limited in [&three, &other_session] {
            assert_eq!(limited.status, StatusCode::UNPROCESSABLE_ENTITY);
            assert_eq!(limited.json()["limit"], "feedback_rate");
            let message = limited.json()["message"].to_string();
            assert!(message.contains("you have sent 2 feedback"), "{message}");
            assert!(!message.contains("this credential"), "{message}");
        }
        assert_eq!(other_user.status, StatusCode::CREATED);
        assert_eq!(rows(&test).await.len(), 3);
    }
}
