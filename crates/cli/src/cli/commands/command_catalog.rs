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
    ActorCommands, AgentCommands, BisectCommands, Cli, Commands, ContextCommands, DaemonCommands,
    DoctorCommands, HookCommands, IntegrationCommands, MaintenanceCommands, MarkerCommands,
    PurgeCommands, RedactCommands, RedactTrustCommands, RemoteCommands, SessionCommands,
    ShellCommands, StackCommands, StashCommands, StoreCommands, ThreadCommands, WorkspaceCommands,
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
    json_kind: &'static str,
    json_discriminators: &'static [CommandJsonDiscriminatorSpec],
    schema_verbs: &'static [&'static str],
    documented_schema_verbs: &'static [&'static str],
    opaque_schema_verbs: &'static [&'static str],
    surface: &'static str,
    help_visibility: &'static str,
    help_rank: u16,
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
    pub supports_op_id: bool,
    pub persists_op_id: bool,
    pub uses_bootstrap_op_id_store: bool,
    pub mutates: bool,
    pub observe_only: bool,
    pub help_visibility: &'static str,
    pub help_rank: u16,
    pub surface: &'static str,
    pub canonical_command: Option<&'static str>,
    pub canonical_kind: Option<&'static str>,
    pub canonical_note: Option<&'static str>,
    pub advertised_action: Option<AdvertisedAction>,
    pub feature_gate: Option<&'static str>,
    pub exit_codes: &'static [(u8, &'static str)],
    pub side_effects: Vec<&'static str>,
    pub side_effect_class: &'static str,
    pub first_run_behavior: &'static str,
    pub json_kind: &'static str,
    pub schema_verbs: &'static [&'static str],
    pub documented_schema_verbs: &'static [&'static str],
    pub opaque_schema_verbs: &'static [&'static str],
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
    "heddle start <name> --workspace materialized",
    "heddle start <name> --path <empty-path>",
    "heddle start <name> --path ../<name>",
    "heddle actor show <session>",
    "heddle stash push -m \"...\"",
    "heddle thread show <THREAD>",
    // Choice placeholders: the user must choose one command and fill
    // in the state after inspecting the bisect result.
    "heddle bisect good <state> or heddle bisect bad <state>",
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
        "heddle start <name> --workspace materialized",
        &["heddle", "start", "<name>", "--workspace", "materialized"],
        &["name"],
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
        "heddle bisect good <state> or heddle bisect bad <state>",
        &["heddle", "bisect", "<good|bad>", "<state>"],
        &["verdict", "state"],
        false,
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
    json_kind: "json",
    json_discriminators: &[],
    schema_verbs: &[],
    documented_schema_verbs: &[],
    opaque_schema_verbs: &[],
    surface: "native",
    help_visibility: "advanced",
    help_rank: 1000,
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
    supports_json: true,
    mutates: true,
    supports_op_id: true,
    persists_op_id: false,
    observe_only: false,
    may_initialize: false,
    may_import_git: false,
    may_write_worktree: false,
    may_move_ref: true,
    destructive_requires_force: false,
    writes_heddle_refs: true,
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
    json_kind: "json",
    json_discriminators: &[],
    schema_verbs: &[],
    documented_schema_verbs: &[],
    opaque_schema_verbs: &[],
    surface: "native",
    help_visibility: "advanced",
    help_rank: 1000,
    canonical_command: None,
    canonical_kind: None,
    canonical_note: None,
    advertised_action: None,
    feature_gate: None,
    exit_codes: &[],
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

const WORKTREE_MUTATION: CommandContract = CommandContract {
    may_write_worktree: true,
    writes_worktree: true,
    ..MUTATING
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
    entry(&["abort"], documented_schemas(MUTATING, &["abort"])),
    entry(
        &["adopt"],
        front_door(
            advertised_action(
                documented_schemas(ADOPT, &["adopt"]),
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
            documented_schemas(DAEMON_MUTATION, &["agent serve"]),
            "automation",
        ),
    ),
    entry(
        &["agent", "status"],
        surface(
            documented_schemas(READ_JSON, &["agent status"]),
            "automation",
        ),
    ),
    entry(
        &["agent", "stop"],
        surface(
            documented_schemas(DAEMON_MUTATION, &["agent stop"]),
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
            documented_schemas(CAPTURE, &["agent capture"]),
            "automation",
        ),
    ),
    entry(
        &["agent", "ready"],
        surface(documented_schemas(CAPTURE, &["agent ready"]), "automation"),
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
        documented_schemas(EXTERNAL_WORKTREE_MUTATION, &["attempt"]),
    ),
    #[cfg(feature = "client")]
    entry(&["auth"], GROUP),
    #[cfg(feature = "client")]
    entry(&["auth", "login"], MUTATING_TEXT),
    #[cfg(feature = "client")]
    entry(&["auth", "logout"], MUTATING_NO_OP_ID),
    #[cfg(feature = "client")]
    entry(&["auth", "status"], READ_JSON),
    #[cfg(feature = "client")]
    entry(&["auth", "create-service-token"], MUTATING_NO_OP_ID),
    entry(&["bisect"], GROUP),
    entry(
        &["bisect", "start"],
        json_discriminators(
            opaque_schemas(WORKTREE_MUTATION, &["bisect start"]),
            &[json_discriminator(
                Some("bisect start"),
                "output_kind",
                "bisect_start",
            )],
        ),
    ),
    entry(
        &["bisect", "good"],
        json_discriminators(
            opaque_schemas(WORKTREE_MUTATION, &["bisect good"]),
            &[json_discriminator(
                Some("bisect good"),
                "output_kind",
                "bisect_good",
            )],
        ),
    ),
    entry(
        &["bisect", "bad"],
        json_discriminators(
            opaque_schemas(WORKTREE_MUTATION, &["bisect bad"]),
            &[json_discriminator(
                Some("bisect bad"),
                "output_kind",
                "bisect_bad",
            )],
        ),
    ),
    entry(
        &["bisect", "reset"],
        json_discriminators(
            opaque_schemas(WORKTREE_MUTATION, &["bisect reset"]),
            &[json_discriminator(
                Some("bisect reset"),
                "output_kind",
                "bisect_reset",
            )],
        ),
    ),
    entry(&["blame"], documented_schemas(READ_JSON, &["blame"])),
    entry(
        &["branch"],
        git_adapter_action(
            documented_schemas(MUTATING, &["branch"]),
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
            documented_schemas(
                CommandContract {
                    writes_heddle_refs: false,
                    writes_git_refs: true,
                    network_io: true,
                    ..MUTATING
                },
                &["bridge git push"],
            ),
            "push",
        ),
    ),
    entry(
        &["bridge", "git", "pull"],
        git_adapter_alias(
            documented_schemas(
                CommandContract {
                    writes_git_refs: true,
                    network_io: true,
                    ..WORKTREE_MUTATION
                },
                &["bridge git pull"],
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
        &["capture"],
        json_discriminators(
            documented_schemas(CAPTURE, &["capture"]),
            &[json_discriminator(
                Some("capture"),
                "output_kind",
                "capture",
            )],
        ),
    ),
    entry(
        &["checkpoint"],
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
    ),
    entry(
        &["checkout"],
        git_adapter_alias(
            documented_schemas(WORKTREE_MUTATION, &["checkout"]),
            "thread switch",
        ),
    ),
    entry(
        &["cherry-pick"],
        json_discriminators(
            opaque_schemas(WORKTREE_MUTATION, &["cherry-pick"]),
            &[json_discriminator(
                Some("cherry-pick"),
                "output_kind",
                "cherry_pick",
            )],
        ),
    ),
    entry(
        &["clean"],
        json_discriminators(
            documented_schemas(DESTRUCTIVE_WORKTREE_ONLY_MUTATION, &["clean"]),
            &[json_discriminator(Some("clean"), "output_kind", "clean")],
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
    entry(&["collapse"], opaque_schemas(MUTATING, &["collapse"])),
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
    entry(&["compare"], opaque_schemas(READ_JSON, &["compare"])),
    entry(&["completion"], READ_TEXT),
    entry(&["conflict"], GROUP),
    entry(
        &["conflict", "list"],
        opaque_schemas(READ_JSON, &["conflict list"]),
    ),
    entry(
        &["conflict", "show"],
        opaque_schemas(READ_JSON, &["conflict show"]),
    ),
    entry(&["continue"], documented_schemas(MUTATING, &["continue"])),
    entry(&["context"], GROUP),
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
        surface(opaque_schemas(DAEMON_MUTATION, &["daemon stop"]), "admin"),
    ),
    entry(
        &["delegate"],
        documented_schemas(WORKTREE_MUTATION, &["delegate"]),
    ),
    entry(&["diagnose"], documented_schemas(READ_JSON, &["diagnose"])),
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
    entry(&["discuss"], GROUP),
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
        front_door(documented_schemas(READ_JSON, &["doctor"]), 120),
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
            documented_schemas(
                CommandContract {
                    writes_git_refs: true,
                    network_io: true,
                    ..MUTATING
                },
                &["fetch"],
            ),
            "pull",
            "workflow",
            "Use pull for the normal remote update workflow; inspect verification output before materializing changes.",
        ),
    ),
    entry(
        &["fork"],
        json_discriminators(
            opaque_schemas(MUTATING, &["fork"]),
            &[json_discriminator(Some("fork"), "output_kind", "fork")],
        ),
    ),
    entry(&["fsck"], documented_schemas(MUTATING, &["fsck"])),
    entry(&["gc"], hidden(opaque_schemas(GC_MUTATION, &["gc"]))),
    entry(
        &["git-overlay"],
        documented_schemas(READ_JSON, &["git-overlay"]),
    ),
    entry(
        &["goto"],
        json_discriminators(
            documented_schemas(WORKTREE_MUTATION, &["goto"]),
            &[json_discriminator(Some("goto"), "output_kind", "goto")],
        ),
    ),
    entry(
        &["harness-bridge"],
        hidden(opaque_schemas(READ_JSONL, &["harness-bridge"])),
    ),
    entry(&["help"], READ_TEXT),
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
        &["index"],
        hidden(documented_schemas(READ_JSON, &["index"])),
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
    entry(&["inspect"], documented_schemas(READ_JSON, &["inspect"])),
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
        front_door(documented_schemas(READ_JSON, &["log", "log --reflog"]), 130),
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
        surface(opaque_schemas(GC_MUTATION, &["maintenance gc"]), "admin"),
    ),
    entry(
        &["maintenance", "index"],
        surface(
            documented_schemas(READ_JSON, &["maintenance index"]),
            "admin",
        ),
    ),
    entry(
        &["maintenance", "monitor"],
        surface(opaque_schemas(READ_JSON, &["maintenance monitor"]), "admin"),
    ),
    entry(&["marker"], GROUP),
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
        exits(
            surface(
                advertised_action(
                    documented_schemas(WORKTREE_MUTATION, &["merge --preview"]),
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
    ),
    entry(
        &["stack"],
        json_discriminators(
            opaque_schemas(READ_JSON, &["stack"]),
            &[json_discriminator(Some("stack"), "output_kind", "stack")],
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
        &["monitor"],
        hidden(opaque_schemas(READ_JSON, &["monitor"])),
    ),
    entry(&["presence"], feature_gated(READ_JSON, "client")),
    entry(&["presence", "publish"], feature_gated(READ_JSON, "client")),
    entry(
        &["pull"],
        exits(
            front_door(
                documented_schemas(
                    CommandContract {
                        writes_git_refs: true,
                        network_io: true,
                        ..WORKTREE_MUTATION
                    },
                    &["pull"],
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
    entry(&["purge"], GROUP),
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
                    documented_schemas(
                        CommandContract {
                            writes_git_refs: true,
                            network_io: true,
                            ..MUTATING
                        },
                        &["push"],
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
    entry(&["query"], documented_schemas(READ_JSON, &["query"])),
    entry(
        &["ready"],
        front_door(documented_schemas(CAPTURE, &["ready"]), 50),
    ),
    entry(&["rebase"], opaque_schemas(WORKTREE_MUTATION, &["rebase"])),
    entry(&["redact"], GROUP),
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
        &["redo"],
        json_discriminators(
            documented_schemas(WORKTREE_MUTATION, &["redo"]),
            &[json_discriminator(Some("redo"), "output_kind", "redo")],
        ),
    ),
    entry(&["remote"], surface(GROUP, "native")),
    entry(
        &["remote", "list"],
        documented_schemas(READ_JSON, &["remote list"]),
    ),
    entry(
        &["remote", "add"],
        documented_schemas(CONFIG_MUTATION, &["remote add"]),
    ),
    entry(
        &["remote", "remove"],
        documented_schemas(CONFIG_MUTATION, &["remote remove"]),
    ),
    entry(
        &["remote", "set-default"],
        documented_schemas(CONFIG_MUTATION, &["remote set-default"]),
    ),
    entry(
        &["remote", "show"],
        documented_schemas(READ_JSON, &["remote show"]),
    ),
    entry(
        &["resolve"],
        front_door(documented_schemas(MUTATING, &["resolve"]), 300),
    ),
    entry(&["retro"], documented_schemas(READ_JSON, &["retro"])),
    entry(
        &["revert"],
        json_discriminators(
            documented_schemas(WORKTREE_MUTATION, &["revert"]),
            &[json_discriminator(Some("revert"), "output_kind", "revert")],
        ),
    ),
    entry(&["review"], GROUP),
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
    entry(&["semantic"], GROUP),
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
    entry(&["shell"], READ_TEXT),
    entry(&["shell", "init"], READ_TEXT),
    entry(
        &["land"],
        front_door(
            documented_schemas(
                CommandContract {
                    writes_git_refs: true,
                    network_io: true,
                    ..MUTATING
                },
                &["land"],
            ),
            70,
        ),
    ),
    entry(
        &["show"],
        front_door(documented_schemas(READ_JSON, &["show"]), 140),
    ),
    entry(
        &["start"],
        front_door(documented_schemas(WORKTREE_MUTATION, &["start"]), 40),
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
                    documented_schemas(READ_JSON_OR_JSONL, &["status"]),
                    &[json_discriminator(Some("status"), "output_kind", "status")],
                ),
                10,
            ),
            &[(0, "ok"), (74, "io reading workspace state")],
        ),
    ),
    entry(&["store"], surface(GROUP, "admin")),
    entry(
        &["store", "warm"],
        surface(opaque_schemas(MUTATING, &["store warm"]), "admin"),
    ),
    entry(&["support"], feature_gated(MUTATING, "client")),
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
            documented_schemas(WORKTREE_MUTATION, &["switch"]),
            "thread switch",
        ),
    ),
    entry(&["sync"], documented_schemas(MUTATING, &["sync"])),
    entry(&["thread"], surface(GROUP, "native")),
    entry(
        &["thread", "create"],
        documented_schemas(MUTATING, &["thread create"]),
    ),
    entry(
        &["thread", "current"],
        documented_schemas(READ_JSON, &["thread current"]),
    ),
    entry(
        &["thread", "switch"],
        documented_schemas(WORKTREE_MUTATION, &["thread switch"]),
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
        documented_schemas(MUTATING, &["thread rename"]),
    ),
    entry(
        &["thread", "refresh"],
        documented_schemas(WORKTREE_MUTATION, &["thread refresh"]),
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
        documented_schemas(MUTATING, &["thread resolve"]),
    ),
    entry(
        &["thread", "promote"],
        documented_schemas(WORKTREE_MUTATION, &["thread promote"]),
    ),
    entry(
        &["thread", "drop"],
        documented_schemas(DESTRUCTIVE_WORKTREE_MUTATION, &["thread drop"]),
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
        documented_schemas(MUTATING, &["thread revoke-approval"]),
    ),
    entry(
        &["thread", "check-merge"],
        documented_schemas(READ_JSON, &["thread check-merge"]),
    ),
    entry(
        &["thread", "cleanup"],
        documented_schemas(DESTRUCTIVE_WORKTREE_MUTATION, &["thread cleanup"]),
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
    entry(
        &["try"],
        documented_schemas(EXTERNAL_WORKTREE_MUTATION, &["try"]),
    ),
    entry(
        &["undo"],
        front_door(
            json_discriminators(
                documented_schemas(WORKTREE_MUTATION, &["undo"]),
                &[json_discriminator(Some("undo"), "output_kind", "undo")],
            ),
            100,
        ),
    ),
    entry(&["version"], documented_schemas(READ_JSON, &["version"])),
    entry(
        &["watch"],
        surface(documented_schemas(READ_JSONL, &["watch"]), "automation"),
    ),
    entry(
        &["workspace"],
        documented_schemas(READ_JSON, &["workspace show"]),
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
            && (tier_filters.is_empty()
                || tier_filters.contains(&command.tier.as_str()))
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
        && let Some(op_id_option) = op_id_option {
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

pub fn command_json_discriminator_for_schema_verb(
    schema_verb: &str,
) -> Option<CommandJsonDiscriminator> {
    active_command_contract_entries()
        .iter()
        .copied()
        .flat_map(|entry| {
            entry
                .contract
                .json_discriminators
                .iter()
                .map(move |discriminator| (entry.path, discriminator))
        })
        .find(|(_, discriminator)| discriminator.schema_verb == Some(schema_verb))
        .map(|(path, discriminator)| json_discriminator_metadata(path, discriminator))
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
        supports_op_id: contract.supports_op_id,
        persists_op_id: contract.persists_op_id,
        uses_bootstrap_op_id_store: uses_bootstrap_op_id_store(contract),
        mutates: contract.mutates,
        observe_only: contract.observe_only,
        help_visibility: contract.help_visibility,
        help_rank: contract.help_rank,
        surface: contract.surface,
        canonical_command: contract.canonical_command,
        canonical_kind: contract.canonical_kind,
        canonical_note: contract.canonical_note,
        advertised_action: contract.advertised_action,
        feature_gate: contract.feature_gate,
        exit_codes: contract.exit_codes,
        side_effects: side_effects(contract),
        side_effect_class: side_effect_class(contract),
        first_run_behavior: first_run_behavior(contract),
        json_kind: contract.json_kind,
        schema_verbs: contract.schema_verbs,
        documented_schema_verbs: contract.documented_schema_verbs,
        opaque_schema_verbs: contract.opaque_schema_verbs,
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
    let mut entries = active_command_contract_entries()
        .iter()
        .copied()
        .filter(|entry| {
            entry.path.len() == 1
                && !matches!(entry.contract.help_visibility, "everyday" | "hidden")
        })
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| (entry.contract.help_rank, entry.path[0]));
    entries.into_iter().map(|entry| entry.path[0]).collect()
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
        Commands::Diagnose(_) => vec!["diagnose"],
        Commands::Verify => vec!["verify"],
        Commands::Doctor(args) => match &args.command {
            None => vec!["doctor"],
            Some(DoctorCommands::Docs(_)) => vec!["doctor", "docs"],
            Some(DoctorCommands::Schemas) => vec!["doctor", "schemas"],
        },
        #[cfg(feature = "git-overlay")]
        Commands::GitOverlay => vec!["git-overlay"],
        Commands::Schemas { .. } => vec!["schemas"],
        Commands::Version => vec!["version"],
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
        Commands::Checkout(_) => vec!["checkout"],
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
        Commands::Revert(_) => vec!["revert"],
        Commands::Undo(_) => vec!["undo"],
        Commands::Redo { .. } => vec!["redo"],
        Commands::Fork { .. } => vec!["fork"],
        Commands::Collapse(_) => vec!["collapse"],
        Commands::Compare { .. } => vec!["compare"],
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
        },
        #[cfg(feature = "semantic")]
        Commands::Semantic { command } => match command {
            SemanticCommands::Hot { .. } => vec!["semantic", "hot"],
        },
        Commands::Completion { .. } => vec!["completion"],
        Commands::Gc { .. } => vec!["gc"],
        Commands::Index { .. } => vec!["index"],
        Commands::Monitor { .. } => vec!["monitor"],
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
        Commands::Store { command } => match command {
            StoreCommands::Warm { .. } => vec!["store", "warm"],
        },
        Commands::Blame { .. } => vec!["blame"],
        Commands::Bisect { command } => match command {
            BisectCommands::Start => vec!["bisect", "start"],
            BisectCommands::Good { .. } => vec!["bisect", "good"],
            BisectCommands::Bad { .. } => vec!["bisect", "bad"],
            BisectCommands::Reset => vec!["bisect", "reset"],
        },
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
        sample(&["bisect", "start"], &["bisect", "start"]),
        sample(&["bisect", "good"], &["bisect", "good"]),
        sample(&["bisect", "bad"], &["bisect", "bad"]),
        sample(&["bisect", "reset"], &["bisect", "reset"]),
        sample(&["blame"], &["blame", "src/lib.rs"]),
        sample(&["branch"], &["branch"]),
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
        sample(&["checkout"], &["checkout", "main"]),
        sample(&["cherry-pick"], &["cherry-pick", "abc123"]),
        sample(&["clean"], &["clean"]),
        sample(&["clone"], &["clone", "remote", "local"]),
        sample(
            &["collapse"],
            &["collapse", "s1", "s2", "--into", "squashed"],
        ),
        sample(&["commit"], &["commit"]),
        sample(&["commands"], &["commands"]),
        sample(&["compare"], &["compare", "s1", "s2"]),
        sample(&["completion"], &["completion", "bash"]),
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
        sample(&["diagnose"], &["diagnose"]),
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
        sample(&["gc"], &["gc"]),
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
        sample(&["index"], &["index"]),
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
        sample(&["monitor"], &["monitor"]),
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
        sample(&["store", "warm"], &["store", "warm"]),
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
        sample(&["try"], &["try", "true"]),
        sample(&["undo"], &["undo"]),
        sample(&["version"], &["version"]),
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
            "heddle clone <remote> <fresh-path>",
            "heddle clone <local-path> <path>",
            "heddle clone /tmp/source <path> --thread main",
            "heddle bridge git import --path <full-git-repo> --ref <ref>",
            "heddle thread promote main",
            "heddle thread resolve main",
            "heddle bisect good <state> or heddle bisect bad <state>",
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
        // cherry-pick, bisect). Any further sweep MUST extend this list
        // and document the addition.
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
                "bisect start",
                "bisect good",
                "bisect bad",
                "bisect reset",
                "bridge git status",
                "bridge git import",
                "bridge git sync",
                "bridge git reconcile",
                "capture",
                "checkpoint",
                "cherry-pick",
                "clean",
                "clone",
                "clone",
                "commit",
                "commands",
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
                "diff",
                "discuss open",
                "discuss append",
                "discuss resolve",
                "discuss list",
                "discuss show",
                "doctor docs",
                "doctor schemas",
                "fork",
                "goto",
                "init",
                "stack",
                "stack ready",
                "stack snapshot",
                "purge apply",
                "purge list",
                "redact apply",
                "redact list",
                "redact show",
                "redact trust add",
                "redact trust list",
                "redact trust remove",
                "redo",
                "revert",
                "review show",
                "review sign",
                "review next",
                "review health",
                "schemas",
                "stash list",
                "stash show",
                "status",
                "thread list",
                "thread show",
                "verify",
                "undo",
                "workspace show",
            ]
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

        let mut schema_verbs = BTreeSet::new();
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
                assert!(
                    schema_verbs.insert(schema_verb),
                    "JSON discriminator schema verb `{schema_verb}` is registered more than once"
                );
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
        assert_eq!(output.possible_values, vec!["json", "text"]);
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
            (vec!["heddle", "completion", "bash"], false),
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
        assert_eq!(runtime.side_effect_class, entry.side_effect_class);
        assert_eq!(
            runtime.side_effects,
            entry
                .side_effects
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
        );
        assert!(runtime.destructive_requires_force);
        assert!(runtime.writes_worktree);
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
