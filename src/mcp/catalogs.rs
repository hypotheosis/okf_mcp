//! `list_catalogs`: the catalogs this connection offers and its selection.

use axum::http::request::Parts;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router, Json};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::cache::directory::DEFAULT_PAGE;
use crate::clock::format_timestamp;
use crate::error::OkfError;
use crate::labels::catalog_label;
use crate::mcp::context::ToolContext;
use crate::mcp::{reply, OkfServer};
use crate::store::teams::SuspendedCatalog;

/// Why a listed catalog is unavailable: its owner team is suspended (M13).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct SuspensionInfo {
    /// The owner team's id.
    pub team: String,
    /// The owner team's label: say this to people, pass `team` in calls.
    pub team_label: String,
    /// The operator's notice, if any.
    pub notice: Option<String>,
}

// `From` is Rust's conversion trait (like a Python `from_x` classmethod);
// implementing it also gives `.into()` and lets `.map(SuspensionInfo::from)`
// turn a principal's view of a suspension into a listing field.
impl From<SuspendedCatalog> for SuspensionInfo {
    #[tracing::instrument(level = "trace", skip_all)]
    fn from(suspended: SuspendedCatalog) -> Self {
        Self {
            team: suspended.team,
            team_label: suspended.team_label,
            notice: suspended.notice,
        }
    }
}

/// One reachable catalog.
// `JsonSchema` gives the tool its output schema; doc comments become the
// fields' descriptions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct CatalogInfo {
    /// The catalog's id: pass it as `catalog` in other tools.
    pub name: String,
    /// Its display name, empty when none is set.
    pub display_name: String,
    /// What to call it when talking to people: its display name, else its
    /// description. Say this, and pass `name` in tool calls.
    pub label: String,
    /// Its description.
    pub description: String,
    /// How many files it holds.
    pub files: usize,
    /// Its newest change (`2026-11-02T21:00:00Z`), or null when empty.
    pub last_change: Option<String>,
    /// Set when its owner team is suspended: then only export works (for a
    /// credential below admin). Null for an active catalog.
    pub suspended: Option<SuspensionInfo>,
}

/// `list_catalogs`' arguments.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct ListCatalogsParams {
    /// The `next` of the previous call, for the page after it; omit it for the first page.
    pub after: Option<String>,
}

/// `list_catalogs`' result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct CatalogList {
    /// One page of the catalogs this connection offers, by name: those the
    /// key reaches, narrowed to the connection's list when it has one.
    pub catalogs: Vec<CatalogInfo>,
    /// Pass this as `after` for the next page; absent on the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    /// The catalogs a call without a `catalog` argument reads, in order.
    pub selection: Vec<String>,
    /// Where writes go by default: the first of `selection`.
    pub write_target: Option<String>,
    /// Why there is no selection, when there is none.
    pub selection_error: Option<String>,
    /// The ids of catalogs on this page left out of `catalogs` because the
    /// store holding them could not be reached (sorted, each once): their
    /// file counts and last changes cannot be read now, so retry. Absent
    /// when every store answered.
    // Left out of the JSON when empty, as the other optional fields are.
    // `default` keeps the key optional in the output schema, as it is in the JSON.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unavailable: Vec<String>,
}

/// Lists one page of the catalogs `ctx` offers ([`ToolContext::offers`])
/// and the connection's selection (M18 spec Decision 7).
///
/// # Arguments
/// * `ctx` - the call's context.
/// * `after` - the `next` of the previous page, if any.
///
/// # Returns
/// The page, by name, with `next` while more follow; a selection that
/// fails is reported in `selection_error` rather than failing the call,
/// since this is the tool to diagnose it with. A suspended catalog is
/// listed with `suspended` set, and the selection leaves it out of a
/// connection's list ([`crate::selection::resolve`]). A catalog whose
/// stored data does not build is left out of its page, and so is one whose
/// store cannot be reached, which is named in `unavailable` instead.
///
/// # Errors
/// [`OkfError::InvalidPath`] for a malformed `after`;
/// [`OkfError::Database`] if the catalog directory cannot be read or a
/// listed catalog cannot be loaded.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn list(ctx: &ToolContext, after: Option<&str>) -> Result<CatalogList, OkfError> {
    let page = ctx
        .app()
        .directory()
        .offered_page(ctx.principal(), ctx.connection(), after, DEFAULT_PAGE)
        .await?;
    let mut catalogs = Vec::with_capacity(page.items.len());
    // Only rows of this page, so only catalogs the key reaches are named.
    let mut unavailable = page.unavailable;
    // Rows on a store the page found down are left unloaded; the rest load
    // concurrently, and the results come back in page order.
    let mut loadable = Vec::with_capacity(page.items.len());
    for row in &page.items {
        if unavailable.contains(&row.name) {
            // The page spent a timeout on this store: the selection below
            // must not spend another.
            ctx.mark_down(row);
            continue;
        }
        loadable.push(row);
    }
    // A listing's snapshot fields; a catalog whose data does not build
    // is left out of its page.
    let states = ctx.listed_states(&loadable).await;
    for (row, state) in loadable.into_iter().zip(states) {
        let state = match state {
            Ok(Some(state)) => state,
            Ok(None) => continue,
            // The store went away since the page was read.
            Err(OkfError::StoreUnavailable { .. }) => {
                ctx.mark_down(row);
                unavailable.push(row.name.clone());
                continue;
            }
            Err(error) => return Err(error),
        };
        let loaded = state.current();
        catalogs.push(CatalogInfo {
            name: row.name.clone(),
            display_name: row.display_name.clone(),
            label: catalog_label(&row.display_name, &row.description),
            description: row.description.clone(),
            files: loaded.files(),
            last_change: loaded.last_change().map(format_timestamp),
            // Listed only after reach (the page), so a credential that
            // does not reach a catalog never learns it is suspended.
            suspended: ctx.principal().suspension(row).map(SuspensionInfo::from),
        });
    }
    // The selection's names only, not its contents: nothing is admitted.
    let (selection, write_target, selection_error) = match ctx.select(None).await {
        Ok(selection) => {
            let write_target = Some(selection.write_target().to_string());
            (selection.catalogs().to_vec(), write_target, None)
        }
        Err(error) => (Vec::new(), None, Some(error.to_string())),
    };
    unavailable.sort();
    Ok(CatalogList {
        catalogs,
        next: page.next,
        selection,
        write_target,
        selection_error,
        unavailable,
    })
}

// `#[tool_router]` is a macro that runs at compile time: it turns the
// `#[tool]` methods of this block into a router named `catalogs_router`,
// which `OkfServer::tools` joins with the other modules' routers.
#[tool_router(router = catalogs_router, vis = "pub(crate)")]
impl OkfServer {
    /// The `list_catalogs` tool.
    // `Extension(parts): Extension<Parts>` destructures the argument in the
    // signature: rmcp fills it with the HTTP request, and `parts` is the
    // value inside. The return type is spelled out, not `ToolResult<...>`:
    // the macro finds the output schema by reading `Json<...>` in the
    // written type, and an alias hides it.
    #[tool(
        title = "List Catalogs",
        description = "List the catalogs this connection offers (name, display name, label, \
                       description, file count, last change). Pass `name` (the id) in other \
                       tools and say `label` to people: those the API key can reach, limited to the \
                       connection's X-OKF-Catalogs list when it has one. A catalog whose \
                       team is suspended is listed with `suspended` set, and only export \
                       works on it. Returns up to 100 catalogs, by name; when `next` is set, \
                       call again with it as `after` for the rest. Also this connection's \
                       selection: the catalogs a call without a `catalog` argument reads, and \
                       the default write target. It comes from the X-OKF-Catalogs header or \
                       `?catalogs=`, else the key's or the server's default catalog, else the \
                       only catalog the key reaches (a key granted every catalog has no such \
                       fallback). When the selection is empty, `selection_error` says why: \
                       pass `catalog`. A catalog whose store cannot be reached is left out of \
                       `catalogs` and listed in `unavailable`.",
        annotations(
            title = "List Catalogs",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn list_catalogs(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(params): Parameters<ListCatalogsParams>,
    ) -> Result<Json<CatalogList>, CallToolResult> {
        // `?` returns the error result at once, like a Python early `return`.
        let ctx = self.context(&parts)?;
        reply(list(&ctx, params.after.as_deref()).await)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::Config;
    use crate::mcp::testing::{app_with, context, principal};

    #[tokio::test(flavor = "multi_thread")]
    async fn lists_reachable_catalogs_and_the_connection_selection() {
        let test = app_with(
            &[
                ("pg", &[("a.md", "# A\n"), ("b.md", "# B\n")]),
                ("work", &[]),
                ("home", &[]),
            ],
            Config::default(),
        )
        .await;
        let key = principal(&["pg", "work"], None);

        let listed = list(&context(&test, key, Some(&["work", "pg"])), None)
            .await
            .unwrap();

        let names: Vec<&str> = listed.catalogs.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["pg", "work"]);
        assert_eq!(listed.catalogs[0].files, 2);
        assert!(listed.catalogs[0].last_change.is_some());
        assert_eq!(listed.catalogs[1].files, 0);
        assert_eq!(listed.catalogs[1].last_change, None);
        assert_eq!(listed.selection, ["work", "pg"]);
        assert_eq!(listed.write_target.as_deref(), Some("work"));
        assert_eq!(listed.selection_error, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_connection_list_hides_the_key_default_and_other_grants() {
        let test = app_with(
            &[("pg", &[]), ("work", &[]), ("home", &[])],
            Config::default(),
        )
        .await;
        let key = principal(&["pg", "work"], Some("pg"));

        let listed = list(&context(&test, Arc::clone(&key), Some(&["work"])), None)
            .await
            .unwrap();
        let mistyped = list(&context(&test, key, Some(&["work", "home"])), None)
            .await
            .unwrap();

        let names: Vec<&str> = listed.catalogs.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["work"]);
        assert_eq!(listed.selection, ["work"]);
        assert_eq!(listed.write_target.as_deref(), Some("work"));
        // An ungranted name is reported, never listed, and nothing falls back.
        let names: Vec<&str> = mistyped.catalogs.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["work"]);
        assert!(mistyped.selection.is_empty());
        assert!(mistyped.selection_error.unwrap().contains("\"home\""));
    }

    /// Issue #663: a key granted exactly one catalog, with no other source,
    /// selects it, so bare reads and `open_changeset` work against it; a key
    /// granted two is still refused, with the client's fix first.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_key_s_only_catalog_is_the_selection_when_nothing_else_names_one() {
        use crate::mcp::documents::{
            get_concept, read_document, GetConceptParams, ReadDocumentParams,
        };
        use crate::mcp::search::{run, SearchParams};
        use crate::mcp::testing::{open, principal_as, store_key};
        use crate::store::keys::Access;

        let test = app_with(
            &[
                (
                    "pg",
                    &[("router.md", "# Router\n"), ("index.md", "# Index\n")],
                ),
                ("work", &[("router.md", "# Router\n")]),
            ],
            Config::default(),
        )
        .await;
        store_key(&test, "only-pg").await;
        let only = context(
            &test,
            principal_as("only-pg", Access::Write, &["pg"], None),
            None,
        );
        let both = context(&test, principal(&["pg", "work"], None), None);
        let search = SearchParams {
            query: "router".to_string(),
            catalogs: None,
            concept_type: None,
            tags: Vec::new(),
            status: None,
            limit: None,
            changeset: None,
        };

        let listed = list(&only, None).await.unwrap();
        let hits = run(&only, search.clone()).await.unwrap();
        let concept = get_concept(
            &only,
            GetConceptParams {
                path: "router".to_string(),
                catalog: None,
                links: false,
                raw: false,
                section: None,
                full: false,
                changeset: None,
            },
        )
        .await
        .unwrap();
        let document = read_document(
            &only,
            ReadDocumentParams {
                path: None,
                catalog: None,
                changeset: None,
            },
        )
        .await
        .unwrap();
        let handle = open(&only).await;
        let refused = run(&both, search).await.unwrap_err();
        let unselected = list(&both, None).await.unwrap();

        assert_eq!(listed.selection, ["pg"]);
        assert_eq!(listed.write_target.as_deref(), Some("pg"));
        assert_eq!(listed.selection_error, None);
        let catalogs: Vec<&str> = hits
            .hits
            .iter()
            .map(|hit| hit.hit.catalog.as_str())
            .collect();
        assert_eq!(catalogs, ["pg"]);
        assert_eq!(concept.catalog, "pg");
        assert_eq!(document.catalog, "pg");
        assert!(!handle.is_empty());
        assert_eq!(refused.kind(), "Refused");
        assert!(unselected.selection.is_empty());
        assert!(unselected
            .selection_error
            .unwrap()
            .starts_with("no catalog selected: pass `catalog` (see `list_catalogs`)"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn without_any_source_the_selection_says_why() {
        let test = app_with(&[("pg", &[])], Config::default()).await;

        let listed = list(&context(&test, principal(&[], None), None), None)
            .await
            .unwrap();

        assert_eq!(listed.catalogs.len(), 1);
        assert!(listed.selection.is_empty());
        assert_eq!(listed.write_target, None);
        assert!(listed
            .selection_error
            .unwrap()
            .contains("no catalog selected"));
    }

    /// M13 Task 4: listing marks a suspended catalog instead of hiding it,
    /// and the connection's selection keeps the other team's catalog.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_suspended_catalog_is_listed_marked_and_the_others_still_select() {
        use crate::mcp::testing::{
            cached_principal, stored_secret, suspend_catalogs, SUSPENDED_TEAM, SUSPENSION_NOTICE,
        };
        use crate::store::keys::Access;
        let test = app_with(
            &[
                ("main", &[("index.md", "# Main\n")]),
                ("other", &[("index.md", "# Other\n")]),
            ],
            Config::default(),
        )
        .await;
        let reader = stored_secret(&test, "reader", Access::Read, &[], false).await;
        let stranger = stored_secret(&test, "stranger", Access::Read, &["other"], false).await;
        let root = stored_secret(&test, "root", Access::Admin, &[], false).await;
        suspend_catalogs(&test, &["main"]).await;
        let on = async |secret: &str, connection: &[&str]| {
            context(
                &test,
                cached_principal(&test, secret).await,
                Some(connection),
            )
        };

        let listed = list(&on(&reader, &["main", "other"]).await, None)
            .await
            .unwrap();
        let only_suspended = list(&on(&reader, &["main"]).await, None).await.unwrap();
        let strangers = list(&on(&stranger, &["other"]).await, None).await.unwrap();
        let admins = list(&on(&root, &["main", "other"]).await, None)
            .await
            .unwrap();

        let main = listed.catalogs.iter().find(|c| c.name == "main").unwrap();
        assert_eq!(
            main.suspended,
            Some(SuspensionInfo {
                team: SUSPENDED_TEAM.to_string(),
                team_label: "Family".to_string(),
                notice: Some(SUSPENSION_NOTICE.to_string()),
            })
        );
        let other = listed.catalogs.iter().find(|c| c.name == "other").unwrap();
        assert_eq!(other.suspended, None, "the other team's catalog is active");
        assert_eq!(
            listed.selection,
            ["other"],
            "the connection keeps the active catalog"
        );
        assert_eq!(listed.write_target.as_deref(), Some("other"));
        assert!(only_suspended.selection.is_empty());
        assert!(
            only_suspended
                .selection_error
                .as_deref()
                .is_some_and(|error| error.contains("suspended")),
            "{:?}",
            only_suspended.selection_error
        );
        let names: Vec<&str> = strangers.catalogs.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            ["other"],
            "a key that does not reach main never sees it"
        );
        assert!(
            admins.catalogs[0].suspended.is_some(),
            "an admin sees the mark"
        );
        assert_eq!(admins.selection, ["main", "other"], "and selects both");
    }

    /// M18 Task 10: `after` continues the list by name, narrowed as the
    /// first page is, and a malformed one is refused.
    #[tokio::test(flavor = "multi_thread")]
    async fn after_continues_the_list_and_a_malformed_one_is_refused() {
        use crate::cache::directory::cursor_of;
        let test = app_with(
            &[("pg", &[]), ("work", &[]), ("home", &[])],
            Config::default(),
        )
        .await;
        let every = principal(&[], None);

        let after_home = list(
            &context(&test, Arc::clone(&every), None),
            Some(&cursor_of("home")),
        )
        .await
        .unwrap();
        let narrowed = list(
            &context(&test, Arc::clone(&every), Some(&["work", "home"])),
            Some(&cursor_of("pg")),
        )
        .await
        .unwrap();
        let refused = list(&context(&test, every, None), Some("not a cursor")).await;

        let names: Vec<&str> = after_home
            .catalogs
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(names, ["pg", "work"]);
        assert_eq!(after_home.next, None);
        let names: Vec<&str> = narrowed.catalogs.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["work"], "the connection's list still narrows it");
        assert!(
            matches!(&refused, Err(OkfError::InvalidPath { path, .. }) if path == "after"),
            "{refused:?}"
        );
    }

    #[test]
    fn the_server_lists_the_tool() {
        let tools = OkfServer::tools().list_all();

        let listed = tools
            .iter()
            .find(|tool| tool.name == "list_catalogs")
            .expect("list_catalogs is registered");
        let schema = listed.output_schema.as_ref().expect("an output schema");
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["selection_error"].is_object());
    }
}
