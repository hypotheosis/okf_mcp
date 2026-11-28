//! Link rewriting across a catalog: `okf.py`'s `rewrite_links` (`:1343`).
//! `okf.py`'s `mv` and `split` use it to point every link into a moved
//! document (or a moved section) at its new place; M6 ports both onto it.
//! Publish replays its link rewrites in `index.md` and `log.md` through
//! [`crate::okf::ops::ReplayOp::Relink`].
//!
//! A link keeps its style: in-page (`#x`), bundle-absolute (`/dir/x.md`) or
//! relative (`x.md`). A document that has just been moved, or created from
//! another's text, gets bundle-absolute links, the catalog's preferred style.
//!
//! Files are read byte for byte (`okf.py` opens them with `newline=""`) and
//! split on `\n` only, so a `\r\n` file keeps its line endings: only link
//! targets change. `rewrite_text` (crate-internal) is one file's rewrite,
//! pure over its text, so a replay can share it with [`rewrite_links`].
//!
//! `delete_concept` uses the same link resolution to unlink a deleted
//! concept from `log.md` files (issue #658): `unlink_text` (crate-internal)
//! turns each link into it into its link text, and `links_to` is the one
//! test of "a link into a document" that the edit and the delete's
//! `StillLinked` check share. Publish replays it through
//! [`crate::okf::ops::ReplayOp::Unlink`].

use std::borrow::Cow;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::OkfError;
use crate::okf::markdown::{encode_link_path, find_links, link_lines, resolve_link, LinkMatch};
use crate::okf::paths::{dirname, is_concept, outside_link_graph, relpath};
use crate::okf::pystr::py_unquote;
use crate::okf::workspace::Workspace;

/// Which links a rewrite redirects, and where: `rewrite_links`' `fn`, as
/// data, so a replay op can store it (a record, so its fields are public).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Redirect {
    /// The catalog-relative path whose links are redirected.
    pub from: String,
    /// Where they point afterwards.
    pub to: String,
    /// `None`: every link into `from`, its fragment kept (`mv`). `Some`:
    /// only links into `from` whose fragment, percent-decoded and
    /// lower-cased, is a key; the value is the new fragment, `None` to drop
    /// it (`split`'s `amap`).
    pub anchors: Option<BTreeMap<String, Option<String>>>,
}

impl Redirect {
    /// Where a link into `target` with `fragment` points now (`okf.py`'s
    /// `fn(t, frag)`).
    ///
    /// # Arguments
    /// * `target` - the link's resolved, catalog-relative target.
    /// * `fragment` - the text after its first `#`, if it has one.
    ///
    /// # Returns
    /// The new target and fragment, or `None` to leave the link alone.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn apply(&self, target: &str, fragment: Option<&str>) -> Option<(String, Option<String>)> {
        if target != self.from {
            return None;
        }
        match &self.anchors {
            None => Some((self.to.clone(), fragment.map(str::to_string))),
            Some(anchors) => {
                // `?` on an `Option`: a link without a fragment is left alone.
                // `norm_fragment`: `unquote(frag).lower()`.
                let wanted = py_unquote(fragment?).to_lowercase();
                anchors
                    .get(&wanted)
                    .map(|new_fragment| (self.to.clone(), new_fragment.clone()))
            }
        }
    }
}

/// A document whose relative and in-page links were written somewhere
/// else: a file just moved, or created from another file's text
/// (`rewrite_links`' `moved`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Moved<'a> {
    /// Where the document is now.
    pub path: &'a str,
    /// The path its links were written against.
    pub written_at: &'a str,
}

/// Port of `rewrite_links`: rewrites the links `redirect` names in every
/// markdown file of the catalog, and writes the files that changed.
///
/// # Arguments
/// * `workspace` - the catalog, after the move or split wrote its files.
/// * `redirect` - which links to redirect, and where.
/// * `moved` - the one document whose links were written at another path.
///
/// # Returns
/// The files changed, in walk order (`os.walk`'s, as `okf.py` lists them).
///
/// # Errors
/// The workspace's read or write error (a markdown file that is not UTF-8
/// is [`OkfError::Io`], where `okf.py` raises `UnicodeDecodeError`).
#[tracing::instrument(level = "debug", skip_all)]
pub fn rewrite_links(
    workspace: &dyn Workspace,
    redirect: &Redirect,
    moved: Option<Moved<'_>>,
) -> Result<Vec<String>, OkfError> {
    let mut changed = Vec::new();
    for path in workspace.list_markdown()? {
        if outside_link_graph(&path) {
            continue;
        }
        let base = match moved {
            Some(moved) if moved.path == path => moved.written_at,
            Some(_) | None => path.as_str(),
        };
        // Byte for byte: `rewrite_text` splits on `\n` and leaves `\r` alone.
        let text = workspace.read_to_string(&path)?;
        if let Some(updated) = rewrite_text(workspace, &path, base, &text, redirect) {
            workspace.write_atomic(&path, &updated)?;
            changed.push(path);
        }
    }
    Ok(changed)
}

/// One file's rewrite: `rewrite_links`' loop body.
///
/// # Arguments
/// * `workspace` - resolves links. A directory link written without a
///   trailing slash (`[x](../a)`) reaches its `index.md` only through
///   [`Workspace::is_dir`]. The replay passes an empty workspace, which is
///   enough only while `redirect.from` is a concept (not an `index.md`): no
///   directory link can point at a concept. `move_concept` and
///   `split_concept` only ever redirect a concept, and the replay
///   refuses any other source with an error. [`rewrite_links`] has the real workspace, so it
///   also handles an `index.md` source, as `okf.py` does.
/// * `path` - the file's catalog-relative path.
/// * `base` - the path its links were written against (`path`, unless it
///   was just moved).
/// * `text` - its content, as stored.
/// * `redirect` - which links to redirect.
///
/// # Returns
/// The new text, or `None` when no line changed.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) fn rewrite_text(
    workspace: &dyn Workspace,
    path: &str,
    base: &str,
    text: &str,
    redirect: &Redirect,
) -> Option<String> {
    let mut changed = false;
    // `Cow` keeps every unchanged line borrowed; only rewritten lines are new strings.
    let lines: Vec<Cow<'_, str>> = link_lines(text)
        .map(
            |(line, links)| match rewrite_line(workspace, path, base, (line, &links), redirect) {
                Some(updated) if updated != line => {
                    changed = true;
                    Cow::Owned(updated)
                }
                Some(_) | None => Cow::Borrowed(line),
            },
        )
        .collect();
    changed.then(|| lines.join("\n"))
}

/// `LINK_RE.sub(repl, line)`: the line with each redirected link's target
/// replaced, or `None` if no link was redirected.
#[tracing::instrument(level = "trace", skip_all)]
fn rewrite_line(
    workspace: &dyn Workspace,
    path: &str,
    base: &str,
    (line, links): (&str, &[LinkMatch]),
    redirect: &Redirect,
) -> Option<String> {
    let mut out: Option<String> = None;
    let mut copied = 0;
    for link in links {
        let Some(target) = redirected_target(workspace, path, base, &link.target, redirect) else {
            continue;
        };
        // `get_or_insert_with` builds the output only once a link changes.
        let buffer = out.get_or_insert_with(|| String::with_capacity(line.len() + 16));
        buffer.push_str(&line[copied..link.target_range.start]);
        buffer.push_str(&target);
        copied = link.target_range.end;
    }
    // `map` runs only when something was rewritten; `mut` lets the closure
    // append the rest of the line to the buffer it now owns.
    out.map(|mut buffer| {
        buffer.push_str(&line[copied..]);
        buffer
    })
}

/// `rewrite_links`' `repl`: the new target text of one link, or `None` to
/// keep the link as written.
#[tracing::instrument(level = "trace", skip_all)]
fn redirected_target(
    workspace: &dyn Workspace,
    path: &str,
    base: &str,
    raw: &str,
    redirect: &Redirect,
) -> Option<String> {
    // `raw.partition("#")`: the part before the first `#`, and the fragment
    // (`None` when there is no `#` at all, `Some("")` for a bare `#`).
    let (target_part, fragment) = match raw.split_once('#') {
        Some((target, fragment)) => (target, Some(fragment)),
        None => (raw, None),
    };
    let own = raw.starts_with('#');
    let (target, absolute) = if own {
        (base.to_string(), true)
    } else {
        // External and empty targets are left alone (`tgt is None`).
        let resolved = resolve_link(workspace, raw, base)?;
        (resolved.path, resolved.absolute)
    };
    let (new_target, new_fragment) = redirect
        .apply(&target, fragment)
        .unwrap_or_else(|| (target.clone(), fragment.map(str::to_string)));
    let unchanged = new_target == target && new_fragment.as_deref() == fragment;
    if unchanged && (base == path || absolute) && (!own || new_target == path) {
        return None;
    }
    // A fragment that is empty counts as none, as Python's `if nfrag`.
    let fragment = new_fragment
        .as_deref()
        .filter(|fragment| !fragment.is_empty());
    let mut new = if own && new_target == path {
        match fragment {
            Some(fragment) => format!("#{fragment}"),
            None => format!("/{}", encode_link_path(&new_target)),
        }
    } else if own || absolute || (base != path && is_concept(path)) {
        // `strip_suffix` gives `Some(folder/)` only when the path ends in `index.md`.
        match new_target.strip_suffix("index.md") {
            Some(folder) if target_part.ends_with('/') && new_target.ends_with("/index.md") => {
                format!("/{}", encode_link_path(folder))
            }
            Some(_) | None => format!("/{}", encode_link_path(&new_target)),
        }
    } else if target_part.ends_with('/') && new_target.ends_with("index.md") {
        // `dirname(ntgt) or "."`: `relpath` reads `""` as the root.
        let folder = relpath(dirname(&new_target), dirname(path));
        format!("{}/", encode_link_path(&folder))
    } else {
        encode_link_path(&relpath(&new_target, dirname(path))).into_owned()
    };
    if let Some(fragment) = fragment {
        if !new.starts_with('#') {
            new.push('#');
            new.push_str(fragment);
        }
    }
    Some(new)
}

/// The links on `line`, a line of the document at `source`, that resolve to
/// `target`: the one test of "a link into a document" that
/// `delete_concept`'s `StillLinked` check and its `log.md` unlinking
/// ([`unlink_text`]) share, so the two never disagree on which links count.
/// It resolves each link as [`rewrite_text`] (a move) and the snapshot's
/// link graph do: in-page (`#x`) and external links never count, and a
/// percent-encoded path is decoded first ([`resolve_link`]).
///
/// # Arguments
/// * `workspace` - resolves directory links (the replay passes an empty one,
///   which is enough while `target` is a concept).
/// * `line` - one line, split on `\n`.
/// * `source` - the document's catalog-relative path, to resolve relative links.
/// * `target` - the catalog-relative path linked to.
///
/// # Returns
/// The matching links, left to right.
#[tracing::instrument(level = "trace", skip_all)]
pub(crate) fn links_to(
    workspace: &dyn Workspace,
    line: &str,
    source: &str,
    target: &str,
) -> Vec<LinkMatch> {
    into_links_to(workspace, find_links(line), source, target)
}

/// The links of `links` that resolve to `target`, as [`links_to`] reads them.
#[tracing::instrument(level = "trace", skip_all)]
fn into_links_to(
    workspace: &dyn Workspace,
    links: Vec<LinkMatch>,
    source: &str,
    target: &str,
) -> Vec<LinkMatch> {
    links
        .into_iter()
        .filter(|link| {
            resolve_link(workspace, &link.target, source)
                .is_some_and(|resolved| resolved.path == target)
        })
        .collect()
}

/// `delete_concept`'s edit of a `log.md`: every link into `target` becomes
/// its link text (`[X](/infra/x.md "title")` becomes `X`), so the entry
/// keeps its words and nothing is left pointing at the deleted concept.
/// Every other byte is kept, `\r\n` endings included, as [`rewrite_text`]
/// keeps them, so the op and its replay produce the same text.
///
/// # Arguments
/// * `workspace` - resolves links, as for [`links_to`].
/// * `path` - the file's catalog-relative path.
/// * `text` - its content, as stored.
/// * `target` - the deleted concept's catalog-relative path.
///
/// # Returns
/// The new text, or `None` when no link resolves to `target`.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) fn unlink_text(
    workspace: &dyn Workspace,
    path: &str,
    text: &str,
    target: &str,
) -> Option<String> {
    let mut changed = false;
    let lines: Vec<Cow<'_, str>> = link_lines(text)
        .map(
            |(line, links)| match unlink_line(workspace, path, (line, links), target) {
                Some(updated) => {
                    changed = true;
                    Cow::Owned(updated)
                }
                None => Cow::Borrowed(line),
            },
        )
        .collect();
    changed.then(|| lines.join("\n"))
}

/// One line of [`unlink_text`]: each link into `target` replaced by its
/// text, repeated until none is left. A second pass is needed only when a
/// link's text and what follows it form a new link (`[[a](x.md)](x.md)`
/// leaves `[a](x.md)`); each pass shortens the line, so it ends, and the
/// result has no link into `target`, which keeps the replay idempotent.
///
/// # Returns
/// The new line, or `None` when no link resolves to `target`.
#[tracing::instrument(level = "trace", skip_all)]
fn unlink_line(
    workspace: &dyn Workspace,
    path: &str,
    (line, found): (&str, Vec<LinkMatch>),
    target: &str,
) -> Option<String> {
    let mut current: Option<String> = None;
    // The first pass uses the links found with the whole text's code ranges;
    // later passes read the shortened line alone.
    let mut first = Some(found);
    loop {
        let text = current.as_deref().unwrap_or(line);
        let links = match first.take() {
            Some(found) => into_links_to(workspace, found, path, target),
            None => links_to(workspace, text, path, target),
        };
        if links.is_empty() {
            return current;
        }
        let mut out = String::with_capacity(text.len());
        let mut copied = 0;
        for link in &links {
            out.push_str(&text[copied..link.range.start]);
            out.push_str(&link.text);
            copied = link.range.end;
        }
        out.push_str(&text[copied..]);
        current = Some(out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::okf::workspace::MemWorkspace;

    /// A catalog just after `moves/alpha.md` became `moves/renamed.md`.
    const MOVE_FILES: [(&str, &str); 8] = [
        ("index.md", "# Root\n\n* [Moves](moves/) - m\n"),
        ("AGENTS.md", "See [alpha](moves/alpha.md).\n"),
        (
            "log.md",
            "# Log\n\n## 2026-11-03\n\n* **Update:** Added [alpha](/moves/alpha.md).\n",
        ),
        ("moves/index.md", "# Moves\n\n* [Alpha](alpha.md) - first\n"),
        (
            "moves/renamed.md",
            "---\ntype: Note\n---\nSee [beta](beta.md), [part](#part) and \
             [web](https://example.com/x).\n\n## Part\n",
        ),
        (
            "moves/beta.md",
            "---\ntype: Note\n---\nBack to [alpha](alpha.md#part) and \
             [abs](/moves/alpha.md).\r\nUntouched line.\r\n",
        ),
        (
            "people/linker.md",
            "---\ntype: Note\n---\nSee [alpha](../moves/alpha.md#Part) and [folder](/moves/).\n",
        ),
        ("people/other.md", "---\ntype: Note\n---\nNo links here.\n"),
    ];

    fn redirect(from: &str, to: &str) -> Redirect {
        Redirect {
            from: from.to_string(),
            to: to.to_string(),
            anchors: None,
        }
    }

    fn text(workspace: &MemWorkspace, path: &str) -> String {
        workspace.read_to_string(path).unwrap()
    }

    #[test]
    fn a_move_redirects_every_style_and_keeps_fragments_and_line_endings() {
        let workspace = MemWorkspace::from_files(MOVE_FILES);
        let moved = Moved {
            path: "moves/renamed.md",
            written_at: "moves/alpha.md",
        };

        let changed = rewrite_links(
            &workspace,
            &redirect("moves/alpha.md", "moves/renamed.md"),
            Some(moved),
        )
        .unwrap();

        assert_eq!(
            changed,
            [
                "log.md",
                "moves/beta.md",
                "moves/index.md",
                "moves/renamed.md",
                "people/linker.md"
            ]
        );
        assert_eq!(
            text(&workspace, "log.md"),
            "# Log\n\n## 2026-11-03\n\n* **Update:** Added [alpha](/moves/renamed.md).\n"
        );
        // Relative stays relative, absolute stays absolute, `\r\n` stays.
        assert_eq!(
            text(&workspace, "moves/beta.md"),
            "---\ntype: Note\n---\nBack to [alpha](renamed.md#part) and \
             [abs](/moves/renamed.md).\r\nUntouched line.\r\n"
        );
        assert_eq!(
            text(&workspace, "moves/index.md"),
            "# Moves\n\n* [Alpha](renamed.md) - first\n"
        );
        // The moved file's relative links become absolute; in-page ones stay.
        assert_eq!(
            text(&workspace, "moves/renamed.md"),
            "---\ntype: Note\n---\nSee [beta](/moves/beta.md), [part](#part) and \
             [web](https://example.com/x).\n\n## Part\n"
        );
        // The fragment is kept as written (`#Part`), and the folder link is not this file's.
        assert_eq!(
            text(&workspace, "people/linker.md"),
            "---\ntype: Note\n---\nSee [alpha](../moves/renamed.md#Part) and [folder](/moves/).\n"
        );
        // The root AGENTS.md is not catalog content.
        assert_eq!(
            text(&workspace, "AGENTS.md"),
            "See [alpha](moves/alpha.md).\n"
        );
    }

    #[test]
    fn a_rewritten_destination_is_percent_encoded_and_still_resolves() {
        let workspace = MemWorkspace::from_files([
            ("moves/alpha.md", "# Alpha\n"),
            (
                "moves/beta.md",
                "[rel](alpha.md#Part) [abs](/moves/alpha.md) [enc](a%20b.md)\n",
            ),
            ("moves/a b.md", "# Spaced\n"),
        ]);
        let moved = Moved {
            path: "moves/100% (new).md",
            written_at: "moves/alpha.md",
        };

        rewrite_links(
            &workspace,
            &redirect("moves/alpha.md", "moves/100% (new).md"),
            Some(moved),
        )
        .unwrap();

        // The new name is encoded, the fragment is kept, and the encoded
        // link to another file is left as written.
        assert_eq!(
            text(&workspace, "moves/beta.md"),
            "[rel](100%25%20%28new%29.md#Part) [abs](/moves/100%25%20%28new%29.md) [enc](a%20b.md)\n"
        );
        // The encoded destination resolves to the moved file.
        let resolved = resolve_link(&workspace, "100%25%20%28new%29.md", "moves/beta.md");
        assert_eq!(resolved.unwrap().path, "moves/100% (new).md");
    }

    #[test]
    fn an_encoded_link_to_the_moved_file_is_redirected() {
        let workspace = MemWorkspace::from_files([
            ("moves/a b.md", "# Spaced\n"),
            ("moves/beta.md", "[x](a%20b.md)\n"),
        ]);

        rewrite_links(&workspace, &redirect("moves/a b.md", "moves/c.md"), None).unwrap();

        assert_eq!(text(&workspace, "moves/beta.md"), "[x](c.md)\n");
    }

    #[test]
    fn an_angle_bracket_link_to_the_moved_file_is_redirected_and_keeps_its_brackets() {
        let workspace = MemWorkspace::from_files([
            ("moves/a b.md", "# Spaced\n"),
            (
                "moves/beta.md",
                "[x](<a b.md#h> \"t\") [y](</moves/a b.md>)\n",
            ),
        ]);

        rewrite_links(&workspace, &redirect("moves/a b.md", "moves/c d.md"), None).unwrap();

        assert_eq!(
            text(&workspace, "moves/beta.md"),
            "[x](<c%20d.md#h> \"t\") [y](</moves/c%20d.md>)\n"
        );
    }

    #[test]
    fn a_split_redirect_moves_only_the_named_anchors() {
        let workspace = MemWorkspace::from_files([
            (
                "splits/parent.md",
                "---\ntype: Note\n---\n# Intro\n\nSee [bg](#background).\n\n## Background\n",
            ),
            (
                "splits/background.md",
                "---\ntype: Note\n---\nIntro [back](#intro) and [detail](#detail).\n\n# Detail\n",
            ),
            (
                "splits/user.md",
                "---\ntype: Note\n---\n[a](/splits/parent.md#background) [b](parent.md#Detail) \
                 [c](/splits/parent.md#intro) [d](/splits/parent.md)\n",
            ),
        ]);
        let anchors = BTreeMap::from([
            ("background".to_string(), None),
            ("detail".to_string(), Some("detail".to_string())),
        ]);
        let redirect = Redirect {
            from: "splits/parent.md".to_string(),
            to: "splits/background.md".to_string(),
            anchors: Some(anchors),
        };
        let moved = Moved {
            path: "splits/background.md",
            written_at: "splits/parent.md",
        };

        let changed = rewrite_links(&workspace, &redirect, Some(moved)).unwrap();

        assert_eq!(
            changed,
            ["splits/background.md", "splits/parent.md", "splits/user.md"]
        );
        assert_eq!(
            text(&workspace, "splits/background.md"),
            "---\ntype: Note\n---\nIntro [back](/splits/parent.md#intro) and \
             [detail](#detail).\n\n# Detail\n"
        );
        assert_eq!(
            text(&workspace, "splits/parent.md"),
            "---\ntype: Note\n---\n# Intro\n\nSee [bg](/splits/background.md).\n\n## Background\n"
        );
        assert_eq!(
            text(&workspace, "splits/user.md"),
            "---\ntype: Note\n---\n[a](/splits/background.md) [b](background.md#detail) \
             [c](/splits/parent.md#intro) [d](/splits/parent.md)\n"
        );
    }

    #[test]
    fn folder_links_keep_their_trailing_slash() {
        let workspace = MemWorkspace::from_files([
            ("a/index.md", "# A\n"),
            ("b/index.md", "# B\n"),
            (
                "c/x.md",
                "---\ntype: Note\n---\n[rel](../a/) [abs](/a/) [file](../a/index.md#top) \
                 [root](../index.md)\n",
            ),
            ("index.md", "# Root\n"),
        ]);

        let changed =
            rewrite_links(&workspace, &redirect("a/index.md", "b/index.md"), None).unwrap();

        assert_eq!(changed, ["c/x.md"]);
        assert_eq!(
            text(&workspace, "c/x.md"),
            "---\ntype: Note\n---\n[rel](../b/) [abs](/b/) [file](../b/index.md#top) \
             [root](../index.md)\n"
        );
    }

    #[test]
    fn relink_leaves_code_spans_and_blocks_untouched() {
        let workspace = MemWorkspace::from_files(MOVE_FILES);
        let text = "[a](alpha.md) `[a](alpha.md)` [b](alpha.md)\n\
                    ```\n[a](alpha.md)\n```\n    [a](alpha.md)\n";
        let redirect = redirect("moves/alpha.md", "moves/renamed.md");

        let rewritten = rewrite_text(
            &workspace,
            "moves/beta.md",
            "moves/beta.md",
            text,
            &redirect,
        )
        .expect("the plain links are rewritten");

        assert_eq!(
            rewritten,
            "[a](renamed.md) `[a](alpha.md)` [b](renamed.md)\n\
             ```\n[a](alpha.md)\n```\n    [a](alpha.md)\n"
        );
    }

    #[test]
    fn unlink_leaves_code_spans_and_blocks_untouched() {
        let workspace = MemWorkspace::from_files(MOVE_FILES);
        let text = "[a](alpha.md) `[a](alpha.md)`\n```\n[a](alpha.md)\n```\n";

        let unlinked = unlink_text(&workspace, "moves/beta.md", text, "moves/alpha.md")
            .expect("the plain link is unlinked");

        assert_eq!(unlinked, "a `[a](alpha.md)`\n```\n[a](alpha.md)\n```\n");
        assert!(unlink_text(
            &workspace,
            "moves/beta.md",
            "`[a](alpha.md)`",
            "moves/alpha.md"
        )
        .is_none());
    }

    #[test]
    fn rewrite_text_is_none_when_nothing_points_at_the_source() {
        let workspace = MemWorkspace::default();
        let text = "Plain [web](https://example.com/a.md) and [other](b.md) and [x](#x).\n";

        let result = rewrite_text(&workspace, "a.md", "a.md", text, &redirect("c.md", "d.md"));

        assert_eq!(result, None);
    }

    #[test]
    fn redirect_matches_anchors_after_decoding_and_lower_casing() {
        let anchors = BTreeMap::from([("open items".to_string(), Some("todo".to_string()))]);
        let redirect = Redirect {
            from: "a.md".to_string(),
            to: "b.md".to_string(),
            anchors: Some(anchors),
        };

        let decoded = redirect.apply("a.md", Some("Open%20Items"));
        let unnamed = redirect.apply("a.md", Some("other"));
        let bare = redirect.apply("a.md", None);
        let elsewhere = redirect.apply("z.md", Some("open items"));

        assert_eq!(
            decoded,
            Some(("b.md".to_string(), Some("todo".to_string())))
        );
        assert_eq!((unnamed, bare, elsewhere), (None, None, None));
    }
}
