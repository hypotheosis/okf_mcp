//! `history`: a document's revisions, from the first selected catalog that
//! has any (Decision 1); and `recent_changes`: a catalog's revisions across
//! its paths, newest first. Both show the person behind each revision
//! (`user`, `user_label`) and filter by person and time.

use std::collections::{BTreeSet, HashMap};

use chrono::{DateTime, Utc};

use axum::http::request::Parts;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router, Json};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::clock::format_timestamp;
use crate::error::OkfError;
use crate::labels::directory::{Labels, Wanted};
use crate::mcp::context::ToolContext;
use crate::mcp::{reply, OkfServer};
use crate::okf::paths::{escapes_root, normpath};
use crate::store::audit::AccessKind;
use crate::store::history::{recent_by, revisions_by, Filter, RecentCursor, RecentRow, RECENT_MAX};
use crate::store::users::{filter_id, names_by_ids, DELETED_USER_PREFIX};

/// The design's default `limit`.
const DEFAULT_LIMIT: u32 = 20;

/// The most revisions one call returns, whatever `limit` asks for, so a
/// response stays a reasonable size for an MCP client (as `search`'s cap).
const MAX_LIMIT: u32 = 200;

/// `history`'s arguments. Doc comments are the parameters' descriptions in
/// the tool's input schema (`JsonSchema` reads them).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct HistoryParams {
    /// The document: `/infra/time-server.md`, `infra/time-server`, `/log.md`.
    pub path: String,
    /// The catalog id (the `name` from `list_catalogs`; default: the first selected catalog with any revision of it; when `list_catalogs` shows an empty selection, pass `catalog`).
    pub catalog: Option<String>,
    /// At most this many revisions (default 20, at most 200; 0 counts as 1).
    pub limit: Option<u32>,
    /// Only revisions made by this person: a user's id (the `name` `whoami` shows as `user`), or, for a user since deleted, the `deleted:<id>` a revision shows. A bare value is always a name.
    pub user: Option<String>,
    /// Only revisions made at or after this time (RFC 3339, `2026-11-02T21:00:00Z`).
    pub since: Option<String>,
}

/// One revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Revision {
    /// The revision number, 1 for the first.
    pub revision: i32,
    /// `create`, `update`, `delete` or `import`.
    pub op: String,
    /// Who made it: the actor of the key, the OAuth client's name, or the user's id for a console session. It names the credential, not the person: see `user`.
    pub actor: String,
    /// The label of the actor when the actor is itself a user (a console session); null for a key or OAuth actor. The person behind any credential is `user_label`.
    pub actor_label: Option<String>,
    /// The person whose credential published it: their id. For a user since deleted this is `deleted:` and their stored `users.id`, which has no label; pass it back as the `user` filter. Null when no person is recorded (an import, a key no user owned, an older revision).
    pub user: Option<String>,
    /// The person's label: say this to people. Null with no `user`, or once that user is deleted.
    pub user_label: Option<String>,
    /// The publish message (`import` for an import).
    pub message: String,
    /// When (`2026-11-02T21:00:00Z`).
    pub at: String,
}

/// `history`'s result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct History {
    /// The catalog it came from (id).
    pub catalog: String,
    /// The catalog's label: say this to people, pass `catalog` in calls.
    pub catalog_label: String,
    /// `/`-prefixed path.
    pub path: String,
    /// Newest first.
    pub revisions: Vec<Revision>,
}

/// The stored paths a reference may mean, as `get_concept` spells them: as
/// written, then with `.md`; the leading `/` is optional. Unlike
/// `get_concept`, any file counts, not only concepts. So if a catalog has
/// both `notes` and `notes.md`, `notes` means the extensionless file (as in
/// `resolve_concept`); ask for `notes.md` to get the other.
///
/// # Arguments
/// * `reference` - the caller's path.
///
/// # Returns
/// The normalized candidates, in order, leaving out any that leave the
/// catalog.
#[tracing::instrument(level = "trace", skip_all)]
fn candidates(reference: &str) -> Vec<String> {
    let bare = reference.trim_start_matches('/');
    [normpath(bare), normpath(&format!("{bare}.md"))]
        .into_iter()
        .filter(|path| !escapes_root(path))
        .collect()
}

/// The number of revisions a call returns at most.
///
/// # Arguments
/// * `requested` - the `limit` argument, if given.
///
/// # Returns
/// [`DEFAULT_LIMIT`] when none was given, else `requested` clamped to
/// 1..=[`MAX_LIMIT`]. Never 0: with 0 every path would look as if it had no
/// history.
#[tracing::instrument(level = "trace", skip_all)]
fn revision_limit(requested: Option<u32>) -> i64 {
    // `map_or(default, f)` is Python's `default if x is None else f(x)`;
    // `clamp(lo, hi)` is `min(max(x, lo), hi)`.
    i64::from(requested.map_or(DEFAULT_LIMIT, |limit| limit.clamp(1, MAX_LIMIT)))
}

/// The people some revisions record, resolved for showing: a revision keeps
/// a `users.id`, and a response shows the user's id (`name`) and label.
#[derive(Debug, Default)]
pub(crate) struct Persons {
    /// `users.id` to `name`, for the users that still exist.
    names: HashMap<String, String>,
}

impl Persons {
    /// Looks up the users the ids name.
    ///
    /// # Arguments
    /// * `ctx` - the call's context.
    /// * `ids` - the `revisions.user_id` values of the rows to show.
    ///
    /// # Errors
    /// [`OkfError::Database`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) async fn load<'a>(
        ctx: &ToolContext,
        ids: impl IntoIterator<Item = &'a str>,
    ) -> Result<Self, OkfError> {
        // Distinct ids: one user made most of a page's revisions.
        let distinct: BTreeSet<&str> = ids.into_iter().collect();
        let ids: Vec<&str> = distinct.into_iter().collect();
        let names = names_by_ids(ctx.app().accounts().executor(), &ids).await?;
        Ok(Self { names })
    }

    /// Adds the users' names to what the response needs labels for.
    ///
    /// # Arguments
    /// * `wanted` - the slugs [`ToolContext::labels_for`] will resolve.
    #[tracing::instrument(level = "trace", skip_all)]
    pub(crate) fn want(&self, wanted: &mut Wanted) {
        for name in self.names.values() {
            wanted.user(name);
        }
    }

    /// The person a revision shows.
    ///
    /// # Arguments
    /// * `user_id` - the revision's `user_id`.
    /// * `labels` - the labels [`Persons::want`] asked for.
    ///
    /// # Returns
    /// `(user, user_label)`: the user's name and label; `deleted:<stored id>` and no
    /// label for a deleted user; both `None` with no person recorded.
    #[tracing::instrument(level = "trace", skip_all)]
    pub(crate) fn show(
        &self,
        user_id: Option<&str>,
        labels: &Labels,
    ) -> (Option<String>, Option<String>) {
        let Some(id) = user_id else {
            return (None, None);
        };
        match self.names.get(id) {
            Some(name) => (Some(name.clone()), labels.user(name).map(str::to_string)),
            None => (Some(format!("{DELETED_USER_PREFIX}{id}")), None),
        }
    }
}

/// A call's `user` and `since` arguments, resolved for the queries.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Selection {
    /// The `users.id` to match.
    pub(crate) user_id: Option<String>,
    /// The earliest time to match.
    pub(crate) since: Option<DateTime<Utc>>,
}

impl Selection {
    /// Resolves a call's filters.
    ///
    /// # Arguments
    /// * `ctx` - the call's context.
    /// * `user` - the `user` argument.
    /// * `since` - the `since` argument.
    ///
    /// # Errors
    /// [`OkfError::Refused`] for a `since` that is not an RFC 3339 time or a
    /// `user` holding a NUL; [`OkfError::Database`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) async fn of(
        ctx: &ToolContext,
        user: Option<&str>,
        since: Option<&str>,
    ) -> Result<Self, OkfError> {
        let since = since
            .map(|text| {
                DateTime::parse_from_rfc3339(text)
                    .map(|at| at.with_timezone(&Utc))
                    .map_err(|_| OkfError::Refused {
                        reason: format!("`since` must be an RFC 3339 time, not {text:?}"),
                    })
            })
            .transpose()?;
        let user_id = match user {
            Some(user) => Some(filter_id(ctx.app().accounts().executor(), user).await?),
            None => None,
        };
        Ok(Self { user_id, since })
    }

    /// Whether a filter narrows the read.
    #[tracing::instrument(level = "trace", skip_all)]
    pub(crate) fn is_filtered(&self) -> bool {
        self.user_id.is_some() || self.since.is_some()
    }

    /// The store filter these select.
    #[tracing::instrument(level = "trace", skip_all)]
    pub(crate) fn filter(&self) -> Filter<'_> {
        Filter {
            user_id: self.user_id.as_deref(),
            since: self.since,
        }
    }
}

/// A document's revisions.
///
/// # Arguments
/// * `ctx` - the call's context.
/// * `params` - the arguments.
///
/// # Returns
/// The revisions from the first catalog of the read set that has any, newest
/// first.
///
/// # Errors
/// [`OkfError::Refused`] for a bad `since`; a selection error
/// ([`ToolContext::read_set`]); [`OkfError::NotFound`] if no catalog in the
/// read set has any revision of the path matching the filters;
/// [`OkfError::StoreUnavailable`] when none of the catalogs before the first
/// unreachable store has one (plan P3); [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn history(ctx: &ToolContext, params: HistoryParams) -> Result<History, OkfError> {
    let limit = revision_limit(params.limit);
    let selection = Selection::of(ctx, params.user.as_deref(), params.since.as_deref()).await?;
    let paths = candidates(&params.path);
    let set = ctx.read_set(params.catalog.as_deref()).await?;
    for state in &set.states {
        // Admitted right before its revisions are read, so a catalog after
        // the one that answers is never recorded (plan Decision 23). A
        // `Busy` here leaves the catalogs already read recorded: they were.
        ctx.admit(state, AccessKind::Read).await?;
        for path in &paths {
            // `.await` suspends this call until Postgres answers, freeing the
            // thread for other requests meanwhile.
            let db = ctx.app().store_db(state.store()).await?;
            let rows = revisions_by(&db, state.id(), path, limit, &selection.filter()).await?;
            if rows.is_empty() {
                // Nothing matched the filters: the path still has a history
                // if an unfiltered read finds a revision, and the answer is
                // then an empty list, not "no such document".
                let exists = selection.is_filtered()
                    && !revisions_by(&db, state.id(), path, 1, &Filter::default())
                        .await?
                        .is_empty();
                if !exists {
                    continue;
                }
            }
            // `into_iter` consumes `rows`, so each row's strings move into
            // the result instead of being copied.
            let persons =
                Persons::load(ctx, rows.iter().filter_map(|row| row.user_id.as_deref())).await?;
            let mut wanted = Wanted::default();
            persons.want(&mut wanted);
            for row in &rows {
                wanted.user(&row.actor);
            }
            let labels = ctx.labels_for(&wanted).await?;
            let revisions = rows
                .into_iter()
                .map(|row| {
                    let (user, user_label) = persons.show(row.user_id.as_deref(), &labels);
                    Revision {
                        actor_label: labels.actor(&row.actor).map(str::to_string),
                        user,
                        user_label,
                        revision: row.revision,
                        op: row.op,
                        actor: row.actor,
                        message: row.message,
                        at: format_timestamp(row.created_at),
                    }
                })
                .collect();
            return Ok(History {
                catalog: state.name().to_string(),
                catalog_label: ctx.catalog_label(state.name()).await?,
                path: format!("/{path}"),
                revisions,
            });
        }
    }
    // An unreachable store before "not found" (plan P3): the catalog behind
    // it might have held the path's revisions.
    if let Some(blocked) = set.blocked {
        return Err(blocked);
    }
    Err(OkfError::NotFound { path: params.path })
}

/// `recent_changes`' arguments.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, JsonSchema)]
pub struct RecentParams {
    /// The catalog id (the `name` from `list_catalogs`; default: the first selected catalog; when `list_catalogs` shows an empty selection, pass `catalog`).
    pub catalog: Option<String>,
    /// At most this many revisions in the page (default 50, at most 200; 0 counts as 1).
    pub limit: Option<u32>,
    /// The previous page's `next_after`, to continue after it.
    pub after: Option<String>,
    /// Only revisions made by this person: a user's id (the `name` `whoami` shows as `user`), or, for a user since deleted, the `deleted:<id>` a revision shows. A bare value is always a name.
    pub user: Option<String>,
    /// Only revisions made at or after this time (RFC 3339, `2026-11-02T21:00:00Z`).
    pub since: Option<String>,
}

/// One revision in a catalog's recent changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct RecentChange {
    /// `/`-prefixed path; a deleted document's too.
    pub path: String,
    /// The path's revision number, 1 for the first.
    pub revision: i32,
    /// `create`, `update`, `delete` or `import`.
    pub op: String,
    /// Who made it: the actor of the key, the OAuth client's name, or the user's id for a console session. It names the credential, not the person: see `user`.
    pub actor: String,
    /// The label of the actor when the actor is itself a user (a console session); null for a key or OAuth actor. The person behind any credential is `user_label`.
    pub actor_label: Option<String>,
    /// The person whose credential published it: their id. For a user since deleted this is `deleted:` and their stored `users.id`, which has no label; pass it back as the `user` filter. Null when no person is recorded (an import, a key no user owned, an older revision).
    pub user: Option<String>,
    /// The person's label: say this to people. Null with no `user`, or once that user is deleted.
    pub user_label: Option<String>,
    /// The publish message (`import` for an import).
    pub message: String,
    /// When (`2026-11-02T21:00:00Z`).
    pub at: String,
}

/// `recent_changes`' result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct RecentChanges {
    /// The catalog it came from (id).
    pub catalog: String,
    /// The catalog's label: say this to people, pass `catalog` in calls.
    pub catalog_label: String,
    /// The page's revisions across every path, newest first (ties by path, then revision, both descending).
    pub changes: Vec<RecentChange>,
    /// Pass this as `after` for the next page; absent on the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(default)]
    pub next_after: Option<String>,
}

/// The page size of `recent_changes` when `limit` is left out.
pub(crate) const RECENT_DEFAULT: u32 = 50;

/// A catalog's revisions across its paths, newest first, one page.
///
/// # Arguments
/// * `ctx` - the call's context.
/// * `params` - the arguments.
///
/// # Returns
/// The page, with `next_after` when more follow.
///
/// # Errors
/// [`OkfError::Refused`] for a bad `since`; a selection or access error
/// ([`ToolContext::one`], [`ToolContext::admit`]); [`OkfError::NotFound`]
/// for an `after` that continues nothing; [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn recent_changes(
    ctx: &ToolContext,
    params: RecentParams,
) -> Result<RecentChanges, OkfError> {
    let state = ctx.one(params.catalog.as_deref()).await?;
    ctx.admit(&state, AccessKind::Read).await?;
    // The arguments are checked once the catalog is admitted, so a refusal
    // never depends on them.
    let after = params
        .after
        .as_deref()
        .map(RecentCursor::parse)
        .transpose()?;
    let selection = Selection::of(ctx, params.user.as_deref(), params.since.as_deref()).await?;
    let limit = params
        .limit
        .map_or(RECENT_DEFAULT, |limit| limit.clamp(1, RECENT_MAX));
    let page = recent_by(
        &ctx.app().store_db(state.store()).await?,
        state.id(),
        after.as_ref(),
        limit,
        &selection.filter(),
    )
    .await?;
    let next_after = page.next.as_ref().map(RecentCursor::text);
    let (rows, labels, persons) = labelled(ctx, page.rows).await?;
    let changes = rows
        .into_iter()
        .map(|row| {
            let (user, user_label) = persons.show(row.user_id.as_deref(), &labels);
            RecentChange {
                path: format!("/{}", row.path),
                revision: row.revision,
                op: row.op,
                actor_label: labels.actor(&row.actor).map(str::to_string),
                actor: row.actor,
                user,
                user_label,
                message: row.message,
                at: format_timestamp(row.created_at),
            }
        })
        .collect();
    Ok(RecentChanges {
        catalog: state.name().to_string(),
        catalog_label: ctx.catalog_label(state.name()).await?,
        changes,
        next_after,
    })
}

/// The labels and persons a page of recent rows shows.
///
/// # Arguments
/// * `ctx` - the call's context.
/// * `rows` - the page.
///
/// # Returns
/// The rows back, with the labels of their actors and users, and the users.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn labelled(
    ctx: &ToolContext,
    rows: Vec<RecentRow>,
) -> Result<(Vec<RecentRow>, Labels, Persons), OkfError> {
    let persons = Persons::load(ctx, rows.iter().filter_map(|row| row.user_id.as_deref())).await?;
    let mut wanted = Wanted::default();
    persons.want(&mut wanted);
    for row in &rows {
        wanted.user(&row.actor);
    }
    let labels = ctx.labels_for(&wanted).await?;
    Ok((rows, labels, persons))
}

// As in `catalogs.rs`: the macro turns this block's `#[tool]` methods into a
// router named `history_router`, joined in `OkfServer::tools`.
#[tool_router(router = history_router, vis = "pub(crate)")]
impl OkfServer {
    /// The `history` tool.
    // The return type is spelled out (not `ToolResult<...>`) so the macro
    // sees `Json<...>` and builds the output schema.
    #[tool(
        title = "Document History",
        description = "A document's revisions, newest first: revision, op (create, update, \
                       delete, import), actor, user, user_label, message and time. `user` is the \
                       person whose credential published the revision (null for an import or a \
                       key no user owned); `actor` names the credential. Deleted documents keep \
                       their history; log.md, indexes and other files have one too. Without \
                       `catalog`, the first selected catalog with any revision of the path \
                       answers. At most `limit` revisions (default 20, at most 200); filter with \
                       `user` (a user's id, or `deleted:<id>` as a revision shows it) and `since` (RFC 3339, inclusive). For a catalog's \
                       changes across documents, use `recent_changes`.",
        annotations(
            title = "Document History",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn history(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(params): Parameters<HistoryParams>,
    ) -> Result<Json<History>, CallToolResult> {
        let ctx = self.context(&parts)?;
        reply(history(&ctx, params).await)
    }

    /// The `recent_changes` tool.
    #[tool(
        title = "Recent Changes",
        description = "A catalog's published revisions across every document, newest first: \
                       path, revision, op (create, update, delete, import), actor, user, \
                       user_label, message and time. `user` is the person whose credential \
                       published it (null for an import or a key no user owned); `actor` names \
                       the credential. Filter with `user` (a user's id, or `deleted:<id>` as a revision shows it) and `since` (RFC 3339, \
                       inclusive). A page holds `limit` revisions (default 50, at most 200); pass \
                       `next_after` as `after` for the next page. Uses `catalog`, else the first \
                       selected catalog. Published history only: an open changeset shows in \
                       `pending_changes`.",
        annotations(
            title = "Recent Changes",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn recent_changes(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(params): Parameters<RecentParams>,
    ) -> Result<Json<RecentChanges>, CallToolResult> {
        let ctx = self.context(&parts)?;
        reply(recent_changes(&ctx, params).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::mcp::testing::{app_with, context, principal};

    fn params(path: &str) -> HistoryParams {
        HistoryParams {
            path: path.to_string(),
            catalog: None,
            limit: None,
            user: None,
            since: None,
        }
    }

    #[test]
    fn the_limit_defaults_to_20_counts_0_as_1_and_is_capped_at_200() {
        let limits = [None, Some(0), Some(5), Some(200), Some(u32::MAX)].map(revision_limit);

        assert_eq!(limits, [20, 1, 5, 200, 200]);
    }

    #[test]
    fn candidates_add_md_and_drop_paths_that_leave_the_catalog() {
        let plain = candidates("/infra/b");
        let escaping = candidates("../x");

        assert_eq!(plain, ["infra/b", "infra/b.md"]);
        assert!(escaping.is_empty());
    }

    #[test]
    fn the_server_lists_history_with_an_object_output_schema() {
        let tools = OkfServer::tools().list_all();

        let listed = tools.iter().find(|tool| tool.name == "history");

        let schema = listed
            .and_then(|tool| tool.output_schema.as_ref())
            .expect("history is registered with an output schema");
        assert_eq!(schema["type"], "object");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn history_finds_the_path_in_the_read_set_and_spells_paths_like_get_concept() {
        let test = app_with(
            &[
                ("pg", &[("a.md", "# A\n")]),
                ("work", &[("infra/b.md", "# B\n"), ("log.md", "# Log\n")]),
            ],
            Config::default(),
        )
        .await;
        let ctx = context(&test, principal(&[], None), Some(&["pg", "work"]));

        let found = history(&ctx, params("infra/b")).await.unwrap();
        let log = history(&ctx, params("/log.md")).await.unwrap();
        let missing = history(&ctx, params("nowhere")).await.unwrap_err();
        let elsewhere = HistoryParams {
            catalog: Some("pg".to_string()),
            ..params("infra/b")
        };
        let not_in_pg = history(&ctx, elsewhere).await.unwrap_err();
        let zero = HistoryParams {
            limit: Some(0),
            ..params("infra/b")
        };
        let zero = history(&ctx, zero).await.unwrap();

        assert_eq!(
            (found.catalog.as_str(), found.path.as_str()),
            ("work", "/infra/b.md")
        );
        assert_eq!(found.revisions.len(), 1);
        assert_eq!(found.revisions[0].op, "import");
        assert_eq!(found.revisions[0].actor, "tester");
        assert!(found.revisions[0].at.ends_with('Z'));
        assert_eq!(log.path, "/log.md");
        assert_eq!(zero.revisions.len(), 1);
        assert!(matches!(missing, OkfError::NotFound { .. }));
        assert!(matches!(not_in_pg, OkfError::NotFound { .. }));
    }
}
