//! `cited_by` (issue #761): the concepts whose `sources` cite a given source.
//!
//! It reads the same catalogs and drafts as `search` and returns every match
//! up to a cap, with the matching entries. The matching itself is
//! [`crate::okf::search::cites`], which a `sources` filter on `search` can
//! call the same way: build a [`SourceMatch`], keep the concepts it finds,
//! and carry the `Vec<CitedEntry>` as an optional field on the hit.

use std::sync::Arc;

use axum::http::request::Parts;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router, Json};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::catalog::CatalogState;
use crate::error::OkfError;
use crate::labels::directory::Wanted;
use crate::mcp::context::{is_broken_overlay, require_selected, ToolContext};
use crate::mcp::search::SkippedCatalog;
use crate::mcp::{blocking, reply, OkfServer};
use crate::okf::search::{cites, CitedEntry, CitedSource, SourceMatch, SourceTally};
use crate::okf::snapshot::Snapshot;
use crate::overlay::View;
use crate::store::audit::AccessKind;

/// The most hits one call returns, and the default; the same cap as `search`.
const MAX_LIMIT: usize = 200;

/// The tool's description: short, since every client loads it up front.
const DESCRIPTION: &str = "List the concepts that cite a source, exactly: `source_id` \
    (case-insensitive) and/or `resource` (case-sensitive); both must match one entry. Returns \
    catalog, path, title and the matching `sources` entries, by catalog then path, at most \
    `limit` (default and maximum 200; `truncated` says more exist). With neither, lists the \
    cited sources and their counts. `catalogs` and `changeset` as in `search`.";

/// `cited_by`'s arguments.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct CitedByParams {
    /// A source entry's `id`, matched exactly but ignoring case. Ids are unique only within a concept; use `resource` to find one source across concepts.
    pub source_id: Option<String>,
    /// A source entry's `resource`, matched exactly (case-sensitive).
    pub resource: Option<String>,
    /// The catalog ids to search (default: this connection's selection).
    pub catalogs: Option<Vec<String>>,
    /// At most this many hits (default and maximum 200).
    pub limit: Option<usize>,
    /// Read through this changeset's drafts (its handle); its catalog must be one this call reads.
    pub changeset: Option<String>,
}

/// One concept that cites the source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct CitingConcept {
    /// The catalog's id.
    pub catalog: String,
    /// The catalog's label: say this to people, pass `catalog` in calls.
    pub catalog_label: String,
    /// The concept's path.
    pub path: String,
    /// The concept's title, if it has one.
    pub title: Option<String>,
    /// The concept's `sources` entries that matched.
    pub sources: Vec<CitedEntry>,
}

/// `cited_by`'s result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct CitedByResult {
    /// By catalog id, then path.
    pub hits: Vec<CitingConcept>,
    /// With neither `source_id` nor `resource`: the sources cited, most
    /// cited first, then by id. Left out of a filtered call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sources: Option<Vec<CitedSource>>,
    /// True when more concepts cite the source (or more sources are cited,
    /// without a filter) than `hits` (or `sources`) holds.
    pub truncated: bool,
    /// Catalogs left out because the `changeset` read through does not
    /// apply over their published files; every other catalog still answered.
    pub skipped: Vec<SkippedCatalog>,
}

/// Finds the concepts citing a source.
///
/// # Arguments
/// * `ctx` - the call's context.
/// * `params` - the arguments.
///
/// # Returns
/// The citing concepts, at most the limit, in catalog then path order; or,
/// when neither `source_id` nor `resource` is given, the cited sources.
///
/// # Errors
/// [`OkfError::Refused`] when `source_id` or `resource` is given but every
/// one given is empty; otherwise as `search`: a selection error, a bad
/// `changeset`, a refused admission or a database error. A catalog whose
/// overlay does not apply is listed in [`CitedByResult::skipped`].
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn run(
    ctx: &ToolContext,
    params: CitedByParams,
) -> Result<CitedByResult, OkfError> {
    // Neither argument is a request for the cited sources; an empty string
    // is a malformed filter, refused rather than widened into that listing.
    let wanted = match (&params.source_id, &params.resource) {
        (None, None) => None,
        (id, resource) => Some(
            SourceMatch::new(
                id.as_deref().filter(|id| !id.is_empty()),
                resource.as_deref().filter(|resource| !resource.is_empty()),
            )
            .ok_or_else(|| OkfError::Refused {
                reason: "give a non-empty `source_id` or `resource` to find its citations"
                    .to_string(),
            })?,
        ),
    };
    let states = ctx.states(params.catalogs.as_deref()).await?;
    let pinned = ctx.pin(params.changeset.as_deref()).await?;
    require_selected(pinned.as_ref(), &states)?;
    ctx.admit_all(&states, AccessKind::Read).await?;
    let limit = params.limit.map_or(MAX_LIMIT, |limit| limit.min(MAX_LIMIT));
    let mut views: Vec<(Arc<CatalogState>, View)> = Vec::with_capacity(states.len());
    let mut skipped = Vec::new();
    for state in states {
        match ctx.view(&state, pinned.as_ref()).await {
            Ok(view) => views.push((state, view)),
            Err(error) if is_broken_overlay(&error) => {
                tracing::warn!(catalog = %state.name(), kind = error.kind(), "cited_by skipped a catalog");
                skipped.push(SkippedCatalog {
                    catalog: state.name().to_string(),
                    catalog_label: ctx.catalog_label(state.name()).await?,
                    error: error.to_json(),
                });
            }
            Err(error) => return Err(error),
        }
    }
    let Some(wanted) = wanted else {
        let (sources, truncated) = blocking(move || {
            let mut tally = SourceTally::default();
            for (_, view) in &views {
                tally.add(view.snapshot());
            }
            tally.top(limit)
        })
        .await;
        return Ok(CitedByResult {
            hits: Vec::new(),
            sources: Some(sources),
            truncated,
            skipped,
        });
    };
    let found = blocking(move || {
        // Every match is collected and sorted before the cut, so the kept
        // hits do not depend on the order the catalogs were selected in.
        let mut found: Vec<(String, String, Option<String>, Vec<CitedEntry>)> = Vec::new();
        for (state, view) in &views {
            let snapshot: &Snapshot = view.snapshot();
            for (path, entries) in cites(snapshot, &wanted) {
                let title = snapshot
                    .get(path)
                    .and_then(|doc| doc.title())
                    .map(str::to_string);
                found.push((state.name().to_string(), path.to_string(), title, entries));
            }
        }
        found.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        found
    })
    .await;
    let truncated = found.len() > limit;
    let found: Vec<_> = found.into_iter().take(limit).collect();
    let mut labels_wanted = Wanted::default();
    for (catalog, ..) in &found {
        labels_wanted.catalog(catalog);
    }
    let labels = ctx.labels_for(&labels_wanted).await?;
    let hits = found
        .into_iter()
        .map(|(catalog, path, title, sources)| CitingConcept {
            catalog_label: labels.catalog(&catalog).unwrap_or(&catalog).to_string(),
            catalog,
            path,
            title,
            sources,
        })
        .collect();
    Ok(CitedByResult {
        hits,
        sources: None,
        truncated,
        skipped,
    })
}

// The router for this module's tool, joined in `OkfServer::tools`.
#[tool_router(router = cited_by_router, vis = "pub(crate)")]
impl OkfServer {
    /// The `cited_by` tool.
    #[tool(
        title = "Find Citations",
        description = DESCRIPTION,
        annotations(
            title = "Find Citations",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn cited_by(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(params): Parameters<CitedByParams>,
    ) -> Result<Json<CitedByResult>, CallToolResult> {
        let ctx = self.context(&parts)?;
        reply(run(&ctx, params).await)
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::config::Config;
    use crate::mcp::testing::{
        app_with, context, open, pending_entry, principal, principal_as, store_key, PendingEdit,
    };
    use crate::store::keys::Access;

    /// A concept citing `id` (resource `res`).
    fn concept(title: &str, id: &str, res: &str) -> String {
        format!(
            "---\ntype: Note\ntitle: {title}\nsources:\n  - id: {id}\n    resource: {res}\n    \
             title: Diary\n    author: human:alice\n  - id: other\n    resource: https://example.com/x\n---\nBody.\n"
        )
    }

    fn params(id: Option<&str>, resource: Option<&str>) -> CitedByParams {
        CitedByParams {
            source_id: id.map(str::to_string),
            resource: resource.map(str::to_string),
            catalogs: None,
            limit: None,
            changeset: None,
        }
    }

    fn paths(result: &CitedByResult) -> Vec<(String, String)> {
        result
            .hits
            .iter()
            .map(|hit| (hit.catalog.clone(), hit.path.clone()))
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn no_match_and_one_match() {
        let a = concept("A", "diary-1", "Family diary");
        let test = app_with(&[("pg", &[("a.md", &a)])], Config::default()).await;
        let ctx = context(&test, principal(&[], Some("pg")), None);

        let none = run(&ctx, params(Some("nope"), None)).await.unwrap();
        let one = run(&ctx, params(Some("diary-1"), None)).await.unwrap();

        assert!(none.hits.is_empty() && !none.truncated);
        assert_eq!(paths(&one), [("pg".to_string(), "a.md".to_string())]);
        assert_eq!(one.hits[0].title.as_deref(), Some("A"));
        assert_eq!(
            one.hits[0].sources,
            [CitedEntry {
                id: Some("diary-1".to_string()),
                resource: Some("Family diary".to_string()),
                title: Some("Diary".to_string()),
                author: Some("human:alice".to_string()),
            }]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn several_concepts_across_catalogs_come_back_by_catalog_then_path() {
        let a = concept("A", "d1", "Family diary");
        let b = concept("B", "d1", "Family diary");
        let c = concept("C", "d1", "Family diary");
        let test = app_with(
            &[
                ("zeta", &[("a.md", &a)]),
                ("alpha", &[("z.md", &b), ("m.md", &c)]),
            ],
            Config::default(),
        )
        .await;
        let ctx = context(&test, principal(&[], None), Some(&["zeta", "alpha"]));

        let found = run(&ctx, params(Some("d1"), None)).await.unwrap();

        assert_eq!(
            paths(&found),
            [
                ("alpha".to_string(), "m.md".to_string()),
                ("alpha".to_string(), "z.md".to_string()),
                ("zeta".to_string(), "a.md".to_string()),
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_resource_finds_citations_whose_ids_differ_and_is_case_sensitive() {
        let a = concept("A", "one", "Family diary");
        let b = concept("B", "two", "Family diary");
        let test = app_with(&[("pg", &[("a.md", &a), ("b.md", &b)])], Config::default()).await;
        let ctx = context(&test, principal(&[], Some("pg")), None);

        let by_resource = run(&ctx, params(None, Some("Family diary"))).await.unwrap();
        let wrong_case = run(&ctx, params(None, Some("family diary"))).await.unwrap();
        let partial = run(&ctx, params(None, Some("Family"))).await.unwrap();
        let both = run(&ctx, params(Some("one"), Some("Family diary")))
            .await
            .unwrap();
        let mismatch = run(&ctx, params(Some("one"), Some("https://example.com/x")))
            .await
            .unwrap();

        assert_eq!(by_resource.hits.len(), 2);
        assert!(wrong_case.hits.is_empty());
        assert!(partial.hits.is_empty());
        assert_eq!(paths(&both), [("pg".to_string(), "a.md".to_string())]);
        // The id and the resource must match one entry, not two.
        assert!(mismatch.hits.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_id_ignores_case_but_matches_whole() {
        let a = concept("A", "Diary-1", "Family diary");
        let test = app_with(&[("pg", &[("a.md", &a)])], Config::default()).await;
        let ctx = context(&test, principal(&[], Some("pg")), None);

        let lower = run(&ctx, params(Some("diary-1"), None)).await.unwrap();
        let prefix = run(&ctx, params(Some("diary"), None)).await.unwrap();

        assert_eq!(lower.hits.len(), 1);
        assert!(prefix.hits.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_empty_filter_is_refused_but_no_filter_is_not() {
        let test = app_with(&[("pg", &[])], Config::default()).await;
        let ctx = context(&test, principal(&[], Some("pg")), None);

        let empty = run(&ctx, params(Some(""), Some(""))).await.unwrap_err();
        let empty_id = run(&ctx, params(Some(""), None)).await.unwrap_err();
        let none = run(&ctx, params(None, None)).await.unwrap();

        assert_eq!(empty.kind(), "Refused");
        assert_eq!(empty_id.kind(), "Refused");
        assert_eq!(none.sources, Some(Vec::new()));
        assert!(none.hits.is_empty() && !none.truncated);
    }

    /// A concept citing each `(id, resource)` of `cites`.
    fn citing(cites: &[(&str, &str)]) -> String {
        let entries: String = cites
            .iter()
            .map(|(id, res)| format!("  - id: {id}\n    resource: {res}\n    title: T-{id}\n"))
            .collect();
        format!("---\ntype: Note\nsources:\n{entries}---\nBody.\n")
    }

    fn listed(result: &CitedByResult) -> Vec<(Option<&str>, Option<&str>, usize)> {
        result
            .sources
            .as_ref()
            .expect("an unfiltered call lists sources")
            .iter()
            .map(|s| (s.source_id.as_deref(), s.resource.as_deref(), s.cited_by))
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn no_filter_counts_each_source_by_concepts_across_catalogs_in_order() {
        // `D1` and `d1` are one source; a concept citing it twice counts once.
        let a = citing(&[("D1", "Diary"), ("d1", "Diary"), ("b", "Ledger")]);
        let b = citing(&[("d1", "Diary")]);
        let c = citing(&[("a", "Memo"), ("b", "Ledger")]);
        let test = app_with(
            &[
                ("pg", &[("a.md", &a), ("b.md", &b)]),
                ("work", &[("c.md", &c)]),
            ],
            Config::default(),
        )
        .await;
        let ctx = context(&test, principal(&[], None), Some(&["pg", "work"]));

        let found = run(&ctx, params(None, None)).await.unwrap();

        assert!(found.hits.is_empty() && !found.truncated);
        // Count descending, then id: `D1` (2), `b` (2), `a` (1).
        assert_eq!(
            listed(&found),
            [
                (Some("D1"), Some("Diary"), 2),
                (Some("b"), Some("Ledger"), 2),
                (Some("a"), Some("Memo"), 1),
            ]
        );
        let first = &found.sources.as_ref().unwrap()[0];
        assert_eq!(first.title.as_deref(), Some("T-D1"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn no_filter_respects_the_limit_and_says_truncated() {
        let text = citing(&[("s0", "r0"), ("s1", "r1"), ("s2", "r2")]);
        let extra = citing(&[("s2", "r2")]);
        let test = app_with(
            &[("pg", &[("a.md", &text), ("b.md", &extra)])],
            Config::default(),
        )
        .await;
        let ctx = context(&test, principal(&[], Some("pg")), None);
        let mut limited = params(None, None);
        limited.limit = Some(2);

        let cut = run(&ctx, limited).await.unwrap();
        let all = run(&ctx, params(None, None)).await.unwrap();

        assert!(cut.truncated);
        assert_eq!(
            listed(&cut),
            [(Some("s2"), Some("r2"), 2), (Some("s0"), Some("r0"), 1)]
        );
        assert!(!all.truncated);
        assert_eq!(listed(&all).len(), 3);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn no_filter_through_a_changeset_counts_the_drafts() {
        let plain = "---\ntype: Note\ntitle: Plain\n---\nBody.\n";
        let test = app_with(&[("pg", &[("a.md", plain)])], Config::default()).await;
        store_key(&test, "writer").await;
        let ctx = context(
            &test,
            principal_as("writer", Access::Write, &[], Some("pg")),
            None,
        );
        let handle = open(&ctx).await;
        let draft = concept("Plain", "draft-1", "Family diary");
        let edit = PendingEdit {
            path: "a.md",
            text: Some(&draft),
            at: Utc::now(),
        };
        pending_entry(&test, &handle, &edit).await;
        let mut through = params(None, None);
        through.changeset = Some(handle);

        let published = run(&ctx, params(None, None)).await.unwrap();
        let drafted = run(&ctx, through).await.unwrap();

        assert_eq!(published.sources, Some(Vec::new()));
        assert_eq!(
            listed(&drafted),
            [
                (Some("draft-1"), Some("Family diary"), 1),
                (Some("other"), Some("https://example.com/x"), 1),
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_filtered_call_has_no_sources_field() {
        let a = concept("A", "d1", "Family diary");
        let test = app_with(&[("pg", &[("a.md", &a)])], Config::default()).await;
        let ctx = context(&test, principal(&[], Some("pg")), None);

        let found = run(&ctx, params(Some("d1"), None)).await.unwrap();

        assert!(found.sources.is_none());
        let json = serde_json::to_value(&found).unwrap();
        assert!(json.get("sources").is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_draft_only_citation_is_found_through_the_changeset() {
        let plain = "---\ntype: Note\ntitle: Plain\n---\nBody.\n";
        let test = app_with(&[("pg", &[("a.md", plain)])], Config::default()).await;
        store_key(&test, "writer").await;
        let ctx = context(
            &test,
            principal_as("writer", Access::Write, &[], Some("pg")),
            None,
        );
        let handle = open(&ctx).await;
        let draft = concept("Plain", "draft-1", "Family diary");
        let edit = PendingEdit {
            path: "a.md",
            text: Some(&draft),
            at: Utc::now(),
        };
        pending_entry(&test, &handle, &edit).await;
        let mut through = params(Some("draft-1"), None);
        through.changeset = Some(handle);

        let published = run(&ctx, params(Some("draft-1"), None)).await.unwrap();
        let drafted = run(&ctx, through).await.unwrap();

        assert!(published.hits.is_empty());
        assert_eq!(paths(&drafted), [("pg".to_string(), "a.md".to_string())]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn more_than_the_limit_is_truncated_and_the_first_in_order_are_kept() {
        let text = concept("N", "d1", "Family diary");
        let files: Vec<(String, String)> =
            (0..5).map(|i| (format!("n{i}.md"), text.clone())).collect();
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(p, t)| (p.as_str(), t.as_str()))
            .collect();
        let test = app_with(&[("pg", &refs)], Config::default()).await;
        let ctx = context(&test, principal(&[], Some("pg")), None);
        let mut limited = params(Some("d1"), None);
        limited.limit = Some(3);

        let cut = run(&ctx, limited).await.unwrap();
        let all = run(&ctx, params(Some("d1"), None)).await.unwrap();

        assert!(cut.truncated);
        let kept: Vec<&str> = cut.hits.iter().map(|hit| hit.path.as_str()).collect();
        assert_eq!(kept, ["n0.md", "n1.md", "n2.md"]);
        assert!(!all.truncated);
        assert_eq!(all.hits.len(), 5);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_catalog_whose_overlay_fails_is_skipped_and_the_others_answer() {
        let router = concept("Router", "d1", "Family diary");
        let switch = concept("Switch", "d1", "Family diary");
        let test = app_with(
            &[
                ("pg", &[("router.md", &router)]),
                ("work", &[("switch.md", &switch)]),
            ],
            Config::default(),
        )
        .await;
        store_key(&test, "writer").await;
        let ctx = context(
            &test,
            principal_as("writer", Access::Write, &[], None),
            Some(&["pg", "work"]),
        );
        // The write target is `pg`, the first selected catalog.
        let handle = open(&ctx).await;
        // `router.md` is a published file, so a pending `router.md/x.md`
        // cannot be applied over it.
        let clash = PendingEdit {
            path: "router.md/x.md",
            text: Some("# X\n"),
            at: Utc::now(),
        };
        pending_entry(&test, &handle, &clash).await;
        let mut through = params(Some("d1"), None);
        through.changeset = Some(handle.clone());

        let found = run(&ctx, through).await.unwrap();

        assert_eq!(
            paths(&found),
            [("work".to_string(), "switch.md".to_string())]
        );
        assert!(!found.truncated);
        assert_eq!(found.skipped.len(), 1);
        assert_eq!(found.skipped[0].catalog, "pg");
        assert_eq!(found.skipped[0].error["error"], "InvalidPath");
        let message = found.skipped[0].error["message"].as_str().unwrap();
        assert!(
            message.contains(&format!("pending changeset {handle}")),
            "{message}"
        );
    }

    #[test]
    fn the_tool_is_read_only_short_and_the_cap_is_200() {
        let listed = OkfServer::tools()
            .list_all()
            .into_iter()
            .find(|tool| tool.name == "cited_by")
            .expect("cited_by is registered");

        let hints = listed.annotations.expect("annotated");
        assert_eq!(hints.read_only_hint, Some(true));
        assert_eq!(hints.idempotent_hint, Some(true));
        assert!(DESCRIPTION.len() < 500);
        assert_eq!(MAX_LIMIT, 200);
    }
}
