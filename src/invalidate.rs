//! The change feed (spec Decisions 10 and 12). A store function that
//! changes what a replica caches calls [`invalidate`] inside its own
//! transaction; Postgres delivers the `NOTIFY` to every listening replica
//! when, and only if, the transaction commits. [`listen`] runs on every
//! replica and applies each message to that replica's caches.
//!
//! A payload carries ids and generations only: never a key hash, a person's
//! name, an email, a label or catalog content.

use std::sync::{Arc, Weak};
use std::time::Duration;

use chrono::Utc;
use sqlx::postgres::{PgListener, PgPoolOptions};
use sqlx::{PgConnection, PgPool};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::app::App;
use crate::backoff::Backoff;
use crate::error::OkfError;
use crate::stores::StoreName;
use crate::telemetry::metrics::Metrics;
use crate::telemetry::subscriber::split_event;

/// The `LISTEN` channel.
pub const CHANNEL: &str = "okf_invalidate";

// The invalidations one request's writes queued, while the request runs
// inside [`capturing`]: so the replica that made a write can apply it to
// its own caches before answering, without waiting for its listener
// (read-your-writes on the writing replica). `task_local!` is a value per
// async task, like a Python `contextvars.ContextVar`; `RefCell` lets the
// owning task push to it through a shared reference (never held across
// an `.await`).
tokio::task_local! {
    static WRITTEN: std::cell::RefCell<Vec<Invalidation>>;
}

/// Runs `request` and collects every invalidation its writes queued
/// ([`invalidate`] inside it). A write that rolled back is collected too:
/// the caller only drops cache entries with them, which is never wrong,
/// only a reread.
///
/// # Arguments
/// * `request` - the work to run (an HTTP handler's future).
///
/// # Returns
/// Its output, and the invalidations, in the order they were queued.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "wraps every HTTP request, an unmatched path included, which must make no span"
    )
)]
pub async fn capturing<F: std::future::Future>(request: F) -> (F::Output, Vec<Invalidation>) {
    WRITTEN
        .scope(std::cell::RefCell::new(Vec::new()), async {
            let output = request.await;
            let written = WRITTEN.with(|written| written.take());
            (output, written)
        })
        .await
}

/// The payload version this build writes and reads.
const VERSION: u8 = 1;

/// The first and longest wait before the listener connects again.
const BACKOFF_FIRST: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Longest the account listener waits to connect (a store's listener uses
/// `store_connect_timeout`).
const ACCOUNT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// What changed. Ids only: never a key hash, a person's name or content.
// `serde(tag = "kind")` writes the variant as a `"kind"` field next to its
// own fields, e.g. `{"kind":"catalog","id":1,"generation":2}`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Invalidation {
    /// A publish, import or repair moved the catalog to `generation`.
    Catalog {
        /// `catalogs.id`.
        id: i64,
        /// The generation the change reached.
        generation: i64,
    },
    /// The catalog row or its access facts changed (create, delete,
    /// details, owner, maintenance, member access, suspension). `name`
    /// (the catalog's slug, an id, not a person's data) lets a replica drop
    /// a cached "no such catalog" entry on create: the id is new, so no
    /// cache can map it back to the name.
    CatalogMeta {
        /// `catalogs.id`.
        id: i64,
        /// The catalog's name.
        name: String,
    },
    /// A key or grant (`api_keys.id`) changed.
    Credential {
        /// `api_keys.id`.
        key_id: String,
    },
    /// A user (`users.id`) changed: profile, admin flag, entitlements,
    /// unlocks, or anything that moves their ceiling.
    User {
        /// `users.id`.
        id: String,
    },
    /// A team (`teams.id`, text) changed: members, roles, label.
    Team {
        /// `teams.id`.
        id: String,
    },
    /// Every catalog a team owns changed in a way the directory caches (a
    /// suspension or its lifting). One message stands for all of them, so a
    /// team owning a million catalogs costs one `NOTIFY`, not a million
    /// `CatalogMeta`s (issue #757). A replica drops each cached directory
    /// row owned by the team; it names no catalog, so nothing grows with
    /// the team's size. A build that predates this kind cannot read it, and
    /// treats it as a lost message: every cache is marked stale, which also
    /// ends a suspended team's catalogs being served as live.
    TeamCatalogs {
        /// `teams.id`.
        team_id: String,
    },
    /// A server setting changed.
    Settings,
    /// A `stores` row changed (its location or credential reference): each
    /// replica drops that store's pool and opens a new one with the new row
    /// on next use ([`crate::stores::Stores::forget`]). A store's name is
    /// operator detail, not content: it may appear in a payload.
    Store {
        /// `stores.name`.
        name: crate::stores::StoreName,
    },
}

/// One received message: what changed, and when it was sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// What changed.
    pub invalidation: Invalidation,
    /// The sender's clock at `invalidate`, in Unix milliseconds, for
    /// `okf.invalidations.lag`.
    pub sent_at_ms: i64,
}

// The wire form: the version and send time beside the flattened variant.
#[derive(serde::Serialize, serde::Deserialize)]
struct Wire {
    v: u8,
    at: i64,
    #[serde(flatten)]
    invalidation: Invalidation,
}

impl Invalidation {
    /// This invalidation's `NOTIFY` payload.
    ///
    /// # Arguments
    /// * `sent_at_ms` - the sender's clock, Unix milliseconds.
    ///
    /// # Returns
    /// The JSON payload.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn to_payload(&self, sent_at_ms: i64) -> String {
        serde_json::to_string(&Wire {
            v: VERSION,
            at: sent_at_ms,
            invalidation: self.clone(),
        })
        // A struct of integers and strings always serializes.
        .expect("an Invalidation always serializes")
    }

    /// The invalidation of one key or grant.
    ///
    /// # Arguments
    /// * `key_id` - its `api_keys.id`.
    ///
    /// # Returns
    /// [`Invalidation::Credential`] for it.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn credential(key_id: &str) -> Self {
        Self::Credential {
            key_id: key_id.to_owned(),
        }
    }

    /// The invalidation of one user.
    ///
    /// # Arguments
    /// * `id` - its `users.id`.
    ///
    /// # Returns
    /// [`Invalidation::User`] for it.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn user(id: &str) -> Self {
        Self::User { id: id.to_owned() }
    }

    /// The invalidation of one team.
    ///
    /// # Arguments
    /// * `id` - its `teams.id`.
    ///
    /// # Returns
    /// [`Invalidation::Team`] for it.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn team(id: &str) -> Self {
        Self::Team { id: id.to_owned() }
    }

    /// The invalidation of every catalog one team owns.
    ///
    /// # Arguments
    /// * `team_id` - its `teams.id`.
    ///
    /// # Returns
    /// [`Invalidation::TeamCatalogs`] for it.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn team_catalogs(team_id: &str) -> Self {
        Self::TeamCatalogs {
            team_id: team_id.to_owned(),
        }
    }

    /// The invalidation of one store's row.
    ///
    /// # Arguments
    /// * `name` - its `stores.name`.
    ///
    /// # Returns
    /// [`Invalidation::Store`] for it.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn store(name: &crate::stores::StoreName) -> Self {
        Self::Store { name: name.clone() }
    }

    /// The serde tag (`"catalog"`, ...), for metric labels and span fields.
    ///
    /// # Returns
    /// A fixed string per variant: never an id.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Catalog { .. } => "catalog",
            Self::CatalogMeta { .. } => "catalog_meta",
            Self::Credential { .. } => "credential",
            Self::User { .. } => "user",
            Self::Team { .. } => "team",
            Self::TeamCatalogs { .. } => "team_catalogs",
            Self::Settings => "settings",
            Self::Store { .. } => "store",
        }
    }
}

impl Message {
    /// Parses a payload.
    ///
    /// # Arguments
    /// * `payload` - the `NOTIFY` payload.
    ///
    /// # Returns
    /// The message; `None` for another version, an unknown kind or anything
    /// else this build cannot read.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn parse(payload: &str) -> Option<Self> {
        let wire: Wire = serde_json::from_str(payload).ok()?;
        // `then_some`: `Some(..)` when the version matches, else `None`.
        (wire.v == VERSION).then_some(Self {
            invalidation: wire.invalidation,
            sent_at_ms: wire.at,
        })
    }

    /// How long ago the sender sent this, by this replica's clock.
    ///
    /// # Arguments
    /// * `now_ms` - this replica's clock, Unix milliseconds.
    ///
    /// # Returns
    /// The lag; zero when the clocks disagree (a negative lag).
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn lag(&self, now_ms: i64) -> Duration {
        // `try_from` fails for a negative number: skew records no lag.
        Duration::from_millis(u64::try_from(now_ms - self.sent_at_ms).unwrap_or(0))
    }
}

/// Records `invalidations` in the caller's transaction (counted as
/// `okf.invalidations.queued`, since only the commit delivers them): bumps
/// `admin_generation` once and sends one `NOTIFY` per invalidation, which
/// Postgres delivers at commit and drops on rollback. Every store function
/// that changes what a replica caches calls this, so the server, the
/// command line and any other writer are covered alike.
///
/// The bump locks the `server_state` row until the commit, so it sits in
/// the writer's lock order where the writer calls this: after its users,
/// teams, catalogs and changesets, before the rest (`lessons.md`). Call it
/// on the paths that write, and only after the people are locked.
///
/// A list of only [`Invalidation::Catalog`] (a publish, import or repair)
/// does not bump: those were never administrative changes, the catalog's
/// own generation (and its `Catalog` message) carries them, and a bump
/// would make every publish wait on the one `server_state` row and mark
/// every replica's credential caches stale for a milestone 16 replica.
///
/// # Arguments
/// * `conn` - the writer's transaction (`&mut tx`).
/// * `invalidations` - what the transaction changed; empty does nothing.
///
/// # Errors
/// [`OkfError::Database`]; the caller's transaction then fails as it would
/// for any other statement.
#[tracing::instrument(level = "debug", skip_all)]
pub async fn invalidate(
    conn: &mut PgConnection,
    invalidations: &[Invalidation],
) -> Result<(), OkfError> {
    // `matches!` is a pattern test that answers a bool.
    if invalidations
        .iter()
        .any(|invalidation| !matches!(invalidation, Invalidation::Catalog { .. }))
    {
        bump_admin_generation(&mut *conn).await?;
    }
    let now = Utc::now().timestamp_millis();
    let metrics = Metrics::global();
    for invalidation in invalidations {
        sqlx::query("SELECT pg_notify($1, $2)")
            .bind(CHANNEL)
            .bind(invalidation.to_payload(now))
            .execute(&mut *conn)
            .await?;
        metrics.invalidation_queued(invalidation.kind());
    }
    // Outside a `capturing` request (the command line, a job) there is
    // nothing to collect into: `try_with` then does nothing.
    WRITTEN
        .try_with(|written| written.borrow_mut().extend_from_slice(invalidations))
        .ok();
    Ok(())
}

/// Bumps `server_state.admin_generation` inside the caller's transaction,
/// locking that row until the commit. Only [`invalidate`] calls it, so no
/// administrative change can bump without telling the other replicas.
///
/// # Arguments
/// * `conn` - the transaction's connection.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "invalidate.bump_admin_generation"))]
async fn bump_admin_generation(conn: &mut PgConnection) -> Result<(), OkfError> {
    sqlx::query("UPDATE server_state SET admin_generation = admin_generation + 1")
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Whose change feed a listener follows.
#[derive(Clone)]
enum Scope {
    /// The account database: every message applies to this replica's caches.
    Account,
    /// One store's database. A lost connection or an unreadable payload
    /// drops only that store's catalogs from the caches.
    Store {
        name: StoreName,
        /// The store's pool, read for its connect options at every connect
        /// (a rotated credential is picked up); never used to run a query.
        pool: PgPool,
        /// Longest a connect may take (`store_connect_timeout`).
        connect_timeout: Duration,
    },
}

/// One listener: a dedicated connection, subscribed to [`CHANNEL`], and what
/// to do with what it hears. [`Feed::run`] is the loop both the account
/// listener and each store's listener run.
struct Feed {
    // `Weak`: a store's listener must not keep the replica alive, and ends
    // when the replica is gone.
    app: Weak<App>,
    scope: Scope,
    // Told after each full catch-up (tests wait on it).
    caught_up: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Feed {
    /// Connects a listener on its own one-connection pool, subscribed to
    /// [`CHANNEL`]. sqlx's `connect_with` keeps one connection from the pool
    /// it is given for good, so the pool whose options are copied would lose
    /// a request slot; the small pool also gives the listener its reconnects.
    /// The connection is outside the store's pool: it does not count against
    /// `store_max_connections`.
    ///
    /// # Arguments
    /// * `pool` - the pool whose connect options the listener copies.
    /// * `connect_timeout` - the longest the connect may take.
    ///
    /// # Errors
    /// [`OkfError::Database`] if the database cannot be reached in time.
    #[tracing::instrument(level = "debug", skip_all)]
    async fn connect(pool: &PgPool, connect_timeout: Duration) -> Result<PgListener, OkfError> {
        let own = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(connect_timeout)
            .connect_with((*pool.connect_options()).clone())
            .await?;
        let mut listener = PgListener::connect_with(&own).await?;
        listener.listen(CHANNEL).await?;
        Ok(listener)
    }

    /// Connects this feed's listener.
    ///
    /// # Returns
    /// The listener, or `None` once the replica is gone.
    ///
    /// # Errors
    /// [`OkfError::Database`] if the database cannot be reached.
    #[tracing::instrument(level = "debug", skip_all)]
    async fn open(&self) -> Result<Option<PgListener>, OkfError> {
        match &self.scope {
            Scope::Account => {
                let Some(app) = self.app.upgrade() else {
                    return Ok(None);
                };
                Self::connect(app.accounts().executor(), ACCOUNT_CONNECT_TIMEOUT)
                    .await
                    .map(Some)
            }
            Scope::Store {
                pool,
                connect_timeout,
                ..
            } => Self::connect(pool, *connect_timeout).await.map(Some),
        }
    }

    /// What a fresh or repaired connection does: anything sent while there
    /// was none is lost, so the caches this feed covers are made to reread.
    ///
    /// # Returns
    /// `false` once the replica is gone.
    #[tracing::instrument(level = "debug", skip_all)]
    async fn catch_up(&self) -> bool {
        let Some(app) = self.app.upgrade() else {
            return false;
        };
        match &self.scope {
            Scope::Account => app.apply_reconnect().await,
            Scope::Store { name, .. } => app.apply_store_reconnect(name).await,
        }
        true
    }

    /// Tells the test hook, if there is one, that a catch-up finished.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "calls a hook")
    )]
    fn signal(&self) {
        if let Some(caught_up) = &self.caught_up {
            caught_up();
        }
    }

    /// Applies one notification: a payload that cannot be read is treated
    /// as a reconnect, for safety (spec "Error handling"). The received
    /// count and the lag are recorded after the message is applied, so the
    /// lag is the time from send until this replica's caches had the change.
    ///
    /// # Returns
    /// The lag recorded, or `None` for an unreadable payload (already caught
    /// up) or once the replica is gone.
    #[tracing::instrument(level = "debug", skip_all)]
    async fn apply_payload(&self, payload: &str) -> Option<Duration> {
        let metrics = Metrics::global();
        let app = self.app.upgrade()?;
        let Some(message) = Message::parse(payload) else {
            metrics.invalidation_unreadable();
            self.catch_up().await;
            return None;
        };
        let kind = message.invalidation.kind();
        // `parent: None`: a background root, like each housekeeping job.
        let span =
            tracing::info_span!(parent: None, "okf.invalidate.apply", okf.invalidation.kind = kind);
        app.apply(&message).instrument(span).await;
        let lag = message.lag(Utc::now().timestamp_millis());
        metrics.invalidation_received(kind, lag);
        Some(lag)
    }

    /// Words for the log: which feed this is.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "names a scope")
    )]
    fn label(&self) -> &'static str {
        match self.scope {
            Scope::Account => "the change feed",
            Scope::Store { .. } => "a store's change feed",
        }
    }

    /// Listens until `shutdown`, applying each message to the replica. A
    /// lost connection (`try_recv` returns `Ok(None)`, already reconnected)
    /// makes the covered caches reread, since messages sent meanwhile are
    /// lost (spec Decision 12). A connect failure backs off from 1 s to
    /// 30 s, and never blocks another feed. Applying a message or catching
    /// up races `shutdown` as well, so a blocked database cannot hold the
    /// shutdown.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "long-lived loop; each message gets its own root span"
        )
    )]
    async fn run(self, shutdown: CancellationToken) {
        let mut backoff = Backoff::new(BACKOFF_FIRST, BACKOFF_MAX);
        loop {
            let connected = tokio::select! {
                () = shutdown.cancelled() => return,
                connected = self.open() => connected,
            };
            let mut listener = match connected {
                Ok(Some(listener)) => listener,
                Ok(None) => return,
                Err(error) => {
                    // A store's name is operator detail: for the local log only.
                    match &self.scope {
                        Scope::Account => split_event!(
                            warn,
                            { kind = error.kind(), },
                            { %error, },
                            "the change feed could not connect; trying again"
                        ),
                        Scope::Store { name, .. } => split_event!(
                            warn,
                            { kind = error.kind(), },
                            { store = %name, %error, },
                            "a store's change feed could not connect; trying again"
                        ),
                    }
                    tokio::select! {
                        () = shutdown.cancelled() => return,
                        () = tokio::time::sleep(backoff.next_delay()) => continue,
                    }
                }
            };
            // A fresh connection: anything sent while we had none is lost.
            // Applying can wait on locks and reads, so it races shutdown too:
            // a blocked apply must not keep the process from stopping.
            match shutdown.run_until_cancelled(self.catch_up()).await {
                None | Some(false) => return,
                Some(true) => {}
            }
            self.signal();
            backoff.reset();
            loop {
                tokio::select! {
                    () = shutdown.cancelled() => return,
                    received = listener.try_recv() => match received {
                        Ok(Some(notification)) => {
                            match shutdown
                                .run_until_cancelled(self.apply_payload(notification.payload()))
                                .await
                            {
                                None => return,
                                // `Some(None)`: unreadable, so it caught up (or the
                                // replica is gone, and the next catch-up ends us).
                                Some(None) => self.signal(),
                                Some(Some(_lag)) => {}
                            }
                        }
                        // The connection was lost. Connect again through the outer
                        // loop, which finishes `LISTEN` before the catch-up, so a
                        // commit after the catch-up's reads is delivered, not missed.
                        Ok(None) => {
                            tracing::warn!("{} was lost; connecting again", self.label());
                            break;
                        }
                        // A failure sqlx could not repair: connect again.
                        Err(error) => {
                            let error = OkfError::from(error);
                            split_event!(
                                warn,
                                { kind = error.kind(), },
                                { %error, },
                                "a change feed connection failed; connecting again"
                            );
                            break;
                        }
                    },
                }
            }
        }
    }
}

/// Listens on [`CHANNEL`] of the account database until `shutdown`, applying
/// each message to `app`: a lost connection makes every cache reread, and a
/// connect failure backs off from 1 s to 30 s.
///
/// # Arguments
/// * `app` - the replica's shared state.
/// * `shutdown` - cancelled to stop.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "long-lived loop; each message gets its own root span"
    )
)]
pub async fn listen(app: Arc<App>, shutdown: CancellationToken) {
    listen_signalling(app, shutdown, None).await;
}

/// [`listen`], telling `caught_up` (if given) each time the listener has
/// finished a full catch-up (`App::apply_reconnect`): after connecting,
/// after a lost connection, and after an unreadable payload. A test waits
/// on it to know the subscription is live before it sends a message.
///
/// # Arguments
/// * `app` - the replica's shared state.
/// * `shutdown` - cancelled to stop.
/// * `caught_up` - notified after each catch-up; `Notify` keeps one permit,
///   so a signal sent before the test waits is not lost.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "long-lived loop; each message gets its own root span"
    )
)]
async fn listen_signalling(
    app: Arc<App>,
    shutdown: CancellationToken,
    caught_up: Option<Arc<tokio::sync::Notify>>,
) {
    let feed = Feed {
        app: Arc::downgrade(&app),
        scope: Scope::Account,
        caught_up: caught_up
            .map(|notify| Arc::new(move || notify.notify_one()) as Arc<dyn Fn() + Send + Sync>),
    };
    // `app` stays held here, so the weak handle upgrades for as long as the
    // account listener runs.
    feed.run(shutdown).await;
    drop(app);
}

/// A running listener on one store's database (Task 17): dropping it
/// cancels the task, which closes the listener's connection and does not
/// reconnect. The registry holds one per opened store, so forgetting a store
/// ends it.
#[derive(Debug)]
pub(crate) struct StoreFeed {
    stop: CancellationToken,
    // Not awaited: the task ends by itself once `stop` is cancelled.
    #[cfg(test)]
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for StoreFeed {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "a Drop impl")
    )]
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl StoreFeed {
    /// Tests: whether the listener's task has ended.
    #[cfg(test)]
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "a test hook")
    )]
    pub(crate) fn is_finished(&self) -> bool {
        self.task
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
    }

    /// Tests: cancels the listener (by dropping it) and returns its task,
    /// to await its end.
    #[cfg(test)]
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "a test hook")
    )]
    pub(crate) fn into_task(mut self) -> tokio::task::JoinHandle<()> {
        self.task.take().expect("the task is taken once")
    }
}

/// What starting a store's listener needs.
pub(crate) struct StoreFeedStart {
    /// The replica the messages are applied to.
    pub(crate) app: Weak<App>,
    /// The store's name.
    pub(crate) name: StoreName,
    /// The store's pool, whose connect options the listener copies.
    pub(crate) pool: PgPool,
    /// Longest one connect may take (`store_connect_timeout`).
    pub(crate) connect_timeout: Duration,
    /// Cancelled when the server shuts down.
    pub(crate) shutdown: CancellationToken,
    /// Tests: told after each catch-up.
    pub(crate) caught_up: Option<Arc<dyn Fn() + Send + Sync>>,
}

/// Starts the listener on one store's database. It follows the store's own
/// `NOTIFY`s (a publish, import or repair on it sends [`Invalidation::Catalog`]
/// from the store's transaction), and stops when the returned [`StoreFeed`]
/// is dropped or `shutdown` is cancelled. A store that cannot be reached
/// retries on its own task, with backoff, and delays nothing else.
///
/// # Arguments
/// * `start` - the replica, store and pool.
///
/// # Returns
/// The handle that owns the listener.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) fn spawn_store_feed(start: StoreFeedStart) -> StoreFeed {
    let stop = start.shutdown.child_token();
    let feed = Feed {
        app: start.app,
        scope: Scope::Store {
            name: start.name,
            pool: start.pool,
            connect_timeout: start.connect_timeout,
        },
        caught_up: start.caught_up,
    };
    let task = tokio::spawn(feed.run(stop.clone()));
    // Without tests the task is detached: `stop` is all that reaches it.
    #[cfg(not(test))]
    drop(task);
    StoreFeed {
        stop,
        #[cfg(test)]
        task: Some(task),
    }
}

#[cfg(test)]
pub(crate) mod store_feed_tests;

/// Test helpers: what a write sends on the change feed.
#[cfg(test)]
pub(crate) mod testing {
    use std::future::Future;
    use std::time::Duration;

    use sqlx::postgres::PgListener;
    use sqlx::PgPool;

    use super::{Invalidation, Message, CHANNEL};
    use crate::app::App;

    /// The payload that marks the end of a write's messages: not JSON, so
    /// no message can look like it.
    const END: &str = "invalidations_of:end";

    /// Longest the helper waits for the end marker.
    const WAIT: Duration = Duration::from_secs(30);

    /// Listens on the change feed, runs `write`, and returns every message
    /// its transactions committed, in order (empty if it rolled back).
    /// After `write` finishes, it sends an end marker in a transaction of
    /// its own: Postgres delivers notifications in commit order, so every
    /// message the write committed arrives before it, and nothing waits a
    /// fixed time.
    ///
    /// # Arguments
    /// * `pool` - the test database (each test has its own, so no other
    ///   test's messages arrive).
    /// * `write` - the write, an `async { .. }` block. A Rust future does
    ///   nothing until it is awaited, so it runs only once the listener is
    ///   subscribed.
    ///
    /// # Returns
    /// The invalidations delivered, in commit order.
    ///
    /// # Panics
    /// When the end marker does not arrive within 30 s, or on a payload
    /// that does not parse.
    pub(crate) async fn invalidations_of(
        pool: &PgPool,
        write: impl Future<Output = ()>,
    ) -> Vec<Invalidation> {
        messages_of(pool, write).await.1
    }

    /// [`invalidations_of`], keeping what `write` returned.
    ///
    /// # Arguments
    /// * `pool` - the test database.
    /// * `write` - the write.
    ///
    /// # Returns
    /// `write`'s output and the invalidations delivered, in commit order.
    ///
    /// # Panics
    /// As [`invalidations_of`].
    // `<T>`: a generic, like a Python `TypeVar`: the helper works for a
    // write of any output type.
    pub(crate) async fn messages_of<T>(
        pool: &PgPool,
        write: impl Future<Output = T>,
    ) -> (T, Vec<Invalidation>) {
        // The account database, and the store `main` when it is another
        // database (`OKF_TEST_STORE=separate`): a publish, import or repair
        // sends its catalog message from the store's transaction.
        let store = crate::db::store_pool(pool);
        let mut pools = vec![pool];
        if !crate::stores::same_database(pool, &store) {
            pools.push(&store);
        }
        let (output, by_database) = messages_by_database(&pools, write).await;
        (output, by_database.into_iter().flatten().collect())
    }

    /// [`messages_of`] with one listener per database in `pools`, keeping
    /// each database's messages apart: what a test needs to say which
    /// database a writer's `NOTIFY` went out on, and that none went out on
    /// another. An end marker is sent on every database after `write`, so a
    /// database that received nothing is also answered within a bound.
    ///
    /// # Arguments
    /// * `pools` - the databases to listen on (distinct databases).
    /// * `write` - the write.
    ///
    /// # Returns
    /// `write`'s output, and for each pool, in order, the invalidations
    /// delivered on its database in commit order.
    ///
    /// # Panics
    /// As [`invalidations_of`].
    pub(crate) async fn messages_by_database<T>(
        pools: &[&PgPool],
        write: impl Future<Output = T>,
    ) -> (T, Vec<Vec<Invalidation>>) {
        let mut listeners = Vec::with_capacity(pools.len());
        for pool in pools {
            let mut listener = PgListener::connect_with(pool).await.expect("listener");
            listener.listen(CHANNEL).await.expect("LISTEN");
            listeners.push(listener);
        }
        let output = write.await;
        let mut by_database = Vec::with_capacity(pools.len());
        for (pool, listener) in pools.iter().zip(&mut listeners) {
            sqlx::query("SELECT pg_notify($1, $2)")
                .bind(CHANNEL)
                .bind(END)
                .execute(*pool)
                .await
                .expect("send the end marker");
            let mut got = Vec::new();
            loop {
                let notification = tokio::time::timeout(WAIT, listener.recv())
                    .await
                    .expect("the end marker within the wait")
                    .expect("receive a notification");
                if notification.payload() == END {
                    break;
                }
                let message = Message::parse(notification.payload()).expect("a readable payload");
                got.push(message.invalidation);
            }
            by_database.push(got);
        }
        (output, by_database)
    }

    /// Runs `write` and applies every change-feed message it committed to
    /// `app`, as that replica's listener would ([`App::apply`]). Lib tests
    /// run no listener, so this is how a test makes a store write it did
    /// outside a request visible to an app whose caches may hold the old
    /// state (the poll loop did it before milestone 18 removed it).
    ///
    /// # Arguments
    /// * `app` - the replica whose caches the messages are applied to.
    /// * `write` - the write, an `async { .. }` block.
    ///
    /// # Returns
    /// What `write` returned.
    ///
    /// # Panics
    /// As [`invalidations_of`].
    pub(crate) async fn applied<T>(app: &App, write: impl Future<Output = T>) -> T {
        let (output, sent) = messages_of(app.accounts().executor(), write).await;
        let sent_at_ms = chrono::Utc::now().timestamp_millis();
        for invalidation in sent {
            app.apply(&Message {
                invalidation,
                sent_at_ms,
            })
            .await;
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::pg_support::TestDb;
    use crate::store::catalogs;
    use tokio::time::timeout;

    /// Longest a test waits for a notification.
    const WAIT: Duration = Duration::from_secs(30);

    #[test]
    fn every_kind_round_trips_through_its_payload() {
        let all = [
            Invalidation::Catalog {
                id: 42,
                generation: 17,
            },
            Invalidation::CatalogMeta {
                id: 42,
                name: "notes".into(),
            },
            Invalidation::Credential {
                key_id: "k_1".to_owned(),
            },
            Invalidation::User {
                id: "u_1".to_owned(),
            },
            Invalidation::Team {
                id: "t_1".to_owned(),
            },
            Invalidation::team_catalogs("t_1"),
            Invalidation::Settings,
            Invalidation::store(&crate::stores::StoreName::parse("shared-1").unwrap()),
        ];
        for invalidation in all {
            let payload = invalidation.to_payload(1_700_000_000_000);
            let message = Message::parse(&payload).expect("parses");
            assert_eq!(message.invalidation, invalidation);
            assert_eq!(message.sent_at_ms, 1_700_000_000_000);
        }
    }

    #[test]
    fn a_payload_of_an_unknown_version_or_kind_does_not_parse() {
        assert!(
            Message::parse(r#"{"v":2,"at":1,"kind":"catalog","id":1,"generation":1}"#).is_none()
        );
        assert!(Message::parse(r#"{"v":1,"at":1,"kind":"nope"}"#).is_none());
        assert!(Message::parse("42").is_none());
    }

    #[test]
    fn a_store_payload_with_an_invalid_name_does_not_parse() {
        let good = Invalidation::store(&crate::stores::StoreName::main()).to_payload(1);
        assert_eq!(good, r#"{"v":1,"at":1,"kind":"store","name":"main"}"#);
        assert!(Message::parse(r#"{"v":1,"at":1,"kind":"store","name":"Main/x"}"#).is_none());
    }

    #[test]
    fn a_negative_lag_from_clock_skew_is_zero() {
        let message = Message::parse(&Invalidation::Settings.to_payload(2_000)).expect("parses");
        assert_eq!(message.lag(1_000), Duration::ZERO);
        assert_eq!(message.lag(2_500), Duration::from_millis(500));
    }

    #[test]
    fn a_payload_names_ids_only() {
        let payload = Invalidation::Credential {
            key_id: "k_1".to_owned(),
        }
        .to_payload(1);
        assert_eq!(
            payload,
            r#"{"v":1,"at":1,"kind":"credential","key_id":"k_1"}"#
        );
    }

    #[tokio::test]
    async fn a_committed_invalidation_is_delivered_and_a_rolled_back_one_is_not() {
        let db = TestDb::start_migrated().await;
        let mut listener = PgListener::connect_with(&db.pool).await.unwrap();
        listener.listen(CHANNEL).await.unwrap();

        let mut tx = db.pool.begin().await.unwrap();
        invalidate(&mut tx, &[Invalidation::Settings])
            .await
            .unwrap();
        tx.rollback().await.unwrap();
        let mut tx = db.pool.begin().await.unwrap();
        invalidate(
            &mut tx,
            &[Invalidation::CatalogMeta {
                id: 3,
                name: "c".into(),
            }],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();

        // The rolled-back message was never sent, so the first one received
        // is the committed one.
        let got = timeout(WAIT, listener.recv())
            .await
            .expect("a notification within the wait")
            .unwrap();
        let message = Message::parse(got.payload()).expect("parses");
        assert_eq!(
            message.invalidation,
            Invalidation::CatalogMeta {
                id: 3,
                name: "c".into()
            }
        );
    }

    #[tokio::test]
    async fn invalidate_bumps_admin_generation_once_per_call() {
        let db = TestDb::start_migrated().await;
        let before = catalogs::admin_generation(&db.pool).await.unwrap();
        let mut tx = db.pool.begin().await.unwrap();
        invalidate(
            &mut tx,
            &[
                Invalidation::Settings,
                Invalidation::User { id: "u".into() },
            ],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(
            catalogs::admin_generation(&db.pool).await.unwrap(),
            before + 1
        );

        // Nothing to invalidate: no bump.
        let mut tx = db.pool.begin().await.unwrap();
        invalidate(&mut tx, &[]).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(
            catalogs::admin_generation(&db.pool).await.unwrap(),
            before + 1
        );
    }

    #[tokio::test]
    async fn a_catalog_invalidation_alone_sends_without_bumping_or_locking_the_generation() {
        let db = TestDb::start_migrated().await;
        let before = catalogs::admin_generation(&db.pool).await.unwrap();
        // Another transaction holds the generation row: a bump would wait
        // on it, so the publish below finishing proves it took no lock.
        let mut holder = db.pool.begin().await.unwrap();
        invalidate(&mut holder, &[Invalidation::Settings])
            .await
            .unwrap();
        let catalog = Invalidation::Catalog {
            id: 7,
            generation: 3,
        };
        let got = testing::invalidations_of(&db.pool, async {
            let mut tx = db.pool.begin().await.unwrap();
            timeout(WAIT, invalidate(&mut tx, std::slice::from_ref(&catalog)))
                .await
                .expect("a catalog invalidation does not wait on the generation row")
                .unwrap();
            tx.commit().await.unwrap();
        })
        .await;
        holder.rollback().await.unwrap();
        assert_eq!(got, [catalog]);
        assert_eq!(catalogs::admin_generation(&db.pool).await.unwrap(), before);
    }

    /// A running listener on a fresh app with one key, caught up and
    /// subscribed: the test waits for the listener's own catch-up signal.
    struct Running {
        db: crate::db::pg_support::TestDb,
        app: Arc<App>,
        key: String,
        caught_up: Arc<tokio::sync::Notify>,
        shutdown: CancellationToken,
        task: tokio::task::JoinHandle<()>,
    }

    impl Running {
        async fn start() -> Self {
            use crate::clock::SystemClock;
            use crate::config::Config;
            use crate::store::keys as key_rows;

            let db = crate::db::test_db().await;
            let key = key_rows::insert_legacy_test_key(
                &db.pool,
                "reader",
                key_rows::Access::Read,
                "tester",
                Utc::now(),
            )
            .await;
            let app = App::load(db.pool.clone(), Config::default(), Arc::new(SystemClock))
                .await
                .unwrap();
            assert!(app.find_key(&key).await.unwrap().is_some());
            let caught_up = Arc::new(tokio::sync::Notify::new());
            let shutdown = CancellationToken::new();
            let task = tokio::spawn(listen_signalling(
                Arc::clone(&app),
                shutdown.clone(),
                Some(Arc::clone(&caught_up)),
            ));
            let running = Self {
                db,
                app,
                key,
                caught_up,
                shutdown,
                task,
            };
            running
                .wait_caught_up("the first subscription and catch-up")
                .await;
            running
        }

        async fn wait_caught_up(&self, what: &str) {
            timeout(WAIT, self.caught_up.notified())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
        }

        /// Revokes the key and bumps `admin_generation` but sends nothing,
        /// as a writer from before the change feed would: only a catch-up
        /// can remove the key then. (The store's `keys::revoke` now sends a
        /// `credential` message, which the listener could apply first.)
        async fn revoke_silently(&self) {
            // Cached first: keys load on demand, and only a cached one can
            // outlive its revocation.
            assert!(self.app.find_key(&self.key).await.unwrap().is_some());
            let mut tx = self.db.pool.begin().await.unwrap();
            sqlx::query("UPDATE api_keys SET revoked_at = now() WHERE id = 'reader'")
                .execute(&mut *tx)
                .await
                .unwrap();
            bump_admin_generation(&mut tx).await.unwrap();
            tx.commit().await.unwrap();
            assert!(self.app.find_key(&self.key).await.unwrap().is_some());
        }

        async fn stop(self) {
            self.shutdown.cancel();
            timeout(WAIT, self.task)
                .await
                .expect("the listener stopped within the wait")
                .unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_listener_applies_a_notification_that_arrives_after_it_is_caught_up() {
        let running = Running::start().await;

        // The subscription is live and the initial catch-up is done, so only
        // the notification can remove the key: revoke and send in one commit.
        let mut tx = running.db.pool.begin().await.unwrap();
        sqlx::query("UPDATE api_keys SET revoked_at = now() WHERE id = 'reader'")
            .execute(&mut *tx)
            .await
            .unwrap();
        invalidate(
            &mut tx,
            &[Invalidation::Credential {
                key_id: "reader".to_owned(),
            }],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();

        timeout(WAIT, async {
            while running.app.find_key(&running.key).await.unwrap().is_some() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("the notification removed the revoked key within the wait");
        running.stop().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn changing_a_store_row_reopens_its_pool_on_every_replica() {
        use crate::clock::SystemClock;
        use crate::config::Config;
        use crate::stores::StoreName;

        let db = crate::db::test_db().await;
        let main = StoreName::main();
        // Two replicas of one database, each with its own listener.
        let mut replicas = Vec::new();
        for _ in 0..2 {
            let app = App::load(db.pool.clone(), Config::default(), Arc::new(SystemClock))
                .await
                .unwrap();
            let caught_up = Arc::new(tokio::sync::Notify::new());
            let shutdown = CancellationToken::new();
            let task = tokio::spawn(listen_signalling(
                Arc::clone(&app),
                shutdown.clone(),
                Some(Arc::clone(&caught_up)),
            ));
            timeout(WAIT, caught_up.notified())
                .await
                .expect("the listener subscribed within the wait");
            app.stores().get(&main).await.unwrap();
            assert_eq!(app.stores().pools_opened(), 1);
            replicas.push((app, shutdown, task));
        }

        // One writer changes the row and says so, in one commit. Neither
        // replica wrote it: only the change feed can reach them.
        let mut tx = db.pool.begin().await.unwrap();
        sqlx::query("UPDATE stores SET created_at = now() WHERE name = 'main'")
            .execute(&mut *tx)
            .await
            .unwrap();
        invalidate(&mut tx, &[Invalidation::store(&main)])
            .await
            .unwrap();
        tx.commit().await.unwrap();

        for (app, _, _) in &replicas {
            // A pool that is not dropped yet is a hit; the first `get` after
            // the message opens the second.
            timeout(WAIT, async {
                while app.stores().pools_opened() < 2 {
                    app.stores().get(&main).await.unwrap();
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await
            .expect("the replica reopened the store's pool within the wait");
            // And it keeps the new pool: nothing else asked for another.
            app.stores().get(&main).await.unwrap();
            assert_eq!(app.stores().pools_opened(), 2);
        }
        for (_, shutdown, task) in replicas {
            shutdown.cancel();
            timeout(WAIT, task).await.unwrap().unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unreadable_payload_makes_the_listener_catch_up() {
        let running = Running::start().await;
        running.revoke_silently().await;

        sqlx::query("SELECT pg_notify($1, 'not json')")
            .bind(CHANNEL)
            .execute(&running.db.pool)
            .await
            .unwrap();

        running
            .wait_caught_up("the catch-up after an unreadable payload")
            .await;
        assert!(running.app.find_key(&running.key).await.unwrap().is_none());
        running.stop().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_dropped_listener_connection_makes_the_listener_catch_up() {
        let running = Running::start().await;
        running.revoke_silently().await;

        // End this test's listener backend (the server is shared with other
        // tests, each in its own database); sqlx reconnects, and the listener
        // catches up without any message.
        let ended: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM (SELECT pg_terminate_backend(pid) \
             FROM pg_stat_activity WHERE query LIKE 'LISTEN%' \
                   AND datname = current_database() AND pid <> pg_backend_pid()) t",
        )
        .fetch_one(&running.db.pool)
        .await
        .unwrap();
        assert_eq!(ended, 1, "exactly one listener backend");

        running
            .wait_caught_up("the catch-up after a lost connection")
            .await;
        assert!(running.app.find_key(&running.key).await.unwrap().is_none());
        // The catch-up is signalled only once the replacement connection is
        // subscribed: a commit after the catch-up's reads is then delivered.
        let listening: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity WHERE query LIKE 'LISTEN%' \
             AND datname = current_database() AND pid <> pg_backend_pid()",
        )
        .fetch_one(&running.db.pool)
        .await
        .unwrap();
        assert_eq!(
            listening, 1,
            "a live LISTEN backend when the catch-up is signalled"
        );
        running.stop().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_lag_includes_the_time_the_application_waited() {
        let db = crate::db::test_db().await;
        let app = App::load(
            db.pool.clone(),
            crate::config::Config::default(),
            Arc::new(crate::clock::SystemClock),
        )
        .await
        .unwrap();
        let payload = Invalidation::Settings.to_payload(Utc::now().timestamp_millis());

        // Block the generation read the reload starts with.
        let mut blocker = db.pool.begin().await.unwrap();
        sqlx::query("LOCK TABLE server_state IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *blocker)
            .await
            .unwrap();
        let applying = tokio::spawn({
            let feed = Feed {
                app: Arc::downgrade(&app),
                scope: Scope::Account,
                caught_up: None,
            };
            async move { feed.apply_payload(&payload).await }
        });
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(!applying.is_finished(), "the apply must be blocked");
        blocker.commit().await.unwrap();

        let lag = timeout(WAIT, applying)
            .await
            .expect("the apply finished once unblocked")
            .unwrap()
            .expect("a readable payload");
        assert!(lag >= Duration::from_millis(400), "{lag:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_blocked_apply_does_not_hold_up_the_listener_stopping() {
        let running = Running::start().await;

        // Block the generation read every apply starts with, then send a
        // message so the listener is stuck inside its apply.
        let mut blocker = running.db.pool.begin().await.unwrap();
        sqlx::query("LOCK TABLE server_state IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *blocker)
            .await
            .unwrap();
        sqlx::query("SELECT pg_notify($1, $2)")
            .bind(CHANNEL)
            .bind(Invalidation::Settings.to_payload(Utc::now().timestamp_millis()))
            .execute(&running.db.pool)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!running.task.is_finished(), "the listener keeps running");

        // Cancel while the blocker is still held: the listener must leave
        // its blocked apply and exit.
        running.shutdown.cancel();
        timeout(Duration::from_secs(5), running.task)
            .await
            .expect("the listener exited although its apply was blocked")
            .unwrap();
        blocker.rollback().await.unwrap();
    }
}
