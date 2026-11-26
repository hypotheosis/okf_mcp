//! Credentials and users on demand (M18 spec Decision 9): the caches that
//! replace the whole-table key cache.
//!
//! Three [`Bounded`] caches hold what a request's principal is built from:
//!
//! * API keys by the SHA-256 of their secret, and OAuth grants by id: the
//!   credential row ([`CredentialRecord`]) with its audit id;
//! * users by id ([`UserEntry`]): the user's sign-in identity and gates
//!   (teams, entitlements, live unlocks, audit id). The user's ceiling is
//!   not here: it is resolved per catalog from the catalog's directory row
//!   and these gates ([`crate::store::ceiling::level_on`]), so an entry and
//!   its load are bounded by the user's teams and entitlements, never by how
//!   many catalogs those teams own (issue #728).
//!
//! A principal is not cached as a whole: [`Credentials::find`] reads the
//! credential and its user from the caches and assembles the principal per
//! request, which is a few map lookups. So a change to a user drops one user
//! entry, never every credential of theirs. What a check needs to know about
//! the catalog it acts on (owner team, suspension, maintenance opt-out,
//! audit team) is not here at all: the caller reads it from the catalog
//! directory at the moment of the check ([`crate::cache::directory`]).
//!
//! Unknown, revoked and expired credentials are never cached: a bad token
//! costs one indexed query (the rate limiters bound guessing), and a key or
//! grant created on another replica works at once, with nothing to catch up.
//!
//! Each cache is kept current by the change feed ([`Credentials::apply`]):
//!
//! | Message | Drops |
//! |---|---|
//! | `credential { key_id }` | that key or grant, by id and by hash |
//! | `user { id }` | that user |
//! | `team { id }` | every cached user in that team |
//! | `catalog_meta { name }` | every cached credential whose reach names that catalog |
//! | `team_catalogs { team_id }` | nothing: a credential holds no suspension state |
//!
//! The indirect ones (by hash, by team, by catalog) scan the resident
//! entries and raise the cache's fence, so a load running at that moment
//! serves its own request but is not kept (spec Decision 11): a key revoked
//! while its lookup is in flight is never cached as live. `credential_ttl`
//! bounds how long an entry survives a lost message.
//!
//! # For Python readers
//!
//! `Arc<T>` is a shared, reference-counted pointer: the caches hand out
//! `Arc`s, so a cached entry is shared by every request that reads it rather
//! than copied.

use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;

use crate::auth::{principal_of, unexpired, AccessFacts, Principal};
use crate::cache::directory::CatalogDirectory;
use crate::cache::{Bounded, CacheName, Limit};
use crate::clock::Clock;
use crate::error::OkfError;
use crate::invalidate::Invalidation;
use crate::keys;
use crate::store::ceiling;
use crate::store::gates::{self, UserGates};
use crate::store::keys::{self as key_rows, CredentialRecord, KeyKind, KeyRecord};
use crate::store::users::{self, UserAuth};
use crate::telemetry::metrics::Metrics;

/// What a user row and its memberships say: what a session principal is
/// built from, and (with each catalog's directory row) what caps a
/// self-issued credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserEntry {
    /// The user's id, name, display name, email and admin flag.
    pub auth: UserAuth,
    /// The user's teams, entitlements, live unlocks and audit id, shared by
    /// every principal of theirs. The teams and entitlements are what the
    /// user's ceiling is resolved from, one catalog at a time
    /// ([`crate::store::ceiling::level_on`]).
    pub gates: Arc<UserGates>,
}

/// The parts of a principal that come from neither the credential nor the
/// user: what [`Credentials::find`] is given by the caller for each request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccessView {
    /// The `require_unlock` setting.
    pub require_unlock: bool,
}

/// A test hook that holds a load after it has read the database and before
/// it returns: the seam the race tests use to apply an invalidation while a
/// load is in flight.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct LoadHold {
    /// Held by a test to stop loads at the hook.
    pub(crate) gate: tokio::sync::Mutex<()>,
    /// Notified each time a load reaches the hook.
    pub(crate) reached: tokio::sync::Notify,
}

#[cfg(test)]
impl LoadHold {
    /// Signals that a load reached the hook, then waits for the gate.
    #[tracing::instrument(level = "trace", skip_all)]
    async fn pass(&self) {
        self.reached.notify_one();
        drop(self.gate.lock().await);
    }
}

/// Credentials and users on demand (spec Decision 9).
pub struct Credentials {
    // API keys by the SHA-256 of their secret.
    by_hash: Bounded<[u8; 32], Arc<CredentialRecord>>,
    // OAuth grants by id: an access token names its grant by id.
    by_id: Bounded<String, Arc<CredentialRecord>>,
    users: Bounded<String, Arc<UserEntry>>,
    // The catalog rows a self-issued credential's ceiling is resolved from:
    // the application's own directory, shared, so its invalidations apply.
    directory: Arc<CatalogDirectory>,
    pool: PgPool,
    clock: Arc<dyn Clock>,
    #[cfg(test)]
    pub(crate) key_hold: LoadHold,
    #[cfg(test)]
    pub(crate) user_hold: LoadHold,
}

impl std::fmt::Debug for Credentials {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "a Debug impl")
    )]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the entries: they hold key hashes.
        f.debug_struct("Credentials")
            .field("by_hash", &self.by_hash)
            .field("by_id", &self.by_id)
            .field("users", &self.users)
            .finish_non_exhaustive()
    }
}

/// The capacities and lifetime of [`Credentials`]' caches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CredentialLimits {
    /// The most keys, and separately grants, kept (`credential_cache_entries`).
    pub credentials: u64,
    /// The most users kept (`user_cache_entries`).
    pub users: u64,
    /// How long an entry is served (`credential_ttl`).
    pub ttl: Duration,
}

impl Credentials {
    /// Empty caches.
    ///
    /// # Arguments
    /// * `pool` - the database.
    /// * `directory` - the catalog directory the ceilings are resolved
    ///   against (the application's, shared).
    /// * `clock` - the server clock, for expiry.
    /// * `limits` - the capacities and the time to live.
    /// * `metrics` - the instruments `okf.cache.*` is recorded on.
    ///
    /// # Returns
    /// The caches.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn new(
        pool: PgPool,
        directory: Arc<CatalogDirectory>,
        clock: Arc<dyn Clock>,
        limits: CredentialLimits,
        metrics: Metrics,
    ) -> Self {
        let entries = Limit::Entries(limits.credentials);
        Self {
            by_hash: Bounded::new(CacheName::Credential, entries, limits.ttl)
                .with_metrics(metrics.clone()),
            by_id: Bounded::new(CacheName::Grant, entries, limits.ttl)
                .with_metrics(metrics.clone()),
            users: Bounded::new(CacheName::User, Limit::Entries(limits.users), limits.ttl)
                .with_metrics(metrics),
            directory,
            pool,
            clock,
            #[cfg(test)]
            key_hold: LoadHold::default(),
            #[cfg(test)]
            user_hold: LoadHold::default(),
        }
    }

    /// The principal of a presented API key.
    ///
    /// # Arguments
    /// * `presented` - the key from the request (never logged or kept: the
    ///   cache is keyed by its SHA-256).
    /// * `view` - the settings the principal is built with.
    ///
    /// # Returns
    /// Its principal, or `None` for an unknown, revoked or expired key.
    ///
    /// # Errors
    /// [`OkfError::Database`] when a load fails; [`OkfError::Busy`] for a
    /// lookup that shared another's failed load.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn find(
        &self,
        presented: &str,
        view: &AccessView,
    ) -> Result<Option<Arc<Principal>>, OkfError> {
        let hash = keys::hash(presented);
        let found = self
            .by_hash
            .get_or_load_present(hash, || self.load(Lookup::Hash(hash)))
            .await?;
        self.assemble(found, view).await
    }

    /// A live OAuth grant's principal.
    ///
    /// # Arguments
    /// * `grant_id` - the grant's id (from its access token or code).
    /// * `view` - as for [`Credentials::find`].
    ///
    /// # Returns
    /// Its principal, or `None` for an unknown, revoked or expired grant, and
    /// for the id of an API key (a key is found only by its secret).
    ///
    /// # Errors
    /// As [`Credentials::find`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn find_grant(
        &self,
        grant_id: &str,
        view: &AccessView,
    ) -> Result<Option<Arc<Principal>>, OkfError> {
        let found = self
            .by_id
            .get_or_load_present(grant_id.to_string(), || self.load(Lookup::Grant(grant_id)))
            .await?;
        self.assemble(found, view).await
    }

    /// A user, by id: loaded once, then served until it changes.
    ///
    /// # Arguments
    /// * `user_id` - the user's id.
    ///
    /// # Returns
    /// The user, or `None` for one that does not exist (never cached).
    ///
    /// # Errors
    /// As [`Credentials::find`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn user(&self, user_id: &str) -> Result<Option<Arc<UserEntry>>, OkfError> {
        self.users
            .get_or_load_present(user_id.to_string(), || self.load_user(user_id))
            .await
    }

    /// The principal of a console session's user, built now from the user as
    /// it is (never as it was at sign-in: the admin flag can change between
    /// the password check and the session's first request).
    ///
    /// # Arguments
    /// * `user_id` - the signed-in user's id.
    /// * `view` - as for [`Credentials::find`].
    ///
    /// # Returns
    /// The principal, or `None` once the user is gone.
    ///
    /// # Errors
    /// As [`Credentials::find`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn session_principal(
        &self,
        user_id: &str,
        view: &AccessView,
    ) -> Result<Option<Principal>, OkfError> {
        let Some(user) = self.user(user_id).await? else {
            return Ok(None);
        };
        let facts = facts_of(Some(&user), None, view);
        Ok(Some(Principal::for_session(&user.auth, facts)))
    }

    /// Applies one change-feed message (see the module docs' table).
    ///
    /// # Arguments
    /// * `invalidation` - what changed.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn apply(&self, invalidation: &Invalidation) {
        match invalidation {
            Invalidation::Credential { key_id } => {
                self.by_id.invalidate(key_id).await;
                // The hash cache is keyed by hash, and the message names the
                // id: a scan, which also fences a load in flight.
                self.by_hash
                    .invalidate_where(|_, entry| entry.record.id == *key_id)
                    .await;
            }
            Invalidation::User { id } => self.users.invalidate(id).await,
            Invalidation::Team { id } => {
                self.users
                    .invalidate_where(|_, user| user.gates.teams.contains(id))
                    .await;
            }
            Invalidation::CatalogMeta { name, .. } => {
                // Reach names catalogs, and a catalog deleted and created
                // again under its name is another catalog: a cached grant of
                // the old one must not reach the new one. Catalog writes are
                // rare, so the scans cost little. No user is dropped: a
                // ceiling is resolved from the catalog's row (which the
                // directory drops on this message) and gates that name
                // catalogs and teams by id.
                let names = |record: &KeyRecord| {
                    record.catalog_access.contains_key(name)
                        || record.default_catalog.as_deref() == Some(name.as_str())
                };
                self.by_hash
                    .invalidate_where(|_, entry| names(&entry.record))
                    .await;
                self.by_id
                    .invalidate_where(|_, entry| names(&entry.record))
                    .await;
            }
            // Catalog content and settings are read per request elsewhere.
            // A team's catalogs changing (suspension) changes no credential
            // or user entry: what a check needs of the catalog comes from
            // its directory row, and the `Team` message sent beside this
            // one drops the team's users.
            Invalidation::Catalog { .. }
            | Invalidation::TeamCatalogs { .. }
            | Invalidation::Settings
            | Invalidation::Store { .. } => {}
        }
    }

    /// After the change feed's connection was lost, messages were missed:
    /// every entry is reread on its next use.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn mark_all_stale(&self) {
        self.by_hash.mark_all_stale();
        self.by_id.mark_all_stale();
        self.users.mark_all_stale();
    }

    /// Reads one live credential: `None` for an unknown, revoked or expired
    /// one, or one of the other kind (a grant's id is not a key's secret).
    #[tracing::instrument(level = "debug", skip_all)]
    async fn load(&self, lookup: Lookup<'_>) -> Result<Option<Arc<CredentialRecord>>, OkfError> {
        let (found, kind) = match lookup {
            Lookup::Hash(hash) => (
                key_rows::record_by_hash(&self.pool, &hash).await?,
                KeyKind::Key,
            ),
            Lookup::Grant(id) => (
                key_rows::record_by_id(&self.pool, id).await?,
                KeyKind::OAuth,
            ),
        };
        #[cfg(test)]
        self.key_hold.pass().await;
        let now = self.clock.now();
        Ok(found
            .filter(|found| {
                found.record.kind == kind
                    && found.record.revoked_at.is_none()
                    && unexpired(found.record.expires_at, now)
            })
            .map(Arc::new))
    }

    /// Reads one user and its gates in one read-only snapshot, so they
    /// describe one moment. Every query is bounded by the user's teams,
    /// entitlements and unlocks; none reads the catalogs those teams own
    /// (issue #728).
    #[tracing::instrument(level = "debug", skip_all)]
    async fn load_user(&self, user_id: &str) -> Result<Option<Arc<UserEntry>>, OkfError> {
        let now = self.clock.now();
        let mut tx = self.pool.begin().await?;
        // Must be the transaction's first statement; every later read sees
        // the snapshot the first query takes.
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *tx)
            .await?;
        let Some(auth) = users::find_auth(&mut *tx, user_id).await? else {
            return Ok(None);
        };
        let gates = gates::for_user(&mut tx, user_id, auth.is_admin, now).await?;
        // Nothing was written; ending the transaction frees its connection.
        tx.commit().await?;
        #[cfg(test)]
        self.user_hold.pass().await;
        Ok(Some(Arc::new(UserEntry {
            auth,
            gates: Arc::new(gates),
        })))
    }

    /// The principal of a cached credential, if it is still live: its user
    /// read from the user cache, and a self-issued one capped by the user's
    /// current ceiling.
    #[tracing::instrument(level = "trace", skip_all)]
    async fn assemble(
        &self,
        found: Option<Arc<CredentialRecord>>,
        view: &AccessView,
    ) -> Result<Option<Arc<Principal>>, OkfError> {
        let Some(found) = found else {
            return Ok(None);
        };
        // A credential that expired since it was cached stops now.
        if !unexpired(found.record.expires_at, self.clock.now()) {
            return Ok(None);
        }
        let user = match found.record.user_id.as_deref() {
            Some(user_id) => self.user(user_id).await?,
            None => None,
        };
        let facts = facts_of(user.as_deref(), found.audit_credential, view);
        // A self-issued credential is capped by its user's ceiling on its own
        // catalogs only: their rows, resolved now, and the user's gates.
        let ceiling = match user.as_deref() {
            Some(user) if found.record.self_issued => {
                Some(self.ceiling_over_grants(&found.record, &user.gates).await?)
            }
            _ => None,
        };
        Ok(Some(Arc::new(principal_of(
            found.record.clone(),
            ceiling.as_ref(),
            facts,
        ))))
    }

    /// A user's ceiling on the catalogs a credential is granted: each
    /// granted catalog's directory row (served from the directory's cache
    /// when it is there, the rest read in one query), at the level the
    /// user's gates give it ([`ceiling::ceiling_over`]). Bounded by the
    /// credential's grants.
    ///
    /// # Errors
    /// [`OkfError::Database`] when the rows' read fails.
    #[tracing::instrument(level = "trace", skip_all)]
    async fn ceiling_over_grants(
        &self,
        record: &KeyRecord,
        gates: &UserGates,
    ) -> Result<ceiling::Ceiling, OkfError> {
        // `principal_of` caps only a by-name reach: an all-catalogs
        // self-issued row (which the schema refuses) reaches nothing.
        if record.all_catalogs {
            return Ok(ceiling::Ceiling::new());
        }
        let names: Vec<String> = record.catalog_access.keys().cloned().collect();
        // A catalog on an unreachable store still gives its access facts:
        // the account database holds them, so sign-in never waits on a store.
        let (rows, _unreachable) = self.directory.resolve_many(&names).await?;
        // `as_ref` turns each `&Arc<CatalogRow>` into `&CatalogRow`.
        Ok(ceiling::ceiling_over(rows.iter().map(AsRef::as_ref), gates))
    }
}

/// How a credential is looked up.
#[derive(Debug, Clone, Copy)]
enum Lookup<'a> {
    /// An API key, by the hash of its secret.
    Hash([u8; 32]),
    /// An OAuth grant, by id.
    Grant(&'a str),
}

/// A principal's access facts.
#[tracing::instrument(level = "trace", skip_all)]
fn facts_of(
    user: Option<&UserEntry>,
    credential: Option<i64>,
    view: &AccessView,
) -> Arc<AccessFacts> {
    Arc::new(AccessFacts {
        user: user.map(|user| Arc::clone(&user.gates)),
        credential,
        require_unlock: view.require_unlock,
    })
}

#[cfg(test)]
#[path = "credentials_tests.rs"]
mod tests;
