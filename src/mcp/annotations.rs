//! The connector directories' tool metadata (M10 Task 1): every tool has a
//! title and all four behaviour hints, and is classified exactly once here,
//! so a new tool cannot ship without deciding what it does to the catalog.
//!
//! The hints themselves live on each `#[tool(...)]` attribute; this module
//! is the single list they are checked against.

#[cfg(test)]
use std::collections::BTreeSet;

#[cfg(test)]
use crate::mcp::OkfServer;

/// Tools that only read: never change a catalog, a changeset or a run.
pub(crate) const READS: &[&str] = &[
    "cited_by",
    "guide",
    "get_concept",
    "history",
    "links_report",
    "lint",
    "list_catalogs",
    "list_changesets",
    "maintenance_get_finding",
    "maintenance_get_run",
    "maintenance_list_findings",
    "maintenance_list_runs",
    "maintenance_read_revision",
    "oldest",
    "open_questions",
    "pending_changes",
    "read_document",
    "recent_changes",
    "search",
    "sprawl",
    "whoami",
];

/// Tools whose effect stays unpublished (a changeset entry, a maintenance
/// lease or report) and that only add: they never overwrite or remove
/// existing content, so they are `destructiveHint: false` (the MCP spec's
/// "only additive updates").
pub(crate) const ADDITIVE: &[&str] = &[
    "add_source",
    "create_concept",
    "log",
    "maintenance_claim",
    "maintenance_dispose",
    "maintenance_renew",
    "maintenance_submit",
    "open_changeset",
    "question",
];

/// Tools whose effect is staged in a changeset (`discard` drops it) but that
/// can overwrite or remove existing content. The MCP spec defines
/// `destructiveHint` by what the tool does, not by whether it can be undone,
/// so these are destructive too (issue #1062).
pub(crate) const STAGED_DESTRUCTIVE: &[&str] = &[
    "edit_document",
    "index",
    "move_concept",
    "remove_source",
    "repair_indexes",
    "replace_body",
    "review",
    "revert",
    "set_frontmatter",
    "split_concept",
    "stamp",
    "unquestion",
];

/// Tools that act at once and cannot be undone from inside the server, or
/// that delete.
pub(crate) const DESTRUCTIVE: &[&str] =
    &["delete_concept", "discard", "maintenance_close", "publish"];

/// Tools that reach a service outside this server (open-world): they change
/// no catalog, but what they send cannot be called back, so they are
/// destructive too.
pub(crate) const EXTERNAL: &[&str] = &["send_feedback"];

/// The hints a tool's class implies (`open_world` is true only for
/// [`EXTERNAL`]: every other tool acts only on this server's catalogs).
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Hints {
    read_only: bool,
    destructive: bool,
    idempotent: bool,
    open_world: bool,
}

/// The hints for `name`'s class, or `None` for an unclassified tool.
#[cfg(test)]
fn expected(name: &str) -> Option<Hints> {
    // `contains(&name)`: the slices hold `&str`, so the needle is a `&&str`.
    if READS.contains(&name) {
        Some(Hints {
            read_only: true,
            destructive: false,
            idempotent: true,
            open_world: false,
        })
    } else if ADDITIVE.contains(&name) {
        Some(Hints {
            read_only: false,
            destructive: false,
            idempotent: false,
            open_world: false,
        })
    } else if STAGED_DESTRUCTIVE.contains(&name) || DESTRUCTIVE.contains(&name) {
        Some(Hints {
            read_only: false,
            destructive: true,
            idempotent: false,
            open_world: false,
        })
    } else if EXTERNAL.contains(&name) {
        Some(Hints {
            read_only: false,
            destructive: true,
            idempotent: false,
            open_world: true,
        })
    } else {
        None
    }
}

/// Is every word of `title` capitalized ("List Catalogs")?
#[cfg(test)]
fn is_title_case(title: &str) -> bool {
    !title.is_empty()
        && title
            .split(' ')
            .all(|word| word.chars().next().is_some_and(|c| c.is_ascii_uppercase()))
}

#[test]
fn every_tool_is_classified_exactly_once() {
    let registered: BTreeSet<String> = OkfServer::tools()
        .list_all()
        .into_iter()
        // `name` is a `Cow<'static, str>`; `into_owned` gives the `String`.
        .map(|tool| tool.name.into_owned())
        .collect();
    let mut classified = BTreeSet::new();

    for name in READS
        .iter()
        .chain(ADDITIVE)
        .chain(STAGED_DESTRUCTIVE)
        .chain(DESTRUCTIVE)
        .chain(EXTERNAL)
    {
        // `insert` returns false for a value already in the set.
        assert!(
            classified.insert((*name).to_string()),
            "{name} is in more than one class"
        );
    }

    assert_eq!(
        registered, classified,
        "the registered tools and the classified tools differ"
    );
    assert_eq!(registered.len(), 47);
}

#[test]
fn every_tool_has_a_title_and_all_four_hints() {
    for tool in OkfServer::tools().list_all() {
        let name = tool.name.as_ref();
        let title = tool
            .title
            .as_deref()
            .unwrap_or_else(|| panic!("{name} has no title"));
        let annotations = tool
            .annotations
            .as_ref()
            .unwrap_or_else(|| panic!("{name} has no annotations"));
        let hints = expected(name).unwrap_or_else(|| panic!("{name} is not classified"));

        assert!(is_title_case(title), "{name}: {title:?} is not Title Case");
        assert_eq!(
            annotations.title.as_deref(),
            Some(title),
            "{name}: the annotation title differs from the tool title"
        );
        assert_eq!(annotations.read_only_hint, Some(hints.read_only), "{name}");
        assert_eq!(
            annotations.destructive_hint,
            Some(hints.destructive),
            "{name}"
        );
        assert_eq!(
            annotations.idempotent_hint,
            Some(hints.idempotent),
            "{name}"
        );
        assert_eq!(
            annotations.open_world_hint,
            Some(hints.open_world),
            "{name}"
        );
    }
}

#[test]
fn a_read_only_tool_is_never_destructive() {
    for tool in OkfServer::tools().list_all() {
        let annotations = tool.annotations.expect("annotated");

        if annotations.read_only_hint == Some(true) {
            assert_eq!(annotations.destructive_hint, Some(false), "{}", tool.name);
        }
    }
}

#[test]
fn the_hints_go_on_the_wire_in_camel_case() {
    let search = OkfServer::tools()
        .list_all()
        .into_iter()
        .find(|tool| tool.name == "search")
        .expect("search is registered");

    let json = serde_json::to_value(&search).expect("a tool serializes");

    assert_eq!(json["title"], "Search Concepts");
    assert_eq!(json["annotations"]["title"], "Search Concepts");
    assert_eq!(json["annotations"]["readOnlyHint"], true);
    assert_eq!(json["annotations"]["destructiveHint"], false);
    assert_eq!(json["annotations"]["idempotentHint"], true);
    assert_eq!(json["annotations"]["openWorldHint"], false);
}

/// Every `description` string in a JSON value: the tool's own and those in its schemas.
#[cfg(test)]
fn collect_descriptions(value: &serde_json::Value, found: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                match child.as_str() {
                    Some(text) if key == "description" => found.push(text.to_string()),
                    _ => collect_descriptions(child, found),
                }
            }
        }
        serde_json::Value::Array(items) => {
            items
                .iter()
                .for_each(|item| collect_descriptions(item, found));
        }
        _ => {}
    }
}

/// Words that mark a contributor note, not text for a client: the reference CLI the server
/// was ported from, and Rust-for-Python-developers asides.
#[cfg(test)]
const CONTRIBUTOR_WORDS: &[&str] = &["okf.py", "Python", "Rust", "`Copy`", "rename_all", "serde"];

#[test]
fn no_tool_text_names_okf_py_or_carries_a_contributor_note() {
    let mut offenders = BTreeSet::new();

    for tool in OkfServer::tools().list_all() {
        let json = serde_json::to_value(&tool).expect("a tool serializes");
        let mut descriptions = Vec::new();
        collect_descriptions(&json, &mut descriptions);

        for text in descriptions {
            for word in CONTRIBUTOR_WORDS {
                if text.contains(word) {
                    offenders.insert(format!("{word:?}: {text}"));
                }
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "tool descriptions with contributor text:\n{}",
        offenders.into_iter().collect::<Vec<_>>().join("\n")
    );
}

/// The tool parameters the directory review flagged ("Add a type to this
/// parameter") that have not been fixed yet, as `(tool, parameter)` pairs.
///
/// This list must stay empty: epic #507 requires it, and every tool parameter
/// now has a direct `type`. Keep the constant so the stale-entry check in
/// [`every_tool_parameter_has_a_direct_type`] still means something; a new
/// untyped parameter must be fixed, not listed here.
#[cfg(test)]
const KNOWN_UNTYPED_PARAMETERS: &[(&str, &str)] = &[];

/// Does this property schema carry a direct `type` keyword?
///
/// A bare `$ref`, an `anyOf` or `oneOf` without `type`, and the `true` and
/// `{}` any-value schemas all fail, which is what the directory's reviewer
/// checks. For an array it also requires the `items` schema to be typed.
#[cfg(test)]
fn has_direct_type(schema: &serde_json::Value) -> bool {
    let Some(object) = schema.as_object() else {
        return false;
    };
    let Some(kind) = object.get("type") else {
        return false;
    };
    let is_array = kind == "array"
        || kind
            .as_array()
            .is_some_and(|kinds| kinds.iter().any(|k| k == "array"));
    // A missing `items` on an array is not checked: only a present items
    // schema can be untyped.
    !is_array || object.get("items").is_none_or(has_direct_type)
}

/// Every `(tool, parameter)` whose top-level input property has no direct
/// `type` (see [`has_direct_type`]), sorted.
#[cfg(test)]
fn untyped_parameters() -> BTreeSet<(String, String)> {
    let mut found = BTreeSet::new();

    for tool in OkfServer::tools().list_all() {
        let schema = serde_json::Value::Object((*tool.input_schema).clone());
        let Some(properties) = schema.get("properties").and_then(|p| p.as_object()) else {
            continue;
        };

        for (parameter, property) in properties {
            if !has_direct_type(property) {
                found.insert((tool.name.to_string(), parameter.clone()));
            }
        }
    }
    found
}

/// Every tool parameter schema carries a direct `type`, apart from the known
/// offenders in [`KNOWN_UNTYPED_PARAMETERS`].
///
/// This is the Claude connector directory's "Add a type to this parameter"
/// check, run locally so a new tool cannot regress it.
///
/// Fix pattern (decided in issue #508): fix each field on its own with a
/// `#[schemars(...)]` attribute (or a `schema_with` function) that inlines the
/// type, as each of #509 to #513 does for its parameter. A generator-wide
/// "inline subschemas" setting is not used here. At most it would help the
/// three `$ref` cases (`disposition`, `submission`, `field`: #509 to #511),
/// and only where the inlined schema itself has a top-level `type`. It cannot
/// help `question` (an untagged enum: `anyOf` of integer and string, #512),
/// `value` (any JSON value, #513) or `log.kind` (`anyOf` with null). It would
/// also reshape every other tool's schema and the
/// `investigator/contract_tools.json` snapshot at once.
#[test]
fn every_tool_parameter_has_a_direct_type() {
    let allowed: BTreeSet<(String, String)> = KNOWN_UNTYPED_PARAMETERS
        .iter()
        .map(|(tool, parameter)| ((*tool).to_string(), (*parameter).to_string()))
        .collect();
    let untyped = untyped_parameters();

    let new: Vec<_> = untyped.difference(&allowed).collect();
    assert!(
        new.is_empty(),
        "these tool parameters have no direct `type` (no bare $ref, anyOf/oneOf without \
         type, or any-value schema): {new:?}"
    );

    let stale: Vec<_> = allowed.difference(&untyped).collect();
    assert!(
        stale.is_empty(),
        "these parameters are typed now; remove them from KNOWN_UNTYPED_PARAMETERS: {stale:?}"
    );
}

#[test]
fn the_type_check_rejects_the_shapes_the_reviewer_rejects() {
    use serde_json::json;

    assert!(has_direct_type(&json!({"type": "string"})));
    assert!(has_direct_type(&json!({"type": ["string", "null"]})));
    assert!(has_direct_type(
        &json!({"type": "array", "items": {"type": "string"}})
    ));
    assert!(!has_direct_type(&json!({"$ref": "#/$defs/Thing"})));
    assert!(!has_direct_type(
        &json!({"anyOf": [{"type": "integer"}, {"type": "string"}]})
    ));
    assert!(!has_direct_type(
        &json!({"oneOf": [{"type": "integer"}, {"type": "string"}]})
    ));
    assert!(!has_direct_type(&json!({})));
    assert!(!has_direct_type(&json!(true)));
    assert!(!has_direct_type(&json!({"type": "array", "items": {}})));
    assert!(!has_direct_type(
        &json!({"type": "array", "items": {"$ref": "#/$defs/Thing"}})
    ));
}
