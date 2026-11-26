//! The crate's error type.

use crate::okf::analysis::Finding;
use crate::okf::paths::{escapes_root, normpath};
use crate::telemetry::subscriber::split_event;

/// Errors from reading, parsing or editing an OKF catalog.
///
/// Later milestones add variants (auth, lint, publish); variants that
/// concern one document carry its catalog-relative path, so an agent can act
/// on it. The internal value may lack the leading `/`; [`OkfError::to_json`]
/// writes every catalog path `/`-prefixed (M8 Decision 9).
#[derive(Debug, thiserror::Error)]
pub enum OkfError {
    /// No concept or document at the given path.
    #[error("{path}: no such concept or document")]
    NotFound {
        /// The path as the caller gave it (`/`-prefixed in the payload).
        path: String,
    },

    /// The path is outside the catalog, reserved, machinery, or already taken.
    #[error("{path}: {reason}")]
    InvalidPath {
        /// The path as the caller gave it, or normalized. The payload
        /// `/`-prefixes it, unless it is empty or climbs out of the catalog
        /// once normalized (`../x`), which it echoes as given.
        path: String,
        /// Why it was refused.
        reason: String,
    },

    /// A document's YAML frontmatter does not parse (or would not, after an edit).
    #[error("{path}: frontmatter does not parse (line {line}): {message}")]
    FrontmatterParse {
        /// Catalog-relative path of the document (`/`-prefixed in the payload).
        path: String,
        /// 1-based line number in the file.
        line: usize,
        /// The parser's message.
        message: String,
    },

    /// A write was refused by the secret scan. Only the rule, the line and
    /// the scan's message are kept; the message shows at most the value's
    /// first four characters, so the secret itself is never returned or logged.
    #[error("{path}: line {line}: write refused, {message} ({rule})")]
    SecretDetected {
        /// Catalog-relative path of the file (`/`-prefixed in the payload).
        path: String,
        /// 1-based line of the first hit.
        line: usize,
        /// The scan rule, e.g. `secret:github-token`.
        rule: String,
        /// The scan's message, with the value masked.
        message: String,
    },

    /// A frontmatter key has a shape the edit can't handle (e.g. `sources` is a string).
    #[error("{path}: {key} has an unexpected shape; fix it by hand")]
    Shape {
        /// Catalog-relative path of the document (`/`-prefixed in the payload).
        path: String,
        /// The frontmatter key.
        key: String,
    },

    /// `edit_document`'s old text does not occur exactly once (or, with
    /// `replace_all`, not at all).
    #[error(
        "{path}: the old text occurs {matches} times; it must occur exactly once, \
         or at least once with replace_all"
    )]
    EditMismatch {
        /// Catalog-relative path of the document (`/`-prefixed in the payload).
        path: String,
        /// How many times the old text occurs.
        matches: usize,
    },

    /// A filesystem operation failed.
    #[error("{path}: {source}")]
    Io {
        /// Catalog-relative path being read or written (sometimes a
        /// filesystem path or an archive entry's name); the payload writes it
        /// unchanged.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// An import was refused; every problem found is listed.
    // `.problems` in the format arguments refers to the field (thiserror syntax).
    #[error("import refused: {}", .problems.join("; "))]
    ImportRefused {
        /// One line per problem: the entry, and what is wrong with it.
        problems: Vec<String>,
    },

    /// A database operation failed. M4 logs the details and returns only the
    /// variant to MCP clients; the CLI prints them.
    #[error("database: {source}")]
    Database {
        /// The driver's error.
        // `#[source]`, not `#[from]`: the `From` impl below is written by
        // hand, so a deadlock becomes `Busy` instead.
        #[source]
        source: sqlx::Error,
    },

    /// Applying the schema migrations failed.
    #[error("migrations: {source}")]
    Migrate {
        /// The migrator's error.
        #[from]
        source: sqlx::migrate::MigrateError,
    },

    /// The configuration file is unreadable or invalid.
    #[error("{path}: {message}")]
    Config {
        /// The file, as given.
        path: String,
        /// What is wrong with it.
        message: String,
    },

    /// The named catalog does not exist (from M4, also: the key may not reach it).
    #[error("catalog {catalog:?} does not exist or is not available")]
    CatalogNotSelected {
        /// The catalog name as given.
        catalog: String,
    },

    /// No user or team by that name.
    #[error("{noun} {name:?} does not exist")]
    UnknownName {
        /// `"user"` or `"team"`. (`&'static str`: a string literal baked into
        /// the binary, so the variant borrows nothing at run time.)
        noun: &'static str,
        /// The name as given.
        name: String,
    },

    /// The key reaches the catalog but its access level does not allow this.
    #[error("forbidden: {reason}")]
    Forbidden {
        /// What was refused, and the access it needs.
        reason: String,
    },

    /// The key reaches the catalog, but its owner team is suspended: only
    /// export works until the operator lifts it.
    // `.catalog`, `.team` and `.notice` name the variant's fields inside
    // `#[error]`; `.as_deref()` turns `&Option<String>` into `Option<&str>`.
    #[error("{}", suspended_message(.catalog, .catalog_label, .team, .team_label, .notice.as_deref()))]
    Suspended {
        /// The catalog asked for (its id).
        catalog: String,
        /// The catalog's label.
        catalog_label: String,
        /// Its owner team (its id).
        team: String,
        /// The owner team's label.
        team_label: String,
        /// The operator's notice, if any.
        notice: Option<String>,
    },

    /// Operator access to a catalog's contents without an active unlock, on
    /// a server with `require_unlock` on (spec Part 6).
    // `*.has_user` copies the `bool` out of the borrowed field.
    #[error("{}", unlock_message(.catalog, *.has_user))]
    UnlockRequired {
        /// The catalog asked for.
        catalog: String,
        /// Whether the credential has a user (one without cannot be unlocked).
        has_user: bool,
    },

    /// The maintenance runner on a catalog whose owners turned maintenance
    /// off. Its kind is `Forbidden` (spec Part 6); the body's
    /// `"maintenance": "off"` lets the runner skip the catalog.
    #[error(
        "forbidden: maintenance is off for catalog {catalog:?}; an owner of its team can turn it on"
    )]
    MaintenanceOff {
        /// The catalog.
        catalog: String,
    },

    /// An administrative request was refused; the reason says why and what to do.
    #[error("{reason}")]
    Refused {
        /// Why.
        reason: String,
    },

    /// The request proves no identity: a wrong password, an unknown email,
    /// a locked account (all worded alike, spec "Passwords"), or no live
    /// session.
    #[error("{reason}")]
    Unauthenticated {
        /// What the person is told.
        reason: String,
    },

    /// A single-use link or token that has expired, been used, or never
    /// existed (worded alike, so a link cannot be probed).
    #[error("{reason}")]
    Gone {
        /// What the person is told.
        reason: String,
    },

    /// A self-service request goes past a limit: the user's ceiling, the
    /// key count, the key lifetime, the password policy, or a rule of
    /// self-issued credentials.
    #[error("{reason}")]
    LimitExceeded {
        /// Which limit, as a setting or rule name (`"ceiling"`,
        /// `"max_keys_per_user"`, `"max_key_lifetime"`, `"password"`, ...).
        /// `&'static str`: always a literal from the code.
        limit: &'static str,
        /// What the person is told.
        reason: String,
    },

    /// The database aborted the request to keep others going (a deadlock or
    /// serialization failure), or the changeset changed under it. Nothing
    /// was stored: retry.
    #[error("busy: {reason}; retry")]
    Busy {
        /// What was aborted.
        reason: String,
    },

    /// The store holding the catalog's data cannot be reached. Retry later.
    /// The message never names the store (it is operator detail); the name
    /// goes to the local log when the error is made, see
    /// [`OkfError::store_unavailable`].
    #[error("the catalog's data is temporarily unavailable; retry later")]
    StoreUnavailable {
        /// The store's name. Read only by the local log, never by a caller.
        store: String,
    },

    /// Publish was refused: lint errors in the files the changeset changes,
    /// or links from other documents into paths it deletes.
    #[error(
        "publish refused: {} error(s) in the changed files; fix them and publish again",
        .errors.len()
    )]
    LintFailed {
        /// The refusing findings (`file` is catalog-relative, no leading `/`;
        /// the payload `/`-prefixes it).
        errors: Vec<Finding>,
        /// Advice: errors in files the changeset does not change, and
        /// warnings in those it does.
        warnings: Vec<Finding>,
    },

    /// Publish found documents changed since the changeset read them.
    #[error(
        "publish conflict: {} file(s) changed since this changeset read them; re-read, \
         re-apply and publish again",
        .files.len()
    )]
    Conflict {
        /// Each conflicting file, with the revision its entry was based on
        /// and the published one.
        files: Vec<ConflictFile>,
    },

    /// `delete_concept` was refused: other documents still link to the
    /// concept.
    #[error(
        "{path}: still linked from {} place(s); retarget or remove those links first \
         (move_concept retargets them)",
        .links.len()
    )]
    StillLinked {
        /// The concept's path, catalog-relative with a leading `/`
        /// (`/infra/x.md`), as tool results (`touched`, `message`) and
        /// `Conflict` write them.
        path: String,
        /// Each link into it, in walk order, then line order.
        links: Vec<LinkSource>,
    },

    /// No changeset with this handle that the key may use: it does not exist,
    /// belongs to another key, or is in a catalog the key does not reach
    /// (worded alike, so a key cannot probe for other keys' handles).
    #[error(
        "{changeset}: no such changeset; open one with open_changeset, or see \
         list_changesets"
    )]
    UnknownChangeset {
        /// The handle as given.
        changeset: String,
    },
}

/// One file of an [`OkfError::Conflict`] (a record, so its fields are public).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct ConflictFile {
    /// `/`-prefixed path.
    pub path: String,
    /// The published revision the changeset's entry was made on (`None`: the
    /// path had no live document then).
    pub base_revision: Option<i32>,
    /// The published revision now (`None`: no live document).
    pub current_revision: Option<i32>,
}

/// One link of an [`OkfError::StillLinked`] (a record, so its fields are
/// public).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct LinkSource {
    /// The linking document's path, catalog-relative with a leading `/`
    /// (`/hub.md`), as tool results (`touched`, `message`) and `Conflict`
    /// write them.
    pub source: String,
    /// The 1-based line of the link (frontmatter lines count, as in lint).
    pub line: usize,
}

impl OkfError {
    /// A [`OkfError::StoreUnavailable`] for `store`, logging the store's name
    /// in the local half of the event only (a store's name is operator
    /// detail: it stays out of the response and out of OTLP).
    ///
    /// # Arguments
    /// * `store` - the store that could not be reached.
    ///
    /// # Returns
    /// The error.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn store_unavailable(store: &crate::stores::StoreName) -> Self {
        split_event!(
            warn,
            {},
            { store = %store, },
            "a catalog store is unavailable"
        );
        OkfError::StoreUnavailable {
            store: store.as_str().to_string(),
        }
    }

    /// The variant's name, as clients see it in a tool error's `error` field.
    ///
    /// # Returns
    /// The name, e.g. `"NotFound"`.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "a pure match over the error's variants: a closed-value classifier called while the tool span records its kind, so a span would record nothing new"
        )
    )]
    pub fn kind(&self) -> &'static str {
        // `{ .. }` matches a variant whatever its fields hold. No `_` arm:
        // a new variant fails to compile here until it is given a name.
        match self {
            OkfError::NotFound { .. } => "NotFound",
            OkfError::InvalidPath { .. } => "InvalidPath",
            OkfError::FrontmatterParse { .. } => "FrontmatterParse",
            OkfError::SecretDetected { .. } => "SecretDetected",
            OkfError::Shape { .. } => "Shape",
            OkfError::EditMismatch { .. } => "EditMismatch",
            OkfError::Io { .. } => "Io",
            OkfError::ImportRefused { .. } => "ImportRefused",
            OkfError::Database { .. } => "Database",
            OkfError::Migrate { .. } => "Migrate",
            OkfError::Config { .. } => "Config",
            OkfError::CatalogNotSelected { .. } => "CatalogNotSelected",
            OkfError::UnknownName { .. } => "UnknownName",
            OkfError::Forbidden { .. } => "Forbidden",
            OkfError::Suspended { .. } => "Suspended",
            OkfError::UnlockRequired { .. } => "UnlockRequired",
            // The one variant whose kind is not its name, on purpose: the
            // spec refuses the runner with `Forbidden`, and the body's
            // `"maintenance": "off"` tells it apart (plan Decision 7).
            OkfError::MaintenanceOff { .. } => "Forbidden",
            OkfError::Refused { .. } => "Refused",
            OkfError::Unauthenticated { .. } => "Unauthenticated",
            OkfError::Gone { .. } => "Gone",
            OkfError::LimitExceeded { .. } => "LimitExceeded",
            OkfError::Busy { .. } => "Busy",
            OkfError::StoreUnavailable { .. } => "StoreUnavailable",
            OkfError::LintFailed { .. } => "LintFailed",
            OkfError::Conflict { .. } => "Conflict",
            OkfError::StillLinked { .. } => "StillLinked",
            OkfError::UnknownChangeset { .. } => "UnknownChangeset",
        }
    }

    /// Is this the server's fault, with details for the log only?
    ///
    /// # Returns
    /// `true` for database, migration, filesystem and configuration failures:
    /// the errors the HTTP routes answer with 500.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "a pure match over the error's variants: a closed-value classifier called while the tool span records its kind, so a span would record nothing new"
        )
    )]
    pub fn is_internal(&self) -> bool {
        matches!(
            self,
            OkfError::Database { .. }
                | OkfError::Migrate { .. }
                | OkfError::Io { .. }
                | OkfError::Config { .. }
        )
    }

    /// Is this an internal error that is probably a passing outage rather
    /// than a bug (issue #766)? An outage hits every connected agent at
    /// once, so the tool error leaves the "report it" hint off for it.
    ///
    /// # Returns
    /// `true` for a [`OkfError::Database`] whose driver error is a pool
    /// timeout, a closed pool, a transport or TLS failure (a reset
    /// connection), a crashed pool worker, or a Postgres connection
    /// exception (SQLSTATE class `08`) or shutdown (`57P01`, `57P02`,
    /// `57P03`). `false` for every other error.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "a pure match over the error's variants: a closed-value classifier called while the tool span records its kind, so a span would record nothing new"
        )
    )]
    pub fn is_transient(&self) -> bool {
        let OkfError::Database { source } = self else {
            return false;
        };
        match source {
            sqlx::Error::PoolTimedOut
            | sqlx::Error::PoolClosed
            | sqlx::Error::Io(_)
            | sqlx::Error::Tls(_)
            | sqlx::Error::WorkerCrashed => true,
            other => other
                .as_database_error()
                .and_then(|db_error| db_error.code())
                .is_some_and(|code| {
                    code.starts_with("08") || ["57P01", "57P02", "57P03"].contains(&code.as_ref())
                }),
        }
    }

    /// The error as clients see it: `{"error": kind, "message": text}`, plus
    /// the `path`, `catalog`, `noun` and `name`, or `problems` it concerns
    /// (a `Suspended` error adds `catalog`, `team` and `notice`, `null` when
    /// the operator left none; a `MaintenanceOff` error, whose kind is
    /// `Forbidden`, adds `catalog` and `"maintenance": "off"`).
    ///
    /// Every catalog path in the body is `/`-prefixed (`path`, `files[].path`,
    /// `links[].source`, `errors[].file`, `warnings[].file`). An
    /// `InvalidPath` that is empty or climbs out of the catalog, and an `Io`
    /// path, are written as they are. The message is unchanged.
    ///
    /// # Returns
    /// The JSON body. An internal error's message says only that the server
    /// log has the details, so a driver message never reaches a client.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn to_json(&self) -> serde_json::Value {
        let message = if self.is_internal() {
            "internal error; the server log has the details".to_string()
        } else {
            // `to_string` comes from the `#[error(...)]` text (thiserror
            // implements `Display`), like Python's `str(exc)`.
            self.to_string()
        };
        let mut body = serde_json::json!({ "error": self.kind(), "message": message });
        // One arm can bind `path` from several variants (`|`) because each
        // has a field of that name and type.
        match self {
            OkfError::NotFound { path }
            | OkfError::FrontmatterParse { path, .. }
            | OkfError::Shape { path, .. } => body["path"] = client_path(path).into(),
            OkfError::InvalidPath { path, .. } => {
                // `escapes_root` expects a normalized path, so normalize first:
                // `infra/../../x.md` climbs out although it starts with a name.
                body["path"] = if path.is_empty() || escapes_root(&normpath(path)) {
                    path.as_str().into()
                } else {
                    client_path(path).into()
                };
            }
            OkfError::Io { path, .. } => body["path"] = path.as_str().into(),
            OkfError::SecretDetected {
                path, line, rule, ..
            } => {
                body["path"] = client_path(path).into();
                body["line"] = (*line).into();
                body["rule"] = rule.as_str().into();
            }
            OkfError::EditMismatch { path, matches } => {
                body["path"] = client_path(path).into();
                // `*matches` copies the `usize` out of the borrowed field.
                body["matches"] = (*matches).into();
            }
            OkfError::CatalogNotSelected { catalog } => body["catalog"] = catalog.as_str().into(),
            OkfError::UnknownName { noun, name } => {
                // `*noun` copies the `&'static str` out of the borrowed field.
                body["noun"] = (*noun).into();
                body["name"] = name.as_str().into();
            }
            OkfError::UnknownChangeset { changeset } => {
                body["changeset"] = changeset.as_str().into();
            }
            OkfError::ImportRefused { problems } => body["problems"] = problems.as_slice().into(),
            OkfError::LintFailed { errors, warnings } => {
                body["errors"] = findings_json(errors);
                body["warnings"] = findings_json(warnings);
            }
            OkfError::Conflict { files } => body["files"] = json_list(files),
            OkfError::StillLinked { path, links } => {
                body["path"] = path.as_str().into();
                body["links"] = json_list(links);
            }
            OkfError::LimitExceeded { limit, .. } => body["limit"] = (*limit).into(),
            OkfError::Suspended {
                catalog,
                team,
                notice,
                ..
            } => {
                body["catalog"] = catalog.as_str().into();
                body["team"] = team.as_str().into();
                // `Option<&str>` into JSON: a string, or `null` for `None`.
                body["notice"] = notice.as_deref().into();
            }
            OkfError::UnlockRequired { catalog, .. } => body["catalog"] = catalog.as_str().into(),
            OkfError::MaintenanceOff { catalog } => {
                body["catalog"] = catalog.as_str().into();
                body["maintenance"] = "off".into();
            }
            OkfError::Database { .. }
            | OkfError::Migrate { .. }
            | OkfError::Config { .. }
            | OkfError::Forbidden { .. }
            | OkfError::Refused { .. }
            | OkfError::Unauthenticated { .. }
            | OkfError::Gone { .. }
            | OkfError::Busy { .. }
            | OkfError::StoreUnavailable { .. } => {}
        }
        body
    }
}

/// The text of [`OkfError::Suspended`]: the catalog's label and id, the
/// team's label and id, then the notice as its own sentence.
///
/// # Arguments
/// * `catalog` - the catalog asked for (its id).
/// * `catalog_label` - its label.
/// * `team` - its owner team (its id).
/// * `team_label` - the team's label.
/// * `notice` - the operator's notice, if any.
///
/// # Returns
/// The message, e.g. `catalog "Family notes" (shared) is unavailable: its
/// team "The Family" (family) is suspended. Contact the operator.`
#[tracing::instrument(level = "trace", skip_all)]
fn suspended_message(
    catalog: &str,
    catalog_label: &str,
    team: &str,
    team_label: &str,
    notice: Option<&str>,
) -> String {
    // `{catalog_label:?}` is the `Debug` form: the label in double quotes, escaped.
    let mut message = format!(
        "catalog {catalog_label:?} ({catalog}) is unavailable: its team {team_label:?} \
         ({team}) is suspended"
    );
    if let Some(notice) = notice {
        message.push_str(". ");
        message.push_str(notice);
    }
    message
}

/// The text of [`OkfError::UnlockRequired`]: how to unlock, or, for a
/// credential with no user, why it cannot be.
///
/// # Arguments
/// * `catalog` - the catalog asked for.
/// * `has_user` - whether the credential has a user.
///
/// # Returns
/// The message.
#[tracing::instrument(level = "trace", skip_all)]
fn unlock_message(catalog: &str, has_user: bool) -> String {
    if has_user {
        format!(
            "catalog {catalog:?} needs an unlock for operator access: okf-mcp unlock create, \
             POST /api/v1/unlocks, or Unlock in the console"
        )
    } else {
        format!(
            "catalog {catalog:?} needs an unlock for operator access, and this key has no user, so \
             it cannot be unlocked: give it a user (okf-mcp key assign)"
        )
    }
}

/// A catalog path as clients see it: catalog-relative with a leading `/`
/// (`infra/x.md` becomes `/infra/x.md`; `/infra/x.md` stays). The one rule
/// for every path an error payload names (M8 Decision 9).
///
/// # Arguments
/// * `path` - a catalog-relative path, with or without its leading `/`.
///
/// # Returns
/// The path with exactly one leading `/` added when it had none.
#[tracing::instrument(level = "trace", skip_all)]
fn client_path(path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    }
}

/// Lint findings for an error body, each `file` as [`client_path`] writes it.
///
/// # Arguments
/// * `findings` - the findings, `file` as the lint wrote it (no leading `/`).
///
/// # Returns
/// The JSON list, in the same order.
#[tracing::instrument(level = "trace", skip_all)]
fn findings_json(findings: &[Finding]) -> serde_json::Value {
    let findings: Vec<Finding> = findings
        .iter()
        // `..finding.clone()` is struct update syntax: the other fields are
        // copied from a clone of `finding` (like `dataclasses.replace`).
        .map(|finding| Finding {
            file: client_path(&finding.file),
            ..finding.clone()
        })
        .collect();
    json_list(&findings)
}

/// A list as JSON, for an error body. `Finding`, `ConflictFile` and
/// `LinkSource` are plain records that always serialize; the `Null` fallback
/// only keeps this free of `unwrap`.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
    )
)]
fn json_list<T: serde::Serialize>(items: &[T]) -> serde_json::Value {
    serde_json::to_value(items).unwrap_or_default()
}

/// SQLSTATEs that mean "Postgres aborted this transaction so others could go
/// on; running it again will likely succeed": `40P01` (deadlock detected) and
/// `40001` (serialization failure, from a repeatable-read snapshot).
const RETRYABLE_SQLSTATES: [&str; 2] = ["40P01", "40001"];

// The conversion `?` uses for every driver error: a retryable failure is
// `Busy`, anything else `Database`. (Before M5, `#[from]` generated this
// impl without the check.)
impl From<sqlx::Error> for OkfError {
    #[tracing::instrument(level = "trace", skip_all)]
    fn from(source: sqlx::Error) -> Self {
        // `code()` borrows `source`, so the check finishes before `source`
        // moves into the variant below. It returns `Option<Cow<str>>`, and
        // `as_ref()` views the `Cow` as a `&str`.
        let retryable = source
            .as_database_error()
            .and_then(|db_error| db_error.code())
            .is_some_and(|code| RETRYABLE_SQLSTATES.contains(&code.as_ref()));
        if crate::stores::is_auth_failure(&source) {
            // A refused login on a store's pool (a rotated secret, an expired
            // token) reaches here from every read that propagates a driver
            // error with `?`, not only from `CatalogDb::classify`: wake the
            // credential tasks. The error itself is unchanged.
            crate::stores::signal_auth_refused(&source);
        }
        if retryable {
            // `Busy` drops the driver error, and as a non-internal error it is
            // logged only at debug level, so keep Postgres' report (the
            // statement and the conflicting lock, never document text) here.
            // `?source` logs its `Debug` form, which includes the detail.
            split_event!(
                warn,
                {},
                { error = ?source, },
                "retryable database error, answered Busy"
            );
            OkfError::Busy {
                reason: "the database aborted this request to resolve a lock conflict".to_string(),
            }
        } else {
            OkfError::Database { source }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suspended_names_the_catalog_team_and_notice() {
        let error = OkfError::Suspended {
            catalog: "shared".to_string(),
            catalog_label: "Family notes".to_string(),
            team: "family".to_string(),
            team_label: "The Family".to_string(),
            notice: Some("Contact the operator.".to_string()),
        };

        let body = error.to_json();

        assert_eq!(error.kind(), "Suspended");
        assert!(!error.is_internal());
        assert_eq!(body["error"], "Suspended");
        assert_eq!(body["catalog"], "shared");
        assert_eq!(body["team"], "family");
        assert_eq!(body["notice"], "Contact the operator.");
        assert_eq!(
            body["message"],
            "catalog \"Family notes\" (shared) is unavailable: its team \"The Family\" \
             (family) is suspended. Contact the operator."
        );
    }

    #[test]
    fn suspended_without_a_notice_has_a_null_notice() {
        let error = OkfError::Suspended {
            catalog: "shared".to_string(),
            catalog_label: "shared".to_string(),
            team: "family".to_string(),
            team_label: "family".to_string(),
            notice: None,
        };

        let body = error.to_json();

        // `get`, not `body["notice"]`: indexing a missing key also yields
        // `Null`, so only `get` proves the field is present.
        assert_eq!(body.get("notice"), Some(&serde_json::Value::Null));
        assert_eq!(
            body["message"],
            "catalog \"shared\" (shared) is unavailable: its team \"family\" (family) \
             is suspended"
        );
    }

    #[test]
    fn unlock_required_names_the_catalog_and_says_how() {
        let error = OkfError::UnlockRequired {
            catalog: "main".to_string(),
            has_user: true,
        };
        let user_less = OkfError::UnlockRequired {
            catalog: "main".to_string(),
            has_user: false,
        };

        let body = error.to_json();

        assert_eq!(
            body,
            serde_json::json!({
                "error": "UnlockRequired",
                "message": "catalog \"main\" needs an unlock for operator access: okf-mcp \
                            unlock create, POST /api/v1/unlocks, or Unlock in the console",
                "catalog": "main",
            })
        );
        assert!(!error.is_internal());
        assert!(user_less.to_string().contains("has no user"));
        assert_eq!(user_less.to_json()["catalog"], "main");
    }

    #[test]
    fn maintenance_off_is_a_forbidden_the_runner_can_tell_apart() {
        let error = OkfError::MaintenanceOff {
            catalog: "main".to_string(),
        };

        let body = error.to_json();

        assert_eq!(error.kind(), "Forbidden");
        assert_eq!(body["error"], "Forbidden");
        assert_eq!(body["maintenance"], "off");
        assert_eq!(body["catalog"], "main");
        // A plain `Forbidden` has no marker, so the runner can tell them apart.
        let plain = OkfError::Forbidden {
            reason: "x".to_string(),
        }
        .to_json();
        assert_eq!(plain.get("maintenance"), None);
    }

    #[test]
    fn still_linked_lists_its_links_and_is_a_conflict_not_internal() {
        let error = OkfError::StillLinked {
            path: "/infra/x.md".to_string(),
            links: vec![LinkSource {
                source: "/hub.md".to_string(),
                line: 5,
            }],
        };

        let body = error.to_json();

        assert_eq!(error.kind(), "StillLinked");
        assert!(!error.is_internal());
        assert_eq!(body["path"], "/infra/x.md");
        assert_eq!(
            body["links"],
            serde_json::json!([{"source": "/hub.md", "line": 5}])
        );
        assert_eq!(
            body["message"],
            "/infra/x.md: still linked from 1 place(s); retarget or remove those links \
             first (move_concept retargets them)"
        );
    }

    #[test]
    fn to_json_names_the_kind_and_what_to_act_on() {
        let not_found = OkfError::NotFound {
            path: "/x.md".to_string(),
        };
        let unselected = OkfError::CatalogNotSelected {
            catalog: "work".to_string(),
        };
        let refused = OkfError::ImportRefused {
            problems: vec!["a: bad".to_string()],
        };

        let not_found_body = not_found.to_json();
        let unselected_body = unselected.to_json();
        let refused_body = refused.to_json();

        assert_eq!(
            not_found_body,
            serde_json::json!({
                "error": "NotFound",
                "message": "/x.md: no such concept or document",
                "path": "/x.md",
            })
        );
        assert_eq!(unselected_body["error"], "CatalogNotSelected");
        assert_eq!(unselected_body["catalog"], "work");
        assert_eq!(refused_body["problems"], serde_json::json!(["a: bad"]));
    }

    #[test]
    fn unknown_changeset_names_the_handle() {
        let error = OkfError::UnknownChangeset {
            changeset: "no-such-handle".to_string(),
        };

        let body = error.to_json();

        assert!(!error.is_internal());
        assert_eq!(
            body,
            serde_json::json!({
                "error": "UnknownChangeset",
                "message": "no-such-handle: no such changeset; open one with open_changeset, \
                            or see list_changesets",
                "changeset": "no-such-handle",
            })
        );
    }

    #[test]
    fn secret_detected_names_the_rule_and_line_but_not_the_value() {
        let error = OkfError::SecretDetected {
            path: "notes.md".to_string(),
            line: 6,
            rule: "secret:assignment".to_string(),
            message: "possible secret: 'password' followed by a literal value Ab1A... \
                      (value mixes letters and digits)"
                .to_string(),
        };

        let body = error.to_json();

        assert!(!error.is_internal());
        assert_eq!(body["error"], "SecretDetected");
        assert_eq!(body["path"], "/notes.md");
        assert_eq!(body["line"], 6);
        assert_eq!(body["rule"], "secret:assignment");
        assert_eq!(
            body["message"],
            "notes.md: line 6: write refused, possible secret: 'password' followed by a \
             literal value Ab1A... (value mixes letters and digits) (secret:assignment)"
        );
    }

    #[test]
    fn internal_errors_hide_their_details() {
        let error = OkfError::Database {
            source: sqlx::Error::PoolTimedOut,
        };

        let body = error.to_json();

        assert!(error.is_internal());
        assert_eq!(body["error"], "Database");
        assert!(!body["message"].as_str().unwrap().contains("pool"));
        assert_eq!(body.as_object().unwrap().len(), 2);
    }

    /// Issue #766: only a passing outage is transient; a bug-shaped failure is not.
    #[test]
    fn only_outage_shaped_database_errors_are_transient() {
        let outage = |source| OkfError::Database { source };
        assert!(outage(sqlx::Error::PoolTimedOut).is_transient());
        assert!(outage(sqlx::Error::PoolClosed).is_transient());
        assert!(outage(sqlx::Error::Tls("handshake reset".into())).is_transient());
        assert!(outage(sqlx::Error::WorkerCrashed).is_transient());
        assert!(outage(sqlx::Error::Io(std::io::Error::other("reset"))).is_transient());
        assert!(!outage(sqlx::Error::RowNotFound).is_transient());
        assert!(!outage(sqlx::Error::Protocol("bad".to_string())).is_transient());
        let io = OkfError::Io {
            path: "a.md".to_string(),
            source: std::io::Error::other("disk"),
        };
        assert!(!io.is_transient());
    }

    /// A driver error with a chosen SQLSTATE (or none), for the classifier's code branch.
    #[derive(Debug)]
    struct CodedError(Option<&'static str>);

    impl std::fmt::Display for CodedError {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(formatter, "coded test error")
        }
    }

    impl std::error::Error for CodedError {}

    // `DatabaseError` is the trait sqlx's driver errors implement; only the
    // required methods and `code` are meaningful here.
    impl sqlx::error::DatabaseError for CodedError {
        fn message(&self) -> &str {
            "coded test error"
        }
        fn code(&self) -> Option<std::borrow::Cow<'_, str>> {
            self.0.map(std::borrow::Cow::Borrowed)
        }
        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }
        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::Other
        }
    }

    /// Issue #766: a Postgres connection exception or shutdown is an outage; other codes, and
    /// a database error with no code, are not.
    #[test]
    fn database_errors_are_transient_by_sqlstate() {
        let cases = [
            (Some("08006"), true),
            (Some("57P01"), true),
            (Some("57P02"), true),
            (Some("57P03"), true),
            (Some("57P04"), false),
            (Some("23505"), false),
            (None, false),
        ];
        for (code, expected) in cases {
            let error = OkfError::Database {
                source: sqlx::Error::Database(Box::new(CodedError(code))),
            };

            assert_eq!(error.is_transient(), expected, "{code:?}");
        }
    }

    #[test]
    fn filesystem_errors_are_internal_and_keep_only_their_path() {
        let error = OkfError::Io {
            path: "a.md".to_string(),
            source: std::io::Error::other("disk on fire"),
        };

        let body = error.to_json();

        assert!(error.is_internal());
        assert_eq!(
            body,
            serde_json::json!({
                "error": "Io",
                "message": "internal error; the server log has the details",
                "path": "a.md",
            })
        );
    }

    #[test]
    fn publish_refusals_carry_their_findings_and_files() {
        let finding = |file: &str, rule: &str| Finding {
            file: file.to_string(),
            line: 3,
            rule: rule.to_string(),
            message: "m".to_string(),
        };
        let lint = OkfError::LintFailed {
            errors: vec![finding("a.md", "broken-link")],
            warnings: vec![finding("b.md", "orphan")],
        };
        let conflict = OkfError::Conflict {
            files: vec![ConflictFile {
                path: "/a.md".to_string(),
                base_revision: Some(1),
                current_revision: Some(2),
            }],
        };

        let lint_body = lint.to_json();
        let conflict_body = conflict.to_json();

        assert!(!lint.is_internal() && !conflict.is_internal());
        assert_eq!(lint_body["error"], "LintFailed");
        assert_eq!(lint_body["errors"][0]["rule"], "broken-link");
        assert_eq!(lint_body["warnings"][0]["file"], "/b.md");
        assert_eq!(conflict_body["error"], "Conflict");
        assert_eq!(
            conflict_body["files"],
            serde_json::json!([{"path": "/a.md", "base_revision": 1, "current_revision": 2}])
        );
        assert!(conflict_body["message"]
            .as_str()
            .unwrap()
            .contains("re-apply"));
    }

    #[test]
    fn edit_mismatch_names_the_path_and_the_count() {
        let error = OkfError::EditMismatch {
            path: "a.md".to_string(),
            matches: 2,
        };

        let body = error.to_json();

        assert!(!error.is_internal());
        assert_eq!(
            body,
            serde_json::json!({
                "error": "EditMismatch",
                "message": "a.md: the old text occurs 2 times; it must occur exactly once, \
                            or at least once with replace_all",
                "path": "/a.md",
                "matches": 2,
            })
        );
    }

    #[test]
    fn client_path_adds_one_leading_slash() {
        assert_eq!(client_path("infra/x.md"), "/infra/x.md");
        assert_eq!(client_path("/infra/x.md"), "/infra/x.md");
    }

    #[test]
    fn to_json_writes_catalog_paths_with_a_leading_slash() {
        let not_found = OkfError::NotFound {
            path: "infra/x.md".to_string(),
        };
        let already = OkfError::NotFound {
            path: "/infra/x.md".to_string(),
        };
        let parse = OkfError::FrontmatterParse {
            path: "a.md".to_string(),
            line: 2,
            message: "m".to_string(),
        };
        let shape = OkfError::Shape {
            path: "a.md".to_string(),
            key: "sources".to_string(),
        };
        let mismatch = OkfError::EditMismatch {
            path: "a.md".to_string(),
            matches: 0,
        };

        assert_eq!(not_found.to_json()["path"], "/infra/x.md");
        // Already prefixed: unchanged, never doubled.
        assert_eq!(already.to_json()["path"], "/infra/x.md");
        assert_eq!(parse.to_json()["path"], "/a.md");
        assert_eq!(shape.to_json()["path"], "/a.md");
        assert_eq!(mismatch.to_json()["path"], "/a.md");
        // The message keeps the internal form.
        assert_eq!(
            not_found.to_json()["message"],
            "infra/x.md: no such concept or document"
        );
    }

    #[test]
    fn lint_failed_findings_name_their_file_with_a_leading_slash() {
        let finding = Finding {
            file: "a.md".to_string(),
            line: 3,
            rule: "broken-link".to_string(),
            message: "links to /b.md, which does not exist".to_string(),
        };
        let error = OkfError::LintFailed {
            errors: vec![finding.clone()],
            warnings: vec![finding],
        };

        let body = error.to_json();

        assert_eq!(body["errors"][0]["file"], "/a.md");
        assert_eq!(body["warnings"][0]["file"], "/a.md");
        assert_eq!(body["errors"][0]["line"], 3);
        assert_eq!(body["errors"][0]["rule"], "broken-link");
    }

    #[test]
    fn invalid_path_is_prefixed_unless_it_leaves_the_catalog() {
        let invalid = |path: &str| OkfError::InvalidPath {
            path: path.to_string(),
            reason: "refused".to_string(),
        };

        // A normalized catalog path (machinery, "already exists").
        assert_eq!(invalid("AGENTS.md").to_json()["path"], "/AGENTS.md");
        assert_eq!(invalid("infra/x.md").to_json()["path"], "/infra/x.md");
        // A path that climbs out is not a catalog path: as given.
        assert_eq!(invalid("../x.md").to_json()["path"], "../x.md");
        assert_eq!(invalid("..").to_json()["path"], "..");
        // Not normalized, and climbs out once it is: as given, never `/infra/../../x.md`.
        assert_eq!(
            invalid("infra/../../x.md").to_json()["path"],
            "infra/../../x.md"
        );
        // Empty: as given, not `/`.
        assert_eq!(invalid("").to_json()["path"], "");
    }

    #[test]
    fn sign_in_errors_carry_their_reason_and_a_limit_names_itself() {
        let unauthenticated = OkfError::Unauthenticated {
            reason: "Email or password is incorrect".to_string(),
        };
        let gone = OkfError::Gone {
            reason: "This link has expired. Ask for a new one.".to_string(),
        };
        let limit = OkfError::LimitExceeded {
            limit: "max_keys_per_user",
            reason: "you already have 20 live keys".to_string(),
        };

        let bodies = [unauthenticated.to_json(), gone.to_json(), limit.to_json()];

        assert_eq!(
            bodies,
            [
                serde_json::json!({
                    "error": "Unauthenticated",
                    "message": "Email or password is incorrect",
                }),
                serde_json::json!({
                    "error": "Gone",
                    "message": "This link has expired. Ask for a new one.",
                }),
                serde_json::json!({
                    "error": "LimitExceeded",
                    "message": "you already have 20 live keys",
                    "limit": "max_keys_per_user",
                }),
            ]
        );
        assert!(![&unauthenticated, &gone, &limit]
            .iter()
            .any(|error| error.is_internal()));
    }
}
