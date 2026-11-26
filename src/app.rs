//! The server's shared state (design "Architecture"): the database, the
//! configuration, the clock, the credential caches, the catalog directory
//! and the catalog cache (catalogs load on demand, M18 Decisions 2 and 6),
//! the change feed's handlers (M18 Decisions 10-14; the poll loop is gone,
//! Decision 18), and the clock check loop and its last result (design
//! "Time").

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use sqlx::PgPool;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::auth::{Admission, Principal};
use crate::cache::catalogs::{CatalogCache, CatchUp};
use crate::cache::credentials::{AccessView, CredentialLimits, Credentials};
use crate::cache::directory::CatalogDirectory;
use crate::cache::labels::{LabelLookup, DEFAULT_LABEL_CACHE_ENTRIES};
use crate::catalog::CatalogState;
use crate::clock::Clock;
use crate::clock_check::{self, ClockStatus};
use crate::config::Config;
use crate::error::OkfError;
use crate::http::limits::SignInLimits;
use crate::http::passkeys::Passkeys;
use crate::http::sessions::SessionBindings;
use crate::keyed_lock::{KeyedGuard, KeyedLocks};
use crate::oauth::OAuth;
use crate::oidc::OidcProviders;
use crate::overlay::Overlays;
use crate::shared;
use crate::store::audit::{self, AccessKind, AccessRecord, ProvisionerUse};
use crate::store::catalogs::{CatalogGeneration, CatalogRow};
use crate::store::changesets::ChangesetKey;
use crate::store::settings::{self, Settings};
use crate::store::{catalogs, unlocks};
use crate::stores::{AccountDb, CatalogDb, PoolSettings, StoreName, Stores};
use crate::telemetry::metrics::{AnnouncementResult, Metrics, Outcome};
use crate::telemetry::subscriber::split_event;

/// How many background writes of provisioner uses may be pending at once;
/// more are dropped, not queued ([`App::note_provisioner_use`]).
pub const MAX_PENDING_PROVISIONER_WRITES: usize = 32;

/// The longest a settings check may take. A check runs before every
/// request whose settings expired, outside the routes' own timeouts, so a
/// blocked read (`server_state` under an exclusive lock, an unresponsive
/// connection) must not hold requests: past this bound the check counts as
/// failed, the loaded settings serve, and the next request retries.
pub const SETTINGS_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The settings as last loaded, with the `admin_generation` they were
/// checked at and when (spec Decision 9): `admin_generation` moves with
/// every administrative change, settings writes included, so an unmoved
/// counter means the loaded settings are still the stored ones.
#[derive(Debug, Clone)]
struct LoadedSettings {
    /// The effective settings.
    settings: Arc<Settings>,
    /// The `admin_generation` read just before they were loaded, or at the
    /// last check that found it unmoved.
    generation: i64,
    /// When the last check began, by the app's clock.
    validated_at: chrono::DateTime<chrono::Utc>,
}

/// The state every route and tool shares. Built once by [`App::load`] and
/// shared as an `Arc<App>`.
pub struct App {
    // The account database: users, teams, keys, sessions and the catalog
    // registry.
    accounts: AccountDb,
    // Every catalog store's pool, opened on first use. Never the account
    // pool, even for `main` (plan decision P1).
    stores: Stores,
    config: Config,
    // `Arc<dyn Clock>`: any clock (the real one, or a fixed one in tests),
    // shared between threads. `dyn Clock` is a trait object: Python's duck
    // typing, with the method looked up at run time.
    clock: Arc<dyn Clock>,
    // Keys, grants and users, loaded on demand (M18 Decision 9).
    credentials: Credentials,
    // Labels of the slugs a response shows, read on demand and cached
    // (M18 Decision 9).
    labels: LabelLookup,
    // Which key opened each MCP session (M5 Decision 2).
    sessions: SessionBindings,
    // The shared Redis (`[redis]`); `None` keeps every short-lived state in
    // this process's memory (M16).
    redis: Option<shared::Redis>,
    // Each open changeset's overlay, cached (M5 Decision 6).
    overlays: Overlays,
    // Each changeset's writers, queued in this process before they take a
    // pooled connection (`write::apply`).
    write_queue: KeyedLocks<ChangesetKey>,
    // The in-process half of each catalog's publish lock (design
    // "Publish"), by catalog id: here rather than in `CatalogState`, so
    // every state of one catalog shares it (M18 Decision 4).
    publish_locks: KeyedLocks,
    // Each catalog's rebuild lock (`CatalogState::refresh`), by catalog id,
    // for the same reason. Its holder may wait for a pooled connection, so
    // nothing holding a connection waits for it (issue #671).
    rebuild_locks: KeyedLocks,
    // Catalog rows by name with their access facts (M18 Decision 6): what
    // "does this catalog exist" and every snapshot's freshness are read from.
    directory: Arc<CatalogDirectory>,
    // Catalog snapshots by id, loaded on demand and bounded in estimated
    // bytes (M18 Decision 2).
    catalog_cache: CatalogCache,
    // The effective sign-in policy (`[auth]` with `server_settings`
    // overrides), with the `admin_generation` it was checked at and when
    // (spec Decision 9). Reloaded on a `Settings` message, and checked again
    // by the first request after `credential_ttl` ([`App::fresh_settings`]).
    // The lock is held only to clone or swap.
    settings: RwLock<LoadedSettings>,
    // Held while the settings are checked or reloaded, so callers that find
    // them expired together run one check (tokio's `Mutex`: it is held
    // across `.await`s).
    settings_reload: tokio::sync::Mutex<()>,
    // How many settings checks have finished, successful or not. A caller
    // that waited for `settings_reload` while one finished takes its result
    // instead of checking again, so during a database outage one failing
    // check serves every request that queued behind it.
    settings_rounds: AtomicU64,
    // How many times the settings were marked stale (a `Settings` message,
    // a reconnect, the request's own write). They stay stale, for
    // every caller, until a check that began after the last mark succeeds:
    // `satisfied_marks` is the count such a check saw when it began, and is
    // written only on success, so a failed or cancelled check satisfies
    // nothing. `checked_marks` is the count the last finished check (failed
    // or not) saw when it began: a waiter shares a finished failed check
    // only when no mark has landed since it began.
    settings_marks: AtomicU64,
    satisfied_marks: AtomicU64,
    checked_marks: AtomicU64,
    // Test hook: a settings check waits for this lock after it began and
    // before it reads the database, so a test can hold a check in flight.
    #[cfg(test)]
    pub(crate) settings_check_hold: tokio::sync::Mutex<()>,
    // Test counters: how many settings checks, and how many
    // `settings::load`s, ran after start-up.
    #[cfg(test)]
    pub(crate) settings_checks: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    pub(crate) settings_loads: std::sync::atomic::AtomicUsize,
    // The last clock check (design "Time"); `None` until the first one, and
    // always without a `time_server`. Written by the check loop only.
    clock_status: RwLock<Option<ClockStatus>>,
    // The OAuth authorization server's state; `None` without `public_url`.
    oauth: Option<OAuth>,
    // The sign-in routes' per-address and per-email limits.
    sign_in_limits: SignInLimits,
    // The configured mail provider (Decision 10), built once at start-up.
    mailer: Arc<dyn crate::mail::Mailer>,
    // The Forminit client `send_feedback`'s worker posts with; `None`
    // without `[feedback]` (issue #674).
    forminit: Option<Arc<crate::mail::feedback::Forminit>>,
    // The WebAuthn relying party and ceremonies; `None` without a domain
    // `public_url`.
    passkeys: Option<Passkeys>,
    // The configured OIDC providers; `None` without `public_url` or providers.
    oidc: Option<OidcProviders>,
    // Background writes of recorded accesses (admin reads). A `JoinSet`
    // (a set of spawned tasks that can be awaited together), so tests and
    // shutdown can wait for them; a `std` mutex, held only to spawn into it
    // or to take it out.
    access_writes: std::sync::Mutex<tokio::task::JoinSet<()>>,
    // Permits for the background writes of provisioner uses: a flood that
    // outruns the database drops its overflow instead of queueing tasks
    // without bound ([`App::note_provisioner_use`]).
    provisioner_writes: Arc<tokio::sync::Semaphore>,
    // The metric instruments (M15 Task 2); built on the global meter provider
    // unless a test supplies its own.
    metrics: Metrics,
    // Test hook: a background access write waits for this lock first, so a
    // test can hold it to run a deletion or a transfer between admission
    // and persistence.
    #[cfg(test)]
    pub(crate) access_hold: Arc<tokio::sync::Mutex<()>>,
}

impl App {
    /// Loads the key cache and the effective settings. No catalog is
    /// loaded: each loads into the catalog cache on its first use (M18 spec
    /// Decision 2), so start-up does not grow with the number of catalogs.
    ///
    /// # Arguments
    /// * `pool` - the database (migrated).
    /// * `config` - the server's settings.
    /// * `clock` - the source of every timestamp.
    ///
    /// # Returns
    /// The shared state.
    ///
    /// # Errors
    /// [`OkfError::Config`] (path `[redis]`) from [`shared::Redis::connect`]
    /// when `[redis]` is set and its variable is unset or blank, its value is
    /// not a Redis URL, or Redis does not answer `PING`;
    /// [`OkfError::Database`] reading the counter, the keys or the settings;
    /// [`OkfError::Config`] from [`OAuth::new`] or [`Passkeys::new`], or from
    /// [`crate::mail::from_config`] (a mail secret missing from the
    /// environment, or no AWS region for SES) or [`OidcProviders::new`] (an
    /// OIDC client secret missing from the environment).
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn load(
        pool: PgPool,
        config: Config,
        clock: Arc<dyn Clock>,
    ) -> Result<Arc<Self>, OkfError> {
        Self::load_with_metrics(pool, config, clock, Metrics::global()).await
    }

    /// [`App::load`] with the metric instruments given: tests pass ones built
    /// on a local provider, so they can read what a real `App` recorded.
    ///
    /// # Arguments
    /// * `pool` - the database (migrated).
    /// * `config` - the server's settings.
    /// * `clock` - the source of every timestamp.
    /// * `metrics` - the instruments the app records on.
    ///
    /// # Returns
    /// The shared state.
    ///
    /// # Errors
    /// As [`App::load`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn load_with_metrics(
        pool: PgPool,
        config: Config,
        clock: Arc<dyn Clock>,
        metrics: Metrics,
    ) -> Result<Arc<Self>, OkfError> {
        // `[redis]` set: connect now, so a bad URL or an unreachable Redis
        // refuses start-up. The URL comes from the named variable only.
        let redis = match &config.redis {
            Some(table) => {
                let redis =
                    shared::Redis::connect(table, &|name| std::env::var(name).ok(), &metrics)
                        .await?;
                // Finds a Redis user that may not run the rate limiter's
                // script now, not on the first request (issue #832).
                shared::limits::check(&redis).await;
                Some(redis)
            }
            None => None,
        };
        // Boxed so this future holds a pointer, not the whole of the load's
        // future: callers nest `App::load` inside big test futures, and the
        // extra level pushed rustc's layout query past its recursion limit.
        Box::pin(Self::load_with_redis(pool, config, clock, metrics, redis)).await
    }

    /// [`App::load_with_metrics`] with the Redis handle given (or none):
    /// tests pass one handle to two apps, as two replicas share one Redis.
    ///
    /// # Arguments
    /// * `pool`, `config`, `clock`, `metrics` - as for `load_with_metrics`.
    /// * `redis` - the shared Redis; `None` keeps everything in memory.
    ///
    /// # Returns
    /// The shared state.
    ///
    /// # Errors
    /// As [`App::load`], except that no `[redis]` connection is made here.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn load_with_redis(
        pool: PgPool,
        config: Config,
        clock: Arc<dyn Clock>,
        metrics: Metrics,
        redis: Option<shared::Redis>,
    ) -> Result<Arc<Self>, OkfError> {
        // Read the counter before the settings: a change committed between
        // them moves it again, so the next check reloads.
        let validated_at = clock.now();
        let admin_generation = catalogs::admin_generation(&pool).await?;
        let settings = settings::load(&pool, &config.auth).await?;
        let labels = LabelLookup::new(
            pool.clone(),
            DEFAULT_LABEL_CACHE_ENTRIES,
            config.credential_ttl,
        )
        .with_metrics(metrics.clone());
        // Shared with the credential cache, which resolves self-issued
        // credentials' ceilings against its rows (issue #728).
        let stores = Stores::new(pool.clone(), PoolSettings::from_config(&config));
        let directory = Arc::new(CatalogDirectory::new(
            pool.clone(),
            stores.clone(),
            config.catalog_directory_entries,
            config.catalog_ttl,
            metrics.clone(),
        ));
        // `clock` moves into `Self` below, so the caches get their own handle.
        let credentials = Credentials::new(
            pool.clone(),
            Arc::clone(&directory),
            Arc::clone(&clock),
            CredentialLimits {
                credentials: config.credential_cache_entries,
                users: config.user_cache_entries,
                ttl: config.credential_ttl,
            },
            metrics.clone(),
        );
        let oauth = OAuth::new(&config, redis.as_ref())?;
        let sessions = SessionBindings::new(redis.as_ref(), config.session_idle_timeout);
        let overlays = Overlays::new(
            config.session_idle_timeout,
            config.overlay_cache_bytes.as_u64(),
            metrics.clone(),
        );
        let mailer = crate::mail::from_config(&config.mail).await?;
        // A `[feedback] api_key_env` that names an unset variable fails
        // start-up here, as a mail secret does.
        let forminit = config
            .feedback
            .as_ref()
            .map(|feedback| {
                crate::mail::feedback::Forminit::from_config(feedback, &|name| {
                    std::env::var(name).ok()
                })
                .map(Arc::new)
            })
            .transpose()?;
        let passkeys = Passkeys::new(&config, redis.as_ref())?;
        // A provider whose secret variable is unset fails start-up here.
        let oidc = OidcProviders::new(&config, |name| std::env::var(name).ok(), redis.as_ref())?;
        // `if let Some(name) = ...`: Python's `if (name := ...) is not None`.
        // One directory read, not a load: the catalog loads when first used.
        if let Some(name) = &config.default_catalog {
            // Boxed: the directory's lookup is a deep future (moka's single
            // flight), and inlining it here pushed rustc's layout query past
            // its recursion limit for callers that nest this load.
            if Box::pin(directory.resolve(name)).await?.is_none() {
                tracing::warn!(catalog = %name, "default_catalog does not exist (yet)");
            }
        }
        let catalog_cache = CatalogCache::new(
            stores.clone(),
            config.catalog_cache_bytes.as_u64(),
            metrics.clone(),
        );
        let sign_in_limits = SignInLimits::new(redis.as_ref());
        let app = Arc::new(Self {
            accounts: AccountDb::new(pool.clone()),
            stores,
            config,
            clock,
            credentials,
            labels,
            sessions,
            redis,
            overlays,
            write_queue: KeyedLocks::new(),
            publish_locks: KeyedLocks::new(),
            rebuild_locks: KeyedLocks::new(),
            directory,
            catalog_cache,
            settings: RwLock::new(LoadedSettings {
                settings: Arc::new(settings),
                generation: admin_generation,
                validated_at,
            }),
            settings_reload: tokio::sync::Mutex::new(()),
            settings_rounds: AtomicU64::new(0),
            settings_marks: AtomicU64::new(0),
            satisfied_marks: AtomicU64::new(0),
            checked_marks: AtomicU64::new(0),
            #[cfg(test)]
            settings_check_hold: tokio::sync::Mutex::new(()),
            #[cfg(test)]
            settings_checks: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            settings_loads: std::sync::atomic::AtomicUsize::new(0),
            clock_status: RwLock::new(None),
            oauth,
            sign_in_limits,
            mailer,
            forminit,
            passkeys,
            oidc,
            access_writes: std::sync::Mutex::new(tokio::task::JoinSet::new()),
            provisioner_writes: Arc::new(tokio::sync::Semaphore::new(
                MAX_PENDING_PROVISIONER_WRITES,
            )),
            metrics,
            #[cfg(test)]
            access_hold: Arc::default(),
        });
        // `Arc::downgrade`: the gauges' callbacks live in the meter provider,
        // which outlives the app, so they hold a weak handle (no cycle).
        app.metrics.observe_pool(Arc::downgrade(&app));
        Ok(app)
    }

    /// Records one operator access in the background, off the request path
    /// (plan Decision 5): an admin read's hourly bucket, or an unlock's
    /// read, write or export tally. A failure is
    /// logged; [`App::settle_access`] waits for every write.
    ///
    /// # Arguments
    /// * `record` - the access, with identities and scope fixed at admission.
    /// * `kind` - read, write or export.
    /// * `admission` - how it was let in.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn note_access(&self, record: AccessRecord, kind: AccessKind, admission: Admission) {
        let pool = self.accounts.executor().clone();
        #[cfg(test)]
        let hold = Arc::clone(&self.access_hold);
        let mut writes = self
            .access_writes
            .lock()
            .expect("access writes lock poisoned");
        // Reap the finished ones, so the set holds only running writes.
        while writes.try_join_next().is_some() {}
        // `async move` hands `pool` and `record` to the task, which outlives
        // this call.
        writes.spawn(async move {
            #[cfg(test)]
            let _held = hold.lock().await;
            let written = match admission {
                Admission::AdminRead => audit::record_admin_read(&pool, &record).await,
                // The unlock id was fixed at admission; the record is not
                // needed for a tally.
                Admission::Unlocked { unlock_id } => unlocks::tally(&pool, unlock_id, kind).await,
                Admission::Customer | Admission::Untracked | Admission::Maintenance => Ok(()),
            };
            if let Err(error) = written {
                split_event!(
                    error,
                    { catalog = %record.catalog, ?kind, },
                    { %error, },
                    "recording an operator access failed"
                );
            }
        });
    }

    /// Records one provisioner request (an email lookup, or a refusal) in
    /// the background, off the request path. At most
    /// [`MAX_PENDING_PROVISIONER_WRITES`] writes are pending: a flood that
    /// outruns the database loses the overflow's counts (logged at debug)
    /// rather than queueing tasks without bound. A failure is logged.
    ///
    /// # Arguments
    /// * `usage` - the request, with the actor fixed at admission.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn note_provisioner_use(&self, usage: ProvisionerUse) {
        let Ok(permit) = Arc::clone(&self.provisioner_writes).try_acquire_owned() else {
            tracing::debug!("a provisioner use was not recorded: too many writes pending");
            return;
        };
        let pool = self.accounts.executor().clone();
        #[cfg(test)]
        let hold = Arc::clone(&self.access_hold);
        let mut writes = self
            .access_writes
            .lock()
            .expect("access writes lock poisoned");
        while writes.try_join_next().is_some() {}
        writes.spawn(async move {
            // The permit is released when the task ends, however it ends.
            let _permit = permit;
            #[cfg(test)]
            let _held = hold.lock().await;
            if let Err(error) = audit::record_provisioner_use(&pool, &usage).await {
                split_event!(
                    error,
                    {},
                    { %error, },
                    "recording a provisioner use failed"
                );
            }
        });
    }

    /// Waits for every background access write started so far (tests, and
    /// shutdown, so a stopping server loses no count).
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn settle_access(&self) {
        // Take the set out under the lock, then wait without holding it
        // (`mem::take` leaves an empty set in its place).
        let mut writes = std::mem::take(
            &mut *self
                .access_writes
                .lock()
                .expect("access writes lock poisoned"),
        );
        while writes.join_next().await.is_some() {}
    }

    /// The shared Redis; `None` without `[redis]`.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "one-line accessor")
    )]
    pub fn redis(&self) -> Option<&shared::Redis> {
        self.redis.as_ref()
    }

    /// The catalog directory: rows by name with their access facts.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "one-line accessor")
    )]
    pub fn directory(&self) -> &CatalogDirectory {
        &self.directory
    }

    /// The catalog cache: snapshots by catalog id, loaded on demand.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "one-line accessor")
    )]
    pub fn catalog_cache(&self) -> &CatalogCache {
        &self.catalog_cache
    }

    /// The instruments this app records on.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "one-line accessor")
    )]
    pub fn metrics(&self) -> &Metrics {
        &self.metrics
    }

    /// The account database: users, teams, keys, sessions and the catalog
    /// registry.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "one-line accessor")
    )]
    pub fn accounts(&self) -> &AccountDb {
        &self.accounts
    }

    /// The registry of catalog stores.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "one-line accessor")
    )]
    pub fn stores(&self) -> &Stores {
        &self.stores
    }

    /// The database holding `row`'s catalog data: its store, opened on first
    /// use.
    ///
    /// # Arguments
    /// * `row` - the catalog's directory row (`row.store`).
    ///
    /// # Returns
    /// The store's handle.
    ///
    /// # Errors
    /// As [`Stores::get`]: [`OkfError::StoreUnavailable`] when the store
    /// cannot be reached.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn catalog_db(&self, row: &CatalogRow) -> Result<CatalogDb, OkfError> {
        self.stores.get(&row.store).await
    }

    /// The database of the store called `store`, for a caller that holds a
    /// loaded catalog ([`crate::catalog::CatalogState::store`]) or a
    /// changeset's catalog and not its directory row.
    ///
    /// # Arguments
    /// * `store` - the store's name.
    ///
    /// # Returns
    /// The store's handle.
    ///
    /// # Errors
    /// As [`Stores::get`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn store_db(&self, store: &StoreName) -> Result<CatalogDb, OkfError> {
        self.stores.get(store).await
    }

    /// The `main` store's handle, for tests that import or read as another
    /// replica or the command line would.
    ///
    /// # Returns
    /// The handle.
    #[cfg(test)]
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "a test helper")
    )]
    pub(crate) async fn main_db(&self) -> CatalogDb {
        self.stores
            .get(&StoreName::main())
            .await
            .expect("the main store opens")
    }

    /// The server's settings.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The source of every timestamp.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn clock(&self) -> &dyn Clock {
        self.clock.as_ref()
    }

    /// The sign-in routes' rate limits.
    ///
    /// # Returns
    /// The limiters.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn sign_in_limits(&self) -> &SignInLimits {
        &self.sign_in_limits
    }

    /// The clock as a shared handle, for tasks that outlive a borrow of
    /// `App` (the mail worker).
    ///
    /// # Returns
    /// The same clock [`App::clock`] lends.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn clock_handle(&self) -> Arc<dyn Clock> {
        Arc::clone(&self.clock)
    }

    /// The configured mail provider.
    ///
    /// # Returns
    /// The mailer the mail worker sends with.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn mailer(&self) -> &Arc<dyn crate::mail::Mailer> {
        &self.mailer
    }

    /// The Forminit client the feedback worker posts with.
    ///
    /// # Returns
    /// The client, or `None` without `[feedback]`.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn forminit(&self) -> Option<&Arc<crate::mail::feedback::Forminit>> {
        self.forminit.as_ref()
    }

    /// The OIDC providers.
    ///
    /// # Returns
    /// `None` when none are configured (or there is no `public_url`).
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn oidc(&self) -> Option<&OidcProviders> {
        self.oidc.as_ref()
    }

    /// The credential and user caches (M18 Decision 9).
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn credentials(&self) -> &Credentials {
        &self.credentials
    }

    /// The label lookup: the labels of the slugs a response shows, read on
    /// demand ([`LabelLookup::labels_for`]).
    ///
    /// # Returns
    /// The lookup.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn labels(&self) -> &LabelLookup {
        &self.labels
    }

    /// What every principal of a request is assembled with besides its
    /// credential and user: the `require_unlock` setting, checked against
    /// the database when older than `credential_ttl`
    /// ([`App::fresh_settings`]).
    ///
    /// # Returns
    /// The view.
    #[tracing::instrument(level = "trace", skip_all)]
    pub async fn access_view(&self) -> AccessView {
        AccessView {
            require_unlock: self.fresh_settings().await.require_unlock,
        }
    }

    /// The principal of a presented API key ([`Credentials::find`]).
    ///
    /// # Arguments
    /// * `presented` - the key from the request.
    ///
    /// # Returns
    /// Its principal, or `None` for an unknown, revoked or expired key.
    ///
    /// # Errors
    /// [`OkfError::Database`] when its load fails; [`OkfError::Busy`] for a
    /// lookup that shared another's failed load.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn find_key(&self, presented: &str) -> Result<Option<Arc<Principal>>, OkfError> {
        self.credentials
            .find(presented, &self.access_view().await)
            .await
    }

    /// A live OAuth grant's principal ([`Credentials::find_grant`]).
    ///
    /// # Arguments
    /// * `grant_id` - the grant's id.
    ///
    /// # Returns
    /// Its principal, or `None` for an unknown, revoked or expired grant.
    ///
    /// # Errors
    /// As [`App::find_key`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn find_grant(&self, grant_id: &str) -> Result<Option<Arc<Principal>>, OkfError> {
        self.credentials
            .find_grant(grant_id, &self.access_view().await)
            .await
    }

    /// The principal of a console session's user
    /// ([`Credentials::session_principal`]).
    ///
    /// # Arguments
    /// * `user_id` - the signed-in user's id.
    ///
    /// # Returns
    /// The principal, or `None` once the user is gone.
    ///
    /// # Errors
    /// As [`App::find_key`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn session_principal(&self, user_id: &str) -> Result<Option<Principal>, OkfError> {
        self.credentials
            .session_principal(user_id, &self.access_view().await)
            .await
    }

    /// The effective sign-in policy, as of the last settings reload. Request
    /// handlers read it here, never from `server_settings`. Every HTTP
    /// request calls [`App::fresh_settings`] first, so what this returns
    /// during a request is never older than `credential_ttl` plus that
    /// request.
    ///
    /// # Returns
    /// A shared snapshot (cheap: an `Arc` clone, one atomic increment).
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn settings(&self) -> Arc<Settings> {
        Arc::clone(
            &self
                .settings
                .read()
                .expect("settings lock poisoned")
                .settings,
        )
    }

    /// The settings, checked against the database first when they are older
    /// than `credential_ttl` or marked stale (spec Decision 9: the TTL
    /// backstop for a lost `Settings` message). The check reads
    /// `admin_generation` and reloads the settings only if it moved.
    /// Callers that find them expired together run one check: the rest wait
    /// for it and take its result, a failed one included, whatever made it
    /// run (unless the settings were marked stale again after it began). A
    /// request that arrives after a failed check runs a check of its own. A failed check keeps serving the loaded
    /// settings, logs at `warn` and leaves them expired, so the next request
    /// tries again.
    ///
    /// # Returns
    /// The settings (an `Arc` clone).
    #[tracing::instrument(level = "trace", skip_all)]
    pub async fn fresh_settings(&self) -> Arc<Settings> {
        if let Some(settings) = self.settings_if_fresh() {
            return settings;
        }
        let round = self.settings_rounds.load(Ordering::Acquire);
        // The guard is held until it goes out of scope at the end of the
        // function, like a Python `with` block.
        let _reload = self.settings_reload.lock().await;
        // Another caller may have checked them while this one waited.
        if let Some(settings) = self.settings_if_fresh() {
            return settings;
        }
        // That check failed: take its answer rather than queue another
        // failing one, unless a mark landed after it began.
        let checked_meanwhile = self.settings_rounds.load(Ordering::Acquire) != round;
        let marked_since = self.settings_marks.load(Ordering::Acquire)
            != self.checked_marks.load(Ordering::Acquire);
        if checked_meanwhile && !marked_since {
            return self.settings();
        }
        // The marks this check answers: one that lands after this read is
        // newer than the check and stays unsatisfied. If this future is
        // dropped (a cancelled request) before the check finishes, nothing
        // below runs and nothing is satisfied.
        let marks = self.settings_marks.load(Ordering::Acquire);
        self.revalidate_settings(marks).await;
        self.checked_marks.store(marks, Ordering::Release);
        self.settings_rounds.fetch_add(1, Ordering::Release);
        self.settings()
    }

    /// Marks the settings stale and checks them at once: what a `Settings`
    /// message and a reconnect do.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn reload_settings(&self) {
        self.mark_settings_stale();
        self.fresh_settings().await;
    }

    /// Marks the settings for a check before their next use, whatever their
    /// age: every caller from now on waits for a check that began after
    /// this mark to succeed.
    #[tracing::instrument(level = "trace", skip_all)]
    fn mark_settings_stale(&self) {
        self.settings_marks.fetch_add(1, Ordering::AcqRel);
    }

    /// The loaded settings if they are neither marked stale nor older than
    /// `credential_ttl`. A validation time in the future (the clock stepped
    /// back) counts as expired, so a clock change cannot keep them forever.
    ///
    /// # Returns
    /// The settings, or `None` when they must be checked first.
    #[tracing::instrument(level = "trace", skip_all)]
    fn settings_if_fresh(&self) -> Option<Arc<Settings>> {
        // Under the read lock: a successful check swaps the settings and
        // satisfies its marks under the write lock, so no caller sees the
        // marks satisfied with the settings from before.
        let loaded = self.settings.read().expect("settings lock poisoned");
        let unsatisfied = self.settings_marks.load(Ordering::Acquire)
            != self.satisfied_marks.load(Ordering::Acquire);
        if unsatisfied {
            return None;
        }
        // `to_std` fails for a negative age.
        let age = self.clock.now().signed_duration_since(loaded.validated_at);
        let fresh = age
            .to_std()
            .is_ok_and(|age| age < self.config.credential_ttl);
        fresh.then(|| Arc::clone(&loaded.settings))
    }

    /// Checks the settings against `admin_generation` and reloads them if it
    /// moved. Called only under `settings_reload`. The counter is read
    /// before the settings, so a change committed between the two reads
    /// moves it again and the next check reloads. Nothing is changed until
    /// the check succeeds, so a failed or cancelled one leaves the settings
    /// as stale as they were.
    ///
    /// # Arguments
    /// * `marks` - the stale marks counted before this check began, which a
    ///   success satisfies.
    #[tracing::instrument(level = "debug", skip_all)]
    async fn revalidate_settings(&self, marks: u64) {
        #[cfg(test)]
        self.settings_checks.fetch_add(1, Ordering::Relaxed);
        #[cfg(test)]
        drop(self.settings_check_hold.lock().await);
        let checked_at = self.clock.now();
        let loaded_generation = self
            .settings
            .read()
            .expect("settings lock poisoned")
            .generation;
        let checking = async {
            let generation = catalogs::admin_generation(self.accounts.executor()).await?;
            if generation == loaded_generation {
                return Ok(None);
            }
            #[cfg(test)]
            self.settings_loads.fetch_add(1, Ordering::Relaxed);
            // A stored setting this server cannot read is not a failure:
            // `settings::load` falls back to the default.
            // The span and metric keep the key cache's name
            // (`okf.keycache.*`), which dashboards know them by.
            let loaded = settings::load(self.accounts.executor(), &self.config.auth)
                .instrument(tracing::info_span!("okf.keycache.reload"))
                .await;
            self.metrics.keycache_reload(if loaded.is_ok() {
                Outcome::Ok
            } else {
                Outcome::Error
            });
            Ok::<_, OkfError>(Some((generation, loaded?)))
        };
        // A check past its bound is a failed one: dropping the future
        // abandons its read (sqlx discards the connection), and nothing
        // below is written, so its marks stay unsatisfied.
        let checked = tokio::time::timeout(SETTINGS_CHECK_TIMEOUT, checking)
            .await
            .unwrap_or_else(|_| {
                Err(OkfError::Busy {
                    reason: "the settings check timed out".to_string(),
                })
            });
        match checked {
            Ok(reloaded) => {
                let mut loaded = self.settings.write().expect("settings lock poisoned");
                loaded.validated_at = checked_at;
                if let Some((generation, settings)) = reloaded {
                    loaded.generation = generation;
                    loaded.settings = Arc::new(settings);
                }
                self.satisfied_marks.store(marks, Ordering::Release);
            }
            Err(error) => {
                split_event!(
                    warn,
                    { kind = error.kind(), },
                    { %error, },
                    "settings check failed; serving the loaded settings until the next request retries"
                );
            }
        }
    }

    /// Which key opened each MCP session (the `/mcp` session binding).
    ///
    /// # Returns
    /// The bindings, shared by every `/mcp` request.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn sessions(&self) -> &SessionBindings {
        &self.sessions
    }

    /// The OAuth state.
    ///
    /// # Returns
    /// `None` when OAuth is off (no `public_url`).
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn oauth(&self) -> Option<&OAuth> {
        self.oauth.as_ref()
    }

    /// The passkey state.
    ///
    /// # Returns
    /// `None` when passkeys are off (no `public_url`, or an address host).
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn passkeys(&self) -> Option<&Passkeys> {
        self.passkeys.as_ref()
    }

    /// The overlay cache (M5 Decision 6).
    ///
    /// # Returns
    /// Every open changeset's cached overlay, shared by every tool call.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn overlays(&self) -> &Overlays {
        &self.overlays
    }

    /// The per-changeset write queue: `write::apply` holds a changeset's
    /// lock here before it begins its transaction, so writers waiting their
    /// turn on one changeset hold no pooled connection.
    ///
    /// # Returns
    /// The locks, keyed by [`ChangesetKey`] (store-local ids collide).
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn write_queue(&self) -> &KeyedLocks<ChangesetKey> {
        &self.write_queue
    }

    /// Waits for the in-process half of a catalog's publish lock (design
    /// "Publish"): one publish or discard of the catalog at a time in this
    /// server. Taken before any pooled connection or Postgres lock; the
    /// other half is the advisory lock in the publish transaction.
    ///
    /// # Arguments
    /// * `catalog_id` - the catalog's database id ([`CatalogState::id`]).
    ///
    /// # Returns
    /// The guard; the lock is held until it is dropped.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn publish_lock(&self, catalog_id: i64) -> KeyedGuard<'_> {
        self.publish_locks.lock(catalog_id).await
    }

    /// The catalog rebuild locks, by catalog id, that
    /// [`CatalogState::refresh`] takes so one catalog builds one snapshot at
    /// a time, whichever of its states asks.
    ///
    /// # Returns
    /// The locks.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn rebuild_locks(&self) -> &KeyedLocks {
        &self.rebuild_locks
    }

    /// The last clock check, if any (`/healthz` shows it).
    ///
    /// # Returns
    /// A copy of the status, or `None` without a `time_server` or before the
    /// first check.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn clock_status(&self) -> Option<ClockStatus> {
        // A std lock is "poisoned" when a thread panicked while holding it;
        // nothing here can panic under the lock, so that is an invariant.
        self.clock_status
            .read()
            .expect("clock status lock poisoned")
            .clone()
    }

    /// Records a clock check.
    ///
    /// # Arguments
    /// * `status` - the check's outcome.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn set_clock_status(&self, status: ClockStatus) {
        *self
            .clock_status
            .write()
            .expect("clock status lock poisoned") = Some(status);
    }

    /// The directory row of catalog `name`: whether it exists, and its
    /// owner, description and access facts, without loading its content.
    ///
    /// # Arguments
    /// * `name` - the catalog's name.
    ///
    /// # Returns
    /// The row, or `None` if there is no such catalog.
    ///
    /// # Errors
    /// As [`CatalogDirectory::resolve`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn catalog_row(&self, name: &str) -> Result<Option<Arc<CatalogRow>>, OkfError> {
        self.directory.resolve(name).await
    }

    /// The state of the catalog `row` describes, at `row.generation` or
    /// newer: from the catalog cache, refreshed in place, or loaded. The
    /// state is that incarnation's (`row.id`), never another catalog's
    /// created since under the name.
    ///
    /// # Arguments
    /// * `row` - the catalog's directory row ([`App::catalog_row`]).
    ///
    /// # Returns
    /// The state.
    ///
    /// # Errors
    /// As [`CatalogCache::get`]: [`OkfError::CatalogNotSelected`] for a
    /// catalog deleted since, or whose stored data does not build;
    /// [`OkfError::Database`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn catalog_state(&self, row: &CatalogRow) -> Result<Arc<CatalogState>, OkfError> {
        self.catalog_cache.get(row, &self.rebuild_locks).await
    }

    /// [`App::catalog_state`] for a listing's row: the state for a listed
    /// catalog's snapshot fields, or `None` for one to leave out of the
    /// page because it was deleted since the page was read or its stored
    /// data does not build (as a catalog that was not loaded was left out
    /// before milestone 18).
    ///
    /// # Arguments
    /// * `row` - a listed catalog's row.
    ///
    /// # Returns
    /// The state, or `None`.
    ///
    /// # Errors
    /// [`OkfError::Database`], or [`OkfError::Busy`] for a load shared with
    /// another request that failed.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn listed_state(
        &self,
        row: &CatalogRow,
    ) -> Result<Option<Arc<CatalogState>>, OkfError> {
        match self.catalog_state(row).await {
            Ok(state) => Ok(Some(state)),
            Err(OkfError::CatalogNotSelected { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// The state of catalog `name`: [`App::catalog_row`], then
    /// [`App::catalog_state`]. For a caller that has only a name and no
    /// request context (the archive routes, tests).
    ///
    /// # Arguments
    /// * `name` - the catalog's name.
    ///
    /// # Returns
    /// The state, or `None` if there is no such catalog, or it was deleted
    /// meanwhile, or its stored data does not build (as a catalog that was
    /// not loaded answered before).
    ///
    /// # Errors
    /// [`OkfError::Database`], or [`OkfError::Busy`] for a load shared with
    /// another request that failed.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn catalog(&self, name: &str) -> Result<Option<Arc<CatalogState>>, OkfError> {
        match self.catalog_row(name).await? {
            Some(row) => self.listed_state(&row).await,
            None => Ok(None),
        }
    }

    /// Tells the other replicas that a catalog's published generation moved
    /// (issue #659), after the write that moved it committed. Best effort and
    /// detached: the publish happens on its own task, so a slow or failing
    /// Redis never delays the caller's answer; a failure is counted and
    /// logged, and the change feed's `catalog` message (and, failing both,
    /// `catalog_ttl`) covers it. Because nothing
    /// waits for the publish or the other replicas' refresh, this is prompt
    /// eventual consistency, not read-your-writes: the caller's next read on
    /// another replica may still see the old snapshot. Does nothing without
    /// `[redis]`.
    ///
    /// # Arguments
    /// * `change` - the catalog and the generation its write committed.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn announce_catalog(&self, change: CatalogGeneration) {
        let Some(redis) = self.redis.clone() else {
            return;
        };
        // `in_current_span` keeps the publish under the caller's trace,
        // although the task may outlive the caller.
        tokio::spawn(shared::announce::publish_catalog(redis, change).in_current_span());
    }

    /// Answers one catalog announcement: refreshes the cached catalog with
    /// that id unless its snapshot already reached the announced
    /// generation ([`CatalogCache::catch_up`]). Each call is the root of its
    /// own trace (`okf.catalog.announcement`) and is counted in
    /// `okf.catalog.announcements`.
    ///
    /// A catalog this replica has not cached is left alone (it loads when
    /// first used), and so is one deleted since: announcements never load
    /// anything (no load storm). Concurrent refreshes of one catalog, from
    /// here, the change feed, a request or a local publish, are serialized
    /// by [`CatalogState::refresh`]'s rebuild lock: the first rebuilds, the
    /// rest see the generation already reached and return, so a snapshot
    /// never moves back and a generation is built once.
    ///
    /// # Arguments
    /// * `change` - the announced catalog and generation.
    ///
    /// # Returns
    /// What the announcement came to.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "makes the okf.catalog.announcement root span itself"
        )
    )]
    pub async fn apply_catalog_announcement(
        &self,
        change: CatalogGeneration,
    ) -> AnnouncementResult {
        // `parent: None`: a background root, like each change-feed message.
        let span = tracing::info_span!(
            parent: None,
            "okf.catalog.announcement",
            okf.catalog = tracing::field::Empty,
            okf.announcement.result = tracing::field::Empty,
        );
        let result = self
            .refresh_announced(change, &span)
            .instrument(span.clone())
            .await;
        span.record("okf.announcement.result", result.as_str());
        self.metrics.catalog_announcement(result);
        result
    }

    /// The body of [`App::apply_catalog_announcement`], inside its span.
    ///
    /// # Arguments
    /// * `change` - the announced catalog and generation.
    /// * `span` - the announcement's span, to record the catalog's name on.
    ///
    /// # Returns
    /// What the announcement came to.
    #[tracing::instrument(level = "debug", skip_all)]
    async fn refresh_announced(
        &self,
        change: CatalogGeneration,
        span: &tracing::Span,
    ) -> AnnouncementResult {
        if let Some(state) = self.catalog_cache.resident(change.catalog_id).await {
            span.record("okf.catalog", state.name());
        }
        let caught_up = self
            .catalog_cache
            .catch_up(change.catalog_id, change.generation, &self.rebuild_locks)
            .await;
        match caught_up {
            Ok(CatchUp::Applied) => AnnouncementResult::Applied,
            Ok(CatchUp::Current) => AnnouncementResult::Current,
            Ok(CatchUp::NotResident) | Err(OkfError::CatalogNotSelected { .. }) => {
                AnnouncementResult::Unknown
            }
            Err(error) => {
                split_event!(
                    error,
                    { catalog_id = change.catalog_id, kind = error.kind(), },
                    { %error, },
                    "refresh after an announcement failed; the next use retries"
                );
                AnnouncementResult::Failed
            }
        }
    }

    /// Applies one change-feed message to this replica's caches: the
    /// catalog directory, the credential, user and label caches, the catalog
    /// cache and the settings.
    ///
    /// A `Catalog` message refreshes that catalog if it is cached and behind
    /// (the body of [`App::apply_catalog_announcement`]). A `CatalogMeta`
    /// message for a catalog that no longer exists under that id (deleted)
    /// drops it from the catalog cache; a catalog that merely changed its
    /// description, owner or maintenance keeps its snapshot, since those
    /// live in the directory row. A `Settings` message reloads the settings.
    /// The caches apply every other kind themselves.
    ///
    /// # Arguments
    /// * `message` - what changed.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn apply(&self, message: &crate::invalidate::Message) {
        use crate::invalidate::Invalidation;
        // The directory and the credentials first: they are a few map
        // operations, and nothing below should be able to delay dropping a
        // row or a revoked credential; the catalog cache below reads the
        // row the directory now holds.
        self.directory.apply(&message.invalidation).await;
        self.credentials.apply(&message.invalidation).await;
        self.labels.apply(&message.invalidation).await;
        match &message.invalidation {
            Invalidation::Catalog { id, generation } => {
                // The announcement's body, without its span and counter:
                // this is the change feed, counted by its own metrics.
                self.refresh_announced(
                    CatalogGeneration {
                        catalog_id: *id,
                        generation: *generation,
                    },
                    &tracing::Span::none(),
                )
                .await;
            }
            Invalidation::Settings => self.reload_settings().await,
            // Every replica reopens the store's pool on its next use, with
            // the row as it is now.
            Invalidation::Store { name } => self.stores.forget(name).await,
            Invalidation::CatalogMeta { id, name } => {
                self.forget_if_deleted(*id, name).await;
            }
            Invalidation::User { .. }
            | Invalidation::Team { .. }
            | Invalidation::Credential { .. } => {}
        }
    }

    /// Applies this replica's own writes to its catalog directory, its
    /// credential, user and label caches and its settings before the request
    /// that made them answers ([`crate::invalidate::capturing`]):
    /// the next request on this replica reads the new rows, without waiting
    /// for the change feed to deliver the same messages. Only in-process
    /// cache drops; nothing here reads the database (a dropped row is read
    /// again by the next lookup, and a snapshot behind its row is refreshed
    /// on its next use). The change feed still applies each message in full
    /// when it arrives.
    ///
    /// # Arguments
    /// * `written` - the invalidations the request's writes queued.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn apply_own_writes(&self, written: &[crate::invalidate::Invalidation]) {
        for invalidation in written {
            self.directory.apply(invalidation).await;
            self.credentials.apply(invalidation).await;
            self.labels.apply(invalidation).await;
            // A mark, not a reload: the next request checks the settings
            // ([`App::fresh_settings`]) whatever their age.
            match invalidation {
                crate::invalidate::Invalidation::Settings => self.mark_settings_stale(),
                crate::invalidate::Invalidation::Store { name } => self.stores.forget(name).await,
                _ => {}
            }
        }
    }

    /// Drops catalog `id` from the catalog cache if `name` no longer names
    /// it (the catalog was deleted, perhaps created again under the name).
    /// A catalog that is not cached costs nothing; one that is costs one
    /// directory read, at the rate of administrative writes.
    ///
    /// # Arguments
    /// * `id` - the catalog's id, from the message.
    /// * `name` - its name, from the message.
    #[tracing::instrument(level = "debug", skip_all)]
    async fn forget_if_deleted(&self, id: i64, name: &str) {
        if self.catalog_cache.resident(id).await.is_none() {
            return;
        }
        // A failed read is treated as gone: the entry is only a cache, and
        // the next use loads it again if the catalog is still there.
        let still_there = matches!(
            self.directory.resolve(name).await,
            Ok(Some(row)) if row.id == id
        );
        if !still_there {
            self.catalog_cache.forget(id).await;
        }
    }

    /// After the change feed's connection was lost (or a payload could not
    /// be read): messages sent meanwhile are gone, so every cache is
    /// suspect (spec Decision 12). The directory, credential, user and label
    /// caches mark every entry stale (each is read again on its next use,
    /// and with a catalog's row its cached snapshot is checked); the
    /// settings are checked.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn apply_reconnect(&self) {
        self.catch_up_catalogs().await;
        self.credentials.mark_all_stale();
        self.labels.mark_all_stale();
        // A `stores` change may have been missed: every pool reopens from
        // its row on next use.
        self.stores.forget_all().await;
        self.reload_settings().await;
    }

    /// What a store's change feed does when its connection was lost or a
    /// payload could not be read: messages sent meanwhile are gone, so the
    /// directory drops the rows of that store's catalogs and rereads them on
    /// next use. Nothing else is touched: another store's catalogs, the
    /// credentials, labels, settings and the pools stay as they are.
    ///
    /// # Arguments
    /// * `store` - the store whose feed was interrupted.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn apply_store_reconnect(&self, store: &crate::stores::StoreName) {
        self.directory.drop_store(store).await;
    }

    /// Makes every cached catalog revalidate on its next use: what the
    /// catalog subscriber does when it (re)subscribes, since announcements
    /// sent while it was not subscribed are lost, and what a lost change
    /// feed does. The directory's rows are marked stale, so each is read
    /// again on its next use, and [`App::catalog_state`] refreshes a
    /// snapshot that is behind its row. Nothing is read now (no load storm).
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn catch_up_catalogs(&self) {
        self.directory.mark_all_stale().await;
    }

    /// Checks the clock at once and then every [`clock_check::CHECK_INTERVAL`]
    /// until `shutdown` is cancelled. Returns at once without a `time_server`.
    ///
    /// # Arguments
    /// * `shutdown` - cancelled when the server stops.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "long-lived loop: its span would never close and would stay open as the parent of every later span"
        )
    )]
    pub async fn check_clock_until(self: Arc<Self>, shutdown: CancellationToken) {
        // `let ... else`: return when there is no server to ask.
        let Some(server) = self.config.time_server.clone() else {
            return;
        };
        let mut ticks = tokio::time::interval(clock_check::CHECK_INTERVAL);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            // The first tick completes at once: the startup check.
            tokio::select! {
                () = shutdown.cancelled() => return,
                _ = ticks.tick() => {}
            }
            // An async call does nothing until awaited: `check` is the
            // query, not yet started, for the `select!` below to drive.
            // Each check is the root of its own trace, whatever started the
            // loop (spec "Background roots").
            // Kept as a handle: the result is recorded on this span, not on
            // whatever span is current when the check finishes.
            let root = tracing::info_span!(
                parent: None,
                "okf.clock.check",
                okf.outcome = tracing::field::Empty,
                okf.clock.offset = tracing::field::Empty,
                otel.status_code = tracing::field::Empty,
            );
            let check = clock_check::check_once(&server, self.clock(), clock_check::QUERY_TIMEOUT)
                .instrument(root.clone());
            // A stop during a query ends it at once instead of waiting up to
            // `QUERY_TIMEOUT` (Brooke's review of PR #138).
            tokio::select! {
                () = shutdown.cancelled() => return,
                status = check => {
                    let outcome = if status.ok { Outcome::Ok } else { Outcome::Error };
                    self.metrics.clock_check(outcome, status.offset_ms);
                    root.record("okf.outcome", outcome.as_str());
                    match status.offset_ms {
                        // Seconds, as an `f64`, like the gauge.
                        Some(offset_ms) => {
                            root.record("okf.clock.offset", offset_ms as f64 / 1000.0);
                        }
                        // A failed check is a failed trace: the WARN event
                        // alone does not set the span's status.
                        None => {
                            root.record("otel.status_code", "error");
                        }
                    }
                    self.set_clock_status(status);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::Files;
    use crate::clock::SystemClock;
    use crate::db::test_db;
    use crate::mcp::testing::app_with;
    use crate::okf::workspace::StoredFile;
    use crate::store::audit::AuditActor;
    use crate::store::import::{import, ImportOptions, ImportScope};
    use crate::store::keys as key_rows;
    use chrono::Utc;
    use std::time::{Duration, Instant};

    async fn import_files(pool: &PgPool, catalog: &str, files: &[(&str, &str)], replace: bool) {
        let files: Files = files
            .iter()
            .map(|(path, text)| {
                let file = StoredFile {
                    content: text.as_bytes().to_vec(),
                    executable: false,
                };
                (path.to_string(), file)
            })
            .collect();
        let options = ImportOptions {
            replace,
            scope: ImportScope::Everything,
            expect_generation: None,
            expect_catalog_id: None,
            actor: "tester",
            audit: &crate::store::audit::AuditActor::cli(),
            now: Utc::now(),
        };
        import(
            &crate::stores::CatalogDb::main_in(pool),
            catalog,
            files,
            &options,
        )
        .await
        .unwrap();
    }

    async fn new_key(pool: &PgPool, id: &str) -> String {
        key_rows::insert_legacy_test_key(pool, id, key_rows::Access::Read, "tester", Utc::now())
            .await
    }

    async fn app(pool: &PgPool) -> Arc<App> {
        App::load(pool.clone(), Config::default(), Arc::new(SystemClock))
            .await
            .unwrap()
    }

    /// A change-feed message as the listener would parse it off the wire.
    fn message(invalidation: crate::invalidate::Invalidation) -> crate::invalidate::Message {
        crate::invalidate::Message {
            invalidation,
            sent_at_ms: Utc::now().timestamp_millis(),
        }
    }

    async fn create_pg(pool: &PgPool) -> i64 {
        catalogs::create(
            pool,
            &crate::store::catalogs::NewCatalog {
                name: "pg",
                display_name: &crate::labels::DisplayName::default(),
                description: "",
                owner_team: crate::db::TEST_TEAM,
            },
            &crate::store::audit::AuditActor::cli(),
            Utc::now(),
        )
        .await
        .unwrap();
        catalogs::list(pool).await.unwrap()[0].id
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_catalog_invalidation_refreshes_that_catalog() {
        use crate::invalidate::Invalidation;
        let db = test_db().await;
        let id = create_pg(&db.pool).await;
        let app = app(&db.pool).await;
        let before = app
            .catalog("pg")
            .await
            .unwrap()
            .unwrap()
            .current()
            .generation();

        // Written on the pool directly, as another replica would.
        import_files(&db.pool, "pg", &[("a.md", "# A\n")], false).await;
        let generation = catalogs::test_generation(&db.pool, "pg").await;
        assert!(generation > before);
        assert_eq!(
            app.catalog("pg")
                .await
                .unwrap()
                .unwrap()
                .current()
                .generation(),
            before
        );

        app.apply(&message(Invalidation::Catalog { id, generation }))
            .await;

        assert_eq!(
            app.catalog("pg")
                .await
                .unwrap()
                .unwrap()
                .current()
                .generation(),
            generation
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn change_feed_messages_reach_the_catalog_directory() {
        use crate::invalidate::Invalidation;
        let db = test_db().await;
        let id = create_pg(&db.pool).await;
        let app = app(&db.pool).await;
        assert!(app.directory().resolve("pg").await.unwrap().is_some());
        assert!(app.directory().resolve("later").await.unwrap().is_none());
        sqlx::query("UPDATE catalogs SET description = 'second' WHERE id = $1")
            .bind(id)
            .execute(&db.pool)
            .await
            .unwrap();

        app.apply(&message(Invalidation::CatalogMeta {
            id,
            name: "pg".to_owned(),
        }))
        .await;
        let row = app.directory().resolve("pg").await.unwrap().unwrap();
        assert_eq!(row.description, "second");

        // A lost connection forgets the remembered absence as well.
        sqlx::query("INSERT INTO catalogs (name, created_at) VALUES ('later', now())")
            .execute(&db.pool)
            .await
            .unwrap();
        assert!(app.directory().resolve("later").await.unwrap().is_none());
        app.apply_reconnect().await;
        assert!(app.directory().resolve("later").await.unwrap().is_some());
    }

    /// Every store write a directory row reflects (details, maintenance,
    /// suspension, owner, delete) sends a message that makes the directory
    /// reread it: each write's own messages, applied here as the listener
    /// would, and nothing else.
    #[tokio::test(flavor = "multi_thread")]
    async fn each_catalog_writers_messages_refresh_its_directory_row() {
        use crate::invalidate::testing::invalidations_of;
        use crate::store::audit::AuditActor;
        use crate::store::catalogs::{DetailsChange, MaintenanceChange, Switch};
        use crate::store::teams;
        let db = test_db().await;
        create_pg(&db.pool).await;
        let app = app(&db.pool).await;
        let cli = AuditActor::cli();
        let row = || async { app.directory().resolve("pg").await.unwrap() };
        // Applies what `write` sent.
        let applied = |sent: Vec<crate::invalidate::Invalidation>| async {
            for invalidation in sent {
                app.apply(&message(invalidation)).await;
            }
        };
        assert!(row().await.is_some_and(|row| !row.maintenance_off));

        let details = DetailsChange {
            catalog: "pg",
            display_name: None,
            description: Some("Notes."),
            owner: None,
        };
        applied(
            invalidations_of(&db.pool, async {
                catalogs::update_details(&db.pool, &details, &cli, Utc::now())
                    .await
                    .unwrap();
            })
            .await,
        )
        .await;
        assert_eq!(row().await.unwrap().description, "Notes.");

        let off = MaintenanceChange {
            catalog: "pg",
            to: Switch::Off,
            owner: None,
        };
        applied(
            invalidations_of(&db.pool, async {
                catalogs::set_maintenance(&db.pool, &off, &cli, Utc::now())
                    .await
                    .unwrap();
            })
            .await,
        )
        .await;
        assert!(row().await.unwrap().maintenance_off);

        applied(
            invalidations_of(&db.pool, async {
                teams::suspend(&db.pool, crate::db::TEST_TEAM, None, &cli, Utc::now())
                    .await
                    .unwrap();
            })
            .await,
        )
        .await;
        assert!(row().await.unwrap().suspended.is_some());

        let other = crate::store::users::NewUser {
            name: "bob",
            display_name: &crate::labels::DisplayName::default(),
        };
        crate::store::users::create(&db.pool, &other, &cli, Utc::now())
            .await
            .unwrap();
        applied(
            invalidations_of(&db.pool, async {
                catalogs::set_owner(&db.pool, "pg", "bob", &cli, Utc::now())
                    .await
                    .unwrap();
            })
            .await,
        )
        .await;
        let moved = row().await.unwrap();
        assert_eq!(moved.owner_team.as_deref(), Some("bob"));
        assert!(moved.suspended.is_none(), "bob's team is not suspended");

        applied(
            invalidations_of(&db.pool, async {
                catalogs::delete_settled(&db.pool, "pg", false, &cli, Utc::now())
                    .await
                    .unwrap();
            })
            .await,
        )
        .await;
        assert!(row().await.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_credential_invalidation_reloads_the_key_cache() {
        use crate::invalidate::Invalidation;
        let db = test_db().await;
        let key = new_key(&db.pool, "reader").await;
        let app = app(&db.pool).await;
        assert!(app.find_key(&key).await.unwrap().is_some());

        key_rows::revoke(
            &db.pool,
            "reader",
            &crate::store::audit::AuditActor::cli(),
            Utc::now(),
        )
        .await
        .unwrap();
        // Still cached: nothing told this replica.
        assert!(app.find_key(&key).await.unwrap().is_some());

        app.apply(&message(Invalidation::Credential {
            key_id: "reader".to_owned(),
        }))
        .await;

        assert!(app.find_key(&key).await.unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_lost_connection_marks_everything_stale() {
        let db = test_db().await;
        create_pg(&db.pool).await;
        let key = new_key(&db.pool, "reader").await;
        let app = app(&db.pool).await;
        let before = app
            .catalog("pg")
            .await
            .unwrap()
            .unwrap()
            .current()
            .generation();

        import_files(&db.pool, "pg", &[("a.md", "# A\n")], false).await;
        key_rows::revoke(
            &db.pool,
            "reader",
            &crate::store::audit::AuditActor::cli(),
            Utc::now(),
        )
        .await
        .unwrap();

        // The directory is marked stale: the next use reads the row again
        // and refreshes the cached snapshot to it.
        app.apply_reconnect().await;

        assert!(
            app.catalog("pg")
                .await
                .unwrap()
                .unwrap()
                .current()
                .generation()
                > before
        );
        assert!(app.find_key(&key).await.unwrap().is_none());
    }

    /// Creates catalog `name`, owned by the fixture team.
    async fn create(pool: &PgPool, name: &str) -> catalogs::Catalog {
        catalogs::create(
            pool,
            &crate::store::catalogs::NewCatalog {
                name,
                display_name: &crate::labels::DisplayName::default(),
                description: "",
                owner_team: crate::db::TEST_TEAM,
            },
            &AuditActor::cli(),
            Utc::now(),
        )
        .await
        .unwrap()
    }

    use crate::invalidate::testing::applied;

    #[tokio::test(flavor = "multi_thread")]
    async fn load_holds_no_catalog_and_every_live_key() {
        let db = test_db().await;
        create(&db.pool, "pg").await;
        create(&db.pool, "work").await;
        import_files(&db.pool, "pg", &[("a.md", "# A\n")], false).await;
        let key = new_key(&db.pool, "reader").await;

        let app = app(&db.pool).await;

        app.catalog_cache().settle().await;
        assert_eq!(
            app.catalog_cache().cached(),
            0,
            "nothing loaded at start-up"
        );
        assert_eq!(app.find_key(&key).await.unwrap().unwrap().key_id, "reader");
        // Each catalog loads on its first use.
        let pg = app.catalog("pg").await.unwrap().unwrap();
        assert_eq!(pg.current().files(), 1);
        assert!(app.catalog("work").await.unwrap().is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn catalogs_changed_beside_the_server_are_served_after_their_messages() {
        let db = test_db().await;
        create(&db.pool, "pg").await;
        create(&db.pool, "gone").await;
        let app = app(&db.pool).await;
        let pg_before = app.catalog("pg").await.unwrap().unwrap();
        assert!(app.catalog("gone").await.unwrap().is_some());
        assert!(app.catalog("work").await.unwrap().is_none());

        // Changes made by the subcommands, beside the running server.
        applied(&app, async {
            import_files(&db.pool, "pg", &[("a.md", "# A\n")], false).await;
            create(&db.pool, "work").await;
            catalogs::delete_settled(&db.pool, "gone", false, &AuditActor::cli(), Utc::now())
                .await
                .unwrap();
        })
        .await;

        // The same state, refreshed in place: readers holding it see the new snapshot.
        let pg_after = app.catalog("pg").await.unwrap().unwrap();
        assert!(Arc::ptr_eq(&pg_before, &pg_after));
        assert_eq!(pg_after.current().files(), 1);
        assert!(app.catalog("work").await.unwrap().is_some());
        assert!(app.catalog("gone").await.unwrap().is_none());
    }

    /// A write's own invalidations, collected around it and applied here,
    /// reach the directory and the label cache at once, with no listener
    /// and no poll.
    #[tokio::test(flavor = "multi_thread")]
    async fn own_writes_reach_the_directory_and_the_labels() {
        let db = test_db().await;
        create(&db.pool, "pg").await;
        let app = app(&db.pool).await;
        let mut wanted = crate::labels::directory::Wanted::default();
        wanted.catalog("pg");
        // Both cached before the write.
        let row = app.catalog_row("pg").await.unwrap().expect("pg");
        app.labels().labels_for(&wanted).await.unwrap();
        let renamed = crate::labels::DisplayName::parse("Renamed").unwrap();
        let change = catalogs::DetailsChange {
            catalog: "pg",
            display_name: Some(&renamed),
            description: Some("Changed."),
            owner: None,
        };

        let ((), written) = crate::invalidate::capturing(async {
            catalogs::update_details(&db.pool, &change, &AuditActor::cli(), Utc::now())
                .await
                .unwrap();
        })
        .await;
        app.apply_own_writes(&written).await;

        assert!(!written.is_empty(), "the write queued its invalidations");
        let after = app.catalog_row("pg").await.unwrap().expect("pg");
        assert_ne!(after.description, row.description);
        let labels = app.labels().labels_for(&wanted).await.unwrap();
        assert_eq!(labels.catalog("pg"), Some("Renamed"));
    }

    /// A settings write's own `Settings` invalidation, applied here, makes
    /// this replica's next request check the settings although they are
    /// young: the writer reads its own write with no listener.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_own_settings_write_reaches_the_next_request() {
        let db = test_db().await;
        let app = app(&db.pool).await;
        let before = app.fresh_settings().await.max_keys_per_user;

        let ((), written) = crate::invalidate::capturing(async {
            set_setting(&db.pool, settings::SettingKey::MaxKeysPerUser, "5").await;
        })
        .await;
        app.apply_own_writes(&written).await;

        assert_eq!(before, 20);
        assert_eq!(
            written,
            [crate::invalidate::Invalidation::Settings],
            "the write queued its invalidation"
        );
        assert_eq!(app.fresh_settings().await.max_keys_per_user, 5);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_revocations_message_drops_the_cached_key() {
        let db = test_db().await;
        let key = new_key(&db.pool, "reader").await;
        let app = app(&db.pool).await;
        // Cached first: only a cached key can outlive its revocation.
        assert!(app.find_key(&key).await.unwrap().is_some());

        applied(&app, async {
            key_rows::revoke(&db.pool, "reader", &AuditActor::cli(), Utc::now())
                .await
                .unwrap();
        })
        .await;

        assert!(app.find_key(&key).await.unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_catalog_created_again_under_its_name_is_loaded_from_scratch() {
        let db = test_db().await;
        let first = create(&db.pool, "pg").await;
        let app = app(&db.pool).await;
        let old = app.catalog("pg").await.unwrap().unwrap();
        assert_eq!(old.id(), first.id);

        applied(&app, async {
            catalogs::delete_settled(&db.pool, "pg", false, &AuditActor::cli(), Utc::now())
                .await
                .unwrap();
            create(&db.pool, "pg").await;
        })
        .await;

        let pg = app.catalog("pg").await.unwrap().unwrap();
        assert_ne!(pg.id(), first.id);
        assert!(app.catalog_cache().resident(first.id).await.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_re_created_catalog_that_fails_to_load_is_not_served_as_the_old_one() {
        let db = test_db().await;
        create(&db.pool, "pg").await;
        import_files(&db.pool, "pg", &[("a.md", "# A\n")], false).await;
        let app = app(&db.pool).await;
        assert!(app.catalog("pg").await.unwrap().is_some());

        applied(&app, async {
            catalogs::delete_settled(&db.pool, "pg", false, &AuditActor::cli(), Utc::now())
                .await
                .unwrap();
            let again = create(&db.pool, "pg").await;
            // A row no workspace accepts (a path escaping the catalog).
            sqlx::query(
                "INSERT INTO catalog.documents (catalog_id, path, content, executable, revision, \
                                        updated_at, updated_by) \
                 VALUES ($1, '../escape.md', '', false, 1, now(), 'test')",
            )
            .bind(again.id)
            .execute(&crate::db::store_pool(&db.pool))
            .await
            .unwrap();
        })
        .await;

        assert!(
            app.catalog("pg").await.unwrap().is_none(),
            "the deleted catalog is not served"
        );
    }

    async fn create_catalog_main(pool: &PgPool) {
        catalogs::create(
            pool,
            &crate::store::catalogs::NewCatalog {
                name: "main",
                display_name: &crate::labels::DisplayName::default(),
                description: "",
                owner_team: crate::db::TEST_TEAM,
            },
            &crate::store::audit::AuditActor::cli(),
            Utc::now(),
        )
        .await
        .unwrap();
    }

    /// Suspends the test team with no change message: the row and
    /// `admin_generation` in one transaction.
    async fn suspend_silently(pool: &PgPool) {
        let mut tx = pool.begin().await.unwrap();
        sqlx::query(
            "UPDATE teams SET suspended_at = now(), suspension_notice = 'Paused.' WHERE name = $1",
        )
        .bind(crate::db::TEST_TEAM)
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query("UPDATE server_state SET admin_generation = admin_generation + 1")
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    async fn suspended(app: &App, writer: &crate::auth::Principal) -> bool {
        matches!(
            writer
                .require_in(app.directory(), key_rows::Access::Read, "main")
                .await,
            Err(OkfError::Suspended { .. })
        )
    }

    /// The backstop for a message that never arrives (a listener that is up
    /// but missed one, or an older replica's write: spec Decision 13), now
    /// that no poll sweeps the caches: a silently revoked key, a silently
    /// suspended catalog row and a silently renamed label are each read again
    /// once their TTL (`credential_ttl`, `catalog_ttl`) has passed, with no
    /// message, no reconnect and no announcement.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_change_no_message_described_is_read_after_the_ttls() {
        const TTL: Duration = Duration::from_millis(200);
        let db = test_db().await;
        create_catalog_main(&db.pool).await;
        let key = new_key(&db.pool, "reader").await;
        let config = Config {
            catalog_ttl: TTL,
            credential_ttl: TTL,
            ..Config::default()
        };
        let app = App::load(db.pool.clone(), config, Arc::new(SystemClock))
            .await
            .unwrap();
        let mut wanted = crate::labels::directory::Wanted::default();
        wanted.catalog("main");
        // All three cached before the silent changes.
        let reader = app.find_key(&key).await.unwrap().expect("the key");
        assert!(reader
            .require_in(app.directory(), key_rows::Access::Read, "main")
            .await
            .is_ok());
        app.labels().labels_for(&wanted).await.unwrap();
        sqlx::query("UPDATE api_keys SET revoked_at = now() WHERE id = 'reader'")
            .execute(&db.pool)
            .await
            .unwrap();
        suspend_silently(&db.pool).await;
        sqlx::query("UPDATE catalogs SET display_name = 'Renamed' WHERE name = 'main'")
            .execute(&db.pool)
            .await
            .unwrap();
        assert!(
            app.find_key(&key).await.unwrap().is_some(),
            "still cached: no message named it"
        );
        assert!(!suspended(&app, &reader).await, "the row is still cached");

        tokio::time::sleep(TTL + Duration::from_millis(200)).await;

        assert!(app.find_key(&key).await.unwrap().is_none(), "the key");
        assert!(suspended(&app, &reader).await, "the catalog row");
        let labels = app.labels().labels_for(&wanted).await.unwrap();
        assert_eq!(labels.catalog("main"), Some("Renamed"), "the label");
    }

    /// A clock a test moves by hand.
    struct MovableClock(std::sync::Mutex<chrono::DateTime<Utc>>);

    impl MovableClock {
        fn advance(&self, by: Duration) {
            let mut now = self.0.lock().expect("clock lock");
            *now += chrono::Duration::from_std(by).expect("a small duration");
        }
    }

    impl Clock for MovableClock {
        #[cfg_attr(
            dylint_lib = "hyp_tracing",
            allow(uninstrumented_fn, reason = "test double")
        )]
        fn now(&self) -> chrono::DateTime<Utc> {
            *self.0.lock().expect("clock lock")
        }
    }

    /// An app on `pool` whose clock the test moves, with the default
    /// `credential_ttl`.
    async fn app_with_movable_clock(pool: &PgPool) -> (Arc<App>, Arc<MovableClock>) {
        let clock = Arc::new(MovableClock(std::sync::Mutex::new(Utc::now())));
        let app = App::load(pool.clone(), Config::default(), clock.clone())
            .await
            .unwrap();
        (app, clock)
    }

    async fn set_setting(pool: &PgPool, key: settings::SettingKey, value: &str) {
        settings::set(pool, key, value, &audit::AuditActor::cli(), Utc::now())
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_settings_invalidation_reloads_the_settings() {
        let db = test_db().await;
        let app = app(&db.pool).await;
        set_setting(&db.pool, settings::SettingKey::MaxKeysPerUser, "5").await;
        let before = app.settings().max_keys_per_user;

        app.apply(&message(crate::invalidate::Invalidation::Settings))
            .await;

        assert_eq!(before, 20, "nothing told this replica yet");
        assert_eq!(app.settings().max_keys_per_user, 5);
    }

    /// The TTL backstop (spec Decision 9): no listener runs in a lib test,
    /// so the setting's `NOTIFY` reaches no one; only the age of the loaded
    /// settings makes the principal's assembly read them again.
    #[tokio::test(flavor = "multi_thread")]
    async fn settings_are_revalidated_after_the_ttl_without_a_notification() {
        let db = test_db().await;
        let key = new_key(&db.pool, "reader").await;
        let (app, clock) = app_with_movable_clock(&db.pool).await;
        let require_unlock = |app: Arc<App>, key: String| async move {
            app.find_key(&key)
                .await
                .unwrap()
                .expect("a live key")
                .facts
                .require_unlock
        };
        assert!(!require_unlock(app.clone(), key.clone()).await);

        set_setting(&db.pool, settings::SettingKey::RequireUnlock, "on").await;
        let within_the_ttl = require_unlock(app.clone(), key.clone()).await;
        clock.advance(app.config().credential_ttl + Duration::from_secs(1));
        let past_the_ttl = require_unlock(app.clone(), key.clone()).await;

        assert!(!within_the_ttl, "younger settings are not read again");
        assert!(past_the_ttl, "the principal carries the new setting");
        assert!(app.settings().require_unlock, "and so does the app");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_requests_past_the_ttl_reload_settings_once() {
        let db = test_db().await;
        let (app, clock) = app_with_movable_clock(&db.pool).await;
        set_setting(&db.pool, settings::SettingKey::MaxKeysPerUser, "5").await;
        clock.advance(app.config().credential_ttl + Duration::from_secs(1));
        let loads_before = app.settings_loads.load(Ordering::Relaxed);
        let checks_before = app.settings_checks.load(Ordering::Relaxed);

        // A `JoinSet` runs the ten calls as concurrent tasks.
        let mut calls = tokio::task::JoinSet::new();
        for _ in 0..10 {
            let app = Arc::clone(&app);
            calls.spawn(async move { app.fresh_settings().await.max_keys_per_user });
        }
        let seen = tokio::time::timeout(Duration::from_secs(30), calls.join_all())
            .await
            .expect("ten settings checks within 30 s");

        assert_eq!(seen, [5; 10]);
        assert_eq!(
            app.settings_loads.load(Ordering::Relaxed) - loads_before,
            1,
            "one load for all ten"
        );
        assert_eq!(
            app.settings_checks.load(Ordering::Relaxed) - checks_before,
            1,
            "and one check: the rest found them fresh once they had the lock"
        );
    }

    /// Ten callers queued behind the check's lock with the database gone,
    /// after `mark` (nothing, for an expired TTL; a stale mark, as an announcement,
    /// reconnect or `Settings` message makes). Returns how many checks the
    /// ten ran and what they got, then how many a later request ran.
    async fn queued_behind_a_failed_check(
        app: &Arc<App>,
        mark: impl FnOnce(&App),
    ) -> (usize, Vec<u32>, usize) {
        // The app's pool, closed: every check fails at once.
        app.accounts().executor().close().await;
        mark(app);
        let checks_before = app.settings_checks.load(Ordering::Relaxed);
        // Hold the check's lock so all ten queue behind it first.
        let held = app.settings_reload.lock().await;
        let mut calls = tokio::task::JoinSet::new();
        for _ in 0..10 {
            let app = Arc::clone(app);
            calls.spawn(async move { app.fresh_settings().await.max_keys_per_user });
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        drop(held);
        let seen = tokio::time::timeout(Duration::from_secs(30), calls.join_all())
            .await
            .expect("ten settings checks within 30 s");
        let queued = app.settings_checks.load(Ordering::Relaxed) - checks_before;
        app.fresh_settings().await;
        let later = app.settings_checks.load(Ordering::Relaxed) - checks_before - queued;
        (queued, seen, later)
    }

    /// With the database gone, callers that queued behind one failing check
    /// take its answer (the loaded settings) instead of each running their
    /// own, one after another; a request after it still retries.
    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_requests_share_one_failed_check() {
        let db = test_db().await;
        let (app, clock) = app_with_movable_clock(&db.pool).await;
        clock.advance(app.config().credential_ttl + Duration::from_secs(1));

        let (queued, seen, later) = queued_behind_a_failed_check(&app, |_| {}).await;

        assert_eq!(seen, [20; 10], "the loaded settings serve");
        assert_eq!(queued, 1, "one failing check for all ten");
        assert_eq!(later, 1, "a later request checks again");
    }

    /// The same when a stale mark started the check: the mark its failure
    /// restores for the retry is not a newer one, so it is still shared.
    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_requests_share_one_failed_check_after_a_stale_mark() {
        let db = test_db().await;
        let (app, _clock) = app_with_movable_clock(&db.pool).await;

        let (queued, seen, later) =
            queued_behind_a_failed_check(&app, |app| app.mark_settings_stale()).await;

        assert_eq!(seen, [20; 10], "the loaded settings serve");
        assert_eq!(queued, 1, "one failing check for all ten");
        assert_eq!(later, 1, "a later request checks again");
    }

    /// A mark that lands while a check runs is newer than that check: a
    /// caller waiting behind it runs its own.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_mark_during_a_check_gets_a_check_of_its_own() {
        let db = test_db().await;
        let (app, _clock) = app_with_movable_clock(&db.pool).await;
        set_setting(&db.pool, settings::SettingKey::MaxKeysPerUser, "5").await;
        app.mark_settings_stale();
        // Stands in for a check that began before the mark: the lock held,
        // the round finished and the marks it saw recorded as of before.
        let held = app.settings_reload.lock().await;
        let waiter = {
            let app = Arc::clone(&app);
            tokio::spawn(async move { app.fresh_settings().await.max_keys_per_user })
        };
        tokio::time::sleep(Duration::from_millis(200)).await;
        app.settings_rounds.fetch_add(1, Ordering::Release);
        drop(held);
        let seen = tokio::time::timeout(Duration::from_secs(30), waiter)
            .await
            .expect("the waiter within 30 s")
            .expect("the waiter's task");

        assert_eq!(seen, 5, "the waiter checked for its own mark");
    }

    /// Holds a stale-triggered check in flight (past its start, before its
    /// database read) and returns its task, for the in-flight tests. The
    /// settings are young, so only the stale mark makes the check run.
    async fn a_stale_check_held_in_flight(
        app: &Arc<App>,
    ) -> (tokio::sync::MutexGuard<'_, ()>, tokio::task::JoinHandle<()>) {
        let hold = app.settings_check_hold.lock().await;
        let checks_before = app.settings_checks.load(Ordering::Relaxed);
        let check = {
            let app = Arc::clone(app);
            tokio::spawn(async move { app.reload_settings().await })
        };
        tokio::time::timeout(Duration::from_secs(30), async {
            while app.settings_checks.load(Ordering::Relaxed) == checks_before {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the check started within 30 s");
        (hold, check)
    }

    /// A request that arrives while a stale-triggered check is in flight
    /// waits for it and gets what it read, not the old settings: they stay
    /// stale until a check that began after the mark has succeeded.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_request_during_a_stale_check_waits_for_its_answer() {
        let db = test_db().await;
        let (app, _clock) = app_with_movable_clock(&db.pool).await;
        set_setting(&db.pool, settings::SettingKey::MaxKeysPerUser, "5").await;
        let (hold, check) = a_stale_check_held_in_flight(&app).await;

        let mut request = {
            let app = Arc::clone(&app);
            tokio::spawn(async move { app.fresh_settings().await.max_keys_per_user })
        };
        let answered_early = tokio::time::timeout(Duration::from_millis(300), &mut request)
            .await
            .is_ok();
        drop(hold);
        let answer = tokio::time::timeout(Duration::from_secs(30), request)
            .await
            .expect("the request within 30 s")
            .expect("the request's task");
        tokio::time::timeout(Duration::from_secs(30), check)
            .await
            .expect("the check within 30 s")
            .expect("the check's task");

        assert!(!answered_early, "the request waited for the check");
        assert_eq!(answer, 5);
    }

    /// A check cancelled mid-read satisfies nothing: the next request,
    /// though the settings are young, checks again and gets the new value.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_cancelled_check_leaves_the_settings_stale() {
        let db = test_db().await;
        let (app, _clock) = app_with_movable_clock(&db.pool).await;
        set_setting(&db.pool, settings::SettingKey::MaxKeysPerUser, "5").await;
        let (hold, check) = a_stale_check_held_in_flight(&app).await;

        check.abort();
        // `abort` only requests it: wait until the task is gone.
        let _ = tokio::time::timeout(Duration::from_secs(30), check)
            .await
            .expect("the cancelled check ends within 30 s");
        drop(hold);
        let next = tokio::time::timeout(Duration::from_secs(30), app.fresh_settings())
            .await
            .expect("the next request within 30 s");

        assert_eq!(next.max_keys_per_user, 5, "the next request checked");
    }

    /// A settings read blocked behind an exclusive lock on `server_state`
    /// must not hold a request: past [`SETTINGS_CHECK_TIMEOUT`] the check
    /// fails, the request is answered with the loaded settings, and the
    /// check is not satisfied, so the next request (once the lock is gone)
    /// checks again.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_blocked_settings_check_answers_within_its_bound() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let db = test_db().await;
        let (app, clock) = app_with_movable_clock(&db.pool).await;
        set_setting(&db.pool, settings::SettingKey::ProductName, "Acme Notes").await;
        clock.advance(app.config().credential_ttl + Duration::from_secs(1));
        // Held on its own connection; dropping the transaction releases it.
        let mut locker = db.pool.begin().await.unwrap();
        sqlx::query("LOCK TABLE server_state IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *locker)
            .await
            .unwrap();
        let product_name = || async {
            let response = crate::http::router(
                Arc::clone(&app),
                &tokio_util::sync::CancellationToken::new(),
            )
            .oneshot(Request::get("/ui/brand.json").body(Body::empty()).unwrap())
            .await
            .unwrap();
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["product_name"].clone()
        };

        let started = Instant::now();
        let blocked = tokio::time::timeout(
            SETTINGS_CHECK_TIMEOUT + Duration::from_secs(10),
            product_name(),
        )
        .await
        .expect("the request answers although the settings read is blocked");
        let waited = started.elapsed();
        locker.rollback().await.unwrap();
        let after = tokio::time::timeout(Duration::from_secs(30), product_name())
            .await
            .expect("the next request within 30 s");

        assert_eq!(blocked, "okf", "the loaded settings serve");
        assert!(
            waited >= SETTINGS_CHECK_TIMEOUT,
            "it waited for the bound: {waited:?}"
        );
        assert_eq!(after, "Acme Notes", "the next request checked again");
    }

    /// A check that finds `admin_generation` unmoved loads nothing; one
    /// marked stale checks again at once, whatever the settings' age.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_check_loads_the_settings_only_when_admin_generation_moved() {
        let db = test_db().await;
        let (app, clock) = app_with_movable_clock(&db.pool).await;
        clock.advance(app.config().credential_ttl + Duration::from_secs(1));
        app.fresh_settings().await;
        let unmoved = app.settings_loads.load(Ordering::Relaxed);

        set_setting(&db.pool, settings::SettingKey::MaxKeysPerUser, "5").await;
        let fresh_and_unmarked = app.fresh_settings().await.max_keys_per_user;
        app.mark_settings_stale();
        let marked = app.fresh_settings().await.max_keys_per_user;

        assert_eq!(unmoved, 0, "an unmoved counter loads nothing");
        assert_eq!(fresh_and_unmarked, 20, "young settings are not checked");
        assert_eq!(marked, 5, "stale ones are");
    }

    /// A clock stepped back past the last check must not keep the settings
    /// forever: a check time in the future counts as expired.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_clock_stepped_back_expires_the_settings() {
        let db = test_db().await;
        let (app, clock) = app_with_movable_clock(&db.pool).await;
        set_setting(&db.pool, settings::SettingKey::MaxKeysPerUser, "5").await;
        *clock.0.lock().expect("clock lock") -= chrono::Duration::hours(1);

        assert_eq!(app.fresh_settings().await.max_keys_per_user, 5);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_settings_change_reaches_the_running_app_through_its_message() {
        let db = test_db().await;
        let app = app(&db.pool).await;
        assert_eq!(app.settings().max_keys_per_user, 20);

        let before_message = applied(&app, async {
            settings::set(
                &db.pool,
                settings::SettingKey::MaxKeysPerUser,
                "5",
                &audit::AuditActor::cli(),
                Utc::now(),
            )
            .await
            .unwrap();
            app.settings().max_keys_per_user
        })
        .await;
        let after_set = app.settings().max_keys_per_user;
        applied(&app, async {
            settings::reset(
                &db.pool,
                settings::SettingKey::MaxKeysPerUser,
                &audit::AuditActor::cli(),
                Utc::now(),
            )
            .await
            .unwrap();
        })
        .await;

        assert_eq!(before_message, 20, "read from memory, not per call");
        assert_eq!(after_set, 5);
        assert_eq!(app.settings().max_keys_per_user, 20, "reset reaches it too");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unreadable_stored_setting_never_stops_the_server() {
        let db = test_db().await;
        // A value a newer server (or a hand edit) wrote that this one cannot
        // read, there before the server starts...
        sqlx::query(
            "INSERT INTO server_settings (key, value, updated_at, updated_by) \
             VALUES ('console_session_idle', '\"forever\"', now(), 'hand')",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        let app = app(&db.pool).await;
        let at_start = app.settings().console_session_idle;
        // ...and one written while it runs, followed by a generation bump.
        sqlx::query(
            "INSERT INTO server_settings (key, value, updated_at, updated_by) \
             VALUES ('max_keys_per_user', '-3', now(), 'hand')",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        applied(&app, async {
            settings::set(
                &db.pool,
                settings::SettingKey::PasswordSignIn,
                "off",
                &audit::AuditActor::cli(),
                Utc::now(),
            )
            .await
            .unwrap();
        })
        .await;

        let current = app.settings();
        assert_eq!(at_start, crate::config::DEFAULT_CONSOLE_SESSION_IDLE);
        assert_eq!(
            current.console_session_idle,
            crate::config::DEFAULT_CONSOLE_SESSION_IDLE
        );
        assert_eq!(current.max_keys_per_user, 20);
        assert!(
            !current.password_sign_in,
            "the readable change still applies"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_settings_reload_keeps_the_old_settings_and_retries() {
        let db = test_db().await;
        let app = app(&db.pool).await;
        settings::set(
            &db.pool,
            settings::SettingKey::MaxKeysPerUser,
            "5",
            &audit::AuditActor::cli(),
            Utc::now(),
        )
        .await
        .unwrap();
        sqlx::query("ALTER TABLE server_settings RENAME TO server_settings_away")
            .execute(&db.pool)
            .await
            .unwrap();

        // What the write's `Settings` message does: the check fails.
        app.reload_settings().await;
        let while_unreadable = app.settings().max_keys_per_user;
        sqlx::query("ALTER TABLE server_settings_away RENAME TO server_settings")
            .execute(&db.pool)
            .await
            .unwrap();
        // No new message: the failed check satisfied nothing, so the next
        // use checks again.
        app.fresh_settings().await;

        assert_eq!(while_unreadable, 20);
        assert_eq!(app.settings().max_keys_per_user, 5);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_clock_check_runs_at_startup_and_stops_with_the_server() {
        // `None`: the server stamps its reply when the request arrives, after
        // `app_with` has started its database (several seconds).
        let server = crate::ntp::testing::fake_server(None, 1).await;
        let config = Config {
            time_server: Some(server.to_string()),
            ..Config::default()
        };
        let test = app_with(&[("pg", &[])], config).await;
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(Arc::clone(&test.app).check_clock_until(shutdown.clone()));

        // Wait for the startup check, but not forever.
        let status = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(status) = test.app.clock_status() {
                    break status;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("a clock check within 10 s");
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the loop stops when cancelled")
            .expect("the loop does not panic");

        assert!(status.ok, "{status:?}");
        // Both ends read the system clock: the offset is a few ms at most.
        assert!(
            status.offset_ms.is_some_and(|ms| ms.abs() < 1_000),
            "{status:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn without_a_time_server_there_is_no_check() {
        let test = app_with(&[("pg", &[])], Config::default()).await;

        // Returns at once, without a cancelled token.
        tokio::time::timeout(
            Duration::from_secs(1),
            Arc::clone(&test.app).check_clock_until(CancellationToken::new()),
        )
        .await
        .expect("returns at once without a time_server");

        assert_eq!(test.app.clock_status(), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pool_gauges() {
        let (test, registry) =
            crate::mcp::testing::app_with_metrics(&[("pg", &[])], Config::default()).await;
        // One connection held out of the pool, so `used` is not zero.
        let _held = test
            .app
            .accounts()
            .executor()
            .acquire()
            .await
            .expect("a connection");

        let text = crate::mcp::testing::render(&registry);

        let sample = |state: &str| {
            crate::mcp::testing::sample_of(
                &text,
                &format!(r#"db_client_connection_count{{state="{state}"}}"#),
            )
            .unwrap_or_else(|| panic!("no {state} series: {text}"))
        };
        let (idle, used) = (sample("idle"), sample("used"));
        assert!(used >= 1.0, "{text}");
        assert_eq!(
            idle + used,
            f64::from(test.app.accounts().executor().size()),
            "{text}"
        );
    }

    #[tokio::test]
    async fn each_clock_check_is_its_own_trace() {
        use crate::telemetry::config::LogFormat;
        use crate::telemetry::subscriber::capture::Observer;
        for filter in ["info", "trace"] {
            // The fake server answers once, so each filter gets its own.
            let server = crate::ntp::testing::fake_server(None, 1).await;
            let config = Config {
                time_server: Some(server.to_string()),
                ..Config::default()
            };
            let (test, registry) =
                crate::mcp::testing::app_with_metrics(&[("pg", &[])], config).await;
            let observer = Observer::start("off", filter, "off", LogFormat::Text);
            let shutdown = CancellationToken::new();
            let running = Arc::clone(&test.app)
                .check_clock_until(shutdown.clone())
                .instrument(tracing::info_span!("caller"));
            // Stop the loop once its startup check has answered.
            let stop = async {
                // `checked_at` has one-second resolution, so count the
                // checks the metric saw instead.
                let counted = || {
                    crate::mcp::testing::sample_of(
                        &crate::mcp::testing::render(&registry),
                        r#"okf_clock_checks_total{outcome="ok"}"#,
                    )
                    .unwrap_or(0.0)
                };
                let before = counted();
                tokio::time::timeout(Duration::from_secs(10), async {
                    while counted() <= before {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                })
                .await
                .expect("a clock check within 10 s");
                shutdown.cancel();
            };
            tokio::time::timeout(Duration::from_secs(15), async {
                tokio::join!(running, stop)
            })
            .await
            .expect("the loop stops when cancelled");
            let observed = observer.finish();

            let caller = &observed.spans_named("caller")[0];
            let checks = observed.spans_named("okf.clock.check");
            assert_eq!(checks.len(), 1, "{filter}");
            assert_eq!(
                checks[0].parent_span_id,
                opentelemetry::trace::SpanId::INVALID
            );
            assert_ne!(
                checks[0].span_context.trace_id(),
                caller.span_context.trace_id(),
                "{filter}"
            );
            // The result is on the root: a closed outcome and the offset in
            // seconds as a number, never a string; a success is not an error.
            let attribute = |key| crate::telemetry::subscriber::capture::attribute(checks[0], key);
            assert_eq!(
                attribute("okf.outcome").map(ToString::to_string).as_deref(),
                Some("ok"),
                "{filter}"
            );
            assert!(
                matches!(attribute("okf.clock.offset"), Some(opentelemetry::Value::F64(offset)) if offset.abs() < 1.0),
                "{filter}: {:?}",
                checks[0].attributes
            );
            assert_eq!(
                checks[0].status,
                opentelemetry::trace::Status::Unset,
                "{filter}"
            );
            let text = crate::mcp::testing::render(&registry);
            assert_eq!(
                crate::mcp::testing::sample_of(&text, r#"okf_clock_checks_total{outcome="ok"}"#),
                Some(1.0),
                "{text}"
            );
            let offset = crate::mcp::testing::sample_of(&text, "okf_clock_offset_seconds")
                .unwrap_or_else(|| panic!("no offset gauge: {text}"));
            assert!(offset.abs() < 1.0, "{text}");
        }
    }

    /// A UDP server that answers one request with a packet too short to be
    /// an NTP reply.
    async fn garbage_time_server() -> std::net::SocketAddr {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = socket.local_addr().expect("address");
        tokio::spawn(async move {
            let mut request = [0u8; 48];
            if let Ok((_, client)) = socket.recv_from(&mut request).await {
                let _ = socket.send_to(&[0u8; 4], client).await;
            }
        });
        address
    }

    #[tokio::test]
    async fn a_failed_clock_check_marks_its_root_span_as_an_error() {
        use crate::telemetry::config::LogFormat;
        use crate::telemetry::subscriber::capture::{attribute, Observer};
        for filter in ["info", "trace"] {
            let server = garbage_time_server().await;
            let config = Config {
                time_server: Some(server.to_string()),
                ..Config::default()
            };
            let (test, registry) =
                crate::mcp::testing::app_with_metrics(&[("pg", &[])], config).await;
            let observer = Observer::start("off", filter, "off", LogFormat::Text);
            let shutdown = CancellationToken::new();
            let running = Arc::clone(&test.app).check_clock_until(shutdown.clone());
            let stop = async {
                let app = &test.app;
                tokio::time::timeout(Duration::from_secs(10), async {
                    while app.clock_status().is_none() {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                })
                .await
                .expect("a clock check within 10 s");
                shutdown.cancel();
            };
            tokio::time::timeout(Duration::from_secs(15), async {
                tokio::join!(running, stop)
            })
            .await
            .expect("the loop stops when cancelled");
            let observed = observer.finish();

            let checks = observed.spans_named("okf.clock.check");
            assert_eq!(checks.len(), 1, "{filter}");
            assert_eq!(
                attribute(checks[0], "okf.outcome")
                    .map(ToString::to_string)
                    .as_deref(),
                Some("error"),
                "{filter}"
            );
            assert!(
                attribute(checks[0], "okf.clock.offset").is_none(),
                "{filter}"
            );
            assert!(
                matches!(checks[0].status, opentelemetry::trace::Status::Error { .. }),
                "{filter}: {:?}",
                checks[0].status
            );
            let text = crate::mcp::testing::render(&registry);
            assert_eq!(
                crate::mcp::testing::sample_of(&text, r#"okf_clock_checks_total{outcome="error"}"#),
                Some(1.0),
                "{filter}: {text}"
            );
        }
    }
}
