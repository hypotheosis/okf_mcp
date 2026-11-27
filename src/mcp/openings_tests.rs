//! Issue #1174: what a model sees of a tool before it loads it.
//!
//! Some clients defer tools: until the model loads one, it sees the tool's name and the first
//! 31 characters of its description. So those characters must say what the tool is for, in plain
//! words, not how it works. Every tool has an approved opening in [`OPENINGS`]; the test fails for
//! a tool with none, so a new tool cannot ship without deciding its label.

use std::collections::BTreeSet;

use crate::mcp::{OkfServer, TOOL_REFERENCE_URL};

/// How much of a description a deferring client shows.
const SHOWN: usize = 31;

/// Every tool's approved opening: at most [`SHOWN`] characters, the description's first
/// sentence, saying what the tool is for. `guide` names the product in a session ("Read first:
/// how to use Telekin", see `guide::tests`); the unbranded text here is what
/// `OkfServer::tools` lists.
const OPENINGS: &[(&str, &str)] = &[
    ("add_source", "Note where a fact came from"),
    ("cited_by", "Find notes citing a source"),
    ("create_concept", "Save what the person wants kept"),
    ("delete_concept", "Remove a note from memory"),
    ("discard", "Throw away unsaved changes"),
    ("edit_document", "Change wording in a saved note"),
    ("get_concept", "Read one saved note in full"),
    ("guide", "Read first: how to use this"),
    ("history", "See how a note has changed"),
    ("index", "List a note in its folder"),
    ("links_report", "Find broken or missing links"),
    ("lint", "Check notes for mistakes"),
    ("list_catalogs", "See which catalogs you can use"),
    ("list_changesets", "Find drafts that are still open"),
    ("log", "Add a line to the change log"),
    ("maintenance_claim", "Upkeep: take a batch to check"),
    ("maintenance_close", "Upkeep: finish a checking run"),
    ("maintenance_dispose", "Upkeep: act on a finding"),
    ("maintenance_get_finding", "Upkeep: read one finding"),
    ("maintenance_get_run", "Upkeep: read a checking run"),
    ("maintenance_list_findings", "Upkeep: list what was found"),
    ("maintenance_list_runs", "Upkeep: list checking runs"),
    ("maintenance_read_revision", "Upkeep: read an old version"),
    ("maintenance_renew", "Upkeep: keep your batch"),
    ("maintenance_submit", "Upkeep: report what you found"),
    ("move_concept", "Move or rename a saved note"),
    ("oldest", "Find notes not checked lately"),
    ("open_changeset", "Step 1 of saving to memory"),
    ("open_questions", "List questions left on notes"),
    ("pending_changes", "Preview what you are saving"),
    ("publish", "Final step of saving to memory"),
    ("question", "Flag a doubt on a saved note"),
    ("read_document", "Read a catalog's index or log"),
    ("recent_changes", "See what changed lately"),
    ("remove_source", "Drop a source from a note"),
    ("repair_indexes", "Fix missing index and log files"),
    ("replace_body", "Rewrite the text of a note"),
    ("revert", "Undo a change to a saved note"),
    ("review", "Record that a note was checked"),
    ("search", "Recall what the person told you"),
    ("send_feedback", "Send feedback to the team"),
    ("set_frontmatter", "Edit a note's tags and details"),
    ("split_concept", "Split a long note in two"),
    ("sprawl", "Find notes that grew too long"),
    ("stamp", "Mark a note written or checked"),
    ("unquestion", "Clear a doubt on a saved note"),
    ("whoami", "Who you act for, and the time"),
];

/// What a deferring client shows of `description`: its first [`SHOWN`] characters.
fn shown(description: &str) -> String {
    description.chars().take(SHOWN).collect()
}

/// Every tool's description, without the reference link `OkfServer::tools` appends.
fn descriptions() -> Vec<(String, String)> {
    OkfServer::tools()
        .list_all()
        .into_iter()
        .map(|tool| {
            let full = tool.description.as_deref().unwrap_or_default();
            let suffix = format!(" Reference: {TOOL_REFERENCE_URL}#{}", tool.name);
            let own = full.strip_suffix(&suffix).unwrap_or(full);
            (tool.name.to_string(), own.to_string())
        })
        .collect()
}

#[test]
fn every_tool_has_exactly_one_approved_opening() {
    let registered: BTreeSet<String> = descriptions().into_iter().map(|(name, _)| name).collect();
    let approved: BTreeSet<String> = OPENINGS.iter().map(|(name, _)| name.to_string()).collect();

    assert_eq!(registered, approved, "OPENINGS and the tool list differ");
    assert_eq!(OPENINGS.len(), approved.len(), "a tool is listed twice");
}

#[test]
fn every_description_opens_with_its_approved_opening_in_plain_words() {
    let approved: std::collections::BTreeMap<_, _> = OPENINGS.iter().copied().collect();
    for (name, description) in descriptions() {
        let opening = approved[name.as_str()];
        assert!(
            opening.chars().count() <= SHOWN,
            "{name}: {opening:?} is longer than {SHOWN} characters"
        );
        assert!(
            description.starts_with(opening),
            "{name} does not open with {opening:?}: {description:?}"
        );
        // The opening is a whole phrase: the text after it starts a new sentence, so the cut
        // never lands inside a word of the label.
        let rest = &description[opening.len()..];
        assert!(
            rest.starts_with(". ") || rest.starts_with(' '),
            "{name}: the opening runs into the next word: {rest:?}"
        );
        // Plain words, not mechanics: no code, paths, parameters or protocol terms.
        for mechanic in ['`', '/', '{', '(', '_'] {
            assert!(
                !shown(&description).contains(mechanic),
                "{name}: {:?} shows mechanics ({mechanic:?})",
                shown(&description)
            );
        }
        for mechanic in ["changeset `", "Replace exact", "Report-only"] {
            assert!(!shown(&description).contains(mechanic), "{name}");
        }
    }
}

#[test]
fn the_save_and_recall_tools_open_as_pinned() {
    let by_name: std::collections::BTreeMap<_, _> = descriptions().into_iter().collect();

    assert_eq!(
        shown(&by_name["create_concept"]),
        "Save what the person wants kept"
    );
    assert_eq!(shown(&by_name["search"]), "Recall what the person told you");
    assert_eq!(
        shown(&by_name["open_changeset"]),
        "Step 1 of saving to memory. Ope"
    );
    assert_eq!(
        shown(&by_name["publish"]),
        "Final step of saving to memory."
    );
}

#[test]
fn the_save_and_recall_tools_use_the_words_people_use_early() {
    let by_name: std::collections::BTreeMap<_, _> = descriptions().into_iter().collect();
    let early = |name: &str| -> String {
        by_name[name]
            .chars()
            .take(300)
            .collect::<String>()
            .to_lowercase()
    };

    for word in ["save", "remember"] {
        assert!(
            early("create_concept").contains(word),
            "create_concept: {word}"
        );
    }
    for word in ["recall", "remember", "notes"] {
        assert!(early("search").contains(word), "search: {word}");
    }
    for word in ["saving", "remember"] {
        assert!(
            early("open_changeset").contains(word),
            "open_changeset: {word}"
        );
    }
}
