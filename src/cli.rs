//! The `okf-mcp` command line: administration directly against the
//! database (design "Process and routes"), and `serve`, the default, which
//! runs the MCP server.
//!
//! Every subcommand that touches the database runs the embedded migrations
//! first (idempotent), so a fresh database needs no separate step.

use std::fs::File;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

// `Context` adds a message to an error (anyhow's `.context(..)`), like
// `raise X from e` with a better message.
use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use sqlx::PgPool;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::app::App;
use crate::archive::{self, Files};
use crate::clock::{format_timestamp, Clock};
use crate::config::Config;
use crate::db;
use crate::http;
use crate::labels::DisplayName;
use crate::shared::announce::announce_from_cli;
use crate::store::audit::AuditActor;
use crate::store::catalogs::{self, CatalogGeneration};
use crate::store::ceiling::MemberAccess;
use crate::store::export;
use crate::store::import::{self, ImportOptions, ImportScope};
use crate::store::keys::{self, Access, Grants, KeyKind, NewKey};
use crate::store::oauth::codes::{self, NewSignInCode, SignInCodeRecord};
use crate::store::repair;
use crate::store::settings::SettingKey;
use crate::store::stores::CredentialKind;
use crate::store::teams::Role;
use crate::store::unlocks::Category;
use crate::stores::{AccountDb, CatalogDb, PoolSettings, StoreName, Stores};
use crate::telemetry::metrics::Metrics;
use crate::telemetry::subscriber::split_event;
use crate::telemetry::{self, StartUp, Telemetry};

// `src/cli/admin.rs`: the `settings` and `mail` handlers.
mod admin;
// `src/cli/audit.rs`: the `audit` handlers.
mod audit;
mod feedback;
// `src/cli/people.rs`: the user, team, assignment and `unassigned` handlers.
mod people;
// `src/cli/stores.rs`: the `stores` handlers.
mod stores;
// `src/cli/unlock.rs`: the `unlock` handlers.
mod unlock;

/// `okf-mcp`: an MCP server for OKF knowledge catalogs.
// `derive(Parser)` generates the argument parser from the struct, as
// `argparse` would from `add_argument` calls; the `#[arg]` attributes are
// the options.
#[derive(Debug, Parser)]
#[command(name = "okf-mcp", version, about)]
pub struct Cli {
    /// The Postgres URL.
    // `global = true` accepts the flag after a subcommand too;
    // `hide_env_values` keeps the URL (and its password) out of `--help`.
    #[arg(long, env = "DATABASE_URL", global = true, hide_env_values = true)]
    pub database_url: Option<String>,
    /// The TOML configuration file.
    #[arg(long, env = "OKF_MCP_CONFIG", global = true)]
    pub config: Option<PathBuf>,
    /// What to do (`serve` when omitted).
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// The subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Serve MCP over HTTP.
    Serve(ServeArgs),
    /// Apply the schema migrations and exit.
    Migrate,
    /// Create, update, list or delete catalogs.
    // A nested subcommand: `okf-mcp catalog create ...`.
    #[command(subcommand)]
    Catalog(CatalogCommand),
    /// Create, list, revoke, grant, ungrant or configure reporting on API keys.
    #[command(subcommand)]
    Key(KeyCommand),
    /// Mint, list and revoke OAuth sign-in codes; list and revoke OAuth grants.
    // clap names a subcommand after its variant in kebab case: `Oauth` is
    // `oauth` (an `OAuth` variant would be `o-auth`).
    #[command(subcommand)]
    Oauth(OauthCommand),
    /// Create, list or delete users (each with a personal team).
    #[command(subcommand)]
    User(UserCommand),
    /// Create, list or delete teams, and manage their members.
    #[command(subcommand)]
    Team(TeamCommand),
    /// List, set or reset the policy settings (password sign-in, key
    /// limits, session and link lifetimes).
    #[command(subcommand)]
    Settings(SettingsCommand),
    /// List the catalog stores, add one, choose the default, and pin a team
    /// to one.
    #[command(subcommand)]
    Stores(StoresCommand),
    /// Send a test email, and list, retry or delete queued mail.
    #[command(subcommand)]
    Mail(MailCommand),
    /// List failed feedback, and send one failed row again.
    #[command(subcommand)]
    Feedback(FeedbackCommand),
    /// Unlock a customer catalog for an admin user, list unlocks, or end
    /// one early.
    #[command(subcommand)]
    Unlock(UnlockCommand),
    /// Read the audit log: administrative changes, catalog writes, exports
    /// and operators' reads.
    #[command(subcommand)]
    Audit(AuditCommand),
    /// List keys, grants, sign-in codes and catalogs with no user or owner.
    Unassigned,
    /// Import a directory or a .tar.gz into an existing catalog.
    Import {
        /// The catalog.
        catalog: String,
        /// A directory, a .tar.gz file, or `-` for a .tar.gz on stdin.
        source: PathBuf,
        /// Replace a non-empty catalog's contents (write the differences); a
        /// new catalog, at generation 0, needs no `--replace`.
        #[arg(long)]
        replace: bool,
        /// Replace only repository machinery (AGENTS.md, tools/, install/,
        /// ...): the source's other files are ignored and the catalog's
        /// concepts are left as they are.
        #[arg(long, requires = "replace")]
        only_machinery: bool,
        /// Refuse unless the catalog is still at this generation, the one
        /// `okf-mcp export` reported for the source.
        #[arg(long, value_name = "N")]
        expect_generation: Option<i64>,
        /// Recorded as the revisions' actor.
        #[arg(long, default_value = "okf-mcp/import")]
        actor: String,
    },
    /// Export a catalog's live documents as a .tar.gz, and report its
    /// generation on stderr.
    Export {
        /// The catalog.
        catalog: String,
        /// The file to write, or `-` for stdout.
        output: PathBuf,
    },
}

/// `okf-mcp serve`'s options.
// `Default`: no flags, which is what `okf-mcp` alone means.
#[derive(Debug, Default, Args)]
pub struct ServeArgs {
    /// The address to listen on, overriding the config's `listen`.
    #[arg(long, env = "OKF_MCP_LISTEN")]
    pub listen: Option<std::net::SocketAddr>,
}

/// `okf-mcp catalog ...`.
#[derive(Debug, Subcommand)]
pub enum CatalogCommand {
    /// Create a catalog holding only a root index.md and a log.md.
    Create {
        /// Its name (`[a-z0-9-]+`).
        name: String,
        /// Its display name, shown instead of the name: at most 80
        /// characters, no control characters or line breaks.
        #[arg(long, default_value = "")]
        display_name: String,
        /// Free text.
        #[arg(long, default_value = "")]
        description: String,
        /// The team that owns it (a user's name names that user's personal
        /// team).
        #[arg(long)]
        owner: String,
    },
    /// Change an existing catalog's display name and/or description. A
    /// running server shows the new text at once (its change feed).
    // At least one flag is required; both may be given.
    #[command(group(
        clap::ArgGroup::new("details")
            .required(true)
            .multiple(true)
            .args(["display_name", "description"])
    ))]
    Update {
        /// Its name.
        name: String,
        /// The new display name (may be empty, which clears it): at most 80
        /// characters, no control characters or line breaks.
        #[arg(long)]
        display_name: Option<String>,
        /// The new free text (may be empty).
        #[arg(long)]
        description: Option<String>,
    },
    /// List catalogs.
    List,
    /// Make a team the owner of a catalog.
    SetOwner {
        /// The catalog.
        name: String,
        /// The team that owns it from now on (a user's name names that
        /// user's personal team).
        #[arg(long)]
        team: String,
    },
    /// Set what the owning team's members may reach in a catalog through
    /// their self-issued keys and grants.
    SetMemberAccess {
        /// The catalog.
        name: String,
        /// `none`, `read` or `write`.
        #[arg(long, value_enum)]
        access: MemberAccess,
    },
    /// Create the root index.md, log.md, folder indexes and listings that
    /// catalogs are missing, and publish them at once (each repaired
    /// catalog's log.md records it). Existing text is kept, except that an
    /// old raw (unencoded) concept or folder listing is percent-encoded in
    /// place, or removed beside a working one; a second run changes
    /// nothing. A running server shows the result at once (its change feed).
    RepairIndexes {
        /// The catalogs to repair. With `--missing-only`, only those of them
        /// without a root index.md; with none named, every such catalog.
        names: Vec<String>,
        /// Repair only catalogs that have no root index.md.
        #[arg(long)]
        missing_only: bool,
        /// Report what each repair would write, and write nothing.
        #[arg(long)]
        dry_run: bool,
        /// Recorded as the revisions' actor.
        #[arg(long, default_value = "okf-mcp/repair-indexes")]
        actor: String,
    },
    /// Let the maintenance runner check a catalog (`on`, the default) or
    /// keep it out (`off`), whatever its key's grants.
    SetMaintenance {
        /// The catalog.
        name: String,
        /// `on` or `off`.
        #[arg(value_enum)]
        state: catalogs::Switch,
    },
    /// Delete a catalog with its documents, history, pending changes and grants.
    Delete {
        /// Its name.
        name: String,
        /// Delete even if a session has unpublished changes.
        #[arg(long)]
        force: bool,
    },
}

/// `okf-mcp key ...`.
#[derive(Debug, Subcommand)]
pub enum KeyCommand {
    /// Create a key and print it once.
    Create(KeyCreateArgs),
    /// List keys (never the keys themselves).
    List,
    /// Set a key's independent maintenance-report capability.
    MaintenanceReport {
        /// The key's immutable id, or an unambiguous active name.
        id: String,
        /// Enable or disable maintenance reporting.
        #[arg(long, required = true, action = clap::ArgAction::Set)]
        enabled: bool,
        /// Select this actor when `id` is a name.
        #[arg(long)]
        actor: Option<String>,
    },
    /// Revoke a key (an OAuth grant is revoked with `oauth grant revoke`).
    Revoke {
        /// The key's id, or an unambiguous active name.
        id: String,
        /// Select this actor when `id` is a name.
        #[arg(long)]
        actor: Option<String>,
    },
    /// Grant a key a catalog.
    Grant {
        /// The key's id, or an unambiguous active name.
        id: String,
        /// Select this actor when `id` is a name.
        #[arg(long)]
        actor: Option<String>,
        /// The catalog.
        catalog: String,
    },
    /// Remove a key's grant of a catalog (and clear it as the key's default).
    Ungrant {
        /// The key's id, or an unambiguous active name.
        id: String,
        /// Select this actor when `id` is a name.
        #[arg(long)]
        actor: Option<String>,
        /// The catalog.
        catalog: String,
    },
    /// Give a key or OAuth grant a user (its access is unchanged).
    Assign {
        /// The key's or grant's id, or an unambiguous active name.
        id: String,
        /// Select this actor when `id` is a name.
        #[arg(long)]
        actor: Option<String>,
        /// The user it belongs to (must exist).
        #[arg(long)]
        user: String,
    },
}

/// `okf-mcp user ...`.
#[derive(Debug, Subcommand)]
pub enum UserCommand {
    /// Create a user and its personal team of the same name.
    Create {
        /// Its name (`[a-z0-9-]+`); also its personal team's.
        name: String,
        /// Its display name, shown instead of the name: at most 80
        /// characters, no control characters or line breaks.
        #[arg(long, default_value = "")]
        display_name: String,
    },
    /// Change a user's display name (empty clears it). Its personal team
    /// follows.
    Update {
        /// Its name.
        name: String,
        /// The new display name: at most 80 characters, no control
        /// characters or line breaks.
        #[arg(long)]
        display_name: String,
    },
    /// List users with their credential counts, emails, admin flags and
    /// teams.
    List {
        /// Only the user with this email address (any letter case); prints
        /// nothing when no user has it.
        #[arg(long, value_name = "ADDRESS")]
        email: Option<String>,
    },
    /// Delete a user, with its keys, grants, sign-in codes, sessions and
    /// personal team; catalogs and history stay. Refused while its personal
    /// team owns a catalog or it is the last owner of a shared team. The
    /// CLI may delete the last admin user.
    Delete {
        /// Its name.
        name: String,
    },
    /// Turn a user's admin flag on or off. The CLI may clear the last admin.
    SetAdmin {
        /// The user.
        name: String,
        /// Make the user an admin.
        // Exactly one of `--on` and `--off`: clap refuses both or neither.
        #[arg(long, conflicts_with = "off", required_unless_present = "off")]
        on: bool,
        /// Make the user a non-admin.
        #[arg(long)]
        off: bool,
    },
    /// Set or clear a user's email (unique, case-insensitive).
    SetEmail {
        /// The user.
        name: String,
        /// The address.
        #[arg(conflicts_with = "clear", required_unless_present = "clear")]
        email: Option<String>,
        /// Remove the email instead.
        #[arg(long)]
        clear: bool,
    },
    /// Email a user a single-use setup link (a running server sends it).
    Invite {
        /// The user; it must have an email.
        name: String,
    },
    /// Let a user reach a catalog beyond its teams' catalogs.
    Entitle {
        /// The user.
        name: String,
        /// The catalog.
        catalog: String,
        /// `read` or `write`.
        #[arg(long, value_enum)]
        access: Access,
    },
    /// Remove a user's entitlement to a catalog.
    Unentitle {
        /// The user.
        name: String,
        /// The catalog.
        catalog: String,
    },
    /// List a user's console sessions (by id prefix, never the id).
    Sessions {
        /// The user.
        name: String,
    },
    /// Sign a user out of every console session.
    RevokeSessions {
        /// The user.
        name: String,
    },
    /// List a user's recent sign-in attempts, newest first.
    SignIns {
        /// The user.
        name: String,
        /// How many to show.
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
}

/// `okf-mcp team ...`.
#[derive(Debug, Subcommand)]
pub enum TeamCommand {
    /// Create a team with its first owner.
    Create {
        /// Its name (`[a-z0-9-]+`, not taken by a user or team).
        name: String,
        /// Its display name, shown instead of the name: at most 80
        /// characters, no control characters or line breaks.
        #[arg(long, default_value = "")]
        display_name: String,
        /// The user who owns it first.
        #[arg(long)]
        owner: String,
    },
    /// Change a shared team's display name (empty clears it). A personal
    /// team's follows its user: use `user update`.
    Update {
        /// Its name.
        name: String,
        /// The new display name: at most 80 characters, no control
        /// characters or line breaks.
        #[arg(long)]
        display_name: String,
    },
    /// List teams with their members and the catalogs they own.
    List,
    /// Add a user to a team (or change its role if it is a member).
    AddMember {
        /// The team.
        team: String,
        /// The user.
        user: String,
        /// Its role.
        // `value_enum`: clap accepts the variants' kebab-case names,
        // `owner` and `member`.
        #[arg(long, value_enum, default_value = "member")]
        role: Role,
    },
    /// Change a member's role (adding the user if it is not a member).
    SetRole {
        /// The team.
        team: String,
        /// The member.
        user: String,
        /// The new role.
        #[arg(long, value_enum)]
        role: Role,
    },
    /// Remove a member (never a team's last owner, nor a user from its
    /// personal team).
    RemoveMember {
        /// The team.
        team: String,
        /// The member.
        user: String,
    },
    /// Delete a team that owns no catalog (never a personal team).
    Delete {
        /// Its name.
        name: String,
    },
    /// Suspend a team: its catalogs stop answering non-admin credentials
    /// (export still works) until `team unsuspend`. Repeating it replaces
    /// the notice and keeps the first suspension time.
    Suspend {
        /// The team.
        name: String,
        /// What its members are told (at most 2000 characters; none when
        /// left out or blank).
        #[arg(long)]
        notice: Option<String>,
    },
    /// Lift a team's suspension (no change when it is not suspended).
    Unsuspend {
        /// The team.
        name: String,
    },
}

/// `okf-mcp settings ...`.
#[derive(Debug, Subcommand)]
pub enum SettingsCommand {
    /// List every setting with its value and where it comes from.
    List,
    /// Override a setting (validated; a running server applies it at once,
    /// through its change feed).
    Set {
        /// The setting, by its snake_case name.
        // `SettingKey` spells its clap values as stored (`max_keys_per_user`),
        // and an unknown one is refused with the list of possible values.
        #[arg(value_enum)]
        key: SettingKey,
        /// Its value (`true`, `20`, `12h`, `7d`, ...).
        value: String,
    },
    /// Remove an override, back to the config file's default.
    Reset {
        /// The setting, by its snake_case name.
        // `SettingKey` spells its clap values as stored (`max_keys_per_user`),
        // and an unknown one is refused with the list of possible values.
        #[arg(value_enum)]
        key: SettingKey,
    },
}

/// `okf-mcp stores ...`: where new catalogs are created (catalog stores
/// design, "Placement and the admin surface"). A catalog's store is chosen
/// when it is created and never changes: none of these moves a catalog.
#[derive(Debug, Subcommand)]
pub enum StoresCommand {
    /// List every store, marking the default.
    List,
    /// Add a store in a database of its own, after checking that it can be
    /// reached. Separate stores are refused until they are enabled.
    Add {
        /// Its name (`[a-z0-9-]+`, at most 63 characters).
        #[arg(value_parser = parse_store_name)]
        name: StoreName,
        /// The database server's host.
        #[arg(long)]
        host: String,
        /// Its port.
        #[arg(long, default_value_t = 5432)]
        port: u16,
        /// The database's name.
        #[arg(long)]
        database: String,
        /// How the password is found: `env`, `secrets-manager` or `iam`.
        #[arg(long, value_parser = parse_credential_kind)]
        credential_kind: CredentialKind,
        /// What names it: a variable, a secret ARN or an IAM identity.
        /// Never the password itself.
        #[arg(long)]
        credential_ref: String,
    },
    /// Make a store the one new catalogs go on, unless their team is pinned.
    SetDefault {
        /// The store.
        #[arg(value_parser = parse_store_name)]
        store: StoreName,
    },
    /// Pin a team to a store: catalogs it owns are created there from now on.
    Pin {
        /// The team.
        team: String,
        /// The store.
        #[arg(value_parser = parse_store_name)]
        store: StoreName,
    },
    /// Remove a team's pin, so the default store applies again.
    Unpin {
        /// The team.
        team: String,
    },
    /// Migrate every store's catalog schema and report each one. A store
    /// that cannot be reached or migrated is reported and the others still
    /// run; the command then exits with an error.
    Migrate,
}

/// Reads a store name argument.
///
/// # Errors
/// The rule the name breaks.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(uninstrumented_fn, reason = "a pure parse of an argument, no I/O")
)]
fn parse_store_name(text: &str) -> Result<StoreName, String> {
    StoreName::parse(text).map_err(|error| error.to_string())
}

/// Reads a `--credential-kind` value as the `stores` table spells it.
///
/// # Errors
/// The accepted spellings, for a value that is none of them.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(uninstrumented_fn, reason = "a pure parse of an argument, no I/O")
)]
fn parse_credential_kind(text: &str) -> Result<CredentialKind, String> {
    CredentialKind::parse(text)
        .ok_or_else(|| format!("{text:?} is not one of env, secrets-manager, iam"))
}

/// `okf-mcp unlock ...`: operator access to a customer catalog's contents
/// on a server with `require_unlock` on.
#[derive(Debug, Subcommand)]
pub enum UnlockCommand {
    /// Unlock a catalog for an admin user, for a reason, for a while.
    Create {
        /// The catalog.
        #[arg(long)]
        catalog: String,
        /// The admin user it is for.
        #[arg(long)]
        user: String,
        /// Why, as a category its owners see.
        // `Category` spells its clap values as stored (`deletion_request`).
        #[arg(long, value_enum)]
        category: Category,
        /// Why, in words (a ticket number); owners never see it.
        #[arg(long)]
        reason: String,
        /// How long, in hours (default and maximum: `unlock_max_hours`).
        #[arg(long)]
        hours: Option<u32>,
    },
    /// List unlocks, newest first.
    List {
        /// Only this catalog's.
        #[arg(long)]
        catalog: Option<String>,
        /// Only live ones.
        #[arg(long)]
        live: bool,
    },
    /// End an unlock before it expires.
    End {
        /// Its id.
        id: i64,
    },
}

/// `okf-mcp mail ...`.
#[derive(Debug, Subcommand)]
pub enum MailCommand {
    /// Queue the test email to an address (a running server sends it).
    Test {
        /// The recipient.
        address: String,
    },
    /// List pending and failed mail (never the bodies).
    Queue,
    /// Send a failed mail again, with a new 24-hour retry window.
    Retry {
        /// Its id, from `mail queue`.
        id: String,
    },
    /// Delete a queued mail.
    Delete {
        /// Its id, from `mail queue`.
        id: String,
    },
}

/// `okf-mcp feedback ...`: the operator's path for feedback the worker gave
/// up on (`failed`).
#[derive(Debug, Subcommand)]
pub enum FeedbackCommand {
    /// List failed feedback, oldest first (never the messages).
    List,
    /// Send a failed row again: it goes back to pending, with no tries.
    Retry {
        /// Its id, from `feedback list`.
        id: String,
    },
}

/// `okf-mcp audit ...`. The CLI has operator access: it reads every entry,
/// unredacted, as an every-catalog admin does over REST.
#[derive(Debug, Subcommand)]
pub enum AuditCommand {
    /// Entries, newest first, one tab-separated line each: cursor, time,
    /// action, actor, target, teams, details.
    List(AuditArgs),
    /// Every matching entry as JSON Lines on stdout, newest first (as
    /// `GET /api/v1/audit/export`; `--limit` is ignored).
    Export(AuditArgs),
}

/// The audit filters, as `GET /api/v1/audit` takes them. An empty value is
/// no filter.
// `Clone` and `Default` let a test build one filter set and run it through
// both subcommands.
#[derive(Debug, Clone, Default, PartialEq, Eq, Args)]
pub struct AuditArgs {
    /// Only entries of this team (every team that had the name; a personal
    /// team by its user's name).
    #[arg(long)]
    pub team: Option<String>,
    /// Only this action (`user.created`, ...).
    #[arg(long)]
    pub action: Option<String>,
    /// Only this actor: a person, a credential id or an actor label.
    #[arg(long)]
    pub actor: Option<String>,
    /// Only this target: a name, a person or a credential id.
    #[arg(long)]
    pub target: Option<String>,
    /// From this RFC 3339 time on.
    #[arg(long)]
    pub since: Option<String>,
    /// Before this RFC 3339 time.
    #[arg(long)]
    pub until: Option<String>,
    /// Only entries older than this cursor: pass a full page's last cursor
    /// (the first column of `list`) to read the next page.
    #[arg(long, value_name = "CURSOR")]
    pub after: Option<String>,
    /// At most this many (`list` only; default 100, at most 500).
    #[arg(long, value_name = "N")]
    pub limit: Option<i64>,
}

/// `okf-mcp key create`'s options.
// `derive(Args)` makes this struct a group of options that a subcommand
// variant (`Create(KeyCreateArgs)`) can hold.
#[derive(Debug, Args)]
pub struct KeyCreateArgs {
    /// Grouping name (`[A-Za-z0-9._-]+`).
    pub name: String,
    /// Access level (required unless creating a provisioner).
    #[arg(long, value_enum, required_unless_present = "provisioner")]
    pub access: Option<Access>,
    /// Manage non-admin accounts, teams and catalogs, without catalog access.
    #[arg(long, conflicts_with_all = ["access", "catalogs", "all", "default", "maintenance_report"])]
    pub provisioner: bool,
    /// Permit maintenance reporting (does not grant write access).
    #[arg(long)]
    pub maintenance_report: bool,
    /// Granted catalogs, comma-separated.
    // Ordinary keys need catalogs or all; provisioners reach neither.
    #[arg(
        long,
        value_delimiter = ',',
        required_unless_present_any = ["all", "provisioner"],
        conflicts_with = "all"
    )]
    pub catalogs: Vec<String>,
    /// Grant every catalog, present and future.
    #[arg(long)]
    pub all: bool,
    /// The key's default catalog (must be granted).
    #[arg(long)]
    pub default: Option<String>,
    /// The actor its writes are recorded as (default: the name).
    #[arg(long)]
    pub actor: Option<String>,
    /// The user it belongs to (must exist).
    #[arg(long)]
    pub user: String,
}

/// `okf-mcp oauth ...`.
#[derive(Debug, Subcommand)]
pub enum OauthCommand {
    /// Sign-in codes: one-time codes a person enters on the consent page.
    #[command(subcommand)]
    Code(CodeCommand),
    /// OAuth grants: the keys sign-in codes became.
    #[command(subcommand)]
    Grant(GrantCommand),
}

/// `okf-mcp oauth code ...`.
#[derive(Debug, Subcommand)]
pub enum CodeCommand {
    /// Mint a code and print it once.
    Create(CodeCreateArgs),
    /// List codes in every state (never the codes themselves).
    List,
    /// Revoke a code nobody has redeemed.
    Revoke {
        /// The code's id (from `oauth code list`).
        id: String,
    },
    /// Give an unassigned sign-in code a user (the grant it became is
    /// assigned with `key assign`).
    Assign {
        /// The code's id (from `oauth code list`).
        id: String,
        /// The user it belongs to (must exist).
        #[arg(long)]
        user: String,
    },
}

/// `okf-mcp oauth grant ...`.
#[derive(Debug, Subcommand)]
pub enum GrantCommand {
    /// List OAuth grants, revoked ones included.
    List,
    /// Revoke an OAuth grant: its tokens stop working at once (the change
    /// feed), or within `credential_ttl` should the message be lost.
    Revoke {
        /// The grant's id (from `oauth grant list`).
        id: String,
    },
}

/// `okf-mcp oauth code create`'s options.
#[derive(Debug, Args)]
pub struct CodeCreateArgs {
    /// A label (`[A-Za-z0-9._-]+`); the grant's name starts with it.
    pub name: String,
    /// The actor the grant's writes are recorded as (default: the name).
    #[arg(long)]
    pub actor: Option<String>,
    /// Access level: `read` or `write` (`admin` is refused).
    #[arg(long, value_enum)]
    pub access: Access,
    /// Granted catalogs, comma-separated.
    #[arg(
        long,
        value_delimiter = ',',
        required_unless_present = "all",
        conflicts_with = "all"
    )]
    pub catalogs: Vec<String>,
    /// Grant every catalog, present and future.
    #[arg(long)]
    pub all: bool,
    /// The grant's default catalog (must be granted).
    #[arg(long)]
    pub default: Option<String>,
    /// How long the code stays valid unredeemed (`24h`, `3d`; at most
    /// `oauth.code_max_expiry`).
    // `value_parser = fn` runs the function on the text; its `Err(String)`
    // becomes clap's usage error.
    #[arg(long, default_value = "24h", value_parser = parse_expiry)]
    pub expires: Duration,
    /// The user the code, and the grant it becomes, belongs to (must exist).
    #[arg(long)]
    pub user: String,
}

/// Parses `--expires` (`humantime` syntax: `90m`, `24h`, `3d`).
///
/// # Errors
/// `humantime`'s message, for clap to print.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
    )
)]
fn parse_expiry(text: &str) -> Result<Duration, String> {
    humantime_serde::re::humantime::parse_duration(text).map_err(|error| error.to_string())
}

/// Runs `serve`: connects, loads the catalogs and serves until a signal.
///
/// # Arguments
/// * `database_url` - `--database-url` / `DATABASE_URL`.
/// * `config` - the loaded configuration (`--listen` overrides its address).
/// * `args` - the `serve` flags.
/// * `clock` - the source of every timestamp written.
/// * `registry` - what `/metrics` renders, from `serve`'s running telemetry
///   (`None` in unit tests, which start none).
///
/// # Errors
/// Any failure to connect, load, bind or serve, with context; a `--listen`
/// that overlaps `[telemetry.prometheus] listen`.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "long-lived loop: its span would never close and would stay open as the parent of every later span"
    )
)]
async fn serve(
    database_url: Option<&str>,
    mut config: Config,
    args: ServeArgs,
    clock: Arc<dyn Clock>,
    registry: Option<prometheus::Registry>,
) -> anyhow::Result<()> {
    if let Some(listen) = args.listen {
        config.listen = listen;
        // The file's own check saw the file's address, not this one.
        config
            .validate_metrics_listen()
            .map_err(anyhow::Error::msg)?;
    }
    let pool = connect(database_url, &config).await?;
    let listen = config.listen;
    let metrics_listen = config.telemetry.prometheus.as_ref().map(|prom| prom.listen);
    let app = App::load(pool, config, clock)
        .await
        .context("load the catalogs")?;
    let listener = TcpListener::bind(listen)
        .await
        .with_context(|| format!("listen on {listen}"))?;
    // Bound before serving, so a taken port stops start-up instead of
    // leaving a server with no `/metrics`.
    let scrape_listener = match (metrics_listen, registry) {
        (Some(address), Some(registry)) => Some((
            TcpListener::bind(address)
                .await
                .with_context(|| format!("listen for /metrics on {address}"))?,
            registry,
        )),
        _ => None,
    };
    tracing::info!(%listen, "serving");
    let shutdown = CancellationToken::new();
    // A background task; it cancels `shutdown` on the first signal.
    tokio::spawn(cancel_on_signal(shutdown.clone()));
    let scraping = scrape_listener.map(|(scrape_listener, registry)| {
        tokio::spawn(telemetry::scrape::serve(
            scrape_listener,
            registry,
            shutdown.clone(),
        ))
    });
    let served = http::serve(app, listener, shutdown.clone())
        .await
        .context("serve");
    // However `http::serve` ended, stop `/metrics` and wait for it: the
    // caller flushes telemetry next, and the listener must be gone by then.
    shutdown.cancel();
    if let Some(scraping) = scraping {
        match scraping.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => split_event!(error, {}, { %error, }, "the /metrics listener failed"),
            Err(error) => {
                split_event!(error, {}, { %error, }, "the /metrics listener task failed");
            }
        }
    }
    served
}

/// [`run`] without the subscriber install, which a process may do once: the
/// unit tests run many commands in one process. Loads the configuration and
/// dispatches the command.
///
/// # Errors
/// As [`run`].
#[cfg(test)]
pub(crate) async fn run_in_tests(cli: Cli, clock: Arc<dyn Clock>) -> anyhow::Result<()> {
    let config = Config::load(cli.config.as_deref())?;
    dispatch(cli, config, None, clock, &|name| std::env::var(name).ok()).await
}

/// Runs one command line.
///
/// # Arguments
/// * `cli` - the parsed command line.
/// * `clock` - the source of every timestamp written (`Arc<dyn Clock>`: any
///   clock, real or fixed, behind a shared handle the server can keep).
///
/// # Errors
/// Any failure, with context, for `main` to print.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "runs before the subscriber exists: it installs the subscriber"
    )
)]
pub async fn run(cli: Cli, clock: Arc<dyn Clock>) -> anyhow::Result<()> {
    // `serve` is also what no subcommand means.
    let serving = matches!(cli.command, None | Some(Command::Serve(_)));
    let env = |name: &str| std::env::var(name).ok();
    // Reads the file first so a bad one fails every command, not only
    // `import`; starts telemetry for `serve`; and drops its temporary
    // start-up subscriber before the real one is installed below.
    let StartUp { config, telemetry } =
        telemetry::start_up(cli.config.as_deref(), serving, &env, std::io::stderr)?;
    telemetry::install(
        &config.telemetry,
        telemetry.as_ref(),
        env("RUST_LOG").as_deref(),
    )?;
    dispatch(cli, config, telemetry, clock, &env).await
}

/// Runs the parsed command with the configuration loaded and the subscriber
/// installed. Split from [`run`] so the unit tests, which run many commands
/// in one process, skip the once-per-process subscriber install.
///
/// # Arguments
/// * `cli` - the parsed command line.
/// * `config` - the loaded configuration.
/// * `telemetry` - `serve`'s running telemetry (`None` for other commands).
/// * `clock` - the source of every timestamp written.
/// * `env` - reads an environment variable (the `[redis]` URL, for the
///   announcement every command but `serve` makes when it succeeds).
///
/// # Errors
/// Any failure, with context, for `main` to print.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "wraps the serve loop: its span would never close"
    )
)]
async fn dispatch(
    cli: Cli,
    config: Config,
    telemetry: Option<Telemetry>,
    clock: Arc<dyn Clock>,
    env: &dyn Fn(&str) -> Option<String>,
) -> anyhow::Result<()> {
    // The catalog generations the command committed (an import or a
    // repair), announced with the rest after it succeeds (issue #659).
    let mut changed: Vec<CatalogGeneration> = Vec::new();
    // `unwrap_or_else` builds the default only when no subcommand was given.
    match cli
        .command
        .unwrap_or_else(|| Command::Serve(ServeArgs::default()))
    {
        Command::Serve(args) => {
            let registry = telemetry.as_ref().map(Telemetry::registry);
            let served = serve(cli.database_url.as_deref(), config, args, clock, registry).await;
            // The closed message only: the error's text can carry anything,
            // and `main` prints the full chain to stderr.
            if served.is_err() {
                tracing::error!("serve failed");
            }
            // Flush last, so the line above is exported too.
            if let Some(telemetry) = telemetry {
                telemetry.shutdown().await;
            }
            served?;
            // `serve` is the one command that does not announce: the serving
            // instances are what listen.
            return Ok(());
        }
        Command::Migrate => {
            connect(cli.database_url.as_deref(), &config).await?;
            println!("migrations applied");
        }
        Command::Catalog(command) => {
            let pool = connect(cli.database_url.as_deref(), &config).await?;
            let stores = Stores::new(pool.clone(), PoolSettings::from_config(&config));
            let ran = run_catalog(&pool, &stores, command, clock.as_ref(), &mut changed).await;
            if ran.is_err() {
                // What it committed before failing is announced all the same.
                announce_from_cli(&config, env, &Metrics::global(), &changed).await;
            }
            ran?;
        }
        Command::Key(command) => {
            let pool = connect(cli.database_url.as_deref(), &config).await?;
            run_key(&pool, command, clock.as_ref()).await?;
        }
        Command::Oauth(command) => {
            let pool = connect(cli.database_url.as_deref(), &config).await?;
            run_oauth(&pool, command, &config, clock.as_ref()).await?;
        }
        Command::User(command) => {
            let pool = connect(cli.database_url.as_deref(), &config).await?;
            print!(
                "{}",
                people::user(&pool, &config, command, clock.now()).await?
            );
        }
        Command::Team(command) => {
            let pool = connect(cli.database_url.as_deref(), &config).await?;
            print!("{}", people::team(&pool, command, clock.now()).await?);
        }
        Command::Settings(command) => {
            let pool = connect(cli.database_url.as_deref(), &config).await?;
            print!(
                "{}",
                admin::settings(&pool, &config.auth, command, clock.now()).await?
            );
        }
        Command::Stores(command) => {
            let pool = connect(cli.database_url.as_deref(), &config).await?;
            let stores = Stores::new(pool.clone(), PoolSettings::from_config(&config));
            print!(
                "{}",
                stores::stores(&pool, &stores, command, clock.now()).await?
            );
        }
        Command::Mail(command) => {
            // Only queues and lists: no mailer is built, so no mail secret
            // is needed here.
            let pool = connect(cli.database_url.as_deref(), &config).await?;
            print!(
                "{}",
                admin::mail(&pool, &config.auth, command, clock.now()).await?
            );
        }
        Command::Feedback(command) => {
            // Only reads and requeues: a running server's worker sends.
            let pool = connect(cli.database_url.as_deref(), &config).await?;
            print!("{}", feedback::feedback(&pool, command).await?);
        }
        Command::Unlock(command) => {
            let pool = connect(cli.database_url.as_deref(), &config).await?;
            print!(
                "{}",
                unlock::unlock(&pool, &config.auth, command, clock.now()).await?
            );
        }
        Command::Audit(command) => {
            let pool = connect(cli.database_url.as_deref(), &config).await?;
            let stores = Stores::new(pool, PoolSettings::from_config(&config));
            print!("{}", audit::audit(&stores, &command).await?);
        }
        Command::Unassigned => {
            let pool = connect(cli.database_url.as_deref(), &config).await?;
            print!("{}", people::unassigned(&pool).await?);
        }
        Command::Import {
            catalog,
            source,
            replace,
            only_machinery,
            expect_generation,
            actor,
        } => {
            // Connect first: a missing or unreachable database fails before a
            // large tarball (or stdin) is read.
            let pool = connect(cli.database_url.as_deref(), &config).await?;
            let limit = config.max_import_bytes.as_u64();
            // Reading files is blocking I/O: run it on a blocking thread, as
            // `import` does for its snapshot, so the runtime's threads stay free.
            let files = tokio::task::spawn_blocking(move || read_source(&source, limit))
                .await
                .context("the import reader stopped")??;
            let options = ImportOptions {
                replace,
                scope: if only_machinery {
                    ImportScope::Machinery
                } else {
                    ImportScope::Everything
                },
                expect_generation,
                // The command line works on the database directly: no
                // admission, so no incarnation to hold it to.
                expect_catalog_id: None,
                actor: &actor,
                audit: &AuditActor::cli(),
                now: clock.now(),
            };
            let db = catalog_store(&pool, &config, &catalog).await?;
            let report = import::import(&db, &catalog, files, &options).await?;
            changed.extend(report.changed);
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Command::Export { catalog, output } => {
            let pool = connect(cli.database_url.as_deref(), &config).await?;
            // Build the archive in memory first: no `StdoutLock` is held across
            // `.await` (which would make this future impossible to
            // `tokio::spawn`), and an unknown catalog leaves no file behind.
            let db = catalog_store(&pool, &config, &catalog).await?;
            let (tarball, generation) = export::export(
                &db,
                &catalog,
                Vec::new(),
                &export::ExportOptions {
                    audit: &AuditActor::cli(),
                    now: clock.now(),
                    expect_catalog_id: None,
                },
            )
            .await?;
            // Writing is blocking I/O: run it on a blocking thread.
            tokio::task::spawn_blocking(move || write_output(&output, &tarball))
                .await
                .context("the export writer stopped")??;
            // stderr, since the archive itself may be on stdout.
            eprintln!("exported catalog {catalog:?} at generation {generation}");
        }
    }
    // Every command that got here succeeded; what it committed is announced.
    announce_from_cli(&config, env, &Metrics::global(), &changed).await;
    Ok(())
}

/// Cancels `shutdown` on SIGINT (Ctrl-C) or SIGTERM (what Kubernetes sends).
///
/// # Arguments
/// * `shutdown` - the server's token.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "long-lived loop: its span would never close and would stay open as the parent of every later span"
    )
)]
async fn cancel_on_signal(shutdown: CancellationToken) {
    // `ctrl_c()` resolves at once with an error if the handler cannot be
    // registered; waiting forever then keeps that from reading as a signal.
    let interrupt = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            split_event!(
                error,
                {},
                { %error, },
                "cannot listen for SIGINT"
            );
            // Wait forever: SIGTERM still works.
            std::future::pending::<()>().await;
        }
    };
    // `tokio::signal::unix` exists only on Unix, the only platform this
    // server runs on.
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                split_event!(
                    error,
                    {},
                    { %error, },
                    "cannot listen for SIGTERM"
                );
                // Wait forever: Ctrl-C still works.
                std::future::pending::<()>().await;
            }
        }
    };
    // Whichever signal comes first; the other future is dropped.
    tokio::select! {
        () = interrupt => {}
        () = terminate => {}
    }
    tracing::info!("shutting down");
    shutdown.cancel();
}

/// Reads an import's files: a directory, a `.tar.gz` file, or (`-`) a
/// `.tar.gz` on stdin.
///
/// # Arguments
/// * `source` - the path given on the command line.
/// * `limit` - `max_import_bytes`, the cap on the uncompressed total.
///
/// # Errors
/// The archive's refusal, or the file failing to open.
#[tracing::instrument(level = "trace", skip_all)]
fn read_source(source: &Path, limit: u64) -> anyhow::Result<Files> {
    let files = if source.as_os_str() == "-" {
        archive::read_tarball(std::io::stdin().lock(), limit)?
    } else if source.is_dir() {
        archive::read_directory(source, limit)?
    } else {
        let file = File::open(source).with_context(|| format!("open {}", source.display()))?;
        archive::read_tarball(BufReader::new(file), limit)?
    };
    Ok(files)
}

/// Writes an export's archive to a file, or (`-`) to stdout.
///
/// # Arguments
/// * `output` - the path given on the command line.
/// * `tarball` - the finished `.tar.gz`.
///
/// # Errors
/// The file failing to be created or written, or stdout failing.
#[tracing::instrument(level = "trace", skip_all)]
fn write_output(output: &Path, tarball: &[u8]) -> anyhow::Result<()> {
    if output.as_os_str() == "-" {
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(tarball).context("write to stdout")?;
        // Locked stdout is line-buffered: flush the tail before returning.
        stdout.flush().context("write to stdout")?;
    } else {
        write_file_atomically(output, tarball)
            .with_context(|| format!("write {}", output.display()))?;
    }
    Ok(())
}

/// Writes `contents` to `path` through a temp file in the same directory,
/// renamed into place once complete, so a failed write (a full disk, say)
/// never leaves a truncated archive. An existing file is replaced, as
/// `tar -cf` would.
///
/// # Arguments
/// * `path` - the file to write.
/// * `contents` - its bytes.
///
/// # Errors
/// The temp file failing to be created, written, synced or renamed.
#[tracing::instrument(level = "trace", skip_all)]
fn write_file_atomically(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    // `parent()` of a bare file name is `Some("")`: the current directory.
    let parent = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    let mut builder = tempfile::Builder::new();
    builder.prefix(".okf-export-").suffix(".tmp");
    // `tempfile` alone creates 0o600; ask for 0o666 minus the umask, as
    // `File::create` does.
    #[cfg(unix)]
    builder.permissions(crate::okf::workspace::new_file_permissions());
    // The temp file is deleted when `temp` is dropped on any early return
    // (`Drop`, like a `with` block's `__exit__`), so a failure leaks nothing.
    let mut temp = builder.tempfile_in(parent)?;
    temp.write_all(contents)?;
    // Durable before the rename, so a crash cannot leave an empty file.
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|error| error.error)?;
    Ok(())
}

/// The lines `catalog update` prints: one per field the caller gave.
///
/// # Arguments
/// * `name` - the catalog.
/// * `previous` - what the fields held before the change.
/// * `display_name` - the new display name, if given.
/// * `description` - the new description, if given.
///
/// # Returns
/// `updated catalog <name> <field>: <old> -> <new>\n` per field given. `{:?}`
/// quotes the text, so an empty value shows as `""`.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(uninstrumented_fn, reason = "pure formatting")
)]
fn describe_details_change(
    name: &str,
    previous: &catalogs::Details,
    display_name: Option<&DisplayName>,
    description: Option<&str>,
) -> String {
    let mut text = String::new();
    if let Some(new) = display_name {
        text.push_str(&format!(
            "updated catalog {name} display name: {:?} -> {:?}\n",
            previous.display_name,
            new.as_str()
        ));
    }
    if let Some(new) = description {
        text.push_str(&format!(
            "updated catalog {name} description: {:?} -> {new:?}\n",
            previous.description
        ));
    }
    text
}

/// Runs `okf-mcp catalog ...`.
///
/// # Arguments
/// * `pool` - the migrated database.
/// * `command` - which catalog subcommand.
/// * `clock` - the source of `created_at`.
/// * `changed` - gets each catalog generation the command committed
///   (`repair-indexes`), for the announcement (issue #659), even when the
///   command then fails.
///
/// # Errors
/// The store's refusal or database error.
#[tracing::instrument(level = "debug", skip_all)]
async fn run_catalog(
    pool: &PgPool,
    stores: &Stores,
    command: CatalogCommand,
    clock: &dyn Clock,
    changed: &mut Vec<CatalogGeneration>,
) -> anyhow::Result<()> {
    match command {
        CatalogCommand::Create {
            name,
            display_name,
            description,
            owner,
        } => {
            let display_name = DisplayName::parse(&display_name)?;
            let new = catalogs::NewCatalog {
                name: &name,
                display_name: &display_name,
                description: &description,
                owner_team: &owner,
            };
            let catalog =
                catalogs::create_in(stores, &new, &AuditActor::cli(), clock.now()).await?;
            println!("created catalog {}", catalog.name);
        }
        CatalogCommand::Update {
            name,
            display_name,
            description,
        } => {
            let parsed = display_name
                .as_deref()
                .map(DisplayName::parse)
                .transpose()?;
            let change = catalogs::DetailsChange {
                catalog: &name,
                display_name: parsed.as_ref(),
                description: description.as_deref(),
                owner: None,
            };
            let previous =
                catalogs::update_details(pool, &change, &AuditActor::cli(), clock.now()).await?;
            print!(
                "{}",
                describe_details_change(&name, &previous, parsed.as_ref(), description.as_deref())
            );
        }
        CatalogCommand::List => {
            for catalog in catalogs::list_in(stores).await? {
                println!("{}", format_catalog(&catalog));
            }
        }
        CatalogCommand::SetOwner { name, team } => {
            print!(
                "{}",
                people::set_owner(pool, &name, &team, clock.now()).await?
            );
        }
        CatalogCommand::SetMemberAccess { name, access } => {
            print!(
                "{}",
                people::set_member_access(pool, &name, access, clock.now()).await?
            );
        }
        CatalogCommand::SetMaintenance { name, state } => {
            print!(
                "{}",
                people::set_maintenance(pool, &name, state, clock.now()).await?
            );
        }
        CatalogCommand::Delete { name, force } => {
            catalogs::delete_in(stores, &name, force, &AuditActor::cli(), clock.now()).await?;
            println!("deleted catalog {name}");
        }
        CatalogCommand::RepairIndexes {
            names,
            missing_only,
            dry_run,
            actor,
        } => {
            let accounts = AccountDb::new(pool.clone());
            let audit = AuditActor::cli();
            let options = repair::RepairOptions {
                dry_run,
                actor: &actor,
                audit: &audit,
                now: clock.now(),
            };
            // Every store in turn (only those holding a named catalog, when
            // names are given): a catalog is repaired on the store that
            // holds it. A store that fails does not stop the others; the
            // failures are reported together at the end.
            let mut failures = Vec::new();
            for store_name in stores.all().await? {
                let outcome = async {
                    // Named catalogs are placed from the account database, so a
                    // store holding none of them is never opened (it may be down).
                    if !names.is_empty()
                        && repair::placed_on(&accounts, &store_name, &names)
                            .await?
                            .is_empty()
                    {
                        return Ok(String::new());
                    }
                    let store = stores.get(&store_name).await?;
                    let (targets, skipped) =
                        repair_targets(&accounts, &store, names.clone(), missing_only).await?;
                    for name in &skipped {
                        println!("skipped {name}: it has a root index.md, or does not exist");
                    }
                    if targets.is_empty() {
                        return Ok(String::new());
                    }
                    repair_catalogs(&store, &targets, &options, changed).await
                }
                .await;
                match outcome {
                    Ok(report) => print!("{report}"),
                    Err(error) => failures.push(format!("store {store_name}: {error:#}")),
                }
            }
            anyhow::ensure!(failures.is_empty(), "{}", failures.join("\n"));
        }
    }
    Ok(())
}

/// The catalogs `catalog repair-indexes` repairs.
///
/// # Arguments
/// * `accounts` - the account database (the catalogs).
/// * `db` - the store holding their documents; only the catalogs placed on
///   it are considered.
/// * `names` - the catalogs named on the command line (those placed on
///   other stores are left to those stores' turn).
/// * `missing_only` - keep only catalogs without a root `index.md` (every
///   such catalog when none is named).
///
/// # Returns
/// The names to repair, in the order given (sorted for `--missing-only`
/// alone), and the named ones `--missing-only` left out (they have a root
/// `index.md`, or no such catalog exists), so the caller can say so.
///
/// # Errors
/// Neither a name nor `--missing-only`; a database error.
#[tracing::instrument(level = "debug", skip_all)]
async fn repair_targets(
    accounts: &AccountDb,
    db: &CatalogDb,
    names: Vec<String>,
    missing_only: bool,
) -> anyhow::Result<(Vec<String>, Vec<String>)> {
    if !missing_only {
        anyhow::ensure!(
            !names.is_empty(),
            "name the catalogs to repair, or pass --missing-only for every catalog \
             without a root index.md"
        );
        return Ok((
            repair::placed_on(accounts, db.store(), &names).await?,
            Vec::new(),
        ));
    }
    let missing = repair::missing_root_index(accounts, db).await?;
    if names.is_empty() {
        return Ok((missing, Vec::new()));
    }
    // `partition` splits one list in two by a test, like two comprehensions.
    let here = repair::placed_on(accounts, db.store(), &names).await?;
    Ok(here.into_iter().partition(|name| missing.contains(name)))
}

/// Repairs each catalog in turn, carrying on past one that fails, and
/// describes each result.
///
/// # Arguments
/// * `db` - the store holding the catalogs.
/// * `targets` - the catalogs.
/// * `options` - dry run, actor and time.
/// * `changed` - gets the generation of each catalog a repair wrote.
///
/// # Returns
/// The report: per catalog its summary, the files written (or that would
/// be) and the notes; then a total line.
///
/// # Errors
/// When any catalog fails, after every other one was tried: the error holds
/// the whole report, then each failed catalog and why.
#[tracing::instrument(level = "debug", skip_all)]
async fn repair_catalogs(
    db: &CatalogDb,
    targets: &[String],
    options: &repair::RepairOptions<'_>,
    changed: &mut Vec<CatalogGeneration>,
) -> anyhow::Result<String> {
    let mut out = String::new();
    let mut failures = Vec::new();
    let mut repaired = 0_usize;
    let mode = if options.dry_run {
        " (dry run: nothing written)"
    } else {
        ""
    };
    for name in targets {
        match repair::repair(db, name, options).await {
            Ok(report) => {
                if !report.written.is_empty() {
                    repaired += 1;
                }
                // `Vec::extend` with an `Option` adds its one value, if any.
                changed.extend(report.changed);
                out.push_str(&format!("{name}{mode}: {}\n", report.message));
                for path in &report.written {
                    out.push_str(&format!("  {path}\n"));
                }
                for note in &report.notes {
                    out.push_str(&format!("  note: {note}\n"));
                }
            }
            Err(error) => failures.push(format!("{name}: {error}")),
        }
    }
    let verb = if options.dry_run {
        "would repair"
    } else {
        "repaired"
    };
    out.push_str(&format!(
        "{verb} {repaired} of {} catalog(s)\n",
        targets.len()
    ));
    if failures.is_empty() {
        Ok(out)
    } else {
        // The successes are part of the error, so nothing done is unreported.
        anyhow::bail!("{out}failed:\n  {}", failures.join("\n  "))
    }
}

/// Runs `okf-mcp key ...`. `create` prints the key alone on stdout (so
/// `$(okf-mcp key create ...)` captures it) and a reminder on stderr; nothing
/// else ever prints or logs a key.
///
/// # Arguments
/// * `pool` - the migrated database.
/// * `command` - which key subcommand.
/// * `clock` - the source of `created_at` and `revoked_at`.
///
/// # Errors
/// The store's refusal or database error.
#[tracing::instrument(level = "debug", skip_all)]
async fn run_key(pool: &PgPool, command: KeyCommand, clock: &dyn Clock) -> anyhow::Result<()> {
    match command {
        KeyCommand::Create(args) => {
            let created = if args.provisioner {
                keys::create_provisioner(
                    pool,
                    &keys::NewProvisioner {
                        name: &args.name,
                        actor: args.actor.as_deref().unwrap_or(&args.name),
                        user: &args.user,
                    },
                    &crate::store::audit::AuditActor::cli(),
                    clock.now(),
                )
                .await?
            } else {
                let grants = if args.all {
                    Grants::All
                } else {
                    Grants::Catalogs(&args.catalogs)
                };
                let new = NewKey {
                    name: &args.name,
                    access: args
                        .access
                        .expect("clap requires access without provisioner"),
                    maintenance_report: args.maintenance_report,
                    grants,
                    default_catalog: args.default.as_deref(),
                    actor: args.actor.as_deref().unwrap_or(&args.name),
                    user: &args.user,
                    self_issued: false,
                    expires_at: None,
                    levels: &[],
                };
                keys::create_with_id(
                    pool,
                    &new,
                    &crate::store::audit::AuditActor::cli(),
                    clock.now(),
                )
                .await?
            };
            println!("{}", created.key);
            eprintln!(
                "key id: {}\nstore this key now: it is not shown again",
                created.id
            );
        }
        KeyCommand::List => {
            for key in keys::list(pool).await? {
                println!("{}", format_key_record(&key));
            }
        }
        KeyCommand::MaintenanceReport { id, enabled, actor } => {
            let resolved = keys::set_maintenance_report_target(
                pool,
                &id,
                actor.as_deref(),
                enabled,
                &crate::store::audit::AuditActor::cli(),
                clock.now(),
            )
            .await?;
            println!("key {resolved} maintenance_report {enabled}");
        }
        KeyCommand::Revoke { id, actor } => {
            let resolved = keys::revoke_target(
                pool,
                &id,
                actor.as_deref(),
                &crate::store::audit::AuditActor::cli(),
                clock.now(),
            )
            .await?;
            println!("revoked key {resolved}");
        }
        KeyCommand::Grant { id, actor, catalog } => {
            let resolved = keys::grant_target(
                pool,
                &id,
                actor.as_deref(),
                &catalog,
                &crate::store::audit::AuditActor::cli(),
                clock.now(),
            )
            .await?;
            println!("granted {catalog} to key {resolved}");
        }
        KeyCommand::Ungrant { id, actor, catalog } => {
            let resolved = keys::ungrant_target(
                pool,
                &id,
                actor.as_deref(),
                &catalog,
                &crate::store::audit::AuditActor::cli(),
                clock.now(),
            )
            .await?;
            println!("removed {catalog} from key {resolved}");
        }
        KeyCommand::Assign { id, actor, user } => {
            print!(
                "{}",
                people::assign_key(pool, &id, actor.as_deref(), &user, clock.now()).await?
            );
        }
    }
    Ok(())
}

/// Joins one listing line's cells with tabs: the layout of every `list`
/// subcommand, so `cut -f` can pick a column.
///
/// # Arguments
/// * `cells` - the cells, in column order.
///
/// # Returns
/// The line, without a newline.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
    )
)]
fn tab_row(cells: &[&str]) -> String {
    cells.join("\t")
}

/// A labelled listing cell, `label value`, with `-` for no value (as in
/// `default -`).
///
/// # Arguments
/// * `label` - the column's name.
/// * `value` - its value, if any.
///
/// # Returns
/// The cell.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
    )
)]
fn labelled(label: &str, value: Option<&str>) -> String {
    format!("{label} {}", value.unwrap_or("-"))
}

/// Comma-joins `items`, or `None` when there are none (a `-` cell).
///
/// # Arguments
/// * `items` - the names.
///
/// # Returns
/// `"a,b"`, or `None` for an empty slice.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
    )
)]
fn comma_list(items: &[String]) -> Option<String> {
    // `then` runs the closure only when the condition holds: `x if c else None`.
    (!items.is_empty()).then(|| items.join(","))
}

/// One `catalog list` line, tab-separated: name, `generation N`, `owner T`
/// (`-` for none), then the free-text description last, since it may
/// contain anything.
///
/// # Arguments
/// * `catalog` - the catalog.
///
/// # Returns
/// The line, without a newline.
#[tracing::instrument(level = "trace", skip_all)]
fn format_catalog(catalog: &catalogs::Catalog) -> String {
    tab_row(&[
        &catalog.name,
        &format!("generation {}", catalog.generation),
        &labelled("owner", catalog.owner_team.as_deref()),
        &catalog.description,
    ])
}

/// One `key list` line, tab-separated: name, actor, id, access, state,
/// catalogs (`*` for all), `default X`, `maintenance_report B`,
/// `provisioner yes|no`, `user U` (`-` for none), `kind key|oauth`,
/// `client NAME` (`-` for a key) and `last_activity T` (a grant's newest
/// token's issue time; `-` for a key or a grant with no token on record).
///
/// # Arguments
/// * `key` - the key or grant.
///
/// # Returns
/// The line, without a newline.
#[tracing::instrument(level = "trace", skip_all)]
fn format_key_record(key: &keys::KeyRecord) -> String {
    let catalogs = if key.all_catalogs {
        "*".to_string()
    } else {
        key.catalogs.join(",")
    };
    let state = if key.revoked_at.is_some() {
        "revoked"
    } else {
        "active"
    };
    tab_row(&[
        &key.name,
        &key.actor,
        &key.id,
        key.access.as_str(),
        state,
        &catalogs,
        &labelled("default", key.default_catalog.as_deref()),
        &format!("maintenance_report {}", key.maintenance_report),
        &format!("provisioner {}", if key.provisioner { "yes" } else { "no" }),
        &labelled("user", key.user.as_deref()),
        &format!("kind {}", key.kind.as_str()),
        &labelled("client", key.client_name.as_deref()),
        &labelled(
            "last_activity",
            key.last_activity.map(format_timestamp).as_deref(),
        ),
    ])
}

/// Runs one `okf-mcp oauth ...` subcommand. `code create` prints the code
/// alone on stdout and its id and expiry on stderr, as `key create` does.
///
/// # Arguments
/// * `pool` - the migrated database.
/// * `command` - the subcommand.
/// * `config` - for `oauth.code_max_expiry`.
/// * `clock` - the source of every timestamp.
///
/// # Errors
/// The store's refusal or database error; an id that is no OAuth grant.
#[tracing::instrument(level = "debug", skip_all)]
async fn run_oauth(
    pool: &PgPool,
    command: OauthCommand,
    config: &Config,
    clock: &dyn Clock,
) -> anyhow::Result<()> {
    let now = clock.now();
    match command {
        OauthCommand::Code(CodeCommand::Create(args)) => {
            let expires_at = codes::expiry(now, args.expires, config.oauth.code_max_expiry)?;
            let grants = if args.all {
                Grants::All
            } else {
                Grants::Catalogs(&args.catalogs)
            };
            let new = NewSignInCode {
                name: &args.name,
                access: args.access,
                grants,
                default_catalog: args.default.as_deref(),
                // The store treats a blank actor as the code's name.
                actor: args.actor.as_deref().unwrap_or(""),
                expires_at,
                user: &args.user,
            };
            let created =
                codes::create(pool, &new, &crate::store::audit::AuditActor::cli(), now).await?;
            println!("{}", created.code);
            eprintln!(
                "code id: {}\nexpires: {}\nhand this code over now: it is not shown again",
                created.id,
                format_timestamp(expires_at)
            );
        }
        OauthCommand::Code(CodeCommand::List) => {
            for code in codes::list(pool).await? {
                println!("{}", format_code_record(&code, now));
            }
        }
        OauthCommand::Code(CodeCommand::Revoke { id }) => {
            codes::revoke(pool, &id, &crate::store::audit::AuditActor::cli(), now).await?;
            println!("revoked sign-in code {id}");
        }
        OauthCommand::Code(CodeCommand::Assign { id, user }) => {
            print!("{}", people::assign_code(pool, &id, &user, now).await?);
        }
        OauthCommand::Grant(GrantCommand::List) => {
            for grant in keys::list(pool).await? {
                if grant.kind == KeyKind::OAuth {
                    println!("{}", format_key_record(&grant));
                }
            }
        }
        OauthCommand::Grant(GrantCommand::Revoke { id }) => {
            // The one exact-id path to a grant on the command line: `key
            // revoke` resolves API keys only (`store::keys`).
            let is_grant = keys::list(pool)
                .await?
                .iter()
                .any(|key| key.id == id && key.kind == KeyKind::OAuth);
            if !is_grant {
                anyhow::bail!("no OAuth grant {id:?} (a plain key is revoked with `key revoke`)");
            }
            keys::revoke_by_id(pool, &id, &crate::store::audit::AuditActor::cli(), now).await?;
            println!("revoked OAuth grant {id}");
        }
    }
    Ok(())
}

/// One `oauth code list` line, tab-separated: name, actor, id, access,
/// state, catalogs (`*` for all), `default X`, `expires T`, `grant G`,
/// `user U` (`-` for none).
///
/// # Arguments
/// * `code` - the code.
/// * `now` - the time its state is computed at.
///
/// # Returns
/// The line, without a newline.
#[tracing::instrument(level = "trace", skip_all)]
fn format_code_record(code: &SignInCodeRecord, now: chrono::DateTime<chrono::Utc>) -> String {
    let catalogs = if code.all_catalogs {
        "*".to_string()
    } else {
        code.catalogs.join(",")
    };
    tab_row(&[
        &code.name,
        &code.actor,
        &code.id,
        code.access.as_str(),
        code.state(now).as_str(),
        &catalogs,
        &labelled("default", code.default_catalog.as_deref()),
        &format!("expires {}", format_timestamp(code.expires_at)),
        &labelled("grant", code.grant_id.as_deref()),
        &labelled("user", code.user.as_deref()),
    ])
}

/// The store database holding catalog `name`'s data, opened through the
/// registry like the server does.
///
/// # Arguments
/// * `pool` - the account database.
/// * `config` - the settings (`store_*`) the store's pool is opened with.
/// * `name` - the catalog's name. One with no registry row (unknown, or not
///   provisioned yet) is looked up on `main`, so the store function's own
///   "no such catalog" refusal is what the caller sees.
///
/// # Returns
/// The store's handle.
///
/// # Errors
/// The registry read, or [`OkfError::StoreUnavailable`] when the store
/// cannot be reached.
#[tracing::instrument(level = "debug", skip_all)]
async fn catalog_store(pool: &PgPool, config: &Config, name: &str) -> anyhow::Result<CatalogDb> {
    let store = match catalogs::row_by_name(pool, name).await? {
        Some(row) => row.store,
        None => StoreName::main(),
    };
    let stores = Stores::new(pool.clone(), PoolSettings::from_config(config));
    Ok(stores.get(&store).await?)
}

/// Connects to the database and applies the migrations.
#[tracing::instrument(level = "debug", skip_all)]
async fn connect(url: Option<&str>, config: &Config) -> anyhow::Result<PgPool> {
    let url = url.context("no database: set DATABASE_URL or pass --database-url")?;
    let pool = db::connect(url, config.database_max_connections)
        .await
        .context("connect to the database")?;
    db::migrate(&pool).await?;
    Ok(pool)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FixedClock;

    use super::run_in_tests as run;

    /// A fixed clock at the current instant, as `run` takes it.
    fn fixed_now() -> Arc<dyn Clock> {
        Arc::new(FixedClock(chrono::Utc::now()))
    }

    fn cli(database_url: Option<String>, command: Command) -> Cli {
        Cli {
            database_url,
            config: None,
            command: Some(command),
        }
    }

    #[test]
    fn serve_is_the_default_and_takes_listen() {
        let default = Cli::try_parse_from(["okf-mcp"]).unwrap();
        let flagged =
            Cli::try_parse_from(["okf-mcp", "serve", "--listen", "127.0.0.1:9000"]).unwrap();
        let migrate = Cli::try_parse_from(["okf-mcp", "migrate"]).unwrap();

        assert!(default.command.is_none());
        assert!(matches!(
            flagged.command,
            Some(Command::Serve(ServeArgs { listen: Some(addr) })) if addr.port() == 9000
        ));
        assert!(matches!(migrate.command, Some(Command::Migrate)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn migrate_applies_the_schema() {
        let db = crate::db::pg_support::TestDb::start().await;
        let clock = fixed_now();

        run(
            cli(Some(db.url.clone()), Command::Migrate),
            Arc::clone(&clock),
        )
        .await
        .unwrap();
        // A second run finds nothing to apply: every later subcommand
        // migrates on connect, so this must be a no-op.
        run(
            cli(Some(db.url.clone()), Command::Migrate),
            Arc::clone(&clock),
        )
        .await
        .unwrap();

        let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        // One row per embedded migration (`iter` walks the files `migrate!`
        // found), so adding a migration does not break this test.
        let embedded = i64::try_from(crate::db::MIGRATOR.iter().count()).unwrap();
        assert_eq!(applied, embedded);
    }

    #[test]
    fn catalog_subcommands_parse() {
        let parsed = Cli::try_parse_from([
            "okf-mcp",
            "catalog",
            "create",
            "pg",
            "--description",
            "D",
            "--owner",
            "family",
        ])
        .unwrap();
        assert!(matches!(
            parsed.command,
            Some(Command::Catalog(CatalogCommand::Create { ref name, ref description, ref owner, .. }))
                if name == "pg" && description == "D" && owner == "family"
        ));
        // Every new catalog has an owner, so `--owner` is required.
        assert!(Cli::try_parse_from(["okf-mcp", "catalog", "create", "pg"]).is_err());
        let parsed =
            Cli::try_parse_from(["okf-mcp", "catalog", "update", "pg", "--description", ""])
                .unwrap();
        assert!(matches!(
            parsed.command,
            Some(Command::Catalog(CatalogCommand::Update { ref name, ref description, ref display_name }))
                if name == "pg" && description.as_deref() == Some("") && display_name.is_none()
        ));
        // `update` changes at least one field, so it needs a flag; either
        // flag alone or both together parse.
        assert!(Cli::try_parse_from(["okf-mcp", "catalog", "update", "pg"]).is_err());
        for flags in [
            vec!["--display-name", "N"],
            vec!["--display-name", "N", "--description", "D"],
        ] {
            let args = ["okf-mcp", "catalog", "update", "pg"]
                .into_iter()
                .chain(flags);
            assert!(Cli::try_parse_from(args).is_ok());
        }
        let parsed =
            Cli::try_parse_from(["okf-mcp", "catalog", "delete", "pg", "--force"]).unwrap();
        assert!(matches!(
            parsed.command,
            Some(Command::Catalog(CatalogCommand::Delete { force: true, .. }))
        ));
    }

    #[test]
    fn catalog_update_prints_one_line_per_field_given() {
        let previous = catalogs::Details {
            display_name: String::new(),
            description: "Old".to_string(),
        };
        let name = DisplayName::parse("Work notes").unwrap();

        let both = describe_details_change("pg", &previous, Some(&name), Some(""));
        let only_name = describe_details_change("pg", &previous, Some(&name), None);

        assert_eq!(
            both,
            "updated catalog pg display name: \"\" -> \"Work notes\"\n\
             updated catalog pg description: \"Old\" -> \"\"\n"
        );
        assert_eq!(
            only_name,
            "updated catalog pg display name: \"\" -> \"Work notes\"\n"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn catalog_create_and_update_store_the_display_name() {
        // Arrange
        let db = crate::db::test_db().await;
        let clock = fixed_now();
        let run_catalog_command = |command: CatalogCommand| {
            run(
                cli(Some(db.url.clone()), Command::Catalog(command)),
                Arc::clone(&clock),
            )
        };
        let stored = || async {
            sqlx::query_as::<_, (String, String)>(
                "SELECT display_name, description FROM catalogs WHERE name = 'pg'",
            )
            .fetch_one(&db.pool)
            .await
            .unwrap()
        };

        // Act and assert: create with a display name (trimmed), then change
        // each field alone, then refuse one over the limit.
        run_catalog_command(CatalogCommand::Create {
            name: "pg".to_string(),
            display_name: " Work ".to_string(),
            description: "D".to_string(),
            owner: crate::db::TEST_TEAM.to_string(),
        })
        .await
        .unwrap();
        assert_eq!(stored().await, ("Work".to_string(), "D".to_string()));
        run_catalog_command(CatalogCommand::Update {
            name: "pg".to_string(),
            display_name: Some("Work notes".to_string()),
            description: None,
        })
        .await
        .unwrap();
        assert_eq!(stored().await, ("Work notes".to_string(), "D".to_string()));
        run_catalog_command(CatalogCommand::Update {
            name: "pg".to_string(),
            display_name: None,
            description: Some("E".to_string()),
        })
        .await
        .unwrap();
        assert_eq!(stored().await, ("Work notes".to_string(), "E".to_string()));
        let refused = run_catalog_command(CatalogCommand::Update {
            name: "pg".to_string(),
            display_name: Some("x".repeat(81)),
            description: None,
        })
        .await
        .expect_err("81 characters are refused");
        assert!(
            refused.to_string().contains("display name refused"),
            "{refused}"
        );
        assert_eq!(stored().await, ("Work notes".to_string(), "E".to_string()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn catalog_create_runs_against_the_database() {
        let db = crate::db::test_db().await;
        let create = CatalogCommand::Create {
            name: "pg".to_string(),
            display_name: String::new(),
            description: String::new(),
            owner: crate::db::TEST_TEAM.to_string(),
        };

        run(
            cli(Some(db.url.clone()), Command::Catalog(create)),
            fixed_now(),
        )
        .await
        .unwrap();

        let names: Vec<String> = sqlx::query_scalar("SELECT name FROM catalogs")
            .fetch_all(&db.pool)
            .await
            .unwrap();
        assert_eq!(names, ["pg"]);
    }

    #[test]
    fn key_create_needs_catalogs_or_all_but_not_both() {
        let parsed = Cli::try_parse_from([
            "okf-mcp",
            "key",
            "create",
            "k",
            "--access",
            "write",
            "--catalogs",
            "pg,work",
            "--user",
            "alice",
        ])
        .unwrap();
        let Some(Command::Key(KeyCommand::Create(args))) = parsed.command else {
            panic!("expected key create");
        };
        assert_eq!(args.name, "k");
        assert_eq!(args.user, "alice");
        assert_eq!(
            (
                args.access.expect("ordinary key access"),
                args.catalogs,
                args.all
            ),
            (
                Access::Write,
                vec!["pg".to_string(), "work".to_string()],
                false
            )
        );

        let both = Cli::try_parse_from([
            "okf-mcp",
            "key",
            "create",
            "k",
            "--access",
            "read",
            "--catalogs",
            "pg",
            "--all",
            "--user",
            "alice",
        ]);
        let neither = Cli::try_parse_from([
            "okf-mcp", "key", "create", "k", "--access", "read", "--user", "alice",
        ]);
        // Every new key names its user, so `--user` is required.
        let no_user =
            Cli::try_parse_from(["okf-mcp", "key", "create", "k", "--access", "read", "--all"]);
        assert_eq!(
            both.unwrap_err().kind(),
            clap::error::ErrorKind::ArgumentConflict
        );
        assert_eq!(
            neither.unwrap_err().kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
        assert_eq!(
            no_user.unwrap_err().kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn key_subcommands_run_against_the_database() {
        let db = crate::db::test_db().await;
        let clock = fixed_now();
        let url = Some(db.url.clone());
        let create = KeyCommand::Create(KeyCreateArgs {
            name: "k".to_string(),
            access: Some(Access::Read),
            provisioner: false,
            maintenance_report: false,
            catalogs: Vec::new(),
            all: true,
            default: None,
            actor: None,
            user: crate::db::TEST_USER.to_string(),
        });

        run(cli(url.clone(), Command::Key(create)), Arc::clone(&clock))
            .await
            .unwrap();
        run(
            cli(url.clone(), Command::Key(KeyCommand::List)),
            Arc::clone(&clock),
        )
        .await
        .unwrap();
        let revoke = KeyCommand::Revoke {
            id: "k".to_string(),
            actor: None,
        };
        run(cli(url, Command::Key(revoke)), Arc::clone(&clock))
            .await
            .unwrap();

        let rows: Vec<(String, bool, bool)> =
            sqlx::query_as("SELECT actor, all_catalogs, revoked_at IS NOT NULL FROM api_keys")
                .fetch_all(&db.pool)
                .await
                .unwrap();
        // The actor defaults to the id.
        assert_eq!(rows, [("k".to_string(), true, true)]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn maintenance_capability_cli_resolves_actor_and_reissued_names() {
        let db = crate::db::test_db().await;
        let clock = fixed_now();
        let url = Some(db.url.clone());
        for actor in ["terminal", "web"] {
            let parsed = Cli::try_parse_from([
                "okf-mcp",
                "key",
                "create",
                "shared",
                "--access",
                "read",
                "--all",
                "--actor",
                actor,
                "--maintenance-report",
                "--user",
                crate::db::TEST_USER,
            ])
            .unwrap();
            let Some(Command::Key(command)) = parsed.command else {
                panic!("expected key create")
            };
            run(cli(url.clone(), Command::Key(command)), Arc::clone(&clock))
                .await
                .unwrap();
        }
        let records = keys::list(&db.pool).await.unwrap();
        assert!(records.iter().all(|record| record.maintenance_report));
        let old_id = records
            .iter()
            .find(|record| record.actor == "terminal")
            .unwrap()
            .id
            .clone();
        assert_ne!(
            old_id,
            records
                .iter()
                .find(|record| record.actor == "web")
                .unwrap()
                .id
        );
        let without_flag = Cli::try_parse_from([
            "okf-mcp",
            "key",
            "create",
            "ordinary",
            "--access",
            "admin",
            "--all",
            "--user",
            crate::db::TEST_USER,
        ])
        .unwrap();
        let Some(Command::Key(ordinary)) = without_flag.command else {
            panic!("expected key create")
        };
        run(cli(url.clone(), Command::Key(ordinary)), Arc::clone(&clock))
            .await
            .unwrap();
        assert!(
            !keys::list(&db.pool)
                .await
                .unwrap()
                .iter()
                .find(|record| record.name == "ordinary")
                .unwrap()
                .maintenance_report
        );
        assert!(Cli::try_parse_from([
            "okf-mcp",
            "key",
            "maintenance-report",
            "shared",
            "--enabled",
            "maybe"
        ])
        .is_err());
        assert!(Cli::try_parse_from(["okf-mcp", "key", "maintenance-report", "shared"]).is_err());
        let toggle = |target: &str, actor: Option<&str>, enabled: &str| {
            let mut args = vec![
                "okf-mcp",
                "key",
                "maintenance-report",
                target,
                "--enabled",
                enabled,
            ];
            if let Some(actor) = actor {
                args.extend(["--actor", actor]);
            }
            let Some(Command::Key(command)) = Cli::try_parse_from(args).unwrap().command else {
                panic!("expected key maintenance-report")
            };
            command
        };
        assert!(run(
            cli(url.clone(), Command::Key(toggle("shared", None, "false"))),
            Arc::clone(&clock)
        )
        .await
        .is_err());
        run(
            cli(
                url.clone(),
                Command::Key(toggle("shared", Some("terminal"), "false")),
            ),
            Arc::clone(&clock),
        )
        .await
        .unwrap();
        let records = keys::list(&db.pool).await.unwrap();
        assert!(
            !records
                .iter()
                .find(|record| record.id == old_id)
                .unwrap()
                .maintenance_report
        );
        assert!(
            records
                .iter()
                .find(|record| record.actor == "web")
                .unwrap()
                .maintenance_report
        );
        assert!(
            format_key_record(records.iter().find(|record| record.id == old_id).unwrap())
                .ends_with(
                "\tmaintenance_report false\tprovisioner no\tuser test-user\tkind key\tclient -\t\
                 last_activity -"
            )
        );
        run(
            cli(
                url.clone(),
                Command::Key(KeyCommand::Revoke {
                    id: old_id.clone(),
                    actor: None,
                }),
            ),
            Arc::clone(&clock),
        )
        .await
        .unwrap();
        let parsed = Cli::try_parse_from([
            "okf-mcp",
            "key",
            "create",
            "shared",
            "--access",
            "read",
            "--all",
            "--actor",
            "terminal",
            "--user",
            crate::db::TEST_USER,
        ])
        .unwrap();
        let Some(Command::Key(command)) = parsed.command else {
            panic!("expected key create")
        };
        run(cli(url.clone(), Command::Key(command)), Arc::clone(&clock))
            .await
            .unwrap();
        let records = keys::list(&db.pool).await.unwrap();
        let replacement = records
            .iter()
            .find(|record| record.actor == "terminal" && record.revoked_at.is_none())
            .unwrap();
        let revoked_credential_id = old_id;
        let replacement_credential_id = &replacement.id;
        assert_ne!(revoked_credential_id, *replacement_credential_id);
        assert!(!replacement.maintenance_report);
        assert!(run(
            cli(
                url.clone(),
                Command::Key(toggle(&revoked_credential_id, None, "true"))
            ),
            Arc::clone(&clock)
        )
        .await
        .is_err());
        run(
            cli(
                url.clone(),
                Command::Key(toggle("shared", Some("terminal"), "true")),
            ),
            Arc::clone(&clock),
        )
        .await
        .unwrap();
        let records = keys::list(&db.pool).await.unwrap();
        assert!(
            records
                .iter()
                .find(|record| record.id == *replacement_credential_id)
                .unwrap()
                .maintenance_report
        );
        assert!(
            !records
                .iter()
                .find(|record| record.id == revoked_credential_id)
                .unwrap()
                .maintenance_report
        );
        assert!(
            records
                .iter()
                .find(|record| record.actor == "web")
                .unwrap()
                .maintenance_report
        );
        assert!(run(
            cli(
                url,
                Command::Key(toggle(replacement_credential_id, Some("terminal"), "false"))
            ),
            clock
        )
        .await
        .is_err());
    }

    #[test]
    fn key_list_columns_have_the_documented_order() {
        let record = keys::KeyRecord {
            id: "credential-id".to_string(),
            name: "claude".to_string(),
            sha256: None,
            access: Access::Write,
            maintenance_report: false,
            provisioner: false,
            all_catalogs: false,
            catalogs: vec!["pg".to_string(), "work".to_string()],
            catalog_access: [("pg", Access::Write), ("work", Access::Write)]
                .into_iter()
                .map(|(name, level)| (name.to_string(), level))
                .collect(),
            default_catalog: Some("pg".to_string()),
            actor: "claude-code-tui".to_string(),
            created_at: chrono::Utc::now(),
            revoked_at: None,
            expires_at: None,
            kind: keys::KeyKind::Key,
            client_id: None,
            client_name: None,
            user: None,
            user_id: None,
            self_issued: false,
            last_activity: None,
        };
        let assigned = keys::KeyRecord {
            user: Some("alice".to_string()),
            ..record.clone()
        };

        assert_eq!(
            format_key_record(&record),
            "claude\tclaude-code-tui\tcredential-id\twrite\tactive\tpg,work\tdefault pg\t\
             maintenance_report false\tprovisioner no\tuser -\tkind key\tclient -\t\
             last_activity -"
        );
        assert!(format_key_record(&assigned)
            .ends_with("\tuser alice\tkind key\tclient -\tlast_activity -"));
        let grant = keys::KeyRecord {
            kind: keys::KeyKind::OAuth,
            client_name: Some("Example Client".to_string()),
            last_activity: Some("2026-11-09T12:00:00Z".parse().expect("a time")),
            ..record.clone()
        };
        assert!(format_key_record(&grant).ends_with(
            "\tuser -\tkind oauth\tclient Example Client\tlast_activity 2026-11-09T12:00:00Z"
        ));
    }

    #[test]
    fn catalog_list_columns_have_the_documented_order() {
        let catalog = catalogs::Catalog {
            id: 1,
            name: "pg".to_string(),
            display_name: String::new(),
            description: "Notes\tand more".to_string(),
            generation: 3,
            created_at: chrono::Utc::now(),
            owner_team: Some("family".to_string()),
        };
        let legacy = catalogs::Catalog {
            owner_team: None,
            description: String::new(),
            ..catalog.clone()
        };

        assert_eq!(
            format_catalog(&catalog),
            "pg\tgeneration 3\towner family\tNotes\tand more"
        );
        assert_eq!(format_catalog(&legacy), "pg\tgeneration 3\towner -\t");
    }

    #[test]
    fn oauth_code_create_parses_like_key_create() {
        let parse = |args: &[&str]| {
            // `chain` puts the program name in front, as the shell would.
            Cli::try_parse_from(["okf-mcp", "oauth", "code", "create"].iter().chain(args))
        };
        let parsed = parse(&[
            "laptop",
            "--actor",
            "example-actor",
            "--access",
            "write",
            "--catalogs",
            "main,other",
            "--default",
            "main",
            "--expires",
            "3d",
            "--user",
            "alice",
        ])
        .unwrap();
        let defaulted = parse(&["c", "--access", "read", "--all", "--user", "a"]).unwrap();

        let Some(Command::Oauth(OauthCommand::Code(CodeCommand::Create(args)))) = parsed.command
        else {
            panic!("expected oauth code create");
        };
        assert_eq!(args.name, "laptop");
        assert_eq!(args.actor.as_deref(), Some("example-actor"));
        assert_eq!(args.user, "alice");
        assert_eq!(args.access, Access::Write);
        assert_eq!(args.catalogs, ["main", "other"]);
        assert!(!args.all);
        assert_eq!(args.default.as_deref(), Some("main"));
        assert_eq!(args.expires, Duration::from_secs(3 * 24 * 60 * 60));
        let Some(Command::Oauth(OauthCommand::Code(CodeCommand::Create(args)))) = defaulted.command
        else {
            panic!("expected oauth code create");
        };
        assert!(args.all);
        // `--actor` is optional: the store defaults it to the code's name.
        assert_eq!(args.actor, None);
        // `default_value = "24h"` and the store's constant agree.
        assert_eq!(args.expires, codes::DEFAULT_EXPIRY);
        // `--user` is required, `--expires` must parse, and `--catalogs`
        // and `--all` exclude each other.
        assert_eq!(
            parse(&["c", "--actor", "a", "--access", "read", "--all"])
                .unwrap_err()
                .kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
        assert_eq!(
            parse(&[
                "c",
                "--actor",
                "a",
                "--access",
                "read",
                "--all",
                "--expires",
                "soon",
                "--user",
                "a"
            ])
            .unwrap_err()
            .kind(),
            clap::error::ErrorKind::ValueValidation
        );
        assert_eq!(
            parse(&[
                "c",
                "--actor",
                "a",
                "--access",
                "read",
                "--all",
                "--catalogs",
                "main",
                "--user",
                "a"
            ])
            .unwrap_err()
            .kind(),
            clap::error::ErrorKind::ArgumentConflict
        );
        assert!(matches!(
            Cli::try_parse_from(["okf-mcp", "oauth", "grant", "revoke", "g"]).unwrap().command,
            Some(Command::Oauth(OauthCommand::Grant(GrantCommand::Revoke { ref id }))) if id == "g"
        ));
        assert!(matches!(
            Cli::try_parse_from(["okf-mcp", "oauth", "code", "revoke", "c"]).unwrap().command,
            Some(Command::Oauth(OauthCommand::Code(CodeCommand::Revoke { ref id }))) if id == "c"
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oauth_subcommands_run_against_the_database() {
        use crate::store::oauth::clients::ClientKind;
        use crate::store::oauth::codes::insert_test_client;

        let db = crate::db::test_db().await;
        let clock = fixed_now();
        let url = Some(db.url.clone());
        // Runs `okf-mcp <args>` against the test database.
        let okf = |args: &[&str]| {
            let command = Cli::try_parse_from(std::iter::once(&"okf-mcp").chain(args))
                .expect("a valid command line")
                .command
                .expect("a subcommand");
            run(cli(url.clone(), command), Arc::clone(&clock))
        };
        okf(&["catalog", "create", "main", "--owner", crate::db::TEST_TEAM])
            .await
            .unwrap();

        okf(&[
            "oauth",
            "code",
            "create",
            "laptop",
            "--actor",
            "example-actor",
            "--access",
            "read",
            "--catalogs",
            "main",
            "--user",
            crate::db::TEST_USER,
        ])
        .await
        .unwrap();
        let too_long = okf(&[
            "oauth",
            "code",
            "create",
            "late",
            "--actor",
            "example-actor",
            "--access",
            "read",
            "--all",
            "--expires",
            "8d",
            "--user",
            crate::db::TEST_USER,
        ])
        .await;
        let admin = okf(&[
            "oauth",
            "code",
            "create",
            "boss",
            "--actor",
            "example-actor",
            "--access",
            "admin",
            "--all",
            "--user",
            crate::db::TEST_USER,
        ])
        .await;
        okf(&["oauth", "code", "list"]).await.unwrap();

        let minted = codes::list(&db.pool).await.unwrap();
        assert_eq!(minted.len(), 1, "8 days and admin are refused");
        assert!(too_long.is_err() && admin.is_err());
        assert_eq!(minted[0].name, "laptop");
        assert_eq!(minted[0].catalogs, ["main"]);
        // Whole seconds: Postgres keeps microseconds, the clock nanoseconds.
        assert_eq!(
            minted[0].expires_at.timestamp(),
            (clock.now() + chrono::TimeDelta::hours(24)).timestamp()
        );
        okf(&["oauth", "code", "revoke", &minted[0].id])
            .await
            .unwrap();
        assert!(codes::list(&db.pool).await.unwrap()[0].revoked_at.is_some());
        assert!(okf(&["oauth", "code", "revoke", &minted[0].id])
            .await
            .is_err());

        // A grant, made the way the consent page makes one.
        let main = ["main".to_string()];
        let code = codes::create(
            &db.pool,
            &NewSignInCode {
                name: "phone",
                access: Access::Write,
                grants: Grants::Catalogs(&main),
                default_catalog: None,
                actor: "example-actor",
                expires_at: clock.now() + chrono::TimeDelta::hours(1),
                user: crate::db::TEST_USER,
            },
            &crate::store::audit::AuditActor::cli(),
            clock.now(),
        )
        .await
        .unwrap();
        let client = insert_test_client(
            &db.pool,
            "dcr-client",
            ClientKind::Dcr,
            "https://client.example.com/callback",
        )
        .await;
        let grant = codes::redeem(&db.pool, &code.id, &client, clock.now())
            .await
            .unwrap();
        let grant_name = keys::list(&db.pool)
            .await
            .unwrap()
            .into_iter()
            .find(|key| key.id == grant)
            .expect("the grant")
            .name;
        // A plain key with the grant's very name and actor (allowed: the
        // live name/actor rule covers keys only).
        let plain = keys::create_with_id(
            &db.pool,
            &NewKey {
                name: &grant_name,
                access: Access::Read,
                maintenance_report: false,
                grants: Grants::All,
                default_catalog: None,
                actor: "example-actor",
                user: crate::db::TEST_USER,
                self_issued: false,
                expires_at: None,
                levels: &[],
            },
            &crate::store::audit::AuditActor::cli(),
            clock.now(),
        )
        .await
        .unwrap();

        okf(&["oauth", "grant", "list"]).await.unwrap();
        // `key ...` by name reaches the key alone: not ambiguous, grant untouched.
        okf(&[
            "key",
            "maintenance-report",
            &grant_name,
            "--enabled",
            "true",
        ])
        .await
        .unwrap();
        // `key revoke` does not take a grant's id, and `oauth grant revoke`
        // does not take a plain key's.
        assert!(okf(&["key", "revoke", &grant]).await.is_err());
        assert!(okf(&["oauth", "grant", "revoke", &plain.id]).await.is_err());
        okf(&["oauth", "grant", "revoke", &grant]).await.unwrap();

        let rows: Vec<(String, bool, bool)> = sqlx::query_as(
            "SELECT id, revoked_at IS NOT NULL, maintenance_report FROM api_keys ORDER BY id",
        )
        .fetch_all(&db.pool)
        .await
        .unwrap();
        assert!(rows.contains(&(grant, true, false)));
        assert!(rows.contains(&(plain.id, false, true)));
    }

    #[test]
    fn code_list_columns_have_the_documented_order() {
        let at = |text: &str| {
            chrono::DateTime::parse_from_rfc3339(text)
                .expect("a valid timestamp")
                .to_utc()
        };
        let record = SignInCodeRecord {
            id: "code-id".to_string(),
            name: "laptop".to_string(),
            access: Access::Read,
            all_catalogs: false,
            catalogs: vec!["main".to_string(), "other".to_string()],
            default_catalog: None,
            actor: "example-actor".to_string(),
            created_at: at("2026-11-11T11:00:00Z"),
            expires_at: at("2026-11-12T12:00:00Z"),
            redeemed_at: None,
            grant_id: None,
            revoked_at: None,
            user: Some("alice".to_string()),
        };
        let redeemed = SignInCodeRecord {
            all_catalogs: true,
            catalogs: Vec::new(),
            default_catalog: Some("main".to_string()),
            redeemed_at: Some(at("2026-11-11T13:00:00Z")),
            grant_id: Some("grant-id".to_string()),
            user: None,
            ..record.clone()
        };

        assert_eq!(
            format_code_record(&record, at("2026-11-11T12:00:00Z")),
            "laptop\texample-actor\tcode-id\tread\tlive\tmain,other\tdefault -\t\
             expires 2026-11-12T12:00:00Z\tgrant -\tuser alice"
        );
        assert_eq!(
            format_code_record(&redeemed, at("2026-11-11T14:00:00Z")),
            "laptop\texample-actor\tcode-id\tread\tredeemed\t*\tdefault main\t\
             expires 2026-11-12T12:00:00Z\tgrant grant-id\tuser -"
        );
    }

    #[test]
    fn repair_indexes_parses_names_and_flags() {
        let parsed = Cli::try_parse_from([
            "okf-mcp",
            "catalog",
            "repair-indexes",
            "pg",
            "work",
            "--missing-only",
            "--dry-run",
        ])
        .unwrap();
        let bare = Cli::try_parse_from(["okf-mcp", "catalog", "repair-indexes", "--missing-only"])
            .unwrap();

        assert!(matches!(
            parsed.command,
            Some(Command::Catalog(CatalogCommand::RepairIndexes {
                ref names,
                missing_only: true,
                dry_run: true,
                ref actor,
            })) if names == &["pg", "work"] && actor == "okf-mcp/repair-indexes"
        ));
        assert!(matches!(
            bare.command,
            Some(Command::Catalog(CatalogCommand::RepairIndexes {
                ref names,
                missing_only: true,
                dry_run: false,
                ..
            })) if names.is_empty()
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn repair_indexes_repairs_catalogs_without_a_root_index() {
        let db = crate::db::test_db().await;
        for name in ["old", "other", "fresh"] {
            let new = catalogs::NewCatalog {
                name,
                display_name: &DisplayName::default(),
                description: "",
                owner_team: crate::db::TEST_TEAM,
            };
            catalogs::create(&db.pool, &new, &AuditActor::cli(), chrono::Utc::now())
                .await
                .unwrap();
        }
        // `old` and `other` lose their starter files, as a catalog from
        // before them has none.
        for name in ["old", "other"] {
            let legacy = catalogs::find(&db.pool, name).await.unwrap();
            catalogs::tests::drop_starter_files(&db.pool, legacy.id).await;
        }
        let audit = AuditActor::cli();
        let options = |dry_run| repair::RepairOptions {
            dry_run,
            actor: "okf-mcp/repair-indexes",
            audit: &audit,
            now: chrono::Utc::now(),
        };

        let unnamed = repair_targets(
            &AccountDb::of(&db.pool),
            &CatalogDb::main_of(&db.pool),
            Vec::new(),
            false,
        )
        .await
        .unwrap_err();
        let (targets, none_skipped) = repair_targets(
            &AccountDb::of(&db.pool),
            &CatalogDb::main_of(&db.pool),
            Vec::new(),
            true,
        )
        .await
        .unwrap();
        let named = vec!["fresh".into(), "old".into(), "typo".into()];
        let (filtered, skipped) = repair_targets(
            &AccountDb::of(&db.pool),
            &CatalogDb::main_of(&db.pool),
            named,
            true,
        )
        .await
        .unwrap();
        let mut changed = Vec::new();
        let dry = repair_catalogs(
            &crate::stores::CatalogDb::main_in(&db.pool),
            &targets,
            &options(true),
            &mut changed,
        )
        .await
        .unwrap();
        let still_missing = repair_targets(
            &AccountDb::of(&db.pool),
            &CatalogDb::main_of(&db.pool),
            Vec::new(),
            true,
        )
        .await
        .unwrap()
        .0;
        // A failing catalog in the middle of a batch stops neither neighbour.
        let batch = ["old".to_string(), "nope".to_string(), "other".to_string()];
        let dry_changed = std::mem::take(&mut changed);
        let mixed = repair_catalogs(
            &crate::stores::CatalogDb::main_in(&db.pool),
            &batch,
            &options(false),
            &mut changed,
        )
        .await
        .unwrap_err()
        .to_string();
        let after = repair_targets(
            &AccountDb::of(&db.pool),
            &CatalogDb::main_of(&db.pool),
            Vec::new(),
            true,
        )
        .await
        .unwrap()
        .0;

        assert!(unnamed.to_string().contains("--missing-only"), "{unnamed}");
        assert_eq!(targets, ["old", "other"]);
        assert!(none_skipped.is_empty());
        assert_eq!(filtered, ["old"]);
        assert_eq!(skipped, ["fresh", "typo"]);
        assert!(
            dry.starts_with("old (dry run: nothing written): repaired 2 file(s)"),
            "{dry}"
        );
        assert!(dry.contains("  /index.md\n  /log.md\n"), "{dry}");
        assert!(dry.ends_with("would repair 2 of 2 catalog(s)\n"), "{dry}");
        assert_eq!(still_missing, ["old", "other"], "the dry run wrote nothing");
        assert!(mixed.starts_with("old: repaired 2 file(s)"), "{mixed}");
        assert!(mixed.contains("\nother: repaired 2 file(s)"), "{mixed}");
        assert!(
            mixed.contains("repaired 2 of 3 catalog(s)\nfailed:\n  nope: "),
            "{mixed}"
        );
        assert!(after.is_empty(), "{after:?}");
        assert!(
            dry_changed.is_empty(),
            "a dry run commits nothing to announce"
        );
        let ids: Vec<i64> = changed.iter().map(|change| change.catalog_id).collect();
        let expected = vec![
            catalogs::find(&db.pool, "old").await.unwrap().id,
            catalogs::find(&db.pool, "other").await.unwrap().id,
        ];
        assert_eq!(
            ids, expected,
            "both repairs are announced, the failure is not"
        );
    }

    #[test]
    fn import_parses() {
        let parsed = Cli::try_parse_from(["okf-mcp", "import", "pg", "-", "--replace"]).unwrap();

        assert!(matches!(
            parsed.command,
            Some(Command::Import { ref catalog, ref source, replace: true, ref actor, .. })
                if catalog == "pg" && source.as_os_str() == "-" && actor == "okf-mcp/import"
        ));
    }

    #[test]
    fn import_parses_the_machinery_scope_and_the_generation_guard() {
        let parsed = Cli::try_parse_from([
            "okf-mcp",
            "import",
            "pg",
            "-",
            "--replace",
            "--only-machinery",
            "--expect-generation",
            "7",
        ])
        .unwrap();
        let without_replace =
            Cli::try_parse_from(["okf-mcp", "import", "pg", "-", "--only-machinery"]);

        assert!(matches!(
            parsed.command,
            Some(Command::Import {
                only_machinery: true,
                expect_generation: Some(7),
                ..
            })
        ));
        assert!(without_replace.is_err(), "--only-machinery needs --replace");
    }

    /// Issue #979 (M21 Task 20): `catalog repair-indexes` loops over every
    /// store, repairing each catalog on the store that holds it, and names
    /// given for a catalog on another store are not repaired on `main`.
    #[tokio::test(flavor = "multi_thread")]
    async fn repair_indexes_repairs_a_catalog_on_each_store() {
        // Arrange: `on-main` on `main` and `on-b` on a host store of its own,
        // both without a root `index.md`.
        let db = crate::db::test_db().await;
        let database = db.extra_store_database("b").await;
        let options = db.pool.connect_options();
        sqlx::query(
            "INSERT INTO stores (name, location_kind, host, port, database, \
             credential_kind, credential_ref, created_at) \
             VALUES ('store-b', 'host', $1, $2, $3, 'env', $4, now())",
        )
        .bind(options.get_host())
        .bind(i32::from(options.get_port()))
        .bind(&database)
        .bind(crate::db::pg::STORE_PASSWORD_VAR)
        .execute(&db.pool)
        .await
        .unwrap();
        let store_b = StoreName::parse("store-b").unwrap();
        let stores = Stores::new(
            db.pool.clone(),
            PoolSettings::from_config(&crate::config::Config::default()),
        );
        for (name, pin) in [("on-main", None), ("on-b", Some(&store_b))] {
            crate::store::teams::set_store(
                &db.pool,
                crate::db::TEST_TEAM,
                pin,
                &AuditActor::cli(),
                chrono::Utc::now(),
            )
            .await
            .unwrap();
            let new = catalogs::NewCatalog {
                name,
                display_name: &DisplayName::default(),
                description: "",
                owner_team: crate::db::TEST_TEAM,
            };
            let created =
                catalogs::create_in(&stores, &new, &AuditActor::cli(), chrono::Utc::now())
                    .await
                    .unwrap();
            let placed: String = sqlx::query_scalar("SELECT store FROM catalogs WHERE id = $1")
                .bind(created.id)
                .fetch_one(&db.pool)
                .await
                .unwrap();
            let handle = stores
                .get(&StoreName::parse(&placed).unwrap())
                .await
                .unwrap();
            for statement in [
                "DELETE FROM catalog.revisions WHERE catalog_id = $1",
                "DELETE FROM catalog.documents WHERE catalog_id = $1",
            ] {
                sqlx::query(statement)
                    .bind(created.id)
                    .execute(handle.executor())
                    .await
                    .unwrap();
            }
        }
        // Act
        run(
            cli(
                Some(db.url.clone()),
                Command::Catalog(CatalogCommand::RepairIndexes {
                    names: Vec::new(),
                    missing_only: true,
                    dry_run: false,
                    actor: "okf-mcp/repair-indexes".to_string(),
                }),
            ),
            fixed_now(),
        )
        .await
        .unwrap();

        // Assert: each store holds the root index of its own catalog only.
        for (store, name) in [(StoreName::main(), "on-main"), (store_b, "on-b")] {
            let held: Vec<i64> = sqlx::query_scalar(
                "SELECT DISTINCT catalog_id FROM catalog.documents WHERE path = 'index.md'",
            )
            .fetch_all(stores.get(&store).await.unwrap().executor())
            .await
            .unwrap();
            assert_eq!(
                held,
                [crate::db::catalog_id(&db.pool, name).await],
                "{store}"
            );
        }
        stores.forget_all().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_targeted_repair_never_opens_a_store_holding_none_of_the_names() {
        // Arrange: `on-main` without a root index, and a registered store
        // that nothing listens on (a closed port).
        let db = crate::db::test_db().await;
        sqlx::query(
            "INSERT INTO stores (name, location_kind, host, port, database, \
             credential_kind, credential_ref, created_at) \
             VALUES ('store-down', 'host', 'localhost', 1, 'down', 'env', $1, now())",
        )
        .bind(crate::db::pg::STORE_PASSWORD_VAR)
        .execute(&db.pool)
        .await
        .unwrap();
        let stores = Stores::new(
            db.pool.clone(),
            PoolSettings::from_config(&crate::config::Config::default()),
        );
        let new = catalogs::NewCatalog {
            name: "on-main",
            display_name: &DisplayName::default(),
            description: "",
            owner_team: crate::db::TEST_TEAM,
        };
        let created = catalogs::create_in(&stores, &new, &AuditActor::cli(), chrono::Utc::now())
            .await
            .unwrap();
        for statement in [
            "DELETE FROM catalog.revisions WHERE catalog_id = $1",
            "DELETE FROM catalog.documents WHERE catalog_id = $1",
        ] {
            sqlx::query(statement)
                .bind(created.id)
                .execute(&crate::db::store_pool(&db.pool))
                .await
                .unwrap();
        }

        // Act: repair the one named catalog.
        let outcome = run(
            cli(
                Some(db.url.clone()),
                Command::Catalog(CatalogCommand::RepairIndexes {
                    names: vec!["on-main".to_string()],
                    missing_only: false,
                    dry_run: false,
                    actor: "okf-mcp/repair-indexes".to_string(),
                }),
            ),
            fixed_now(),
        )
        .await;

        // Assert: it succeeded without the down store, and wrote the index.
        outcome.unwrap();
        let written: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM catalog.documents WHERE catalog_id = $1 AND path = 'index.md'",
        )
        .bind(created.id)
        .fetch_one(&crate::db::store_pool(&db.pool))
        .await
        .unwrap();
        assert_eq!(written, 1);
        stores.forget_all().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unavailable_store_does_not_stop_the_repair_of_the_stores_after_it() {
        // Arrange: `a-down` (sorts before `main`) on a closed port, and
        // `on-main` without a root index.
        let db = crate::db::test_db().await;
        sqlx::query(
            "INSERT INTO stores (name, location_kind, host, port, database, \
             credential_kind, credential_ref, created_at) \
             VALUES ('a-down', 'host', 'localhost', 1, 'down', 'env', $1, now())",
        )
        .bind(crate::db::pg::STORE_PASSWORD_VAR)
        .execute(&db.pool)
        .await
        .unwrap();
        let stores = Stores::new(
            db.pool.clone(),
            PoolSettings::from_config(&crate::config::Config::default()),
        );
        let new = catalogs::NewCatalog {
            name: "on-main",
            display_name: &DisplayName::default(),
            description: "",
            owner_team: crate::db::TEST_TEAM,
        };
        let created = catalogs::create_in(&stores, &new, &AuditActor::cli(), chrono::Utc::now())
            .await
            .unwrap();
        for statement in [
            "DELETE FROM catalog.revisions WHERE catalog_id = $1",
            "DELETE FROM catalog.documents WHERE catalog_id = $1",
        ] {
            sqlx::query(statement)
                .bind(created.id)
                .execute(&crate::db::store_pool(&db.pool))
                .await
                .unwrap();
        }

        // Act: repair every catalog without a root index.
        let outcome = run(
            cli(
                Some(db.url.clone()),
                Command::Catalog(CatalogCommand::RepairIndexes {
                    names: Vec::new(),
                    missing_only: true,
                    dry_run: false,
                    actor: "okf-mcp/repair-indexes".to_string(),
                }),
            ),
            fixed_now(),
        )
        .await;

        // Assert: the command reports the down store, and `main` was still repaired.
        let error = format!("{:#}", outcome.unwrap_err());
        assert!(error.contains("store a-down"), "{error}");
        assert!(!error.contains("store main"), "{error}");
        let written: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM catalog.documents WHERE catalog_id = $1 AND path = 'index.md'",
        )
        .bind(created.id)
        .fetch_one(&crate::db::store_pool(&db.pool))
        .await
        .unwrap();
        assert_eq!(written, 1);
        stores.forget_all().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn import_reads_a_directory() {
        let db = crate::db::test_db().await;
        let clock = fixed_now();
        let create = CatalogCommand::Create {
            name: "pg".to_string(),
            display_name: String::new(),
            description: String::new(),
            owner: crate::db::TEST_TEAM.to_string(),
        };
        run(
            cli(Some(db.url.clone()), Command::Catalog(create)),
            Arc::clone(&clock),
        )
        .await
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.md"), "# Index\n").unwrap();
        let import = Command::Import {
            catalog: "pg".to_string(),
            source: dir.path().to_path_buf(),
            replace: false,
            only_machinery: false,
            expect_generation: None,
            actor: "test".to_string(),
        };

        run(cli(Some(db.url.clone()), import), Arc::clone(&clock))
            .await
            .unwrap();

        // Live documents: the starter log.md the import dropped is a tombstone.
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT path, updated_by FROM catalog.documents WHERE deleted_at IS NULL",
        )
        .fetch_all(&crate::db::store_pool(&db.pool))
        .await
        .unwrap();
        assert_eq!(rows, [("index.md".to_string(), "test".to_string())]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn import_reads_a_tarball_file_and_explains_a_missing_one() {
        let db = crate::db::test_db().await;
        let clock = fixed_now();
        let create = CatalogCommand::Create {
            name: "pg".to_string(),
            display_name: String::new(),
            description: String::new(),
            owner: crate::db::TEST_TEAM.to_string(),
        };
        run(
            cli(Some(db.url.clone()), Command::Catalog(create)),
            Arc::clone(&clock),
        )
        .await
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let tarball = dir.path().join("pg.tar.gz");
        let script = archive::ExportFile {
            path: "tools/run.sh",
            content: b"#!/bin/sh\n",
            executable: true,
            modified: clock.now(),
        };
        archive::write_tarball(File::create(&tarball).unwrap(), &[script]).unwrap();
        let import = |source: PathBuf| Command::Import {
            catalog: "pg".to_string(),
            source,
            replace: false,
            only_machinery: false,
            expect_generation: None,
            actor: "test".to_string(),
        };

        run(
            cli(Some(db.url.clone()), import(tarball)),
            Arc::clone(&clock),
        )
        .await
        .unwrap();
        let missing = dir.path().join("missing.tar.gz");
        let error = run(
            cli(Some(db.url.clone()), import(missing.clone())),
            Arc::clone(&clock),
        )
        .await
        .unwrap_err();

        // Live documents: the starter files the import dropped are tombstones.
        let rows: Vec<(String, bool)> = sqlx::query_as(
            "SELECT path, executable FROM catalog.documents WHERE deleted_at IS NULL",
        )
        .fetch_all(&crate::db::store_pool(&db.pool))
        .await
        .unwrap();
        assert_eq!(rows, [("tools/run.sh".to_string(), true)]);
        assert!(error
            .to_string()
            .contains(&format!("open {}", missing.display())));
    }

    #[tokio::test]
    async fn import_checks_the_database_before_reading_the_source() {
        // A source that does not exist: reading it first would report "open".
        let import = Command::Import {
            catalog: "pg".to_string(),
            source: PathBuf::from("/nonexistent-okf-import.tar.gz"),
            replace: false,
            only_machinery: false,
            expect_generation: None,
            actor: "test".to_string(),
        };

        let error = run(cli(None, import), fixed_now()).await.unwrap_err();

        assert!(error.to_string().contains("DATABASE_URL"));
    }

    #[test]
    fn export_parses() {
        let parsed = Cli::try_parse_from(["okf-mcp", "export", "pg", "-"]).unwrap();

        assert!(matches!(
            parsed.command,
            Some(Command::Export { ref catalog, ref output })
                if catalog == "pg" && output.as_os_str() == "-"
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn export_writes_a_tarball_file_and_none_for_an_unknown_catalog() {
        let db = crate::db::test_db().await;
        let clock = fixed_now();
        let create = CatalogCommand::Create {
            name: "pg".to_string(),
            display_name: String::new(),
            description: String::new(),
            owner: crate::db::TEST_TEAM.to_string(),
        };
        run(
            cli(Some(db.url.clone()), Command::Catalog(create)),
            Arc::clone(&clock),
        )
        .await
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.md"), "# Index\n").unwrap();
        let import = Command::Import {
            catalog: "pg".to_string(),
            source: dir.path().to_path_buf(),
            replace: false,
            only_machinery: false,
            expect_generation: None,
            actor: "test".to_string(),
        };
        run(cli(Some(db.url.clone()), import), Arc::clone(&clock))
            .await
            .unwrap();
        let export = |catalog: &str, output: PathBuf| Command::Export {
            catalog: catalog.to_string(),
            output,
        };
        let written = dir.path().join("pg.tar.gz");
        let unwritten = dir.path().join("nope.tar.gz");

        run(
            cli(Some(db.url.clone()), export("pg", written.clone())),
            Arc::clone(&clock),
        )
        .await
        .unwrap();
        let unknown = run(
            cli(Some(db.url.clone()), export("nope", unwritten.clone())),
            Arc::clone(&clock),
        )
        .await;

        let files = archive::read_tarball(File::open(&written).unwrap(), u64::MAX).unwrap();
        assert_eq!(files.keys().collect::<Vec<_>>(), ["index.md"]);
        assert_eq!(files["index.md"].content, b"# Index\n");
        assert!(unknown.is_err());
        assert!(!unwritten.exists());
        // The temp file was renamed into place: nothing else is left behind.
        let mut names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names, ["index.md", "pg.tar.gz"]);
    }

    #[tokio::test]
    async fn a_missing_database_url_is_explained() {
        let error = run(cli(None, Command::Migrate), fixed_now())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("DATABASE_URL"));
    }

    #[tokio::test]
    async fn serve_listen_overlapping_the_metrics_listener_is_refused() {
        let mut config = Config::default();
        config.telemetry.prometheus = Some(crate::telemetry::config::PrometheusConfig {
            listen: "127.0.0.1:9090".parse().unwrap(),
        });
        let args = ServeArgs {
            listen: Some("127.0.0.1:9090".parse().unwrap()),
        };

        // No database is given: the overlap is refused before any connection.
        let error = serve(None, config, args, fixed_now(), None)
            .await
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("telemetry.prometheus.listen overlaps listen"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn serve_without_a_database_is_explained() {
        let serve = Command::Serve(ServeArgs::default());

        let error = run(cli(None, serve), fixed_now()).await.unwrap_err();

        assert!(error.to_string().contains("DATABASE_URL"));
    }
    #[test]
    fn key_create_provisioner_takes_no_grants_or_access() {
        let ok = Cli::try_parse_from([
            "okf-mcp",
            "key",
            "create",
            "signup",
            "--provisioner",
            "--user",
            "alice",
        ]);
        let with_catalogs = Cli::try_parse_from([
            "okf-mcp",
            "key",
            "create",
            "signup",
            "--provisioner",
            "--catalogs",
            "main",
            "--user",
            "alice",
        ]);
        let with_access = Cli::try_parse_from([
            "okf-mcp",
            "key",
            "create",
            "signup",
            "--provisioner",
            "--access",
            "admin",
            "--user",
            "alice",
        ]);
        let plain_without_access =
            Cli::try_parse_from(["okf-mcp", "key", "create", "k", "--all", "--user", "alice"]);

        assert!(ok.is_ok());
        assert!(with_catalogs.is_err() && with_access.is_err());
        assert!(
            plain_without_access.is_err(),
            "--access is still required without --provisioner"
        );
    }

    /// Runs `key create k` (no `[redis]`, so nothing is announced).
    async fn created_key_config(db: &crate::db::pg_support::TestDb, clock: &Arc<dyn Clock>) {
        let create = KeyCommand::Create(KeyCreateArgs {
            name: "k".to_string(),
            access: Some(Access::Read),
            provisioner: false,
            maintenance_report: false,
            catalogs: Vec::new(),
            all: true,
            default: None,
            actor: None,
            user: crate::db::TEST_USER.to_string(),
        });
        run(
            cli(Some(db.url.clone()), Command::Key(create)),
            Arc::clone(clock),
        )
        .await
        .unwrap();
    }

    fn revoke_cli(db: &crate::db::pg_support::TestDb) -> Cli {
        cli(
            Some(db.url.clone()),
            Command::Key(KeyCommand::Revoke {
                id: "k".to_string(),
                actor: None,
            }),
        )
    }

    /// Runs `key revoke` with `[redis]` on `server` and checks nothing is
    /// published on the catalog channel: an administrative change reaches
    /// the serving instances through the change feed, not through Redis.
    async fn revoke_announces_nothing(server: crate::db::redis_support::TestRedis) {
        use crate::shared::{announce::CATALOG_CHANNEL, SubscriptionEvent};
        const WAIT: Duration = Duration::from_secs(10);

        let db = crate::db::test_db().await;
        let clock = fixed_now();
        created_key_config(&db, &clock).await;
        let redis = crate::shared::testing::connect(&server).await;
        let mut subscription = redis
            .subscribe(&redis.channel(CATALOG_CHANNEL))
            .await
            .unwrap();
        let first = tokio::time::timeout(WAIT, subscription.next())
            .await
            .expect("the subscription confirms");
        assert!(matches!(first, Some(SubscriptionEvent::Subscribed)));
        let mut config = Config::default();
        config.redis = Some(crate::shared::testing::config(&server));

        dispatch(revoke_cli(&db), config, None, clock, &server.env())
            .await
            .unwrap();

        let more = tokio::time::timeout(Duration::from_millis(300), subscription.next()).await;
        assert!(more.is_err(), "an announcement arrived");
    }

    /// `okf-mcp import` on a database two replicas serve announces the
    /// catalog's new generation: B refreshes its snapshot without a poll
    /// (issue #659). B's subscriber handles events in order, so once B has
    /// the first import (through its announcement or the catch-up on
    /// subscribing), the second can reach it only through its announcement.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_import_on_the_command_line_is_announced_to_the_serving_replicas() {
        const WAIT: Duration = Duration::from_secs(10);
        let (first, b, server) = tokio::time::timeout(
            Duration::from_secs(120),
            crate::mcp::testing::replica_pair(
                &[("pg", &[("index.md", "# PG\n")])],
                Config::default(),
            ),
        )
        .await
        .expect("starting two replicas within 120 s");
        let (shutdown, listener) = crate::shared::testing::listening_for_catalogs(&b).await;
        let clock = fixed_now();
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.redis = Some(crate::shared::testing::config(&server));
        // Whether `app` serves `pg` with an `index.md` ending in `text`.
        async fn holds(app: &App, text: &str) -> bool {
            app.catalog("pg")
                .await
                .unwrap()
                .and_then(|state| state.current().workspace().stored("index.md"))
                .is_some_and(|file| file.content.ends_with(text.as_bytes()))
        }
        let mut reached = Vec::new();

        for text in ["First.\n", "Imported.\n"] {
            let tarball = dir.path().join("pg.tar.gz");
            let content = format!("# PG\n\n{text}");
            let index = archive::ExportFile {
                path: "index.md",
                content: content.as_bytes(),
                executable: false,
                modified: clock.now(),
            };
            archive::write_tarball(File::create(&tarball).unwrap(), &[index]).unwrap();
            let import = Command::Import {
                catalog: "pg".to_string(),
                source: tarball,
                replace: true,
                only_machinery: false,
                expect_generation: None,
                actor: "test".to_string(),
            };
            dispatch(
                cli(Some(first.db_url().to_string()), import),
                config.clone(),
                None,
                Arc::clone(&clock),
                &server.env(),
            )
            .await
            .unwrap();
            let refreshed = tokio::time::timeout(WAIT, async {
                while !holds(&b, text).await {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await;
            reached.push(refreshed.is_ok());
        }
        shutdown.cancel();
        tokio::time::timeout(WAIT, listener)
            .await
            .expect("the subscriber stops")
            .expect("the subscriber's task");

        assert_eq!(reached, [true, true], "B refreshed after each import");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn key_revoke_announces_nothing_to_a_standalone_redis() {
        revoke_announces_nothing(crate::db::redis_support::TestRedis::start().await).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn key_revoke_announces_nothing_to_a_cluster() {
        revoke_announces_nothing(crate::db::redis_support::TestRedis::start_cluster().await).await;
    }

    /// A catalog change a command committed, to announce.
    const COMMITTED: crate::store::catalogs::CatalogGeneration =
        crate::store::catalogs::CatalogGeneration {
            catalog_id: 1,
            generation: 1,
        };

    // A current-thread runtime, so the thread-local subscriber sees the
    // warning `dispatch` logs.
    #[tokio::test]
    async fn an_unset_variable_only_warns() {
        let mut config = Config::default();
        config.redis = Some(crate::config::RedisConfig {
            url_env: "OKF_TEST_UNSET_REDIS_URL".to_string(),
            key_prefix: "okftest:".to_string(),
            mode: crate::config::RedisMode::Standalone,
        });
        let capture = crate::telemetry::subscriber::capture::Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_ansi(false)
            .finish();
        let guard = crate::test_tracing::set_default(subscriber);

        announce_from_cli(&config, &|_| None, &Metrics::global(), &[COMMITTED]).await;

        drop(guard);
        let logged = capture.text();
        assert!(logged.contains("WARN"), "{logged}");
        assert!(logged.contains("announcement skipped"), "{logged}");
        assert!(!logged.contains("redis://"), "{logged}");
    }

    #[tokio::test]
    async fn an_unreachable_redis_only_warns() {
        let mut config = Config::default();
        config.redis = Some(crate::config::RedisConfig {
            url_env: "OKF_TEST_DEAD_REDIS_URL".to_string(),
            key_prefix: "okftest:".to_string(),
            mode: crate::config::RedisMode::Standalone,
        });
        // A held, unlistened loopback port: refused at once.
        let refusing = crate::test_net::Refused::new();
        let url = format!("redis://user:sekrit-7f3a@{}/0", refusing.address());
        let env = |_: &str| Some(url.clone());
        let capture = crate::telemetry::subscriber::capture::Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_ansi(false)
            .finish();
        let guard = crate::test_tracing::set_default(subscriber);

        crate::test_net::bounded(
            "the announcement to a refusing redis",
            announce_from_cli(&config, &env, &Metrics::global(), &[COMMITTED]),
        )
        .await;

        drop(guard);
        let logged = capture.text();
        assert!(logged.contains("announcement skipped"), "{logged}");
        assert!(!logged.contains("sekrit-7f3a"), "{logged}");
    }
}
