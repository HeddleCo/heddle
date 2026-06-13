// SPDX-License-Identifier: Apache-2.0
//! Machine-readable command catalog.

use std::sync::OnceLock;

use anyhow::Result;
use clap::{ArgAction, CommandFactory};
use schemars::JsonSchema;
use serde::Serialize;

#[cfg(feature = "semantic")]
use crate::cli::SemanticCommands;
use crate::cli::{
    ActorCommands, AgentCommands, Cli, Commands, ContextCommands, DaemonCommands, DoctorCommands,
    HookCommands, IntegrationCommands, MaintenanceCommands, MarkerCommands, PurgeCommands,
    RedactCommands, RedactTrustCommands, RemoteCommands, SessionCommands, ShellCommands,
    StackCommands, StashCommands, ThreadCommands, VisibilityCommands, WorkspaceCommands,
    cli_args::{
        CommandCatalogArgs, ConflictCommands, DiscussCommands, ReviewCommands, TransactionCommands,
    },
    render::{shell_quote, write_json_stdout, write_stdout},
    should_output_json, style,
};
#[cfg(feature = "client")]
use crate::cli::{AuthCommands, PresenceCommands, SupportCommands};
#[cfg(feature = "git-overlay")]
use crate::cli::{BridgeCommands, GitCommands};

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CommandCatalogOutput {
    pub kind: String,
    pub executable_path: String,
    pub commands: Vec<CommandCatalogEntry>,
    pub global_options: Vec<CommandCatalogOption>,
    pub json_discriminators: Vec<CommandJsonDiscriminator>,
    pub recommended_action_placeholders: Vec<String>,
    pub recommended_action_templates: Vec<ActionTemplate>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CommandCatalogEntry {
    pub path: Vec<String>,
    pub display: String,
    pub aliases: Vec<String>,
    pub tier: String,
    pub surface: String,
    pub help_visibility: String,
    pub help_rank: u16,
    pub canonical_command: Option<String>,
    pub canonical_action: Option<CanonicalAction>,
    pub command_action: Option<CommandAction>,
    pub summary: String,
    pub has_subcommands: bool,
    pub supports_json: bool,
    pub mutates: bool,
    pub supports_op_id: bool,
    pub persists_op_id: bool,
    pub op_id_behavior: String,
    pub op_id_store_scope: String,
    pub observe_only: bool,
    pub may_initialize: bool,
    pub may_import_git: bool,
    pub may_write_worktree: bool,
    pub may_move_ref: bool,
    pub destructive_requires_force: bool,
    pub writes_heddle_refs: bool,
    pub writes_git_refs: bool,
    pub writes_worktree: bool,
    pub writes_config: bool,
    pub writes_hooks: bool,
    pub network_io: bool,
    pub daemon_process: bool,
    pub object_gc: bool,
    pub external_command: bool,
    pub requires_git_executable: bool,
    pub destructive_data: bool,
    pub side_effects: Vec<String>,
    pub side_effect_class: String,
    pub first_run_behavior: String,
    pub json_kind: String,
    pub json_discriminators: Vec<CommandJsonDiscriminator>,
    pub schema_verbs: Vec<String>,
    pub documented_schema_verbs: Vec<String>,
    pub options: Vec<CommandCatalogOption>,
    pub arguments: Vec<CommandCatalogArgument>,
    /// Sysexits-style codes this command may legitimately return, with a
    /// one-line agent-facing reason. Empty for commands not yet swept. See
    /// `docs/exit-codes.md` for the full taxonomy.
    pub exit_codes: Vec<CommandCatalogExitCode>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CommandCatalogExitCode {
    pub code: u8,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CanonicalAction {
    pub command: String,
    pub kind: String,
    pub executable: bool,
    pub note: String,
    pub argv: Option<Vec<String>>,
    pub template: Option<ActionTemplate>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CommandAction {
    pub action: String,
    pub executable: bool,
    pub argv: Option<Vec<String>>,
    pub template: Option<ActionTemplate>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CommandCatalogOption {
    pub id: String,
    pub long: Option<String>,
    pub aliases: Vec<String>,
    pub short: Option<String>,
    pub value_names: Vec<String>,
    pub value_kind: String,
    pub default_values: Vec<String>,
    pub possible_values: Vec<String>,
    pub help: Option<String>,
    pub required: bool,
    pub global: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CommandCatalogArgument {
    pub id: String,
    pub value_names: Vec<String>,
    pub help: Option<String>,
    pub required: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ActionTemplate {
    pub action: String,
    pub argv_template: Vec<String>,
    pub required_inputs: Vec<String>,
    pub agent_may_fill: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct ActionFields {
    pub action: Option<String>,
    pub template: Option<ActionTemplate>,
}

impl ActionFields {
    pub(crate) fn from_optional_action(action: Option<String>) -> Self {
        let Some(action) = action.filter(|action| !action.trim().is_empty()) else {
            return Self::none();
        };
        validate_recommended_action(&action)
            .unwrap_or_else(|err| panic!("invalid recommended action `{action}`: {err}"));
        Self {
            template: recommended_action_template(&action),
            action: Some(action),
        }
    }

    pub(crate) fn from_optional_action_ref(action: Option<&str>) -> Self {
        Self::from_optional_action(action.map(str::to_string))
    }

    pub(crate) fn from_action(action: &str) -> Self {
        Self::from_optional_action_ref(Some(action))
    }

    pub(crate) fn none() -> Self {
        Self {
            action: None,
            template: None,
        }
    }
}

pub(crate) fn checked_action_from_argv<I, S>(argv: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let action = argv
        .into_iter()
        .map(|arg| shell_quote(arg.as_ref()))
        .collect::<Vec<_>>()
        .join(" ");
    validate_recommended_action(&action)
        .unwrap_or_else(|err| panic!("invalid recommended action `{action}`: {err}"));
    action
}

pub(crate) fn heddle_action<I, S>(args: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let argv = std::iter::once("heddle".to_string())
        .chain(args.into_iter().map(|arg| arg.as_ref().to_string()))
        .collect::<Vec<_>>();
    checked_action_from_argv(argv)
}

/// Build the `--thread <id>` argv fragment for splicing into a [`heddle_action`]
/// argv. A historical / `new_unchecked` thread id that starts with `-` is
/// rendered as the combined `--thread=<id>` token, which clap binds as the
/// flag's value; the plain `--thread`, `<id>` pair would otherwise be re-parsed
/// (after `split_recommended_action`) with `-foo` as another option, and
/// `checked_action_from_argv` would panic. (heddle#464 close-the-class.)
pub(crate) fn thread_flag_args(thread_id: &str) -> Vec<String> {
    if thread_id.starts_with('-') {
        vec![format!("--thread={thread_id}")]
    } else {
        vec!["--thread".to_string(), thread_id.to_string()]
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct CommandJsonDiscriminator {
    pub path: Vec<String>,
    pub display: String,
    pub schema_verb: Option<String>,
    pub field: String,
    pub value: String,
    pub no_schema_reason: Option<String>,
}

impl CommandCatalogOutput {
    pub fn command_by_display(&self, display: &str) -> Option<&CommandCatalogEntry> {
        self.commands.iter().find(|entry| entry.display == display)
    }

    pub fn command_by_path(&self, path: &[String]) -> Option<&CommandCatalogEntry> {
        self.commands.iter().find(|entry| entry.path == path)
    }

    pub fn options_for_display(&self, display: &str) -> Option<Vec<&CommandCatalogOption>> {
        let entry = self.command_by_display(display)?;
        Some(self.options_for_entry(entry))
    }

    pub fn options_for_path(&self, path: &[String]) -> Option<Vec<&CommandCatalogOption>> {
        let entry = self.command_by_path(path)?;
        Some(self.options_for_entry(entry))
    }

    pub fn options_for_entry<'a>(
        &'a self,
        entry: &'a CommandCatalogEntry,
    ) -> Vec<&'a CommandCatalogOption> {
        self.global_options
            .iter()
            .chain(entry.options.iter())
            .collect()
    }
}

#[derive(Debug, Clone, Copy)]
struct CommandContract {
    supports_json: bool,
    supports_json_compact: bool,
    mutates: bool,
    supports_op_id: bool,
    persists_op_id: bool,
    observe_only: bool,
    may_initialize: bool,
    may_import_git: bool,
    may_write_worktree: bool,
    may_move_ref: bool,
    destructive_requires_force: bool,
    writes_heddle_refs: bool,
    writes_git_refs: bool,
    writes_worktree: bool,
    writes_config: bool,
    writes_hooks: bool,
    network_io: bool,
    daemon_process: bool,
    object_gc: bool,
    external_command: bool,
    requires_git_executable: bool,
    destructive_data: bool,
    operator_envelope: bool,
    json_kind: &'static str,
    json_discriminators: &'static [CommandJsonDiscriminatorSpec],
    schema_verbs: &'static [&'static str],
    documented_schema_verbs: &'static [&'static str],
    opaque_schema_verbs: &'static [&'static str],
    surface: &'static str,
    help_visibility: &'static str,
    help_rank: u16,
    /// Area grouping for the `heddle help advanced` listing (heddle#652).
    /// Only meaningful on native-surface root commands with an advanced
    /// help visibility; commands on the `automation` / `admin` /
    /// `git_adapter` surfaces derive their group from the surface itself
    /// (see [`advanced_help_groups`]). The
    /// `advanced_help_groups_cover_every_advanced_verb` test forces
    /// every advanced native root command to pick one, so the grouped
    /// help can never silently grow an uncategorized verb.
    help_category: Option<&'static str>,
    canonical_command: Option<&'static str>,
    canonical_kind: Option<&'static str>,
    canonical_note: Option<&'static str>,
    advertised_action: Option<AdvertisedAction>,
    feature_gate: Option<&'static str>,
    /// Sysexits-style codes this command may legitimately return, paired
    /// with a one-line agent-facing reason. Empty slice means "0 on
    /// success, generic IoErr (74) on failure" — the implicit default for
    /// commands not yet swept. See `docs/exit-codes.md` and
    /// `crates/cli/src/exit.rs::HeddleExitCode`.
    exit_codes: &'static [(u8, &'static str)],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CommandJsonDiscriminatorSpec {
    schema_verb: Option<&'static str>,
    field: &'static str,
    value: &'static str,
    no_schema_reason: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdvertisedAction {
    action: &'static str,
    argv_template: &'static [&'static str],
    required_inputs: &'static [&'static str],
    agent_may_fill: bool,
    executable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRuntimeContract {
    pub path: Vec<&'static str>,
    pub display: String,
    pub supports_json: bool,
    pub supports_json_compact: bool,
    pub supports_op_id: bool,
    pub persists_op_id: bool,
    pub uses_bootstrap_op_id_store: bool,
    pub help_visibility: &'static str,
    pub help_rank: u16,
    pub surface: &'static str,
    pub canonical_command: Option<&'static str>,
    pub json_kind: &'static str,
}

#[derive(Debug, Clone, Copy)]
struct CommandContractEntry {
    path: &'static [&'static str],
    contract: CommandContract,
}

const RECOMMENDED_ACTION_PLACEHOLDERS: &[&str] = &[
    // Message templates are display-only until the caller supplies a
    // real message. They must not be exposed as directly executable
    // argv because the literal ellipsis would create bad history.
    "heddle capture -m \"...\"",
    "heddle capture -m \"...\" --confidence <confidence>",
    "heddle checkpoint -m \"...\"",
    "heddle commit -m \"...\"",
    "heddle commit -m \"...\" --confidence <confidence>",
    "heddle init --principal-name <name> --principal-email <email>",
    "heddle ready -m \"...\"",
    "heddle context get --path <path>",
    "heddle context set --path <path> --scope file -m \"...\"",
    "heddle session start",
    "heddle start <name> --path <empty-path>",
    "heddle start <name> --path ../<name>",
    "heddle actor show <session>",
    "heddle stash push -m \"...\"",
    "heddle thread show <THREAD>",
    // Remote setup requires filling in a real name and URL after
    // inspecting current configuration.
    "heddle remote add <name> <url>",
    "heddle remote set-default <name>",
    // Clone recovery commands must name a real remote and an empty
    // destination path chosen by the operator.
    "heddle clone <local-path> <path>",
    "heddle clone <remote> <path>",
    "heddle clone <remote> <new-path>",
    "heddle clone <remote> <fresh-path>",
    "heddle clone <remote> <path> --thread <thread>",
    // Shallow Git import recovery requires choosing a complete checkout.
    "heddle bridge git import --path <full-git-repo>",
    "heddle bridge git import --path <full-git-repo> --ref <ref>",
    // Detached Git-overlay recovery requires the caller to choose the
    // branch to reattach before retrying branch-writing operations.
    "heddle switch <branch>",
    // Merge recovery placeholders require choosing the source thread.
    "heddle merge <thread> --git-commit",
];

const RECOMMENDED_ACTION_TEMPLATES: &[(&str, &[&str], &[&str], bool)] = &[
    (
        "heddle capture -m \"...\"",
        &["heddle", "capture", "-m", "<message>"],
        &["message"],
        true,
    ),
    (
        "heddle capture -m \"...\" --confidence <confidence>",
        &[
            "heddle",
            "capture",
            "-m",
            "<message>",
            "--confidence",
            "<confidence>",
        ],
        &["message", "confidence"],
        true,
    ),
    (
        "heddle checkpoint -m \"...\"",
        &["heddle", "checkpoint", "-m", "<message>"],
        &["message"],
        true,
    ),
    (
        "heddle commit -m \"...\"",
        &["heddle", "commit", "-m", "<message>"],
        &["message"],
        true,
    ),
    (
        "heddle commit -m \"...\" --confidence <confidence>",
        &[
            "heddle",
            "commit",
            "-m",
            "<message>",
            "--confidence",
            "<confidence>",
        ],
        &["message", "confidence"],
        true,
    ),
    (
        "heddle commit --all -m \"...\"",
        &["heddle", "commit", "--all", "-m", "<message>"],
        &["message"],
        true,
    ),
    (
        "heddle init --principal-name <name> --principal-email <email>",
        &[
            "heddle",
            "init",
            "--principal-name",
            "<name>",
            "--principal-email",
            "<email>",
        ],
        &["name", "email"],
        true,
    ),
    (
        "heddle ready -m \"...\"",
        &["heddle", "ready", "-m", "<message>"],
        &["message"],
        true,
    ),
    (
        "heddle context get --path <path>",
        &["heddle", "context", "get", "--path", "<path>"],
        &["path"],
        true,
    ),
    (
        "heddle context set --path <path> --scope file -m \"...\"",
        &[
            "heddle",
            "context",
            "set",
            "--path",
            "<path>",
            "--scope",
            "file",
            "-m",
            "<message>",
        ],
        &["path", "message"],
        true,
    ),
    (
        "heddle session start",
        &[
            "heddle",
            "session",
            "start",
            "--provider",
            "<provider>",
            "--model",
            "<model>",
        ],
        &["provider", "model"],
        true,
    ),
    (
        "heddle start <name> --path <empty-path>",
        &["heddle", "start", "<name>", "--path", "<empty-path>"],
        &["name", "path"],
        true,
    ),
    (
        "heddle start <name> --path ../<name>",
        &["heddle", "start", "<name>", "--path", "../<name>"],
        &["name", "path"],
        true,
    ),
    (
        "heddle actor show <session>",
        &["heddle", "actor", "show", "<session>"],
        &["session"],
        true,
    ),
    (
        "heddle ready --thread <name>",
        &["heddle", "ready", "--thread", "<thread>"],
        &["thread"],
        true,
    ),
    (
        "heddle land --thread <name>",
        &["heddle", "land", "--thread", "<thread>"],
        &["thread"],
        true,
    ),
    (
        "heddle sync --thread <name>",
        &["heddle", "sync", "--thread", "<thread>"],
        &["thread"],
        true,
    ),
    (
        "heddle run --thread <name> -- <cmd...>",
        &["heddle", "run", "--thread", "<thread>", "--", "<cmd...>"],
        &["thread", "command"],
        false,
    ),
    (
        "heddle thread switch <name>",
        &["heddle", "thread", "switch", "<thread>"],
        &["thread"],
        true,
    ),
    // Current-thread drop recovery (heddle#258): switch to a sibling
    // thread first (or create one) instead of the circular `thread list`.
    // The `<other>` slot is agent-fillable so a JSON caller can run it
    // after picking a real sibling thread name.
    (
        "heddle thread switch <other>",
        &["heddle", "thread", "switch", "<other>"],
        &["other"],
        true,
    ),
    (
        "heddle thread create <other>",
        &["heddle", "thread", "create", "<other>"],
        &["other"],
        true,
    ),
    (
        "heddle thread show <THREAD>",
        &["heddle", "thread", "show", "<thread>"],
        &["thread"],
        true,
    ),
    (
        "heddle delegate --parent <THREAD> <task>",
        &["heddle", "delegate", "--parent", "<thread>", "<task>"],
        &["thread", "task"],
        false,
    ),
    (
        "heddle stash push -m \"...\"",
        &["heddle", "stash", "push", "-m", "<message>"],
        &["message"],
        true,
    ),
    (
        "heddle remote add <name> <url>",
        &["heddle", "remote", "add", "<name>", "<url>"],
        &["name", "url"],
        false,
    ),
    (
        "heddle remote set-default <name>",
        &["heddle", "remote", "set-default", "<name>"],
        &["name"],
        false,
    ),
    (
        "heddle merge <thread> --git-commit",
        &["heddle", "merge", "<thread>", "--git-commit"],
        &["thread"],
        false,
    ),
    (
        "heddle clone <local-path> <path>",
        &["heddle", "clone", "<local-path>", "<path>"],
        &["local_path", "path"],
        false,
    ),
    (
        "heddle clone <remote> <path>",
        &["heddle", "clone", "<remote>", "<path>"],
        &["remote", "path"],
        false,
    ),
    (
        "heddle clone <remote> <new-path>",
        &["heddle", "clone", "<remote>", "<new-path>"],
        &["remote", "path"],
        false,
    ),
    (
        "heddle clone <remote> <path> --thread <thread>",
        &[
            "heddle", "clone", "<remote>", "<path>", "--thread", "<thread>",
        ],
        &["remote", "path", "thread"],
        false,
    ),
    (
        "heddle clone <remote> <fresh-path>",
        &["heddle", "clone", "<remote>", "<fresh-path>"],
        &["remote", "path"],
        false,
    ),
    (
        "heddle bridge git import --path <full-git-repo>",
        &[
            "heddle",
            "bridge",
            "git",
            "import",
            "--path",
            "<full-git-repo>",
        ],
        &["path"],
        false,
    ),
    (
        "heddle bridge git import --path <full-git-repo> --ref <ref>",
        &[
            "heddle",
            "bridge",
            "git",
            "import",
            "--path",
            "<full-git-repo>",
            "--ref",
            "<ref>",
        ],
        &["path", "ref"],
        false,
    ),
    (
        "heddle switch <branch>",
        &["heddle", "switch", "<branch>"],
        &["branch"],
        false,
    ),
];

const READ_JSON: CommandContract = CommandContract {
    supports_json: true,
    supports_json_compact: false,
    mutates: false,
    supports_op_id: false,
    persists_op_id: false,
    observe_only: true,
    may_initialize: false,
    may_import_git: false,
    may_write_worktree: false,
    may_move_ref: false,
    destructive_requires_force: false,
    writes_heddle_refs: false,
    writes_git_refs: false,
    writes_worktree: false,
    writes_config: false,
    writes_hooks: false,
    network_io: false,
    daemon_process: false,
    object_gc: false,
    external_command: false,
    requires_git_executable: false,
    destructive_data: false,
    operator_envelope: false,
    json_kind: "json",
    json_discriminators: &[],
    schema_verbs: &[],
    documented_schema_verbs: &[],
    opaque_schema_verbs: &[],
    surface: "native",
    help_visibility: "advanced",
    help_rank: 1000,
    help_category: None,
    canonical_command: None,
    canonical_kind: None,
    canonical_note: None,
    advertised_action: None,
    feature_gate: None,
    exit_codes: &[],
};

const READ_TEXT: CommandContract = CommandContract {
    supports_json: false,
    json_kind: "none",
    ..READ_JSON
};

const GROUP: CommandContract = CommandContract {
    supports_json: false,
    json_kind: "none",
    ..READ_JSON
};

const READ_JSONL: CommandContract = CommandContract {
    json_kind: "jsonl",
    ..READ_JSON
};

const READ_JSON_OR_JSONL: CommandContract = CommandContract {
    json_kind: "json_or_jsonl",
    ..READ_JSON
};

const MUTATING: CommandContract = CommandContract {
    mutates: true,
    supports_op_id: true,
    observe_only: false,
    may_move_ref: true,
    writes_heddle_refs: true,
    ..READ_JSON
};

const MUTATING_NO_OP_ID: CommandContract = CommandContract {
    supports_op_id: false,
    ..MUTATING
};

const MUTATING_TEXT: CommandContract = CommandContract {
    supports_json: false,
    supports_op_id: false,
    json_kind: "none",
    ..MUTATING
};

const INIT: CommandContract = CommandContract {
    may_initialize: true,
    may_move_ref: false,
    writes_heddle_refs: false,
    writes_config: true,
    ..MUTATING
};

const CAPTURE: CommandContract = CommandContract { ..MUTATING };

const fn compact_json(contract: CommandContract) -> CommandContract {
    CommandContract {
        supports_json_compact: true,
        ..contract
    }
}

const fn operator_envelope(contract: CommandContract) -> CommandContract {
    CommandContract {
        operator_envelope: true,
        ..contract
    }
}

const WORKTREE_MUTATION: CommandContract = CommandContract {
    may_write_worktree: true,
    writes_worktree: true,
    ..MUTATING
};

const WORKTREE_MUTATION_JSONL: CommandContract = CommandContract {
    json_kind: "jsonl",
    ..WORKTREE_MUTATION
};

const WORKTREE_ONLY_MUTATION: CommandContract = CommandContract {
    may_move_ref: false,
    writes_heddle_refs: false,
    ..WORKTREE_MUTATION
};

const DESTRUCTIVE_WORKTREE_MUTATION: CommandContract = CommandContract {
    destructive_requires_force: true,
    destructive_data: true,
    ..WORKTREE_MUTATION
};

const DESTRUCTIVE_WORKTREE_ONLY_MUTATION: CommandContract = CommandContract {
    destructive_requires_force: true,
    destructive_data: true,
    ..WORKTREE_ONLY_MUTATION
};

const DATA_MUTATION: CommandContract = CommandContract {
    may_move_ref: false,
    writes_heddle_refs: false,
    ..MUTATING
};

const DESTRUCTIVE_DATA_MUTATION: CommandContract = CommandContract {
    destructive_data: true,
    ..DATA_MUTATION
};

const IMPORTING_MUTATION: CommandContract = CommandContract {
    may_import_git: true,
    ..MUTATING
};

const ADOPT: CommandContract = CommandContract {
    may_initialize: true,
    may_import_git: true,
    writes_config: true,
    ..MUTATING
};

const CONFIG_MUTATION: CommandContract = CommandContract {
    may_move_ref: false,
    writes_heddle_refs: false,
    writes_config: true,
    ..MUTATING
};

const HOOK_MUTATION: CommandContract = CommandContract {
    writes_hooks: true,
    ..CONFIG_MUTATION
};

const DAEMON_MUTATION: CommandContract = CommandContract {
    may_move_ref: false,
    writes_heddle_refs: false,
    daemon_process: true,
    ..MUTATING_NO_OP_ID
};

const GC_MUTATION: CommandContract = CommandContract {
    may_move_ref: false,
    writes_heddle_refs: false,
    object_gc: true,
    ..MUTATING
};

const EXTERNAL_COMMAND_MUTATION: CommandContract = CommandContract {
    may_move_ref: false,
    writes_heddle_refs: false,
    external_command: true,
    ..MUTATING_TEXT
};

const EXTERNAL_WORKTREE_COMMAND: CommandContract = CommandContract {
    may_write_worktree: true,
    external_command: true,
    ..EXTERNAL_COMMAND_MUTATION
};

const EXTERNAL_WORKTREE_MUTATION: CommandContract = CommandContract {
    external_command: true,
    ..WORKTREE_MUTATION
};

const fn documented_schemas(
    contract: CommandContract,
    schema_verbs: &'static [&'static str],
) -> CommandContract {
    CommandContract {
        schema_verbs,
        documented_schema_verbs: schema_verbs,
        ..contract
    }
}

const fn opaque_schemas(
    contract: CommandContract,
    schema_verbs: &'static [&'static str],
) -> CommandContract {
    CommandContract {
        schema_verbs,
        documented_schema_verbs: schema_verbs,
        opaque_schema_verbs: schema_verbs,
        ..contract
    }
}

const fn json_discriminators(
    contract: CommandContract,
    discriminators: &'static [CommandJsonDiscriminatorSpec],
) -> CommandContract {
    CommandContract {
        json_discriminators: discriminators,
        ..contract
    }
}

const fn json_discriminator(
    schema_verb: Option<&'static str>,
    field: &'static str,
    value: &'static str,
) -> CommandJsonDiscriminatorSpec {
    CommandJsonDiscriminatorSpec {
        schema_verb,
        field,
        value,
        no_schema_reason: None,
    }
}

/// Helper for advertising a JSON discriminator value that is *not*
/// backed by a documented schema verb — used for preliminary /
/// transport-envelope records emitted alongside the primary payload
/// (e.g. `clone_connection` ahead of the final `clone` object on
/// hosted clones). The `reason` is required by the metadata invariant
/// test so the catalog can't carry orphan discriminators.
const fn json_discriminator_no_schema(
    reason: &'static str,
    field: &'static str,
    value: &'static str,
) -> CommandJsonDiscriminatorSpec {
    CommandJsonDiscriminatorSpec {
        schema_verb: None,
        field,
        value,
        no_schema_reason: Some(reason),
    }
}

const fn front_door(contract: CommandContract, help_rank: u16) -> CommandContract {
    CommandContract {
        help_visibility: "everyday",
        help_rank,
        ..contract
    }
}

const fn hidden(contract: CommandContract) -> CommandContract {
    CommandContract {
        surface: "internal",
        help_visibility: "hidden",
        ..contract
    }
}

const fn surface(contract: CommandContract, surface: &'static str) -> CommandContract {
    CommandContract {
        surface,
        ..contract
    }
}

/// Assign the `heddle help advanced` area group for a native-surface
/// advanced root command (heddle#652). See [`advanced_help_groups`] for
/// the recognized ids and their display titles.
const fn category(contract: CommandContract, help_category: &'static str) -> CommandContract {
    CommandContract {
        help_category: Some(help_category),
        ..contract
    }
}

const fn feature_gated(contract: CommandContract, feature_gate: &'static str) -> CommandContract {
    CommandContract {
        feature_gate: Some(feature_gate),
        ..contract
    }
}

const fn exits(
    contract: CommandContract,
    exit_codes: &'static [(u8, &'static str)],
) -> CommandContract {
    CommandContract {
        exit_codes,
        ..contract
    }
}

const fn git_adapter_alias(
    contract: CommandContract,
    canonical_command: &'static str,
) -> CommandContract {
    git_adapter_action(
        contract,
        canonical_command,
        "direct_command",
        "Use this native Heddle command for the same operation.",
    )
}

const fn git_adapter_action(
    contract: CommandContract,
    canonical_command: &'static str,
    canonical_kind: &'static str,
    canonical_note: &'static str,
) -> CommandContract {
    CommandContract {
        surface: "git_adapter",
        help_visibility: "git_adapter",
        canonical_command: Some(canonical_command),
        canonical_kind: Some(canonical_kind),
        canonical_note: Some(canonical_note),
        ..contract
    }
}

const fn advertised_action(
    contract: CommandContract,
    action: &'static str,
    argv_template: &'static [&'static str],
    required_inputs: &'static [&'static str],
    agent_may_fill: bool,
    executable: bool,
) -> CommandContract {
    CommandContract {
        advertised_action: Some(AdvertisedAction {
            action,
            argv_template,
            required_inputs,
            agent_may_fill,
            executable,
        }),
        ..contract
    }
}

const CONTRACTS: &[CommandContractEntry] = &[
    entry(
        &["abort"],
        category(
            json_discriminators(
                documented_schemas(operator_envelope(compact_json(MUTATING)), &["abort"]),
                &[json_discriminator(Some("abort"), "output_kind", "abort")],
            ),
            "recovery",
        ),
    ),
    entry(
        &["adopt"],
        front_door(
            advertised_action(
                json_discriminators(
                    documented_schemas(ADOPT, &["adopt"]),
                    &[json_discriminator(Some("adopt"), "output_kind", "adopt")],
                ),
                "heddle adopt --ref <branch>",
                &["heddle", "adopt", "--ref", "<branch>"],
                &["branch"],
                true,
                false,
            ),
            210,
        ),
    ),
    entry(&["actor"], surface(GROUP, "automation")),
    entry(
        &["actor", "spawn"],
        surface(documented_schemas(MUTATING, &["actor spawn"]), "automation"),
    ),
    entry(
        &["actor", "list"],
        surface(documented_schemas(READ_JSON, &["actor list"]), "automation"),
    ),
    entry(
        &["actor", "show"],
        surface(documented_schemas(READ_JSON, &["actor show"]), "automation"),
    ),
    entry(
        &["actor", "explain"],
        surface(
            documented_schemas(READ_JSON, &["actor explain"]),
            "automation",
        ),
    ),
    entry(
        &["actor", "done"],
        surface(documented_schemas(MUTATING, &["actor done"]), "automation"),
    ),
    entry(&["agent"], surface(GROUP, "automation")),
    entry(
        &["agent", "serve"],
        surface(
            json_discriminators(
                documented_schemas(DAEMON_MUTATION, &["agent serve"]),
                &[json_discriminator(
                    Some("agent serve"),
                    "output_kind",
                    "agent_serve",
                )],
            ),
            "automation",
        ),
    ),
    entry(
        &["agent", "status"],
        surface(
            json_discriminators(
                documented_schemas(READ_JSON, &["agent status"]),
                &[json_discriminator(
                    Some("agent status"),
                    "output_kind",
                    "agent_status",
                )],
            ),
            "automation",
        ),
    ),
    entry(
        &["agent", "stop"],
        surface(
            json_discriminators(
                documented_schemas(DAEMON_MUTATION, &["agent stop"]),
                &[json_discriminator(
                    Some("agent stop"),
                    "output_kind",
                    "agent_stop",
                )],
            ),
            "automation",
        ),
    ),
    entry(
        &["agent", "reserve"],
        surface(
            documented_schemas(MUTATING, &["agent reserve"]),
            "automation",
        ),
    ),
    entry(
        &["agent", "heartbeat"],
        surface(
            documented_schemas(MUTATING, &["agent heartbeat"]),
            "automation",
        ),
    ),
    entry(
        &["agent", "capture"],
        surface(
            json_discriminators(
                documented_schemas(CAPTURE, &["agent capture"]),
                &[json_discriminator(
                    Some("agent capture"),
                    "output_kind",
                    "capture",
                )],
            ),
            "automation",
        ),
    ),
    entry(
        &["agent", "ready"],
        surface(
            json_discriminators(
                documented_schemas(CAPTURE, &["agent ready"]),
                &[json_discriminator(
                    Some("agent ready"),
                    "output_kind",
                    "ready",
                )],
            ),
            "automation",
        ),
    ),
    entry(
        &["agent", "release"],
        surface(
            documented_schemas(MUTATING, &["agent release"]),
            "automation",
        ),
    ),
    entry(
        &["agent", "list"],
        surface(documented_schemas(READ_JSON, &["agent list"]), "automation"),
    ),
    entry(
        &["attempt"],
        category(
            documented_schemas(EXTERNAL_WORKTREE_MUTATION, &["attempt"]),
            "threads",
        ),
    ),
    #[cfg(feature = "client")]
    entry(&["auth"], category(GROUP, "repo")),
    #[cfg(feature = "client")]
    entry(&["auth", "login"], MUTATING_TEXT),
    #[cfg(feature = "client")]
    entry(&["auth", "logout"], MUTATING_NO_OP_ID),
    #[cfg(feature = "client")]
    entry(&["auth", "status"], READ_JSON),
    #[cfg(feature = "client")]
    entry(&["auth", "create-service-token"], MUTATING_NO_OP_ID),
    entry(
        &["blame"],
        category(
            json_discriminators(
                documented_schemas(READ_JSON, &["blame"]),
                &[json_discriminator(Some("blame"), "output_kind", "blame")],
            ),
            "states",
        ),
    ),
    entry(
        &["branch"],
        git_adapter_action(
            json_discriminators(
                documented_schemas(MUTATING, &["branch"]),
                &[
                    // No-arg `branch` emits the thread-list contract; this
                    // is the shape the registered `branch` schema mirrors.
                    json_discriminator(Some("branch"), "output_kind", "thread_list"),
                    // `branch <name>` (create/rename/delete) delegates to
                    // the thread family and emits the delegate's record
                    // (e.g. `thread_create`). Advertised without a schema
                    // verb — only the listing shape is schema-backed —
                    // mirroring how hosted `clone` advertises its
                    // preliminary `clone_connection` envelope.
                    json_discriminator_no_schema(
                        "branch mutations delegate to the thread family; the \
                         registered `branch` schema mirrors only the no-arg \
                         listing shape",
                        "output_kind",
                        "thread_create",
                    ),
                    json_discriminator_no_schema(
                        "branch -m delegates to `thread rename`; the registered \
                         `branch` schema mirrors only the no-arg listing shape",
                        "output_kind",
                        "thread_rename",
                    ),
                    json_discriminator_no_schema(
                        "branch -d delegates to `thread drop`; the registered \
                         `branch` schema mirrors only the no-arg listing shape",
                        "output_kind",
                        "thread_drop",
                    ),
                ],
            ),
            "thread",
            "command_family",
            "Use the thread command family for branch listing, creation, rename, and deletion.",
        ),
    ),
    entry(&["bridge"], surface(GROUP, "git_adapter")),
    entry(&["bridge", "git"], surface(GROUP, "git_adapter")),
    entry(
        &["bridge", "git", "status"],
        git_adapter_alias(
            json_discriminators(
                documented_schemas(READ_JSON, &["bridge git status"]),
                &[json_discriminator(
                    Some("bridge git status"),
                    "output_kind",
                    "bridge_git_status",
                )],
            ),
            "status",
        ),
    ),
    entry(
        &["bridge", "git", "init"],
        git_adapter_alias(documented_schemas(INIT, &["bridge git init"]), "init"),
    ),
    entry(
        &["bridge", "git", "export"],
        git_adapter_alias(
            documented_schemas(
                CommandContract {
                    writes_git_refs: true,
                    ..MUTATING
                },
                &["bridge git export"],
            ),
            "push",
        ),
    ),
    entry(
        &["bridge", "git", "import"],
        exits(
            git_adapter_action(
                json_discriminators(
                    documented_schemas(IMPORTING_MUTATION, &["bridge git import"]),
                    &[json_discriminator(
                        Some("bridge git import"),
                        "output_kind",
                        "bridge_git_import",
                    )],
                ),
                "adopt",
                "workflow",
                "Use adopt for the guided Git-to-Heddle conversion workflow.",
            ),
            &[
                (0, "ok"),
                (65, "malformed git repo or unimportable refs"),
                (74, "io reading git refs"),
            ],
        ),
    ),
    entry(
        &["bridge", "git", "sync"],
        exits(
            git_adapter_action(
                json_discriminators(
                    documented_schemas(IMPORTING_MUTATION, &["bridge git sync"]),
                    &[json_discriminator(
                        Some("bridge git sync"),
                        "output_kind",
                        "bridge_git_sync",
                    )],
                ),
                "adopt",
                "workflow",
                "Use adopt for the guided Git-to-Heddle conversion workflow.",
            ),
            &[
                (0, "ok"),
                (75, "remote unreachable; safe to retry"),
                (76, "remote rejected payload"),
            ],
        ),
    ),
    entry(
        &["bridge", "git", "reconcile"],
        exits(
            git_adapter_action(
                json_discriminators(
                    documented_schemas(IMPORTING_MUTATION, &["bridge git reconcile"]),
                    &[json_discriminator(
                        Some("bridge git reconcile"),
                        "output_kind",
                        "bridge_git_reconcile",
                    )],
                ),
                "adopt",
                "workflow",
                "Use adopt for the guided Git-to-Heddle conversion workflow.",
            ),
            &[
                (0, "ok"),
                (65, "unmergeable divergence; manual resolution required"),
            ],
        ),
    ),
    entry(
        &["bridge", "git", "push"],
        git_adapter_alias(
            json_discriminators(
                documented_schemas(
                    CommandContract {
                        writes_heddle_refs: false,
                        writes_git_refs: true,
                        network_io: true,
                        ..MUTATING
                    },
                    &["bridge git push"],
                ),
                &[json_discriminator(
                    Some("bridge git push"),
                    "output_kind",
                    "bridge_git_push",
                )],
            ),
            "push",
        ),
    ),
    entry(
        &["bridge", "git", "pull"],
        git_adapter_alias(
            json_discriminators(
                documented_schemas(
                    CommandContract {
                        writes_git_refs: true,
                        network_io: true,
                        ..WORKTREE_MUTATION
                    },
                    &["bridge git pull"],
                ),
                &[json_discriminator(
                    Some("bridge git pull"),
                    "output_kind",
                    "bridge_git_pull",
                )],
            ),
            "pull",
        ),
    ),
    entry(
        &["bridge", "git", "ingest"],
        surface(
            opaque_schemas(IMPORTING_MUTATION, &["bridge git ingest"]),
            "git_adapter",
        ),
    ),
    entry(
        &["bridge", "git", "reason"],
        surface(
            opaque_schemas(DATA_MUTATION, &["bridge git reason"]),
            "git_adapter",
        ),
    ),
    entry(
        &["bridge", "backfill-fidelity"],
        surface(
            json_discriminators(
                opaque_schemas(DATA_MUTATION, &["bridge backfill-fidelity"]),
                &[json_discriminator(
                    Some("bridge backfill-fidelity"),
                    "output_kind",
                    "bridge_backfill_fidelity",
                )],
            ),
            "git_adapter",
        ),
    ),
    entry(
        &["capture"],
        category(
            json_discriminators(
                documented_schemas(compact_json(CAPTURE), &["capture"]),
                &[json_discriminator(
                    Some("capture"),
                    "output_kind",
                    "capture",
                )],
            ),
            "states",
        ),
    ),
    entry(
        &["checkpoint"],
        category(
            json_discriminators(
                documented_schemas(
                    CommandContract {
                        writes_git_refs: true,
                        ..CAPTURE
                    },
                    &["checkpoint"],
                ),
                &[json_discriminator(
                    Some("checkpoint"),
                    "output_kind",
                    "checkpoint",
                )],
            ),
            "states",
        ),
    ),
    entry(
        &["cherry-pick"],
        category(
            json_discriminators(
                opaque_schemas(WORKTREE_MUTATION, &["cherry-pick"]),
                &[json_discriminator(
                    Some("cherry-pick"),
                    "output_kind",
                    "cherry_pick",
                )],
            ),
            "states",
        ),
    ),
    entry(
        &["clean"],
        category(
            json_discriminators(
                documented_schemas(DESTRUCTIVE_WORKTREE_ONLY_MUTATION, &["clean"]),
                &[json_discriminator(Some("clean"), "output_kind", "clean")],
            ),
            "recovery",
        ),
    ),
    entry(
        &["clone"],
        front_door(
            json_discriminators(
                documented_schemas(
                    CommandContract {
                        may_initialize: true,
                        may_write_worktree: true,
                        may_move_ref: true,
                        writes_worktree: true,
                        network_io: true,
                        ..MUTATING
                    },
                    &["clone"],
                ),
                &[
                    json_discriminator(Some("clone"), "output_kind", "clone"),
                    // `clone --output json` on a hosted/network remote
                    // emits a preliminary connection envelope before the
                    // final clone payload. Both records carry
                    // `output_kind` so agents that route on the
                    // discriminator (per heddle#272) can classify each
                    // line without falling back to text parsing. The
                    // envelope has no separate schema verb — it's a
                    // small inline object, not a Serialize struct in
                    // `schemas`. Source-of-truth value:
                    // `cli::cli::commands::CLONE_CONNECTION_OUTPUT_KIND`.
                    json_discriminator_no_schema(
                        "preliminary connection envelope emitted by hosted clones \
                         before the final clone payload (no separate schema)",
                        "output_kind",
                        "clone_connection",
                    ),
                ],
            ),
            220,
        ),
    ),
    entry(
        &["collapse"],
        category(opaque_schemas(MUTATING, &["collapse"]), "states"),
    ),
    entry(
        &["commit"],
        exits(
            front_door(
                advertised_action(
                    json_discriminators(
                        documented_schemas(
                            CommandContract {
                                writes_git_refs: true,
                                ..CAPTURE
                            },
                            &["commit"],
                        ),
                        &[json_discriminator(Some("commit"), "output_kind", "commit")],
                    ),
                    "heddle commit -m <message>",
                    &["heddle", "commit", "-m", "<message>"],
                    &["message"],
                    true,
                    false,
                ),
                30,
            ),
            &[
                (0, "ok"),
                (65, "dirty worktree refused or unmergeable input"),
                (74, "io while writing state"),
            ],
        ),
    ),
    entry(
        &["commands"],
        surface(
            json_discriminators(
                documented_schemas(READ_JSON, &["commands"]),
                &[json_discriminator(
                    Some("commands"),
                    "kind",
                    "command_catalog",
                )],
            ),
            "automation",
        ),
    ),
    entry(&["conflict"], category(GROUP, "recovery")),
    entry(
        &["conflict", "list"],
        opaque_schemas(READ_JSON, &["conflict list"]),
    ),
    entry(
        &["conflict", "show"],
        json_discriminators(
            opaque_schemas(READ_JSON, &["conflict show"]),
            &[json_discriminator(
                Some("conflict show"),
                "output_kind",
                "conflict_show",
            )],
        ),
    ),
    entry(
        &["continue"],
        category(
            json_discriminators(
                documented_schemas(operator_envelope(compact_json(MUTATING)), &["continue"]),
                &[json_discriminator(
                    Some("continue"),
                    "output_kind",
                    "continue",
                )],
            ),
            "recovery",
        ),
    ),
    entry(&["context"], category(GROUP, "collab")),
    entry(
        &["context", "set"],
        json_discriminators(
            opaque_schemas(MUTATING, &["context set"]),
            &[json_discriminator(
                Some("context set"),
                "output_kind",
                "context_set",
            )],
        ),
    ),
    entry(
        &["context", "get"],
        json_discriminators(
            opaque_schemas(READ_JSON, &["context get"]),
            &[json_discriminator(
                Some("context get"),
                "output_kind",
                "context_get",
            )],
        ),
    ),
    entry(
        &["context", "list"],
        json_discriminators(
            opaque_schemas(READ_JSON, &["context list"]),
            &[json_discriminator(
                Some("context list"),
                "output_kind",
                "context_list",
            )],
        ),
    ),
    entry(
        &["context", "history"],
        json_discriminators(
            opaque_schemas(READ_JSON, &["context history"]),
            &[json_discriminator(
                Some("context history"),
                "output_kind",
                "context_history",
            )],
        ),
    ),
    entry(
        &["context", "edit"],
        json_discriminators(
            opaque_schemas(MUTATING, &["context edit"]),
            &[json_discriminator(
                Some("context edit"),
                "output_kind",
                "context_edit",
            )],
        ),
    ),
    entry(
        &["context", "supersede"],
        json_discriminators(
            opaque_schemas(MUTATING, &["context supersede"]),
            &[json_discriminator(
                Some("context supersede"),
                "output_kind",
                "context_supersede",
            )],
        ),
    ),
    entry(
        &["context", "rm"],
        json_discriminators(
            opaque_schemas(MUTATING, &["context rm"]),
            &[json_discriminator(
                Some("context rm"),
                "output_kind",
                "context_rm",
            )],
        ),
    ),
    entry(
        &["context", "check"],
        json_discriminators(
            opaque_schemas(READ_JSON, &["context check"]),
            &[json_discriminator(
                Some("context check"),
                "output_kind",
                "context_check",
            )],
        ),
    ),
    entry(
        &["context", "suggest"],
        json_discriminators(
            opaque_schemas(READ_JSON, &["context suggest"]),
            &[json_discriminator(
                Some("context suggest"),
                "output_kind",
                "context_suggest",
            )],
        ),
    ),
    entry(
        &["context", "audit"],
        json_discriminators(
            opaque_schemas(READ_JSON, &["context audit"]),
            &[json_discriminator(
                Some("context audit"),
                "output_kind",
                "context_audit",
            )],
        ),
    ),
    entry(&["daemon"], surface(GROUP, "admin")),
    entry(
        &["daemon", "serve"],
        surface(opaque_schemas(DAEMON_MUTATION, &["daemon serve"]), "admin"),
    ),
    entry(
        &["daemon", "status"],
        surface(opaque_schemas(READ_JSON, &["daemon status"]), "admin"),
    ),
    entry(
        &["daemon", "stop"],
        surface(
            json_discriminators(
                opaque_schemas(DAEMON_MUTATION, &["daemon stop"]),
                &[json_discriminator(
                    Some("daemon stop"),
                    "output_kind",
                    "daemon_stop",
                )],
            ),
            "admin",
        ),
    ),
    entry(
        &["delegate"],
        category(
            documented_schemas(WORKTREE_MUTATION, &["delegate"]),
            "threads",
        ),
    ),
    entry(
        &["diff"],
        front_door(
            json_discriminators(
                documented_schemas(READ_JSON, &["diff"]),
                &[json_discriminator(Some("diff"), "output_kind", "diff")],
            ),
            20,
        ),
    ),
    entry(&["discuss"], category(GROUP, "collab")),
    entry(
        &["discuss", "open"],
        json_discriminators(
            documented_schemas(MUTATING, &["discuss open"]),
            &[json_discriminator(
                Some("discuss open"),
                "output_kind",
                "discuss_open",
            )],
        ),
    ),
    entry(
        &["discuss", "append"],
        json_discriminators(
            documented_schemas(MUTATING, &["discuss append"]),
            &[json_discriminator(
                Some("discuss append"),
                "output_kind",
                "discuss_append",
            )],
        ),
    ),
    entry(
        &["discuss", "resolve"],
        json_discriminators(
            documented_schemas(MUTATING, &["discuss resolve"]),
            &[json_discriminator(
                Some("discuss resolve"),
                "output_kind",
                "discuss_resolve",
            )],
        ),
    ),
    entry(
        &["discuss", "list"],
        json_discriminators(
            documented_schemas(READ_JSON, &["discuss list"]),
            &[json_discriminator(
                Some("discuss list"),
                "output_kind",
                "discuss_list",
            )],
        ),
    ),
    entry(
        &["discuss", "show"],
        json_discriminators(
            documented_schemas(READ_JSON, &["discuss show"]),
            &[json_discriminator(
                Some("discuss show"),
                "output_kind",
                "discuss_show",
            )],
        ),
    ),
    entry(
        &["doctor"],
        front_door(
            json_discriminators(
                documented_schemas(READ_JSON, &["doctor"]),
                &[json_discriminator(
                    Some("doctor"),
                    "output_kind",
                    "diagnose",
                )],
            ),
            120,
        ),
    ),
    entry(
        &["doctor", "docs"],
        json_discriminators(
            documented_schemas(READ_JSON, &["doctor docs"]),
            &[json_discriminator(
                Some("doctor docs"),
                "output_kind",
                "doctor_docs",
            )],
        ),
    ),
    entry(
        &["doctor", "schemas"],
        json_discriminators(
            documented_schemas(READ_JSON, &["doctor schemas"]),
            &[json_discriminator(
                Some("doctor schemas"),
                "output_kind",
                "doctor_schemas",
            )],
        ),
    ),
    entry(
        &["fetch"],
        git_adapter_action(
            json_discriminators(
                documented_schemas(
                    CommandContract {
                        writes_git_refs: true,
                        network_io: true,
                        ..MUTATING
                    },
                    &["fetch"],
                ),
                &[json_discriminator(Some("fetch"), "output_kind", "fetch")],
            ),
            "pull",
            "workflow",
            "Use pull for the normal remote update workflow; inspect verification output before materializing changes.",
        ),
    ),
    entry(
        &["fork"],
        category(
            json_discriminators(
                opaque_schemas(MUTATING, &["fork"]),
                &[json_discriminator(Some("fork"), "output_kind", "fork")],
            ),
            "threads",
        ),
    ),
    entry(
        &["fsck"],
        category(documented_schemas(MUTATING, &["fsck"]), "recovery"),
    ),
    entry(
        &["git-overlay"],
        category(documented_schemas(READ_JSON, &["git-overlay"]), "repo"),
    ),
    entry(
        &["goto"],
        category(
            json_discriminators(
                documented_schemas(WORKTREE_MUTATION, &["goto"]),
                &[json_discriminator(Some("goto"), "output_kind", "goto")],
            ),
            "threads",
        ),
    ),
    entry(
        &["harness-bridge"],
        hidden(opaque_schemas(READ_JSONL, &["harness-bridge"])),
    ),
    entry(&["help"], category(READ_TEXT, "repo")),
    entry(&["hook"], surface(GROUP, "automation")),
    entry(
        &["hook", "list"],
        surface(opaque_schemas(READ_JSON, &["hook list"]), "automation"),
    ),
    entry(
        &["hook", "install"],
        surface(
            opaque_schemas(HOOK_MUTATION, &["hook install"]),
            "automation",
        ),
    ),
    entry(
        &["hook", "uninstall"],
        surface(
            opaque_schemas(HOOK_MUTATION, &["hook uninstall"]),
            "automation",
        ),
    ),
    entry(
        &["hook", "events"],
        surface(opaque_schemas(READ_JSON, &["hook events"]), "automation"),
    ),
    entry(
        &["init"],
        exits(
            front_door(
                json_discriminators(
                    documented_schemas(INIT, &["init"]),
                    &[json_discriminator(Some("init"), "output_kind", "init")],
                ),
                200,
            ),
            &[
                (0, "ok"),
                (73, "cannot create state directory"),
                (78, "workspace config invalid"),
            ],
        ),
    ),
    entry(
        &["inspect"],
        category(
            json_discriminators(
                documented_schemas(READ_JSON, &["inspect", "thread show"]),
                &[
                    json_discriminator(Some("inspect"), "output_kind", "inspect_state"),
                    json_discriminator(Some("thread show"), "output_kind", "thread_show"),
                ],
            ),
            "states",
        ),
    ),
    entry(&["integration"], surface(GROUP, "admin")),
    entry(
        &["integration", "list"],
        surface(opaque_schemas(READ_JSON, &["integration list"]), "admin"),
    ),
    entry(
        &["integration", "install"],
        surface(opaque_schemas(MUTATING, &["integration install"]), "admin"),
    ),
    entry(
        &["integration", "doctor"],
        surface(opaque_schemas(READ_JSON, &["integration doctor"]), "admin"),
    ),
    entry(
        &["integration", "uninstall"],
        surface(
            opaque_schemas(MUTATING, &["integration uninstall"]),
            "admin",
        ),
    ),
    entry(
        &["integration", "upgrade"],
        surface(opaque_schemas(MUTATING, &["integration upgrade"]), "admin"),
    ),
    entry(
        &["integration", "relay"],
        hidden(surface(
            opaque_schemas(MUTATING, &["integration relay"]),
            "admin",
        )),
    ),
    entry(
        &["log"],
        front_door(
            json_discriminators(
                documented_schemas(READ_JSON, &["log", "log --reflog"]),
                &[
                    json_discriminator(Some("log"), "output_kind", "log"),
                    json_discriminator(Some("log --reflog"), "output_kind", "log_reflog"),
                ],
            ),
            130,
        ),
    ),
    entry(&["maintenance"], surface(GROUP, "admin")),
    entry(
        &["maintenance", "inspect"],
        surface(opaque_schemas(READ_JSON, &["maintenance inspect"]), "admin"),
    ),
    entry(
        &["maintenance", "run"],
        surface(opaque_schemas(MUTATING, &["maintenance run"]), "admin"),
    ),
    entry(
        &["maintenance", "gc"],
        surface(
            json_discriminators(
                opaque_schemas(GC_MUTATION, &["maintenance gc"]),
                &[json_discriminator(
                    Some("maintenance gc"),
                    "output_kind",
                    "gc",
                )],
            ),
            "admin",
        ),
    ),
    entry(
        &["maintenance", "index"],
        surface(
            json_discriminators(
                documented_schemas(READ_JSON, &["maintenance index"]),
                &[json_discriminator(
                    Some("maintenance index"),
                    "output_kind",
                    "index",
                )],
            ),
            "admin",
        ),
    ),
    entry(
        &["maintenance", "monitor"],
        surface(opaque_schemas(READ_JSON, &["maintenance monitor"]), "admin"),
    ),
    entry(&["marker"], category(GROUP, "collab")),
    entry(
        &["marker", "list"],
        documented_schemas(READ_JSON, &["marker list"]),
    ),
    entry(
        &["marker", "create"],
        documented_schemas(MUTATING, &["marker create"]),
    ),
    entry(
        &["marker", "delete"],
        documented_schemas(MUTATING, &["marker delete", "marker delete --prefix"]),
    ),
    entry(
        &["marker", "show"],
        documented_schemas(READ_JSON, &["marker show"]),
    ),
    entry(
        &["merge"],
        category(
            exits(
                surface(
                    advertised_action(
                        json_discriminators(
                            documented_schemas(
                                compact_json(WORKTREE_MUTATION),
                                &["merge --preview"],
                            ),
                            &[json_discriminator(
                                Some("merge --preview"),
                                "output_kind",
                                "merge",
                            )],
                        ),
                        "heddle merge <thread> --preview",
                        &["heddle", "merge", "<thread>", "--preview"],
                        &["thread"],
                        true,
                        false,
                    ),
                    "native",
                ),
                &[
                    (0, "ok"),
                    (65, "conflict requires manual resolution"),
                    (74, "io while writing state"),
                ],
            ),
            "threads",
        ),
    ),
    entry(
        &["stack"],
        category(
            json_discriminators(
                opaque_schemas(READ_JSON, &["stack"]),
                &[json_discriminator(Some("stack"), "output_kind", "stack")],
            ),
            "threads",
        ),
    ),
    entry(
        &["stack", "ready"],
        json_discriminators(
            opaque_schemas(READ_JSON, &["stack ready"]),
            &[json_discriminator(
                Some("stack ready"),
                "output_kind",
                "stack_ready",
            )],
        ),
    ),
    entry(
        &["stack", "snapshot"],
        json_discriminators(
            opaque_schemas(READ_JSON, &["stack snapshot"]),
            &[json_discriminator(
                Some("stack snapshot"),
                "output_kind",
                "stack_snapshot",
            )],
        ),
    ),
    entry(
        &["presence"],
        category(feature_gated(READ_JSON, "client"), "collab"),
    ),
    entry(&["presence", "publish"], feature_gated(READ_JSON, "client")),
    entry(
        &["pull"],
        exits(
            front_door(
                json_discriminators(
                    documented_schemas(
                        CommandContract {
                            writes_git_refs: true,
                            network_io: true,
                            ..WORKTREE_MUTATION
                        },
                        &["pull"],
                    ),
                    &[json_discriminator(Some("pull"), "output_kind", "pull")],
                ),
                90,
            ),
            &[
                (0, "ok"),
                (75, "remote unreachable; safe to retry"),
                (76, "upstream protocol error"),
                (78, "no upstream configured"),
            ],
        ),
    ),
    entry(&["purge"], category(GROUP, "recovery")),
    entry(
        &["purge", "apply"],
        json_discriminators(
            opaque_schemas(
                CommandContract {
                    destructive_requires_force: true,
                    ..DESTRUCTIVE_DATA_MUTATION
                },
                &["purge apply"],
            ),
            &[json_discriminator(
                Some("purge apply"),
                "output_kind",
                "purge_apply",
            )],
        ),
    ),
    entry(
        &["purge", "list"],
        json_discriminators(
            opaque_schemas(READ_JSON, &["purge list"]),
            &[json_discriminator(
                Some("purge list"),
                "output_kind",
                "purge_list",
            )],
        ),
    ),
    entry(
        &["push"],
        exits(
            front_door(
                advertised_action(
                    json_discriminators(
                        documented_schemas(
                            CommandContract {
                                writes_git_refs: true,
                                network_io: true,
                                ..MUTATING
                            },
                            &["push"],
                        ),
                        &[json_discriminator(Some("push"), "output_kind", "push")],
                    ),
                    "heddle push",
                    &["heddle", "push"],
                    &[],
                    true,
                    true,
                ),
                80,
            ),
            &[
                (0, "ok"),
                (75, "remote unreachable; safe to retry"),
                (
                    76,
                    "remote rejected payload; do not retry without changing inputs",
                ),
                (78, "no upstream configured"),
            ],
        ),
    ),
    entry(
        &["query"],
        category(
            json_discriminators(
                documented_schemas(READ_JSON, &["query"]),
                &[json_discriminator(Some("query"), "output_kind", "query")],
            ),
            "states",
        ),
    ),
    entry(
        &["ready"],
        front_door(
            json_discriminators(
                documented_schemas(compact_json(CAPTURE), &["ready"]),
                &[json_discriminator(Some("ready"), "output_kind", "ready")],
            ),
            50,
        ),
    ),
    entry(
        &["rebase"],
        category(
            json_discriminators(
                opaque_schemas(WORKTREE_MUTATION_JSONL, &["rebase"]),
                &[json_discriminator(
                    Some("rebase"),
                    "output_kind",
                    "rebase_progress",
                )],
            ),
            "threads",
        ),
    ),
    entry(&["redact"], category(GROUP, "recovery")),
    entry(
        &["redact", "apply"],
        json_discriminators(
            opaque_schemas(DATA_MUTATION, &["redact apply"]),
            &[json_discriminator(
                Some("redact apply"),
                "output_kind",
                "redact_apply",
            )],
        ),
    ),
    entry(
        &["redact", "list"],
        json_discriminators(
            opaque_schemas(READ_JSON, &["redact list"]),
            &[json_discriminator(
                Some("redact list"),
                "output_kind",
                "redact_list",
            )],
        ),
    ),
    entry(
        &["redact", "show"],
        json_discriminators(
            opaque_schemas(READ_JSON, &["redact show"]),
            &[json_discriminator(
                Some("redact show"),
                "output_kind",
                "redact_show",
            )],
        ),
    ),
    entry(&["redact", "trust"], GROUP),
    entry(
        &["redact", "trust", "add"],
        json_discriminators(
            opaque_schemas(DATA_MUTATION, &["redact trust add"]),
            &[json_discriminator(
                Some("redact trust add"),
                "output_kind",
                "redact_trust_add",
            )],
        ),
    ),
    entry(
        &["redact", "trust", "list"],
        json_discriminators(
            opaque_schemas(READ_JSON, &["redact trust list"]),
            &[json_discriminator(
                Some("redact trust list"),
                "output_kind",
                "redact_trust_list",
            )],
        ),
    ),
    entry(
        &["redact", "trust", "remove"],
        json_discriminators(
            opaque_schemas(DATA_MUTATION, &["redact trust remove"]),
            &[json_discriminator(
                Some("redact trust remove"),
                "output_kind",
                "redact_trust_remove",
            )],
        ),
    ),
    entry(
        // `redo` is the symmetric inverse of `undo`, a standalone top-level verb
        // again after the heddle#473 phase-1 re-split. It emits exactly one
        // output_kind — `redo` — schema-backed by `UndoSchema` (shared payload
        // shape with `undo`).
        &["redo"],
        category(
            json_discriminators(
                documented_schemas(WORKTREE_MUTATION, &["redo"]),
                &[json_discriminator(Some("redo"), "output_kind", "redo")],
            ),
            "recovery",
        ),
    ),
    entry(&["remote"], category(surface(GROUP, "native"), "repo")),
    entry(
        &["remote", "list"],
        json_discriminators(
            documented_schemas(READ_JSON, &["remote list"]),
            &[json_discriminator(
                Some("remote list"),
                "output_kind",
                "remote_list",
            )],
        ),
    ),
    entry(
        &["remote", "add"],
        json_discriminators(
            documented_schemas(CONFIG_MUTATION, &["remote add"]),
            &[json_discriminator(
                Some("remote add"),
                "output_kind",
                "remote_add",
            )],
        ),
    ),
    entry(
        &["remote", "remove"],
        json_discriminators(
            documented_schemas(CONFIG_MUTATION, &["remote remove"]),
            &[json_discriminator(
                Some("remote remove"),
                "output_kind",
                "remote_remove",
            )],
        ),
    ),
    entry(
        &["remote", "set-default"],
        json_discriminators(
            documented_schemas(CONFIG_MUTATION, &["remote set-default"]),
            &[json_discriminator(
                Some("remote set-default"),
                "output_kind",
                "remote_set_default",
            )],
        ),
    ),
    entry(
        &["remote", "show"],
        json_discriminators(
            documented_schemas(READ_JSON, &["remote show"]),
            &[json_discriminator(
                Some("remote show"),
                "output_kind",
                "remote_show",
            )],
        ),
    ),
    entry(
        &["resolve"],
        front_door(documented_schemas(MUTATING, &["resolve"]), 300),
    ),
    entry(
        &["retro"],
        category(documented_schemas(READ_JSON, &["retro"]), "states"),
    ),
    entry(
        &["revert"],
        category(
            json_discriminators(
                documented_schemas(WORKTREE_MUTATION, &["revert"]),
                &[json_discriminator(Some("revert"), "output_kind", "revert")],
            ),
            "states",
        ),
    ),
    entry(&["review"], category(GROUP, "collab")),
    entry(
        &["review", "show"],
        json_discriminators(
            documented_schemas(READ_JSON, &["review show"]),
            &[json_discriminator(
                Some("review show"),
                "output_kind",
                "review_show",
            )],
        ),
    ),
    entry(
        &["review", "sign"],
        json_discriminators(
            documented_schemas(MUTATING, &["review sign"]),
            &[json_discriminator(
                Some("review sign"),
                "output_kind",
                "review_sign",
            )],
        ),
    ),
    entry(
        &["review", "next"],
        json_discriminators(
            documented_schemas(READ_JSON, &["review next"]),
            &[json_discriminator(
                Some("review next"),
                "output_kind",
                "review_next",
            )],
        ),
    ),
    entry(
        &["review", "health"],
        json_discriminators(
            documented_schemas(READ_JSON, &["review health"]),
            &[json_discriminator(
                Some("review health"),
                "output_kind",
                "review_health",
            )],
        ),
    ),
    entry(&["run"], surface(EXTERNAL_WORKTREE_COMMAND, "automation")),
    entry(
        &["schemas"],
        surface(
            json_discriminators(
                documented_schemas(READ_JSON, &["schemas"]),
                &[json_discriminator(
                    Some("schemas"),
                    "output_kind",
                    "schemas",
                )],
            ),
            "automation",
        ),
    ),
    entry(&["semantic"], category(GROUP, "states")),
    entry(
        &["semantic", "hot"],
        opaque_schemas(READ_JSON, &["semantic hot"]),
    ),
    entry(&["session"], surface(GROUP, "automation")),
    entry(
        &["session", "start"],
        surface(
            documented_schemas(MUTATING, &["session start"]),
            "automation",
        ),
    ),
    entry(
        &["session", "segment"],
        surface(
            documented_schemas(MUTATING, &["session segment"]),
            "automation",
        ),
    ),
    entry(
        &["session", "end"],
        surface(documented_schemas(MUTATING, &["session end"]), "automation"),
    ),
    entry(
        &["session", "show"],
        surface(
            documented_schemas(READ_JSON, &["session show"]),
            "automation",
        ),
    ),
    entry(
        &["session", "list"],
        surface(
            documented_schemas(READ_JSON, &["session list"]),
            "automation",
        ),
    ),
    entry(&["shell"], category(READ_TEXT, "repo")),
    entry(&["shell", "init"], READ_TEXT),
    entry(&["shell", "completion"], READ_TEXT),
    entry(
        &["land"],
        front_door(
            json_discriminators(
                documented_schemas(
                    CommandContract {
                        writes_git_refs: true,
                        network_io: true,
                        ..compact_json(MUTATING)
                    },
                    &["land"],
                ),
                &[json_discriminator(Some("land"), "output_kind", "land")],
            ),
            70,
        ),
    ),
    entry(
        &["show"],
        front_door(
            json_discriminators(
                documented_schemas(READ_JSON, &["show"]),
                &[json_discriminator(Some("show"), "output_kind", "show")],
            ),
            140,
        ),
    ),
    entry(
        &["start"],
        front_door(
            json_discriminators(
                documented_schemas(WORKTREE_MUTATION, &["start"]),
                &[json_discriminator(
                    Some("start"),
                    "output_kind",
                    "thread_start",
                )],
            ),
            40,
        ),
    ),
    entry(
        &["stash"],
        git_adapter_action(
            READ_TEXT,
            "capture",
            "conceptual_home",
            "Use capture, commit, and thread captures for durable Heddle saves.",
        ),
    ),
    entry(
        &["stash", "push"],
        git_adapter_action(
            documented_schemas(WORKTREE_ONLY_MUTATION, &["stash push"]),
            "capture",
            "workflow",
            "Use capture for a durable named save point before changing the worktree.",
        ),
    ),
    entry(
        &["stash", "list"],
        git_adapter_action(
            json_discriminators(
                documented_schemas(READ_JSON, &["stash list"]),
                &[json_discriminator(
                    Some("stash list"),
                    "output_kind",
                    "stash_list",
                )],
            ),
            "thread captures",
            "conceptual_home",
            "Use thread captures to inspect durable Heddle save points.",
        ),
    ),
    entry(
        &["stash", "pop"],
        git_adapter_action(
            documented_schemas(
                CommandContract {
                    destructive_data: true,
                    ..WORKTREE_ONLY_MUTATION
                },
                &["stash pop"],
            ),
            "undo",
            "conceptual_home",
            "Use undo to reverse the last Heddle operation; stash pop is not a direct semantic match.",
        ),
    ),
    entry(
        &["stash", "apply"],
        git_adapter_action(
            documented_schemas(WORKTREE_ONLY_MUTATION, &["stash apply"]),
            "undo",
            "conceptual_home",
            "Use undo to reverse the last Heddle operation; stash apply is not a direct semantic match.",
        ),
    ),
    entry(
        &["stash", "drop"],
        git_adapter_action(
            documented_schemas(DESTRUCTIVE_DATA_MUTATION, &["stash drop"]),
            "thread captures",
            "conceptual_home",
            "Use thread captures to inspect and manage durable Heddle save points.",
        ),
    ),
    entry(
        &["stash", "clear"],
        git_adapter_action(
            documented_schemas(DESTRUCTIVE_DATA_MUTATION, &["stash clear"]),
            "thread captures",
            "conceptual_home",
            "Use thread captures to inspect and manage durable Heddle save points.",
        ),
    ),
    entry(
        &["stash", "show"],
        git_adapter_alias(
            json_discriminators(
                documented_schemas(READ_JSON, &["stash show"]),
                &[json_discriminator(
                    Some("stash show"),
                    "output_kind",
                    "stash_show",
                )],
            ),
            "show",
        ),
    ),
    entry(
        &["status"],
        exits(
            front_door(
                json_discriminators(
                    documented_schemas(compact_json(READ_JSON_OR_JSONL), &["status"]),
                    &[json_discriminator(Some("status"), "output_kind", "status")],
                ),
                10,
            ),
            &[(0, "ok"), (74, "io reading workspace state")],
        ),
    ),
    entry(
        &["support"],
        category(feature_gated(MUTATING, "client"), "repo"),
    ),
    entry(
        &["support", "grant"],
        feature_gated(MUTATING_NO_OP_ID, "client"),
    ),
    entry(&["support", "list"], feature_gated(READ_JSON, "client")),
    entry(
        &["support", "revoke"],
        feature_gated(MUTATING_NO_OP_ID, "client"),
    ),
    entry(
        &["switch"],
        git_adapter_alias(
            json_discriminators(
                documented_schemas(WORKTREE_MUTATION, &["switch"]),
                &[
                    // Thread targets delegate to `thread switch` and emit
                    // its record; this is the shape the registered
                    // `switch` schema mirrors.
                    json_discriminator(Some("switch"), "output_kind", "thread_switch"),
                    // State targets fall through to the state-checkout
                    // (`goto`) shape — advertised without a schema verb,
                    // mirroring the `branch` / hosted-`clone` precedent.
                    json_discriminator_no_schema(
                        "switch falls through to the state-checkout (goto) \
                         shape when the target resolves as a state; the \
                         registered `switch` schema mirrors only the thread \
                         path",
                        "output_kind",
                        "goto",
                    ),
                ],
            ),
            "thread switch",
        ),
    ),
    entry(
        &["sync"],
        category(
            json_discriminators(
                documented_schemas(operator_envelope(compact_json(MUTATING)), &["sync"]),
                &[json_discriminator(Some("sync"), "output_kind", "sync")],
            ),
            "threads",
        ),
    ),
    entry(&["thread"], category(surface(GROUP, "native"), "threads")),
    entry(
        &["thread", "create"],
        json_discriminators(
            documented_schemas(MUTATING, &["thread create"]),
            &[json_discriminator(
                Some("thread create"),
                "output_kind",
                "thread_create",
            )],
        ),
    ),
    entry(
        &["thread", "current"],
        documented_schemas(READ_JSON, &["thread current"]),
    ),
    entry(
        &["thread", "switch"],
        json_discriminators(
            documented_schemas(WORKTREE_MUTATION, &["thread switch"]),
            &[json_discriminator(
                Some("thread switch"),
                "output_kind",
                "thread_switch",
            )],
        ),
    ),
    entry(&["thread", "cd"], READ_TEXT),
    entry(
        &["thread", "list"],
        json_discriminators(
            documented_schemas(READ_JSON, &["thread list"]),
            &[json_discriminator(
                Some("thread list"),
                "output_kind",
                "thread_list",
            )],
        ),
    ),
    entry(
        &["thread", "show"],
        json_discriminators(
            documented_schemas(READ_JSON_OR_JSONL, &["thread show"]),
            &[json_discriminator(
                Some("thread show"),
                "output_kind",
                "thread_show",
            )],
        ),
    ),
    entry(
        &["thread", "captures"],
        documented_schemas(READ_JSON, &["thread captures"]),
    ),
    entry(
        &["thread", "rename"],
        json_discriminators(
            documented_schemas(MUTATING, &["thread rename"]),
            &[json_discriminator(
                Some("thread rename"),
                "output_kind",
                "thread_rename",
            )],
        ),
    ),
    entry(
        &["thread", "refresh"],
        json_discriminators(
            documented_schemas(WORKTREE_MUTATION, &["thread refresh"]),
            &[json_discriminator(
                Some("thread refresh"),
                "output_kind",
                "thread_refresh",
            )],
        ),
    ),
    entry(
        &["thread", "move"],
        documented_schemas(MUTATING, &["thread move"]),
    ),
    entry(
        &["thread", "absorb"],
        documented_schemas(MUTATING, &["thread absorb"]),
    ),
    entry(
        &["thread", "resolve"],
        json_discriminators(
            documented_schemas(MUTATING, &["thread resolve"]),
            &[json_discriminator(
                Some("thread resolve"),
                "output_kind",
                "thread_resolve",
            )],
        ),
    ),
    entry(
        &["thread", "promote"],
        json_discriminators(
            documented_schemas(WORKTREE_MUTATION, &["thread promote"]),
            &[json_discriminator(
                Some("thread promote"),
                "output_kind",
                "thread_promote",
            )],
        ),
    ),
    entry(
        &["thread", "drop"],
        json_discriminators(
            documented_schemas(DESTRUCTIVE_WORKTREE_MUTATION, &["thread drop"]),
            &[json_discriminator(
                Some("thread drop"),
                "output_kind",
                "thread_drop",
            )],
        ),
    ),
    entry(
        &["thread", "approve"],
        documented_schemas(MUTATING, &["thread approve"]),
    ),
    entry(
        &["thread", "approvals"],
        documented_schemas(READ_JSON, &["thread approvals"]),
    ),
    entry(
        &["thread", "revoke-approval"],
        json_discriminators(
            documented_schemas(MUTATING, &["thread revoke-approval"]),
            &[json_discriminator(
                Some("thread revoke-approval"),
                "output_kind",
                "thread_revoke_approval",
            )],
        ),
    ),
    entry(
        &["thread", "check-merge"],
        documented_schemas(READ_JSON, &["thread check-merge"]),
    ),
    entry(
        &["thread", "cleanup"],
        json_discriminators(
            documented_schemas(DESTRUCTIVE_WORKTREE_MUTATION, &["thread cleanup"]),
            &[json_discriminator(
                Some("thread cleanup"),
                "output_kind",
                "thread_cleanup",
            )],
        ),
    ),
    entry(&["transaction"], hidden(GROUP)),
    entry(
        &["transaction", "begin"],
        hidden(opaque_schemas(MUTATING, &["transaction begin"])),
    ),
    entry(
        &["transaction", "commit"],
        documented_schemas(MUTATING, &["transaction commit"]),
    ),
    entry(
        &["transaction", "abort"],
        hidden(opaque_schemas(MUTATING, &["transaction abort"])),
    ),
    entry(
        &["transaction", "status"],
        hidden(opaque_schemas(READ_JSON, &["transaction status"])),
    ),
    entry(
        &["verify"],
        exits(
            front_door(
                json_discriminators(
                    documented_schemas(READ_JSON, &["verify"]),
                    &[json_discriminator(Some("verify"), "output_kind", "verify")],
                ),
                110,
            ),
            &[
                (0, "verified clean"),
                (65, "verification reports blocked state"),
                (74, "io reading state"),
            ],
        ),
    ),
    entry(&["visibility"], category(GROUP, "collab")),
    entry(
        &["visibility", "set"],
        json_discriminators(
            opaque_schemas(DATA_MUTATION, &["visibility set"]),
            &[json_discriminator(
                Some("visibility set"),
                "output_kind",
                "visibility_set",
            )],
        ),
    ),
    entry(
        &["visibility", "promote"],
        json_discriminators(
            opaque_schemas(DATA_MUTATION, &["visibility promote"]),
            &[json_discriminator(
                Some("visibility promote"),
                "output_kind",
                "visibility_promote",
            )],
        ),
    ),
    entry(
        &["visibility", "show"],
        json_discriminators(
            opaque_schemas(READ_JSON, &["visibility show"]),
            &[json_discriminator(
                Some("visibility show"),
                "output_kind",
                "visibility_show",
            )],
        ),
    ),
    entry(
        &["visibility", "list"],
        json_discriminators(
            opaque_schemas(READ_JSON, &["visibility list"]),
            &[json_discriminator(
                Some("visibility list"),
                "output_kind",
                "visibility_list",
            )],
        ),
    ),
    entry(
        &["try"],
        category(
            documented_schemas(EXTERNAL_WORKTREE_MUTATION, &["try"]),
            "threads",
        ),
    ),
    entry(
        &["undo"],
        front_door(
            json_discriminators(
                // `undo` keeps its own `--list` history view, so this one command
                // path emits TWO output_kinds: `undo` (the default rewind /
                // `--preview`) and `undo_list` (`--list`). The former `redo` verb
                // is its own top-level command again (heddle#473 phase 1 re-split),
                // so `redo` is advertised on the `redo` entry below, not here.
                // Every kind the handler can emit must be advertised or an agent
                // validating responses via `heddle commands --output json` rejects
                // the off-contract record. `undo --list` has its own
                // `UndoListSchema`.
                documented_schemas(WORKTREE_MUTATION, &["undo", "undo --list"]),
                &[
                    json_discriminator(Some("undo"), "output_kind", "undo"),
                    json_discriminator(Some("undo --list"), "output_kind", "undo_list"),
                ],
            ),
            100,
        ),
    ),
    entry(
        &["watch"],
        surface(documented_schemas(READ_JSONL, &["watch"]), "automation"),
    ),
    entry(
        &["workspace"],
        category(
            documented_schemas(READ_JSON, &["workspace show"]),
            "threads",
        ),
    ),
    entry(
        &["workspace", "show"],
        json_discriminators(
            documented_schemas(READ_JSON_OR_JSONL, &["workspace show"]),
            &[json_discriminator(
                Some("workspace show"),
                "output_kind",
                "workspace_summary",
            )],
        ),
    ),
];

static ACTIVE_COMMAND_CONTRACT_ENTRIES: OnceLock<Vec<&'static CommandContractEntry>> =
    OnceLock::new();

const fn entry(path: &'static [&'static str], contract: CommandContract) -> CommandContractEntry {
    CommandContractEntry { path, contract }
}

pub fn cmd_commands(cli: &Cli, args: &CommandCatalogArgs) -> Result<()> {
    let mut output = build_command_catalog();
    apply_command_catalog_filters(&mut output, args);
    if should_output_json(cli, None) {
        write_json_stdout(&output)?;
        return Ok(());
    }

    let mut rendered = String::new();
    rendered.push_str(&format!("{}\n", style::bold("Command catalog")));
    rendered.push_str(
        "Use `heddle commands --output json` for flags, arguments, side effects, schemas, and canonical command mappings.\n\n",
    );
    for title in [
        "Native loop",
        "Power surfaces",
        "Git interop",
        "Automation and admin",
    ] {
        rendered.push_str(&format!("{}:\n", style::bold(title)));
        let mut section_commands = output
            .commands
            .iter()
            .filter(|command| command_in_text_section(command, title))
            .collect::<Vec<_>>();
        section_commands.sort_by_key(|command| (command.help_rank, command.display.as_str()));
        for command in section_commands {
            let canonical = command
                .canonical_action
                .as_ref()
                .map_or_else(String::new, canonical_action_text_suffix);
            rendered.push_str(&format!(
                "  {:<14}  {}{}\n",
                command.display, command.summary, canonical
            ));
        }
        rendered.push('\n');
    }
    write_stdout(&rendered)?;
    Ok(())
}

fn apply_command_catalog_filters(output: &mut CommandCatalogOutput, args: &CommandCatalogArgs) {
    if args.commands.is_empty() && args.tier.is_empty() && !args.mutating && !args.supports_op_id {
        return;
    }

    let command_filters = args
        .commands
        .iter()
        .map(|command| normalize_command_filter(command))
        .filter(|command| !command.is_empty())
        .collect::<Vec<_>>();
    let tier_filters = args
        .tier
        .iter()
        .map(|tier| tier.as_str())
        .collect::<Vec<_>>();

    output.commands.retain(|command| {
        (command_filters.is_empty()
            || command_filters
                .iter()
                .any(|filter| command_matches_filter(command, filter)))
            && (tier_filters.is_empty() || tier_filters.contains(&command.tier.as_str()))
            && (!args.mutating || command.mutates)
            && (!args.supports_op_id || command.supports_op_id)
    });
}

fn normalize_command_filter(command: &str) -> Vec<String> {
    command
        .split_whitespace()
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty())
        .collect()
}

fn command_matches_filter(command: &CommandCatalogEntry, filter: &[String]) -> bool {
    command.path == filter || command.path.starts_with(filter)
}

fn command_in_text_section(command: &CommandCatalogEntry, title: &str) -> bool {
    if command.path.len() != 1 {
        return false;
    }
    match title {
        "Native loop" => command.help_visibility == "everyday",
        "Power surfaces" => command.help_visibility == "advanced" && command.surface == "native",
        "Git interop" => command.surface == "git_adapter",
        "Automation and admin" => matches!(command.surface.as_str(), "automation" | "admin"),
        _ => false,
    }
}

pub fn build_command_catalog() -> CommandCatalogOutput {
    debug_assert!(
        RECOMMENDED_ACTION_PLACEHOLDERS
            .iter()
            .all(|action| validate_recommended_action(action).is_ok())
    );

    let command = Cli::command();
    let mut global_options: Vec<_> = command
        .get_arguments()
        .filter(|arg| arg.is_global_set() && should_catalog_global_option(arg))
        .map(catalog_option)
        .collect();
    if !command.is_disable_help_flag_set() {
        global_options.push(generated_help_option());
    }

    let mut commands = Vec::new();
    let op_id_option = command
        .get_arguments()
        .find(|arg| arg.get_long() == Some("op-id"))
        .map(catalog_option);
    walk_commands(&command, &mut Vec::new(), &mut commands, &op_id_option);
    CommandCatalogOutput {
        kind: "command_catalog".to_string(),
        executable_path: heddle_argv0(),
        commands,
        global_options,
        json_discriminators: command_json_discriminators(),
        recommended_action_placeholders: RECOMMENDED_ACTION_PLACEHOLDERS
            .iter()
            .map(|action| (*action).to_string())
            .collect(),
        recommended_action_templates: RECOMMENDED_ACTION_TEMPLATES
            .iter()
            .map(|(action, argv_template, required_inputs, agent_may_fill)| {
                action_template_from_parts(action, argv_template, required_inputs, *agent_may_fill)
            })
            .collect(),
    }
}

pub(crate) fn heddle_argv0() -> String {
    match std::env::current_exe() {
        Ok(path) => {
            let file_name = path.file_name().and_then(|name| name.to_str());
            if matches!(file_name, Some("heddle") | Some("heddle.exe")) {
                path.display().to_string()
            } else {
                "heddle".to_string()
            }
        }
        Err(_) => "heddle".to_string(),
    }
}

pub(crate) fn normalize_heddle_argv(mut argv: Vec<String>) -> Vec<String> {
    if argv.first().is_some_and(|first| first == "heddle") {
        argv[0] = heddle_argv0();
    }
    argv
}

fn walk_commands(
    command: &clap::Command,
    prefix: &mut Vec<String>,
    out: &mut Vec<CommandCatalogEntry>,
    op_id_option: &Option<CommandCatalogOption>,
) {
    for subcommand in command.get_subcommands() {
        prefix.push(subcommand.get_name().to_string());
        out.push(catalog_entry(subcommand, prefix, op_id_option));
        walk_commands(subcommand, prefix, out, op_id_option);
        prefix.pop();
    }
}

fn catalog_entry(
    command: &clap::Command,
    path: &[String],
    op_id_option: &Option<CommandCatalogOption>,
) -> CommandCatalogEntry {
    let mut options = Vec::new();
    let mut arguments = Vec::new();
    for arg in command.get_arguments().filter(|arg| !arg.is_hide_set()) {
        if arg.get_long().is_some() || arg.get_short().is_some() {
            options.push(catalog_option(arg));
        } else {
            arguments.push(catalog_argument(arg));
        }
    }

    let contract = command_contract(path);
    if contract.supports_op_id
        && let Some(op_id_option) = op_id_option
    {
        options.push(op_id_option.clone());
    }
    CommandCatalogEntry {
        path: path.to_vec(),
        display: path.join(" "),
        aliases: command
            .get_all_aliases()
            .map(std::string::ToString::to_string)
            .collect(),
        tier: help_visibility_to_tier(contract.help_visibility).to_string(),
        surface: contract.surface.to_string(),
        help_visibility: contract.help_visibility.to_string(),
        help_rank: contract.help_rank,
        canonical_command: contract
            .canonical_command
            .map(std::string::ToString::to_string),
        canonical_action: canonical_action(contract),
        command_action: command_action(command, path, contract),
        summary: clean_catalog_summary(
            command
                .get_about()
                .or_else(|| command.get_long_about())
                .map(|about| about.to_string().lines().next().unwrap_or("").to_string())
                .unwrap_or_default(),
        ),
        has_subcommands: command.get_subcommands().next().is_some(),
        supports_json: contract.supports_json,
        mutates: contract.mutates,
        supports_op_id: contract.supports_op_id,
        persists_op_id: contract.persists_op_id,
        op_id_behavior: op_id_behavior(contract).to_string(),
        op_id_store_scope: op_id_store_scope(contract).to_string(),
        observe_only: contract.observe_only,
        may_initialize: contract.may_initialize,
        may_import_git: contract.may_import_git,
        may_write_worktree: contract.may_write_worktree,
        may_move_ref: contract.may_move_ref,
        destructive_requires_force: contract.destructive_requires_force,
        writes_heddle_refs: contract.writes_heddle_refs,
        writes_git_refs: contract.writes_git_refs,
        writes_worktree: contract.writes_worktree,
        writes_config: contract.writes_config,
        writes_hooks: contract.writes_hooks,
        network_io: contract.network_io,
        daemon_process: contract.daemon_process,
        object_gc: contract.object_gc,
        external_command: contract.external_command,
        requires_git_executable: contract.requires_git_executable,
        destructive_data: contract.destructive_data,
        side_effects: side_effects(contract)
            .iter()
            .map(|effect| (*effect).to_string())
            .collect(),
        side_effect_class: side_effect_class(contract).to_string(),
        first_run_behavior: first_run_behavior(contract).to_string(),
        json_kind: contract.json_kind.to_string(),
        json_discriminators: json_discriminators_for_path(path.iter().map(String::as_str)),
        schema_verbs: contract
            .schema_verbs
            .iter()
            .map(|verb| (*verb).to_string())
            .collect(),
        documented_schema_verbs: contract
            .documented_schema_verbs
            .iter()
            .map(|verb| (*verb).to_string())
            .collect(),
        options,
        arguments,
        exit_codes: contract
            .exit_codes
            .iter()
            .map(|(code, reason)| CommandCatalogExitCode {
                code: *code,
                reason: (*reason).to_string(),
            })
            .collect(),
    }
}

fn canonical_action(contract: CommandContract) -> Option<CanonicalAction> {
    let command = contract.canonical_command?;
    let kind = contract.canonical_kind.unwrap_or("direct_command");
    let (argv, template) = canonical_action_metadata(command, kind);
    Some(CanonicalAction {
        command: command.to_string(),
        kind: kind.to_string(),
        executable: argv.is_some(),
        note: contract.canonical_note.unwrap_or_default().to_string(),
        argv,
        template,
    })
}

fn canonical_action_metadata(
    command: &str,
    kind: &str,
) -> (Option<Vec<String>>, Option<ActionTemplate>) {
    match (command, kind) {
        ("adopt", "workflow") => (
            None,
            Some(action_template_from_parts(
                "heddle adopt --ref <branch>",
                &["heddle", "adopt", "--ref", "<branch>"],
                &["branch"],
                true,
            )),
        ),
        ("capture", "workflow") => (
            None,
            Some(action_template_from_parts(
                "heddle capture -m <message>",
                &["heddle", "capture", "-m", "<message>"],
                &["message"],
                true,
            )),
        ),
        ("thread switch", "direct_command") => (
            None,
            Some(action_template_from_parts(
                "heddle thread switch <thread>",
                &["heddle", "thread", "switch", "<thread>"],
                &["thread"],
                true,
            )),
        ),
        (_, "direct_command") => {
            let argv = std::iter::once("heddle".to_string())
                .chain(command.split_whitespace().map(str::to_string))
                .collect::<Vec<_>>();
            (Some(normalize_heddle_argv(argv)), None)
        }
        _ => (None, None),
    }
}

fn command_action(
    command: &clap::Command,
    path: &[String],
    contract: CommandContract,
) -> Option<CommandAction> {
    if let Some(action) = contract.advertised_action {
        return Some(command_action_from_advertised(action));
    }
    if command.get_subcommands().next().is_some() {
        return None;
    }

    let mut argv_template = std::iter::once("heddle".to_string())
        .chain(path.iter().cloned())
        .collect::<Vec<_>>();
    let mut required_inputs = Vec::new();
    for arg in command.get_arguments().filter(|arg| !arg.is_hide_set()) {
        if !arg.is_required_set() {
            continue;
        }
        if let Some(long) = arg.get_long() {
            argv_template.push(format!("--{long}"));
        } else if let Some(short) = arg.get_short() {
            argv_template.push(format!("-{short}"));
        }
        let names = value_names(arg);
        if names.is_empty() {
            required_inputs.push(arg.get_id().as_str().to_string());
            if arg.get_long().is_none() && arg.get_short().is_none() {
                argv_template.push(format!("<{}>", arg.get_id().as_str()));
            }
        } else {
            for name in names {
                let input = name.to_ascii_lowercase();
                required_inputs.push(input.clone());
                argv_template.push(format!("<{input}>"));
            }
        }
    }

    let action = argv_template.join(" ");
    if required_inputs.is_empty() {
        Some(CommandAction {
            action,
            executable: true,
            argv: Some(normalize_heddle_argv(argv_template)),
            template: None,
        })
    } else {
        Some(CommandAction {
            action: action.clone(),
            executable: false,
            argv: None,
            template: Some(action_template_from_owned(
                action,
                argv_template,
                required_inputs,
                true,
            )),
        })
    }
}

fn command_action_from_advertised(action: AdvertisedAction) -> CommandAction {
    let argv_template = action
        .argv_template
        .iter()
        .map(|part| (*part).to_string())
        .collect::<Vec<_>>();
    if action.executable {
        CommandAction {
            action: action.action.to_string(),
            executable: true,
            argv: Some(normalize_heddle_argv(argv_template)),
            template: None,
        }
    } else {
        CommandAction {
            action: action.action.to_string(),
            executable: false,
            argv: None,
            template: Some(action_template_from_parts(
                action.action,
                action.argv_template,
                action.required_inputs,
                action.agent_may_fill,
            )),
        }
    }
}

fn action_template_from_parts(
    action: &str,
    argv_template: &[&str],
    required_inputs: &[&str],
    agent_may_fill: bool,
) -> ActionTemplate {
    action_template_from_owned(
        action.to_string(),
        argv_template
            .iter()
            .map(|part| (*part).to_string())
            .collect(),
        required_inputs
            .iter()
            .map(|input| (*input).to_string())
            .collect(),
        agent_may_fill,
    )
}

fn action_template_from_owned(
    action: String,
    argv_template: Vec<String>,
    required_inputs: Vec<String>,
    agent_may_fill: bool,
) -> ActionTemplate {
    ActionTemplate {
        action,
        argv_template: normalize_heddle_argv(argv_template),
        required_inputs,
        agent_may_fill,
    }
}

fn canonical_action_text_suffix(action: &CanonicalAction) -> String {
    let verb = match action.kind.as_str() {
        "direct_command" => "use",
        "command_family" => "see",
        "workflow" => "start with",
        "conceptual_home" => "see",
        _ => "see",
    };
    format!(" ({verb} `{}`)", action.command)
}

fn clean_catalog_summary(summary: String) -> String {
    let stripped = summary
        .trim_start_matches("Automation/workflow command:")
        .trim_start();
    let mut chars = stripped.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn should_catalog_global_option(arg: &clap::Arg) -> bool {
    !arg.is_hide_set()
}

fn side_effect_class(contract: CommandContract) -> &'static str {
    if contract.observe_only {
        "observe_only"
    } else if contract.destructive_requires_force && contract.writes_worktree {
        "destructive_worktree_mutation"
    } else if contract.destructive_data {
        "destructive_data"
    } else if contract.object_gc {
        "object_gc"
    } else if contract.writes_worktree {
        "worktree_mutation"
    } else if contract.may_import_git {
        "git_import"
    } else if contract.may_initialize {
        "initialize"
    } else if contract.writes_hooks {
        "hook_mutation"
    } else if contract.writes_config {
        "config_mutation"
    } else if contract.network_io {
        "network_mutation"
    } else if contract.daemon_process {
        "daemon_process"
    } else if contract.external_command {
        "external_command"
    } else if contract.writes_heddle_refs || contract.writes_git_refs || contract.may_move_ref {
        "ref_mutation"
    } else if contract.mutates {
        "mutation"
    } else {
        "none"
    }
}

fn side_effects(contract: CommandContract) -> Vec<&'static str> {
    if contract.observe_only {
        return vec!["observe_only"];
    }

    let mut effects = Vec::new();
    if contract.may_initialize {
        effects.push("initialize");
    }
    if contract.may_import_git {
        effects.push("import_git");
    }
    if contract.writes_heddle_refs {
        effects.push("writes_heddle_refs");
    }
    if contract.writes_git_refs {
        effects.push("writes_git_refs");
    }
    if contract.writes_worktree {
        effects.push("writes_worktree");
    } else if contract.may_write_worktree {
        effects.push("may_write_worktree");
    }
    if contract.writes_config {
        effects.push("writes_config");
    }
    if contract.writes_hooks {
        effects.push("writes_hooks");
    }
    if contract.network_io {
        effects.push("network_io");
    }
    if contract.daemon_process {
        effects.push("daemon_process");
    }
    if contract.object_gc {
        effects.push("object_gc");
    }
    if contract.external_command {
        effects.push("external_command");
    }
    if contract.destructive_requires_force {
        effects.push("destructive_requires_force");
    }
    if contract.destructive_data {
        effects.push("destructive_data");
    }
    if effects.is_empty() && contract.mutates {
        effects.push("mutation");
    }
    effects
}

fn op_id_behavior(contract: CommandContract) -> &'static str {
    if contract.persists_op_id {
        "generated_resume"
    } else if contract.supports_op_id {
        "explicit_replay"
    } else {
        "none"
    }
}

fn uses_bootstrap_op_id_store(contract: CommandContract) -> bool {
    contract.supports_op_id && contract.may_initialize
}

fn op_id_store_scope(contract: CommandContract) -> &'static str {
    if !contract.supports_op_id {
        "none"
    } else if uses_bootstrap_op_id_store(contract) {
        "bootstrap"
    } else {
        "repository"
    }
}

fn first_run_behavior(contract: CommandContract) -> &'static str {
    if contract.observe_only {
        "observe_only_no_init"
    } else if contract.may_initialize && contract.may_import_git {
        "may_initialize_and_import_git"
    } else if contract.may_initialize {
        "may_initialize"
    } else if contract.may_import_git {
        "may_import_git"
    } else if contract.mutates {
        "requires_initialized_repo"
    } else {
        "no_repo_required"
    }
}

fn catalog_option(arg: &clap::Arg) -> CommandCatalogOption {
    CommandCatalogOption {
        id: arg.get_id().as_str().to_string(),
        long: arg.get_long().map(str::to_string),
        aliases: option_aliases(arg),
        short: arg.get_short().map(|short| short.to_string()),
        value_names: value_names(arg),
        value_kind: value_kind(arg).to_string(),
        default_values: arg
            .get_default_values()
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect(),
        possible_values: arg
            .get_possible_values()
            .iter()
            .map(|value| value.get_name().to_string())
            .collect(),
        help: arg.get_help().map(|help| help.to_string()),
        required: arg.is_required_set(),
        global: arg.is_global_set(),
    }
}

fn generated_help_option() -> CommandCatalogOption {
    CommandCatalogOption {
        id: "help".to_string(),
        long: Some("help".to_string()),
        aliases: Vec::new(),
        short: Some("h".to_string()),
        value_names: Vec::new(),
        value_kind: "boolean".to_string(),
        default_values: Vec::new(),
        possible_values: Vec::new(),
        help: Some("Print help".to_string()),
        required: false,
        global: true,
    }
}

fn option_aliases(arg: &clap::Arg) -> Vec<String> {
    let mut aliases = std::collections::BTreeSet::new();
    for alias in arg.get_all_aliases().unwrap_or_default() {
        aliases.insert(alias.to_string());
    }
    for alias in arg.get_visible_aliases().unwrap_or_default() {
        aliases.insert(alias.to_string());
    }
    aliases.into_iter().collect()
}

fn catalog_argument(arg: &clap::Arg) -> CommandCatalogArgument {
    CommandCatalogArgument {
        id: arg.get_id().as_str().to_string(),
        value_names: value_names(arg),
        help: arg.get_help().map(|help| help.to_string()),
        required: arg.is_required_set(),
    }
}

fn value_kind(arg: &clap::Arg) -> &'static str {
    match arg.get_action() {
        ArgAction::SetTrue | ArgAction::SetFalse => "boolean",
        ArgAction::Count => "count",
        ArgAction::Append => "list",
        ArgAction::Set if !arg.get_possible_values().is_empty() => "enum",
        ArgAction::Set => "string",
        _ => "unknown",
    }
}

fn value_names(arg: &clap::Arg) -> Vec<String> {
    arg.get_value_names()
        .map(|names| names.iter().map(|name| name.to_string()).collect())
        .unwrap_or_default()
}

fn command_contract(path: &[String]) -> CommandContract {
    command_contract_for_path(path.iter().map(String::as_str))
        .unwrap_or_else(|| panic!("missing command contract for `{}`", path.join(" ")))
}

fn command_contract_for_path<'a>(
    path: impl IntoIterator<Item = &'a str>,
) -> Option<CommandContract> {
    let path = path.into_iter().collect::<Vec<_>>();
    active_command_contract_entries()
        .iter()
        .copied()
        .find(|entry| entry.path == path.as_slice())
        .map(|entry| entry.contract)
}

#[cfg(test)]
fn raw_command_contract_for_path<'a>(
    path: impl IntoIterator<Item = &'a str>,
) -> Option<CommandContract> {
    let path = path.into_iter().collect::<Vec<_>>();
    CONTRACTS
        .iter()
        .find(|entry| entry.path == path.as_slice())
        .map(|entry| entry.contract)
}

#[cfg(test)]
pub(crate) fn sibling_documented_schema_verbs(schema_verb: &str) -> Vec<&'static str> {
    active_command_contract_entries()
        .iter()
        .filter(|entry| {
            entry
                .contract
                .documented_schema_verbs
                .contains(&schema_verb)
        })
        .flat_map(|entry| entry.contract.documented_schema_verbs.iter().copied())
        .filter(|documented| *documented != schema_verb)
        .collect()
}

fn active_command_contract_entries() -> &'static [&'static CommandContractEntry] {
    ACTIVE_COMMAND_CONTRACT_ENTRIES
        .get_or_init(|| {
            let command = Cli::command();
            CONTRACTS
                .iter()
                .filter(|entry| clap_command_path_exists(&command, entry.path))
                .collect()
        })
        .as_slice()
}

fn clap_command_path_exists(command: &clap::Command, path: &[&str]) -> bool {
    let mut current = command;
    for part in path {
        let Some(next) = current
            .get_subcommands()
            .find(|subcommand| subcommand.get_name() == *part)
        else {
            return false;
        };
        current = next;
    }
    true
}

pub fn command_json_discriminators() -> Vec<CommandJsonDiscriminator> {
    active_command_contract_entries()
        .iter()
        .copied()
        .flat_map(|entry| {
            entry
                .contract
                .json_discriminators
                .iter()
                .map(move |discriminator| json_discriminator_metadata(entry.path, discriminator))
        })
        .collect()
}

pub fn operator_envelope_verbs() -> Vec<String> {
    active_command_contract_entries()
        .iter()
        .copied()
        .filter(|entry| entry.contract.operator_envelope)
        .map(|entry| entry.path.join(" "))
        .collect()
}

#[cfg(test)]
pub fn command_json_discriminator_for_schema_verb(
    schema_verb: &str,
) -> Option<CommandJsonDiscriminator> {
    command_json_discriminators_for_schema_verb(schema_verb)
        .into_iter()
        .next()
}

pub fn command_json_discriminators_for_schema_verb(
    schema_verb: &str,
) -> Vec<CommandJsonDiscriminator> {
    active_command_contract_entries()
        .iter()
        .copied()
        .filter(|entry| {
            entry
                .contract
                .json_discriminators
                .iter()
                .any(|discriminator| discriminator.schema_verb == Some(schema_verb))
        })
        .flat_map(|entry| {
            let include_same_command_siblings = entry.contract.schema_verbs.len() == 1
                && entry.contract.schema_verbs[0] == schema_verb;
            entry.contract.json_discriminators.iter().filter_map(
                move |discriminator| {
                    if discriminator.schema_verb == Some(schema_verb)
                        || (include_same_command_siblings
                            && discriminator.schema_verb.is_none())
                    {
                        Some(json_discriminator_metadata(entry.path, discriminator))
                    } else {
                        None
                    }
                },
            )
        })
        .collect()
}

fn json_discriminators_for_path<'a>(
    path: impl IntoIterator<Item = &'a str>,
) -> Vec<CommandJsonDiscriminator> {
    let path = path.into_iter().collect::<Vec<_>>();
    active_command_contract_entries()
        .iter()
        .copied()
        .filter(|entry| entry.path == path.as_slice())
        .flat_map(|entry| {
            entry
                .contract
                .json_discriminators
                .iter()
                .map(move |discriminator| json_discriminator_metadata(entry.path, discriminator))
        })
        .collect()
}

fn json_discriminator_metadata(
    path: &'static [&'static str],
    discriminator: &CommandJsonDiscriminatorSpec,
) -> CommandJsonDiscriminator {
    CommandJsonDiscriminator {
        path: path.iter().map(|part| (*part).to_string()).collect(),
        display: path.join(" "),
        schema_verb: discriminator.schema_verb.map(str::to_string),
        field: discriminator.field.to_string(),
        value: discriminator.value.to_string(),
        no_schema_reason: discriminator.no_schema_reason.map(str::to_string),
    }
}

pub fn command_runtime_contract_for_command(command: &Commands) -> CommandRuntimeContract {
    let path = command_path(command);
    runtime_contract_for_path(path.iter().copied())
        .unwrap_or_else(|| panic!("missing command contract for `{}`", path.join(" ")))
}

pub fn command_runtime_contract(command_name: &str) -> Option<CommandRuntimeContract> {
    runtime_contract_for_path(command_name.split_whitespace())
}

fn runtime_contract_for_path<'a>(
    path: impl IntoIterator<Item = &'a str>,
) -> Option<CommandRuntimeContract> {
    let path = path.into_iter().collect::<Vec<_>>();
    active_command_contract_entries()
        .iter()
        .copied()
        .find(|entry| entry.path == path.as_slice())
        .map(|entry| runtime_contract(entry.path, entry.contract))
}

fn runtime_contract(
    path: &'static [&'static str],
    contract: CommandContract,
) -> CommandRuntimeContract {
    CommandRuntimeContract {
        path: path.to_vec(),
        display: path.join(" "),
        supports_json: contract.supports_json,
        supports_json_compact: contract.supports_json_compact,
        supports_op_id: contract.supports_op_id,
        persists_op_id: contract.persists_op_id,
        uses_bootstrap_op_id_store: uses_bootstrap_op_id_store(contract),
        help_visibility: contract.help_visibility,
        help_rank: contract.help_rank,
        surface: contract.surface,
        canonical_command: contract.canonical_command,
        json_kind: contract.json_kind,
    }
}

pub fn command_supports_op_id(command_name: &str) -> bool {
    command_runtime_contract(command_name)
        .map(|contract| contract.supports_op_id)
        .unwrap_or(false)
}

pub fn command_persists_op_id(command_name: &str) -> bool {
    command_runtime_contract(command_name)
        .map(|contract| contract.persists_op_id)
        .unwrap_or(false)
}

pub fn command_uses_bootstrap_op_id_store(command_name: &str) -> bool {
    command_runtime_contract(command_name)
        .map(|contract| contract.uses_bootstrap_op_id_store)
        .unwrap_or(false)
}

pub(crate) fn feature_gated_command_roots() -> Vec<&'static str> {
    let mut roots = CONTRACTS
        .iter()
        .filter(|entry| entry.path.len() == 1 && entry.contract.feature_gate.is_some())
        .map(|entry| entry.path[0])
        .collect::<Vec<_>>();
    roots.sort_unstable();
    roots.dedup();
    roots
}

pub fn command_supports_op_id_for_command(command: &Commands) -> bool {
    command_runtime_contract_for_command(command).supports_op_id
}

pub fn command_supports_json_for_command(command: &Commands) -> bool {
    command_runtime_contract_for_command(command).supports_json
}

pub fn command_help_tier(command_name: &str) -> &'static str {
    command_runtime_contract(command_name)
        .map(|contract| help_visibility_to_tier(contract.help_visibility))
        .unwrap_or("advanced")
}

pub fn command_surface(command_name: &str) -> &'static str {
    command_runtime_contract(command_name)
        .map(|contract| contract.surface)
        .unwrap_or("native")
}

pub fn command_help_visibility(command_name: &str) -> &'static str {
    command_runtime_contract(command_name)
        .map(|contract| contract.help_visibility)
        .unwrap_or("advanced")
}

pub fn command_canonical_command(command_name: &str) -> Option<&'static str> {
    command_runtime_contract(command_name).and_then(|contract| contract.canonical_command)
}

pub fn root_commands_for_help_visibility(visibility: &str) -> Vec<&'static str> {
    let mut entries = active_command_contract_entries()
        .iter()
        .copied()
        .filter(|entry| entry.path.len() == 1 && entry.contract.help_visibility == visibility)
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| (entry.contract.help_rank, entry.path[0]));
    entries.into_iter().map(|entry| entry.path[0]).collect()
}

pub fn root_commands_for_advanced_help() -> Vec<&'static str> {
    let mut entries = advanced_help_root_entries();
    entries.sort_by_key(|entry| (entry.contract.help_rank, entry.path[0]));
    entries.into_iter().map(|entry| entry.path[0]).collect()
}

/// Root commands on the advanced help surface, unsorted.
fn advanced_help_root_entries() -> Vec<&'static CommandContractEntry> {
    active_command_contract_entries()
        .iter()
        .copied()
        .filter(|entry| {
            entry.path.len() == 1
                && !matches!(entry.contract.help_visibility, "everyday" | "hidden")
        })
        .collect()
}

/// Area groups for the `heddle help advanced` listing, in render order,
/// as `(display title, verbs)` pairs (heddle#652). The grouping is
/// contract-table data, not a hand-maintained help string: native
/// advanced commands carry an explicit `help_category` on their
/// registration, while `automation` / `admin` / `git_adapter` commands
/// derive their group from the surface they already declare. Verbs keep
/// contract order (help_rank, then name) within each group — the same
/// ordering the flat list used. Feature-gated verbs absent from the
/// current build simply don't appear; a group may come back empty.
pub fn advanced_help_groups() -> Vec<(&'static str, Vec<&'static str>)> {
    const GROUPS: &[(&str, &str)] = &[
        ("threads", "Threads and integration"),
        ("states", "States and history"),
        ("collab", "Collaboration and review"),
        ("recovery", "Recovery and integrity"),
        ("repo", "Repo and environment"),
        ("automation", "Agents and automation"),
        ("git-interop", "Git interop"),
        ("admin", "Admin and maintenance"),
    ];
    let mut entries = advanced_help_root_entries();
    entries.sort_by_key(|entry| (entry.contract.help_rank, entry.path[0]));
    GROUPS
        .iter()
        .map(|(id, title)| {
            (
                *title,
                entries
                    .iter()
                    .filter(|entry| advanced_help_group_id(&entry.contract) == *id)
                    .map(|entry| entry.path[0])
                    .collect(),
            )
        })
        .collect()
}

/// Resolve which advanced-help group a root command belongs to. The
/// non-native surfaces are themselves the grouping; native commands use
/// the `help_category` set at registration. An unset category on a
/// native advanced command maps to "" (member of no group) — the
/// `advanced_help_groups_cover_every_advanced_verb` test turns that into
/// a build failure rather than a silently missing help line.
fn advanced_help_group_id(contract: &CommandContract) -> &'static str {
    match contract.surface {
        "automation" => "automation",
        "admin" => "admin",
        "git_adapter" => "git-interop",
        _ => contract.help_category.unwrap_or(""),
    }
}

pub(crate) fn recommended_action_template(action: &str) -> Option<ActionTemplate> {
    let trimmed = action.trim();
    if trimmed.is_empty() {
        return None;
    }
    RECOMMENDED_ACTION_TEMPLATES
        .iter()
        .find(|(template_action, _, _, _)| *template_action == trimmed)
        .map(
            |(template_action, argv_template, required_inputs, agent_may_fill)| {
                action_template_from_parts(
                    template_action,
                    argv_template,
                    required_inputs,
                    *agent_may_fill,
                )
            },
        )
        .or_else(|| dynamic_recommended_action_template(trimmed))
        .or_else(|| concrete_recommended_action_template(trimmed))
}

/// Fallback template for a concrete, placeholder-free recommended action
/// (e.g. `heddle status`). The template *is* the parsed argv with no inputs
/// left to fill — agents run `argv_template` verbatim. This makes
/// `recommended_action_template` total over every valid action so the
/// fillable `_template` is the single canonical machine shape and the
/// always-null `_argv` sibling could be dropped (HeddleCo/heddle#254).
///
/// Returns `None` for placeholder/display-only actions that lack a
/// registered structured template (they are invalid and surface upstream),
/// matching the previous parsed-argv contract.
fn concrete_recommended_action_template(action: &str) -> Option<ActionTemplate> {
    if RECOMMENDED_ACTION_PLACEHOLDERS.contains(&action) || is_display_only_template(action) {
        return None;
    }
    if validate_recommended_action(action).is_err() {
        return None;
    }
    let argv = split_recommended_action(action).ok()?;
    Some(action_template_from_owned(
        action.to_string(),
        argv,
        Vec::new(),
        false,
    ))
}

fn dynamic_recommended_action_template(action: &str) -> Option<ActionTemplate> {
    let argv = split_recommended_action(action).ok()?;
    if let Some(template) = dynamic_message_recommended_action_template(action, &argv) {
        return Some(template);
    }
    match argv.as_slice() {
        [heddle, clone, remote, path]
            if heddle == "heddle" && clone == "clone" && is_placeholder_arg(path) =>
        {
            Some(action_template_from_owned(
                action.to_string(),
                vec![
                    "heddle".to_string(),
                    "clone".to_string(),
                    remote.clone(),
                    path.clone(),
                ],
                vec![placeholder_input_name(path)],
                false,
            ))
        }
        [heddle, clone, remote, path, flag, thread]
            if heddle == "heddle"
                && clone == "clone"
                && flag == "--thread"
                && is_placeholder_arg(path) =>
        {
            Some(action_template_from_owned(
                action.to_string(),
                vec![
                    "heddle".to_string(),
                    "clone".to_string(),
                    remote.clone(),
                    path.clone(),
                    "--thread".to_string(),
                    thread.clone(),
                ],
                vec![placeholder_input_name(path)],
                false,
            ))
        }
        [heddle, start, thread_name, path_flag, path]
            if heddle == "heddle"
                && start == "start"
                && path_flag == "--path"
                && is_placeholder_arg(path) =>
        {
            let required_inputs = [thread_name, path]
                .iter()
                .filter(|arg| is_placeholder_arg(arg))
                .map(|arg| placeholder_input_name(arg))
                .collect();
            Some(action_template_from_owned(
                action.to_string(),
                vec![
                    "heddle".to_string(),
                    "start".to_string(),
                    thread_name.clone(),
                    "--path".to_string(),
                    path.clone(),
                ],
                required_inputs,
                true,
            ))
        }
        [heddle, thread_cmd, absorb, thread_name, into_flag, parent]
            if heddle == "heddle"
                && thread_cmd == "thread"
                && absorb == "absorb"
                && into_flag == "--into"
                && is_placeholder_arg(parent) =>
        {
            Some(action_template_from_owned(
                action.to_string(),
                vec![
                    "heddle".to_string(),
                    "thread".to_string(),
                    "absorb".to_string(),
                    thread_name.clone(),
                    "--into".to_string(),
                    parent.clone(),
                ],
                vec![placeholder_input_name(parent)],
                true,
            ))
        }
        _ => None,
    }
}

fn dynamic_message_recommended_action_template(
    action: &str,
    argv: &[String],
) -> Option<ActionTemplate> {
    match argv {
        [heddle, command, message_flag, message]
            if heddle == "heddle"
                && matches!(
                    command.as_str(),
                    "capture" | "checkpoint" | "commit" | "ready"
                )
                && is_message_flag(message_flag)
                && is_message_placeholder_arg(message) =>
        {
            Some(action_template_from_owned(
                action.to_string(),
                vec![
                    "heddle".to_string(),
                    command.clone(),
                    "-m".to_string(),
                    "<message>".to_string(),
                ],
                vec!["message".to_string()],
                true,
            ))
        }
        [
            heddle,
            capture,
            message_flag,
            message,
            confidence_flag,
            confidence,
        ] if heddle == "heddle"
            && capture == "capture"
            && is_message_flag(message_flag)
            && is_message_placeholder_arg(message)
            && confidence_flag == "--confidence"
            && is_placeholder_arg(confidence) =>
        {
            Some(action_template_from_owned(
                action.to_string(),
                vec![
                    "heddle".to_string(),
                    "capture".to_string(),
                    "-m".to_string(),
                    "<message>".to_string(),
                    "--confidence".to_string(),
                    confidence.clone(),
                ],
                vec!["message".to_string(), placeholder_input_name(confidence)],
                true,
            ))
        }
        // The confidence/verification policy-blocker recovery scopes itself to
        // the thread's checkout via the global `--repo <path>` flag (heddle#464).
        // `<path>` is a concrete worktree path, not a placeholder, so the only
        // fillable inputs remain the message and confidence.
        [
            heddle,
            repo_flag,
            repo_path,
            command,
            message_flag,
            message,
            confidence_flag,
            confidence,
        ] if heddle == "heddle"
            && repo_flag == "--repo"
            && matches!(command.as_str(), "capture" | "commit")
            && is_message_flag(message_flag)
            && is_message_placeholder_arg(message)
            && confidence_flag == "--confidence"
            && is_placeholder_arg(confidence) =>
        {
            Some(action_template_from_owned(
                action.to_string(),
                vec![
                    "heddle".to_string(),
                    "--repo".to_string(),
                    repo_path.clone(),
                    command.clone(),
                    "-m".to_string(),
                    "<message>".to_string(),
                    "--confidence".to_string(),
                    confidence.clone(),
                ],
                vec!["message".to_string(), placeholder_input_name(confidence)],
                true,
            ))
        }
        [heddle, stash, push, message_flag, message]
            if heddle == "heddle"
                && stash == "stash"
                && push == "push"
                && is_message_flag(message_flag)
                && is_message_placeholder_arg(message) =>
        {
            Some(action_template_from_owned(
                action.to_string(),
                vec![
                    "heddle".to_string(),
                    "stash".to_string(),
                    "push".to_string(),
                    "-m".to_string(),
                    "<message>".to_string(),
                ],
                vec!["message".to_string()],
                true,
            ))
        }
        _ => None,
    }
}

fn is_message_flag(value: &str) -> bool {
    value == "-m" || value == "--message"
}

fn is_message_placeholder_arg(value: &str) -> bool {
    matches!(value, "..." | "…") || value == "<message>"
}

fn is_placeholder_arg(value: &str) -> bool {
    value.starts_with('<') && value.ends_with('>') && value.len() > 2
}

fn placeholder_input_name(value: &str) -> String {
    value
        .trim_start_matches('<')
        .trim_end_matches('>')
        .replace('-', "_")
}

fn help_visibility_to_tier(help_visibility: &str) -> &'static str {
    match help_visibility {
        "everyday" => "everyday",
        "hidden" => "hidden",
        _ => "advanced",
    }
}

pub fn observe_only_root_commands() -> Vec<&'static str> {
    active_command_contract_entries()
        .iter()
        .copied()
        .filter(|entry| {
            entry.path.len() == 1 && entry.contract.observe_only && !entry.contract.mutates
        })
        .map(|entry| entry.path[0])
        .collect()
}

pub fn command_contract_root_commands() -> Vec<&'static str> {
    active_command_contract_entries()
        .iter()
        .copied()
        .filter(|entry| entry.path.len() == 1)
        .map(|entry| entry.path[0])
        .collect()
}

pub(crate) fn validate_recommended_action(action: &str) -> std::result::Result<(), String> {
    let trimmed = action.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    if RECOMMENDED_ACTION_PLACEHOLDERS.contains(&trimmed) {
        return recommended_action_template(trimmed)
            .map(|_| ())
            .ok_or_else(|| {
                format!(
                    "recommended action placeholder `{trimmed}` must have a structured template"
                )
            });
    }
    if is_display_only_template(trimmed) {
        return recommended_action_template(trimmed).map(|_| ()).ok_or_else(|| {
            format!(
                "display-only recommended action `{trimmed}` must be registered as a structured template"
            )
        });
    }

    let argv = split_recommended_action(trimmed)?;
    match argv.first().map(String::as_str) {
        Some("heddle") => Cli::command()
            .try_get_matches_from(argv)
            .map(|_| ())
            .map_err(|err| err.to_string()),
        Some(other) => Err(format!(
            "recommended action must start with `heddle` or be registered as a placeholder, found `{other}`"
        )),
        None => Ok(()),
    }
}

fn is_display_only_template(action: &str) -> bool {
    action.contains("...") || action.contains('…') || (action.contains('<') && action.contains('>'))
}

pub(crate) fn split_recommended_action(action: &str) -> std::result::Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut chars = action.chars().peekable();
    let mut in_single_quote = false;
    let mut in_double_quote = false;

    while let Some(ch) = chars.next() {
        match (ch, in_single_quote, in_double_quote) {
            ('\'', false, false) => in_single_quote = true,
            ('\'', true, false) => in_single_quote = false,
            ('"', false, false) => in_double_quote = true,
            ('"', false, true) => in_double_quote = false,
            ('\\', false, _) => match chars.next() {
                Some(next) => current.push(next),
                None => current.push('\\'),
            },
            (ch, false, false) if ch.is_whitespace() => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            (ch, _, _) => current.push(ch),
        }
    }

    if in_single_quote {
        return Err("unterminated single quote in recommended action".to_string());
    }
    if in_double_quote {
        return Err("unterminated double quote in recommended action".to_string());
    }
    if !current.is_empty() {
        args.push(current);
    }
    Ok(args)
}

pub(crate) fn schema_verbs() -> Vec<&'static str> {
    let mut verbs = collect_schema_verbs(|contract| contract.schema_verbs);
    verbs.push("error");
    verbs
}

pub(crate) fn documented_schema_verbs() -> Vec<&'static str> {
    let mut verbs = collect_schema_verbs(|contract| contract.documented_schema_verbs);
    verbs.push("error");
    verbs
}

pub(crate) fn opaque_schema_verbs() -> Vec<&'static str> {
    collect_schema_verbs(|contract| contract.opaque_schema_verbs)
}

fn collect_schema_verbs(
    select: impl Fn(CommandContract) -> &'static [&'static str],
) -> Vec<&'static str> {
    let mut verbs = Vec::new();
    for entry in active_command_contract_entries().iter().copied() {
        for verb in select(entry.contract) {
            if !verbs.contains(verb) {
                verbs.push(*verb);
            }
        }
    }
    verbs
}

pub fn command_path(command: &Commands) -> Vec<&'static str> {
    match command {
        Commands::Init(_) => vec!["init"],
        Commands::Adopt(_) => vec!["adopt"],
        Commands::Help { .. } => vec!["help"],
        Commands::Status { .. } => vec!["status"],
        Commands::Watch(_) => vec!["watch"],
        Commands::Verify => vec!["verify"],
        Commands::Doctor(args) => match &args.command {
            None => vec!["doctor"],
            Some(DoctorCommands::Docs(_)) => vec!["doctor", "docs"],
            Some(DoctorCommands::Schemas) => vec!["doctor", "schemas"],
        },
        #[cfg(feature = "git-overlay")]
        Commands::GitOverlay => vec!["git-overlay"],
        Commands::Schemas { .. } => vec!["schemas"],
        Commands::Commands(_) => vec!["commands"],
        Commands::Start(_) => vec!["start"],
        Commands::Try(_) => vec!["try"],
        Commands::Attempt(_) => vec!["attempt"],
        Commands::Run(_) => vec!["run"],
        Commands::Sync(_) => vec!["sync"],
        Commands::Continue => vec!["continue"],
        Commands::Abort => vec!["abort"],
        Commands::Land(_) => vec!["land"],
        Commands::Delegate(_) => vec!["delegate"],
        Commands::Ready(_) => vec!["ready"],
        Commands::Capture(_) => vec!["capture"],
        Commands::Commit(_) => vec!["commit"],
        Commands::Checkpoint(_) => vec!["checkpoint"],
        Commands::Log(_) => vec!["log"],
        Commands::Show { .. } => vec!["show"],
        Commands::Retro(_) => vec!["retro"],
        Commands::Inspect { .. } => vec!["inspect"],
        Commands::Goto { .. } => vec!["goto"],
        Commands::Clean { .. } => vec!["clean"],
        Commands::Diff(_) => vec!["diff"],
        Commands::Branch(_) => vec!["branch"],
        Commands::Switch(_) => vec!["switch"],
        Commands::Discuss { command } => match command {
            DiscussCommands::Open(_) => vec!["discuss", "open"],
            DiscussCommands::Append(_) => vec!["discuss", "append"],
            DiscussCommands::Resolve(_) => vec!["discuss", "resolve"],
            DiscussCommands::List(_) => vec!["discuss", "list"],
            DiscussCommands::Show(_) => vec!["discuss", "show"],
        },
        Commands::Query(_) => vec!["query"],
        Commands::Transaction { command } => match command {
            TransactionCommands::Begin(_) => vec!["transaction", "begin"],
            TransactionCommands::Commit(_) => vec!["transaction", "commit"],
            TransactionCommands::Abort(_) => vec!["transaction", "abort"],
            TransactionCommands::Status(_) => vec!["transaction", "status"],
        },
        Commands::Conflict { command } => match command {
            ConflictCommands::List => vec!["conflict", "list"],
            ConflictCommands::Show(_) => vec!["conflict", "show"],
        },
        Commands::Review { command } => match command {
            ReviewCommands::Show(_) => vec!["review", "show"],
            ReviewCommands::Sign(_) => vec!["review", "sign"],
            ReviewCommands::Next(_) => vec!["review", "next"],
            ReviewCommands::Health(_) => vec!["review", "health"],
        },
        Commands::Redact { command } => match command {
            RedactCommands::Apply(_) => vec!["redact", "apply"],
            RedactCommands::List(_) => vec!["redact", "list"],
            RedactCommands::Show(_) => vec!["redact", "show"],
            RedactCommands::Trust(command) => match command {
                RedactTrustCommands::Add(_) => vec!["redact", "trust", "add"],
                RedactTrustCommands::List(_) => vec!["redact", "trust", "list"],
                RedactTrustCommands::Remove(_) => vec!["redact", "trust", "remove"],
            },
        },
        Commands::Purge { command } => match command {
            PurgeCommands::Apply(_) => vec!["purge", "apply"],
            PurgeCommands::List(_) => vec!["purge", "list"],
        },
        Commands::Visibility { command } => match command {
            VisibilityCommands::Set(_) => vec!["visibility", "set"],
            VisibilityCommands::Promote(_) => vec!["visibility", "promote"],
            VisibilityCommands::Show(_) => vec!["visibility", "show"],
            VisibilityCommands::List(_) => vec!["visibility", "list"],
        },
        Commands::Revert(_) => vec!["revert"],
        Commands::Undo(_) => vec!["undo"],
        Commands::Redo(_) => vec!["redo"],
        Commands::Fork { .. } => vec!["fork"],
        Commands::Collapse(_) => vec!["collapse"],
        Commands::Marker { command } => match command {
            MarkerCommands::List { .. } => vec!["marker", "list"],
            MarkerCommands::Create { .. } => vec!["marker", "create"],
            MarkerCommands::Delete { .. } => vec!["marker", "delete"],
            MarkerCommands::Show { .. } => vec!["marker", "show"],
        },
        Commands::Thread { command } => match command {
            ThreadCommands::Create { .. } => vec!["thread", "create"],
            ThreadCommands::Current => vec!["thread", "current"],
            ThreadCommands::Switch { .. } => vec!["thread", "switch"],
            ThreadCommands::Cd { .. } => vec!["thread", "cd"],
            ThreadCommands::List(_) => vec!["thread", "list"],
            ThreadCommands::Show(_) => vec!["thread", "show"],
            ThreadCommands::Captures(_) => vec!["thread", "captures"],
            ThreadCommands::Rename(_) => vec!["thread", "rename"],
            ThreadCommands::Refresh(_) => vec!["thread", "refresh"],
            ThreadCommands::Move(_) => vec!["thread", "move"],
            ThreadCommands::Absorb(_) => vec!["thread", "absorb"],
            ThreadCommands::Resolve(_) => vec!["thread", "resolve"],
            ThreadCommands::Promote(_) => vec!["thread", "promote"],
            ThreadCommands::Drop(_) => vec!["thread", "drop"],
            ThreadCommands::Approve(_) => vec!["thread", "approve"],
            ThreadCommands::Approvals(_) => vec!["thread", "approvals"],
            ThreadCommands::RevokeApproval(_) => vec!["thread", "revoke-approval"],
            ThreadCommands::CheckMerge(_) => vec!["thread", "check-merge"],
            ThreadCommands::Cleanup(_) => vec!["thread", "cleanup"],
        },
        Commands::Shell { command } => match command {
            ShellCommands::Init { .. } => vec!["shell", "init"],
            ShellCommands::Completion { .. } => vec!["shell", "completion"],
        },
        Commands::Workspace { command } => match command {
            None => vec!["workspace"],
            Some(WorkspaceCommands::Show(_)) => vec!["workspace", "show"],
        },
        Commands::Merge(_) => vec!["merge"],
        Commands::Stack(args) => match &args.command {
            None => vec!["stack"],
            Some(StackCommands::Ready { .. }) => vec!["stack", "ready"],
            Some(StackCommands::Snapshot { .. }) => vec!["stack", "snapshot"],
        },
        Commands::Resolve(_) => vec!["resolve"],
        Commands::Fsck { .. } => vec!["fsck"],
        Commands::Fetch { .. } => vec!["fetch"],
        Commands::Push(_) => vec!["push"],
        Commands::Pull(_) => vec!["pull"],
        Commands::Remote { command } => match command {
            RemoteCommands::List => vec!["remote", "list"],
            RemoteCommands::Add { .. } => vec!["remote", "add"],
            RemoteCommands::Remove { .. } => vec!["remote", "remove"],
            RemoteCommands::SetDefault { .. } => vec!["remote", "set-default"],
            RemoteCommands::Show { .. } => vec!["remote", "show"],
        },
        #[cfg(feature = "client")]
        Commands::Auth { command } => match command {
            AuthCommands::Login { .. } => vec!["auth", "login"],
            AuthCommands::Logout { .. } => vec!["auth", "logout"],
            AuthCommands::Status { .. } => vec!["auth", "status"],
            AuthCommands::CreateServiceToken { .. } => vec!["auth", "create-service-token"],
        },
        Commands::Context { command } => match command {
            ContextCommands::Set(_) => vec!["context", "set"],
            ContextCommands::Get(_) => vec!["context", "get"],
            ContextCommands::List(_) => vec!["context", "list"],
            ContextCommands::History(_) => vec!["context", "history"],
            ContextCommands::Edit(_) => vec!["context", "edit"],
            ContextCommands::Supersede(_) => vec!["context", "supersede"],
            ContextCommands::Rm(_) => vec!["context", "rm"],
            ContextCommands::Check(_) => vec!["context", "check"],
            ContextCommands::Suggest(_) => vec!["context", "suggest"],
            ContextCommands::Audit(_) => vec!["context", "audit"],
        },
        Commands::Integration { command } => match command {
            IntegrationCommands::List => vec!["integration", "list"],
            IntegrationCommands::Install(_) => vec!["integration", "install"],
            IntegrationCommands::Doctor => vec!["integration", "doctor"],
            IntegrationCommands::Uninstall(_) => vec!["integration", "uninstall"],
            IntegrationCommands::Upgrade(_) => vec!["integration", "upgrade"],
            IntegrationCommands::Relay(_) => vec!["integration", "relay"],
        },
        Commands::Stash { command } => match command {
            StashCommands::Push { .. } => vec!["stash", "push"],
            StashCommands::List => vec!["stash", "list"],
            StashCommands::Pop => vec!["stash", "pop"],
            StashCommands::Apply => vec!["stash", "apply"],
            StashCommands::Drop => vec!["stash", "drop"],
            StashCommands::Clear => vec!["stash", "clear"],
            StashCommands::Show => vec!["stash", "show"],
        },
        #[cfg(feature = "client")]
        Commands::Support { command } => match command {
            SupportCommands::Grant(_) => vec!["support", "grant"],
            SupportCommands::List(_) => vec!["support", "list"],
            SupportCommands::Revoke(_) => vec!["support", "revoke"],
        },
        #[cfg(feature = "git-overlay")]
        Commands::Bridge { command } => match command {
            BridgeCommands::Git { command } => match command {
                GitCommands::Status => vec!["bridge", "git", "status"],
                GitCommands::Init { .. } => vec!["bridge", "git", "init"],
                GitCommands::Export { .. } => vec!["bridge", "git", "export"],
                GitCommands::Import { .. } => vec!["bridge", "git", "import"],
                GitCommands::Sync { .. } => vec!["bridge", "git", "sync"],
                GitCommands::Reconcile { .. } => vec!["bridge", "git", "reconcile"],
                GitCommands::Push { .. } => vec!["bridge", "git", "push"],
                GitCommands::Pull { .. } => vec!["bridge", "git", "pull"],
                #[cfg(feature = "ingest")]
                GitCommands::Ingest { .. } => vec!["bridge", "git", "ingest"],
                #[cfg(feature = "ingest")]
                GitCommands::Reason { .. } => vec!["bridge", "git", "reason"],
            },
            BridgeCommands::BackfillFidelity => vec!["bridge", "backfill-fidelity"],
        },
        #[cfg(feature = "semantic")]
        Commands::Semantic { command } => match command {
            SemanticCommands::Hot { .. } => vec!["semantic", "hot"],
        },
        Commands::Daemon { command } => match command {
            DaemonCommands::Serve => vec!["daemon", "serve"],
            DaemonCommands::Status => vec!["daemon", "status"],
            DaemonCommands::Stop => vec!["daemon", "stop"],
        },
        Commands::Agent { command } => match command {
            AgentCommands::Serve(_) => vec!["agent", "serve"],
            AgentCommands::Status => vec!["agent", "status"],
            AgentCommands::Stop => vec!["agent", "stop"],
            AgentCommands::Reserve(_) => vec!["agent", "reserve"],
            AgentCommands::Heartbeat(_) => vec!["agent", "heartbeat"],
            AgentCommands::Capture(_) => vec!["agent", "capture"],
            AgentCommands::Ready(_) => vec!["agent", "ready"],
            AgentCommands::Release(_) => vec!["agent", "release"],
            AgentCommands::List(_) => vec!["agent", "list"],
        },
        Commands::Maintenance { command } => match command {
            MaintenanceCommands::Inspect => vec!["maintenance", "inspect"],
            MaintenanceCommands::Run => vec!["maintenance", "run"],
            MaintenanceCommands::Gc { .. } => vec!["maintenance", "gc"],
            MaintenanceCommands::Index { .. } => vec!["maintenance", "index"],
            MaintenanceCommands::Monitor { .. } => vec!["maintenance", "monitor"],
        },
        Commands::Blame { .. } => vec!["blame"],
        Commands::CherryPick { .. } => vec!["cherry-pick"],
        Commands::Clone(_) => vec!["clone"],
        Commands::Rebase { .. } => vec!["rebase"],
        Commands::Hook { command } => match command {
            HookCommands::List => vec!["hook", "list"],
            HookCommands::Install { .. } => vec!["hook", "install"],
            HookCommands::Uninstall { .. } => vec!["hook", "uninstall"],
            HookCommands::Events { .. } => vec!["hook", "events"],
        },
        Commands::HarnessBridge => vec!["harness-bridge"],
        Commands::Actor { command } => match command {
            ActorCommands::Spawn(_) => vec!["actor", "spawn"],
            ActorCommands::List(_) => vec!["actor", "list"],
            ActorCommands::Show(_) => vec!["actor", "show"],
            ActorCommands::Explain(_) => vec!["actor", "explain"],
            ActorCommands::Done(_) => vec!["actor", "done"],
        },
        Commands::Session { command } => match command {
            SessionCommands::Start(_) => vec!["session", "start"],
            SessionCommands::Segment(_) => vec!["session", "segment"],
            SessionCommands::End(_) => vec!["session", "end"],
            SessionCommands::Show(_) => vec!["session", "show"],
            SessionCommands::List(_) => vec!["session", "list"],
        },
        #[cfg(feature = "client")]
        Commands::Presence { command } => match command {
            PresenceCommands::Publish { .. } => vec!["presence", "publish"],
        },
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use clap::Parser;

    use super::*;

    struct RuntimeContractParseSample {
        path: &'static [&'static str],
        argv_tail: &'static [&'static str],
    }

    // Representative parseable invocations for every runtime leaf command in
    // CONTRACTS. The only intentionally skipped rows are non-runtime grouping
    // contracts whose Clap variants require a subcommand, e.g. `thread`,
    // `bridge git`, and `redact trust`.
    const RUNTIME_CONTRACT_PARSE_SAMPLES: &[RuntimeContractParseSample] = &[
        sample(&["abort"], &["abort"]),
        sample(&["adopt"], &["adopt"]),
        sample(&["actor", "spawn"], &["actor", "spawn"]),
        sample(&["actor", "list"], &["actor", "list"]),
        sample(&["actor", "show"], &["actor", "show"]),
        sample(&["actor", "explain"], &["actor", "explain"]),
        sample(&["actor", "done"], &["actor", "done"]),
        sample(&["agent", "serve"], &["agent", "serve"]),
        sample(&["agent", "status"], &["agent", "status"]),
        sample(&["agent", "stop"], &["agent", "stop"]),
        sample(
            &["agent", "reserve"],
            &["agent", "reserve", "--thread", "main"],
        ),
        sample(
            &["agent", "heartbeat"],
            &["agent", "heartbeat", "--session", "session-1"],
        ),
        sample(
            &["agent", "capture"],
            &["agent", "capture", "--session", "session-1"],
        ),
        sample(
            &["agent", "ready"],
            &["agent", "ready", "--session", "session-1"],
        ),
        sample(
            &["agent", "release"],
            &["agent", "release", "--session", "session-1"],
        ),
        sample(&["agent", "list"], &["agent", "list"]),
        sample(&["attempt"], &["attempt", "1", "true"]),
        sample(&["blame"], &["blame", "src/lib.rs"]),
        sample(&["branch"], &["branch"]),
        #[cfg(feature = "git-overlay")]
        sample(
            &["bridge", "backfill-fidelity"],
            &["bridge", "backfill-fidelity"],
        ),
        #[cfg(feature = "git-overlay")]
        sample(&["bridge", "git", "status"], &["bridge", "git", "status"]),
        #[cfg(feature = "git-overlay")]
        sample(&["bridge", "git", "init"], &["bridge", "git", "init"]),
        #[cfg(feature = "git-overlay")]
        sample(&["bridge", "git", "export"], &["bridge", "git", "export"]),
        #[cfg(feature = "git-overlay")]
        sample(&["bridge", "git", "import"], &["bridge", "git", "import"]),
        #[cfg(feature = "git-overlay")]
        sample(&["bridge", "git", "sync"], &["bridge", "git", "sync"]),
        #[cfg(feature = "git-overlay")]
        sample(
            &["bridge", "git", "reconcile"],
            &[
                "bridge",
                "git",
                "reconcile",
                "--prefer",
                "heddle",
                "--ref",
                "main",
            ],
        ),
        #[cfg(feature = "git-overlay")]
        sample(&["bridge", "git", "push"], &["bridge", "git", "push"]),
        #[cfg(feature = "git-overlay")]
        sample(&["bridge", "git", "pull"], &["bridge", "git", "pull"]),
        #[cfg(all(feature = "git-overlay", feature = "ingest"))]
        sample(
            &["bridge", "git", "ingest"],
            &["bridge", "git", "ingest", "--path", "."],
        ),
        #[cfg(all(feature = "git-overlay", feature = "ingest"))]
        sample(
            &["bridge", "git", "reason"],
            &["bridge", "git", "reason", "--path", "."],
        ),
        sample(&["capture"], &["capture"]),
        sample(&["checkpoint"], &["checkpoint"]),
        sample(&["cherry-pick"], &["cherry-pick", "abc123"]),
        sample(&["clean"], &["clean"]),
        sample(&["clone"], &["clone", "remote", "local"]),
        sample(
            &["collapse"],
            &["collapse", "s1", "s2", "--into", "squashed"],
        ),
        sample(&["commit"], &["commit"]),
        sample(&["commands"], &["commands"]),
        sample(&["conflict", "list"], &["conflict", "list"]),
        sample(&["conflict", "show"], &["conflict", "show", "conflict-1"]),
        sample(&["continue"], &["continue"]),
        sample(
            &["context", "set"],
            &["context", "set", "--path", "src/lib.rs"],
        ),
        sample(
            &["context", "get"],
            &["context", "get", "--path", "src/lib.rs"],
        ),
        sample(&["context", "list"], &["context", "list"]),
        sample(&["context", "history"], &["context", "history", "ctx-1"]),
        sample(&["context", "edit"], &["context", "edit", "ctx-1"]),
        sample(
            &["context", "supersede"],
            &["context", "supersede", "ctx-1", "--path", "src/lib.rs"],
        ),
        sample(
            &["context", "rm"],
            &["context", "rm", "--path", "src/lib.rs"],
        ),
        sample(&["context", "check"], &["context", "check"]),
        sample(&["context", "suggest"], &["context", "suggest"]),
        sample(&["context", "audit"], &["context", "audit"]),
        sample(&["daemon", "serve"], &["daemon", "serve"]),
        sample(&["daemon", "status"], &["daemon", "status"]),
        sample(&["daemon", "stop"], &["daemon", "stop"]),
        sample(&["delegate"], &["delegate", "task"]),
        sample(&["diff"], &["diff"]),
        sample(
            &["discuss", "open"],
            &["discuss", "open", "src/lib.rs", "symbol", "body"],
        ),
        sample(
            &["discuss", "append"],
            &["discuss", "append", "discussion-1", "body"],
        ),
        sample(
            &["discuss", "resolve"],
            &["discuss", "resolve", "discussion-1", "--mode", "dismiss"],
        ),
        sample(&["discuss", "list"], &["discuss", "list"]),
        sample(&["discuss", "show"], &["discuss", "show", "discussion-1"]),
        sample(&["doctor"], &["doctor"]),
        sample(&["doctor", "docs"], &["doctor", "docs"]),
        sample(&["doctor", "schemas"], &["doctor", "schemas"]),
        sample(&["fetch"], &["fetch"]),
        sample(&["fork"], &["fork"]),
        sample(&["fsck"], &["fsck"]),
        #[cfg(feature = "git-overlay")]
        sample(&["git-overlay"], &["git-overlay"]),
        sample(&["goto"], &["goto", "HEAD"]),
        sample(&["harness-bridge"], &["harness-bridge"]),
        sample(&["help"], &["help"]),
        sample(&["hook", "list"], &["hook", "list"]),
        sample(&["hook", "install"], &["hook", "install", "pre-snapshot"]),
        sample(
            &["hook", "uninstall"],
            &["hook", "uninstall", "pre-snapshot"],
        ),
        sample(&["hook", "events"], &["hook", "events"]),
        sample(&["init"], &["init"]),
        sample(&["inspect"], &["inspect"]),
        sample(&["integration", "list"], &["integration", "list"]),
        sample(&["integration", "install"], &["integration", "install"]),
        sample(&["integration", "doctor"], &["integration", "doctor"]),
        sample(&["integration", "uninstall"], &["integration", "uninstall"]),
        sample(&["integration", "upgrade"], &["integration", "upgrade"]),
        sample(
            &["integration", "relay"],
            &["integration", "relay", "codex", "agent_done"],
        ),
        sample(&["log"], &["log"]),
        sample(&["maintenance", "inspect"], &["maintenance", "inspect"]),
        sample(&["maintenance", "run"], &["maintenance", "run"]),
        sample(&["maintenance", "gc"], &["maintenance", "gc"]),
        sample(&["maintenance", "index"], &["maintenance", "index"]),
        sample(&["maintenance", "monitor"], &["maintenance", "monitor"]),
        sample(&["marker", "list"], &["marker", "list"]),
        sample(&["marker", "create"], &["marker", "create", "mark-1"]),
        sample(&["marker", "delete"], &["marker", "delete", "mark-1"]),
        sample(&["marker", "show"], &["marker", "show", "mark-1"]),
        sample(&["merge"], &["merge", "feature"]),
        sample(&["pull"], &["pull"]),
        sample(
            &["purge", "apply"],
            &["purge", "apply", "HEAD", "--path", "secret.txt", "--force"],
        ),
        sample(&["purge", "list"], &["purge", "list"]),
        sample(&["push"], &["push"]),
        sample(&["query"], &["query"]),
        sample(&["ready"], &["ready"]),
        sample(&["rebase"], &["rebase"]),
        sample(&["stack"], &["stack"]),
        sample(&["stack", "ready"], &["stack", "ready"]),
        sample(&["stack", "snapshot"], &["stack", "snapshot"]),
        sample(
            &["redact", "apply"],
            &[
                "redact",
                "apply",
                "HEAD",
                "--path",
                "secret.txt",
                "--reason",
                "secret",
            ],
        ),
        sample(&["redact", "list"], &["redact", "list"]),
        sample(&["redact", "show"], &["redact", "show", "redaction-1"]),
        sample(
            &["redact", "trust", "add"],
            &[
                "redact",
                "trust",
                "add",
                "--algorithm",
                "ed25519",
                "--public-key",
                "abcd",
            ],
        ),
        sample(&["redact", "trust", "list"], &["redact", "trust", "list"]),
        sample(
            &["redact", "trust", "remove"],
            &["redact", "trust", "remove", "abcd"],
        ),
        sample(&["redo"], &["redo"]),
        sample(&["remote", "list"], &["remote", "list"]),
        sample(
            &["remote", "add"],
            &["remote", "add", "origin", "localhost:9418"],
        ),
        sample(&["remote", "remove"], &["remote", "remove", "origin"]),
        sample(
            &["remote", "set-default"],
            &["remote", "set-default", "origin"],
        ),
        sample(&["remote", "show"], &["remote", "show", "origin"]),
        sample(&["resolve"], &["resolve"]),
        sample(&["retro"], &["retro"]),
        sample(&["revert"], &["revert", "HEAD"]),
        sample(&["review", "show"], &["review", "show"]),
        sample(
            &["review", "sign"],
            &[
                "review",
                "sign",
                "HEAD",
                "--kind",
                "read",
                "--public-key",
                "abcd",
                "--signature",
                "ef01",
                "--signed-at-unix",
                "1",
            ],
        ),
        sample(&["review", "next"], &["review", "next"]),
        sample(&["review", "health"], &["review", "health"]),
        sample(&["run"], &["run", "true"]),
        sample(&["schemas"], &["schemas"]),
        #[cfg(feature = "semantic")]
        sample(&["semantic", "hot"], &["semantic", "hot"]),
        sample(
            &["session", "start"],
            &[
                "session",
                "start",
                "--provider",
                "openai",
                "--model",
                "gpt-5",
            ],
        ),
        sample(
            &["session", "segment"],
            &[
                "session",
                "segment",
                "--provider",
                "openai",
                "--model",
                "gpt-5",
            ],
        ),
        sample(&["session", "end"], &["session", "end"]),
        sample(&["session", "show"], &["session", "show"]),
        sample(&["session", "list"], &["session", "list"]),
        sample(&["shell", "init"], &["shell", "init", "bash"]),
        sample(&["shell", "completion"], &["shell", "completion", "bash"]),
        sample(&["land"], &["land"]),
        sample(&["show"], &["show", "HEAD"]),
        sample(&["start"], &["start", "feature"]),
        sample(&["stash", "push"], &["stash", "push"]),
        sample(&["stash", "list"], &["stash", "list"]),
        sample(&["stash", "pop"], &["stash", "pop"]),
        sample(&["stash", "apply"], &["stash", "apply"]),
        sample(&["stash", "drop"], &["stash", "drop"]),
        sample(&["stash", "clear"], &["stash", "clear"]),
        sample(&["stash", "show"], &["stash", "show"]),
        sample(&["status"], &["status"]),
        sample(&["switch"], &["switch", "main"]),
        sample(&["sync"], &["sync"]),
        sample(&["thread", "create"], &["thread", "create", "feature"]),
        sample(&["thread", "current"], &["thread", "current"]),
        sample(&["thread", "switch"], &["thread", "switch", "feature"]),
        sample(&["thread", "cd"], &["thread", "cd", "feature"]),
        sample(&["thread", "list"], &["thread", "list"]),
        sample(&["thread", "show"], &["thread", "show"]),
        sample(&["thread", "captures"], &["thread", "captures"]),
        sample(
            &["thread", "rename"],
            &["thread", "rename", "old-feature", "new-feature"],
        ),
        sample(&["thread", "refresh"], &["thread", "refresh", "feature"]),
        sample(
            &["thread", "move"],
            &["thread", "move", "source", "dest", "--path", "src/lib.rs"],
        ),
        sample(&["thread", "absorb"], &["thread", "absorb", "feature"]),
        sample(&["thread", "resolve"], &["thread", "resolve", "feature"]),
        sample(&["thread", "promote"], &["thread", "promote", "feature"]),
        sample(&["thread", "drop"], &["thread", "drop", "feature"]),
        sample(
            &["thread", "approve"],
            &["thread", "approve", "source", "target"],
        ),
        sample(
            &["thread", "approvals"],
            &["thread", "approvals", "source", "target"],
        ),
        sample(
            &["thread", "revoke-approval"],
            &[
                "thread",
                "revoke-approval",
                "00000000-0000-0000-0000-000000000000",
            ],
        ),
        sample(
            &["thread", "check-merge"],
            &["thread", "check-merge", "source", "target"],
        ),
        sample(&["thread", "cleanup"], &["thread", "cleanup", "--merged"]),
        sample(&["transaction", "begin"], &["transaction", "begin"]),
        sample(
            &["transaction", "commit"],
            &["transaction", "commit", "tx-1"],
        ),
        sample(&["transaction", "abort"], &["transaction", "abort", "tx-1"]),
        sample(
            &["transaction", "status"],
            &["transaction", "status", "tx-1"],
        ),
        sample(&["verify"], &["verify"]),
        sample(
            &["visibility", "set"],
            &["visibility", "set", "HEAD", "--tier", "internal"],
        ),
        sample(
            &["visibility", "promote"],
            &["visibility", "promote", "HEAD", "--tier", "internal"],
        ),
        sample(&["visibility", "show"], &["visibility", "show", "HEAD"]),
        sample(&["visibility", "list"], &["visibility", "list"]),
        sample(&["try"], &["try", "true"]),
        sample(&["undo"], &["undo"]),
        sample(&["watch"], &["watch"]),
        sample(&["workspace"], &["workspace"]),
        sample(&["workspace", "show"], &["workspace", "show"]),
    ];

    const fn sample(
        path: &'static [&'static str],
        argv_tail: &'static [&'static str],
    ) -> RuntimeContractParseSample {
        RuntimeContractParseSample { path, argv_tail }
    }

    #[test]
    fn recommended_actions_parse_through_clap_or_registered_placeholders() {
        for action in [
            "",
            "heddle init",
            "heddle capture -m \"...\"",
            "heddle commit -m \"...\"",
            "heddle stash push -m \"...\"",
            "heddle capture -m \"Preserve raw Git operation work\"",
            "heddle switch <branch>",
            "heddle start feature/auth --path <dir>",
            "heddle clone <remote> <fresh-path>",
            "heddle clone <local-path> <path>",
            "heddle clone /tmp/source <path> --thread main",
            "heddle bridge git import --path <full-git-repo> --ref <ref>",
            "heddle thread promote main",
            "heddle thread resolve main",
        ] {
            validate_recommended_action(action)
                .unwrap_or_else(|err| panic!("expected `{action}` to validate: {err}"));
        }
        #[cfg(feature = "git-overlay")]
        {
            for action in [
                "heddle bridge git import --ref main",
                "heddle bridge git import --ref origin/main",
                "heddle merge origin/main --preview",
                "heddle bridge git reconcile --ref main --preview",
                "heddle bridge git reconcile --prefer heddle --ref main --preview",
            ] {
                validate_recommended_action(action)
                    .unwrap_or_else(|err| panic!("expected `{action}` to validate: {err}"));
            }
        }
    }

    #[test]
    fn recommended_action_templates_describe_display_only_placeholders() {
        let catalog = build_command_catalog();
        for placeholder in RECOMMENDED_ACTION_PLACEHOLDERS {
            assert!(
                recommended_action_template(placeholder).is_some(),
                "placeholder `{placeholder}` must have a structured template"
            );
        }
        for template in &catalog.recommended_action_templates {
            validate_recommended_action(&template.action).unwrap_or_else(|err| {
                panic!(
                    "recommended action template `{}` must validate: {err}",
                    template.action
                )
            });
        }

        let commit = catalog
            .recommended_action_templates
            .iter()
            .find(|template| template.action == "heddle commit -m \"...\"")
            .expect("commit placeholder should have a structured template");
        assert_eq!(
            commit.argv_template,
            vec!["heddle", "commit", "-m", "<message>"]
        );
        assert_eq!(commit.required_inputs, vec!["message"]);
        assert!(commit.agent_may_fill);

        let template = recommended_action_template("heddle checkpoint -m \"...\"")
            .expect("checkpoint placeholder should resolve");
        assert_eq!(
            template.argv_template,
            vec!["heddle", "checkpoint", "-m", "<message>"]
        );

        let switch = recommended_action_template("heddle switch <branch>")
            .expect("switch placeholder should resolve");
        assert_eq!(switch.argv_template, vec!["heddle", "switch", "<branch>"]);
        assert_eq!(switch.required_inputs, vec!["branch"]);
        assert!(!switch.agent_may_fill);

        let clone = recommended_action_template("heddle clone <remote> <fresh-path>")
            .expect("clone recovery placeholder should resolve");
        assert_eq!(
            clone.argv_template,
            vec!["heddle", "clone", "<remote>", "<fresh-path>"]
        );
        assert_eq!(clone.required_inputs, vec!["remote", "path"]);
        assert!(!clone.agent_may_fill);

        let start = recommended_action_template("heddle start feature/auth --path <dir>")
            .expect("start path placeholder should resolve");
        assert_eq!(
            start.argv_template,
            vec!["heddle", "start", "feature/auth", "--path", "<dir>"]
        );
        assert_eq!(start.required_inputs, vec!["dir"]);
        assert!(start.agent_may_fill);

        let local_clone = recommended_action_template("heddle clone <local-path> <path>")
            .expect("local clone recovery placeholder should resolve");
        assert_eq!(
            local_clone.argv_template,
            vec!["heddle", "clone", "<local-path>", "<path>"]
        );
        assert_eq!(local_clone.required_inputs, vec!["local_path", "path"]);
        assert!(!local_clone.agent_may_fill);

        let dynamic_clone =
            recommended_action_template("heddle clone /tmp/source <path> --thread main")
                .expect("dynamic clone recovery placeholder should resolve");
        assert_eq!(
            dynamic_clone.argv_template,
            vec![
                "heddle",
                "clone",
                "/tmp/source",
                "<path>",
                "--thread",
                "main"
            ]
        );
        assert_eq!(dynamic_clone.required_inputs, vec!["path"]);
        assert!(!dynamic_clone.agent_may_fill);

        let import = recommended_action_template(
            "heddle bridge git import --path <full-git-repo> --ref <ref>",
        )
        .expect("shallow import recovery placeholder should resolve");
        assert_eq!(
            import.argv_template,
            vec![
                "heddle",
                "bridge",
                "git",
                "import",
                "--path",
                "<full-git-repo>",
                "--ref",
                "<ref>"
            ]
        );
        assert_eq!(import.required_inputs, vec!["path", "ref"]);
        assert!(!import.agent_may_fill);

        let merge = recommended_action_template("heddle merge <thread> --git-commit")
            .expect("merge recovery placeholder should resolve");
        assert_eq!(
            merge.argv_template,
            vec!["heddle", "merge", "<thread>", "--git-commit"]
        );
        assert_eq!(merge.required_inputs, vec!["thread"]);
        assert!(!merge.agent_may_fill);
    }

    #[test]
    fn action_fields_template_dirty_worktree_message_placeholders() {
        for (action, expected_argv_template) in [
            (
                "heddle commit -m \"...\"",
                vec!["heddle", "commit", "-m", "<message>"],
            ),
            (
                "heddle capture -m \"...\"",
                vec!["heddle", "capture", "-m", "<message>"],
            ),
            (
                "heddle stash push -m \"...\"",
                vec!["heddle", "stash", "push", "-m", "<message>"],
            ),
        ] {
            let fields = ActionFields::from_action(action);
            assert_eq!(fields.action.as_deref(), Some(action));
            let template = fields
                .template
                .unwrap_or_else(|| panic!("`{action}` should expose a structured template"));
            assert_eq!(template.argv_template, expected_argv_template);
            assert_eq!(template.required_inputs, vec!["message"]);
            assert!(template.agent_may_fill);
        }
    }

    #[test]
    fn action_fields_template_argv_normalized_message_placeholders() {
        for (action, expected_argv_template) in [
            (
                "heddle commit -m ...",
                vec!["heddle", "commit", "-m", "<message>"],
            ),
            (
                "heddle capture -m ...",
                vec!["heddle", "capture", "-m", "<message>"],
            ),
            (
                "heddle stash push -m ...",
                vec!["heddle", "stash", "push", "-m", "<message>"],
            ),
        ] {
            let fields = ActionFields::from_action(action);
            assert_eq!(fields.action.as_deref(), Some(action));
            let template = fields
                .template
                .unwrap_or_else(|| panic!("`{action}` should expose a structured template"));
            assert_eq!(template.argv_template, expected_argv_template);
            assert_eq!(template.required_inputs, vec!["message"]);
            assert!(template.agent_may_fill);
        }
    }

    #[test]
    fn display_only_recommended_actions_must_be_templated() {
        let err = validate_recommended_action("heddle switch <missing-template>")
            .expect_err("unregistered display placeholder should fail validation");
        assert!(
            err.contains("structured template"),
            "error should explain missing template: {err}"
        );

        assert!(
            recommended_action_template("heddle switch <missing-template>").is_none(),
            "unregistered display placeholder must not resolve to a fillable template"
        );
    }

    #[test]
    fn recommended_action_validator_rejects_unknown_commands() {
        let err = validate_recommended_action("heddle definitely-not-a-command")
            .expect_err("unknown heddle command should fail validation");
        assert!(
            err.contains("definitely-not-a-command"),
            "error should name the bad command: {err}"
        );

        let err = validate_recommended_action("git status")
            .expect_err("raw git action must be explicitly registered");
        assert!(
            err.contains("registered as a placeholder"),
            "error should explain placeholder registration: {err}"
        );
    }

    #[test]
    fn leading_dash_thread_breadcrumbs_pass_validation() {
        // A historical / `new_unchecked` thread id literally named `-foo` renders
        // breadcrumbs via the `=` (flag) and `--` (positional) forms; the
        // validator splits to argv and runs clap, which would reject the bare
        // `--thread -foo` form as an unknown flag. (heddle#464 round 8.)
        for action in [
            repo::RecommendedAction::Sync,
            repo::RecommendedAction::Ready,
            repo::RecommendedAction::Land,
            repo::RecommendedAction::Promote,
        ] {
            if let Some(cmd) = action.command("-foo") {
                validate_recommended_action(&cmd).unwrap_or_else(|err| {
                    panic!("breadcrumb `{cmd}` must validate for a leading-dash id: {err}")
                });
            }
        }
    }

    #[test]
    fn action_fields_fail_loudly_for_invalid_recommendations() {
        let panic = std::panic::catch_unwind(|| ActionFields::from_action("git status"));
        assert!(
            panic.is_err(),
            "ActionFields must not silently erase invalid action sidecars"
        );
    }

    #[test]
    fn recommended_action_parser_supports_shell_quoted_arguments() {
        let template = recommended_action_template("heddle merge 'feature with spaces' --preview")
            .expect("single-quoted thread action should resolve to a template");
        assert_eq!(
            template.argv_template[1..],
            ["merge", "feature with spaces", "--preview"]
        );

        let template =
            recommended_action_template("heddle merge 'feature '\\''quoted'\\''' --preview")
                .expect("shell-quoted apostrophe should resolve to a template");
        assert_eq!(
            template.argv_template[1..],
            ["merge", "feature 'quoted'", "--preview"]
        );
    }

    #[test]
    fn checked_action_builder_quotes_and_validates_from_argv() {
        let action = heddle_action(["merge", "feature with spaces", "--preview"]);
        assert_eq!(action, "heddle merge 'feature with spaces' --preview");
        let template = recommended_action_template(&action)
            .expect("built action should resolve to a template");
        assert_eq!(
            template.argv_template[1..],
            ["merge", "feature with spaces", "--preview"]
        );

        let panic = std::panic::catch_unwind(|| checked_action_from_argv(["git", "status"]));
        assert!(
            panic.is_err(),
            "non-Heddle actions should not enter runtime advice sidecars"
        );
    }

    #[test]
    fn command_contract_table_matches_clap_command_tree() {
        let raw_contract_paths = CONTRACTS
            .iter()
            .map(|entry| {
                entry
                    .path
                    .iter()
                    .map(|part| (*part).to_string())
                    .collect::<Vec<_>>()
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(
            raw_contract_paths.len(),
            CONTRACTS.len(),
            "command contract table contains duplicate paths"
        );
        let active_contract_paths = active_command_contract_entries()
            .iter()
            .copied()
            .map(|entry| {
                entry
                    .path
                    .iter()
                    .map(|part| (*part).to_string())
                    .collect::<Vec<_>>()
            })
            .collect::<BTreeSet<_>>();

        let mut clap_paths = BTreeSet::new();
        collect_clap_command_paths(&Cli::command(), &mut Vec::new(), &mut clap_paths);

        let missing_contracts = clap_paths
            .difference(&active_contract_paths)
            .map(|path| path.join(" "))
            .collect::<Vec<_>>();
        assert!(
            missing_contracts.is_empty(),
            "Clap exposes command path(s) with no command contract: {missing_contracts:?}"
        );

        let stale_contracts = active_contract_paths
            .difference(&clap_paths)
            .map(|path| path.join(" "))
            .collect::<Vec<_>>();
        assert!(
            stale_contracts.is_empty(),
            "command contract table contains path(s) not exposed by Clap: {stale_contracts:?}"
        );
    }

    fn collect_clap_command_paths(
        command: &clap::Command,
        prefix: &mut Vec<String>,
        out: &mut BTreeSet<Vec<String>>,
    ) {
        for subcommand in command.get_subcommands() {
            prefix.push(subcommand.get_name().to_string());
            out.insert(prefix.clone());
            collect_clap_command_paths(subcommand, prefix, out);
            prefix.pop();
        }
    }

    #[test]
    fn parsed_runtime_contract_lookup_matches_contract_table_for_parseable_commands() {
        let active_contracts = active_command_contract_entries();
        let sample_paths = RUNTIME_CONTRACT_PARSE_SAMPLES
            .iter()
            .map(|sample| sample.path.to_vec())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            sample_paths.len(),
            RUNTIME_CONTRACT_PARSE_SAMPLES.len(),
            "runtime contract parse samples contain duplicate paths"
        );

        let child_contract_paths = contract_paths_with_children(active_contracts);
        let unsampled_contracts = active_contracts
            .iter()
            .filter(|entry| !child_contract_paths.contains(entry.path))
            .filter(|entry| !sample_paths.contains(entry.path))
            .map(|entry| entry.path.join(" "))
            .collect::<Vec<_>>();
        assert!(
            unsampled_contracts.is_empty(),
            "parseable leaf/runtime contract path(s) need parse samples: {unsampled_contracts:?}"
        );

        for sample in RUNTIME_CONTRACT_PARSE_SAMPLES {
            let expected = active_contracts
                .iter()
                .find(|entry| entry.path == sample.path)
                .unwrap_or_else(|| {
                    panic!(
                        "runtime contract parse sample references missing contract `{}`",
                        sample.path.join(" ")
                    )
                });
            let mut argv = vec!["heddle"];
            argv.extend_from_slice(sample.argv_tail);
            let cli = Cli::try_parse_from(argv.clone())
                .unwrap_or_else(|err| panic!("failed to parse sample {argv:?}: {err}"));
            let runtime = command_runtime_contract_for_command(&cli.command);

            assert_eq!(
                runtime.path,
                expected.path,
                "parsed sample {argv:?} must resolve to contract path `{}`",
                expected.path.join(" ")
            );
            assert_eq!(runtime.display, expected.path.join(" "));
            assert_eq!(runtime.display, command_path(&cli.command).join(" "));
        }
    }

    #[test]
    fn json_compact_runtime_contract_is_projection_or_rejection() {
        let expected_compact = BTreeSet::from([
            "abort".to_string(),
            "capture".to_string(),
            "continue".to_string(),
            "land".to_string(),
            "merge".to_string(),
            "ready".to_string(),
            "status".to_string(),
            "sync".to_string(),
        ]);
        let actual_compact = active_command_contract_entries()
            .iter()
            .filter(|entry| entry.contract.supports_json_compact)
            .map(|entry| entry.path.join(" "))
            .collect::<BTreeSet<_>>();
        assert_eq!(actual_compact, expected_compact);

        let json_output_commands = active_command_contract_entries()
            .iter()
            .filter(|entry| entry.contract.supports_json)
            .map(|entry| entry.path.join(" "))
            .collect::<BTreeSet<_>>();
        let compact_rejections = active_command_contract_entries()
            .iter()
            .filter(|entry| entry.contract.supports_json && !entry.contract.supports_json_compact)
            .map(|entry| entry.path.join(" "))
            .collect::<BTreeSet<_>>();
        let classified_commands = actual_compact
            .union(&compact_rejections)
            .cloned()
            .collect::<BTreeSet<_>>();
        assert_eq!(
            classified_commands, json_output_commands,
            "every JSON-output command must either project json-compact or reject it before execution"
        );
        assert!(
            compact_rejections.contains("commands"),
            "the harness must include commands that accept --output json but reject json-compact"
        );

        for sample in RUNTIME_CONTRACT_PARSE_SAMPLES {
            let mut argv = vec!["heddle", "--output", "json-compact"];
            argv.extend_from_slice(sample.argv_tail);
            let cli = Cli::try_parse_from(argv.clone())
                .unwrap_or_else(|err| panic!("failed to parse sample {argv:?}: {err}"));
            let runtime = command_runtime_contract_for_command(&cli.command);
            assert!(
                runtime.supports_json_compact || !expected_compact.contains(&runtime.display),
                "`{}` accepts json-compact at parse time but lacks an explicit compact projection; main must reject it before command execution",
                runtime.display
            );
            assert!(
                runtime.supports_json || !runtime.supports_json_compact,
                "`{}` cannot support json-compact without supporting json",
                runtime.display
            );
        }
    }

    fn contract_paths_with_children(
        entries: &[&'static CommandContractEntry],
    ) -> BTreeSet<Vec<&'static str>> {
        entries
            .iter()
            .filter(|candidate| {
                entries.iter().any(|entry| {
                    entry.path.len() > candidate.path.len()
                        && entry.path.starts_with(candidate.path)
                })
            })
            .map(|entry| entry.path.to_vec())
            .collect()
    }

    #[test]
    fn command_contract_metadata_is_internally_consistent() {
        for entry in CONTRACTS {
            let display = entry.path.join(" ");
            let contract = entry.contract;
            let json_capable = matches!(contract.json_kind, "json" | "jsonl" | "json_or_jsonl");
            assert_eq!(
                contract.supports_json, json_capable,
                "`{display}` supports_json must agree with json_kind `{}`",
                contract.json_kind
            );
            assert!(
                matches!(
                    contract.json_kind,
                    "json" | "jsonl" | "json_or_jsonl" | "none"
                ),
                "`{display}` has unknown json_kind `{}`",
                contract.json_kind
            );
            assert!(
                matches!(
                    contract.surface,
                    "native" | "git_adapter" | "automation" | "admin" | "internal"
                ),
                "`{display}` has unknown product surface `{}`",
                contract.surface
            );
            assert!(
                matches!(
                    contract.help_visibility,
                    "everyday" | "advanced" | "git_adapter" | "hidden"
                ),
                "`{display}` has unknown help visibility `{}`",
                contract.help_visibility
            );
            if contract.help_visibility == "git_adapter" {
                assert_eq!(
                    contract.surface, "git_adapter",
                    "`{display}` Git adapter commands must live on the Git adapter surface"
                );
                assert!(
                    contract.canonical_command.is_some(),
                    "`{display}` Git-shaped aliases must name a canonical Heddle command"
                );
                assert!(
                    contract.canonical_kind.is_some(),
                    "`{display}` Git-shaped aliases must classify the canonical action"
                );
                assert!(
                    contract.canonical_note.is_some(),
                    "`{display}` Git-shaped aliases must explain the canonical action"
                );
            }
            if contract.help_visibility == "everyday" {
                assert!(
                    contract.help_rank < 1000,
                    "`{display}` everyday commands must choose an explicit help rank"
                );
            }
            if let Some(canonical) = contract.canonical_command {
                let canonical_kind = contract
                    .canonical_kind
                    .unwrap_or_else(|| panic!("`{display}` canonical command must have a kind"));
                assert!(
                    matches!(
                        canonical_kind,
                        "direct_command" | "command_family" | "workflow" | "conceptual_home"
                    ),
                    "`{display}` has unknown canonical action kind `{canonical_kind}`"
                );
                assert!(
                    raw_command_contract_for_path(canonical.split_whitespace()).is_some(),
                    "`{display}` points at missing canonical command `{canonical}`"
                );
            } else {
                assert!(
                    contract.canonical_kind.is_none() && contract.canonical_note.is_none(),
                    "`{display}` cannot describe a canonical action without a canonical command"
                );
            }
            if contract.persists_op_id {
                assert!(
                    contract.supports_op_id,
                    "`{display}` cannot persist op-ids unless it supports op-id replay"
                );
                assert!(
                    contract.mutates,
                    "`{display}` cannot persist op-ids for an observe-only command"
                );
            }
            if contract.observe_only {
                assert!(
                    !contract.mutates,
                    "`{display}` cannot be both observe_only and mutating"
                );
                assert!(
                    !contract.supports_op_id && !contract.persists_op_id,
                    "`{display}` observe-only commands must not reserve op-id slots"
                );
                assert!(
                    !contract.may_initialize
                        && !contract.may_import_git
                        && !contract.may_write_worktree
                        && !contract.may_move_ref
                        && !contract.destructive_requires_force
                        && !contract.writes_heddle_refs
                        && !contract.writes_git_refs
                        && !contract.writes_worktree
                        && !contract.writes_config
                        && !contract.writes_hooks
                        && !contract.network_io
                        && !contract.daemon_process
                        && !contract.object_gc
                        && !contract.external_command
                        && !contract.requires_git_executable
                        && !contract.destructive_data,
                    "`{display}` observe-only commands must not advertise write side effects"
                );
            }
            assert!(
                !contract.requires_git_executable,
                "`{display}` must not require a `git` executable; Git-format work belongs in native/library code"
            );
            let effects = side_effects(contract);
            assert!(
                !effects.is_empty(),
                "`{display}` must advertise at least one side effect"
            );
            if contract.observe_only {
                assert_eq!(
                    effects,
                    vec!["observe_only"],
                    "`{display}` observe-only side_effects must stay exact"
                );
            } else {
                for (flag, effect) in [
                    (contract.may_initialize, "initialize"),
                    (contract.may_import_git, "import_git"),
                    (contract.writes_heddle_refs, "writes_heddle_refs"),
                    (contract.writes_git_refs, "writes_git_refs"),
                    (contract.writes_worktree, "writes_worktree"),
                    (contract.writes_config, "writes_config"),
                    (contract.writes_hooks, "writes_hooks"),
                    (contract.network_io, "network_io"),
                    (contract.daemon_process, "daemon_process"),
                    (contract.object_gc, "object_gc"),
                    (contract.external_command, "external_command"),
                    (
                        contract.destructive_requires_force,
                        "destructive_requires_force",
                    ),
                    (contract.destructive_data, "destructive_data"),
                ] {
                    assert_eq!(
                        effects.contains(&effect),
                        flag,
                        "`{display}` side_effects must mirror `{effect}`"
                    );
                }
                if contract.may_write_worktree && !contract.writes_worktree {
                    assert!(
                        effects.contains(&"may_write_worktree"),
                        "`{display}` side_effects must preserve flag-sensitive worktree writes"
                    );
                }
            }
            assert_eq!(
                contract.may_move_ref,
                contract.writes_heddle_refs || contract.writes_git_refs,
                "`{display}` may_move_ref must summarize concrete ref dimensions"
            );
            if contract.destructive_requires_force {
                assert!(
                    contract.mutates,
                    "`{display}` destructive commands must be mutating commands"
                );
            }
            for verb in contract.documented_schema_verbs {
                assert!(
                    contract.schema_verbs.contains(verb),
                    "`{display}` documents schema verb `{verb}` without registering it"
                );
            }
            for verb in contract.opaque_schema_verbs {
                assert!(
                    contract.schema_verbs.contains(verb),
                    "`{display}` marks schema verb `{verb}` opaque without registering it"
                );
                assert!(
                    contract.documented_schema_verbs.contains(verb),
                    "`{display}` marks schema verb `{verb}` opaque without documenting it"
                );
            }
            if !contract.schema_verbs.is_empty() {
                assert!(
                    contract.supports_json,
                    "`{display}` registers JSON schema verbs but does not support JSON"
                );
            }
        }
    }

    #[cfg(not(feature = "git-overlay"))]
    #[test]
    fn native_only_catalog_excludes_git_overlay_commands() {
        let catalog = build_command_catalog();
        for display in [
            "bridge",
            "bridge git",
            "bridge git status",
            "bridge git init",
            "bridge git import",
            "bridge git export",
            "bridge git sync",
            "bridge git reconcile",
            "bridge git push",
            "bridge git pull",
            "bridge git ingest",
            "bridge git reason",
            "git-overlay",
        ] {
            assert!(
                catalog.command_by_display(display).is_none(),
                "native-only catalog must not advertise git-overlay command `{display}`"
            );
            assert!(
                command_runtime_contract(display).is_none(),
                "native-only runtime contracts must not resolve git-overlay command `{display}`"
            );
        }
    }

    #[test]
    fn json_kind_marks_streaming_command_surfaces() {
        let catalog = build_command_catalog();
        for (display, kind) in [
            ("watch", "jsonl"),
            ("rebase", "jsonl"),
            ("status", "json_or_jsonl"),
            ("thread show", "json_or_jsonl"),
            ("workspace show", "json_or_jsonl"),
        ] {
            let entry = catalog
                .commands
                .iter()
                .find(|entry| entry.display == display)
                .unwrap_or_else(|| panic!("missing command catalog entry for `{display}`"));
            assert_eq!(
                entry.json_kind, kind,
                "`{display}` must advertise its streaming JSON contract"
            );
        }
    }

    #[test]
    fn json_discriminator_table_starts_with_bounded_command_slice() {
        // Wire-format-stable list. PR #251 instrumented the initial set;
        // heddle#272 swept the named-by-persona verbs (stack, goto, fork,
        // revert, purge, redact, stash, clean, discuss, context, review,
        // cherry-pick, bisect); heddle#641 swept the remaining verbs whose
        // runtime JSON already emits `output_kind` (abort, adopt, the agent
        // session verbs, blame, branch, bridge git push/pull, conflict show,
        // continue, daemon stop, doctor, fetch, inspect, land, log,
        // maintenance gc/index, merge --preview, pull, push, query, ready,
        // the remote family, start, switch, sync, and the thread lifecycle
        // verbs). Any further sweep MUST extend this list and document the
        // addition.
        //
        // `clone` appears twice because hosted `clone --output json`
        // emits two JSON records per invocation (a preliminary
        // `clone_connection` envelope followed by the final `clone`
        // payload); both discriminator values are advertised so agents
        // can route on either record. See heddle#272 (PR #281 r3).
        let displays = raw_json_discriminator_specs()
            .iter()
            .map(|(path, _)| path.join(" "))
            .collect::<Vec<_>>();
        assert_eq!(
            displays,
            vec![
                // heddle#641 swept every remaining verb whose runtime JSON
                // already carries `output_kind` (probed live + verified against
                // the emitting structs). The values are the RUNTIME truths, not
                // snake-cased display paths — see the overrides documented in
                // tests/cli_integration/output_kind_invariant.rs.
                "abort",
                "adopt",
                "agent serve",
                "agent status",
                "agent stop",
                "agent capture",
                "agent ready",
                "blame",
                "branch",
                "branch",
                "branch",
                "branch",
                "bridge git status",
                "bridge git import",
                "bridge git sync",
                "bridge git reconcile",
                "bridge git push",
                "bridge git pull",
                "bridge backfill-fidelity",
                "capture",
                "checkpoint",
                "cherry-pick",
                "clean",
                "clone",
                "clone",
                "commit",
                "commands",
                "conflict show",
                "continue",
                "context set",
                "context get",
                "context list",
                "context history",
                "context edit",
                "context supersede",
                "context rm",
                "context check",
                "context suggest",
                "context audit",
                "daemon stop",
                "diff",
                "discuss open",
                "discuss append",
                "discuss resolve",
                "discuss list",
                "discuss show",
                "doctor",
                "doctor docs",
                "doctor schemas",
                "fetch",
                "fork",
                "goto",
                "init",
                "inspect",
                "inspect",
                // `log` appears twice: the entry advertises both `log` and the
                // `log --reflog` variant (`log_reflog`), mirroring `undo`/`clone`.
                "log",
                "log",
                "maintenance gc",
                "maintenance index",
                "merge",
                "stack",
                "stack ready",
                "stack snapshot",
                "pull",
                "purge apply",
                "purge list",
                "push",
                "query",
                "ready",
                "rebase",
                "redact apply",
                "redact list",
                "redact show",
                "redact trust add",
                "redact trust list",
                "redact trust remove",
                "redo",
                "remote list",
                "remote add",
                "remote remove",
                "remote set-default",
                "remote show",
                "revert",
                "review show",
                "review sign",
                "review next",
                "review health",
                "schemas",
                "land",
                "show",
                "start",
                "stash list",
                "stash show",
                "status",
                "switch",
                "switch",
                "sync",
                "thread create",
                "thread switch",
                "thread list",
                "thread show",
                "thread rename",
                "thread refresh",
                "thread resolve",
                "thread promote",
                "thread drop",
                "thread revoke-approval",
                "thread cleanup",
                "verify",
                "visibility set",
                "visibility promote",
                "visibility show",
                "visibility list",
                "undo",
                "undo",
                "workspace show",
            ]
        );
    }

    /// heddle#641 close-the-class conformance: every schema verb whose
    /// JSON schema declares an `output_kind` property MUST have a
    /// catalog discriminator whose value matches the schema's const.
    ///
    /// Why this closes the gap: `schema_for_verb` only injects the
    /// `output_kind` enum-const into a schema when the catalog
    /// advertises a discriminator for that verb. A verb whose mirror
    /// struct declares `output_kind` (because the runtime payload
    /// emits it) but whose catalog entry lacks the discriminator
    /// therefore surfaces here as a *const-less* `output_kind`
    /// property — exactly the advertise-nothing gap that left ~111
    /// schema-bearing commands with `json_discriminators: []`. A new
    /// command that emits `output_kind` without registering its
    /// discriminator fails this test; one that registers a
    /// discriminator that diverges from the schema const fails the
    /// mismatch arm. Runtime-vs-catalog equality is enforced
    /// separately by `tests/cli_integration/output_kind_invariant.rs`.
    #[test]
    fn schema_output_kind_discriminators_are_complete_and_consistent() {
        use crate::cli::commands::schema_for_verb;
        use std::collections::BTreeSet;

        fn resolve_schema_ref<'a>(
            root: &'a serde_json::Value,
            reference: &str,
        ) -> &'a serde_json::Value {
            reference
                .strip_prefix("#/$defs/")
                .or_else(|| reference.strip_prefix("#/definitions/"))
                .and_then(|name| {
                    root.get("$defs")
                        .or_else(|| root.get("definitions"))
                        .and_then(|defs| defs.get(name))
                })
                .unwrap_or_else(|| panic!("schema reference `{reference}` resolves"))
        }

        fn collect_output_kind_values<'a>(
            root: &'a serde_json::Value,
            schema: &'a serde_json::Value,
            values: &mut BTreeSet<String>,
        ) {
            if let Some(reference) = schema.get("$ref").and_then(|value| value.as_str()) {
                collect_output_kind_values(root, resolve_schema_ref(root, reference), values);
                return;
            }

            if let Some(enum_values) = schema
                .get("properties")
                .and_then(|properties| properties.get("output_kind"))
                .and_then(|property| property.get("enum"))
                .and_then(|values| values.as_array())
            {
                values.extend(
                    enum_values
                        .iter()
                        .filter_map(|value| value.as_str())
                        .map(str::to_string),
                );
            } else if let Some(value) = schema
                .get("properties")
                .and_then(|properties| properties.get("output_kind"))
                .and_then(|property| property.get("const"))
                .and_then(|value| value.as_str())
            {
                values.insert(value.to_string());
            }

            for combinator in ["anyOf", "oneOf", "allOf"] {
                if let Some(schemas) = schema.get(combinator).and_then(|value| value.as_array()) {
                    for schema in schemas {
                        collect_output_kind_values(root, schema, values);
                    }
                }
            }
        }

        let mut missing = Vec::new();
        let mut mismatched = Vec::new();
        let mut checked = 0usize;

        for verb in schema_verbs() {
            let Some(schema) = schema_for_verb(verb) else {
                panic!("catalog schema verb `{verb}` has no registered schema");
            };
            let mut actual = BTreeSet::new();
            collect_output_kind_values(&schema, &schema, &mut actual);
            if actual.is_empty() {
                // The schema does not declare `output_kind` — the verb's
                // runtime payload genuinely lacks the discriminator (the
                // UNSWEPT_TODO rolldown in output_kind_invariant.rs).
                continue;
            };
            checked += 1;

            let mut expected_discriminators = command_json_discriminators_for_schema_verb(verb);
            if schema.get("anyOf").is_some() {
                expected_discriminators.extend(command_json_discriminators().into_iter().filter(
                    |discriminator| {
                        discriminator.display == verb
                            && discriminator.schema_verb.as_deref() != Some(verb)
                    },
                ));
            }
            let expected = expected_discriminators
                .into_iter()
                .filter(|discriminator| discriminator.field == "output_kind")
                .map(|discriminator| discriminator.value)
                .collect::<BTreeSet<_>>();
            if expected.is_empty() {
                missing.push(format!(
                    "`{verb}`: schema declares an `output_kind` property but the \
                     catalog advertises no json_discriminator for it"
                ));
                continue;
            }

            if actual != expected {
                mismatched.push(format!(
                    "`{verb}`: schema output_kind values {actual:?} != catalog discriminators {expected:?}"
                ));
            }
        }

        assert!(
            missing.is_empty() && mismatched.is_empty(),
            "Catalog json_discriminators drift from the schema `output_kind` contract. \
             Register the discriminator with `json_discriminator(Some(\"<verb>\"), \
             \"output_kind\", \"<runtime value>\")` on the command's catalog entry \
             (the value must match what the command actually emits).\n\nMissing:\n  - {}\n\nMismatched:\n  - {}",
            missing.join("\n  - "),
            mismatched.join("\n  - ")
        );
        assert!(
            checked >= 100,
            "expected the conformance sweep to inspect the full discriminator \
             surface (~107 schema verbs declare `output_kind`); only {checked} \
             were checked — the schema injection or verb collection likely regressed"
        );
    }

    #[test]
    fn json_discriminator_metadata_is_internally_consistent() {
        let raw_discriminators = raw_json_discriminator_specs();
        // A single command path MAY advertise more than one
        // discriminator (e.g. `clone` carries both `clone` and
        // `clone_connection` because hosted `clone --output json`
        // emits a preliminary connection envelope before the final
        // payload — see heddle#272). But each (path, value) pair must
        // still be unique, otherwise two entries would advertise the
        // same wire-format token and agents couldn't tell them apart.
        let path_value_pairs = raw_discriminators
            .iter()
            .map(|(path, d)| (path.to_vec(), d.value))
            .collect::<BTreeSet<_>>();
        assert_eq!(
            path_value_pairs.len(),
            raw_discriminators.len(),
            "JSON discriminator table contains duplicate (path, value) pairs"
        );

        let mut schema_verb_values = std::collections::BTreeMap::new();
        for (path, discriminator) in raw_discriminators {
            let display = path.join(" ");
            let contract = raw_command_contract_for_path(path.iter().copied())
                .unwrap_or_else(|| panic!("JSON discriminator references unknown `{display}`"));
            assert!(
                contract.supports_json,
                "`{display}` advertises JSON discriminator `{}` but does not support JSON",
                discriminator.value
            );
            assert!(
                matches!(discriminator.field, "kind" | "output_kind"),
                "`{display}` advertises unsupported discriminator field `{}`",
                discriminator.field
            );
            assert!(
                !discriminator.value.is_empty(),
                "`{display}` discriminator value must be non-empty"
            );

            if let Some(schema_verb) = discriminator.schema_verb {
                let schema_value = (discriminator.field, discriminator.value);
                if let Some(previous) = schema_verb_values.insert(schema_verb, schema_value) {
                    assert_eq!(
                        previous, schema_value,
                        "JSON discriminator schema verb `{schema_verb}` is registered with \
                         conflicting discriminator values"
                    );
                }
                assert!(
                    contract.schema_verbs.contains(&schema_verb),
                    "`{display}` advertises discriminator schema verb `{schema_verb}` not present in its command contract"
                );
                assert!(
                    discriminator.no_schema_reason.is_none(),
                    "`{display}` cannot have both a schema verb and a no-schema reason"
                );
            } else {
                assert!(
                    discriminator
                        .no_schema_reason
                        .is_some_and(|reason| !reason.is_empty()),
                    "`{display}` discriminator without a schema verb must document why"
                );
            }
        }
    }

    #[test]
    fn command_catalog_exposes_active_json_discriminator_metadata() {
        let catalog = build_command_catalog();
        let active = command_json_discriminators();
        for discriminator in &active {
            let entry = catalog
                .commands
                .iter()
                .find(|command| command.display == discriminator.display)
                .unwrap_or_else(|| {
                    panic!(
                        "active JSON discriminator references missing command `{}`",
                        discriminator.display
                    )
                });
            assert!(entry.supports_json);
            assert!(
                entry
                    .json_discriminators
                    .iter()
                    .any(|entry_discriminator| entry_discriminator == discriminator),
                "`{}` catalog entry must expose its JSON discriminator metadata",
                discriminator.display
            );
        }

        let status = catalog
            .commands
            .iter()
            .find(|entry| entry.display == "status")
            .expect("status should be cataloged");
        assert_eq!(status.json_discriminators.len(), 1);
        assert_eq!(status.json_discriminators[0].field, "output_kind");
        assert_eq!(status.json_discriminators[0].value, "status");
    }

    fn raw_json_discriminator_specs() -> Vec<(
        &'static [&'static str],
        &'static CommandJsonDiscriminatorSpec,
    )> {
        CONTRACTS
            .iter()
            .flat_map(|entry| {
                entry
                    .contract
                    .json_discriminators
                    .iter()
                    .map(move |discriminator| (entry.path, discriminator))
            })
            .collect()
    }

    #[test]
    fn catalog_option_lookup_includes_globals_and_finite_values() {
        let catalog = build_command_catalog();

        let start_options = catalog
            .options_for_display("start")
            .expect("start should be cataloged");
        let output = start_options
            .iter()
            .find(|option| option.long.as_deref() == Some("output"))
            .expect("global --output should be included in command options");
        assert_eq!(output.possible_values, vec!["json", "json-compact", "text"]);
        for command in &catalog.commands {
            assert!(
                !command
                    .options
                    .iter()
                    .any(|option| option.long.as_deref() == Some("json")),
                "legacy --json should not be included in command options for {}",
                command.path.join(" ")
            );
        }
        assert!(
            start_options
                .iter()
                .any(|option| option.long.as_deref() == Some("help")),
            "generated --help should be included in command options"
        );

        let workspace = start_options
            .iter()
            .find(|option| option.long.as_deref() == Some("workspace"))
            .expect("start --workspace should be cataloged");
        assert_eq!(
            workspace.possible_values,
            vec!["auto", "materialized", "virtualized", "solid"]
        );

        let context_set_options = catalog
            .options_for_path(&["context".to_string(), "set".to_string()])
            .expect("context set should be cataloged");
        let scope = context_set_options
            .iter()
            .find(|option| option.long.as_deref() == Some("scope"))
            .expect("context set --scope should be cataloged");
        assert!(
            scope.possible_values.is_empty(),
            "context scope accepts open-ended values like symbol:<name>"
        );
        let kind = context_set_options
            .iter()
            .find(|option| option.long.as_deref() == Some("kind"))
            .expect("context set --kind should be cataloged");
        assert_eq!(
            kind.possible_values,
            vec!["constraint", "invariant", "rationale"]
        );

        let integration_install_options = catalog
            .options_for_display("integration install")
            .expect("integration install should be cataloged");
        let scope = integration_install_options
            .iter()
            .find(|option| option.long.as_deref() == Some("scope"))
            .expect("integration install --scope should be cataloged");
        assert_eq!(scope.possible_values, vec!["repo", "user"]);
        assert_eq!(scope.aliases, vec!["harness-install-scope"]);
    }

    #[test]
    fn command_contract_table_drives_help_tiers() {
        let catalog = build_command_catalog();
        for (display, tier, surface, visibility, canonical, canonical_kind, executable) in [
            (
                "status", "everyday", "native", "everyday", None, None, false,
            ),
            (
                "verify", "everyday", "native", "everyday", None, None, false,
            ),
            (
                "commit", "everyday", "native", "everyday", None, None, false,
            ),
            ("land", "everyday", "native", "everyday", None, None, false),
            ("push", "everyday", "native", "everyday", None, None, false),
            (
                "capture", "advanced", "native", "advanced", None, None, false,
            ),
            (
                "thread create",
                "advanced",
                "native",
                "advanced",
                None,
                None,
                false,
            ),
            (
                "thread promote",
                "advanced",
                "native",
                "advanced",
                None,
                None,
                false,
            ),
            (
                "checkpoint",
                "advanced",
                "native",
                "advanced",
                None,
                None,
                false,
            ),
            (
                "switch",
                "advanced",
                "git_adapter",
                "git_adapter",
                Some("thread switch"),
                Some("direct_command"),
                false,
            ),
        ] {
            let entry = catalog
                .commands
                .iter()
                .find(|entry| entry.display == display)
                .unwrap_or_else(|| panic!("missing command catalog entry for `{display}`"));
            assert_eq!(entry.tier, tier);
            assert_eq!(entry.surface, surface);
            assert_eq!(entry.help_visibility, visibility);
            assert_eq!(entry.canonical_command.as_deref(), canonical);
            assert_eq!(
                entry
                    .canonical_action
                    .as_ref()
                    .map(|action| action.kind.as_str()),
                canonical_kind
            );
            assert_eq!(
                entry
                    .canonical_action
                    .as_ref()
                    .is_some_and(|action| action.executable),
                executable
            );
            assert_eq!(command_help_tier(display), tier);
            assert_eq!(command_surface(display), surface);
            assert_eq!(command_help_visibility(display), visibility);
            assert_eq!(command_canonical_command(display), canonical);
        }
        for (display, canonical, kind) in [
            ("branch", "thread", "command_family"),
            ("stash pop", "undo", "conceptual_home"),
            ("fetch", "pull", "workflow"),
        ] {
            let entry = catalog
                .commands
                .iter()
                .find(|entry| entry.display == display)
                .unwrap_or_else(|| panic!("missing command catalog entry for `{display}`"));
            let action = entry
                .canonical_action
                .as_ref()
                .unwrap_or_else(|| panic!("`{display}` should expose a canonical action"));
            assert_eq!(action.command, canonical);
            assert_eq!(action.kind, kind);
            assert!(
                !action.executable,
                "`{display}` is not a direct command replacement"
            );
            assert!(!action.note.is_empty());
        }
        assert_eq!(command_help_tier("transaction"), "hidden");

        let thread_list = catalog
            .commands
            .iter()
            .find(|entry| entry.display == "thread list")
            .expect("thread list should be cataloged");
        assert_eq!(thread_list.tier, "advanced");
    }

    #[test]
    fn parsed_command_op_id_support_reads_contract_table() {
        for (argv, expected) in [
            (vec!["heddle", "status"], false),
            (vec!["heddle", "commit", "-m", "checkpoint"], true),
            (vec!["heddle", "thread", "list"], false),
            (vec!["heddle", "thread", "drop", "feature"], true),
        ] {
            let cli = Cli::try_parse_from(argv.clone())
                .unwrap_or_else(|err| panic!("failed to parse {argv:?}: {err}"));
            let display = command_path(&cli.command).join(" ");
            assert_eq!(
                command_supports_op_id_for_command(&cli.command),
                expected,
                "`{display}` op-id support must come from its parsed command contract"
            );
            assert_eq!(
                command_supports_op_id(&display),
                expected,
                "`{display}` string lookup must agree with parsed command contract"
            );
        }
    }

    #[test]
    fn parsed_command_json_support_reads_contract_table() {
        for (argv, expected) in [
            (vec!["heddle", "status"], true),
            (vec!["heddle", "commands"], true),
            (vec!["heddle", "shell", "completion", "bash"], false),
            (vec!["heddle", "thread", "cd", "feature"], false),
        ] {
            let cli = Cli::try_parse_from(argv.clone())
                .unwrap_or_else(|err| panic!("failed to parse {argv:?}: {err}"));
            let display = command_path(&cli.command).join(" ");
            let entry = build_command_catalog()
                .commands
                .into_iter()
                .find(|entry| entry.display == display)
                .unwrap_or_else(|| panic!("missing command catalog entry for `{display}`"));
            assert_eq!(
                command_supports_json_for_command(&cli.command),
                expected,
                "`{display}` JSON support must come from its parsed command contract"
            );
            assert_eq!(entry.supports_json, expected);
        }
    }

    #[test]
    fn parsed_command_runtime_contract_exposes_catalog_fields() {
        let cli = Cli::try_parse_from(["heddle", "thread", "drop", "feature"])
            .expect("thread drop should parse");
        let runtime = command_runtime_contract_for_command(&cli.command);
        let catalog = build_command_catalog();
        let entry = catalog
            .commands
            .iter()
            .find(|entry| entry.display == runtime.display)
            .expect("runtime command should be present in catalog");

        assert_eq!(runtime.path, vec!["thread", "drop"]);
        assert_eq!(runtime.supports_json, entry.supports_json);
        assert_eq!(runtime.supports_op_id, entry.supports_op_id);
        assert_eq!(runtime.persists_op_id, entry.persists_op_id);
        assert_eq!(
            runtime.uses_bootstrap_op_id_store,
            entry.op_id_store_scope == "bootstrap"
        );
        assert_eq!(runtime.help_visibility, entry.help_visibility);
        assert_eq!(runtime.help_rank, entry.help_rank);
        assert_eq!(runtime.surface, entry.surface);
    }

    #[test]
    fn op_id_persistence_reads_contract_table() {
        let catalog = build_command_catalog();
        for (display, persists, store_scope) in [
            ("capture", false, "repository"),
            ("review sign", false, "repository"),
            ("commit", false, "repository"),
            ("status", false, "none"),
            ("init", false, "bootstrap"),
            ("adopt", false, "bootstrap"),
            ("clone", false, "bootstrap"),
            ("bridge git init", false, "bootstrap"),
        ] {
            let entry = catalog
                .commands
                .iter()
                .find(|entry| entry.display == display)
                .unwrap_or_else(|| panic!("missing command catalog entry for `{display}`"));
            assert_eq!(
                entry.persists_op_id, persists,
                "`{display}` op-id persistence must be cataloged"
            );
            assert_eq!(
                entry.op_id_store_scope, store_scope,
                "`{display}` op-id store scope must be cataloged"
            );
            assert_eq!(
                command_persists_op_id(display),
                persists,
                "`{display}` runtime op-id persistence must come from the contract table"
            );
            assert_eq!(
                command_uses_bootstrap_op_id_store(display),
                store_scope == "bootstrap",
                "`{display}` runtime op-id store scope must come from the contract table"
            );
            if persists {
                assert!(
                    entry.supports_op_id,
                    "`{display}` cannot persist op-ids unless it supports op-id replay"
                );
            }
        }
    }

    #[test]
    fn feature_gated_command_roots_are_catalog_owned() {
        assert_eq!(feature_gated_command_roots(), &["presence", "support"]);
    }
}
