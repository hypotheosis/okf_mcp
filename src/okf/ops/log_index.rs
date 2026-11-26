//! The bookkeeping ops `log` and `index` (`okf.py:752-820`) and
//! `ensure_index` (`:1471`). Each writes through the workspace and records
//! the [`ReplayOp`]s that redo its edit at publish (see [`mod@super::replay`]).
//!
//! Also two server-only pieces `okf.py` has no command for (issue #426): the
//! [`starter_files`] a new catalog is created with, and [`repair_indexes`],
//! which creates the root `index.md` and `log.md`, folder indexes and listings
//! a catalog is missing, so a catalog created empty can be repaired through a
//! changeset; [`repair_catalog`] is the operator's direct-write variant.
//!
//! Divergences from `okf.py`:
//! - a missing `log.md` is [`OkfError::NotFound`] (okf.py: `no log.md at <root>`);
//! - `ensure_index` upper-cases a folder name's first character where
//!   Python's `capitalize` title-cases it; they differ only for the few
//!   digraph letters such as `ǆ`;
//! - `ensure_index` needs no existing directory (`okf.py` expects `mv` to
//!   have created it): a workspace creates parents on write;
//! - `index` refuses a list or mapping `title` with "has no title", where
//!   `okf.py` crashes (`AttributeError` on `.replace`). Scalar titles such as
//!   `2026` or `true` are strings in both parsers, so they list alike.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};

use crate::error::OkfError;
use crate::okf::doc::Doc;
use crate::okf::markdown::encode_link_path;
use crate::okf::ops::replay::{
    already_lists, first_raw_listing, folder_heading, insert_index_bullet_text, lists_target,
    log_text, missing_heading, new_index_text, rewrite_raw_listing_text, root_index_text,
    set_listing_summary_text, ReplayOp, LOG_TEMPLATE,
};
use crate::okf::ops::{load_concept, refused, LogKind, OpContext, OpOutcome};
use crate::okf::paths::{basename, dirname, escapes_root, is_concept, join, normpath};
use crate::okf::pystr::is_py_space;
use crate::okf::rules::is_machinery;
use crate::okf::workspace::Workspace;
use crate::okf::yaml::Value;

/// The catalog's log, at the root.
const LOG: &str = "log.md";
/// The root index.
const ROOT_INDEX: &str = "index.md";
/// A folder's index file name.
const INDEX: &str = "index.md";
/// The summary of a folder listing nobody has described yet.
const FOLDER_TODO: &str = "TODO: describe this folder";
/// The summary of a concept listing whose concept has no `description`.
const CONCEPT_TODO: &str = "TODO: describe this concept";
/// The end of `ensure_index`'s note for a folder it listed with the TODO
/// placeholder. `okf.py` says "; replace its TODO lines (and the parent's
/// listing)", which steers to a free edit of the parent index, the one edit
/// publish cannot replay (issue #668); `tests/parity.rs` translates it.
pub(crate) const FOLDER_NOTE: &str =
    "; replace its TODO line, and describe the folder with folder_summary, \
                           or with index on the folder's path (ending in /)";
/// The heading folder listings go under.
const SECTIONS: &str = "Sections";

/// Python's `" ".join(text.split())`: runs of whitespace become one space,
/// and leading and trailing whitespace goes. Shared with `move_concept` and
/// `split_concept`, whose listings collapse the description the same way.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) fn collapse_whitespace(text: &str) -> String {
    text.split(is_py_space)
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Port of `cmd_log`: adds `* **<kind>:** <text>` under today's (UTC)
/// heading in the root `log.md`.
///
/// # Arguments
/// * `workspace` - the catalog.
/// * `ctx` - its `now` gives the date.
/// * `kind` - the entry's kind.
/// * `text` - the entry; whitespace runs collapse to single spaces.
///
/// # Returns
/// The outcome, with a `Log` replay op for `log.md`; nothing is touched
/// when the bullet is already under today's heading.
///
/// # Errors
/// [`OkfError::NotFound`] without a `log.md`; the workspace's write error.
#[tracing::instrument(level = "debug", skip_all)]
pub fn log(
    workspace: &dyn Workspace,
    ctx: &OpContext<'_>,
    kind: LogKind,
    text: &str,
) -> Result<OpOutcome, OkfError> {
    // Text mode with universal newlines, as `okf.py` reads `log.md`.
    let current = workspace.read_text(LOG)?;
    let date = ctx.now.format("%Y-%m-%d").to_string();
    let bullet = format!("* **{}:** {}", kind.as_str(), collapse_whitespace(text));
    let Some(updated) = log_text(&current, &date, &bullet) else {
        return Ok(OpOutcome::unchanged(format!(
            "{LOG}: already logged under {date}"
        )));
    };
    workspace.write_atomic(LOG, &updated)?;
    // A struct literal with `..default()` (not assignments after
    // `default()`, which clippy's `field_reassign_with_default` flags).
    let mut outcome = OpOutcome {
        message: format!("{LOG}: ## {date}: {bullet}"),
        ..OpOutcome::default()
    };
    outcome.record(LOG, ReplayOp::Log { date, bullet });
    Ok(outcome)
}

/// What [`list_concept`] lists: `index`'s arguments once resolved.
pub(crate) struct Listing<'a> {
    /// Catalog-relative path of the concept.
    pub(crate) concept: &'a str,
    /// The link text, before escaping.
    pub(crate) title: &'a str,
    /// The summary after ` - `; whitespace collapses.
    pub(crate) summary: &'a str,
    /// The heading to list under, if any.
    pub(crate) section: Option<&'a str>,
    /// Count an older raw (unencoded) bullet as already listing the
    /// concept: `index` and `repair_indexes` do; creating a concept never
    /// does, so a new concept always gets its working, encoded listing.
    pub(crate) accept_raw: bool,
}

/// `cmd_index` after the concept is loaded: lists it in its folder's
/// `index.md` unless the index already links it, recording the replay op.
///
/// # Returns
/// `okf.py`'s message: `<folder>/index.md: <bullet>`, or
/// `<folder>/index.md: already lists <concept>`, or, when `index` changed
/// the summary of an existing listing, `<folder>/index.md: <bullet>`.
///
/// # Errors
/// [`OkfError::Refused`] without a folder `index.md`, or when `section`
/// names a missing heading; the workspace's read or write error.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) fn list_concept(
    workspace: &dyn Workspace,
    listing: &Listing<'_>,
    outcome: &mut OpOutcome,
) -> Result<String, OkfError> {
    let folder = dirname(listing.concept);
    let index_path = join(folder, "index.md");
    if !workspace.exists(&index_path) {
        return Err(refused(format!(
            "no index.md in /{folder}; create it first"
        )));
    }
    let text = workspace.read_text(&index_path)?;
    let listed = if listing.accept_raw {
        already_lists(workspace, &text, &index_path, listing.concept)
    } else {
        lists_target(workspace, &text, &index_path, listing.concept)
    };
    if listed {
        if listing.accept_raw {
            if let Some(outcome_message) =
                redescribe_listing(workspace, &text, &index_path, listing, outcome)?
            {
                return Ok(outcome_message);
            }
        }
        if !lists_target(workspace, &text, &index_path, listing.concept) {
            // Only an older raw bullet lists it: no link resolves, so say so
            // rather than append a second bullet beside it.
            outcome.notes.push(format!(
                "{index_path} lists /{} with a link that does not resolve; change its destination \
                 to {} (repair_indexes rewrites it)",
                listing.concept,
                encode_link_path(basename(listing.concept))
            ));
        }
        return Ok(format!("{index_path}: already lists {}", listing.concept));
    }
    require_link_back(workspace, &index_path, listing)?;
    let bullet = concept_bullet(listing);
    let updated = insert_index_bullet_text(&text, &bullet, listing.section, false)
        .ok_or_else(|| missing_heading(&index_path, listing.section.unwrap_or("")))?;
    workspace.write_atomic(&index_path, &updated)?;
    let message = format!("{index_path}: {bullet}");
    outcome.record(
        &index_path,
        ReplayOp::ListBullet {
            target: listing.concept.to_string(),
            bullet,
            section: listing.section.map(str::to_string),
            create_section: false,
        },
    );
    Ok(message)
}

/// `index` on an already-listed concept (issue #790): when the listing's
/// bullet has a working leading link and `listing.summary` differs from its
/// summary, sets it and records a [`ReplayOp::SetListingSummary`], so publish
/// redoes it on newer text instead of conflicting (as for a folder). The
/// label and link stay as they are.
///
/// # Returns
/// `Some(message)` when the summary was changed; `None` when nothing is to
/// change (summary empty or unchanged, or no bullet with a working leading
/// link: a raw one is left to `list_concept`'s note, which points at
/// `repair_indexes`).
///
/// # Errors
/// The workspace's write error.
#[tracing::instrument(level = "debug", skip_all)]
fn redescribe_listing(
    workspace: &dyn Workspace,
    text: &str,
    index_path: &str,
    listing: &Listing<'_>,
    outcome: &mut OpOutcome,
) -> Result<Option<String>, OkfError> {
    let summary = collapse_whitespace(listing.summary);
    if summary.is_empty() {
        return Ok(None);
    }
    let Some(updated) =
        set_listing_summary_text(workspace, text, index_path, listing.concept, &summary)
    else {
        return Ok(None);
    };
    if updated == text {
        return Ok(None);
    }
    workspace.write_atomic(index_path, &updated)?;
    let message = format!("{index_path}: {}", set_line(&updated, text));
    outcome.record(
        index_path,
        ReplayOp::SetListingSummary {
            target: listing.concept.to_string(),
            summary,
        },
    );
    Ok(Some(message))
}

/// The #432 round trip: refuses a listing whose bullet no later run would
/// read back as a listing of the concept (a label that cannot be written).
/// `list_concept` runs it before writing; `create_concept` runs it before
/// it writes anything.
///
/// # Arguments
/// * `workspace` - the catalog.
/// * `index_path` - the folder index the bullet would go in.
/// * `listing` - what would be listed.
///
/// # Errors
/// [`OkfError::Refused`] when the bullet does not link back.
#[tracing::instrument(level = "trace", skip_all)]
pub(crate) fn require_link_back(
    workspace: &dyn Workspace,
    index_path: &str,
    listing: &Listing<'_>,
) -> Result<(), OkfError> {
    if links_back(
        workspace,
        index_path,
        &concept_bullet(listing),
        listing.concept,
    ) {
        return Ok(());
    }
    Err(refused(format!(
        "no markdown link to /{} can be written in /{index_path} (its title or path cannot be \
         escaped); list it by editing the index",
        listing.concept
    )))
}

/// The bullet [`list_concept`] writes: `* [<title>](<file name>) - <summary>`,
/// with `[` and `]` in the title escaped and the summary's whitespace
/// collapsed.
#[tracing::instrument(level = "trace", skip_all)]
fn concept_bullet(listing: &Listing<'_>) -> String {
    let label = listing.title.replace('[', "\\[").replace(']', "\\]");
    format!(
        "* [{label}]({}) - {}",
        encode_link_path(basename(listing.concept)),
        collapse_whitespace(listing.summary)
    )
}

/// Port of `cmd_index`: lists a concept in its folder's `index.md`. A
/// folder reference instead sets that folder's listing summary in its
/// parent's index (issue #668, see `describe_folder`); `okf.py` has no
/// such form.
///
/// # Arguments
/// * `workspace` - the catalog; `_ctx` is unused (no timestamp).
/// * `concept` - the concept reference, or a folder: `/infra/`,
///   `/infra/index.md`, or `/infra` when no concept `/infra.md` exists.
/// * `summary` - the text after ` - `.
/// * `section` - the heading to list under; `None` for `# Concepts`.
/// * `title` - the link text; `None` (or empty) for the concept's `title`.
///
/// # Returns
/// The outcome, with a `ListBullet` replay op (a folder: a
/// `SetListingSummary`); nothing is touched when the index already links
/// the concept (or the folder's listing already has that summary).
///
/// # Errors
/// As [`crate::okf::ops::stamp`] when loading the concept;
/// [`OkfError::Refused`] without a title, without a folder `index.md`, or
/// for a missing `section` heading; for a folder, a `section` or `title`,
/// and `describe_folder`'s errors.
#[tracing::instrument(level = "debug", skip_all)]
pub fn index(
    workspace: &dyn Workspace,
    _ctx: &OpContext<'_>,
    concept: &str,
    summary: &str,
    section: Option<&str>,
    title: Option<&str>,
) -> Result<OpOutcome, OkfError> {
    let loaded = match folder_reference(concept) {
        Some(folder) => return index_folder(workspace, &folder?, summary, section, title),
        None => match load_concept(workspace, concept) {
            Ok(loaded) => loaded,
            // `/infra` names no concept but a folder: describe the folder.
            Err(OkfError::NotFound { .. })
                if workspace.exists(&join(&bare_reference(concept), INDEX)) =>
            {
                return index_folder(workspace, &bare_reference(concept), summary, section, title);
            }
            Err(error) => return Err(error),
        },
    };
    // `a.title or data.get("title")`: an empty string counts as absent. Every
    // YAML scalar is a `Value::Str`, so only a list or mapping is dropped here.
    let frontmatter_title = loaded.data.get("title").and_then(Value::as_str);
    let title = title
        .filter(|t| !t.is_empty())
        .or(frontmatter_title.filter(|t| !t.is_empty()))
        .ok_or_else(|| refused(format!("{} has no title; pass title", loaded.path)))?;
    let listing = Listing {
        concept: &loaded.path,
        title,
        summary,
        section,
        accept_raw: true,
    };
    let mut outcome = OpOutcome::default();
    let message = list_concept(workspace, &listing, &mut outcome)?;
    outcome.message = message;
    Ok(outcome)
}

/// `reference` without its leading `/`s and `.`/`..` components.
#[tracing::instrument(level = "trace", skip_all)]
fn bare_reference(reference: &str) -> String {
    let normal = normpath(reference.trim_start_matches('/'));
    if normal == "." {
        String::new()
    } else {
        normal
    }
}

/// The folder a reference names when it is written as one: ending in `/`,
/// or naming an `index.md` (`/` and `/index.md` are the root, `""`).
///
/// # Returns
/// `None` for any other reference (a concept's); `Some(Err)` with
/// [`OkfError::InvalidPath`] for a folder outside the catalog.
#[tracing::instrument(level = "trace", skip_all)]
fn folder_reference(reference: &str) -> Option<Result<String, OkfError>> {
    let bare = reference.trim_start_matches('/');
    let folder = if bare.is_empty() || bare.ends_with('/') {
        bare.trim_end_matches('/')
    } else if bare == INDEX {
        ""
    } else {
        bare.strip_suffix("/index.md")?
    };
    let folder = bare_reference(folder);
    if escapes_root(&folder) {
        return Some(Err(OkfError::InvalidPath {
            path: reference.to_string(),
            reason: "is outside the catalog".to_string(),
        }));
    }
    Some(Ok(folder))
}

/// `index` on a folder: `describe_folder`, refusing the concept-only
/// arguments.
#[tracing::instrument(level = "trace", skip_all)]
fn index_folder(
    workspace: &dyn Workspace,
    folder: &str,
    summary: &str,
    section: Option<&str>,
    title: Option<&str>,
) -> Result<OpOutcome, OkfError> {
    if section.is_some_and(|s| !s.is_empty()) || title.is_some_and(|t| !t.is_empty()) {
        return Err(refused(format!(
            "/{folder}/ is a folder: index sets its listing's summary only; leave out section \
             and title"
        )));
    }
    describe_folder(workspace, folder, summary)
}

/// Port of `ensure_index`: makes sure `folder/index.md` exists and is
/// listed in its parent's index, creating missing ancestor indexes too.
/// [`ensure_folder_index`] with no summary: every new folder is listed with
/// the TODO placeholder, as `okf.py` lists it.
///
/// # Arguments
/// * `workspace` - the catalog.
/// * `folder` - catalog-relative folder; `""` (the root) does nothing.
/// * `outcome` - gains each file written, its replay ops and notes.
///
/// # Errors
/// As [`ensure_folder_index`].
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "one-line delegation to an instrumented function"
    )
)]
pub fn ensure_index(
    workspace: &dyn Workspace,
    folder: &str,
    outcome: &mut OpOutcome,
) -> Result<(), OkfError> {
    ensure_folder_index(workspace, folder, None, outcome)
}

/// `ensure_index`, with an optional summary for `folder`'s own listing in
/// its parent (`folder_summary`, issue #668).
///
/// As in `okf.py`, the folder's own `index.md` is written first; then the
/// parent is ensured (recursively), and only after that is the folder listed
/// in it. So the notes come out outermost-first.
///
/// A new index is `# <Folder name>` and a TODO line; its parent gains
/// `* [<Folder name>](<name>/) - <summary>` under `# Sections` (created
/// when missing), with an absolute `/<folder>/` href when the parent's
/// bullets are absolute. The summary is `summary` with its whitespace
/// collapsed, for `folder` only (the innermost new folder); every ancestor
/// created on the way, and `folder` without a summary, gets
/// `TODO: describe this folder`. That default is `okf.py`'s; the summary is
/// a server-only divergence.
///
/// # Arguments
/// * `workspace` - the catalog.
/// * `folder` - catalog-relative folder; `""` (the root) does nothing.
/// * `summary` - the summary of `folder`'s listing, if it is created.
/// * `outcome` - gains each file written, its replay ops (`CreateIndex`,
///   then the parent's `ListBullet`) and one note per created index.
///
/// # Errors
/// The workspace's read or write error (a missing root `index.md` is
/// [`OkfError::NotFound`]). Files written before the error stay written,
/// so run it on a scratch copy (as the write path does) and drop the copy
/// on error.
#[tracing::instrument(level = "debug", skip_all)]
pub fn ensure_folder_index(
    workspace: &dyn Workspace,
    folder: &str,
    summary: Option<&str>,
    outcome: &mut OpOutcome,
) -> Result<(), OkfError> {
    if folder.is_empty() {
        return Ok(());
    }
    let index_path = join(folder, "index.md");
    if workspace.exists(&index_path) {
        return Ok(());
    }
    workspace.write_atomic(&index_path, &new_index_text(folder))?;
    outcome.record(
        &index_path,
        ReplayOp::CreateIndex {
            folder: folder.to_string(),
        },
    );
    let parent = dirname(folder);
    ensure_folder_index(workspace, parent, None, outcome)?;
    let parent_index = join(parent, INDEX);
    let parent_text = workspace.read_text(&parent_index)?;
    insert_folder_listing(workspace, folder, summary, &parent_text, outcome)?;
    outcome.notes.push(match summary {
        Some(_) => format!("created /{folder}/index.md; replace its TODO line"),
        None => format!("created /{folder}/index.md{FOLDER_NOTE}"),
    });
    Ok(())
}

/// Refuses a `folder_summary` that would describe no new folder: the
/// concept's folder is the root, or its `index.md` exists already. Checked
/// before an op writes anything.
///
/// # Arguments
/// * `workspace` - the catalog.
/// * `folder` - the folder `ensure_folder_index` will be called for.
///
/// # Errors
/// [`OkfError::Refused`], pointing at `index` for an existing folder.
#[tracing::instrument(level = "trace", skip_all)]
pub(crate) fn check_new_folder(workspace: &dyn Workspace, folder: &str) -> Result<(), OkfError> {
    if folder.is_empty() {
        return Err(refused(
            "folder_summary describes a new folder, and the root is no new folder; leave it out"
                .to_string(),
        ));
    }
    if workspace.exists(&join(folder, INDEX)) {
        return Err(refused(format!(
            "folder_summary describes a new folder, and /{folder}/ exists; leave it out, or \
             describe /{folder}/ with index (path /{folder}/, summary)"
        )));
    }
    Ok(())
}

/// `ensure_index`'s listing of `folder` in its parent's index:
/// `* [<Folder name>](<name>/) - <summary>` (the TODO placeholder without
/// one) under `# Sections` (created when missing), with an absolute
/// `/<folder>/` href when the parent's bullets are absolute. It does not
/// check whether the parent lists the folder already ([`repair_indexes`]
/// does, first).
///
/// # Arguments
/// * `workspace` - the catalog.
/// * `folder` - catalog-relative folder, not the root.
/// * `summary` - the listing's summary; `None` for the TODO placeholder.
/// * `parent_text` - the parent's `index.md`, as read.
/// * `outcome` - gains the parent's `ListBullet` replay op.
///
/// # Errors
/// The workspace's write error.
#[tracing::instrument(level = "trace", skip_all)]
fn insert_folder_listing(
    workspace: &dyn Workspace,
    folder: &str,
    summary: Option<&str>,
    parent_text: &str,
    outcome: &mut OpOutcome,
) -> Result<(), OkfError> {
    let parent_index = join(dirname(folder), INDEX);
    let bullet = folder_bullet(folder, summary, parent_text);
    let updated = insert_index_bullet_text(parent_text, &bullet, Some(SECTIONS), true)
        .expect("create_section always finds a place for the bullet");
    workspace.write_atomic(&parent_index, &updated)?;
    outcome.record(
        &parent_index,
        ReplayOp::ListBullet {
            target: join(folder, INDEX),
            bullet,
            section: Some(SECTIONS.to_string()),
            create_section: true,
        },
    );
    Ok(())
}

/// The bullet [`insert_folder_listing`] writes for `folder` in its parent's
/// index: `* [<Folder name>](<href>) - <summary>`, the summary collapsed (or
/// `TODO: describe this folder`), with the href `/<folder>/` when the
/// parent's bullets are absolute, else `<name>/`.
#[tracing::instrument(level = "trace", skip_all)]
fn folder_bullet(folder: &str, summary: Option<&str>, parent_text: &str) -> String {
    let absolute = parent_text
        .split('\n')
        .any(|line| line.starts_with("* [") && line.contains("](/"));
    let href = if absolute {
        format!("/{}/", encode_link_path(folder))
    } else {
        format!("{}/", encode_link_path(basename(folder)))
    };
    let summary = summary.map_or_else(|| FOLDER_TODO.to_string(), collapse_whitespace);
    format!("* [{}]({href}) - {summary}", folder_heading(folder))
}

/// `index` on a folder (issue #668): sets the summary of `folder`'s listing
/// in its parent's `index.md` (the text after ` - `), recording a
/// [`ReplayOp::SetListingSummary`] so publish redoes it on newer text
/// instead of conflicting. The label and link stay as they are.
///
/// # Arguments
/// * `workspace` - the catalog.
/// * `folder` - catalog-relative folder, not the root, e.g. `infra/hosts`.
/// * `summary` - the new summary; whitespace runs collapse to single spaces.
///
/// # Returns
/// The outcome: `<parent>/index.md: <bullet>`, or nothing touched when the
/// listing already has that summary.
///
/// # Errors
/// [`OkfError::Refused`] for the root, or when the parent's index does not
/// list the folder with a working leading link (a raw, unencoded one points
/// at `repair_indexes`, which rewrites it); [`OkfError::NotFound`] without
/// the folder's or its parent's `index.md`; the workspace's write error.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) fn describe_folder(
    workspace: &dyn Workspace,
    folder: &str,
    summary: &str,
) -> Result<OpOutcome, OkfError> {
    if folder.is_empty() {
        return Err(refused(
            "the root is listed in no index.md; describe the catalog by editing /index.md"
                .to_string(),
        ));
    }
    let target = join(folder, INDEX);
    if !workspace.exists(&target) {
        return Err(OkfError::NotFound {
            path: format!("/{target}"),
        });
    }
    let parent_index = join(dirname(folder), INDEX);
    let text = workspace.read_text(&parent_index)?;
    let summary = collapse_whitespace(summary);
    let Some(updated) =
        set_listing_summary_text(workspace, &text, &parent_index, &target, &summary)
    else {
        let how = if first_raw_listing(workspace, &text, &parent_index, &target).is_some() {
            "its listing's link does not resolve; run repair_indexes first"
        } else {
            "no bullet there starts with a link to it; repair_indexes lists it"
        };
        return Err(refused(format!(
            "/{parent_index} does not list /{folder}/: {how}"
        )));
    };
    if updated == text {
        return Ok(OpOutcome::unchanged(format!(
            "{parent_index}: /{folder}/ already has that summary"
        )));
    }
    workspace.write_atomic(&parent_index, &updated)?;
    let line = set_line(&updated, &text);
    let mut outcome = OpOutcome {
        message: format!("{parent_index}: {line}"),
        ..OpOutcome::default()
    };
    outcome.record(
        &parent_index,
        ReplayOp::SetListingSummary { target, summary },
    );
    Ok(outcome)
}

/// The line of `updated` that differs from `before` (the two have the same
/// number of lines: [`set_listing_summary_text`] rewrites one in place).
#[tracing::instrument(level = "trace", skip_all)]
fn set_line<'a>(updated: &'a str, before: &str) -> &'a str {
    updated
        .split('\n')
        .zip(before.split('\n'))
        .find(|(new, old)| new != old)
        .map_or("", |(new, _)| new)
}

/// Does `bullet`, written in `index`, link `target`? Not for every label: a
/// label ends at the first unescaped `]` (see [`crate::okf::markdown`]), so
/// a folder `a]` or a title ending in `\` cannot be listed this way. (Paths
/// are no obstacle: whitespace, `)` and `#` are written percent-encoded.)
/// The repair checks the exact
/// bullet it would write and skips one that no later run would recognise.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "one-line delegation to an instrumented function"
    )
)]
fn links_back(workspace: &dyn Workspace, index: &str, bullet: &str, target: &str) -> bool {
    lists_target(workspace, bullet, index, target)
}

/// The note for a listing [`repair_indexes`] cannot write.
#[tracing::instrument(level = "trace", skip_all)]
fn unlinkable_note(target: &str, index: &str) -> String {
    format!("did not list /{target} in /{index}: no markdown link to it can be written")
}

/// How many [`starter_files`] a new catalog holds.
pub const STARTER_FILE_COUNT: usize = 2;

/// The files a new catalog starts with: a root `index.md` and a `log.md`
/// with one `Creation` entry, so the bookkeeping tools (`log`, `index`,
/// `create_concept` with `index_summary`) work in it at once. They follow
/// `okf-catalog-template`'s skeleton, without its people and projects.
///
/// # Arguments
/// * `heading` - the root index's heading: the catalog's display name, or
///   `ROOT_HEADING` (the root index's default heading) when it has none.
/// * `description` - the catalog's description, the index's paragraph (a
///   TODO line when blank).
/// * `now` - the creation time; its UTC date heads the log entry.
///
/// # Returns
/// `(path, text)` pairs, sorted by path.
///
/// # Examples
/// ```
/// use chrono::{TimeZone, Utc};
/// use okf_mcp::okf::ops::starter_files;
/// let now = Utc.with_ymd_and_hms(2026, 11, 14, 12, 0, 0).unwrap();
/// let files = starter_files("Systems", "About systems.", now);
/// assert_eq!(files[0].0, "index.md");
/// assert!(files[0].1.contains("okf_version") && files[0].1.contains("# Systems\n\nAbout systems.\n"));
/// assert!(files[1].1.ends_with("## 2026-11-14\n\n* **Creation:** Created the catalog.\n"));
/// ```
#[tracing::instrument(level = "debug", skip_all)]
pub fn starter_files(
    heading: &str,
    description: &str,
    now: DateTime<Utc>,
) -> [(&'static str, String); STARTER_FILE_COUNT] {
    let date = now.format("%Y-%m-%d").to_string();
    let bullet = format!("* **{}:** Created the catalog.", LogKind::Creation.as_str());
    let log = log_text(LOG_TEMPLATE, &date, &bullet)
        .expect("an empty log has no entry under any date yet");
    [
        (ROOT_INDEX, root_index_text(heading, description)),
        (LOG, log),
    ]
}

/// Creates whatever bookkeeping a catalog is missing, never changing what
/// it has: the root `index.md` and `log.md`, an `index.md` for every folder
/// holding a concept or an index (and their ancestors), a listing of every
/// folder index in its parent's `# Sections`, and a listing of every concept
/// in its folder's index (where `index` puts it). Machinery is skipped,
/// since no write tool may change it.
///
/// Existing text is added to, with one exception: headings, prose, bullets
/// and summaries stay as they are, and a target an index already links
/// (however it is worded) is not listed again. The exception is a raw
/// (unencoded) concept or folder listing that versions before issue #453
/// wrote, such as `* [A](a b.md) - x` or, under `# Sections`,
/// `* [My docs](my docs/) - x`, whose link never resolves (issues #717 and
/// #731): its destination is percent-encoded in place
/// (`* [A](a%20b.md) - x`, label and summary kept), or the bullet is
/// removed when another bullet already links the concept or folder; each
/// gets a note quoting the bullet. Only a bullet
/// `lists_raw_target` accepts is touched, so a link that resolves to an
/// existing document, or a name holding a `%XX` escape, is left alone.
/// Running it twice changes nothing the second time. A file or folder no
/// markdown link can list (a label that cannot be written, such as a
/// folder named `a]`) is not listed; a note names it instead. Every edit is
/// a replay op ([`ReplayOp::CreateIndex`], [`ReplayOp::CreateLog`],
/// [`ReplayOp::ListBullet`], [`ReplayOp::RewriteRawListing`]), so publish
/// redoes it onto indexes that changed meanwhile instead of overwriting
/// them.
///
/// A listing takes the concept's frontmatter `title` (else its first `# `
/// heading, else its file name) and `description`; what is missing becomes
/// a TODO placeholder, and each one gets a note.
///
/// # Arguments
/// * `workspace` - the catalog.
///
/// # Returns
/// The outcome: every file written, its replay ops, a summary message, and
/// one note per created file and per placeholder.
///
/// # Errors
/// The workspace's listing, read or write error. Files written before the
/// error stay written, so run it on a scratch copy (as the write path does).
#[tracing::instrument(level = "debug", skip_all)]
pub fn repair_indexes(workspace: &dyn Workspace) -> Result<OpOutcome, OkfError> {
    let (mut outcome, counts) = repair_counted(workspace)?;
    outcome.message = repair_message(outcome.touched.len(), counts);
    Ok(outcome)
}

/// [`repair_indexes`]' fix of the raw (unencoded) bullets older versions
/// wrote for `target`, a concept (issue #717) or a folder's `index.md` in
/// its parent's `# Sections` (issue #731): each bullet
/// [`crate::okf::ops::replay::first_raw_listing`] finds (exactly
/// `lists_raw_target`'s rules, so a bullet whose parsed link resolves to an
/// existing document is never touched) is removed when another bullet
/// already links the target, else its destination is percent-encoded,
/// keeping the label and the text after it. Each fix is a
/// [`ReplayOp::RewriteRawListing`] and a note quoting both lines. A rewrite
/// that would not read back as a listing of the target is skipped with a
/// note (it cannot happen with an encoded destination; the check is the
/// same round trip every listing gets).
///
/// # Arguments
/// * `workspace` - the catalog.
/// * `text` - the index holding the listings, as read.
/// * `index_path` - its catalog-relative path.
/// * `target` - the catalog-relative path listed: a concept, or a
///   folder's `index.md`.
/// * `outcome` - gains the index's ops and notes.
///
/// # Returns
/// The index's text after the fixes, and how many bullets were fixed.
///
/// # Errors
/// The workspace's write error.
#[tracing::instrument(level = "trace", skip_all)]
fn rewrite_raw_listings(
    workspace: &dyn Workspace,
    text: &str,
    index_path: &str,
    target: &str,
    outcome: &mut OpOutcome,
) -> Result<(String, usize), OkfError> {
    // Notes name a folder as `/d/my docs/`, as the folder pass's other notes
    // do; a concept never ends in `/index.md`, so it is named as it is.
    let shown = target
        .strip_suffix("/index.md")
        .map_or_else(|| target.to_string(), |dir| format!("{dir}/"));
    let mut current = text.to_string();
    let mut fixed = 0_usize;
    // Each pass removes one raw bullet (a rewritten destination is encoded,
    // so it is never raw again), so the loop ends.
    while let Some(raw) = first_raw_listing(workspace, &current, index_path, target) {
        if !links_back(workspace, index_path, &raw.fixed, target) {
            outcome.notes.push(format!(
                "did not rewrite the raw listing of /{shown} in /{index_path}: {} \
                 would not link back",
                raw.fixed
            ));
            break;
        }
        let Some((updated, removed)) = rewrite_raw_listing_text(
            workspace, &current, index_path, target, &raw.line, &raw.fixed,
        ) else {
            // Unreachable: `first_raw_listing` took the line from `current`.
            break;
        };
        outcome.notes.push(if removed {
            format!(
                "removed the raw listing of /{shown} from /{index_path}, beside a working one: {}",
                raw.line
            )
        } else {
            format!(
                "rewrote the raw listing of /{shown} in /{index_path}: {} -> {}",
                raw.line, raw.fixed
            )
        });
        outcome.record(
            index_path,
            ReplayOp::RewriteRawListing {
                target: target.to_string(),
                line: raw.line,
                fixed: raw.fixed,
            },
        );
        current = updated;
        fixed += 1;
    }
    if fixed > 0 {
        workspace.write_atomic(index_path, &current)?;
    }
    Ok((current, fixed))
}

/// What [`repair_counted`] counted, for the summary message.
#[derive(Debug, Default, Clone, Copy)]
struct RepairCounts {
    /// TODO placeholders written.
    placeholders: usize,
    /// Raw listing bullets rewritten or removed (issue #717).
    rewritten: usize,
}

/// [`repair_indexes`]' edits, and what they counted; the outcome's message
/// is left empty for the caller.
#[tracing::instrument(level = "trace", skip_all)]
fn repair_counted(workspace: &dyn Workspace) -> Result<(OpOutcome, RepairCounts), OkfError> {
    let markdown = workspace.list_markdown()?;
    let mut outcome = OpOutcome::default();
    let mut counts = RepairCounts::default();
    if !workspace.exists(ROOT_INDEX) {
        workspace.write_atomic(ROOT_INDEX, &new_index_text(""))?;
        outcome.record(
            ROOT_INDEX,
            ReplayOp::CreateIndex {
                folder: String::new(),
            },
        );
        outcome
            .notes
            .push("created /index.md; replace its heading and TODO line".to_string());
        counts.placeholders += 1;
    }
    if !workspace.exists(LOG) {
        workspace.write_atomic(LOG, LOG_TEMPLATE)?;
        outcome.record(LOG, ReplayOp::CreateLog);
        outcome.notes.push("created /log.md".to_string());
    }
    let concepts: Vec<&str> = markdown
        .iter()
        .map(String::as_str)
        .filter(|path| is_concept(path) && !is_machinery(path))
        .collect();
    // Every folder below the root that holds a concept or an index, and
    // every ancestor of one (an existing nested index may have lost its
    // parent's), sorted: a `BTreeSet` is a sorted set, and a folder sorts
    // before anything inside it, so a parent's index exists before its
    // children are listed in it.
    let mut folders: BTreeSet<&str> = BTreeSet::new();
    for path in markdown.iter().map(String::as_str) {
        if is_machinery(path) || (basename(path) != INDEX && !is_concept(path)) {
            continue;
        }
        let mut folder = dirname(path);
        // `insert` returns false once a folder (and so all its ancestors)
        // is already in the set.
        while !folder.is_empty() && folders.insert(folder) {
            folder = dirname(folder);
        }
    }
    for folder in folders {
        let index_path = join(folder, INDEX);
        if !workspace.exists(&index_path) {
            // Not `ensure_index`: that lists the new index in its parent
            // unconditionally, and the parent may link the folder already.
            workspace.write_atomic(&index_path, &new_index_text(folder))?;
            outcome.record(
                &index_path,
                ReplayOp::CreateIndex {
                    folder: folder.to_string(),
                },
            );
            outcome
                .notes
                .push(format!("created /{index_path}; replace its TODO line"));
            counts.placeholders += 1;
        }
        let parent_index = join(dirname(folder), INDEX);
        let read = workspace.read_text(&parent_index)?;
        // A raw `# Sections` bullet older versions wrote for the folder is
        // fixed first, as a concept's is below (issue #731), so the check
        // after it sees a working listing and appends no second one.
        let (parent_text, fixed) =
            rewrite_raw_listings(workspace, &read, &parent_index, &index_path, &mut outcome)?;
        counts.rewritten += fixed;
        if !already_lists(workspace, &parent_text, &parent_index, &index_path) {
            let bullet = folder_bullet(folder, None, &parent_text);
            if !links_back(workspace, &parent_index, &bullet, &index_path) {
                outcome
                    .notes
                    .push(unlinkable_note(&format!("{folder}/"), &parent_index));
                continue;
            }
            insert_folder_listing(workspace, folder, None, &parent_text, &mut outcome)?;
            outcome.notes.push(format!(
                "listed /{folder}/ in /{parent_index} with a TODO summary; describe it with \
                 index on /{folder}/"
            ));
            counts.placeholders += 1;
        }
    }
    for concept in concepts {
        let index_path = join(dirname(concept), INDEX);
        let read = workspace.read_text(&index_path)?;
        let (text, fixed) =
            rewrite_raw_listings(workspace, &read, &index_path, concept, &mut outcome)?;
        counts.rewritten += fixed;
        if already_lists(workspace, &text, &index_path, concept) {
            continue;
        }
        let about = ConceptAbout::read(workspace, concept)?;
        let title = about
            .title
            .clone()
            .unwrap_or_else(|| folder_heading(concept.strip_suffix(".md").unwrap_or(concept)));
        let listing = Listing {
            concept,
            title: &title,
            summary: about.description.as_deref().unwrap_or(CONCEPT_TODO),
            section: None,
            accept_raw: true,
        };
        if !links_back(workspace, &index_path, &concept_bullet(&listing), concept) {
            outcome.notes.push(unlinkable_note(concept, &index_path));
            continue;
        }
        if about.title.is_none() {
            outcome.notes.push(format!(
                "listed /{concept} under its file name: it has no title"
            ));
            counts.placeholders += 1;
        }
        if about.description.is_none() {
            outcome.notes.push(format!(
                "listed /{concept} with a TODO summary: it has no description; describe it with \
                 index on /{concept}"
            ));
            counts.placeholders += 1;
        }
        list_concept(workspace, &listing, &mut outcome)?;
    }
    Ok((outcome, counts))
}

/// A repair's summary line.
#[tracing::instrument(level = "trace", skip_all)]
fn repair_message(written: usize, counts: RepairCounts) -> String {
    let RepairCounts {
        placeholders,
        rewritten,
    } = counts;
    if written == 0 {
        "indexes complete: nothing to repair".to_string()
    } else if rewritten == 0 {
        format!(
            "repaired {written} file(s); {placeholders} TODO placeholder(s) to fill in (see notes)"
        )
    } else {
        format!(
            "repaired {written} file(s); fixed {rewritten} raw listing(s); \
             {placeholders} TODO placeholder(s) to fill in (see notes)"
        )
    }
}

/// The operator's repair (`okf-mcp catalog repair-indexes`): as
/// [`repair_indexes`], except that a missing root `index.md` is the one a
/// new catalog gets (headed with the catalog's name, its description as the
/// first paragraph, see [`starter_files`]), and a repair that wrote anything
/// adds an `Update` entry to `log.md` saying so, since no agent is there to
/// log it.
///
/// The outcome is for a direct write, not a changeset: it records no replay
/// ops, because the edit is published at once under the catalog's lock.
///
/// # Arguments
/// * `workspace` - the catalog's live documents.
/// * `ctx` - its `now` dates the log entry.
/// * `name` - the catalog's name.
/// * `description` - the catalog's description.
///
/// # Returns
/// The files written (the root first), a summary message and one note per
/// created file and per TODO placeholder; nothing written when nothing is
/// missing.
///
/// # Errors
/// As [`repair_indexes`].
#[tracing::instrument(level = "debug", skip_all)]
pub fn repair_catalog(
    workspace: &dyn Workspace,
    ctx: &OpContext<'_>,
    name: &str,
    description: &str,
) -> Result<OpOutcome, OkfError> {
    let mut outcome = OpOutcome::default();
    let mut placeholders = 0_usize;
    if !workspace.exists(ROOT_INDEX) {
        workspace.write_atomic(ROOT_INDEX, &root_index_text(name, description))?;
        outcome.touch(ROOT_INDEX);
        if collapse_whitespace(description).is_empty() {
            outcome.notes.push(
                "created /index.md; the catalog has no description, so replace its TODO line"
                    .to_string(),
            );
            placeholders += 1;
        } else {
            outcome
                .notes
                .push("created /index.md from the catalog's name and description".to_string());
        }
    }
    let (repaired, mut counts) = repair_counted(workspace)?;
    counts.placeholders += placeholders;
    for path in &repaired.touched {
        outcome.touch(path);
    }
    outcome.notes.extend(repaired.notes);
    if !outcome.touched.is_empty() {
        let text = format!(
            "Repaired the catalog's indexes (okf-mcp catalog repair-indexes): wrote {}.",
            outcome.touched.join(", ")
        );
        log(workspace, ctx, LogKind::Update, &text)?;
        outcome.touch(LOG);
    }
    outcome.message = repair_message(outcome.touched.len(), counts);
    Ok(outcome)
}

/// What a listing says about a concept, from its text.
struct ConceptAbout {
    /// Frontmatter `title`, else the first `# ` heading; whitespace collapsed.
    title: Option<String>,
    /// Frontmatter `description`; whitespace collapsed.
    description: Option<String>,
}

impl ConceptAbout {
    /// Reads a concept's title and description. Frontmatter that does not
    /// parse counts as none: the listing gets placeholders instead of
    /// failing the whole repair.
    ///
    /// # Arguments
    /// * `workspace` - the catalog.
    /// * `concept` - its catalog-relative path.
    ///
    /// # Errors
    /// The workspace's read error.
    #[tracing::instrument(level = "trace", skip_all)]
    fn read(workspace: &dyn Workspace, concept: &str) -> Result<Self, OkfError> {
        let text = workspace.read_text(concept)?;
        let doc = Doc::from_text(concept, &text);
        // `.ok()` turns a parse error into `None`, like `except: data = None`.
        let data = if doc.has_frontmatter() {
            doc.parse().ok()
        } else {
            None
        };
        // Blank values count as missing, as `index`'s `title or ...` does.
        let field = |key: &str| {
            data.as_ref()
                .and_then(|data| data.get(key))
                .and_then(Value::as_str)
                .map(collapse_whitespace)
                .filter(|value| !value.is_empty())
        };
        let heading = || {
            doc.body()
                .split('\n')
                .find_map(|line| line.strip_prefix("# "))
                .map(collapse_whitespace)
                .filter(|heading| !heading.is_empty())
        };
        Ok(Self {
            title: field("title").or_else(heading),
            description: field("description"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::okf::workspace::MemWorkspace;
    use chrono::{TimeZone, Utc};

    const INFRA_INDEX: &str =
        "# Infra\n\n# Concepts\n\n* [Time server](time-server.md) - GPS clock\n";

    fn ctx() -> OpContext<'static> {
        OpContext {
            now: Utc.with_ymd_and_hms(2026, 10, 27, 12, 0, 0).unwrap(),
            by: "tester",
        }
    }

    fn refusal(error: OkfError) -> String {
        match error {
            OkfError::Refused { reason } => reason,
            other => panic!("expected Refused, got {other:?}"),
        }
    }

    fn infra() -> MemWorkspace {
        MemWorkspace::from_files([
            ("infra/index.md", INFRA_INDEX),
            (
                "infra/ntp.md",
                "---\ntype: Service\ntitle: NTP [pool]\n---\n",
            ),
            ("infra/untitled.md", "---\ntype: Note\n---\n"),
            ("people/x.md", "---\ntype: Person\ntitle: X\n---\n"),
        ])
    }

    #[test]
    fn log_writes_the_entry_and_records_its_replay() {
        let ws = MemWorkspace::from_files([("log.md", "# Log\n")]);

        let outcome = log(&ws, &ctx(), LogKind::Creation, "  Added   the\n pool. ").unwrap();

        assert_eq!(
            ws.read_to_string("log.md").unwrap(),
            "# Log\n\n## 2026-10-27\n\n* **Creation:** Added the pool.\n"
        );
        assert_eq!(outcome.touched, ["log.md"]);
        assert_eq!(
            outcome.replay["log.md"],
            [ReplayOp::Log {
                date: "2026-10-27".to_string(),
                bullet: "* **Creation:** Added the pool.".to_string(),
            }]
        );
        assert_eq!(
            outcome.message,
            "log.md: ## 2026-10-27: * **Creation:** Added the pool."
        );
    }

    #[test]
    fn log_reports_a_duplicate_and_needs_log_md() {
        let ws = MemWorkspace::from_files([("log.md", "# Log\n")]);
        log(&ws, &ctx(), LogKind::Update, "Once.").unwrap();

        let again = log(&ws, &ctx(), LogKind::Update, "Once.").unwrap();
        let missing = log(&MemWorkspace::default(), &ctx(), LogKind::Update, "x").unwrap_err();

        assert!(again.touched.is_empty());
        assert_eq!(again.message, "log.md: already logged under 2026-10-27");
        assert!(matches!(missing, OkfError::NotFound { .. }));
    }

    #[test]
    fn index_lists_a_concept_with_its_title_escaped() {
        let ws = infra();

        let outcome = index(&ws, &ctx(), "infra/ntp", "  the   pool ", None, None).unwrap();

        let bullet = "* [NTP \\[pool\\]](ntp.md) - the pool";
        assert_eq!(
            ws.read_to_string("infra/index.md").unwrap(),
            format!("{INFRA_INDEX}{bullet}\n")
        );
        assert_eq!(outcome.message, format!("infra/index.md: {bullet}"));
        assert_eq!(
            outcome.replay["infra/index.md"],
            [ReplayOp::ListBullet {
                target: "infra/ntp.md".to_string(),
                bullet: bullet.to_string(),
                section: None,
                create_section: false,
            }]
        );
    }

    #[test]
    fn index_of_a_listed_concept_with_the_same_summary_changes_nothing() {
        let ws = infra();
        index(&ws, &ctx(), "infra/ntp", "pool", None, None).unwrap();

        let again = index(&ws, &ctx(), "infra/ntp.md", " pool ", None, Some("T")).unwrap();
        let empty = index(&ws, &ctx(), "infra/ntp.md", "", None, None).unwrap();

        assert!(again.touched.is_empty() && empty.touched.is_empty());
        assert_eq!(again.message, "infra/index.md: already lists infra/ntp.md");
        assert_eq!(empty.message, again.message);
    }

    #[test]
    fn index_of_a_listed_concept_sets_its_summary_and_records_the_op() {
        let ws = infra();
        index(
            &ws,
            &ctx(),
            "infra/ntp",
            "TODO: describe this concept",
            None,
            None,
        )
        .unwrap();

        let again = index(&ws, &ctx(), "infra/ntp.md", "  the  pool ", None, Some("T")).unwrap();

        let bullet = "* [NTP \\[pool\\]](ntp.md) - the pool";
        assert_eq!(
            ws.read_to_string("infra/index.md").unwrap(),
            format!("{INFRA_INDEX}{bullet}\n")
        );
        assert_eq!(again.message, format!("infra/index.md: {bullet}"));
        assert_eq!(
            again.replay["infra/index.md"],
            [ReplayOp::SetListingSummary {
                target: "infra/ntp.md".to_string(),
                summary: "the pool".to_string(),
            }]
        );
    }

    /// File names the issue (#453) names, one per character class.
    const AWKWARD_NAMES: [&str; 6] = [
        "a b.md",
        "a).md",
        "a#b.md",
        "100%.md",
        "a\u{a0}b.md",
        "a:b.md",
    ];

    fn awkward_ws(name: &str, index: &str) -> MemWorkspace {
        let ws = MemWorkspace::default();
        ws.write_atomic("d/index.md", index).unwrap();
        ws.write_atomic(
            &format!("d/{name}"),
            "---\ntype: Note\ntitle: Odd\ndescription: An odd one.\n---\n",
        )
        .unwrap();
        ws
    }

    /// Every link in `text` resolves to a file that exists.
    fn no_broken_link(ws: &MemWorkspace, text: &str) -> bool {
        let links = crate::okf::markdown::find_links(text);
        !links.is_empty()
            && links.iter().all(|l| {
                crate::okf::markdown::resolve_link(ws, &l.target, "d/index.md")
                    .is_none_or(|r| ws.exists(&r.path))
            })
    }

    #[test]
    fn index_links_awkward_names_and_does_not_duplicate() {
        for name in AWKWARD_NAMES {
            let ws = awkward_ws(name, "# D\n\n# Concepts\n");
            let concept = format!("d/{name}");

            let first = index(&ws, &ctx(), &concept, "odd", None, None).unwrap();
            let after_first = ws.read_to_string("d/index.md").unwrap();
            let second = index(&ws, &ctx(), &concept, "odd", None, None).unwrap();

            assert!(no_broken_link(&ws, &after_first), "{name}: {after_first}");
            assert!(
                lists_target(&ws, &after_first, "d/index.md", &concept),
                "{name}"
            );
            assert!(second.touched.is_empty(), "{name}: duplicated");
            assert_eq!(
                ws.read_to_string("d/index.md").unwrap(),
                after_first,
                "{name}"
            );
            assert!(!first.touched.is_empty(), "{name}");
        }
    }

    #[test]
    fn index_does_not_duplicate_an_old_raw_bullet() {
        // `index` recognises it and leaves it alone, with a note;
        // `repair_indexes` rewrites it in place (issue #717).
        // Written before #632: the raw name, which no parser reads back as
        // the concept for these names.
        for name in AWKWARD_NAMES {
            let raw = format!("# D\n\n# Concepts\n\n* [Odd]({name}) - An odd one.\n");
            let ws = awkward_ws(name, &raw);
            let concept = format!("d/{name}");

            // The summary it already has: an ordinary listing (`100%.md`)
            // would otherwise be re-described (issue #790).
            let outcome = index(&ws, &ctx(), &concept, "An odd one.", None, None).unwrap();

            assert_eq!(ws.read_to_string("d/index.md").unwrap(), raw, "{name}");
            assert!(outcome.touched.is_empty(), "{name}");
            // `100%.md` still resolves as written (a bare `%.` is no escape),
            // so it is an ordinary listing; the rest are unresolvable.
            let want = usize::from(name != "100%.md");
            assert_eq!(outcome.notes.len(), want, "{name}: {:?}", outcome.notes);
            // Repair never appends a second bullet: it encodes the raw
            // one's destination (`100%.md` is an ordinary listing, kept).
            repair_indexes(&ws).unwrap();
            let want = if name == "100%.md" {
                raw.clone()
            } else {
                format!(
                    "# D\n\n# Concepts\n\n* [Odd]({}) - An odd one.\n",
                    encode_link_path(name)
                )
            };
            assert_eq!(ws.read_to_string("d/index.md").unwrap(), want, "{name}");
        }
    }

    /// The four raw forms the issue (#717) names, one per character class.
    const RAW_FORMS: [&str; 4] = ["a b.md", "a).md", "a#b.md", "a:b.md"];

    /// [`awkward_ws`] plus the root `index.md` and `log.md`, so a repair of
    /// it touches only `d/index.md`.
    fn repairable_ws(name: &str, index: &str) -> MemWorkspace {
        let ws = awkward_ws(name, index);
        ws.write_atomic("index.md", "# Root\n\n# Sections\n\n* [D](d/) - d\n")
            .unwrap();
        ws.write_atomic("log.md", "# Log\n").unwrap();
        ws
    }

    #[test]
    fn repair_indexes_rewrites_each_raw_form_in_place() {
        // Relative and absolute, with and without a fragment and a summary:
        // only the destination changes, and a second run changes nothing.
        for name in RAW_FORMS {
            let encoded = encode_link_path(name);
            let mut cases = vec![
                (
                    format!("* [Odd]({name}) - An odd one."),
                    format!("* [Odd]({encoded}) - An odd one."),
                ),
                (
                    format!("- [O\\]dd]({name}#part) and more"),
                    format!("- [O\\]dd]({encoded}#part) and more"),
                ),
            ];
            // `/d/a:b.md` is no URI (a scheme cannot start with `/`): it is
            // a working link, so it is not a raw listing (see below).
            if name != "a:b.md" {
                cases.push((
                    format!("* [Odd](/d/{name})"),
                    format!("* [Odd](/d/{encoded})"),
                ));
            }
            for (raw, fixed) in cases {
                let ws = repairable_ws(
                    name,
                    &format!("# D\n\nAbout d.\n\n# Concepts\n\n{raw}\n* [X](x.md) - x\n"),
                );

                let outcome = repair_indexes(&ws).unwrap();

                let got = ws.read_to_string("d/index.md").unwrap();
                assert_eq!(
                    got,
                    format!("# D\n\nAbout d.\n\n# Concepts\n\n{fixed}\n* [X](x.md) - x\n"),
                    "{raw}"
                );
                assert!(
                    lists_target(&ws, &got, "d/index.md", &format!("d/{name}")),
                    "{raw}"
                );
                assert_eq!(outcome.touched, ["d/index.md"], "{raw}");
                assert_eq!(
                    outcome.replay["d/index.md"],
                    [ReplayOp::RewriteRawListing {
                        target: format!("d/{name}"),
                        line: raw.clone(),
                        fixed: fixed.clone(),
                    }],
                    "{raw}"
                );
                assert_eq!(
                    outcome.notes,
                    [format!(
                        "rewrote the raw listing of /d/{name} in /d/index.md: {raw} -> {fixed}"
                    )],
                    "{raw}"
                );
                assert!(
                    outcome.message.contains("fixed 1 raw listing(s)"),
                    "{}",
                    outcome.message
                );
                let again = repair_indexes(&ws).unwrap();
                assert!(
                    again.touched.is_empty() && again.notes.is_empty(),
                    "{raw}: {again:?}"
                );
                assert_eq!(ws.read_to_string("d/index.md").unwrap(), got, "{raw}");
            }
        }
    }

    #[test]
    fn repair_indexes_keeps_an_absolute_colon_link_that_already_resolves() {
        let index = "# D\n\n# Concepts\n\n* [Odd](/d/a:b.md)\n";
        let ws = repairable_ws("a:b.md", index);

        let outcome = repair_indexes(&ws).unwrap();

        assert!(outcome.touched.is_empty(), "{outcome:?}");
        assert_eq!(ws.read_to_string("d/index.md").unwrap(), index);
    }

    #[test]
    fn repair_indexes_removes_a_raw_duplicate_beside_a_working_bullet() {
        // Either order, and a second raw copy: one working bullet is left.
        for name in RAW_FORMS {
            let encoded = encode_link_path(name);
            let raw = format!("* [Odd]({name}) - old");
            let working = format!("* [Odd]({encoded}) - new");
            for lines in [
                vec![raw.clone(), working.clone()],
                vec![working.clone(), raw.clone()],
                vec![raw.clone(), working.clone(), raw.clone()],
            ] {
                let ws = repairable_ws(
                    name,
                    &format!("# D\n\n# Concepts\n\n{}\n", lines.join("\n")),
                );

                let outcome = repair_indexes(&ws).unwrap();

                let got = ws.read_to_string("d/index.md").unwrap();
                assert_eq!(
                    got,
                    format!("# D\n\n# Concepts\n\n{working}\n"),
                    "{lines:?}"
                );
                let removals = lines.len() - 1;
                assert_eq!(outcome.replay["d/index.md"].len(), removals, "{lines:?}");
                assert_eq!(
                    outcome.notes,
                    vec![
                        format!("removed the raw listing of /d/{name} from /d/index.md, beside a working one: {raw}");
                        removals
                    ],
                    "{lines:?}"
                );
                assert!(repair_indexes(&ws).unwrap().touched.is_empty(), "{lines:?}");
            }
        }
    }

    /// The raw folder names older versions wrote unencoded under
    /// `# Sections` (issue #731): one per character that ends or changes a
    /// destination.
    const RAW_FOLDERS: [&str; 4] = ["a b", "a)b", "a#b", "a:b"];

    /// A catalog whose `d/index.md` holds `sections` under `# Sections`, with
    /// each folder in `folders` inside `d/` complete (its index lists its one
    /// concept), so a repair touches only `d/index.md`.
    fn folder_ws(folders: &[&str], sections: &str) -> MemWorkspace {
        let ws = MemWorkspace::default();
        ws.write_atomic("index.md", "# Root\n\n# Sections\n\n* [D](d/) - d\n")
            .unwrap();
        ws.write_atomic("log.md", "# Log\n").unwrap();
        ws.write_atomic(
            "d/index.md",
            &format!("# D\n\nAbout d.\n\n# Sections\n\n{sections}\n"),
        )
        .unwrap();
        for folder in folders {
            ws.write_atomic(&format!("d/{folder}/index.md"), "# N\n\n* [C](c.md) - c\n")
                .unwrap();
            ws.write_atomic(
                &format!("d/{folder}/c.md"),
                "---\ntype: Note\ntitle: C\ndescription: c\n---\n",
            )
            .unwrap();
        }
        ws
    }

    /// A `d/` with the concepts `names` (each with its own title) and the
    /// given concept bullets in `d/index.md`, plus root `index.md`/`log.md`.
    fn two_name_ws(names: &[(&str, &str)], bullets: &str) -> MemWorkspace {
        let ws = repairable_ws(names[0].0, &format!("# D\n\n# Concepts\n\n{bullets}\n"));
        for (name, title) in names {
            ws.write_atomic(
                &format!("d/{name}"),
                &format!("---\ntype: Note\ntitle: {title}\ndescription: {title}.\n---\n"),
            )
            .unwrap();
        }
        ws
    }

    #[test]
    fn a_raw_bullet_for_a_longer_name_is_not_taken_for_the_shorter_one() {
        // Issue #734: `a b.md` ends the prefix at `#` (or `)`) inside the
        // longer name, so the long bullet must never be rewritten to the
        // short concept, whichever target is looked at first.
        for (short, long) in [
            ("a b.md", "a b.md#extra.md"),
            ("a b.md", "a b.md)x.md"),
            ("a b.md", "a b.md)x.md#frag.md"),
        ] {
            let long_bullet = format!("* [Long]({long}) - long summary");
            let short_bullet = format!("* [Short]({short}) - short summary");
            let long_fixed = format!("* [Long]({}) - long summary", encode_link_path(long));
            let short_fixed = format!("* [Short]({}) - short summary", encode_link_path(short));
            let ws = two_name_ws(
                &[(short, "Short"), (long, "Long")],
                &format!("{long_bullet}\n{short_bullet}"),
            );
            let text = ws.read_to_string("d/index.md").unwrap();
            let (short_path, long_path) = (format!("d/{short}"), format!("d/{long}"));

            // Each target finds only its own bullet, in either order.
            let found = |target: &str| first_raw_listing(&ws, &text, "d/index.md", target);
            assert_eq!(found(&short_path).unwrap().line, short_bullet, "{long}");
            assert_eq!(found(&short_path).unwrap().fixed, short_fixed, "{long}");
            assert_eq!(found(&long_path).unwrap().line, long_bullet, "{long}");
            assert_eq!(found(&long_path).unwrap().fixed, long_fixed, "{long}");

            // With only the long bullet, the short target finds nothing and
            // the long one still does.
            let only_long = format!("# D\n\n# Concepts\n\n{long_bullet}\n");
            assert!(first_raw_listing(&ws, &only_long, "d/index.md", &short_path).is_none());
            assert!(first_raw_listing(&ws, &only_long, "d/index.md", &long_path).is_some());
            assert!(!crate::okf::ops::replay::lists_raw_target(
                &ws,
                &only_long,
                "d/index.md",
                &short_path
            ));

            let outcome = repair_indexes(&ws).unwrap();
            assert_eq!(
                ws.read_to_string("d/index.md").unwrap(),
                format!("# D\n\n# Concepts\n\n{long_fixed}\n{short_fixed}\n"),
                "{long}: {:?}",
                outcome.notes
            );
        }
    }

    #[test]
    fn a_longer_name_with_an_escape_still_keeps_its_bullet_from_the_shorter_target() {
        // `a b.md#%41.md` is no raw name of its own document (rule 2), yet
        // its bullet must not be taken for `a b.md`'s.
        let bullet = "* [Long](a b.md#%41.md) - long";
        let ws = two_name_ws(&[("a b.md", "Short"), ("a b.md#%41.md", "Long")], bullet);
        let text = ws.read_to_string("d/index.md").unwrap();
        assert!(first_raw_listing(&ws, &text, "d/index.md", "d/a b.md").is_none());
        repair_indexes(&ws).unwrap();
        assert!(
            !ws.read_to_string("d/index.md")
                .unwrap()
                .contains("(a%20b.md#%41.md)"),
            "redirected to the shorter concept"
        );
    }

    #[test]
    fn adding_the_short_concept_is_not_suppressed_by_the_long_raw_bullet() {
        let ws = two_name_ws(
            &[("a b.md", "Short"), ("a b.md#extra.md", "Long")],
            "* [Long](a b.md#extra.md) - long summary",
        );
        index(&ws, &ctx(), "d/a b.md", "short", None, None).unwrap();
        let text = ws.read_to_string("d/index.md").unwrap();
        assert!(
            text.contains("* [Long](a b.md#extra.md) - long summary"),
            "{text}"
        );
        assert!(text.contains("(a%20b.md)"), "{text}");
    }

    #[test]
    fn a_raw_bullet_for_a_longer_name_is_still_rewritten_when_it_is_alone() {
        // No longer document exists: `a b.md#extra.md` is the short
        // concept's bullet with a fragment, and it is rewritten as before.
        let ws = two_name_ws(&[("a b.md", "Short")], "* [Long](a b.md#extra.md) - s");
        repair_indexes(&ws).unwrap();
        assert_eq!(
            ws.read_to_string("d/index.md").unwrap(),
            "# D\n\n# Concepts\n\n* [Long](a%20b.md#extra.md) - s\n"
        );
    }

    #[test]
    fn a_raw_folder_bullet_for_a_longer_concept_name_is_left_alone() {
        // The folder pass shares the code: `a b/index.md#x.md` (or `)x.md`)
        // is a concept of the folder `a b`, whose own listing
        // `a b/index.md` ends at `#` (or `)`). The folder target skips the
        // bullet; no pass rewrites it (the concept's pass reads only its
        // own folder's index), so it stays as written, never misdirected (the folder is listed anew).
        for long in ["index.md#x.md", "index.md)x.md"] {
            let bullet = format!("* [Long](a b/{long}) - long");
            let ws = folder_ws(&["a b"], &bullet);
            ws.write_atomic(
                &format!("d/a b/{long}"),
                "---\ntype: Note\ntitle: Long\ndescription: long.\n---\n",
            )
            .unwrap();
            let before = ws.read_to_string("d/index.md").unwrap();

            assert!(first_raw_listing(&ws, &before, "d/index.md", "d/a b/index.md").is_none());
            repair_indexes(&ws).unwrap();
            // The long bullet is byte for byte as written; the folder, not
            // listed by it, gets its own encoded listing appended.
            let after = ws.read_to_string("d/index.md").unwrap();
            assert!(after.contains(&format!("{bullet}\n")), "{long}: {after}");
            assert!(after.contains("(a%20b/)"), "{long}: {after}");
            assert!(!after.contains("a%20b/index.md"), "{long}: {after}");
        }
    }

    #[test]
    fn repair_indexes_rewrites_each_raw_folder_form_in_place() {
        // The folder link old versions wrote (`name/`), the index file
        // itself (`name/index.md`), each relative and `/`-absolute, with a
        // fragment and trailing text: only the destination changes, nothing
        // is appended, and a second run changes nothing.
        for name in RAW_FOLDERS {
            let encoded = encode_link_path(name);
            let mut cases = vec![
                (
                    format!("* [Odd]({name}/) - TODO: describe this folder"),
                    format!("* [Odd]({encoded}/) - TODO: describe this folder"),
                ),
                (
                    format!("- [O\\]dd]({name}/index.md#part) and more"),
                    format!("- [O\\]dd]({encoded}/index.md#part) and more"),
                ),
            ];
            // `/d/a:b/` is no URI (a scheme cannot start with `/`): it is a
            // working link, so it is not a raw listing (see below).
            if name != "a:b" {
                cases.push((
                    format!("* [Odd](/d/{name}/) - x"),
                    format!("* [Odd](/d/{encoded}/) - x"),
                ));
                cases.push((
                    format!("* [Odd](/d/{name}/index.md)"),
                    format!("* [Odd](/d/{encoded}/index.md)"),
                ));
            }
            for (raw, fixed) in cases {
                let ws = folder_ws(&[name], &format!("{raw}\n* [X](x/) - x"));
                let target = format!("d/{name}/index.md");

                let outcome = repair_indexes(&ws).unwrap();

                let got = ws.read_to_string("d/index.md").unwrap();
                assert_eq!(
                    got,
                    format!("# D\n\nAbout d.\n\n# Sections\n\n{fixed}\n* [X](x/) - x\n"),
                    "{raw}"
                );
                assert!(lists_target(&ws, &got, "d/index.md", &target), "{raw}");
                assert_eq!(outcome.touched, ["d/index.md"], "{raw}");
                assert_eq!(
                    outcome.replay["d/index.md"],
                    [ReplayOp::RewriteRawListing {
                        target,
                        line: raw.clone(),
                        fixed: fixed.clone(),
                    }],
                    "{raw}"
                );
                assert_eq!(
                    outcome.notes,
                    [format!(
                        "rewrote the raw listing of /d/{name}/ in /d/index.md: {raw} -> {fixed}"
                    )],
                    "{raw}"
                );
                assert!(
                    outcome.message.contains("fixed 1 raw listing(s)"),
                    "{}",
                    outcome.message
                );
                let again = repair_indexes(&ws).unwrap();
                assert!(
                    again.touched.is_empty() && again.notes.is_empty(),
                    "{raw}: {again:?}"
                );
                assert_eq!(ws.read_to_string("d/index.md").unwrap(), got, "{raw}");
            }
        }
    }

    #[test]
    fn repair_indexes_rewrites_a_raw_folder_bullet_in_the_root_index() {
        // A top-level folder: its listing sits in the root `index.md`.
        let ws = MemWorkspace::default();
        ws.write_atomic(
            "index.md",
            "# Root\n\n# Sections\n\n* [My docs](my docs/) - mine\n",
        )
        .unwrap();
        ws.write_atomic("log.md", "# Log\n").unwrap();
        ws.write_atomic("my docs/index.md", "# My docs\n\n* [C](c.md) - c\n")
            .unwrap();
        ws.write_atomic(
            "my docs/c.md",
            "---\ntype: Note\ntitle: C\ndescription: c\n---\n",
        )
        .unwrap();

        let outcome = repair_indexes(&ws).unwrap();

        assert_eq!(
            ws.read_to_string("index.md").unwrap(),
            "# Root\n\n# Sections\n\n* [My docs](my%20docs/) - mine\n"
        );
        assert_eq!(outcome.touched, ["index.md"], "{outcome:?}");
        assert!(repair_indexes(&ws).unwrap().touched.is_empty());
    }

    #[test]
    fn repair_indexes_keeps_an_absolute_colon_folder_link_that_already_resolves() {
        let sections = "* [Odd](/d/a:b/) - x";
        let ws = folder_ws(&["a:b"], sections);

        let outcome = repair_indexes(&ws).unwrap();

        assert!(outcome.touched.is_empty(), "{outcome:?}");
        assert_eq!(
            ws.read_to_string("d/index.md").unwrap(),
            format!("# D\n\nAbout d.\n\n# Sections\n\n{sections}\n")
        );
    }

    #[test]
    fn repair_indexes_removes_a_raw_folder_duplicate_beside_a_working_bullet() {
        // Either order, a second raw copy, and a working bullet in each form
        // the parser reads as the folder: one working bullet is left, never
        // a second one appended.
        for name in RAW_FOLDERS {
            let encoded = encode_link_path(name);
            let raw = format!("* [Odd]({name}/) - old");
            for working in [
                format!("* [Odd]({encoded}/) - new"),
                format!("* [Odd]({encoded}/index.md) - new"),
                format!("* [Odd]({encoded}) - new"),
            ] {
                for lines in [
                    vec![raw.clone(), working.clone()],
                    vec![working.clone(), raw.clone()],
                    vec![raw.clone(), working.clone(), raw.clone()],
                ] {
                    let ws = folder_ws(&[name], &lines.join("\n"));

                    let outcome = repair_indexes(&ws).unwrap();

                    let got = ws.read_to_string("d/index.md").unwrap();
                    assert_eq!(
                        got,
                        format!("# D\n\nAbout d.\n\n# Sections\n\n{working}\n"),
                        "{lines:?}"
                    );
                    let removals = lines.len() - 1;
                    assert_eq!(outcome.replay["d/index.md"].len(), removals, "{lines:?}");
                    assert_eq!(
                        outcome.notes,
                        vec![
                            format!(
                                "removed the raw listing of /d/{name}/ from /d/index.md, \
                                 beside a working one: {raw}"
                            );
                            removals
                        ],
                        "{lines:?}"
                    );
                    assert!(repair_indexes(&ws).unwrap().touched.is_empty(), "{lines:?}");
                }
            }
        }
    }

    #[test]
    fn repair_indexes_leaves_a_raw_folder_name_with_a_percent_escape_alone() {
        // `x%41 y/` decodes to `xA y/`: its raw form cannot be told from an
        // encoded name, so the bullet is not a raw listing (rule 2). The
        // folder gets its own, encoded bullet and the old line stays.
        let name = "x%41 y";
        let raw = format!("* [Odd]({name}/) - old");
        let ws = folder_ws(&[name], &raw);

        let outcome = repair_indexes(&ws).unwrap();

        let got = ws.read_to_string("d/index.md").unwrap();
        assert!(got.contains(&format!("{raw}\n")), "{got}");
        assert!(got.contains("](x%2541%20y/)"), "{got}");
        assert!(!outcome.replay["d/index.md"]
            .iter()
            .any(|op| matches!(op, ReplayOp::RewriteRawListing { .. })));
        assert!(repair_indexes(&ws).unwrap().touched.is_empty());
    }

    #[test]
    fn repair_indexes_leaves_a_folder_link_to_another_existing_document_alone() {
        // With a folder `d/a` beside `d/a)b` and `d/a#b`, `[A](a)b/)` and
        // `[A](a#b/)` are working links to `d/a/index.md` (rule 3): never
        // rewritten. Each other folder gets its own encoded bullet, and
        // `d/a` is not listed again.
        for name in ["a)b", "a#b"] {
            let line = format!("* [A]({name}/) - a");
            let ws = folder_ws(&["a", name], &line);

            let outcome = repair_indexes(&ws).unwrap();

            let got = ws.read_to_string("d/index.md").unwrap();
            assert!(
                got.starts_with(&format!("# D\n\nAbout d.\n\n# Sections\n\n{line}\n")),
                "{got}"
            );
            assert!(
                got.contains(&format!("]({}/)", encode_link_path(name))),
                "{got}"
            );
            assert_eq!(got.matches("](a/)").count(), 0, "{got}");
            assert!(
                outcome.replay["d/index.md"]
                    .iter()
                    .all(|op| matches!(op, ReplayOp::ListBullet { .. })),
                "{name}: {outcome:?}"
            );
            assert!(repair_indexes(&ws).unwrap().touched.is_empty(), "{name}");
        }
    }

    #[test]
    fn repair_indexes_rewrites_the_first_of_two_raw_bullets_and_removes_the_second() {
        let ws = repairable_ws(
            "a b.md",
            "# D\n\n* [One](a b.md) - 1\n* [Two](a b.md) - 2\n",
        );

        let outcome = repair_indexes(&ws).unwrap();

        assert_eq!(
            ws.read_to_string("d/index.md").unwrap(),
            "# D\n\n* [One](a%20b.md) - 1\n"
        );
        assert_eq!(outcome.notes.len(), 2, "{:?}", outcome.notes);
        assert!(
            outcome.message.contains("fixed 2 raw listing(s)"),
            "{}",
            outcome.message
        );
    }

    #[test]
    fn repair_indexes_leaves_a_raw_name_with_a_percent_escape_alone() {
        // `x%41 y.md` decodes to `xA y.md`: its raw form cannot be told from
        // an encoded name, so the bullet is not a raw listing (rule 2). The
        // concept gets its own, encoded bullet and the old line stays.
        let name = "x%41 y.md";
        let raw = format!("* [Odd]({name}) - old");
        let ws = repairable_ws(name, &format!("# D\n\n# Concepts\n\n{raw}\n"));

        let outcome = repair_indexes(&ws).unwrap();

        let got = ws.read_to_string("d/index.md").unwrap();
        assert!(got.contains(&format!("{raw}\n")), "{got}");
        assert!(got.contains("](x%2541%20y.md)"), "{got}");
        assert!(!outcome.replay["d/index.md"]
            .iter()
            .any(|op| matches!(op, ReplayOp::RewriteRawListing { .. })));
    }

    #[test]
    fn repair_indexes_leaves_a_link_to_another_existing_document_alone() {
        // `[A](a.md).md)` and `[A](a.md#b.md)` are working links to `d/a.md`
        // (rule 3): never rewritten, even though `d/a.md).md` exists too.
        for name in ["a.md).md", "a.md#b.md"] {
            let line = format!("* [A]({name}) - a");
            let ws = repairable_ws("a.md", &format!("# D\n\n# Concepts\n\n{line}\n"));
            let text = "---\ntype: Note\ntitle: B\ndescription: b\n---\n";
            ws.write_atomic(&format!("d/{name}"), text).unwrap();

            let outcome = repair_indexes(&ws).unwrap();

            let got = ws.read_to_string("d/index.md").unwrap();
            assert!(
                got.starts_with(&format!("# D\n\n# Concepts\n\n{line}\n")),
                "{got}"
            );
            assert!(
                got.contains(&format!("]({})", encode_link_path(name))),
                "{got}"
            );
            assert!(
                outcome.replay["d/index.md"]
                    .iter()
                    .all(|op| matches!(op, ReplayOp::ListBullet { .. })),
                "{name}"
            );
        }
    }

    #[test]
    fn repair_indexes_writes_the_bullet_index_writes() {
        for name in AWKWARD_NAMES {
            let by_index = awkward_ws(name, "# D\n\n# Concepts\n");
            let concept = format!("d/{name}");
            index(&by_index, &ctx(), &concept, "An odd one.", None, None).unwrap();
            let by_repair = awkward_ws(name, "# D\n\n# Concepts\n");
            by_repair.write_atomic("index.md", "# Root\n").unwrap();
            by_repair.write_atomic("log.md", "# Log\n").unwrap();

            repair_indexes(&by_repair).unwrap();

            let expected = by_index.read_to_string("d/index.md").unwrap();
            let got = by_repair.read_to_string("d/index.md").unwrap();
            assert!(
                got.contains(expected.lines().last().unwrap()),
                "{name}: {got}"
            );
            assert!(no_broken_link(&by_repair, &got), "{name}: {got}");
            let again = repair_indexes(&by_repair).unwrap();
            assert!(!again.touched.contains(&"d/index.md".to_string()), "{name}");
        }
    }

    #[test]
    fn index_refuses_a_bullet_that_would_not_link_back() {
        // A title ending in `\` swallows the label's closing `]`: the
        // documented unrepresentable-label case beside `links_back`.
        let ws = awkward_ws("plain.md", "# D\n\n# Concepts\n");

        let error = index(&ws, &ctx(), "d/plain", "x", None, Some("Odd\\")).unwrap_err();

        let reason = refusal(error);
        assert!(
            reason.contains("no markdown link to /d/plain.md can be written"),
            "{reason}"
        );
        assert_eq!(
            ws.read_to_string("d/index.md").unwrap(),
            "# D\n\n# Concepts\n"
        );
    }

    #[test]
    fn replay_adds_the_working_bullet_beside_a_raw_one() {
        // Replay cannot tell which files exist, so a raw match is never
        // "already listed": the worst case is an old broken bullet beside
        // a working one.
        for name in ["a b.md", "a).md", "a#b.md", "a:b.md"] {
            let raw = format!("# D\n\n# Concepts\n\n* [Odd]({name}) - old\n");
            let bullet = format!("* [Odd]({}) - new", encode_link_path(name));
            let op = ReplayOp::ListBullet {
                target: format!("d/{name}"),
                bullet: bullet.clone(),
                section: None,
                create_section: false,
            };

            let got = crate::okf::ops::replay("d/index.md", Some(&raw), &[op]).unwrap();

            assert_eq!(got, format!("{raw}{bullet}\n"), "{name}");
        }
    }

    #[test]
    fn replay_keeps_the_staged_bullet_when_another_listing_reads_like_a_raw_one() {
        // `d/a.md` and `d/a.md#b.md` exist; the index holds a working link to
        // `d/a.md` whose text is `d/a.md#b.md`'s raw name.
        let current = "# D\n\n# Concepts\n\n* [A](a.md#b.md) - a\n";
        let bullet = "* [B](a.md%23b.md) - b";
        let op = ReplayOp::ListBullet {
            target: "d/a.md#b.md".to_string(),
            bullet: bullet.to_string(),
            section: None,
            create_section: false,
        };

        let got = crate::okf::ops::replay("d/index.md", Some(current), &[op]).unwrap();

        assert_eq!(got, format!("{current}{bullet}\n"));
    }

    #[test]
    fn index_ignores_text_that_only_looks_like_a_raw_link() {
        // None of these is a bullet older versions wrote, so the real
        // listing is appended.
        let lines = [
            "* Old destination: ](a b.md)",
            "* ![Odd](a b.md)",
            "* `[Odd](a b.md)` in a code span",
            "* see [Odd](a b.md) - prose before the link",
            "* [Odd [x](a b.md) - unbalanced",
            "  * [Odd](a b.md) - indented, not a bullet start",
            "[Odd](a b.md) - not a bullet",
        ];
        for line in lines {
            let existing = format!("# D\n\n# Concepts\n\n{line}\n");
            let ws = awkward_ws("a b.md", &existing);

            index(&ws, &ctx(), "d/a b.md", "x", None, None).unwrap();

            let got = ws.read_to_string("d/index.md").unwrap();
            assert!(got.contains("](a%20b.md)"), "{line}: {got}");
        }
    }

    #[test]
    fn index_lists_a_name_whose_raw_form_reads_as_another_working_link() {
        // `* [A](a.md).md)` is a working link to `d/a.md`, not a raw
        // listing of `d/a.md).md`; likewise `a.md#b.md` for `d/a.md#b.md`.
        for (other, name) in [("a.md", "a.md).md"), ("a.md", "a.md#b.md")] {
            let existing = format!("# D\n\n# Concepts\n\n* [A]({name}) - a\n");
            let ws = awkward_ws(other, &existing);
            let text = "---\ntype: Note\ntitle: B\ndescription: b\n---\n";
            ws.write_atomic(&format!("d/{name}"), text).unwrap();
            let encoded = encode_link_path(name);

            index(&ws, &ctx(), &format!("d/{name}"), "b", None, None).unwrap();

            let got = ws.read_to_string("d/index.md").unwrap();
            assert!(got.contains(&format!("]({encoded})")), "{name}: {got}");
            let again = repair_indexes(&ws).unwrap();
            assert!(!again.touched.contains(&"d/index.md".to_string()), "{name}");
        }
    }

    #[test]
    fn a_literal_percent_name_is_not_taken_for_an_encoded_one() {
        // `d/a b.md` is listed as `a%20b.md`; `d/a%20b.md` is another file
        // and needs its own bullet, `a%2520b.md`, from index, repair and replay.
        let files = [("d/a b.md", "Space"), ("d/a%20b.md", "Percent")];
        let make = || {
            let ws = MemWorkspace::default();
            ws.write_atomic("index.md", "# Root\n").unwrap();
            ws.write_atomic("log.md", "# Log\n").unwrap();
            ws.write_atomic("d/index.md", "# D\n\n# Concepts\n")
                .unwrap();
            for (path, title) in files {
                let text = format!("---\ntype: Note\ntitle: {title}\ndescription: d\n---\n");
                ws.write_atomic(path, &text).unwrap();
            }
            ws
        };
        let check = |ws: &MemWorkspace, how: &str| {
            let text = ws.read_to_string("d/index.md").unwrap();
            assert!(text.contains("](a%20b.md)"), "{how}: {text}");
            assert!(text.contains("](a%2520b.md)"), "{how}: {text}");
            assert_eq!(text.matches("* [").count(), 2, "{how}: {text}");
        };

        let by_index = make();
        for (path, _) in files {
            index(&by_index, &ctx(), path, "d", None, None).unwrap();
        }
        check(&by_index, "index");

        let by_repair = make();
        repair_indexes(&by_repair).unwrap();
        check(&by_repair, "repair");

        let ws = make();
        let ops = [
            ("d/a b.md", "* [Space](a%20b.md) - d"),
            ("d/a%20b.md", "* [Percent](a%2520b.md) - d"),
        ]
        .map(|(target, bullet)| ReplayOp::ListBullet {
            target: target.to_string(),
            bullet: bullet.to_string(),
            section: None,
            create_section: false,
        });
        let got = crate::okf::ops::replay("d/index.md", Some("# D\n\n# Concepts\n"), &ops).unwrap();
        ws.write_atomic("d/index.md", &got).unwrap();
        check(&ws, "replay");
    }

    #[test]
    fn index_refusals_use_okf_py_wording() {
        let ws = infra();

        let untitled = index(&ws, &ctx(), "infra/untitled", "s", None, None).unwrap_err();
        let no_index = index(&ws, &ctx(), "people/x", "s", None, None).unwrap_err();
        let no_heading = index(
            &ws,
            &ctx(),
            "infra/untitled",
            "s",
            Some("Nowhere"),
            Some("T"),
        )
        .unwrap_err();

        assert_eq!(
            refusal(untitled),
            "infra/untitled.md has no title; pass title"
        );
        assert_eq!(refusal(no_index), "no index.md in /people; create it first");
        assert_eq!(
            refusal(no_heading),
            "no heading 'Nowhere' in infra/index.md"
        );
        assert_eq!(ws.read_to_string("infra/index.md").unwrap(), INFRA_INDEX);
    }

    #[test]
    fn index_lists_a_scalar_title_and_refuses_a_list_title() {
        let ws = MemWorkspace::from_files([
            ("infra/index.md", INFRA_INDEX),
            ("infra/year.md", "---\ntype: Note\ntitle: 2026\n---\n"),
            ("infra/listed.md", "---\ntype: Note\ntitle: [a, b]\n---\n"),
        ]);

        let year = index(&ws, &ctx(), "infra/year", "y", None, None).unwrap();
        let listed = index(&ws, &ctx(), "infra/listed", "l", None, None).unwrap_err();

        // okf.py writes the same bullet for `title: 2026`, and crashes on the list.
        assert_eq!(year.message, "infra/index.md: * [2026](year.md) - y");
        assert_eq!(refusal(listed), "infra/listed.md has no title; pass title");
    }

    #[test]
    fn ensure_index_follows_the_parents_link_style() {
        let ws = MemWorkspace::from_files([
            ("index.md", "# Root\n\n* [Infra](infra/) - systems\n"),
            (
                "analysis/index.md",
                "# Analysis\n\n# Sections\n\n* [Sub](/analysis/sub/) - a folder\n",
            ),
        ]);
        let mut absolute = OpOutcome::default();
        let mut nested = OpOutcome::default();

        ensure_index(&ws, "analysis/new-stuff", &mut absolute).unwrap();
        ensure_index(&ws, "ops/deep", &mut nested).unwrap();

        assert_eq!(
            absolute.touched,
            ["analysis/new-stuff/index.md", "analysis/index.md"]
        );
        assert_eq!(
            ws.read_to_string("analysis/new-stuff/index.md").unwrap(),
            "# New stuff\n\nTODO: say what this folder holds.\n"
        );
        assert_eq!(
            ws.read_to_string("analysis/index.md").unwrap(),
            concat!(
                "# Analysis\n\n# Sections\n\n* [Sub](/analysis/sub/) - a folder\n",
                "* [New stuff](/analysis/new-stuff/) - TODO: describe this folder\n",
            )
        );
        assert_eq!(
            absolute.replay["analysis/index.md"],
            [ReplayOp::ListBullet {
                target: "analysis/new-stuff/index.md".to_string(),
                bullet: "* [New stuff](/analysis/new-stuff/) - TODO: describe this folder"
                    .to_string(),
                section: Some("Sections".to_string()),
                create_section: true,
            }]
        );
        assert_eq!(
            nested.touched,
            ["ops/deep/index.md", "ops/index.md", "index.md"]
        );
        assert_eq!(
            ws.read_to_string("index.md").unwrap(),
            concat!(
                "# Root\n\n* [Infra](infra/) - systems\n\n# Sections\n\n",
                "* [Ops](ops/) - TODO: describe this folder\n",
            )
        );
        assert_eq!(
            nested.notes,
            [
                format!("created /ops/index.md{FOLDER_NOTE}"),
                format!("created /ops/deep/index.md{FOLDER_NOTE}"),
            ]
        );
    }

    #[test]
    fn ensure_index_of_an_existing_folder_or_the_root_does_nothing() {
        let ws = MemWorkspace::from_files([("index.md", "# Root\n"), ("ops/index.md", "# Ops\n")]);
        let mut outcome = OpOutcome::default();

        ensure_index(&ws, "ops", &mut outcome).unwrap();
        ensure_index(&ws, "", &mut outcome).unwrap();

        assert_eq!(outcome, OpOutcome::default());
    }

    /// A catalog from before starter files: no root index or log, a curated
    /// folder index that misses a concept and a subfolder, and machinery.
    fn legacy() -> MemWorkspace {
        MemWorkspace::from_files([
            ("infra/index.md", LEGACY_INFRA),
            (
                "infra/time-server.md",
                "---\ntype: System\ntitle: Time server\ndescription: GPS clock.\n---\n",
            ),
            (
                "infra/ntp.md",
                "---\ntype: Service\ntitle: NTP\ndescription: \"Keeps\\n   time.\"\n---\n",
            ),
            ("infra/hosts/web.md", "---\ntype: Host\n---\n# Web   host\n"),
            ("a-note.md", "No frontmatter at all.\n"),
            ("tools/helper.md", "# Tooling\n"),
        ])
    }

    const LEGACY_INFRA: &str = concat!(
        "# Infra\n\nOur systems, by hand.\n\n# Concepts\n\n",
        "* [Time server](time-server.md) - curated words\n",
    );

    fn sorted(mut notes: Vec<String>) -> Vec<String> {
        notes.sort();
        notes
    }

    #[test]
    fn repair_indexes_creates_what_an_empty_catalog_lacks() {
        let ws = MemWorkspace::default();

        let outcome = repair_indexes(&ws).unwrap();

        assert_eq!(outcome.touched, ["index.md", "log.md"]);
        assert_eq!(
            ws.read_to_string("index.md").unwrap(),
            "---\nokf_version: \"0.2\"\n---\n\n# Catalog\n\nTODO: say what this catalog holds.\n"
        );
        assert_eq!(
            ws.read_to_string("log.md").unwrap(),
            "# Knowledge Catalog Update Log\n"
        );
        assert_eq!(
            outcome.replay["index.md"],
            [ReplayOp::CreateIndex {
                folder: String::new()
            }]
        );
        assert_eq!(outcome.replay["log.md"], [ReplayOp::CreateLog]);
        assert_eq!(
            outcome.message,
            "repaired 2 file(s); 1 TODO placeholder(s) to fill in (see notes)"
        );
        assert_eq!(
            outcome.notes,
            [
                "created /index.md; replace its heading and TODO line",
                "created /log.md"
            ]
        );
    }

    #[test]
    fn repair_indexes_fills_a_legacy_catalog_and_keeps_its_curated_text() {
        let ws = legacy();

        let outcome = repair_indexes(&ws).unwrap();

        assert_eq!(
            outcome.touched,
            [
                "index.md",
                "log.md",
                "infra/hosts/index.md",
                "infra/index.md"
            ]
        );
        assert_eq!(
            ws.read_to_string("index.md").unwrap(),
            concat!(
                "---\nokf_version: \"0.2\"\n---\n\n# Catalog\n\n",
                "TODO: say what this catalog holds.\n\n# Concepts\n\n",
                "* [A note](a-note.md) - TODO: describe this concept\n\n# Sections\n\n",
                "* [Infra](infra/) - TODO: describe this folder\n",
            )
        );
        // The curated prose and listing stay; the missing ones are added.
        assert_eq!(
            ws.read_to_string("infra/index.md").unwrap(),
            concat!(
                "# Infra\n\nOur systems, by hand.\n\n# Concepts\n\n",
                "* [Time server](time-server.md) - curated words\n",
                "* [NTP](ntp.md) - Keeps time.\n\n# Sections\n\n",
                "* [Hosts](hosts/) - TODO: describe this folder\n",
            )
        );
        assert_eq!(
            ws.read_to_string("infra/hosts/index.md").unwrap(),
            concat!(
                "# Hosts\n\nTODO: say what this folder holds.\n\n# Concepts\n\n",
                "* [Web host](web.md) - TODO: describe this concept\n",
            )
        );
        // Machinery is never written: no write tool may change it.
        assert!(!ws.exists("tools/index.md"));
        assert_eq!(
            outcome.message,
            "repaired 4 file(s); 7 TODO placeholder(s) to fill in (see notes)"
        );
        assert_eq!(
            sorted(outcome.notes),
            [
                "created /index.md; replace its heading and TODO line",
                "created /infra/hosts/index.md; replace its TODO line",
                "created /log.md",
                "listed /a-note.md under its file name: it has no title",
                "listed /a-note.md with a TODO summary: it has no description; describe it with index on /a-note.md",
                "listed /infra/ in /index.md with a TODO summary; describe it with index on /infra/",
                "listed /infra/hosts/ in /infra/index.md with a TODO summary; describe it with index on /infra/hosts/",
                "listed /infra/hosts/web.md with a TODO summary: it has no description; describe it with index on /infra/hosts/web.md",
            ]
        );
        // Every edit of an index is a replay op, so publish never overwrites
        // an index that changed meanwhile.
        assert_eq!(
            outcome.replay["infra/index.md"],
            [
                ReplayOp::ListBullet {
                    target: "infra/hosts/index.md".to_string(),
                    bullet: "* [Hosts](hosts/) - TODO: describe this folder".to_string(),
                    section: Some("Sections".to_string()),
                    create_section: true,
                },
                ReplayOp::ListBullet {
                    target: "infra/ntp.md".to_string(),
                    bullet: "* [NTP](ntp.md) - Keeps time.".to_string(),
                    section: None,
                    create_section: false,
                },
            ]
        );
    }

    #[test]
    fn repair_indexes_twice_changes_nothing_the_second_time() {
        let ws = legacy();
        repair_indexes(&ws).unwrap();
        let before = ws.read_to_string("infra/index.md").unwrap();

        let again = repair_indexes(&ws).unwrap();

        assert_eq!(again.touched, Vec::<String>::new());
        assert!(again.replay.is_empty() && again.notes.is_empty());
        assert_eq!(again.message, "indexes complete: nothing to repair");
        assert_eq!(ws.read_to_string("infra/index.md").unwrap(), before);
    }

    #[test]
    fn repair_indexes_accepts_any_wording_of_an_existing_listing() {
        // Absolute links, a folder linked by its index file, and a concept
        // listed under # Sections all count as listed: nothing is duplicated.
        let root = concat!(
            "---\nokf_version: \"0.2\"\n---\n# Root\n\n# Sections\n\n",
            "* [Ops, by hand](/ops/index.md) - curated\n* [Top](/top.md) - misfiled\n",
        );
        let ws = MemWorkspace::from_files([
            ("index.md", root),
            ("log.md", "# Log\n"),
            ("top.md", "---\ntype: Note\ntitle: Top\n---\n"),
            ("ops/index.md", "# Ops\n\n* [Run](/ops/run.md) - curated\n"),
            ("ops/run.md", "---\ntype: Note\ntitle: Run\n---\n"),
            ("ops/deep/index.md", "# Deep\n"),
        ]);

        let outcome = repair_indexes(&ws).unwrap();

        // Only the unlisted folder index is listed, in its parent's style.
        assert_eq!(outcome.touched, ["ops/index.md"]);
        assert_eq!(ws.read_to_string("index.md").unwrap(), root);
        assert_eq!(
            ws.read_to_string("ops/index.md").unwrap(),
            concat!(
                "# Ops\n\n* [Run](/ops/run.md) - curated\n\n# Sections\n\n",
                "* [Deep](/ops/deep/) - TODO: describe this folder\n",
            )
        );
        assert_eq!(
            outcome.notes,
            ["listed /ops/deep/ in /ops/index.md with a TODO summary; describe it with index on /ops/deep/"]
        );
    }

    #[test]
    fn repair_indexes_creates_the_missing_ancestors_of_a_nested_index() {
        // `infra/hosts/index.md` exists, `infra/index.md` does not: the
        // ancestor is created and listed, and the nested index listed in it.
        let hosts = "# Hosts\n\nCurated hosts.\n";
        let ws = MemWorkspace::from_files([
            ("index.md", "---\nokf_version: \"0.2\"\n---\n# Root\n"),
            ("log.md", "# Log\n"),
            ("infra/hosts/index.md", hosts),
        ]);

        let outcome = repair_indexes(&ws).unwrap();
        let again = repair_indexes(&ws).unwrap();

        assert_eq!(outcome.touched, ["infra/index.md", "index.md"]);
        assert_eq!(
            ws.read_to_string("infra/index.md").unwrap(),
            concat!(
                "# Infra\n\nTODO: say what this folder holds.\n\n# Sections\n\n",
                "* [Hosts](hosts/) - TODO: describe this folder\n",
            )
        );
        assert_eq!(
            ws.read_to_string("index.md").unwrap(),
            concat!(
                "---\nokf_version: \"0.2\"\n---\n# Root\n\n# Sections\n\n",
                "* [Infra](infra/) - TODO: describe this folder\n",
            )
        );
        assert_eq!(ws.read_to_string("infra/hosts/index.md").unwrap(), hosts);
        assert_eq!(
            outcome.message,
            "repaired 2 file(s); 3 TODO placeholder(s) to fill in (see notes)"
        );
        assert!(again.touched.is_empty() && again.notes.is_empty());
    }

    #[test]
    fn repair_indexes_keeps_a_curated_link_to_a_folder_whose_index_is_missing() {
        // The root links `infra/` already, but `infra/index.md` is gone: the
        // index is created, and the root keeps its one curated link.
        let root = concat!(
            "---\nokf_version: \"0.2\"\n---\n# Root\n\n# Sections\n\n",
            "* [Our systems](infra/) - curated\n",
        );
        let ws = MemWorkspace::from_files([
            ("index.md", root),
            ("log.md", "# Log\n"),
            (
                "infra/ntp.md",
                "---\ntype: Note\ntitle: NTP\ndescription: Keeps time.\n---\n",
            ),
        ]);

        let outcome = repair_indexes(&ws).unwrap();
        let again = repair_indexes(&ws).unwrap();

        assert_eq!(outcome.touched, ["infra/index.md"]);
        assert_eq!(ws.read_to_string("index.md").unwrap(), root);
        assert_eq!(
            ws.read_to_string("infra/index.md").unwrap(),
            concat!(
                "# Infra\n\nTODO: say what this folder holds.\n\n# Concepts\n\n",
                "* [NTP](ntp.md) - Keeps time.\n",
            )
        );
        assert!(!outcome.replay.contains_key("index.md"));
        assert_eq!(
            outcome.notes,
            ["created /infra/index.md; replace its TODO line"]
        );
        assert!(again.touched.is_empty() && again.notes.is_empty());
    }

    #[test]
    fn repair_indexes_percent_encodes_names_a_plain_link_cannot_hold() {
        // A link's destination stops at whitespace or `)`, and `#` starts
        // its fragment, so these names are written percent-encoded
        // (`a%20note.md`); `resolve_link` decodes them, so the next run
        // recognises each listing and writes nothing.
        let note = "---\ntype: Note\ntitle: N\ndescription: D.\n---\n";
        let root = "---\nokf_version: \"0.2\"\n---\n# Root\n\n* [Top](/top.md) - curated\n";
        let ws = MemWorkspace::from_files([
            ("index.md", root),
            ("log.md", "# Log\n"),
            ("top.md", note),
            ("a note.md", note),
            ("x)y.md", note),
            ("h#1.md", note),
            ("tab\tx.md", note),
            ("my notes/a.md", note),
            ("ok/b.md", note),
        ]);

        let outcome = repair_indexes(&ws).unwrap();
        let again = repair_indexes(&ws).unwrap();

        let index = ws.read_to_string("index.md").unwrap();
        for listing in [
            "(/my%20notes/)",
            "(/ok/)",
            "(a%20note.md)",
            "(x%29y.md)",
            "(h%231.md)",
            "(tab%09x.md)",
        ] {
            assert!(index.contains(listing), "{listing} in {index}");
        }
        assert!(ws
            .read_to_string("my notes/index.md")
            .unwrap()
            .ends_with("# Concepts\n\n* [N](a.md) - D.\n"));
        assert!(
            !outcome
                .notes
                .iter()
                .any(|line| line.starts_with("did not list")),
            "{:?}",
            outcome.notes
        );
        // The second run recognises every listing: it writes nothing.
        assert!(again.touched.is_empty() && again.replay.is_empty());
        assert!(again.notes.is_empty(), "{:?}", again.notes);
    }

    #[test]
    fn repair_indexes_checks_the_whole_bullet_label_included() {
        // The path is linkable but the label is not: a folder `a]` gets the
        // heading `A]`, and a title ending in `\` escapes the closing `]`.
        // Neither bullet would parse, so neither is written, and the
        // repair settles.
        let ws = MemWorkspace::from_files([
            ("index.md", "---\nokf_version: \"0.2\"\n---\n# Root\n"),
            ("log.md", "# Log\n"),
            ("a]/index.md", "# A\n"),
            (
                "c.md",
                "---\ntype: Note\ntitle: 'C:\\'\ndescription: A drive.\n---\n",
            ),
            (
                "d.md",
                "---\ntype: Note\ntitle: D [draft]\ndescription: Kept.\n---\n",
            ),
        ]);

        let outcome = repair_indexes(&ws).unwrap();
        let again = repair_indexes(&ws).unwrap();

        // The title with brackets is escaped and listed; the others aren't.
        assert_eq!(
            ws.read_to_string("index.md").unwrap(),
            concat!(
                "---\nokf_version: \"0.2\"\n---\n# Root\n\n# Concepts\n\n",
                "* [D \\[draft\\]](d.md) - Kept.\n",
            )
        );
        assert_eq!(
            sorted(outcome.notes),
            [
                "did not list /a]/ in /index.md: no markdown link to it can be written",
                "did not list /c.md in /index.md: no markdown link to it can be written",
            ]
        );
        assert!(again.touched.is_empty() && again.replay.is_empty());
    }

    #[test]
    fn repair_indexes_lists_a_concept_whose_frontmatter_does_not_parse() {
        let ws = MemWorkspace::from_files([
            ("index.md", "---\nokf_version: \"0.2\"\n---\n# Root\n"),
            ("log.md", "# Log\n"),
            (
                "broken.md",
                "---\ntitle: [unclosed\n---\n# Broken heading\n",
            ),
        ]);

        let outcome = repair_indexes(&ws).unwrap();

        assert_eq!(
            ws.read_to_string("index.md").unwrap(),
            concat!(
                "---\nokf_version: \"0.2\"\n---\n# Root\n\n# Concepts\n\n",
                "* [Broken heading](broken.md) - TODO: describe this concept\n",
            )
        );
        assert_eq!(
            outcome.notes,
            ["listed /broken.md with a TODO summary: it has no description; describe it with index on /broken.md"]
        );
    }

    #[test]
    fn starter_files_are_a_titled_root_index_and_a_dated_log() {
        let [(index_path, index_text), (log_path, log_text)] =
            starter_files("Systems", "  Systems\n and   people. ", ctx().now);
        let [(_, untitled), _] = starter_files("Catalog", " ", ctx().now);

        assert_eq!((index_path, log_path), ("index.md", "log.md"));
        assert_eq!(
            index_text,
            "---\nokf_version: \"0.2\"\n---\n\n# Systems\n\nSystems and people.\n"
        );
        assert_eq!(
            untitled,
            "---\nokf_version: \"0.2\"\n---\n\n# Catalog\n\nTODO: say what this catalog holds.\n"
        );
        assert_eq!(
            log_text,
            concat!(
                "# Knowledge Catalog Update Log\n\n## 2026-10-27\n\n",
                "* **Creation:** Created the catalog.\n",
            )
        );
        // The bookkeeping tools work on them at once.
        let ws = MemWorkspace::from_files([
            (index_path, index_text.as_str()),
            (log_path, log_text.as_str()),
            ("a.md", "---\ntype: Note\ntitle: A\n---\n"),
        ]);
        assert_eq!(
            index(&ws, &ctx(), "a", "first", None, None)
                .unwrap()
                .message,
            "index.md: * [A](a.md) - first"
        );
        assert_eq!(
            log(&ws, &ctx(), LogKind::Update, "x").unwrap().touched,
            ["log.md"]
        );
    }

    #[test]
    fn repair_catalog_names_the_root_and_logs_what_it_wrote() {
        let ws = MemWorkspace::from_files([("infra/ntp.md", "---\ntype: S\ntitle: NTP\n---\n")]);

        let outcome = repair_catalog(&ws, &ctx(), "pg", "Team notes.").unwrap();
        let again = repair_catalog(&ws, &ctx(), "pg", "Team notes.").unwrap();

        assert_eq!(outcome.touched, ["index.md", "log.md", "infra/index.md"]);
        assert!(
            outcome.replay.is_empty(),
            "a direct write records no replay"
        );
        assert_eq!(
            ws.read_to_string("index.md").unwrap(),
            concat!(
                "---\nokf_version: \"0.2\"\n---\n\n# pg\n\nTeam notes.\n\n# Sections\n\n",
                "* [Infra](infra/) - TODO: describe this folder\n",
            )
        );
        assert_eq!(
            ws.read_to_string("log.md").unwrap(),
            concat!(
                "# Knowledge Catalog Update Log\n\n## 2026-10-27\n\n",
                "* **Update:** Repaired the catalog's indexes (okf-mcp catalog repair-indexes): ",
                "wrote index.md, log.md, infra/index.md.\n",
            )
        );
        assert_eq!(
            outcome.notes[0],
            "created /index.md from the catalog's name and description"
        );
        // infra/index.md and its listing (2), and NTP's missing description.
        assert_eq!(
            outcome.message,
            "repaired 3 file(s); 3 TODO placeholder(s) to fill in (see notes)"
        );
        assert!(again.touched.is_empty(), "{again:?}");
        assert_eq!(again.message, "indexes complete: nothing to repair");
    }

    #[test]
    fn repair_catalog_of_a_complete_catalog_logs_nothing() {
        let ws = MemWorkspace::from_files([
            ("index.md", "---\nokf_version: \"0.2\"\n---\n# Root\n"),
            ("log.md", "# Log\n"),
        ]);

        let outcome = repair_catalog(&ws, &ctx(), "pg", "").unwrap();

        assert!(outcome.touched.is_empty());
        assert_eq!(ws.read_to_string("log.md").unwrap(), "# Log\n");
    }

    /// A root listing `learning/`, with the folder's own index.
    fn learning(root: &str) -> MemWorkspace {
        MemWorkspace::from_files([
            ("index.md", root),
            (
                "learning/index.md",
                "# Learning\n\nTODO: say what this folder holds.\n",
            ),
            ("my docs/index.md", "# My docs\n"),
        ])
    }

    const LEARNING_ROOT: &str =
        "# Root\n\n# Sections\n\n* [Learning](learning/) - TODO: describe this folder\n";

    #[test]
    fn index_on_a_folder_sets_its_listing_summary_and_records_the_op() {
        for path in ["/learning/", "learning/", "/learning/index.md", "/learning"] {
            let ws = learning(LEARNING_ROOT);

            let outcome = index(&ws, &ctx(), path, " What we\n  learn. ", None, None).unwrap();

            assert_eq!(
                ws.read_to_string("index.md").unwrap(),
                "# Root\n\n# Sections\n\n* [Learning](learning/) - What we learn.\n",
                "{path}"
            );
            assert_eq!(outcome.touched, ["index.md"], "{path}");
            assert_eq!(
                outcome.replay["index.md"],
                [ReplayOp::SetListingSummary {
                    target: "learning/index.md".to_string(),
                    summary: "What we learn.".to_string(),
                }],
                "{path}"
            );
            assert_eq!(
                outcome.message,
                "index.md: * [Learning](learning/) - What we learn."
            );
        }
    }

    #[test]
    fn index_on_a_folder_with_that_summary_already_changes_nothing() {
        let ws = learning("# Root\n\n* [Learning](/learning/) - Ours.\n");

        let outcome = index(&ws, &ctx(), "/learning/", "Ours.", None, None).unwrap();

        assert!(outcome.touched.is_empty(), "{outcome:?}");
        assert_eq!(
            outcome.message,
            "index.md: /learning/ already has that summary"
        );
    }

    #[test]
    fn index_on_a_folder_refuses_what_it_cannot_describe() {
        let ws = learning("# Root\n\n# Sections\n\n* [My docs](my docs/) - Mine.\n");

        let section = index(&ws, &ctx(), "/learning/", "x", Some("Sections"), None);
        let title = index(&ws, &ctx(), "/learning/", "x", None, Some("L"));
        let root = index(&ws, &ctx(), "/", "x", None, None);
        let unlisted = index(&ws, &ctx(), "/learning/", "x", None, None);
        let raw = index(&ws, &ctx(), "/my docs/", "x", None, None);
        let missing = index(&ws, &ctx(), "/nowhere/", "x", None, None);
        let outside = index(&ws, &ctx(), "/../x/", "x", None, None);

        assert!(refusal(section.unwrap_err()).contains("leave out section and title"));
        assert!(refusal(title.unwrap_err()).contains("leave out section and title"));
        assert!(refusal(root.unwrap_err()).starts_with("the root is listed in no index.md"));
        assert_eq!(
            refusal(unlisted.unwrap_err()),
            "/index.md does not list /learning/: no bullet there starts with a link to it; \
             repair_indexes lists it"
        );
        assert_eq!(
            refusal(raw.unwrap_err()),
            "/index.md does not list /my docs/: its listing's link does not resolve; run \
             repair_indexes first"
        );
        assert!(
            matches!(missing, Err(OkfError::NotFound { ref path }) if path == "/nowhere/index.md")
        );
        assert!(
            matches!(outside, Err(OkfError::InvalidPath { .. })),
            "{outside:?}"
        );
        // Nothing was written.
        assert_eq!(
            ws.read_to_string("index.md").unwrap(),
            "# Root\n\n# Sections\n\n* [My docs](my docs/) - Mine.\n"
        );
    }

    #[test]
    fn ensure_folder_index_describes_the_innermost_new_folder_only() {
        let ws = MemWorkspace::from_files([("index.md", "# Root\n")]);
        let mut outcome = OpOutcome::default();

        ensure_folder_index(&ws, "ops/deep", Some("  Deep\n ops. "), &mut outcome).unwrap();

        assert_eq!(
            ws.read_to_string("index.md").unwrap(),
            "# Root\n\n# Sections\n\n* [Ops](ops/) - TODO: describe this folder\n"
        );
        assert_eq!(
            ws.read_to_string("ops/index.md").unwrap(),
            "# Ops\n\nTODO: say what this folder holds.\n\n# Sections\n\n* [Deep](deep/) - Deep ops.\n"
        );
        assert_eq!(
            outcome.replay["ops/index.md"][1],
            ReplayOp::ListBullet {
                target: "ops/deep/index.md".to_string(),
                bullet: "* [Deep](deep/) - Deep ops.".to_string(),
                section: Some("Sections".to_string()),
                create_section: true,
            }
        );
        assert_eq!(
            outcome.notes,
            [
                format!("created /ops/index.md{FOLDER_NOTE}"),
                "created /ops/deep/index.md; replace its TODO line".to_string(),
            ]
        );
    }

    #[test]
    fn check_new_folder_refuses_the_root_and_an_existing_folder() {
        let ws = learning(LEARNING_ROOT);

        let root = check_new_folder(&ws, "").unwrap_err();
        let existing = check_new_folder(&ws, "learning").unwrap_err();

        assert!(refusal(root).contains("the root is no new folder"));
        assert_eq!(
            refusal(existing),
            "folder_summary describes a new folder, and /learning/ exists; leave it out, or \
             describe /learning/ with index (path /learning/, summary)"
        );
        assert!(check_new_folder(&ws, "fresh").is_ok());
    }
}
