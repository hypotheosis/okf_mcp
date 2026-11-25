//! The public tool reference, `docs/reference/tools.md`: one section per tool, rendered from
//! the live tool list so the page cannot drift from what `tools/list` sends. Every tool's
//! description ends with a link to its heading here (see [`super::TOOL_REFERENCE_URL`]).
//!
//! A test renders the page and fails when the checked-in file differs. To refresh the file after
//! changing a tool, run `OKF_UPDATE_TOOL_REFERENCE=1 cargo test --lib mcp::reference`.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use rmcp::model::Tool;
use serde_json::Value;

use crate::mcp::{OkfServer, TOOL_REFERENCE_URL};

/// The checked-in page, relative to the crate root.
const PAGE: &str = "docs/reference/tools.md";

/// The page's opening, up to the tool index.
const INTRO: &str = "# Telekin tool reference

<!-- Generated from the server's tool list by `src/mcp/reference.rs`. Do not edit by hand:
run `OKF_UPDATE_TOOL_REFERENCE=1 cargo test --lib mcp::reference` to refresh it. -->

Telekin serves Open Knowledge Format (OKF) catalogs over MCP: concepts (markdown files with YAML
frontmatter) about systems, projects, people and decisions. This page lists every tool the
server offers, with its parameters. Each tool's description, as a client sees it in `tools/list`,
ends with a link to its section here.

How a session usually goes:

- `search` for a concept, then `get_concept` to read it.
- A concept path looks like `/infra/time-server.md`; the leading slash and the `.md` are
  optional.
- Without `catalog`, a tool uses your connection's selection: the catalogs the connection or
  the key's default names, else the only catalog the key reaches (`list_catalogs` shows it). A
  key granted every catalog has no such fallback. When `list_catalogs` shows an empty
  selection, pass `catalog`.
- To write, call `open_changeset` first and pass the handle it returns as `changeset` to every
  edit, `publish`, `discard`, `revert` and `pending_changes`. Reads take it too, to see the
  drafts. Nothing is visible to others until `publish`.
- When the person asks you to remember something, save it to the catalog (under a fitting
  folder) rather than only in the conversation; asking is their consent, so don't ask again.
  Don't save what they didn't ask you to keep.
- A catalog's data lives on a store, and a store can be unreachable (a separate database that
  is down). Then a call that needs it fails with `StoreUnavailable` (retry later; the message
  never names the store), and the other catalogs keep working:
  - A read that merges catalogs (`search`, `list_catalogs`, `list_changesets` without
    `catalog`) answers from the others and lists the left-out catalog ids in `unavailable`
    (absent when every store answered). Say so to the person: those catalogs were not searched.
  - A read that answers from the first selected catalog holding the document (`get_concept`,
    `read_document` and `history` without `catalog`) answers from a catalog before the first
    unreachable one. If none of those has it, it fails with `StoreUnavailable` rather than
    `NotFound`, because the unreachable catalog might hold it: retry, or pass `catalog`.
  - A write and `cited_by` (it does not merge) fail with `StoreUnavailable` while a store they
    need is down. So does a changeset handle that is not found before a lost store is reached
    (the stores are asked in turn, `main` first): a handle found earlier still works.

A tool is one of five kinds:

- read-only: it changes nothing.
- staged: it adds an entry to a changeset, which `discard` drops and `publish` applies.
- maintenance: a step in a maintenance run (claim, renew, submit, dispose). It takes no
  changeset and is saved at once, so `discard` does not undo it; it never touches a concept.
- destructive: it deletes, or cannot be undone from inside the server.
- external: it sends something to a service outside the server (`send_feedback`, offered
  only when the operator configured it). It changes no catalog.

";

/// The tool's description without the trailing reference link `OkfServer::tools` adds.
///
/// # Arguments
/// * `tool` - a tool from the router.
///
/// # Returns
/// The description as written on the tool.
fn own_description(tool: &Tool) -> String {
    let full = tool.description.as_deref().unwrap_or_default();
    let suffix = format!(" Reference: {TOOL_REFERENCE_URL}#{}", tool.name);
    full.strip_suffix(&suffix).unwrap_or(full).to_string()
}

/// A short type name for a property schema: its `type`, an enum's values, a `$ref`'s target, or
/// the alternatives of an `anyOf`. `null` is dropped (an optional parameter says so itself).
///
/// # Arguments
/// * `schema` - one property's JSON Schema.
///
/// # Returns
/// A phrase such as `string`, `array of string` or `one of: a, b`.
fn type_name(schema: &Value) -> String {
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        let listed: Vec<String> = values
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        return format!("one of: {}", listed.join(", "));
    }
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        let name = reference.rsplit('/').next().unwrap_or(reference);
        // Linked to the type's section at the end of the page (see `render_type`).
        return format!("[`{name}`](#{})", type_anchor(name));
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(options) = schema.get(key).and_then(Value::as_array) {
            let names: Vec<String> = options
                .iter()
                .map(type_name)
                .filter(|name| name != "null")
                .collect();
            return names.join(" or ");
        }
    }
    let kinds: Vec<&str> = match schema.get("type") {
        Some(Value::String(kind)) => vec![kind.as_str()],
        Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    let names: Vec<String> = kinds
        .into_iter()
        .filter(|kind| *kind != "null")
        .map(|kind| match (kind, schema.get("items")) {
            ("array", Some(items)) => format!("array of {}", type_name(items)),
            _ => kind.to_string(),
        })
        .collect();
    if names.is_empty() {
        // Only `null` (an `anyOf` arm that says "or absent"), or no `type` at all.
        if schema.get("type").is_some() {
            "null"
        } else {
            "any"
        }
        .to_string()
    } else {
        names.join(" or ")
    }
}

/// The anchor GitHub gives a type's heading, `## Type `Name```: the heading's text, lowercased,
/// with punctuation dropped and spaces turned into hyphens. The "type" prefix keeps it apart from
/// a tool's anchor (a type `Lint` would otherwise collide with the `lint` tool).
///
/// # Arguments
/// * `name` - the type's name in `$defs`.
///
/// # Returns
/// The fragment, without `#`.
fn type_anchor(name: &str) -> String {
    format!("type-{}", name.to_lowercase())
}

/// Make text safe for one table cell: one line, no pipe characters.
///
/// # Arguments
/// * `text` - a description.
///
/// # Returns
/// The text on one line with `|` escaped.
fn cell(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('|', "\\|")
}

/// The behaviour class a tool's hints name.
///
/// # Arguments
/// * `tool` - a tool from the router.
///
/// # Returns
/// `read-only`, `staged`, `maintenance`, `destructive` or `external`.
fn class(tool: &Tool) -> &'static str {
    let hints = tool.annotations.as_ref();
    if hints.and_then(|h| h.open_world_hint) == Some(true) {
        "external"
    } else if hints.and_then(|h| h.read_only_hint) == Some(true) {
        "read-only"
    } else if hints.and_then(|h| h.destructive_hint) == Some(true) {
        "destructive"
    } else if tool.name.starts_with("maintenance_") {
        // Run steps persist at once and take no changeset handle, so they are not "staged".
        "maintenance"
    } else {
        "staged"
    }
}

/// Append one tool's section.
///
/// # Arguments
/// * `page` - the page being built.
/// * `tool` - the tool to describe.
fn render_tool(page: &mut String, tool: &Tool) {
    let title = tool.title.as_deref().unwrap_or_default();
    // A heading that is only the tool's name makes `#<name>` the anchor on GitHub.
    let _ = write!(
        page,
        "## `{}`\n\n**{title}** ({})\n\n{}\n\n",
        tool.name,
        class(tool),
        own_description(tool)
    );
    let schema = Value::Object((*tool.input_schema).clone());
    let required = required_names(&schema);
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        page.push_str("No parameters.\n\n");
        return;
    };
    property_table(page, "Parameter", properties, &required);
    nested_fields(page, "", properties);
}

/// Append a table for every property that is itself an object written inline (or an array of
/// them), then recurse into it, so a parameter such as `submission` shows the fields it needs.
/// Types that live in `$defs` get their own section instead (see [`render_type`]).
///
/// # Arguments
/// * `page` - the page being built.
/// * `prefix` - the dotted path of the object these properties belong to (empty at the top).
/// * `properties` - the schema's `properties` object.
fn nested_fields(page: &mut String, prefix: &str, properties: &serde_json::Map<String, Value>) {
    for (name, property) in properties {
        // An array's element schema is where its fields are described.
        let object = property.get("items").unwrap_or(property);
        let Some(fields) = object.get("properties").and_then(Value::as_object) else {
            continue;
        };
        let path = format!("{prefix}{name}");
        let each = if property.get("items").is_some() {
            " (each item)"
        } else {
            ""
        };
        let _ = write!(page, "Fields of `{path}`{each}:\n\n");
        property_table(page, "Field", fields, &required_names(object));
        nested_fields(page, &format!("{path}."), fields);
    }
}

/// Append a table of properties.
///
/// # Arguments
/// * `page` - the page being built.
/// * `heading` - the first column's title (`Parameter` or `Field`).
/// * `properties` - the schema's `properties` object.
/// * `required` - the names the schema lists as required.
fn property_table(
    page: &mut String,
    heading: &str,
    properties: &serde_json::Map<String, Value>,
    required: &[&str],
) {
    let _ = writeln!(
        page,
        "| {heading} | Type | Required | Description |\n|---|---|---|---|"
    );
    for (name, property) in properties {
        let description = property
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let _ = writeln!(
            page,
            "| `{name}` | {} | {} | {} |",
            cell(&type_name(property)),
            if required.contains(&name.as_str()) {
                "yes"
            } else {
                "no"
            },
            cell(description)
        );
    }
    page.push('\n');
}

/// The names a schema lists under `required`.
///
/// # Arguments
/// * `schema` - an object schema.
///
/// # Returns
/// The required property names (none when the key is absent).
fn required_names(schema: &Value) -> Vec<&str> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// Every named definition (`$defs`) in the tools' input schemas, by name. A type two tools share
/// has one definition, so the first wins.
///
/// # Arguments
/// * `tools` - the tools from the router.
///
/// # Returns
/// The definitions, in name order.
fn collect_defs(tools: &[Tool]) -> BTreeMap<String, Value> {
    let mut defs = BTreeMap::new();
    for tool in tools {
        let defs_here = tool.input_schema.get("$defs").and_then(Value::as_object);
        for (name, schema) in defs_here.into_iter().flatten() {
            defs.entry(name.clone()).or_insert_with(|| schema.clone());
        }
    }
    defs
}

/// Append one type's section: its description, then its values (an enum), its fields (an
/// object) or its alternatives (an `anyOf` or `oneOf`, each with its own fields).
///
/// # Arguments
/// * `page` - the page being built.
/// * `name` - the type's name.
/// * `schema` - its definition.
fn render_type(page: &mut String, name: &str, schema: &Value) {
    let _ = write!(page, "### Type `{name}`\n\n");
    if let Some(text) = schema.get("description").and_then(Value::as_str) {
        let _ = write!(page, "{}\n\n", text.trim());
    }
    render_shape(page, schema);
}

/// Append what a schema allows: enum values, object fields, or its alternatives.
///
/// # Arguments
/// * `page` - the page being built.
/// * `schema` - a definition or one alternative of one.
fn render_shape(page: &mut String, schema: &Value) {
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        property_table(page, "Field", properties, &required_names(schema));
        return;
    }
    if schema.get("enum").is_some() || schema.get("type").is_some() && schema.get("oneOf").is_none()
    {
        let _ = write!(page, "Value: {}\n\n", type_name(schema));
        return;
    }
    for key in ["oneOf", "anyOf"] {
        let Some(options) = schema.get(key).and_then(Value::as_array) else {
            continue;
        };
        page.push_str("One of:\n\n");
        for option in options {
            let _ = write!(page, "- {}", type_name(option));
            if let Some(text) = option.get("description").and_then(Value::as_str) {
                let _ = write!(page, ": {}", cell(text));
            }
            page.push('\n');
        }
        page.push('\n');
        // An alternative that is an object lists its fields under its bullet.
        for option in options {
            if option.get("properties").is_some() {
                render_shape(page, option);
            }
        }
    }
}

/// The whole page for the live tool list.
///
/// # Returns
/// The markdown, tools in name order.
fn render() -> String {
    let mut tools = OkfServer::tools().list_all();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    let mut page = String::from(INTRO);
    page.push_str("| Tool | Title | Kind |\n|---|---|---|\n");
    for tool in &tools {
        let _ = writeln!(
            page,
            "| [`{0}`](#{0}) | {1} | {2} |",
            tool.name,
            tool.title.as_deref().unwrap_or_default(),
            class(tool)
        );
    }
    page.push('\n');
    for tool in &tools {
        render_tool(&mut page, tool);
    }
    // Every type a parameter's type links to is defined here, with its values or fields.
    let defs = collect_defs(&tools);
    if !defs.is_empty() {
        page.push_str("## Types\n\nTypes the parameters above link to.\n\n");
        for (name, schema) in &defs {
            render_type(&mut page, name, schema);
        }
    }
    page.trim_end().to_string() + "\n"
}

#[test]
fn the_checked_in_tool_reference_matches_the_tool_list() {
    let rendered = render();
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(PAGE);

    if std::env::var_os("OKF_UPDATE_TOOL_REFERENCE").is_some() {
        std::fs::write(&path, &rendered).expect("the reference page is writable");
    }

    let checked_in = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        checked_in == rendered,
        "{PAGE} is out of date: run `OKF_UPDATE_TOOL_REFERENCE=1 cargo test --lib mcp::reference`"
    );
}

#[test]
fn the_reference_lists_every_tool_with_a_heading_its_link_points_at() {
    let page = render();
    let tools = OkfServer::tools().list_all();

    assert_eq!(tools.len(), 46);
    for tool in &tools {
        assert!(
            page.contains(&format!("\n## `{}`\n", tool.name)),
            "{}",
            tool.name
        );
        let description = tool.description.as_deref().unwrap_or_default();
        assert!(
            description.ends_with(&format!(" Reference: {TOOL_REFERENCE_URL}#{}", tool.name)),
            "{} does not end with its reference link",
            tool.name
        );
    }
}

#[test]
fn the_reference_has_no_private_details() {
    let page = render();

    assert!(!page.contains("okf.py"));
    assert!(!page.contains("vsix"));
}

#[test]
fn every_type_a_parameter_links_to_is_defined_with_its_fields() {
    let page = render();
    let tools = OkfServer::tools().list_all();
    let defs = collect_defs(&tools);

    // A link target with no section would leave the reader unable to build the argument.
    for link in page.split("](#type-").skip(1) {
        let anchor = link.split(')').next().unwrap_or_default();
        assert!(
            defs.keys().any(|name| name.to_lowercase() == anchor),
            "no section defines the type linked as #type-{anchor}"
        );
    }
    // The parameter the reviewer named: `submission`'s required fields appear in the output.
    let submit = tools
        .iter()
        .find(|tool| tool.name == "maintenance_submit")
        .expect("maintenance_submit is registered");
    let submission = &submit.input_schema["properties"]["submission"];
    let section = page
        .split("Fields of `submission`")
        .nth(1)
        .expect("a section for the submission's fields");
    assert!(!required_names(submission).is_empty());
    for field in required_names(submission) {
        assert!(
            section.contains(&format!("| `{field}` |")),
            "submission field {field}"
        );
    }
}
