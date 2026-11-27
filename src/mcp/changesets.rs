//! The changeset tools (design "Write tools"): `publish` (Task 10);
//! `discard`, `revert` and `pending_changes` (Task 11); `list_changesets`
//! (M8 Task 4, replacing Task 12's `stranded_sessions` and
//! `adopt_changeset`); `open_changeset` (M8 Task 2). Each task's tools are
//! one `#[tool_router]` block with its own router, joined in
//! `OkfServer::tools`.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use axum::http::request::Parts;
use chrono::{DateTime, TimeDelta, Utc};
use rayon::prelude::*;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router, Json};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use similar::TextDiff;

use crate::auth::Reach;
use crate::catalog::Loaded;
use crate::clock::format_timestamp;
use crate::error::OkfError;
use crate::labels::directory::Wanted;
use crate::mcp::context::{Pinned, ToolContext};
use crate::mcp::{blocking, reply, OkfServer};
use crate::okf::ops::OpOutcome;
use crate::okf::paths::{escapes_root, normpath};
use crate::okf::rules;
use crate::okf::workspace::Workspace;
use crate::store::audit::Action;
use crate::store::catalogs;
use crate::store::keys::Access;
// `as publishing`: the module's `publish` would clash with this file's
// `publish` function.
use crate::mcp::history::Persons;
use crate::store::publish as publishing;
use crate::stores::{StoreName, READ_CONCURRENCY};
use crate::write::{apply_audited, OpAudit, WriteReport};

/// `list_changesets`' arguments.
// `Deserialize` reads them from the call's JSON; `JsonSchema` gives the tool
// its input schema, with the doc comments as the fields' descriptions.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct ListParams {
    /// Only this catalog id (the `name` from `list_catalogs`; default: every catalog the connection offers: the
    /// key's, limited to the connection's list when it has one).
    pub catalog: Option<String>,
}

/// One open changeset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ListedChangeset {
    /// Its handle: pass it as `changeset` to resume work on it.
    pub changeset: String,
    /// Its catalog (id).
    pub catalog: String,
    /// The catalog's label: say this to people, pass `catalog` in calls.
    pub catalog_label: String,
    /// The key it belongs to; null once that key is deleted.
    pub key: Option<String>,
    /// The key's grouping name; null once that key is deleted.
    pub key_name: Option<String>,
    /// The actor of the key that opened it.
    pub actor: String,
    /// The label of the actor when the actor is itself a user (a console session); null for a key or OAuth actor. The person behind any credential is `user_label`.
    pub actor_label: Option<String>,
    /// The person who opened it: their user id; null for a key no user owns. Read from the key now, so reassigning the key changes it.
    pub user: Option<String>,
    /// The person's label: say this to people. Null with no `user`.
    pub user_label: Option<String>,
    /// When it was opened.
    pub created: String,
    /// Its last change.
    pub last_change: String,
    /// No change within `session_idle_timeout`.
    pub idle: bool,
    /// Its changed paths, `/`-prefixed and sorted.
    pub paths: Vec<String>,
}

/// `list_changesets`' result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Listed {
    /// By catalog, oldest change first.
    pub changesets: Vec<ListedChangeset>,
    /// The ids of catalogs this call offers whose store could not be reached
    /// (sorted, each once), so their changesets are not listed: retry. Only
    /// set without `catalog`; absent when every store answered.
    // Left out of the JSON when empty, as the other optional fields are.
    // `default` keeps the key optional in the output schema, as it is in the JSON.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unavailable: Vec<String>,
}

/// The time before which an open changeset's last change makes it idle.
///
/// # Arguments
/// * `now` - the current time (server clock).
/// * `idle` - `session_idle_timeout`.
///
/// # Returns
/// `now - idle`, or the earliest representable time if that would overflow
/// (an absurdly long timeout: nothing is idle).
#[tracing::instrument(level = "trace", skip_all)]
fn idle_cutoff(now: DateTime<Utc>, idle: Duration) -> DateTime<Utc> {
    // `TimeDelta::from_std` fails for a duration beyond chrono's range, and
    // `checked_sub_signed` for a result before its earliest date; both `ok()`
    // and `and_then` turn a failure into `None`.
    TimeDelta::from_std(idle)
        .ok()
        .and_then(|idle| now.checked_sub_signed(idle))
        .unwrap_or(DateTime::<Utc>::MIN_UTC)
}

/// Lists the open changesets this key may use (M8 Decision 8).
///
/// # Arguments
/// * `ctx` - the call's context.
/// * `params` - an optional catalog to narrow the list to.
///
/// # Returns
/// Open changesets, empty ones included, in the catalogs the connection
/// offers ([`ToolContext::offers`]) or the one named: the key's own, and
/// with `admin` every key's. Without `catalog`, a catalog whose owner team
/// is suspended is left out for a key below `admin` (M13).
///
/// # Errors
/// A selection error for `catalog` (including [`OkfError::Suspended`]), or
/// for a connection list ([`ToolContext::check_connection`]: one whose
/// every catalog is suspended is refused); [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn list_changesets(
    ctx: &ToolContext,
    params: ListParams,
) -> Result<Listed, OkfError> {
    // Local, as in the other tasks' functions here.
    use crate::store::changesets;

    let principal = ctx.principal();
    let admin = principal.access >= Access::Admin;
    // The database narrows by key (unless admin) and catalog, so other
    // keys' changesets and paths never leave it.
    let key_filter = (!admin).then_some(principal.key_id.as_str());
    let accounts = ctx.app().accounts();
    let mut unavailable = Vec::new();
    let rows = match params.catalog.as_deref() {
        // `one` refuses a catalog the key does not reach; it checks the name
        // and the incarnation whose changesets are listed, and admits
        // nothing: a list of changesets reads no catalog's contents.
        Some(name) => {
            let state = ctx.one(Some(name)).await?;
            let store = ctx.app().store_db(state.store()).await?;
            changesets::list_open(accounts, &store, &[state.id()], key_filter).await?
        }
        None => {
            // A list naming a bad catalog fails here as it does elsewhere.
            ctx.check_connection().await?;
            // Narrowed in SQL to the catalogs the connection offers (its
            // list, else the key's grants; every catalog for a key that
            // reaches them all), without enumerating the catalogs first.
            let names: Option<Vec<String>> = match (ctx.connection(), &principal.reach) {
                (Some(list), _) if !list.is_empty() => Some(list.to_vec()),
                (_, Reach::Catalogs(levels)) => Some(levels.keys().cloned().collect()),
                // A console session's ceiling is not listed: it is never
                // admin, so the key filter (its own changesets) bounds the
                // rows, and each is checked against its row below.
                (_, Reach::All | Reach::Ceiling) => None,
            };
            // Across catalogs: every store in turn, `main` first, each at
            // most `READ_CONCURRENCY` at a time; a store that cannot be
            // reached is left out and its catalogs are named below.
            let (rows, down) = open_in_every_store(ctx, names.as_deref(), key_filter).await?;
            unavailable = unreachable_catalogs(ctx, names.as_deref(), &down).await?;
            // A closed catalog shows no changesets: its handles and paths are
            // part of what suspension (M13) and a lock (M14) withhold.
            // `offers` (reach) is checked first; the lock is checked against
            // the incarnation each row belongs to, not only its name.
            let mut open = Vec::with_capacity(rows.len());
            for row in rows {
                // The by-name half of `offers` first, so a row the name
                // rules out costs no lookup.
                if principal.excludes(&row.catalog) {
                    continue;
                }
                let catalog_id = row.changeset.catalog_id;
                let catalog = match ctx.row_by_id(&row.catalog, catalog_id).await {
                    Ok(catalog) => catalog,
                    // A failed read is not a closed catalog.
                    Err(error @ OkfError::Database { .. }) => return Err(error),
                    // Gone, or now another catalog: nothing of it to list.
                    Err(_) => continue,
                };
                if ctx.offers(&catalog)
                    && principal.check_not_suspended(&catalog).is_ok()
                    && ctx
                        .check_contents_by_id(&row.catalog, catalog_id, Access::Read)
                        .await
                        .is_ok()
                {
                    open.push(row);
                }
            }
            open
        }
    };
    let idle_since = idle_cutoff(
        ctx.app().clock().now(),
        ctx.app().config().session_idle_timeout,
    );
    let persons = Persons::load(ctx, rows.iter().filter_map(|row| row.user_id.as_deref())).await?;
    let mut wanted = Wanted::default();
    persons.want(&mut wanted);
    for row in &rows {
        wanted.catalog(&row.catalog).user(&row.changeset.actor);
    }
    let labels = ctx.labels_for(&wanted).await?;
    let changesets = rows
        .into_iter()
        .map(|row| {
            let (user, user_label) = persons.show(row.user_id.as_deref(), &labels);
            ListedChangeset {
                user,
                user_label,
                changeset: row.changeset.handle,
                catalog_label: labels
                    .catalog(&row.catalog)
                    .unwrap_or(&row.catalog)
                    .to_string(),
                catalog: row.catalog,
                actor_label: labels.actor(&row.changeset.actor).map(str::to_string),
                key: row.changeset.key_id,
                key_name: row.key_name,
                actor: row.changeset.actor,
                created: format_timestamp(row.changeset.created_at),
                last_change: format_timestamp(row.changeset.updated_at),
                idle: row.changeset.updated_at < idle_since,
                paths: row.paths.iter().map(|path| format!("/{path}")).collect(),
            }
        })
        .collect();
    Ok(Listed {
        changesets,
        unavailable,
    })
}

/// The open changesets of the catalogs `names` (every catalog for `None`) in
/// every store, read together (at most [`READ_CONCURRENCY`] stores at once)
/// and each store within its connect timeout.
///
/// # Arguments
/// * `ctx` - the call's context.
/// * `names` - as for [`crate::store::changesets::open_for_catalogs`].
/// * `key_filter` - as for it.
///
/// # Returns
/// The rows, by catalog name then last change (as one store gives them) and
/// the stores that could not be reached.
///
/// # Errors
/// [`OkfError::Database`] for a reachable store's failed read.
#[tracing::instrument(level = "debug", skip_all)]
async fn open_in_every_store(
    ctx: &ToolContext,
    names: Option<&[String]>,
    key_filter: Option<&str>,
) -> Result<(Vec<crate::store::changesets::ListedRow>, Vec<StoreName>), OkfError> {
    use crate::store::changesets;

    let accounts = ctx.app().accounts();
    let bound = ctx.app().stores().connect_timeout();
    let stores = ctx.stores_main_first().await?;
    // A chunk's reads run together; chunks of `READ_CONCURRENCY` bound how
    // many stores are asked at once, and the stores' order is kept.
    let mut answers: Vec<Result<Vec<changesets::ListedRow>, OkfError>> =
        Vec::with_capacity(stores.len());
    for chunk in stores.chunks(READ_CONCURRENCY) {
        answers.extend(
            futures_util::future::join_all(chunk.iter().map(|store| async move {
                let read = async {
                    let db = ctx.app().store_db(store).await?;
                    changesets::open_for_catalogs(accounts, &db, names, key_filter)
                        .await
                        .map_err(|error| match error {
                            // A connection that fails mid-query is the store
                            // being unreachable, as for a connect.
                            OkfError::Database { source } => db.classify(source),
                            other => other,
                        })
                };
                tokio::time::timeout(bound, read)
                    .await
                    .unwrap_or_else(|_| Err(OkfError::store_unavailable(store)))
            }))
            .await,
        );
    }
    let mut rows = Vec::new();
    let mut down = Vec::new();
    let mut contributing = 0;
    for (store, answer) in stores.into_iter().zip(answers) {
        match answer {
            Ok(found) => {
                contributing += usize::from(!found.is_empty());
                rows.extend(found);
            }
            Err(OkfError::StoreUnavailable { .. }) => down.push(store),
            Err(error) => return Err(error),
        }
    }
    // One store's own order stands (it is the database's collation); stores
    // merge by catalog name, and a stable sort keeps each catalog's changesets
    // in the order its store gave them.
    if contributing > 1 {
        rows.sort_by(|left, right| left.catalog.cmp(&right.catalog));
    }
    Ok((rows, down))
}

/// The ids of the catalogs this call offers that sit on the `down` stores:
/// what a list names as unavailable. Read from the account database, and
/// only catalogs the key reaches and the connection offers.
///
/// # Arguments
/// * `ctx` - the call's context.
/// * `names` - the catalogs the key reaches by name, or `None` for any.
/// * `down` - the stores that could not be reached.
///
/// # Returns
/// The ids, sorted, each once.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all)]
async fn unreachable_catalogs(
    ctx: &ToolContext,
    names: Option<&[String]>,
    down: &[StoreName],
) -> Result<Vec<String>, OkfError> {
    let mut unavailable = Vec::new();
    for store in down {
        let rows = match names {
            Some(names) => catalogs::rows_by_names(ctx.app().accounts().executor(), names)
                .await?
                .into_iter()
                .filter(|row| row.store == *store)
                .collect(),
            None => catalogs::rows_on_store(ctx.app().accounts().executor(), store).await?,
        };
        unavailable.extend(
            rows.iter()
                .filter(|row| ctx.offers(row))
                .map(|row| row.name.clone()),
        );
    }
    unavailable.sort();
    unavailable.dedup();
    Ok(unavailable)
}

// As in the read tool modules: the macro turns this block's `#[tool]`
// methods into `lifecycle_router`, which `OkfServer::tools` joins.
#[tool_router(router = lifecycle_router, vis = "pub(crate)")]
impl OkfServer {
    /// The `list_changesets` tool.
    #[tool(
        title = "List Changesets",
        description = "List the open changesets this key may use (an admin: every key's, in the \
                       catalogs it reaches), with their handles, paths and last change; idle \
                       means nothing changed within the idle timeout. Empty changesets are \
                       listed too, but one with no write within 24 hours is discarded and no \
                       longer listed: open a new one then. Use it to find a handle again after a \
                       reconnect or a lost context. Read access. A catalog whose store cannot be \
                       reached is listed in `unavailable`.",
        annotations(
            title = "List Changesets",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn list_changesets(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(params): Parameters<ListParams>,
    ) -> Result<Json<Listed>, CallToolResult> {
        let ctx = self.context(&parts)?;
        reply(list_changesets(&ctx, params).await)
    }
}

// ---------------------------------------------------------------------------
// open_changeset (M8 Task 2)
// ---------------------------------------------------------------------------

/// `open_changeset`'s arguments.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct OpenParams {
    /// The catalog id (the `name` from `list_catalogs`; default: this connection's write target; when `list_catalogs` shows none, pass `catalog`).
    pub catalog: Option<String>,
}

/// `open_changeset`'s result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Opened {
    /// The new changeset's handle: pass it as `changeset` to every write, `publish`, `discard`, `revert` and `pending_changes`, and to reads that should see the drafts.
    pub changeset: String,
    /// Its catalog (id).
    pub catalog: String,
    /// The catalog's label: say this to people, pass `catalog` in calls.
    pub catalog_label: String,
}

/// Opens a changeset and returns its handle (M8 Decision 1).
///
/// # Arguments
/// * `ctx` - the call's context.
/// * `params` - the catalog; else the connection's write target.
///
/// # Returns
/// The new changeset's handle and its catalog's name.
///
/// # Errors
/// A selection error for `catalog`; [`OkfError::Forbidden`] without `write`,
/// or for a console session (it has no key row to own the changeset; M12
/// Decision 5); [`OkfError::Refused`] if the catalog was deleted meanwhile;
/// [`OkfError::Busy`] on a deadlock; [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn open_changeset(
    ctx: &ToolContext,
    params: OpenParams,
) -> Result<Opened, OkfError> {
    // Local, as in the other tasks' functions here.
    use crate::auth::CredentialKind;
    use crate::store::changesets;

    // Opening reads no contents: nothing is admitted (the writes are
    // recorded as publishes).
    let state = ctx.one(params.catalog.as_deref()).await?;
    let principal = ctx.principal();
    // `changesets.key_id` references `api_keys`. MCP takes only bearer
    // credentials, so this guards any later `/api/v1` caller.
    if matches!(principal.kind, CredentialKind::Session { .. }) {
        return Err(OkfError::Forbidden {
            reason: "a console session cannot open a changeset; use an API key".to_string(),
        });
    }
    let now = ctx.app().clock().now();
    // The changeset's writes go to the catalog's contents, so they need
    // what any write does, the unlock included (M14 Task 9b).
    // Decided by the row of this very incarnation: an unlock is of one
    // catalog incarnation, and one of a catalog created again under its
    // name must not open a changeset in the replacement (PR #442).
    ctx.require_contents(Access::Write, &state).await?;
    let db = ctx.app().store_db(state.store()).await?;
    // The interlock with a deletion of the key (a user's, issue #972): its
    // row `FOR KEY SHARE` on the account connection, taken BEFORE the
    // store's connection (`CatalogDb::begin`'s order) and kept until after
    // the store commits. `changesets.key_id` has no foreign key now, so
    // this is what makes the deletion (which locks the key `FOR UPDATE`)
    // wait for the open, and the cleanup it owes then find the changeset;
    // an open that comes after the deletion finds no key and is refused.
    let mut account_tx = db.accounts().begin().await?;
    let held: Option<String> =
        sqlx::query_scalar("SELECT id FROM api_keys WHERE id = $1 FOR KEY SHARE")
            .bind(&principal.key_id)
            .fetch_optional(&mut *account_tx)
            .await?;
    if held.is_none() {
        return Err(OkfError::Forbidden {
            reason: "this key no longer exists".to_string(),
        });
    }
    // The lock order of every changeset write: the catalog row first, so a
    // concurrent `catalog delete` either waits for this or is seen here.
    let mut tx = db.begin().await?;
    changesets::lock_catalog(&mut tx, state.id()).await?;
    let opened = changesets::open(
        &mut tx,
        state.id(),
        &principal.key_id,
        &principal.actor,
        now,
    )
    .await?;
    tx.commit().await.map_err(|error| db.classify(error))?;
    account_tx.commit().await?;
    Ok(Opened {
        changeset: opened.handle,
        catalog: state.name().to_string(),
        catalog_label: ctx.catalog_label(state.name()).await?,
    })
}

#[tool_router(router = open_router, vis = "pub(crate)")]
impl OkfServer {
    /// The `open_changeset` tool.
    #[tool(
        title = "Open Changeset",
        description = "Open a changeset in a catalog and return its handle: the first step \
                       to saving what the person shares or asks you to remember, which the \
                       catalog keeps across conversations. Every edit, \
                       publish, discard, revert and pending_changes needs a handle, and the \
                       reads take one to see its drafts. Nothing is published until \
                       publish; a published concept is visible only to those with access to \
                       the catalog (a personal catalog's owner unless they share it, a team \
                       catalog's team). Several changesets may be open at once. A changeset with no \
                       edits is discarded after 24 hours. Write access.",
        annotations(
            title = "Open Changeset",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn open_changeset(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(params): Parameters<OpenParams>,
    ) -> Result<Json<Opened>, CallToolResult> {
        let ctx = self.context(&parts)?;
        reply(open_changeset(&ctx, params).await)
    }
}

// ---------------------------------------------------------------------------
// publish (Task 10)
// ---------------------------------------------------------------------------

/// `publish`'s arguments.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct PublishParams {
    /// What the change is, recorded on every revision it writes (like a commit message).
    pub message: String,
    /// The changeset to publish: the handle `open_changeset` returned.
    pub changeset: String,
}

/// What `publish` says when the changeset held no entries (issue #758).
const EMPTY_PUBLISH_REASON: &str = "nothing to publish: this changeset has no entries. If you \
    sent edits in the same turn, publish after they return.";

/// `publish`'s result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Published {
    /// The changeset's catalog (id).
    pub catalog: String,
    /// The catalog's label: say this to people, pass `catalog` in calls.
    pub catalog_label: String,
    /// The changeset's handle. With `published: false` it was not published and stays open (it held no entries, for instance after a discard emptied it).
    pub changeset: String,
    /// Why nothing was published (absent when something was, or when every edit already matched the published text): the changeset held no entries. Edits sent in the same turn as `publish` may not have landed yet; publish again after they return.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// What was written; `flatten` puts its fields beside these.
    #[serde(flatten)]
    pub report: crate::store::publish::PublishReport,
}

/// Publishes the changeset a handle names, in its catalog.
///
/// # Arguments
/// * `ctx` - the call's context.
/// * `params` - the message and the changeset.
///
/// A publish that writes revisions records `catalog.published` in its own
/// transaction, as the call's [`ToolContext::audit_actor`].
///
/// # Returns
/// The report; `published: false` for a changeset with no entries.
///
/// # Errors
/// [`OkfError::Refused`] for a blank message; as [`ToolContext::changeset`]
/// (with `write`); as [`crate::store::publish::publish`].
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn publish(
    ctx: &ToolContext,
    params: PublishParams,
) -> Result<Published, OkfError> {
    let message = params.message.trim();
    if message.is_empty() {
        return Err(OkfError::Refused {
            reason: "publish needs a message saying what changed".to_string(),
        });
    }
    let Pinned { state, row } = ctx.changeset(&params.changeset, Access::Write).await?;
    let input = publishing::PublishInput {
        changeset: &row,
        actor: &ctx.principal().actor,
        message,
    };
    let report = publishing::publish(ctx.app(), &state, &input, &ctx.audit_actor()).await?;
    let reason = (!report.published).then(|| EMPTY_PUBLISH_REASON.to_string());
    Ok(Published {
        catalog: state.name().to_string(),
        catalog_label: ctx.catalog_label(state.name()).await?,
        changeset: row.handle,
        reason,
        report,
    })
}

#[tool_router(router = publish_router, vis = "pub(crate)")]
impl OkfServer {
    /// The `publish` tool.
    #[tool(
        title = "Publish Changeset",
        description = "Publish the changeset `changeset` names: its edits become the \
                       published documents, each with a new revision carrying `message`. Refused \
                       with Conflict when a document changed since this changeset read it \
                       (log.md and index.md bookkeeping, and the stamp, review, question, unquestion, \
                       add_source and remove_source edits of a concept, are replayed onto the new \
                       text instead; a replay that can no longer apply is refused with its reason), \
                       or with LintFailed when a changed file has lint errors; the changeset \
                       then stays open (an error the published file already has is a warning, not \
                       a refusal). Lint errors elsewhere come back as warnings. When every \
                       edit already matches the published text, the changeset is closed with \
                       published: true and no revisions. A changeset with no entries returns published: false with a \
                       `reason` (edits sent in the same turn as publish may not have landed; publish \
                       again after they return). With several replicas, a read on another replica may still show the previous revision for a \
                       moment after this returns (usually well under a second); reads through a \
                       changeset and pending_changes are always current for its own edits. \
                       Write access.",
        annotations(
            title = "Publish Changeset",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn publish(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(params): Parameters<PublishParams>,
    ) -> Result<Json<Published>, CallToolResult> {
        let ctx = self.context(&parts)?;
        reply(publish(&ctx, params).await)
    }
}

// ---------------------------------------------------------------------------
// discard, revert and pending_changes (Task 11)
// ---------------------------------------------------------------------------

/// `discard`'s arguments.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct DiscardParams {
    /// The paths to drop, e.g. `["/infra/ntp.md"]` (default: every entry, which also closes the changeset for good).
    pub paths: Option<Vec<String>>,
    /// The changeset to drop entries from: the handle `open_changeset` returned.
    pub changeset: String,
}

/// `discard`'s result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Discarded {
    /// The changeset's catalog (id).
    pub catalog: String,
    /// The catalog's label: say this to people, pass `catalog` in calls.
    pub catalog_label: String,
    /// The changeset's handle.
    pub changeset: String,
    /// How many entries were dropped.
    pub removed: u64,
    /// Whether the changeset was closed (`discarded`): no `paths` given. Its handle is then refused; call `open_changeset` for new work.
    pub closed: bool,
}

/// `revert`'s arguments.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct RevertParams {
    /// The document as stored, with its extension, e.g. `/infra/ntp.md`.
    pub path: String,
    /// The revision to go back to, as `history` lists it; a `delete` revision means deleting the document.
    pub revision: i32,
    /// The document's `latest_revision` as `history` showed it (for a live document the same number as `current_revision`; for a deleted one, the deletion's revision). If the document has been published since, the revert is refused with Conflict naming the revision now (nothing is written), so you can re-read `history`. Omit it to revert over whatever is published now.
    #[serde(default)]
    pub expected_revision: Option<i32>,
    /// The changeset to write to: the handle `open_changeset` returned.
    pub changeset: String,
}

/// One file of `pending_changes`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct PendingFile {
    /// `/`-prefixed path.
    pub path: String,
    /// `create`, `update` or `delete`, against the published catalog now.
    pub change: &'static str,
    /// The published revision the entry was made on (null: the path had no live document).
    pub base_revision: Option<i32>,
    /// The path's published revision now (null: no live document).
    pub current_revision: Option<i32>,
    /// Whether the path was published since the entry was made (`base_revision` differs from `current_revision`). Publish replays a stale replayable entry and refuses any other with Conflict.
    pub stale: bool,
    /// Whether publish can replay the entry onto newer text (bookkeeping only: log.md and index.md edits, and a concept's stamp, review, question, unquestion, add_source and remove_source edits, unless a free edit of the path follows).
    pub replayable: bool,
    /// Whether `diff` shows the entry replayed onto the published text, the bytes publish would write: true only for a stale, replayable entry whose replay succeeds. Publish still runs its lint on that result. False: `diff` shows the entry's stored text, or the `replay_error` note.
    pub replayed: bool,
    /// Why publish cannot replay this stale, replayable entry (null otherwise): publish would refuse the changeset until the path is discarded and the change made again. `diff` is then a one-line note saying so.
    pub replay_error: Option<String>,
    /// A warning on a stale entry publish cannot replay at all, a free edit of the path (null otherwise: a fresh entry, or a stale replayable one, whose `replay_error` says when it fails): publish would refuse the changeset with Conflict until the path is discarded and the edit made again on the current text. `diff` is then the entry's stored text against the text published now, not what publish would write.
    pub conflict_note: Option<String>,
    /// A unified diff against the published text (`/dev/null` for a new or deleted file); for a stale, replayable entry, of its replayed text (`replayed`). A deletion of a path deleted since (no published file), a file equal to the published one, or a replay that would fail at publish (`replay_error`) gets a one-line note instead.
    pub diff: String,
}

/// `pending_changes`' arguments.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct PendingParams {
    /// The changeset to show: the handle `open_changeset` returned.
    pub changeset: String,
}

/// `pending_changes`' result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct PendingChanges {
    /// The changeset's catalog (id).
    pub catalog: String,
    /// The catalog's label: say this to people, pass `catalog` in calls.
    pub catalog_label: String,
    /// The changeset's handle.
    pub changeset: String,
    /// The person who opened it: their user id; null for a key no user owns. Read from the key now, so reassigning the key changes it.
    pub user: Option<String>,
    /// The person's label: say this to people. Null with no `user`.
    pub user_label: Option<String>,
    /// Its entries, in path order.
    pub files: Vec<PendingFile>,
}

/// A path argument as stored: no leading `/`, normalized, inside the
/// catalog. Unlike [`rules::check_path`] it allows machinery, so `discard`
/// can drop any entry, even one a later rule would refuse.
///
/// # Arguments
/// * `reference` - the path as the agent gave it.
///
/// # Returns
/// The stored form.
///
/// # Errors
/// [`OkfError::InvalidPath`] for a path outside the catalog or the root.
#[tracing::instrument(level = "trace", skip_all)]
fn stored_path(reference: &str) -> Result<String, OkfError> {
    let path = normpath(reference.trim_start_matches('/'));
    if escapes_root(&path) || path == "." {
        return Err(OkfError::InvalidPath {
            path: reference.to_string(),
            reason: "not a path inside the catalog".to_string(),
        });
    }
    Ok(path)
}

/// Drops entries from the changeset a handle names; without `paths`, all
/// of them, and the changeset is closed (`discarded`) for good: its handle
/// is refused from then on.
///
/// Takes publish's locks in publish's order: the catalog's in-process
/// publish lock, then (in the transaction) the advisory lock, the catalog
/// row and the changeset row. So a discard and a publish of one changeset
/// run one after the other, and a write queued behind a closing discard is
/// refused.
///
/// Closing a changeset another key owns (an admin's) records
/// `catalog.changeset_discarded` in the same transaction; the owner's own
/// discards are not administrative changes and record nothing.
///
/// # Arguments
/// * `ctx` - the call's context.
/// * `params` - the paths and the changeset.
///
/// # Returns
/// How many entries went, and whether the changeset was closed.
///
/// # Errors
/// [`OkfError::InvalidPath`] for a path outside the catalog; as
/// [`ToolContext::changeset`] (with `write`); [`OkfError::Refused`] if a
/// publish or another discard closed the changeset meanwhile;
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn discard(
    ctx: &ToolContext,
    params: DiscardParams,
) -> Result<Discarded, OkfError> {
    use crate::store::audit::{self, CatalogEntry};
    use crate::store::catalogs;
    use crate::store::changesets::{self, ChangesetState};

    // `transpose` turns `Option<Result<..>>` into `Result<Option<..>>`, so
    // `?` can raise a bad path while "no paths" stays `None`.
    let paths = params
        .paths
        .map(|paths| {
            paths
                .iter()
                .map(|path| stored_path(path))
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?;
    let Pinned {
        state,
        row: changeset,
    } = ctx.changeset(&params.changeset, Access::Write).await?;
    let mut discarded = Discarded {
        catalog: state.name().to_string(),
        catalog_label: ctx.catalog_label(state.name()).await?,
        changeset: params.changeset,
        removed: 0,
        closed: false,
    };
    // An empty list drops nothing: no lock, no version bump, and the cached
    // overlay stays valid.
    if paths.as_ref().is_some_and(Vec::is_empty) {
        return Ok(discarded);
    }
    let app = ctx.app();
    // The guard releases the mutex when it goes out of scope, after the
    // overlay is forgotten.
    let _closing = app.publish_lock(state.id()).await;
    let now = app.clock().now();
    // Closing another key's changeset is an admin's administrative change;
    // an owner closing its own is not recorded. Decided from the row read
    // before the lock, so its users can be locked before the changeset.
    let principal = ctx.principal();
    let recorded =
        paths.is_none() && changeset.key_id.as_deref() != Some(principal.key_id.as_str());
    // Dropping `tx` without `commit()` (any `?`) rolls back.
    // A recorded close wraps the store transaction in an account one (the
    // entry's account ids); the store commits first. The entry itself is
    // written in the store transaction.
    let mut account_tx = match recorded {
        true => Some(app.accounts().begin().await?),
        false => None,
    };
    let mut resolved = None;
    if let Some(account_tx) = account_tx.as_mut() {
        // The entry's users before the changeset, as publish (issue #418).
        crate::store::publish::lock_people(account_tx, &ctx.audit_actor(), state.name()).await?;
        // Still before the store connection: the entry's account ids.
        resolved = Some(audit::resolve_actor(account_tx, &ctx.audit_actor(), state.id()).await?);
    }
    // Acquisition order (plan Task 11, issue #970): the account transaction
    // and every account row lock (`lock_people`) come BEFORE the store
    // connection. A change that held the store's connection while it waited
    // for a user row would block a user deletion (which holds those rows and
    // then needs a store connection) in a cycle PostgreSQL cannot see,
    // because one edge is a pool wait.
    let mut tx = app.store_db(state.store()).await?.begin().await?;
    catalogs::lock(&mut tx, state.id()).await?;
    changesets::lock_catalog(&mut tx, state.id()).await?;
    changesets::lock(&mut tx, changeset.id)
        .await?
        .filter(|row| row.state == ChangesetState::Open)
        .ok_or_else(|| OkfError::Refused {
            reason: format!(
                "changeset {} was published or discarded meanwhile",
                changeset.handle
            ),
        })?;
    discarded.removed =
        changesets::remove_entries(&mut tx, changeset.id, paths.as_deref(), now).await?;
    discarded.closed = paths.is_none();
    if discarded.closed {
        changesets::set_state(&mut tx, changeset.id, ChangesetState::Discarded, now).await?;
        if let Some(resolved) = resolved.as_ref() {
            let entry = CatalogEntry {
                action: Action::CatalogChangesetDiscarded,
                catalog_id: state.id(),
                catalog: state.name(),
                details: serde_json::json!({
                    "changeset": changeset.handle,
                    "files": discarded.removed,
                    "reason": "admin",
                }),
            };
            audit::record_catalog(&mut tx, resolved, &entry, now).await?;
        }
    }
    tx.commit().await?;
    if let Some(account_tx) = account_tx {
        account_tx.commit().await?;
    }
    app.overlays().forget(changeset.key()).await;
    Ok(discarded)
}

/// Adds a document's content at `revision` (or its deletion, for a
/// `delete` revision) to the changeset a handle names, as a free edit.
///
/// It goes through [`apply_audited`], so every write rule applies, and
/// records `catalog.reverted` in the edit's own transaction. The entry's
/// base is the revision published now, in the snapshot the write's overlay
/// is built on, even when the changeset had edited the path before (issue
/// #788): publish then conflicts only if the path is published again after
/// the revert. Sending a path that is not published now (one the changeset
/// created, or one deleted since) back to "absent" leaves no entry for it,
/// so the report's `touched` is empty.
///
/// # Arguments
/// * `ctx` - the call's context.
/// * `params` - the path, revision, optional `expected_revision` and
///   changeset.
///
/// # Returns
/// The write's report.
///
/// # Errors
/// [`OkfError::Conflict`] when `expected_revision` is given and is not the
/// path's latest revision now (nothing is written; issues #795, #1127);
/// [`OkfError::InvalidPath`] for machinery or a path outside the catalog
/// (before any history is read); as [`ToolContext::changeset`] (with
/// `write`); [`OkfError::NotFound`] for a
/// revision the path never had; [`OkfError::Refused`] for content that is
/// not UTF-8, or a deletion of a path this changeset's view does not hold;
/// as [`apply_audited`].
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn revert(
    ctx: &ToolContext,
    params: RevertParams,
) -> Result<WriteReport, OkfError> {
    use crate::store::history;

    let RevertParams {
        path,
        revision,
        expected_revision,
        changeset,
    } = params;
    // The write rules refuse machinery here, before the history is read;
    // `apply` checks the touched path again.
    let path = rules::check_path(&path)?;
    // The history is the changeset's catalog's; `apply` resolves the handle
    // again under its locks.
    let state = ctx.changeset(&changeset, Access::Write).await?.state;
    let stored = history::content_at(
        &ctx.app().store_db(state.store()).await?,
        state.id(),
        &path,
        revision,
    )
    .await?
    .ok_or_else(|| OkfError::NotFound {
        path: format!("/{path} (revision {revision})"),
    })?;
    // `transpose` again: `Option<Result<String, _>>` to `Result<Option<..>>`.
    let content = stored
        .content
        .map(String::from_utf8)
        .transpose()
        .map_err(|_| OkfError::Refused {
            reason: format!(
                "/{path} revision {revision} is not text; only text documents can be reverted"
            ),
        })?;
    // `move` hands `path` and `content` to the closure, which `apply` runs
    // on a blocking thread.
    // The entry is recorded in the edit's own transaction.
    let audit = OpAudit {
        action: Action::CatalogReverted,
        // `Map::from_iter` builds the JSON object from (key, value) pairs.
        details: serde_json::Map::from_iter([
            ("path".to_string(), format!("/{path}").into()),
            ("revision".to_string(), revision.into()),
        ]),
    };
    apply_audited(ctx, &changeset, None, Some(audit), move |ws, _| {
        match &content {
            Some(text) => ws.write_atomic(&path, text)?,
            None if !ws.remove(&path)? => {
                return Err(OkfError::Refused {
                    reason: format!("/{path} is already absent in this changeset's view"),
                });
            }
            None => {}
        }
        let message = format!("{path}: reverted to revision {revision}");
        // No replay: a revert is a free edit, which conflicts on a stale
        // base. Its text is a whole-file choice made against the published
        // history, so its base is the revision published now, even when the
        // changeset already had an entry for the path (`rebased`, issue
        // #788): an earlier edit's base and replay no longer apply to it.
        // `path` moves into the outcome after one copy for `rebased`.
        Ok(OpOutcome {
            touched: vec![path.clone()],
            replay: BTreeMap::new(),
            message,
            notes: Vec::new(),
            // The caller's `history` read, checked in `apply` against the
            // snapshot the base comes from (issue #795).
            expected: expected_revision
                .map(|seen| (path.clone(), seen))
                .into_iter()
                .collect(),
            rebased: BTreeSet::from([path]),
        })
    })
    .await
}

/// The changeset a handle names, as diffs against the published text.
///
/// It reads the entries and the published state rather than building the
/// overlay, so a changeset whose overlay no longer applies can still be
/// listed, and then discarded.
///
/// # Arguments
/// * `ctx` - the call's context.
/// * `params` - the changeset.
///
/// # Returns
/// The changed paths and their diffs.
///
/// # Errors
/// As [`ToolContext::changeset`] with `read`: the key that owns it or an
/// admin may ask; a `read` key owns none.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn pending_changes(
    ctx: &ToolContext,
    params: PendingParams,
) -> Result<PendingChanges, OkfError> {
    use crate::store::changesets;

    let Pinned { state, row } = ctx.changeset(&params.changeset, Access::Read).await?;
    let store = ctx.app().store_db(state.store()).await?;
    let entries = changesets::entries(store.executor(), row.id).await?;
    // An entry made on another replica (or before this one's snapshot moved)
    // can have a base newer than the snapshot here: refresh before diffing,
    // or the diff would be against older text (issue #659).
    let bases = entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry.base_revision));
    // The changeset's latest base, not the request's pinned snapshot: the
    // diff is against what the entries were made on.
    let before = state.current().generation();
    let loaded = state
        .current_covering(&store, ctx.app().rebuild_locks(), bases)
        .await?;
    ctx.app()
        .catalog_cache()
        .refreshed_if_moved(&state, before)
        .await;
    let files = blocking(move || pending_files(&loaded, &entries)).await?;
    let opener = match row.key_id.as_deref() {
        Some(key) => changesets::opener_users(ctx.app().accounts(), &[key])
            .await?
            .remove(key),
        None => None,
    };
    let persons = Persons::load(ctx, opener.as_deref()).await?;
    let mut wanted = Wanted::default();
    persons.want(&mut wanted);
    let labels = ctx.labels_for(&wanted).await?;
    let (user, user_label) = persons.show(opener.as_deref(), &labels);
    Ok(PendingChanges {
        catalog: state.name().to_string(),
        catalog_label: ctx.catalog_label(state.name()).await?,
        changeset: row.handle,
        user,
        user_label,
        files,
    })
}

/// Each entry against the published file (on a blocking thread).
///
/// # Arguments
/// * `loaded` - the published state.
/// * `entries` - the changeset's entries, in path order.
///
/// # Returns
/// One [`PendingFile`] per entry, in the same order.
///
/// # Errors
/// [`OkfError::Busy`] if a path's published revision in `loaded` is older
/// than its entry's base: the snapshot is behind the changeset, and a diff
/// against it would show published text as pending (issue #659). The
/// response never reports `current_revision < base_revision`;
/// [`CatalogState::current_covering`](crate::catalog::CatalogState::current_covering)
/// refreshes first, so only a snapshot that a refresh could not bring up to
/// date reaches this.
#[tracing::instrument(level = "trace", skip_all)]
fn pending_files(
    loaded: &Loaded,
    entries: &[crate::store::changesets::EntryRow],
) -> Result<Vec<PendingFile>, OkfError> {
    // The diffs are independent: rayon's `par_iter` spreads them over the
    // CPU pool, and `collect` keeps the entries' (path) order.
    entries
        .par_iter()
        .map(|entry| {
            let published = loaded.workspace().stored(&entry.path);
            let change = match (&published, &entry.content) {
                (_, None) => "delete",
                (None, Some(_)) => "create",
                (Some(_), Some(_)) => "update",
            };
            let current_revision = loaded.revision(&entry.path);
            // The invariant: a published revision is never older than the
            // base an entry was made on.
            if let (Some(base), Some(current)) = (entry.base_revision, current_revision) {
                if current < base {
                    return Err(OkfError::Busy {
                        reason: format!(
                            "this replica's snapshot of /{} is at revision {current}, older than \
                             the changeset's base revision {base}",
                            entry.path
                        ),
                    });
                }
            }
            let old = published.as_ref().map(|file| file.content.as_slice());
            let stale = entry.base_revision != current_revision;
            let replayable = entry.replay.is_some();
            // A stale, replayable entry is shown as publish would write it:
            // its operations redone on the published text by the function
            // publish calls (issue #787). Its stored text was built on the
            // old base, so a diff of that would show everything published
            // since as deleted.
            let (replayed, replay_error, diff) = if stale && replayable {
                match publishing::replayed_text(entry, old) {
                    Ok(Some(text)) => (
                        true,
                        None,
                        unified_diff(&entry.path, old, Some(text.as_bytes())),
                    ),
                    Ok(None) => {
                        let reason = cannot_replay(&entry.path, current_revision);
                        (false, Some(reason.clone()), format!("{reason}\n"))
                    }
                    Err(error) => {
                        let reason = error.to_string();
                        let note = format!(
                            "Publish cannot replay this entry onto the published text: {reason}\n"
                        );
                        (false, Some(reason), note)
                    }
                }
            } else {
                let diff = unified_diff(&entry.path, old, entry.content.as_deref());
                (false, None, diff)
            };
            let conflict_note = (stale && !replayable).then(|| stale_free_edit_note(&entry.path));
            Ok(PendingFile {
                path: format!("/{}", entry.path),
                change,
                base_revision: entry.base_revision,
                current_revision,
                stale,
                replayable,
                replayed,
                replay_error,
                conflict_note,
                diff,
            })
        })
        .collect()
}

/// Why a stale, replayable entry has no replay at publish (the cases
/// [`publishing::replayed_text`] answers `None` for): publish refuses it
/// with Conflict.
///
/// # Arguments
/// * `path` - catalog-relative path.
/// * `current` - the path's published revision now (`None`: not live).
///
/// # Returns
/// The reason, naming the path and what to do.
#[tracing::instrument(level = "trace", skip_all)]
pub(crate) fn cannot_replay(path: &str, current: Option<i32>) -> String {
    let now = match current {
        Some(revision) => format!("revision {revision}"),
        None => "a deleted document".to_string(),
    };
    format!(
        "Publish cannot replay this entry onto /{path} as published now ({now}) and would \
         refuse it with Conflict; discard /{path} from this changeset and make the change again"
    )
}

/// The warning on a stale entry that is not replayable (a free edit of a
/// path published since): publish refuses it with Conflict, and its `diff`
/// is not what publish would write.
///
/// # Arguments
/// * `path` - catalog-relative path.
///
/// # Returns
/// The note, naming the path and what to do.
#[tracing::instrument(level = "trace", skip_all)]
fn stale_free_edit_note(path: &str) -> String {
    format!(
        "/{path} was published since this edit was made and the edit cannot be replayed, so \
         publish would refuse this changeset with Conflict until /{path} is discarded from it \
         and the edit made again on the current text. `diff` compares this entry's stored \
         text with the text published now; it is not what publish would write"
    )
}

/// A unified diff (3 lines of context) between the published and the
/// pending file. A missing side is `/dev/null`, as `git diff` writes it.
///
/// # Arguments
/// * `path` - catalog-relative path.
/// * `published` - the published bytes, if the file is published.
/// * `pending` - the pending bytes, or `None` for a deletion.
///
/// # Returns
/// The diff text; a one-line "Binary files ... differ" when either side is
/// not UTF-8; a one-line note for a deletion of a path that is not
/// published now (deleted by someone else since the entry was made), or for
/// pending bytes equal to the published ones.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) fn unified_diff(path: &str, published: Option<&[u8]>, pending: Option<&[u8]>) -> String {
    if published.is_none() && pending.is_none() {
        return format!(
            "No published {path} to delete: it was deleted after this entry was made\n"
        );
    }
    // Byte for byte the published file (a revert to the revision published
    // now, say): a diff would be empty, so say why.
    if published.is_some() && published == pending {
        return "No change from the published text\n".to_string();
    }
    // A missing side diffs as empty text. A nested `fn`, not a closure:
    // its signature ties the `&str` it returns to the bytes it borrows,
    // which closure inference cannot express.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    fn as_text(bytes: Option<&[u8]>) -> Result<&str, std::str::Utf8Error> {
        bytes.map_or(Ok(""), std::str::from_utf8)
    }
    let (Ok(old), Ok(new)) = (as_text(published), as_text(pending)) else {
        return format!("Binary files a/{path} and b/{path} differ\n");
    };
    let old_header = published.map_or_else(|| "/dev/null".to_string(), |_| format!("a/{path}"));
    let new_header = pending.map_or_else(|| "/dev/null".to_string(), |_| format!("b/{path}"));
    TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(&old_header, &new_header)
        .to_string()
}

#[tool_router(router = changes_router, vis = "pub(crate)")]
impl OkfServer {
    /// The `discard` tool.
    #[tool(
        title = "Discard Changeset Entries",
        description = "Drop entries from the changeset `changeset` names: the named `paths`, or \
                       every entry, which closes the changeset for good (its handle is refused \
                       from then on; call open_changeset for new work). Published documents are \
                       not touched. Write access.",
        annotations(
            title = "Discard Changeset Entries",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn discard(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(params): Parameters<DiscardParams>,
    ) -> Result<Json<Discarded>, CallToolResult> {
        let ctx = self.context(&parts)?;
        reply(discard(&ctx, params).await)
    }

    /// The `revert` tool.
    #[tool(
        title = "Revert Document",
        description = "Add a document's content at an earlier revision (from `history`), or its \
                       deletion for a `delete` revision, to the changeset `changeset` names, for \
                       review and publish like any other edit. Every write rule applies, so \
                       machinery cannot be reverted. Prefer passing `expected_revision` (the \
                       `latest_revision` from `history`, which also guards restoring a deleted \
                       document): it refuses with Conflict, writing \
                       nothing, if the document was published since. The revert is based on the revision \
                       published now, even if this changeset edited the document before, so \
                       publish conflicts only if the document is published again after it. \
                       Reverting a file this changeset created to a `delete` revision drops it \
                       from the changeset. Write access.",
        annotations(
            title = "Revert Document",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn revert(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(params): Parameters<RevertParams>,
    ) -> Result<Json<WriteReport>, CallToolResult> {
        let ctx = self.context(&parts)?;
        reply(revert(&ctx, params).await)
    }

    /// The `pending_changes` tool.
    #[tool(
        title = "Show Pending Changes",
        description = "The unpublished changeset `changeset` names: each changed path \
                       (create, update or delete), the revision it was based on and the one \
                       published now, whether that base is stale, whether publish can replay \
                       it, and a unified diff against the published text. A stale entry \
                       publish can replay is diffed as replayed onto the published text \
                       (replayed: true), the bytes publish would write; one whose replay would \
                       fail gets replay_error and a note instead. A stale entry publish cannot \
                       replay (a free edit) gets conflict_note: publish would refuse it with \
                       Conflict until the path is discarded and the edit redone, and its diff \
                       is against the text published now.",
        annotations(
            title = "Show Pending Changes",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn pending_changes(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(params): Parameters<PendingParams>,
    ) -> Result<Json<PendingChanges>, CallToolResult> {
        let ctx = self.context(&parts)?;
        reply(pending_changes(&ctx, params).await)
    }
}

#[cfg(test)]
mod open_tests {
    use super::*;

    use crate::config::Config;
    use crate::mcp::testing::{app_with, principal, writer};

    #[tokio::test(flavor = "multi_thread")]
    async fn open_changeset_returns_a_handle_that_resolves() {
        let test = app_with(&[("pg", &[]), ("work", &[])], Config::default()).await;
        let ctx = writer(&test, "alice", "pg").await;
        let reader = crate::mcp::testing::context(&test, principal(&[], Some("pg")), None);

        let opened = open_changeset(&ctx, OpenParams { catalog: None })
            .await
            .unwrap();
        let in_work = open_changeset(
            &ctx,
            OpenParams {
                catalog: Some("work".to_string()),
            },
        )
        .await
        .unwrap();
        let pinned = ctx
            .changeset(&opened.changeset, Access::Write)
            .await
            .unwrap();
        let refused = open_changeset(&reader, OpenParams { catalog: None })
            .await
            .unwrap_err();

        assert_eq!(opened.catalog, "pg");
        assert_eq!(in_work.catalog, "work");
        assert_ne!(opened.changeset, in_work.changeset);
        assert_eq!(pinned.state.name(), "pg");
        assert_eq!(pinned.row.handle, opened.changeset);
        assert_eq!(pinned.row.key_id.as_deref(), Some("alice"));
        assert_eq!(refused.kind(), "Forbidden");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_console_session_cannot_open_a_changeset() {
        let test = app_with(&[("pg", &[])], Config::default()).await;
        let user = crate::store::users::UserAuth {
            id: "9eaae459-441c-49c2-8613-e3996bdc96ee".to_string(),
            name: "alice".to_string(),
            display_name: String::new(),
            email: None,
            is_admin: true,
        };
        // An admin session: it passes every level check.
        let session = crate::auth::Principal::for_session(&user, Default::default());
        let ctx = crate::mcp::testing::context(&test, std::sync::Arc::new(session), None);

        let refused = open_changeset(
            &ctx,
            OpenParams {
                catalog: Some("pg".to_string()),
            },
        )
        .await
        .unwrap_err();

        assert_eq!(refused.kind(), "Forbidden", "{refused}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_changeset_call_on_a_multi_catalog_connection_records_one_admin_read() {
        use crate::mcp::testing::{cached_principal, context, settle_access, staff_root};
        let test = app_with(
            &[
                ("main", &[("index.md", "# Main\n")]),
                ("other", &[("index.md", "# Other\n")]),
            ],
            Config::default(),
        )
        .await;
        // An every-catalog admin key of `staff`, outside both catalogs' owner team.
        let secret = staff_root(&test).await;
        let ctx = context(
            &test,
            cached_principal(&test, &secret).await,
            Some(&["main", "other"]),
        );
        let named = |name: &str| ListParams {
            catalog: Some(name.to_string()),
        };

        // Opening, listing and the connection checks they run read no
        // catalog's contents; the changeset read reads `main`'s.
        let opened = open_changeset(
            &ctx,
            OpenParams {
                catalog: Some("main".to_string()),
            },
        )
        .await
        .unwrap();
        ctx.changeset(&opened.changeset, Access::Read)
            .await
            .unwrap();
        list_changesets(&ctx, ListParams { catalog: None })
            .await
            .unwrap();
        list_changesets(&ctx, named("other")).await.unwrap();
        settle_access(&test).await;

        let reads = crate::store::audit::testing::entries(
            test.app.accounts().executor(),
            "catalog.admin_read",
        )
        .await;
        assert_eq!(
            reads.len(),
            1,
            "only the changeset's own catalog: {reads:?}"
        );
        assert_eq!(reads[0].target.as_deref(), Some("main"));
        assert_eq!(
            reads[0].requests,
            Some(1),
            "the connection check and the lists record nothing"
        );
    }

    #[test]
    fn open_changeset_is_listed_with_an_object_output_schema() {
        let tools = OkfServer::tools().list_all();

        let listed = tools
            .iter()
            .find(|tool| tool.name == "open_changeset")
            .expect("open_changeset is registered");

        let schema = listed.output_schema.as_ref().expect("an output schema");
        assert_eq!(schema["type"], "object");
        assert!(listed.input_schema["properties"]
            .as_object()
            .unwrap()
            .contains_key("catalog"));
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use chrono::{TimeDelta, Utc};

    use crate::config::Config;
    use crate::mcp::testing::{
        app_with, context, open_at, pending_entry, principal_as, store_key, PendingEdit, TestApp,
    };

    /// `pg` and `work`, and the keys `alice`, `bob` and `root`.
    async fn test_app() -> TestApp {
        let test = app_with(
            &[("pg", &[("a.md", "# A\n")]), ("work", &[])],
            Config::default(),
        )
        .await;
        for key in ["alice", "bob", "root"] {
            store_key(&test, key).await;
        }
        test
    }

    /// A connection of `key` with `access`, reaching `reach` (empty: all).
    fn connection(test: &TestApp, key: &str, access: Access, reach: &[&str]) -> ToolContext {
        context(test, principal_as(key, access, reach, Some("pg")), None)
    }

    /// A new changeset of `ctx`'s key in `pg` with an edit of `a.md`, both
    /// `hours_ago` hours ago; its handle.
    async fn edit_hours_ago(test: &TestApp, ctx: &ToolContext, hours_ago: i64) -> String {
        let at = Utc::now() - TimeDelta::hours(hours_ago);
        let opened = open_at(test, ctx, "pg", at).await;
        let edit = PendingEdit {
            path: "a.md",
            text: Some("# Pending\n"),
            at,
        };
        pending_entry(test, &opened.handle, &edit).await.handle
    }

    /// The handles listed, in order.
    fn handles(listed: &Listed) -> Vec<&str> {
        listed
            .changesets
            .iter()
            .map(|changeset| changeset.changeset.as_str())
            .collect()
    }

    fn all() -> ListParams {
        ListParams { catalog: None }
    }

    #[test]
    fn the_idle_cutoff_is_now_less_the_timeout_and_never_overflows() {
        let now = Utc::now();

        let cutoff = idle_cutoff(now, std::time::Duration::from_secs(7200));
        let huge = idle_cutoff(now, std::time::Duration::MAX);

        assert_eq!(cutoff, now - TimeDelta::hours(2));
        assert_eq!(huge, chrono::DateTime::<Utc>::MIN_UTC);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_changesets_lists_only_the_connection_s_catalogs() {
        let test = test_app().await;
        let alice = connection(&test, "alice", Access::Write, &[]);
        let in_pg = open_at(&test, &alice, "pg", Utc::now()).await.handle;
        let in_work = open_at(&test, &alice, "work", Utc::now()).await.handle;
        let work_only = context(
            &test,
            principal_as("alice", Access::Write, &[], Some("pg")),
            Some(&["work"]),
        );

        let unlisted = list_changesets(&alice, all()).await.unwrap();
        let listed = list_changesets(&work_only, all()).await.unwrap();

        assert_eq!(unlisted.changesets.len(), 2);
        assert!(handles(&unlisted).contains(&in_pg.as_str()));
        assert_eq!(handles(&listed), [in_work.as_str()]);
        // `nope` does not exist; the second key reaches only `work`, not `pg`.
        for (list, reach) in [(["work", "nope"], &[][..]), (["work", "pg"], &["work"][..])] {
            let on_list = context(
                &test,
                principal_as("alice", Access::Write, reach, Some("work")),
                Some(&list),
            );

            let refused = list_changesets(&on_list, all()).await.unwrap_err();

            assert_eq!(refused.kind(), "CatalogNotSelected", "{list:?}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_changesets_shows_a_key_its_own_and_an_admin_everyone_s() {
        let test = test_app().await;
        let alice = connection(&test, "alice", Access::Write, &[]);
        let empty = open_at(&test, &alice, "pg", Utc::now()).await.handle;
        // Three hours is past the default `session_idle_timeout` of two.
        let bobs = edit_hours_ago(&test, &connection(&test, "bob", Access::Write, &[]), 3).await;
        let admin = connection(&test, "root", Access::Admin, &[]);

        let by_alice = list_changesets(&alice, all()).await.unwrap();
        let by_admin = list_changesets(&admin, all()).await.unwrap();
        let by_admin_of_work =
            list_changesets(&connection(&test, "root", Access::Admin, &["work"]), all())
                .await
                .unwrap();
        let narrowed = list_changesets(
            &admin,
            ListParams {
                catalog: Some("work".to_string()),
            },
        )
        .await
        .unwrap();
        let unreachable = list_changesets(
            &connection(&test, "alice", Access::Write, &["work"]),
            ListParams {
                catalog: Some("pg".to_string()),
            },
        )
        .await;

        assert_eq!(handles(&by_alice), [empty.as_str()]);
        let own = &by_alice.changesets[0];
        assert_eq!(
            (
                own.catalog.as_str(),
                own.key_name.as_deref(),
                own.key.as_deref(),
                own.actor.as_str()
            ),
            ("pg", Some("alice"), Some("alice"), "alice")
        );
        assert!(own.paths.is_empty(), "an empty changeset is listed too");
        assert!(!own.idle);
        assert_eq!(
            handles(&by_admin),
            [bobs.as_str(), empty.as_str()],
            "oldest change first"
        );
        let old = &by_admin.changesets[0];
        assert_eq!(old.key.as_deref(), Some("bob"));
        assert_eq!(old.paths, ["/a.md"]);
        assert!(old.idle);
        assert!(old.last_change < own.last_change);
        assert!(by_admin_of_work.changesets.is_empty());
        assert!(narrowed.changesets.is_empty());
        assert_eq!(unreachable.unwrap_err().kind(), "CatalogNotSelected");
    }

    #[test]
    fn list_changesets_is_listed_and_the_old_tools_are_gone() {
        let tools = OkfServer::tools().list_all();

        let listed = tools
            .iter()
            .find(|tool| tool.name == "list_changesets")
            .expect("list_changesets is registered");
        let schema = listed.output_schema.as_ref().expect("an output schema");
        assert_eq!(schema["type"], "object");
        for gone in ["stranded_sessions", "adopt_changeset"] {
            assert!(tools.iter().all(|tool| tool.name != gone), "{gone}");
        }
    }
}

#[cfg(test)]
mod publish_tests {
    use super::*;
    use std::time::Duration;

    use chrono::Utc;

    use crate::config::Config;
    use crate::error::ConflictFile;
    use crate::mcp::testing::{
        app_with, app_with_metrics, changeset_row, context, files, open, principal_as, render,
        store_key, writer, TestApp,
    };
    use crate::mcp::writes::{replace_body, ReplaceBodyParams};
    use crate::okf::ops::{self, LogKind};
    use crate::store::changesets::{self, ChangesetState, EntryRow};
    use crate::store::import::{import, ImportOptions, ImportScope};
    use crate::store::keys::Access;
    use crate::store::publish::PublishedRevision;
    use crate::store::{catalogs, history};
    use crate::telemetry::subscriber::capture::{SeenSpan, SeenValue, SpanRecorder};
    use crate::write::{apply, WriteReport};

    const NOTE: &str = "---\ntype: Note\ntitle: A\ndescription: A note.\n---\n# A\n";
    const X: &str = "---\ntype: Note\ntitle: X\ndescription: X.\n---\n# X\n";
    const Y: &str = "---\ntype: Note\ntitle: Y\ndescription: Y.\n---\n# Y\n";
    const HUB: &str =
        "---\ntype: Note\ntitle: Hub\ndescription: Links.\n---\nSee [X](/infra/x.md).\n";

    async fn pg() -> TestApp {
        let files: &[(&str, &str)] = &[
            ("a.md", NOTE),
            ("hub.md", HUB),
            ("infra/index.md", "# Infra\n"),
            ("infra/x.md", X),
            ("infra/y.md", Y),
            ("log.md", "# Log\n"),
        ];
        app_with(&[("pg", files)], Config::default()).await
    }

    /// [`pg`] with its own meter provider, so the test reads what the app
    /// recorded.
    async fn pg_with_metrics() -> (TestApp, prometheus::Registry) {
        let files: &[(&str, &str)] = &[
            ("a.md", NOTE),
            ("hub.md", HUB),
            ("infra/index.md", "# Infra\n"),
            ("infra/x.md", X),
            ("infra/y.md", Y),
            ("log.md", "# Log\n"),
        ];
        app_with_metrics(&[("pg", files)], Config::default()).await
    }

    /// The one span named `name` in `spans`.
    fn span<'a>(spans: &'a [SeenSpan], name: &str) -> &'a SeenSpan {
        let mut named = spans.iter().filter(|span| span.name == name);
        let found = named.next().unwrap_or_else(|| panic!("no span {name}"));
        assert!(named.next().is_none(), "more than one span {name}");
        found
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn publish_records_duration_and_lock_wait() {
        let (test, registry) = pg_with_metrics().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;
        body(&ctx, &handle, "/a.md", "# A\n\nNew.\n").await;
        let recorder = SpanRecorder::new();

        recorder.run(publish_as(&ctx, &handle, "edit a")).unwrap();

        let spans = recorder.finished();
        let publish = span(&spans, "okf.publish");
        for child in ["okf.publish.lock", "okf.publish.lint", "okf.publish.write"] {
            assert_eq!(span(&spans, child).parent, Some(publish.id), "{child}");
        }
        assert_eq!(
            publish.field("okf.changeset.entries"),
            Some(&SeenValue::I64(1))
        );
        let text = render(&registry);
        assert!(
            text.contains(r#"okf_publish_duration_seconds_count{outcome="ok"} 1"#),
            "{text}"
        );
        assert!(
            text.contains(r#"okf_publish_lock_wait_seconds_count 1"#),
            "{text}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_lint_failure_counts_by_rule() {
        let (test, registry) = pg_with_metrics().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;
        body(&ctx, &handle, "/a.md", "See [gone](/gone.md).\n").await;

        let error = publish_as(&ctx, &handle, "broken").await.unwrap_err();

        assert_eq!(error.kind(), "LintFailed");
        let text = render(&registry);
        assert!(
            text.contains(r#"okf_lint_rejections_total{rule="broken-link"}"#),
            "{text}"
        );
        assert!(
            text.contains(r#"okf_publish_duration_seconds_count{outcome="error"} 1"#),
            "{text}"
        );
        // The lock was waited for before the lint refused.
        assert!(
            text.contains(r#"okf_publish_lock_wait_seconds_count 1"#),
            "{text}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn open_changesets_gauge() {
        let (test, registry) = pg_with_metrics().await;
        let first = writer(&test, "writer-a", "pg").await;
        let second = writer(&test, "writer-b", "pg").await;
        let _kept = open(&first).await;
        let published = open(&second).await;
        body(&second, &published, "/a.md", "# A\n\nNew.\n").await;
        // Counted by the `cluster_stats` housekeeping job, read into the
        // gauge by the ticker (issue #693): one cluster-wide series.
        let count = || async {
            // The store's count, then the cluster's sum (plan Task 16).
            crate::housekeeping::run_on_store(
                crate::housekeeping::Task::ClusterStats,
                &crate::stores::StoreName::main(),
                &test.app,
            )
            .await
            .unwrap();
            crate::housekeeping::run(crate::housekeeping::Task::ClusterStats, &test.app)
                .await
                .unwrap();
            crate::housekeeping::refresh_gauges(&test.app).await;
        };
        count().await;
        let text = render(&registry);
        assert!(text.contains("okf_changesets_open 2"), "{text}");

        publish_as(&second, &published, "edit a").await.unwrap();
        // Counted by the job, not read at the scrape.
        assert!(render(&registry).contains("okf_changesets_open 2"));
        count().await;

        let text = render(&registry);
        assert!(text.contains("okf_changesets_open 1"), "{text}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_settings_message_counts_the_settings_load() {
        let (test, registry) = pg_with_metrics().await;
        // A settings write moves `admin_generation`, so its message's check
        // loads the settings.
        crate::invalidate::testing::applied(&test.app, async {
            crate::store::settings::set(
                test.app.accounts().executor(),
                crate::store::settings::SettingKey::MaxKeysPerUser,
                "5",
                &crate::store::audit::AuditActor::cli(),
                chrono::Utc::now(),
            )
            .await
            .unwrap();
        })
        .await;

        let text = render(&registry);
        // No poll (M18 Task 16), and catalogs were never polled (Task 8).
        assert!(!text.contains("okf_poll_duration_seconds"), "{text}");
        assert!(!text.contains("okf_catalog_refreshes_total"), "{text}");
        assert!(
            text.contains(r#"okf_keycache_reloads_total{outcome="ok"} 1"#),
            "{text}"
        );
    }

    async fn body(ctx: &ToolContext, handle: &str, path: &str, text: &str) -> WriteReport {
        let params = ReplaceBodyParams {
            path: path.to_string(),
            body: text.to_string(),
            changeset: handle.to_string(),
        };
        replace_body(ctx, params).await.unwrap()
    }

    /// A `log` write, through the write path as Task 9's tool makes it.
    async fn log_entry(ctx: &ToolContext, handle: &str, text: &str) -> WriteReport {
        let text = text.to_string();
        apply(ctx, handle, None, move |workspace, op| {
            ops::log(workspace, op, LogKind::Update, &text)
        })
        .await
        .unwrap()
    }

    /// An `index` write, through the write path as Task 9's tool makes it.
    async fn list(ctx: &ToolContext, handle: &str, path: &str, summary: &str) -> WriteReport {
        let (path, summary) = (path.to_string(), summary.to_string());
        apply(ctx, handle, None, move |workspace, op| {
            ops::index(workspace, op, &path, &summary, None, None)
        })
        .await
        .unwrap()
    }

    async fn publish_as(
        ctx: &ToolContext,
        handle: &str,
        message: &str,
    ) -> Result<Published, OkfError> {
        let params = PublishParams {
            message: message.to_string(),
            changeset: handle.to_string(),
        };
        publish(ctx, params).await
    }

    async fn published_text(test: &TestApp, path: &str) -> String {
        let loaded = test.app.catalog("pg").await.unwrap().unwrap().current();
        loaded
            .snapshot()
            .get(path)
            .map(|doc| doc.text().to_string())
            .unwrap_or_default()
    }

    async fn state_of(test: &TestApp, handle: &str) -> ChangesetState {
        changeset_row(test, handle).await.state
    }

    /// Stores an entry directly, as no write tool would (a rule changed, a
    /// hand edit), to show publish checks again.
    async fn insert_entry(test: &TestApp, handle: &str, entry: EntryRow) {
        let state = test.app.catalog("pg").await.unwrap().unwrap();
        let row = changeset_row(test, handle).await;
        let mut tx = test.app.main_db().await.begin().await.unwrap();
        changesets::lock_catalog(&mut tx, state.id()).await.unwrap();
        changesets::upsert_entry(&mut tx, row.id, &entry, Utc::now())
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    /// Waits until `count` backends of this database wait on a lock.
    async fn lock_waiters(test: &TestApp, count: i64) {
        for _ in 0..500 {
            let waiting: i64 = sqlx::query_scalar(concat!(
                "SELECT count(*) FROM pg_stat_activity WHERE ",
                crate::test_locks::own_databases!("datname"),
                " AND wait_event_type = 'Lock'"
            ))
            .fetch_one(test.app.accounts().executor())
            .await
            .unwrap();
            if waiting >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{count} lock waiter(s) never appeared");
    }

    /// A publish whose changeset closed after the call resolved it (another
    /// publish won the race) is refused under the lock, naming the handle,
    /// never the internal id clients don't see.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_publish_of_a_changeset_closed_meanwhile_names_its_handle() {
        let test = pg().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;
        body(&ctx, &handle, "/a.md", "# A\n\nNew.\n").await;
        // Resolved while open, as the losing publish read it before the lock.
        let stale = changeset_row(&test, &handle).await;
        let state = test.app.catalog("pg").await.unwrap().unwrap();
        publish_as(&ctx, &handle, "first").await.unwrap();

        let input = crate::store::publish::PublishInput {
            changeset: &stale,
            actor: "a",
            message: "second",
        };
        let refused = crate::store::publish::publish(&test.app, &state, &input, &ctx.audit_actor())
            .await
            .unwrap_err();

        assert_eq!(refused.kind(), "Refused");
        let message = refused.to_string();
        assert!(message.contains(&handle), "{message}");
        assert!(
            !message.contains(&format!("changeset {} ", stale.id)),
            "{message}"
        );
    }

    /// A publish sends its catalog's new generation on the change feed
    /// (milestone 18 Task 2), without bumping `admin_generation`; a publish
    /// that writes nothing sends nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_publish_invalidates_its_catalog_at_the_new_generation() {
        use crate::invalidate::{testing::invalidations_of, Invalidation};
        let test = pg().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let pool = test.app.accounts().executor();
        let catalog = catalogs::find(pool, "pg").await.unwrap();
        let admin_before = catalogs::admin_generation(pool).await.unwrap();
        let handle = open(&ctx).await;
        body(&ctx, &handle, "/a.md", "# A\n\nNew.\n").await;

        let got = invalidations_of(pool, async {
            publish_as(&ctx, &handle, "edit a").await.unwrap();
        })
        .await;
        let empty = open(&ctx).await;
        let nothing = invalidations_of(pool, async {
            publish_as(&ctx, &empty, "nothing").await.unwrap();
        })
        .await;

        assert_eq!(
            got,
            [Invalidation::Catalog {
                id: catalog.id,
                generation: catalogs::test_generation(pool, "pg").await,
            }]
        );
        assert_eq!(nothing, []);
        assert_eq!(
            catalogs::admin_generation(pool).await.unwrap(),
            admin_before,
            "a publish is not an administrative change"
        );
    }

    /// Issue #970: the counter a publish bumps is the store's
    /// `catalog_state` row, read on the store's own pool; the registry row
    /// has no generation any more.
    #[tokio::test(flavor = "multi_thread")]
    async fn publish_bumps_generation_in_catalog_state() {
        let test = pg().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let state = test.app.catalog("pg").await.unwrap().unwrap();
        let on_store = || async {
            sqlx::query_scalar::<_, i64>("SELECT generation FROM catalog_state WHERE id = $1")
                .bind(state.id())
                .fetch_one(test.app.main_db().await.executor())
                .await
                .unwrap()
        };
        let before = on_store().await;
        let handle = open(&ctx).await;
        body(&ctx, &handle, "/a.md", "# A\n\nNew.\n").await;

        publish_as(&ctx, &handle, "edit a").await.unwrap();

        assert_eq!(on_store().await, before + 1);
        let registry_columns: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM information_schema.columns \
             WHERE table_schema = 'public' AND table_name = 'catalogs' \
               AND column_name = 'generation'",
        )
        .fetch_one(test.app.accounts().executor())
        .await
        .unwrap();
        assert_eq!(registry_columns, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn publish_writes_revisions_bumps_the_generation_and_swaps_the_snapshot() {
        let test = pg().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;
        let state = test.app.catalog("pg").await.unwrap().unwrap();
        let before = catalogs::test_generation(test.app.accounts().executor(), "pg").await;
        let written = body(&ctx, &handle, "/a.md", "# A\n\nNew.\n").await;

        let published = publish_as(&ctx, &handle, "edit a").await.unwrap();

        assert!(published.report.published);
        assert_eq!(published.changeset, written.changeset);
        assert_eq!(
            published.report.revisions,
            [PublishedRevision {
                path: "/a.md".to_string(),
                revision: 2,
                op: "update"
            }]
        );
        assert!(published.report.replayed.is_empty());
        let after = catalogs::test_generation(test.app.accounts().executor(), "pg").await;
        assert_eq!(after, before + 1);
        assert_eq!(state.current().generation(), after);
        assert!(published_text(&test, "a.md")
            .await
            .ends_with("# A\n\nNew.\n"));
        let rows = history::revisions(&test.app.main_db().await, state.id(), "a.md", 5)
            .await
            .unwrap();
        assert_eq!(
            (
                rows[0].revision,
                rows[0].op.as_str(),
                rows[0].actor.as_str(),
                rows[0].message.as_str()
            ),
            (2, "update", "writer-a", "edit a")
        );
        let changeset_id: Option<i64> = sqlx::query_scalar(
            "SELECT changeset_id FROM catalog.revisions WHERE path = 'a.md' AND revision = 2",
        )
        .fetch_one(&crate::db::store_pool(test.app.accounts().executor()))
        .await
        .unwrap();
        assert_eq!(changeset_id, Some(changeset_row(&test, &handle).await.id));
        let updated_by: String =
            sqlx::query_scalar("SELECT updated_by FROM catalog.documents WHERE path = 'a.md'")
                .fetch_one(&crate::db::store_pool(test.app.accounts().executor()))
                .await
                .unwrap();
        assert_eq!(updated_by, "writer-a");
        assert_eq!(state_of(&test, &handle).await, ChangesetState::Published);
        // The handle is spent: reading or writing through it is refused.
        let spent = ctx.pin(Some(&handle)).await.unwrap_err();
        assert_eq!(spent.kind(), "Refused");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_publish_records_one_entry_with_the_handle_files_and_generations() {
        use crate::store::audit::testing::entries;
        let test = pg().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;
        body(&ctx, &handle, "/a.md", "# A\n\nNew.\n").await;
        body(&ctx, &handle, "/infra/x.md", "# X\n\nNew.\n").await;
        // A changeset whose only edit is the published text (a revert to
        // the current revision): its publish writes no revision, so it is
        // no catalog write.
        let unchanged = open(&ctx).await;
        let same = RevertParams {
            path: "/infra/y.md".to_string(),
            revision: 1,
            expected_revision: None,
            changeset: unchanged.clone(),
        };
        revert(&ctx, same).await.unwrap();

        publish_as(&ctx, &handle, "two edits").await.unwrap();
        let noop = publish_as(&ctx, &unchanged, "nothing").await.unwrap();

        assert!(noop.report.revisions.is_empty(), "{noop:?}");
        let recorded = entries(test.app.accounts().executor(), "catalog.published").await;
        assert_eq!(recorded.len(), 1, "{recorded:?}");
        let entry = &recorded[0];
        assert_eq!(entry.target.as_deref(), Some("pg"));
        assert_eq!(entry.teams, [crate::db::TEST_TEAM]);
        assert_eq!(entry.details["changeset"], handle.as_str());
        assert_eq!(entry.details["files"], 2);
        let generation: i64 =
            crate::store::catalogs::test_generation(test.app.accounts().executor(), "pg").await;
        assert_eq!(entry.details["generation_to"], generation);
        assert_eq!(entry.details["generation_from"], generation - 1);
        assert_eq!(
            (entry.source.as_str(), entry.actor_label.as_str()),
            ("api", "api key")
        );
        assert!(!entry.actor_operator, "a write key is not an operator");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_audit_write_fails_the_publish() {
        use crate::http::api::testing::fail_on;
        let test = pg().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;
        body(&ctx, &handle, "/a.md", "# A\n\nNew.\n").await;
        fail_on(
            test.app.accounts().executor(),
            "BEFORE INSERT ON catalog.catalog_audit_entries FOR EACH ROW",
        )
        .await;

        let refused = publish_as(&ctx, &handle, "edit a").await;

        assert!(refused.is_err(), "{refused:?}");
        assert_eq!(state_of(&test, &handle).await, ChangesetState::Open);
        assert_eq!(
            published_text(&test, "a.md").await,
            NOTE,
            "the publish rolled back"
        );
        let state = test.app.catalog("pg").await.unwrap().unwrap();
        let rows = history::revisions(&test.app.main_db().await, state.id(), "a.md", 5)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "no revision was written");
    }

    /// The count of rows in the account's and the store's audit tables for
    /// `catalog.published`.
    async fn published_entries(test: &TestApp) -> (i64, i64) {
        let pool = test.app.accounts().executor();
        let account = sqlx::query_scalar(
            "SELECT count(*) FROM audit_entries WHERE action = 'catalog.published'",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        let store = sqlx::query_scalar(
            "SELECT count(*) FROM catalog.catalog_audit_entries WHERE action = 'catalog.published'",
        )
        .fetch_one(&crate::db::store_pool(pool))
        .await
        .unwrap();
        (account, store)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_publish_writes_its_audit_entry_on_the_store() {
        let test = pg().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;
        body(&ctx, &handle, "/a.md", "# A\n\nNew.\n").await;

        publish_as(&ctx, &handle, "edit a").await.unwrap();

        assert_eq!(
            published_entries(&test).await,
            (0, 1),
            "the entry is on the store and none is written to the account database"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_publish_writes_no_audit_entry() {
        use crate::http::api::testing::fail_on;
        let test = pg().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;
        body(&ctx, &handle, "/a.md", "# A\n\nNew.\n").await;
        // The entry is written before the changeset is marked published, so
        // a failure there rolls the entry back with the revisions.
        fail_on(
            test.app.accounts().executor(),
            "BEFORE UPDATE ON catalog.changesets FOR EACH ROW",
        )
        .await;

        let refused = publish_as(&ctx, &handle, "edit a").await;

        assert!(refused.is_err(), "{refused:?}");
        assert_eq!(published_entries(&test).await, (0, 0));
        assert_eq!(state_of(&test, &handle).await, ChangesetState::Open);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_admin_may_publish_another_key_s_changeset() {
        let test = pg().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;
        body(&ctx, &handle, "/a.md", "# A\n\nBy a.\n").await;
        store_key(&test, "root").await;
        let admin = context(
            &test,
            principal_as("root", Access::Admin, &[], Some("pg")),
            None,
        );

        let published = publish_as(&admin, &handle, "take over").await.unwrap();

        assert!(published.report.published);
        assert_eq!(state_of(&test, &handle).await, ChangesetState::Published);
        let state = test.app.catalog("pg").await.unwrap().unwrap();
        let rows = history::revisions(&test.app.main_db().await, state.id(), "a.md", 5)
            .await
            .unwrap();
        // The publishing key's actor, not the opener's.
        assert_eq!(
            (rows[0].actor.as_str(), rows[0].message.as_str()),
            ("root", "take over")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_published_changeset_is_no_longer_listed() {
        let test = pg().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let published = open(&ctx).await;
        let kept = open(&ctx).await;
        body(&ctx, &published, "/a.md", "# A\n\nNew.\n").await;
        let all = || ListParams { catalog: None };
        let before = list_changesets(&ctx, all()).await.unwrap();

        publish_as(&ctx, &published, "edit a").await.unwrap();
        let after = list_changesets(&ctx, all()).await.unwrap();

        let listed = |listed: &Listed| -> Vec<String> {
            listed
                .changesets
                .iter()
                .map(|changeset| changeset.changeset.clone())
                .collect()
        };
        let mut both = listed(&before);
        both.sort_unstable();
        let mut expected = vec![published.clone(), kept.clone()];
        expected.sort_unstable();
        assert_eq!(both, expected);
        assert_eq!(listed(&after), [kept]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nothing_to_publish_is_a_result_and_a_message_is_required() {
        let test = pg().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;
        let before = catalogs::test_generation(test.app.accounts().executor(), "pg").await;

        let nothing = publish_as(&ctx, &handle, "nothing").await.unwrap();
        let blank = publish_as(&ctx, &handle, "  ").await.unwrap_err();
        // The message check comes first, so even a made-up handle gets it.
        let blank_unknown = publish_as(&ctx, "made-up", " ").await.unwrap_err();
        let unknown = publish_as(&ctx, "made-up", "x").await.unwrap_err();

        assert!(!nothing.report.published);
        assert_eq!(nothing.changeset, handle);
        assert_eq!(state_of(&test, &handle).await, ChangesetState::Open);
        assert_eq!(blank_unknown.kind(), "Refused");
        assert_eq!(unknown.kind(), "UnknownChangeset");
        assert_eq!(blank.kind(), "Refused");
        let after = catalogs::test_generation(test.app.accounts().executor(), "pg").await;
        assert_eq!(after, before);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_publish_on_a_stale_snapshot_needs_one_connection() {
        // Arrange: an import moves the database past the server's snapshot
        // (no refresh), and every pooled connection but one is taken.
        let test = pg().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;
        body(&ctx, &handle, "/a.md", "Edited.\n").await;
        let options = ImportOptions {
            replace: true,
            scope: ImportScope::Everything,
            expect_generation: None,
            expect_catalog_id: None,
            actor: "t",
            audit: &crate::store::audit::AuditActor::cli(),
            now: Utc::now(),
        };
        let republished = files(&[
            ("a.md", NOTE),
            ("hub.md", HUB),
            ("infra/index.md", "# Infra\n"),
            ("infra/x.md", X),
            ("infra/y.md", Y),
            ("log.md", "# Log\n"),
            ("z.md", "# Z\n"),
        ]);
        import(&test.app.main_db().await, "pg", republished, &options)
            .await
            .unwrap();
        let state = test.app.catalog("pg").await.unwrap().unwrap();
        let stale = state.current().generation();
        let size = test
            .app
            .accounts()
            .executor()
            .options()
            .get_max_connections();
        let mut held = Vec::with_capacity(size as usize);
        for _ in 1..size {
            held.push(test.app.accounts().executor().acquire().await.unwrap());
        }

        // Act: well inside sqlx's 30 s acquire timeout, which a publish
        // waiting for a second connection would run into.
        let published = tokio::time::timeout(
            Duration::from_secs(10),
            publish_as(&ctx, &handle, "on a stale snapshot"),
        )
        .await
        .expect("the publish needs no second connection");
        drop(held);

        // Assert
        assert!(published.unwrap().report.published);
        assert!(state.current().generation() > stale);
        assert!(state.current().snapshot().get("z.md").is_some());
        assert!(published_text(&test, "a.md").await.ends_with("Edited.\n"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stale_free_edit_conflicts_and_leaves_the_changeset_open() {
        let test = pg().await;
        let first = writer(&test, "writer-a", "pg").await;
        let second = writer(&test, "writer-b", "pg").await;
        let (first_handle, mine) = (open(&first).await, open(&second).await);
        body(&first, &first_handle, "/a.md", "From a.\n").await;
        body(&second, &mine, "/a.md", "From b.\n").await;
        publish_as(&first, &first_handle, "a first").await.unwrap();

        let error = publish_as(&second, &mine, "b second").await.unwrap_err();

        assert!(matches!(
            error,
            OkfError::Conflict { ref files } if files == &[ConflictFile {
                path: "/a.md".to_string(),
                base_revision: Some(1),
                current_revision: Some(2),
            }]
        ));
        assert_eq!(state_of(&test, &mine).await, ChangesetState::Open);
        let entries = changesets::entries(
            test.app.main_db().await.executor(),
            changeset_row(&test, &mine).await.id,
        )
        .await
        .unwrap();
        assert_eq!(entries.len(), 1);
        assert!(published_text(&test, "a.md").await.ends_with("From a.\n"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_log_entries_both_publish_and_keep_both_bullets() {
        let test = pg().await;
        let first = writer(&test, "writer-a", "pg").await;
        let second = writer(&test, "writer-b", "pg").await;
        let (a, b) = (open(&first).await, open(&second).await);
        log_entry(&first, &a, "from a").await;
        log_entry(&second, &b, "from b").await;

        // Both at once: the publish lock runs them one after the other, and
        // the second replays its entry onto the first's log.md.
        let (one, two) = tokio::join!(publish_as(&first, &a, "a"), publish_as(&second, &b, "b"));

        let (one, two) = (one.unwrap(), two.unwrap());
        assert!(one.report.published && two.report.published);
        let replayed: Vec<&String> = one
            .report
            .replayed
            .iter()
            .chain(&two.report.replayed)
            .collect();
        assert_eq!(replayed, ["/log.md"]);
        let text = published_text(&test, "log.md").await;
        assert!(
            text.contains("* **Update:** from a") && text.contains("* **Update:** from b"),
            "{text}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stale_index_listing_is_replayed() {
        let test = pg().await;
        let first = writer(&test, "writer-a", "pg").await;
        let second = writer(&test, "writer-b", "pg").await;
        let (a, b) = (open(&first).await, open(&second).await);
        list(&first, &a, "/infra/x.md", "the x").await;
        list(&second, &b, "/infra/y.md", "the y").await;
        publish_as(&first, &a, "list x").await.unwrap();

        let published = publish_as(&second, &b, "list y").await.unwrap();

        assert_eq!(published.report.replayed, ["/infra/index.md"]);
        let text = published_text(&test, "infra/index.md").await;
        assert!(
            text.contains("[X](x.md) - the x") && text.contains("[Y](y.md) - the y"),
            "{text}"
        );
    }

    /// `pending_changes` on `handle`.
    async fn shown(ctx: &ToolContext, handle: &str) -> PendingChanges {
        let params = PendingParams {
            changeset: handle.to_string(),
        };
        pending_changes(ctx, params).await.unwrap()
    }

    /// The `+` and `-` lines of a unified diff, without its two headers.
    fn changed_lines(diff: &str) -> Vec<&str> {
        diff.lines()
            .filter(|line| !line.starts_with("+++ ") && !line.starts_with("--- "))
            .filter(|line| line.starts_with('+') || line.starts_with('-'))
            .collect()
    }

    /// The replayable bookkeeping write a test makes.
    #[derive(Clone, Copy)]
    enum Bookkeeping {
        /// A `log` entry (log.md).
        Log,
        /// An `index` listing of `/infra/<name>.md` (infra/index.md).
        Listing,
    }

    impl Bookkeeping {
        /// The path the write changes.
        fn path(self) -> &'static str {
            match self {
                Bookkeeping::Log => "log.md",
                Bookkeeping::Listing => "infra/index.md",
            }
        }

        /// Writes `name`'s entry in `handle`; returns the text it adds.
        async fn write(self, ctx: &ToolContext, handle: &str, name: &str) -> String {
            match self {
                Bookkeeping::Log => {
                    let text = format!("from {name}");
                    log_entry(ctx, handle, &text).await;
                    text
                }
                Bookkeeping::Listing => {
                    let summary = format!("the {name}");
                    list(ctx, handle, &format!("/infra/{name}.md"), &summary).await;
                    summary
                }
            }
        }
    }

    /// A stale, replayable entry in `a` (another changeset published its
    /// path since) is shown as publish writes it: one `+` line for this
    /// entry, and nothing published since shown as deleted (issue #787).
    /// Publishing then writes exactly the diff shown.
    async fn a_stale_replayable_entry_is_shown_as_publish_writes_it(kind: Bookkeeping) {
        let test = pg().await;
        let first = writer(&test, "writer-a", "pg").await;
        let second = writer(&test, "writer-b", "pg").await;
        let (a, b) = (open(&first).await, open(&second).await);
        let ours = kind.write(&first, &a, "x").await;
        let theirs = kind.write(&second, &b, "y").await;
        publish_as(&second, &b, "theirs").await.unwrap();
        let path = kind.path();
        let before = published_text(&test, path).await;
        assert!(before.contains(&theirs), "{before}");

        let pending = shown(&first, &a).await;

        let file = &pending.files[0];
        let expected_path = format!("/{path}");
        assert_eq!(
            (
                file.path.as_str(),
                file.stale,
                file.replayable,
                file.replayed,
                file.replay_error.as_deref()
            ),
            (expected_path.as_str(), true, true, true, None)
        );
        let changed = changed_lines(&file.diff);
        assert_eq!(changed.len(), 1, "{}", file.diff);
        assert!(
            changed[0].starts_with('+') && changed[0].contains(&ours),
            "{}",
            file.diff
        );
        publish_as(&first, &a, "ours").await.unwrap();
        let after = published_text(&test, path).await;
        assert!(after.contains(&ours) && after.contains(&theirs), "{after}");
        assert_eq!(
            file.diff,
            unified_diff(path, Some(before.as_bytes()), Some(after.as_bytes()))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stale_log_entry_is_shown_replayed_without_deleting_one_published_since() {
        a_stale_replayable_entry_is_shown_as_publish_writes_it(Bookkeeping::Log).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stale_index_listing_is_shown_replayed_without_deleting_one_published_since() {
        a_stale_replayable_entry_is_shown_as_publish_writes_it(Bookkeeping::Listing).await;
    }

    /// The revision of `path` in this server's snapshot.
    async fn published_revision(test: &TestApp, path: &str) -> Option<i32> {
        let state = test.app.catalog("pg").await.unwrap().unwrap();
        state.current().revision(path)
    }

    /// The entry `handle` holds for `path` (`path` without its `/`).
    async fn entry_for(test: &TestApp, handle: &str, path: &str) -> Option<EntryRow> {
        let row = changeset_row(test, handle).await;
        let entries = changesets::entries(test.app.main_db().await.executor(), row.id)
            .await
            .unwrap();
        entries.into_iter().find(|entry| entry.path == path)
    }

    /// `revert` of `path` to `revision` in `handle`.
    async fn revert_in(ctx: &ToolContext, handle: &str, path: &str, revision: i32) -> WriteReport {
        let params = RevertParams {
            path: path.to_string(),
            revision,
            expected_revision: None,
            changeset: handle.to_string(),
        };
        revert(ctx, params).await.unwrap()
    }

    /// A concept for a.md whose body is the heading `title`.
    fn note(title: &str) -> String {
        format!("---\ntype: Note\ntitle: A\ndescription: A note.\n---\n# {title}\n")
    }

    /// Publishes, in a changeset of its own, `text` at `path` (`None`
    /// deletes it), as another writer would.
    async fn publish_file(ctx: &ToolContext, path: &str, text: Option<&str>) {
        let handle = open(ctx).await;
        let (path, text) = (path.to_string(), text.map(str::to_string));
        apply(ctx, &handle, None, move |workspace, _| {
            match &text {
                Some(text) => workspace.write_atomic(&path, text)?,
                None => {
                    workspace.remove(&path)?;
                }
            }
            let mut outcome = OpOutcome::default();
            outcome.touch(&path);
            Ok(outcome)
        })
        .await
        .unwrap();
        publish_as(ctx, &handle, "another writer").await.unwrap();
    }

    /// The issue's sequence: a `log` entry, two publishes of log.md by
    /// others, a revert to the revision published now and the same `log`
    /// again. The entry is based on that revision, is not stale, shows one
    /// added line, and publishes it beside both others' lines (issue #788).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_revert_to_the_published_revision_rebases_a_stale_entry_so_it_publishes() {
        let test = pg().await;
        let ours = writer(&test, "writer-a", "pg").await;
        let theirs = writer(&test, "writer-b", "pg").await;
        let handle = open(&ours).await;
        log_entry(&ours, &handle, "from a").await;
        let first_base = published_revision(&test, "log.md").await;
        for text in ["from b one", "from b two"] {
            let other = open(&theirs).await;
            log_entry(&theirs, &other, text).await;
            publish_as(&theirs, &other, text).await.unwrap();
        }
        let now = published_revision(&test, "log.md").await.unwrap();
        assert_eq!(first_base.map(|base| base + 2), Some(now));
        let before = published_text(&test, "log.md").await;
        assert!(shown(&ours, &handle).await.files[0].stale);

        revert_in(&ours, &handle, "/log.md", now).await;
        log_entry(&ours, &handle, "from a").await;

        let entry = entry_for(&test, &handle, "log.md").await.unwrap();
        // Based on the revision the revert read; the revert is a free edit,
        // so the `log` after it adds no replay to it.
        assert_eq!((entry.base_revision, entry.replay), (Some(now), None));
        let pending = shown(&ours, &handle).await;
        let file = &pending.files[0];
        assert_eq!(
            (
                file.base_revision,
                file.current_revision,
                file.stale,
                file.replayable,
                file.replayed,
                file.replay_error.as_deref()
            ),
            (Some(now), Some(now), false, false, false, None)
        );
        let changed = changed_lines(&file.diff);
        assert_eq!(changed.len(), 1, "{}", file.diff);
        assert!(
            changed[0].starts_with('+') && changed[0].contains("from a"),
            "{}",
            file.diff
        );
        let published = publish_as(&ours, &handle, "ours").await.unwrap();
        assert!(published.report.published);
        let after = published_text(&test, "log.md").await;
        assert!(
            ["from a", "from b one", "from b two"]
                .iter()
                .all(|text| after.contains(&format!("* **Update:** {text}"))),
            "{after}"
        );
        assert_eq!(
            file.diff,
            unified_diff("log.md", Some(before.as_bytes()), Some(after.as_bytes()))
        );
    }

    /// A revert to an older revision of a path the changeset had edited is
    /// a whole-file choice based on the revision published now: it publishes
    /// that revision's text over the later ones. Another revert, then a
    /// publish by someone else, conflicts on the revision the revert read.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_revert_to_an_older_revision_conflicts_only_on_a_publish_after_it() {
        let test = pg().await;
        let ours = writer(&test, "writer-a", "pg").await;
        let theirs = writer(&test, "writer-b", "pg").await;
        let (handle, later) = (open(&ours).await, open(&ours).await);
        body(&ours, &handle, "/a.md", "# A\n\nOurs.\n").await;
        body(&ours, &later, "/a.md", "# A\n\nLater.\n").await;
        publish_file(&theirs, "a.md", Some(&note("Two"))).await;
        publish_file(&theirs, "a.md", Some(&note("Three"))).await;
        let three = published_revision(&test, "a.md").await;

        revert_in(&ours, &handle, "/a.md", 1).await;
        let entry = entry_for(&test, &handle, "a.md").await.unwrap();
        assert_eq!(
            (entry.base_revision, entry.content.as_deref()),
            (three, Some(NOTE.as_bytes()))
        );
        publish_as(&ours, &handle, "back to one").await.unwrap();
        assert_eq!(published_text(&test, "a.md").await, NOTE);

        // `later` was based on revision 1 too; reverting it reads revision
        // 4 (the one just published), and a publish after that conflicts.
        let four = published_revision(&test, "a.md").await;
        revert_in(&ours, &later, "/a.md", 2).await;
        publish_file(&theirs, "a.md", Some(&note("Five"))).await;
        let five = published_revision(&test, "a.md").await;
        let error = publish_as(&ours, &later, "back to two").await.unwrap_err();
        assert!(
            matches!(
                error,
                OkfError::Conflict { ref files } if files == &[ConflictFile {
                    path: "/a.md".to_string(),
                    base_revision: four,
                    current_revision: five,
                }]
            ),
            "{error:?}"
        );
    }

    /// `revert`'s `expected_revision` (issue #795): a matching value
    /// succeeds, a stale one is refused with a Conflict naming the revision
    /// now and writes nothing, and none behaves as before.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_revert_with_an_expected_revision_refuses_a_stale_one_and_writes_nothing() {
        let test = pg().await;
        let ours = writer(&test, "writer-a", "pg").await;
        let theirs = writer(&test, "writer-b", "pg").await;
        let (stale, fresh, plain) = (open(&ours).await, open(&ours).await, open(&ours).await);
        publish_file(&theirs, "a.md", Some(&note("Two"))).await;
        // What the caller's `history` showed.
        let seen = published_revision(&test, "a.md").await;
        assert_eq!(seen, Some(2));
        let shown = |limit, path: &str| crate::mcp::history::HistoryParams {
            path: path.to_string(),
            catalog: None,
            limit,
            user: None,
            since: None,
        };
        // `history` shows it, even when `limit` leaves the newest rows out.
        let live = crate::mcp::history::history(&ours, shown(Some(1), "/a.md"))
            .await
            .unwrap();
        assert_eq!(live.current_revision, seen);
        // Someone else publishes between that read and the revert.
        publish_file(&theirs, "a.md", Some(&note("Three"))).await;
        let now = published_revision(&test, "a.md").await;

        let with = |handle: &str, expected| RevertParams {
            path: "/a.md".to_string(),
            revision: 1,
            expected_revision: expected,
            changeset: handle.to_string(),
        };
        let error = revert(&ours, with(&stale, seen)).await.unwrap_err();
        assert!(
            matches!(
                error,
                OkfError::Conflict { ref files } if files == &[ConflictFile {
                    path: "/a.md".to_string(),
                    base_revision: seen,
                    current_revision: now,
                }]
            ),
            "{error:?}"
        );
        assert!(entry_for(&test, &stale, "a.md").await.is_none());
        assert_eq!(published_text(&test, "a.md").await, note("Three"));

        let report = revert(&ours, with(&fresh, now)).await.unwrap();
        assert_eq!(report.touched, ["/a.md"]);
        let entry = entry_for(&test, &fresh, "a.md").await.unwrap();
        assert_eq!(
            (entry.base_revision, entry.content.as_deref()),
            (now, Some(NOTE.as_bytes()))
        );

        // A deleted path has no live revision.
        publish_file(&theirs, "a.md", None).await;
        let gone = crate::mcp::history::history(&ours, shown(None, "/a.md"))
            .await
            .unwrap();
        assert_eq!(gone.current_revision, None);

        // No argument: unchanged behaviour.
        revert(&ours, with(&plain, None)).await.unwrap();
        assert!(entry_for(&test, &plain, "a.md").await.is_some());
    }

    /// `revert`'s `expected_revision` on a deleted path (issue #1127):
    /// `history`'s `latest_revision` is the deletion's number, the revert
    /// with it succeeds, and a re-create in between makes the same call
    /// refuse with a Conflict and write nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_revert_onto_a_deleted_path_is_guarded_by_the_latest_revision() {
        let test = pg().await;
        let ours = writer(&test, "writer-a", "pg").await;
        let theirs = writer(&test, "writer-b", "pg").await;
        let (fresh, stale) = (open(&ours).await, open(&ours).await);
        publish_file(&theirs, "a.md", None).await;
        let shown = crate::mcp::history::history(
            &ours,
            crate::mcp::history::HistoryParams {
                path: "/a.md".to_string(),
                catalog: None,
                limit: None,
                user: None,
                since: None,
            },
        )
        .await
        .unwrap();
        // The deletion is revision 2; the path has no live revision.
        assert_eq!(
            (shown.current_revision, shown.latest_revision),
            (None, Some(2))
        );
        let with = |handle: &str| RevertParams {
            path: "/a.md".to_string(),
            revision: 1,
            expected_revision: shown.latest_revision,
            changeset: handle.to_string(),
        };

        revert(&ours, with(&fresh)).await.unwrap();
        assert!(entry_for(&test, &fresh, "a.md").await.is_some());

        // Someone re-creates the path between `history` and the revert.
        publish_file(&theirs, "a.md", Some(&note("Again"))).await;
        let now = published_revision(&test, "a.md").await;
        assert_eq!(now, Some(3));
        let error = revert(&ours, with(&stale)).await.unwrap_err();
        assert!(
            matches!(
                error,
                OkfError::Conflict { ref files } if files == &[ConflictFile {
                    path: "/a.md".to_string(),
                    base_revision: Some(2),
                    current_revision: now,
                }]
            ),
            "{error:?}"
        );
        assert!(entry_for(&test, &stale, "a.md").await.is_none());
        assert_eq!(published_text(&test, "a.md").await, note("Again"));
    }

    /// A revert to a `delete` revision of a path the changeset had edited
    /// takes the revision published now as its base too: a deletion of a
    /// file published since, or no entry once the file is gone already.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_revert_to_a_deletion_rebases_or_drops_the_entry() {
        let test = pg().await;
        let ours = writer(&test, "writer-a", "pg").await;
        let theirs = writer(&test, "writer-b", "pg").await;
        let (gone, back) = (open(&ours).await, open(&ours).await);
        body(&ours, &gone, "/a.md", "# A\n\nOurs.\n").await;
        body(&ours, &back, "/a.md", "# A\n\nOurs.\n").await;
        publish_file(&theirs, "a.md", None).await;
        // The deletion is a.md's revision 2.
        let deleted = 2;

        // Deleted since: nothing to delete, so the entry goes.
        let report = revert_in(&ours, &gone, "/a.md", deleted).await;
        assert!(report.touched.is_empty(), "{report:?}");
        assert!(entry_for(&test, &gone, "a.md").await.is_none());

        // Created again since: a deletion based on that revision, which
        // publishes.
        publish_file(&theirs, "a.md", Some(&note("Again"))).await;
        let again = published_revision(&test, "a.md").await;
        revert_in(&ours, &back, "/a.md", deleted).await;
        let entry = entry_for(&test, &back, "a.md").await.unwrap();
        assert_eq!((entry.base_revision, entry.content), (again, None));
        publish_as(&ours, &back, "delete again").await.unwrap();
        assert_eq!(published_revision(&test, "a.md").await, None);
    }

    /// A revert on a snapshot this server has not refreshed (here after an
    /// import, which refreshes nothing) refreshes it first, so the entry's
    /// base is the revision published now and publish writes the revert over
    /// it instead of reporting a conflict on a base that was only ever out
    /// of date (issue #672).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_revert_on_a_stale_snapshot_refreshes_and_takes_the_current_base() {
        let test = pg().await;
        let ours = writer(&test, "writer-a", "pg").await;
        let theirs = writer(&test, "writer-b", "pg").await;
        let handle = open(&ours).await;
        // Based on revision 1; revision 2 is published, and seen, after it.
        body(&ours, &handle, "/a.md", "# A\n\nOurs.\n").await;
        publish_file(&theirs, "a.md", Some(&note("Two"))).await;
        let options = ImportOptions {
            replace: true,
            scope: ImportScope::Everything,
            expect_generation: None,
            expect_catalog_id: None,
            actor: "cli",
            audit: &crate::store::audit::AuditActor::cli(),
            now: Utc::now(),
        };
        let incoming = files(&[
            ("a.md", "# Imported\n"),
            ("hub.md", HUB),
            ("infra/index.md", "# Infra\n"),
            ("infra/x.md", X),
            ("infra/y.md", Y),
            ("log.md", "# Log\n"),
        ]);
        import(&test.app.main_db().await, "pg", incoming, &options)
            .await
            .unwrap();
        // The import wrote revision 3; this server's snapshot still has 2
        // (an import refreshes nothing, and the cached catalog directory row
        // still names the old generation, so no read here refreshes it).
        // The assertion on `seen` checks that premise.
        let seen = published_revision(&test, "a.md").await;
        assert_eq!(seen, Some(2));

        revert_in(&ours, &handle, "/a.md", 1).await;

        // Not the stale snapshot's 2, nor the first edit's 1: the revision
        // published now.
        let entry = entry_for(&test, &handle, "a.md").await.unwrap();
        assert_eq!(entry.base_revision, Some(3));
        publish_as(&ours, &handle, "revert").await.unwrap();
        assert_ne!(published_text(&test, "a.md").await, "# Imported\n");
    }

    /// An entry nobody published over keeps today's diff: its stored text
    /// against the published text, not marked replayed.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_fresh_replayable_entry_is_diffed_as_stored() {
        let test = pg().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;
        log_entry(&ctx, &handle, "from a").await;
        let published = published_text(&test, "log.md").await;
        let row = changeset_row(&test, &handle).await;
        let entries = changesets::entries(test.app.main_db().await.executor(), row.id)
            .await
            .unwrap();

        let pending = shown(&ctx, &handle).await;

        let file = &pending.files[0];
        assert_eq!(
            (
                file.stale,
                file.replayable,
                file.replayed,
                file.replay_error.as_deref()
            ),
            (false, true, false, None)
        );
        assert_eq!(
            file.diff,
            unified_diff(
                "log.md",
                Some(published.as_bytes()),
                entries[0].content.as_deref()
            )
        );
    }

    /// A stale entry whose replay publish would refuse as a conflict (its
    /// stored operations do not read) shows why, not a diff.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_stale_entry_publish_cannot_replay_shows_the_conflict_not_a_diff() {
        let test = pg().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;
        let unreadable = EntryRow {
            path: "a.md".to_string(),
            base_revision: Some(0),
            content: Some(b"# A\n".to_vec()),
            executable: false,
            replay: Some(serde_json::json!("not a list of operations")),
        };
        insert_entry(&test, &handle, unreadable).await;

        let pending = shown(&ctx, &handle).await;

        let file = &pending.files[0];
        assert_eq!(
            (file.stale, file.replayable, file.replayed),
            (true, true, false)
        );
        let reason = file.replay_error.as_deref().unwrap();
        assert!(
            reason.contains("refuse it with Conflict") && reason.contains("discard /a.md"),
            "{reason}"
        );
        assert_eq!(file.diff, format!("{reason}\n"));
        let error = publish_as(&ctx, &handle, "a").await.unwrap_err();
        assert_eq!(error.kind(), "Conflict");
    }

    /// A stale free edit (a path published since) carries `conflict_note`,
    /// and publish does refuse it with Conflict (issue #911); a fresh entry
    /// and a stale replayable one carry none.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_stale_free_edit_says_publish_will_refuse_it() {
        let test = pg().await;
        let first = writer(&test, "writer-a", "pg").await;
        let second = writer(&test, "writer-b", "pg").await;
        let seed = open(&first).await;
        body(&first, &seed, "/a.md", "# A\n").await;
        publish_as(&first, &seed, "seed").await.unwrap();
        let (ours, theirs) = (open(&first).await, open(&second).await);
        body(&first, &ours, "/a.md", "# A\n\nours\n").await;
        let fresh = shown(&first, &ours).await;
        assert_eq!(fresh.files[0].conflict_note, None);
        body(&second, &theirs, "/a.md", "# A\n\ntheirs\n").await;
        publish_as(&second, &theirs, "theirs").await.unwrap();

        let pending = shown(&first, &ours).await;

        let file = &pending.files[0];
        assert_eq!((file.stale, file.replayable), (true, false));
        let note = file.conflict_note.as_deref().unwrap();
        assert!(
            note.contains("refuse this changeset with Conflict")
                && note.contains("discarded")
                && note.contains("not what publish would write"),
            "{note}"
        );
        assert!(file.replay_error.is_none() && !file.diff.contains(note));
        let error = publish_as(&first, &ours, "ours").await.unwrap_err();
        assert_eq!(error.kind(), "Conflict");
    }

    /// A stale replayable entry, replayed or not, carries no
    /// `conflict_note`: its `replay_error` speaks for it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_stale_replayable_entry_carries_no_conflict_note() {
        let test = pg().await;
        let first = writer(&test, "writer-a", "pg").await;
        let second = writer(&test, "writer-b", "pg").await;
        let (a, b) = (open(&first).await, open(&second).await);
        log_entry(&first, &a, "from a").await;
        log_entry(&second, &b, "from b").await;
        publish_as(&second, &b, "theirs").await.unwrap();
        let replayed = shown(&first, &a).await;
        assert_eq!(
            (replayed.files[0].replayed, &replayed.files[0].conflict_note),
            (true, &None)
        );
        let unreadable = EntryRow {
            path: "log.md".to_string(),
            base_revision: Some(0),
            content: Some(b"# Stored log\n".to_vec()),
            executable: false,
            replay: Some(serde_json::json!("not a list of operations")),
        };
        insert_entry(&test, &a, unreadable).await;

        let failed = shown(&first, &a).await;

        assert!(failed.files[0].replay_error.is_some());
        assert_eq!(failed.files[0].conflict_note, None);
    }

    /// `read_document` of `/log.md` through `handle`.
    async fn read_log(ctx: &ToolContext, handle: &str) -> crate::mcp::documents::Document {
        let params = crate::mcp::documents::ReadDocumentParams {
            path: Some("/log.md".to_string()),
            catalog: None,
            changeset: Some(handle.to_string()),
        };
        crate::mcp::documents::read_document(ctx, params)
            .await
            .unwrap()
    }

    /// A read through a changeset whose `log` entry went stale shows the
    /// entry redone on the published text, with the entries published since
    /// (issue #792), also when an overlay cached before the publish is
    /// there. The stored entry, which a write builds on, is untouched.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_read_through_a_stale_log_entry_shows_what_publish_would_write() {
        let test = pg().await;
        let first = writer(&test, "writer-a", "pg").await;
        let second = writer(&test, "writer-b", "pg").await;
        let (a, b) = (open(&first).await, open(&second).await);
        log_entry(&first, &a, "from a").await;
        log_entry(&second, &b, "from b").await;
        let stored = entry_for(&test, &a, "log.md").await.unwrap().content;
        // Warm the overlay cache before the other changeset publishes.
        let fresh = read_log(&first, &a).await;
        assert!(!fresh.replayed && fresh.replay_error.is_none());
        assert!(fresh.text.contains("from a") && !fresh.text.contains("from b"));
        publish_as(&second, &b, "theirs").await.unwrap();

        let shown = read_log(&first, &a).await;

        assert!(shown.replayed && shown.replay_error.is_none());
        assert!(
            shown.text.contains("from a") && shown.text.contains("from b"),
            "{}",
            shown.text
        );
        assert_eq!(
            entry_for(&test, &a, "log.md").await.unwrap().content,
            stored
        );
        publish_as(&first, &a, "ours").await.unwrap();
        assert_eq!(published_text(&test, "log.md").await, shown.text);
    }

    /// A stale entry publish cannot replay is read as stored, with the
    /// reason `pending_changes` gives.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_read_through_an_entry_publish_cannot_replay_shows_its_stored_text_and_why() {
        let test = pg().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;
        let unreadable = EntryRow {
            path: "log.md".to_string(),
            base_revision: Some(0),
            content: Some(b"# Stored log\n".to_vec()),
            executable: false,
            replay: Some(serde_json::json!("not a list of operations")),
        };
        insert_entry(&test, &handle, unreadable).await;

        let shown = read_log(&ctx, &handle).await;

        assert!(!shown.replayed);
        let reason = shown.replay_error.as_deref().unwrap();
        assert!(reason.contains("refuse it with Conflict"), "{reason}");
        assert_eq!(shown.text, "# Stored log\n");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_lint_error_in_a_changed_file_is_refused() {
        let test = pg().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;
        body(&ctx, &handle, "/a.md", "See [gone](/gone.md).\n").await;

        let error = publish_as(&ctx, &handle, "broken").await.unwrap_err();

        // `&error`: the pattern borrows, so the `else` branch can still print it.
        let OkfError::LintFailed { errors, .. } = &error else {
            panic!("not LintFailed: {error:?}")
        };
        assert!(errors
            .iter()
            .any(|finding| finding.file == "a.md" && finding.rule == "broken-link"));
        assert_eq!(state_of(&test, &handle).await, ChangesetState::Open);
        assert_eq!(published_text(&test, "a.md").await, NOTE);
    }

    /// A catalog whose `log.md` and `old.md` hold a broken link already
    /// (text from before angle-bracket links parsed as links).
    async fn pg_with_old_broken_links() -> TestApp {
        let old = format!("{NOTE}See [x](<a b.md>).\n");
        let log = "# Log\n\n## 2026-11-01\n\n* **Update:** was ([x](<a b.md>), ...)\n";
        let files: &[(&str, &str)] = &[("a.md", NOTE), ("old.md", &old), ("log.md", log)];
        app_with(&[("pg", files)], Config::default()).await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_logged_publish_over_an_old_broken_link_in_log_md_reports_it_as_a_warning() {
        let test = pg_with_old_broken_links().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;
        log_entry(&ctx, &handle, "fine").await;

        let published = publish_as(&ctx, &handle, "log").await.unwrap();

        assert!(published.report.published);
        let text = published_text(&test, "log.md").await;
        assert!(text.contains("* **Update:** fine"), "{text}");
        assert!(
            published
                .report
                .warnings
                .iter()
                .any(|finding| finding.file == "log.md"
                    && finding.rule == "broken-link"
                    && finding.message.contains("a b.md")),
            "{:?}",
            published.report.warnings
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_new_broken_link_in_a_log_entry_is_still_refused_beside_an_old_one() {
        let test = pg_with_old_broken_links().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;
        // The same target as the old finding: one old plus one new is one new.
        log_entry(&ctx, &handle, "now [y](<a b.md>)").await;

        let error = publish_as(&ctx, &handle, "log").await.unwrap_err();

        let OkfError::LintFailed { errors, warnings } = &error else {
            panic!("not LintFailed: {error:?}")
        };
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(
            (errors[0].file.as_str(), errors[0].rule.as_str()),
            ("log.md", "broken-link")
        );
        assert!(warnings.iter().any(|finding| finding.rule == "broken-link"));
        assert_eq!(state_of(&test, &handle).await, ChangesetState::Open);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_old_broken_link_in_a_concept_is_a_warning_when_the_concept_is_edited() {
        let test = pg_with_old_broken_links().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;
        body(
            &ctx,
            &handle,
            "/old.md",
            "See [x](<a b.md>).\n\nMore text.\n",
        )
        .await;

        let published = publish_as(&ctx, &handle, "edit").await.unwrap();

        assert!(published.report.published);
        assert!(published
            .report
            .warnings
            .iter()
            .any(|finding| finding.file == "old.md" && finding.rule == "broken-link"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_deletion_still_linked_from_elsewhere_is_refused() {
        let test = pg().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;
        let deletion = EntryRow {
            path: "infra/x.md".to_string(),
            base_revision: Some(1),
            content: None,
            executable: false,
            replay: None,
        };
        insert_entry(&test, &handle, deletion).await;

        let error = publish_as(&ctx, &handle, "drop x").await.unwrap_err();

        let OkfError::LintFailed { errors, .. } = &error else {
            panic!("not LintFailed: {error:?}")
        };
        assert!(errors
            .iter()
            .any(|finding| finding.file == "hub.md" && finding.rule == "link-into-deleted"));
        assert_eq!(published_text(&test, "infra/x.md").await, X);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_publish_checks_against_an_import_the_snapshot_has_not_seen() {
        let test = pg().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;
        let deletion = EntryRow {
            path: "infra/y.md".to_string(),
            base_revision: Some(1),
            content: None,
            executable: false,
            replay: None,
        };
        insert_entry(&test, &handle, deletion).await;
        // A CLI import (another process) adds a link to infra/y.md; this
        // server's snapshot has not been refreshed since.
        let linker = "---\ntype: Note\ntitle: L\ndescription: L.\n---\nSee [Y](/infra/y.md).\n";
        let incoming = [
            ("a.md", NOTE),
            ("hub.md", HUB),
            ("infra/index.md", "# Infra\n"),
            ("infra/x.md", X),
            ("infra/y.md", Y),
            ("linker.md", linker),
            ("log.md", "# Log\n"),
        ];
        let options = ImportOptions {
            replace: true,
            scope: ImportScope::Everything,
            expect_generation: None,
            expect_catalog_id: None,
            actor: "cli",
            audit: &crate::store::audit::AuditActor::cli(),
            now: Utc::now(),
        };
        import(&test.app.main_db().await, "pg", files(&incoming), &options)
            .await
            .unwrap();
        let state = test.app.catalog("pg").await.unwrap().unwrap();
        let stale = state.current().generation();

        let error = publish_as(&ctx, &handle, "drop y").await.unwrap_err();

        // Step 2 rebuilt the snapshot first, so the new link was seen.
        let OkfError::LintFailed { errors, .. } = &error else {
            panic!("not LintFailed: {error:?}")
        };
        assert!(errors
            .iter()
            .any(|finding| finding.file == "linker.md" && finding.rule == "link-into-deleted"));
        assert_eq!(state.current().generation(), stale + 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_secret_is_refused_at_publish_too() {
        let test = pg().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;
        // Built at run time, so no source file holds a secret-shaped string.
        let leaked = format!("{NOTE}password = {}\n", "Ab1".repeat(4));
        let entry = EntryRow {
            path: "a.md".to_string(),
            base_revision: Some(1),
            content: Some(leaked.into_bytes()),
            executable: false,
            replay: None,
        };
        insert_entry(&test, &handle, entry).await;

        let error = publish_as(&ctx, &handle, "leak").await.unwrap_err();

        assert_eq!(error.kind(), "SecretDetected");
        assert_eq!(published_text(&test, "a.md").await, NOTE);
    }

    /// `(revision, deleted, content)` of a document row.
    async fn document_row(test: &TestApp, path: &str) -> (i32, bool, Vec<u8>) {
        sqlx::query_as(
            "SELECT revision, deleted_at IS NOT NULL, content FROM catalog.documents WHERE path = $1",
        )
        .bind(path)
        .fetch_one(&crate::db::store_pool(test.app.accounts().executor()))
        .await
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_publish_creates_deletes_and_continues_a_tombstone_s_number() {
        let test = pg().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;
        let created = "---\ntype: Note\ntitle: B\ndescription: B.\n---\n# B\n";
        let new_file = EntryRow {
            path: "b.md".to_string(),
            base_revision: None,
            content: Some(created.as_bytes().to_vec()),
            executable: false,
            replay: None,
        };
        // Nothing links to infra/y.md, so its deletion passes the link check.
        let deletion = EntryRow {
            path: "infra/y.md".to_string(),
            base_revision: Some(1),
            content: None,
            executable: false,
            replay: None,
        };
        insert_entry(&test, &handle, new_file).await;
        insert_entry(&test, &handle, deletion).await;

        let first = publish_as(&ctx, &handle, "add b, drop y").await.unwrap();
        // A tombstone: the number moved on, the content and the snapshot entry are gone.
        let tombstone = document_row(&test, "infra/y.md").await;
        let gone = test.app.catalog("pg").await.unwrap().unwrap().current();
        assert_eq!(tombstone, (2, true, Vec::new()));
        assert!(gone.snapshot().get("infra/y.md").is_none());
        // The changeset is closed, so a new one brings y back on its tombstone.
        let again_handle = open(&ctx).await;
        let again = EntryRow {
            path: "infra/y.md".to_string(),
            base_revision: None,
            content: Some(Y.as_bytes().to_vec()),
            executable: false,
            replay: None,
        };
        insert_entry(&test, &again_handle, again).await;
        let second = publish_as(&ctx, &again_handle, "bring y back")
            .await
            .unwrap();

        let revision = |path: &str, revision: i32, op: &'static str| PublishedRevision {
            path: path.to_string(),
            revision,
            op,
        };
        assert_eq!(
            first.report.revisions,
            [
                revision("/b.md", 1, "create"),
                revision("/infra/y.md", 2, "delete")
            ]
        );
        assert_eq!(
            second.report.revisions,
            [revision("/infra/y.md", 3, "create")]
        );
        assert_eq!(published_text(&test, "b.md").await, created);
        assert_eq!(published_text(&test, "infra/y.md").await, Y);
        assert_eq!(
            document_row(&test, "infra/y.md").await,
            (3, false, Y.as_bytes().to_vec())
        );
        // The deletion's history row: no content, and the changeset that made it.
        let deleted: (Option<Vec<u8>>, Option<i64>) = sqlx::query_as(
            "SELECT content, changeset_id FROM catalog.revisions WHERE path = 'infra/y.md' AND revision = 2",
        )
        .fetch_one(&crate::db::store_pool(test.app.accounts().executor()))
        .await
        .unwrap();
        assert_eq!(
            deleted,
            (None, Some(changeset_row(&test, &first.changeset).await.id))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_revision_written_meanwhile_is_a_conflict_not_a_database_error() {
        let test = pg().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;
        let state = test.app.catalog("pg").await.unwrap().unwrap();
        body(&ctx, &handle, "/a.md", "# A\n\nMine.\n").await;
        // A revision 2 of a.md that the publish lock should have ruled out
        // (a hand edit of the database): the backstop must report it.
        sqlx::query(
            "INSERT INTO catalog.revisions (catalog_id, path, revision, op, content, executable, actor, \
                                    message, created_at) \
             VALUES ($1, 'a.md', 2, 'update', 'x'::bytea, false, 'intruder', 'x', now())",
        )
        .bind(state.id())
        .execute(&crate::db::store_pool(test.app.accounts().executor()))
        .await
        .unwrap();

        let error = publish_as(&ctx, &handle, "mine").await.unwrap_err();

        assert!(matches!(
            error,
            // The live revision, re-read after the rollback: the stray row
            // never moved the document past 1.
            OkfError::Conflict { ref files } if files == &[ConflictFile {
                path: "/a.md".to_string(),
                base_revision: Some(1),
                current_revision: Some(1),
            }]
        ));
        assert_eq!(state_of(&test, &handle).await, ChangesetState::Open);
        assert_eq!(document_row(&test, "a.md").await.0, 1);
        assert_eq!(published_text(&test, "a.md").await, NOTE);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_write_queued_behind_a_publish_is_refused() {
        let test = pg().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;
        let state = test.app.catalog("pg").await.unwrap().unwrap();
        body(&ctx, &handle, "/a.md", "# A\n\nFirst.\n").await;
        // Hold the changeset's row lock, so the publish and then the write
        // queue on it in that order.
        let mut holder = test.app.main_db().await.begin().await.unwrap();
        changesets::lock_catalog(&mut holder, state.id())
            .await
            .unwrap();
        changesets::lock(&mut holder, changeset_row(&test, &handle).await.id)
            .await
            .unwrap();
        // Two connections of one key, each with its own context and copy of
        // the handle (`ToolContext` is not `Clone`); `move` gives each task its own.
        let same_key = || {
            (
                context(
                    &test,
                    principal_as("writer-a", Access::Write, &[], Some("pg")),
                    None,
                ),
                handle.clone(),
            )
        };
        let (publisher, to_publish) = same_key();
        let publishing =
            tokio::spawn(async move { publish_as(&publisher, &to_publish, "first").await });
        lock_waiters(&test, 1).await;
        let (late, to_write) = same_key();
        let writing = tokio::spawn(async move {
            let params = ReplaceBodyParams {
                path: "/infra/y.md".to_string(),
                body: "# Y\n\nLate.\n".to_string(),
                changeset: to_write,
            };
            replace_body(&late, params).await
        });
        lock_waiters(&test, 2).await;

        holder.rollback().await.unwrap();
        let published = publishing.await.unwrap().unwrap();
        let refused = writing.await.unwrap().unwrap_err();

        // The publish took the lock first and closed the changeset. The
        // write re-reads the row once its lock is granted, sees it
        // published, and is refused: never `Busy`, and never stored.
        assert_eq!(published.changeset, handle);
        assert_eq!(
            published.report.revisions,
            [PublishedRevision {
                path: "/a.md".to_string(),
                revision: 2,
                op: "update"
            }]
        );
        assert_eq!(refused.kind(), "Refused");
        let message = refused.to_string();
        assert!(message.contains("while this write waited"), "{message}");
        assert_eq!(published_text(&test, "infra/y.md").await, Y);
        let entries: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM catalog.changeset_entries WHERE path = 'infra/y.md'",
        )
        .fetch_one(&crate::db::store_pool(test.app.accounts().executor()))
        .await
        .unwrap();
        assert_eq!(entries, 0);
    }
}

#[cfg(test)]
mod changes_tests {
    use super::*;
    use std::time::Duration;

    use chrono::Utc;

    use crate::config::Config;
    use crate::mcp::testing::{
        app_with, changeset_row, context, files, open, pending_entry, principal, principal_as,
        writer, PendingEdit, TestApp,
    };
    use crate::mcp::writes::{
        create_concept, replace_body, CreateConceptParams, ReplaceBodyParams,
    };
    use crate::store::changesets::{self, ChangesetState};
    use crate::store::import::{import, ImportOptions, ImportScope};

    const NOTE: &str = "---\ntype: Note\ntitle: A\ndescription: A note.\n---\n# A\n";
    const NOTE_B: &str = "---\ntype: Note\ntitle: B\ndescription: B note.\n---\n# B\n";

    async fn pg() -> TestApp {
        let files: &[(&str, &str)] = &[
            ("a.md", NOTE),
            ("b.md", NOTE_B),
            ("AGENTS.md", "# Agents\n"),
        ];
        app_with(&[("pg", files)], Config::default()).await
    }

    /// Replaces the published catalog with `entries` (an import standing in
    /// for other sessions' publishes), and lets the server notice.
    async fn republish(test: &TestApp, entries: &[(&str, &str)]) {
        let options = ImportOptions {
            replace: true,
            scope: ImportScope::Everything,
            expect_generation: None,
            expect_catalog_id: None,
            actor: "tester",
            audit: &crate::store::audit::AuditActor::cli(),
            now: Utc::now(),
        };
        import(&test.app.main_db().await, "pg", files(entries), &options)
            .await
            .unwrap();
        test.app
            .catalog("pg")
            .await
            .unwrap()
            .unwrap()
            .refresh(&test.app.main_db().await, test.app.rebuild_locks())
            .await
            .unwrap();
    }

    fn body_params(handle: &str, path: &str, text: &str) -> ReplaceBodyParams {
        ReplaceBodyParams {
            path: path.to_string(),
            body: text.to_string(),
            changeset: handle.to_string(),
        }
    }

    async fn body(ctx: &ToolContext, handle: &str, path: &str, text: &str) -> WriteReport {
        replace_body(ctx, body_params(handle, path, text))
            .await
            .unwrap()
    }

    async fn new_note(ctx: &ToolContext, handle: &str, path: &str) -> WriteReport {
        let frontmatter =
            serde_json::json!({"type": "Note", "title": "New", "description": "New."});
        let params = CreateConceptParams {
            path: path.to_string(),
            frontmatter: frontmatter.as_object().unwrap().clone(),
            body: "# New\n".to_string(),
            index_summary: None,
            index_section: None,
            folder_summary: None,
            by: None,
            changeset: handle.to_string(),
        };
        create_concept(ctx, params).await.unwrap()
    }

    fn revert_to(handle: &str, path: &str, revision: i32) -> RevertParams {
        RevertParams {
            path: path.to_string(),
            revision,
            expected_revision: None,
            changeset: handle.to_string(),
        }
    }

    fn discard_paths(handle: &str, paths: Option<&[&str]>) -> DiscardParams {
        let paths = paths.map(|paths| paths.iter().map(|path| path.to_string()).collect());
        DiscardParams {
            paths,
            changeset: handle.to_string(),
        }
    }

    fn pending_params(handle: &str) -> PendingParams {
        PendingParams {
            changeset: handle.to_string(),
        }
    }

    async fn pending(ctx: &ToolContext, handle: &str) -> PendingChanges {
        pending_changes(ctx, pending_params(handle)).await.unwrap()
    }

    async fn state_of(test: &TestApp, handle: &str) -> ChangesetState {
        changeset_row(test, handle).await.state
    }

    /// The paths of the changeset `handle` names, in order.
    async fn entry_paths(test: &TestApp, handle: &str) -> Vec<String> {
        let row = changeset_row(test, handle).await;
        changesets::entries(test.app.main_db().await.executor(), row.id)
            .await
            .unwrap()
            .into_iter()
            .map(|entry| entry.path)
            .collect()
    }

    /// The writer key `writer-a` demoted to `read`: it still owns its changesets.
    fn demoted(test: &TestApp) -> ToolContext {
        context(
            test,
            principal_as("writer-a", Access::Read, &[], Some("pg")),
            None,
        )
    }

    /// Waits until `count` backends wait on a lock (as `publish_tests` does).
    async fn lock_waiters(test: &TestApp, count: i64) {
        for _ in 0..500 {
            let waiting: i64 = sqlx::query_scalar(concat!(
                "SELECT count(*) FROM pg_stat_activity WHERE ",
                crate::test_locks::own_databases!("datname"),
                " AND wait_event_type = 'Lock'"
            ))
            .fetch_one(test.app.accounts().executor())
            .await
            .unwrap();
            if waiting >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{count} lock waiter(s) never appeared");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn discard_drops_named_paths_or_closes_the_whole_changeset() {
        let test = pg().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;
        body(&ctx, &handle, "/a.md", "One.\n").await;
        body(&ctx, &handle, "/b.md", "Two.\n").await;
        let before = changeset_row(&test, &handle).await.version;

        let empty = discard(&ctx, discard_paths(&handle, Some(&[])))
            .await
            .unwrap();
        let unchanged = changeset_row(&test, &handle).await.version;
        let some = discard(&ctx, discard_paths(&handle, Some(&["/a.md", "/c.md"])))
            .await
            .unwrap();
        let left = pending(&ctx, &handle).await;
        let outside = discard(&ctx, discard_paths(&handle, Some(&["/../x.md"])))
            .await
            .unwrap_err();
        let all = discard(&ctx, discard_paths(&handle, None)).await.unwrap();
        // A full discard kills the handle: every later call is refused.
        let after = pending_changes(&ctx, pending_params(&handle))
            .await
            .unwrap_err();
        let twice = discard(&ctx, discard_paths(&handle, None))
            .await
            .unwrap_err();
        let late = replace_body(&ctx, body_params(&handle, "/a.md", "Three.\n"))
            .await
            .unwrap_err();

        // An empty list drops nothing and leaves the version alone.
        assert_eq!(
            (empty.changeset.as_str(), empty.removed, empty.closed),
            (handle.as_str(), 0, false)
        );
        assert_eq!(unchanged, before);
        assert_eq!(
            (some.changeset.as_str(), some.removed, some.closed),
            (handle.as_str(), 1, false)
        );
        let paths: Vec<&str> = left.files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(paths, ["/b.md"]);
        assert_eq!(outside.kind(), "InvalidPath");
        assert_eq!((all.removed, all.closed), (1, true));
        assert_eq!(state_of(&test, &handle).await, ChangesetState::Discarded);
        for refused in [after, twice, late] {
            assert_eq!(refused.kind(), "Refused");
            let message = refused.to_string();
            assert!(
                message.contains(&handle) && message.contains("discarded"),
                "{message}"
            );
        }
        // The published documents are untouched.
        let loaded = test.app.catalog("pg").await.unwrap().unwrap().current();
        assert_eq!(
            loaded.snapshot().get("a.md").map(|doc| doc.text()),
            Some(NOTE)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn discard_needs_the_owner_s_write_access() {
        let test = pg().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;
        let reader = context(&test, principal(&[], Some("pg")), None);

        let unknown = discard(&reader, discard_paths(&handle, None))
            .await
            .unwrap_err();
        let forbidden = discard(&demoted(&test), discard_paths(&handle, None))
            .await
            .unwrap_err();

        // A `read` key owns no changeset; the owner below `write` is refused.
        assert_eq!(unknown.kind(), "UnknownChangeset");
        assert_eq!(forbidden.kind(), "Forbidden");
        assert_eq!(state_of(&test, &handle).await, ChangesetState::Open);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_write_queued_behind_a_discard_is_refused() {
        let test = pg().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;
        let state = test.app.catalog("pg").await.unwrap().unwrap();
        body(&ctx, &handle, "/a.md", "First.\n").await;
        // Hold the changeset's row lock, so the discard and then the write
        // queue on it in that order.
        let mut holder = test.app.main_db().await.begin().await.unwrap();
        changesets::lock_catalog(&mut holder, state.id())
            .await
            .unwrap();
        changesets::lock(&mut holder, changeset_row(&test, &handle).await.id)
            .await
            .unwrap();
        // Two connections of one key, each with its own context and copy of
        // the handle (`ToolContext` is not `Clone`); `move` gives each task its own.
        let same_key = || {
            (
                context(
                    &test,
                    principal_as("writer-a", Access::Write, &[], Some("pg")),
                    None,
                ),
                handle.clone(),
            )
        };
        let (discarder, to_discard) = same_key();
        let discarding =
            tokio::spawn(
                async move { discard(&discarder, discard_paths(&to_discard, None)).await },
            );
        lock_waiters(&test, 1).await;
        let (late, to_write) = same_key();
        let writing = tokio::spawn(async move {
            replace_body(&late, body_params(&to_write, "/b.md", "Late.\n")).await
        });
        lock_waiters(&test, 2).await;

        holder.rollback().await.unwrap();
        let discarded = discarding.await.unwrap().unwrap();
        let refused = writing.await.unwrap().unwrap_err();

        // The discard took the lock first and closed the changeset. The
        // write re-reads the row once its lock is granted, sees it
        // discarded, and is refused: never `Busy`, and nothing stored.
        assert_eq!(
            (
                discarded.changeset.as_str(),
                discarded.removed,
                discarded.closed
            ),
            (handle.as_str(), 1, true)
        );
        assert_eq!(state_of(&test, &handle).await, ChangesetState::Discarded);
        assert_eq!(refused.kind(), "Refused");
        let message = refused.to_string();
        assert!(message.contains("while this write waited"), "{message}");
        assert!(entry_paths(&test, &handle).await.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn revert_writes_an_old_revision_as_a_free_edit() {
        let test = pg().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;
        republish(
            &test,
            &[
                ("a.md", "# Two\n"),
                ("b.md", NOTE_B),
                ("AGENTS.md", "# Agents\n"),
            ],
        )
        .await;

        let reverted = revert(&ctx, revert_to(&handle, "/a.md", 1)).await.unwrap();
        let shown = pending(&ctx, &handle).await;
        republish(
            &test,
            &[
                ("a.md", "# Three\n"),
                ("b.md", NOTE_B),
                ("AGENTS.md", "# Agents\n"),
            ],
        )
        .await;
        let stale = pending(&ctx, &handle).await;

        assert_eq!(reverted.touched, ["/a.md"]);
        assert_eq!(reverted.message, "a.md: reverted to revision 1");
        let entries = changesets::entries(
            test.app.main_db().await.executor(),
            changeset_row(&test, &handle).await.id,
        )
        .await
        .unwrap();
        assert_eq!(entries[0].content.as_deref(), Some(NOTE.as_bytes()));
        // Based on what is published now (revision 2), with no replay.
        assert_eq!(
            (entries[0].base_revision, entries[0].replay.as_ref()),
            (Some(2), None)
        );
        let file = &shown.files[0];
        assert_eq!(
            (file.change, file.base_revision, file.current_revision),
            ("update", Some(2), Some(2))
        );
        assert!(!file.stale && !file.replayable);
        assert!(
            file.diff.starts_with("--- a/a.md\n+++ b/a.md\n"),
            "{}",
            file.diff
        );
        assert!(file.diff.contains("-# Two\n+---\n"), "{}", file.diff);
        // Published again since: the base is stale, and the diff is against revision 3.
        let file = &stale.files[0];
        assert_eq!(
            (file.base_revision, file.current_revision, file.stale),
            (Some(2), Some(3), true)
        );
        // A free edit has no replay: the stored text, as before issue #787.
        assert!(!file.replayed && file.replay_error.is_none());
        assert!(file.diff.contains("-# Three\n"), "{}", file.diff);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_revert_is_recorded_when_it_is_written_and_an_admins_discard_too() {
        use crate::mcp::testing::store_key;
        use crate::store::audit::testing::entries;
        let test = pg().await;
        let owner = writer(&test, "writer-a", "pg").await;
        let handle = open(&owner).await;
        let kept = open(&owner).await;
        body(&owner, &handle, "/a.md", "# A\n\nChanged.\n").await;
        body(&owner, &kept, "/b.md", "# B\n\nChanged.\n").await;
        store_key(&test, "root").await;
        let admin = context(
            &test,
            principal_as("root", Access::Admin, &[], Some("pg")),
            None,
        );

        revert(&owner, revert_to(&handle, "/a.md", 1))
            .await
            .unwrap();
        // The owner's own discards, of some paths or the whole changeset,
        // are not administrative changes.
        discard(&owner, discard_paths(&handle, Some(&["/b.md"])))
            .await
            .unwrap();
        discard(&owner, discard_paths(&kept, None)).await.unwrap();
        let closed = discard(&admin, discard_paths(&handle, None)).await.unwrap();

        let reverted = entries(test.app.accounts().executor(), "catalog.reverted").await;
        let discarded = entries(
            test.app.accounts().executor(),
            "catalog.changeset_discarded",
        )
        .await;
        assert_eq!(reverted.len(), 1, "{reverted:?}");
        assert_eq!(reverted[0].target.as_deref(), Some("pg"));
        assert_eq!(reverted[0].teams, [crate::db::TEST_TEAM]);
        assert_eq!(
            reverted[0].details,
            serde_json::json!({"path": "/a.md", "revision": 1, "changeset": handle})
        );
        assert!(!reverted[0].actor_operator);
        assert_eq!(discarded.len(), 1, "{discarded:?}");
        assert_eq!(discarded[0].target.as_deref(), Some("pg"));
        assert_eq!(discarded[0].teams, [crate::db::TEST_TEAM]);
        assert_eq!(
            discarded[0].details,
            serde_json::json!({"changeset": handle, "files": closed.removed, "reason": "admin"})
        );
        assert_eq!(closed.removed, 1, "the reverted a.md");
        assert!(discarded[0].actor_operator, "an admin key is an operator");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_audit_write_fails_the_revert_and_the_admins_discard() {
        use crate::http::api::testing::fail_on;
        use crate::mcp::testing::store_key;
        let test = pg().await;
        let owner = writer(&test, "writer-a", "pg").await;
        let handle = open(&owner).await;
        body(&owner, &handle, "/a.md", "# A\n\nChanged.\n").await;
        store_key(&test, "root").await;
        let admin = context(
            &test,
            principal_as("root", Access::Admin, &[], Some("pg")),
            None,
        );
        fail_on(
            test.app.accounts().executor(),
            "BEFORE INSERT ON catalog.catalog_audit_entries FOR EACH ROW",
        )
        .await;

        let reverted = revert(&owner, revert_to(&handle, "/b.md", 1)).await;
        let discarded = discard(&admin, discard_paths(&handle, None)).await;

        assert!(reverted.is_err(), "{reverted:?}");
        assert!(discarded.is_err(), "{discarded:?}");
        assert_eq!(
            entry_paths(&test, &handle).await,
            ["a.md"],
            "both rolled back"
        );
        assert_eq!(state_of(&test, &handle).await, ChangesetState::Open);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn revert_to_a_delete_revision_deletes_and_machinery_stays_refused() {
        let test = pg().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;
        // b.md: 1 imported, 2 deleted, 3 imported again.
        republish(&test, &[("a.md", NOTE), ("AGENTS.md", "# Agents\n")]).await;
        republish(
            &test,
            &[
                ("a.md", NOTE),
                ("b.md", NOTE_B),
                ("AGENTS.md", "# Agents\n"),
            ],
        )
        .await;

        let deleted = revert(&ctx, revert_to(&handle, "/b.md", 2)).await.unwrap();
        let twice = revert(&ctx, revert_to(&handle, "/b.md", 2))
            .await
            .unwrap_err();
        let machinery = revert(&ctx, revert_to(&handle, "/AGENTS.md", 1))
            .await
            .unwrap_err();
        let no_such = revert(&ctx, revert_to(&handle, "/a.md", 9))
            .await
            .unwrap_err();
        // A revision whose bytes are not UTF-8 (no write tool stores one).
        sqlx::query("UPDATE catalog.revisions SET content = '\\xff'::bytea WHERE path = 'a.md'")
            .execute(&crate::db::store_pool(test.app.accounts().executor()))
            .await
            .unwrap();
        let binary = revert(&ctx, revert_to(&handle, "/a.md", 1))
            .await
            .unwrap_err();
        let read_only = revert(&demoted(&test), revert_to(&handle, "/a.md", 1))
            .await
            .unwrap_err();

        assert_eq!(deleted.touched, ["/b.md"]);
        let entries = changesets::entries(
            test.app.main_db().await.executor(),
            changeset_row(&test, &handle).await.id,
        )
        .await
        .unwrap();
        assert_eq!(
            (entries[0].path.as_str(), entries[0].content.as_ref()),
            ("b.md", None)
        );
        assert_eq!(entries[0].base_revision, Some(3));
        let shown = pending(&ctx, &handle).await;
        assert_eq!(shown.files[0].change, "delete");
        assert!(
            shown.files[0]
                .diff
                .starts_with("--- a/b.md\n+++ /dev/null\n"),
            "{}",
            shown.files[0].diff
        );
        assert_eq!(twice.kind(), "Refused");
        assert_eq!(machinery.kind(), "InvalidPath");
        assert_eq!(no_such.kind(), "NotFound");
        assert_eq!(binary.kind(), "Refused");
        assert_eq!(read_only.kind(), "Forbidden");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reverting_a_created_file_to_absent_leaves_no_entry() {
        let test = pg().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;
        // b.md: 1 imported, 2 deleted. The changeset then creates it anew.
        republish(&test, &[("a.md", NOTE), ("AGENTS.md", "# Agents\n")]).await;
        let created = new_note(&ctx, &handle, "/b.md").await;

        let reverted = revert(&ctx, revert_to(&handle, "/b.md", 2)).await.unwrap();

        assert_eq!(created.touched, ["/b.md"]);
        // Never published as the changeset made it and not published now: the
        // changeset's own entry goes, so nothing is left to list.
        assert!(reverted.touched.is_empty(), "{:?}", reverted.touched);
        assert_eq!(reverted.changeset, created.changeset);
        assert!(entry_paths(&test, &handle).await.is_empty());
        assert!(pending(&ctx, &handle).await.files.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pending_changes_diffs_a_new_file_against_dev_null_and_needs_only_read() {
        let test = pg().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;
        new_note(&ctx, &handle, "/new.md").await;

        let shown = pending(&ctx, &handle).await;
        // The owner below `write` may still look; a `read` key owns none.
        let owner_reading = pending(&demoted(&test), &handle).await;
        let reader = context(&test, principal(&[], Some("pg")), None);
        let unknown = pending_changes(&reader, pending_params(&handle))
            .await
            .unwrap_err();

        assert_eq!(
            (shown.catalog.as_str(), shown.changeset.as_str()),
            ("pg", handle.as_str())
        );
        let file = &shown.files[0];
        assert_eq!(
            (file.path.as_str(), file.change, file.base_revision),
            ("/new.md", "create", None)
        );
        assert_eq!((file.current_revision, file.stale), (None, false));
        assert!(
            file.diff
                .starts_with("--- /dev/null\n+++ b/new.md\n@@ -0,0 +1,"),
            "{}",
            file.diff
        );
        assert!(file.diff.contains("+# New\n"));
        assert_eq!(owner_reading, shown);
        assert_eq!(unknown.kind(), "UnknownChangeset");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_changeset_whose_overlay_no_longer_applies_can_be_listed_and_discarded() {
        let test = pg().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;
        let state = test.app.catalog("pg").await.unwrap().unwrap();
        // `a.md` is a published file, so an entry under it can never apply
        // (no write tool stores one; a publish made after the edit can).
        let edit = PendingEdit {
            path: "a.md/x.md",
            text: Some("# X\n"),
            at: Utc::now(),
        };
        pending_entry(&test, &handle, &edit).await;
        // Pinned per read, as each read call does.
        let pinned = || async { ctx.pin(Some(&handle)).await.unwrap() };

        let broken = ctx.view(&state, pinned().await.as_ref()).await.err();
        let shown = pending(&ctx, &handle).await;
        let dropped = discard(&ctx, discard_paths(&handle, Some(&["/a.md/x.md"])))
            .await
            .unwrap();
        let fixed = ctx.view(&state, pinned().await.as_ref()).await;

        assert!(
            matches!(broken, Some(OkfError::InvalidPath { .. })),
            "{broken:?}"
        );
        let paths: Vec<&str> = shown.files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(paths, ["/a.md/x.md"]);
        assert_eq!(dropped.removed, 1);
        assert!(
            fixed.is_ok(),
            "the overlay applies again once the entry is gone"
        );
    }

    #[test]
    fn a_file_equal_to_the_published_one_is_a_one_line_note() {
        let text = Some(&b"# A\n"[..]);

        let same = unified_diff("a.md", text, text);
        let binary = unified_diff("logo.png", Some(b"\x89PNG\xff"), Some(b"\x89PNG\xff"));

        assert_eq!(same, "No change from the published text\n");
        assert_eq!(binary, "No change from the published text\n");
    }

    #[test]
    fn deleting_a_path_deleted_since_is_a_one_line_note() {
        let diff = unified_diff("a.md", None, None);

        assert_eq!(
            diff,
            "No published a.md to delete: it was deleted after this entry was made\n"
        );
    }

    #[test]
    fn a_file_that_is_not_text_is_reported_as_binary() {
        let diff = unified_diff("logo.png", Some(b"\x89PNG\xff"), Some(b"\x89PNG\xfe"));

        assert_eq!(diff, "Binary files a/logo.png and b/logo.png differ\n");
    }

    #[test]
    fn a_stored_path_is_normalized_and_stays_inside() {
        assert_eq!(stored_path("/infra/./ntp.md").unwrap(), "infra/ntp.md");
        assert_eq!(stored_path("AGENTS.md").unwrap(), "AGENTS.md");
        assert!(matches!(
            stored_path("/"),
            Err(OkfError::InvalidPath { .. })
        ));
        assert!(matches!(
            stored_path("a/../../x.md"),
            Err(OkfError::InvalidPath { .. })
        ));
    }

    #[test]
    fn the_three_tools_are_listed_with_object_output_schemas() {
        let tools = OkfServer::tools().list_all();

        for name in ["discard", "revert", "pending_changes"] {
            let listed = tools
                .iter()
                .find(|tool| tool.name == name)
                .unwrap_or_else(|| panic!("{name} is registered"));
            let schema = listed.output_schema.as_ref().expect("an output schema");
            assert_eq!(schema["type"], "object", "{name}");
        }
    }
}

#[cfg(test)]
mod restructure_publish_tests {
    use super::*;

    use crate::config::Config;
    use crate::error::ConflictFile;
    use crate::mcp::testing::{app_with, changeset_row, open, TestApp};
    use crate::mcp::writes::{
        create_concept, edit_document, replace_body, CreateConceptParams, EditDocumentParams,
        ReplaceBodyParams,
    };
    use crate::okf::ops::{self, LogKind, Split, DEFAULT_SPLIT_TYPE};
    use crate::store::changesets::ChangesetState;
    use crate::store::history;
    use crate::write::apply;

    const X: &str = "---\ntype: Note\ntitle: X\ndescription: X.\n---\n# X\n";
    const Y: &str = "---\ntype: Note\ntitle: Y\ndescription: Y.\n---\n# Y\n";
    const BIG: &str = "---\ntype: Note\ntitle: Big\ndescription: Big.\n---\n# Big\n\n\
                       ## Part\n\nPart text.\n\n### Deep\n\nDeep text.\n\n## Rest\n\nRest text.\n";
    const HUB: &str = "---\ntype: Note\ntitle: Hub\ndescription: Links.\n---\n\
                       See [X](/infra/x.md) and [the part](/infra/big.md#part).\n";

    async fn pg() -> TestApp {
        let files: &[(&str, &str)] = &[
            (
                "index.md",
                "# PG\n\n# Sections\n\n* [Infra](/infra/index.md) - systems\n",
            ),
            (
                "infra/index.md",
                "# Infra\n\n# Concepts\n\n* [X](x.md) - x\n* [Y](y.md) - y\n* [Big](big.md) - big\n",
            ),
            ("infra/x.md", X),
            ("infra/y.md", Y),
            ("infra/big.md", BIG),
            ("hub.md", HUB),
            (
                "log.md",
                "# Log\n\n## 2026-11-01\n\n* **Update:** Added [X](/infra/x.md).\n",
            ),
        ];
        app_with(&[("pg", files)], Config::default()).await
    }

    /// A writer key's connection and the changeset it works in.
    struct Session {
        ctx: ToolContext,
        handle: String,
    }

    /// A `write` key on `pg` with one open changeset.
    async fn writer(test: &TestApp, key_id: &str, default: &str) -> Session {
        let ctx = crate::mcp::testing::writer(test, key_id, default).await;
        let handle = open(&ctx).await;
        Session { ctx, handle }
    }

    /// A `move_concept` write, through the write path as Task 6's tool makes it.
    async fn moved(session: &Session, old: &str, new: &str) -> WriteReport {
        let (old, new) = (old.to_string(), new.to_string());
        apply(&session.ctx, &session.handle, None, move |ws, _| {
            ops::move_concept(ws, &old, &new, None)
        })
        .await
        .unwrap()
    }

    /// A `delete_concept` write.
    async fn deleted(session: &Session, path: &str) -> WriteReport {
        let path = path.to_string();
        apply(&session.ctx, &session.handle, None, move |ws, _| {
            ops::delete_concept(ws, &path)
        })
        .await
        .unwrap()
    }

    /// A `log` write.
    async fn logged(session: &Session, text: &str) -> WriteReport {
        let text = text.to_string();
        apply(&session.ctx, &session.handle, None, move |ws, op| {
            ops::log(ws, op, LogKind::Update, &text)
        })
        .await
        .unwrap()
    }

    /// A `split_concept` write: `## Part` of `infra/big.md` (with its
    /// `### Deep`) to `infra/part.md`.
    async fn split_part(session: &Session) -> WriteReport {
        apply(&session.ctx, &session.handle, None, |ws, op| {
            let split = Split {
                concept: "infra/big.md",
                section: "Part",
                to: "infra/part.md",
                kind: DEFAULT_SPLIT_TYPE,
                title: None,
                description: Some("The part."),
                stub: None,
                sources: &[],
                folder_summary: None,
            };
            ops::split_concept(ws, op, &split)
        })
        .await
        .unwrap()
    }

    /// A new concept `Q` at `path` with `body`; listed in its folder when `listed`.
    async fn created(session: &Session, path: &str, body: &str, listed: bool) -> WriteReport {
        let frontmatter = serde_json::json!({"type": "Note", "title": "Q", "description": "Q."});
        let params = CreateConceptParams {
            path: path.to_string(),
            frontmatter: frontmatter.as_object().cloned().unwrap_or_default(),
            body: body.to_string(),
            // `bool::then`: `Some(..)` when true, like `x if cond else None`.
            index_summary: listed.then(|| "q".to_string()),
            index_section: None,
            folder_summary: None,
            by: None,
            changeset: session.handle.clone(),
        };
        create_concept(&session.ctx, params).await.unwrap()
    }

    async fn publish_as(session: &Session, message: &str) -> Result<Published, OkfError> {
        let params = PublishParams {
            message: message.to_string(),
            changeset: session.handle.clone(),
        };
        publish(&session.ctx, params).await
    }

    async fn published_text(test: &TestApp, path: &str) -> String {
        let loaded = test.app.catalog("pg").await.unwrap().unwrap().current();
        loaded
            .snapshot()
            .get(path)
            .map(|doc| doc.text().to_string())
            .unwrap_or_default()
    }

    /// Whether `path`'s `documents` row is a tombstone (`deleted_at` set).
    async fn tombstoned(test: &TestApp, path: &str) -> bool {
        // `$1` is a bind parameter; `documents` stores paths without the `/`.
        sqlx::query_scalar("SELECT deleted_at IS NOT NULL FROM catalog.documents WHERE path = $1")
            .bind(path)
            .fetch_one(&crate::db::store_pool(test.app.accounts().executor()))
            .await
            .unwrap()
    }

    /// The publish's revisions as sorted `(path, op, revision)`.
    fn revisions(published: &Published) -> Vec<(&str, &str, i32)> {
        let mut rows: Vec<(&str, &str, i32)> = published
            .report
            .revisions
            .iter()
            .map(|row| (row.path.as_str(), row.op, row.revision))
            .collect();
        rows.sort_unstable();
        rows
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_published_move_deletes_the_old_path_and_creates_the_new_one() {
        let test = pg().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let state = test.app.catalog("pg").await.unwrap().unwrap();
        moved(&ctx, "infra/x.md", "infra/z.md").await;

        let published = publish_as(&ctx, "move x").await.unwrap();

        assert_eq!(
            revisions(&published),
            [
                ("/hub.md", "update", 2),
                ("/infra/index.md", "update", 2),
                ("/infra/x.md", "delete", 2),
                ("/infra/z.md", "create", 1),
                // Revision 1 is the starter log.md, 2 the fixture's import.
                ("/log.md", "update", 3),
            ]
        );
        assert!(published.report.replayed.is_empty());
        let old = history::revisions(&test.app.main_db().await, state.id(), "infra/x.md", 5)
            .await
            .unwrap();
        let new = history::revisions(&test.app.main_db().await, state.id(), "infra/z.md", 5)
            .await
            .unwrap();
        // Each path keeps its own history (plan Decision 16).
        assert_eq!((old.len(), old[0].op.as_str()), (2, "delete"));
        assert_eq!(old[0].message, "move x");
        assert_eq!((new.len(), new[0].op.as_str()), (1, "create"));
        assert!(tombstoned(&test, "infra/x.md").await);
        assert_eq!(published_text(&test, "infra/z.md").await, X);
        assert!(published_text(&test, "hub.md")
            .await
            .contains("[X](/infra/z.md)"));
        assert!(published_text(&test, "infra/index.md")
            .await
            .contains("* [X](z.md) - x"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_move_replays_its_listing_onto_a_concurrent_index_of_the_folder() {
        let test = pg().await;
        let mover = writer(&test, "writer-a", "pg").await;
        let other = writer(&test, "writer-b", "pg").await;
        moved(&mover, "infra/y.md", "infra/w.md").await;
        created(&other, "/infra/q.md", "# Q\n", true).await;
        publish_as(&other, "add q").await.unwrap();

        let published = publish_as(&mover, "rename y").await.unwrap();

        assert_eq!(published.report.replayed, ["/infra/index.md"]);
        let index = published_text(&test, "infra/index.md").await;
        assert!(index.contains("* [Y](w.md) - y"), "{index}");
        assert!(index.contains("* [Q](q.md) - q"), "{index}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_move_relinks_log_md_onto_a_concurrent_log() {
        let test = pg().await;
        let mover = writer(&test, "writer-a", "pg").await;
        let other = writer(&test, "writer-b", "pg").await;
        moved(&mover, "infra/x.md", "infra/z.md").await;
        logged(&other, "from b").await;
        publish_as(&other, "log b").await.unwrap();

        let published = publish_as(&mover, "move x").await.unwrap();

        assert_eq!(published.report.replayed, ["/log.md"]);
        let log = published_text(&test, "log.md").await;
        assert!(log.contains("* **Update:** from b"), "{log}");
        assert!(log.contains("Added [X](/infra/z.md)."), "{log}");
        assert!(!log.contains("/infra/x.md"), "{log}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_move_conflicts_with_a_concurrent_edit_of_a_linking_file() {
        let test = pg().await;
        let mover = writer(&test, "writer-a", "pg").await;
        let other = writer(&test, "writer-b", "pg").await;
        let report = moved(&mover, "infra/x.md", "infra/z.md").await;
        let body = ReplaceBodyParams {
            path: "/hub.md".to_string(),
            body: "Nothing linked.\n".to_string(),
            changeset: other.handle.clone(),
        };
        replace_body(&other.ctx, body).await.unwrap();
        publish_as(&other, "edit hub").await.unwrap();

        let error = publish_as(&mover, "move x").await.unwrap_err();

        assert!(
            matches!(
                error,
                OkfError::Conflict { ref files } if files == &[ConflictFile {
                    path: "/hub.md".to_string(),
                    base_revision: Some(1),
                    current_revision: Some(2),
                }]
            ),
            "{error:?}"
        );
        let row = changeset_row(&test, &report.changeset).await;
        assert_eq!(row.state, ChangesetState::Open);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_link_to_the_old_path_published_meanwhile_refuses_the_move() {
        let test = pg().await;
        let mover = writer(&test, "writer-a", "pg").await;
        let other = writer(&test, "writer-b", "pg").await;
        let report = moved(&mover, "infra/y.md", "infra/w.md").await;
        created(&other, "/b.md", "See [y](/infra/y.md).\n", false).await;
        publish_as(&other, "add b").await.unwrap();

        let error = publish_as(&mover, "rename y").await.unwrap_err();

        // `links_into_deleted` (store/publish.rs): b.md is not in the mover's
        // changeset and links to the path it deletes.
        let OkfError::LintFailed { errors, .. } = error else {
            panic!("expected LintFailed, got {error:?}");
        };
        assert!(
            errors
                .iter()
                .any(|finding| finding.file == "b.md" && finding.rule == "link-into-deleted"),
            "{errors:?}"
        );
        let row = changeset_row(&test, &report.changeset).await;
        assert_eq!(row.state, ChangesetState::Open);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_partial_discard_of_a_move_leaves_a_link_publish_refuses() {
        let test = pg().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let report = moved(&ctx, "infra/x.md", "infra/z.md").await;
        // Drop only hub.md's rewrite: the published hub.md still links the old path.
        let params = DiscardParams {
            paths: Some(vec!["/hub.md".to_string()]),
            changeset: ctx.handle.clone(),
        };
        discard(&ctx.ctx, params).await.unwrap();

        let error = publish_as(&ctx, "rename x").await.unwrap_err();

        let OkfError::LintFailed { errors, .. } = error else {
            panic!("expected LintFailed, got {error:?}");
        };
        assert!(
            errors
                .iter()
                .any(|finding| finding.file == "hub.md" && finding.rule == "link-into-deleted"),
            "{errors:?}"
        );
        let row = changeset_row(&test, &report.changeset).await;
        assert_eq!(row.state, ChangesetState::Open);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_listing_changed_since_a_move_to_another_folder_refuses_the_publish() {
        let test = pg().await;
        let mover = writer(&test, "writer-a", "pg").await;
        let other = writer(&test, "writer-b", "pg").await;
        let report = moved(&mover, "infra/y.md", "ops/y.md").await;
        let edit = EditDocumentParams {
            path: "/infra/index.md".to_string(),
            old_str: "* [Y](y.md) - y".to_string(),
            new_str: "* [Y](y.md) - why".to_string(),
            replace_all: false,
            changeset: other.handle.clone(),
        };
        edit_document(&other.ctx, edit).await.unwrap();
        publish_as(&other, "reword y").await.unwrap();

        let error = publish_as(&mover, "move y").await.unwrap_err();

        assert!(
            matches!(error, OkfError::Refused { ref reason } if reason.contains("changed since")),
            "{error:?}"
        );
        let row = changeset_row(&test, &report.changeset).await;
        assert_eq!(row.state, ChangesetState::Open);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_published_delete_leaves_a_tombstone_and_unlists_onto_newer_text() {
        let test = pg().await;
        let deleter = writer(&test, "writer-a", "pg").await;
        let other = writer(&test, "writer-b", "pg").await;
        let report = deleted(&deleter, "infra/y.md").await;
        created(&other, "/infra/q.md", "# Q\n", true).await;
        publish_as(&other, "add q").await.unwrap();

        let published = publish_as(&deleter, "drop y").await.unwrap();

        assert_eq!(report.touched, ["/infra/y.md", "/infra/index.md"]);
        assert_eq!(published.report.replayed, ["/infra/index.md"]);
        assert_eq!(
            revisions(&published),
            [
                ("/infra/index.md", "update", 3),
                ("/infra/y.md", "delete", 2)
            ]
        );
        let index = published_text(&test, "infra/index.md").await;
        assert!(
            !index.contains("[Y](y.md)") && index.contains("[Q](q.md)"),
            "{index}"
        );
        assert!(tombstoned(&test, "infra/y.md").await);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_published_split_writes_the_parent_the_new_concept_and_its_listing() {
        let test = pg().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        split_part(&ctx).await;

        let published = publish_as(&ctx, "split big").await.unwrap();

        assert_eq!(
            revisions(&published),
            [
                ("/hub.md", "update", 2),
                ("/infra/big.md", "update", 2),
                ("/infra/index.md", "update", 2),
                ("/infra/part.md", "create", 1),
            ]
        );
        assert!(published_text(&test, "infra/part.md")
            .await
            .contains("Part text."));
        assert!(published_text(&test, "infra/big.md")
            .await
            .contains("## Part\n\n[Part](/infra/part.md): The part.\n"));
        assert!(published_text(&test, "hub.md")
            .await
            .contains("[the part](/infra/part.md)"));
        assert!(published_text(&test, "infra/index.md")
            .await
            .contains("* [Part](part.md) - The part."));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn links_into_a_split_section_published_meanwhile_resolve_or_only_warn() {
        let test = pg().await;
        let splitter = writer(&test, "writer-a", "pg").await;
        let other = writer(&test, "writer-b", "pg").await;
        split_part(&splitter).await;
        created(&other, "/b.md", "See [deep](/infra/big.md#deep).\n", false).await;
        created(
            &other,
            "/c.md",
            "See [the part](/infra/big.md#part).\n",
            false,
        )
        .await;
        publish_as(&other, "add b and c").await.unwrap();

        let published = publish_as(&splitter, "split big").await.unwrap();

        // Plan Decision 18: split moves no path. `### Deep` moved to part.md,
        // so b.md's anchor is broken, in a file this publish does not change:
        // a warning. The parent keeps `## Part` (with its stub), so c.md's
        // link still resolves and has no finding at all.
        assert!(published.report.published);
        let warnings = &published.report.warnings;
        let broken: Vec<&str> = warnings
            .iter()
            .filter(|finding| finding.rule == "broken-anchor")
            .map(|finding| finding.file.as_str())
            .collect();
        assert_eq!(broken, ["b.md"], "{warnings:?}");
        assert!(
            !warnings.iter().any(|finding| finding.file == "c.md"),
            "{warnings:?}"
        );
        assert!(published_text(&test, "infra/big.md")
            .await
            .contains("## Part\n"));
        assert!(published_text(&test, "infra/part.md")
            .await
            .contains("Deep text."));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_delete_unlists_a_listing_reworded_meanwhile() {
        let test = pg().await;
        let deleter = writer(&test, "writer-a", "pg").await;
        let other = writer(&test, "writer-b", "pg").await;
        deleted(&deleter, "infra/y.md").await;
        let edit = EditDocumentParams {
            path: "/infra/index.md".to_string(),
            old_str: "* [Y](y.md) - y".to_string(),
            new_str: "* [Y](y.md) - why".to_string(),
            replace_all: false,
            changeset: other.handle.clone(),
        };
        edit_document(&other.ctx, edit).await.unwrap();
        publish_as(&other, "reword y").await.unwrap();

        let published = publish_as(&deleter, "drop y").await.unwrap();

        // `delete_concept` records `Unlist { bullet: None }` (plan Decision 6):
        // the bullet goes whatever it reads now, unlike a move's `Some`.
        assert_eq!(published.report.replayed, ["/infra/index.md"]);
        assert_eq!(
            revisions(&published),
            [
                ("/infra/index.md", "update", 3),
                ("/infra/y.md", "delete", 2)
            ]
        );
        let index = published_text(&test, "infra/index.md").await;
        assert!(!index.contains("y.md"), "{index}");
        assert!(index.contains("* [X](x.md) - x"), "{index}");
        assert!(tombstoned(&test, "infra/y.md").await);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_split_conflicts_with_a_concurrent_edit_of_its_parent() {
        let test = pg().await;
        let splitter = writer(&test, "writer-a", "pg").await;
        let other = writer(&test, "writer-b", "pg").await;
        let report = split_part(&splitter).await;
        let edit = EditDocumentParams {
            path: "/infra/big.md".to_string(),
            old_str: "Rest text.".to_string(),
            new_str: "Rest text, edited.".to_string(),
            replace_all: false,
            changeset: other.handle.clone(),
        };
        edit_document(&other.ctx, edit).await.unwrap();
        publish_as(&other, "edit big").await.unwrap();

        let error = publish_as(&splitter, "split big").await.unwrap_err();

        // The parent is a free edit (plan Decision 1), so a stale base conflicts.
        assert!(
            matches!(
                error,
                OkfError::Conflict { ref files } if files == &[ConflictFile {
                    path: "/infra/big.md".to_string(),
                    base_revision: Some(1),
                    current_revision: Some(2),
                }]
            ),
            "{error:?}"
        );
        let row = changeset_row(&test, &report.changeset).await;
        assert_eq!(row.state, ChangesetState::Open);
    }
}

/// M13 Task 3: every changeset path (open, pinned writes and reads,
/// publish, discard, revert, pending changes) refuses a suspended catalog to
/// a non-admin key, through `Principal::require`; a key that does not reach
/// the catalog never learns it is suspended; an admin is exempt.
#[cfg(test)]
mod suspension_tests {
    use super::*;

    use crate::config::Config;
    use crate::mcp::testing::{
        app_with, cached_principal, context, lift_suspension, stored_secret, suspend_catalogs,
        TestApp, SUSPENDED_TEAM,
    };
    use crate::mcp::writes::{replace_body, ReplaceBodyParams};

    const NOTE: &str = "---\ntype: Note\ntitle: A\ndescription: A note.\n---\n# A\n";

    /// A connection of the key behind `secret`, as the cache holds it now.
    async fn connect(test: &TestApp, secret: &str) -> ToolContext {
        context(test, cached_principal(test, secret).await, None)
    }

    /// Opens a changeset in `catalog` and edits `a.md` in it.
    async fn open_with_edit(ctx: &ToolContext, catalog: &str) -> String {
        let handle = open_changeset(
            ctx,
            OpenParams {
                catalog: Some(catalog.to_string()),
            },
        )
        .await
        .expect("open a changeset")
        .changeset;
        let edit = ReplaceBodyParams {
            path: "a.md".to_string(),
            body: "# A\n\nEdited.\n".to_string(),
            changeset: handle.clone(),
        };
        replace_body(ctx, edit).await.expect("edit a.md");
        handle
    }

    /// Every changeset tool on `handle` (and `open_changeset` on its
    /// catalog), as `(tool, error kind or "ok")`.
    async fn every_path(
        ctx: &ToolContext,
        catalog: &str,
        handle: &str,
    ) -> Vec<(&'static str, String)> {
        // `map_or_else` turns each result into its error's kind, or "ok".
        fn outcome<T>(result: Result<T, OkfError>) -> String {
            result.map_or_else(|error| error.kind().to_string(), |_| "ok".to_string())
        }
        let changeset = handle.to_string();
        vec![
            (
                "open_changeset",
                outcome(
                    open_changeset(
                        ctx,
                        OpenParams {
                            catalog: Some(catalog.to_string()),
                        },
                    )
                    .await,
                ),
            ),
            (
                "pin (a read through the changeset)",
                outcome(ctx.pin(Some(handle)).await),
            ),
            (
                "replace_body",
                outcome(
                    replace_body(
                        ctx,
                        ReplaceBodyParams {
                            path: "a.md".to_string(),
                            body: "# A\n\nAgain.\n".to_string(),
                            changeset: changeset.clone(),
                        },
                    )
                    .await,
                ),
            ),
            (
                "revert",
                outcome(
                    revert(
                        ctx,
                        RevertParams {
                            path: "a.md".to_string(),
                            revision: 1,
                            expected_revision: None,
                            changeset: changeset.clone(),
                        },
                    )
                    .await,
                ),
            ),
            (
                "pending_changes",
                outcome(
                    pending_changes(
                        ctx,
                        PendingParams {
                            changeset: changeset.clone(),
                        },
                    )
                    .await,
                ),
            ),
            (
                "discard",
                outcome(
                    discard(
                        ctx,
                        DiscardParams {
                            paths: Some(vec!["/nothing.md".to_string()]),
                            changeset: changeset.clone(),
                        },
                    )
                    .await,
                ),
            ),
            (
                "publish",
                outcome(
                    publish(
                        ctx,
                        PublishParams {
                            message: "Edit a.md.".to_string(),
                            changeset,
                        },
                    )
                    .await,
                ),
            ),
        ]
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn every_changeset_path_refuses_a_suspended_catalog_but_to_an_admin() {
        let test = app_with(
            &[("pg", &[("a.md", NOTE)]), ("work", &[("a.md", NOTE)])],
            Config::default(),
        )
        .await;
        let writer = stored_secret(&test, "writer", Access::Write, &["pg", "work"], false).await;
        let stranger = stored_secret(&test, "stranger", Access::Write, &["work"], false).await;
        let root = stored_secret(&test, "root", Access::Admin, &[], false).await;
        let handle = open_with_edit(&connect(&test, &writer).await, "pg").await;
        let in_work = open_with_edit(&connect(&test, &writer).await, "work").await;
        let roots = open_with_edit(&connect(&test, &root).await, "pg").await;

        suspend_catalogs(&test, &["pg"]).await;
        let suspended = every_path(&connect(&test, &writer).await, "pg", &handle).await;
        let refused = open_changeset(
            &connect(&test, &writer).await,
            OpenParams {
                catalog: Some("pg".to_string()),
            },
        )
        .await
        .unwrap_err();
        let elsewhere = every_path(&connect(&test, &writer).await, "work", &in_work).await;
        let strangers = every_path(&connect(&test, &stranger).await, "pg", &handle).await;
        let admins = every_path(&connect(&test, &root).await, "pg", &roots).await;

        for (tool, kind) in &suspended {
            assert_eq!(kind, "Suspended", "{tool} on the suspended catalog");
        }
        assert!(matches!(
            refused,
            OkfError::Suspended { ref catalog, ref team, .. } if catalog == "pg" && team == SUSPENDED_TEAM
        ));
        for (tool, kind) in &elsewhere {
            assert_eq!(kind, "ok", "{tool} on the other team's catalog");
        }
        for (tool, kind) in &strangers {
            assert_ne!(
                kind, "Suspended",
                "{tool}: a key that does not reach pg never learns"
            );
        }
        assert_eq!(strangers[0].1, "CatalogNotSelected", "open_changeset");
        assert!(
            strangers[1..]
                .iter()
                .all(|(_, kind)| kind == "UnknownChangeset"),
            "{strangers:?}"
        );
        for (tool, kind) in &admins {
            assert_eq!(kind, "ok", "{tool} as an admin");
        }

        lift_suspension(&test).await;
        let lifted = pending_changes(
            &connect(&test, &writer).await,
            PendingParams {
                changeset: handle.clone(),
            },
        )
        .await;
        assert!(lifted.is_ok(), "lifting restores the key: {lifted:?}");
    }

    /// The catalogs of `listed`'s changesets, sorted, without repeats.
    fn catalogs_of(listed: &Listed) -> Vec<&str> {
        let mut names: Vec<&str> = listed
            .changesets
            .iter()
            .map(|row| row.catalog.as_str())
            .collect();
        // Rows are ordered by key and age, not catalog; `dedup` drops only
        // consecutive repeats, so sort first.
        names.sort_unstable();
        names.dedup();
        names
    }

    /// `list_changesets` without `catalog` enumerates catalogs itself
    /// (`ctx.offers`), so it bypasses `resolve`: M13 Task 4 leaves suspended
    /// catalogs out there, and naming one is refused through `resolve`.
    #[tokio::test(flavor = "multi_thread")]
    async fn list_changesets_leaves_a_suspended_catalog_out_but_to_an_admin() {
        let test = app_with(
            &[("pg", &[("a.md", NOTE)]), ("work", &[("a.md", NOTE)])],
            Config::default(),
        )
        .await;
        let writer = stored_secret(&test, "writer", Access::Write, &["pg", "work"], false).await;
        let stranger = stored_secret(&test, "stranger", Access::Write, &["work"], false).await;
        let root = stored_secret(&test, "root", Access::Admin, &[], false).await;
        open_with_edit(&connect(&test, &writer).await, "pg").await;
        open_with_edit(&connect(&test, &writer).await, "work").await;
        let all = || ListParams { catalog: None };
        let named = |catalog: &str| ListParams {
            catalog: Some(catalog.to_string()),
        };
        let on = async |secret: &str, connection: &[&str]| {
            context(
                &test,
                cached_principal(&test, secret).await,
                Some(connection),
            )
        };

        suspend_catalogs(&test, &["pg"]).await;
        let plain = list_changesets(&connect(&test, &writer).await, all())
            .await
            .unwrap();
        let mixed = list_changesets(&on(&writer, &["pg", "work"]).await, all())
            .await
            .unwrap();
        let only_suspended = list_changesets(&on(&writer, &["pg"]).await, all()).await;
        let refused = list_changesets(&connect(&test, &writer).await, named("pg")).await;
        let unreached = list_changesets(&connect(&test, &stranger).await, named("pg")).await;
        let admin = list_changesets(&connect(&test, &root).await, all())
            .await
            .unwrap();

        assert_eq!(catalogs_of(&plain), ["work"], "no connection list");
        assert_eq!(catalogs_of(&mixed), ["work"], "a mixed connection list");
        assert!(
            matches!(only_suspended, Err(OkfError::Suspended { ref catalog, .. }) if catalog == "pg"),
            "a list of only suspended catalogs is refused: {only_suspended:?}"
        );
        assert!(
            matches!(refused, Err(OkfError::Suspended { ref catalog, .. }) if catalog == "pg"),
            "naming the suspended catalog: {refused:?}"
        );
        assert!(
            matches!(unreached, Err(OkfError::CatalogNotSelected { .. })),
            "a key that does not reach pg never learns: {unreached:?}"
        );
        assert_eq!(catalogs_of(&admin), ["pg", "work"], "an admin is exempt");

        lift_suspension(&test).await;
        let lifted = list_changesets(&connect(&test, &writer).await, all())
            .await
            .unwrap();
        assert_eq!(
            catalogs_of(&lifted),
            ["pg", "work"],
            "lifting restores them"
        );
    }
}

#[cfg(test)]
#[path = "changesets/lock_order_tests.rs"]
mod lock_order_tests;
