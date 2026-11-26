//! `GET /api/v1/search` (web console design, "API": Search): the `search`
//! tool over the catalogs a request names, or every catalog the key reaches.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};

use crate::app::App;
use crate::auth::Principal;
use crate::error::OkfError;
use crate::http::api::context;
use crate::http::requester::Requester;
use crate::http::AppError;
use crate::mcp::context::ToolContext;
use crate::mcp::search::{run_with, LabeledHit, SearchParams, SearchResult, SkippedCatalog};
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
    Router::new().route("/search", get(search))
}

/// `?q=&catalogs=&limit=`.
#[derive(Debug, Clone, Default, Deserialize)]
struct SearchArgs {
    /// Free text, as the tool's `query`; absent means every concept.
    #[serde(default)]
    q: String,
    /// Comma-separated catalog names; absent or empty means every catalog
    /// the key reaches.
    catalogs: Option<String>,
    /// At most this many hits (as the tool: default 20, at most 200).
    limit: Option<usize>,
}

/// One hit as `GET /search` answers it: the `search` tool's hit, plus its
/// snippet as plain text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct WebHit {
    /// The tool's hit; `flatten` writes its fields beside `snippet_text`.
    #[serde(flatten)]
    pub hit: LabeledHit,
    /// The snippet's text without markdown syntax
    /// ([`crate::okf::search::SearchHit::snippet_text`]), for a client that
    /// shows it as text (the mobile reader); null without a snippet.
    pub snippet_text: Option<String>,
}

/// `GET /search`'s answer: the tool's result, each hit a [`WebHit`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct WebSearch {
    /// As [`SearchResult::hits`], best first.
    pub hits: Vec<WebHit>,
    /// As [`SearchResult::skipped`].
    pub skipped: Vec<SkippedCatalog>,
    /// As [`SearchResult::unavailable`].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unavailable: Vec<String>,
}

/// The tool's result with each hit's plain snippet moved beside it.
///
/// # Arguments
/// * `result` - the search's result, made with plain snippets.
///
/// # Returns
/// The same hits, in the same order.
#[tracing::instrument(level = "trace", skip_all)]
fn with_plain_snippets(result: SearchResult) -> WebSearch {
    let hits = result
        .hits
        .into_iter()
        .map(|mut hit| {
            // `hit.hit.hit` is the LabeledHit's CatalogHit's SearchHit;
            // `take` moves the text out, leaving `None` (it is not serialized there).
            let snippet_text = hit.hit.hit.snippet_text.take();
            WebHit { hit, snippet_text }
        })
        .collect();
    WebSearch {
        hits,
        skipped: result.skipped,
        unavailable: result.unavailable,
    }
}

/// The names in a `catalogs=` value.
///
/// # Arguments
/// * `listed` - the raw value, such as `main, other`.
///
/// # Returns
/// The names, trimmed, empty ones dropped, in order.
#[tracing::instrument(level = "trace", skip_all)]
fn catalog_names(listed: &str) -> Vec<String> {
    listed
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

/// The catalogs a search that names none searches: every catalog the key
/// reaches, when there are at most `search_fanout_max` of them (M18 spec
/// Decision 8), leaving out those it may not read now.
///
/// # Arguments
/// * `app` - the application, for its directory and its config.
/// * `ctx` - the search's own context: each catalog is loaded through it
///   and held, so the search reads the same state without a second load.
///
/// # Returns
/// The names, sorted.
///
/// # Errors
/// [`OkfError::InvalidPath`] (400, naming `catalogs`) when the key reaches
/// more than `search_fanout_max` catalogs; [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all)]
async fn every_reachable(app: &App, ctx: &ToolContext) -> Result<Vec<String>, OkfError> {
    let principal = ctx.principal();
    let most = app.config().search_fanout_max;
    // One row more than the cap: enough to tell that the key reaches too
    // many, without reading every catalog for a key that reaches them all.
    // The count is of what the key reaches, not of what is open to it now,
    // so the cap does not move with suspensions or unlocks.
    let page = app
        .directory()
        .reachable_page(principal, None, most.saturating_add(1))
        .await?;
    if page.next.is_some() || page.items.len() > most as usize {
        return Err(OkfError::InvalidPath {
            path: "catalogs".to_string(),
            reason: format!("name the catalogs to search (at most {most} are searched by default)"),
        });
    }
    // A closed catalog is not searched: a suspended team's (M13), or one the
    // key needs an unlock for (M14). Reach was checked by the page; naming
    // one explicitly is refused by the selection. A catalog whose stored
    // data does not build is left out, as one that was not loaded was
    // before: each open one is loaded here, which the search does anyway.
    let now = app.clock().now();
    // Each entry is a name to keep, with whether its snapshot is loaded (an
    // open catalog on a reachable store) before it is kept.
    let mut kept = Vec::with_capacity(page.items.len());
    let mut loadable = Vec::with_capacity(page.items.len());
    for row in &page.items {
        if principal.check_open(row, Access::Read, now).is_err() {
            continue;
        }
        // One on an unreachable store is kept, unloaded: the search names
        // it in its `unavailable` list instead of failing for it.
        if page.unavailable.contains(&row.name) {
            // The page spent a timeout on this store: the search's
            // selection and loads must not spend another.
            ctx.mark_down(row);
            kept.push((row, true));
            continue;
        }
        kept.push((row, false));
        loadable.push(row);
    }
    // The open ones load concurrently; the results come back in page order.
    let mut states = ctx.listed_states(&loadable).await.into_iter();
    let mut names = Vec::with_capacity(kept.len());
    for (row, unloaded) in kept {
        if unloaded {
            names.push(row.name.clone());
            continue;
        }
        match states.next().expect("one result per loaded row") {
            Ok(Some(_)) => names.push(row.name.clone()),
            Ok(None) => {}
            // The store went away since the page was read.
            Err(OkfError::StoreUnavailable { .. }) => names.push(row.name.clone()),
            Err(error) => return Err(error),
        }
    }
    Ok(names)
}

/// `GET /search`.
///
/// Unlike the MCP tool, whose default is the connection's catalogs (else
/// the key's default catalog), the console has no connection: without
/// `catalogs` it searches every loaded catalog the key reaches, leaving out
/// those whose owner team is suspended (unless the key is admin), as long
/// as the key reaches at most `search_fanout_max` catalogs.
///
/// # Errors
/// As `search`: 404 for a named catalog the key does not reach or that does
/// not exist; 403 `Suspended` for a named catalog whose team is suspended;
/// 400 without `catalogs` for a key that reaches more than
/// `search_fanout_max` catalogs.
#[tracing::instrument(level = "debug", skip_all)]
async fn search(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    requester: Requester,
    Query(args): Query<SearchArgs>,
) -> Result<Json<WebSearch>, AppError> {
    let named = args
        .catalogs
        .as_deref()
        .map(catalog_names)
        .unwrap_or_default();
    // First, so the catalogs an unnamed search loads to check are held for
    // the search itself (one build each, even for one the cache cannot keep).
    let ctx = context(&app, principal, requester);
    let catalogs = if named.is_empty() {
        // An empty list (a key that reaches nothing active) falls back to the
        // key's default inside the selection, which then refuses as the tool
        // would.
        every_reachable(&app, &ctx).await?
    } else {
        named
    };
    let result = run_with(
        &ctx,
        SearchParams {
            query: args.q,
            catalogs: Some(catalogs),
            concept_type: None,
            tags: Vec::new(),
            status: None,
            limit: args.limit,
            changeset: None,
        },
        true,
    )
    .await?;
    Ok(Json(with_plain_snippets(result)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::api::testing::{app, assert_matrix, keys, send, Row};

    /// An unnamed search loads each catalog once: the check that a catalog
    /// builds and the search share one load, even for a catalog the cache
    /// cannot keep (a budget smaller than one catalog).
    #[tokio::test(flavor = "multi_thread")]
    async fn an_unnamed_search_builds_each_catalog_once() {
        use crate::http::api::testing::MAIN;
        let config = crate::config::Config {
            catalog_cache_bytes: bytesize::ByteSize::b(1),
            ..crate::config::Config::default()
        };
        let (test, registry) = crate::mcp::testing::app_with_metrics(
            &[("main", MAIN), ("other", &[("index.md", "# Other\n")])],
            config,
        )
        .await;
        let keys = keys(&test).await;
        let loads = || {
            crate::mcp::testing::sample_of(
                &crate::mcp::testing::render(&registry),
                r#"okf_cache_load_duration_seconds_count{cache="catalog",outcome="ok"}"#,
            )
            .unwrap_or(0.0)
        };
        let before = loads();

        let (status, _) = send(
            &test,
            "GET",
            "/api/v1/search?q=alpha",
            Some(&keys.read),
            None,
        )
        .await;

        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(loads() - before, 1.0, "one build of `main` for one search");
    }

    #[test]
    fn catalog_names_are_trimmed_and_empty_ones_dropped() {
        assert_eq!(catalog_names(" main, other,,"), ["main", "other"]);
        assert!(catalog_names("").is_empty());
    }

    #[tokio::test]
    async fn search_needs_read_on_every_named_catalog() {
        let test = app().await;
        let row = |uri: &str, stranger: u16| Row {
            method: "GET",
            uri: uri.to_string(),
            body: None,
            read: 200,
            write: 200,
            admin: 200,
            stranger,
        };

        assert_matrix(
            &test,
            &[
                row("/api/v1/search?q=alpha&catalogs=main", 404),
                // Without `catalogs`, each key searches what it reaches.
                row("/api/v1/search?q=alpha", 200),
            ],
        )
        .await;
    }

    #[tokio::test]
    async fn search_finds_a_concept_with_its_catalog_and_snippet() {
        let test = app().await;
        let keys = keys(&test).await;

        let (status, found) = send(
            &test,
            "GET",
            "/api/v1/search?q=runs%20beta",
            Some(&keys.read),
            None,
        )
        .await;
        let (_, limited) = send(
            &test,
            "GET",
            "/api/v1/search?q=&limit=1",
            Some(&keys.read),
            None,
        )
        .await;
        let (_, elsewhere) = send(
            &test,
            "GET",
            "/api/v1/search?q=alpha",
            Some(&keys.stranger),
            None,
        )
        .await;

        assert_eq!(status.as_u16(), 200, "{found}");
        assert_eq!(found["hits"][0]["catalog"], "main");
        assert_eq!(found["hits"][0]["path"], "/systems/alpha.md");
        assert_eq!(found["hits"][0]["snippet"], "Alpha runs [Beta](beta.md).");
        // The same text without its markdown syntax, for the reader.
        assert_eq!(found["hits"][0]["snippet_text"], "Alpha runs Beta.");
        // An empty query has no snippet, so no plain one either.
        assert_eq!(limited["hits"][0]["snippet_text"], serde_json::Value::Null);
        assert_eq!(limited["hits"].as_array().map(Vec::len), Some(1));
        assert_eq!(
            elsewhere["hits"],
            serde_json::json!([]),
            "other has no concepts"
        );
    }

    /// M13 Task 4: searching every reached catalog leaves a suspended
    /// team's catalogs out (an admin's search keeps them); naming one is a
    /// 403 `Suspended`, and a key that does not reach it gets 404 still.
    #[tokio::test]
    async fn search_leaves_out_or_refuses_a_suspended_catalog() {
        use crate::config::Config;
        use crate::http::api::testing::{assert_answers, Expect};
        use crate::mcp::testing::{app_with, stored_secret, suspend_catalogs};
        use crate::store::keys::Access;
        const ALPHA: &str = "---\ntype: system\ntitle: Alpha\n---\n# Alpha\n";
        let test = app_with(
            &[
                ("main", &[("alpha.md", ALPHA)]),
                ("other", &[("alpha.md", ALPHA)]),
            ],
            Config::default(),
        )
        .await;
        let member = stored_secret(&test, "member", Access::Read, &["main", "other"], false).await;
        let only_main = stored_secret(&test, "only-main", Access::Read, &["main"], false).await;
        let stranger = stored_secret(&test, "stranger", Access::Read, &["other"], false).await;
        let root = stored_secret(&test, "root", Access::Admin, &[], false).await;
        suspend_catalogs(&test, &["main"]).await;
        let searched = |listed: &serde_json::Value| -> Vec<String> {
            let mut catalogs: Vec<String> = listed["hits"]
                .as_array()
                .expect("hits")
                .iter()
                .filter_map(|hit| hit["catalog"].as_str().map(str::to_string))
                .collect();
            catalogs.dedup();
            catalogs
        };

        let (status, members) =
            send(&test, "GET", "/api/v1/search?q=alpha", Some(&member), None).await;
        let (_, admins) = send(&test, "GET", "/api/v1/search?q=alpha", Some(&root), None).await;
        let named = "/api/v1/search?q=alpha&catalogs=main";
        let mixed = "/api/v1/search?q=alpha&catalogs=other,main";
        let every = "/api/v1/search?q=alpha";
        let expect = |uri, key, who, status, error| Expect {
            method: "GET",
            uri,
            body: None,
            key,
            who,
            status,
            error,
        };

        assert_eq!(status.as_u16(), 200, "{members}");
        assert_eq!(
            searched(&members),
            ["other"],
            "the suspended catalog is left out"
        );
        assert_eq!(
            searched(&admins),
            ["main", "other"],
            "an admin searches both"
        );
        assert_answers(
            &test,
            &[
                expect(named, &member, "member", 403, Some("Suspended")),
                expect(mixed, &member, "member", 403, Some("Suspended")),
                expect(
                    named,
                    &stranger,
                    "stranger",
                    404,
                    Some("CatalogNotSelected"),
                ),
                expect(named, &root, "root", 200, None),
                // Every catalog it reaches is suspended: its default (`main`)
                // is then the selection, and is refused as named.
                expect(every, &only_main, "only-main", 403, Some("Suspended")),
            ],
        )
        .await;
    }
}
