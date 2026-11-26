//! What one tool call acts as and on: the principal, the connection's
//! catalogs, the catalogs it has resolved with the snapshot of each it
//! reads (M18 spec Decision 3), and the changeset a handle names
//! ([`Pinned`]).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use axum::http::request::Parts;
use chrono::{DateTime, Utc};

use crate::app::App;
use crate::auth::{Admission, CatalogScope, Principal, Reach};
use crate::cache::directory::Lookup;
use crate::catalog::{CatalogState, Loaded};
use crate::error::OkfError;
use crate::http::requester::Requester;
use crate::labels::directory::{Labels, Wanted};
use crate::overlay::View;
use crate::selection::{connection_catalogs, in_scope, resolve, Selection, Sources};
use crate::store::audit::{AccessKind, AccessRecord, AuditActor, Source};
use crate::store::catalogs::{self, CatalogRow};
use crate::store::changesets::{self, ChangesetRow, ChangesetState};
use crate::store::keys::Access;
use crate::stores::{StoreName, READ_CONCURRENCY};

/// Is this [`ToolContext::view`] error a changeset's overlay failing to
/// apply, rather than the database failing?
///
/// Only [`crate::overlay::build_overlay`] reports one, always as an
/// [`OkfError::InvalidPath`] naming the changeset: a clash with the
/// published files, or a pending file the snapshot cannot read. The match is
/// positive, so any other error a later change lets `view` return is not
/// mistaken for one. A read across several catalogs leaves such a catalog
/// out and carries on; any other error (a database error, `Busy`) fails the
/// call, so the agent is told to retry instead of seeing fewer results.
///
/// # Arguments
/// * `error` - the error `view` returned.
///
/// # Returns
/// `true` for an overlay that does not apply.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
    )
)]
pub fn is_broken_overlay(error: &OkfError) -> bool {
    matches!(error, OkfError::InvalidPath { .. })
}

/// A changeset a call acts on, and its catalog (M8 Decision 5: the catalog
/// comes from the changeset).
#[derive(Debug, Clone)]
pub struct Pinned {
    /// The changeset's catalog.
    pub state: Arc<CatalogState>,
    /// The changeset, as read when the call resolved it (not locked).
    pub row: ChangesetRow,
}

/// A catalog one call has resolved: its state, and the snapshot the call
/// reads (taken once, so one call sees one snapshot per catalog even when
/// the state is refreshed in place or evicted meanwhile).
#[derive(Debug, Clone)]
struct Held {
    state: Arc<CatalogState>,
    snapshot: Arc<Loaded>,
}

/// Locks one of the context's maps, ignoring poisoning: they hold handles
/// only, so a panic elsewhere cannot leave one half-updated.
#[tracing::instrument(level = "trace", skip_all)]
fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What this call has found unreachable: whole stores (so the next catalog
/// on one is not probed again, one connect timeout per store and not per
/// catalog), and the catalogs resolved on them, by name.
#[derive(Debug, Default)]
struct Down {
    stores: HashSet<StoreName>,
    catalogs: HashMap<String, StoreName>,
}

/// The catalogs a first-match read looks through, in order (design Decision
/// 13, plan P3): the states up to the first catalog whose store cannot be
/// reached, and that failure. A tool answers from `states` if one holds
/// what it was asked for, and otherwise returns `blocked` rather than
/// "not found": the catalog behind the failure might have held the answer,
/// and the catalogs after it are never consulted.
#[derive(Debug)]
pub struct ReadSet {
    /// The catalogs before the first unreachable one, in selection order.
    pub states: Vec<Arc<CatalogState>>,
    /// The [`OkfError::StoreUnavailable`] that ended the list, if one did.
    pub blocked: Option<OkfError>,
}

// `Deref` lets a `ReadSet` be indexed and iterated as its `Vec` of states
// (like a Python class delegating to a list), so a caller that only reads
// the states needs no `.states`.
impl std::ops::Deref for ReadSet {
    type Target = Vec<Arc<CatalogState>>;

    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "one-line accessor")
    )]
    fn deref(&self) -> &Self::Target {
        &self.states
    }
}

/// One tool call's context.
// `Arc<T>` is a shared, reference-counted pointer (like holding a Python
// reference): cloning one bumps a counter, it never copies `T`.
pub struct ToolContext {
    app: Arc<App>,
    principal: Arc<Principal>,
    connection: Option<Vec<String>>,
    // The directory rows `select` read for its candidates, by name, so the
    // states it then resolves need no second read. `std` mutexes, as for
    // `noted`: held only for a map operation, never across an `.await`.
    rows: Mutex<HashMap<String, Arc<CatalogRow>>>,
    // Each catalog this call has resolved, by name, with the snapshot it
    // reads (M18 spec Decision 3). Bounded by the catalogs one call names.
    held: Mutex<HashMap<String, Held>>,
    // The request's address and source, for the audit log.
    requester: Requester,
    // The accesses this call has noted for the audit log, so each catalog
    // and kind counts once per call (plan Decision 5). A `std` mutex: held
    // only to insert, never across an `.await`.
    noted: Mutex<HashSet<(String, AccessKind)>>,
    // The stores and catalogs this call found unreachable. A `std` mutex,
    // as above.
    down: Mutex<Down>,
}

impl ToolContext {
    /// A context for `principal` on a connection naming `connection`.
    ///
    /// # Arguments
    /// * `app` - the shared state.
    /// * `principal` - the calling key.
    /// * `connection` - the connection's catalogs (`X-OKF-Catalogs` or
    ///   `?catalogs=`), if it names any.
    ///
    /// # Returns
    /// The context, holding no catalog yet (each resolves on first use),
    /// for a request with no known address from an API credential;
    /// [`ToolContext::with_requester`] sets the request's own.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn new(app: Arc<App>, principal: Arc<Principal>, connection: Option<Vec<String>>) -> Self {
        Self {
            app,
            principal,
            connection,
            rows: Mutex::default(),
            held: Mutex::default(),
            requester: Requester {
                ip: None,
                source: Source::Api,
            },
            noted: Mutex::default(),
            down: Mutex::default(),
        }
    }

    /// A new context for the same principal, connection and requester: the
    /// next call on this connection, holding no catalog (tests that make
    /// several calls with one context use it where a later call must not
    /// see the snapshots an earlier one pinned).
    #[cfg(test)]
    pub(crate) fn fresh(&self) -> Self {
        Self::new(
            Arc::clone(&self.app),
            Arc::clone(&self.principal),
            self.connection.clone(),
        )
        .with_requester(self.requester)
    }

    /// The same context, for a request with this address and source.
    ///
    /// # Arguments
    /// * `requester` - the request's address and source.
    ///
    /// # Returns
    /// The context.
    // `mut self` takes the context by value and changes it before handing it
    // back, so calls chain: `ToolContext::new(..).with_requester(..)`.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn with_requester(mut self, requester: Requester) -> Self {
        self.requester = requester;
        self
    }

    /// Who this call is, for the audit log.
    ///
    /// # Returns
    /// The principal with the request's address and source.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn audit_actor(&self) -> AuditActor {
        AuditActor::of(&self.principal, &self.requester)
    }

    /// The context of the HTTP request rmcp handed the tool.
    ///
    /// # Arguments
    /// * `app` - the shared state.
    /// * `parts` - the request: its extensions hold the principal the auth
    ///   layer inserted, its header or query the connection's catalogs.
    ///
    /// # Returns
    /// The context.
    ///
    /// # Errors
    /// [`OkfError::Forbidden`] if the request carries no principal, which
    /// only a router without the auth layer would allow.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn from_parts(app: &Arc<App>, parts: &Parts) -> Result<Self, OkfError> {
        // Extensions are a map keyed by type: `get::<Arc<Principal>>()`
        // finds the value the auth middleware inserted, if any.
        let principal = parts
            .extensions
            .get::<Arc<Principal>>()
            .cloned()
            .ok_or_else(|| OkfError::Forbidden {
                reason: "the request carries no API key".to_string(),
            })?;
        Ok(
            Self::new(Arc::clone(app), principal, connection_catalogs(parts))
                .with_requester(Requester::of(parts)),
        )
    }

    /// The shared state.
    ///
    /// # Returns
    /// The app this call runs in.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn app(&self) -> &App {
        &self.app
    }

    /// The calling key.
    ///
    /// # Returns
    /// Its principal.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    /// The labels of what a tool output will show.
    ///
    /// # Arguments
    /// * `wanted` - the slugs.
    ///
    /// # Returns
    /// Their labels.
    ///
    /// # Errors
    /// [`OkfError::Database`].
    #[tracing::instrument(level = "trace", skip_all)]
    pub async fn labels_for(&self, wanted: &Wanted) -> Result<Labels, OkfError> {
        self.app.labels().labels_for(wanted).await
    }

    /// A catalog's label for a tool output.
    ///
    /// # Arguments
    /// * `slug` - the catalog's id.
    ///
    /// # Returns
    /// Its label, or the id when no such catalog exists.
    ///
    /// # Errors
    /// [`OkfError::Database`].
    #[tracing::instrument(level = "trace", skip_all)]
    pub async fn catalog_label(&self, slug: &str) -> Result<String, OkfError> {
        let mut wanted = Wanted::default();
        wanted.catalog(slug);
        let labels = self.labels_for(&wanted).await?;
        Ok(labels.catalog(slug).unwrap_or(slug).to_string())
    }

    /// The connection's catalog list (`X-OKF-Catalogs` or `?catalogs=`).
    ///
    /// # Returns
    /// The names as the connection gave them, or `None` without a list.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn connection(&self) -> Option<&[String]> {
        self.connection.as_deref()
    }

    /// Does this call see the catalog `row` describes: the key reaches it
    /// and the connection's list, if it has one, names it? What a tool that
    /// lists catalogs (or what is in them) filters by, so an omitted
    /// catalog never shows. By row, so a console session's ceiling decides
    /// it too ([`Principal::reaches_row`]).
    ///
    /// # Arguments
    /// * `row` - the catalog's directory row.
    ///
    /// # Returns
    /// `true` if the call may see the catalog.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn offers(&self, row: &CatalogRow) -> bool {
        self.principal.reaches_row(row) && in_scope(self.connection.as_deref(), &row.name)
    }

    /// Checks the connection's list whole, as [`resolve`] does: for the
    /// paths that reach a catalog without selecting one (a changeset
    /// handle, `list_changesets` without `catalog`), so a list naming a bad
    /// catalog fails them too. Without a list there is nothing to check.
    ///
    /// # Errors
    /// [`OkfError::CatalogNotSelected`] naming the first listed catalog that
    /// does not exist or that the key does not reach; then
    /// [`OkfError::Suspended`] when every listed catalog is suspended (and
    /// the key is not admin), as [`resolve`] refuses such a list.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn check_connection(&self) -> Result<(), OkfError> {
        match self.connection.as_deref() {
            // With a list, `select(None)` resolves to exactly that list.
            // `map(drop)` discards the selection, keeping only `Ok(())`.
            Some(list) if !list.is_empty() => self.select(None).await.map(drop),
            _ => Ok(()),
        }
    }

    /// Resolves the call's catalogs (design "Catalog selection"). Admits
    /// nothing: a tool admits each catalog right before it reads its
    /// contents (`ToolContext::view`, `ToolContext::admit`), so only the
    /// catalogs actually read are recorded (plan Decision 23).
    ///
    /// # Arguments
    /// * `argument` - the call's `catalogs` (or `catalog` as one name).
    ///
    /// # Returns
    /// The selection, every name a catalog in the directory.
    ///
    /// # Errors
    /// As [`resolve`]; [`OkfError::Database`] if the directory cannot be
    /// read.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn select(&self, argument: Option<&[String]>) -> Result<Selection, OkfError> {
        let sources = Sources {
            argument,
            connection: self.connection.as_deref(),
            server_default: self.app.config().default_catalog.as_deref(),
            now: self.app.clock().now(),
        };
        let rows = self.candidate_rows(&sources).await?;
        // Kept, so the states this call resolves next need no second
        // lookup; `resolve` checks names and the closed checks against the
        // rows read now.
        locked(&self.rows).extend(
            rows.iter()
                .map(|(name, row)| (name.clone(), Arc::clone(row))),
        );
        // `as_ref` turns the `&Arc<CatalogRow>` into `&CatalogRow`.
        resolve(&self.principal, sources, |name| {
            rows.get(name).map(AsRef::as_ref)
        })
    }

    /// The directory rows of every catalog a selection from `sources` can
    /// name and the key reaches: a handful, each from the directory's cache
    /// when it is there.
    ///
    /// # Errors
    /// [`OkfError::Database`] when a row's read fails.
    #[tracing::instrument(level = "trace", skip_all)]
    async fn candidate_rows(
        &self,
        sources: &Sources<'_>,
    ) -> Result<HashMap<String, Arc<CatalogRow>>, OkfError> {
        // `chain` joins the sources into one sequence of names, like
        // `itertools.chain` in Python.
        // Collected before the first `.await`: a lazy iterator held across
        // one would have to be `Send` for every lifetime, which this one is
        // not.
        let names: Vec<&str> = sources
            .argument
            .into_iter()
            .chain(sources.connection)
            .flatten()
            .map(|name| name.as_str())
            .chain(self.principal.default_catalog.as_deref())
            .chain(sources.server_default)
            .collect();
        let mut rows = HashMap::new();
        for name in &names {
            // A by-name reach that leaves the name out needs no row; any
            // other is decided by the row (a session's ceiling).
            if rows.contains_key(*name) || self.principal.excludes(name) {
                continue;
            }
            let known_down = locked(&self.down).stores.clone();
            let found = self
                .app
                .directory()
                .resolve_or_unavailable(name, &known_down)
                .await?;
            // A catalog on an unreachable store is kept like any other: the
            // key's reach is decided by the row (from the account
            // database), so only a catalog the key reaches is ever named
            // as unavailable.
            let (row, unreachable) = match found {
                Some(Lookup::Available(row)) => (row, false),
                Some(Lookup::Unavailable(row)) => (row, true),
                None => continue,
            };
            if self.principal.reaches_row(&row) {
                if unreachable {
                    self.mark_down(&row);
                }
                rows.insert(name.to_string(), row);
            }
        }
        // With no source naming a catalog, `resolve` falls back to the only
        // catalog the key is granted (issue #663), which needs to know which
        // of the granted names exist. One query for the grant set (the names
        // the key was given, not a scan of the directory), and only then.
        if names.is_empty() {
            if let Reach::Catalogs(levels) = &self.principal.reach {
                let granted: Vec<String> = levels.keys().cloned().collect();
                let (found, down) = self.app.directory().resolve_many(&granted).await?;
                for row in found {
                    if down.contains(&row.store) {
                        self.mark_down(&row);
                    }
                    rows.insert(row.name.clone(), row);
                }
            }
        }
        Ok(rows)
    }

    /// Notes that `row`'s store could not be reached in this call, so the
    /// catalog is unavailable and no other catalog on the store is probed.
    /// A caller that read a directory page and found stores down tells the
    /// call, so the selection and the state loads that follow do not spend a
    /// timeout on the same store again.
    ///
    /// # Arguments
    /// * `row` - a catalog on the unreachable store.
    #[tracing::instrument(level = "trace", skip_all)]
    pub(crate) fn mark_down(&self, row: &CatalogRow) {
        let mut down = locked(&self.down);
        down.stores.insert(row.store.clone());
        down.catalogs.insert(row.name.clone(), row.store.clone());
    }

    /// The unreachable store `name` was found on in this call, if any.
    ///
    /// # Arguments
    /// * `name` - a catalog name.
    ///
    /// # Returns
    /// The store, or `None` for a catalog not found unreachable.
    #[tracing::instrument(level = "trace", skip_all)]
    fn down_store_of(&self, name: &str) -> Option<StoreName> {
        locked(&self.down).catalogs.get(name).cloned()
    }

    /// The directory row of `state`'s catalog, as it is now: what every
    /// check against that catalog's access facts reads.
    ///
    /// # Arguments
    /// * `state` - a catalog from this call's map.
    ///
    /// # Returns
    /// The row of the same incarnation.
    ///
    /// # Errors
    /// [`OkfError::CatalogNotSelected`] when the catalog no longer exists
    /// (deleted since the map was taken); [`OkfError::Busy`] when the name
    /// is now another catalog (deleted and created again since): nothing
    /// is decided for one incarnation with the facts of another, and a
    /// retry sees the map catch up; [`OkfError::Database`].
    #[tracing::instrument(level = "trace", skip_all)]
    pub async fn row(&self, state: &CatalogState) -> Result<Arc<CatalogRow>, OkfError> {
        self.row_by_id(state.name(), state.id()).await
    }

    /// [`ToolContext::row`] for a catalog known by its name and
    /// `catalogs.id` only.
    ///
    /// # Errors
    /// As [`ToolContext::row`].
    #[tracing::instrument(level = "trace", skip_all)]
    pub(crate) async fn row_by_id(
        &self,
        catalog: &str,
        catalog_id: i64,
    ) -> Result<Arc<CatalogRow>, OkfError> {
        let Some(row) = self.app.directory().resolve(catalog).await? else {
            return Err(OkfError::CatalogNotSelected {
                catalog: catalog.to_string(),
            });
        };
        if row.id != catalog_id {
            return Err(OkfError::Busy {
                reason: format!("catalog {catalog:?} is being reloaded"),
            });
        }
        Ok(row)
    }

    /// [`Principal::require`] on `state`'s catalog, with its row as it is
    /// now ([`ToolContext::row`]).
    ///
    /// # Arguments
    /// * `access` - the level the operation needs.
    /// * `state` - a catalog from this call's map.
    ///
    /// # Errors
    /// As [`ToolContext::row`], then as [`Principal::require`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn require(&self, access: Access, state: &CatalogState) -> Result<(), OkfError> {
        // Reach and level first, so a key that does not reach the catalog
        // learns nothing from the row's lookup.
        self.principal.precheck_level(access, state.name())?;
        self.principal.require(access, &*self.row(state).await?)
    }

    /// [`Principal::require_contents`] on `state`'s catalog, with its row as
    /// it is now ([`ToolContext::row`]).
    ///
    /// # Arguments
    /// * `access` - the level the operation needs.
    /// * `state` - a catalog from this call's map.
    ///
    /// # Returns
    /// The admission.
    ///
    /// # Errors
    /// As [`ToolContext::row`], then as [`Principal::require_contents`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn require_contents(
        &self,
        access: Access,
        state: &CatalogState,
    ) -> Result<Admission, OkfError> {
        self.principal.precheck_level(access, state.name())?;
        let row = self.row(state).await?;
        self.principal
            .require_contents(access, &row, self.app.clock().now())
    }

    /// [`Principal::require_maintenance_report`] on `state`'s catalog, with
    /// its row as it is now ([`ToolContext::row`]).
    ///
    /// # Arguments
    /// * `state` - a catalog from this call's map.
    ///
    /// # Errors
    /// As [`ToolContext::row`], then as
    /// [`Principal::require_maintenance_report`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn require_maintenance_report(&self, state: &CatalogState) -> Result<(), OkfError> {
        self.principal.precheck_level(Access::Read, state.name())?;
        self.principal
            .require_maintenance_report(&*self.row(state).await?)
    }

    /// The states of the selected catalogs, in order.
    ///
    /// # Arguments
    /// * `argument` - as [`ToolContext::select`].
    ///
    /// # Returns
    /// One state per selected catalog; never empty.
    ///
    /// # Errors
    /// As [`ToolContext::select`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn states(
        &self,
        argument: Option<&[String]>,
    ) -> Result<Vec<Arc<CatalogState>>, OkfError> {
        let selection = self.select(argument).await?;
        let mut states = Vec::with_capacity(selection.catalogs().len());
        for name in selection.catalogs() {
            states.push(self.state(name).await?);
        }
        Ok(states)
    }

    /// [`ToolContext::states`] for a read that merges what the catalogs
    /// hold (a search): the catalogs whose store cannot be reached do not
    /// fail the call. Their states load together, at most
    /// [`READ_CONCURRENCY`] at a time.
    ///
    /// # Arguments
    /// * `argument` - as [`ToolContext::select`].
    ///
    /// # Returns
    /// The states of the catalogs that could be read, in selection order
    /// (possibly none), and the ids of the selected catalogs that could not,
    /// sorted and each once. Only selected catalogs, which the key reaches
    /// and the connection offers, are ever named.
    ///
    /// # Errors
    /// As [`ToolContext::select`]; any other error from loading a state.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn states_or_unavailable(
        &self,
        argument: Option<&[String]>,
    ) -> Result<(Vec<Arc<CatalogState>>, Vec<String>), OkfError> {
        let selection = self.select(argument).await?;
        // `join_all` runs a chunk's loads together; chunks of
        // `READ_CONCURRENCY` bound how many run at once, in selection order.
        let mut loads: Vec<Result<Arc<CatalogState>, OkfError>> =
            Vec::with_capacity(selection.catalogs().len());
        for chunk in selection.catalogs().chunks(READ_CONCURRENCY) {
            loads.extend(
                futures_util::future::join_all(chunk.iter().map(|name| self.state(name))).await,
            );
        }
        let mut states = Vec::with_capacity(loads.len());
        let mut unavailable = std::collections::BTreeSet::new();
        for (name, load) in selection.catalogs().iter().zip(loads) {
            match load {
                Ok(state) => states.push(state),
                Err(OkfError::StoreUnavailable { .. }) => {
                    unavailable.insert(name.clone());
                }
                Err(error) => return Err(error),
            }
        }
        Ok((states, unavailable.into_iter().collect()))
    }

    /// The read set of a tool with a single `catalog` argument: that catalog,
    /// else the selected catalogs (Decision 1), up to the first whose store
    /// cannot be reached ([`ReadSet`]).
    ///
    /// # Arguments
    /// * `catalog` - the call's `catalog` argument.
    ///
    /// # Returns
    /// The states, in order, and the failure that ended them, if one did.
    ///
    /// # Errors
    /// As [`ToolContext::select`], and any error from loading a state but
    /// [`OkfError::StoreUnavailable`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn read_set(&self, catalog: Option<&str>) -> Result<ReadSet, OkfError> {
        // `select` takes a slice of `String`s, so the name is copied into one
        // `String` (a small allocation); `std::slice::from_ref` then views it
        // as a one-element slice without also allocating a `Vec`.
        let argument = catalog.map(str::to_string);
        let selection = self
            .select(argument.as_ref().map(std::slice::from_ref))
            .await?;
        let mut states = Vec::with_capacity(selection.catalogs().len());
        for name in selection.catalogs() {
            match self.state(name).await {
                Ok(state) => states.push(state),
                // P3: stop here. A later catalog must not answer in place of
                // this one, which might have held the answer.
                Err(error @ OkfError::StoreUnavailable { .. }) => {
                    return Ok(ReadSet {
                        states,
                        blocked: Some(error),
                    })
                }
                Err(error) => return Err(error),
            }
        }
        Ok(ReadSet {
            states,
            blocked: None,
        })
    }

    /// The one catalog a call acts on: `catalog`, else the first selected.
    /// Admits nothing, like [`ToolContext::select`]: a tool that reads the
    /// catalog's contents calls `ToolContext::admit` after its own access
    /// checks, right before the read, and one that only mutates or
    /// authorizes against it (a maintenance claim, opening a changeset)
    /// records nothing (plan Decision 23).
    ///
    /// # Arguments
    /// * `catalog` - the call's `catalog` argument.
    ///
    /// # Returns
    /// That catalog's state.
    ///
    /// # Errors
    /// As [`ToolContext::select`]; then as `ToolContext::check_contents`
    /// for the state's incarnation.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn one(&self, catalog: Option<&str>) -> Result<Arc<CatalogState>, OkfError> {
        let argument = catalog.map(str::to_string);
        let selection = self
            .select(argument.as_ref().map(std::slice::from_ref))
            .await?;
        // `write_target` is the first selected catalog.
        let state = self.state(selection.write_target()).await?;
        // The selection checked the name; every caller acts on this state's
        // `catalogs.id`, so check the incarnation too (records nothing):
        // an unlock of a deleted catalog must not reach its replacement.
        self.check_contents(&state, Access::Read).await?;
        Ok(state)
    }

    /// Notes one access to `state`'s contents for the audit log: at most
    /// once per call, catalog and kind (one request, one count), and only
    /// for an admission that is recorded. The admission and the scope come
    /// from the catalog's directory row as it is now, which must describe
    /// the incarnation this call reads (the state's `catalogs.id`). The
    /// identities and scope are fixed now ([`Principal::access_record`]);
    /// the write runs in the background ([`App::note_access`]), off the
    /// request path.
    ///
    /// # Arguments
    /// * `state` - a catalog this call reaches at the level `kind` needs,
    ///   that this call resolved.
    /// * `kind` - what the call does with its contents.
    ///
    /// # Errors
    /// As [`ToolContext::admission_to`]: the refusal of the row that
    /// describes the incarnation read (an unlock of a deleted catalog does
    /// not open its replacement), or [`OkfError::Busy`] when the name is
    /// now another catalog than the one this call's map holds: its access
    /// is neither recorded under the wrong catalog or team nor left out. A
    /// retry sees the map catch up.
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) async fn admit(
        &self,
        state: &CatalogState,
        kind: AccessKind,
    ) -> Result<(), OkfError> {
        if let Some(admitted) = self.admission_of(state, kind).await? {
            self.note(admitted, kind);
        }
        Ok(())
    }

    /// Admits every state of a read that reads them all before answering
    /// (a search), all or nothing: each is checked first, and nothing is
    /// noted unless every one passes, so a call refused with
    /// [`OkfError::Busy`] queues no read.
    ///
    /// # Arguments
    /// * `states` - the catalogs the call reads.
    /// * `kind` - what the call does with their contents.
    ///
    /// # Errors
    /// As `ToolContext::admit`, for any of them.
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) async fn admit_all(
        &self,
        states: &[Arc<CatalogState>],
        kind: AccessKind,
    ) -> Result<(), OkfError> {
        let mut admitted = Vec::with_capacity(states.len());
        for state in states {
            admitted.push(self.admission_of(state, kind).await?);
        }
        // `flatten` skips the `None`s (reads that record nothing).
        for admitted in admitted.into_iter().flatten() {
            self.note(admitted, kind);
        }
        Ok(())
    }

    /// What `ToolContext::admit` would note for `state`, with no side
    /// effect: the record and its admission, or `None` for a read that
    /// records nothing.
    ///
    /// # Errors
    /// As `ToolContext::admit`.
    #[tracing::instrument(level = "trace", skip_all)]
    async fn admission_of(
        &self,
        state: &CatalogState,
        kind: AccessKind,
    ) -> Result<Option<(AccessRecord, Admission)>, OkfError> {
        let access = match kind {
            AccessKind::Write => Access::Write,
            AccessKind::Read | AccessKind::Export => Access::Read,
        };
        let bound = self.admission_to(state, access).await?;
        if !bound.admission.is_recorded() {
            return Ok(None);
        }
        let record =
            self.principal
                .access_record(&self.requester, state.name(), bound.scope, bound.now);
        Ok(Some((record, bound.admission)))
    }

    /// Checks that this call may reach `state`'s contents for `access`,
    /// decided by the directory row of that very incarnation
    /// ([`ToolContext::admission_to`]): for a path that reads or writes by
    /// `state.id()` without admitting (a changeset list, opening a
    /// changeset). Name-based checks (the selection, `require_contents`)
    /// alone would let an unlock of a deleted catalog reach its
    /// replacement.
    ///
    /// # Arguments
    /// * `state` - a catalog this call resolved.
    /// * `access` - what the call does with it.
    ///
    /// # Errors
    /// As [`ToolContext::admission_to`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) async fn check_contents(
        &self,
        state: &CatalogState,
        access: Access,
    ) -> Result<(), OkfError> {
        self.admission_to(state, access).await.map(drop)
    }

    /// [`ToolContext::check_contents`] for a catalog a query named by its
    /// name and `catalogs.id` (a changeset row), with no state loaded: the
    /// same check, bound to that incarnation.
    ///
    /// # Arguments
    /// * `catalog` - the catalog's name.
    /// * `catalog_id` - the `catalogs.id` the row belongs to.
    /// * `access` - what the call does with it.
    ///
    /// # Errors
    /// As [`ToolContext::admission_to`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) async fn check_contents_by_id(
        &self,
        catalog: &str,
        catalog_id: i64,
        access: Access,
    ) -> Result<(), OkfError> {
        self.admission_by_id(catalog, catalog_id, access)
            .await
            .map(drop)
    }

    /// The admission of this call's principal to `state` for `access`,
    /// decided by the catalog's directory row as it is now, which must be
    /// the incarnation this call reads (the state's `catalogs.id`;
    /// [`ToolContext::row`]). The principal's facts were assembled for this
    /// request and match catalogs by id, so a refusal from them stands and
    /// is returned, never turned into an unrecorded success.
    ///
    /// # Arguments
    /// * `state` - a catalog this call resolved.
    /// * `access` - what the call does with it.
    ///
    /// # Returns
    /// The admission with the scope it was decided for.
    ///
    /// # Errors
    /// As [`ToolContext::row`]: [`OkfError::Busy`] when the name is now
    /// another catalog, [`OkfError::CatalogNotSelected`] when it is gone;
    /// then as [`Principal::admission`].
    #[tracing::instrument(level = "trace", skip_all)]
    async fn admission_to(
        &self,
        state: &CatalogState,
        access: Access,
    ) -> Result<BoundAdmission, OkfError> {
        self.admission_by_id(state.name(), state.id(), access).await
    }

    /// [`ToolContext::admission_to`] for a catalog known by its name and
    /// `catalogs.id` only (a row a query returned), with no state at hand.
    ///
    /// # Errors
    /// As [`ToolContext::admission_to`].
    #[tracing::instrument(level = "trace", skip_all)]
    async fn admission_by_id(
        &self,
        catalog: &str,
        catalog_id: i64,
        access: Access,
    ) -> Result<BoundAdmission, OkfError> {
        let row = self.row_by_id(catalog, catalog_id).await?;
        let now = self.app.clock().now();
        // `?`: the row describes what is read, so its refusal stands.
        let admission = self.principal.admission(&row, access, now)?;
        Ok(BoundAdmission {
            admission,
            scope: CatalogScope::of(&row),
            now,
        })
    }

    /// Notes an admitted access, once per call, catalog and kind.
    #[tracing::instrument(level = "trace", skip_all)]
    fn note(&self, (record, admission): (AccessRecord, Admission), kind: AccessKind) {
        let fresh = self
            .noted
            .lock()
            .expect("noted lock poisoned")
            .insert((record.catalog.clone(), kind));
        if fresh {
            self.app.note_access(record, kind, admission);
        }
    }

    /// The call's changeset, if it named one: what a read tool passes to
    /// [`ToolContext::view`] (M8 Decision 6). Reads need only `read` access,
    /// so the key that owns the changeset, or an admin, may read through it.
    ///
    /// # Arguments
    /// * `handle` - the call's optional `changeset` argument.
    ///
    /// # Returns
    /// The pinned changeset, or `None` without a handle.
    ///
    /// # Errors
    /// As [`ToolContext::changeset`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn pin(&self, handle: Option<&str>) -> Result<Option<Pinned>, OkfError> {
        match handle {
            None => Ok(None),
            // `.map(Some)` wraps the `Ok` value: `Ok(pinned)` becomes `Ok(Some(pinned))`.
            // Resolved without admitting: the read tool's `view` admits the
            // pinned catalog when, and only if, it reads it.
            Some(handle) => self.resolve_changeset(handle, Access::Read).await.map(Some),
        }
    }

    /// The snapshot of `state` this call reads: the one taken when the call
    /// first resolved the catalog, so every read of one call sees one
    /// snapshot even if the catalog is refreshed in place or evicted
    /// meanwhile (M18 spec Decision 3). A state this call did not resolve
    /// (another incarnation under the name) is read as it is now.
    ///
    /// # Arguments
    /// * `state` - a catalog this call resolved.
    ///
    /// # Returns
    /// The snapshot (a shared handle, cheap to clone).
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn snapshot(&self, state: &CatalogState) -> Arc<Loaded> {
        locked(&self.held)
            .get(state.name())
            .filter(|held| held.state.id() == state.id())
            .map_or_else(|| state.current(), |held| Arc::clone(&held.snapshot))
    }

    /// What this call reads of `state`: the pinned changeset's overlay if
    /// it is in this catalog, else the published state (M8 Decision 6).
    /// Without a pinned changeset a read is always published, whatever the
    /// key's open changesets.
    ///
    /// The pinned row was read unlocked when the handle resolved. The overlay
    /// cache matches that row's `version` and the catalog's generation, and
    /// every write, publish and discard forgets the cached overlay, so a write
    /// that commits meanwhile is seen by the next call, never cached stale.
    ///
    /// # Arguments
    /// * `state` - a catalog this call selected.
    /// * `pinned` - the call's changeset, from [`ToolContext::pin`].
    ///
    /// # Returns
    /// The view.
    ///
    /// Admits the read first (`ToolContext::admit`): a view is taken only
    /// to read it.
    ///
    /// # Errors
    /// [`OkfError::Busy`] as `ToolContext::admit`; an overlay build error
    /// ([`crate::overlay::build_overlay`]); [`OkfError::Database`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn view(
        &self,
        state: &CatalogState,
        pinned: Option<&Pinned>,
    ) -> Result<View, OkfError> {
        self.admit(state, AccessKind::Read).await?;
        // `filter` keeps the pinned changeset only if it is in this catalog.
        match pinned.filter(|pinned| pinned.state.id() == state.id()) {
            None => Ok(View::Published(self.snapshot(state))),
            Some(pinned) => {
                // An overlay is built on the changeset's latest base, which
                // may refresh the state in place: then the cache re-weighs it.
                let before = state.current().generation();
                let overlay = self
                    .app
                    .overlays()
                    .get(
                        &self.app.store_db(state.store()).await?,
                        self.app.rebuild_locks(),
                        state,
                        &pinned.row,
                    )
                    .await;
                self.app
                    .catalog_cache()
                    .refreshed_if_moved(state, before)
                    .await;
                Ok(View::Pending(overlay?))
            }
        }
    }

    /// The one catalog a report runs on, given its changeset: `catalog`, else
    /// the pinned changeset's catalog, else the first selected. A `pg`
    /// handle on a `work,pg` connection reports on `pg`.
    ///
    /// The pinned changeset's catalog stands in for a `catalog` argument.
    /// [`ToolContext::changeset`] has already checked that the key reaches
    /// it and that the connection's list (if any) names it, so `one` cannot
    /// refuse it: a `pg` handle on a `work`-only connection never resolves.
    ///
    /// # Arguments
    /// * `catalog` - the call's `catalog` argument.
    /// * `pinned` - the call's changeset, from [`ToolContext::pin`].
    ///
    /// # Returns
    /// That catalog's state.
    ///
    /// # Errors
    /// As [`ToolContext::one`] for an explicit `catalog` (or, without a
    /// changeset, the connection's selection); as [`require_selected`] for an
    /// explicit `catalog` that is not the changeset's.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn report_catalog(
        &self,
        catalog: Option<&str>,
        pinned: Option<&Pinned>,
    ) -> Result<Arc<CatalogState>, OkfError> {
        // `or_else` runs the closure only when `catalog` is `None`.
        let name = catalog.or_else(|| pinned.map(|pinned| pinned.state.name()));
        let state = self.one(name).await?;
        require_selected(pinned, std::slice::from_ref(&state))?;
        Ok(state)
    }

    /// Resolves a changeset handle for this call (M8 Decisions 1 and 3).
    ///
    /// # Arguments
    /// * `handle` - the call's `changeset` argument.
    /// * `access` - what the call needs on the changeset's catalog.
    ///
    /// # Returns
    /// The open changeset and its catalog. The row is read without a lock:
    /// a write locks it again (`changesets::lock`) before trusting its state.
    ///
    /// # Errors
    /// - [`OkfError::CatalogNotSelected`] for a connection list naming a bad
    ///   catalog ([`ToolContext::check_connection`]), before the handle is
    ///   looked up;
    /// - [`OkfError::UnknownChangeset`] for a handle that does not exist, is
    ///   in a catalog this key does not reach (or that is not loaded, or
    ///   that the connection's list leaves out), or
    ///   belongs to another key while this one is not `admin`;
    /// - [`OkfError::Forbidden`] below `access`;
    /// - [`OkfError::UnlockRequired`] or [`OkfError::MaintenanceOff`] as
    ///   [`Principal::admission`] for `access`;
    /// - [`OkfError::Refused`] for a changeset that was published or
    ///   discarded (a discarded one with a hint that an empty changeset is
    ///   discarded after 24 hours);
    /// - [`OkfError::Busy`] as `ToolContext::admit`;
    /// - [`OkfError::Database`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn changeset(&self, handle: &str, access: Access) -> Result<Pinned, OkfError> {
        let pinned = self.resolve_changeset(handle, access).await?;
        // The changeset's own catalog only, never the connection's others
        // (plan Decision 23). `if` is an expression, like Python's
        // conditional expression.
        let kind = if access == Access::Write {
            AccessKind::Write
        } else {
            AccessKind::Read
        };
        self.admit(&pinned.state, kind).await?;
        Ok(pinned)
    }

    /// [`ToolContext::changeset`] without the admission: for a handle a read
    /// tool pins, whose catalog it admits only if it reads it.
    ///
    /// # Errors
    /// As [`ToolContext::changeset`], but never [`OkfError::Busy`] from the
    /// admission.
    #[tracing::instrument(level = "debug", skip_all)]
    async fn resolve_changeset(&self, handle: &str, access: Access) -> Result<Pinned, OkfError> {
        self.check_connection().await?;
        // A closure, so each refusal below builds its own error value.
        let unknown = || OkfError::UnknownChangeset {
            changeset: handle.to_string(),
        };
        // A handle does not name its store, so every store is asked, `main`
        // first. One that cannot be reached ends the search with
        // `StoreUnavailable` (plan P3): the handle might be on it.
        let (row, store) = self
            .changeset_by_handle(handle)
            .await?
            .ok_or_else(unknown)?;
        // By the changeset's `catalogs.id`: that incarnation, never one
        // created since under its name. A catalog deleted since, or one
        // whose data does not build, resolves no handle.
        // The registry row carries no generation; the store's state row does.
        let catalog = catalogs::row_by_id(self.app.accounts().executor(), row.catalog_id).await?;
        let catalog = self
            .app
            .directory()
            .with_generation(catalog)
            .await?
            .ok_or_else(unknown)?;
        // A changeset row on a store the catalog is not on belongs to no
        // catalog this call can reach.
        if catalog.store != store {
            return Err(unknown());
        }
        let state = match self.held_state(&catalog).await {
            Ok(state) => state,
            Err(OkfError::CatalogNotSelected { .. }) => return Err(unknown()),
            Err(error) => return Err(error),
        };
        // Ownership is checked before `require(access, ..)`, on purpose: a key
        // that neither owns the changeset nor is `admin` gets
        // `UnknownChangeset`, never `Forbidden`, so it cannot tell a real
        // handle from a made-up one (Decision 3). So a `read` key, which owns
        // none, never resolves one, and `access` only matters for owners and
        // admins. Don't reorder these checks.
        let own = row.key_id.as_deref() == Some(self.principal.key_id.as_str());
        let admin = self.principal.access == Access::Admin;
        if !self.offers(&catalog) || !(own || admin) {
            return Err(unknown());
        }
        // The whole content check, the unlock last (M14 Task 9b): it refuses
        // here, before `changeset` admits.
        self.require_contents(access, &state).await?;
        // Only a discarded changeset gets the 24-hour hint: the prune is one
        // way to be discarded, and a published one was never pruned.
        let hint = match row.state {
            ChangesetState::Open => return Ok(Pinned { state, row }),
            ChangesetState::Published => "",
            ChangesetState::Discarded => " (a changeset with no edits is discarded after 24 hours)",
        };
        Err(OkfError::Refused {
            reason: format!(
                "changeset {handle} is {}; open a new one with open_changeset{hint}",
                row.state.as_str()
            ),
        })
    }

    /// Every store's name, `main` first and the rest in name order: the
    /// order a lookup that must try each store follows.
    ///
    /// # Errors
    /// [`OkfError::Database`] if the registry cannot be read;
    /// [`OkfError::Busy`] when the `stores` table (in the account database,
    /// not a store) is locked past `store_connect_timeout`. No store is
    /// unavailable then, so the call is told to retry, not given an
    /// `unavailable` entry.
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) async fn stores_main_first(&self) -> Result<Vec<StoreName>, OkfError> {
        let mut stores = self.app.stores().all().await?;
        // A stable sort on `false < true` moves `main` to the front and
        // keeps the others in the order they came.
        stores.sort_by_key(|store| *store != StoreName::main());
        Ok(stores)
    }

    /// The changeset a handle names and the store holding it, trying each
    /// store in turn ([`ToolContext::stores_main_first`]).
    ///
    /// # Arguments
    /// * `handle` - the handle as the caller gave it.
    ///
    /// # Returns
    /// The changeset's row and its store, or `None` if no store holds it
    /// (or the handle is not the shape of an issued one, which no store is
    /// asked about).
    ///
    /// # Errors
    /// [`OkfError::StoreUnavailable`] when a store that comes before the one
    /// holding the handle (or any, for an unknown handle) cannot be
    /// reached, since the handle might be on it; [`OkfError::Database`].
    #[tracing::instrument(level = "debug", skip_all)]
    async fn changeset_by_handle(
        &self,
        handle: &str,
    ) -> Result<Option<(ChangesetRow, StoreName)>, OkfError> {
        if !changesets::is_issued_handle(handle) {
            return Ok(None);
        }
        for store in self.stores_main_first().await? {
            let db = self.app.store_db(&store).await?;
            let found = changesets::by_handle(db.executor(), handle)
                .await
                // A connection that fails mid-query is the store being
                // unreachable, as for a connect.
                .map_err(|error| match error {
                    OkfError::Database { source } => db.classify(source),
                    other => other,
                })?;
            if let Some(row) = found {
                return Ok(Some((row, store)));
            }
        }
        Ok(None)
    }

    /// The state of catalog `name` for this call: the one it already
    /// resolved, else resolved now from the directory row (the one
    /// [`ToolContext::select`] read, if it did) and the catalog cache, its
    /// snapshot held for the rest of the call. For writers that must see
    /// the latest (a publish under its lock), the state's own `current()`;
    /// for reads, [`ToolContext::snapshot`].
    ///
    /// # Arguments
    /// * `name` - a catalog name, usually one `select` returned.
    ///
    /// # Returns
    /// The state.
    ///
    /// # Errors
    /// [`OkfError::CatalogNotSelected`] for a name with no catalog (deleted
    /// since `select`), or one whose stored data does not build;
    /// [`OkfError::Database`] if the directory or the load fails.
    #[tracing::instrument(level = "trace", skip_all)]
    pub(crate) async fn state(&self, name: &str) -> Result<Arc<CatalogState>, OkfError> {
        if let Some(held) = locked(&self.held).get(name) {
            return Ok(Arc::clone(&held.state));
        }
        if let Some(store) = self.down_store_of(name) {
            return Err(OkfError::store_unavailable(&store));
        }
        // `remove`: the row is needed once, then the state stands for it.
        let selected = locked(&self.rows).remove(name);
        let row =
            match selected {
                Some(row) => row,
                None => self.app.catalog_row(name).await?.ok_or_else(|| {
                    OkfError::CatalogNotSelected {
                        catalog: name.to_string(),
                    }
                })?,
            };
        self.held_state(&row).await
    }

    /// [`ToolContext::state`] for a listed row: the catalog's state, held
    /// for the rest of the call, or `None` for one to leave out because it
    /// was deleted since the row was read or its stored data does not build
    /// (as [`App::listed_state`]). A later read of it in this call reuses the
    /// held state instead of loading it again.
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
    #[tracing::instrument(level = "trace", skip_all)]
    pub(crate) async fn listed_state(
        &self,
        row: &CatalogRow,
    ) -> Result<Option<Arc<CatalogState>>, OkfError> {
        match self.held_state(row).await {
            Ok(state) => Ok(Some(state)),
            Err(OkfError::CatalogNotSelected { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// [`ToolContext::listed_state`] for a page's rows, loading up to
    /// [`LISTING_LOAD_CONCURRENCY`] at once.
    ///
    /// The first row of each store that needs the store (not held by this
    /// call, not cached at its generation) loads first, the stores
    /// concurrently, then the rest: a store that is lost is marked down by
    /// that row, so the others on it are refused from the down set without a
    /// connect attempt each, as a call probes a lost store once. A row
    /// already cached is no probe: it loads in the first wave too, so a lost
    /// store never hides it.
    ///
    /// # Arguments
    /// * `rows` - the listed catalogs' rows, in page order.
    ///
    /// # Returns
    /// One result per row, in the order of `rows`: as `listed_state` gives
    /// it, so a caller handles a broken or unreachable catalog as before.
    #[tracing::instrument(level = "trace", skip_all)]
    pub(crate) async fn listed_states(
        &self,
        rows: &[&Arc<CatalogRow>],
    ) -> Vec<Result<Option<Arc<CatalogState>>, OkfError>> {
        let mut seen = std::collections::HashSet::new();
        let mut heads = Vec::new();
        let mut tails = Vec::new();
        for (at, row) in rows.iter().enumerate() {
            // A cached row costs no probe: it answers in the first wave, before
            // a lost store is marked down, as it would in a serial walk.
            if !self.needs_store(row).await || seen.insert(row.store.as_str()) {
                heads.push(at);
            } else {
                tails.push(at);
            }
        }
        // Loops, not `iter().map(..)`: a closure returning a borrowing
        // future makes the handlers' futures fail Axum's `Send` check.
        let mut results: Vec<Option<Result<Option<Arc<CatalogState>>, OkfError>>> =
            std::iter::repeat_with(|| None).take(rows.len()).collect();
        for wave in [heads, tails] {
            let mut loads = Vec::with_capacity(wave.len());
            for &at in &wave {
                loads.push(self.listed_state(rows[at]));
            }
            for (at, result) in wave
                .into_iter()
                .zip(buffered_in_order(loads, LISTING_LOAD_CONCURRENCY).await)
            {
                results[at] = Some(result);
            }
        }
        results
            .into_iter()
            .map(|result| result.expect("every row is in the heads or the tails"))
            .collect()
    }

    /// Whether loading `row`'s state would contact its store: it is neither
    /// held by this call nor cached at the row's generation.
    #[tracing::instrument(level = "trace", skip_all)]
    async fn needs_store(&self, row: &CatalogRow) -> bool {
        let held = locked(&self.held)
            .get(&row.name)
            .is_some_and(|held| held.state.id() == row.id);
        if held {
            return false;
        }
        match self.app.catalog_cache().resident(row.id).await {
            Some(state) => state.current().generation() < row.generation,
            None => true,
        }
    }

    /// The state of `row`'s catalog for this call, resolved and held (with
    /// its snapshot) on first use. A state already held under the name is
    /// kept only if it is the same incarnation.
    ///
    /// # Errors
    /// As [`App::catalog_state`].
    #[tracing::instrument(level = "trace", skip_all)]
    async fn held_state(&self, row: &CatalogRow) -> Result<Arc<CatalogState>, OkfError> {
        let held = locked(&self.held)
            .get(&row.name)
            .filter(|held| held.state.id() == row.id)
            .map(|held| Arc::clone(&held.state));
        if let Some(state) = held {
            return Ok(state);
        }
        if locked(&self.down).stores.contains(&row.store) {
            return Err(OkfError::store_unavailable(&row.store));
        }
        let state = match self.app.catalog_state(row).await {
            Ok(state) => state,
            Err(error @ OkfError::StoreUnavailable { .. }) => {
                self.mark_down(row);
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        // `entry(..).or_insert`: a concurrent resolve of the same catalog in
        // this call (a `join!`) keeps the first snapshot taken.
        let mut held = locked(&self.held);
        let entry = held.entry(row.name.clone()).or_insert_with(|| Held {
            snapshot: state.current(),
            state: Arc::clone(&state),
        });
        if entry.state.id() == row.id {
            return Ok(Arc::clone(&entry.state));
        }
        Ok(state)
    }
}

/// How many snapshots of one listing page load at once. Bounded so a page of
/// uncached catalogs cannot take more than this many pool connections.
const LISTING_LOAD_CONCURRENCY: usize = 8;

/// Polls `loads` at most `limit` at a time and returns their outputs in the
/// order of `loads`.
///
/// # Arguments
/// * `loads` - the futures to run, not yet started (the iterator is pulled
///   as slots free up).
/// * `limit` - the most in flight at once (at least 1).
///
/// # Returns
/// Each output, in input order (a stream's `buffered`, not
/// `buffer_unordered`, so the page keeps its order).
#[tracing::instrument(level = "trace", skip_all)]
async fn buffered_in_order<I>(
    loads: I,
    limit: usize,
) -> Vec<<I::Item as std::future::Future>::Output>
where
    I: IntoIterator,
    I::Item: std::future::Future,
{
    use futures_util::StreamExt;
    futures_util::stream::iter(loads)
        .buffered(limit.max(1))
        .collect()
        .await
}

/// An admission and the incarnation it was decided for
/// ([`ToolContext::admission_to`]).
struct BoundAdmission {
    /// How it is let in.
    admission: Admission,
    /// The incarnation, from its directory row.
    scope: CatalogScope,
    /// When it was decided (server clock).
    now: DateTime<Utc>,
}

/// Refuses a pinned changeset whose catalog the call does not read (M8
/// Decision 6), so a handle never silently reads as published.
///
/// # Arguments
/// * `pinned` - the call's changeset, if any.
/// * `states` - the catalogs the call reads.
///
/// # Errors
/// [`OkfError::Refused`] naming the changeset's catalog.
#[tracing::instrument(level = "debug", skip_all)]
pub fn require_selected(
    pinned: Option<&Pinned>,
    states: &[Arc<CatalogState>],
) -> Result<(), OkfError> {
    // `let ... else`: without a pinned changeset there is nothing to check.
    let Some(pinned) = pinned else {
        return Ok(());
    };
    if states.iter().any(|state| state.id() == pinned.state.id()) {
        return Ok(());
    }
    Err(OkfError::Refused {
        reason: format!(
            "changeset {} is in catalog {:?}, which this call does not read",
            pinned.row.handle,
            pinned.state.name()
        ),
    })
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::{buffered_in_order, require_selected, ToolContext};
    use crate::config::Config;
    use crate::mcp::changesets::{open_changeset, OpenParams};
    use crate::mcp::testing::{
        app_with, context, open, pending_entry, principal, principal_as, store_key, PendingEdit,
    };
    use crate::store::changesets::{self, ChangesetState};
    use crate::store::keys::Access;

    // `Arc<CatalogState>` to its name, for asserts.
    fn names(states: &[std::sync::Arc<crate::catalog::CatalogState>]) -> Vec<&str> {
        states.iter().map(|state| state.name()).collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_argument_narrows_the_connection_and_reports_use_the_first_selected_catalog() {
        let config = Config {
            default_catalog: Some("home".to_string()),
            ..Config::default()
        };
        let test = app_with(&[("pg", &[]), ("work", &[]), ("home", &[])], config).await;
        let on_connection = context(&test, principal(&[], None), Some(&["work", "pg"]));
        let on_default = context(&test, principal(&["pg", "home"], None), None);
        let argument = ["pg".to_string()];

        let states = on_connection.states(None).await.unwrap();
        let chosen = on_connection.states(Some(&argument)).await.unwrap();
        let read_set = on_connection.read_set(Some("pg")).await.unwrap();
        let escaped = on_connection.read_set(Some("home")).await.unwrap_err();
        let first = on_connection.one(None).await.unwrap();
        let fallback = on_default.read_set(None).await.unwrap();
        let unreachable = on_default.one(Some("work")).await.unwrap_err();

        assert_eq!(names(&states), ["work", "pg"]);
        assert_eq!(names(&chosen), ["pg"]);
        assert_eq!(names(&read_set), ["pg"]);
        assert_eq!(escaped.kind(), "CatalogNotSelected");
        let directory = test.app.directory();
        let pg = directory.resolve("pg").await.unwrap().expect("pg");
        let home = directory.resolve("home").await.unwrap().expect("home");
        let work = directory.resolve("work").await.unwrap().expect("work");
        assert!(on_connection.offers(&pg) && !on_connection.offers(&home));
        assert!(on_default.offers(&home) && !on_default.offers(&work));
        assert_eq!(first.name(), "work");
        assert_eq!(names(&fallback), ["home"]);
        assert_eq!(unreachable.kind(), "CatalogNotSelected");
    }

    /// `pg`, the keys `writer`, `other` and `root`, and a changeset the
    /// writer opened in `pg` (through `open_changeset`, as a client would).
    async fn opened_by_writer() -> (crate::mcp::testing::TestApp, String) {
        let test = app_with(&[("pg", &[])], Config::default()).await;
        for key in ["writer", "other", "root"] {
            store_key(&test, key).await;
        }
        let writer = as_key(&test, "writer", Access::Write);
        let opened = open_changeset(&writer, OpenParams { catalog: None })
            .await
            .unwrap();
        (test, opened.changeset)
    }

    fn as_key(test: &crate::mcp::testing::TestApp, key: &str, access: Access) -> ToolContext {
        context(test, principal_as(key, access, &[], Some("pg")), None)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_handle_resolves_for_its_key_and_an_admin_only() {
        let (test, handle) = opened_by_writer().await;

        let mine = as_key(&test, "writer", Access::Write)
            .changeset(&handle, Access::Write)
            .await
            .unwrap();
        let admin = as_key(&test, "root", Access::Admin)
            .changeset(&handle, Access::Write)
            .await
            .unwrap();
        let other = as_key(&test, "other", Access::Write)
            .changeset(&handle, Access::Write)
            .await
            .unwrap_err();
        let made_up = as_key(&test, "writer", Access::Write)
            .changeset("made-up", Access::Write)
            .await
            .unwrap_err();

        assert_eq!(
            (mine.state.name(), mine.row.handle.as_str()),
            ("pg", handle.as_str())
        );
        assert_eq!(admin.row.id, mine.row.id);
        assert_eq!(other.kind(), "UnknownChangeset");
        assert_eq!(made_up.kind(), "UnknownChangeset");
        assert_eq!(
            other.to_string().replace(&handle, "made-up"),
            made_up.to_string()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_closed_changeset_is_refused_by_name() {
        let (test, discarded) = opened_by_writer().await;
        let writer = as_key(&test, "writer", Access::Write);
        let published = open_changeset(&writer, OpenParams { catalog: None })
            .await
            .unwrap()
            .changeset;
        let mut tx = test.app.main_db().await.begin().await.unwrap();
        for (handle, closed) in [
            (&discarded, ChangesetState::Discarded),
            (&published, ChangesetState::Published),
        ] {
            let row = writer.changeset(handle, Access::Write).await.unwrap().row;
            changesets::set_state(&mut tx, row.id, closed, Utc::now())
                .await
                .unwrap();
        }
        tx.commit().await.unwrap();

        let refused = writer
            .changeset(&discarded, Access::Write)
            .await
            .unwrap_err();
        let refused_published = writer
            .changeset(&published, Access::Write)
            .await
            .unwrap_err();

        assert_eq!(refused.kind(), "Refused");
        let message = refused.to_string();
        assert!(message.contains(&discarded), "{message}");
        assert!(message.contains("discarded"), "{message}");
        assert!(message.contains("24 hours"), "{message}");
        // Only the prune discards after 24 hours; a published one gets no hint.
        assert_eq!(refused_published.kind(), "Refused");
        assert_eq!(
            refused_published.to_string(),
            format!("changeset {published} is published; open a new one with open_changeset")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_handle_outside_the_connection_list_does_not_resolve() {
        let test = app_with(&[("pg", &[]), ("work", &[])], Config::default()).await;
        store_key(&test, "writer").await;
        let writer = as_key(&test, "writer", Access::Write);
        let handle = open(&writer).await;
        let work_only = context(
            &test,
            principal_as("writer", Access::Write, &[], Some("pg")),
            Some(&["work"]),
        );

        let refused = work_only
            .changeset(&handle, Access::Write)
            .await
            .unwrap_err();

        assert_eq!(refused.kind(), "UnknownChangeset");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bad_connection_list_refuses_a_handle_it_would_otherwise_resolve() {
        let test = app_with(&[("pg", &[]), ("work", &[])], Config::default()).await;
        store_key(&test, "writer").await;
        let handle = open(&as_key(&test, "writer", Access::Write)).await;
        // `nope` does not exist; `work` exists but the key does not reach it.
        for (list, bad) in [(["pg", "nope"], "nope"), (["pg", "work"], "work")] {
            let on_list = context(
                &test,
                principal_as("writer", Access::Write, &["pg"], Some("pg")),
                Some(&list),
            );

            let refused = on_list.changeset(&handle, Access::Write).await.unwrap_err();

            assert_eq!(refused.kind(), "CatalogNotSelected", "{list:?}");
            assert!(
                refused.to_string().contains(&format!("{bad:?}")),
                "{refused}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_read_key_resolves_nothing() {
        let (test, handle) = opened_by_writer().await;
        let reader = context(&test, principal(&[], Some("pg")), None);

        let refused = reader.changeset(&handle, Access::Read).await.unwrap_err();

        assert_eq!(refused.kind(), "UnknownChangeset");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_read_without_a_changeset_is_published_even_for_a_writer_with_drafts() {
        let test = app_with(&[("pg", &[("a.md", "# A\n")])], Config::default()).await;
        store_key(&test, "writer").await;
        let writer = as_key(&test, "writer", Access::Write);
        let handle = open(&writer).await;
        let edit = PendingEdit {
            path: "a.md",
            text: Some("# Pending\n"),
            at: Utc::now(),
        };
        pending_entry(&test, &handle, &edit).await;
        let state = test.app.catalog("pg").await.unwrap().unwrap();

        let pinned = writer.pin(Some(&handle)).await.unwrap();
        let unpinned = writer.pin(None).await.unwrap();
        let published = writer.view(&state, None).await.unwrap();
        let drafts = writer.view(&state, pinned.as_ref()).await.unwrap();

        assert!(unpinned.is_none());
        assert!(!published.pending());
        assert_eq!(published.snapshot().get("a.md").unwrap().text(), "# A\n");
        assert!(drafts.pending());
        assert_eq!(drafts.snapshot().get("a.md").unwrap().text(), "# Pending\n");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_pinned_changeset_applies_only_to_its_own_catalog() {
        let test = app_with(
            &[("pg", &[("a.md", "# A\n")]), ("work", &[("a.md", "# W\n")])],
            Config::default(),
        )
        .await;
        store_key(&test, "writer").await;
        let writer = as_key(&test, "writer", Access::Write);
        let handle = open(&writer).await;
        let edit = PendingEdit {
            path: "a.md",
            text: Some("# Pending\n"),
            at: Utc::now(),
        };
        pending_entry(&test, &handle, &edit).await;
        let pinned = writer.pin(Some(&handle)).await.unwrap();
        let pg = test.app.catalog("pg").await.unwrap().unwrap();
        let work = test.app.catalog("work").await.unwrap().unwrap();

        let in_work = writer.view(&work, pinned.as_ref()).await.unwrap();
        let in_pg = writer.view(&pg, pinned.as_ref()).await.unwrap();
        let outside = require_selected(pinned.as_ref(), std::slice::from_ref(&work)).unwrap_err();
        let inside = require_selected(pinned.as_ref(), &[work, pg]);

        assert!(!in_work.pending());
        assert_eq!(in_work.snapshot().get("a.md").unwrap().text(), "# W\n");
        assert!(in_pg.pending());
        assert_eq!(outside.kind(), "Refused");
        let message = outside.to_string();
        assert!(
            message.contains(&handle) && message.contains("\"pg\""),
            "{message}"
        );
        assert!(inside.is_ok());
        assert!(require_selected(None, &[]).is_ok());
    }

    /// Every way a tool picks catalogs through the selection (M13 Task 4):
    /// `select`, `states`, `read_set`, `one`, `report_catalog`,
    /// `check_connection` and the `search` tool. Each refuses a named
    /// suspended catalog, drops it from a connection's list, refuses a list
    /// of only suspended catalogs, answers `CatalogNotSelected` to a key that
    /// does not reach it, and leaves an admin alone.
    #[tokio::test(flavor = "multi_thread")]
    async fn every_selection_path_refuses_or_drops_a_suspended_catalog() {
        use crate::error::OkfError;
        use crate::mcp::search::{run, SearchParams};
        use crate::mcp::testing::{cached_principal, stored_secret, suspend_catalogs};
        let test = app_with(
            &[
                ("pg", &[("a.md", "# Alpha\n")]),
                ("work", &[("a.md", "# Alpha\n")]),
            ],
            Config::default(),
        )
        .await;
        let member = stored_secret(&test, "member", Access::Read, &["pg", "work"], false).await;
        let stranger = stored_secret(&test, "stranger", Access::Read, &["work"], false).await;
        let root = stored_secret(&test, "root", Access::Admin, &[], false).await;
        suspend_catalogs(&test, &["pg"]).await;
        let on = async |secret: &str, connection: Option<&[&str]>| {
            context(&test, cached_principal(&test, secret).await, connection)
        };
        // Each path's answer for `catalog` (`None`: the connection's list),
        // as the selected names or the error's kind.
        async fn paths(ctx: &ToolContext, catalog: Option<&str>) -> Vec<(&'static str, String)> {
            fn kind(error: OkfError) -> String {
                error.kind().to_string()
            }
            fn named(states: Vec<std::sync::Arc<crate::catalog::CatalogState>>) -> String {
                names(&states).join(",")
            }
            let argument = catalog.map(|name| vec![name.to_string()]);
            let searched = run(
                ctx,
                SearchParams {
                    query: "alpha".to_string(),
                    catalogs: argument.clone(),
                    concept_type: None,
                    tags: Vec::new(),
                    status: None,
                    limit: None,
                    changeset: None,
                },
            )
            .await
            .map(|result| {
                let mut catalogs: Vec<String> =
                    result.hits.into_iter().map(|hit| hit.hit.catalog).collect();
                catalogs.dedup();
                catalogs.join(",")
            });
            vec![
                (
                    "select",
                    ctx.select(argument.as_deref())
                        .await
                        .map_or_else(kind, |selection| selection.catalogs().join(",")),
                ),
                (
                    "states",
                    ctx.states(argument.as_deref())
                        .await
                        .map_or_else(kind, named),
                ),
                (
                    "read_set",
                    ctx.read_set(catalog)
                        .await
                        .map(|set| set.states)
                        .map_or_else(kind, named),
                ),
                (
                    "one",
                    ctx.one(catalog)
                        .await
                        .map_or_else(kind, |state| state.name().to_string()),
                ),
                (
                    "report_catalog",
                    ctx.report_catalog(catalog, None)
                        .await
                        .map_or_else(kind, |state| state.name().to_string()),
                ),
                ("search", searched.unwrap_or_else(kind)),
            ]
        }
        // `one` and `report_catalog` answer only the first selected catalog.
        let expect = |answers: &[(&'static str, String)], want: &str, case: &str| {
            for (path, answer) in answers {
                let want = match *path {
                    "one" | "report_catalog" => want.split(',').next().unwrap_or(want),
                    _ => want,
                };
                assert_eq!(answer, want, "{path}: {case}");
            }
        };

        expect(
            &paths(&on(&member, None).await, Some("pg")).await,
            "Suspended",
            "named",
        );
        expect(
            &paths(&on(&member, Some(&["pg", "work"])).await, Some("pg")).await,
            "Suspended",
            "named on a connection listing it",
        );
        expect(
            &paths(&on(&member, Some(&["pg", "work"])).await, None).await,
            "work",
            "a mixed connection list keeps the active catalog",
        );
        expect(
            &paths(&on(&member, Some(&["pg"])).await, None).await,
            "Suspended",
            "a connection list of only suspended catalogs",
        );
        expect(
            &paths(&on(&stranger, None).await, Some("pg")).await,
            "CatalogNotSelected",
            "a key that does not reach pg never learns it is suspended",
        );
        expect(
            &paths(&on(&root, None).await, Some("pg")).await,
            "pg",
            "an admin, named",
        );
        expect(
            &paths(&on(&root, Some(&["pg", "work"])).await, None).await,
            "pg,work",
            "an admin keeps the whole connection list",
        );
        assert!(on(&member, Some(&["pg", "work"]))
            .await
            .check_connection()
            .await
            .is_ok());
        assert!(matches!(
            on(&member, Some(&["pg"])).await.check_connection().await,
            Err(OkfError::Suspended { .. })
        ));
        assert!(matches!(
            on(&stranger, Some(&["pg"])).await.check_connection().await,
            Err(OkfError::CatalogNotSelected { .. })
        ));
        assert!(on(&root, Some(&["pg"]))
            .await
            .check_connection()
            .await
            .is_ok());
    }

    /// M14 Task 7a: a tool call's audit actor carries the request's
    /// address and source, as the auth layer left them in its extensions.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_context_from_a_request_records_its_address_and_source() {
        use std::net::IpAddr;

        use crate::http::console_session::ConsoleSession;
        use crate::http::requester::ClientIp;
        use crate::store::audit::Source;

        let test = app_with(&[("pg", &[])], Config::default()).await;
        let ip: IpAddr = "192.0.2.4".parse().unwrap();
        let (parts, ()) = axum::http::Request::builder()
            .extension(principal(&[], None))
            .extension(ClientIp(Some(ip)))
            .extension(ConsoleSession::for_test())
            .body(())
            .unwrap()
            .into_parts();

        let from_request = ToolContext::from_parts(&test.app, &parts).unwrap();
        let bare = context(&test, principal(&[], None), None);

        let actor = from_request.audit_actor();
        assert_eq!((actor.ip, actor.source), (Some(ip), Source::Console));
        let actor = bare.audit_actor();
        assert_eq!((actor.ip, actor.source), (None, Source::Api));
    }

    #[tokio::test]
    async fn buffered_in_order_keeps_input_order_when_loads_finish_out_of_order() {
        // Arrange: earlier items sleep longer, so they finish last.
        let items = vec![30_u64, 20, 10, 0];

        // Act
        let out = buffered_in_order(
            items.into_iter().map(|delay| async move {
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                delay
            }),
            4,
        )
        .await;

        // Assert
        assert_eq!(out, vec![30, 20, 10, 0]);
    }

    #[tokio::test]
    async fn buffered_in_order_never_runs_more_than_the_limit_at_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        // Arrange: a loader that counts the loads in flight.
        let running = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);

        // Act
        let out = buffered_in_order(
            (0..20_usize).map(|item| {
                let (running, peak) = (&running, &peak);
                async move {
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    running.fetch_sub(1, Ordering::SeqCst);
                    item
                }
            }),
            3,
        )
        .await;

        // Assert: all ran, the bound held, and it was actually used.
        assert_eq!(out, (0..20).collect::<Vec<_>>());
        assert_eq!(peak.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn buffered_in_order_returns_each_error_at_its_place() {
        let out = buffered_in_order(
            (0..4_u32).map(|item| async move {
                if item % 2 == 1 {
                    Err(format!("item {item} failed"))
                } else {
                    Ok(item)
                }
            }),
            2,
        )
        .await;

        assert_eq!(
            out,
            vec![
                Ok(0),
                Err("item 1 failed".to_string()),
                Ok(2),
                Err("item 3 failed".to_string())
            ]
        );
    }

    #[tokio::test]
    async fn buffered_in_order_treats_a_zero_limit_as_one() {
        let out = buffered_in_order((0..3_u32).map(|item| async move { item }), 0).await;

        assert_eq!(out, vec![0, 1, 2]);
    }
}
