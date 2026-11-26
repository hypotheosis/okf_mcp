//! Ranked full-text search within one catalog snapshot.
//!
//! A concept matches when every whitespace-separated term of the query
//! occurs (case-insensitively, as a substring) in at least one of its fields.
//! Fields, best first: title or alias, tag, description, body. The tier is
//! the best field holding the whole query (terms joined by one space), else
//! 4 plus the best field holding any term. Deprecated concepts sort after the
//! rest; ties break by path.
//!
//! `okf.py` has no search command, so there is nothing to keep parity with:
//! the rules above (decision 3 of the M2 plan) and this module's tests are
//! the specification. [`search_catalogs`] (behind M4's `search` tool) runs
//! [`search`] per catalog and merges the hits by [`SearchHit::rank`], then
//! catalog order, then path.

use std::cmp::Ordering;
use std::collections::HashMap;

// `rayon::prelude` brings `par_iter` into scope (trait methods are only
// callable when the trait is imported).
use rayon::prelude::*;

use crate::okf::caller::Caller;
use serde::Serialize;

use crate::okf::markdown::split_frontmatter;
use crate::okf::pystr::py_strip;
use crate::okf::snapshot::{Snapshot, SnapshotDoc};
use crate::okf::yaml::Value;
use crate::render::{block_text, excerpt};

/// Snippets are cut to this many characters.
const SNIPPET_LEN: usize = 160;

/// The ranked fields. The discriminants are the tiers (`field as u8`), and
/// the derived `Ord` follows declaration order, so `Names` is the best.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Field {
    Names = 0,
    Tags = 1,
    Description = 2,
    Body = 3,
}

impl Field {
    /// Every field, best first.
    const ALL: [Field; 4] = [Field::Names, Field::Tags, Field::Description, Field::Body];
    /// Added to the tier when only single terms (not the whole query) match.
    const COUNT: u8 = Field::ALL.len() as u8;
}

/// A concept's lowercased search fields, built once per snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchFields {
    /// Title and aliases.
    names: Vec<String>,
    tags: Vec<String>,
    description: String,
    body: String,
    concept_type: String,
    /// `status`, or `stable` when it is missing, empty or not a string.
    status: String,
}

impl SearchFields {
    /// Builds the fields from a parsed concept.
    ///
    /// # Arguments
    /// * `doc` - the concept; its title and aliases must already be set.
    ///
    /// # Returns
    /// The lowercased fields. Non-string `type`, `description` and `status`
    /// count as empty; a scalar `tags` counts as a one-item list.
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) fn of(doc: &SnapshotDoc) -> Self {
        let data = doc.frontmatter().data();
        // A closure (Python lambda) capturing `data`; `unwrap_or_default` on
        // an `Option<&str>` gives `""` for a missing key.
        let text = |key: &str| {
            data.and_then(|d| d.get(key))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_lowercase()
        };
        let tags = match data.and_then(|d| d.get("tags")) {
            Some(Value::Seq(items)) => items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_lowercase)
                .collect(),
            Some(Value::Str(tag)) => vec![tag.to_lowercase()],
            Some(Value::Null | Value::Map(_)) | None => Vec::new(),
        };
        // `Option::into_iter` yields zero or one item, so the title (if any)
        // and the aliases chain into one list.
        let names = doc
            .title()
            .into_iter()
            .chain(doc.aliases().iter().map(String::as_str))
            .map(str::to_lowercase)
            .collect();
        let status = text("status");
        Self {
            names,
            tags,
            description: text("description"),
            body: split_frontmatter(doc.text()).1.to_lowercase(),
            concept_type: text("type"),
            status: if status.is_empty() {
                "stable".to_string()
            } else {
                status
            },
        }
    }

    /// Does `field` contain the lowercased `needle`?
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "per-element predicate or scanner called in a hot loop (parser or analysis)"
        )
    )]
    fn contains(&self, field: Field, needle: &str) -> bool {
        match field {
            Field::Names => self.names.iter().any(|n| n.contains(needle)),
            Field::Tags => self.tags.iter().any(|t| t.contains(needle)),
            Field::Description => self.description.contains(needle),
            Field::Body => self.body.contains(needle),
        }
    }

    /// The best field containing `needle`, or `None` if none does.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "per-element predicate or scanner called in a hot loop (parser or analysis)"
        )
    )]
    fn best_field(&self, needle: &str) -> Option<Field> {
        Field::ALL
            .into_iter()
            .find(|&field| self.contains(field, needle))
    }
}

/// A search request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchQuery {
    /// Free text; empty (or only whitespace) matches every concept that
    /// passes the filters.
    pub text: String,
    /// Only concepts of this `type` (case-insensitive).
    pub concept_type: Option<String>,
    /// Only concepts carrying every one of these tags (case-insensitive,
    /// whole tags).
    pub tags: Vec<String>,
    /// Only concepts with this `status` (case-insensitive; missing counts as
    /// `stable`).
    pub status: Option<String>,
    /// At most this many hits.
    pub limit: usize,
    /// Also make each hit's [`SearchHit::snippet_text`] (the web API's
    /// reader asks for it; the MCP tool does not).
    pub plain_snippets: bool,
}

impl SearchQuery {
    /// A query for `text` with no filters and the design's default limit (20).
    ///
    /// # Arguments
    /// * `text` - the free text.
    ///
    /// # Returns
    /// The query; set the public fields to add filters or change the limit.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn new(text: &str) -> Self {
        Self {
            text: text.to_string(),
            concept_type: None,
            tags: Vec::new(),
            status: None,
            limit: 20,
            plain_snippets: false,
        }
    }
}

/// How a hit ranks. The derived `Ord` compares fields in declaration order,
/// so it sorts best first: non-deprecated before deprecated, then by tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SearchRank {
    /// Deprecated concepts sort after all others (`false < true`).
    pub deprecated: bool,
    /// 0-3 for the field holding the whole query, 4-7 for the best field
    /// holding a term (0 = names, 1 = tags, 2 = description, 3 = body).
    pub tier: u8,
}

/// One result.
// `JsonSchema` gives the `search` tool its output schema; the skipped `rank`
// is left out of it, as it is out of the JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct SearchHit {
    /// `/`-prefixed concept path.
    pub path: String,
    /// The concept's title.
    pub title: Option<String>,
    /// Its description, as written.
    pub description: Option<String>,
    /// The first body line holding the whole query, else the first holding
    /// any term, stripped and cut to 160 characters; `None` for an empty
    /// query or when no body line matches.
    pub snippet: Option<String>,
    /// The snippet's text without markdown syntax, with `plain_snippets`:
    /// the text of the block holding the snippet's line, from that line on,
    /// cut to 160 characters with an ellipsis. Not serialized:
    /// the web API copies it into its own answer.
    #[serde(skip)]
    pub snippet_text: Option<String>,
    /// Lines in the concept's whole file, frontmatter included: about what
    /// a `get_concept` or `read_document` of it puts in context. A last line
    /// without a trailing newline counts; an empty file has 0.
    pub lines: usize,
    /// Characters (Unicode code points, not bytes) in the whole file,
    /// frontmatter included, counted with `\r\n` line endings read as `\n`.
    pub chars: usize,
    /// The ranking key ([`search_catalogs`] merges catalogs by it); not
    /// serialized.
    #[serde(skip)]
    pub rank: SearchRank,
}

/// The query, lowercased once.
struct Prepared {
    terms: Vec<String>,
    /// The terms joined by one space.
    phrase: String,
}

/// Searches the concepts of one snapshot.
///
/// # Arguments
/// * `snapshot` - the catalog.
/// * `query` - text, filters and limit.
///
/// # Returns
/// At most `query.limit` hits, best first ([`SearchRank`], then path).
#[tracing::instrument(level = "debug", skip_all)]
pub fn search(snapshot: &Snapshot, query: &SearchQuery) -> Vec<SearchHit> {
    if query.limit == 0 {
        return Vec::new();
    }
    let terms: Vec<String> = query
        .text
        .split_whitespace()
        .map(str::to_lowercase)
        .collect();
    let prepared = Prepared {
        phrase: terms.join(" "),
        terms,
    };
    let wanted_tags: Vec<String> = query.tags.iter().map(|t| t.to_lowercase()).collect();
    let wanted_type = query.concept_type.as_deref().map(str::to_lowercase);
    let wanted_status = query.status.as_deref().map(str::to_lowercase);

    // Matching is independent per document, so it runs in parallel; the
    // `?` inside `filter_map`'s closure returns `None` (skip) early.
    // `map_init` re-enters the caller's span on each worker (see `okf::caller`);
    // `flatten` then drops the `None`s that `filter_map` would have dropped.
    let caller = Caller::capture();
    let mut ranked: Vec<(SearchRank, &SnapshotDoc, &SearchFields)> = snapshot
        .docs()
        .par_iter()
        .map_init(
            || caller.enter(),
            |_entered, doc| {
                let fields = doc.search_fields()?;
                let filtered_out = wanted_type
                    .as_ref()
                    .is_some_and(|t| *t != fields.concept_type)
                    || wanted_status.as_ref().is_some_and(|s| *s != fields.status)
                    || !wanted_tags.iter().all(|t| fields.tags.contains(t));
                if filtered_out {
                    return None;
                }
                let rank = SearchRank {
                    deprecated: fields.status == "deprecated",
                    tier: tier(fields, &prepared)?,
                };
                Some((rank, doc, fields))
            },
        )
        .flatten()
        .collect();

    let order = |a: &(SearchRank, &SnapshotDoc, &SearchFields),
                 b: &(SearchRank, &SnapshotDoc, &SearchFields)|
     -> Ordering { a.0.cmp(&b.0).then_with(|| a.1.path().cmp(b.1.path())) };
    // Only the best `limit` need sorting: partition around the cut first
    // (average O(n)), then sort the kept `limit` items.
    if ranked.len() > query.limit {
        ranked.select_nth_unstable_by(query.limit - 1, order);
        ranked.truncate(query.limit);
    }
    // Paths are unique, so the order is total and an unstable sort is exact.
    ranked.sort_unstable_by(order);
    ranked
        .into_iter()
        .map(|(rank, doc, fields)| hit(doc, fields, rank, &prepared, query.plain_snippets))
        .collect()
}

/// One hit of a search across catalogs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct CatalogHit {
    /// The catalog holding the concept.
    pub catalog: String,
    /// The hit; `flatten` writes its fields beside `catalog` in the JSON
    /// (and in the schema), rather than nested under a `hit` key.
    #[serde(flatten)]
    pub hit: SearchHit,
}

/// Searches several catalogs and merges the hits: best [`SearchRank`] first,
/// then catalog order, then path.
///
/// # Arguments
/// * `catalogs` - `(name, snapshot)` pairs, in the selection's order.
/// * `query` - text, filters and limit.
///
/// # Returns
/// At most `query.limit` hits.
#[tracing::instrument(level = "debug", skip_all)]
pub fn search_catalogs(catalogs: &[(&str, &Snapshot)], query: &SearchQuery) -> Vec<CatalogHit> {
    // Each search already runs across its documents with rayon and returns
    // at most `limit` hits, so the merge sorts at most catalogs x limit.
    let mut ranked: Vec<(usize, SearchHit)> = catalogs
        .iter()
        .enumerate()
        // `move` hands the closure its own copy of `order` (a plain `usize`),
        // since the inner iterator outlives this call of the outer closure.
        .flat_map(|(order, (_, snapshot))| {
            search(snapshot, query)
                .into_iter()
                .map(move |hit| (order, hit))
        })
        .collect();
    // A path is unique within a catalog, so (catalog order, path) is unique
    // and the order is total: an unstable sort is exact.
    ranked.sort_unstable_by(|(left_order, left), (right_order, right)| {
        left.rank
            .cmp(&right.rank)
            .then(left_order.cmp(right_order))
            .then_with(|| left.path.cmp(&right.path))
    });
    ranked.truncate(query.limit);
    ranked
        .into_iter()
        .map(|(order, hit)| CatalogHit {
            catalog: catalogs[order].0.to_string(),
            hit,
        })
        .collect()
}

/// What a `sources` entry must match to count as citing a source
/// (issue #761). It knows nothing of the `cited_by` tool's arguments, so a
/// `sources` filter on `search` can build one from its own.
///
/// `id` matches case-insensitively and `resource` case-sensitively, both
/// exactly (no substring). With both set, one entry must match both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceMatch {
    /// The entry's `id`, lowercased; `None` leaves the id unconstrained.
    id: Option<String>,
    /// The entry's `resource`, as given; `None` leaves it unconstrained.
    resource: Option<String>,
}

impl SourceMatch {
    /// Builds a matcher.
    ///
    /// # Arguments
    /// * `id` - the entry id to match, case-insensitively.
    /// * `resource` - the resource to match, exactly.
    ///
    /// # Returns
    /// The matcher, or `None` when neither is given (it would match every
    /// entry).
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn new(id: Option<&str>, resource: Option<&str>) -> Option<Self> {
        if id.is_none() && resource.is_none() {
            return None;
        }
        Some(Self {
            id: id.map(str::to_lowercase),
            resource: resource.map(str::to_string),
        })
    }

    /// The entry this `sources` item is, if it matches.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "per-element predicate or scanner called in a hot loop (parser or analysis)"
        )
    )]
    fn entry(&self, item: &Value) -> Option<CitedEntry> {
        let Value::Map(map) = item else { return None };
        let text = |key: &str| map.get(key).and_then(Value::as_str);
        let id = text("id");
        let resource = text("resource");
        let id_matches = self
            .id
            .as_ref()
            .is_none_or(|wanted| id.is_some_and(|id| id.to_lowercase() == *wanted));
        let resource_matches = self
            .resource
            .as_ref()
            .is_none_or(|wanted| resource == Some(wanted.as_str()));
        (id_matches && resource_matches).then(|| CitedEntry {
            id: id.map(str::to_string),
            resource: resource.map(str::to_string),
            title: text("title").map(str::to_string),
            author: text("author").map(str::to_string),
        })
    }
}

/// One `sources` entry that matched, as a hit shows it. A `search` hit could
/// carry a list of these as an optional field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct CitedEntry {
    /// The entry's id, unique within its concept.
    pub id: Option<String>,
    /// What was consulted: a URI, a path or a description.
    pub resource: Option<String>,
    /// The entry's title, if it has one.
    pub title: Option<String>,
    /// The entry's author, if it has one.
    pub author: Option<String>,
}

/// The concepts of a snapshot that cite a source.
///
/// # Arguments
/// * `snapshot` - the catalog.
/// * `wanted` - what an entry must match.
///
/// # Returns
/// Each citing concept's path with its matching entries (at least one), in
/// snapshot order; a caller that needs a fixed order sorts by path. A
/// concept whose `sources` is not a list cites nothing.
#[tracing::instrument(level = "debug", skip_all)]
pub fn cites<'a>(
    snapshot: &'a Snapshot,
    wanted: &'a SourceMatch,
) -> impl Iterator<Item = (&'a str, Vec<CitedEntry>)> + 'a {
    snapshot.concepts().filter_map(move |doc| {
        let Some(Value::Seq(items)) = doc.frontmatter().data().and_then(|d| d.get("sources"))
        else {
            return None;
        };
        let matched: Vec<CitedEntry> = items.iter().filter_map(|item| wanted.entry(item)).collect();
        (!matched.is_empty()).then(|| (doc.path(), matched))
    })
}

/// The tier, or `None` when some term occurs in no field.
#[tracing::instrument(level = "trace", skip_all)]
fn tier(fields: &SearchFields, query: &Prepared) -> Option<u8> {
    if query.terms.is_empty() {
        return Some(0);
    }
    // The best field holding any term is the best of each term's best field;
    // `try_fold` stops with `None` as soon as one term is found nowhere.
    let best_term = query.terms.iter().try_fold(Field::Body, |best, term| {
        fields.best_field(term).map(|field| best.min(field))
    })?;
    // A one-term phrase is the term itself.
    if query.terms.len() == 1 {
        return Some(best_term as u8);
    }
    // A field holding the phrase holds every term, so no field better than
    // `best_term` can hold it: start the phrase search there.
    let phrase_field = Field::ALL[best_term as usize..]
        .iter()
        .find(|&&field| fields.contains(field, &query.phrase));
    Some(phrase_field.map_or(Field::COUNT + best_term as u8, |&field| field as u8))
}

/// Builds the result for a ranked concept.
#[tracing::instrument(level = "trace", skip_all)]
fn hit(
    doc: &SnapshotDoc,
    fields: &SearchFields,
    rank: SearchRank,
    query: &Prepared,
    plain: bool,
) -> SearchHit {
    let description = doc
        .frontmatter()
        .data()
        .and_then(|d| d.get("description"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let text = doc.text();
    let body = split_frontmatter(text).1;
    let matched = matched_line(body, &fields.body, query);
    let snippet = matched.map(|(_, line)| cut(line));
    // `bool::then` runs the closure only when asked; `flatten` turns the
    // `Option<Option<_>>` into one. The line's byte range is `start` up to
    // its length (its `\n` excluded).
    let snippet_text = plain
        .then(|| matched.map(|(start, line)| block_text(body, start..start + line.len())))
        .flatten()
        .filter(|text| !text.is_empty())
        .map(|text| excerpt(&text, SNIPPET_LEN));
    SearchHit {
        path: format!("/{}", doc.path()),
        title: doc.title().map(str::to_string),
        description,
        snippet,
        snippet_text,
        // Measured only for this catalog's (at most `limit`) hits, not every
        // match; `search_catalogs` merges several catalogs' hits and then cuts
        // to `limit`, so it measures at most catalogs x limit documents.
        // `str::lines` splits on `\n` (and `\r\n`) and does not count an
        // empty piece after a final newline, as `wc -l` would not either.
        lines: text.lines().count(),
        chars: text.chars().count(),
        rank,
    }
}

/// The first body line holding the phrase, else the first holding any term,
/// with the byte offset where it starts in `body`.
///
/// `lowered` is `body.to_lowercase()`. Lowercasing never adds or removes a
/// `\n`, so the two split into the same lines and can be zipped, which avoids
/// lowercasing the body again. The offsets are `body`'s (the original
/// lines' lengths), never `lowered`'s.
#[tracing::instrument(level = "trace", skip_all)]
fn matched_line<'a>(body: &'a str, lowered: &str, query: &Prepared) -> Option<(usize, &'a str)> {
    if query.terms.is_empty() {
        return None;
    }
    let mut first_term_line = None;
    let mut start = 0;
    for (line, lower) in body.split('\n').zip(lowered.split('\n')) {
        if lower.contains(query.phrase.as_str()) {
            return Some((start, line));
        }
        if first_term_line.is_none() && query.terms.iter().any(|t| lower.contains(t.as_str())) {
            first_term_line = Some((start, line));
        }
        // The next line starts after this one and its `\n`.
        start += line.len() + 1;
    }
    first_term_line
}

/// A line stripped and cut to [`SNIPPET_LEN`] characters (not bytes).
#[tracing::instrument(level = "trace", skip_all)]
fn cut(line: &str) -> String {
    let text = py_strip(line);
    // `char_indices().nth(n)` finds the byte offset of the n-th character, so
    // the cut never splits a multi-byte UTF-8 character.
    text.char_indices()
        .nth(SNIPPET_LEN)
        .map_or(text, |(end, _)| &text[..end])
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::okf::workspace::MemWorkspace;

    fn snapshot() -> Snapshot {
        Snapshot::build(&MemWorkspace::from_files([
            (
                "infra/time-server.md",
                "---\ntype: Service\ntitle: GPS time server\ndescription: Stratum 1 NTP.\n\
                 aliases: [NTP server]\ntags: [ntp, time]\n---\nServes time to the LAN.\n",
            ),
            (
                "infra/clock.md",
                "---\ntype: Host\ntitle: Wall clock\ndescription: Shows the time server's time.\n\
                 tags: [Time]\n---\nA clock.\n",
            ),
            (
                "infra/old-ntp.md",
                "---\ntype: Service\ntitle: Old NTP server\nstatus: deprecated\n---\nRetired.\n",
            ),
            (
                "notes/body.md",
                "---\ntype: Note\ntitle: Notes\n---\nfirst line\nWe asked the TIME SERVER twice.\n",
            ),
            ("index.md", "time server\n"),
        ]))
        .unwrap()
    }

    fn paths(hits: &[SearchHit]) -> Vec<&str> {
        hits.iter().map(|h| h.path.as_str()).collect()
    }

    #[test]
    fn ranks_by_field_then_deprecated_last() {
        // The phrase is in time-server's title (tier 0), clock's description
        // (tier 2) and body.md's body (tier 3); old-ntp has no "time" at all.
        let hits = search(&snapshot(), &SearchQuery::new("time server"));
        assert_eq!(
            paths(&hits),
            ["/infra/time-server.md", "/infra/clock.md", "/notes/body.md"]
        );
        let tiers: Vec<u8> = hits.iter().map(|h| h.rank.tier).collect();
        assert_eq!(tiers, [0, 2, 3]);
        // Both match "ntp server" in a name (tier 0); the deprecated one sorts last.
        let hits = search(&snapshot(), &SearchQuery::new("ntp server"));
        assert_eq!(paths(&hits), ["/infra/time-server.md", "/infra/old-ntp.md"]);
        assert!(hits[1].rank.deprecated && hits[1].rank.tier == 0);
    }

    #[test]
    fn every_term_must_match_somewhere() {
        assert!(search(&snapshot(), &SearchQuery::new("time banana")).is_empty());
        assert_eq!(
            paths(&search(&snapshot(), &SearchQuery::new("lan gps"))),
            ["/infra/time-server.md"]
        );
    }

    #[test]
    fn split_terms_rank_by_their_best_field_plus_four() {
        // "lan" is only in the body, "gps" in the title, the phrase nowhere.
        let hits = search(&snapshot(), &SearchQuery::new("lan gps"));
        assert_eq!(hits[0].rank.tier, 4);
        // "clock" is clock's title, "shows" its description: names win.
        let hits = search(&snapshot(), &SearchQuery::new("shows clock"));
        assert_eq!(paths(&hits), ["/infra/clock.md"]);
        assert_eq!(hits[0].rank.tier, 4);
        // One term is its own phrase, so it ranks in 0-3.
        let hits = search(&snapshot(), &SearchQuery::new("RETIRED"));
        assert_eq!(paths(&hits), ["/infra/old-ntp.md"]);
        assert_eq!(hits[0].rank.tier, 3);
    }

    #[test]
    fn every_tier_is_reachable() {
        let snapshot = Snapshot::build(&MemWorkspace::from_files([(
            "a.md",
            "---\ntitle: Alpha\ntags: [time sync]\ndescription: Keeps clocks right.\n---\n\
             Chrony runs here.\n",
        )]))
        .unwrap();
        let tier = |text: &str| search(&snapshot, &SearchQuery::new(text))[0].rank.tier;
        // The whole multi-word query inside one tag.
        assert_eq!(tier("time sync"), 1);
        // The phrase is nowhere; the best field holding a term decides.
        assert_eq!(tier("chrony alpha"), 4);
        assert_eq!(tier("chrony sync"), 5);
        assert_eq!(tier("chrony clocks"), 6);
        assert_eq!(tier("runs chrony"), 7);
    }

    #[test]
    fn snippets_show_the_first_matching_body_line() {
        let hits = search(&snapshot(), &SearchQuery::new("time server"));
        let body = hits.iter().find(|h| h.path == "/notes/body.md").unwrap();
        assert_eq!(
            body.snippet.as_deref(),
            Some("We asked the TIME SERVER twice.")
        );
        assert_eq!(hits[0].title.as_deref(), Some("GPS time server"));
        assert_eq!(hits[0].description.as_deref(), Some("Stratum 1 NTP."));
        // No body line holds the phrase, so the first line with a term wins.
        let hits = search(&snapshot(), &SearchQuery::new("lan gps"));
        assert_eq!(hits[0].snippet.as_deref(), Some("Serves time to the LAN."));
        // An empty query has no snippet.
        assert!(search(&snapshot(), &SearchQuery::new(""))
            .iter()
            .all(|h| h.snippet.is_none()));
    }

    #[test]
    fn snippets_are_stripped_and_cut_by_characters() {
        let line = "\u{e9}".repeat(200);
        let text = format!("---\ntitle: Long\n---\n  {line}  \n");
        let snapshot =
            Snapshot::build(&MemWorkspace::from_files([("a.md", text.as_str())])).unwrap();
        let hits = search(&snapshot, &SearchQuery::new("\u{c9}"));
        let snippet = hits[0].snippet.as_deref().unwrap();
        assert_eq!(snippet, "\u{e9}".repeat(SNIPPET_LEN));
    }

    #[test]
    fn filters_and_limit() {
        let mut query = SearchQuery::new("");
        query.tags = vec!["TIME".to_string()];
        assert_eq!(
            paths(&search(&snapshot(), &query)),
            ["/infra/clock.md", "/infra/time-server.md"]
        );
        let mut query = SearchQuery::new("server");
        query.concept_type = Some("service".to_string());
        query.status = Some("stable".to_string());
        assert_eq!(
            paths(&search(&snapshot(), &query)),
            ["/infra/time-server.md"]
        );
        let mut query = SearchQuery::new("time");
        query.limit = 1;
        assert_eq!(
            paths(&search(&snapshot(), &query)),
            ["/infra/time-server.md"]
        );
        query.limit = 0;
        assert!(search(&snapshot(), &query).is_empty());
    }

    #[test]
    fn only_concepts_carry_search_fields() {
        let snapshot = snapshot();
        assert!(snapshot.get("index.md").unwrap().search_fields().is_none());
        assert!(snapshot
            .get("notes/body.md")
            .unwrap()
            .search_fields()
            .is_some());
    }

    #[test]
    fn hits_serialize_without_the_rank() {
        let hits = search(&snapshot(), &SearchQuery::new("retired"));
        assert_eq!(
            serde_json::to_string(&hits[0]).unwrap(),
            r#"{"path":"/infra/old-ntp.md","title":"Old NTP server","description":null,"snippet":"Retired.","lines":6,"chars":72}"#
        );
    }

    #[test]
    fn hits_measure_the_whole_file_with_or_without_frontmatter() {
        let with_frontmatter = "---\ntitle: \u{c9}t\u{e9}\n---\nBody \u{e9}t\u{e9}.\nMore.";
        let without = "# \u{c9}t\u{e9}\n\nBody \u{e9}t\u{e9}.\n";
        let snapshot = Snapshot::build(&MemWorkspace::from_files([
            ("a.md", with_frontmatter),
            ("b.md", without),
        ]))
        .unwrap();

        let hits = search(&snapshot, &SearchQuery::new("body"));

        let sizes: Vec<(&str, usize, usize)> = hits
            .iter()
            .map(|h| (h.path.as_str(), h.lines, h.chars))
            .collect();
        // a.md: 5 lines, the last unterminated; b.md: 3 lines. Characters
        // are code points, so each accented letter counts once.
        assert_eq!(
            sizes,
            [
                ("/a.md", 5, with_frontmatter.chars().count()),
                ("/b.md", 3, without.chars().count()),
            ]
        );
        assert_eq!(with_frontmatter.chars().count(), 34);
        assert_eq!(without.chars().count(), 17);
    }

    #[test]
    fn an_empty_concept_has_no_lines_or_characters() {
        let snapshot = Snapshot::build(&MemWorkspace::from_files([("empty.md", "")])).unwrap();

        let hits = search(&snapshot, &SearchQuery::new(""));

        assert_eq!(paths(&hits), ["/empty.md"]);
        assert_eq!((hits[0].lines, hits[0].chars), (0, 0));
    }

    #[test]
    fn crlf_line_endings_count_as_one_character() {
        let snapshot = Snapshot::build(&MemWorkspace::from_files([(
            "a.md",
            "---\r\ntitle: A\r\n---\r\nBody.\r\n",
        )]))
        .unwrap();

        let hits = search(&snapshot, &SearchQuery::new("body"));

        // As the LF file "---\ntitle: A\n---\nBody.\n": 4 lines, 23 characters.
        assert_eq!((hits[0].lines, hits[0].chars), (4, 23));
    }

    #[test]
    fn search_catalogs_merges_by_rank_then_catalog_order_then_path() {
        // "time" is in work's z.md title (tier 0) but only in the bodies of
        // pg's a.md and b.md and of work's a.md (tier 3). Among those, pg
        // comes first (catalog order), even though work's a.md sorts before
        // pg's b.md by path.
        let pg = Snapshot::build(&MemWorkspace::from_files([
            ("b.md", "---\ntype: Note\ntitle: Beta\n---\nAbout time.\n"),
            ("a.md", "---\ntype: Note\ntitle: Alpha\n---\nAbout time.\n"),
        ]))
        .unwrap();
        let work = Snapshot::build(&MemWorkspace::from_files([
            ("z.md", "---\ntype: Note\ntitle: Time\n---\nText.\n"),
            ("a.md", "---\ntype: Note\ntitle: Gamma\n---\nAbout time.\n"),
        ]))
        .unwrap();
        let mut query = SearchQuery::new("time");

        let all = search_catalogs(&[("pg", &pg), ("work", &work)], &query);
        query.limit = 2;
        let two = search_catalogs(&[("pg", &pg), ("work", &work)], &query);

        let order = |hits: &[CatalogHit]| -> Vec<(String, String)> {
            hits.iter()
                .map(|hit| (hit.catalog.clone(), hit.hit.path.clone()))
                .collect()
        };
        assert_eq!(
            order(&all),
            [
                ("work".to_string(), "/z.md".to_string()),
                ("pg".to_string(), "/a.md".to_string()),
                ("pg".to_string(), "/b.md".to_string()),
                ("work".to_string(), "/a.md".to_string()),
            ]
        );
        assert_eq!(order(&two), order(&all)[..2]);
        let json = serde_json::to_value(&all[0]).unwrap();
        assert_eq!(json["catalog"], "work");
        assert_eq!(json["path"], "/z.md");
    }

    #[test]
    fn cites_matches_entries_exactly_and_ignores_odd_sources() {
        let snapshot = Snapshot::build(&MemWorkspace::from_files([
            (
                "a.md",
                "---\ntype: Note\nsources:\n  - id: Diary\n    resource: R1\n  - id: x\n    resource: R2\n---\n",
            ),
            ("b.md", "---\ntype: Note\nsources: Diary\n---\n"),
            ("c.md", "---\ntype: Note\nsources:\n  - Diary\n---\n"),
        ]))
        .unwrap();
        let found = |id, resource| -> Vec<(String, usize)> {
            let wanted = SourceMatch::new(id, resource).unwrap();
            cites(&snapshot, &wanted)
                .map(|(path, entries)| (path.to_string(), entries.len()))
                .collect()
        };

        assert_eq!(found(Some("diary"), None), [("a.md".to_string(), 1)]);
        assert_eq!(found(None, Some("R2")), [("a.md".to_string(), 1)]);
        assert!(found(Some("diary"), Some("R2")).is_empty());
        assert!(found(None, Some("r1")).is_empty());
        assert!(SourceMatch::new(None, None).is_none());
    }

    /// The hit for `body` (the one concept of a catalog) searched for
    /// `query` with plain snippets.
    fn plain_hit(body: &str, query: &str) -> SearchHit {
        let snapshot = Snapshot::build(&MemWorkspace::from_files([(
            "a.md",
            format!("---\ntitle: A\n---\n{body}").as_str(),
        )]))
        .expect("snapshot");
        let mut search_query = SearchQuery::new(query);
        search_query.plain_snippets = true;
        search(&snapshot, &search_query).remove(0)
    }

    #[test]
    fn a_plain_snippet_reads_emphasis_that_crosses_the_cut() {
        let line = format!(
            "{} **bold words that run past the cut** marmalade",
            "x".repeat(150)
        );
        let hit = plain_hit(&format!("{line}\n"), "marmalade");
        // The raw snippet is unchanged: the line cut at 160 characters, mid-span.
        assert_eq!(hit.snippet.as_deref(), Some(&line[..160]));
        let plain = hit.snippet_text.expect("a plain snippet");
        assert!(!plain.contains('*'), "{plain}");
        assert!(plain.starts_with(&"x".repeat(150)), "{plain}");
        assert!(
            plain.ends_with('\u{2026}') && plain.chars().count() <= SNIPPET_LEN,
            "{plain}"
        );
    }

    #[test]
    fn a_plain_snippet_reads_a_link_that_crosses_the_cut() {
        let line = format!(
            "{} [a link whose text runs past the cut](b.md) marmalade",
            "y".repeat(150)
        );
        let plain = plain_hit(&format!("{line}\n"), "marmalade")
            .snippet_text
            .expect("a plain snippet");
        assert!(!plain.contains('[') && !plain.contains("]("), "{plain}");
        assert!(plain.contains("a link"), "{plain}");
    }

    #[test]
    fn a_plain_snippet_resolves_a_reference_link_and_drops_code_fences() {
        let reference = plain_hit(
            "Read [the guide][g] for marmalade.\n\n[g]: guide.md\n",
            "marmalade",
        );
        assert_eq!(
            reference.snippet_text.as_deref(),
            Some("Read the guide for marmalade.")
        );
        let fenced = plain_hit("Intro.\n\n```sh\nmarmalade --serve\n```\n", "marmalade");
        assert_eq!(fenced.snippet.as_deref(), Some("marmalade --serve"));
        assert_eq!(fenced.snippet_text.as_deref(), Some("marmalade --serve"));
        // The match on a later line of the fence starts the plain snippet,
        // even after a line long enough to fill the cut on its own.
        let later = plain_hit(
            &format!(
                "```sh\nunrelated setup {}\nmarmalade --serve\n```\n",
                "z".repeat(200)
            ),
            "marmalade",
        );
        assert_eq!(later.snippet_text.as_deref(), Some("marmalade --serve"));
    }

    #[test]
    fn a_plain_snippet_reads_a_quote_a_list_item_and_a_table_row() {
        for (body, plain) in [
            ("> marmalade on toast\n", "marmalade on toast"),
            ("- parent\n  - **child** marmalade\n", "child marmalade"),
            (
                "| name | note |\n|---|---|\n| jam | marmalade |\n",
                "jam marmalade",
            ),
        ] {
            let hit = plain_hit(body, "marmalade");
            assert_eq!(hit.snippet_text.as_deref(), Some(plain), "{body}");
        }
    }

    #[test]
    fn plain_snippets_are_made_only_when_asked_for() {
        let hits = search(&snapshot(), &SearchQuery::new("time server"));
        assert!(hits.iter().all(|hit| hit.snippet_text.is_none()));
    }
}

/// One source as `cited_by` lists it when given no filter: the grouping key
/// of its entries, how many concepts cite it, and a title.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct CitedSource {
    /// The entry id (the spelling that sorts first when entries differ only in case).
    pub source_id: Option<String>,
    /// What was consulted: a URI, a path or a description.
    pub resource: Option<String>,
    /// The source's title (the one that sorts first when entries differ).
    pub title: Option<String>,
    /// How many concepts cite it, each counted once.
    pub cited_by: usize,
}

/// The grouping key of a source: `id` lowercased and `resource` as given,
/// the two comparisons [`SourceMatch`] makes.
type SourceKey = (Option<String>, Option<String>);

/// Counts, in one pass over the catalogs, how many concepts cite each source.
#[derive(Debug, Default)]
pub struct SourceTally {
    /// Each key's first-sorting original id, first-sorting title and count.
    groups: HashMap<SourceKey, (Option<String>, Option<String>, usize)>,
}

impl SourceTally {
    /// Adds every concept of `snapshot`; a concept citing one source
    /// several times counts once for it.
    ///
    /// # Arguments
    /// * `snapshot` - the catalog.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn add(&mut self, snapshot: &Snapshot) {
        for doc in snapshot.concepts() {
            let Some(Value::Seq(items)) = doc.frontmatter().data().and_then(|d| d.get("sources"))
            else {
                continue;
            };
            let mut seen: Vec<SourceKey> = Vec::with_capacity(items.len());
            for item in items {
                let Value::Map(map) = item else { continue };
                let text = |key: &str| map.get(key).and_then(Value::as_str);
                let key = (
                    text("id").map(str::to_lowercase),
                    text("resource").map(str::to_string),
                );
                if key == (None, None) || seen.contains(&key) {
                    continue;
                }
                let group = self.groups.entry(key.clone()).or_default();
                group.2 += 1;
                keep_smaller(&mut group.0, text("id"));
                keep_smaller(&mut group.1, text("title"));
                seen.push(key);
            }
        }
    }

    /// The sources by count descending, then id, then resource, cut to `limit`.
    ///
    /// # Arguments
    /// * `limit` - the most sources to return.
    ///
    /// # Returns
    /// The kept sources, and whether more existed.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn top(self, limit: usize) -> (Vec<CitedSource>, bool) {
        let mut all: Vec<CitedSource> = self
            .groups
            .into_iter()
            .map(
                |((_, resource), (source_id, title, cited_by))| CitedSource {
                    source_id,
                    resource,
                    title,
                    cited_by,
                },
            )
            .collect();
        // The key (lowercased id, resource) is unique, so this order is
        // total apart from ids that differ only in case; the resource breaks that.
        let order = |a: &CitedSource, b: &CitedSource| {
            b.cited_by
                .cmp(&a.cited_by)
                .then_with(|| a.source_id.cmp(&b.source_id))
                .then_with(|| a.resource.cmp(&b.resource))
        };
        let truncated = all.len() > limit;
        if truncated {
            // Partition the kept head in linear time, then sort only it.
            all.select_nth_unstable_by(limit, order);
            all.truncate(limit);
        }
        all.sort_unstable_by(order);
        (all, truncated)
    }
}

/// Keeps the smaller of `slot` and `candidate`, so the choice does not
/// depend on the order concepts were visited in.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "per-element helper called in a hot loop (parser or analysis)"
    )
)]
fn keep_smaller(slot: &mut Option<String>, candidate: Option<&str>) {
    let Some(candidate) = candidate else { return };
    if slot.as_deref().is_none_or(|current| candidate < current) {
        *slot = Some(candidate.to_string());
    }
}
