//! Feedback delivery (issue #674): the `send_feedback` tool's submissions,
//! and the console's (`POST /api/v1/feedback`, issue #793), both checked by
//! `check_and_submit`, stored in the `feedback` table and forwarded to the
//! operator's Forminit form by a worker on the same apalis-postgres queue machinery as mail
//! ([`super::queue`]), in a queue of its own ([`QUEUE`]).
//!
//! The life of a submission:
//! 1. [`submit`] checks the credential's rate limit against the table (so it
//!    holds across replicas), inserts a `pending` row and, in the same
//!    transaction, one `apalis.jobs` row whose payload is only the
//!    feedback's id. The tool answers at once.
//! 2. A worker ([`spawn_worker`], on the replica with `[feedback] worker`)
//!    fetches the job, counts the try on the row and posts the row's
//!    fields to `https://forminit.com/f/<form id>` ([`Forminit::post`]).
//! 3. A 2xx: the row is `sent`, with Forminit's `submission.hashId`. A 429,
//!    a 5xx or a network error: the job is rescheduled with the mail
//!    queue's backoff (30 s doubling, capped at an hour) until
//!    [`MAX_TRIES`] tries, then the row is `failed`. Any other 4xx
//!    (`FORM_NOT_FOUND`, `FORM_DISABLED`, a validation error): `failed` at
//!    once, with Forminit's error code, never retried.
//! 4. [`prune`] (a housekeeping job) deletes `sent` and `failed` rows past
//!    `[feedback] retention`, and the finished jobs that named them. An
//!    operator sees failed rows with [`list_failed`] and queues one again
//!    with [`retry`] (`okf-mcp feedback list` and `retry`).
//! 5. Deleting the user (`store::users::delete`) deletes its `pending` and
//!    `failed` rows. A try that claims its row after the deletion commits
//!    finds none and ends as skipped; a post claimed before it is not
//!    cancelled and can still reach Forminit, but its outcome updates no
//!    row, so it is never posted again.
//!
//! Delivery is at-least-once. Fetching a job is exclusive in normal
//! operation (apalis takes it `FOR UPDATE SKIP LOCKED`), but a try whose
//! acceptance was unconfirmed (a timeout after Forminit took the post) or
//! unrecorded (marking the row `sent` failed) is retried, possibly on
//! another replica, and can post a duplicate. Every submission carries the
//! row's id as its `feedback_id` block, so the operator can de-duplicate in
//! Forminit.
//!
//! What is forwarded is exactly `submission`'s blocks: the message, kind,
//! origin (`from`), tool, server version, MCP client name, credential kind,
//! the feedback's own id, rating and the user's slug as the sender's
//! `userId`. Never an email, display name, key name, token or catalog
//! content.

use std::sync::Arc;
use std::time::Duration;

use apalis::prelude::{
    AbortError, BackendExt, BoxDynError, Data, TaskBuilder, TaskId, WorkerBuilder,
};
use apalis_postgres::queries::push_tasks;
use apalis_postgres::{Config as QueueConfig, PostgresStorage};
use chrono::{DateTime, TimeDelta, Utc};
use reqwest::header::{HeaderValue, ACCEPT};
use reqwest::StatusCode;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::PgPool;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::queue::{process_worker_name, reschedule_job, worker_pool_named, WorkerTiming};
use crate::app::App;
use crate::clock::Clock;
use crate::config::FeedbackConfig;
use crate::error::OkfError;
use crate::telemetry::subscriber::split_event;

/// The apalis queue (`apalis.jobs.job_type`).
pub const QUEUE: &str = "send_feedback";

/// The longest `message` accepted, in characters (Unicode code points).
pub const MAX_MESSAGE_CHARS: usize = 4000;

/// The longest `tool` name accepted, in bytes.
pub const MAX_TOOL_BYTES: usize = 64;

/// The longest MCP client name forwarded, in characters; a longer one is
/// cut.
pub const MAX_CLIENT_CHARS: usize = 100;

/// How many times a submission is posted before it is given up on
/// (`failed`) after retryable errors. With the backoff (30 s doubling,
/// capped at an hour), the last try is about three hours after the first.
pub const MAX_TRIES: i32 = 10;

/// apalis's own `max_attempts`: more than [`MAX_TRIES`], so the row's count,
/// not apalis's, ends the retries.
const APALIS_MAX_ATTEMPTS: usize = 64;

/// The only origin feedback is posted to outside tests.
pub const FORMINIT_ORIGIN: &str = "https://forminit.com";

/// How long one post may take, body included.
const FORMINIT_TIMEOUT: Duration = Duration::from_secs(30);

/// The header carrying the form's API key.
const API_KEY_HEADER: &str = "X-API-KEY";

/// The connections the feedback worker's own pool may open: one held by
/// LISTEN, one for the fetch, ack and heartbeat cycle, one for the handler.
pub const WORKER_POOL_SIZE: u32 = 3;

/// The `application_name` of the feedback worker's connections.
pub const WORKER_APPLICATION_NAME: &str = "okf-feedback-worker";

/// The first key of the per-credential rate-limit lock (`okfb` in ASCII),
/// apart from the catalog locks' namespace.
const RATE_LOCK_NAMESPACE: i32 = 0x6f6b_6662;

/// The rate limit's window: the last hour.
const RATE_WINDOW_HOURS: i64 = 1;

/// What the feedback is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackKind {
    /// Something is broken.
    Bug,
    /// Something is hard to understand.
    Confusing,
    /// Something is missing.
    Missing,
    /// Something works well.
    Praise,
    /// Anything else (the default).
    Other,
}

impl FeedbackKind {
    /// Every kind's name, in the order the tool lists them.
    pub const NAMES: [&'static str; 5] = ["bug", "confusing", "missing", "praise", "other"];

    /// Parses a kind's name.
    ///
    /// # Arguments
    /// * `name` - `bug`, `confusing`, `missing`, `praise` or `other`.
    ///
    /// # Returns
    /// The kind, or `None` for any other text.
    #[tracing::instrument(level = "trace", skip_all)]
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "bug" => Some(Self::Bug),
            "confusing" => Some(Self::Confusing),
            "missing" => Some(Self::Missing),
            "praise" => Some(Self::Praise),
            "other" => Some(Self::Other),
            _ => None,
        }
    }

    /// The kind's name, as stored and forwarded.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bug => "bug",
            Self::Confusing => "confusing",
            Self::Missing => "missing",
            Self::Praise => "praise",
            Self::Other => "other",
        }
    }
}

/// Who the feedback is from (`from` in the tool).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackOrigin {
    /// The person asked for it to be sent.
    User,
    /// The agent reports its own experience (the default).
    Agent,
}

impl FeedbackOrigin {
    /// Parses `user` or `agent`.
    ///
    /// # Arguments
    /// * `name` - the text.
    ///
    /// # Returns
    /// The origin, or `None` for any other text.
    #[tracing::instrument(level = "trace", skip_all)]
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "user" => Some(Self::User),
            "agent" => Some(Self::Agent),
            _ => None,
        }
    }

    /// The origin's name, as stored and forwarded.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Agent => "agent",
        }
    }
}

/// One checked submission, ready to store. `'a`: it borrows the caller's
/// strings rather than copying them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewFeedback<'a> {
    /// The calling key's id (`api_keys.id`), for the rate limit; never
    /// forwarded.
    pub credential: &'a str,
    /// The credential's user slug, forwarded as the sender's `userId`.
    pub user_slug: &'a str,
    /// Whether `user_slug` names a user. [`submit`] then locks that user's
    /// row `FOR KEY SHARE` first, so it cannot interleave with
    /// `store::users::delete` (which holds the row `FOR UPDATE` while it
    /// deletes the user's undelivered feedback), and refuses when the user
    /// is gone. `false` for a key with no user (the placeholder slug).
    pub registered: bool,
    /// The feedback text (1 to [`MAX_MESSAGE_CHARS`] characters, no NUL).
    pub message: &'a str,
    /// What it is about.
    pub kind: FeedbackKind,
    /// Who it is from.
    pub origin: FeedbackOrigin,
    /// The tool it is about, if any.
    pub tool: Option<&'a str>,
    /// A 1-5 rating, if given.
    pub rating: Option<i16>,
    /// The MCP client's name from `initialize`.
    pub client: &'a str,
    /// `api_key`, `oauth` or `session` (the console).
    pub credential_kind: &'static str,
    /// Whom the rate limit's refusal addresses.
    pub caller: Caller,
}

/// Who the rate limit counts, so its refusal can name them: an MCP
/// credential, or the signed-in person behind the console.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caller {
    /// An API key or OAuth token: "this credential has sent ...".
    Credential,
    /// A console user, counted across their sessions: "you have sent ...".
    User,
}

impl Caller {
    /// The subject of the rate-limit refusal's first clause.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: maps a variant to a string, no I/O"
        )
    )]
    fn subject(self) -> &'static str {
        match self {
            Self::Credential => "this credential has",
            Self::User => "you have",
        }
    }
}

/// Queues one [`QUEUE`] job naming feedback `id`, in the caller's
/// transaction (so the row and its job commit together).
///
/// # Errors
/// [`OkfError::Io`] if the queue refuses the job.
#[tracing::instrument(level = "debug", skip_all)]
async fn queue_job(tx: &mut sqlx::PgConnection, id: &str) -> Result<(), OkfError> {
    let job = FeedbackJob {
        feedback_id: id.to_string(),
    };
    // apalis-postgres stores the payload as JSON bytes, as for mail.
    let payload = serde_json::to_vec(&job).map_err(queue_error)?;
    let task = TaskBuilder::new(payload)
        .max_attempts(APALIS_MAX_ATTEMPTS)
        .build();
    push_tasks(tx, QUEUE, vec![task])
        .await
        .map_err(queue_error)?;
    Ok(())
}

/// Stores a submission and queues its delivery, refusing it when the
/// credential has already sent `max_per_hour` in the last hour. The count is
/// read from the table under a per-credential advisory lock, so it holds
/// across replicas and two concurrent calls cannot both take the last slot.
///
/// # Arguments
/// * `pool` - the database.
/// * `feedback` - the checked submission.
/// * `max_per_hour` - `[feedback] max_per_hour`.
/// * `now` - the server clock (the row's `created_at`).
///
/// # Returns
/// The feedback's id (32 hexadecimal characters).
///
/// # Errors
/// [`OkfError::LimitExceeded`] (`limit: "feedback_rate"`) past the rate
/// limit; [`OkfError::Unauthenticated`] when `registered` and the user is
/// gone; [`OkfError::Io`] if the random source or the queue fails;
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "feedback.submit"))]
pub async fn submit(
    pool: &PgPool,
    feedback: &NewFeedback<'_>,
    max_per_hour: u32,
    now: DateTime<Utc>,
) -> Result<String, OkfError> {
    let id = crate::maintenance::types::new_id()?;
    let mut tx = pool.begin().await?;
    // The user row first, as every writer of a user's children does (lock
    // order: the user, then the rest). `store::users::delete` takes it `FOR
    // UPDATE` and deletes the user's pending feedback before it commits, so
    // a submission is either stored before the deletion (and deleted with
    // the user) or finds the user gone; it cannot slip in after.
    if feedback.registered {
        let user: Option<String> =
            sqlx::query_scalar("SELECT id FROM users WHERE name = $1 FOR KEY SHARE")
                .bind(feedback.user_slug)
                .fetch_optional(&mut *tx)
                .await?;
        if user.is_none() {
            return Err(OkfError::Unauthenticated {
                reason: "the user this credential belongs to no longer exists".to_string(),
            });
        }
    }
    // Serializes one credential's submissions until the commit; no row is
    // locked, so this takes no place in the row lock order. `hashtext`
    // folds the id into the lock's `int4` key; a collision only makes two
    // credentials wait for each other briefly.
    sqlx::query("SELECT pg_advisory_xact_lock($1, hashtext($2))")
        .bind(RATE_LOCK_NAMESPACE)
        .bind(feedback.credential)
        .execute(&mut *tx)
        .await?;
    let recent: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM feedback WHERE credential = $1 AND created_at > $2",
    )
    .bind(feedback.credential)
    .bind(now - TimeDelta::hours(RATE_WINDOW_HOURS))
    .fetch_one(&mut *tx)
    .await?;
    if recent >= i64::from(max_per_hour) {
        return Err(OkfError::LimitExceeded {
            limit: "feedback_rate",
            reason: format!(
                "{} sent {max_per_hour} feedback messages in the last hour, \
                 the most allowed; try again later",
                feedback.caller.subject()
            ),
        });
    }
    sqlx::query(
        "INSERT INTO feedback (id, credential, user_slug, message, kind, origin, tool, rating, \
             server_version, client, credential_kind, created_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
    )
    .bind(&id)
    .bind(feedback.credential)
    .bind(feedback.user_slug)
    .bind(feedback.message)
    .bind(feedback.kind.as_str())
    .bind(feedback.origin.as_str())
    .bind(feedback.tool)
    .bind(feedback.rating)
    .bind(env!("CARGO_PKG_VERSION"))
    .bind(feedback.client)
    .bind(feedback.credential_kind)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    queue_job(&mut tx, &id).await?;
    tx.commit().await?;
    tracing::info!(feedback = %id, "feedback queued");
    Ok(id)
}

/// The third-party disclosure every feedback entry point shows: the
/// `send_feedback` tool's description and the console's feedback form
/// (`console_disclosure`). A macro, not a `const`, so the tool's description
/// can still be one `const` built with `concat!` (which takes only
/// literals). "this server" is replaced with the product name where one is
/// shown.
macro_rules! third_party_disclosure {
    () => {
        "The feedback goes to this server's team through a third-party form service (Forminit)."
    };
}
// Lets other modules of this crate name the macro by path
// (`crate::mail::feedback::third_party_disclosure!`).
pub(crate) use third_party_disclosure;

/// The words of [`third_party_disclosure!`] that name the product.
const THIS_SERVER: &str = "this server";

/// What the console's feedback form says before anyone sends: the
/// third-party disclosure, naming the product, and what is sent with the
/// message. Served in `GET /ui/brand.json` (`feedback_form.disclosure`), so
/// the form shows exactly this text.
///
/// # Arguments
/// * `product_name` - the effective `product_name` setting.
///
/// # Returns
/// The disclosure, "The feedback goes to Acme Notes's team through a
/// third-party form service (Forminit). ...".
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial conversion: pure arithmetic or formatting on its arguments, no I/O"
    )
)]
pub(crate) fn console_disclosure(product_name: &str) -> String {
    format!(
        "{} What is sent: your message, the kind and rating you choose, your user name, the \
         server version and the feedback's id. Leave out anything you don't want to share.",
        third_party_disclosure!().replace(THIS_SERVER, product_name)
    )
}

/// Who sends a submission, as the caller's credential says. Built by each
/// entry point (the `send_feedback` tool, `POST /api/v1/feedback`) and
/// passed to [`check_and_submit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Sender<'a> {
    /// What the rate limit counts: an MCP credential's key id, or
    /// `user:<users.id>` for the console ([`console_credential`]).
    pub credential: &'a str,
    /// The user slug forwarded as the sender's `userId`.
    pub user_slug: &'a str,
    /// Whether `user_slug` names a user (not the placeholder for a key
    /// with no user): the submission then takes that user's row lock and
    /// is refused once the user is gone (see [`NewFeedback::registered`]).
    pub registered: bool,
    /// The client name forwarded as `client` (already cleaned).
    pub client: &'a str,
    /// `api_key`, `oauth` or `session`.
    pub credential_kind: &'static str,
    /// Whom the rate limit's refusal addresses.
    pub caller: Caller,
}

/// A submission's fields as the caller gave them, before any check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Draft<'a> {
    /// The feedback text (trimmed here).
    pub message: &'a str,
    /// The kind's name; `None` is `other`.
    pub kind: Option<&'a str>,
    /// `user` or `agent`; `None` is `default_origin`.
    pub from: Option<&'a str>,
    /// Who it is from when `from` is `None`.
    pub default_origin: FeedbackOrigin,
    /// The tool it is about, if any.
    pub tool: Option<&'a str>,
    /// A rating, if any (checked to be 1-5).
    pub rating: Option<i64>,
}

/// The rate limit's `credential` for console submissions from `user_id`:
/// one count per user, whichever session sent them. `user:` keeps it apart
/// from every key id (hexadecimal, no colon).
///
/// # Arguments
/// * `user_id` - the signed-in user's `users.id`.
///
/// # Returns
/// `user:<user_id>`.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial conversion: pure arithmetic or formatting on its arguments, no I/O"
    )
)]
pub(crate) fn console_credential(user_id: &str) -> String {
    format!("user:{user_id}")
}

/// A refused argument.
#[tracing::instrument(level = "trace", skip_all)]
fn refused(reason: String) -> OkfError {
    OkfError::Refused { reason }
}

/// Checks a submission and queues it: the one path both the
/// `send_feedback` tool and the console's `POST /api/v1/feedback` take, so
/// both refuse and limit exactly alike.
///
/// # Arguments
/// * `app` - the shared state (its `[feedback]` config, pool and clock).
/// * `sender` - who sends it.
/// * `draft` - what was sent.
///
/// # Returns
/// The feedback's id.
///
/// # Errors
/// [`OkfError::Refused`] when feedback is not configured, for a blank
/// message, an unknown `kind` or `from`, a malformed `tool`, a `rating`
/// outside 1-5, or a NUL in the text; [`OkfError::LimitExceeded`] for a
/// message over 4,000 characters (`limit: "feedback_message"`) or past the
/// sender's hourly limit (`limit: "feedback_rate"`); as [`submit`].
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn check_and_submit(
    app: &App,
    sender: &Sender<'_>,
    draft: &Draft<'_>,
) -> Result<String, OkfError> {
    let Some(config) = app.config().feedback.as_ref() else {
        return Err(refused(
            "feedback is not configured on this server".to_string(),
        ));
    };
    let message = draft.message.trim();
    if message.is_empty() {
        return Err(refused("message is empty".to_string()));
    }
    // Postgres refuses NUL in text, which would surface as a database error.
    if message.contains('\0') {
        return Err(refused(
            "message must not contain NUL characters".to_string(),
        ));
    }
    let length = message.chars().count();
    if length > MAX_MESSAGE_CHARS {
        return Err(OkfError::LimitExceeded {
            limit: "feedback_message",
            reason: format!(
                "message is {length} characters; the most is {MAX_MESSAGE_CHARS}. \
                 Shorten it or split it into several messages"
            ),
        });
    }
    let kind = match draft.kind {
        None => FeedbackKind::Other,
        Some(name) => FeedbackKind::parse(name).ok_or_else(|| {
            refused(format!(
                "kind must be one of {}",
                FeedbackKind::NAMES.join(", ")
            ))
        })?,
    };
    let origin = match draft.from {
        None => draft.default_origin,
        Some(name) => FeedbackOrigin::parse(name)
            .ok_or_else(|| refused("from must be user or agent".to_string()))?,
    };
    let tool = draft.tool.map(str::trim);
    if tool.is_some_and(|tool| {
        tool.is_empty()
            || tool.len() > MAX_TOOL_BYTES
            || !tool
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    }) {
        return Err(refused(format!(
            "tool must be a tool's name: 1 to {MAX_TOOL_BYTES} letters, digits, _ and -"
        )));
    }
    let rating = match draft.rating {
        None => None,
        // `i16::try_from` cannot fail inside 1..=5; `ok()` keeps it total.
        Some(rating @ 1..=5) => i16::try_from(rating).ok(),
        Some(_) => return Err(refused("rating must be 1 to 5".to_string())),
    };
    let new = NewFeedback {
        credential: sender.credential,
        user_slug: sender.user_slug,
        registered: sender.registered,
        message,
        kind,
        origin,
        tool,
        rating,
        client: sender.client,
        credential_kind: sender.credential_kind,
        caller: sender.caller,
    };
    submit(
        app.accounts().executor(),
        &new,
        config.max_per_hour,
        app.clock().now(),
    )
    .await
}

/// The error for a queue operation apalis refused.
#[tracing::instrument(level = "trace", skip_all)]
fn queue_error(error: impl std::fmt::Display) -> OkfError {
    OkfError::Io {
        path: "<feedback queue>".to_string(),
        source: std::io::Error::other(error.to_string()),
    }
}

/// Deletes `sent` and `failed` feedback finished more than `retention`
/// before `now`, and the finished jobs of [`QUEUE`] as old. A `failed` row
/// stays that long for the operator (`feedback list`, `feedback retry`).
/// Called by the housekeeping job's prune.
///
/// # Arguments
/// * `pool` - the database.
/// * `now` - the server clock.
/// * `retention` - `[feedback] retention`.
///
/// # Returns
/// How many feedback rows were deleted.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "feedback.prune"))]
pub async fn prune(
    pool: &PgPool,
    now: DateTime<Utc>,
    retention: TimeDelta,
) -> Result<u64, OkfError> {
    let before = now - retention;
    let pruned =
        sqlx::query("DELETE FROM feedback WHERE status IN ('sent', 'failed') AND finished_at < $1")
            .bind(before)
            .execute(pool)
            .await?
            .rows_affected();
    // The job holds only the id; once finished, it is bookkeeping.
    sqlx::query(
        "DELETE FROM apalis.jobs WHERE job_type = $1 AND done_at < $2 AND ( \
             status IN ('Done', 'Killed') OR (status = 'Failed' AND attempts >= max_attempts))",
    )
    .bind(QUEUE)
    .bind(before)
    .execute(pool)
    .await?;
    Ok(pruned)
}

/// A `failed` feedback row, as `okf-mcp feedback list` shows it. The
/// message and the credential are left out, as `mail queue` leaves out
/// bodies.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct FailedFeedback {
    /// The feedback's id.
    pub id: String,
    /// The sender's user name (`unassigned` for a key with no user).
    pub user_slug: String,
    /// The kind the sender chose.
    pub kind: String,
    /// Tries made.
    pub attempts: i32,
    /// Forminit's error code, or why the last try failed.
    pub last_error: Option<String>,
    /// When it was given up on.
    pub finished_at: Option<DateTime<Utc>>,
}

/// The `failed` feedback rows, oldest first.
///
/// # Arguments
/// * `pool` - the database.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "feedback.list_failed"))]
pub async fn list_failed(pool: &PgPool) -> Result<Vec<FailedFeedback>, OkfError> {
    Ok(sqlx::query_as::<_, FailedFeedback>(
        "SELECT id, user_slug, kind, attempts, last_error, finished_at FROM feedback \
         WHERE status = 'failed' ORDER BY finished_at, id",
    )
    .fetch_all(pool)
    .await?)
}

/// Puts a `failed` row back to `pending` with no tries and queues a new
/// job for it, in one transaction. The old job stays in the queue as a
/// finished one, which [`prune`] removes; `last_error` is kept until the
/// next outcome replaces it.
///
/// # Arguments
/// * `pool` - the database.
/// * `id` - the feedback's id, from [`list_failed`].
///
/// # Errors
/// [`OkfError::UnknownName`] (`noun: "feedback"`) for an id that names no
/// row; [`OkfError::Refused`] for a row that has not failed;
/// [`OkfError::Io`] if the queue refuses the job; [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "feedback.retry"))]
pub async fn retry(pool: &PgPool, id: &str) -> Result<(), OkfError> {
    let mut tx = pool.begin().await?;
    let requeued = sqlx::query(
        "UPDATE feedback SET status = 'pending', attempts = 0, finished_at = NULL \
         WHERE id = $1 AND status = 'failed'",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if requeued == 0 {
        let status: Option<String> =
            sqlx::query_scalar("SELECT status FROM feedback WHERE id = $1")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        return Err(match status {
            None => OkfError::UnknownName {
                noun: "feedback",
                name: id.to_string(),
            },
            Some(status) => OkfError::Refused {
                reason: format!("feedback {id} is {status}; only failed feedback is retried"),
            },
        });
    }
    queue_job(&mut tx, id).await?;
    tx.commit().await?;
    tracing::info!(feedback = %id, "failed feedback queued again");
    Ok(())
}

/// Why a post did not go through.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeliveryError {
    /// A 429, a 5xx or a network problem: try again later.
    #[error("retryable: {0}")]
    Retryable(String),
    /// Any other refusal, with Forminit's error code: never retried.
    #[error("refused: {0}")]
    Refused(String),
}

/// The Forminit client: one form's endpoint, and its API key if any. Its
/// `Debug` shows whether a key is set, never the key.
pub struct Forminit {
    client: reqwest::Client,
    endpoint: String,
    // `SecretString` zeroes the key on drop and prints `[REDACTED]`.
    api_key: Option<SecretString>,
}

impl std::fmt::Debug for Forminit {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "Debug impl: runs while an event is formatted, and a span there re-enters the subscriber"
        )
    )]
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Forminit")
            .field("endpoint", &self.endpoint)
            .field("api_key", &self.api_key.as_ref().map(|_| "<set>"))
            .finish()
    }
}

/// A `[feedback]` problem found at start-up.
#[tracing::instrument(level = "debug", skip_all)]
fn config_error(message: String) -> OkfError {
    OkfError::Config {
        path: "[feedback]".to_string(),
        message,
    }
}

impl Forminit {
    /// Builds the client for `config`, reading the API key from the
    /// variable `api_key_env` names, as the mail providers read theirs.
    /// `App::load` calls it, so a missing key stops start-up.
    ///
    /// # Arguments
    /// * `config` - `[feedback]` (already validated).
    /// * `env` - looks an environment variable up (tests pass their own).
    ///
    /// # Returns
    /// The client, posting to `https://forminit.com/f/<forminit_form_id>`.
    ///
    /// # Errors
    /// [`OkfError::Config`] when the named variable is unset or blank, holds
    /// characters a header cannot, the form id is not one, or the HTTP
    /// client cannot be built. The message names the variable, never its
    /// value.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn from_config(
        config: &FeedbackConfig,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, OkfError> {
        // Checked again here, so no caller can build a URL from a bad id.
        if !crate::config::is_forminit_form_id(&config.forminit_form_id) {
            return Err(config_error(
                "feedback.forminit_form_id is not a form id".to_string(),
            ));
        }
        let api_key = match config.api_key_env.as_deref() {
            None => None,
            Some(name) => match env(name) {
                Some(value) if !value.trim().is_empty() => {
                    let value = value.trim().to_string();
                    // Checked now, so a bad key stops start-up, not each post.
                    if HeaderValue::from_str(&value).is_err() {
                        return Err(config_error(format!(
                            "feedback.api_key_env names {name}, which holds characters a header cannot"
                        )));
                    }
                    Some(SecretString::from(value))
                }
                _ => {
                    return Err(config_error(format!(
                        "feedback.api_key_env names {name}, which is not set or is empty"
                    )))
                }
            },
        };
        // The origin is fixed outside tests; a test points it at a mock.
        #[cfg(test)]
        let origin = config
            .base_url_for_tests
            .as_deref()
            .unwrap_or(FORMINIT_ORIGIN);
        #[cfg(not(test))]
        let origin = FORMINIT_ORIGIN;
        let client = reqwest::Client::builder()
            // Forminit answers directly; a redirect could go anywhere.
            .redirect(reqwest::redirect::Policy::none())
            .timeout(FORMINIT_TIMEOUT)
            .https_only(origin == FORMINIT_ORIGIN)
            .user_agent(concat!("okf-mcp/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| config_error(format!("building the Forminit client: {error}")))?;
        Ok(Self {
            client,
            endpoint: format!("{origin}/f/{}", config.forminit_form_id),
            api_key,
        })
    }

    /// The URL submissions are posted to.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Posts one submission, once.
    ///
    /// # Arguments
    /// * `body` - the JSON submission (`submission`).
    ///
    /// # Returns
    /// Forminit's `submission.hashId`, or `None` when a 2xx answer carried
    /// none.
    ///
    /// # Errors
    /// [`DeliveryError::Retryable`] for a 429, a 5xx or a network error;
    /// [`DeliveryError::Refused`] with the error code for any other answer.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn post(&self, body: &Value) -> Result<Option<String>, DeliveryError> {
        let mut request = self
            .client
            .post(&self.endpoint)
            .header(ACCEPT, "application/json")
            .json(body);
        if let Some(key) = &self.api_key {
            // Checked at start-up; `set_sensitive` hides it from reqwest's
            // own debug output too.
            let mut value = HeaderValue::from_str(key.expose_secret())
                .map_err(|_| DeliveryError::Refused("INVALID_API_KEY".to_string()))?;
            value.set_sensitive(true);
            request = request.header(API_KEY_HEADER, value);
        }
        let response = request.send().await.map_err(|error| {
            // `without_url`: the error's text, without the endpoint.
            DeliveryError::Retryable(format!("network: {}", error.without_url()))
        })?;
        let status = response.status();
        // An unreadable or non-JSON body is an empty object.
        let reply: Value = response.json().await.unwrap_or_else(|_| json!({}));
        if status.is_success() {
            return Ok(reply
                .pointer("/submission/hashId")
                .and_then(Value::as_str)
                .filter(|hash| is_code_like(hash, |byte| byte.is_ascii_alphanumeric()))
                .map(str::to_string));
        }
        if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            return Err(DeliveryError::Retryable(format!(
                "forminit answered {}",
                status.as_u16()
            )));
        }
        Err(DeliveryError::Refused(error_code(status, &reply)))
    }
}

/// Is `text` 1 to 64 bytes, each passing `allowed`? Forminit's answers are
/// stored and logged only in this shape, so a reply can never put free text
/// (or anything it echoed) into the row or the log.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
    )
)]
fn is_code_like(text: &str, allowed: impl Fn(u8) -> bool) -> bool {
    !text.is_empty() && text.len() <= 64 && text.bytes().all(allowed)
}

/// A refusal's error code: the reply's `error` (or `code`) when it looks
/// like one (`FORM_NOT_FOUND`), else `HTTP_<status>`.
///
/// # Arguments
/// * `status` - the answer's status.
/// * `reply` - its JSON body (an empty object when it had none).
///
/// # Returns
/// The code.
#[tracing::instrument(level = "trace", skip_all)]
fn error_code(status: StatusCode, reply: &Value) -> String {
    let code_like = |byte: u8| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_';
    ["error", "code"]
        .iter()
        .filter_map(|key| reply.get(*key).and_then(Value::as_str))
        .find(|code| is_code_like(code, code_like))
        .map_or_else(|| format!("HTTP_{}", status.as_u16()), str::to_string)
}

/// One stored submission, as the worker reads it.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub(crate) struct StoredFeedback {
    pub id: String,
    pub user_slug: String,
    pub message: String,
    pub kind: String,
    pub origin: String,
    pub tool: Option<String>,
    pub rating: Option<i16>,
    pub server_version: String,
    pub client: String,
    pub credential_kind: String,
    /// Tries so far, this one included.
    pub attempts: i32,
}

/// A Forminit text block.
#[tracing::instrument(level = "trace", skip_all)]
fn text_block(name: &str, value: &str) -> Value {
    json!({"type": "text", "name": name, "value": value})
}

/// The JSON submission for a stored row: text blocks `message`, `kind`,
/// `from`, `tool` (when given), `server_version`, `client`,
/// `credential_kind` and `feedback_id` (the row's random id, for
/// de-duplicating a retried post); a `rating` block when given; and the
/// `sender` block with `userId`, the one sender field Forminit requires.
/// Nothing else.
///
/// # Arguments
/// * `row` - the stored feedback.
///
/// # Returns
/// `{"blocks": [...]}`.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) fn submission(row: &StoredFeedback) -> Value {
    let mut blocks = vec![
        text_block("message", &row.message),
        text_block("kind", &row.kind),
        text_block("from", &row.origin),
    ];
    if let Some(tool) = &row.tool {
        blocks.push(text_block("tool", tool));
    }
    blocks.push(text_block("server_version", &row.server_version));
    blocks.push(text_block("client", &row.client));
    blocks.push(text_block("credential_kind", &row.credential_kind));
    blocks.push(text_block("feedback_id", &row.id));
    if let Some(rating) = row.rating {
        blocks.push(json!({"type": "rating", "name": "rating", "value": rating}));
    }
    blocks.push(json!({"type": "sender", "properties": {"userId": row.user_slug}}));
    json!({ "blocks": blocks })
}

/// The job's payload: only the feedback's id; the content stays in the
/// `feedback` row, so pruning the row removes it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct FeedbackJob {
    feedback_id: String,
}

/// What the job handler needs (cloned per job: every field is a handle).
#[derive(Clone)]
pub(crate) struct FeedbackContext {
    /// The worker's own pool.
    pub pool: PgPool,
    /// Where to post.
    pub forminit: Arc<Forminit>,
    /// The server clock (`finished_at`).
    pub clock: Arc<dyn Clock>,
}

/// Counts this try on a pending row and reads it.
///
/// # Returns
/// The row with its new try count, or `None` when it is no longer pending
/// (sent, failed or pruned): the job then has nothing to do.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "feedback.take"))]
async fn take(pool: &PgPool, id: &str) -> Result<Option<StoredFeedback>, OkfError> {
    Ok(sqlx::query_as::<_, StoredFeedback>(
        "UPDATE feedback SET attempts = attempts + 1 WHERE id = $1 AND status = 'pending' \
         RETURNING id, user_slug, message, kind, origin, tool, rating, server_version, client, \
                   credential_kind, attempts",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

/// Records how a try ended.
///
/// # Arguments
/// * `pool` - the worker's pool.
/// * `id` - the feedback's id.
/// * `status` - `pending` (to retry), `sent` or `failed`.
/// * `submission_id` - Forminit's hashId, for `sent`.
/// * `last_error` - the failure, if any.
/// * `now` - the server clock; `finished_at` for `sent` and `failed`.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "feedback.settle"))]
async fn settle(
    pool: &PgPool,
    id: &str,
    status: &str,
    submission_id: Option<&str>,
    last_error: Option<&str>,
    now: DateTime<Utc>,
) -> Result<(), OkfError> {
    sqlx::query(
        "UPDATE feedback SET status = $2, submission_id = $3, last_error = $4, \
             finished_at = CASE WHEN $2 = 'pending' THEN NULL ELSE $5 END \
         WHERE id = $1",
    )
    .bind(id)
    .bind(status)
    .bind(submission_id)
    .bind(last_error)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

/// The job handler: one post, then the outcome (see the module doc). Each
/// job is a root `okf.feedback.job` span carrying the outcome; never the
/// message or the user.
#[tracing::instrument(
    level = "info",
    name = "okf.feedback.job",
    parent = None,
    skip_all,
    fields(okf.outcome = tracing::field::Empty)
)]
async fn forward_job(
    job: FeedbackJob,
    context: Data<FeedbackContext>,
    id: TaskId,
) -> Result<(), BoxDynError> {
    let (result, outcome) = forward(&job.feedback_id, &context, &id.to_string()).await;
    tracing::Span::current().record("okf.outcome", outcome);
    result
}

/// One try at the feedback `feedback_id`.
///
/// # Arguments
/// * `feedback_id` - the row's id.
/// * `context` - the worker's state.
/// * `job_id` - the apalis job's id (for the reschedule).
///
/// # Returns
/// What to tell apalis, and the outcome's name for the span.
#[tracing::instrument(level = "debug", skip_all)]
async fn forward(
    feedback_id: &str,
    context: &FeedbackContext,
    job_id: &str,
) -> (Result<(), BoxDynError>, &'static str) {
    let row = match take(&context.pool, feedback_id).await {
        Ok(Some(row)) => row,
        Ok(None) => return (Ok(()), "skipped"),
        // A plain error: apalis fetches the job again.
        Err(error) => return (Err(Box::new(error)), "retry"),
    };
    let now = context.clock.now();
    let error = match context.forminit.post(&submission(&row)).await {
        Ok(hash) => {
            if let Err(error) = settle(
                &context.pool,
                feedback_id,
                "sent",
                hash.as_deref(),
                None,
                now,
            )
            .await
            {
                // Forminit has it; the retry posts it again (a duplicate,
                // with the same `feedback_id` block), which is better than
                // leaving the row pending for good. Delivery is
                // at-least-once.
                return (Err(Box::new(error)), "retry");
            }
            tracing::info!(feedback = %feedback_id, "feedback forwarded");
            return (Ok(()), "sent");
        }
        Err(error) => error,
    };
    // `None`: retry; `Some(reason)`: give up with `reason` on the row.
    let give_up = match &error {
        DeliveryError::Refused(code) => Some(code.clone()),
        DeliveryError::Retryable(reason) if row.attempts >= MAX_TRIES => {
            Some(format!("gave up after {MAX_TRIES} tries: {reason}"))
        }
        DeliveryError::Retryable(_) => None,
    };
    if let Some(reason) = give_up {
        split_event!(
            warn,
            { feedback = %feedback_id, },
            { %reason, },
            "feedback not delivered; marked failed"
        );
        if let Err(error) = settle(
            &context.pool,
            feedback_id,
            "failed",
            None,
            Some(&reason),
            now,
        )
        .await
        {
            return (Err(Box::new(error)), "retry");
        }
        return (Err(Box::new(AbortError::new(error))), "failed");
    }
    let reason = error.to_string();
    if let Err(error) = settle(
        &context.pool,
        feedback_id,
        "pending",
        None,
        Some(&reason),
        now,
    )
    .await
    {
        split_event!(
            error,
            { feedback = %feedback_id, },
            { %error, },
            "recording a feedback try failed"
        );
    }
    if let Err(error) = reschedule_job(&context.pool, QUEUE, job_id).await {
        // Still a plain error: apalis fetches it again, just sooner.
        split_event!(
            error,
            { feedback = %feedback_id, },
            { %error, },
            "rescheduling feedback failed"
        );
    }
    split_event!(
        info,
        { feedback = %feedback_id, },
        { %reason, },
        "feedback not delivered; will retry"
    );
    (Err(Box::new(error)), "retry")
}

/// Starts the feedback worker for a serving process (`http::serve` calls it
/// when `[feedback]` is set with `worker = true`).
///
/// # Arguments
/// * `app` - the shared state (its pool's connect options, Forminit client
///   and clock).
/// * `shutdown` - cancelled when the server stops.
///
/// # Returns
/// The worker's task, or `None` when feedback is not configured.
#[tracing::instrument(level = "debug", skip_all)]
pub fn spawn_worker(app: Arc<App>, shutdown: CancellationToken) -> Option<JoinHandle<()>> {
    let forminit = Arc::clone(app.forminit()?);
    let context = FeedbackContext {
        pool: worker_pool_named(
            app.accounts().executor(),
            WORKER_APPLICATION_NAME,
            WORKER_POOL_SIZE,
        ),
        forminit,
        clock: app.clock_handle(),
    };
    Some(run_worker(
        context,
        process_worker_name("okf-feedback"),
        WorkerTiming::SERVE,
        shutdown,
    ))
}

/// Runs one apalis worker on [`QUEUE`] until `shutdown`, restarting it
/// after `timing.restart` if it stops on an error, as the mail worker does.
/// It closes `context.pool` when it ends.
///
/// # Arguments
/// * `context` - what the handler needs; its pool is the worker's own.
/// * `name` - the worker's name; a restart registers as `<name>-<restarts>`.
/// * `timing` - the poll, heartbeat and restart delays.
/// * `shutdown` - cancelled to stop.
///
/// # Returns
/// The worker's task.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) fn run_worker(
    context: FeedbackContext,
    name: String,
    timing: WorkerTiming,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        run_until_cancelled(&context, &name, timing, &shutdown).await;
        context.pool.close().await;
    })
}

/// [`run_worker`]'s loop: returns once `shutdown` is cancelled.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "long-lived loop: its span would never close and would stay open as the parent of every later span"
    )
)]
async fn run_until_cancelled(
    context: &FeedbackContext,
    name: &str,
    timing: WorkerTiming,
    shutdown: &CancellationToken,
) {
    for restarts in 0_u64.. {
        let config = QueueConfig::default()
            .queue(QUEUE)
            .heartbeat_interval(timing.heartbeat)
            .missed_heartbeats(timing.missed_heartbeats);
        let backend = PostgresStorage::<FeedbackJob>::new(&context.pool)
            .with_config(config)
            .with_pubsub()
            .poll_with_interval(timing.poll);
        let registered = if restarts == 0 {
            name.to_string()
        } else {
            format!("{name}-{restarts}")
        };
        let worker = WorkerBuilder::new(registered)
            .backend(backend)
            .data(context.clone())
            .build(forward_job);
        let stop = shutdown.clone();
        let signal = async move {
            stop.cancelled().await;
            Ok::<(), std::io::Error>(())
        };
        match worker.run_until(signal).await {
            Ok(()) => return,
            Err(error) => {
                split_event!(
                    error,
                    {},
                    { %error, },
                    "the feedback worker stopped; restarting it soon"
                );
            }
        }
        tokio::select! {
            () = shutdown.cancelled() => return,
            () = tokio::time::sleep(timing.restart) => {}
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use axum::extract::{Path, State};
    use axum::http::HeaderMap;
    use axum::routing::post;
    use axum::Router;

    use crate::clock::FixedClock;
    use crate::db::test_db;

    /// A placeholder form id (not a real form).
    pub(crate) const FORM: &str = "form-placeholder_1";

    /// A placeholder API key (not a real key).
    pub(crate) const API_KEY: &str = "fi-test-key-zq7c";

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-18T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    /// One request the mock saw: the form id in its path, its headers, its
    /// JSON body.
    pub(crate) type Seen = (String, HeaderMap, Value);

    /// A mock Forminit: answers each post from a script of `(status,
    /// body)` (then `200` with a hashId once it runs out), remembering what
    /// it was sent.
    #[derive(Clone, Default)]
    pub(crate) struct MockForminit {
        pub seen: Arc<Mutex<Vec<Seen>>>,
        answers: Arc<Mutex<VecDeque<(u16, Value)>>>,
        /// When set, each post is recorded, then held unanswered until
        /// [`Hold::release`] is notified (a delivery in flight).
        hold: Option<Arc<Hold>>,
    }

    /// The two signals of a held post. `Notify::notify_one` keeps a permit
    /// when no one is waiting yet, so neither side can miss the other.
    #[derive(Default)]
    pub(crate) struct Hold {
        /// Notified once a post has been recorded (and is being held).
        pub arrived: tokio::sync::Notify,
        /// Notify to let the held post be answered.
        pub release: tokio::sync::Notify,
    }

    impl MockForminit {
        /// What it has been sent so far.
        pub(crate) fn seen(&self) -> Vec<Seen> {
            self.seen.lock().unwrap().clone()
        }

        /// Waits (up to 20 s) until it has been sent `count` posts.
        pub(crate) async fn wait_for(&self, count: usize) -> Vec<Seen> {
            for _ in 0..200 {
                let seen = self.seen();
                if seen.len() >= count {
                    return seen;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            panic!("the mock saw {} posts, not {count}", self.seen().len());
        }
    }

    async fn handle(
        State(mock): State<MockForminit>,
        Path(form): Path<String>,
        headers: HeaderMap,
        body: axum::Json<Value>,
    ) -> (axum::http::StatusCode, axum::Json<Value>) {
        mock.seen.lock().unwrap().push((form, headers, body.0));
        if let Some(hold) = &mock.hold {
            hold.arrived.notify_one();
            hold.release.notified().await;
        }
        let (status, reply) = mock.answers.lock().unwrap().pop_front().unwrap_or((
            200,
            json!({"success": true, "submission": {"hashId": "hash0001", "date": "2026-11-18"}}),
        ));
        (
            axum::http::StatusCode::from_u16(status).unwrap(),
            axum::Json(reply),
        )
    }

    /// Serves a mock `/f/{form}` on a free loopback port.
    ///
    /// # Returns
    /// Its origin (`http://127.0.0.1:<port>`) and the mock.
    pub(crate) async fn mock_forminit(answers: Vec<(u16, Value)>) -> (String, MockForminit) {
        serve_mock(MockForminit {
            seen: Arc::default(),
            answers: Arc::new(Mutex::new(answers.into())),
            hold: None,
        })
        .await
    }

    /// As [`mock_forminit`], but every post is held until its [`Hold`] is
    /// released.
    async fn held_forminit(answers: Vec<(u16, Value)>) -> (String, MockForminit, Arc<Hold>) {
        let hold = Arc::new(Hold::default());
        let (origin, mock) = serve_mock(MockForminit {
            seen: Arc::default(),
            answers: Arc::new(Mutex::new(answers.into())),
            hold: Some(Arc::clone(&hold)),
        })
        .await;
        (origin, mock, hold)
    }

    async fn serve_mock(mock: MockForminit) -> (String, MockForminit) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let router = Router::new()
            .route("/f/{form}", post(handle))
            .with_state(mock.clone());
        tokio::spawn(async move { axum::serve(listener, router).await });
        (origin, mock)
    }

    /// A `[feedback]` table for [`FORM`] on `origin`, with its key in
    /// `key_env` when given.
    pub(crate) fn config_on(origin: &str, key_env: Option<&str>) -> FeedbackConfig {
        FeedbackConfig {
            api_key_env: key_env.map(str::to_string),
            base_url_for_tests: Some(origin.to_string()),
            ..FeedbackConfig::for_form(FORM)
        }
    }

    fn client_on(origin: &str, key: Option<&str>) -> Forminit {
        let key = key.map(str::to_string);
        Forminit::from_config(&config_on(origin, key.as_ref().map(|_| "KEY")), &|_| {
            key.clone()
        })
        .unwrap()
    }

    fn sample<'a>(credential: &'a str) -> NewFeedback<'a> {
        NewFeedback {
            credential,
            user_slug: "alice",
            registered: false,
            message: "the publish error was hard to read",
            kind: FeedbackKind::Confusing,
            origin: FeedbackOrigin::User,
            tool: Some("publish"),
            rating: Some(3),
            client: "MCPJam",
            credential_kind: "api_key",
            caller: Caller::Credential,
        }
    }

    /// The exact submission [`sample`] makes, stored as feedback `id`.
    fn sample_body(id: &str) -> Value {
        json!({"blocks": [
            {"type": "text", "name": "message", "value": "the publish error was hard to read"},
            {"type": "text", "name": "kind", "value": "confusing"},
            {"type": "text", "name": "from", "value": "user"},
            {"type": "text", "name": "tool", "value": "publish"},
            {"type": "text", "name": "server_version", "value": env!("CARGO_PKG_VERSION")},
            {"type": "text", "name": "client", "value": "MCPJam"},
            {"type": "text", "name": "credential_kind", "value": "api_key"},
            {"type": "text", "name": "feedback_id", "value": id},
            {"type": "rating", "name": "rating", "value": 3},
            {"type": "sender", "properties": {"userId": "alice"}},
        ]})
    }

    const FAST: WorkerTiming = WorkerTiming {
        poll: Duration::from_millis(100),
        heartbeat: Duration::from_secs(1),
        restart: Duration::from_millis(200),
        ..WorkerTiming::SERVE
    };

    fn start(pool: &PgPool, forminit: Forminit) -> (CancellationToken, JoinHandle<()>) {
        let context = FeedbackContext {
            pool: worker_pool_named(pool, WORKER_APPLICATION_NAME, WORKER_POOL_SIZE),
            forminit: Arc::new(forminit),
            clock: Arc::new(FixedClock(now())),
        };
        let shutdown = CancellationToken::new();
        let worker = run_worker(
            context,
            process_worker_name("okf-feedback-test"),
            FAST,
            shutdown.clone(),
        );
        (shutdown, worker)
    }

    async fn stop(shutdown: CancellationToken, worker: JoinHandle<()>) {
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(20), worker)
            .await
            .expect("the worker stops within 20 s")
            .unwrap();
    }

    /// `(status, attempts, last_error, submission_id)` of feedback `id`.
    type RowState = (String, i32, Option<String>, Option<String>);

    async fn row_state(pool: &PgPool, id: &str) -> RowState {
        sqlx::query_as(
            "SELECT status, attempts, last_error, submission_id FROM feedback WHERE id = $1",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// Waits (up to 20 s) until feedback `id` has `status`, making its job
    /// due at once each time (as if its backoff had passed).
    async fn wait_status(pool: &PgPool, id: &str, status: &str) -> RowState {
        for _ in 0..200 {
            let state = row_state(pool, id).await;
            if state.0 == status {
                return state;
            }
            sqlx::query(
                "UPDATE apalis.jobs SET run_at = now() - interval '1 second' \
                 WHERE job_type = $1 AND status = 'Failed'",
            )
            .bind(QUEUE)
            .execute(pool)
            .await
            .unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!(
            "feedback {id} is {:?}, not {status}",
            row_state(pool, id).await
        );
    }

    async fn job_status(pool: &PgPool) -> String {
        sqlx::query_scalar("SELECT status FROM apalis.jobs WHERE job_type = $1")
            .bind(QUEUE)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[test]
    fn the_submission_holds_exactly_the_documented_blocks() {
        let row = StoredFeedback {
            id: "id".to_string(),
            user_slug: "alice".to_string(),
            message: "the publish error was hard to read".to_string(),
            kind: "confusing".to_string(),
            origin: "user".to_string(),
            tool: Some("publish".to_string()),
            rating: Some(3),
            server_version: env!("CARGO_PKG_VERSION").to_string(),
            client: "MCPJam".to_string(),
            credential_kind: "api_key".to_string(),
            attempts: 1,
        };
        let bare = StoredFeedback {
            tool: None,
            rating: None,
            ..row.clone()
        };

        assert_eq!(submission(&row), sample_body("id"));
        let bare_body = submission(&bare);
        let names: Vec<&str> = bare_body["blocks"]
            .as_array()
            .unwrap()
            .iter()
            // A field block's name; the sender block has only its type.
            .filter_map(|block| block["name"].as_str().or(block["type"].as_str()))
            .collect();
        assert_eq!(
            names,
            [
                "message",
                "kind",
                "from",
                "server_version",
                "client",
                "credential_kind",
                "feedback_id",
                "sender"
            ]
        );
    }

    #[test]
    fn the_client_posts_to_forminit_and_never_shows_its_key() {
        let config = FeedbackConfig {
            api_key_env: Some("OKF_TEST_FORMINIT_KEY".to_string()),
            ..FeedbackConfig::for_form(FORM)
        };

        let client = Forminit::from_config(&config, &|_| Some(API_KEY.to_string())).unwrap();

        assert_eq!(client.endpoint(), format!("https://forminit.com/f/{FORM}"));
        let shown = format!("{client:?}");
        assert!(
            shown.contains("<set>") && !shown.contains(API_KEY),
            "{shown}"
        );
    }

    #[test]
    fn a_missing_or_unusable_key_stops_start_up_naming_only_the_variable() {
        let config = FeedbackConfig {
            api_key_env: Some("OKF_TEST_FORMINIT_KEY".to_string()),
            ..FeedbackConfig::for_form(FORM)
        };

        let missing = Forminit::from_config(&config, &|_| None).unwrap_err();
        let blank = Forminit::from_config(&config, &|_| Some("  ".to_string())).unwrap_err();
        let bad = Forminit::from_config(&config, &|_| Some("zq\nsecret".to_string())).unwrap_err();
        let bad_form = Forminit::from_config(&FeedbackConfig::for_form("../x"), &|_| None);

        for error in [missing, blank, bad] {
            let text = error.to_string();
            assert!(matches!(error, OkfError::Config { .. }), "{text}");
            assert!(text.contains("OKF_TEST_FORMINIT_KEY"), "{text}");
            assert!(!text.contains("secret"), "{text}");
        }
        assert!(matches!(bad_form, Err(OkfError::Config { .. })));
    }

    #[tokio::test]
    async fn a_post_carries_the_key_only_when_one_is_configured() {
        let (origin, mock) = mock_forminit(vec![]).await;

        let with_key = client_on(&origin, Some(API_KEY))
            .post(&sample_body("id"))
            .await;
        let without = client_on(&origin, None).post(&sample_body("id")).await;

        assert_eq!(with_key, Ok(Some("hash0001".to_string())));
        assert_eq!(without, Ok(Some("hash0001".to_string())));
        let seen = mock.seen();
        assert_eq!(seen[0].0, FORM);
        assert_eq!(seen[0].1["x-api-key"], API_KEY);
        assert_eq!(seen[0].1["content-type"], "application/json");
        assert!(seen[0].1["user-agent"]
            .to_str()
            .unwrap()
            .starts_with("okf-mcp/"));
        assert_eq!(seen[0].2, sample_body("id"));
        assert!(seen[1].1.get("x-api-key").is_none());
    }

    #[tokio::test]
    async fn answers_are_sorted_into_sent_retryable_and_refused() {
        let (origin, _mock) = mock_forminit(vec![
            (429, json!({"success": false, "error": "TOO_MANY_REQUESTS"})),
            (503, json!({})),
            (
                404,
                json!({"success": false, "error": "FORM_NOT_FOUND", "message": "x"}),
            ),
            (
                400,
                json!({"success": false, "error": "a free text error, not a code"}),
            ),
            (200, json!({"success": true})),
        ])
        .await;
        let client = client_on(&origin, None);

        let mut answers = Vec::new();
        for _ in 0..5 {
            answers.push(client.post(&sample_body("id")).await);
        }
        let unreachable = client_on("http://127.0.0.1:9", None)
            .post(&sample_body("id"))
            .await;

        assert_eq!(
            answers,
            [
                Err(DeliveryError::Retryable(
                    "forminit answered 429".to_string()
                )),
                Err(DeliveryError::Retryable(
                    "forminit answered 503".to_string()
                )),
                Err(DeliveryError::Refused("FORM_NOT_FOUND".to_string())),
                Err(DeliveryError::Refused("HTTP_400".to_string())),
                Ok(None),
            ]
        );
        assert!(matches!(unreachable, Err(DeliveryError::Retryable(_))));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_worker_forwards_a_submission_and_stores_its_hash() {
        let db = test_db().await;
        let (origin, mock) = mock_forminit(vec![]).await;
        let (shutdown, worker) = start(&db.pool, client_on(&origin, Some(API_KEY)));

        let id = submit(&db.pool, &sample("key-1"), 10, now()).await.unwrap();
        let state = wait_status(&db.pool, &id, "sent").await;
        stop(shutdown, worker).await;

        assert_eq!(id.len(), 32);
        assert_eq!(
            state,
            ("sent".to_string(), 1, None, Some("hash0001".to_string()))
        );
        let seen = mock.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, FORM);
        assert_eq!(seen[0].1["x-api-key"], API_KEY);
        assert_eq!(seen[0].2, sample_body(&id));
        assert_eq!(job_status(&db.pool).await, "Done");
        // The job carries the id only, never the message.
        let payload: String =
            sqlx::query_scalar("SELECT convert_from(job, 'UTF8') FROM apalis.jobs")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(payload, format!("{{\"feedback_id\":\"{id}\"}}"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rate_limits_and_server_errors_are_retried_with_backoff() {
        let db = test_db().await;
        let (origin, mock) = mock_forminit(vec![(429, json!({})), (503, json!({}))]).await;
        let (shutdown, worker) = start(&db.pool, client_on(&origin, None));

        let id = submit(&db.pool, &sample("key-1"), 10, now()).await.unwrap();
        // After the first failure the job waits out its backoff (30 s).
        mock.wait_for(1).await;
        let mut waited = 0.0_f64;
        for _ in 0..100 {
            waited = sqlx::query_scalar(
                "SELECT extract(epoch FROM run_at - now())::float8 FROM apalis.jobs \
                 WHERE status = 'Failed'",
            )
            .fetch_optional(&db.pool)
            .await
            .unwrap()
            .unwrap_or(0.0);
            if waited > 0.0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let retrying = row_state(&db.pool, &id).await;
        let state = wait_status(&db.pool, &id, "sent").await;
        stop(shutdown, worker).await;

        assert!(
            waited > 20.0 && waited <= 31.0,
            "the first retry waits 30 s: {waited}"
        );
        assert_eq!(retrying.0, "pending");
        assert_eq!(
            retrying.2.as_deref(),
            Some("retryable: forminit answered 429")
        );
        assert_eq!(
            state,
            ("sent".to_string(), 3, None, Some("hash0001".to_string()))
        );
        assert_eq!(mock.seen().len(), 3);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_refusal_marks_the_feedback_failed_without_a_retry() {
        for (status, reply, code) in [
            (
                404,
                json!({"success": false, "error": "FORM_NOT_FOUND"}),
                "FORM_NOT_FOUND",
            ),
            (
                400,
                json!({"success": false, "error": "VALIDATION_ERROR"}),
                "VALIDATION_ERROR",
            ),
        ] {
            let db = test_db().await;
            let (origin, mock) = mock_forminit(vec![(status, reply)]).await;
            let (shutdown, worker) = start(&db.pool, client_on(&origin, None));

            let id = submit(&db.pool, &sample("key-1"), 10, now()).await.unwrap();
            let state = wait_status(&db.pool, &id, "failed").await;
            // Long enough for a retry to have happened, were there one.
            tokio::time::sleep(Duration::from_millis(500)).await;
            stop(shutdown, worker).await;

            assert_eq!(
                state,
                ("failed".to_string(), 1, Some(code.to_string()), None)
            );
            assert_eq!(mock.seen().len(), 1, "{code} is not retried");
            assert_eq!(job_status(&db.pool).await, "Killed");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn retryable_errors_give_up_after_the_last_try() {
        let db = test_db().await;
        let (origin, mock) = mock_forminit(vec![(503, json!({}))]).await;
        let id = submit(&db.pool, &sample("key-1"), 10, now()).await.unwrap();
        // As if the earlier tries had all failed.
        sqlx::query("UPDATE feedback SET attempts = $1")
            .bind(MAX_TRIES - 1)
            .execute(&db.pool)
            .await
            .unwrap();
        let (shutdown, worker) = start(&db.pool, client_on(&origin, None));

        let state = wait_status(&db.pool, &id, "failed").await;
        stop(shutdown, worker).await;

        assert_eq!(state.1, MAX_TRIES);
        assert_eq!(
            state.2.as_deref(),
            Some("gave up after 10 tries: forminit answered 503")
        );
        assert_eq!(mock.seen().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn serve_refuses_to_start_when_the_key_variable_is_unset() {
        let db = test_db().await;
        let config = crate::config::Config {
            feedback: Some(FeedbackConfig {
                api_key_env: Some("OKF_TEST_FEEDBACK_KEY_UNSET_674".to_string()),
                ..FeedbackConfig::for_form(FORM)
            }),
            ..crate::config::Config::default()
        };

        let loaded = App::load(db.pool.clone(), config, Arc::new(FixedClock(now()))).await;

        match loaded {
            Err(error @ OkfError::Config { .. }) => {
                assert!(
                    error
                        .to_string()
                        .contains("OKF_TEST_FEEDBACK_KEY_UNSET_674"),
                    "{error}"
                );
            }
            Err(other) => panic!("not a config error: {other}"),
            Ok(_) => panic!("the app loaded without its key"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_rate_limit_counts_each_credentials_last_hour() {
        let db = test_db().await;
        for _ in 0..3 {
            submit(&db.pool, &sample("key-1"), 3, now()).await.unwrap();
        }

        let refused = submit(&db.pool, &sample("key-1"), 3, now()).await;
        let other = submit(&db.pool, &sample("key-2"), 3, now()).await;
        let later = submit(
            &db.pool,
            &sample("key-1"),
            3,
            now() + TimeDelta::minutes(61),
        )
        .await;

        match refused {
            Err(OkfError::LimitExceeded {
                limit: "feedback_rate",
                reason,
            }) => assert!(
                reason.contains("3 feedback messages in the last hour"),
                "{reason}"
            ),
            other => panic!("not refused: {other:?}"),
        }
        assert!(other.is_ok());
        assert!(later.is_ok());
        let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM feedback")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(stored, 5);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_rate_limit_holds_across_replicas_submitting_at_once() {
        let db = test_db().await;
        // Two pools, as two replicas have, racing 20 submissions.
        let replicas = [
            worker_pool_named(&db.pool, "replica-a", 10),
            worker_pool_named(&db.pool, "replica-b", 10),
        ];
        let mut tasks = tokio::task::JoinSet::new();
        for index in 0..20 {
            let pool = replicas[index % 2].clone();
            tasks.spawn(async move { submit(&pool, &sample("key-1"), 10, now()).await });
        }
        let mut admitted = 0;
        let mut refused = 0;
        let results = tokio::time::timeout(Duration::from_secs(30), tasks.join_all())
            .await
            .expect("the submissions finish within 30 s");
        for result in results {
            match result {
                Ok(_) => admitted += 1,
                Err(OkfError::LimitExceeded { .. }) => refused += 1,
                Err(error) => panic!("{error}"),
            }
        }

        assert_eq!((admitted, refused), (10, 10));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn prune_deletes_sent_and_failed_feedback_past_retention_only() {
        let db = test_db().await;
        let mut ids = Vec::new();
        for _ in 0..4 {
            ids.push(submit(&db.pool, &sample("key-1"), 10, now()).await.unwrap());
        }
        let old = now() - TimeDelta::days(31);
        for (id, status, finished) in [
            (&ids[0], "sent", Some(old)),
            (&ids[1], "sent", Some(now() - TimeDelta::days(29))),
            (&ids[2], "failed", Some(old)),
            (&ids[3], "pending", None),
        ] {
            sqlx::query(
                "UPDATE feedback SET status = $2, finished_at = $3, created_at = $4 WHERE id = $1",
            )
            .bind(id)
            .bind(status)
            .bind(finished)
            .bind(old - TimeDelta::days(1))
            .execute(&db.pool)
            .await
            .unwrap();
        }

        let pruned = prune(&db.pool, now(), TimeDelta::days(30)).await.unwrap();

        assert_eq!(pruned, 2);
        let left: Vec<String> = sqlx::query_scalar("SELECT id FROM feedback ORDER BY created_at")
            .fetch_all(&db.pool)
            .await
            .unwrap();
        assert_eq!(left.len(), 2);
        assert!(!left.contains(&ids[0]) && !left.contains(&ids[2]));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_row_is_listed_and_retried_once() {
        let db = test_db().await;
        let failed = submit(&db.pool, &sample("key-1"), 10, now()).await.unwrap();
        let pending = submit(&db.pool, &sample("key-1"), 10, now()).await.unwrap();
        sqlx::query(
            "UPDATE feedback SET status = 'failed', attempts = 1, last_error = 'FORM_NOT_FOUND', \
                 finished_at = $2 WHERE id = $1",
        )
        .bind(&failed)
        .bind(now())
        .execute(&db.pool)
        .await
        .unwrap();
        let count_jobs = || async {
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM apalis.jobs WHERE job_type = $1")
                .bind(QUEUE)
                .fetch_one(&db.pool)
                .await
                .unwrap()
        };
        let jobs_before = count_jobs().await;

        let listed = list_failed(&db.pool).await.unwrap();
        retry(&db.pool, &failed).await.unwrap();
        let again = retry(&db.pool, &failed).await;
        let not_failed = retry(&db.pool, &pending).await;
        let unknown = retry(&db.pool, "nope").await;

        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, failed);
        assert_eq!(listed[0].last_error.as_deref(), Some("FORM_NOT_FOUND"));
        assert_eq!(listed[0].user_slug, "alice");
        let state = row_state(&db.pool, &failed).await;
        assert_eq!((state.0.as_str(), state.1), ("pending", 0));
        assert!(list_failed(&db.pool).await.unwrap().is_empty());
        assert_eq!(
            count_jobs().await,
            jobs_before + 1,
            "one new job, for the retry only"
        );
        assert!(matches!(again, Err(OkfError::Refused { .. })), "{again:?}");
        assert!(
            matches!(not_failed, Err(OkfError::Refused { .. })),
            "{not_failed:?}"
        );
        assert!(matches!(
            unknown,
            Err(OkfError::UnknownName {
                noun: "feedback",
                ..
            })
        ));
    }

    /// Review of PR #951: user deletion does not wait for, or cancel, a
    /// delivery the worker already started. The post is held after `take`
    /// claimed the row; the deletion completes meanwhile (no lock is held
    /// across the post), the held post still reaches Forminit, and whether
    /// it then succeeds or asks for a retry, the deleted row is not
    /// recreated and a later try of the job posts nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_delivery_in_flight_is_not_cancelled_by_user_deletion_and_never_retried() {
        use crate::store::audit::AuditActor;
        use crate::store::users::{delete, tests::seed_user, DeleteOptions};

        for (answer, outcome) in [
            ((200, json!({"success": true})), "sent"),
            ((503, json!({})), "retry"),
        ] {
            // Arrange: alice's feedback, its post held after the claim.
            let db = test_db().await;
            seed_user(&db.pool, "alice", None).await;
            let new = NewFeedback {
                registered: true,
                ..sample("key-1")
            };
            let id = submit(&db.pool, &new, 10, now()).await.unwrap();
            let (origin, mock, hold) = held_forminit(vec![answer]).await;
            let context = FeedbackContext {
                pool: db.pool.clone(),
                forminit: Arc::new(client_on(&origin, None)),
                clock: Arc::new(FixedClock(now())),
            };
            let in_flight = {
                let (context, id) = (context.clone(), id.clone());
                tokio::spawn(async move { forward(&id, &context, "job-1").await.1 })
            };
            hold.arrived.notified().await;

            // Act: delete alice while the post is held, then answer it.
            tokio::time::timeout(
                Duration::from_secs(30),
                delete(
                    &db.pool,
                    "alice",
                    DeleteOptions::default(),
                    &AuditActor::cli(),
                    now(),
                ),
            )
            .await
            .expect("the deletion does not wait for the delivery in flight")
            .unwrap();
            hold.release.notify_one();
            let first = in_flight.await.unwrap();
            // The job's next try, after the deletion.
            let later = forward(&id, &context, "job-1").await.1;

            // Assert
            assert_eq!(first, outcome);
            assert_eq!(mock.seen().len(), 1, "the in-flight post went out, once");
            let left: i64 = sqlx::query_scalar("SELECT count(*) FROM feedback")
                .fetch_one(&db.pool)
                .await
                .unwrap();
            assert_eq!(left, 0, "settling the try did not recreate the row");
            assert_eq!(later, "skipped");
            assert_eq!(mock.seen().len(), 1, "nothing posted after the deletion");
        }
    }
}
