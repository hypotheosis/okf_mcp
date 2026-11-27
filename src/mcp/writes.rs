//! The write tools (design "Write tools"): the content tools here (Task 8),
//! the bookkeeping tools below them (Task 9). Each goes through
//! [`crate::write::apply`], so it acts on the changeset its required
//! `changeset` handle names (in that changeset's catalog), the write rules
//! apply, and nothing is visible to anyone else until `publish`.

use axum::http::request::Parts;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router, Json};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::error::OkfError;
use crate::mcp::context::ToolContext;
use crate::mcp::{reply_write, OkfServer};
use crate::okf::ops::{self, LogKind, NewConcept, SourceEntry, StampField};
use crate::write::{apply, WriteReport};

/// `create_concept`'s arguments. Doc comments are the parameters'
/// descriptions in the tool's input schema.
#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
pub struct CreateConceptParams {
    /// The new concept's path, e.g. `/infra/ntp.md` (the `.md` is optional); it must not exist.
    pub path: String,
    /// Frontmatter keys as a JSON object; `type` is required, and `generated`, `verified` and `reviewed` are refused (the server writes `generated`).
    pub frontmatter: serde_json::Map<String, serde_json::Value>,
    /// The markdown body after the frontmatter.
    pub body: String,
    /// List the concept in its folder's index.md with this summary, creating folder indexes as needed.
    pub index_summary: Option<String>,
    /// The index.md section to list it under (default: the section the server chooses for it); needs `index_summary`.
    pub index_section: Option<String>,
    /// With `index_summary`, the new folder's summary in its parent's index.md (default: a TODO); for nested new folders, the innermost. Refused if the folder exists.
    pub folder_summary: Option<String>,
    /// Who wrote it, for the `generated` stamp (default: the key's actor).
    pub by: Option<String>,
    /// The changeset to write to: the handle `open_changeset` returned.
    pub changeset: String,
}

/// `edit_document`'s arguments.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct EditDocumentParams {
    /// A full path with `.md`: a concept or an index.md, e.g. `/infra/ntp.md` (`/infra/ntp` is refused).
    pub path: String,
    /// The exact text to replace; without `replace_all` it must occur exactly once. The edit may not add or change frontmatter.
    pub old_str: String,
    /// The replacement text.
    pub new_str: String,
    /// Replace every occurrence instead of exactly one.
    #[serde(default)]
    pub replace_all: bool,
    /// The changeset to write to: the handle `open_changeset` returned.
    pub changeset: String,
}

/// `replace_body`'s arguments.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct ReplaceBodyParams {
    /// The concept, with or without `.md`, e.g. `/infra/ntp` or `/infra/ntp.md`.
    pub path: String,
    /// The new body; the frontmatter is kept byte for byte, and the body may not add any.
    pub body: String,
    /// The changeset to write to: the handle `open_changeset` returned.
    pub changeset: String,
}

/// `set_frontmatter`'s arguments.
#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
pub struct SetFrontmatterParams {
    /// The concept, with or without `.md`, e.g. `/infra/ntp` or `/infra/ntp.md`.
    pub path: String,
    /// A descriptive key such as `title`, `status` or `tags`; the stamps, `open_questions` and `sources` have their own tools.
    pub key: String,
    /// The new value as JSON; required, and an explicit null deletes the key (`type` cannot be deleted, and must be a non-blank string).
    // No `#[serde(default)]`: a missing `value` is a parameter error, so
    // forgetting the argument can never delete the key. `serde_json::Value`
    // is not an `Option`, so serde treats the field as required.
    // `Value` publishes an any-value schema, which has no `type`; the
    // extension names every JSON type the op accepts (issue #513), and the
    // typed `items` keeps array schemas valid for the annotations test.
    #[schemars(extend(
        "type" = ["string", "number", "integer", "boolean", "array", "object", "null"],
        "items" = {"type": ["string", "number", "integer", "boolean", "array", "object", "null"]}
    ))]
    pub value: serde_json::Value,
    /// The changeset to write to: the handle `open_changeset` returned.
    pub changeset: String,
}

/// Creates a concept (and, with `index_summary`, its listing).
///
/// # Arguments
/// * `ctx` - the call's context.
/// * `params` - the concept.
///
/// # Returns
/// The write's report.
///
/// # Errors
/// As [`apply`]; the op's [`OkfError::InvalidPath`] (taken, reserved,
/// machinery) or [`OkfError::Refused`] (a missing `type`, a stamp key, a
/// listing it cannot make).
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn create_concept(
    ctx: &ToolContext,
    params: CreateConceptParams,
) -> Result<WriteReport, OkfError> {
    // Destructuring moves each field into its own variable, so the closure
    // can take the ones it needs while `changeset` and `by` stay here.
    let CreateConceptParams {
        path,
        frontmatter,
        body,
        index_summary,
        index_section,
        folder_summary,
        by,
        changeset,
    } = params;
    apply(ctx, &changeset, by.as_deref(), move |ws, op_ctx| {
        let new = NewConcept {
            path: &path,
            frontmatter: &frontmatter,
            body: &body,
            index_summary: index_summary.as_deref(),
            index_section: index_section.as_deref(),
            folder_summary: folder_summary.as_deref(),
        };
        ops::create_concept(ws, op_ctx, &new)
    })
    .await
}

/// Replaces exact text in a concept or an `index.md`.
///
/// # Arguments
/// * `ctx` - the call's context.
/// * `params` - the document (a full `.md` path) and the replacement.
///
/// # Returns
/// The write's report.
///
/// # Errors
/// As [`apply`]; [`OkfError::EditMismatch`] when `old_str` is found 0 or
/// (without `replace_all`) more than 1 times; [`OkfError::InvalidPath`] for
/// `log.md`, machinery or a path without `.md`; [`OkfError::Refused`] for an
/// edit that adds or changes frontmatter.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn edit_document(
    ctx: &ToolContext,
    params: EditDocumentParams,
) -> Result<WriteReport, OkfError> {
    let EditDocumentParams {
        path,
        old_str,
        new_str,
        replace_all,
        changeset,
    } = params;
    apply(ctx, &changeset, None, move |ws, _| {
        ops::edit_document(ws, &path, &old_str, &new_str, replace_all)
    })
    .await
}

/// Replaces a concept's body, keeping its frontmatter byte for byte.
///
/// # Arguments
/// * `ctx` - the call's context.
/// * `params` - the concept reference and its new body.
///
/// # Returns
/// The write's report.
///
/// # Errors
/// As [`apply`]; the op's [`OkfError::NotFound`], [`OkfError::InvalidPath`]
/// or [`OkfError::Refused`] (a body that adds frontmatter).
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn replace_body(
    ctx: &ToolContext,
    params: ReplaceBodyParams,
) -> Result<WriteReport, OkfError> {
    let ReplaceBodyParams {
        path,
        body,
        changeset,
    } = params;
    apply(ctx, &changeset, None, move |ws, _| {
        ops::replace_body(ws, &path, &body)
    })
    .await
}

/// Sets or deletes one descriptive frontmatter key, as a textual edit.
///
/// # Arguments
/// * `ctx` - the call's context.
/// * `params` - the concept reference, the key and the value (null deletes).
///
/// # Returns
/// The write's report.
///
/// # Errors
/// As [`apply`]; the op's refusal of a dedicated key, of deleting `type`
/// or of a `type` that is not a non-blank string.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn set_frontmatter(
    ctx: &ToolContext,
    params: SetFrontmatterParams,
) -> Result<WriteReport, OkfError> {
    let SetFrontmatterParams {
        path,
        key,
        value,
        changeset,
    } = params;
    // The op reads a JSON null as a delete, so the value passes as it is.
    apply(ctx, &changeset, None, move |ws, _| {
        ops::set_frontmatter(ws, &path, &key, Some(&value))
    })
    .await
}

// The content tools; `OkfServer::tools` joins `writes_router()` with the rest.
#[tool_router(router = writes_router, vis = "pub(crate)")]
impl OkfServer {
    /// The `create_concept` tool.
    #[tool(
        title = "Create Concept",
        description = "Save what the person wants kept. Use it whenever the person shares \
                       something worth keeping (a fact about themselves, a preference, a \
                       decision, a note), and certainly when they ask you to remember it: the \
                       catalog keeps it across conversations, so save it without asking first \
                       and tell them what you saved. Creates a concept in the changeset \
                       `changeset` names (open one with open_changeset). Nothing is published \
                       until `publish`; a published concept is visible only to those with \
                       access to the catalog (a personal catalog's owner unless they share it, \
                       a team catalog's team). `path` is \
                       the new concept's path, `.md` optional (e.g. /infra/ntp). Needs `type` in \
                       `frontmatter`; the server writes `generated: { by, at }` itself. With \
                       `index_summary`, also lists it in its folder's index.md (creating folder \
                       indexes as needed); `folder_summary` describes a new folder there. \
                       Write access.",
        annotations(
            title = "Create Concept",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn create_concept(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(params): Parameters<CreateConceptParams>,
    ) -> Result<Json<WriteReport>, CallToolResult> {
        let ctx = self.context(&parts)?;
        reply_write(create_concept(&ctx, params).await)
    }

    /// The `edit_document` tool.
    #[tool(
        title = "Edit Document",
        description = "Change wording in a saved note. Replace exact text in a concept or an index.md, in the changeset \
                       `changeset` names (open one with open_changeset). `path` is a full path \
                       with .md (e.g. /infra/ntp.md or /infra/index.md). `old_str` must occur \
                       exactly once unless `replace_all`; the edit may not add or change \
                       frontmatter. log.md and machinery are refused (use `log`). Staged in the \
                       changeset: nothing changes until `publish`, and `discard` drops it. Write \
                       access.",
        annotations(
            title = "Edit Document",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn edit_document(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(params): Parameters<EditDocumentParams>,
    ) -> Result<Json<WriteReport>, CallToolResult> {
        let ctx = self.context(&parts)?;
        reply_write(edit_document(&ctx, params).await)
    }

    /// The `replace_body` tool.
    #[tool(
        title = "Replace Concept Body",
        description = "Rewrite the text of a note. Replace a concept's body, keeping its frontmatter byte for byte, in the \
                       changeset `changeset` names (open one with open_changeset). `path` is a \
                       concept reference, .md optional (/infra/ntp or /infra/ntp.md). Staged in \
                       the changeset: nothing changes until `publish`, and `discard` drops it. \
                       Write access.",
        annotations(
            title = "Replace Concept Body",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn replace_body(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(params): Parameters<ReplaceBodyParams>,
    ) -> Result<Json<WriteReport>, CallToolResult> {
        let ctx = self.context(&parts)?;
        reply_write(replace_body(&ctx, params).await)
    }

    /// The `set_frontmatter` tool.
    #[tool(
        title = "Set Frontmatter",
        description = "Edit a note's tags and details. Set (or, with a null value, delete) one descriptive frontmatter key of a \
                       concept, in the changeset `changeset` names (open one with \
                       open_changeset). `path` is a concept reference, .md optional (/infra/ntp \
                       or /infra/ntp.md). The stamps, open_questions and sources have their own \
                       tools; `type` cannot be deleted. Staged in the changeset: nothing \
                       changes until `publish`, and `discard` drops it. Write access.",
        annotations(
            title = "Set Frontmatter",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn set_frontmatter(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(params): Parameters<SetFrontmatterParams>,
    ) -> Result<Json<WriteReport>, CallToolResult> {
        let ctx = self.context(&parts)?;
        reply_write(set_frontmatter(&ctx, params).await)
    }
}

// ---------------------------------------------------------------------------
// The bookkeeping tools (Task 9): ports of okf.py's concept commands, `log`
// and `index`. A block of its own (`bookkeeping_router`), so the content
// tools above and these `okf.py` ports stay two groups, as in the design.
// ---------------------------------------------------------------------------

/// A queued question, by 1-based position or by its exact text.
// `untagged`: serde tries each variant in order, so `2` is a number and
// `"Who owns it?"` is text, with no wrapper object in the JSON.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum QuestionRef {
    /// Its 1-based position, as `open_questions` numbers it.
    Number(u64),
    /// Its exact text (a string of digits is read as a position).
    Text(String),
}

impl QuestionRef {
    /// The argument `okf.py unquestion` takes: a number as its digits, or the
    /// text.
    ///
    /// # Returns
    /// The string the op parses (digits are a 1-based position).
    #[tracing::instrument(level = "trace", skip_all)]
    fn into_argument(self) -> String {
        match self {
            QuestionRef::Number(number) => number.to_string(),
            QuestionRef::Text(text) => text,
        }
    }
}

/// `stamp`'s arguments.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct StampParams {
    /// The concept, with or without `.md`, e.g. `/infra/ntp` or `/infra/ntp.md`.
    pub path: String,
    /// `generated` (content written) or `verified` (checked against the real system).
    pub field: StampField,
    /// Who did it (default: the key's actor); say `human:<name>` only when a person really did.
    pub by: Option<String>,
    /// The changeset to write to: the handle `open_changeset` returned.
    pub changeset: String,
}

/// `review`'s arguments.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct ReviewParams {
    /// The concept, with or without `.md`, e.g. `/infra/ntp` or `/infra/ntp.md`.
    pub path: String,
    /// The outcome: verified, updated, partial or unverifiable.
    pub outcome: String,
    /// What was and wasn't checked.
    pub note: String,
    /// Questions to queue in `open_questions` (exact duplicates are skipped).
    pub questions: Option<Vec<String>>,
    /// Who reviewed (default: the key's actor).
    pub by: Option<String>,
    /// The changeset to write to: the handle `open_changeset` returned.
    pub changeset: String,
}

/// `question`'s arguments.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct QuestionParams {
    /// The concept, with or without `.md`, e.g. `/infra/ntp` or `/infra/ntp.md`.
    pub path: String,
    /// The self-contained question to queue in `open_questions`.
    pub question: String,
    /// The changeset to write to: the handle `open_changeset` returned.
    pub changeset: String,
}

/// `unquestion`'s arguments.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct UnquestionParams {
    /// The concept, with or without `.md`, e.g. `/infra/ntp` or `/infra/ntp.md`.
    pub path: String,
    /// The question to remove: its 1-based number in `open_questions`, or its exact text.
    #[schemars(extend("type" = ["integer", "string"]))]
    pub question: QuestionRef,
    /// The changeset to write to: the handle `open_changeset` returned.
    pub changeset: String,
}

/// `add_source`'s arguments.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct AddSourceParams {
    /// The concept, with or without `.md`, e.g. `/infra/ntp` or `/infra/ntp.md`.
    pub path: String,
    /// The entry's id; an entry with this id must not exist.
    pub id: String,
    /// What was consulted: a URI, a path or a description.
    pub resource: String,
    /// Its title.
    pub title: Option<String>,
    /// Its author, e.g. `human:alice`.
    pub author: Option<String>,
    /// The changeset to write to: the handle `open_changeset` returned.
    pub changeset: String,
}

/// `remove_source`'s arguments.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct RemoveSourceParams {
    /// The concept, with or without `.md`, e.g. `/infra/ntp` or `/infra/ntp.md`.
    pub path: String,
    /// The id of the entry to remove; no footnote in the body may still cite it.
    pub id: String,
    /// The changeset to write to: the handle `open_changeset` returned.
    pub changeset: String,
}

/// `log`'s arguments.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct LogParams {
    /// The entry; runs of whitespace become single spaces.
    pub text: String,
    /// Update, Creation, Change or Import (default Update).
    #[schemars(extend("type" = ["string", "null"]))]
    pub kind: Option<LogKind>,
    /// The changeset to write to: the handle `open_changeset` returned.
    pub changeset: String,
}

/// `index`'s arguments.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct IndexParams {
    /// The concept to list, with or without `.md`, e.g. `/infra/ntp`; its folder's index.md must exist (`repair_indexes` creates missing ones). Or a folder, e.g. `/infra/`, to set its listing's summary in its parent's index.md.
    pub path: String,
    /// The text after the link's ` - `. For a concept already listed, replaces its listing's summary.
    pub summary: String,
    /// The heading to list it under, e.g. `Build infrastructure` (default: `Concepts`).
    pub section: Option<String>,
    /// The link text (default: the concept's frontmatter title).
    pub title: Option<String>,
    /// The changeset to write to: the handle `open_changeset` returned.
    pub changeset: String,
}

/// `repair_indexes`'s arguments.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct RepairIndexesParams {
    /// The changeset to write to: the handle `open_changeset` returned.
    pub changeset: String,
}

/// `okf.py stamp`: sets `generated`, or appends to `verified`, with
/// `{ by, at: now }`.
///
/// # Arguments
/// * `ctx` - the call's context.
/// * `params` - the concept, the field and `by`.
///
/// # Returns
/// The write's report.
///
/// # Errors
/// As [`apply`]; the op's [`OkfError::NotFound`] or
/// [`OkfError::InvalidPath`] for the reference, [`OkfError::Refused`] for a
/// concept without frontmatter, [`OkfError::FrontmatterParse`] or
/// [`OkfError::Shape`].
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn stamp(ctx: &ToolContext, params: StampParams) -> Result<WriteReport, OkfError> {
    let StampParams {
        path,
        field,
        by,
        changeset,
    } = params;
    apply(ctx, &changeset, by.as_deref(), move |ws, op_ctx| {
        ops::stamp(ws, op_ctx, &path, field)
    })
    .await
}

/// `okf.py review`: replaces `reviewed` and queues any questions.
///
/// # Arguments
/// * `ctx` - the call's context.
/// * `params` - the concept, outcome, note, questions and `by`.
///
/// # Returns
/// The write's report.
///
/// # Errors
/// As [`stamp`]; [`OkfError::Refused`] for an unknown outcome.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn review(
    ctx: &ToolContext,
    params: ReviewParams,
) -> Result<WriteReport, OkfError> {
    let ReviewParams {
        path,
        outcome,
        note,
        questions,
        by,
        changeset,
    } = params;
    apply(ctx, &changeset, by.as_deref(), move |ws, op_ctx| {
        // `unwrap_or_default` gives an empty list when no questions were sent.
        let questions = questions.unwrap_or_default();
        ops::review(ws, op_ctx, &path, &outcome, &note, &questions)
    })
    .await
}

/// `okf.py question`: queues a question in `open_questions`.
///
/// # Arguments
/// * `ctx` - the call's context.
/// * `params` - the concept and the question.
///
/// # Returns
/// The write's report; nothing touched if the question is already queued.
///
/// # Errors
/// As [`stamp`].
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn question(
    ctx: &ToolContext,
    params: QuestionParams,
) -> Result<WriteReport, OkfError> {
    let QuestionParams {
        path,
        question,
        changeset,
    } = params;
    apply(ctx, &changeset, None, move |ws, op_ctx| {
        ops::question(ws, op_ctx, &path, &question)
    })
    .await
}

/// `okf.py unquestion`: removes a queued question by number or text,
/// deleting `open_questions` when none are left.
///
/// # Arguments
/// * `ctx` - the call's context.
/// * `params` - the concept and the question.
///
/// # Returns
/// The write's report.
///
/// # Errors
/// As [`stamp`]; [`OkfError::Refused`] when there is no such question.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn unquestion(
    ctx: &ToolContext,
    params: UnquestionParams,
) -> Result<WriteReport, OkfError> {
    let UnquestionParams {
        path,
        question,
        changeset,
    } = params;
    let question = question.into_argument();
    apply(ctx, &changeset, None, move |ws, op_ctx| {
        ops::unquestion(ws, op_ctx, &path, &question)
    })
    .await
}

/// `okf.py source`: appends an entry to `sources`.
///
/// # Arguments
/// * `ctx` - the call's context.
/// * `params` - the concept and the entry.
///
/// # Returns
/// The write's report.
///
/// # Errors
/// As [`stamp`]; [`OkfError::Refused`] for a duplicate id.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn add_source(
    ctx: &ToolContext,
    params: AddSourceParams,
) -> Result<WriteReport, OkfError> {
    let AddSourceParams {
        path,
        id,
        resource,
        title,
        author,
        changeset,
    } = params;
    apply(ctx, &changeset, None, move |ws, op_ctx| {
        let entry = SourceEntry {
            id: &id,
            resource: &resource,
            title: title.as_deref(),
            author: author.as_deref(),
        };
        ops::source(ws, op_ctx, &path, &entry)
    })
    .await
}

/// Removes an entry from `sources` by id (the server's inverse of
/// `okf.py source`), deleting the key when none are left.
///
/// # Arguments
/// * `ctx` - the call's context.
/// * `params` - the concept and the entry's id.
///
/// # Returns
/// The write's report.
///
/// # Errors
/// As [`stamp`]; [`OkfError::Refused`] when no entry has the id, or a
/// footnote in the body still cites it.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn remove_source(
    ctx: &ToolContext,
    params: RemoveSourceParams,
) -> Result<WriteReport, OkfError> {
    let RemoveSourceParams {
        path,
        id,
        changeset,
    } = params;
    apply(ctx, &changeset, None, move |ws, op_ctx| {
        ops::remove_source(ws, op_ctx, &path, &id)
    })
    .await
}

/// `okf.py log`: adds a bullet under today's (UTC) heading in `log.md`.
/// Its replay op is stored on the `log.md` entry.
///
/// # Arguments
/// * `ctx` - the call's context.
/// * `params` - the text and kind (default [`LogKind::Update`]).
///
/// # Returns
/// The write's report; nothing touched if the bullet is already there today.
///
/// # Errors
/// As [`apply`]; [`OkfError::NotFound`] for a catalog without `log.md`.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn log(ctx: &ToolContext, params: LogParams) -> Result<WriteReport, OkfError> {
    let LogParams {
        text,
        kind,
        changeset,
    } = params;
    let kind = kind.unwrap_or_default();
    apply(ctx, &changeset, None, move |ws, op_ctx| {
        ops::log(ws, op_ctx, kind, &text)
    })
    .await
}

/// `okf.py index`: lists a concept in its folder's `index.md`. Its replay
/// op is stored on the `index.md` entry.
///
/// # Arguments
/// * `ctx` - the call's context.
/// * `params` - the concept, summary, section and title.
///
/// # Returns
/// The write's report; nothing touched if the index already links it.
///
/// # Errors
/// As [`stamp`]; [`OkfError::Refused`] for a folder without `index.md`, a
/// concept without a title or a missing `section` heading.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn index(ctx: &ToolContext, params: IndexParams) -> Result<WriteReport, OkfError> {
    let IndexParams {
        path,
        summary,
        section,
        title,
        changeset,
    } = params;
    apply(ctx, &changeset, None, move |ws, op_ctx| {
        ops::index(
            ws,
            op_ctx,
            &path,
            &summary,
            section.as_deref(),
            title.as_deref(),
        )
    })
    .await
}

/// Creates the bookkeeping the changeset's catalog is missing: the root
/// `index.md` and `log.md`, folder indexes and listings (see
/// [`ops::repair_indexes`]). Its replay ops are stored on each index's entry.
///
/// # Arguments
/// * `ctx` - the call's context.
/// * `params` - the changeset.
///
/// # Returns
/// The write's report: the files written, and one note per created file
/// and per TODO placeholder; nothing touched when nothing is missing.
///
/// # Errors
/// As [`apply`] (below `write`, [`OkfError::Forbidden`]).
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn repair_indexes(
    ctx: &ToolContext,
    params: RepairIndexesParams,
) -> Result<WriteReport, OkfError> {
    apply(ctx, &params.changeset, None, |ws, _| {
        ops::repair_indexes(ws)
    })
    .await
}

// The bookkeeping tools; `OkfServer::tools` joins `bookkeeping_router()`.
#[tool_router(router = bookkeeping_router, vis = "pub(crate)")]
impl OkfServer {
    /// The `stamp` tool.
    #[tool(
        title = "Stamp Concept",
        description = "Mark a note written or checked. Set a concept's `generated` (content written) or append to \
                       its `verified` (checked against the real system) with { by, at: now }, in \
                       the changeset `changeset` names (open one with open_changeset). `by` \
                       defaults to the key's actor. Replayed at publish onto text published meanwhile, unless a free edit of the concept follows (`pending_changes` shows the result). \
                       Write access.",
        annotations(
            title = "Stamp Concept",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn stamp(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(params): Parameters<StampParams>,
    ) -> Result<Json<WriteReport>, CallToolResult> {
        let ctx = self.context(&parts)?;
        reply_write(stamp(&ctx, params).await)
    }

    /// The `review` tool.
    #[tool(
        title = "Record Review",
        description = "Record that a note was checked. Replace a concept's `reviewed` with { by, at: now, outcome, \
                       note } and queue any `questions` in open_questions, in the changeset \
                       `changeset` names (open one with open_changeset). `by` defaults to the \
                       key's actor. Replayed at publish onto text published meanwhile, unless a free edit of the concept follows (`pending_changes` shows the result). \
                       Write access.",
        annotations(
            title = "Record Review",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn review(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(params): Parameters<ReviewParams>,
    ) -> Result<Json<WriteReport>, CallToolResult> {
        let ctx = self.context(&parts)?;
        reply_write(review(&ctx, params).await)
    }

    /// The `question` tool.
    #[tool(
        title = "Add Question",
        description = "Flag a doubt on a saved note. Queue one question in a concept's open_questions without \
                       recording a review, in the changeset `changeset` names (open one with \
                       open_changeset). Replayed at publish onto text published meanwhile, unless a free edit of the concept follows (`pending_changes` shows the result). \
                       Write access.",
        annotations(
            title = "Add Question",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn question(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(params): Parameters<QuestionParams>,
    ) -> Result<Json<WriteReport>, CallToolResult> {
        let ctx = self.context(&parts)?;
        reply_write(question(&ctx, params).await)
    }

    /// The `unquestion` tool.
    #[tool(
        title = "Resolve Question",
        description = "Clear a doubt on a saved note. Remove one queued question, by its 1-based number in \
                       open_questions or its exact text, deleting the key when none are left, in \
                       the changeset `changeset` names (open one with open_changeset). Write \
                       access. Replayed at publish onto text published meanwhile, unless a free edit of the concept follows (`pending_changes` shows the result).",
        annotations(
            title = "Resolve Question",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn unquestion(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(params): Parameters<UnquestionParams>,
    ) -> Result<Json<WriteReport>, CallToolResult> {
        let ctx = self.context(&parts)?;
        reply_write(unquestion(&ctx, params).await)
    }

    /// The `add_source` tool.
    #[tool(
        title = "Add Source",
        description = "Note where a fact came from. Append an entry (id, resource, title, author) to a \
                       concept's `sources`, refusing a duplicate id, in the changeset `changeset` \
                       names (open one with open_changeset); `cited_by` finds a source's citations. Replayed at publish onto text published meanwhile, unless a free edit of the concept follows (`pending_changes` shows the result). \
                       Write access.",
        annotations(
            title = "Add Source",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn add_source(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(params): Parameters<AddSourceParams>,
    ) -> Result<Json<WriteReport>, CallToolResult> {
        let ctx = self.context(&parts)?;
        reply_write(add_source(&ctx, params).await)
    }

    /// The `remove_source` tool.
    #[tool(
        title = "Remove Source",
        description = "Drop a source from a note. Remove the entry with `id` from a concept's `sources`, deleting the key \
                       when none are left, in the changeset `changeset` names (open one with \
                       open_changeset). Refused while a footnote in the body still cites it (its \
                       label is the id, or its definition names the id, title or resource): \
                       remove the citation first. `cited_by` finds a source's citations. Replayed at publish onto text published meanwhile, unless a free edit of the concept follows (`pending_changes` shows the result). \
                       Write access.",
        annotations(
            title = "Remove Source",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn remove_source(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(params): Parameters<RemoveSourceParams>,
    ) -> Result<Json<WriteReport>, CallToolResult> {
        let ctx = self.context(&parts)?;
        reply_write(remove_source(&ctx, params).await)
    }

    /// The `log` tool.
    #[tool(
        title = "Add Log Entry",
        description = "Add a line to the change log. Add `* **<kind>:** <text>` under today's UTC date in log.md, \
                       the only way to write log.md, in the changeset `changeset` names (open one \
                       with open_changeset). Replayed at publish onto a log.md that changed \
                       meanwhile, so concurrent entries never conflict. Write access.",
        annotations(
            title = "Add Log Entry",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn log(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(params): Parameters<LogParams>,
    ) -> Result<Json<WriteReport>, CallToolResult> {
        let ctx = self.context(&parts)?;
        reply_write(log(&ctx, params).await)
    }

    /// The `index` tool.
    #[tool(
        title = "Add Index Entry",
        description = "List a note in its folder. List a concept in its folder's index.md as `* \
                       [Title](name.md) - summary`, in the changeset `changeset` names (open one \
                       with open_changeset); if it is already linked, only its summary is set to `summary` (a name with a space, `)`, `#` or `:` is written percent-encoded, e.g. `a%20b.md`; an older raw bullet for it counts as linked, with a note; repair_indexes rewrites it). Replayed at \
                       publish onto an index.md that changed meanwhile, unless a free edit of \
                       that index.md follows. With a folder `path` (`/infra/`), sets the summary \
                       of its listing in the parent's index.md instead, replayed the same way. \
                       Write access.",
        annotations(
            title = "Add Index Entry",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn index(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(params): Parameters<IndexParams>,
    ) -> Result<Json<WriteReport>, CallToolResult> {
        let ctx = self.context(&parts)?;
        reply_write(index(&ctx, params).await)
    }

    /// The `repair_indexes` tool.
    #[tool(
        title = "Repair Indexes",
        description = "Fix missing index and log files. Create whatever bookkeeping the catalog is missing, in the changeset \
                       `changeset` names (open one with open_changeset): the root index.md and \
                       log.md, an index.md for every folder holding concepts, each folder's \
                       listing in its parent's # Sections, and each concept's listing in its \
                       folder's index.md (its title and description; TODO placeholders where \
                       they are missing, each named in `notes`). Existing text, listings and \
                       summaries are kept, except an old raw (unencoded) concept or folder \
                       listing whose link never resolves, such as `[A](a b.md)` or \
                       `[My docs](my docs/)`: its destination is \
                       percent-encoded in place, or the bullet removed beside a working one, \
                       and `notes` quotes each. A second run changes nothing. A file or folder \
                       no markdown link can list (a `]` in a folder name) is not listed; \
                       `notes` names it. \
                       Review the result with pending_changes before publishing. Write access.",
        annotations(
            title = "Repair Indexes",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn repair_indexes(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(params): Parameters<RepairIndexesParams>,
    ) -> Result<Json<WriteReport>, CallToolResult> {
        let ctx = self.context(&parts)?;
        reply_write(repair_indexes(&ctx, params).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::mcp::testing::{app_with, changeset_row, open, writer, TestApp};
    use crate::store::changesets;

    const NOTE: &str = "---\ntype: Note\ntitle: A\ndescription: A note.\n---\n# A\n\nOld text.\n";

    async fn pg() -> TestApp {
        app_with(
            &[(
                "pg",
                &[
                    (
                        "index.md",
                        "# Catalog\n\n# Sections\n\n* [Infra](infra/index.md)\n",
                    ),
                    ("a.md", NOTE),
                    ("infra/index.md", "# Infra\n"),
                    ("log.md", "# Log\n"),
                ],
            )],
            Config::default(),
        )
        .await
    }

    /// The text of `path` as reads through the changeset `handle` see it.
    async fn seen(ctx: &ToolContext, handle: &str, path: &str) -> String {
        let state = ctx.app().catalog("pg").await.unwrap().unwrap();
        let pinned = ctx.pin(Some(handle)).await.unwrap();
        let view = ctx.view(&state, pinned.as_ref()).await.unwrap();
        view.snapshot()
            .get(path)
            .map(|doc| doc.text().to_string())
            .unwrap_or_default()
    }

    fn edit(handle: &str, path: &str, old_str: &str, new_str: &str) -> EditDocumentParams {
        EditDocumentParams {
            path: path.to_string(),
            old_str: old_str.to_string(),
            new_str: new_str.to_string(),
            replace_all: false,
            changeset: handle.to_string(),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_content_tools_edit_through_the_overlay() {
        let test = pg().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;
        let frontmatter =
            serde_json::json!({"type": "System", "title": "NTP", "description": "Hosts sync."});

        let created = create_concept(
            &ctx,
            CreateConceptParams {
                path: "/infra/ntp.md".to_string(),
                frontmatter: frontmatter.as_object().unwrap().clone(),
                body: "# NTP\n\nSee [A](/a.md).\n".to_string(),
                index_summary: None,
                index_section: None,
                folder_summary: None,
                by: None,
                changeset: handle.clone(),
            },
        )
        .await
        .unwrap();
        let edited = edit_document(&ctx, edit(&handle, "/a.md", "Old text.", "New text."))
            .await
            .unwrap();
        replace_body(
            &ctx,
            ReplaceBodyParams {
                path: "/infra/ntp".to_string(),
                body: "# NTP\n\nRewritten.\n".to_string(),
                changeset: handle.clone(),
            },
        )
        .await
        .unwrap();
        set_frontmatter(
            &ctx,
            SetFrontmatterParams {
                path: "/a.md".to_string(),
                key: "status".to_string(),
                value: serde_json::json!("draft"),
                changeset: handle.clone(),
            },
        )
        .await
        .unwrap();
        let mismatch = edit_document(&ctx, edit(&handle, "/a.md", "not there", "x"))
            .await
            .unwrap_err();

        assert_eq!(created.touched, ["/infra/ntp.md"]);
        assert_eq!(edited.changeset, created.changeset);
        let ntp = seen(&ctx, &handle, "infra/ntp.md").await;
        // `by` defaulted to the key's actor in the `generated` stamp.
        assert!(
            ntp.contains("generated:") && ntp.contains("writer-a"),
            "{ntp}"
        );
        assert!(ntp.ends_with("# NTP\n\nRewritten.\n"), "{ntp}");
        let a = seen(&ctx, &handle, "a.md").await;
        assert!(
            a.contains("status: draft") && a.contains("New text."),
            "{a}"
        );
        assert!(matches!(
            mismatch,
            OkfError::EditMismatch { matches: 0, .. }
        ));
        let entries = changesets::entries(
            test.app.main_db().await.executor(),
            changeset_row(&test, &handle).await.id,
        )
        .await
        .unwrap();
        let paths: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
        assert_eq!(paths, ["a.md", "infra/ntp.md"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn edit_document_refuses_log_md() {
        let test = pg().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;

        let log = edit_document(&ctx, edit(&handle, "/log.md", "# Log", "# Mine"))
            .await
            .unwrap_err();
        // A concept reference without `.md` is not a document path here.
        let bare = edit_document(&ctx, edit(&handle, "/a", "Old text.", "New."))
            .await
            .unwrap_err();

        assert_eq!(log.kind(), "InvalidPath");
        assert_eq!(bare.kind(), "InvalidPath");
    }

    #[test]
    fn set_frontmatter_requires_value_and_only_an_explicit_null_deletes() {
        let missing = serde_json::json!({"path": "/a", "key": "status", "changeset": "h"});
        let null =
            serde_json::json!({"path": "/a", "key": "status", "value": null, "changeset": "h"});

        let refused = serde_json::from_value::<SetFrontmatterParams>(missing);
        let delete = serde_json::from_value::<SetFrontmatterParams>(null).unwrap();
        let tools = OkfServer::writes_router().list_all();
        let tool = tools
            .iter()
            .find(|tool| tool.name == "set_frontmatter")
            .unwrap();

        let error = refused.unwrap_err().to_string();
        assert!(error.contains("missing field `value`"), "{error}");
        assert_eq!(delete.value, serde_json::Value::Null);
        let required = tool.input_schema["required"].as_array().unwrap();
        assert!(required.iter().any(|name| name == "value"), "{required:?}");
    }

    #[test]
    fn unquestion_question_has_a_direct_type_and_still_accepts_both_forms() {
        let tools = OkfServer::bookkeeping_router().list_all();
        let tool = tools
            .iter()
            .find(|tool| tool.name == "unquestion")
            .expect("unquestion is registered");
        let question = &tool.input_schema["properties"]["question"];

        assert_eq!(question["type"], serde_json::json!(["integer", "string"]));
        assert!(question["description"].as_str().is_some(), "{question}");
        assert_eq!(question["$ref"], "#/$defs/QuestionRef");

        let number: QuestionRef = serde_json::from_value(serde_json::json!(2)).unwrap();
        let text: QuestionRef = serde_json::from_value(serde_json::json!("Who owns it?")).unwrap();
        assert_eq!(number, QuestionRef::Number(2));
        assert_eq!(text, QuestionRef::Text("Who owns it?".to_string()));
    }

    #[test]
    fn the_write_tools_have_object_output_schemas_and_say_who_by_defaults_to() {
        let tools = OkfServer::writes_router().list_all();

        for name in [
            "create_concept",
            "edit_document",
            "replace_body",
            "set_frontmatter",
        ] {
            let tool = tools
                .iter()
                .find(|tool| tool.name == name)
                .unwrap_or_else(|| panic!("{name}"));
            let output = tool.output_schema.as_ref().expect("an output schema");
            assert_eq!(output["type"], "object", "{name}");
        }
        let create = tools
            .iter()
            .find(|tool| tool.name == "create_concept")
            .unwrap();
        let by = create.input_schema["properties"]["by"]["description"]
            .as_str()
            .unwrap();
        // The phrase sits on one doc-comment line (schemars keeps line breaks).
        assert!(by.contains("default: the key's actor"), "{by}");
        // The catalog comes from the changeset (M8 Decision 5).
        for tool in &tools {
            let required = tool.input_schema["required"].as_array().unwrap();
            let inputs = tool.input_schema["properties"].as_object().unwrap();
            assert!(
                required.iter().any(|name| name == "changeset") && !inputs.contains_key("catalog"),
                "{}",
                tool.name
            );
        }
    }
}

#[cfg(test)]
mod bookkeeping_tests {
    use super::*;
    use crate::config::Config;
    use crate::mcp::testing::{app_with, changeset_row, open, writer, TestApp};
    use crate::store::changesets::{self, EntryRow};

    const NOTE: &str = "---\ntype: Note\ntitle: A\ndescription: A note.\n---\n# A\n";
    const NTP: &str = "---\ntype: System\ntitle: NTP\ndescription: Hosts sync.\n---\n# NTP\n";

    async fn pg() -> TestApp {
        app_with(
            &[(
                "pg",
                &[
                    ("a.md", NOTE),
                    (
                        "infra/index.md",
                        "# Infra\n\n* [Time server](time.md) - the GPS clock\n",
                    ),
                    ("infra/time.md", NOTE),
                    ("infra/ntp.md", NTP),
                    ("log.md", "# Log\n"),
                ],
            )],
            Config::default(),
        )
        .await
    }

    /// The entry for `path` of the changeset `handle` names.
    async fn entry(test: &TestApp, handle: &str, path: &str) -> EntryRow {
        let row = changeset_row(test, handle).await;
        changesets::entries(test.app.main_db().await.executor(), row.id)
            .await
            .unwrap()
            .into_iter()
            .find(|entry| entry.path == path)
            .unwrap_or_else(|| panic!("no entry for {path}"))
    }

    /// The text of `path` as reads through the changeset `handle` see it.
    async fn seen(ctx: &ToolContext, handle: &str, path: &str) -> String {
        let state = ctx.app().catalog("pg").await.unwrap().unwrap();
        let pinned = ctx.pin(Some(handle)).await.unwrap();
        let view = ctx.view(&state, pinned.as_ref()).await.unwrap();
        view.snapshot()
            .get(path)
            .map(|doc| doc.text().to_string())
            .unwrap_or_default()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn log_records_its_replay_on_the_log_md_entry() {
        let test = pg().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;

        let report = log(
            &ctx,
            LogParams {
                text: "hello   world".to_string(),
                kind: None,
                changeset: handle.clone(),
            },
        )
        .await
        .unwrap();

        assert_eq!(report.touched, ["/log.md"]);
        let stored = entry(&test, &report.changeset, "log.md").await;
        let replay = stored.replay.expect("a log entry is replayable");
        // `kind` defaulted to Update; okf.py collapses the whitespace.
        assert_eq!(replay[0]["op"], "log");
        assert_eq!(replay[0]["bullet"], "* **Update:** hello world");
        assert!(seen(&ctx, &handle, "log.md")
            .await
            .contains("* **Update:** hello world"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_free_edit_of_an_index_after_index_clears_its_replay() {
        let test = pg().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;

        let listed = index(
            &ctx,
            IndexParams {
                path: "/infra/ntp.md".to_string(),
                summary: "how hosts sync".to_string(),
                section: None,
                title: None,
                changeset: handle.clone(),
            },
        )
        .await
        .unwrap();
        let after_index = entry(&test, &listed.changeset, "infra/index.md").await;
        edit_document(
            &ctx,
            EditDocumentParams {
                path: "/infra/index.md".to_string(),
                old_str: "the GPS clock".to_string(),
                new_str: "the GPS stratum 1 clock".to_string(),
                replace_all: false,
                changeset: handle.clone(),
            },
        )
        .await
        .unwrap();
        let after_edit = entry(&test, &listed.changeset, "infra/index.md").await;

        assert_eq!(
            after_index.replay.expect("index is replayable")[0]["op"],
            "list_bullet"
        );
        assert_eq!(after_edit.replay, None);
        let text = seen(&ctx, &handle, "infra/index.md").await;
        assert!(
            text.contains("[NTP](ntp.md) - how hosts sync") && text.contains("stratum 1"),
            "{text}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_concept_commands_edit_frontmatter_as_replayable_edits() {
        let test = pg().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;
        let path = || "/a.md".to_string();
        let ask = |question: &str| QuestionParams {
            path: path(),
            question: question.to_string(),
            changeset: handle.clone(),
        };

        stamp(
            &ctx,
            StampParams {
                path: path(),
                field: StampField::Verified,
                by: Some("human:pgrace".to_string()),
                changeset: handle.clone(),
            },
        )
        .await
        .unwrap();
        question(&ctx, ask("Who owns it?")).await.unwrap();
        question(&ctx, ask("Is it backed up?")).await.unwrap();
        // A number is the question's 1-based position, as `open_questions` numbers it.
        unquestion(
            &ctx,
            UnquestionParams {
                path: path(),
                question: QuestionRef::Number(1),
                changeset: handle.clone(),
            },
        )
        .await
        .unwrap();
        review(
            &ctx,
            ReviewParams {
                path: path(),
                outcome: "verified".to_string(),
                note: "checked".to_string(),
                questions: None,
                by: None,
                changeset: handle.clone(),
            },
        )
        .await
        .unwrap();
        for (id, resource) in [
            ("runbook", "https://example.com/runbook"),
            ("spare", "https://example.com/spare"),
        ] {
            add_source(
                &ctx,
                AddSourceParams {
                    path: path(),
                    id: id.to_string(),
                    resource: resource.to_string(),
                    title: Some("Runbook".to_string()),
                    author: None,
                    changeset: handle.clone(),
                },
            )
            .await
            .unwrap();
        }
        let last = remove_source(
            &ctx,
            RemoveSourceParams {
                path: path(),
                id: "spare".to_string(),
                changeset: handle.clone(),
            },
        )
        .await
        .unwrap();

        let text = seen(&ctx, &handle, "a.md").await;
        assert!(
            text.contains("verified:") && text.contains("human:pgrace"),
            "{text}"
        );
        assert!(
            text.contains("reviewed:") && text.contains("writer-a"),
            "{text}"
        );
        assert!(
            text.contains("Is it backed up?") && !text.contains("Who owns it?"),
            "{text}"
        );
        assert!(text.contains("https://example.com/runbook"), "{text}");
        assert!(!text.contains("spare"), "{text}");
        // Frontmatter edits are stored as replayable operations (issue #830).
        assert_eq!(last.touched, ["/a.md"]);
        let stored = entry(&test, &last.changeset, "a.md").await.replay;
        let ops: Vec<&str> = stored
            .as_ref()
            .and_then(|ops| ops.as_array())
            .expect("the frontmatter edits are replayable")
            .iter()
            .map(|op| op["op"].as_str().unwrap())
            .collect();
        assert_eq!(
            ops,
            [
                "stamp",
                "add_question",
                "add_question",
                "remove_question",
                "set_reviewed",
                "add_source",
                "add_source",
                "remove_source"
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_refused_op_is_a_tool_error_and_stores_nothing() {
        let test = pg().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;

        let outcome = review(
            &ctx,
            ReviewParams {
                path: "/a.md".to_string(),
                outcome: "current".to_string(),
                note: "checked".to_string(),
                questions: None,
                by: None,
                changeset: handle.clone(),
            },
        )
        .await
        .unwrap_err();
        // The fixture has no root index.md, so a root concept cannot be listed.
        let unlisted = index(
            &ctx,
            IndexParams {
                path: "/a.md".to_string(),
                summary: "a note".to_string(),
                section: None,
                title: None,
                changeset: handle.clone(),
            },
        )
        .await
        .unwrap_err();
        // `a.md` has no open_questions, so any number is refused.
        let none_queued = unquestion(
            &ctx,
            UnquestionParams {
                path: "/a.md".to_string(),
                question: QuestionRef::Number(3),
                changeset: handle.clone(),
            },
        )
        .await
        .unwrap_err();

        // `a.md` has no sources, so no id is there to remove.
        let no_source = remove_source(
            &ctx,
            RemoveSourceParams {
                path: "/a.md".to_string(),
                id: "runbook".to_string(),
                changeset: handle.clone(),
            },
        )
        .await
        .unwrap_err();

        assert_eq!(outcome.kind(), "Refused", "{outcome}");
        assert_eq!(unlisted.kind(), "Refused", "{unlisted}");
        assert_eq!(none_queued.kind(), "Refused", "{none_queued}");
        assert_eq!(no_source.kind(), "Refused", "{no_source}");
        // `apply` rolls back on an op error: the changeset holds nothing.
        let row = changeset_row(&test, &handle).await;
        assert!(
            changesets::entries(test.app.main_db().await.executor(), row.id)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn log_kind_takes_each_kind_null_or_nothing_and_refuses_the_rest() {
        let parse = |kind: Option<serde_json::Value>| {
            let mut args = serde_json::json!({"text": "x", "changeset": "h"});
            if let Some(kind) = kind {
                args["kind"] = kind;
            }
            serde_json::from_value::<LogParams>(args)
        };

        for (name, kind) in [
            ("Update", LogKind::Update),
            ("Creation", LogKind::Creation),
            ("Change", LogKind::Change),
            ("Import", LogKind::Import),
        ] {
            assert_eq!(parse(Some(name.into())).unwrap().kind, Some(kind));
        }
        assert_eq!(parse(Some(serde_json::Value::Null)).unwrap().kind, None);
        assert_eq!(parse(None).unwrap().kind, None);
        assert!(parse(Some("Delete".into())).is_err());
    }

    #[test]
    fn the_log_kind_schema_has_a_direct_type_and_the_four_kinds() {
        let schema = serde_json::to_value(schemars::schema_for!(LogParams)).unwrap();
        let kind = &schema["properties"]["kind"];

        assert_eq!(kind["type"], serde_json::json!(["string", "null"]));
        let kinds = &schema["$defs"]["LogKind"];
        let mut names: Vec<&str> = kinds["oneOf"]
            .as_array()
            .map_or(&[][..], Vec::as_slice)
            .iter()
            .filter_map(|variant| variant["const"].as_str())
            .collect();
        names.sort_unstable();
        assert_eq!(names, ["Change", "Creation", "Import", "Update"]);
    }

    #[test]
    fn unquestion_takes_a_number_or_text_and_log_takes_okf_py_kinds() {
        let number = serde_json::json!({"path": "/a", "question": 2, "changeset": "h"});
        let text = serde_json::json!({"path": "/a", "question": "Who owns it?", "changeset": "h"});
        let lower = serde_json::json!({"text": "x", "kind": "update", "changeset": "h"});

        let number = serde_json::from_value::<UnquestionParams>(number).unwrap();
        let text = serde_json::from_value::<UnquestionParams>(text).unwrap();
        let change = serde_json::from_value::<LogParams>(serde_json::json!({
            "text": "x",
            "kind": "Change",
            "changeset": "h"
        }))
        .unwrap();
        let refused = serde_json::from_value::<LogParams>(lower);

        assert_eq!(number.question.into_argument(), "2");
        assert_eq!(text.question.into_argument(), "Who owns it?");
        assert_eq!(change.kind, Some(LogKind::Change));
        // okf.py's argparse `choices` are case-sensitive, and so is this.
        assert!(refused.is_err());
    }

    #[test]
    fn the_bookkeeping_tools_are_listed_with_their_defaults() {
        let tools = OkfServer::bookkeeping_router().list_all();
        // `&*` borrows the name (a `Cow<str>`) as a plain `&str`.
        let names: Vec<&str> = tools.iter().map(|tool| &*tool.name).collect();
        let log = tools.iter().find(|tool| tool.name == "log").unwrap();
        let kind = log.input_schema["properties"]["kind"]["description"]
            .as_str()
            .unwrap();

        for name in [
            "stamp",
            "review",
            "question",
            "unquestion",
            "add_source",
            "remove_source",
            "log",
            "index",
            "repair_indexes",
        ] {
            assert!(names.contains(&name), "{name} is registered");
        }
        assert_eq!(names.len(), 9, "{names:?}");
        assert!(kind.contains("default Update"), "{kind}");
        for tool in &tools {
            let output = tool.output_schema.as_ref().expect("an output schema");
            assert_eq!(output["type"], "object", "{}", tool.name);
            // No tool takes a timestamp: every one comes from the server clock.
            let properties = tool.input_schema["properties"].as_object().unwrap();
            assert!(
                !properties.keys().any(|key| key == "at" || key == "now"),
                "{}",
                tool.name
            );
        }
    }
}

/// Issue #426: a new catalog works with the bookkeeping tools at once, and
/// `repair_indexes` repairs a catalog created empty, through a changeset.
#[cfg(test)]
mod repair_tests {
    use super::*;
    use crate::config::Config;
    use crate::mcp::changesets::{publish, PublishParams, Published};
    use crate::mcp::testing::{
        app_with, app_with_new, context, open, principal_as, writer, TestApp,
    };
    use crate::store::keys::Access;

    const NOTE: &str = "---\ntype: Note\ntitle: A\ndescription: A note.\n---\n# A\n";
    const NTP: &str = "---\ntype: System\ntitle: NTP\ndescription: Hosts sync.\n---\n# NTP\n";

    /// A catalog created before starter files: concepts, no index or log.
    async fn legacy() -> TestApp {
        app_with(
            &[("pg", &[("a.md", NOTE), ("infra/ntp.md", NTP)])],
            Config::default(),
        )
        .await
    }

    async fn published_text(test: &TestApp, path: &str) -> Option<String> {
        let loaded = test.app.catalog("pg").await.unwrap().unwrap().current();
        loaded
            .snapshot()
            .get(path)
            .map(|doc| doc.text().to_string())
    }

    async fn publish_as(ctx: &ToolContext, handle: &str) -> Result<Published, OkfError> {
        let params = PublishParams {
            message: "repair".to_string(),
            changeset: handle.to_string(),
        };
        publish(ctx, params).await
    }

    async fn repair(ctx: &ToolContext, handle: &str) -> Result<WriteReport, OkfError> {
        let params = RepairIndexesParams {
            changeset: handle.to_string(),
        };
        repair_indexes(ctx, params).await
    }

    fn concept(path: &str, title: &str, summary: &str, handle: &str) -> CreateConceptParams {
        let frontmatter = serde_json::json!({"type": "Note", "title": title});
        CreateConceptParams {
            path: path.to_string(),
            frontmatter: frontmatter.as_object().unwrap().clone(),
            body: format!("# {title}\n"),
            index_summary: Some(summary.to_string()),
            index_section: None,
            folder_summary: None,
            by: None,
            changeset: handle.to_string(),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_new_catalog_lists_root_and_nested_concepts_and_logs_at_once() {
        let test = app_with_new(&["pg"], Config::default()).await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;

        create_concept(&ctx, concept("/hello", "Hello", "The first note.", &handle))
            .await
            .unwrap();
        create_concept(&ctx, concept("/infra/hosts/web", "Web", "A host.", &handle))
            .await
            .unwrap();
        // Created unlisted, then listed with the `index` tool.
        let mut later = concept("/later", "Later", "unused", &handle);
        later.index_summary = None;
        create_concept(&ctx, later).await.unwrap();
        let indexed = index(
            &ctx,
            IndexParams {
                path: "/later".to_string(),
                summary: "Listed afterwards.".to_string(),
                section: None,
                title: None,
                changeset: handle.clone(),
            },
        )
        .await
        .unwrap();
        let logged = log(
            &ctx,
            LogParams {
                text: "Added the first notes.".to_string(),
                kind: None,
                changeset: handle.clone(),
            },
        )
        .await
        .unwrap();
        let published = publish_as(&ctx, &handle).await.unwrap();

        assert!(published.report.published, "{published:?}");
        assert_eq!(logged.touched, ["/log.md"]);
        let root = published_text(&test, "index.md").await.unwrap();
        assert!(
            root.contains("* [Hello](hello.md) - The first note."),
            "{root}"
        );
        assert_eq!(indexed.touched, ["/index.md"]);
        assert!(
            root.contains("* [Later](later.md) - Listed afterwards."),
            "{root}"
        );
        // The nested indexes are created and linked from their parents.
        assert!(root.contains("* [Infra](infra/) - "), "{root}");
        let infra = published_text(&test, "infra/index.md").await.unwrap();
        assert!(infra.contains("* [Hosts](hosts/) - "), "{infra}");
        let hosts = published_text(&test, "infra/hosts/index.md").await.unwrap();
        assert!(hosts.contains("* [Web](web.md) - A host."), "{hosts}");
        assert!(published_text(&test, "log.md")
            .await
            .unwrap()
            .contains("* **Update:** Added the first notes."));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_changeset_based_on_a_starter_file_conflicts_after_the_first_import() {
        let test = app_with_new(&["pg"], Config::default()).await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;
        // A free edit of the starter index.md (revision 1), not yet published.
        edit_document(
            &ctx,
            EditDocumentParams {
                path: "/index.md".to_string(),
                old_str: "TODO: say what this catalog holds.".to_string(),
                new_str: "Stale words.".to_string(),
                replace_all: false,
                changeset: handle.clone(),
            },
        )
        .await
        .unwrap();
        // The first import replaces the starter files without `replace`.
        let options = crate::store::import::ImportOptions {
            replace: false,
            scope: crate::store::import::ImportScope::Everything,
            expect_generation: None,
            expect_catalog_id: None,
            actor: "tester",
            audit: &crate::store::audit::AuditActor::cli(),
            now: chrono::Utc::now(),
        };
        let imported = "---\nokf_version: \"0.2\"\n---\n# Imported\n";
        crate::invalidate::testing::applied(&test.app, async {
            crate::store::import::import(
                &test.app.main_db().await,
                "pg",
                crate::mcp::testing::files(&[("index.md", imported), ("log.md", "# Log\n")]),
                &options,
            )
            .await
            .unwrap();
        })
        .await;

        let refused = publish_as(&ctx, &handle).await.unwrap_err();

        // The import's index.md is revision 2, so the edit based on
        // revision 1 conflicts instead of overwriting it.
        assert!(matches!(refused, OkfError::Conflict { .. }), "{refused:?}");
        assert_eq!(published_text(&test, "index.md").await.unwrap(), imported);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_legacy_catalog_refuses_listings_until_repaired() {
        let test = legacy().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;
        assert_eq!(published_text(&test, "index.md").await, None);

        let refused = create_concept(&ctx, concept("/b", "B", "b", &handle))
            .await
            .unwrap_err();

        assert!(
            matches!(&refused, OkfError::Refused { reason } if reason.contains("repair_indexes")),
            "{refused:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn repair_indexes_stages_a_legacy_catalog_s_indexes_until_publish() {
        let test = legacy().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;

        let report = repair(&ctx, &handle).await.unwrap();

        assert_eq!(
            report.touched,
            ["/index.md", "/log.md", "/infra/index.md"],
            "{report:?}"
        );
        // Each TODO the user should fill in is named.
        assert!(report
            .notes
            .iter()
            .any(|note| note == "created /index.md; replace its heading and TODO line"));
        assert!(
            report.message.starts_with("repaired 3 file(s)"),
            "{report:?}"
        );
        // Staged: nothing is live before publish.
        assert_eq!(published_text(&test, "index.md").await, None);
        let published = publish_as(&ctx, &handle).await.unwrap();
        assert!(published.report.published);
        let root = published_text(&test, "index.md").await.unwrap();
        assert!(
            root.starts_with("---\nokf_version: \"0.2\"\n---\n"),
            "{root}"
        );
        assert!(root.contains("* [A](a.md) - A note."), "{root}");
        assert!(
            root.contains("* [Infra](infra/) - TODO: describe this folder"),
            "{root}"
        );
        let infra = published_text(&test, "infra/index.md").await.unwrap();
        assert!(infra.contains("* [NTP](ntp.md) - Hosts sync."), "{infra}");
        assert_eq!(
            published_text(&test, "log.md").await.unwrap(),
            "# Knowledge Catalog Update Log\n"
        );

        // Repeating it changes nothing, so it leaves the new changeset empty.
        let again_handle = open(&ctx).await;
        let again = repair(&ctx, &again_handle).await.unwrap();
        assert!(again.touched.is_empty(), "{again:?}");
        assert_eq!(again.message, "indexes complete: nothing to repair");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn repair_indexes_needs_write_access() {
        let test = legacy().await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;
        // The same key, demoted to read: it still owns the changeset.
        let reader = context(
            &test,
            principal_as("writer-a", Access::Read, &[], Some("pg")),
            None,
        );

        let refused = repair(&reader, &handle).await.unwrap_err();

        assert!(matches!(refused, OkfError::Forbidden { .. }), "{refused:?}");
        assert_eq!(published_text(&test, "index.md").await, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_repair_published_after_another_keeps_the_curated_indexes() {
        let test = legacy().await;
        let first = writer(&test, "writer-a", "pg").await;
        let second = writer(&test, "writer-b", "pg").await;
        let first_handle = open(&first).await;
        let second_handle = open(&second).await;
        repair(&first, &first_handle).await.unwrap();
        repair(&second, &second_handle).await.unwrap();
        // The second writer curates both new indexes, then publishes first.
        edit_document(
            &second,
            EditDocumentParams {
                path: "/index.md".to_string(),
                old_str: "# Catalog\n\nTODO: say what this catalog holds.".to_string(),
                new_str: "# Ours\n\nCurated words.".to_string(),
                replace_all: false,
                changeset: second_handle.clone(),
            },
        )
        .await
        .unwrap();
        edit_document(
            &second,
            EditDocumentParams {
                path: "/infra/index.md".to_string(),
                old_str: "TODO: say what this folder holds.".to_string(),
                new_str: "Our systems.".to_string(),
                replace_all: false,
                changeset: second_handle.clone(),
            },
        )
        .await
        .unwrap();
        publish_as(&second, &second_handle).await.unwrap();
        let curated_root = published_text(&test, "index.md").await.unwrap();
        let curated_infra = published_text(&test, "infra/index.md").await.unwrap();

        // The first repair replays onto the curated files: nothing is
        // overwritten, and nothing listed twice.
        let published = publish_as(&first, &first_handle).await.unwrap();

        assert!(published.report.published, "{published:?}");
        assert_eq!(
            published_text(&test, "index.md").await.unwrap(),
            curated_root
        );
        assert_eq!(
            published_text(&test, "infra/index.md").await.unwrap(),
            curated_infra
        );
        assert!(curated_root.contains("Curated words.") && curated_infra.contains("Our systems."));
        assert_eq!(curated_root.matches("(a.md)").count(), 1, "{curated_root}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_staged_raw_listing_rewrite_replays_onto_an_index_changed_meanwhile() {
        // Issue #717: the raw bullet written before #453 is rewritten in the
        // changeset; another writer edits the index and publishes first, and
        // publish redoes the rewrite onto their text.
        let raw_index = "# D\n\nAbout d.\n\n# Concepts\n\n* [Odd](a b.md) - An odd one.\n";
        let odd = "---\ntype: Note\ntitle: Odd\ndescription: An odd one.\n---\n";
        let test = app_with(
            &[(
                "pg",
                &[
                    ("index.md", "# Root\n\n# Sections\n\n* [D](d/) - d\n"),
                    ("log.md", "# Log\n"),
                    ("d/index.md", raw_index),
                    ("d/a b.md", odd),
                ],
            )],
            Config::default(),
        )
        .await;
        let first = writer(&test, "writer-a", "pg").await;
        let second = writer(&test, "writer-b", "pg").await;
        let first_handle = open(&first).await;
        let second_handle = open(&second).await;

        let report = repair(&first, &first_handle).await.unwrap();
        assert_eq!(report.touched, ["/d/index.md"], "{report:?}");
        assert!(
            report.notes.iter().any(|note| note
                == "rewrote the raw listing of /d/a b.md in /d/index.md: \
                    * [Odd](a b.md) - An odd one. -> * [Odd](a%20b.md) - An odd one."),
            "{report:?}"
        );
        edit_document(
            &second,
            EditDocumentParams {
                path: "/d/index.md".to_string(),
                old_str: "About d.".to_string(),
                new_str: "Curated words.".to_string(),
                replace_all: false,
                changeset: second_handle.clone(),
            },
        )
        .await
        .unwrap();
        publish_as(&second, &second_handle).await.unwrap();

        let published = publish_as(&first, &first_handle).await.unwrap();

        assert!(published.report.published, "{published:?}");
        assert_eq!(
            published_text(&test, "d/index.md").await.unwrap(),
            "# D\n\nCurated words.\n\n# Concepts\n\n* [Odd](a%20b.md) - An odd one.\n"
        );
        let again_handle = open(&first).await;
        let again = repair(&first, &again_handle).await.unwrap();
        assert!(again.touched.is_empty(), "{again:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_staged_raw_folder_listing_rewrite_replays_onto_an_index_changed_meanwhile() {
        // Issue #731: the raw `# Sections` bullet of a folder is rewritten in
        // the changeset; another writer edits the parent index and publishes
        // first, and publish redoes the rewrite onto their text, with no
        // second listing of the folder.
        let raw_root = "# Root\n\nAbout it.\n\n# Sections\n\n* [My docs](my docs/) - Mine.\n";
        let note = "---\ntype: Note\ntitle: C\ndescription: c\n---\n";
        let test = app_with(
            &[(
                "pg",
                &[
                    ("index.md", raw_root),
                    ("log.md", "# Log\n"),
                    ("my docs/index.md", "# My docs\n\n* [C](c.md) - c\n"),
                    ("my docs/c.md", note),
                ],
            )],
            Config::default(),
        )
        .await;
        let first = writer(&test, "writer-a", "pg").await;
        let second = writer(&test, "writer-b", "pg").await;
        let first_handle = open(&first).await;
        let second_handle = open(&second).await;

        let report = repair(&first, &first_handle).await.unwrap();
        assert_eq!(report.touched, ["/index.md"], "{report:?}");
        assert!(
            report.notes.iter().any(|note| note
                == "rewrote the raw listing of /my docs/ in /index.md: \
                    * [My docs](my docs/) - Mine. -> * [My docs](my%20docs/) - Mine."),
            "{report:?}"
        );
        edit_document(
            &second,
            EditDocumentParams {
                path: "/index.md".to_string(),
                old_str: "About it.".to_string(),
                new_str: "Curated words.".to_string(),
                replace_all: false,
                changeset: second_handle.clone(),
            },
        )
        .await
        .unwrap();
        publish_as(&second, &second_handle).await.unwrap();

        let published = publish_as(&first, &first_handle).await.unwrap();

        assert!(published.report.published, "{published:?}");
        assert_eq!(
            published_text(&test, "index.md").await.unwrap(),
            "# Root\n\nCurated words.\n\n# Sections\n\n* [My docs](my%20docs/) - Mine.\n"
        );
        let again_handle = open(&first).await;
        let again = repair(&first, &again_handle).await.unwrap();
        assert!(again.touched.is_empty(), "{again:?}");
    }
}

/// Issue #668: describing a new folder needs no free edit of its parent
/// index, so two changesets that each create a folder both publish.
#[cfg(test)]
mod folder_summary_tests {
    use super::*;
    use crate::config::Config;
    use crate::mcp::changesets::{publish, PublishParams, Published};
    use crate::mcp::restructure::{
        move_concept, split_concept, MoveConceptParams, SplitConceptParams,
    };
    use crate::mcp::testing::{app_with, changeset_row, open, writer, TestApp};
    use crate::store::changesets;

    const ROOT: &str = "# Catalog\n\nAbout.\n\n# Sections\n\n* [Infra](infra/) - Systems.\n";
    const TIME: &str = concat!(
        "---\ntype: System\ntitle: Time\ndescription: The clock.\n---\n# Time\n\n",
        "## Ports\n\nUDP 123.\n",
    );
    const OTHER: &str = concat!(
        "---\ntype: Note\ntitle: Other\ndescription: Another.\n---\n# Other\n\n",
        "## Notes\n\nSome notes.\n",
    );

    async fn pg(extra: &[(&'static str, &'static str)]) -> TestApp {
        pg_with(ROOT, extra).await
    }

    /// The catalog with `root` as its root index, plus `extra` files.
    async fn pg_with(root: &'static str, extra: &[(&'static str, &'static str)]) -> TestApp {
        let mut files: Vec<(&str, &str)> = vec![
            ("index.md", root),
            ("log.md", "# Log\n"),
            (
                "infra/index.md",
                "# Infra\n\nSystems.\n\n# Concepts\n\n* [Time](time.md) - The clock.\n\
                 * [Other](other.md) - Another.\n",
            ),
            ("infra/time.md", TIME),
            ("infra/other.md", OTHER),
        ];
        files.extend_from_slice(extra);
        app_with(&[("pg", &files)], Config::default()).await
    }

    async fn published_text(test: &TestApp, path: &str) -> String {
        let loaded = test.app.catalog("pg").await.unwrap().unwrap().current();
        loaded
            .snapshot()
            .get(path)
            .map(|doc| doc.text().to_string())
            .unwrap_or_else(|| panic!("{path} is not published"))
    }

    async fn publish_as(ctx: &ToolContext, handle: &str) -> Result<Published, OkfError> {
        let params = PublishParams {
            message: "folders".to_string(),
            changeset: handle.to_string(),
        };
        publish(ctx, params).await
    }

    fn concept(
        path: &str,
        title: &str,
        folder_summary: Option<&str>,
        handle: &str,
    ) -> CreateConceptParams {
        let frontmatter = serde_json::json!({"type": "Note", "title": title});
        CreateConceptParams {
            path: path.to_string(),
            frontmatter: frontmatter.as_object().unwrap().clone(),
            body: format!("# {title}\n"),
            index_summary: Some(format!("About {title}.")),
            index_section: None,
            folder_summary: folder_summary.map(str::to_string),
            by: None,
            changeset: handle.to_string(),
        }
    }

    fn describe(path: &str, summary: &str, handle: &str) -> IndexParams {
        IndexParams {
            path: path.to_string(),
            summary: summary.to_string(),
            section: None,
            title: None,
            changeset: handle.to_string(),
        }
    }

    /// One session's work: a concept in a new top-level folder `folder`,
    /// described either with `folder_summary` or with the TODO and then
    /// `index` on the folder; with `extra`, also a log entry and a concept
    /// listed in the root index.
    async fn create_folder(
        ctx: &ToolContext,
        handle: &str,
        folder: &str,
        at_create: bool,
        extra: bool,
    ) {
        let summary = format!("All about {folder}.");
        let path = format!("/{folder}/first.md");
        let given = at_create.then_some(summary.as_str());
        let created = create_concept(ctx, concept(&path, "First", given, handle))
            .await
            .unwrap();
        if at_create {
            assert!(
                created
                    .notes
                    .iter()
                    .any(|n| n == &format!("created /{folder}/index.md; replace its TODO line")),
                "{created:?}"
            );
        } else {
            assert!(
                created
                    .notes
                    .iter()
                    .any(|n| n.contains("with index on the folder's path")),
                "{created:?}"
            );
            index(ctx, describe(&format!("/{folder}/"), &summary, handle))
                .await
                .unwrap();
        }
        if extra {
            let root_concept = format!("/{folder}-note.md");
            create_concept(ctx, concept(&root_concept, "Note", None, handle))
                .await
                .unwrap();
            let text = format!("made /{folder}/");
            let params = LogParams {
                text,
                kind: None,
                changeset: handle.to_string(),
            };
            log(ctx, params).await.unwrap();
        }
    }

    /// Two changesets each create a new top-level folder; both publish, and
    /// the root lists both folders with their real summaries.
    async fn two_new_folders_both_publish(at_create: bool, extra: bool) {
        let test = pg(&[]).await;
        let first = writer(&test, "writer-a", "pg").await;
        let second = writer(&test, "writer-b", "pg").await;
        let (a, b) = (open(&first).await, open(&second).await);
        create_folder(&first, &a, "learning", at_create, extra).await;
        create_folder(&second, &b, "preferences", at_create, extra).await;

        let one = publish_as(&first, &a).await.unwrap();
        let two = publish_as(&second, &b).await.unwrap();

        assert!(
            one.report.published && two.report.published,
            "{one:?} {two:?}"
        );
        assert!(
            two.report.replayed.contains(&"/index.md".to_string()),
            "{two:?}"
        );
        let root = published_text(&test, "index.md").await;
        assert!(
            root.contains("* [Learning](learning/) - All about learning.\n"),
            "{root}"
        );
        assert!(
            root.contains("* [Preferences](preferences/) - All about preferences.\n"),
            "{root}"
        );
        assert!(!root.contains("TODO"), "{root}");
        if extra {
            assert!(
                root.contains("* [Note](learning-note.md) - About Note."),
                "{root}"
            );
            assert!(
                root.contains("* [Note](preferences-note.md) - About Note."),
                "{root}"
            );
            let log = published_text(&test, "log.md").await;
            assert!(
                log.contains("made /learning/") && log.contains("made /preferences/"),
                "{log}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn two_folders_described_with_folder_summary_both_publish() {
        two_new_folders_both_publish(true, false).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn two_folders_described_after_the_todo_both_publish() {
        two_new_folders_both_publish(false, false).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn two_folders_with_folder_summary_a_log_entry_and_a_listing_both_publish() {
        two_new_folders_both_publish(true, true).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn two_folders_described_after_the_todo_with_a_log_entry_and_a_listing_both_publish() {
        two_new_folders_both_publish(false, true).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_summary_whose_listing_another_changeset_removed_fails_at_publish() {
        let root = concat!(
            "# Catalog\n\nAbout.\n\n# Sections\n\n* [Infra](infra/) - Systems.\n",
            "* [Learning](learning/) - TODO: describe this folder\n",
        );
        let test = pg_with(root, &[("learning/index.md", "# Learning\n")]).await;
        let first = writer(&test, "writer-a", "pg").await;
        let second = writer(&test, "writer-b", "pg").await;
        let (a, b) = (open(&first).await, open(&second).await);
        index(&first, describe("/learning/", "What we learn.", &a))
            .await
            .unwrap();
        // The other session drops the folder's listing (a free edit) first.
        edit_document(
            &second,
            EditDocumentParams {
                path: "/index.md".to_string(),
                old_str: "* [Learning](learning/) - TODO: describe this folder\n".to_string(),
                new_str: String::new(),
                replace_all: false,
                changeset: b.clone(),
            },
        )
        .await
        .unwrap();
        publish_as(&second, &b).await.unwrap();
        let after_removal = published_text(&test, "index.md").await;

        let error = publish_as(&first, &a).await.unwrap_err();

        let OkfError::Refused { reason } = &error else {
            panic!("not Refused: {error:?}")
        };
        assert!(
            reason.contains("index.md no longer lists /learning/"),
            "{reason}"
        );
        assert_eq!(published_text(&test, "index.md").await, after_removal);
        assert!(!after_removal.contains("What we learn."), "{after_removal}");
        // Nothing was published: the changeset still holds the edit.
        assert!(entry(&test, &a, "index.md").await.replay.is_some());
    }

    /// `pending_changes` names the replay error publish would refuse with,
    /// in place of a diff of the stored text (issue #787).
    #[tokio::test(flavor = "multi_thread")]
    async fn pending_changes_shows_a_summary_whose_listing_is_gone_as_a_replay_error() {
        use crate::mcp::changesets::{pending_changes, PendingParams};
        let root = concat!(
            "# Catalog\n\nAbout.\n\n# Sections\n\n* [Infra](infra/) - Systems.\n",
            "* [Learning](learning/) - TODO: describe this folder\n",
        );
        let test = pg_with(root, &[("learning/index.md", "# Learning\n")]).await;
        let first = writer(&test, "writer-a", "pg").await;
        let second = writer(&test, "writer-b", "pg").await;
        let (a, b) = (open(&first).await, open(&second).await);
        index(&first, describe("/learning/", "What we learn.", &a))
            .await
            .unwrap();
        edit_document(
            &second,
            EditDocumentParams {
                path: "/index.md".to_string(),
                old_str: "* [Learning](learning/) - TODO: describe this folder\n".to_string(),
                new_str: String::new(),
                replace_all: false,
                changeset: b.clone(),
            },
        )
        .await
        .unwrap();
        publish_as(&second, &b).await.unwrap();

        let params = PendingParams {
            changeset: a.clone(),
        };
        let pending = pending_changes(&first, params).await.unwrap();

        let file = &pending.files[0];
        assert_eq!(
            (
                file.path.as_str(),
                file.stale,
                file.replayable,
                file.replayed
            ),
            ("/index.md", true, true, false)
        );
        let reason = file.replay_error.as_deref().unwrap();
        assert!(
            reason.contains("index.md no longer lists /learning/"),
            "{reason}"
        );
        assert_eq!(
            file.diff,
            format!("Publish cannot replay this entry onto the published text: {reason}\n")
        );
        // Publish refuses with the same reason.
        let error = publish_as(&first, &a).await.unwrap_err();
        assert_eq!(error.to_string(), reason);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_free_edit_after_index_on_a_folder_is_stale_checked() {
        let test = pg(&[]).await;
        let first = writer(&test, "writer-a", "pg").await;
        let second = writer(&test, "writer-b", "pg").await;
        let (a, b) = (open(&first).await, open(&second).await);
        index(&first, describe("/infra/", "Our systems.", &a))
            .await
            .unwrap();
        let described = entry(&test, &a, "index.md").await;
        edit_document(
            &first,
            EditDocumentParams {
                path: "/index.md".to_string(),
                old_str: "About.".to_string(),
                new_str: "About ours.".to_string(),
                replace_all: false,
                changeset: a.clone(),
            },
        )
        .await
        .unwrap();
        let edited = entry(&test, &a, "index.md").await;
        create_folder(&second, &b, "learning", true, false).await;
        publish_as(&second, &b).await.unwrap();

        let error = publish_as(&first, &a).await.unwrap_err();

        assert_eq!(
            described.replay.expect("index on a folder is replayable")[0]["op"],
            "set_listing_summary"
        );
        assert_eq!(edited.replay, None);
        assert!(matches!(error, OkfError::Conflict { .. }), "{error:?}");
    }

    /// Issue #790: `index` on an already-listed concept re-describes it as a
    /// replayable op, so two changesets doing so for different concepts in
    /// one folder index both publish.
    #[tokio::test(flavor = "multi_thread")]
    async fn two_concepts_re_described_in_one_folder_index_both_publish() {
        let test = pg(&[]).await;
        let first = writer(&test, "writer-a", "pg").await;
        let second = writer(&test, "writer-b", "pg").await;
        let (a, b) = (open(&first).await, open(&second).await);
        index(&first, describe("/infra/time", "Keeps the clock.", &a))
            .await
            .unwrap();
        index(&second, describe("/infra/other", "More notes.", &b))
            .await
            .unwrap();
        let stored = entry(&test, &a, "infra/index.md").await;

        let one = publish_as(&first, &a).await.unwrap();
        let two = publish_as(&second, &b).await.unwrap();

        assert_eq!(
            stored.replay.expect("replayable")[0]["op"],
            "set_listing_summary"
        );
        assert!(
            one.report.published && two.report.published,
            "{one:?} {two:?}"
        );
        assert!(
            two.report.replayed.contains(&"/infra/index.md".to_string()),
            "{two:?}"
        );
        let text = published_text(&test, "infra/index.md").await;
        assert!(
            text.contains("* [Time](time.md) - Keeps the clock.\n"),
            "{text}"
        );
        assert!(
            text.contains("* [Other](other.md) - More notes.\n"),
            "{text}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_concept_summary_whose_listing_another_changeset_removed_fails_at_publish() {
        let test = pg(&[]).await;
        let first = writer(&test, "writer-a", "pg").await;
        let second = writer(&test, "writer-b", "pg").await;
        let (a, b) = (open(&first).await, open(&second).await);
        index(&first, describe("/infra/time", "Keeps the clock.", &a))
            .await
            .unwrap();
        edit_document(
            &second,
            EditDocumentParams {
                path: "/infra/index.md".to_string(),
                old_str: "* [Time](time.md) - The clock.\n".to_string(),
                new_str: String::new(),
                replace_all: false,
                changeset: b.clone(),
            },
        )
        .await
        .unwrap();
        publish_as(&second, &b).await.unwrap();

        let error = publish_as(&first, &a).await.unwrap_err();

        let OkfError::Refused { reason } = &error else {
            panic!("not Refused: {error:?}")
        };
        assert!(
            reason.contains("infra/index.md no longer lists /infra/time.md"),
            "{reason}"
        );
        let text = published_text(&test, "infra/index.md").await;
        assert!(!text.contains("Keeps the clock."), "{text}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_free_edit_after_index_on_a_concept_is_stale_checked() {
        let test = pg(&[]).await;
        let first = writer(&test, "writer-a", "pg").await;
        let second = writer(&test, "writer-b", "pg").await;
        let (a, b) = (open(&first).await, open(&second).await);
        index(&first, describe("/infra/time", "Keeps the clock.", &a))
            .await
            .unwrap();
        edit_document(
            &first,
            EditDocumentParams {
                path: "/infra/index.md".to_string(),
                old_str: "Systems.".to_string(),
                new_str: "Our systems.".to_string(),
                replace_all: false,
                changeset: a.clone(),
            },
        )
        .await
        .unwrap();
        index(&second, describe("/infra/other", "More notes.", &b))
            .await
            .unwrap();
        publish_as(&second, &b).await.unwrap();

        let error = publish_as(&first, &a).await.unwrap_err();

        assert!(matches!(error, OkfError::Conflict { .. }), "{error:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_move_and_a_split_into_new_folders_both_publish() {
        let test = pg(&[]).await;
        let first = writer(&test, "writer-a", "pg").await;
        let second = writer(&test, "writer-b", "pg").await;
        let (a, b) = (open(&first).await, open(&second).await);
        let moved = MoveConceptParams {
            old: "/infra/time".to_string(),
            new: "/clocks/time.md".to_string(),
            folder_summary: Some("Every clock.".to_string()),
            changeset: a.clone(),
        };
        let moved = move_concept(&first, moved).await.unwrap();
        let split = SplitConceptParams {
            path: "/infra/other".to_string(),
            section: "Notes".to_string(),
            to: "/notes/other-notes.md".to_string(),
            kind: None,
            title: None,
            description: Some("Notes on other.".to_string()),
            stub: None,
            sources: None,
            folder_summary: Some("Loose notes.".to_string()),
            by: None,
            changeset: b.clone(),
        };
        let split = split_concept(&second, split).await.unwrap();

        let one = publish_as(&second, &b).await.unwrap();
        let two = publish_as(&first, &a).await.unwrap();

        assert!(
            moved
                .notes
                .iter()
                .any(|n| n == "created /clocks/index.md; replace its TODO line"),
            "{moved:?}"
        );
        assert!(
            split
                .notes
                .iter()
                .any(|n| n == "created /notes/index.md; replace its TODO line"),
            "{split:?}"
        );
        assert!(
            one.report.published && two.report.published,
            "{one:?} {two:?}"
        );
        assert!(
            two.report.replayed.contains(&"/index.md".to_string()),
            "{two:?}"
        );
        let root = published_text(&test, "index.md").await;
        assert!(
            root.contains("* [Clocks](clocks/) - Every clock.\n"),
            "{root}"
        );
        assert!(
            root.contains("* [Notes](notes/) - Loose notes.\n"),
            "{root}"
        );
    }

    /// The entry for `path` of the changeset `handle` names.
    async fn entry(test: &TestApp, handle: &str, path: &str) -> crate::store::changesets::EntryRow {
        let row = changeset_row(test, handle).await;
        changesets::entries(test.app.main_db().await.executor(), row.id)
            .await
            .unwrap()
            .into_iter()
            .find(|entry| entry.path == path)
            .unwrap_or_else(|| panic!("no entry for {path}"))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_secret_in_a_folder_summary_is_refused() {
        let test = pg(&[]).await;
        let ctx = writer(&test, "writer-a", "pg").await;
        let handle = open(&ctx).await;
        // Built at run time, so no source file holds a secret-shaped string.
        let secret = format!("password = {}", "Ab1".repeat(4));

        let created = create_concept(&ctx, concept("/ops/x.md", "X", Some(&secret), &handle)).await;
        let described = index(&ctx, describe("/infra/", &secret, &handle)).await;

        assert!(
            matches!(created, Err(OkfError::SecretDetected { .. })),
            "{created:?}"
        );
        assert!(
            matches!(described, Err(OkfError::SecretDetected { .. })),
            "{described:?}"
        );
    }
}

#[cfg(test)]
mod set_frontmatter_value_schema_tests {
    use super::*;
    use serde_json::json;

    /// The published `value` schema names a direct type for every JSON type.
    #[test]
    fn the_published_value_schema_lists_every_json_type() {
        let schema = schemars::schema_for!(SetFrontmatterParams);
        let schema = serde_json::to_value(&schema).unwrap();
        let kinds = &schema["properties"]["value"]["type"];
        let mut listed: Vec<&str> = kinds
            .as_array()
            .expect("value has a type array")
            .iter()
            .filter_map(|k| k.as_str())
            .collect();
        listed.sort_unstable();
        assert_eq!(
            listed,
            ["array", "boolean", "integer", "null", "number", "object", "string"]
        );
        assert!(schema["properties"]["value"]["items"]["type"].is_array());
    }

    /// Every kind the schema lists still deserializes, null included.
    #[test]
    fn every_listed_value_kind_still_deserializes() {
        for value in [
            json!("text"),
            json!(3),
            json!(1.5),
            json!(true),
            json!(["a", "b"]),
            json!([{"k": ["nested"]}]),
            json!({"by": "x", "n": {"deep": 1}}),
            json!(null),
        ] {
            let params: SetFrontmatterParams = serde_json::from_value(
                json!({"path": "/a", "key": "k", "value": value, "changeset": "c"}),
            )
            .expect("accepted kind deserializes");
            assert_eq!(params.value, value);
        }
    }

    /// A missing `value` stays a parameter error.
    #[test]
    fn a_missing_value_is_still_rejected() {
        let missing = serde_json::from_value::<SetFrontmatterParams>(
            json!({"path": "/a", "key": "k", "changeset": "c"}),
        );
        assert!(missing.is_err());
    }
}

/// Issue #830: the frontmatter bookkeeping tools (`stamp`, `review`,
/// `question`, `unquestion`, `add_source`, `remove_source`) replay onto
/// text another changeset published since, instead of forcing a discard.
#[cfg(test)]
mod frontmatter_replay_tests {
    use super::*;
    use crate::config::Config;
    use crate::mcp::changesets::{pending_changes, publish, PendingParams, PublishParams};
    use crate::mcp::testing::{app_with, open, writer, TestApp};

    const NTP: &str = concat!(
        "---\ntype: System\ntitle: NTP\ndescription: Hosts sync.\n",
        "sources:\n  - id: s0\n    resource: https://example.com/ntp\n",
        "open_questions:\n  - \"Old?\"\n---\n# NTP\n",
    );

    async fn pg() -> TestApp {
        app_with(&[("pg", &[("infra/ntp.md", NTP)])], Config::default()).await
    }

    /// Two writers with a changeset each, both on `/infra/ntp.md`'s revision 1.
    async fn two(test: &TestApp) -> (ToolContext, String, ToolContext, String) {
        let first = writer(test, "writer-a", "pg").await;
        let second = writer(test, "writer-b", "pg").await;
        let (a, b) = (open(&first).await, open(&second).await);
        (first, a, second, b)
    }

    async fn publish_as(
        ctx: &ToolContext,
        handle: &str,
    ) -> Result<crate::mcp::changesets::Published, OkfError> {
        let params = PublishParams {
            message: "m".to_string(),
            changeset: handle.to_string(),
        };
        publish(ctx, params).await
    }

    async fn published_ntp(test: &TestApp) -> String {
        let loaded = test.app.catalog("pg").await.unwrap().unwrap().current();
        loaded
            .snapshot()
            .get("infra/ntp.md")
            .map(|doc| doc.text().to_string())
            .unwrap()
    }

    async fn add(ctx: &ToolContext, handle: &str, id: &str) {
        let params = AddSourceParams {
            path: "/infra/ntp.md".to_string(),
            id: id.to_string(),
            resource: format!("https://example.com/{id}"),
            title: None,
            author: None,
            changeset: handle.to_string(),
        };
        add_source(ctx, params).await.unwrap();
    }

    async fn stamp_verified(ctx: &ToolContext, handle: &str) {
        let params = StampParams {
            path: "/infra/ntp.md".to_string(),
            field: StampField::Verified,
            by: Some("other".to_string()),
            changeset: handle.to_string(),
        };
        stamp(ctx, params).await.unwrap();
    }

    /// `ctx`'s pending entry for the concept: (stale, replayable, replayed, error).
    async fn shown(ctx: &ToolContext, handle: &str) -> crate::mcp::changesets::PendingFile {
        let params = PendingParams {
            changeset: handle.to_string(),
        };
        let mut files = pending_changes(ctx, params).await.unwrap().files;
        assert_eq!(files.len(), 1, "{files:?}");
        files.remove(0)
    }

    /// Checks a stale entry replays (shown and published), and returns the
    /// published text, which then holds the other writer's stamp too.
    async fn replays(test: &TestApp, first: &ToolContext, a: &str) -> String {
        let file = shown(first, a).await;
        assert_eq!(
            (
                file.stale,
                file.replayable,
                file.replayed,
                file.replay_error.as_deref()
            ),
            (true, true, true, None),
            "{}",
            file.diff
        );
        let published = publish_as(first, a).await.unwrap();
        assert_eq!(published.report.replayed, ["/infra/ntp.md"]);
        let text = published_ntp(test).await;
        assert!(text.contains("by: other"), "{text}");
        text
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn add_source_replays_over_a_concurrent_stamp() {
        let test = pg().await;
        let (first, a, second, b) = two(&test).await;
        add(&first, &a, "s1").await;
        add(&first, &a, "s2").await;
        stamp_verified(&second, &b).await;
        publish_as(&second, &b).await.unwrap();

        let text = replays(&test, &first, &a).await;

        assert!(text.contains("id: s1") && text.contains("id: s2"), "{text}");
        assert!(text.contains("id: s0"), "{text}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remove_source_replays_over_a_concurrent_stamp() {
        let test = pg().await;
        let (first, a, second, b) = two(&test).await;
        let params = RemoveSourceParams {
            path: "/infra/ntp.md".to_string(),
            id: "s0".to_string(),
            changeset: a.clone(),
        };
        remove_source(&first, params).await.unwrap();
        stamp_verified(&second, &b).await;
        publish_as(&second, &b).await.unwrap();

        let text = replays(&test, &first, &a).await;

        assert!(!text.contains("sources"), "{text}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn question_and_unquestion_replay_over_a_concurrent_stamp() {
        let test = pg().await;
        let (first, a, second, b) = two(&test).await;
        let question_params = QuestionParams {
            path: "/infra/ntp.md".to_string(),
            question: "New?".to_string(),
            changeset: a.clone(),
        };
        question(&first, question_params).await.unwrap();
        let unquestion_params = UnquestionParams {
            path: "/infra/ntp.md".to_string(),
            question: QuestionRef::Number(1),
            changeset: a.clone(),
        };
        unquestion(&first, unquestion_params).await.unwrap();
        stamp_verified(&second, &b).await;
        publish_as(&second, &b).await.unwrap();

        let text = replays(&test, &first, &a).await;

        assert!(
            text.contains("\"New?\"") && !text.contains("Old?"),
            "{text}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stamp_and_review_replay_over_a_concurrent_source() {
        let test = pg().await;
        let (first, a, second, b) = two(&test).await;
        let review_params = ReviewParams {
            path: "/infra/ntp.md".to_string(),
            outcome: "verified".to_string(),
            note: "Checked it.".to_string(),
            questions: Some(vec!["Why?".to_string()]),
            by: Some("reviewer".to_string()),
            changeset: a.clone(),
        };
        review(&first, review_params).await.unwrap();
        let stamp_params = StampParams {
            path: "/infra/ntp.md".to_string(),
            field: StampField::Generated,
            by: Some("stamper".to_string()),
            changeset: a.clone(),
        };
        stamp(&first, stamp_params).await.unwrap();
        add(&second, &b, "s9").await;
        publish_as(&second, &b).await.unwrap();

        let file = shown(&first, &a).await;
        assert_eq!(
            (file.stale, file.replayable, file.replayed),
            (true, true, true),
            "{}",
            file.diff
        );
        publish_as(&first, &a).await.unwrap();

        let text = published_ntp(&test).await;
        assert!(text.contains("id: s9"), "{text}");
        assert!(
            text.contains("by: reviewer") && text.contains("by: stamper"),
            "{text}"
        );
        assert!(
            text.contains("\"Why?\"") && text.contains("\"Old?\""),
            "{text}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_same_source_id_on_both_sides_is_a_replay_error() {
        let test = pg().await;
        let (first, a, second, b) = two(&test).await;
        add(&first, &a, "s1").await;
        add(&second, &b, "s1").await;
        publish_as(&second, &b).await.unwrap();

        let file = shown(&first, &a).await;

        assert_eq!(
            (file.stale, file.replayable, file.replayed),
            (true, true, false)
        );
        let reason = file.replay_error.as_deref().unwrap();
        assert!(
            reason.contains("already has a source with id 's1'"),
            "{reason}"
        );
        let error = publish_as(&first, &a).await.unwrap_err();
        assert_eq!(error.to_string(), reason);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_source_now_cited_cannot_be_removed_on_replay() {
        let test = pg().await;
        let (first, a, second, b) = two(&test).await;
        let params = RemoveSourceParams {
            path: "/infra/ntp.md".to_string(),
            id: "s0".to_string(),
            changeset: a.clone(),
        };
        remove_source(&first, params).await.unwrap();
        let edit = EditDocumentParams {
            path: "/infra/ntp.md".to_string(),
            old_str: "# NTP\n".to_string(),
            new_str: "# NTP\n\nSyncs.[^s0]\n\n[^s0]: s0\n".to_string(),
            replace_all: false,
            changeset: b.clone(),
        };
        edit_document(&second, edit).await.unwrap();
        publish_as(&second, &b).await.unwrap();

        let file = shown(&first, &a).await;

        assert_eq!((file.replayable, file.replayed), (true, false));
        let reason = file.replay_error.as_deref().unwrap();
        assert!(reason.contains("still cites source 's0'"), "{reason}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_free_edit_on_the_path_still_makes_the_entry_a_conflict() {
        let test = pg().await;
        let (first, a, second, b) = two(&test).await;
        add(&first, &a, "s1").await;
        let edit = EditDocumentParams {
            path: "/infra/ntp.md".to_string(),
            old_str: "# NTP\n".to_string(),
            new_str: "# NTP\n\nMine.\n".to_string(),
            replace_all: false,
            changeset: a.clone(),
        };
        edit_document(&first, edit).await.unwrap();
        stamp_verified(&second, &b).await;
        publish_as(&second, &b).await.unwrap();

        let file = shown(&first, &a).await;
        let error = publish_as(&first, &a).await.unwrap_err();

        assert_eq!((file.stale, file.replayable), (true, false));
        assert!(matches!(error, OkfError::Conflict { .. }), "{error:?}");
    }
}
