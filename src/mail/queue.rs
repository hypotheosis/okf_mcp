//! The `SendMail` job on apalis-postgres (spec "Background jobs";
//! Decision 11's durable retry).
//!
//! The life of a job:
//! 1. [`enqueue`] renders the template and inserts one `apalis.jobs` row in
//!    queue [`QUEUE`] with `max_attempts` [`MAX_ATTEMPTS`]. The payload is
//!    the rendered [`Mail`], the template's name and `queued_at`.
//! 2. A worker in every `serve` process ([`spawn_worker`]) fetches it. apalis
//!    takes jobs with `FOR UPDATE SKIP LOCKED` and marks them as its own, so
//!    two workers (two replicas) never take the same job. A replica that
//!    dies loses its job to another one after [`ORPHAN_TIMEOUT`] (one
//!    minute); delivery is at least once. The workers share one
//!    [`WorkerPool`] of [`WORKER_POOL_SIZE`] connections, so N replicas
//!    open `3 * N` beside the request pools.
//! 3. The handler sends once. Success: `Done`. A permanent error: an
//!    `AbortError`, so apalis marks the job `Killed` and never fetches it
//!    again. A retryable error within [`RETRY_WINDOW_HOURS`] of `queued_at`:
//!    the handler moves `run_at` to now plus the backoff and returns a plain
//!    error, so apalis marks it `Failed` with one more attempt and fetches it
//!    again once `run_at` passes. After the window: an `AbortError`.
//! 4. [`list`], [`retry`], [`delete`], [`prune`] and [`prune_workers`] read and write
//!    `apalis.jobs` in SQL, where apalis-postgres has no API.

use std::sync::Arc;
use std::time::Duration;

use apalis::prelude::{
    AbortError, BackendExt, BoxDynError, Data, TaskBuilder, TaskId, WorkerBuilder,
};
use apalis_postgres::queries::push_tasks;
use apalis_postgres::{Config as QueueConfig, PostgresStorage};
use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgConnection, PgPool};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::{render, Mail, MailError, Mailer, Template};
use crate::app::App;
use crate::clock::Clock;
use crate::config::MailProvider;
use crate::error::OkfError;
use crate::store::settings::{self, Brand};
use crate::store::users::check_email;
use crate::telemetry::metrics::{MailOutcome, MailStatus, Metrics};
use crate::telemetry::subscriber::split_event;

/// The apalis queue (`apalis.jobs.job_type`). Set explicitly: apalis's
/// default is the Rust type's path, which a rename would orphan.
pub const QUEUE: &str = "send_mail";

/// `max_attempts` for every job: more than the 24-hour window can use, so
/// the window, not the count, ends the retries (Decision 11).
pub const MAX_ATTEMPTS: usize = 64;

/// How long a mail keeps being retried after it was queued.
pub const RETRY_WINDOW_HOURS: i64 = 24;

/// The first retry's delay; each later one doubles, up to [`BACKOFF_MAX_SECS`].
const BACKOFF_FIRST_SECS: f64 = 30.0;

/// The longest delay between two tries.
const BACKOFF_MAX_SECS: f64 = 3600.0;

/// How often an idle worker looks for due jobs that no NOTIFY announced
/// (rescheduled and manually retried ones).
pub const POLL_INTERVAL: Duration = Duration::from_secs(30);

/// How long a worker that stopped on an error waits before starting again.
const RESTART_DELAY: Duration = Duration::from_secs(30);

/// How often a worker tells apalis it is alive (apalis's default is 30 s;
/// 10 s shortens the orphan timeout to a minute, issue #458).
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

/// How many heartbeats a worker may miss before apalis counts it dead and
/// re-enqueues its jobs (apalis-postgres's default is 10).
const MISSED_HEARTBEATS: usize = 6;

/// The orphan timeout: how long a dead replica's mail stays `sending`
/// before another replica takes it back (`HEARTBEAT_INTERVAL` times
/// `MISSED_HEARTBEATS`, one minute).
pub const ORPHAN_TIMEOUT: Duration =
    Duration::from_secs(HEARTBEAT_INTERVAL.as_secs() * MISSED_HEARTBEATS as u64);

/// [`prune_workers`] keeps the row of a worker seen within this many orphan
/// timeouts, so a worker that is merely slow is never pruned.
const WORKER_ROW_ORPHAN_TIMEOUTS: u32 = 10;

/// The connections the process's one worker pool may open: the mail
/// worker's LISTEN, the mail worker's fetch, ack, heartbeat and the handler's
/// reschedule writes, and the housekeeping worker's fetch, ack and heartbeat
/// (it polls, with no LISTEN). Never taken from the request pool, so
/// `database_max_connections` stays the request side's alone. A deployment
/// of N replicas opens at most `3 * N` of them in all.
pub const WORKER_POOL_SIZE: u32 = 3;

/// The `application_name` the worker's connections carry, so an operator
/// (or a test) can tell them apart in `pg_stat_activity`.
pub const WORKER_APPLICATION_NAME: &str = "okf-mail-worker";

/// The process's one pool for background workers: [`spawn_worker`] and
/// housekeeping's worker share it (so the connection cost per replica is
/// [`WORKER_POOL_SIZE`] however many workers run). Cloning it shares the
/// pool; whoever created it closes it once every worker has stopped.
#[derive(Clone)]
pub struct WorkerPool {
    pool: PgPool,
}

impl WorkerPool {
    /// Builds the pool (lazily: it connects on first use).
    ///
    /// # Arguments
    /// * `pool` - the request pool, whose connect options are copied.
    ///
    /// # Returns
    /// A pool of at most [`WORKER_POOL_SIZE`] connections named
    /// [`WORKER_APPLICATION_NAME`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn new(pool: &PgPool) -> Self {
        Self {
            pool: worker_pool(pool),
        }
    }

    /// The pool itself, for a worker to run on.
    ///
    /// # Returns
    /// The shared pool (a cheap handle; clone it to keep one).
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "one-line accessor")
    )]
    pub(crate) fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Closes the pool once every worker that uses it has stopped.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn close(&self) {
        self.pool.close().await;
    }
}

/// How long sent mail jobs are kept.
const DONE_RETENTION_DAYS: i64 = 7;

/// How long failed mail jobs are kept.
const FAILED_RETENTION_DAYS: i64 = 30;

/// The job's payload: the rendered mail, its template's name (for the
/// queue listing) and when it was queued (Unix seconds, server clock; the
/// start of its retry window). `Debug` omits the mail's bodies.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct SendMail {
    mail: Mail,
    template: String,
    queued_at: i64,
}

impl std::fmt::Debug for SendMail {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "Debug impl: runs while an event is formatted, and a span there re-enters the subscriber"
        )
    )]
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `Mail`'s own `Debug` already leaves the bodies out.
        formatter
            .debug_struct("SendMail")
            .field("mail", &self.mail)
            .field("template", &self.template)
            .field("queued_at", &self.queued_at)
            .finish()
    }
}

/// One mail job as `mail queue` and the Settings page show it: never the body.
// `sqlx::FromRow` maps a result row's columns to the fields by name.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct QueuedMail {
    /// The job id (a ULID), for retry and delete.
    pub id: String,
    /// The recipient.
    pub to: String,
    /// The template's name.
    pub template: String,
    /// `pending`, `sending`, `retrying` or `failed`.
    pub status: String,
    /// Tries so far.
    pub attempts: i32,
    /// The last try's error, if it failed.
    pub last_error: Option<String>,
    /// When it is (or was) due.
    pub run_at: DateTime<Utc>,
}

/// The SQL expression for a job's state as `mail queue` and the depth
/// gauge name it: `failed`, `retrying`, `sending` or `pending`.
///
/// A macro, not a `const`, so `concat!` can build the query at compile
/// time: sqlx takes only literal SQL.
macro_rules! status_sql {
    () => {
        "CASE WHEN status = 'Killed' \
                OR (status = 'Failed' AND attempts >= max_attempts) THEN 'failed' \
             WHEN status = 'Failed' THEN 'retrying' \
             WHEN status IN ('Queued', 'Running') THEN 'sending' \
             ELSE 'pending' END"
    };
}

/// The error for a queue operation apalis refused (its own error type).
#[tracing::instrument(level = "trace", skip_all)]
fn queue_error(error: impl std::fmt::Display) -> OkfError {
    OkfError::Io {
        path: "<mail queue>".to_string(),
        source: std::io::Error::other(error.to_string()),
    }
}

/// Renders a template and queues it for `to`. Call it after the request's
/// own transaction commits, so a rolled-back change sends nothing, or use
/// [`enqueue_in`] inside that transaction.
///
/// # Arguments
/// * `pool` - the database.
/// * `to` - one recipient address.
/// * `template` - what to send.
/// * `now` - when (server clock): the start of the 24-hour retry window.
/// * `defaults` - the config's branding (`AuthConfig::default_brand`):
///   what a mail takes where no setting overrides it.
///
/// # Errors
/// As [`enqueue_in`].
#[tracing::instrument(level = "debug", skip_all)]
pub async fn enqueue(
    pool: &PgPool,
    to: &str,
    template: &Template,
    now: DateTime<Utc>,
    defaults: &Brand,
) -> Result<(), OkfError> {
    let mut conn = pool.acquire().await?;
    // `&mut *conn`: the pooled connection's inner `PgConnection`.
    enqueue_in(&mut conn, to, template, now, defaults).await
}

/// [`enqueue`], inside the caller's transaction: the job exists only if the
/// change it reports commits (apalis' notify is delivered at the commit),
/// and nothing runs after that commit (a sign-in's "method added" notice).
///
/// # Arguments
/// * `conn` - the caller's connection or transaction (`&mut tx`).
/// * `to` - one recipient address.
/// * `template` - what to send.
/// * `now` - when (server clock): the start of the 24-hour retry window.
/// * `defaults` - the config's branding (`AuthConfig::default_brand`):
///   what a mail takes where no setting overrides it.
///
/// # Errors
/// [`OkfError::LimitExceeded`] with `limit: "email"` for a malformed
/// address; [`OkfError::Database`] or [`OkfError::Io`] if the insert fails.
#[tracing::instrument(level = "debug", skip_all)]
pub async fn enqueue_in(
    conn: &mut PgConnection,
    to: &str,
    template: &Template,
    now: DateTime<Utc>,
    defaults: &Brand,
) -> Result<(), OkfError> {
    check_email(to)?;
    // The brand stored now, read on the same connection: whoever queues the
    // mail (a server or the CLI), it names the product the operator set.
    let brand = settings::brand_in(&mut *conn, defaults).await?;
    let (subject, text, html) = render(template, &brand);
    let job = SendMail {
        mail: Mail {
            to: to.to_string(),
            subject,
            text,
            html,
        },
        template: template.name().to_string(),
        queued_at: now.timestamp(),
    };
    // apalis-postgres stores the payload as JSON bytes (its JsonCodec), so
    // encode it the same way the worker decodes it.
    let payload = serde_json::to_vec(&job).map_err(queue_error)?;
    let task = TaskBuilder::new(payload).max_attempts(MAX_ATTEMPTS).build();
    push_tasks(&mut *conn, QUEUE, vec![task])
        .await
        .map_err(queue_error)?;
    tracing::info!(template = template.name(), "mail queued");
    Ok(())
}

/// Every mail job that is not done, oldest due first.
///
/// # Arguments
/// * `pool` - the database.
///
/// # Returns
/// The jobs (pending, sending, retrying and failed), without their bodies.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all)]
pub async fn list(pool: &PgPool) -> Result<Vec<QueuedMail>, OkfError> {
    // `job` is the JSON payload as bytes; `convert_from(..)::jsonb` reads
    // it, and only the recipient and the template's name leave the
    // database. `"to"` is quoted: TO is an SQL keyword.
    Ok(sqlx::query_as::<_, QueuedMail>(concat!(
        "SELECT id, \
                    convert_from(job, 'UTF8')::jsonb -> 'mail' ->> 'to' AS \"to\", \
                    convert_from(job, 'UTF8')::jsonb ->> 'template' AS template, ",
        status_sql!(),
        " AS status, \
                    attempts, last_result ->> 'Err' AS last_error, run_at \
             FROM apalis.jobs WHERE job_type = $1 AND status <> 'Done' \
             ORDER BY run_at, id"
    ))
    .bind(QUEUE)
    .fetch_all(pool)
    .await?)
}

/// Counts the jobs not yet sent, by status. The `cluster_stats`
/// housekeeping job stores the counts in the `cluster_stats` row, which
/// every replica's `okf.mail.queue.depth` gauge reads (M18 spec Decision 16),
/// so a scrape never queries Postgres.
///
/// # Arguments
/// * `pool` - the database.
///
/// # Returns
/// `(status, jobs)` for each status with jobs; a missing status has none.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all)]
pub async fn depth(pool: &PgPool) -> Result<Vec<(MailStatus, i64)>, OkfError> {
    let rows = sqlx::query_as::<_, (String, i64)>(concat!(
        "SELECT ",
        status_sql!(),
        " AS status, count(*) FROM apalis.jobs \
         WHERE job_type = $1 AND status <> 'Done' GROUP BY 1"
    ))
    .bind(QUEUE)
    .fetch_all(pool)
    .await?;
    // `filter_map` drops a status this build does not know (none today).
    Ok(rows
        .into_iter()
        .filter_map(|(status, jobs)| MailStatus::parse(&status).map(|known| (known, jobs)))
        .collect())
}

/// Whether `id` can be a job id (a ULID: ASCII letters and digits). Text
/// that cannot (a NUL, which Postgres refuses as a database error, among
/// others) names no job and never reaches SQL.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
    )
)]
fn is_job_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

/// The error for a job id that names no mail job.
#[tracing::instrument(level = "trace", skip_all)]
fn unknown_job(id: &str) -> OkfError {
    OkfError::UnknownName {
        noun: "mail job",
        name: id.to_string(),
    }
}

/// Explains why a job-id write matched no row: the job is unknown, or in a
/// state the operation does not take.
#[tracing::instrument(level = "debug", skip_all)]
async fn refusal(pool: &PgPool, id: &str, why: &str) -> Result<OkfError, OkfError> {
    let exists: Option<String> =
        sqlx::query_scalar("SELECT status FROM apalis.jobs WHERE id = $1 AND job_type = $2")
            .bind(id)
            .bind(QUEUE)
            .fetch_optional(pool)
            .await?;
    Ok(match exists {
        None => unknown_job(id),
        Some(_) => OkfError::Refused {
            reason: format!("mail job {id} {why}"),
        },
    })
}

/// Puts a failed (or retrying) job back in the queue: due now, no attempts,
/// and a fresh 24-hour window from `now`.
///
/// # Arguments
/// * `pool` - the database.
/// * `id` - the job id from [`list`].
/// * `now` - when (server clock).
///
/// # Errors
/// [`OkfError::UnknownName`] (`noun: "mail job"`); [`OkfError::Refused`]
/// for a job that has not failed; [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all)]
pub async fn retry(pool: &PgPool, id: &str, now: DateTime<Utc>) -> Result<(), OkfError> {
    if !is_job_id(id) {
        return Err(unknown_job(id));
    }
    // `jsonb_set` rewrites `queued_at` inside the stored payload. No NOTIFY
    // fires for an UPDATE; the worker's interval poll finds the job. A row
    // a worker holds is no longer `Failed`, so it is never taken back.
    let updated = sqlx::query(
        "UPDATE apalis.jobs SET status = 'Pending', attempts = 0, run_at = now(), \
             lock_by = NULL, lock_at = NULL, done_at = NULL, last_result = NULL, \
             job = convert_to(jsonb_set(convert_from(job, 'UTF8')::jsonb, \
                                        '{queued_at}', to_jsonb($3::bigint))::text, 'UTF8') \
         WHERE id = $1 AND job_type = $2 AND status IN ('Failed', 'Killed')",
    )
    .bind(id)
    .bind(QUEUE)
    .bind(now.timestamp())
    .execute(pool)
    .await?
    .rows_affected();
    if updated == 0 {
        return Err(refusal(pool, id, "has not failed; only failed mail is retried").await?);
    }
    tracing::info!(job = id, "mail job queued again");
    Ok(())
}

/// Deletes a job that is not being sent right now.
///
/// # Arguments
/// * `pool` - the database.
/// * `id` - the job id from [`list`].
///
/// # Errors
/// [`OkfError::UnknownName`] (`noun: "mail job"`); [`OkfError::Refused`]
/// while a worker holds it; [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all)]
pub async fn delete(pool: &PgPool, id: &str) -> Result<(), OkfError> {
    if !is_job_id(id) {
        return Err(unknown_job(id));
    }
    let deleted = sqlx::query(
        "DELETE FROM apalis.jobs \
         WHERE id = $1 AND job_type = $2 AND status NOT IN ('Queued', 'Running')",
    )
    .bind(id)
    .bind(QUEUE)
    .execute(pool)
    .await?
    .rows_affected();
    if deleted == 0 {
        return Err(refusal(pool, id, "is being sent; try again in a moment").await?);
    }
    tracing::info!(job = id, "mail job deleted");
    Ok(())
}

/// Deletes sent jobs finished over 7 days before `now`, and failed ones
/// over 30 (spec "Secrets in payloads"). Called by the hourly `prune_oauth`
/// housekeeping job ([`crate::housekeeping`]).
///
/// # Arguments
/// * `pool` - the database.
/// * `now` - the server clock.
///
/// # Returns
/// How many were deleted.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all)]
pub async fn prune(pool: &PgPool, now: DateTime<Utc>) -> Result<u64, OkfError> {
    Ok(sqlx::query(
        "DELETE FROM apalis.jobs WHERE job_type = $1 AND ( \
             (status = 'Done' AND done_at < $2) \
          OR ((status = 'Killed' OR (status = 'Failed' AND attempts >= max_attempts)) \
              AND done_at < $3))",
    )
    .bind(QUEUE)
    .bind(now - TimeDelta::days(DONE_RETENTION_DAYS))
    .bind(now - TimeDelta::days(FAILED_RETENTION_DAYS))
    .execute(pool)
    .await?
    .rows_affected())
}

/// Deletes the `apalis.workers` rows of workers that stopped for good: last
/// seen over ten orphan timeouts ago and referenced by no job. Every start
/// and every restart after a database outage registers a new row, and
/// nothing else removes them. A row a job still names (`lock_by`, a foreign
/// key) is kept, whatever the job's state: a `Running` job's worker is
/// alive or about to be declared dead by apalis, and the row of a worker
/// that finished jobs goes once [`prune`] deletes those jobs.
///
/// # Arguments
/// * `pool` - the database.
/// * `now` - the server clock.
///
/// # Returns
/// How many rows were deleted.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all)]
pub async fn prune_workers(pool: &PgPool, now: DateTime<Utc>) -> Result<u64, OkfError> {
    let timeouts =
        TimeDelta::from_std(ORPHAN_TIMEOUT * WORKER_ROW_ORPHAN_TIMEOUTS).map_err(queue_error)?;
    Ok(sqlx::query(
        "DELETE FROM apalis.workers w WHERE w.last_seen < $1 \
         AND NOT EXISTS (SELECT 1 FROM apalis.jobs j WHERE j.lock_by = w.id)",
    )
    .bind(now - timeouts)
    .execute(pool)
    .await?
    .rows_affected())
}

/// What the job handler needs, given to apalis as worker data (cloned per
/// job: every field is a cheap handle).
#[derive(Clone)]
pub(crate) struct MailContext {
    /// The process's [`WorkerPool`]: apalis's LISTEN, fetches,
    /// acks and heartbeats, and the handler's reschedules.
    pub pool: PgPool,
    /// The provider.
    pub mailer: Arc<dyn Mailer>,
    /// The server clock (for the retry window).
    pub clock: Arc<dyn Clock>,
    /// The instruments each job counts on.
    pub metrics: Metrics,
    /// The configured provider, for the job's span and counter.
    pub provider: MailProvider,
}

/// Moves a mail job's `run_at` to now plus its backoff ([`reschedule_job`]).
#[tracing::instrument(level = "debug", skip_all)]
async fn reschedule(pool: &PgPool, id: &str) -> Result<(), OkfError> {
    reschedule_job(pool, QUEUE, id).await
}

/// Moves a job's `run_at` to now plus its backoff: 30 s doubling per
/// attempt so far, capped at an hour. In SQL, on the database's clock,
/// which is the one apalis compares `run_at` with. Shared by every queue on
/// `apalis.jobs` (mail, and feedback since issue #674).
///
/// # Arguments
/// * `pool` - the worker's pool.
/// * `queue` - the job's queue (`apalis.jobs.job_type`).
/// * `id` - the job id.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn reschedule_job(pool: &PgPool, queue: &str, id: &str) -> Result<(), OkfError> {
    // `attempts` is the count before this try is acked (0 on the first
    // failure), so the first retry waits 30 s. `LEAST(attempts, 16)` keeps
    // `power` small; the hour cap is reached at 7 already.
    sqlx::query(
        "UPDATE apalis.jobs SET run_at = now() + make_interval(secs => \
             LEAST($3, $4 * power(2.0::float8, LEAST(attempts, 16)))) \
         WHERE id = $1 AND job_type = $2",
    )
    .bind(id)
    .bind(queue)
    .bind(BACKOFF_MAX_SECS)
    .bind(BACKOFF_FIRST_SECS)
    .execute(pool)
    .await?;
    Ok(())
}

/// The job handler: one send, then Decision 11's outcome (see the module
/// doc). apalis fills each argument from the job: the payload, the worker
/// data and the job id.
///
/// Each job is a root `okf.mail.job` span (`parent = None`: a job started
/// by the worker has no request to belong to) carrying the template's
/// name, the provider and the outcome; never the address.
#[tracing::instrument(
    level = "info",
    name = "okf.mail.job",
    parent = None,
    skip_all,
    fields(
        okf.mail.template = %job.template,
        okf.mail.provider = context.provider.as_str(),
        okf.outcome = tracing::field::Empty,
    )
)]
async fn send_mail(
    job: SendMail,
    context: Data<MailContext>,
    id: TaskId,
) -> Result<(), BoxDynError> {
    let (result, outcome) = deliver(&job, &context, &id.to_string()).await;
    tracing::Span::current().record("okf.outcome", outcome.as_str());
    context.metrics.mail_job(context.provider, outcome);
    result
}

/// One try at `job`: the send, and what to tell apalis.
///
/// # Returns
/// The handler's answer, and how the try ended for the span and counter.
#[tracing::instrument(level = "debug", skip_all)]
async fn deliver(
    job: &SendMail,
    context: &MailContext,
    id: &str,
) -> (Result<(), BoxDynError>, MailOutcome) {
    let id = id.to_string();
    let error = match context.mailer.send(&job.mail).await {
        Ok(()) => {
            tracing::info!(job = %id, template = %job.template, "mail sent");
            return (Ok(()), MailOutcome::Sent);
        }
        Err(error) => error,
    };
    // `Box::new(...)` makes the boxed error trait object apalis expects;
    // `AbortError` tells it never to fetch the job again.
    match error {
        MailError::Permanent(_) => {
            split_event!(
                warn,
                { job = %id, template = %job.template, },
                { %error, },
                "mail refused for good"
            );
            (Err(Box::new(AbortError::new(error))), MailOutcome::Failed)
        }
        MailError::Retryable(reason) => {
            let age = context.clock.now().timestamp() - job.queued_at;
            if age >= RETRY_WINDOW_HOURS * 60 * 60 {
                split_event!(
                    warn,
                    { job = %id, template = %job.template, },
                    { %reason, },
                    "mail given up"
                );
                let abort = AbortError::new(MailError::Retryable(format!(
                    "gave up after {RETRY_WINDOW_HOURS} hours: {reason}"
                )));
                return (Err(Box::new(abort)), MailOutcome::Failed);
            }
            if let Err(error) = reschedule(&context.pool, &id).await {
                // Still a plain error: apalis fetches it again, just sooner.
                split_event!(
                    error,
                    { job = %id, },
                    { %error, },
                    "rescheduling a mail job failed"
                );
            }
            split_event!(
                info,
                { job = %id, template = %job.template, },
                { %reason, },
                "mail failed; will retry"
            );
            (
                Err(Box::new(MailError::Retryable(reason))),
                MailOutcome::Retry,
            )
        }
    }
}

/// This process's worker name: unique per host and process, so replicas
/// never share an `apalis.workers` row (Decision 11).
///
/// # Returns
/// `okf-mail-<host>-<pid>`.
#[tracing::instrument(level = "trace", skip_all)]
fn worker_name() -> String {
    process_worker_name("okf-mail")
}

/// A worker name unique per host and process: `<prefix>-<host>-<pid>`, so
/// replicas never share an `apalis.workers` row.
///
/// # Arguments
/// * `prefix` - the queue's worker prefix (`okf-mail`, `okf-feedback`).
///
/// # Returns
/// The name.
#[tracing::instrument(level = "trace", skip_all)]
pub(crate) fn process_worker_name(prefix: &str) -> String {
    // `$HOSTNAME`, else the kernel's name for the host, else a placeholder.
    let host = std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/proc/sys/kernel/hostname").ok())
        .map(|host| host.trim().to_string())
        .filter(|host| !host.is_empty())
        .unwrap_or_else(|| "host".to_string());
    format!("{prefix}-{host}-{}", std::process::id())
}

/// The worker's own pool: the same database and credentials as `pool`, at
/// most [`WORKER_POOL_SIZE`] connections, named [`WORKER_APPLICATION_NAME`].
/// It connects lazily, on the worker's first query.
///
/// # Arguments
/// * `pool` - the request pool, whose connect options are copied.
///
/// # Returns
/// A new pool; closing it leaves `pool` untouched.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) fn worker_pool(pool: &PgPool) -> PgPool {
    worker_pool_named(pool, WORKER_APPLICATION_NAME, WORKER_POOL_SIZE)
}

/// A worker's own pool: the same database and credentials as `pool`, at
/// most `size` connections, named `application_name`. It connects lazily.
///
/// # Arguments
/// * `pool` - the request pool, whose connect options are copied.
/// * `application_name` - what `pg_stat_activity` shows for it.
/// * `size` - its most connections.
///
/// # Returns
/// A new pool; closing it leaves `pool` untouched.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) fn worker_pool_named(pool: &PgPool, application_name: &str, size: u32) -> PgPool {
    // `connect_options()` is shared (`Arc`); `as_ref().clone()` copies it so
    // the name can be changed for this pool only.
    let options = pool
        .connect_options()
        .as_ref()
        .clone()
        .application_name(application_name);
    PgPoolOptions::new()
        .max_connections(size)
        .connect_lazy_with(options)
}

/// How often a worker polls, heartbeats and restarts. Tests shorten them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WorkerTiming {
    /// How often to look for due jobs no NOTIFY announced.
    pub poll: Duration,
    /// How often to heartbeat (and re-enqueue jobs of dead workers).
    pub heartbeat: Duration,
    /// How long to wait before restarting after a backend error.
    pub restart: Duration,
    /// How many heartbeats may be missed before a worker's jobs are
    /// re-enqueued.
    pub missed_heartbeats: usize,
}

impl WorkerTiming {
    /// The timing `serve` uses.
    pub(crate) const SERVE: Self = Self {
        poll: POLL_INTERVAL,
        heartbeat: HEARTBEAT_INTERVAL,
        restart: RESTART_DELAY,
        missed_heartbeats: MISSED_HEARTBEATS,
    };
}

/// Starts the mail worker for a serving process (`http::serve` calls it on
/// every replica, unless a test turns it off through `ServeOptions`).
///
/// # Arguments
/// * `app` - the shared state (its mailer and clock).
/// * `pool` - the process's worker pool; it stays open when the worker ends.
/// * `shutdown` - cancelled when the server stops.
///
/// # Returns
/// The worker's task; it ends once `shutdown` is cancelled and the job in
/// hand (if any) is finished.
#[tracing::instrument(level = "debug", skip_all)]
pub fn spawn_worker(
    app: Arc<App>,
    pool: &WorkerPool,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    let context = MailContext {
        pool: pool.pool.clone(),
        mailer: Arc::clone(app.mailer()),
        clock: app.clock_handle(),
        metrics: app.metrics().clone(),
        provider: app.config().mail.provider,
    };
    run_worker(context, worker_name(), WorkerTiming::SERVE, shutdown)
}

/// Runs one apalis worker on [`QUEUE`] until `shutdown`, restarting it
/// after `timing.restart` if it stops on an error (a lost database, say).
/// It leaves `context.pool` open: the pool's owner closes it.
///
/// # Arguments
/// * `context` - what the handler needs; its pool is the process's
///   [`WorkerPool`].
/// * `name` - the worker's name ([`worker_name`]); a restart registers as
///   `<name>-<restarts>`, a row of its own, so apalis's session-scoped lock
///   on the old row can never refuse it.
/// * `timing` - the poll, heartbeat and restart delays.
/// * `shutdown` - cancelled to stop.
///
/// # Returns
/// The worker's task.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) fn run_worker(
    context: MailContext,
    name: String,
    timing: WorkerTiming,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        run_until_cancelled(&context, &name, timing, &shutdown).await;
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
    context: &MailContext,
    name: &str,
    timing: WorkerTiming,
    shutdown: &CancellationToken,
) {
    // `0..`: an endless range, counting the restarts.
    for restarts in 0_u64.. {
        let config = QueueConfig::default()
            .queue(QUEUE)
            .heartbeat_interval(timing.heartbeat)
            .missed_heartbeats(timing.missed_heartbeats);
        let backend = PostgresStorage::<SendMail>::new(&context.pool)
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
            .build(send_mail);
        let stop = shutdown.clone();
        // The future apalis waits on: it resolves when we are cancelled.
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
                    "the mail worker stopped; restarting it soon"
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
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use crate::clock::FixedClock;
    use crate::db::test_db;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-12T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    /// A mailer that answers from a script (then `Ok` once it runs out) and
    /// remembers what it was asked to send.
    #[derive(Default)]
    struct Scripted {
        answers: Mutex<VecDeque<Result<(), MailError>>>,
        sent: Mutex<Vec<Mail>>,
        delay: Duration,
    }

    impl Scripted {
        fn with(answers: Vec<Result<(), MailError>>) -> Arc<Self> {
            Arc::new(Self {
                answers: Mutex::new(answers.into()),
                ..Self::default()
            })
        }

        fn sent(&self) -> usize {
            self.sent.lock().unwrap().len()
        }
    }

    #[async_trait::async_trait]
    impl Mailer for Scripted {
        async fn send(&self, mail: &Mail) -> Result<(), MailError> {
            tokio::time::sleep(self.delay).await;
            self.sent.lock().unwrap().push(mail.clone());
            self.answers.lock().unwrap().pop_front().unwrap_or(Ok(()))
        }
    }

    /// Fast timing for tests: polls every 100 ms, heartbeats every second
    /// and restarts after 200 ms.
    const FAST: WorkerTiming = WorkerTiming {
        poll: Duration::from_millis(100),
        heartbeat: Duration::from_secs(1),
        restart: Duration::from_millis(200),
        missed_heartbeats: MISSED_HEARTBEATS,
    };

    /// Runs one worker named `name` on `own_pool` (a [`worker_pool`], as
    /// `serve`'s is), with [`FAST`] timing, until the returned token is
    /// cancelled. The test keeps a clone of the pool to [`starve`] it.
    fn start_on(
        own_pool: PgPool,
        mailer: Arc<Scripted>,
        name: &str,
    ) -> (CancellationToken, JoinHandle<()>) {
        let context = context_with(own_pool, mailer, Metrics::global(), MailProvider::Log);
        start_context(context, name)
    }

    fn context_with(
        pool: PgPool,
        mailer: Arc<Scripted>,
        metrics: Metrics,
        provider: MailProvider,
    ) -> MailContext {
        MailContext {
            pool,
            mailer,
            clock: Arc::new(FixedClock(now())),
            metrics,
            provider,
        }
    }

    fn start_context(context: MailContext, name: &str) -> (CancellationToken, JoinHandle<()>) {
        let shutdown = CancellationToken::new();
        let pool = context.pool.clone();
        let worker = run_worker(context, name.to_string(), FAST, shutdown.clone());
        // `run_worker` leaves the pool to its owner (here, the test).
        let closed = tokio::spawn(async move {
            let _ = worker.await;
            pool.close().await;
        });
        (shutdown, closed)
    }

    fn start(pool: &PgPool, mailer: Arc<Scripted>) -> (CancellationToken, JoinHandle<()>) {
        start_on(worker_pool(pool), mailer, &worker_name())
    }

    /// Holds every connection of a worker's pool that its LISTEN does not
    /// (the listener keeps one for good), so the worker can neither claim
    /// nor finish a job until the returned connections are dropped. Its
    /// LISTEN still hears every insert.
    async fn starve(own_pool: &PgPool) -> Vec<sqlx::pool::PoolConnection<sqlx::Postgres>> {
        let mut held = Vec::with_capacity(WORKER_POOL_SIZE as usize - 1);
        // `1..WORKER_POOL_SIZE`: one fewer than the pool's size, the LISTEN's.
        for _ in 1..WORKER_POOL_SIZE {
            let connection = tokio::time::timeout(Duration::from_secs(10), own_pool.acquire())
                .await
                .expect("a worker connection frees up within 10 s")
                .unwrap();
            held.push(connection);
        }
        held
    }

    /// Waits (up to 10 s) until `count` worker sessions on this test's
    /// database are LISTENing.
    async fn wait_listening(pool: &PgPool, count: i64) {
        let mut listening = 0_i64;
        for _ in 0..100 {
            listening = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity \
                 WHERE datname = current_database() AND application_name = $1 \
                 AND query LIKE 'LISTEN %'",
            )
            .bind(WORKER_APPLICATION_NAME)
            .fetch_one(pool)
            .await
            .unwrap();
            if listening == count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("{listening} workers are listening, not {count}");
    }

    /// Enqueues mail to `to` and waits (up to 10 s) for `worker` to claim it.
    async fn enqueue_claimed_by(pool: &PgPool, to: &str, worker: &str) {
        enqueue(pool, to, &Template::Test, now(), &Brand::default())
            .await
            .unwrap();
        let mut claimed_by: Option<String> = None;
        for _ in 0..100 {
            // `convert_from(job, 'UTF8')`: the payload is JSON bytes.
            claimed_by = sqlx::query_scalar(
                "SELECT lock_by FROM apalis.jobs \
                 WHERE convert_from(job, 'UTF8')::jsonb #>> '{mail,to}' = $1",
            )
            .bind(to)
            .fetch_one(pool)
            .await
            .unwrap();
            if claimed_by.as_deref() == Some(worker) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("mail to {to} was claimed by {claimed_by:?}, not {worker}");
    }

    /// Stops a worker and waits for it.
    async fn stop(shutdown: CancellationToken, worker: JoinHandle<()>) {
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(20), worker)
            .await
            .expect("the worker stops within 20 s")
            .unwrap();
    }

    /// The one job's `(status, attempts)`.
    async fn state(pool: &PgPool) -> (String, i32) {
        sqlx::query_as("SELECT status, attempts FROM apalis.jobs WHERE job_type = $1")
            .bind(QUEUE)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// Waits (up to 20 s) for the one job to reach `status` with `attempts`.
    async fn wait_for(pool: &PgPool, status: &str, attempts: i32) {
        for _ in 0..200 {
            if state(pool).await == (status.to_string(), attempts) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!(
            "the job is {:?}, not ({status}, {attempts})",
            state(pool).await
        );
    }

    /// Seconds from the database's now to the one job's `run_at`.
    async fn wait_left(pool: &PgPool) -> f64 {
        sqlx::query_scalar("SELECT extract(epoch FROM run_at - now())::float8 FROM apalis.jobs")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// Makes the one job due now, as if its backoff had passed.
    async fn make_due(pool: &PgPool) {
        sqlx::query(
            "UPDATE apalis.jobs SET run_at = now() - interval '1 second' WHERE job_type = $1",
        )
        .bind(QUEUE)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn only_id(pool: &PgPool) -> String {
        sqlx::query_scalar("SELECT id FROM apalis.jobs WHERE job_type = $1")
            .bind(QUEUE)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_enqueued_mail_is_listed_without_its_body() {
        let db = test_db().await;
        let template = Template::Reset {
            link: crate::mail::link("https://okf.example.com", crate::mail::RESET_PATH, "s3cr3t"),
        };

        enqueue(
            &db.pool,
            "alice@example.com",
            &template,
            now(),
            &Brand::default(),
        )
        .await
        .unwrap();

        let queued = list(&db.pool).await.unwrap();
        assert_eq!(queued.len(), 1);
        let job = &queued[0];
        assert_eq!(
            (
                job.to.as_str(),
                job.template.as_str(),
                job.status.as_str(),
                job.attempts
            ),
            ("alice@example.com", "reset", "pending", 0)
        );
        assert_eq!(job.last_error, None);
        assert!(!format!("{queued:?}").contains("s3cr3t"));
        let (max, queued_at): (i32, i64) = sqlx::query_as(
            "SELECT max_attempts, (convert_from(job, 'UTF8')::jsonb ->> 'queued_at')::bigint \
             FROM apalis.jobs",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!((max, queued_at), (64, now().timestamp()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_malformed_recipient_is_refused_before_it_is_queued() {
        let db = test_db().await;

        let refused = enqueue(
            &db.pool,
            "alice@example.com, bob@example.com",
            &Template::Test,
            now(),
            &Brand::default(),
        )
        .await;

        assert!(matches!(
            refused,
            Err(OkfError::LimitExceeded { limit: "email", .. })
        ));
        assert!(list(&db.pool).await.unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_mail_is_rendered_with_the_brand_stored_when_it_is_queued() {
        // The subject a queued mail to `to` was rendered with.
        async fn stored_subject(pool: &PgPool, to: &str) -> String {
            sqlx::query_scalar::<_, String>(
                "SELECT convert_from(job, 'UTF8')::jsonb -> 'mail' ->> 'subject' \
                 FROM apalis.jobs \
                 WHERE convert_from(job, 'UTF8')::jsonb -> 'mail' ->> 'to' = $1",
            )
            .bind(to)
            .fetch_one(pool)
            .await
            .unwrap()
        }
        let db = test_db().await;
        enqueue(
            &db.pool,
            "alice@example.com",
            &Template::Test,
            now(),
            &Brand::default(),
        )
        .await
        .unwrap();
        let before = stored_subject(&db.pool, "alice@example.com").await;

        settings::set(
            &db.pool,
            settings::SettingKey::ProductName,
            "Acme Notes",
            &crate::store::audit::AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();
        // Inside a caller's transaction too.
        let mut tx = db.pool.begin().await.unwrap();
        enqueue_in(
            &mut tx,
            "bob@example.com",
            &Template::Test,
            now(),
            &Brand::default(),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();

        assert_eq!(before, "okf test email");
        assert_eq!(
            stored_subject(&db.pool, "bob@example.com").await,
            "Acme Notes test email"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_worker_sends_a_queued_mail_once() {
        let db = test_db().await;
        let mailer = Scripted::with(vec![]);
        let (shutdown, worker) = start(&db.pool, Arc::clone(&mailer));
        let link = crate::mail::link("https://okf.example.com", crate::mail::SETUP_PATH, "tok");

        let template = Template::Setup {
            link: link.clone(),
            user: "alice".to_string(),
        };
        enqueue(
            &db.pool,
            "alice@example.com",
            &template,
            now(),
            &Brand::default(),
        )
        .await
        .unwrap();
        wait_for(&db.pool, "Done", 1).await;
        stop(shutdown, worker).await;
        let listed = list(&db.pool).await.unwrap();

        let sent = mailer.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].to, "alice@example.com");
        assert_eq!(sent[0].subject, "Set up your okf account");
        assert!(sent[0].text.contains(&link), "the link survives the queue");
        assert!(sent[0].html.contains(&link));
        assert!(listed.is_empty(), "done jobs are not listed");
    }

    /// Waits (up to 10 s) until `series` renders as `want`.
    async fn wait_for_series(registry: &prometheus::Registry, series: &str, want: f64) {
        use crate::mcp::testing::{render, sample_of};
        for _ in 0..100 {
            if sample_of(&render(registry), series) == Some(want) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("{series} never reached {want}:\n{}", render(registry));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mail_jobs_count_by_provider_and_outcome() {
        use crate::mcp::testing::{local_metrics, render, sample_of};
        let (_provider, metrics, registry) = local_metrics();
        let db = test_db().await;
        let mailer = Scripted::with(vec![
            Err(MailError::Retryable("connection reset".to_string())),
            Ok(()),
            Err(MailError::Permanent("no such mailbox".to_string())),
        ]);
        let context = context_with(
            worker_pool(&db.pool),
            Arc::clone(&mailer),
            metrics,
            MailProvider::Smtp,
        );
        let (shutdown, worker) = start_context(context, &worker_name());

        enqueue(
            &db.pool,
            "alice@example.com",
            &Template::Test,
            now(),
            &Brand::default(),
        )
        .await
        .unwrap();
        wait_for_series(
            &registry,
            "okf_mail_jobs_total{outcome=\"retry\",provider=\"smtp\"}",
            1.0,
        )
        .await;
        make_due(&db.pool).await;
        wait_for_series(
            &registry,
            "okf_mail_jobs_total{outcome=\"sent\",provider=\"smtp\"}",
            1.0,
        )
        .await;
        enqueue(
            &db.pool,
            "bob@example.com",
            &Template::Test,
            now(),
            &Brand::default(),
        )
        .await
        .unwrap();
        wait_for_series(
            &registry,
            "okf_mail_jobs_total{outcome=\"failed\",provider=\"smtp\"}",
            1.0,
        )
        .await;
        stop(shutdown, worker).await;

        let text = render(&registry);
        assert_eq!(
            sample_of(
                &text,
                "okf_mail_jobs_total{outcome=\"retry\",provider=\"smtp\"}"
            ),
            Some(1.0),
            "one try per outcome, none of them under another provider:\n{text}"
        );
        for secret in ["alice", "bob", "example.com"] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn each_mail_job_is_its_own_trace() {
        use crate::telemetry::config::LogFormat;
        use crate::telemetry::subscriber::capture::observe;
        use opentelemetry::trace::SpanId;

        let db = test_db().await;
        let mailer = Scripted::with(vec![]);
        let context = context_with(
            db.pool.clone(),
            mailer,
            Metrics::global(),
            MailProvider::Postmark,
        );
        let job = || SendMail {
            mail: Mail {
                to: "alice@example.com".to_string(),
                subject: "subject".to_string(),
                text: "body".to_string(),
                html: "<p>body</p>".to_string(),
            },
            template: "test".to_string(),
            queued_at: now().timestamp(),
        };

        // Both jobs run inside a caller's span: a job must not join it.
        let observed = tokio::task::block_in_place(|| {
            observe("trace", "trace", "trace", LogFormat::Text, || {
                let caller = tracing::info_span!("caller");
                let _entered = caller.enter();
                for number in 1..=2 {
                    let answered = tokio::runtime::Handle::current().block_on(send_mail(
                        job(),
                        Data::new(context.clone()),
                        TaskId::from_int(number),
                    ));
                    assert!(answered.is_ok());
                }
            })
        });

        let jobs: Vec<_> = observed
            .spans
            .iter()
            .filter(|span| span.name == "okf.mail.job")
            .collect();
        assert_eq!(jobs.len(), 2, "{:?}", observed.spans);
        let caller = observed
            .spans
            .iter()
            .find(|span| span.name == "caller")
            .expect("the caller's span");
        for job in &jobs {
            assert_eq!(job.parent_span_id, SpanId::INVALID, "a root span");
            assert_ne!(job.span_context.trace_id(), caller.span_context.trace_id());
            let attribute = |name: &str| {
                job.attributes
                    .iter()
                    .find(|attribute| attribute.key.as_str() == name)
                    .map(|attribute| attribute.value.to_string())
            };
            assert_eq!(attribute("okf.mail.template").as_deref(), Some("test"));
            assert_eq!(attribute("okf.mail.provider").as_deref(), Some("postmark"));
            assert_eq!(attribute("okf.outcome").as_deref(), Some("sent"));
        }
        assert_ne!(
            jobs[0].span_context.trace_id(),
            jobs[1].span_context.trace_id()
        );
        let exported = observed.exported_text();
        for secret in ["alice", "example.com"] {
            assert!(!exported.contains(secret), "{secret} in {exported}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn depth_counts_the_unsent_jobs_by_status() {
        let db = test_db().await;
        for to in ["a@example.com", "b@example.com", "c@example.com"] {
            enqueue(&db.pool, to, &Template::Test, now(), &Brand::default())
                .await
                .unwrap();
        }
        sqlx::query(
            "UPDATE apalis.jobs SET status = 'Killed' \
             WHERE id = (SELECT id FROM apalis.jobs WHERE job_type = $1 ORDER BY id LIMIT 1)",
        )
        .bind(QUEUE)
        .execute(&db.pool)
        .await
        .unwrap();
        // A job of another queue is not mail.
        sqlx::query(
            "INSERT INTO apalis.jobs (id, job_type, job, status, attempts, max_attempts, run_at) \
             VALUES ('other', 'housekeeping', convert_to('{}', 'UTF8'), 'Pending', 0, 3, now())",
        )
        .execute(&db.pool)
        .await
        .unwrap();

        let mut counted = depth(&db.pool).await.unwrap();
        counted.sort_by_key(|(status, _)| status.as_str());

        assert_eq!(
            counted,
            [(MailStatus::Failed, 1), (MailStatus::Pending, 2)],
            "Done is not counted; a missing status has no row"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_provider_error_reschedules_with_doubling_backoff_then_the_retry_succeeds() {
        let db = test_db().await;
        let mailer = Scripted::with(vec![
            Err(MailError::Retryable("connection reset".to_string())),
            Err(MailError::Retryable("timed out".to_string())),
        ]);
        let (shutdown, worker) = start(&db.pool, Arc::clone(&mailer));

        enqueue(
            &db.pool,
            "alice@example.com",
            &Template::Test,
            now(),
            &Brand::default(),
        )
        .await
        .unwrap();
        wait_for(&db.pool, "Failed", 1).await;
        let first_wait = wait_left(&db.pool).await;
        let listed = list(&db.pool).await.unwrap();
        // Several polls pass; the job is not due, so nobody takes it.
        tokio::time::sleep(Duration::from_millis(600)).await;
        let sent_while_waiting = mailer.sent();
        let state_while_waiting = state(&db.pool).await;
        make_due(&db.pool).await;
        wait_for(&db.pool, "Failed", 2).await;
        let second_wait = wait_left(&db.pool).await;
        let listed_again = list(&db.pool).await.unwrap();
        make_due(&db.pool).await;
        wait_for(&db.pool, "Done", 3).await;
        stop(shutdown, worker).await;

        assert!(
            (20.0..=30.0).contains(&first_wait),
            "the first retry waits 30 s: {first_wait}"
        );
        assert!(
            (50.0..=60.0).contains(&second_wait),
            "the second waits 60 s: {second_wait}"
        );
        assert_eq!(
            (sent_while_waiting, state_while_waiting),
            (1, ("Failed".to_string(), 1))
        );
        assert_eq!(listed[0].status, "retrying");
        assert!(
            listed[0]
                .last_error
                .as_deref()
                .is_some_and(|e| e.contains("connection reset")),
            "{listed:?}"
        );
        assert_eq!(listed_again[0].attempts, 2);
        assert!(
            listed_again[0]
                .last_error
                .as_deref()
                .is_some_and(|e| e.contains("timed out")),
            "{listed_again:?}"
        );
        assert_eq!(mailer.sent(), 3);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_permanent_error_fails_at_once_and_retry_from_the_queue_sends_it() {
        let db = test_db().await;
        let mailer = Scripted::with(vec![Err(MailError::Permanent(
            "422 invalid To".to_string(),
        ))]);
        let (shutdown, worker) = start(&db.pool, Arc::clone(&mailer));

        enqueue(
            &db.pool,
            "alice@example.com",
            &Template::Test,
            now(),
            &Brand::default(),
        )
        .await
        .unwrap();
        wait_for(&db.pool, "Killed", 1).await;
        let listed = list(&db.pool).await.unwrap();
        // Killed is final: polls pass and nobody takes it again.
        tokio::time::sleep(Duration::from_millis(600)).await;
        let sent_before_retry = mailer.sent();
        retry(&db.pool, &only_id(&db.pool).await, now())
            .await
            .unwrap();
        wait_for(&db.pool, "Done", 1).await;
        stop(shutdown, worker).await;

        assert_eq!(listed[0].status, "failed");
        assert!(listed[0]
            .last_error
            .as_deref()
            .is_some_and(|e| e.contains("422 invalid To")));
        assert_eq!(sent_before_retry, 1);
        assert_eq!(mailer.sent(), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_mail_still_failing_after_24_hours_gives_up_and_retry_gives_a_new_window() {
        let db = test_db().await;
        let mailer = Scripted::with(vec![
            Err(MailError::Retryable("timed out".to_string())),
            Err(MailError::Retryable("timed out again".to_string())),
        ]);
        let (shutdown, worker) = start(&db.pool, Arc::clone(&mailer));

        // Queued 25 hours before the worker's clock.
        enqueue(
            &db.pool,
            "alice@example.com",
            &Template::Test,
            now() - TimeDelta::hours(25),
            &Brand::default(),
        )
        .await
        .unwrap();
        wait_for(&db.pool, "Killed", 1).await;
        let given_up = list(&db.pool).await.unwrap();
        tokio::time::sleep(Duration::from_millis(600)).await;
        let sent_after_giving_up = mailer.sent();
        retry(&db.pool, &only_id(&db.pool).await, now())
            .await
            .unwrap();
        wait_for(&db.pool, "Failed", 1).await;
        let retried = list(&db.pool).await.unwrap();
        stop(shutdown, worker).await;

        assert_eq!(given_up[0].status, "failed");
        assert!(
            given_up[0]
                .last_error
                .as_deref()
                .is_some_and(|e| e.contains("24 hours") && e.contains("timed out")),
            "{given_up:?}"
        );
        assert_eq!(sent_after_giving_up, 1, "no second try");
        assert_eq!(retried[0].status, "retrying", "a fresh window: {retried:?}");
        assert_eq!(mailer.sent(), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn two_workers_never_send_the_same_mail_twice() {
        let db = test_db().await;
        let mailer = Arc::new(Scripted {
            delay: Duration::from_millis(20),
            ..Scripted::default()
        });
        let recipients: Vec<String> = (0..30).map(|n| format!("user{n}@example.com")).collect();
        // Two replicas: one queue, two worker names, both listening before
        // the mail arrives, so every NOTIFY wakes both.
        let pool_a = worker_pool(&db.pool);
        let pool_b = worker_pool(&db.pool);
        let (stop_a, worker_a) = start_on(pool_a.clone(), Arc::clone(&mailer), "okf-mail-a-1");
        let (stop_b, worker_b) = start_on(pool_b.clone(), Arc::clone(&mailer), "okf-mail-b-2");
        wait_listening(&db.pool, 2).await;
        // apalis claims up to ten jobs a fetch, so either worker can win
        // every race (issue #335). Give each one a mail of its own first,
        // by starving the other of connections while that mail is claimed.
        // `drop` returns the held connections to the pool at once, as
        // leaving a `with` block would in Python.
        let held_b = starve(&pool_b).await;
        enqueue_claimed_by(&db.pool, &recipients[0], "okf-mail-a-1").await;
        drop(held_b);
        let held_a = starve(&pool_a).await;
        enqueue_claimed_by(&db.pool, &recipients[1], "okf-mail-b-2").await;
        drop(held_a);
        // The rest arrive with both workers free to race for each one.
        for to in &recipients[2..] {
            enqueue(&db.pool, to, &Template::Test, now(), &Brand::default())
                .await
                .unwrap();
        }
        let mut done = 0_i64;
        for _ in 0..200 {
            done = sqlx::query_scalar("SELECT count(*) FROM apalis.jobs WHERE status = 'Done'")
                .fetch_one(&db.pool)
                .await
                .unwrap();
            if done == 30 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        stop(stop_a, worker_a).await;
        stop(stop_b, worker_b).await;

        assert_eq!(done, 30);
        let mut sent: Vec<String> = mailer
            .sent
            .lock()
            .unwrap()
            .iter()
            .map(|mail| mail.to.clone())
            .collect();
        sent.sort();
        let mut expected = recipients.clone();
        expected.sort();
        assert_eq!(sent, expected, "each mail exactly once");
        let by_worker: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT lock_by FROM apalis.jobs WHERE lock_by IS NOT NULL ORDER BY 1",
        )
        .fetch_all(&db.pool)
        .await
        .unwrap();
        assert_eq!(
            by_worker,
            ["okf-mail-a-1", "okf-mail-b-2"],
            "both workers took mail"
        );
    }

    /// A worker that dies without shutting down (its task is aborted, as a
    /// killed pod's would be) loses its mail to another worker once its
    /// heartbeat stops: `missed_heartbeats` (6) times the heartbeat. Timing
    /// is production's, scaled from 10 s to 1 s, so the orphan timeout is
    /// 6 s; the wait is bounded at 60 s because load on the machine delays
    /// the surviving worker's heartbeat, which does the re-enqueue.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_dead_workers_job_returns_within_a_minute() {
        let db = test_db().await;
        // The first worker's send never finishes: it dies holding the mail.
        let stuck = Arc::new(Scripted {
            delay: Duration::from_secs(3600),
            ..Scripted::default()
        });
        let context = context_with(
            worker_pool(&db.pool),
            Arc::clone(&stuck),
            Metrics::global(),
            MailProvider::Log,
        );
        // `run_worker`'s own handle, not `start_context`'s wrapper: aborting
        // the wrapper would leave the worker task running.
        let dead = run_worker(
            context,
            "okf-mail-dead".to_string(),
            FAST,
            CancellationToken::new(),
        );
        wait_listening(&db.pool, 1).await;
        enqueue_claimed_by(&db.pool, "alice@example.com", "okf-mail-dead").await;
        wait_for(&db.pool, "Running", 0).await;
        // Kill it: no shutdown signal, no ack, no further heartbeat.
        dead.abort();
        let _ = dead.await;

        let rescuer = Scripted::with(vec![]);
        let (stop_live, live) =
            start_on(worker_pool(&db.pool), Arc::clone(&rescuer), "okf-mail-live");
        let sent = tokio::time::timeout(Duration::from_secs(60), async {
            while rescuer.sent() == 0 {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await;
        stop(stop_live, live).await;

        assert!(
            sent.is_ok(),
            "another worker sent the dead worker's mail within 60 s"
        );
        assert_eq!(rescuer.sent(), 1);
        assert_eq!(stuck.sent(), 0, "the dead worker never finished its send");
        assert_eq!(state(&db.pool).await.0, "Done");
    }

    /// Inserts an `apalis.workers` row last seen `age` before `now()`.
    async fn insert_worker(pool: &PgPool, id: &str, age: TimeDelta) {
        sqlx::query(
            "INSERT INTO apalis.workers (id, worker_type, storage_name, layers, last_seen) \
             VALUES ($1, $2, 'PostgresStorage', '', $3)",
        )
        .bind(id)
        .bind(QUEUE)
        .bind(now() - age)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stale_worker_rows_are_pruned_and_live_ones_kept() {
        let db = test_db().await;
        insert_worker(&db.pool, "stale", TimeDelta::hours(1)).await;
        insert_worker(&db.pool, "holds-a-job", TimeDelta::hours(1)).await;
        insert_worker(&db.pool, "recent", TimeDelta::seconds(30)).await;
        enqueue(
            &db.pool,
            "alice@example.com",
            &Template::Test,
            now(),
            &Brand::default(),
        )
        .await
        .unwrap();
        sqlx::query("UPDATE apalis.jobs SET status = 'Running', lock_by = 'holds-a-job'")
            .execute(&db.pool)
            .await
            .unwrap();

        let pruned = prune_workers(&db.pool, now()).await.unwrap();

        assert_eq!(pruned, 1);
        let left: Vec<String> = sqlx::query_scalar("SELECT id FROM apalis.workers ORDER BY id")
            .fetch_all(&db.pool)
            .await
            .unwrap();
        assert_eq!(left, ["holds-a-job", "recent"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stale_worker_row_that_finished_jobs_goes_once_the_jobs_do() {
        let db = test_db().await;
        insert_worker(&db.pool, "finished", TimeDelta::days(9)).await;
        enqueue(
            &db.pool,
            "alice@example.com",
            &Template::Test,
            now(),
            &Brand::default(),
        )
        .await
        .unwrap();
        sqlx::query(
            "UPDATE apalis.jobs SET status = 'Done', lock_by = 'finished', \
             done_at = $1 - interval '8 days'",
        )
        .bind(now())
        .execute(&db.pool)
        .await
        .unwrap();

        // The foreign key from the job keeps the row for now (and the
        // delete must not fail on it).
        assert_eq!(prune_workers(&db.pool, now()).await.unwrap(), 0);
        assert_eq!(prune(&db.pool, now()).await.unwrap(), 1);
        assert_eq!(prune_workers(&db.pool, now()).await.unwrap(), 1);
    }

    #[test]
    fn the_orphan_timeout_is_one_minute() {
        assert_eq!(ORPHAN_TIMEOUT, Duration::from_secs(60));
        assert_eq!(
            WorkerTiming::SERVE.heartbeat * WorkerTiming::SERVE.missed_heartbeats as u32,
            ORPHAN_TIMEOUT
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_finishes_the_mail_in_hand() {
        let db = test_db().await;
        let mailer = Arc::new(Scripted {
            delay: Duration::from_millis(1500),
            ..Scripted::default()
        });
        let (shutdown, worker) = start(&db.pool, Arc::clone(&mailer));
        enqueue(
            &db.pool,
            "alice@example.com",
            &Template::Test,
            now(),
            &Brand::default(),
        )
        .await
        .unwrap();
        wait_for(&db.pool, "Running", 0).await;

        stop(shutdown, worker).await;

        assert_eq!(mailer.sent(), 1);
        assert_eq!(state(&db.pool).await, ("Done".to_string(), 1));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn retry_and_delete_refuse_unknown_and_unfinished_jobs() {
        let db = test_db().await;
        enqueue(
            &db.pool,
            "alice@example.com",
            &Template::Test,
            now(),
            &Brand::default(),
        )
        .await
        .unwrap();
        let id = only_id(&db.pool).await;

        let pending_retry = retry(&db.pool, &id, now()).await;
        let unknown_retry = retry(&db.pool, "no-such-job", now()).await;
        let unknown_delete = delete(&db.pool, "no-such-job").await;
        sqlx::query("UPDATE apalis.jobs SET status = 'Running'")
            .execute(&db.pool)
            .await
            .unwrap();
        let running_delete = delete(&db.pool, &id).await;
        sqlx::query("UPDATE apalis.jobs SET status = 'Pending'")
            .execute(&db.pool)
            .await
            .unwrap();
        delete(&db.pool, &id).await.unwrap();

        assert!(
            matches!(pending_retry, Err(OkfError::Refused { .. })),
            "{pending_retry:?}"
        );
        assert!(matches!(
            unknown_retry,
            Err(OkfError::UnknownName {
                noun: "mail job",
                ..
            })
        ));
        assert!(matches!(
            unknown_delete,
            Err(OkfError::UnknownName {
                noun: "mail job",
                ..
            })
        ));
        assert!(
            matches!(running_delete, Err(OkfError::Refused { .. })),
            "{running_delete:?}"
        );
        assert!(list(&db.pool).await.unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn prune_keeps_a_week_of_done_and_a_month_of_failed_jobs() {
        let db = test_db().await;
        for _ in 0..4 {
            enqueue(
                &db.pool,
                "alice@example.com",
                &Template::Test,
                now(),
                &Brand::default(),
            )
            .await
            .unwrap();
        }
        // Age the four jobs by hand: done 8 days ago, done 6 days ago,
        // failed 31 days ago, failed 29 days ago.
        sqlx::query(
            "WITH aged AS (SELECT id, row_number() OVER (ORDER BY id) AS n FROM apalis.jobs) \
             UPDATE apalis.jobs j SET \
                 status = CASE WHEN aged.n <= 2 THEN 'Done' ELSE 'Killed' END, \
                 done_at = $1 - CASE aged.n WHEN 1 THEN interval '8 days' \
                                            WHEN 2 THEN interval '6 days' \
                                            WHEN 3 THEN interval '31 days' \
                                            ELSE interval '29 days' END \
             FROM aged WHERE j.id = aged.id",
        )
        .bind(now())
        .execute(&db.pool)
        .await
        .unwrap();

        let pruned = prune(&db.pool, now()).await.unwrap();

        assert_eq!(pruned, 2);
        let left: Vec<String> =
            sqlx::query_scalar("SELECT status FROM apalis.jobs ORDER BY status")
                .fetch_all(&db.pool)
                .await
                .unwrap();
        assert_eq!(left, ["Done", "Killed"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn after_losing_the_database_the_worker_restarts_and_sends() {
        let db = test_db().await;
        let database: String = sqlx::query_scalar("SELECT current_database()")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        // A connection to the server's `template1` database (the test's own
        // may be `postgres`): Postgres refuses to close the database a
        // connection is in, and while the test's database refuses
        // connections this is the way back in.
        // `use sqlx::Connection` brings the `connect_with` constructor into scope.
        use sqlx::Connection;
        let mut admin = sqlx::PgConnection::connect_with(
            &db.pool
                .connect_options()
                .as_ref()
                .clone()
                .database("template1"),
        )
        .await
        .unwrap();
        let mailer = Scripted::with(vec![]);
        let (shutdown, worker) = start_on(worker_pool(&db.pool), Arc::clone(&mailer), "okf-mail-r");
        for _ in 0..200 {
            let registered: i64 = sqlx::query_scalar("SELECT count(*) FROM apalis.workers")
                .fetch_one(&db.pool)
                .await
                .unwrap();
            if registered == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // The outage: no new connections, and the worker's own are cut.
        // `format!` for the name: ALTER DATABASE takes no bind parameters.
        // `AssertSqlSafe`: the name is the test database's own.
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "ALTER DATABASE \"{database}\" WITH ALLOW_CONNECTIONS false"
        )))
        .execute(&mut admin)
        .await
        .unwrap();
        let cut: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM (SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
             WHERE datname = $1 AND application_name = $2) AS cut",
        )
        .bind(&database)
        .bind(WORKER_APPLICATION_NAME)
        .fetch_one(&mut admin)
        .await
        .unwrap();
        // Long enough for the next poll to fail and a restart or two to fail too.
        tokio::time::sleep(Duration::from_millis(800)).await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "ALTER DATABASE \"{database}\" WITH ALLOW_CONNECTIONS true"
        )))
        .execute(&mut admin)
        .await
        .unwrap();
        admin.close().await.unwrap();
        enqueue(
            &db.pool,
            "alice@example.com",
            &Template::Test,
            now(),
            &Brand::default(),
        )
        .await
        .unwrap();
        wait_for(&db.pool, "Done", 1).await;
        let sent_by: String = sqlx::query_scalar("SELECT lock_by FROM apalis.jobs")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        stop(shutdown, worker).await;

        assert!(cut >= 1, "the worker had connections to cut");
        assert!(
            sent_by.starts_with("okf-mail-r-") && sent_by != "okf-mail-r",
            "sent by a restarted worker: {sent_by}"
        );
        assert_eq!(mailer.sent(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_worker_stops_when_told() {
        let db = test_db().await;
        let (shutdown, worker) = start(&db.pool, Scripted::with(vec![]));

        stop(shutdown, worker).await;
    }

    #[test]
    fn the_worker_name_names_this_process() {
        let name = worker_name();

        assert!(name.starts_with("okf-mail-"), "{name}");
        assert!(
            name.ends_with(&format!("-{}", std::process::id())),
            "{name}"
        );
    }

    #[test]
    fn debug_hides_the_payloads_bodies() {
        let job = SendMail {
            mail: Mail {
                to: "alice@example.com".to_string(),
                subject: "Reset your password".to_string(),
                text: "token=s3cr3t".to_string(),
                html: "token=s3cr3t".to_string(),
            },
            template: "reset".to_string(),
            queued_at: 0,
        };

        let shown = format!("{job:?}");

        assert!(
            shown.contains("reset") && !shown.contains("s3cr3t"),
            "{shown}"
        );
    }
}
