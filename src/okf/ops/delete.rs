//! `delete_concept` (design "Write tools"; `okf.py` has no counterpart).
//! It removes a concept and its folder listing, turns its links in `log.md`
//! files into plain text, and refuses while any other document links to it
//! ([`OkfError::StillLinked`], listing those links).
//!
//! What counts as a link (plan Decision 4) is what publish's link check
//! reads: every markdown file in the walk except the root `AGENTS.md`,
//! `CLAUDE.md` and `README.md`, so `index.md` files count. The folder's own
//! listing (the first bullet of its `index.md` linking the concept, which
//! this op removes) and the concept's links to itself do not.
//!
//! Nor do links from a `log.md` (issue #658): only the `log` tool writes
//! one, and it only appends, so a link there could never be removed and the
//! concept could never be deleted. The delete turns each such link into its
//! link text instead (`[X](/infra/x.md)` becomes `X`), with the same link
//! test as the refusal (`relink::links_to`), so nothing is left broken.
//!
//! The listing goes through a [`ReplayOp::Unlist`] (any bullet, since it is
//! deleted either way) and each `log.md` through a [`ReplayOp::Unlink`], so
//! publish replays them onto newer text: a `log` entry another changeset
//! publishes meanwhile does not conflict. The concept is removed with
//! [`MemWorkspace::remove`], and `write::apply` stores a deletion. An
//! emptied folder keeps its `index.md`; a note says so.

use crate::error::{LinkSource, OkfError};
use crate::okf::ops::relink::{links_to, unlink_text};
use crate::okf::ops::replay::{listing_line, unlist_text, ReplayOp};
use crate::okf::ops::OpOutcome;
use crate::okf::paths::{basename, dirname, join, outside_link_graph, resolve_concept};
use crate::okf::rules::check_path;
use crate::okf::workspace::{MemWorkspace, Workspace};

/// The name of the files a delete unlinks instead of refusing for.
const LOG: &str = "log.md";

/// Deletes a concept and its listing in its folder's `index.md`, and turns
/// its links in `log.md` files into their link text.
///
/// # Arguments
/// * `workspace` - the catalog (the write path's scratch copy of an overlay).
/// * `reference` - the concept, `/dir/x.md`, `dir/x` or `/dir/x`.
///
/// # Returns
/// The outcome: the concept (removed); if it was listed, the folder index
/// with its `Unlist` op; each `log.md` that linked it, with its `Unlink` op
/// and a note counting the links; the message `deleted /<path>`; a note
/// when the folder is left holding only its `index.md`.
///
/// # Errors
/// - As [`resolve_concept`] ([`OkfError::NotFound`]; [`OkfError::InvalidPath`]
///   for `index.md`, `log.md` and the root readmes).
/// - [`OkfError::InvalidPath`] for a concept in machinery ([`check_path`]).
/// - [`OkfError::StillLinked`] while another document, not a `log.md`,
///   links to it.
/// - The workspace's read or write error. Every walked markdown file is read,
///   so one that is not UTF-8, anywhere in the catalog, is [`OkfError::Io`],
///   as it is for `Snapshot::build` (and so for publish). A stored catalog
///   holds none: import refuses such a file and the write tools write only
///   UTF-8.
#[tracing::instrument(level = "debug", skip_all)]
pub fn delete_concept(workspace: &MemWorkspace, reference: &str) -> Result<OpOutcome, OkfError> {
    let path = check_path(&resolve_concept(workspace, reference)?)?;
    let folder = dirname(&path);
    let index = join(folder, "index.md");
    let index_text = if workspace.is_file(&index) {
        Some(workspace.read_text(&index)?)
    } else {
        None
    };
    // `as_deref` views `Option<String>` as `Option<&str>`; `and_then` chains
    // the lookup, like `text and listing_line(...)` in Python.
    let listing = index_text
        .as_deref()
        .and_then(|text| listing_line(workspace, text, &index, &path));
    // `partition` splits one list in two by a test, like two list
    // comprehensions over the same items: the `log.md` links, then the rest.
    let (logged, links): (Vec<LinkSource>, Vec<LinkSource>) =
        links_into(workspace, &path, &index, listing)?
            .into_iter()
            .partition(|link| basename(&link.source) == LOG);
    if !links.is_empty() {
        // Catalog-relative with a leading `/`, as tool results (`touched`,
        // `message`) and `Conflict` write them.
        return Err(OkfError::StillLinked {
            path: format!("/{path}"),
            links,
        });
    }
    let unlisted = index_text
        .as_deref()
        .and_then(|text| unlist_text(workspace, text, &index, &path));
    workspace.remove(&path)?;
    let mut outcome = OpOutcome::default();
    outcome.touch(&path);
    if let Some((rest, _)) = unlisted {
        workspace.write_atomic(&index, &rest)?;
        let op = ReplayOp::Unlist {
            target: path.clone(),
            bullet: None,
        };
        outcome.record(&index, op);
    }
    unlink_logs(workspace, &path, &logged, &mut outcome)?;
    if !folder.is_empty() && only_the_index_is_left(workspace, folder, &index)? {
        outcome.notes.push(format!(
            "note: /{folder}/ now holds only its index.md, which stays; delete_concept never \
             deletes an index"
        ));
    }
    outcome.message = format!("deleted /{path}");
    Ok(outcome)
}

/// Every link into `path` from another document, except the folder
/// listing's line (`listing`, a 0-based line of `index`). Each source is
/// written with a leading `/` (`/hub.md`), as tool results (`touched`,
/// `message`) and `Conflict` write them.
///
/// # Errors
/// The workspace's read error.
#[tracing::instrument(level = "trace", skip_all)]
fn links_into(
    workspace: &dyn Workspace,
    path: &str,
    index: &str,
    listing: Option<usize>,
) -> Result<Vec<LinkSource>, OkfError> {
    let mut links = Vec::new();
    for source in workspace.list_markdown()? {
        if outside_link_graph(&source) || source == path {
            continue;
        }
        // Universal newlines, as the snapshot (and so lint) numbers lines.
        let text = workspace.read_text(&source)?;
        for (number, line) in text.split('\n').enumerate() {
            if source == index && listing == Some(number) {
                continue;
            }
            // In-page links (`#x`) resolve to `None`, so they never count.
            // `links_to` is also the test `unlink_text` applies to `log.md`.
            let hits = links_to(workspace, line, &source, path).len();
            // `repeat_with(..).take(n)`: one entry per link on the line.
            links.extend(
                std::iter::repeat_with(|| LinkSource {
                    source: format!("/{source}"),
                    line: number + 1,
                })
                .take(hits),
            );
        }
    }
    Ok(links)
}

/// Turns the links into `path` in each `log.md` of `logged` into their link
/// text, recording a [`ReplayOp::Unlink`] and a note per file.
///
/// # Arguments
/// * `workspace` - the catalog.
/// * `path` - the deleted concept.
/// * `logged` - its links from `log.md` files, as [`links_into`] lists them
///   (walk order, then line order; sources with a leading `/`).
/// * `outcome` - the delete's outcome, extended in place.
///
/// # Errors
/// The workspace's read or write error. [`OkfError::Io`] (an internal error)
/// when `logged` holds an empty run or a file with no link into `path`:
/// [`links_into`] never produces either, so it is a bug, and failing here
/// beats a publish refusal that names no cause.
#[tracing::instrument(level = "trace", skip_all)]
fn unlink_logs(
    workspace: &dyn Workspace,
    path: &str,
    logged: &[LinkSource],
    outcome: &mut OpOutcome,
) -> Result<(), OkfError> {
    // `chunk_by` groups neighbouring items with the same source, like
    // `itertools.groupby`: `logged` is in walk order, so each file is one run.
    for run in logged.chunk_by(|a, b| a.source == b.source) {
        let Some(first) = run.first() else {
            return Err(invariant_broken("", "an empty run of links"));
        };
        let log = first.source.trim_start_matches('/');
        // Byte for byte, as the replay rewrites it (`\r\n` endings kept).
        let text = workspace.read_to_string(log)?;
        let Some(unlinked) = unlink_text(workspace, log, &text, path) else {
            return Err(invariant_broken(log, "no link to the deleted concept"));
        };
        workspace.write_atomic(log, &unlinked)?;
        let op = ReplayOp::Unlink {
            target: path.to_string(),
        };
        outcome.record(log, op);
        let count = run.len();
        let links = if count == 1 { "link" } else { "links" };
        outcome.notes.push(format!(
            "note: /{log}: {count} {links} to /{path} became plain text"
        ));
    }
    Ok(())
}

/// The internal error for a state `delete_concept` never reaches.
///
/// # Arguments
/// * `path` - the file concerned (empty when there is none).
/// * `what` - what was found.
///
/// # Returns
/// An [`OkfError::Io`], which [`OkfError::is_internal`] counts as the
/// server's fault: clients see a generic message and the log has the detail.
#[tracing::instrument(level = "trace", skip_all)]
fn invariant_broken(path: &str, what: &str) -> OkfError {
    OkfError::Io {
        path: path.to_string(),
        source: std::io::Error::other(format!("delete_concept invariant broken: {what}")),
    }
}

/// Does `folder` hold its `index.md` and nothing else?
///
/// # Errors
/// The workspace's listing error (none for a `MemWorkspace`).
#[tracing::instrument(level = "trace", skip_all)]
fn only_the_index_is_left(
    workspace: &dyn Workspace,
    folder: &str,
    index: &str,
) -> Result<bool, OkfError> {
    let prefix = format!("{folder}/");
    // The `is_file` check first: a folder whose last file was the concept,
    // and which never had an index, is simply gone.
    Ok(workspace.is_file(index)
        && workspace
            .list_files()?
            .iter()
            .filter(|file| file.starts_with(&prefix))
            .all(|file| file == index))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::okf::markdown::find_links;
    use crate::okf::ops::replay::replay;

    const INFRA_INDEX: &str = concat!(
        "# Infra\n\n# Concepts\n\n* [X](x.md) - x\n* [Y](y.md) - y\n",
        "* [X again](/infra/x.md) - listed twice\n* [W](w.md) - w\n",
    );
    const FILES: [(&str, &str); 11] = [
        (
            "index.md",
            "# Root\n\n# Sections\n\n* [Infra](infra/) - systems\n\nSee the [hub](/hub.md).\n",
        ),
        ("AGENTS.md", "Read [x](infra/x.md).\n"),
        (
            "hub.md",
            "---\ntype: Note\ntitle: Hub\n---\nSee [x](/infra/x.md#ports) and [y](infra/y.md).\n",
        ),
        (
            "log.md",
            "# Log\n\n## 2026-11-01\n\n* **Creation:** Added [x](/infra/x.md).\n",
        ),
        ("infra/index.md", INFRA_INDEX),
        (
            "infra/x.md",
            "---\ntype: Note\ntitle: X\n---\n# X\n\nSee [itself](#x) and [self](/infra/x.md).\n",
        ),
        ("infra/y.md", "---\ntype: Note\ntitle: Y\n---\n# Y\n"),
        ("infra/w.md", "---\ntype: Note\ntitle: W\n---\n# W\n"),
        ("infra/z.md", "---\ntype: Note\ntitle: Z\n---\n# Z\n"),
        (
            "solo/index.md",
            "# Solo\n\n* [Only](only.md) - the only one\n",
        ),
        (
            "solo/only.md",
            "---\ntype: Note\ntitle: Only\n---\n# Only\n",
        ),
    ];

    fn link(source: &str, line: usize) -> LinkSource {
        LinkSource {
            // Tools return `/`-prefixed paths, `StillLinked`'s included.
            source: format!("/{source}"),
            line,
        }
    }

    #[test]
    fn unlink_logs_fails_hard_on_a_file_with_no_link() {
        let workspace = MemWorkspace::from_files([("log.md", "# Log\n")]);
        let mut outcome = OpOutcome::default();

        let no_link = [link("log.md", 1)];
        let error = unlink_logs(&workspace, "infra/x.md", &no_link, &mut outcome).unwrap_err();

        assert!(error.is_internal(), "{error:?}");
        assert!(error.to_string().contains("no link"), "{error}");
        assert!(outcome.notes.is_empty());
        assert_eq!(workspace.read_text("log.md").unwrap(), "# Log\n");
    }

    #[test]
    fn a_linked_concept_is_refused_with_every_link_but_its_listing() {
        let workspace = MemWorkspace::from_files(FILES);

        let error = delete_concept(&workspace, "/infra/x").unwrap_err();

        // Not counted: the root AGENTS.md, the first bullet in infra/index.md
        // (the listing delete removes), x.md's links to itself, and log.md's
        // link (a delete unlinks it, issue #658).
        assert!(
            matches!(
                error,
                OkfError::StillLinked { ref path, ref links } if path == "/infra/x.md" && links == &[
                    link("hub.md", 5),
                    link("infra/index.md", 7),
                ]
            ),
            "{error:?}"
        );
        assert!(workspace.is_file("infra/x.md"));
    }

    #[test]
    fn a_relative_link_counts_too() {
        let workspace = MemWorkspace::from_files(FILES);

        let error = delete_concept(&workspace, "infra/y.md").unwrap_err();

        assert!(matches!(
            error,
            OkfError::StillLinked { ref links, .. } if links == &[link("hub.md", 5)]
        ));
    }

    #[test]
    fn an_unlinked_concept_goes_with_its_listing() {
        let workspace = MemWorkspace::from_files(FILES);

        let outcome = delete_concept(&workspace, "infra/w.md").unwrap();

        assert_eq!(outcome.message, "deleted /infra/w.md");
        assert_eq!(outcome.touched, ["infra/w.md", "infra/index.md"]);
        assert!(outcome.notes.is_empty());
        assert!(!workspace.is_file("infra/w.md"));
        let listed = workspace.read_to_string("infra/index.md").unwrap();
        assert_eq!(listed, INFRA_INDEX.replace("* [W](w.md) - w\n", ""));
        let ops = &outcome.replay["infra/index.md"];
        assert_eq!(
            ops,
            &[ReplayOp::Unlist {
                target: "infra/w.md".to_string(),
                bullet: None
            }]
        );
        assert_eq!(
            replay("infra/index.md", Some(INFRA_INDEX), ops).unwrap(),
            listed
        );
    }

    #[test]
    fn an_unlisted_concept_touches_only_itself() {
        let workspace = MemWorkspace::from_files(FILES);

        let outcome = delete_concept(&workspace, "infra/z.md").unwrap();

        assert_eq!(outcome.touched, ["infra/z.md"]);
        assert!(outcome.replay.is_empty());
        assert_eq!(
            workspace.read_to_string("infra/index.md").unwrap(),
            INFRA_INDEX
        );
    }

    #[test]
    fn a_root_level_concept_is_unlisted_from_the_root_index_with_no_note() {
        let workspace = MemWorkspace::from_files([
            (
                "index.md",
                "# Root\n\n* [Top](top.md) - t\n* [Other](other.md) - o\n",
            ),
            ("top.md", "---\ntype: Note\ntitle: Top\n---\n# Top\n"),
            ("other.md", "---\ntype: Note\ntitle: Other\n---\n# Other\n"),
        ]);

        let outcome = delete_concept(&workspace, "top").unwrap();

        // `folder` is "", so the listing is the root `index.md`, and the
        // emptied-folder note never applies to the root.
        assert_eq!(outcome.touched, ["top.md", "index.md"]);
        assert!(outcome.notes.is_empty());
        assert_eq!(
            workspace.read_to_string("index.md").unwrap(),
            "# Root\n\n* [Other](other.md) - o\n"
        );
    }

    #[test]
    fn a_crlf_index_is_unlisted_with_newline_endings_like_its_replay() {
        let crlf = "# Infra\r\n\r\n* [W](w.md) - w\r\n* [Y](y.md) - y\r\n";
        let workspace = MemWorkspace::from_files([
            ("infra/index.md", crlf),
            ("infra/w.md", "---\ntype: Note\ntitle: W\n---\n# W\n"),
            ("infra/y.md", "---\ntype: Note\ntitle: Y\n---\n# Y\n"),
        ]);

        let outcome = delete_concept(&workspace, "infra/w.md").unwrap();

        // `Unlist` works on universal-newline text (plan Task 2), so the
        // op and its replay both write `\n` endings.
        let listed = workspace.read_to_string("infra/index.md").unwrap();
        assert_eq!(listed, "# Infra\n\n* [Y](y.md) - y\n");
        let ops = &outcome.replay["infra/index.md"];
        assert_eq!(replay("infra/index.md", Some(crlf), ops).unwrap(), listed);
    }

    #[test]
    fn an_emptied_folder_keeps_its_index_and_says_so() {
        let workspace = MemWorkspace::from_files(FILES);

        let outcome = delete_concept(&workspace, "solo/only.md").unwrap();

        assert_eq!(
            outcome.notes,
            [
                "note: /solo/ now holds only its index.md, which stays; delete_concept never \
              deletes an index"
            ]
        );
        // Only the bullet's line goes, as `unlist_text` (and its replay) take it.
        assert_eq!(
            workspace.read_to_string("solo/index.md").unwrap(),
            "# Solo\n\n"
        );
    }

    #[test]
    fn non_concepts_machinery_and_missing_paths_are_refused() {
        let mut files = FILES.to_vec();
        files.push(("install/setup.md", "---\ntype: Note\n---\n"));
        let workspace = MemWorkspace::from_files(files);

        let refusals = ["infra/index.md", "log.md", "AGENTS.md", "install/setup.md"]
            .map(|path| delete_concept(&workspace, path).unwrap_err());
        let missing = delete_concept(&workspace, "infra/none").unwrap_err();
        // Plan Decision 4: a machinery reference that names no file is
        // `NotFound`, since `check_path` runs only on a resolved path.
        let missing_machinery = delete_concept(&workspace, "install/none.md").unwrap_err();

        for error in &refusals {
            assert_eq!(error.kind(), "InvalidPath", "{error}");
        }
        assert!(
            refusals[3].to_string().contains("machinery"),
            "{}",
            refusals[3]
        );
        assert!(matches!(missing, OkfError::NotFound { .. }));
        assert!(matches!(missing_machinery, OkfError::NotFound { .. }));
    }

    #[test]
    fn each_link_on_a_line_is_its_own_entry() {
        let mut files = FILES.to_vec();
        files.push((
            "twice.md",
            "See [z](infra/z.md) and [z again](/infra/z.md#top).\n",
        ));
        let workspace = MemWorkspace::from_files(files);

        let error = delete_concept(&workspace, "infra/z.md").unwrap_err();

        assert!(
            matches!(
                error,
                OkfError::StillLinked { ref links, .. }
                    if links == &[link("twice.md", 1), link("twice.md", 1)]
            ),
            "{error:?}"
        );
    }
    /// A `log.md` linking `infra/v.md` in every form the link graph
    /// recognises, with links it must leave alone; `\r\n` endings on its
    /// last line. Line 6 holds the forms, line 7 what is not a link into v.
    const LOG_FORMS: &str = concat!(
        "# Log\n\n## 2026-11-02\n\n",
        "* **Update:** Removed old notes.\n",
        "* **Creation:** abs [A](/infra/v.md), rel [B](infra/v.md), dot [C](./infra/v.md), ",
        "frag [D](/infra/v.md#ports), title [E](/infra/v.md \"the v\"), ",
        "encoded [F](/infra/%76.md), up [H](infra/../infra/v.md#ports \"t\").\n",
        "* **Update:** kept [Y](/infra/y.md), [G](infra/v.md 'single quotes'), [web](https://example.com/infra/v.md), ",
        "[bare](/infra/v), ![img](/infra/v.md), [self](#2026-11-02).\r\n",
    );
    const LOG_UNLINKED: &str = concat!(
        "# Log\n\n## 2026-11-02\n\n",
        "* **Update:** Removed old notes.\n",
        "* **Creation:** abs A, rel B, dot C, frag D, title E, encoded F, up H.\n",
        "* **Update:** kept [Y](/infra/y.md), [G](infra/v.md 'single quotes'), [web](https://example.com/infra/v.md), ",
        "[bare](/infra/v), ![img](/infra/v.md), [self](#2026-11-02).\r\n",
    );

    /// A catalog where `infra/v.md` is linked only from `log.md` files.
    fn logged_files() -> Vec<(&'static str, &'static str)> {
        vec![
            ("index.md", "# Root\n\n* [Infra](infra/) - systems\n"),
            ("log.md", LOG_FORMS),
            (
                "infra/index.md",
                "# Infra\n\n* [V](v.md) - v\n* [Y](y.md) - y\n",
            ),
            (
                "infra/log.md",
                "# Infra log\n\n* Added [v](v.md) and [y](y.md).\n",
            ),
            (
                "infra/v.md",
                "---\ntype: Note\ntitle: V\n---\n# V\n\n## Ports\n",
            ),
            ("infra/y.md", "---\ntype: Note\ntitle: Y\n---\n# Y\n"),
        ]
    }

    #[test]
    fn links_from_log_md_become_their_text_and_replay() {
        let workspace = MemWorkspace::from_files(logged_files());

        let outcome = delete_concept(&workspace, "/infra/v").unwrap();

        assert_eq!(outcome.message, "deleted /infra/v.md");
        assert_eq!(
            outcome.touched,
            ["infra/v.md", "infra/index.md", "log.md", "infra/log.md"]
        );
        let log = workspace.read_to_string("log.md").unwrap();
        assert_eq!(log, LOG_UNLINKED);
        let infra_log = workspace.read_to_string("infra/log.md").unwrap();
        assert_eq!(infra_log, "# Infra log\n\n* Added v and [y](y.md).\n");
        let unlink = ReplayOp::Unlink {
            target: "infra/v.md".to_string(),
        };
        // `from_ref` views one value as a one-item slice, without a copy.
        let ops = std::slice::from_ref(&unlink);
        assert_eq!(outcome.replay["log.md"], ops);
        assert_eq!(outcome.replay["infra/log.md"], ops);
        assert_eq!(
            outcome.notes,
            [
                "note: /log.md: 7 links to /infra/v.md became plain text",
                "note: /infra/log.md: 1 link to /infra/v.md became plain text",
            ]
        );
        // The replay redoes the edit, and again changes nothing.
        let replayed = replay("log.md", Some(LOG_FORMS), ops).unwrap();
        assert_eq!(replayed, log);
        assert_eq!(replay("log.md", Some(&replayed), ops).unwrap(), log);
    }

    #[test]
    fn a_log_md_link_does_not_refuse_but_other_links_still_do() {
        let mut files = logged_files();
        files.push(("hub.md", "---\ntype: Note\n---\nSee [v](/infra/v.md).\n"));
        files.push((
            "people/index.md",
            "# People\n\n* Owner of [v](../infra/v.md)\n",
        ));
        let workspace = MemWorkspace::from_files(files);

        let error = delete_concept(&workspace, "/infra/v").unwrap_err();

        // Only the concept and the other index.md: the log links are not listed.
        assert!(
            matches!(
                error,
                OkfError::StillLinked { ref links, .. }
                    if links == &[link("hub.md", 4), link("people/index.md", 3)]
            ),
            "{error:?}"
        );
        // Nothing was written.
        assert_eq!(workspace.read_to_string("log.md").unwrap(), LOG_FORMS);
        assert!(workspace.is_file("infra/v.md"));
    }

    /// `move_concept` already rewrites links in `log.md` (a `Relink`); a
    /// delete unlinks exactly the links a move redirects, line for line.
    #[test]
    fn a_delete_unlinks_the_log_links_a_move_redirects() {
        let moving = MemWorkspace::from_files(logged_files());
        let deleting = MemWorkspace::from_files(logged_files());

        let moved =
            crate::okf::ops::move_concept(&moving, "/infra/v", "/infra/w.md", None).unwrap();
        delete_concept(&deleting, "/infra/v").unwrap();

        assert!(matches!(
            moved.replay["log.md"].as_slice(),
            [ReplayOp::Relink { .. }]
        ));
        for log in ["log.md", "infra/log.md"] {
            let before = MemWorkspace::from_files(logged_files())
                .read_to_string(log)
                .unwrap();
            let after_move = moving.read_to_string(log).unwrap();
            let after_delete = deleting.read_to_string(log).unwrap();
            let lines = |text: &str| text.split('\n').map(str::to_string).collect::<Vec<_>>();
            let (before, after_move, after_delete) =
                (lines(&before), lines(&after_move), lines(&after_delete));
            for (number, line) in before.iter().enumerate() {
                let links = find_links(line);
                // A link the move redirected is one the delete removed.
                let redirected = links
                    .iter()
                    .zip(find_links(&after_move[number]))
                    .filter(|(old, new)| old.target != new.target)
                    .count();
                let removed = links.len() - find_links(&after_delete[number]).len();
                assert_eq!(redirected, removed, "{log} line {}: {line}", number + 1);
            }
        }
    }
}
