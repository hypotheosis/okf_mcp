//! API-key and OAuth-token authentication (design "Data flow", "Read", step
//! 1): the [`Principal`] a request acts as, the [`BearerVerifier`] seam
//! between credential kinds, the axum middleware that requires a bearer
//! credential on MCP and the archive routes, and the principal of a console
//! session (`/api/v1` admits those through
//! `http::console_session::require_principal`).
//! Principals are assembled per request from the credential and user caches
//! ([`crate::cache::credentials`]); each self-issued credential is capped by
//! its user's ceiling ([`crate::store::ceiling`]), and admin-issued ones keep
//! their stored levels. A check against a catalog takes that catalog's
//! directory row ([`CatalogRow`]), read at the moment of the check.
//!
//! Keys are looked up by their SHA-256. A timing difference in that lookup
//! can tell an attacker only how the *hash* of a guess compares with stored
//! hashes, which does not help find a key, so there is no constant-time scan
//! (the design's deviation, recorded in the M4 plan).

use std::collections::BTreeMap;
// `Future` is not in the 2021 prelude; the trait below names it.
use std::future::Future;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};

use crate::app::App;
use crate::cache::directory::CatalogDirectory;
use crate::error::OkfError;
use crate::http::requester::Requester;
use crate::http::AppError;
use crate::keys;
use crate::oauth::access::OAuthVerifier;
use crate::store::audit::{AccessRecord, AdmittedActor, AuditActor};
use crate::store::catalogs::CatalogRow;
use crate::store::ceiling::{cap, Ceiling};
use crate::store::gates::UserGates;
use crate::store::keys::{Access, KeyKind, KeyRecord};
use crate::store::teams::SuspendedCatalog;
use crate::store::users::UserAuth;
use crate::telemetry::subscriber::split_event;

/// The `key_id` prefix of a console session's principal. Key ids match
/// `^[A-Za-z0-9._-]+$`, so `session:<name>` never equals one (plan
/// Decision 5).
pub const SESSION_KEY_PREFIX: &str = "session:";

/// The catalogs a credential may reach, and at what level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reach {
    /// Every catalog, present and future (`key create --all`), at the
    /// principal's own `access`.
    All,
    /// These catalogs, by name, each at its own level (Decision 1).
    Catalogs(BTreeMap<String, Access>),
    /// A non-admin console session: the catalogs its user's ceiling reaches,
    /// each at the ceiling's level there. It is resolved per catalog, from
    /// the catalog's directory row and the user's gates in
    /// [`AccessFacts::user`] ([`crate::store::ceiling::level_on`]), never
    /// listed whole, so a session costs nothing per catalog its user's teams
    /// own (issue #728). A name alone cannot decide it: checks that a
    /// session can reach take the row ([`Principal::level_at`]).
    Ceiling,
}

/// What credential a request came with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialKind {
    /// An `okf_` API key.
    ApiKey,
    /// An OAuth access token of a grant.
    OAuth {
        /// The grant's client id (`None` once the client row is gone).
        client_id: Option<String>,
        /// The grant's client, as it names itself.
        client_name: Option<String>,
    },
    /// A signed-in console session (`okf_session` cookie): never an MCP
    /// credential, and it owns no changesets.
    Session {
        /// The signed-in user's id.
        user_id: String,
    },
}

/// What a principal's admissions depend on besides its credential and the
/// catalog's own row: its user's gates, its credential's audit id and the
/// `require_unlock` setting, as they were when the principal
/// was assembled for this request (M18 spec Decision 9). Empty for a
/// principal built outside the credential cache (`From<KeyRecord>`, tests),
/// which then admits no one as a customer and records nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccessFacts {
    /// The credential's user's teams, entitlements, unlocks, admin flag and
    /// audit id; `None` for a credential without a user (or one built
    /// outside the cache).
    pub user: Option<Arc<UserGates>>,
    /// The credential's `audit_credentials` id.
    pub credential: Option<i64>,
    /// The `require_unlock` setting.
    pub require_unlock: bool,
}

/// A catalog as an access record names it, fixed at admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogScope {
    /// `catalogs.id`, never reused.
    pub id: i64,
    /// The owner team's `audit_teams` id; `None` for an unowned catalog.
    pub team: Option<i64>,
}

impl CatalogScope {
    /// The scope of the catalog incarnation `row` describes.
    ///
    /// # Arguments
    /// * `row` - the catalog's directory row.
    ///
    /// # Returns
    /// Its id and owner team's audit id.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn of(row: &CatalogRow) -> Self {
        Self {
            id: row.id,
            team: row.audit_team,
        }
    }
}

/// How a content read or write is let in (spec Parts 5 and 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// A member of the owner team: nothing is recorded.
    Customer,
    /// Neither a customer nor an admin read: nothing is recorded (writes
    /// are recorded as publishes).
    Untracked,
    /// An admin credential reading a catalog outside its user's teams: an
    /// hourly `catalog.admin_read` bucket (only while `require_unlock` is
    /// off).
    AdminRead,
    /// The maintenance runner reading (M14 Decision 8): nothing is recorded
    /// here; its passes are (Task 9c).
    Maintenance,
    /// Let in by a live unlock: the unlock's read, write or export tally.
    Unlocked {
        /// The unlock's id, fixed at admission.
        unlock_id: i64,
    },
}

impl Admission {
    /// Whether this admission writes an audit row (or an unlock's tally).
    ///
    /// # Returns
    /// `true` for [`Admission::AdminRead`] and [`Admission::Unlocked`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn is_recorded(self) -> bool {
        // Every variant is named, so a new one must decide.
        match self {
            Admission::AdminRead | Admission::Unlocked { .. } => true,
            Admission::Customer | Admission::Untracked | Admission::Maintenance => false,
        }
    }
}

/// Who a request acts as: one live API key, OAuth grant or console session
/// (never the secret itself).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    /// The key's id.
    pub key_id: String,
    /// The credential's highest level, for admin checks and display; what
    /// it may do in a catalog is [`Principal::level`]. An admin credential
    /// has admin on every catalog it reaches (the store refuses lower
    /// per-catalog levels on an admin key), so `access == Admin` never
    /// widens a catalog.
    pub access: Access,
    /// Independent opt-in to submit maintenance reports.
    pub maintenance_report: bool,
    /// A provisioner reaches no catalog and may call only allow-listed API routes.
    pub provisioner: bool,
    /// Which catalogs it reaches.
    pub reach: Reach,
    /// Its default catalog (design "Catalog selection", step 3).
    pub default_catalog: Option<String>,
    /// The actor its writes are recorded as.
    pub actor: String,
    /// An API key or an OAuth grant.
    pub kind: CredentialKind,
    /// The user this credential belongs to; `None` for one not yet
    /// assigned. Display only: it grants nothing, so `reaches` and
    /// `require` never read it.
    pub user: Option<String>,
    /// The `users.id` of the credential's user (`None` for a legacy key):
    /// what the audit log and the access gates key people by. It grants
    /// nothing by itself.
    pub user_id: Option<String>,
    /// What its admissions depend on besides the catalog's row (shared by
    /// the request's checks). Empty for a principal built outside the
    /// credential cache, which then records no access.
    pub facts: Arc<AccessFacts>,
}

impl Principal {
    /// The level this principal has on `catalog`, decided by name alone.
    ///
    /// A [`Reach::Ceiling`] (a non-admin console session) cannot be decided
    /// by name, so it answers `None` here: it fails closed. Every check a
    /// session can reach uses [`Self::level_at`] with the catalog's row.
    ///
    /// # Arguments
    /// * `catalog` - a catalog name.
    ///
    /// # Returns
    /// Its level there, or `None` when it does not reach the catalog (or
    /// the reach is a ceiling).
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn level(&self, catalog: &str) -> Option<Access> {
        match &self.reach {
            Reach::All => Some(self.access),
            // `copied` turns the map's `Option<&Access>` into
            // `Option<Access>` (`Access` is `Copy`).
            Reach::Catalogs(levels) => levels.get(catalog).copied(),
            Reach::Ceiling => None,
        }
    }

    /// The level this principal has on the catalog `row` describes: what
    /// every reach check uses, and the only one a [`Reach::Ceiling`]
    /// answers.
    ///
    /// # Arguments
    /// * `row` - the catalog's directory row.
    ///
    /// # Returns
    /// Its level there, or `None` when it does not reach the catalog.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn level_at(&self, row: &CatalogRow) -> Option<Access> {
        match &self.reach {
            Reach::All | Reach::Catalogs(_) => self.level(&row.name),
            // A session without gates (built outside the credential cache)
            // reaches nothing.
            Reach::Ceiling => self
                .facts
                .user
                .as_deref()
                .and_then(|gates| crate::store::ceiling::level_on(row, gates)),
        }
    }

    /// Does the key reach the catalog `row` describes ([`Self::level_at`])?
    ///
    /// # Arguments
    /// * `row` - the catalog's directory row.
    ///
    /// # Returns
    /// `true` when it reaches it at any level.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn reaches_row(&self, row: &CatalogRow) -> bool {
        self.level_at(row).is_some()
    }

    /// Whether the name alone shows the key does not reach `catalog`: a
    /// [`Reach::Catalogs`] that does not name it. Lets a caller skip the
    /// row's lookup; a `false` decides nothing, so the caller then checks
    /// the row ([`Self::reaches_row`]).
    ///
    /// # Arguments
    /// * `catalog` - a catalog name.
    ///
    /// # Returns
    /// `true` only for a by-name reach without the catalog.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn excludes(&self, catalog: &str) -> bool {
        match &self.reach {
            Reach::Catalogs(levels) => !levels.contains_key(catalog),
            Reach::All | Reach::Ceiling => false,
        }
    }

    /// Does the key reach this catalog?
    ///
    /// # Arguments
    /// * `catalog` - a catalog name.
    ///
    /// # Returns
    /// `true` for a key granted every catalog, or this one.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn reaches(&self, catalog: &str) -> bool {
        self.level(catalog).is_some()
    }

    /// Checks that the key reaches `catalog` at `access` or higher, ignoring
    /// suspension: the first two of [`Self::require`]'s three checks.
    ///
    /// # Arguments
    /// * `access` - the level the operation needs.
    /// * `catalog` - the catalog it acts on.
    ///
    /// # Errors
    /// [`OkfError::CatalogNotSelected`] if the key does not reach the
    /// catalog (worded like an unknown catalog, so a key cannot probe which
    /// catalogs exist); [`OkfError::Forbidden`] if it does but its level
    /// there is lower than `access`.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn require_level(&self, access: Access, catalog: &str) -> Result<(), OkfError> {
        self.check_level(self.level(catalog), access, catalog)
    }

    /// [`Self::require_level`] on the catalog `row` describes
    /// ([`Self::level_at`]): the form a [`Reach::Ceiling`] passes.
    ///
    /// # Arguments
    /// * `access` - the level the operation needs.
    /// * `row` - the catalog's directory row.
    ///
    /// # Errors
    /// As [`Self::require_level`].
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn require_level_at(&self, access: Access, row: &CatalogRow) -> Result<(), OkfError> {
        self.check_level(self.level_at(row), access, &row.name)
    }

    /// [`Self::require_level`] where the name decides it, run before a
    /// row's lookup so a key that does not reach the catalog learns nothing
    /// from it. A [`Reach::Ceiling`] passes here: its caller must then
    /// check the row ([`Self::require_level_at`], which [`Self::require`]
    /// runs).
    ///
    /// # Arguments
    /// * `access` - the level the operation needs.
    /// * `catalog` - the catalog's name.
    ///
    /// # Errors
    /// As [`Self::require_level`], never for a ceiling.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn precheck_level(&self, access: Access, catalog: &str) -> Result<(), OkfError> {
        match self.reach {
            Reach::Ceiling => Ok(()),
            Reach::All | Reach::Catalogs(_) => self.require_level(access, catalog),
        }
    }

    /// The refusal, if any, of `level` (this principal's on `catalog`) for
    /// an operation needing `access`.
    ///
    /// # Errors
    /// As [`Self::require_level`].
    #[tracing::instrument(level = "trace", skip_all)]
    fn check_level(
        &self,
        level: Option<Access>,
        access: Access,
        catalog: &str,
    ) -> Result<(), OkfError> {
        // `let ... else`: bind the level, or leave early when there is none.
        let Some(level) = level else {
            return Err(OkfError::CatalogNotSelected {
                catalog: catalog.to_string(),
            });
        };
        // `Access` derives `Ord` in declaration order: Read < Write < Admin.
        if level < access {
            return Err(OkfError::Forbidden {
                reason: format!(
                    "key {:?} has {} access to {catalog:?}; this needs {}",
                    self.key_id,
                    level.as_str(),
                    access.as_str()
                ),
            });
        }
        Ok(())
    }

    /// The suspension of the catalog `row` describes, if its owner team is
    /// suspended: a fact shown to anyone who reaches the catalog (listing),
    /// whatever their level. Callers check reach first, so a credential
    /// that does not reach a catalog never learns it is suspended.
    ///
    /// # Arguments
    /// * `row` - the catalog's directory row.
    ///
    /// # Returns
    /// The owner team, its notice and both labels, or `None` for an active
    /// catalog.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn suspension(&self, row: &CatalogRow) -> Option<SuspendedCatalog> {
        let suspended = row.suspended.as_ref()?;
        // Both labels come from the row itself: the catalog's from its
        // display name and description, the team's from the suspension.
        Some(SuspendedCatalog {
            team: suspended.team.clone(),
            notice: suspended.notice.clone(),
            catalog_label: crate::labels::catalog_label(&row.display_name, &row.description),
            team_label: suspended.team_label.clone(),
        })
    }

    /// Refuses a suspended catalog for every credential but an admin (M13
    /// Decision 1: the operator must still inspect, export and repair it).
    ///
    /// # Arguments
    /// * `row` - the directory row of a catalog the principal reaches (check
    ///   reach first).
    ///
    /// # Errors
    /// [`OkfError::Suspended`] when the catalog's owner team is suspended and
    /// the principal is not an admin.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn check_not_suspended(&self, row: &CatalogRow) -> Result<(), OkfError> {
        if self.access == Access::Admin {
            return Ok(());
        }
        match self.suspension(row) {
            None => Ok(()),
            Some(suspended) => Err(OkfError::Suspended {
                catalog: row.name.clone(),
                catalog_label: suspended.catalog_label,
                team: suspended.team,
                team_label: suspended.team_label,
                notice: suspended.notice,
            }),
        }
    }

    /// Checks that the key may do something needing `access` to the catalog
    /// `row` describes: reach, then level, then suspension (M13 Decision 2),
    /// so a credential that does not reach the catalog never learns it is
    /// suspended.
    ///
    /// # Arguments
    /// * `access` - the level the operation needs.
    /// * `row` - the directory row of the catalog it acts on.
    ///
    /// # Errors
    /// [`OkfError::CatalogNotSelected`] if the key does not reach the
    /// catalog (worded like an unknown catalog, so a key cannot probe which
    /// catalogs exist); [`OkfError::Forbidden`] if it does but its level
    /// there is lower than `access`; then [`OkfError::Suspended`] (see
    /// [`Self::check_not_suspended`]).
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn require(&self, access: Access, row: &CatalogRow) -> Result<(), OkfError> {
        self.require_level_at(access, row)?;
        self.check_not_suspended(row)
    }

    /// [`Self::require`] for a catalog known only by name, reading its row
    /// from the directory only when the answer depends on it: after the
    /// reach and level checks pass, and for a credential below admin. A
    /// catalog with no row (unknown, or deleted) is not suspended: the
    /// operation's own lookup then refuses it as before.
    ///
    /// # Arguments
    /// * `directory` - the catalog directory.
    /// * `access` - the level the operation needs.
    /// * `catalog` - the catalog's name.
    ///
    /// # Errors
    /// As [`Self::require`]; [`OkfError::Database`] when the row's read fails.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn require_in(
        &self,
        directory: &CatalogDirectory,
        access: Access,
        catalog: &str,
    ) -> Result<(), OkfError> {
        if matches!(self.reach, Reach::Ceiling) {
            // Decided by the row: one that does not exist is not reached.
            // A ceiling never holds admin, so suspension is always checked.
            return match directory.resolve(catalog).await? {
                Some(row) => self.require(access, &row),
                None => Err(OkfError::CatalogNotSelected {
                    catalog: catalog.to_string(),
                }),
            };
        }
        self.require_level(access, catalog)?;
        if self.access == Access::Admin {
            return Ok(());
        }
        match directory.resolve(catalog).await? {
            Some(row) => self.check_not_suspended(&row),
            None => Ok(()),
        }
    }

    /// The row of a catalog known by name that this principal reaches at
    /// `access`: reach and level are checked first, so a key that does not
    /// reach the catalog learns nothing from the lookup.
    ///
    /// # Arguments
    /// * `directory` - the catalog directory.
    /// * `access` - the level the operation needs.
    /// * `catalog` - the catalog's name.
    ///
    /// # Returns
    /// Its directory row, as it is now.
    ///
    /// # Errors
    /// As [`Self::require_level`]; [`OkfError::CatalogNotSelected`] for a
    /// catalog that does not exist; [`OkfError::Database`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn reached_row(
        &self,
        directory: &CatalogDirectory,
        access: Access,
        catalog: &str,
    ) -> Result<Arc<CatalogRow>, OkfError> {
        self.precheck_level(access, catalog)?;
        let row =
            directory
                .resolve(catalog)
                .await?
                .ok_or_else(|| OkfError::CatalogNotSelected {
                    catalog: catalog.to_string(),
                })?;
        // The whole check for a ceiling; for the others, the same answer
        // as the precheck.
        self.require_level_at(access, &row)?;
        Ok(row)
    }

    /// Checks that the key may export `catalog`: read access, suspended or
    /// not (M13 spec: export stays open so members can take their data).
    ///
    /// # Arguments
    /// * `row` - the directory row of the catalog to export.
    ///
    /// # Errors
    /// As [`Self::require`] with [`Access::Read`], but never
    /// [`OkfError::Suspended`].
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn require_export(&self, row: &CatalogRow) -> Result<(), OkfError> {
        self.require_level_at(Access::Read, row)
    }

    /// The principal of a console session (plan Decision 5): an admin
    /// user acts as an admin on every catalog; anyone else reaches exactly
    /// their ceiling, at the ceiling's level for each catalog
    /// ([`Reach::Ceiling`], resolved per catalog from `facts`' gates).
    ///
    /// Callers pass the user as the user cache knows it now, never as read
    /// at sign-in: the admin flag can change between the password check
    /// and the session's first request.
    ///
    /// # Arguments
    /// * `user` - the signed-in user, from the user cache.
    /// * `facts` - the access facts, with this user's gates (what a
    ///   non-admin's ceiling is resolved from).
    ///
    /// # Returns
    /// The principal. An admin's `access` is `Admin`; a non-admin's is
    /// `Read`, which grants nothing by itself (its levels are the ceiling's,
    /// per catalog) and only ever fails the admin checks; `GET /whoami`
    /// shows the ceiling's highest level instead ([`crate::store::ceiling::highest`]).
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn for_session(user: &UserAuth, facts: Arc<AccessFacts>) -> Self {
        let (access, reach) = if user.is_admin {
            (Access::Admin, Reach::All)
        } else {
            (Access::Read, Reach::Ceiling)
        };
        Self {
            key_id: format!("{SESSION_KEY_PREFIX}{}", user.name),
            access,
            maintenance_report: false,
            provisioner: false,
            reach,
            default_catalog: None,
            actor: user.name.clone(),
            kind: CredentialKind::Session {
                user_id: user.id.clone(),
            },
            user: Some(user.name.clone()),
            user_id: Some(user.id.clone()),
            facts,
        }
    }

    /// How this principal is let into the contents of the catalog `row`
    /// describes, for `access` (spec "Who needs an unlock"). Call it only on
    /// a catalog the principal reaches at that level.
    ///
    /// In order: the maintenance runner (a non-admin reporting key) is
    /// refused where the catalog's owners turned maintenance off, whatever
    /// its grants and whatever `require_unlock`, and needs no unlock to read
    /// (M14 Decision 8: reads only); a customer (in the owner team, or a
    /// non-admin user entitled to the catalog) is let in; with
    /// `require_unlock` off everyone else is too, an admin credential's read
    /// as an admin read; with it on, everyone else needs a live unlock of
    /// their own user on this catalog.
    ///
    /// Every fact is matched by the row's `catalogs.id` (the owner team by
    /// its `teams.id`), never by name, so an entitlement or unlock of a
    /// catalog deleted and created again under its name never opens the new
    /// one.
    ///
    /// # Arguments
    /// * `row` - the catalog's directory row.
    /// * `access` - what the operation needs.
    /// * `now` - the server clock, for the unlock's expiry.
    ///
    /// # Returns
    /// The admission.
    ///
    /// # Errors
    /// [`OkfError::MaintenanceOff`] for the runner on a catalog that opted
    /// out; [`OkfError::UnlockRequired`] when `require_unlock` is on and
    /// nothing above lets the principal in.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn admission(
        &self,
        row: &CatalogRow,
        access: Access,
        now: DateTime<Utc>,
    ) -> Result<Admission, OkfError> {
        if self.maintenance_report && self.access != Access::Admin {
            if row.maintenance_off {
                return Err(OkfError::MaintenanceOff {
                    catalog: row.name.clone(),
                });
            }
            if access == Access::Read {
                return Ok(Admission::Maintenance);
            }
        }
        // `as_deref` borrows the gates out of the `Option<Arc<..>>`.
        let user = self.facts.user.as_deref();
        let admin_user = user.is_some_and(|user| user.is_admin);
        let member = user.is_some_and(|user| {
            row.owner_team_id
                .as_ref()
                .is_some_and(|team| user.teams.contains(team))
        });
        let entitled = user.is_some_and(|user| user.entitled.contains_key(&row.id));
        if member || (!admin_user && entitled) {
            return Ok(Admission::Customer);
        }
        if !self.facts.require_unlock {
            return Ok(if self.access == Access::Admin && access == Access::Read {
                Admission::AdminRead
            } else {
                Admission::Untracked
            });
        }
        // `find` stops at the first live unlock of this very catalog.
        let unlock = user.and_then(|user| {
            user.unlocks
                .iter()
                .find(|unlock| unlock.catalog_id == row.id && unlock.expires_at > now)
        });
        match unlock {
            Some(unlock) => Ok(Admission::Unlocked {
                unlock_id: unlock.id,
            }),
            None => Err(OkfError::UnlockRequired {
                catalog: row.name.clone(),
                has_user: self.user_id.is_some(),
            }),
        }
    }

    /// The content gate at the selection: suspension (M13), then the
    /// admission. The selection has checked reach but no level: a tool that
    /// selects first checks its own level after (M14 Decision 24), so there
    /// a lock is refused before a low level is.
    ///
    /// # Arguments
    /// * `row` - the directory row of a catalog the principal reaches (check
    ///   reach first).
    /// * `access` - what the operation needs.
    /// * `now` - the server clock.
    ///
    /// # Returns
    /// The admission.
    ///
    /// # Errors
    /// [`OkfError::Suspended`] (see [`Self::check_not_suspended`]); then as
    /// [`Self::admission`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn check_open(
        &self,
        row: &CatalogRow,
        access: Access,
        now: DateTime<Utc>,
    ) -> Result<Admission, OkfError> {
        self.check_not_suspended(row)?;
        self.admission(row, access, now)
    }

    /// The whole check for a route that reads or writes a catalog's
    /// contents itself: reach, level and suspension ([`Self::require`]),
    /// then the admission (spec "The refusal": the unlock is checked last).
    /// Administrative routes keep [`Self::require`].
    ///
    /// # Arguments
    /// * `access` - the level the operation needs.
    /// * `row` - the directory row of the catalog it acts on.
    /// * `now` - the server clock.
    ///
    /// # Returns
    /// The admission.
    ///
    /// # Errors
    /// As [`Self::require`]; then as [`Self::admission`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn require_contents(
        &self,
        access: Access,
        row: &CatalogRow,
        now: DateTime<Utc>,
    ) -> Result<Admission, OkfError> {
        self.require(access, row)?;
        self.admission(row, access, now)
    }

    /// This principal as the audit log will record it, with every identity
    /// fixed now from the access facts, so a write that runs later neither
    /// looks anything up by name nor creates a person or credential row
    /// (M14 Decisions 21 and 22).
    ///
    /// # Arguments
    /// * `requester` - the request's address and source.
    /// * `now` - when it was admitted (server clock).
    ///
    /// # Returns
    /// The actor.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn admitted_actor(&self, requester: &Requester, now: DateTime<Utc>) -> AdmittedActor {
        let actor = AuditActor::of(self, requester);
        let user = self.facts.user.as_deref();
        AdmittedActor {
            source: requester.source,
            // A session acts with no credential, so it records none.
            credential: actor.credential.as_ref().and(self.facts.credential),
            person: user.and_then(|user| user.person),
            label: actor.stored_label(),
            // As `audit::record` marks it: an operator credential, or a
            // credential of an admin user.
            operator: actor.operator || user.is_some_and(|user| user.is_admin),
            ip: requester.ip,
            at: now,
        }
    }

    /// This principal's access to `catalog` as the audit log will record
    /// it ([`Self::admitted_actor`], plus the catalog's identity and scope).
    ///
    /// # Arguments
    /// * `requester` - the request's address and source.
    /// * `catalog` - the catalog's name.
    /// * `scope` - the incarnation read ([`CatalogScope::of`] its row).
    /// * `now` - when it was admitted (server clock).
    ///
    /// # Returns
    /// The record.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn access_record(
        &self,
        requester: &Requester,
        catalog: &str,
        scope: CatalogScope,
        now: DateTime<Utc>,
    ) -> AccessRecord {
        AccessRecord {
            actor: self.admitted_actor(requester, now),
            catalog: catalog.to_string(),
            scope,
        }
    }

    /// Requires read access to the catalog and the independent reporting flag.
    ///
    /// # Arguments
    /// * `row` - the catalog's directory row.
    ///
    /// # Errors
    /// As [`Self::require`], or [`OkfError::Forbidden`] when reporting is disabled.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn require_maintenance_report(&self, row: &CatalogRow) -> Result<(), OkfError> {
        self.require(Access::Read, row)?;
        if !self.maintenance_report {
            return Err(OkfError::Forbidden {
                reason: format!("key {:?} cannot submit maintenance reports", self.key_id),
            });
        }
        Ok(())
    }
}

// `From` is Rust's conversion trait: `Principal::from(record)` (or
// `record.into()`) builds a principal from a stored key.
impl From<KeyRecord> for Principal {
    #[tracing::instrument(level = "trace", skip_all)]
    fn from(record: KeyRecord) -> Self {
        let reach = if record.all_catalogs {
            Reach::All
        } else {
            Reach::Catalogs(record.catalog_access)
        };
        let kind = match record.kind {
            KeyKind::Key => CredentialKind::ApiKey,
            // Moving the two fields out of `record` is fine: the struct
            // literal below moves only its other fields.
            KeyKind::OAuth => CredentialKind::OAuth {
                client_id: record.client_id,
                client_name: record.client_name,
            },
        };
        Self {
            key_id: record.id,
            access: record.access,
            maintenance_report: record.maintenance_report,
            provisioner: record.provisioner,
            reach,
            default_catalog: record.default_catalog,
            actor: record.actor,
            kind,
            user: record.user,
            user_id: record.user_id,
            // Filled in by the credential cache (`principal_of`); a
            // principal built outside it admits no customer and records
            // nothing.
            facts: Arc::default(),
        }
    }
}

/// Whether a credential with this expiry still works at `now`.
///
/// # Arguments
/// * `expires_at` - its expiry; `None` never expires.
/// * `now` - the server clock.
///
/// # Returns
/// `true` before the expiry (or with none).
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
    )
)]
pub(crate) fn unexpired(expires_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    // `is_none_or`: true for `None`, else the test's answer.
    expires_at.is_none_or(|at| at > now)
}

/// The ceiling of a self-issued credential whose user has none.
// `BTreeMap::new` is a `const fn`, so an empty map can be a `static`.
static NO_CEILING: Ceiling = BTreeMap::new();

/// A record's principal: as stored for an admin-issued credential, capped by
/// its user's ceiling for a self-issued one (spec "Effective access").
///
/// # Arguments
/// * `record` - the row.
/// * `ceiling` - its user's current ceiling, at least on the record's own
///   catalogs (the cache passes it restricted to them,
///   [`crate::store::ceiling::ceiling_over`]); `None` for a credential whose
///   user is unknown (a self-issued one then reaches nothing).
/// * `facts` - the access facts, shared with the principal.
///
/// # Returns
/// The principal.
#[tracing::instrument(level = "trace", skip_all)]
pub(crate) fn principal_of(
    record: KeyRecord,
    ceiling: Option<&Ceiling>,
    facts: Arc<AccessFacts>,
) -> Principal {
    let ceiling = record.self_issued.then(|| ceiling.unwrap_or(&NO_CEILING));
    let mut principal = Principal::from(record);
    principal.facts = facts;
    if let Some(ceiling) = ceiling {
        let levels = match &principal.reach {
            Reach::Catalogs(levels) => cap(levels, ceiling),
            // The schema refuses a self-issued all-catalogs row; one edited
            // by hand reaches nothing rather than everything. A stored
            // record is never a ceiling reach (only a session is).
            Reach::All | Reach::Ceiling => Ceiling::new(),
        };
        // Its highest level left, for display; `Read` when nothing is left
        // (it then reaches no catalog at all).
        principal.access = levels.values().copied().max().unwrap_or(Access::Read);
        principal.reach = Reach::Catalogs(levels);
    }
    principal
}

/// Why a bearer credential was refused with a 401 (issue #1012): the
/// `okf.refusal` span value on the request span.
///
/// The set is closed. [`AuthFailure::as_str`] is the only spelling that
/// reaches telemetry; the credential, a prefix of it, its hash and its key
/// id never do, and no spelling contains a substring (`auth`, `token`,
/// `secret`, `password`, `key`, `cookie`, `session`) that trace backends'
/// default scrubbers hide. It never changes the response: every variant
/// gets the identical 401 from `unauthorized`.
///
/// Where a refusal cannot be told apart without another query, it is the
/// less specific name: an API key that is revoked or expired is `Unknown`
/// (its cache load answers only "not live"), and so is a console session
/// cookie that matches no live session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthFailure {
    /// No `Authorization` header, a `Bearer` one with an empty token, or no
    /// console session cookie either (the API).
    Missing,
    /// An `Authorization` header that is not `Bearer <token>`: another
    /// scheme, no space, or bytes that are not text.
    Scheme,
    /// A token (or session cookie) that matches no live credential: never
    /// issued, a refresh token's value, an `okfat_` token with OAuth off, an
    /// API key that is revoked or expired, a dead session, or a session
    /// whose user is gone.
    Unknown,
    /// An OAuth access token past its expiry.
    Expired,
    /// An OAuth access token that was revoked, or whose grant is no longer
    /// live (revoked, or past its own expiry).
    Revoked,
    /// An OAuth access token issued for another resource (RFC 8707).
    WrongResource,
}

impl AuthFailure {
    /// The `okf.refusal` span value.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "a const mapping of a variant to its name")
    )]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Scheme => "scheme",
            Self::Unknown => "unknown",
            Self::Expired => "expired",
            Self::Revoked => "revoked",
            Self::WrongResource => "wrong_resource",
        }
    }
}

/// A bearer token's verdict: its principal, or why it was refused.
pub type Verdict = Result<Arc<Principal>, AuthFailure>;

/// Turns a presented bearer token into the principal it acts as. The seam
/// for credential kinds: API keys ([`KeyVerifier`]), OAuth access tokens
/// ([`OAuthVerifier`]) and, in a later round, an external issuer.
// `-> impl Future<...> + Send`: the method returns some future that can move
// between threads (tokio's worker threads need that), without naming its
// type; an implementation may simply be an `async fn`.
pub trait BearerVerifier {
    /// The principal of `token`.
    ///
    /// # Arguments
    /// * `token` - the bearer token, as presented.
    ///
    /// # Returns
    /// The principal, or the [`AuthFailure`] for an unknown, expired,
    /// revoked or wrong-audience token. Naming the failure must cost no
    /// query that a token of another failure does not also cost.
    ///
    /// # Errors
    /// [`OkfError::Database`] when the lookup needs the database and it fails.
    fn verify(&self, token: &str) -> impl Future<Output = Result<Verdict, OkfError>> + Send;
}

/// API keys, through the credential cache.
pub struct KeyVerifier<'a> {
    /// The shared state.
    pub app: &'a App,
}

impl BearerVerifier for KeyVerifier<'_> {
    #[tracing::instrument(level = "debug", skip_all)]
    async fn verify(&self, token: &str) -> Result<Verdict, OkfError> {
        Ok(self.app.find_key(token).await?.ok_or(AuthFailure::Unknown))
    }
}

/// The principal of a bearer token. Dispatch is by prefix with no fallback
/// between kinds: an `okfat_` token is only ever an OAuth access token
/// (refused as unknown with OAuth off), and anything else is looked up only
/// among the API keys, which all carry `okf_`. Refresh tokens, sign-in codes
/// and authorization codes are never bearer credentials and match nothing.
///
/// # Arguments
/// * `app` - the shared state.
/// * `token` - the bearer token.
///
/// # Returns
/// The principal, or why the token was refused.
///
/// # Errors
/// [`OkfError::Database`] when a credential's load fails;
/// [`OkfError::Busy`] for a lookup that shared another's failed load.
#[tracing::instrument(level = "debug", skip_all)]
pub async fn authenticate(app: &App, token: &str) -> Result<Verdict, OkfError> {
    if token.starts_with(keys::ACCESS_TOKEN_PREFIX) {
        return match app.oauth() {
            Some(oauth) => OAuthVerifier { app, oauth }.verify(token).await,
            None => Ok(Err(AuthFailure::Unknown)),
        };
    }
    KeyVerifier { app }.verify(token).await
}

/// The `WWW-Authenticate` value of a 401: with OAuth on, it names the
/// Protected Resource Metadata (RFC 9728 section 5.1), so a client can
/// discover the authorization server.
///
/// # Arguments
/// * `app` - the shared state.
///
/// # Returns
/// `Bearer`, or `Bearer resource_metadata="<public_url>/.well-known/oauth-protected-resource"`.
#[tracing::instrument(level = "debug", skip_all)]
pub fn challenge(app: &App) -> HeaderValue {
    app.oauth().map_or_else(
        || HeaderValue::from_static("Bearer"),
        |oauth| oauth.challenge.clone(),
    )
}

/// The token of an `Authorization: Bearer <token>` header.
///
/// # Arguments
/// * `headers` - the request's headers.
///
/// # Returns
/// The token (trimmed), or `None` without a header, with another scheme
/// (the scheme is matched case-insensitively, as RFC 6750 allows) or with an
/// empty token.
#[tracing::instrument(level = "debug", skip_all)]
pub fn bearer(headers: &HeaderMap) -> Option<&str> {
    credential(headers).ok()
}

/// As [`bearer`], naming why there is no token.
///
/// # Arguments
/// * `headers` - the request's headers.
///
/// # Returns
/// The token (trimmed); [`AuthFailure::Missing`] without a header or with an
/// empty `Bearer` token; [`AuthFailure::Scheme`] for any other header value.
#[tracing::instrument(level = "debug", skip_all)]
pub fn credential(headers: &HeaderMap) -> Result<&str, AuthFailure> {
    let value = headers
        .get(header::AUTHORIZATION)
        .ok_or(AuthFailure::Missing)?
        .to_str()
        .map_err(|_| AuthFailure::Scheme)?;
    // A bare `Bearer` is a scheme with no token; anything else bare is no scheme.
    let (scheme, token) = value.split_once(' ').unwrap_or((value, ""));
    if !scheme.eq_ignore_ascii_case("bearer") {
        return Err(AuthFailure::Scheme);
    }
    let token = token.trim();
    if token.is_empty() {
        return Err(AuthFailure::Missing);
    }
    Ok(token)
}

/// The 401 for a request without a usable credential, with [`challenge`].
///
/// # Arguments
/// * `app` - the shared state.
///
/// # Returns
/// The response.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) fn unauthorized(app: &App) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, challenge(app))],
        "missing or unknown API key\n",
    )
        .into_response()
}

/// The 401 for a refused request, after recording why on the request span
/// ([`record_auth_failure`]). The response is [`unauthorized`]'s whatever
/// the failure.
///
/// # Arguments
/// * `app` - the shared state.
/// * `extensions` - the request's extensions, which hold the request span.
/// * `failure` - why the credential was refused.
///
/// # Returns
/// The response.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) fn refuse(
    app: &App,
    extensions: &axum::http::Extensions,
    failure: AuthFailure,
) -> Response {
    record_auth_failure(extensions, failure);
    unauthorized(app)
}

/// Records `okf.refusal` on the request span the telemetry middleware
/// stored in `extensions`, if there is one (no span for an unmatched route).
/// The span handle, not `Span::current()`: under a raised trace filter the
/// current span is a child that declares no such field.
///
/// # Arguments
/// * `extensions` - the request's extensions.
/// * `failure` - why the credential was refused.
#[tracing::instrument(level = "trace", skip_all)]
pub(crate) fn record_auth_failure(extensions: &axum::http::Extensions, failure: AuthFailure) {
    if let Some(crate::http::RequestSpan(span)) = extensions.get::<crate::http::RequestSpan>() {
        span.record("okf.refusal", failure.as_str());
    }
}

/// The `okf.key.id` span attribute for a principal: the credential's id for
/// an API key or an OAuth grant, and the bare word `session` for a console
/// session. A session's `key_id` is `session:<user name>` (see
/// [`SESSION_KEY_PREFIX`]), and a name never goes in a span attribute.
///
/// # Arguments
/// * `principal` - who the request acts as.
///
/// # Returns
/// The value to record.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(uninstrumented_fn, reason = "a pure mapping of two fields")
)]
pub(crate) fn span_key_id(principal: &Principal) -> &str {
    match principal.kind {
        CredentialKind::Session { .. } => "session",
        CredentialKind::ApiKey | CredentialKind::OAuth { .. } => &principal.key_id,
    }
}

/// Records the principal's ids on `span`: `enduser.id` (the user's id, when
/// the credential has a user) and `okf.key.id` ([`span_key_id`]). The one
/// helper both the HTTP request span and the MCP tool span use, so the two
/// never disagree about what may be exported. `span` must be a handle to the
/// span (the request or tool root), never `Span::current()`, which under a
/// raised trace filter is a child span that declares neither field.
///
/// # Arguments
/// * `span` - the root span's handle.
/// * `principal` - who the request acts as.
#[tracing::instrument(level = "trace", skip_all)]
pub(crate) fn record_principal_ids(span: &tracing::Span, principal: &Principal) {
    span.record("okf.key.id", span_key_id(principal));
    if let Some(user_id) = principal.user_id.as_deref() {
        span.record("enduser.id", user_id);
    }
}

/// Records the principal's ids on the request span the telemetry middleware
/// stored in `extensions`, if there is one (no span for an unmatched route).
///
/// # Arguments
/// * `extensions` - the request's extensions.
/// * `principal` - who the request acts as.
#[tracing::instrument(level = "trace", skip_all)]
pub(crate) fn record_on_request_span(extensions: &axum::http::Extensions, principal: &Principal) {
    if let Some(crate::http::RequestSpan(span)) = extensions.get::<crate::http::RequestSpan>() {
        record_principal_ids(span, principal);
    }
}

/// axum middleware: admits a request with a live API key or OAuth access
/// token, inserting its `Arc<Principal>` into the request's extensions, and
/// answers anything else with 401 and [`challenge`].
///
/// # Arguments
/// * `app` - the shared state (the layer's state).
/// * `request` - the incoming request.
/// * `next` - the rest of the stack.
///
/// # Returns
/// The route's response; 401 with the challenge; or 500 if an access-token
/// lookup fails (logged).
// Used as `axum::middleware::from_fn_with_state(app, require_key)`; axum
// fills each argument from the request (the extractor pattern).
#[tracing::instrument(level = "debug", skip_all)]
pub async fn require_key(
    State(app): State<Arc<App>>,
    mut request: Request,
    next: Next,
) -> Response {
    // The borrowed token (`&str` into the headers) ends with this `match`,
    // before the request is changed below.
    let found = match credential(request.headers()) {
        Ok(token) => authenticate(&app, token).await,
        Err(failure) => Ok(Err(failure)),
    };
    match found {
        Ok(Ok(principal)) if principal.provisioner => {
            // Audited as the API's provisioner gate does: the key, the
            // method and the matched template, never the literal URI.
            crate::http::console_session::insert_client_ip(&app, &mut request);
            crate::http::api::AdmittedRefusal::of(&app, &principal, &request)
                .note(&app, crate::store::audit::RefusalReason::RouteNotAllowed);
            AppError(OkfError::Forbidden {
                reason: "a provisioner key is not an MCP or archive credential".to_string(),
            })
            .into_response()
        }
        Ok(Ok(principal)) => {
            split_event!(
                debug,
                {},
                { key = %principal.key_id, },
                "authenticated"
            );
            record_on_request_span(request.extensions(), &principal);
            request.extensions_mut().insert(principal);
            crate::http::console_session::insert_client_ip(&app, &mut request);
            next.run(request).await
        }
        Ok(Err(failure)) => refuse(&app, request.extensions(), failure),
        // `AppError` logs it and answers 500 without detail.
        Err(error) => AppError(error).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::routing::get;
    use axum::Router;
    use chrono::Utc;

    use crate::config::Config;
    use crate::store::catalogs::SuspendedRow;
    use crate::store::ceiling::MemberAccess;
    use crate::store::gates::UserUnlock;
    use crate::store::keys as key_rows;
    // `oneshot` sends one request through a router without a socket.
    use tower::ServiceExt;

    /// The catalog-to-level map of a record granted `names` at `access`.
    fn levels(names: &[&str], access: Access) -> BTreeMap<String, Access> {
        names
            .iter()
            .map(|name| (name.to_string(), access))
            .collect()
    }

    fn record(id: &str, key: &str, revoked: bool) -> KeyRecord {
        KeyRecord {
            id: id.to_string(),
            name: id.to_string(),
            sha256: Some(keys::hash(key).to_vec()),
            access: Access::Read,
            maintenance_report: false,
            provisioner: false,
            all_catalogs: false,
            catalogs: vec!["pg".to_string()],
            catalog_access: levels(&["pg"], Access::Read),
            default_catalog: Some("pg".to_string()),
            actor: format!("actor-{id}"),
            created_at: Utc::now(),
            revoked_at: revoked.then(Utc::now),
            expires_at: None,
            kind: KeyKind::Key,
            client_id: None,
            client_name: None,
            user: Some(crate::db::TEST_USER.to_string()),
            user_id: None,
            self_issued: false,
            last_activity: None,
        }
    }

    fn grant_record(id: &str, revoked: bool) -> KeyRecord {
        KeyRecord {
            sha256: None,
            kind: KeyKind::OAuth,
            client_id: Some("https://client.example.com/client.json".into()),
            client_name: Some("Example Client".into()),
            ..record(id, "unused", revoked)
        }
    }

    /// The directory row of catalog `name` (id `id`), owned by team
    /// `family` (`t-family`), active, with maintenance on.
    fn row(name: &str, id: i64) -> CatalogRow {
        CatalogRow {
            store: crate::stores::StoreName::main(),
            id,
            name: name.to_string(),
            display_name: String::new(),
            description: String::new(),
            generation: 1,
            owner_team: Some("family".to_string()),
            owner_team_id: Some("t-family".to_string()),
            suspended: None,
            maintenance_off: false,
            audit_team: Some(40 + id),
            member_access: MemberAccess::Write,
        }
    }

    /// The access facts of a console session whose user has `gates`.
    fn session_facts(gates: UserGates) -> Arc<AccessFacts> {
        Arc::new(AccessFacts {
            user: Some(Arc::new(gates)),
            ..AccessFacts::default()
        })
    }

    /// [`row`], with its owner team suspended.
    fn suspended_row(name: &str, id: i64) -> CatalogRow {
        CatalogRow {
            suspended: Some(SuspendedRow {
                team: "family".to_string(),
                notice: Some("Paused.".to_string()),
                team_label: "family".to_string(),
            }),
            ..row(name, id)
        }
    }

    fn headers(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, value.parse().unwrap());
        headers
    }

    #[test]
    fn credential_names_why_there_is_no_bearer_token() {
        assert_eq!(credential(&headers("Bearer reader-key")), Ok("reader-key"));
        assert_eq!(credential(&HeaderMap::new()), Err(AuthFailure::Missing));
        assert_eq!(credential(&headers("Bearer ")), Err(AuthFailure::Missing));
        assert_eq!(credential(&headers("Bearer")), Err(AuthFailure::Missing));
        assert_eq!(credential(&headers("Basic abc")), Err(AuthFailure::Scheme));
        assert_eq!(credential(&headers("reader-key")), Err(AuthFailure::Scheme));
        let mut binary = HeaderMap::new();
        binary.insert(
            header::AUTHORIZATION,
            HeaderValue::from_bytes(b"Bearer \xff").expect("opaque bytes"),
        );
        assert_eq!(credential(&binary), Err(AuthFailure::Scheme));
    }

    #[test]
    fn bearer_reads_the_token_with_any_scheme_case() {
        assert_eq!(bearer(&headers("Bearer reader-key")), Some("reader-key"));
        assert_eq!(bearer(&headers("bearer  reader-key ")), Some("reader-key"));
        assert_eq!(bearer(&headers("Basic reader-key")), None);
        assert_eq!(bearer(&headers("Bearer ")), None);
        assert_eq!(bearer(&headers("reader-key")), None);
        assert_eq!(bearer(&HeaderMap::new()), None);
    }

    #[test]
    fn a_record_becomes_its_principal_and_a_grant_carries_its_client() {
        let key = Principal::from(record("reader", "reader-key", false));
        let grant = Principal::from(grant_record("grant-1", false));

        assert_eq!(key.key_id, "reader");
        assert_eq!(key.actor, "actor-reader");
        assert_eq!(key.user.as_deref(), Some(crate::db::TEST_USER));
        assert_eq!(key.default_catalog.as_deref(), Some("pg"));
        assert_eq!(key.kind, CredentialKind::ApiKey);
        assert_eq!(
            grant.kind,
            CredentialKind::OAuth {
                client_id: Some("https://client.example.com/client.json".into()),
                client_name: Some("Example Client".into())
            }
        );
        assert_eq!(
            *key.facts,
            AccessFacts::default(),
            "nothing outside the cache"
        );
    }

    #[test]
    fn maintenance_capability_does_not_imply_write() {
        let mut reporter_record = record("reporter", "reporter-key", false);
        reporter_record.maintenance_report = true;
        let reporter = Principal::from(reporter_record);
        let pg = row("pg", 1);
        assert!(reporter.require_maintenance_report(&pg).is_ok());
        assert!(reporter.require(Access::Write, &pg).is_err());
        assert!(matches!(
            reporter.require_maintenance_report(&row("other", 2)),
            Err(OkfError::CatalogNotSelected { .. })
        ));
        let writer_without_flag = Principal {
            access: Access::Write,
            maintenance_report: false,
            provisioner: false,
            ..reporter.clone()
        };
        assert!(matches!(
            writer_without_flag.require_maintenance_report(&pg),
            Err(OkfError::Forbidden { .. })
        ));
        let admin_without_flag = Principal {
            access: Access::Admin,
            ..writer_without_flag
        };
        assert!(admin_without_flag.require_maintenance_report(&pg).is_err());
    }

    #[test]
    fn require_checks_reach_before_access() {
        let principal = Principal::from(record("reader", "reader-key", false));
        let pg = row("pg", 1);

        assert!(principal.require(Access::Read, &pg).is_ok());
        assert!(matches!(
            principal.require(Access::Write, &pg),
            Err(OkfError::Forbidden { .. })
        ));
        assert!(matches!(
            principal.require(Access::Admin, &pg),
            Err(OkfError::Forbidden { .. })
        ));
        // A write key may write and read, but not administer. Its level is
        // per catalog, so the record (not just `access`) says write.
        let writer = Principal::from(KeyRecord {
            access: Access::Write,
            catalog_access: levels(&["pg"], Access::Write),
            ..record("writer", "writer-key", false)
        });
        assert!(writer.require(Access::Write, &pg).is_ok());
        assert!(writer.require(Access::Read, &pg).is_ok());
        assert!(matches!(
            writer.require(Access::Admin, &pg),
            Err(OkfError::Forbidden { .. })
        ));
        // A catalog the key does not reach is "not available", whatever the access.
        assert!(matches!(
            principal.require(Access::Read, &row("work", 2)),
            Err(OkfError::CatalogNotSelected { catalog }) if catalog == "work"
        ));
        let mut everything = record("all", "all-key", false);
        everything.all_catalogs = true;
        everything.catalogs.clear();
        everything.catalog_access.clear();
        assert!(Principal::from(everything).reaches("work"));
    }

    #[test]
    fn require_checks_the_level_of_each_catalog() {
        // A write key whose grant of `work` is read only.
        let principal = Principal::from(KeyRecord {
            access: Access::Write,
            catalogs: vec!["pg".to_string(), "work".to_string()],
            catalog_access: BTreeMap::from([
                ("pg".to_string(), Access::Write),
                ("work".to_string(), Access::Read),
            ]),
            ..record("mixed", "mixed-key", false)
        });
        let everything = Principal {
            access: Access::Write,
            reach: Reach::All,
            ..principal.clone()
        };

        assert_eq!(principal.level("pg"), Some(Access::Write));
        assert_eq!(principal.level("work"), Some(Access::Read));
        assert_eq!(principal.level("other"), None);
        assert!(principal.require(Access::Write, &row("pg", 1)).is_ok());
        assert!(principal.require(Access::Read, &row("work", 2)).is_ok());
        assert!(matches!(
            principal.require(Access::Write, &row("work", 2)),
            Err(OkfError::Forbidden { reason }) if reason.contains("\"work\"")
        ));
        assert!(matches!(
            principal.require_level(Access::Read, "other"),
            Err(OkfError::CatalogNotSelected { .. })
        ));
        // Every catalog: the principal's own level everywhere.
        assert_eq!(everything.level("anything"), Some(Access::Write));
    }

    #[test]
    fn self_issued_credentials_are_capped_by_their_users_ceiling_and_admin_issued_ones_are_not() {
        // A key of user `alice-id` granted pg and work at write.
        let alices = |id: &str, secret: &str, self_issued: bool| KeyRecord {
            access: Access::Write,
            catalogs: vec!["pg".to_string(), "work".to_string()],
            catalog_access: levels(&["pg", "work"], Access::Write),
            user_id: Some("alice-id".to_string()),
            self_issued,
            last_activity: None,
            ..record(id, secret, false)
        };
        // A self-issued OAuth grant of hers on pg at write.
        let self_grant = KeyRecord {
            access: Access::Write,
            catalog_access: levels(&["pg"], Access::Write),
            user_id: Some("alice-id".to_string()),
            self_issued: true,
            last_activity: None,
            ..grant_record("grant-mine", false)
        };
        let alice = BTreeMap::from([
            ("pg".to_string(), Access::Read),
            ("docs".to_string(), Access::Write),
        ]);
        let capped = |record: KeyRecord, ceiling: Option<&Ceiling>| {
            principal_of(record, ceiling, Arc::default())
        };

        let mine = capped(alices("mine", "mine-key", true), Some(&alice));
        let issued = capped(alices("issued", "issued-key", false), Some(&alice));
        let grant = capped(self_grant, Some(&alice));
        // A self-issued key whose user is unknown: no ceiling at all.
        let unknown_user = capped(alices("stranger", "stranger-key", true), None);

        // pg: the lower of write and read; work: not in the ceiling; docs:
        // not in the grant.
        assert_eq!(mine.reach, Reach::Catalogs(levels(&["pg"], Access::Read)));
        assert_eq!(mine.access, Access::Read, "its highest level left");
        assert_eq!(
            issued.reach,
            Reach::Catalogs(levels(&["pg", "work"], Access::Write))
        );
        assert_eq!(issued.access, Access::Write);
        assert_eq!(
            grant.level("pg"),
            Some(Access::Read),
            "grants are capped too"
        );
        assert_eq!(unknown_user.reach, Reach::Catalogs(BTreeMap::new()));
    }

    /// A router whose one route echoes the principal's key id, behind the
    /// real middleware over `app`.
    fn echo(app: &Arc<App>) -> Router {
        Router::new()
            .route(
                "/",
                get(|request: Request| async move {
                    request
                        .extensions()
                        .get::<Arc<Principal>>()
                        .map(|principal| principal.key_id.clone())
                        .unwrap_or_default()
                }),
            )
            .route_layer(axum::middleware::from_fn_with_state(
                Arc::clone(app),
                require_key,
            ))
            .with_state(Arc::clone(app))
    }

    /// The response's status, its `WWW-Authenticate` header and its body.
    async fn status_and_body(
        router: Router,
        authorization: Option<&str>,
    ) -> (StatusCode, Option<String>, String) {
        let mut request = axum::http::Request::builder().uri("/");
        if let Some(value) = authorization {
            request = request.header(header::AUTHORIZATION, value);
        }
        let response = router
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let challenge = response
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .map(|value| value.to_str().unwrap().to_string());
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
            .await
            .unwrap();
        (
            status,
            challenge,
            String::from_utf8(bytes.to_vec()).unwrap(),
        )
    }

    /// A stored read key on `main` called `name`.
    async fn stored_key(app: &App, name: &str) -> (String, String) {
        let names = ["main".to_string()];
        let created = key_rows::create_with_id(
            app.accounts().executor(),
            &key_rows::NewKey {
                name,
                access: Access::Read,
                maintenance_report: false,
                grants: key_rows::Grants::Catalogs(&names),
                default_catalog: Some("main"),
                actor: "example-actor",
                user: crate::db::TEST_USER,
                self_issued: false,
                expires_at: None,
                levels: &[],
            },
            &crate::store::audit::AuditActor::cli(),
            Utc::now(),
        )
        .await
        .unwrap();
        (created.id, created.key)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_middleware_admits_a_known_key_and_refuses_the_rest() {
        let test = crate::mcp::testing::app_with(&[("main", &[])], Config::default()).await;
        let (id, key) = stored_key(&test.app, "reader").await;
        let (old_id, old_key) = stored_key(&test.app, "old").await;
        key_rows::revoke_by_id(
            test.app.accounts().executor(),
            &old_id,
            &crate::store::audit::AuditActor::cli(),
            Utc::now(),
        )
        .await
        .unwrap();
        let router = || echo(&test.app);

        let admitted = status_and_body(router(), Some(&format!("Bearer {key}"))).await;
        let refused = [
            status_and_body(router(), Some("Bearer okf_other-key")).await,
            status_and_body(router(), Some(&format!("Bearer {old_key}"))).await,
            status_and_body(router(), Some(&format!("Basic {key}"))).await,
            // With OAuth off, an access-token-shaped bearer is simply unknown.
            status_and_body(router(), Some("Bearer okfat_anything")).await,
            status_and_body(router(), None).await,
        ];

        assert_eq!(admitted, (StatusCode::OK, None, id));
        for (status, challenge, body) in refused {
            assert_eq!(status, StatusCode::UNAUTHORIZED);
            assert_eq!(challenge.as_deref(), Some("Bearer"));
            assert_eq!(body, "missing or unknown API key\n");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn with_oauth_on_a_401_names_the_resource_metadata() {
        let config = crate::store::oauth::testing::oauth_config("https://okf.example.com");
        let test = crate::mcp::testing::app_with(&[("main", &[])], config).await;
        let (_, key) = stored_key(&test.app, "reader").await;

        let refused = status_and_body(echo(&test.app), None).await;
        let admitted = status_and_body(echo(&test.app), Some(&format!("Bearer {key}"))).await;

        assert_eq!(refused.0, StatusCode::UNAUTHORIZED);
        assert_eq!(
            refused.1.as_deref(),
            Some(
                "Bearer resource_metadata=\"https://okf.example.com/.well-known/oauth-protected-resource\""
            )
        );
        // Static keys keep working beside OAuth.
        assert_eq!(admitted.0, StatusCode::OK);
    }

    fn user(is_admin: bool) -> UserAuth {
        UserAuth {
            id: "9eaae459-441c-49c2-8613-e3996bdc96ee".to_string(),
            name: "alice".to_string(),
            display_name: String::new(),
            email: Some("alice@example.com".to_string()),
            is_admin,
        }
    }

    #[test]
    fn a_non_admin_session_reaches_its_ceiling_at_each_level() {
        // Entitled to `main` (1) at write and `other` (2) at read; in no
        // team, so the owner's member access gives nothing.
        let principal = Principal::for_session(
            &user(false),
            session_facts(UserGates {
                entitled: [(1, Access::Write), (2, Access::Read)].into(),
                ..UserGates::default()
            }),
        );

        assert_eq!(principal.key_id, "session:alice");
        assert_eq!(principal.actor, "alice");
        assert_eq!(principal.user.as_deref(), Some("alice"));
        assert_eq!(principal.reach, Reach::Ceiling);
        assert_eq!(
            principal.access,
            Access::Read,
            "grants nothing by itself: the levels are the ceiling's"
        );
        assert!(!principal.maintenance_report);
        assert!(principal.require(Access::Write, &row("main", 1)).is_ok());
        assert!(matches!(
            principal.require(Access::Write, &row("other", 2)),
            Err(OkfError::Forbidden { .. })
        ));
        assert!(principal.require(Access::Read, &row("other", 2)).is_ok());
        assert!(matches!(
            principal.require(Access::Read, &row("elsewhere", 3)),
            Err(OkfError::CatalogNotSelected { .. })
        ));
        assert!(matches!(
            principal.require(Access::Admin, &row("main", 1)),
            Err(OkfError::Forbidden { .. })
        ));
        assert!(
            matches!(principal.kind, CredentialKind::Session { ref user_id } if user_id == &user(false).id)
        );
    }

    #[test]
    fn an_admin_users_session_is_an_admin_on_every_catalog() {
        let principal = Principal::for_session(&user(true), Arc::default());

        assert_eq!(principal.access, Access::Admin);
        assert_eq!(principal.reach, Reach::All);
        assert!(principal
            .require(Access::Admin, &row("anything", 1))
            .is_ok());
    }

    #[test]
    fn a_session_with_an_empty_ceiling_reaches_nothing() {
        let principal = Principal::for_session(&user(false), session_facts(UserGates::default()));
        let gateless = Principal::for_session(&user(false), Arc::default());

        assert_eq!(principal.access, Access::Read);
        assert!(!principal.reaches("main"));
        assert!(!principal.reaches_row(&row("main", 1)));
        assert!(
            !gateless.reaches_row(&row("main", 1)),
            "no gates, no ceiling"
        );
    }

    /// An API key reaching `catalogs`, each at `access`, with no user.
    fn key_on(catalogs: &[&str], access: Access) -> Principal {
        Principal {
            key_id: "k".to_string(),
            access,
            maintenance_report: true,
            provisioner: false,
            reach: Reach::Catalogs(levels(catalogs, access)),
            default_catalog: None,
            actor: "a".to_string(),
            kind: CredentialKind::ApiKey,
            user: None,
            user_id: None,
            facts: Arc::default(),
        }
    }

    /// The gates of a user in team `family`, which owns every [`row`].
    fn member() -> UserGates {
        UserGates {
            teams: ["t-family".to_string()].into(),
            ..UserGates::default()
        }
    }

    #[test]
    fn an_admin_reading_outside_its_teams_is_an_admin_read_and_a_member_is_a_customer() {
        let principal = |user: Option<UserGates>, access| Principal {
            user_id: user.as_ref().map(|_| "u".to_string()),
            facts: Arc::new(AccessFacts {
                user: user.map(Arc::new),
                ..AccessFacts::default()
            }),
            // `key_on` opts into reporting; Task 9b exempts reporting keys.
            maintenance_report: false,
            ..key_on(&["main"], access)
        };
        let now = Utc::now();
        let main = row("main", 1);
        let admitted = |user, access, needs| principal(user, access).admission(&main, needs, now);

        let member_admin = admitted(Some(member()), Access::Admin, Access::Read);
        let staff = admitted(Some(UserGates::default()), Access::Admin, Access::Read);
        let legacy = admitted(None, Access::Admin, Access::Read);
        let staff_write = admitted(Some(UserGates::default()), Access::Admin, Access::Write);
        let reader = admitted(Some(UserGates::default()), Access::Read, Access::Read);

        assert_eq!(member_admin.unwrap(), Admission::Customer);
        assert_eq!(staff.unwrap(), Admission::AdminRead);
        assert_eq!(
            legacy.unwrap(),
            Admission::AdminRead,
            "a key with no user always counts"
        );
        assert_eq!(
            staff_write.unwrap(),
            Admission::Untracked,
            "writes are recorded as publishes"
        );
        assert_eq!(reader.unwrap(), Admission::Untracked);
        assert!(Admission::AdminRead.is_recorded());
        assert!(!Admission::Customer.is_recorded() && !Admission::Untracked.is_recorded());
    }

    #[test]
    fn membership_and_entitlement_follow_the_rows_owner_and_id_not_the_name() {
        let now = Utc::now();
        let customer = |gates: UserGates| Principal {
            user_id: Some("u".to_string()),
            facts: Arc::new(AccessFacts {
                user: Some(Arc::new(gates)),
                require_unlock: true,
                ..AccessFacts::default()
            }),
            maintenance_report: false,
            ..key_on(&["main"], Access::Read)
        };
        let entitled = customer(UserGates {
            entitled: [(1, Access::Read)].into(),
            ..UserGates::default()
        });
        // `main` deleted and created again (id 2) for another team.
        let replacement = CatalogRow {
            owner_team_id: Some("t-acme".to_string()),
            ..row("main", 2)
        };

        assert_eq!(
            entitled
                .admission(&row("main", 1), Access::Read, now)
                .unwrap(),
            Admission::Customer
        );
        assert!(
            matches!(
                entitled.admission(&replacement, Access::Read, now),
                Err(OkfError::UnlockRequired { .. })
            ),
            "an entitlement of the old catalog does not open the new one"
        );
        assert!(
            matches!(
                customer(member()).admission(&replacement, Access::Read, now),
                Err(OkfError::UnlockRequired { .. })
            ),
            "a member of the old owner is not one of the new"
        );
    }

    #[test]
    fn require_refuses_a_suspended_catalog_after_reach_and_level() {
        // `theirs` is suspended too, but this key does not reach it.
        let principal = key_on(&["shared", "mine"], Access::Write);
        let shared = suspended_row("shared", 1);

        let suspended = principal.require(Access::Read, &shared);
        let unreached = principal.require(Access::Read, &suspended_row("theirs", 2));
        let too_low = principal.require(Access::Admin, &shared);
        let fine = principal.require(Access::Write, &row("mine", 3));
        let reporting = principal.require_maintenance_report(&shared);

        assert!(matches!(
            suspended,
            Err(OkfError::Suspended { ref catalog, ref team, ref notice, .. })
                if catalog == "shared" && team == "family" && notice.as_deref() == Some("Paused.")
        ));
        assert!(
            matches!(unreached, Err(OkfError::CatalogNotSelected { .. })),
            "an unreached catalog never reads as suspended: {unreached:?}"
        );
        assert!(
            matches!(too_low, Err(OkfError::Forbidden { .. })),
            "level is checked before suspension: {too_low:?}"
        );
        assert!(fine.is_ok());
        assert!(matches!(reporting, Err(OkfError::Suspended { .. })));
    }

    #[test]
    fn a_suspension_is_named_with_the_labels_of_its_row() {
        let principal = key_on(&["shared", "new"], Access::Read);
        let mut shared = suspended_row("shared", 1);
        shared.display_name = "Shared notes".to_string();
        if let Some(suspended) = shared.suspended.as_mut() {
            suspended.team_label = "The Family".to_string();
        }
        let unlabelled = suspended_row("new", 2);

        let named = principal.suspension(&shared).unwrap();
        let plain = principal.suspension(&unlabelled).unwrap();

        assert_eq!(
            (named.catalog_label.as_str(), named.team_label.as_str()),
            ("Shared notes", "The Family")
        );
        assert_eq!(
            plain.catalog_label,
            crate::labels::catalog_label("", ""),
            "a catalog with no name or description reads as the rule says"
        );
        assert_eq!(principal.suspension(&row("shared", 1)), None);
    }

    #[test]
    fn an_admin_is_not_suspended_and_export_ignores_suspension() {
        let admin = key_on(&["shared"], Access::Admin);
        let reader = key_on(&["shared"], Access::Read);
        let shared = suspended_row("shared", 1);

        assert!(admin.require(Access::Admin, &shared).is_ok());
        assert!(admin.require(Access::Write, &shared).is_ok());
        assert!(reader.require_export(&shared).is_ok());
        assert!(matches!(
            reader.require_export(&row("other", 2)),
            Err(OkfError::CatalogNotSelected { .. })
        ));
        assert_eq!(
            reader.suspension(&shared).map(|s| s.team),
            Some("family".to_string()),
            "the fact is visible to anyone who reaches it"
        );
        assert_eq!(reader.suspension(&row("mine", 2)), None);
    }

    #[test]
    fn a_member_session_is_suspended_and_an_admin_session_is_not() {
        let shared = suspended_row("shared", 1);

        // In the owner team, whose member access is `write`.
        let member = Principal::for_session(&user(false), session_facts(member()));
        let admin = Principal::for_session(&user(true), Arc::default());

        assert!(matches!(
            member.require(Access::Read, &shared),
            Err(OkfError::Suspended { .. })
        ));
        assert!(member.require_export(&shared).is_ok());
        assert!(admin.require(Access::Write, &shared).is_ok());
    }

    #[hegel::test]
    fn bearer_never_panics_and_returns_a_trimmed_nonempty_token(tc: hegel::TestCase) {
        use hegel::generators;
        let value: String = tc.draw(generators::text());
        let mut headers = HeaderMap::new();
        // Text that is not a valid header value is simply not a header.
        let Ok(parsed) = value.parse() else { return };
        headers.insert(header::AUTHORIZATION, parsed);

        if let Some(token) = bearer(&headers) {
            assert!(!token.is_empty());
            assert_eq!(token, token.trim());
        }
    }

    /// M14 Task 9b's users of `main` (catalog 1, owned by `family`):
    /// `member` is in the owner team, `entitled` (a customer) and `staff`
    /// (an admin user) are entitled to it, `other` is neither, and nobody has
    /// an unlock.
    fn gates_of(who: &str) -> UserGates {
        match who {
            "member" => member(),
            "entitled" => UserGates {
                entitled: [(1, Access::Read)].into(),
                ..UserGates::default()
            },
            "staff" => UserGates {
                is_admin: true,
                entitled: [(1, Access::Read)].into(),
                ..UserGates::default()
            },
            _ => UserGates::default(),
        }
    }

    /// A key of `user` (none for a legacy key) on `main` at `access`, with
    /// its gates, and not a reporting key (`key_on` opts into reporting,
    /// which `admission` treats as the maintenance runner).
    fn as_user(user: Option<UserGates>, access: Access, require_unlock: bool) -> Principal {
        Principal {
            user_id: user.as_ref().map(|_| "u".to_string()),
            facts: Arc::new(AccessFacts {
                user: user.map(Arc::new),
                require_unlock,
                ..AccessFacts::default()
            }),
            maintenance_report: false,
            ..key_on(&["main"], access)
        }
    }

    #[test]
    fn with_require_unlock_operators_are_refused_and_customers_are_not() {
        let now = Utc::now();
        let main = row("main", 1);
        let refused = |principal: Principal| {
            matches!(
                principal.admission(&main, Access::Read, now),
                Err(OkfError::UnlockRequired { .. })
            )
        };
        let admitted = |principal: Principal| principal.admission(&main, Access::Read, now);
        let of = |who| Some(gates_of(who));

        assert!(
            refused(as_user(of("staff"), Access::Admin, true)),
            "an admin user, even entitled"
        );
        assert!(
            refused(as_user(of("other"), Access::Read, true)),
            "an admin-issued key outside its teams"
        );
        assert!(
            matches!(
                as_user(None, Access::Read, true).admission(&main, Access::Read, now),
                Err(OkfError::UnlockRequired {
                    has_user: false,
                    ..
                })
            ),
            "a key with no user, told it has none"
        );
        assert_eq!(
            admitted(as_user(of("member"), Access::Admin, true)).unwrap(),
            Admission::Customer,
            "a member's admin key"
        );
        assert_eq!(
            admitted(as_user(of("entitled"), Access::Read, true)).unwrap(),
            Admission::Customer,
            "an entitled customer"
        );
        assert_eq!(
            admitted(as_user(of("staff"), Access::Admin, false)).unwrap(),
            Admission::AdminRead,
            "off: nothing changes but the hourly record"
        );
        assert_eq!(
            admitted(as_user(None, Access::Read, false)).unwrap(),
            Admission::Untracked
        );
    }

    #[test]
    fn an_unlock_admits_exactly_its_user_to_exactly_its_catalog_until_it_expires() {
        let now = Utc::now();
        let with_unlock = UserGates {
            unlocks: vec![UserUnlock {
                id: 7,
                catalog_id: 1,
                expires_at: now + chrono::TimeDelta::hours(1),
            }],
            ..gates_of("staff")
        };
        let staff = as_user(Some(with_unlock), Access::Admin, true);
        let other = as_user(Some(gates_of("staff")), Access::Admin, true);
        let elsewhere = Principal {
            reach: Reach::All,
            ..staff.clone()
        };
        let main = row("main", 1);

        assert_eq!(
            staff.admission(&main, Access::Write, now).unwrap(),
            Admission::Unlocked { unlock_id: 7 }
        );
        assert_eq!(
            staff.admission(&main, Access::Read, now).unwrap(),
            Admission::Unlocked { unlock_id: 7 }
        );
        assert!(Admission::Unlocked { unlock_id: 7 }.is_recorded());
        assert!(
            staff
                .admission(&main, Access::Read, now + chrono::TimeDelta::hours(1))
                .is_err(),
            "expired at its expiry"
        );
        assert!(
            other.admission(&main, Access::Read, now).is_err(),
            "another admin"
        );
        assert!(
            elsewhere
                .admission(&row("other", 2), Access::Read, now)
                .is_err(),
            "another catalog"
        );
        assert!(
            staff.admission(&row("main", 3), Access::Read, now).is_err(),
            "the catalog created again under its name"
        );
    }

    #[test]
    fn the_maintenance_runner_needs_no_unlock_unless_the_catalog_opted_out() {
        let now = Utc::now();
        let main = row("main", 1);
        let opted_out = CatalogRow {
            maintenance_off: true,
            ..row("main", 1)
        };
        let runner = |access, require_unlock| Principal {
            maintenance_report: true,
            ..as_user(Some(gates_of("other")), access, require_unlock)
        };

        assert_eq!(
            runner(Access::Read, true)
                .admission(&main, Access::Read, now)
                .unwrap(),
            Admission::Maintenance
        );
        assert!(!Admission::Maintenance.is_recorded());
        assert!(matches!(
            runner(Access::Read, true).admission(&opted_out, Access::Read, now),
            Err(OkfError::MaintenanceOff { .. })
        ));
        assert!(
            matches!(
                runner(Access::Admin, true).admission(&main, Access::Read, now),
                Err(OkfError::UnlockRequired { .. })
            ),
            "an admin key with the flag is not exempt"
        );
        assert!(
            matches!(
                runner(Access::Write, true).admission(&main, Access::Write, now),
                Err(OkfError::UnlockRequired { .. })
            ),
            "the exemption covers reads and reports only"
        );
        assert!(
            matches!(
                Principal {
                    maintenance_report: true,
                    ..as_user(Some(member()), Access::Read, false)
                }
                .admission(&opted_out, Access::Read, now),
                Err(OkfError::MaintenanceOff { .. })
            ),
            "off keeps the runner out on any server, whatever its grants"
        );
    }

    #[test]
    fn require_contents_checks_level_and_suspension_before_the_unlock() {
        let now = Utc::now();
        let reader = as_user(Some(gates_of("other")), Access::Read, true);
        let main = row("main", 1);
        let suspended = suspended_row("main", 1);

        assert!(matches!(
            reader.require_contents(Access::Write, &main, now),
            Err(OkfError::Forbidden { .. })
        ));
        assert!(matches!(
            reader.require_contents(Access::Read, &row("elsewhere", 2), now),
            Err(OkfError::CatalogNotSelected { .. })
        ));
        assert!(matches!(
            reader.require_contents(Access::Read, &suspended, now),
            Err(OkfError::Suspended { .. })
        ));
        assert!(matches!(
            reader.require_contents(Access::Read, &main, now),
            Err(OkfError::UnlockRequired { .. })
        ));
        assert!(matches!(
            reader.check_open(&suspended, Access::Read, now),
            Err(OkfError::Suspended { .. })
        ));
        assert!(
            matches!(
                reader.check_open(&main, Access::Write, now),
                Err(OkfError::UnlockRequired { .. })
            ),
            "the selection checks no level"
        );
    }

    #[test]
    fn an_access_record_takes_its_ids_from_the_facts() {
        let principal = Principal {
            user_id: Some("u".to_string()),
            facts: Arc::new(AccessFacts {
                user: Some(Arc::new(UserGates {
                    person: Some(11),
                    is_admin: true,
                    ..UserGates::default()
                })),
                credential: Some(22),
                ..AccessFacts::default()
            }),
            ..key_on(&["main"], Access::Read)
        };
        let requester = Requester {
            ip: None,
            source: crate::store::audit::Source::Api,
        };
        let scope = CatalogScope::of(&row("main", 1));

        let record = principal.access_record(&requester, "main", scope, Utc::now());

        assert_eq!(
            (
                record.actor.credential,
                record.actor.person,
                record.actor.operator
            ),
            (Some(22), Some(11), true)
        );
        assert_eq!(
            record.scope,
            CatalogScope {
                id: 1,
                team: Some(41)
            }
        );
    }
}
