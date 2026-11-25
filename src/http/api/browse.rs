//! Browsing under `/api/v1` (web console design, "API": Browse): a catalog's
//! tree, a concept with its body rendered to safe HTML, its links, its
//! history, one revision with its diff, the non-concept documents, and the
//! catalog's recent changes (every document's revisions, newest first).
//!
//! An axum wildcard (`{*path}`) must be the last segment of a route, so the
//! per-concept routes put their kind before the path
//! (`/catalogs/{catalog}/links/{*path}`), not after it as the design wrote
//! (`/concepts/{*path}/links`).

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};

use crate::app::App;
use crate::auth::Principal;
use crate::catalog::Loaded;
use crate::clock::format_timestamp;
use crate::error::OkfError;
use crate::http::api::context;
use crate::http::requester::Requester;
use crate::http::AppError;
use crate::mcp::blocking;
use crate::mcp::changesets::unified_diff;
use crate::mcp::documents::{
    get_concept, read_document, text_field, Concept, Document, GetConceptParams, InboundLink,
    OutboundLink, ReadDocumentParams,
};
use crate::mcp::history::{history, labelled, History, HistoryParams, Selection, RECENT_DEFAULT};
use crate::okf::outline::Outline;
use crate::okf::pystr::universal_newlines;
use crate::okf::snapshot::DocKind;
use crate::render::{to_html, Page};
use crate::store::audit::AccessKind;
use crate::store::history::{content_at, recent_by, RecentCursor};
use crate::store::keys::Access;

/// This area's routes.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "router builder: runs once at startup, outside any request, so a span would be an orphan root"
    )
)]
pub(crate) fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/catalogs/{catalog}/tree", get(tree))
        .route("/catalogs/{catalog}/concepts/{*path}", get(concept))
        .route("/catalogs/{catalog}/links/{*path}", get(links))
        .route("/catalogs/{catalog}/history/{*path}", get(revisions))
        .route(
            "/catalogs/{catalog}/revisions/{revision}/{*path}",
            get(revision),
        )
        .route("/catalogs/{catalog}/documents/{*path}", get(document))
        .route("/catalogs/{catalog}/recent-changes", get(recent_changes))
}

/// One document in a catalog's tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct TreeDocument {
    /// `/`-prefixed path.
    pub path: String,
    /// `concept`, `index`, `log` or `machinery` (the root `AGENTS.md`).
    pub kind: &'static str,
    /// Its title, if it has one.
    pub title: Option<String>,
    /// Its frontmatter `description`, when that is a string.
    pub description: Option<String>,
}

/// `GET /catalogs/{catalog}/tree`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Tree {
    /// The catalog.
    pub catalog: String,
    /// Every document the console can open, in the catalog's walk order.
    /// The client builds the folders from the paths.
    pub documents: Vec<TreeDocument>,
}

/// `GET /catalogs/{catalog}/concepts/{*path}`: `get_concept`'s result with
/// the rendered body.
// No `Eq`: `Concept` holds JSON frontmatter, which is only `PartialEq`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct ConceptView {
    /// The concept, as `get_concept` returns it with `raw`; `outline` is
    /// always present (the whole outline, also within the limit).
    // `flatten` writes the concept's fields beside `body_html`.
    #[serde(flatten)]
    pub concept: Concept,
    /// The body as safe HTML ([`crate::render`]); `None` when the body is over
    /// `concept_outline_chars` and `full` was not passed.
    pub body_html: Option<String>,
}

/// `GET /catalogs/{catalog}/links/{*path}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ConceptLinks {
    /// The catalog.
    pub catalog: String,
    /// `/`-prefixed path of the concept.
    pub path: String,
    /// Its outbound links, in-page links left out.
    pub links: Vec<OutboundLink>,
    /// The documents linking to it.
    pub backlinks: Vec<InboundLink>,
}

/// `GET /catalogs/{catalog}/revisions/{revision}/{*path}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct RevisionView {
    /// The catalog.
    pub catalog: String,
    /// `/`-prefixed path.
    pub path: String,
    /// The revision shown.
    pub revision: i32,
    /// `create`, `update`, `delete` or `import`.
    pub op: String,
    /// The file at this revision; `None` for a deletion or bytes that are not UTF-8.
    pub text: Option<String>,
    /// A unified diff from the previous revision (from nothing for revision 1).
    pub diff: String,
}

/// `GET /catalogs/{catalog}/documents/{*path}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct DocumentView {
    /// The file, as `read_document` returns it (`catalog`, `path`, `text`).
    #[serde(flatten)]
    pub document: Document,
    /// The body as safe HTML.
    pub body_html: String,
}

/// `?full=` on the concept route.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
struct FullQuery {
    /// Render the whole body even over `concept_outline_chars`.
    // `default`: an absent parameter is `false`.
    #[serde(default)]
    full: bool,
}

/// `?limit=&user=&since=` on the history route.
#[derive(Debug, Clone, Default, Deserialize)]
struct LimitQuery {
    /// At most this many revisions (as `history`: default 20, at most 200).
    limit: Option<u32>,
    /// Only this person's revisions (as `history`'s `user`).
    user: Option<String>,
    /// Only revisions at or after this RFC 3339 time (as `history`'s `since`).
    since: Option<String>,
}

/// `?limit=&after=&user=&since=` on the recent-changes route.
#[derive(Debug, Clone, Default, Deserialize)]
struct RecentQuery {
    /// The page size, 1..=200 (default 50); outside it is a 409.
    limit: Option<u32>,
    /// The previous page's `next_after`; an unknown or malformed one is a 404.
    after: Option<String>,
    /// Only this person's revisions: a user's id, or the `deleted:<id>` a revision shows for a deleted user.
    user: Option<String>,
    /// Only revisions at or after this RFC 3339 time; a bad one is a 409.
    since: Option<String>,
}

/// One revision in `GET /catalogs/{catalog}/recent-changes`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct RecentChange {
    /// `/`-prefixed path; a deleted document's too.
    pub path: String,
    /// The document's title as the catalog has it now; `None` for a
    /// deletion, a document gone since, or one without a title.
    pub title: Option<String>,
    /// Its frontmatter `description` now, when that is a string; `None` as
    /// for `title`.
    pub description: Option<String>,
    /// The path's revision number, 1 for the first.
    pub revision: i32,
    /// `create`, `update`, `delete` or `import`.
    pub op: String,
    /// Who published it.
    pub actor: String,
    /// The label of the actor when the actor is itself a user (a console
    /// session); `None` for a key or OAuth actor. The person behind any
    /// credential is `user_label`.
    pub actor_label: Option<String>,
    /// The person whose credential published it: their user id, or the
    /// `deleted:<stored id>` of a deleted user; `None` when no person is recorded.
    pub user: Option<String>,
    /// The person's label; `None` with no `user`, or once that user is deleted.
    pub user_label: Option<String>,
    /// The publish message (`import` for an import).
    pub message: String,
    /// When, as the `history` tool writes times.
    pub at: String,
}

/// `GET /catalogs/{catalog}/recent-changes`: one page, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct RecentChanges {
    /// The catalog.
    pub catalog: String,
    /// Its label.
    pub catalog_label: String,
    /// The page's revisions, newest first; ties by path, then revision,
    /// both descending.
    pub changes: Vec<RecentChange>,
    /// `after` for the next page; `None` on the last.
    pub next_after: Option<String>,
}

/// The tree's name for a document kind, or `None` for a file the console
/// cannot open: `read_document` serves only the root `AGENTS.md` among the
/// machinery files.
///
/// # Arguments
/// * `kind` - the document's kind.
/// * `path` - its catalog-relative path.
#[tracing::instrument(level = "trace", skip_all)]
fn kind_name(kind: DocKind, path: &str) -> Option<&'static str> {
    match kind {
        DocKind::Concept => Some("concept"),
        DocKind::Index => Some("index"),
        DocKind::Log => Some("log"),
        // `bool::then_some(x)` is `Some(x)` when true, else `None`.
        DocKind::Machinery => (path == "AGENTS.md").then_some("machinery"),
    }
}

/// `GET /catalogs/{catalog}/tree`.
///
/// # Errors
/// 404 for a catalog the key does not reach.
#[tracing::instrument(level = "debug", skip_all)]
async fn tree(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    requester: Requester,
    Path(catalog): Path<String>,
) -> Result<Json<Tree>, AppError> {
    let ctx = context(&app, principal, requester);
    let state = ctx.one(Some(&catalog)).await?;
    // The tree lists the catalog's documents: a read of its contents.
    ctx.admit(&state, AccessKind::Read).await?;
    let loaded = ctx.snapshot(&state);
    let documents = loaded
        .snapshot()
        .docs()
        .iter()
        // `filter_map` keeps the `Some`s; `?` inside the closure skips a doc.
        .filter_map(|doc| {
            Some(TreeDocument {
                path: format!("/{}", doc.path()),
                kind: kind_name(doc.kind(), doc.path())?,
                title: doc.title().map(str::to_string),
                description: text_field(doc, "description"),
            })
        })
        .collect();
    Ok(Json(Tree {
        catalog: state.name().to_string(),
        documents,
    }))
}

/// `GET /catalogs/{catalog}/concepts/{*path}?full=`: `get_concept` with
/// `raw`, plus `body_html` and the whole outline.
///
/// # Errors
/// As `get_concept`: 404 for an unreached catalog or a missing concept, 400
/// for a path that is not a concept.
#[tracing::instrument(level = "debug", skip_all)]
async fn concept(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    requester: Requester,
    Path((catalog, path)): Path<(String, String)>,
    Query(query): Query<FullQuery>,
) -> Result<Json<ConceptView>, AppError> {
    let ctx = context(&app, principal, requester);
    let concept = get_concept(
        &ctx,
        GetConceptParams {
            path,
            catalog: Some(catalog),
            links: false,
            raw: true,
            section: None,
            full: query.full,
            changeset: None,
        },
    )
    .await?;
    let state = ctx.one(Some(&concept.catalog)).await?;
    let loaded = ctx.snapshot(&state);
    // Rendering is CPU work proportional to the body: off the async threads.
    // `move` hands `concept` and `loaded` (an `Arc`) to the blocking thread.
    Ok(Json(blocking(move || with_html(concept, &loaded)).await))
}

/// Adds the rendered body and the whole outline to a concept.
///
/// # Arguments
/// * `concept` - `get_concept`'s result, read with `raw`.
/// * `loaded` - its catalog, for resolving links.
///
/// # Returns
/// The view. `raw` is present exactly when the body is (within the limit, or
/// `full`), so a concept without `raw` keeps `get_concept`'s outline and gets
/// no `body_html`.
#[tracing::instrument(level = "trace", skip_all)]
fn with_html(mut concept: Concept, loaded: &Loaded) -> ConceptView {
    // `let ... else`: bind the `Some` value or return early.
    let Some(raw) = concept.raw.as_deref() else {
        return ConceptView {
            concept,
            body_html: None,
        };
    };
    // The outline's view of the file: `\r\n` read as `\n`, as the snapshot reads it.
    let text = universal_newlines(raw);
    let page = Page::new(
        &concept.catalog,
        concept.path.trim_start_matches('/'),
        loaded.workspace(),
    );
    let body_html = to_html(&text, &page);
    let outline = Outline::of(&text).entries();
    // The borrows of `concept.raw` (through `text`) end above, so the
    // concept can be changed and moved now.
    concept.outline = Some(outline);
    ConceptView {
        concept,
        body_html: Some(body_html),
    }
}

/// `GET /catalogs/{catalog}/links/{*path}`.
///
/// # Errors
/// As `get_concept`.
#[tracing::instrument(level = "debug", skip_all)]
async fn links(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    requester: Requester,
    Path((catalog, path)): Path<(String, String)>,
) -> Result<Json<ConceptLinks>, AppError> {
    let ctx = context(&app, principal, requester);
    let concept = get_concept(
        &ctx,
        GetConceptParams {
            path,
            catalog: Some(catalog),
            links: true,
            raw: false,
            section: None,
            full: false,
            changeset: None,
        },
    )
    .await?;
    Ok(Json(ConceptLinks {
        catalog: concept.catalog,
        path: concept.path,
        // With `links: true` both are `Some`; `unwrap_or_default` keeps the
        // handler total anyway.
        links: concept.links.unwrap_or_default(),
        backlinks: concept.backlinks.unwrap_or_default(),
    }))
}

/// `GET /catalogs/{catalog}/history/{*path}?limit=`: the `history` tool.
///
/// # Errors
/// As `history`: 404 for an unreached catalog or a path without revisions.
#[tracing::instrument(level = "debug", skip_all)]
async fn revisions(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    requester: Requester,
    Path((catalog, path)): Path<(String, String)>,
    Query(query): Query<LimitQuery>,
) -> Result<Json<History>, AppError> {
    let ctx = context(&app, principal, requester);
    let found = history(
        &ctx,
        HistoryParams {
            path,
            catalog: Some(catalog),
            limit: query.limit,
            user: query.user,
            since: query.since,
        },
    )
    .await?;
    Ok(Json(found))
}

/// `GET /catalogs/{catalog}/revisions/{revision}/{*path}`: the file at a
/// revision and a unified diff against the revision before it.
///
/// # Errors
/// 404 for an unreached catalog, a path without revisions, or a revision
/// the path never had; 400 for a revision that is not a number.
#[tracing::instrument(level = "debug", skip_all)]
async fn revision(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    requester: Requester,
    Path((catalog, revision, path)): Path<(String, i32, String)>,
) -> Result<Json<RevisionView>, AppError> {
    // This reads the store directly, so it checks access itself, the unlock
    // included (before the principal moves into the context).
    let row = principal
        .reached_row(app.directory(), Access::Read, &catalog)
        .await?;
    principal.require_contents(Access::Read, &row, app.clock().now())?;
    let ctx = context(&app, principal, requester);
    // `history` resolves the path as the History tab spelled it (`.md`
    // optional) and answers 404 for a path that never existed.
    let found = history(
        &ctx,
        HistoryParams {
            path,
            catalog: Some(catalog),
            limit: Some(1),
            user: None,
            since: None,
        },
    )
    .await?;
    let state = ctx.one(Some(&found.catalog)).await?;
    let stored = found.path.trim_start_matches('/');
    let current = content_at(
        &app.store_db(state.store()).await?,
        state.id(),
        stored,
        revision,
    )
    .await?
    .ok_or_else(|| OkfError::NotFound {
        path: format!("{} (revision {revision})", found.path),
    })?;
    let previous = if revision > 1 {
        // `and_then`: a missing row or a deletion both mean "no text before".
        content_at(
            &app.store_db(state.store()).await?,
            state.id(),
            stored,
            revision - 1,
        )
        .await?
        .and_then(|row| row.content)
    } else {
        None
    };
    let diff = unified_diff(stored, previous.as_deref(), current.content.as_deref());
    // `.ok()` turns bytes that are not UTF-8 into `None`, like a deletion.
    let text = current
        .content
        .and_then(|bytes| String::from_utf8(bytes).ok());
    Ok(Json(RevisionView {
        catalog: found.catalog,
        path: found.path,
        revision,
        op: current.op,
        text,
        diff,
    }))
}

/// `GET /catalogs/{catalog}/documents/{*path}`: `read_document` (any
/// folder's `index.md` or `log.md`, the root `AGENTS.md`) with `body_html`.
///
/// # Errors
/// As `read_document`: 400 for any other path, 404 for an unreached catalog
/// or a missing file.
#[tracing::instrument(level = "debug", skip_all)]
async fn document(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    requester: Requester,
    Path((catalog, path)): Path<(String, String)>,
) -> Result<Json<DocumentView>, AppError> {
    let ctx = context(&app, principal, requester);
    let document = read_document(
        &ctx,
        ReadDocumentParams {
            path: Some(path),
            catalog: Some(catalog),
            changeset: None,
        },
    )
    .await?;
    let state = ctx.one(Some(&document.catalog)).await?;
    let loaded = ctx.snapshot(&state);
    let view = blocking(move || {
        // A block, so the borrow of `document` ends before it moves below.
        let body_html = {
            let text = universal_newlines(&document.text);
            let page = Page::new(
                &document.catalog,
                document.path.trim_start_matches('/'),
                loaded.workspace(),
            );
            to_html(&text, &page)
        };
        DocumentView {
            document,
            body_html,
        }
    })
    .await;
    Ok(Json(view))
}

/// `GET /catalogs/{catalog}/recent-changes?limit=&after=`: the revisions
/// published in the catalog, every document's, newest first (published
/// history only: no open changeset shows). Read-only.
///
/// # Errors
/// As the revision route: 404 for a catalog the credential does not reach
/// or that does not exist, 403 for one it may not read now (suspended,
/// locked); then 409 for a `limit` outside 1..=200 and 404 for an unknown
/// or malformed `after`.
#[tracing::instrument(level = "debug", skip_all)]
async fn recent_changes(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    requester: Requester,
    Path(catalog): Path<String>,
    Query(query): Query<RecentQuery>,
) -> Result<Json<RecentChanges>, AppError> {
    // This reads the store directly, so it checks access as the revision
    // route does: the unlock first (before the principal moves into the
    // context), then the catalog's selection and its admission, which also
    // binds the read to the incarnation admitted.
    let row = principal
        .reached_row(app.directory(), Access::Read, &catalog)
        .await?;
    principal.require_contents(Access::Read, &row, app.clock().now())?;
    let ctx = context(&app, principal, requester);
    let state = ctx.one(Some(&catalog)).await?;
    ctx.admit(&state, AccessKind::Read).await?;
    // The parameters are checked only once the catalog is admitted, so a
    // refusal never depends on them.
    // `transpose` turns Option<Result<..>> into Result<Option<..>> for `?`.
    let after = query
        .after
        .as_deref()
        .map(RecentCursor::parse)
        .transpose()?;
    let limit = query.limit.unwrap_or(RECENT_DEFAULT);
    let selection = Selection::of(&ctx, query.user.as_deref(), query.since.as_deref()).await?;
    let page = recent_by(
        &app.store_db(state.store()).await?,
        state.id(),
        after.as_ref(),
        limit,
        &selection.filter(),
    )
    .await?;
    let (rows, labels, persons) = labelled(&ctx, page.rows).await?;
    // Titles and descriptions come from the published snapshot, read now:
    // the history rows hold no content.
    let loaded = ctx.snapshot(&state);
    let snapshot = loaded.snapshot();
    let changes = rows
        .into_iter()
        .map(|row| {
            let (user, user_label) = persons.show(row.user_id.as_deref(), &labels);
            // A deletion names no document, even if one was created again
            // at the path since.
            let doc = if row.op == "delete" {
                None
            } else {
                snapshot.get(&row.path)
            };
            RecentChange {
                path: format!("/{}", row.path),
                title: doc.and_then(|doc| doc.title().map(str::to_string)),
                description: doc.and_then(|doc| text_field(doc, "description")),
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
    Ok(Json(RecentChanges {
        catalog: state.name().to_string(),
        catalog_label: ctx.catalog_label(state.name()).await?,
        changes,
        next_after: page.next.as_ref().map(RecentCursor::text),
    }))
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::{json, Value};

    use crate::archive::files;
    use crate::config::Config;
    use crate::http::api::testing::{app, assert_matrix, keys, send, Row, MAIN};
    use crate::mcp::testing::app_with;
    use crate::store::audit::AuditActor;
    use crate::store::import::{import, ImportOptions, ImportScope};
    use crate::store::users::AdminChange;

    /// The `main` files with `systems/alpha.md`'s last line changed.
    fn edited_main() -> Vec<(&'static str, String)> {
        MAIN.iter()
            .map(|(path, text)| {
                let text = if *path == "systems/alpha.md" {
                    text.replace("Nothing yet.", "Two settings.")
                } else {
                    (*text).to_string()
                };
                (*path, text)
            })
            .collect()
    }

    /// The entry for `path` in a tree response.
    fn entry<'a>(tree: &'a Value, path: &str) -> &'a Value {
        tree["documents"]
            .as_array()
            .expect("documents")
            .iter()
            .find(|doc| doc["path"] == path)
            .unwrap_or_else(|| panic!("{path} in {tree}"))
    }

    #[tokio::test]
    async fn every_browse_route_needs_read_on_its_catalog() {
        let test = app().await;
        let rows: Vec<Row> = [
            "/api/v1/catalogs/main/tree",
            "/api/v1/catalogs/main/concepts/systems/alpha.md",
            "/api/v1/catalogs/main/links/systems/alpha.md",
            "/api/v1/catalogs/main/history/systems/alpha.md",
            "/api/v1/catalogs/main/revisions/1/systems/alpha.md",
            "/api/v1/catalogs/main/documents/systems/index.md",
            "/api/v1/catalogs/main/recent-changes",
        ]
        .into_iter()
        .map(|uri| Row {
            method: "GET",
            uri: uri.to_string(),
            body: None,
            read: 200,
            write: 200,
            admin: 200,
            stranger: 404,
        })
        .collect();

        assert_matrix(&test, &rows).await;
    }

    #[tokio::test]
    async fn the_tree_lists_concepts_indexes_the_log_and_agents_with_titles() {
        let test = app().await;
        let keys = keys(&test).await;

        let (status, tree) = send(
            &test,
            "GET",
            "/api/v1/catalogs/main/tree",
            Some(&keys.read),
            None,
        )
        .await;

        assert_eq!(status.as_u16(), 200, "{tree}");
        assert_eq!(tree["catalog"], "main");
        assert_eq!(
            entry(&tree, "/systems/alpha.md"),
            &json!({"path": "/systems/alpha.md", "kind": "concept", "title": "Alpha", "description": null})
        );
        assert_eq!(entry(&tree, "/systems/index.md")["kind"], "index");
        assert_eq!(entry(&tree, "/log.md")["kind"], "log");
        assert_eq!(entry(&tree, "/AGENTS.md")["kind"], "machinery");
        assert_eq!(tree["documents"].as_array().map(Vec::len), Some(MAIN.len()));
    }

    #[tokio::test]
    async fn the_tree_gives_each_document_its_description() {
        let test = app_with(
            &[
                (
                    "main",
                    &[
                        ("index.md", "---\ntitle: Main\ndescription: Everything we keep.\n---\n# Main\n"),
                        ("systems/alpha.md", "---\ntype: system\ntitle: Alpha\ndescription: Runs the jobs.\n---\n# Alpha\n"),
                        ("systems/beta.md", "---\ntype: system\ntitle: Beta\n---\n# Beta\n"),
                        ("systems/gamma.md", "---\ntitle: Gamma\ndescription: [not, a, string]\n---\n# Gamma\n"),
                    ],
                ),
                ("other", &[("index.md", "# Other\n")]),
            ],
            Config::default(),
        )
        .await;
        let keys = keys(&test).await;

        let (status, tree) = send(
            &test,
            "GET",
            "/api/v1/catalogs/main/tree",
            Some(&keys.read),
            None,
        )
        .await;

        assert_eq!(status.as_u16(), 200, "{tree}");
        assert_eq!(
            entry(&tree, "/index.md")["description"],
            "Everything we keep."
        );
        assert_eq!(
            entry(&tree, "/systems/alpha.md")["description"],
            "Runs the jobs."
        );
        // No description, or one that is not a string: null.
        assert_eq!(entry(&tree, "/systems/beta.md")["description"], Value::Null);
        assert_eq!(
            entry(&tree, "/systems/gamma.md")["description"],
            Value::Null
        );
    }

    #[tokio::test]
    async fn a_concept_comes_with_its_rendered_body_raw_text_and_outline() {
        let test = app().await;
        let keys = keys(&test).await;

        let (status, concept) = send(
            &test,
            "GET",
            "/api/v1/catalogs/main/concepts/systems/alpha.md",
            Some(&keys.read),
            None,
        )
        .await;

        assert_eq!(status.as_u16(), 200, "{concept}");
        assert_eq!(concept["path"], "/systems/alpha.md");
        assert_eq!(concept["frontmatter"]["title"], "Alpha");
        let html = concept["body_html"].as_str().expect("body_html");
        assert!(
            html.contains(r#"<h1 id="section-alpha" data-slug="alpha">Alpha</h1>"#),
            "{html}"
        );
        assert!(
            html.contains(r##"<a href="#/browse/main/systems/beta.md">Beta</a>"##),
            "{html}"
        );
        assert!(!html.contains("type: system"), "no frontmatter in {html}");
        assert!(concept["raw"]
            .as_str()
            .expect("raw")
            .starts_with("---\ntype: system\n"));
        let slugs: Vec<&str> = concept["outline"]
            .as_array()
            .expect("outline")
            .iter()
            .filter_map(|heading| heading["slug"].as_str())
            .collect();
        assert_eq!(slugs, ["alpha", "config"]);
    }

    #[tokio::test]
    async fn a_body_over_the_limit_is_outlined_until_full_is_asked_for() {
        let config = Config {
            concept_outline_chars: 10,
            ..Config::default()
        };
        let test = app_with(
            &[("main", MAIN), ("other", &[("index.md", "# Other\n")])],
            config,
        )
        .await;
        let keys = keys(&test).await;
        let uri = "/api/v1/catalogs/main/concepts/systems/alpha.md";

        let (_, outlined) = send(&test, "GET", uri, Some(&keys.read), None).await;
        let (_, full) = send(
            &test,
            "GET",
            &format!("{uri}?full=true"),
            Some(&keys.read),
            None,
        )
        .await;

        assert_eq!(outlined["truncated"], true);
        assert_eq!(outlined["body_html"], Value::Null);
        assert!(outlined.get("raw").is_none(), "{outlined}");
        assert_eq!(outlined["outline"][1]["slug"], "config");
        assert_eq!(full["truncated"], false);
        assert!(full["body_html"]
            .as_str()
            .is_some_and(|html| html.contains("Nothing yet.")));
    }

    #[tokio::test]
    async fn links_list_targets_and_backlinks() {
        let test = app().await;
        let keys = keys(&test).await;

        let (_, alpha) = send(
            &test,
            "GET",
            "/api/v1/catalogs/main/links/systems/alpha.md",
            Some(&keys.read),
            None,
        )
        .await;
        let (_, beta) = send(
            &test,
            "GET",
            "/api/v1/catalogs/main/links/systems/beta.md",
            Some(&keys.read),
            None,
        )
        .await;

        assert_eq!(alpha["links"][0]["target"], "/systems/beta.md");
        assert_eq!(alpha["links"][0]["broken"], false);
        let sources: Vec<&str> = beta["backlinks"]
            .as_array()
            .expect("backlinks")
            .iter()
            .filter_map(|link| link["path"].as_str())
            .collect();
        assert!(sources.contains(&"/systems/alpha.md"), "{beta}");
        assert!(sources.contains(&"/systems/index.md"), "{beta}");
    }

    #[tokio::test]
    async fn history_lists_revisions_and_a_revision_diffs_against_the_one_before() {
        let test = app().await;
        let keys = keys(&test).await;
        let edited = edited_main();
        let entries: Vec<(&str, &str)> = edited
            .iter()
            .map(|(path, text)| (*path, text.as_str()))
            .collect();
        let options = ImportOptions {
            replace: true,
            scope: ImportScope::Everything,
            expect_generation: None,
            expect_catalog_id: None,
            actor: "editor",
            audit: &crate::store::audit::AuditActor::cli(),
            now: Utc::now(),
        };
        import(&test.app.main_db().await, "main", files(&entries), &options)
            .await
            .expect("import the edit");

        let (status, listed) = send(
            &test,
            "GET",
            "/api/v1/catalogs/main/history/systems/alpha.md",
            Some(&keys.read),
            None,
        )
        .await;
        let (_, second) = send(
            &test,
            "GET",
            "/api/v1/catalogs/main/revisions/2/systems/alpha.md",
            Some(&keys.read),
            None,
        )
        .await;
        let (_, first) = send(
            &test,
            "GET",
            "/api/v1/catalogs/main/revisions/1/systems/alpha.md",
            Some(&keys.read),
            None,
        )
        .await;
        let (missing, _) = send(
            &test,
            "GET",
            "/api/v1/catalogs/main/revisions/9/systems/alpha.md",
            Some(&keys.read),
            None,
        )
        .await;
        let (bad, _) = send(
            &test,
            "GET",
            "/api/v1/catalogs/main/revisions/latest/systems/alpha.md",
            Some(&keys.read),
            None,
        )
        .await;

        assert_eq!(status.as_u16(), 200, "{listed}");
        assert_eq!(listed["revisions"][0]["revision"], 2);
        assert_eq!(listed["revisions"][0]["actor"], "editor");
        assert_eq!(second["op"], "import");
        let diff = second["diff"].as_str().expect("diff");
        assert!(
            diff.contains("-Nothing yet.\n") && diff.contains("+Two settings.\n"),
            "{diff}"
        );
        assert!(second["text"]
            .as_str()
            .is_some_and(|text| text.contains("Two settings.")));
        assert!(first["diff"]
            .as_str()
            .is_some_and(|diff| diff.starts_with("--- /dev/null")));
        assert_eq!(missing.as_u16(), 404);
        assert_eq!(bad.as_u16(), 400);
    }

    #[tokio::test]
    async fn documents_render_indexes_and_refuse_concepts() {
        let test = app().await;
        let keys = keys(&test).await;

        let (status, index) = send(
            &test,
            "GET",
            "/api/v1/catalogs/main/documents/systems/index.md",
            Some(&keys.read),
            None,
        )
        .await;
        let (concept, _) = send(
            &test,
            "GET",
            "/api/v1/catalogs/main/documents/systems/alpha.md",
            Some(&keys.read),
            None,
        )
        .await;
        let (unknown, _) = send(
            &test,
            "GET",
            "/api/v1/catalogs/main/concepts/systems/gamma.md",
            Some(&keys.read),
            None,
        )
        .await;

        assert_eq!(status.as_u16(), 200, "{index}");
        assert_eq!(
            index["text"],
            "# Systems\n\n- [Alpha](alpha.md)\n- [Beta](beta.md)\n"
        );
        let html = index["body_html"].as_str().expect("body_html");
        assert!(
            html.contains(r##"href="#/browse/main/systems/alpha.md""##),
            "{html}"
        );
        assert_eq!(concept.as_u16(), 400);
        assert_eq!(unknown.as_u16(), 404);
    }

    /// M13 Task 3: the revision route reads the store directly and checks
    /// access itself, so it refuses a suspended catalog through
    /// `Principal::require`: to member keys and member sessions, not to an
    /// admin key or an admin session, and never to a key that does not reach
    /// it (404). Export stays open to the member session.
    #[tokio::test]
    async fn a_revision_of_a_suspended_catalog_is_refused_to_members_only() {
        use crate::http::api::testing::{assert_answers, Browser, Expect, Payload};
        use crate::mcp::testing::suspend_catalogs;
        let test = app().await;
        let keys = keys(&test).await;
        suspend_catalogs(&test, &["main"]).await;
        let uri = "/api/v1/catalogs/main/revisions/1/index.md";
        let expect = |who, key, status, error| Expect {
            method: "GET",
            uri,
            body: None,
            key,
            who,
            status,
            error,
        };

        assert_answers(
            &test,
            &[
                expect("read", &keys.read, 403, Some("Suspended")),
                expect("write", &keys.write, 403, Some("Suspended")),
                expect("stranger", &keys.stranger, 404, Some("CatalogNotSelected")),
                expect("admin", &keys.admin, 200, None),
            ],
        )
        .await;

        // The fixture user owns the suspended team, so its session reaches
        // `main` (write) and `other` (its personal team, still active).
        let user = crate::db::TEST_USER;
        let mut member = Browser::session_for(&test, user).await;
        let (refused, why) = member.send(&test, "GET", uri, Payload::Empty).await.parts();
        let (active, _) = member
            .send(
                &test,
                "GET",
                "/api/v1/catalogs/other/revisions/1/index.md",
                Payload::Empty,
            )
            .await
            .parts();
        let (exported, _) = member
            .send(&test, "GET", "/api/v1/catalogs/main/export", Payload::Empty)
            .await
            .parts();
        crate::invalidate::testing::applied(&test.app, async {
            crate::store::users::set_admin(
                test.app.accounts().executor(),
                user,
                AdminChange {
                    on: true,
                    allow_last: false,
                },
                &AuditActor::cli(),
                Utc::now(),
            )
            .await
            .expect("make the user an admin");
        })
        .await;
        let mut admin = Browser::session_for(&test, user).await;
        let (as_admin, answer) = admin.send(&test, "GET", uri, Payload::Empty).await.parts();

        assert_eq!(refused.as_u16(), 403, "{why}");
        assert_eq!(why["error"], "Suspended");
        assert_eq!(why["team"], "family");
        assert_eq!(
            active.as_u16(),
            200,
            "the other team's catalog is untouched"
        );
        assert_eq!(exported.as_u16(), 200, "export stays open to the member");
        assert_eq!(
            as_admin.as_u16(),
            200,
            "an admin session is exempt: {answer}"
        );
    }

    /// M13 Task 4: the read routes that run a tool through a `ToolContext`
    /// (browse, health and reports) select their catalog through
    /// `selection::resolve`, which refuses a suspended one to members, keeps
    /// it for an admin and answers 404 to a key that does not reach it. The
    /// same routes on the other team's catalog still answer the member.
    #[tokio::test]
    async fn every_selection_backed_read_route_refuses_a_suspended_catalog_to_members() {
        use crate::http::api::testing::{assert_answers, Browser, Expect, Payload};
        use crate::mcp::testing::suspend_catalogs;
        let test = app().await;
        let keys = keys(&test).await;
        suspend_catalogs(&test, &["main"]).await;
        let uris = [
            "/api/v1/catalogs/main/tree",
            "/api/v1/catalogs/main/concepts/systems/alpha.md",
            "/api/v1/catalogs/main/links/systems/alpha.md",
            "/api/v1/catalogs/main/history/systems/alpha.md",
            "/api/v1/catalogs/main/documents/systems/index.md",
            "/api/v1/catalogs/main/recent-changes",
            "/api/v1/catalogs/main/health",
            "/api/v1/catalogs/main/reports/lint",
            "/api/v1/catalogs/main/reports/sprawl",
            "/api/v1/catalogs/main/reports/links",
            "/api/v1/catalogs/main/reports/oldest",
            "/api/v1/catalogs/main/reports/questions",
        ];
        let mut expected = Vec::new();
        for uri in uris {
            let expect = |who, key, status, error| Expect {
                method: "GET",
                uri,
                body: None,
                key,
                who,
                status,
                error,
            };
            expected.extend([
                expect("read", &keys.read, 403, Some("Suspended")),
                expect("write", &keys.write, 403, Some("Suspended")),
                expect("stranger", &keys.stranger, 404, Some("CatalogNotSelected")),
                expect("admin", &keys.admin, 200, None),
            ]);
        }

        assert_answers(&test, &expected).await;

        // The fixture user's session reaches `main` (suspended) and `other`
        // (its personal team, still active).
        let mut member = Browser::session_for(&test, crate::db::TEST_USER).await;
        for uri in uris {
            let (refused, why) = member.send(&test, "GET", uri, Payload::Empty).await.parts();
            assert_eq!(refused.as_u16(), 403, "{uri} as the member session: {why}");
            assert_eq!(why["error"], "Suspended", "{uri}");
            // `other` holds only a non-concept `index.md`, so the concept
            // routes are not repeated there.
            if uri.contains("systems/alpha.md") {
                continue;
            }
            let elsewhere = uri
                .replace("/main/", "/other/")
                .replace("systems/index.md", "index.md");
            let (active, body) = member
                .send(&test, "GET", &elsewhere, Payload::Empty)
                .await
                .parts();
            assert_eq!(active.as_u16(), 200, "{elsewhere} still answers: {body}");
        }
    }

    /// Imports `entries` into `catalog` as `actor`, replacing it, and applies its change messages.
    async fn publish(
        test: &crate::mcp::testing::TestApp,
        catalog: &str,
        entries: &[(&str, &str)],
        actor: &str,
    ) {
        let options = ImportOptions {
            replace: true,
            scope: ImportScope::Everything,
            expect_generation: None,
            expect_catalog_id: None,
            actor,
            audit: &AuditActor::cli(),
            now: Utc::now(),
        };
        crate::invalidate::testing::applied(&test.app, async {
            import(&test.app.main_db().await, catalog, files(entries), &options)
                .await
                .expect("import");
        })
        .await;
    }

    /// `(path, revision, op, actor)` of each change in a recent-changes page.
    fn summary(page: &Value) -> Vec<(String, i64, String, String)> {
        page["changes"]
            .as_array()
            .expect("changes")
            .iter()
            .map(|change| {
                (
                    change["path"].as_str().expect("path").to_string(),
                    change["revision"].as_i64().expect("revision"),
                    change["op"].as_str().expect("op").to_string(),
                    change["actor"].as_str().expect("actor").to_string(),
                )
            })
            .collect()
    }

    /// Opens a changeset on `main` as `key`, writes `text` at `path` in it,
    /// and publishes it with `message`, or leaves it open (a draft) when
    /// `message` is `None`.
    async fn stage(
        test: &crate::mcp::testing::TestApp,
        key: &str,
        (path, text): (&str, &str),
        message: Option<&str>,
    ) {
        use crate::mcp::changesets::{open_changeset, publish, OpenParams, PublishParams};
        use crate::mcp::testing::{pending_entry, PendingEdit};
        let ctx = crate::mcp::testing::context(
            test,
            crate::mcp::testing::cached_principal(test, key).await,
            None,
        );
        let opened = open_changeset(
            &ctx,
            OpenParams {
                catalog: Some("main".to_string()),
            },
        )
        .await
        .expect("open a changeset");
        let edit = PendingEdit {
            path,
            text: Some(text),
            at: Utc::now(),
        };
        pending_entry(test, &opened.changeset, &edit).await;
        if let Some(message) = message {
            publish(
                &ctx,
                PublishParams {
                    message: message.to_string(),
                    changeset: opened.changeset,
                },
            )
            .await
            .expect("publish");
        }
    }

    /// `main` after three rounds: the fixture's import, an import by
    /// `editor` that changes alpha and drops beta, and a publish by `writer`
    /// that creates gamma; then a draft left open, and an import into
    /// `other` by `intruder`.
    async fn three_rounds(
        test: &crate::mcp::testing::TestApp,
        keys: &crate::http::api::testing::Keys,
    ) {
        let edited = edited_main();
        let kept: Vec<(&str, &str)> = edited
            .iter()
            .filter(|(path, _)| *path != "systems/beta.md")
            .map(|(path, text)| (*path, text.as_str()))
            .collect();
        publish(test, "main", &kept, "editor").await;
        let gamma = "---\ntype: system\ntitle: Gamma\n---\n# Gamma\n\nNew.\n";
        stage(
            test,
            &keys.write,
            ("systems/gamma.md", gamma),
            Some("add gamma"),
        )
        .await;
        let draft = "---\ntype: system\ntitle: Draft\n---\n# Draft\n\nNot yet.\n";
        stage(test, &keys.write, ("systems/draft.md", draft), None).await;
        publish(
            test,
            "other",
            &[("index.md", "# Other, changed\n")],
            "intruder",
        )
        .await;
    }

    #[tokio::test]
    async fn recent_changes_list_one_catalogs_published_revisions_newest_first() {
        let test = app().await;
        let keys = keys(&test).await;
        three_rounds(&test, &keys).await;

        let (status, page) = send(
            &test,
            "GET",
            "/api/v1/catalogs/main/recent-changes",
            Some(&keys.read),
            None,
        )
        .await;
        // The deleted document's revision opens through the revision route.
        let (deleted, removal) = send(
            &test,
            "GET",
            "/api/v1/catalogs/main/revisions/2/systems/beta.md",
            Some(&keys.read),
            None,
        )
        .await;

        assert_eq!(status.as_u16(), 200, "{page}");
        assert_eq!(page["catalog"], "main");
        assert!(page["catalog_label"].is_string(), "{page}");
        assert_eq!(page.get("next_after"), Some(&Value::Null));
        let changes = summary(&page);
        let own = |path: &str, revision, op: &str, actor: &str| {
            (
                path.to_string(),
                revision,
                op.to_string(),
                actor.to_string(),
            )
        };
        // The publish first; then the editor's import, whose two rows share
        // a time and so come by path, descending; then the first import.
        assert_eq!(
            changes[..3],
            [
                own("/systems/gamma.md", 1, "create", "writer"),
                own("/systems/beta.md", 2, "delete", "editor"),
                own("/systems/alpha.md", 2, "import", "editor"),
            ],
            "{page}"
        );
        // Then the fixture: creating the catalog wrote `index.md` and
        // `log.md`, and its import wrote every file of MAIN.
        assert_eq!(
            changes.len(),
            3 + 2 + MAIN.len(),
            "no draft, nothing of `other`: {page}"
        );
        assert!(
            changes[3..]
                .iter()
                .all(|(_, _, op, _)| op == "import" || op == "create"),
            "{page}"
        );
        assert!(
            changes[3..]
                .iter()
                .any(|(path, _, _, _)| path == "/systems/alpha.md"),
            "alpha's first revision is listed too: {page}"
        );
        assert!(!changes
            .iter()
            .any(|(path, _, _, _)| path == "/systems/draft.md"));
        assert!(!changes.iter().any(|(_, _, _, actor)| actor == "intruder"));
        let first = &page["changes"][0];
        assert_eq!(first["message"], "add gamma");
        assert!(
            first["at"].as_str().is_some_and(|at| at.ends_with('Z')),
            "{first}"
        );
        assert!(first.get("actor_label").is_some(), "{first}");
        assert_eq!(page["changes"][1]["message"], "import");
        assert_eq!(deleted.as_u16(), 200, "{removal}");
        assert_eq!(removal["op"], "delete");
        assert_eq!(removal.get("text"), Some(&Value::Null));
    }

    /// Each change names its document as the catalog has it now: the title
    /// and description of the current file, and null for a deletion or a
    /// document that is gone since.
    #[tokio::test]
    async fn recent_changes_carry_titles_and_descriptions_from_the_snapshot() {
        let test = app().await;
        let keys = keys(&test).await;
        let alpha = "---\ntype: system\ntitle: Alpha\ndescription: Runs the jobs.\n---\n# Alpha\n";
        let entries: Vec<(&str, &str)> = MAIN
            .iter()
            .filter(|(path, _)| *path != "systems/beta.md")
            .map(|(path, text)| {
                if *path == "systems/alpha.md" {
                    (*path, alpha)
                } else {
                    (*path, *text)
                }
            })
            .collect();
        publish(&test, "main", &entries, "editor").await;

        let (status, page) = send(
            &test,
            "GET",
            "/api/v1/catalogs/main/recent-changes",
            Some(&keys.read),
            None,
        )
        .await;

        assert_eq!(status.as_u16(), 200, "{page}");
        let change = |path: &str, revision: i64| -> Value {
            page["changes"]
                .as_array()
                .expect("changes")
                .iter()
                .find(|change| change["path"] == path && change["revision"] == revision)
                .cloned()
                .unwrap_or_else(|| panic!("{path} revision {revision} in {page}"))
        };
        let edited = change("/systems/alpha.md", 2);
        assert_eq!(edited["title"], "Alpha");
        assert_eq!(edited["description"], "Runs the jobs.");
        // An older revision of a file that still exists names it as it is now.
        assert_eq!(
            change("/systems/alpha.md", 1)["description"],
            "Runs the jobs."
        );
        // The deletion, and the earlier revision of the deleted file: null.
        for revision in [1, 2] {
            let gone = change("/systems/beta.md", revision);
            assert_eq!(gone["title"], Value::Null, "{gone}");
            assert_eq!(gone["description"], Value::Null, "{gone}");
        }
        // A file with no title: null, not its path.
        assert_eq!(change("/log.md", 1)["title"], Value::Null);
    }

    #[tokio::test]
    async fn recent_changes_page_by_cursor_without_repeats_or_gaps() {
        let test = app().await;
        let keys = keys(&test).await;
        three_rounds(&test, &keys).await;
        let get = |uri: String| {
            let test = &test;
            let key = keys.read.clone();
            async move { send(test, "GET", &uri, Some(&key), None).await }
        };
        let (_, whole) = get("/api/v1/catalogs/main/recent-changes?limit=200".into()).await;

        // Pages of two, each continued from the one before: the fixture's
        // import wrote every row at one time, so most pages split a tie.
        let mut paged = Vec::new();
        let mut uri = "/api/v1/catalogs/main/recent-changes?limit=2".to_string();
        let mut pages = 0;
        loop {
            let (status, page) = get(uri.clone()).await;
            assert_eq!(status.as_u16(), 200, "{uri}: {page}");
            pages += 1;
            assert!(pages < 20, "paging never ends");
            paged.extend(summary(&page));
            match page["next_after"].as_str() {
                Some(after) => {
                    let after = url_escape(after);
                    uri = format!("/api/v1/catalogs/main/recent-changes?limit=2&after={after}");
                }
                None => break,
            }
        }

        assert_eq!(paged, summary(&whole));
        assert_eq!(pages, paged.len().div_ceil(2));
    }

    #[tokio::test]
    async fn recent_changes_and_history_take_user_and_since_and_show_the_person() {
        let test = app().await;
        let keys = keys(&test).await;
        three_rounds(&test, &keys).await;
        let get = |uri: String| {
            let test = &test;
            let key = keys.read.clone();
            async move { send(test, "GET", &uri, Some(&key), None).await }
        };
        let base = "/api/v1/catalogs/main/recent-changes";

        let (_, all) = get(format!("{base}?limit=200")).await;
        let (_, nobody) = get(format!("{base}?user=nobody-at-all")).await;
        let (_, future) = get(format!("{base}?since=2999-01-01T00:00:00Z")).await;
        let (_, past) = get(format!("{base}?since=2000-01-01T00:00:00Z&limit=200")).await;
        let (bad, body) = get(format!("{base}?since=yesterday")).await;
        let (hist_status, hist) =
            get("/api/v1/catalogs/main/history/index.md?user=nobody".into()).await;

        let first = &all["changes"][0];
        assert!(
            first.get("user").is_some() && first.get("user_label").is_some(),
            "{first}"
        );
        assert!(nobody["changes"].as_array().unwrap().is_empty(), "{nobody}");
        assert_eq!(nobody["next_after"], serde_json::Value::Null);
        assert!(future["changes"].as_array().unwrap().is_empty());
        assert_eq!(past["changes"], all["changes"]);
        assert_eq!(bad.as_u16(), 409, "{body}");
        assert_eq!(hist_status.as_u16(), 200, "{hist}");
        assert!(hist["revisions"].as_array().unwrap().is_empty(), "{hist}");
    }

    /// Percent-encodes everything but unreserved characters, for a query value.
    fn url_escape(text: &str) -> String {
        text.bytes()
            .map(|byte| match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    char::from(byte).to_string()
                }
                _ => format!("%{byte:02X}"),
            })
            .collect()
    }

    #[tokio::test]
    async fn recent_changes_refuse_a_bad_page_size_or_cursor() {
        let test = app().await;
        let keys = keys(&test).await;
        let base = "/api/v1/catalogs/main/recent-changes";
        let mut answers = Vec::new();
        for query in [
            "limit=0",
            "limit=201",
            "limit=200",
            "limit=many",
            "after=nonsense",
            "after=0%3Aindex.md",
            "after=9%3Aindex.md",
            "after=1%3Aindex.md",
        ] {
            let (status, body) = send(
                &test,
                "GET",
                &format!("{base}?{query}"),
                Some(&keys.read),
                None,
            )
            .await;
            answers.push((
                query,
                status.as_u16(),
                body["error"].as_str().map(str::to_string),
            ));
        }

        let statuses: Vec<(&str, u16)> = answers
            .iter()
            .map(|(query, status, _)| (*query, *status))
            .collect();
        assert_eq!(
            statuses,
            [
                ("limit=0", 409),
                ("limit=201", 409),
                ("limit=200", 200),
                ("limit=many", 400),
                ("after=nonsense", 404),
                ("after=0%3Aindex.md", 404),
                ("after=9%3Aindex.md", 404),
                ("after=1%3Aindex.md", 200),
            ],
            "{answers:?}"
        );
        assert_eq!(answers[0].2.as_deref(), Some("Refused"));
        assert_eq!(answers[4].2.as_deref(), Some("NotFound"));
    }

    /// A route's status and `error`.
    type Answer = (u16, Option<String>);

    /// The recent-changes route refuses exactly as the history and revision
    /// routes do: without a credential, for a catalog the key does not
    /// reach, for one that does not exist, for a suspended one and for a
    /// locked one, with the same status and error.
    #[tokio::test]
    async fn recent_changes_refuse_exactly_as_history_and_revisions_do() {
        use crate::mcp::testing::{require_unlock, staff_root, suspend_catalogs};
        let test = app().await;
        let keys = keys(&test).await;
        let staff = staff_root(&test).await;
        let routes = |catalog: &str| {
            [
                format!("/api/v1/catalogs/{catalog}/recent-changes"),
                format!("/api/v1/catalogs/{catalog}/history/index.md"),
                format!("/api/v1/catalogs/{catalog}/revisions/1/index.md"),
            ]
        };
        let answer = |key: Option<String>, uri: String| {
            let test = &test;
            async move {
                let (status, body) = send(test, "GET", &uri, key.as_deref(), None).await;
                (status.as_u16(), body["error"].as_str().map(str::to_string))
            }
        };
        let mut cases: Vec<(&str, Vec<Answer>)> = Vec::new();
        for (case, key, catalog) in [
            ("no credential", None, "main"),
            ("unreached", Some(keys.stranger.clone()), "main"),
            ("no such catalog", Some(keys.read.clone()), "nope"),
            ("reached", Some(keys.read.clone()), "main"),
        ] {
            let mut got = Vec::new();
            for uri in routes(catalog) {
                got.push(answer(key.clone(), uri).await);
            }
            cases.push((case, got));
        }
        suspend_catalogs(&test, &["main"]).await;
        let mut got = Vec::new();
        for uri in routes("main") {
            got.push(answer(Some(keys.read.clone()), uri).await);
        }
        cases.push(("suspended", got));
        // `staff` reaches every catalog as an admin, and `require_unlock`
        // makes it ask for an unlock first; `other` is not suspended.
        crate::store::users::set_admin(
            test.app.accounts().executor(),
            "staff",
            AdminChange {
                on: true,
                allow_last: false,
            },
            &AuditActor::cli(),
            Utc::now(),
        )
        .await
        .expect("make staff an admin");
        require_unlock(&test).await;
        let mut got = Vec::new();
        for uri in routes("other") {
            got.push(answer(Some(staff.clone()), uri).await);
        }
        cases.push(("locked", got));

        let expected: [(&str, (u16, Option<&str>)); 6] = [
            ("no credential", (401, None::<&str>)),
            ("unreached", (404, Some("CatalogNotSelected"))),
            ("no such catalog", (404, Some("CatalogNotSelected"))),
            ("reached", (200, None)),
            ("suspended", (403, Some("Suspended"))),
            ("locked", (403, Some("UnlockRequired"))),
        ];
        for ((case, got), (name, (status, error))) in cases.iter().zip(expected) {
            assert_eq!(*case, name);
            assert!(
                got.iter().all(|answer| *answer == got[0]),
                "{case}: recent-changes, history and revision answered {got:?}"
            );
            assert_eq!(got[0].0, status, "{case}: {got:?}");
            if let Some(error) = error {
                assert_eq!(got[0].1.as_deref(), Some(error), "{case}: {got:?}");
            }
        }
    }
}
