//! Edit operations on a catalog: Rust ports of `okf.py`'s concept commands
//! (`stamp`, `review`, `question`, `unquestion`, `source`), its bookkeeping
//! (`log`, `index`, Task 3), the server's content edits (`create_concept`,
//! `edit_document`, `replace_body`, `set_frontmatter`) and `remove_source`,
//! and the restructuring edits (M6): `rewrite_links` (`relink`),
//! `move_concept`, `split_concept` and `delete_concept`.
//!
//! Every op is a pure function over a [`Workspace`]: it reads through it,
//! writes the files it changes with [`Workspace::write_atomic`], and returns
//! an [`OpOutcome`] naming them. The write path (`crate::write`, Task 8) runs
//! an op on a copy of a changeset's overlay and stores what it touched;
//! nothing here knows about changesets or the database.
//!
//! Byte parity with `okf.py` is the point: messages, rendered lines and
//! refusals are `okf.py`'s, checked by `tests/parity.rs` (`ops.json`).
//! `okf.py`'s `die(msg)` becomes [`OkfError::Refused`] with `msg` as its
//! reason, except where an M1 variant already names the problem
//! ([`OkfError::NotFound`], [`OkfError::InvalidPath`],
//! [`OkfError::FrontmatterParse`], [`OkfError::Shape`]).

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::clock::format_timestamp;
use crate::error::OkfError;
use crate::okf::doc::Doc;
use crate::okf::paths::{basename, resolve_concept};
use crate::okf::workspace::Workspace;
use crate::okf::yaml::Value;

pub mod content;
pub mod delete;
pub mod frontmatter;
pub mod log_index;
pub mod moves;
pub mod relink;
pub mod replay;
pub mod split;

// `pub use` re-exports names from the submodules, so callers write
// `okf::ops::stamp` (like a package `__init__.py` importing from its modules).
pub use content::{create_concept, edit_document, replace_body, set_frontmatter, NewConcept};
pub use delete::delete_concept;
pub use frontmatter::{question, remove_source, review, source, stamp, unquestion, OUTCOMES};
pub use log_index::{
    ensure_folder_index, ensure_index, index, log, repair_catalog, repair_indexes, starter_files,
    STARTER_FILE_COUNT,
};
pub use moves::move_concept;
pub use relink::{rewrite_links, Moved, Redirect};
// `replay` names both the module and its main function: Rust keeps modules
// and functions in separate namespaces, so `ops::replay(...)` and
// `ops::replay::ReplayOp` both work.
pub use replay::{insert_index_bullet_text, log_text, replay, ReplayOp};
pub use split::{split_concept, Split, DEFAULT_SPLIT_TYPE};

/// What an op needs from its caller besides the catalog.
///
/// The `'a` lifetime says the struct borrows `by` and cannot outlive the
/// string it points at (Rust checks this at compile time; Python has no
/// equivalent because everything is reference-counted).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpContext<'a> {
    /// The server clock's "now"; every timestamp the op writes is this instant.
    pub now: DateTime<Utc>,
    /// The actor recorded in `by:` fields (the key's `actor`, or the call's `by`).
    pub by: &'a str,
}

/// What an op did. A plain record, so its fields are public.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpOutcome {
    /// Catalog-relative paths written, in order, without duplicates. Empty
    /// when the op changed nothing.
    pub touched: Vec<String>,
    /// Per touched path, the operations that redo the edit on newer text
    /// (Task 3). A touched path missing here is a free edit.
    pub replay: BTreeMap<String, Vec<ReplayOp>>,
    /// The line `okf.py` prints (several lines joined with `\n` for `review`).
    pub message: String,
    /// What `okf.py` prints to stderr, without its `okf: ` prefix.
    pub notes: Vec<String>,
    /// Touched paths whose entry starts again on the revision published
    /// now, as if the changeset had not edited them before: the earlier
    /// base revision and replay are dropped. `revert` sets it, because its
    /// text is a whole-file choice made against the published history
    /// (issue #788). Every other op leaves it empty, so an entry keeps its
    /// first edit's base.
    pub rebased: BTreeSet<String>,
    /// Per path, the latest revision the caller says it saw (the live one,
    /// or a `delete` revision for a deleted path). Checked against the
    /// path's latest revision now, in the snapshot the write is built on
    /// (or, with no live document, the newest stored revision read in the
    /// write's transaction), before anything is stored: a different one is
    /// an [`OkfError::Conflict`] and writes nothing. `revert` sets it from
    /// its `expected_revision` (issues #795, #1127); every other op leaves
    /// it empty.
    pub expected: BTreeMap<String, i32>,
}

impl OpOutcome {
    /// An outcome that changed nothing, carrying `okf.py`'s message.
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) fn unchanged(message: String) -> Self {
        // `..Self::default()` fills the other fields with their defaults.
        Self {
            message,
            ..Self::default()
        }
    }

    /// Records that `path` was written (once; later calls are no-ops).
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) fn touch(&mut self, path: &str) {
        if !self.touched.iter().any(|p| p == path) {
            self.touched.push(path.to_string());
        }
    }

    /// Records that `path` was written by a replayable edit.
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) fn record(&mut self, path: &str, op: ReplayOp) {
        self.touch(path);
        // `entry(..).or_default()` is `dict.setdefault(key, [])`.
        self.replay.entry(path.to_string()).or_default().push(op);
    }

    /// Records the files `rewrite_links` changed: an `index.md` or `log.md`
    /// with a [`ReplayOp::Relink`], so publish can redo the rewrite on newer
    /// text (plan Decision 7); any other file as a free edit.
    ///
    /// # Arguments
    /// * `changed` - the paths [`relink::rewrite_links`] returned.
    /// * `redirect` - the redirect it applied.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn record_rewrites(&mut self, changed: &[String], redirect: &Redirect) {
        for path in changed {
            if is_bookkeeping(path) {
                // `clone`: each recorded op owns its own copy of the redirect.
                let op = ReplayOp::Relink {
                    redirect: redirect.clone(),
                };
                self.record(path, op);
            } else {
                self.touch(path);
            }
        }
    }
}

/// Which stamp `stamp` writes: `"generated"` or `"verified"`.
// Contributor note: `Copy` makes it a plain value (passing it copies it, like a
// Python int). `Deserialize` reads the lowercase spellings, and `JsonSchema`
// describes them in the `stamp` tool's input schema (the variants' doc comments
// become the choices' descriptions).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(inline, extend("type" = "string"))]
pub enum StampField {
    /// Replace `generated`: the content was (re)written.
    Generated,
    /// Append to `verified`: the facts were checked against the real system.
    Verified,
}

impl StampField {
    /// The key's name.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn as_str(self) -> &'static str {
        match self {
            StampField::Generated => "generated",
            StampField::Verified => "verified",
        }
    }
}

/// One `sources` entry for [`source`]. `title` and `author` are left out
/// when `None` or empty (`okf.py`'s `if a.title:`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceEntry<'a> {
    /// The entry's id, unique within the concept.
    pub id: &'a str,
    /// What was consulted: a URI, a path or a description.
    pub resource: &'a str,
    /// Optional title.
    pub title: Option<&'a str>,
    /// Optional author, e.g. `human:alice`.
    pub author: Option<&'a str>,
}

/// The kind of a `log.md` entry: `Update` (the default), `Creation`, `Change` or `Import`.
// Contributor note: there is no `rename_all`; the JSON spellings are the
// capitalized ones (`"Update"` etc.). `#[default]` marks the variant
// `Default::default()` returns, the `kind` default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, JsonSchema)]
pub enum LogKind {
    /// `Update` (the default).
    #[default]
    Update,
    /// `Creation`.
    Creation,
    /// `Change`.
    Change,
    /// `Import`.
    Import,
}

impl LogKind {
    /// The kind as written in the bullet, `Update` etc.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn as_str(self) -> &'static str {
        match self {
            LogKind::Update => "Update",
            LogKind::Creation => "Creation",
            LogKind::Change => "Change",
            LogKind::Import => "Import",
        }
    }
}

/// A concept loaded for editing (`okf.py`'s `load_concept` result).
pub(crate) struct LoadedConcept {
    /// Catalog-relative path.
    pub(crate) path: String,
    /// The document, ready for textual edits.
    pub(crate) doc: Doc,
    /// The frontmatter as parsed before any edit.
    pub(crate) data: IndexMap<String, Value>,
}

/// `okf.py`'s `load_concept`: resolves `reference`, reads the file byte for
/// byte and parses its frontmatter.
///
/// # Errors
/// [`OkfError::NotFound`] / [`OkfError::InvalidPath`] from
/// [`resolve_concept`]; [`OkfError::Refused`] without frontmatter;
/// [`OkfError::FrontmatterParse`] when it does not parse.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) fn load_concept(
    workspace: &dyn Workspace,
    reference: &str,
) -> Result<LoadedConcept, OkfError> {
    let path = resolve_concept(workspace, reference)?;
    let doc = Doc::from_text(&path, &workspace.read_to_string(&path)?);
    if !doc.has_frontmatter() {
        return Err(refused(format!(
            "{path} has no frontmatter; add it with your editor first"
        )));
    }
    let data = doc.parse_checked()?;
    Ok(LoadedConcept { path, doc, data })
}

/// `okf.py`'s `Doc.save`: refuses frontmatter an edit broke, then writes.
///
/// # Errors
/// [`OkfError::FrontmatterParse`] (nothing is written), or the workspace's
/// write error.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) fn save(
    workspace: &dyn Workspace,
    doc: &Doc,
    outcome: &mut OpOutcome,
) -> Result<(), OkfError> {
    let text = doc.render_checked()?;
    workspace.write_atomic(doc.path(), &text)?;
    outcome.touch(doc.path());
    Ok(())
}

/// `{ by, at }` for a stamp: the actor and the frozen "now".
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) fn stamp_value(ctx: &OpContext<'_>) -> Value {
    stamp_entry(ctx.by, &format_timestamp(ctx.now))
}

/// `{ by, at }` from an actor and an already formatted timestamp: what a
/// stamp replay writes (the values the original edit recorded).
#[tracing::instrument(level = "trace", skip_all)]
pub(crate) fn stamp_entry(by: &str, at: &str) -> Value {
    Value::Map(IndexMap::from([
        ("by".to_string(), Value::Str(by.to_string())),
        ("at".to_string(), Value::Str(at.to_string())),
    ]))
}

/// `okf.py`'s `die(reason)` as an error.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial constructor: builds a value from its arguments"
    )
)]
pub(crate) fn refused(reason: String) -> OkfError {
    OkfError::Refused { reason }
}

/// The refusal for a concept `type` that is blank (Python's `str.strip()`)
/// or not a string: `set_frontmatter`'s and `split_concept`'s rule, since
/// every concept needs one.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "one-line delegation to an instrumented function"
    )
)]
pub(crate) fn blank_type() -> OkfError {
    refused("type must be a non-blank string; every concept needs one".to_string())
}

/// Does publish replay edits to this file (`index.md`, `log.md`)?
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "per-element predicate or scanner called in a hot loop (parser or analysis)"
    )
)]
fn is_bookkeeping(path: &str) -> bool {
    matches!(basename(path), "index.md" | "log.md")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_field_has_a_direct_string_type_and_accepts_what_it_did() {
        let schema = serde_json::to_value(schemars::schema_for!(StampField)).unwrap();
        assert_eq!(schema["type"], "string");
        let choices: Vec<_> = schema["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .map(|choice| choice["const"].as_str().unwrap())
            .collect();
        assert_eq!(choices, ["generated", "verified"]);

        let parse = |text: &str| serde_json::from_value::<StampField>(text.into());
        assert_eq!(parse("generated").unwrap(), StampField::Generated);
        assert_eq!(parse("verified").unwrap(), StampField::Verified);
        for rejected in ["Generated", "VERIFIED", "reviewed", ""] {
            assert!(parse(rejected).is_err(), "{rejected:?} must stay rejected");
        }
        assert!(serde_json::from_value::<StampField>(serde_json::json!(1)).is_err());
    }

    #[test]
    fn rewrites_of_bookkeeping_files_replay_and_the_others_are_free() {
        let redirect = Redirect {
            from: "infra/x.md".to_string(),
            to: "infra/y.md".to_string(),
            anchors: None,
        };
        let mut outcome = OpOutcome::default();
        outcome.touch("infra/y.md");
        let changed = [
            "hub.md".to_string(),
            "infra/index.md".to_string(),
            "infra/y.md".to_string(),
            "log.md".to_string(),
        ];

        outcome.record_rewrites(&changed, &redirect);

        assert_eq!(
            outcome.touched,
            ["infra/y.md", "hub.md", "infra/index.md", "log.md"]
        );
        let relink = ReplayOp::Relink { redirect };
        assert_eq!(
            outcome.replay,
            BTreeMap::from([
                ("infra/index.md".to_string(), vec![relink.clone()]),
                ("log.md".to_string(), vec![relink]),
            ])
        );
    }
}
