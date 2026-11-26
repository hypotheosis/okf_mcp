//! Replay operations: how a bookkeeping edit of `log.md` or an `index.md`
//! is redone on newer text at publish (design "Data flow", "Write" step 3
//! and "Publish" step 3), and the pure text edits that both the ops
//! (`log_index.rs`) and [`replay`] run, so an edit and its replay cannot
//! drift apart. M6 adds [`ReplayOp::Unlist`] (a listing taken out of a
//! folder index) and [`ReplayOp::Relink`] (links redirected by a move or
//! split), whose edits are `unlist_text` and `relink::rewrite_text` (both
//! crate-internal: a public doc may not link them, or `cargo doc -D
//! warnings` fails). `Unlist` works on universal-newline text, so a CRLF
//! `index.md` comes out with `\n` endings, while `Relink` keeps bytes as
//! they are. Both match `okf.py`: `cmd_mv` reads and writes the old index
//! in text mode (`okf.py:1538`), and `rewrite_links` opens files with
//! `newline=''` (`okf.py:1359`). This is not a parity bug.
//!
//! Issue #658 adds [`ReplayOp::Unlink`]: a deleted concept's links in a
//! `log.md` become their link text. Its edit is `relink::unlink_text`
//! (crate-internal too), which resolves links as `rewrite_text` does and
//! keeps bytes as `Relink` does. `okf.py` has no counterpart, as it has no
//! delete command.
//!
//! Issue #717 adds [`ReplayOp::RewriteRawListing`]: `repair_indexes`
//! percent-encodes the destination of a raw listing bullet older versions
//! wrote, or removes it beside a working one. Its edit,
//! `rewrite_raw_listing_text`, works on universal-newline text, like the
//! other index edits.
//!
//! Issue #668 adds [`ReplayOp::SetListingSummary`]: `index` on a folder
//! replaces the summary of the folder's listing in its parent's index, so
//! describing a folder needs no free edit of that (often shared) index.
//! Its edit, `set_listing_summary_text`, works on universal-newline text.
//!
//! Issue #830 adds the frontmatter bookkeeping ops of a concept,
//! [`ReplayOp::AddSource`], [`ReplayOp::RemoveSource`], [`ReplayOp::Stamp`],
//! [`ReplayOp::SetReviewed`], [`ReplayOp::AddQuestion`] and
//! [`ReplayOp::RemoveQuestion`]. Each is redone by the same edit the tool
//! ran (`frontmatter::replay_edit`), on the file's own line endings, and
//! refuses with the tool's own reason when it no longer applies.
//!
//! [`log_text`] is `okf.py`'s `cmd_log` body (`:752`) and
//! [`insert_index_bullet_text`] its `insert_index_bullet` (`:1410`), line
//! for line. `okf.py` reads and writes these files in text mode, so both
//! work on universal-newline text and produce `\n` endings.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::error::OkfError;
use crate::okf::analysis::links::INDEX_HEADING;
use crate::okf::markdown::{encode_link_path, find_links, resolve_link};
use crate::okf::ops::frontmatter::replay_edit;
use crate::okf::ops::relink::{rewrite_text, unlink_text, Redirect};
use crate::okf::ops::StampField;
use crate::okf::paths::{basename, is_concept};
use crate::okf::pystr::{py_repr_str, py_strip, py_unquote, universal_newlines};
use crate::okf::workspace::{MemWorkspace, Workspace};

/// One replayable bookkeeping edit, stored in a changeset entry's `replay`
/// column as JSON (`{"op": "log", "date": ..., "bullet": ...}`).
///
/// `#[serde(tag = "op")]` writes the variant name into an `op` field next
/// to the variant's own fields, like a Python dict with a "type" key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ReplayOp {
    /// `log`: add `bullet` under `## <date>` in `log.md`.
    Log {
        /// The UTC date of the heading, `2026-11-03`.
        date: String,
        /// The whole bullet line, `* **Update:** text`.
        bullet: String,
    },
    /// `index` and folder listings: add `bullet` to an `index.md` unless it
    /// already links `target`.
    ListBullet {
        /// Catalog-relative path the bullet lists (a concept, or a folder's
        /// `index.md`).
        target: String,
        /// The whole bullet line.
        bullet: String,
        /// The heading to put it under; `None` for `okf.py`'s default place.
        section: Option<String>,
        /// Create the heading when it is missing (folder listings under `# Sections`).
        create_section: bool,
    },
    /// `ensure_index` and `repair_indexes`: create a folder's `index.md`
    /// unless it exists.
    CreateIndex {
        /// Catalog-relative folder, `infra/hosts`; `""` is the root, whose
        /// index is `new_index_text`'s root template (an `okf_version`
        /// frontmatter, a heading and a TODO line).
        folder: String,
    },
    /// `repair_indexes`: create the root `log.md` unless it exists.
    CreateLog,
    /// `move_concept` and `delete_concept`: remove the first bullet of an
    /// `index.md` that links `target`.
    Unlist {
        /// Catalog-relative path the bullet lists.
        target: String,
        /// `Some`: the bullet as it read when the edit was made. A move
        /// carries that text to the new folder, so the replay refuses a
        /// listing that reads otherwise now, rather than drop someone
        /// else's change to it. `None`: whatever it reads (a delete).
        bullet: Option<String>,
    },
    /// `move_concept` and `split_concept`: redirect the links of an
    /// `index.md` or `log.md` that point into the moved document or section.
    Relink {
        /// Which links, and where to.
        redirect: Redirect,
    },
    /// `delete_concept`: turn every link of a `log.md` into the deleted
    /// concept `target` into its link text (issue #658). `log.md` is written
    /// by the `log` tool only, so nobody could remove such a link by hand,
    /// and a link left into a deleted path is a broken link.
    Unlink {
        /// Catalog-relative path of the deleted concept.
        target: String,
    },
    /// `repair_indexes`: fix a raw (unencoded) listing bullet of `target`
    /// that versions before issue #453 wrote (issue #717). The first line
    /// equal to `line` is removed when another bullet already links
    /// `target`, else replaced by `fixed` (the same line with its
    /// destination percent-encoded); a line no longer there is left alone.
    ///
    /// `repair_indexes` decided that `line` is a raw listing against the
    /// changeset's documents (`lists_raw_target`'s rules 0-3). Replay cannot
    /// tell which files exist, so it cannot re-check rule 3: it leaves the
    /// line as it is when the link the parser reads in it resolves to any
    /// path (`[A](a).md)` reads as a link to `a`, which a concurrent change
    /// may have created). The worst case is a broken raw bullet left for the
    /// next repair, never a working listing lost.
    RewriteRawListing {
        /// Catalog-relative path of the concept listed.
        target: String,
        /// The raw bullet line, as written.
        line: String,
        /// Its rewrite.
        fixed: String,
    },
    /// `index` on a folder or on an already-listed concept: replace the summary (the text after ` - `) of
    /// the first bullet whose leading link resolves to `target` (issues #668
    /// and #790). The label and link are kept as they read now, so someone
    /// else's edit of them survives; a bullet no longer there is refused,
    /// never silently dropped or applied to another line.
    SetListingSummary {
        /// Catalog-relative path the bullet lists (a folder's `index.md`, or a concept).
        target: String,
        /// The new summary, its whitespace already collapsed.
        summary: String,
    },
    /// `source`: append the entry to the concept's `sources`; refused when
    /// the text now has a source with this `id` (issue #830).
    AddSource {
        /// The entry's id.
        id: String,
        /// What was consulted.
        resource: String,
        /// Optional title.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        /// Optional author.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        author: Option<String>,
    },
    /// `remove_source`: remove the `sources` entry with this `id`; refused
    /// when it is gone, or a footnote now cites it (issue #830).
    RemoveSource {
        /// The entry's id.
        id: String,
    },
    /// `stamp`: set `generated` or append to `verified` with the actor and
    /// time the original edit recorded (issue #830), not the replay's.
    Stamp {
        /// Which stamp.
        field: StampField,
        /// The actor.
        by: String,
        /// The timestamp, `2026-11-03T12:00:00Z`.
        at: String,
    },
    /// `review`: replace `reviewed` with what the original edit recorded
    /// (issue #830). The questions it queued are separate
    /// [`ReplayOp::AddQuestion`]s.
    SetReviewed {
        /// The actor.
        by: String,
        /// The timestamp, `2026-11-03T12:00:00Z`.
        at: String,
        /// One of the review outcomes.
        outcome: String,
        /// What was and was not checked.
        note: String,
    },
    /// `question` and the questions of `review`: queue `text` unless an
    /// identical question is already queued (issue #830).
    AddQuestion {
        /// The question.
        text: String,
        /// `review`'s rule for a mapping-valued `open_questions`: treat it
        /// as empty and convert it to a list. `question` leaves this off
        /// and refuses such a value.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        convert_mapping: bool,
    },
    /// `unquestion`: remove the open question whose text is exactly `text`
    /// (never read as an index); refused when none is (issue #830). Not
    /// recorded for a question that is not a string.
    RemoveQuestion {
        /// The removed question's text.
        text: String,
    },
}

/// Redoes `ops` on a file's current text (design "Publish", step 3).
///
/// Idempotent where publish needs it: a log bullet already under its date,
/// a listing whose `target` the index already links, a `CreateIndex` or
/// `CreateLog` of a file that exists (someone else's text wins), an `Unlist` whose listing is gone, a `Relink` with
/// no link left into its source, an `Unlink` with no link left into its
/// target, a `RewriteRawListing` whose raw line is gone, and a
/// `SetListingSummary` whose bullet already reads so all leave the text
/// alone, as does an `AddQuestion` of a question already queued. Text that no operation
/// changes is returned byte for byte; text a bookkeeping edit changed has
/// `\n` endings, as `okf.py` writes it, while a `Relink` or an `Unlink`
/// keeps the file's own line endings.
///
/// # Arguments
/// * `path` - the file's catalog-relative path (`log.md`, `infra/index.md`),
///   used to resolve relative links and in errors.
/// * `current` - its current published text; `None` when it does not exist.
/// * `ops` - the operations recorded for it, in order.
///
/// # Returns
/// The text after the operations.
///
/// # Errors
/// [`OkfError::NotFound`] when a `Log`, `ListBullet`, `Unlist`, `Relink`,
/// `Unlink`, `RewriteRawListing` or `SetListingSummary` (or nothing) finds
/// no file. For `Unlist` this is deliberate, not "already
/// gone": publish never replays onto a file deleted since the entry's base
/// (`replayed_text` makes that a conflict), so a missing file here means the
/// ops themselves are wrong. [`OkfError::Refused`] when a listing's section
/// heading is gone, when a move's `Unlist` finds its listing changed since
/// the move, when a `SetListingSummary` finds no bullet listing its target
/// (another changeset removed or rewrote it), or when a `Relink` redirects
/// (or an `Unlink` names) a non-concept (`index.md`, `log.md`), which only a
/// corrupt stored op can hold. For the concept ops ([`ReplayOp::AddSource`]
/// and the other five of issue #830), [`OkfError::Refused`] with the tool's
/// own reason when the text now has the source id, lacks the source or
/// question, or a footnote cites the source; [`OkfError::Shape`] when
/// `sources` or `open_questions` is no longer a list;
/// [`OkfError::FrontmatterParse`] when the frontmatter no longer parses.
///
/// # Examples
/// ```
/// use okf_mcp::okf::ops::{replay, ReplayOp};
/// let ops = [ReplayOp::Log { date: "2026-11-03".into(), bullet: "* **Update:** x".into() }];
/// let once = replay("log.md", Some("# Log\n"), &ops).unwrap();
/// assert_eq!(once, "# Log\n\n## 2026-11-03\n\n* **Update:** x\n");
/// assert_eq!(replay("log.md", Some(&once), &ops).unwrap(), once);
/// ```
#[tracing::instrument(level = "debug", skip_all)]
pub fn replay(path: &str, current: Option<&str>, ops: &[ReplayOp]) -> Result<String, OkfError> {
    // Publish has no directory listing, so links resolve as if no path were
    // a directory; `lists_target` then also accepts `folder` for `folder/index.md`.
    let no_dirs = MemWorkspace::default();
    // `Cow` holds either the borrowed `current` or a newly built `String`,
    // so unchanged text is never copied.
    let mut text: Option<Cow<'_, str>> = current.map(Cow::Borrowed);
    for op in ops {
        let updated = match op {
            ReplayOp::CreateIndex { folder } => text.is_none().then(|| new_index_text(folder)),
            ReplayOp::CreateLog => text.is_none().then(|| LOG_TEMPLATE.to_string()),
            ReplayOp::Log { date, bullet } => {
                log_text(&universal_newlines(present(path, &text)?), date, bullet)
            }
            ReplayOp::ListBullet {
                target,
                bullet,
                section,
                create_section,
            } => {
                let base = universal_newlines(present(path, &text)?);
                if lists_target(&no_dirs, &base, path, target) {
                    None
                } else {
                    let section = section.as_deref();
                    let inserted =
                        insert_index_bullet_text(&base, bullet, section, *create_section);
                    Some(inserted.ok_or_else(|| missing_heading(path, section.unwrap_or("")))?)
                }
            }
            ReplayOp::Unlist { target, bullet } => {
                let base = universal_newlines(present(path, &text)?);
                match unlist_text(&no_dirs, &base, path, target) {
                    // Already gone: nothing to do (idempotent).
                    None => None,
                    // `is_some_and` is false for `None`: a delete removes any bullet.
                    Some((_, found)) if bullet.as_deref().is_some_and(|want| want != found) => {
                        return Err(changed_listing(path, target));
                    }
                    Some((rest, _)) => Some(rest),
                }
            }
            // Byte for byte, as the op itself rewrote the file, so a concurrent `log` entry and the delete's
            // unlinking both land in whichever order they publish.
            ReplayOp::Unlink { target } => {
                // `no_dirs` resolves concept links only (see `Relink`).
                if !is_concept(target) {
                    return Err(non_concept_unlink(path, target));
                }
                unlink_text(&no_dirs, path, present(path, &text)?, target)
            }
            ReplayOp::RewriteRawListing {
                target,
                line,
                fixed,
            } => {
                let base = universal_newlines(present(path, &text)?);
                if parses_to_a_path(line, path) {
                    None
                } else {
                    rewrite_raw_listing_text(&no_dirs, &base, path, target, line, fixed)
                        .map(|(updated, _)| updated)
                }
            }
            ReplayOp::SetListingSummary { target, summary } => {
                let base = universal_newlines(present(path, &text)?);
                // A raw (unencoded) bullet never matches here: its link does
                // not resolve to `target`, so the replay refuses rather than
                // guess (`index` refuses one at write time too).
                match set_listing_summary_text(&no_dirs, &base, path, target, summary) {
                    None => return Err(lost_listing(path, target)),
                    // Already reads so: unchanged, byte for byte.
                    Some(updated) if updated == base.as_ref() => None,
                    Some(updated) => Some(updated),
                }
            }
            // Edits of one concept's frontmatter, redone on its current text.
            ReplayOp::AddSource { .. }
            | ReplayOp::RemoveSource { .. }
            | ReplayOp::Stamp { .. }
            | ReplayOp::SetReviewed { .. }
            | ReplayOp::AddQuestion { .. }
            | ReplayOp::RemoveQuestion { .. } => replay_edit(path, present(path, &text)?, op)?,
            ReplayOp::Relink { redirect } => {
                // `no_dirs` cannot tell a directory link (`[x](../a)`) from a file
                // link, which only matters when `from` is an `index.md`. Moves and
                // splits only redirect concepts (see `rewrite_text`'s doc). The op
                // comes from the database, so a bad one is an error in every build
                // profile, never a panic or a silently wrong rewrite.
                if !is_concept(&redirect.from) {
                    return Err(non_concept_relink(path, &redirect.from));
                }
                rewrite_text(&no_dirs, path, path, present(path, &text)?, redirect)
            }
        };
        if let Some(updated) = updated {
            text = Some(Cow::Owned(updated));
        }
    }
    text.map(Cow::into_owned).ok_or_else(|| OkfError::NotFound {
        path: path.to_string(),
    })
}

/// The current text, or `NotFound` for an operation that needs a file.
#[tracing::instrument(level = "trace", skip_all)]
fn present<'t>(path: &str, text: &'t Option<Cow<'_, str>>) -> Result<&'t str, OkfError> {
    text.as_deref().ok_or_else(|| OkfError::NotFound {
        path: path.to_string(),
    })
}

/// `okf.py`'s "no heading 'X' in <path>" refusal of a listing.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) fn missing_heading(path: &str, section: &str) -> OkfError {
    OkfError::Refused {
        reason: format!("no heading {} in {path}", py_repr_str(section)),
    }
}

/// Does `text` (an `index.md` at `index`) link `target`? `cmd_index`'s
/// "already lists" check. A link resolving to `folder` also counts for
/// `folder/index.md`, which only matters when `workspace` cannot tell that
/// `folder` is a directory (replay): a concept target never ends in
/// `/index.md`, so `index`'s own check is exactly `okf.py`'s.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) fn lists_target(
    workspace: &dyn Workspace,
    text: &str,
    index: &str,
    target: &str,
) -> bool {
    find_links(text)
        .iter()
        .filter_map(|link| resolve_link(workspace, &link.target, index))
        .any(|resolved| {
            resolved.path == target
                || target.strip_suffix("/index.md") == Some(resolved.path.as_str())
        })
}

/// Does `text` hold a bullet that lists `target` in the raw form older
/// versions wrote (issue #453): the path as written, unencoded, such as
/// `[A](a b.md)`, `[A](a).md)` or `[A](a:b.md)`? No parser reads such a
/// destination back as `target` ([`lists_target`] cannot see it), so
/// without this check `index` appended a second, working bullet beside it.
///
/// Only a name that [`encode_link_path`] would change can have a raw form
/// that differs from the written one; for any other name this is `false`
/// (the ordinary check already sees its link).
///
/// The rule, conservative on purpose (a rare duplicate bullet is far better
/// than a missing listing): a bullet is a raw listing of `target` only when
/// 0. it has the shape older versions wrote, `* [label](destination...`,
///    with the link first in the bullet (see `legacy_destination_start`);
/// 1. its destination, as written between `](` and the `)` or `#` that
///    follows it, equals the target's raw name (not merely a prefix the
///    parser reads: `[A](a.md).md)` is a working link to `a.md`, so it is
///    not a raw listing of `a.md).md`);
/// 2. the name holds no valid `%XX` escape (`py_unquote` leaves it alone):
///    `a%20b.md` as a raw name is indistinguishable from the encoded name
///    of `a b.md`, which is that file's listing; and
/// 3. the link [`find_links`] parses at that spot (if any) is broken: it
///    resolves to no document that exists in `workspace`. A link that
///    resolves to any existing document, another concept included, is that
///    document's listing (`a#b.md` reads as a link to `a` plus a fragment).
///
/// Publish replay does not call this: it has no view of which files exist,
/// so it cannot apply rule 3, and a wrong "already listed" there would
/// silently drop the staged listing. Replay counts only a bullet whose
/// parsed link resolves to the target ([`lists_target`]); after a concurrent
/// change the worst case is an old broken raw bullet beside a new working
/// one, which lint shows and the next `repair_indexes` removes (issue
/// #717).
///
/// # Arguments
/// * `workspace` - tells which documents exist (rule 3).
/// * `text` - an `index.md`, with universal newlines.
/// * `index` - its catalog-relative path; the raw destination is the
///   target relative to its folder, or `/`-absolute (for a folder's
///   `index.md`, also the folder link `name/` or `/folder/`; see
///   `raw_names`).
/// * `target` - the catalog-relative path listed: a concept, or a
///   folder's `index.md` (`repair_indexes`' `# Sections` pass, issue #731).
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "one-line delegation to an instrumented function"
    )
)]
pub(crate) fn lists_raw_target(
    workspace: &dyn Workspace,
    text: &str,
    index: &str,
    target: &str,
) -> bool {
    first_raw_listing(workspace, text, index, target).is_some()
}

/// A bullet [`first_raw_listing`] found: where, in `text`'s `\n`-split
/// lines, a raw listing of the target sits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawListing {
    /// The whole bullet line, as written.
    pub(crate) line: String,
    /// The same line with only its destination percent-encoded
    /// ([`encode_link_path`] of the raw destination, relative or
    /// `/`-absolute as written): the label and everything after the
    /// destination are kept.
    pub(crate) fixed: String,
}

/// The destinations, as written, that an unencoded listing of `target` in
/// `index` can have (rule 1 of [`lists_raw_target`]): `target` relative to
/// the index's folder (when it lies inside it) and `/`-absolute. A folder's
/// `index.md` also has the folder link older versions wrote under
/// `# Sections` (issue #731): `name/` and `/folder/`, beside the
/// `name/index.md` and `/folder/index.md` that name the file itself. A
/// concept never ends in `/index.md`, so its names are unchanged.
///
/// # Arguments
/// * `index` - the `index.md` holding the listing.
/// * `target` - the catalog-relative path listed.
///
/// # Returns
/// The candidate raw names, before rule 2 and the "would encoding change
/// it" filter.
#[tracing::instrument(level = "trace", skip_all)]
fn raw_names(index: &str, target: &str) -> Vec<String> {
    let folder = crate::okf::paths::dirname(index);
    // `target` relative to `folder`, or `None` when it lies outside it.
    let relative = |path: &str| -> Option<String> {
        if folder.is_empty() {
            Some(path.to_string())
        } else {
            path.strip_prefix(folder)
                .and_then(|rest| rest.strip_prefix('/'))
                .map(str::to_string)
        }
    };
    let mut raws: Vec<String> = relative(target).into_iter().collect();
    raws.push(format!("/{target}"));
    if let Some(dir) = target.strip_suffix("/index.md") {
        let dir_link = format!("{dir}/");
        raws.extend(relative(&dir_link));
        raws.push(format!("/{dir_link}"));
    }
    raws
}

/// [`raw_names`] that really are raw: encoding would change them, and they
/// hold no `%XX` escape (rule 1 of [`lists_raw_target`]).
#[tracing::instrument(level = "trace", skip_all)]
fn unencoded_raw_names(index: &str, target: &str) -> Vec<String> {
    raw_names(index, target)
        .into_iter()
        .filter(|raw| encode_link_path(raw) != raw.as_str())
        .filter(|raw| py_unquote(raw) == raw.as_str())
        .collect()
}

/// Does the bullet's destination, which starts at `start` in `line` and
/// matched `raw` (a raw name of `target`), match a strictly longer raw name
/// of another existing document, ending at `)` or `#` as written (issue
/// #734)? A `#` or `)` ends the shorter match inside the longer name
/// (`a b.md` inside `a b.md#extra.md`), so the bullet could be either
/// document's: the longest name wins and the shorter target leaves the
/// bullet alone. This is the conservative choice for the shorter target
/// (it never rewrites or counts the bullet). For two concepts of one folder
/// the longer one's own pass still rewrites it, in whichever order the
/// targets are processed; for a folder index against a concept of that
/// folder no pass reads the parent's bullet, so it stays as written. The
/// check runs only on a match, so the catalog is listed rarely. A failed
/// listing counts as ambiguous: leaving a bullet is always safe.
///
/// # Arguments
/// * `workspace` - lists the existing documents.
/// * `line` - the bullet.
/// * `start` - the byte offset of its destination in `line`.
/// * `index` - the index holding it.
/// * `target` - the target being matched.
/// * `raw` - the raw name of `target` that matched.
///
/// # Returns
/// `true` when another document's longer raw name matches the bullet.
#[tracing::instrument(level = "trace", skip_all)]
fn belongs_to_longer_raw(
    workspace: &dyn Workspace,
    line: &str,
    start: usize,
    index: &str,
    target: &str,
    raw: &str,
) -> bool {
    let Ok(documents) = workspace.list_markdown() else {
        return true;
    };
    let destination = &line[start..];
    documents
        .iter()
        .filter(|other| other.as_str() != target)
        // Every name another document could be written as, with no rule-2
        // (`%XX`) filter: a bullet that holds an escape is not raw for its
        // own document, but it still shows the destination is not ours.
        .flat_map(|other| raw_names(index, other))
        .filter(|longer| longer.len() > raw.len())
        .any(|longer| {
            destination
                .strip_prefix(longer.as_str())
                .is_some_and(|rest| rest.starts_with(')') || rest.starts_with('#'))
        })
}

/// The first bullet of `text` that [`lists_raw_target`] counts as a raw
/// listing of `target`, by exactly its rules 0-3, and the line it would be
/// rewritten to.
///
/// # Arguments
/// As [`lists_raw_target`].
///
/// # Returns
/// The bullet and its rewrite, or `None` when no bullet is a raw listing.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) fn first_raw_listing(
    workspace: &dyn Workspace,
    text: &str,
    index: &str,
    target: &str,
) -> Option<RawListing> {
    let raws = unencoded_raw_names(index, target);
    // Lines in order, so the first raw bullet is the one found (a replay
    // of the rewrite takes the first line equal to it, the same line).
    text.split('\n')
        .filter(|line| is_bullet(line))
        .find_map(|line| {
            let start = legacy_destination_start(line)?;
            raws.iter().find_map(|raw| {
                let rest = line[start..].strip_prefix(raw.as_str())?;
                // Rule 1: the destination ends here, as written.
                let ends = rest.starts_with(')') || rest.starts_with('#');
                // Rule 3: the parsed link at this spot, if any, is broken.
                let broken = find_links(line)
                    .iter()
                    .find(|link| link.target_range.start == start)
                    .is_none_or(|link| {
                        resolve_link(workspace, &link.target, index)
                            .is_none_or(|r| !workspace.exists(&r.path))
                    });
                // Issue #734: another existing document's raw name may extend
                // this one (`a b.md` and `a b.md#extra.md`), and then the
                // bullet is that document's listing, not this target's.
                (ends
                    && broken
                    && !belongs_to_longer_raw(workspace, line, start, index, target, raw))
                .then(|| RawListing {
                    line: line.to_string(),
                    fixed: format!("{}{}{rest}", &line[..start], encode_link_path(raw)),
                })
            })
        })
}

/// Does the link [`find_links`] parses at a legacy bullet's destination
/// resolve to any path at all? Publish replay cannot tell which files
/// exist, so it treats every such path as one that might (see
/// [`ReplayOp::RewriteRawListing`]). A space ends the parse before the
/// link closes and a `:` reads as a URI scheme, so `[A](a b.md)` and
/// `[A](a:b.md)` resolve to nothing; `[A](a).md)` and `[A](a#b.md)` resolve
/// to `a`.
#[tracing::instrument(level = "trace", skip_all)]
fn parses_to_a_path(line: &str, index: &str) -> bool {
    let Some(start) = legacy_destination_start(line) else {
        // Not the shape a raw listing has: never rewrite it.
        return true;
    };
    find_links(line)
        .iter()
        .find(|link| link.target_range.start == start)
        .is_some_and(|link| resolve_link(&MemWorkspace::default(), &link.target, index).is_some())
}

/// The edit of [`ReplayOp::RewriteRawListing`], shared by `repair_indexes`
/// and its replay: the first line of `text` equal to `line` is removed when
/// another bullet of the index already links `target` (the raw bullet is a
/// duplicate), else replaced by `fixed`.
///
/// # Arguments
/// * `workspace` - resolves the other bullets' links (replay passes an
///   empty one).
/// * `text` - the index, with universal newlines.
/// * `index` - its catalog-relative path.
/// * `target` - the catalog-relative path listed.
/// * `line` - the raw bullet, as written.
/// * `fixed` - its rewrite.
///
/// # Returns
/// The new text, and whether the line was removed (`true`) or rewritten;
/// `None` when no line equals `line`.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) fn rewrite_raw_listing_text(
    workspace: &dyn Workspace,
    text: &str,
    index: &str,
    target: &str,
    line: &str,
    fixed: &str,
) -> Option<(String, bool)> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    let at = lines.iter().position(|l| *l == line)?;
    lines.remove(at);
    let rest = lines.join("\n");
    if listing_line(workspace, &rest, index, target).is_some() {
        return Some((rest, true));
    }
    lines.insert(at, fixed);
    Some((lines.join("\n"), false))
}

/// Where the destination starts in a bullet written the way older versions
/// wrote one: `* [label](destination...` with the link as the first thing in
/// the bullet. The label follows the link grammar (it ends at the first
/// unescaped `]`) and must hold no unescaped `[`, which older versions
/// escaped; anything else (no label, an image `![..]`, a link inside a code
/// span or after prose, unbalanced brackets) is not a legacy listing. A false
/// "no" only leaves a duplicate bullet; a false "yes" drops a listing.
///
/// # Returns
/// The byte offset in `line` just after `](`, or `None`.
#[tracing::instrument(level = "trace", skip_all)]
fn legacy_destination_start(line: &str) -> Option<usize> {
    let rest = line
        .strip_prefix("* ")
        .or_else(|| line.strip_prefix("- "))?;
    let after_marker = line.len() - rest.len();
    let label = rest.strip_prefix('[')?;
    let mut chars = label.char_indices();
    while let Some((i, ch)) = chars.next() {
        match ch {
            // A backslash escapes the next character (the LINK grammar).
            '\\' => {
                chars.next();
            }
            '[' => return None,
            ']' => {
                let tail = &label[i + 1..];
                let _ = tail.strip_prefix('(')?;
                return Some(after_marker + 1 + i + 2);
            }
            _ => {}
        }
    }
    None
}

/// [`lists_target`], or [`lists_raw_target`]: does `text` list `target` in
/// any form, working or an older raw one? What `index` and `repair_indexes`
/// use to decide that a concept is already listed (replay uses only
/// [`lists_target`]).
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "one-line delegation to instrumented functions"
    )
)]
pub(crate) fn already_lists(
    workspace: &dyn Workspace,
    text: &str,
    index: &str,
    target: &str,
) -> bool {
    lists_target(workspace, text, index, target) || lists_raw_target(workspace, text, index, target)
}

/// The line of `text` (an `index.md` at `index`) that lists `target`: the
/// first bullet (`* ` or `- `) with a link resolving to it, as `cmd_mv`
/// takes it.
///
/// # Arguments
/// * `workspace` - resolves directory links (replay passes an empty one).
/// * `text` - the index, with universal newlines.
/// * `index` - its catalog-relative path, to resolve relative links.
/// * `target` - the catalog-relative path listed.
///
/// # Returns
/// The 0-based line index, or `None` when no bullet links `target`.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) fn listing_line(
    workspace: &dyn Workspace,
    text: &str,
    index: &str,
    target: &str,
) -> Option<usize> {
    text.split('\n').position(|line| {
        is_bullet(line)
            && find_links(line).iter().any(|link| {
                resolve_link(workspace, &link.target, index)
                    .is_some_and(|resolved| resolved.path == target)
            })
    })
}

/// `cmd_mv`'s edit of the old folder's index: the text without the line
/// [`listing_line`] finds.
///
/// # Arguments
/// As [`listing_line`].
///
/// # Returns
/// The text without that line and the line itself, or `None` when no
/// bullet links `target`.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) fn unlist_text(
    workspace: &dyn Workspace,
    text: &str,
    index: &str,
    target: &str,
) -> Option<(String, String)> {
    let at = listing_line(workspace, text, index, target)?;
    let mut lines: Vec<&str> = text.split('\n').collect();
    // `Vec::remove(at)` takes the line out, like Python's `lines.pop(at)`.
    let bullet = lines.remove(at).to_string();
    Some((lines.join("\n"), bullet))
}

/// Does the link at the start of `line` (its first link, opening right
/// after the bullet marker) resolve to `target`, in the sense of
/// [`lists_target`] (a link to `folder` counts for `folder/index.md`)?
///
/// # Returns
/// That link, or `None`.
#[tracing::instrument(level = "trace", skip_all)]
fn leading_link_to(
    workspace: &dyn Workspace,
    line: &str,
    index: &str,
    target: &str,
) -> Option<crate::okf::markdown::LinkMatch> {
    if !is_bullet(line) {
        return None;
    }
    let link = find_links(line).into_iter().next()?;
    // `* [`: the link must open the bullet, so a link in another bullet's
    // summary (`* [A](a.md) - see [infra](infra/)`) is never taken for the
    // listing of `infra/`.
    if link.range.start != 2 {
        return None;
    }
    let resolved = resolve_link(workspace, &link.target, index)?;
    (resolved.path == target || target.strip_suffix("/index.md") == Some(resolved.path.as_str()))
        .then_some(link)
}

/// The edit of [`ReplayOp::SetListingSummary`], shared by `index` and its
/// replay: the first bullet of `text` whose leading link resolves to
/// `target` gets `summary` as the text after its first ` - ` past the link
/// (or ` - <summary>` after the link when it has none); the label, the link
/// and anything between the link and the ` - ` are kept.
///
/// # Arguments
/// * `workspace` - resolves directory links (replay passes an empty one).
/// * `text` - the index, with universal newlines.
/// * `index` - its catalog-relative path.
/// * `target` - the catalog-relative path listed.
/// * `summary` - the new summary, whitespace already collapsed.
///
/// # Returns
/// The new text (equal to `text` when the bullet already reads so), or
/// `None` when no bullet lists `target` that way.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) fn set_listing_summary_text(
    workspace: &dyn Workspace,
    text: &str,
    index: &str,
    target: &str,
    summary: &str,
) -> Option<String> {
    let mut lines: Vec<Cow<'_, str>> = text.split('\n').map(Cow::Borrowed).collect();
    let (at, end) = lines.iter().enumerate().find_map(|(i, line)| {
        leading_link_to(workspace, line, index, target).map(|link| (i, link.range.end))
    })?;
    let line = &lines[at];
    let keep = match line[end..].find(" - ") {
        Some(dash) => &line[..end + dash],
        None => &line[..end],
    };
    let updated = format!("{keep} - {summary}");
    lines[at] = Cow::Owned(updated);
    Some(lines.join("\n"))
}

/// The refusal of a `SetListingSummary` whose bullet is gone at publish.
#[tracing::instrument(level = "trace", skip_all)]
fn lost_listing(index: &str, target: &str) -> OkfError {
    // A folder's `index.md` is shown as the folder; a concept as itself.
    let listed = if target == "index.md" {
        ""
    } else {
        target
            .strip_suffix("/index.md")
            .map_or(target, |folder| &target[..=folder.len()])
    };
    OkfError::Refused {
        reason: format!(
            "{index} no longer lists /{listed} (another changeset removed or rewrote its \
             listing since this one set its summary); discard {index} from this changeset \
             and set the summary again"
        ),
    }
}

/// The refusal of a stored `Relink` whose source is not a concept: the
/// replay's empty workspace cannot resolve directory links into it.
#[tracing::instrument(level = "trace", skip_all)]
fn non_concept_relink(path: &str, from: &str) -> OkfError {
    OkfError::Refused {
        reason: format!(
            "{path}: a stored relink redirects /{from}, which is not a concept; \
             only moves and splits of concepts record one, so discard this path \
             and redo the operation"
        ),
    }
}

/// The refusal of a stored `Unlink` whose target is not a concept: the
/// replay's empty workspace cannot resolve directory links into it.
#[tracing::instrument(level = "trace", skip_all)]
fn non_concept_unlink(path: &str, target: &str) -> OkfError {
    OkfError::Refused {
        reason: format!(
            "{path}: a stored unlink names /{target}, which is not a concept; \
             only delete_concept records one, so discard this path and redo \
             the operation"
        ),
    }
}

/// The refusal of a move's `Unlist` whose listing changed since the move.
#[tracing::instrument(level = "trace", skip_all)]
fn changed_listing(index: &str, target: &str) -> OkfError {
    OkfError::Refused {
        reason: format!(
            "{index}: the listing of /{target} changed since this changeset moved it; \
             discard every path of the move together (pending_changes lists them: \
             both concept paths, both folder indexes and the files whose links it \
             rewrote), or the whole changeset, then move the concept again"
        ),
    }
}

/// Python's `str.capitalize`: the first character upper-cased, the rest
/// lower-cased.
#[tracing::instrument(level = "trace", skip_all)]
fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        // `to_uppercase` yields one or more chars (`ß` becomes `SS`), so
        // the pieces are chained and collected rather than pushed one by one.
        Some(first) => first
            .to_uppercase()
            .chain(chars.flat_map(char::to_lowercase))
            .collect(),
        None => String::new(),
    }
}

/// A folder's heading: `ensure_index`'s `name.replace("-", " ").capitalize()`.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) fn folder_heading(folder: &str) -> String {
    capitalize(&basename(folder).replace('-', " "))
}

/// The first lines of a new `log.md`: its heading, which `log` adds dated
/// entries under (`okf-catalog-template`'s `install/skeleton/log.md`).
pub(crate) const LOG_TEMPLATE: &str = "# Knowledge Catalog Update Log\n";

/// The heading of a root `index.md` with no display name to use: one that
/// `repair_indexes` (or its replay) creates, and a new catalog's with no
/// display name.
pub(crate) const ROOT_HEADING: &str = "Catalog";

/// The placeholder line of a root `index.md` with nothing to say yet.
pub(crate) const ROOT_TODO: &str = "TODO: say what this catalog holds.";

/// A root `index.md`: the `okf_version` lint requires there, a heading and
/// one paragraph (`okf-catalog-template`'s `install/skeleton/index.md`,
/// without its sections, which a catalog grows as it gains folders).
///
/// # Arguments
/// * `heading` - the `# ` heading, the catalog's name when it is known.
/// * `about` - the paragraph under it; its whitespace collapses to single
///   spaces, and a blank one becomes [`ROOT_TODO`].
///
/// # Returns
/// The file's text, ending in a newline.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) fn root_index_text(heading: &str, about: &str) -> String {
    let about = crate::okf::ops::log_index::collapse_whitespace(about);
    let about = if about.is_empty() { ROOT_TODO } else { &about };
    format!("---\nokf_version: \"0.2\"\n---\n\n# {heading}\n\n{about}\n")
}

/// The `index.md` `ensure_index` writes for a new folder; for the root
/// (`""`), the [`root_index_text`] `repair_indexes` writes.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) fn new_index_text(folder: &str) -> String {
    if folder.is_empty() {
        return root_index_text(ROOT_HEADING, "");
    }
    format!(
        "# {}\n\nTODO: say what this folder holds.\n",
        folder_heading(folder)
    )
}

/// `cmd_log`'s edit: adds `bullet` under `## <date>`, newest heading first
/// and newest bullet first under it.
///
/// # Arguments
/// * `text` - `log.md`, with universal newlines.
/// * `date` - the UTC date, `2026-11-03`.
/// * `bullet` - the whole bullet line.
///
/// # Returns
/// The new text, or `None` when `bullet` is already under that heading
/// (`okf.py`'s "already logged").
///
/// # Examples
/// ```
/// use okf_mcp::okf::ops::log_text;
/// let text = log_text("# Log\n", "2026-11-03", "* **Update:** x").unwrap();
/// assert_eq!(text, "# Log\n\n## 2026-11-03\n\n* **Update:** x\n");
/// assert_eq!(log_text(&text, "2026-11-03", "* **Update:** x"), None);
/// ```
#[tracing::instrument(level = "debug", skip_all)]
pub fn log_text(text: &str, date: &str, bullet: &str) -> Option<String> {
    let heading = format!("## {date}");
    // `lines` borrows from `text`, `heading` and `bullet`; Rust checks they
    // all outlive it, which is why `heading` is declared first.
    let mut lines: Vec<&str> = text.split('\n').collect();
    if let Some(at) = lines.iter().position(|line| *line == heading) {
        let blank = lines[at + 1..]
            .iter()
            .take_while(|line| py_strip(line).is_empty())
            .count();
        let first = at + 1 + blank;
        let section_end = first
            + lines[first..]
                .iter()
                .take_while(|line| !line.starts_with("## "))
                .count();
        if lines[first..section_end].contains(&bullet) {
            return None;
        }
        if lines.get(first).is_some_and(|line| line.starts_with("* ")) {
            lines.insert(first, bullet);
        } else {
            // `splice` is Python's slice assignment `lines[a:b] = [...]`;
            // dropping the iterator it returns applies the edit.
            lines.splice(at + 1..first, ["", bullet, ""]);
        }
    } else if let Some(first) = lines.iter().position(|line| line.starts_with("## ")) {
        lines.splice(first..first, [heading.as_str(), "", bullet, ""]);
    } else {
        let title = lines.iter().position(|line| line.starts_with("# "));
        while lines.last() == Some(&"") {
            lines.pop();
        }
        let insert_at = title.map_or(lines.len(), |t| t + 1);
        let mut block = vec!["", heading.as_str(), "", bullet];
        if insert_at < lines.len() {
            block.push("");
        }
        lines.splice(insert_at..insert_at, block);
        lines.push("");
    }
    Some(lines.join("\n"))
}

/// Is this line an index bullet (`okf.py`'s `_index_bullet`)?
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "per-element predicate or scanner called in a hot loop (parser or analysis)"
    )
)]
fn is_bullet(line: &str) -> bool {
    line.starts_with("* ") || line.starts_with("- ")
}

/// Drops trailing empty lines (`while lines and lines[-1] == "": lines.pop()`).
#[tracing::instrument(level = "trace", skip_all)]
fn trim_trailing_blank(lines: &mut Vec<Cow<'_, str>>) {
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
}

/// The first bullet of a heading's span at `start`, with the blank lines
/// `okf.py` puts around it.
#[tracing::instrument(level = "trace", skip_all)]
fn insert_first_bullet<'a>(lines: &mut Vec<Cow<'a, str>>, start: usize, bullet: &'a str) {
    let text_follows = lines
        .get(start)
        .is_some_and(|line| !py_strip(line).is_empty());
    let mut block: Vec<Cow<'a, str>> = vec!["".into(), bullet.into()];
    if text_follows {
        block.push("".into());
    }
    lines.splice(start..start, block);
}

/// `insert_index_bullet`'s edit: adds `bullet` after the last bullet of
/// its `section` heading, or, with no section, at the end of `# Concepts`
/// (or after the last bullet outside `# Sections`), creating `# Concepts`
/// ahead of `# Sections` when there is nowhere else to put it.
///
/// # Arguments
/// * `text` - the `index.md`, with universal newlines.
/// * `bullet` - the whole bullet line.
/// * `section` - the heading to list under (matched case-insensitively,
///   leading `#`s ignored); `None` or empty for the default place.
/// * `create_section` - append the heading when it is missing.
///
/// # Returns
/// The new text, ending in a newline; `None` when `section` is missing and
/// `create_section` is false (`okf.py`'s "no heading 'X'").
#[tracing::instrument(level = "debug", skip_all)]
pub fn insert_index_bullet_text(
    text: &str,
    bullet: &str,
    section: Option<&str>,
    create_section: bool,
) -> Option<String> {
    let mut lines: Vec<Cow<'_, str>> = text.split('\n').map(Cow::Borrowed).collect();
    let total = lines.len();
    let headings: Vec<(usize, String)> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| INDEX_HEADING.is_match(line))
        .map(|(i, line)| (i, py_strip(line.trim_start_matches('#')).to_lowercase()))
        .collect();
    // The lines after heading `i`, up to the next heading (`okf.py`'s `span`).
    let span = |i: usize| {
        let stop = headings
            .iter()
            .map(|(j, _)| *j)
            .find(|j| *j > i)
            .unwrap_or(total);
        (i + 1)..stop
    };
    let named = |want: &str| -> Vec<usize> {
        headings
            .iter()
            .filter(|(_, name)| name == want)
            .map(|(i, _)| *i)
            .collect()
    };
    if let Some(section) = section.filter(|s| !s.is_empty()) {
        let want = py_strip(py_strip(section).trim_start_matches('#'));
        match named(&want.to_lowercase()).first() {
            None if !create_section => return None,
            None => {
                trim_trailing_blank(&mut lines);
                // `Vec::extend` has two impls (owned and `Copy` references),
                // so the items are spelled as `Cow`s rather than `.into()`.
                lines.extend([
                    Cow::Borrowed(""),
                    Cow::Owned(format!("# {want}")),
                    Cow::Borrowed(""),
                    Cow::Borrowed(bullet),
                    Cow::Borrowed(""),
                ]);
            }
            Some(&hit) => {
                let range = span(hit);
                let start = range.start;
                // The last bullet: searched from the end (`rev().find`).
                let last = range.rev().find(|&i| is_bullet(&lines[i]));
                match last {
                    Some(last) => lines.insert(last + 1, bullet.into()),
                    None => insert_first_bullet(&mut lines, start, bullet),
                }
            }
        }
    } else {
        // Concept bullets never go under `# Sections`, which lists sub-folders.
        let concepts = named("concepts");
        let sections = named("sections");
        let last = match concepts.first() {
            Some(&heading) => span(heading).rev().find(|&i| is_bullet(&lines[i])),
            None => {
                let mut barred = vec![false; total];
                for range in sections.iter().map(|&s| span(s)) {
                    barred[range].iter_mut().for_each(|b| *b = true);
                }
                (0..total)
                    .rev()
                    .find(|&i| !barred[i] && is_bullet(&lines[i]))
            }
        };
        if let Some(last) = last {
            lines.insert(last + 1, bullet.into());
        } else if let Some(&heading) = concepts.first() {
            insert_first_bullet(&mut lines, heading + 1, bullet);
        } else if let Some(&heading) = sections.first() {
            let mut at = heading;
            while at > 0 && lines[at - 1].is_empty() {
                at -= 1;
            }
            lines.splice(
                at..at,
                [
                    Cow::Borrowed(""),
                    Cow::Borrowed("# Concepts"),
                    Cow::Borrowed(""),
                    Cow::Borrowed(bullet),
                ],
            );
        } else {
            trim_trailing_blank(&mut lines);
            lines.extend([
                Cow::Borrowed(""),
                Cow::Borrowed("# Concepts"),
                Cow::Borrowed(""),
                Cow::Borrowed(bullet),
                Cow::Borrowed(""),
            ]);
        }
    }
    if lines.last().is_none_or(|line| !line.is_empty()) {
        lines.push("".into());
    }
    Some(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::okf::ops::{ensure_index, log, LogKind, OpContext, OpOutcome};
    use chrono::{TimeZone, Utc};
    use serde_json::json;

    const FIXTURE_LOG: &str = "# Log\n\n## 2026-10-26\n\n* **Creation:** Added fixtures.\n";

    fn on(month: u32, day: u32) -> OpContext<'static> {
        OpContext {
            now: Utc.with_ymd_and_hms(2026, month, day, 12, 0, 0).unwrap(),
            by: "tester",
        }
    }

    fn listing(target: &str, bullet: &str, section: Option<&str>) -> ReplayOp {
        ReplayOp::ListBullet {
            target: target.to_string(),
            bullet: bullet.to_string(),
            section: section.map(str::to_string),
            create_section: false,
        }
    }

    #[test]
    fn log_text_adds_a_newer_heading_above_older_ones() {
        let text = log_text(FIXTURE_LOG, "2026-10-27", "* **Update:** Ran.").unwrap();

        assert_eq!(
            text,
            concat!(
                "# Log\n\n## 2026-10-27\n\n* **Update:** Ran.\n\n",
                "## 2026-10-26\n\n* **Creation:** Added fixtures.\n",
            )
        );
    }

    #[test]
    fn log_text_puts_the_newest_bullet_first_and_skips_a_duplicate() {
        let once = log_text(FIXTURE_LOG, "2026-10-26", "* **Change:** More.").unwrap();

        let again = log_text(&once, "2026-10-26", "* **Creation:** Added fixtures.");

        assert_eq!(
            once,
            "# Log\n\n## 2026-10-26\n\n* **Change:** More.\n* **Creation:** Added fixtures.\n"
        );
        assert_eq!(again, None);
    }

    #[test]
    fn log_text_fills_an_empty_heading_and_logs_without_dates() {
        let date = "2026-10-27";

        let empty_heading = log_text("# Log\n\n## 2026-10-27\n", date, "* b").unwrap();
        let title_only = log_text("# Log\n", date, "* b").unwrap();
        let title_and_intro = log_text("# Log\n\nIntro.\n", date, "* b").unwrap();
        let no_title = log_text("Intro, no title\n", date, "* b").unwrap();
        let empty = log_text("", date, "* b").unwrap();

        // Each expected value is what okf.py's cmd_log writes.
        assert_eq!(empty_heading, "# Log\n\n## 2026-10-27\n\n* b\n");
        assert_eq!(title_only, "# Log\n\n## 2026-10-27\n\n* b\n");
        assert_eq!(
            title_and_intro,
            "# Log\n\n## 2026-10-27\n\n* b\n\n\nIntro.\n"
        );
        assert_eq!(no_title, "Intro, no title\n\n## 2026-10-27\n\n* b\n");
        assert_eq!(empty, "\n## 2026-10-27\n\n* b\n");
    }

    #[test]
    fn insert_under_a_named_section() {
        let blank_after =
            insert_index_bullet_text("# A\n\n# Extras\n\nText.\n", "* b", Some("Extras"), false);
        let text_after =
            insert_index_bullet_text("# A\n\n# Extras\nText.\n", "* b", Some("## extras "), false);
        let after_bullets = insert_index_bullet_text(
            "# A\n\n# Extras\n\n* old\n- older\n\n# B\n",
            "* b",
            Some("Extras"),
            false,
        );

        assert_eq!(blank_after.unwrap(), "# A\n\n# Extras\n\n* b\n\nText.\n");
        assert_eq!(text_after.unwrap(), "# A\n\n# Extras\n\n* b\n\nText.\n");
        assert_eq!(
            after_bullets.unwrap(),
            "# A\n\n# Extras\n\n* old\n- older\n* b\n\n# B\n"
        );
    }

    #[test]
    fn insert_without_a_section_avoids_the_sections_list() {
        let sections = "\n# Sections\n\n* [S](s/) - s\n";
        let under_concepts = format!("# A\n\n# Concepts\n\nText.\n{sections}");
        let loose_bullets = format!("* [x](x.md) - x\n{sections}");
        let only_sections = format!("# A\n\nIntro.\n\n{sections}");

        let results = [
            insert_index_bullet_text(&under_concepts, "* b", None, false),
            insert_index_bullet_text(&loose_bullets, "* b", None, false),
            insert_index_bullet_text(&only_sections, "* b", None, false),
            insert_index_bullet_text("# A", "* b", None, false),
            insert_index_bullet_text("", "* b", Some(""), false),
        ];

        assert_eq!(
            results.map(Option::unwrap),
            [
                format!("# A\n\n# Concepts\n\n* b\n\nText.\n{sections}"),
                format!("* [x](x.md) - x\n* b\n{sections}"),
                format!("# A\n\nIntro.\n\n# Concepts\n\n* b\n\n{sections}"),
                "# A\n\n# Concepts\n\n* b\n".to_string(),
                "\n# Concepts\n\n* b\n".to_string(),
            ]
        );
    }

    #[test]
    fn a_missing_section_is_refused_or_created() {
        let refused = insert_index_bullet_text("# A\n", "* b", Some("Nope"), false);
        let created = insert_index_bullet_text(
            "# Deep\n\nTODO: say what this folder holds.\n",
            "* [Er](er/) - TODO: describe this folder",
            Some("Sections"),
            true,
        );

        assert_eq!(refused, None);
        assert_eq!(
            created.unwrap(),
            concat!(
                "# Deep\n\nTODO: say what this folder holds.\n\n# Sections\n\n",
                "* [Er](er/) - TODO: describe this folder\n",
            )
        );
    }

    #[test]
    fn create_index_creates_only_a_missing_file() {
        let create = [ReplayOp::CreateIndex {
            folder: "ops/new-stuff".to_string(),
        }];

        let missing = replay("ops/new-stuff/index.md", None, &create).unwrap();
        let present = replay("ops/new-stuff/index.md", Some("# Mine\n"), &create).unwrap();

        assert_eq!(
            missing,
            "# New stuff\n\nTODO: say what this folder holds.\n"
        );
        assert_eq!(present, "# Mine\n");
    }

    #[test]
    fn a_listing_is_added_once_and_a_folder_link_needs_no_slash() {
        let ops = [listing("ops/a.md", "* [A](a.md) - a", None)];
        let folder = [listing(
            "ops/index.md",
            "* [Ops](ops/) - o",
            Some("Sections"),
        )];

        let once = replay("ops/index.md", Some("# Ops\n"), &ops).unwrap();
        let twice = replay("ops/index.md", Some(&once), &ops).unwrap();
        let listed = replay("index.md", Some("# Root\n\n* [Ops](ops) - o\n"), &folder).unwrap();

        assert_eq!(once, "# Ops\n\n# Concepts\n\n* [A](a.md) - a\n");
        assert_eq!(twice, once);
        assert_eq!(listed, "# Root\n\n* [Ops](ops) - o\n");
    }

    #[test]
    fn replay_normalizes_newlines_only_when_it_changes_the_text() {
        let crlf_log = "# Log\r\n\r\n## 2026-10-26\r\n\r\n* **Update:** old\r\n";
        let crlf_index = "# Ops\r\n\r\n* [A](a.md) - a\r\n";
        let log_op = [ReplayOp::Log {
            date: "2026-10-27".to_string(),
            bullet: "* **Update:** b".to_string(),
        }];
        let listed = [listing("ops/a.md", "* [A](a.md) - a", None)];

        let changed = replay("log.md", Some(crlf_log), &log_op).unwrap();
        let unchanged = replay("ops/index.md", Some(crlf_index), &listed).unwrap();

        assert_eq!(
            changed,
            "# Log\n\n## 2026-10-27\n\n* **Update:** b\n\n## 2026-10-26\n\n* **Update:** old\n"
        );
        assert_eq!(unchanged, crlf_index);
    }

    #[test]
    fn replay_refuses_a_missing_file_or_heading() {
        let log_op = [ReplayOp::Log {
            date: "2026-10-27".to_string(),
            bullet: "* b".to_string(),
        }];
        let in_section = [listing("people/x.md", "* [X](x.md) - x", Some("Nowhere"))];

        let no_log = replay("log.md", None, &log_op).unwrap_err();
        let no_heading = replay("people/index.md", Some("# People\n"), &in_section).unwrap_err();

        assert!(matches!(no_log, OkfError::NotFound { ref path } if path == "log.md"));
        assert!(matches!(
            no_heading,
            OkfError::Refused { ref reason } if reason == "no heading 'Nowhere' in people/index.md"
        ));
    }

    #[test]
    fn create_index_of_the_root_and_create_log_keep_text_that_exists() {
        let root = [ReplayOp::CreateIndex {
            folder: String::new(),
        }];
        let log = [ReplayOp::CreateLog];

        let new_root = replay("index.md", None, &root).unwrap();
        let kept_root = replay("index.md", Some("# Mine\n"), &root).unwrap();
        let new_log = replay("log.md", None, &log).unwrap();
        let kept_log = replay("log.md", Some("# Log\n\n## x\n"), &log).unwrap();

        assert_eq!(new_root, new_index_text(""));
        assert!(new_root.starts_with("---\nokf_version: \"0.2\"\n---\n"));
        assert_eq!(kept_root, "# Mine\n");
        assert_eq!(new_log, LOG_TEMPLATE);
        assert_eq!(kept_log, "# Log\n\n## x\n");
        assert_eq!(
            serde_json::to_value(&log[0]).unwrap(),
            json!({"op": "create_log"})
        );
    }

    #[test]
    fn repair_ops_replay_onto_a_root_index_created_meanwhile() {
        // A repair in one changeset creates the root and lists a concept and
        // a folder; meanwhile someone else published a curated root.
        let workspace = MemWorkspace::from_files([
            (
                "a.md",
                "---\ntype: Note\ntitle: A\ndescription: First.\n---\n",
            ),
            ("ops/index.md", "# Ops\n"),
        ]);
        let outcome = crate::okf::ops::repair_indexes(&workspace).unwrap();
        let curated = "---\nokf_version: \"0.2\"\n---\n# Ours\n\nCurated words.\n";

        let replayed = replay("index.md", Some(curated), &outcome.replay["index.md"]).unwrap();

        assert_eq!(
            replayed,
            concat!(
                "---\nokf_version: \"0.2\"\n---\n# Ours\n\nCurated words.\n\n",
                "# Concepts\n\n* [A](a.md) - First.\n\n",
                "# Sections\n\n* [Ops](ops/) - TODO: describe this folder\n",
            )
        );
    }

    #[test]
    fn replay_ops_serialize_with_an_op_tag() {
        let op = listing("a/b.md", "* [B](b.md) - b", None);

        let value = serde_json::to_value(&op).unwrap();
        let back: ReplayOp = serde_json::from_value(value.clone()).unwrap();

        assert_eq!(
            value,
            json!({
                "op": "list_bullet",
                "target": "a/b.md",
                "bullet": "* [B](b.md) - b",
                "section": null,
                "create_section": false,
            })
        );
        assert_eq!(back, op);
    }

    #[test]
    fn ensure_index_ops_rebuild_every_file_they_touched() {
        let root = "# Root\n\n* [Infra](infra/) - systems\n";
        let workspace = MemWorkspace::from_files([("index.md", root)]);
        let mut outcome = OpOutcome::default();

        ensure_index(&workspace, "ops/deep/er", &mut outcome).unwrap();

        assert_eq!(
            outcome.touched,
            [
                "ops/deep/er/index.md",
                "ops/deep/index.md",
                "ops/index.md",
                "index.md"
            ]
        );
        for path in &outcome.touched {
            let original = (path == "index.md").then_some(root);
            let replayed = replay(path, original, &outcome.replay[path]).unwrap();
            assert_eq!(replayed, workspace.read_to_string(path).unwrap(), "{path}");
        }
    }

    #[hegel::test(derandomize = true)]
    fn log_replay_redoes_the_ops_and_is_idempotent(tc: hegel::TestCase) {
        use hegel::generators;
        let bases = vec![
            FIXTURE_LOG,
            "# Log\n",
            "",
            "Intro, no title\n",
            "# Log\n\n## 2026-10-27\n",
            "# Log\r\n\r\n## 2026-10-27\r\n\r\n* **Update:** a\r\n",
        ];
        let words = vec!["a", "b", "c  d", "e\nf"];
        let base: &str = tc.draw(generators::sampled_from(&bases));
        let workspace = MemWorkspace::from_files([("log.md", base)]);
        let count = tc.draw(generators::integers::<usize>().max_value(6));
        let mut ops = Vec::new();
        for _ in 0..count {
            let day = tc.draw(generators::integers::<u32>().min_value(26).max_value(28));
            let word: &str = tc.draw(generators::sampled_from(&words));
            let outcome = log(&workspace, &on(10, day), LogKind::Update, word).unwrap();
            ops.extend(outcome.replay.get("log.md").cloned().unwrap_or_default());
        }

        let once = replay("log.md", Some(base), &ops).unwrap();
        let twice = replay("log.md", Some(&once), &ops).unwrap();

        assert_eq!(once, workspace.read_to_string("log.md").unwrap());
        assert_eq!(twice, once);
    }

    #[hegel::test(derandomize = true)]
    fn replaying_a_branch_onto_another_keeps_every_bullet(tc: hegel::TestCase) {
        use hegel::generators;
        let left = MemWorkspace::from_files([("log.md", FIXTURE_LOG)]);
        let right = MemWorkspace::from_files([("log.md", FIXTURE_LOG)]);
        let mut bullets = vec!["* **Creation:** Added fixtures.".to_string()];
        let mut right_ops = Vec::new();
        for (workspace, side) in [(&left, "left"), (&right, "right")] {
            let count = tc.draw(generators::integers::<usize>().max_value(4));
            for n in 0..count {
                let day = tc.draw(generators::integers::<u32>().min_value(26).max_value(28));
                let text = format!("{side} {n}");
                let outcome = log(workspace, &on(10, day), LogKind::Update, &text).unwrap();
                let ops = outcome.replay["log.md"].clone();
                bullets.push(format!("* **Update:** {text}"));
                if side == "right" {
                    right_ops.extend(ops);
                }
            }
        }
        let published = left.read_to_string("log.md").unwrap();

        let merged = replay("log.md", Some(&published), &right_ops).unwrap();
        let again = replay("log.md", Some(&merged), &right_ops).unwrap();

        for bullet in &bullets {
            let copies = merged.split('\n').filter(|line| line == bullet).count();
            assert_eq!(copies, 1, "{bullet} in\n{merged}");
        }
        assert_eq!(again, merged);
    }

    const INFRA: &str =
        "# Infra\n\n# Concepts\n\n* [Time](time.md) - the clock\n* [NTP](ntp.md) - sync\n";

    fn unlist(target: &str, bullet: Option<&str>) -> ReplayOp {
        ReplayOp::Unlist {
            target: target.to_string(),
            bullet: bullet.map(str::to_string),
        }
    }

    fn relink(from: &str, to: &str) -> ReplayOp {
        ReplayOp::Relink {
            redirect: Redirect {
                from: from.to_string(),
                to: to.to_string(),
                anchors: None,
            },
        }
    }

    #[test]
    fn unlist_text_takes_the_first_bullet_linking_the_target() {
        let workspace = MemWorkspace::default();
        let text = concat!(
            "# Infra\n\nSee [NTP](ntp.md).\n\n",
            "* [NTP](/infra/ntp.md) - sync\n* [NTP again](ntp.md) - twice\n",
        );

        let line = listing_line(&workspace, text, "infra/index.md", "infra/ntp.md");
        let taken = unlist_text(&workspace, text, "infra/index.md", "infra/ntp.md");
        let missing = unlist_text(&workspace, INFRA, "infra/index.md", "infra/other.md");

        // The prose link on line 2 is not a bullet; the absolute bullet is first.
        assert_eq!(line, Some(4));
        assert_eq!(
            taken,
            Some((
                "# Infra\n\nSee [NTP](ntp.md).\n\n* [NTP again](ntp.md) - twice\n".to_string(),
                "* [NTP](/infra/ntp.md) - sync".to_string()
            ))
        );
        assert_eq!(missing, None);
    }

    #[test]
    fn unlist_replays_idempotently_and_refuses_a_changed_listing() {
        let moved = [unlist("infra/ntp.md", Some("* [NTP](ntp.md) - sync"))];
        let edited = INFRA.replace("- sync", "- how hosts sync");

        let once = replay("infra/index.md", Some(INFRA), &moved).unwrap();
        let twice = replay("infra/index.md", Some(&once), &moved).unwrap();
        let refused = replay("infra/index.md", Some(&edited), &moved).unwrap_err();
        let deleted = replay(
            "infra/index.md",
            Some(&edited),
            &[unlist("infra/ntp.md", None)],
        );

        assert_eq!(
            once,
            "# Infra\n\n# Concepts\n\n* [Time](time.md) - the clock\n"
        );
        assert_eq!(twice, once);
        assert!(
            matches!(refused, OkfError::Refused { ref reason } if reason.contains("changed since")),
            "{refused:?}"
        );
        // A delete removes the listing whatever it reads now.
        assert_eq!(deleted.unwrap(), once);
    }

    #[test]
    fn unlist_writes_newline_endings_like_cmd_mv() {
        let crlf = INFRA.replace('\n', "\r\n");

        let replayed = replay(
            "infra/index.md",
            Some(&crlf),
            &[unlist("infra/ntp.md", None)],
        );

        // `cmd_mv` reads and writes the old index in text mode.
        assert_eq!(
            replayed.unwrap(),
            "# Infra\n\n# Concepts\n\n* [Time](time.md) - the clock\n"
        );
    }

    #[test]
    fn unlist_of_a_missing_index_is_not_found() {
        let missing = replay("infra/index.md", None, &[unlist("infra/ntp.md", None)]);

        assert!(
            matches!(missing, Err(OkfError::NotFound { ref path }) if path == "infra/index.md"),
            "{missing:?}"
        );
    }

    #[test]
    fn relink_of_a_non_concept_is_refused_not_a_panic() {
        let ops = [relink("infra/index.md", "hosts/index.md")];

        let refused = replay("index.md", Some("* [Infra](infra/) - x\n"), &ops);

        assert!(
            matches!(
                refused,
                Err(OkfError::Refused { ref reason })
                    if reason.contains("/infra/index.md, which is not a concept")
            ),
            "{refused:?}"
        );
    }

    #[test]
    fn relink_replays_byte_for_byte_and_idempotently() {
        let crlf = "# Infra\r\n\r\n* [Time](time.md) - the clock\r\n";
        let ops = [relink("infra/time.md", "infra/clock.md")];

        let once = replay("infra/index.md", Some(crlf), &ops).unwrap();
        let twice = replay("infra/index.md", Some(&once), &ops).unwrap();

        assert_eq!(once, "# Infra\r\n\r\n* [Time](clock.md) - the clock\r\n");
        assert_eq!(twice, once);
    }

    #[test]
    fn a_log_entry_then_a_relink_replay_in_order() {
        let ops = [
            ReplayOp::Log {
                date: "2026-11-03".to_string(),
                bullet: "* **Update:** Added [x](/infra/x.md).".to_string(),
            },
            relink("infra/x.md", "infra/y.md"),
        ];

        let replayed = replay("log.md", Some("# Log\n"), &ops).unwrap();

        assert_eq!(
            replayed,
            "# Log\n\n## 2026-11-03\n\n* **Update:** Added [x](/infra/y.md).\n"
        );
    }

    #[test]
    fn the_new_ops_serialize_with_an_op_tag() {
        let ops = vec![unlist("a/b.md", None), relink("a/b.md", "a/c.md")];

        let value = serde_json::to_value(&ops).unwrap();
        let back: Vec<ReplayOp> = serde_json::from_value(value.clone()).unwrap();

        assert_eq!(
            value,
            json!([
                {"op": "unlist", "target": "a/b.md", "bullet": null},
                {"op": "relink", "redirect": {"from": "a/b.md", "to": "a/c.md", "anchors": null}},
            ])
        );
        assert_eq!(back, ops);
    }
    fn unlink(target: &str) -> ReplayOp {
        ReplayOp::Unlink {
            target: target.to_string(),
        }
    }

    #[test]
    fn unlink_keeps_the_text_and_every_other_byte_and_is_idempotent() {
        let crlf = "# Log\r\n\r\n* Added [x](/infra/x.md#a \"t\") and [y](infra/y.md).\r\n";
        let ops = [unlink("infra/x.md")];

        let once = replay("log.md", Some(crlf), &ops).unwrap();
        let twice = replay("log.md", Some(&once), &ops).unwrap();

        assert_eq!(once, "# Log\r\n\r\n* Added x and [y](infra/y.md).\r\n");
        assert_eq!(twice, once);
    }

    #[test]
    fn unlink_of_text_with_no_link_into_the_target_keeps_it() {
        let text = "# Log\r\n\r\n* Added [y](/infra/y.md).\r\n";

        let replayed = replay("log.md", Some(text), &[unlink("infra/x.md")]).unwrap();

        assert_eq!(replayed, text);
    }

    #[test]
    fn unlink_leaves_no_link_even_when_the_text_forms_a_new_one() {
        // The first link's text is `[a`; replacing the link with it leaves
        // `[a](/infra/x.md)`, a new link into the target, which goes too.
        let text = "* [[a](/infra/x.md)](/infra/x.md) and [[b](infra/y.md)](/infra/x.md)\n";

        let replayed = replay("log.md", Some(text), &[unlink("infra/x.md")]).unwrap();

        // The second part's link is `[[b](infra/y.md)`, into y; what follows
        // it is text, as the link graph reads it, so it stays.
        assert_eq!(replayed, "* a and [[b](infra/y.md)](/infra/x.md)\n");
    }

    #[test]
    fn a_log_entry_and_an_unlink_replay_in_either_order() {
        let base = "# Log\n\n## 2026-11-02\n\n* **Creation:** Added [x](/infra/x.md).\n";
        let entry = ReplayOp::Log {
            date: "2026-11-03".to_string(),
            bullet: "* **Update:** Synced the [hub](/hub.md).".to_string(),
        };
        let expected = concat!(
            "# Log\n\n## 2026-11-03\n\n* **Update:** Synced the [hub](/hub.md).\n\n",
            "## 2026-11-02\n\n* **Creation:** Added x.\n",
        );

        // The delete published first, then the entry replayed onto it.
        let deleted = replay("log.md", Some(base), &[unlink("infra/x.md")]).unwrap();
        let entry_last = replay("log.md", Some(&deleted), std::slice::from_ref(&entry)).unwrap();
        // The entry published first, then the delete's unlink replayed onto it.
        let logged = replay("log.md", Some(base), &[entry]).unwrap();
        let unlink_last = replay("log.md", Some(&logged), &[unlink("infra/x.md")]).unwrap();

        assert_eq!(entry_last, expected);
        assert_eq!(unlink_last, expected);
    }

    #[test]
    fn unlink_of_a_missing_file_is_not_found_and_of_a_non_concept_refused() {
        let missing = replay("log.md", None, &[unlink("infra/x.md")]);
        let index = replay("log.md", Some("# Log\n"), &[unlink("infra/index.md")]);

        assert!(
            matches!(missing, Err(OkfError::NotFound { .. })),
            "{missing:?}"
        );
        assert!(
            matches!(
                index,
                Err(OkfError::Refused { ref reason })
                    if reason.contains("/infra/index.md, which is not a concept")
            ),
            "{index:?}"
        );
    }

    #[test]
    fn unlink_serializes_with_an_op_tag() {
        let ops = vec![unlink("a/b.md")];

        let value = serde_json::to_value(&ops).unwrap();
        let back: Vec<ReplayOp> = serde_json::from_value(value.clone()).unwrap();

        assert_eq!(value, json!([{"op": "unlink", "target": "a/b.md"}]));
        assert_eq!(back, ops);
    }

    /// The op `repair_indexes` records for the raw bullet `line` of
    /// `d/{name}` in `d/index.md`: its destination `name`, encoded.
    fn rewrite(name: &str, line: &str) -> ReplayOp {
        let start = legacy_destination_start(line).unwrap();
        assert!(line[start..].starts_with(name), "{line}");
        let rest = &line[start + name.len()..];
        ReplayOp::RewriteRawListing {
            target: format!("d/{name}"),
            line: line.to_string(),
            fixed: format!("{}{}{rest}", &line[..start], encode_link_path(name)),
        }
    }

    #[test]
    fn rewrite_raw_listing_replays_onto_newer_text_and_idempotently() {
        // Someone added prose and another bullet since the repair: the raw
        // line is still rewritten, and nothing else changes.
        for name in ["a b.md", "a:b.md"] {
            let line = format!("* [Odd]({name}) - odd");
            let current = format!("# D\n\nNew prose.\n\n{line}\n* [New](new.md) - n\n");
            let ops = [rewrite(name, &line)];

            let once = replay("d/index.md", Some(&current), &ops).unwrap();
            let twice = replay("d/index.md", Some(&once), &ops).unwrap();

            let fixed = format!("* [Odd]({}) - odd", encode_link_path(name));
            assert_eq!(
                once,
                format!("# D\n\nNew prose.\n\n{fixed}\n* [New](new.md) - n\n"),
                "{name}"
            );
            assert_eq!(twice, once, "{name}");
        }
    }

    #[test]
    fn rewrite_raw_listing_replay_removes_or_rewrites_by_the_current_text() {
        let line = "* [Odd](a b.md) - old";
        let ops = [rewrite("a b.md", line)];
        // A working bullet added meanwhile: the raw one is a duplicate.
        let beside = format!("# D\n\n{line}\n* [Odd](a%20b.md) - new\n");
        // The working bullet the repair saw was removed meanwhile: the raw
        // one is rewritten, never dropped.
        let alone = format!("# D\n\n{line}\n");

        assert_eq!(
            replay("d/index.md", Some(&beside), &ops).unwrap(),
            "# D\n\n* [Odd](a%20b.md) - new\n"
        );
        assert_eq!(
            replay("d/index.md", Some(&alone), &ops).unwrap(),
            "# D\n\n* [Odd](a%20b.md) - old\n"
        );
    }

    #[test]
    fn rewrite_raw_listing_replay_leaves_a_changed_line_and_a_parsed_link_alone() {
        // The line was edited meanwhile: left as it is, byte for byte.
        let ops = [rewrite("a b.md", "* [Odd](a b.md) - old")];
        let edited = "# D\r\n\r\n* [Odd](a b.md) - edited\r\n";
        assert_eq!(replay("d/index.md", Some(edited), &ops).unwrap(), edited);
        // `a).md` and `a#b.md` read as a link to `d/a`, which replay cannot
        // tell is missing (a concurrent change may have created it): left.
        for name in ["a).md", "a#b.md"] {
            let line = format!("* [Odd]({name}) - old");
            let current = format!("# D\n\n{line}\n");
            let ops = [rewrite(name, &line)];

            let got = replay("d/index.md", Some(&current), &ops).unwrap();

            assert_eq!(got, current, "{name}");
        }
    }

    /// A [`ReplayOp::RewriteRawListing`] of the folder `d/<name>`'s listing
    /// in `d/index.md`, as `repair_indexes` records it (issue #731).
    fn rewrite_folder(name: &str, line: &str) -> ReplayOp {
        let start = legacy_destination_start(line).unwrap();
        assert!(line[start..].starts_with(name), "{line}");
        let rest = &line[start + name.len()..];
        ReplayOp::RewriteRawListing {
            target: format!("d/{name}/index.md"),
            line: line.to_string(),
            fixed: format!("{}{}{rest}", &line[..start], encode_link_path(name)),
        }
    }

    #[test]
    fn rewrite_raw_folder_listing_replays_onto_newer_text_and_idempotently() {
        for name in ["a b", "a:b"] {
            let line = format!("* [Odd]({name}/) - odd");
            let current = format!("# D\n\nNew prose.\n\n# Sections\n\n{line}\n* [New](new/) - n\n");
            let ops = [rewrite_folder(name, &line)];

            let once = replay("d/index.md", Some(&current), &ops).unwrap();
            let twice = replay("d/index.md", Some(&once), &ops).unwrap();

            let fixed = format!("* [Odd]({}/) - odd", encode_link_path(name));
            assert_eq!(
                once,
                format!("# D\n\nNew prose.\n\n# Sections\n\n{fixed}\n* [New](new/) - n\n"),
                "{name}"
            );
            assert_eq!(twice, once, "{name}");
        }
    }

    #[test]
    fn rewrite_raw_folder_listing_replay_never_loses_the_listing() {
        let line = "* [Odd](a b/) - old";
        let ops = [rewrite_folder("a b", line)];
        // A working bullet added meanwhile, as a folder link or the index
        // file: the raw one is a duplicate and goes.
        for working in ["* [Odd](a%20b/) - new", "* [Odd](a%20b/index.md) - new"] {
            let beside = format!("# D\n\n{line}\n{working}\n");
            assert_eq!(
                replay("d/index.md", Some(&beside), &ops).unwrap(),
                format!("# D\n\n{working}\n"),
                "{working}"
            );
        }
        // Replay cannot tell that `a%20b` (no slash) is a directory, so it
        // does not count as a working listing: the raw bullet is rewritten,
        // never dropped (two working bullets are left, which is safe).
        let unslashed = format!("# D\n\n{line}\n* [Odd](a%20b) - new\n");
        assert_eq!(
            replay("d/index.md", Some(&unslashed), &ops).unwrap(),
            "# D\n\n* [Odd](a%20b/) - old\n* [Odd](a%20b) - new\n"
        );
        // The working bullet the repair saw was removed meanwhile: the raw
        // one is rewritten.
        assert_eq!(
            replay("d/index.md", Some(&format!("# D\n\n{line}\n")), &ops).unwrap(),
            "# D\n\n* [Odd](a%20b/) - old\n"
        );
    }

    #[test]
    fn rewrite_raw_folder_listing_replay_leaves_a_parsed_link_alone() {
        // `a)b/` and `a#b/` read as a link to `d/a`, which replay cannot tell
        // is missing (a concurrent change may have created it): left.
        for name in ["a)b", "a#b"] {
            let line = format!("* [Odd]({name}/) - old");
            let current = format!("# D\n\n{line}\n");
            let ops = [rewrite_folder(name, &line)];

            let got = replay("d/index.md", Some(&current), &ops).unwrap();

            assert_eq!(got, current, "{name}");
        }
    }

    #[test]
    fn rewrite_raw_listing_serializes_with_an_op_tag() {
        let ops = vec![rewrite("a b.md", "* [A](a b.md)")];

        let value = serde_json::to_value(&ops).unwrap();
        let back: Vec<ReplayOp> = serde_json::from_value(value.clone()).unwrap();

        assert_eq!(
            value,
            json!([{
                "op": "rewrite_raw_listing",
                "target": "d/a b.md",
                "line": "* [A](a b.md)",
                "fixed": "* [A](a%20b.md)"
            }])
        );
        assert_eq!(back, ops);
    }

    /// A catalog whose reads work but whose `list_markdown` fails, to reach
    /// `belongs_to_longer_raw`'s fallback (issue #734).
    #[derive(Debug, Default)]
    struct FailingListing(MemWorkspace);

    impl Workspace for FailingListing {
        fn read_to_string(&self, path: &str) -> Result<String, OkfError> {
            self.0.read_to_string(path)
        }
        fn write_atomic(&self, path: &str, contents: &str) -> Result<(), OkfError> {
            self.0.write_atomic(path, contents)
        }
        fn is_file(&self, path: &str) -> bool {
            self.0.is_file(path)
        }
        fn is_dir(&self, path: &str) -> bool {
            self.0.is_dir(path)
        }
        fn list_markdown(&self) -> Result<Vec<String>, OkfError> {
            Err(OkfError::Io {
                path: String::new(),
                source: std::io::Error::other("listing unavailable"),
            })
        }
        fn list_files(&self) -> Result<Vec<String>, OkfError> {
            self.0.list_files()
        }
    }

    #[test]
    fn a_failed_catalog_listing_leaves_a_raw_bullet_unassigned() {
        // Issue #734: with the catalog unlistable, the raw bullet might
        // belong to a longer document, so no target takes it.
        let bullet = "* [Short](a b.md) - summary";
        let working = MemWorkspace::default();
        let failing = FailingListing::default();
        for ws in [&working as &dyn Workspace, &failing] {
            ws.write_atomic("d/a b.md", "---\ntype: Note\n---\n")
                .unwrap();
        }
        // The same call finds the bullet when the catalog lists fine, so the
        // `None` below is the fallback and nothing else.
        assert!(first_raw_listing(&working, bullet, "d/index.md", "d/a b.md").is_some());
        assert!(first_raw_listing(&failing, bullet, "d/index.md", "d/a b.md").is_none());
        assert!(!lists_raw_target(
            &failing,
            bullet,
            "d/index.md",
            "d/a b.md"
        ));
    }

    /// The op `index` records for describing `folder`.
    fn describe(folder: &str, summary: &str) -> ReplayOp {
        ReplayOp::SetListingSummary {
            target: format!("{folder}/index.md"),
            summary: summary.to_string(),
        }
    }

    const ROOT_WITH_LEARNING: &str =
        "# Root\n\n# Sections\n\n* [Learning](learning/) - TODO: describe this folder\n";

    #[test]
    fn set_listing_summary_serializes_with_an_op_tag() {
        let ops = vec![describe("learning", "What we learn.")];

        let value = serde_json::to_value(&ops).unwrap();
        let back: Vec<ReplayOp> = serde_json::from_value(value.clone()).unwrap();

        assert_eq!(
            value,
            json!([{
                "op": "set_listing_summary",
                "target": "learning/index.md",
                "summary": "What we learn."
            }])
        );
        assert_eq!(back, ops);
    }

    #[test]
    fn set_listing_summary_replays_onto_newer_text_and_keeps_their_label() {
        // Since the edit, someone else listed another folder and relabelled
        // ours: the replay changes only the summary.
        let newer = concat!(
            "# Root\n\nAbout.\n\n# Sections\n\n",
            "* [Our learning](learning/) - TODO: describe this folder\n",
            "* [Preferences](preferences/) - What we like.\n",
        );

        let text = replay(
            "index.md",
            Some(newer),
            &[describe("learning", "What we learn.")],
        );

        assert_eq!(
            text.unwrap(),
            concat!(
                "# Root\n\nAbout.\n\n# Sections\n\n",
                "* [Our learning](learning/) - What we learn.\n",
                "* [Preferences](preferences/) - What we like.\n",
            )
        );
    }

    #[test]
    fn a_listing_and_its_summary_both_replay_onto_a_concurrent_folder_line() {
        // The TODO-then-describe flow: `ensure_index`'s listing, then `index`
        // on the folder, both replayed onto a root that gained a line.
        let ops = [
            ReplayOp::ListBullet {
                target: "learning/index.md".to_string(),
                bullet: "* [Learning](learning/) - TODO: describe this folder".to_string(),
                section: Some("Sections".to_string()),
                create_section: true,
            },
            describe("learning", "What we learn."),
        ];
        let newer = "# Root\n\n# Sections\n\n* [Preferences](preferences/) - What we like.\n";

        let text = replay("index.md", Some(newer), &ops).unwrap();

        assert_eq!(
            text,
            concat!(
                "# Root\n\n# Sections\n\n",
                "* [Preferences](preferences/) - What we like.\n",
                "* [Learning](learning/) - What we learn.\n",
            )
        );
    }

    #[test]
    fn set_listing_summary_refuses_a_listing_another_changeset_removed() {
        let gone = "# Root\n\n# Sections\n\n* [Preferences](preferences/) - What we like.\n";

        let error = replay("index.md", Some(gone), &[describe("learning", "x")]).unwrap_err();

        let OkfError::Refused { reason } = &error else {
            panic!("not Refused: {error:?}")
        };
        assert!(
            reason.starts_with("index.md no longer lists /learning/ (another changeset"),
            "{reason}"
        );
    }

    #[test]
    fn set_listing_summary_of_a_missing_index_is_not_found() {
        let error = replay("index.md", None, &[describe("learning", "x")]).unwrap_err();

        assert!(matches!(error, OkfError::NotFound { ref path } if path == "index.md"));
    }

    #[test]
    fn set_listing_summary_that_already_reads_so_keeps_the_bytes() {
        let crlf = ROOT_WITH_LEARNING
            .replace("TODO: describe this folder", "Ours.")
            .replace('\n', "\r\n");

        let text = replay("index.md", Some(&crlf), &[describe("learning", "Ours.")]).unwrap();

        assert_eq!(text, crlf);
    }

    #[test]
    fn set_listing_summary_never_edits_a_bullet_that_only_mentions_the_folder() {
        // A link to the folder in another bullet's summary is not its
        // listing: the listing below is the one changed.
        let text = concat!(
            "# Root\n\n# Concepts\n\n* [Notes](notes.md) - see [learning](learning/)\n",
            "* See [learning](learning/) - for more\n\n",
            "# Sections\n\n* [Learning](learning/) - TODO: describe this folder\n",
        );
        let only_mentioned = concat!(
            "# Root\n\n* [Notes](notes.md) - see [learning](learning/)\n",
            "* See [learning](learning/) - for more\n",
        );

        let set = replay("index.md", Some(text), &[describe("learning", "New.")]).unwrap();
        let refused = replay(
            "index.md",
            Some(only_mentioned),
            &[describe("learning", "New.")],
        );

        assert_eq!(
            set,
            concat!(
                "# Root\n\n# Concepts\n\n* [Notes](notes.md) - see [learning](learning/)\n",
                "* See [learning](learning/) - for more\n\n",
                "# Sections\n\n* [Learning](learning/) - New.\n",
            )
        );
        assert!(
            matches!(refused, Err(OkfError::Refused { .. })),
            "{refused:?}"
        );
    }

    #[test]
    fn set_listing_summary_refuses_a_raw_listing_rather_than_guess() {
        // Issue #731's raw folder bullet: its link does not resolve, so the
        // replay cannot tell it lists `my docs/`; it refuses, never edits it.
        let raw = "# Root\n\n# Sections\n\n* [My docs](my docs/) - Mine.\n";

        let result = replay("index.md", Some(raw), &[describe("my docs", "New.")]);

        assert!(
            matches!(result, Err(OkfError::Refused { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn set_listing_summary_text_keeps_what_sits_between_the_link_and_the_dash() {
        let ws = MemWorkspace::default();
        let with_aside = "* [L](/learning/) (archived) - old - older\n";
        let bare = "- [L](learning) \n";

        let aside =
            set_listing_summary_text(&ws, with_aside, "index.md", "learning/index.md", "New.");
        let appended = set_listing_summary_text(&ws, bare, "index.md", "learning/index.md", "New.");

        assert_eq!(aside.unwrap(), "* [L](/learning/) (archived) - New.\n");
        assert_eq!(appended.unwrap(), "- [L](learning) - New.\n");
    }
}
