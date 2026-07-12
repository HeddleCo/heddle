// SPDX-License-Identifier: Apache-2.0
//! Machine-readable command catalog.

use std::sync::OnceLock;

use clap::{ArgAction, CommandFactory};
pub use heddle_core::ActionTemplate;
use heddle_core::{
    DiffReport, FsckReport, MachineOutputKind, QueryReport, ReportContract as CoreReportContract,
    StatusReport, VerifyReport,
};
use schemars::JsonSchema;
use serde::Serialize;

#[cfg(feature = "semantic")]
use crate::cli::SemanticCommands;
#[cfg(feature = "git-overlay")]
use crate::cli::cli_args::SyncCommands;
use crate::cli::{
    ActorCommands, AgentCommands, Cli, Commands, ContextCommands, DaemonCommands, DoctorCommands,
    HookCommands, IntegrationCommands, MaintenanceCommands, OplogCommands, PurgeCommands,
    RedactCommands, RedactTrustCommands, RemoteCommands, SessionCommands, ShellCommands,
    StashCommands, ThreadCommands, ThreadMarkerCommands, TimelineCommands, VisibilityCommands,
    cli_args::{
        AgentFanoutCommands, AgentTaskCommands, DiscussCommands, ReviewCommands,
        TransactionCommands,
    },
    render::shell_quote,
};
#[cfg(feature = "client")]
use crate::cli::{AuthCommands, PresenceCommands, ProveCommands, SpoolCommands, SupportCommands};
#[cfg(feature = "git-overlay")]
use crate::cli::{ExportCommands, ImportCommands};

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
    pub side_effects: Vec<CommandSideEffect>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CommandSideEffect {
    ObserveOnly,
    Initialize,
    ImportGit,
    WritesHeddleRefs,
    WritesGitRefs,
    WritesWorktree,
    MayWriteWorktree,
    WritesMetadata,
    WritesConfig,
    WritesHooks,
    NetworkIo,
    DaemonProcess,
    ObjectGc,
    ExternalCommand,
    DestructiveRequiresForce,
    DestructiveData,
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
    pub hidden: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CommandCatalogArgument {
    pub id: String,
    pub value_names: Vec<String>,
    pub help: Option<String>,
    pub required: bool,
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
    writes_metadata: bool,
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
    report_contract: Option<CoreReportContract>,
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
    /// `git_projection` surfaces derive their group from the surface itself
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
    "heddle import git --path <full-git-repo>",
    "heddle import git --path <full-git-repo> --ref <ref>",
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
        "heddle start <task> --parent-thread <THREAD>",
        &["heddle", "start", "<task>", "--parent-thread", "<thread>"],
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
        "heddle import git --path <full-git-repo>",
        &["heddle", "import", "git", "--path", "<full-git-repo>"],
        &["path"],
        false,
    ),
    (
        "heddle import git --path <full-git-repo> --ref <ref>",
        &[
            "heddle",
            "import",
            "git",
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
    writes_metadata: false,
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
    report_contract: None,
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

const MUTATION_BASE: CommandContract = CommandContract {
    mutates: true,
    supports_op_id: true,
    observe_only: false,
    ..READ_JSON
};

const REF_MUTATION: CommandContract = CommandContract {
    may_move_ref: true,
    writes_heddle_refs: true,
    ..MUTATION_BASE
};

const METADATA_MUTATION: CommandContract = CommandContract {
    writes_metadata: true,
    ..MUTATION_BASE
};

const REF_AND_METADATA_MUTATION: CommandContract = CommandContract {
    writes_metadata: true,
    ..REF_MUTATION
};

const NETWORK_METADATA_MUTATION: CommandContract = CommandContract {
    network_io: true,
    ..METADATA_MUTATION
};

const METADATA_MUTATION_NO_OP_ID: CommandContract = CommandContract {
    supports_op_id: false,
    ..METADATA_MUTATION
};

const NETWORK_METADATA_MUTATION_NO_OP_ID: CommandContract = CommandContract {
    network_io: true,
    ..METADATA_MUTATION_NO_OP_ID
};

const NETWORK_METADATA_MUTATION_TEXT: CommandContract = CommandContract {
    supports_json: false,
    supports_op_id: false,
    network_io: true,
    json_kind: "none",
    ..METADATA_MUTATION
};

const CONFIG_MUTATION_NO_OP_ID: CommandContract = CommandContract {
    supports_op_id: false,
    ..CONFIG_MUTATION
};

const NETWORK_CONFIG_MUTATION_TEXT: CommandContract = CommandContract {
    supports_json: false,
    supports_op_id: false,
    network_io: true,
    json_kind: "none",
    ..CONFIG_MUTATION
};

const INIT: CommandContract = CommandContract {
    may_initialize: true,
    may_move_ref: false,
    writes_heddle_refs: false,
    writes_config: true,
    ..MUTATION_BASE
};

const CAPTURE: CommandContract = CommandContract { ..REF_MUTATION };

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
    ..REF_MUTATION
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

const REF_METADATA_WORKTREE_MUTATION: CommandContract = CommandContract {
    writes_metadata: true,
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

const DESTRUCTIVE_DATA_MUTATION: CommandContract = CommandContract {
    destructive_data: true,
    ..METADATA_MUTATION
};

const DESTRUCTIVE_REF_MUTATION: CommandContract = CommandContract {
    destructive_data: true,
    ..REF_MUTATION
};

const IMPORTING_MUTATION: CommandContract = CommandContract {
    may_import_git: true,
    ..REF_MUTATION
};

const ADOPT: CommandContract = CommandContract {
    may_initialize: true,
    may_import_git: true,
    writes_config: true,
    ..REF_MUTATION
};

const CONFIG_MUTATION: CommandContract = CommandContract {
    writes_config: true,
    ..MUTATION_BASE
};

const HOOK_MUTATION: CommandContract = CommandContract {
    writes_hooks: true,
    ..CONFIG_MUTATION
};

const INTEGRATION_INSTALL_MUTATION: CommandContract = CommandContract {
    writes_metadata: true,
    ..HOOK_MUTATION
};

const DAEMON_MUTATION: CommandContract = CommandContract {
    daemon_process: true,
    ..METADATA_MUTATION_NO_OP_ID
};

const GC_MUTATION: CommandContract = CommandContract {
    object_gc: true,
    ..MUTATION_BASE
};

const EXTERNAL_COMMAND_MUTATION: CommandContract = CommandContract {
    external_command: true,
    supports_json: false,
    supports_op_id: false,
    json_kind: "none",
    ..MUTATION_BASE
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

const fn documented_core_report_schema(
    contract: CommandContract,
    report_contract: CoreReportContract,
) -> CommandContract {
    CommandContract {
        supports_json: true,
        json_kind: machine_output_kind_json_kind(report_contract.machine_output_kind),
        report_contract: Some(report_contract),
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

fn contract_schema_verbs(contract: CommandContract) -> impl Iterator<Item = &'static str> {
    contract
        .report_contract
        .into_iter()
        .map(|report_contract| report_contract.schema_name)
        .chain(contract.schema_verbs.iter().copied())
}

fn contract_documented_schema_verbs(
    contract: CommandContract,
) -> impl Iterator<Item = &'static str> {
    contract
        .report_contract
        .into_iter()
        .map(|report_contract| report_contract.schema_name)
        .chain(contract.documented_schema_verbs.iter().copied())
}

fn contract_json_discriminators(
    contract: CommandContract,
) -> impl Iterator<Item = CommandJsonDiscriminatorSpec> {
    contract
        .report_contract
        .into_iter()
        .filter_map(report_json_discriminator_from_contract)
        .chain(contract.json_discriminators.iter().copied())
}

fn report_json_discriminator_from_contract(
    report_contract: CoreReportContract,
) -> Option<CommandJsonDiscriminatorSpec> {
    report_contract
        .output_discriminator
        .map(|discriminator| CommandJsonDiscriminatorSpec {
            schema_verb: Some(report_contract.schema_name),
            field: discriminator.field,
            value: discriminator.value,
            no_schema_reason: None,
        })
}

const fn machine_output_kind_json_kind(kind: MachineOutputKind) -> &'static str {
    match kind {
        MachineOutputKind::Json => "json",
        MachineOutputKind::JsonLines => "jsonl",
        MachineOutputKind::JsonOrJsonLines => "json_or_jsonl",
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

const QUERY_ATTRIBUTION_SCHEMA_VERBS: &[&str] = &["query --attribution"];
const QUERY_JSON_DISCRIMINATORS: &[CommandJsonDiscriminatorSpec] = &[json_discriminator(
    Some("query --attribution"),
    "output_kind",
    "query_attribution",
)];

const fn exits(
    contract: CommandContract,
    exit_codes: &'static [(u8, &'static str)],
) -> CommandContract {
    CommandContract {
        exit_codes,
        ..contract
    }
}

const fn git_projection_alias(
    contract: CommandContract,
    canonical_command: &'static str,
) -> CommandContract {
    git_projection_action(
        contract,
        canonical_command,
        "direct_command",
        "Use this native Heddle command for the same operation.",
    )
}

const fn git_projection_action(
    contract: CommandContract,
    canonical_command: &'static str,
    canonical_kind: &'static str,
    canonical_note: &'static str,
) -> CommandContract {
    CommandContract {
        surface: "git_projection",
        help_visibility: "git_projection",
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
                documented_schemas(operator_envelope(compact_json(REF_MUTATION)), &["abort"]),
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
        surface(
            json_discriminators(
                documented_schemas(REF_AND_METADATA_MUTATION, &["actor spawn"]),
                &[json_discriminator(
                    Some("actor spawn"),
                    "output_kind",
                    "actor_spawn",
                )],
            ),
            "automation",
        ),
    ),
    entry(
        &["actor", "list"],
        surface(
            json_discriminators(
                documented_schemas(READ_JSON, &["actor list"]),
                &[json_discriminator(
                    Some("actor list"),
                    "output_kind",
                    "actor_list",
                )],
            ),
            "automation",
        ),
    ),
    entry(
        &["actor", "show"],
        surface(
            json_discriminators(
                documented_schemas(READ_JSON, &["actor show"]),
                &[json_discriminator(
                    Some("actor show"),
                    "output_kind",
                    "actor_show",
                )],
            ),
            "automation",
        ),
    ),
    entry(
        &["actor", "explain"],
        surface(
            json_discriminators(
                documented_schemas(READ_JSON, &["actor explain"]),
                &[json_discriminator(
                    Some("actor explain"),
                    "output_kind",
                    "actor_explain",
                )],
            ),
            "automation",
        ),
    ),
    entry(
        &["actor", "done"],
        surface(
            json_discriminators(
                documented_schemas(METADATA_MUTATION, &["actor done"]),
                &[json_discriminator(
                    Some("actor done"),
                    "output_kind",
                    "actor_done",
                )],
            ),
            "automation",
        ),
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
            documented_schemas(REF_AND_METADATA_MUTATION, &["agent reserve"]),
            "automation",
        ),
    ),
    entry(
        &["agent", "heartbeat"],
        surface(
            documented_schemas(METADATA_MUTATION, &["agent heartbeat"]),
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
            documented_schemas(METADATA_MUTATION, &["agent release"]),
            "automation",
        ),
    ),
    entry(
        &["agent", "list"],
        surface(documented_schemas(READ_JSON, &["agent list"]), "automation"),
    ),
    entry(&["agent", "task"], surface(GROUP, "automation")),
    entry(
        &["agent", "task", "create"],
        surface(
            json_discriminators(
                documented_schemas(METADATA_MUTATION, &["agent task create"]),
                &[json_discriminator(
                    Some("agent task create"),
                    "output_kind",
                    "agent_task_create",
                )],
            ),
            "automation",
        ),
    ),
    entry(
        &["agent", "task", "list"],
        surface(
            json_discriminators(
                documented_schemas(READ_JSON, &["agent task list"]),
                &[json_discriminator(
                    Some("agent task list"),
                    "output_kind",
                    "agent_task_list",
                )],
            ),
            "automation",
        ),
    ),
    entry(
        &["agent", "task", "show"],
        surface(
            json_discriminators(
                documented_schemas(READ_JSON, &["agent task show"]),
                &[json_discriminator(
                    Some("agent task show"),
                    "output_kind",
                    "agent_task_show",
                )],
            ),
            "automation",
        ),
    ),
    entry(
        &["agent", "task", "update"],
        surface(
            json_discriminators(
                documented_schemas(METADATA_MUTATION, &["agent task update"]),
                &[json_discriminator(
                    Some("agent task update"),
                    "output_kind",
                    "agent_task_update",
                )],
            ),
            "automation",
        ),
    ),
    entry(&["agent", "fanout"], surface(GROUP, "automation")),
    entry(
        &["agent", "fanout", "plan"],
        surface(
            json_discriminators(
                documented_schemas(READ_JSON, &["agent fanout plan"]),
                &[json_discriminator(
                    Some("agent fanout plan"),
                    "output_kind",
                    "agent_fanout_plan",
                )],
            ),
            "automation",
        ),
    ),
    entry(
        &["agent", "fanout", "start"],
        surface(
            json_discriminators(
                documented_schemas(WORKTREE_MUTATION, &["agent fanout start"]),
                &[json_discriminator(
                    Some("agent fanout start"),
                    "output_kind",
                    "agent_fanout_start",
                )],
            ),
            "automation",
        ),
    ),
    entry(&["auth"], category(feature_gated(GROUP, "client"), "repo")),
    entry(
        &["auth", "login"],
        feature_gated(NETWORK_CONFIG_MUTATION_TEXT, "client"),
    ),
    entry(
        &["auth", "logout"],
        feature_gated(
            json_discriminators(
                documented_schemas(CONFIG_MUTATION_NO_OP_ID, &["auth logout"]),
                &[json_discriminator(
                    Some("auth logout"),
                    "output_kind",
                    "auth_logout",
                )],
            ),
            "client",
        ),
    ),
    entry(
        &["auth", "status"],
        feature_gated(
            json_discriminators(
                documented_schemas(READ_JSON, &["auth status"]),
                &[json_discriminator(
                    Some("auth status"),
                    "output_kind",
                    "auth_status",
                )],
            ),
            "client",
        ),
    ),
    entry(
        &["auth", "create-service-token"],
        feature_gated(
            json_discriminators(
                documented_schemas(
                    NETWORK_METADATA_MUTATION_NO_OP_ID,
                    &["auth create-service-token"],
                ),
                &[json_discriminator(
                    Some("auth create-service-token"),
                    "output_kind",
                    "auth_create_service_token",
                )],
            ),
            "client",
        ),
    ),
    entry(&["import"], surface(GROUP, "git_projection")),
    entry(
        &["import", "git"],
        exits(
            json_discriminators(
                documented_schemas(IMPORTING_MUTATION, &["import git"]),
                &[json_discriminator(
                    Some("import git"),
                    "output_kind",
                    "import_git",
                )],
            ),
            &[
                (0, "ok"),
                (65, "malformed git repo or unimportable refs"),
                (74, "io reading git refs"),
            ],
        ),
    ),
    entry(&["export"], surface(GROUP, "git_projection")),
    entry(
        &["export", "git"],
        json_discriminators(
            documented_schemas(
                CommandContract {
                    writes_git_refs: true,
                    ..REF_MUTATION
                },
                &["export git"],
            ),
            &[json_discriminator(
                Some("export git"),
                "output_kind",
                "export_git",
            )],
        ),
    ),
    entry(
        &["sync", "git"],
        exits(
            git_projection_action(
                json_discriminators(
                    documented_schemas(IMPORTING_MUTATION, &["sync git"]),
                    &[json_discriminator(
                        Some("sync git"),
                        "output_kind",
                        "sync_git",
                    )],
                ),
                "adopt",
                "workflow",
                "Use adopt to initialize Heddle from an existing Git repository and import its history.",
            ),
            &[
                (0, "ok"),
                (75, "remote unreachable; safe to retry"),
                (76, "remote rejected payload"),
            ],
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
                        ..REF_MUTATION
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
                    // `clone --recursive --output json` (Spool epic P9) emits a
                    // monorepo summary instead of the single-spool `clone`
                    // payload: the placed per-spool ops + the skipped (EdgeSkip)
                    // child edges. Inline object, no separate schema verb.
                    // Source: `clone::monorepo_clone_output_json`.
                    json_discriminator_no_schema(
                        "monorepo clone summary emitted by `clone --recursive` \
                         (placed spools + skipped child edges; no separate schema)",
                        "output_kind",
                        "clone_monorepo",
                    ),
                ],
            ),
            220,
        ),
    ),
    entry(
        &["collapse"],
        category(opaque_schemas(REF_MUTATION, &["collapse"]), "states"),
    ),
    entry(
        &["expand"],
        category(
            json_discriminators(
                documented_schemas(READ_JSON, &["expand"]),
                &[json_discriminator(Some("expand"), "output_kind", "expand")],
            ),
            "states",
        ),
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
        &["continue"],
        category(
            json_discriminators(
                documented_schemas(operator_envelope(compact_json(REF_MUTATION)), &["continue"]),
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
            opaque_schemas(REF_AND_METADATA_MUTATION, &["context set"]),
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
            opaque_schemas(REF_AND_METADATA_MUTATION, &["context edit"]),
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
            opaque_schemas(REF_AND_METADATA_MUTATION, &["context supersede"]),
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
            opaque_schemas(REF_AND_METADATA_MUTATION, &["context rm"]),
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
    #[cfg(all(feature = "git-overlay", feature = "ingest"))]
    entry(&["context", "reason"], category(GROUP, "context")),
    #[cfg(all(feature = "git-overlay", feature = "ingest"))]
    entry(
        &["context", "reason", "git"],
        surface(
            opaque_schemas(METADATA_MUTATION, &["context reason git"]),
            "git_projection",
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
        &["diff"],
        front_door(
            documented_core_report_schema(READ_JSON, DiffReport::CONTRACT),
            20,
        ),
    ),
    entry(&["discuss"], category(GROUP, "collab")),
    entry(
        &["discuss", "open"],
        json_discriminators(
            documented_schemas(REF_AND_METADATA_MUTATION, &["discuss open"]),
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
            documented_schemas(REF_AND_METADATA_MUTATION, &["discuss append"]),
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
            documented_schemas(REF_AND_METADATA_MUTATION, &["discuss resolve"]),
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
        git_projection_action(
            json_discriminators(
                documented_schemas(
                    CommandContract {
                        writes_git_refs: true,
                        network_io: true,
                        ..REF_MUTATION
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
        &["fsck"],
        category(
            documented_schemas(
                documented_core_report_schema(
                    CommandContract {
                        supports_op_id: false,
                        writes_git_refs: true,
                        writes_metadata: true,
                        may_write_worktree: true,
                        writes_worktree: true,
                        ..IMPORTING_MUTATION
                    },
                    FsckReport::CONTRACT,
                ),
                &["fsck --repair git"],
            ),
            "recovery",
        ),
    ),
    entry(&["oplog"], category(GROUP, "recovery")),
    entry(
        &["oplog", "recover"],
        category(
            json_discriminators(
                opaque_schemas(METADATA_MUTATION_NO_OP_ID, &["oplog recover"]),
                &[json_discriminator(
                    Some("oplog recover"),
                    "output_kind",
                    "oplog_recover",
                )],
            ),
            "recovery",
        ),
    ),
    entry(
        &["git-overlay"],
        category(documented_schemas(READ_JSON, &["git-overlay"]), "repo"),
    ),
    entry(
        &["help"],
        category(
            json_discriminators(
                opaque_schemas(READ_JSON, &["help"]),
                &[json_discriminator(Some("help"), "kind", "command_catalog")],
            ),
            "repo",
        ),
    ),
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
    entry(&["integration"], surface(GROUP, "admin")),
    entry(
        &["integration", "list"],
        surface(
            documented_schemas(READ_JSON, &["integration list"]),
            "admin",
        ),
    ),
    entry(
        &["integration", "install"],
        surface(
            opaque_schemas(INTEGRATION_INSTALL_MUTATION, &["integration install"]),
            "admin",
        ),
    ),
    entry(
        &["integration", "doctor"],
        surface(
            documented_schemas(READ_JSON, &["integration doctor"]),
            "admin",
        ),
    ),
    entry(
        &["integration", "uninstall"],
        surface(
            opaque_schemas(INTEGRATION_INSTALL_MUTATION, &["integration uninstall"]),
            "admin",
        ),
    ),
    entry(
        &["integration", "upgrade"],
        surface(
            opaque_schemas(INTEGRATION_INSTALL_MUTATION, &["integration upgrade"]),
            "admin",
        ),
    ),
    entry(
        &["integration", "relay"],
        hidden(surface(
            opaque_schemas(REF_METADATA_WORKTREE_MUTATION, &["integration relay"]),
            "admin",
        )),
    ),
    entry(
        &["log"],
        front_door(
            json_discriminators(
                documented_schemas(READ_JSON, &["log", "log --reflog", "log --timeline"]),
                &[
                    json_discriminator(Some("log"), "output_kind", "log"),
                    json_discriminator(Some("log --reflog"), "output_kind", "log_reflog"),
                    json_discriminator(Some("log --timeline"), "output_kind", "timeline_log"),
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
        surface(
            opaque_schemas(
                CommandContract {
                    object_gc: true,
                    ..METADATA_MUTATION
                },
                &["maintenance run"],
            ),
            "admin",
        ),
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
        &["presence"],
        category(feature_gated(GROUP, "client"), "collab"),
    ),
    entry(
        &["presence", "publish"],
        feature_gated(NETWORK_METADATA_MUTATION_TEXT, "client"),
    ),
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
                                ..REF_MUTATION
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
                documented_schemas(
                    documented_core_report_schema(READ_JSON, QueryReport::CONTRACT),
                    QUERY_ATTRIBUTION_SCHEMA_VERBS,
                ),
                QUERY_JSON_DISCRIMINATORS,
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
            opaque_schemas(METADATA_MUTATION, &["redact apply"]),
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
    entry(&["redact", "purge"], GROUP),
    entry(
        &["redact", "purge", "apply"],
        json_discriminators(
            opaque_schemas(
                CommandContract {
                    destructive_requires_force: true,
                    ..DESTRUCTIVE_DATA_MUTATION
                },
                &["redact purge apply"],
            ),
            &[json_discriminator(
                Some("redact purge apply"),
                "output_kind",
                "purge_apply",
            )],
        ),
    ),
    entry(
        &["redact", "purge", "list"],
        json_discriminators(
            opaque_schemas(READ_JSON, &["redact purge list"]),
            &[json_discriminator(
                Some("redact purge list"),
                "output_kind",
                "purge_list",
            )],
        ),
    ),
    entry(&["redact", "trust"], GROUP),
    entry(
        &["redact", "trust", "add"],
        json_discriminators(
            opaque_schemas(CONFIG_MUTATION, &["redact trust add"]),
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
            opaque_schemas(CONFIG_MUTATION, &["redact trust remove"]),
            &[json_discriminator(
                Some("redact trust remove"),
                "output_kind",
                "redact_trust_remove",
            )],
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
        front_door(
            json_discriminators(
                documented_schemas(REF_MUTATION, &["resolve"]),
                &[json_discriminator(
                    Some("resolve"),
                    "output_kind",
                    "resolve",
                )],
            ),
            300,
        ),
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
            documented_schemas(METADATA_MUTATION, &["review sign"]),
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
            documented_schemas(METADATA_MUTATION, &["session start"]),
            "automation",
        ),
    ),
    entry(
        &["session", "segment"],
        surface(
            documented_schemas(METADATA_MUTATION, &["session segment"]),
            "automation",
        ),
    ),
    entry(
        &["session", "end"],
        surface(
            documented_schemas(METADATA_MUTATION, &["session end"]),
            "automation",
        ),
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
    entry(&["shell", "prompt"], READ_TEXT),
    entry(&["complete"], hidden(READ_TEXT)),
    entry(
        &["land"],
        front_door(
            json_discriminators(
                documented_schemas(
                    CommandContract {
                        writes_git_refs: true,
                        network_io: true,
                        ..compact_json(REF_MUTATION)
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
        git_projection_action(
            READ_TEXT,
            "capture",
            "conceptual_home",
            "Use capture, commit, and thread captures for durable Heddle saves.",
        ),
    ),
    entry(
        &["stash", "push"],
        git_projection_action(
            documented_schemas(WORKTREE_ONLY_MUTATION, &["stash push"]),
            "capture",
            "workflow",
            "Use capture for a durable named save point before changing the worktree.",
        ),
    ),
    entry(
        &["stash", "list"],
        git_projection_action(
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
        git_projection_action(
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
        git_projection_action(
            documented_schemas(WORKTREE_ONLY_MUTATION, &["stash apply"]),
            "undo",
            "conceptual_home",
            "Use undo to reverse the last Heddle operation; stash apply is not a direct semantic match.",
        ),
    ),
    entry(
        &["stash", "drop"],
        git_projection_action(
            documented_schemas(DESTRUCTIVE_DATA_MUTATION, &["stash drop"]),
            "thread captures",
            "conceptual_home",
            "Use thread captures to inspect and manage durable Heddle save points.",
        ),
    ),
    entry(
        &["stash", "clear"],
        git_projection_action(
            documented_schemas(DESTRUCTIVE_DATA_MUTATION, &["stash clear"]),
            "thread captures",
            "conceptual_home",
            "Use thread captures to inspect and manage durable Heddle save points.",
        ),
    ),
    entry(
        &["stash", "show"],
        git_projection_alias(
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
                documented_core_report_schema(
                    compact_json(READ_JSON_OR_JSONL),
                    StatusReport::CONTRACT,
                ),
                10,
            ),
            &[(0, "ok"), (74, "io reading workspace state")],
        ),
    ),
    entry(&["spool"], category(feature_gated(GROUP, "client"), "repo")),
    entry(
        &["spool", "attach"],
        category(
            feature_gated(NETWORK_METADATA_MUTATION_TEXT, "client"),
            "repo",
        ),
    ),
    entry(
        &["spool", "detach"],
        category(
            feature_gated(NETWORK_METADATA_MUTATION_TEXT, "client"),
            "repo",
        ),
    ),
    entry(
        &["spool", "children"],
        category(feature_gated(READ_TEXT, "client"), "repo"),
    ),
    entry(
        &["spool", "governance"],
        category(feature_gated(READ_TEXT, "client"), "repo"),
    ),
    entry(
        &["spool", "membership"],
        category(feature_gated(READ_TEXT, "client"), "repo"),
    ),
    entry(&["prove"], category(feature_gated(GROUP, "client"), "repo")),
    entry(
        &["prove", "submit"],
        category(
            feature_gated(NETWORK_METADATA_MUTATION_TEXT, "client"),
            "repo",
        ),
    ),
    entry(
        &["prove", "list"],
        category(feature_gated(READ_TEXT, "client"), "repo"),
    ),
    entry(
        &["support"],
        category(feature_gated(GROUP, "client"), "repo"),
    ),
    entry(
        &["support", "grant"],
        feature_gated(
            json_discriminators(
                documented_schemas(NETWORK_METADATA_MUTATION_NO_OP_ID, &["support grant"]),
                &[json_discriminator(
                    Some("support grant"),
                    "output_kind",
                    "support_grant",
                )],
            ),
            "client",
        ),
    ),
    entry(
        &["support", "list"],
        feature_gated(
            json_discriminators(
                documented_schemas(READ_JSON, &["support list"]),
                &[json_discriminator(
                    Some("support list"),
                    "output_kind",
                    "support_list",
                )],
            ),
            "client",
        ),
    ),
    entry(
        &["support", "revoke"],
        feature_gated(
            json_discriminators(
                documented_schemas(NETWORK_METADATA_MUTATION_NO_OP_ID, &["support revoke"]),
                &[json_discriminator(
                    Some("support revoke"),
                    "output_kind",
                    "support_revoke",
                )],
            ),
            "client",
        ),
    ),
    entry(
        &["switch"],
        git_projection_alias(
            json_discriminators(
                documented_schemas(WORKTREE_MUTATION, &["switch"]),
                &[json_discriminator(
                    Some("switch"),
                    "output_kind",
                    "thread_switch",
                )],
            ),
            "thread switch",
        ),
    ),
    entry(
        &["sync"],
        category(
            json_discriminators(
                documented_schemas(operator_envelope(compact_json(REF_MUTATION)), &["sync"]),
                &[json_discriminator(Some("sync"), "output_kind", "sync")],
            ),
            "threads",
        ),
    ),
    entry(&["thread"], category(surface(GROUP, "native"), "threads")),
    entry(
        &["thread", "create"],
        json_discriminators(
            documented_schemas(REF_MUTATION, &["thread create"]),
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
            documented_schemas(REF_MUTATION, &["thread rename"]),
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
        documented_schemas(REF_MUTATION, &["thread move"]),
    ),
    entry(
        &["thread", "absorb"],
        documented_schemas(REF_MUTATION, &["thread absorb"]),
    ),
    entry(
        &["thread", "resolve"],
        json_discriminators(
            documented_schemas(REF_MUTATION, &["thread resolve"]),
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
        documented_schemas(NETWORK_METADATA_MUTATION, &["thread approve"]),
    ),
    entry(
        &["thread", "approvals"],
        documented_schemas(READ_JSON, &["thread approvals"]),
    ),
    entry(
        &["thread", "revoke-approval"],
        json_discriminators(
            documented_schemas(NETWORK_METADATA_MUTATION, &["thread revoke-approval"]),
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
    entry(&["thread", "marker"], GROUP),
    entry(
        &["thread", "marker", "list"],
        json_discriminators(
            documented_schemas(READ_JSON, &["thread marker list"]),
            &[json_discriminator(
                Some("thread marker list"),
                "output_kind",
                "thread_marker_list",
            )],
        ),
    ),
    entry(
        &["thread", "marker", "create"],
        json_discriminators(
            documented_schemas(REF_MUTATION, &["thread marker create"]),
            &[json_discriminator(
                Some("thread marker create"),
                "output_kind",
                "thread_marker_create",
            )],
        ),
    ),
    entry(
        &["thread", "marker", "delete"],
        json_discriminators(
            documented_schemas(DESTRUCTIVE_REF_MUTATION, &["thread marker delete"]),
            &[json_discriminator(
                Some("thread marker delete"),
                "output_kind",
                "thread_marker_delete",
            )],
        ),
    ),
    entry(
        &["thread", "marker", "show"],
        json_discriminators(
            documented_schemas(READ_JSON, &["thread marker show"]),
            &[json_discriminator(
                Some("thread marker show"),
                "output_kind",
                "thread_marker_show",
            )],
        ),
    ),
    entry(&["timeline"], surface(GROUP, "automation")),
    entry(
        &["timeline", "status"],
        surface(
            json_discriminators(
                documented_schemas(READ_JSON, &["timeline status"]),
                &[json_discriminator(
                    Some("timeline status"),
                    "output_kind",
                    "timeline_status",
                )],
            ),
            "automation",
        ),
    ),
    entry(
        &["timeline", "record-start"],
        surface(
            json_discriminators(
                documented_schemas(METADATA_MUTATION, &["timeline record-start"]),
                &[json_discriminator(
                    Some("timeline record-start"),
                    "output_kind",
                    "timeline_record_start",
                )],
            ),
            "automation",
        ),
    ),
    entry(
        &["timeline", "record-finish"],
        surface(
            json_discriminators(
                documented_schemas(METADATA_MUTATION, &["timeline record-finish"]),
                &[json_discriminator(
                    Some("timeline record-finish"),
                    "output_kind",
                    "timeline_record_finish",
                )],
            ),
            "automation",
        ),
    ),
    entry(
        &["timeline", "fork"],
        surface(
            json_discriminators(
                documented_schemas(METADATA_MUTATION, &["timeline fork"]),
                &[json_discriminator(
                    Some("timeline fork"),
                    "output_kind",
                    "timeline_action",
                )],
            ),
            "automation",
        ),
    ),
    entry(
        &["timeline", "reset"],
        surface(
            json_discriminators(
                documented_schemas(REF_METADATA_WORKTREE_MUTATION, &["timeline reset"]),
                &[json_discriminator(
                    Some("timeline reset"),
                    "output_kind",
                    "timeline_action",
                )],
            ),
            "automation",
        ),
    ),
    entry(
        &["timeline", "recover"],
        surface(
            json_discriminators(
                documented_schemas(METADATA_MUTATION, &["timeline recover"]),
                &[json_discriminator(
                    Some("timeline recover"),
                    "output_kind",
                    "timeline_action",
                )],
            ),
            "automation",
        ),
    ),
    entry(&["transaction"], hidden(GROUP)),
    entry(
        &["transaction", "begin"],
        hidden(opaque_schemas(METADATA_MUTATION, &["transaction begin"])),
    ),
    entry(
        &["transaction", "commit"],
        documented_schemas(METADATA_MUTATION, &["transaction commit"]),
    ),
    entry(
        &["transaction", "abort"],
        hidden(opaque_schemas(METADATA_MUTATION, &["transaction abort"])),
    ),
    entry(
        &["transaction", "status"],
        hidden(opaque_schemas(READ_JSON, &["transaction status"])),
    ),
    entry(
        &["verify"],
        exits(
            front_door(
                documented_core_report_schema(READ_JSON, VerifyReport::CONTRACT),
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
            opaque_schemas(METADATA_MUTATION, &["visibility set"]),
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
            opaque_schemas(METADATA_MUTATION, &["visibility promote"]),
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
                // `undo` keeps its own `--list` history view and owns
                // redo-mode after the top-level `redo` deletion.
                // Every kind the handler can emit must be advertised or an agent
                // validating responses via `heddle help --output json` rejects
                // the off-contract record. `undo --list` has its own
                // `UndoListSchema`.
                documented_schemas(WORKTREE_MUTATION, &["undo", "undo --list", "undo --redo"]),
                &[
                    json_discriminator(Some("undo"), "output_kind", "undo"),
                    json_discriminator(Some("undo --list"), "output_kind", "undo_list"),
                    json_discriminator(Some("undo --redo"), "output_kind", "redo"),
                ],
            ),
            100,
        ),
    ),
    entry(
        &["watch"],
        surface(documented_schemas(READ_JSONL, &["watch"]), "automation"),
    ),
];

static ACTIVE_COMMAND_CONTRACT_ENTRIES: OnceLock<Vec<&'static CommandContractEntry>> =
    OnceLock::new();

static ADVERTISED_COMMAND_CONTRACT_ENTRIES: OnceLock<Vec<&'static CommandContractEntry>> =
    OnceLock::new();

const fn entry(path: &'static [&'static str], contract: CommandContract) -> CommandContractEntry {
    CommandContractEntry { path, contract }
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
        .filter(|arg| arg.is_global_set())
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
    append_feature_gated_command_entries(&command, &mut commands, &op_id_option);
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

/// Append catalog entries for `feature_gated` contracts whose clap
/// subcommand is compiled out of THIS build (e.g. the `client`-gated
/// `auth`/`support`/`presence` surfaces in a default `cargo install`).
///
/// `walk_commands` only sees the live clap tree, so without this the
/// hosted verbs would be missing from the catalog `commands` list even
/// though their contract (schema verbs, `output_kind` discriminators)
/// is advertised — breaking the wire-format-stable promise agents read.
/// Entries already produced by the walk (the feature IS on) are skipped.
fn append_feature_gated_command_entries(
    root: &clap::Command,
    out: &mut Vec<CommandCatalogEntry>,
    op_id_option: &Option<CommandCatalogOption>,
) {
    let present: std::collections::BTreeSet<Vec<String>> =
        out.iter().map(|entry| entry.path.clone()).collect();
    for entry in CONTRACTS.iter() {
        if entry.contract.feature_gate.is_none() {
            continue;
        }
        let owned_path: Vec<String> = entry.path.iter().map(|s| (*s).to_string()).collect();
        if present.contains(&owned_path) {
            continue;
        }
        // Defensive: never synthesize an entry that the clap tree
        // actually carries (the feature is enabled in this build).
        if clap_command_path_exists(root, entry.path) {
            continue;
        }
        out.push(feature_gated_catalog_entry(
            &owned_path,
            entry.contract,
            op_id_option,
        ));
    }
}

/// Build a [`CommandCatalogEntry`] for a contract with no live clap node.
/// Clap-derived fields (aliases, summary, options, arguments,
/// subcommand-ness) are empty/false; every behavioral field comes from
/// the contract, identically to [`catalog_entry`].
fn feature_gated_catalog_entry(
    path: &[String],
    contract: CommandContract,
    op_id_option: &Option<CommandCatalogOption>,
) -> CommandCatalogEntry {
    let mut options = Vec::new();
    if contract.supports_op_id
        && let Some(op_id_option) = op_id_option
    {
        options.push(op_id_option.clone());
    }
    CommandCatalogEntry {
        path: path.to_vec(),
        display: path.join(" "),
        aliases: Vec::new(),
        tier: help_visibility_to_tier(contract.help_visibility).to_string(),
        surface: contract.surface.to_string(),
        help_visibility: contract.help_visibility.to_string(),
        help_rank: contract.help_rank,
        canonical_command: contract
            .canonical_command
            .map(std::string::ToString::to_string),
        canonical_action: canonical_action(contract),
        command_action: contract
            .advertised_action
            .map(command_action_from_advertised),
        summary: String::new(),
        has_subcommands: false,
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
        side_effects: side_effects(contract),
        side_effect_class: side_effect_class(contract).to_string(),
        first_run_behavior: first_run_behavior(contract).to_string(),
        json_kind: contract.json_kind.to_string(),
        json_discriminators: json_discriminators_for_path(path.iter().map(String::as_str)),
        schema_verbs: contract_schema_verbs(contract)
            .map(str::to_string)
            .collect(),
        documented_schema_verbs: contract_documented_schema_verbs(contract)
            .map(str::to_string)
            .collect(),
        options,
        arguments: Vec::new(),
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

fn catalog_entry(
    command: &clap::Command,
    path: &[String],
    op_id_option: &Option<CommandCatalogOption>,
) -> CommandCatalogEntry {
    let mut options = Vec::new();
    let mut arguments = Vec::new();
    for arg in command.get_arguments() {
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
        side_effects: side_effects(contract),
        side_effect_class: side_effect_class(contract).to_string(),
        first_run_behavior: first_run_behavior(contract).to_string(),
        json_kind: contract.json_kind.to_string(),
        json_discriminators: json_discriminators_for_path(path.iter().map(String::as_str)),
        schema_verbs: contract_schema_verbs(contract)
            .map(str::to_string)
            .collect(),
        documented_schema_verbs: contract_documented_schema_verbs(contract)
            .map(str::to_string)
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
    } else if contract.writes_metadata {
        "metadata_mutation"
    } else {
        "none"
    }
}

fn side_effects(contract: CommandContract) -> Vec<CommandSideEffect> {
    if contract.observe_only {
        return vec![CommandSideEffect::ObserveOnly];
    }

    let mut effects = Vec::new();
    if contract.may_initialize {
        effects.push(CommandSideEffect::Initialize);
    }
    if contract.may_import_git {
        effects.push(CommandSideEffect::ImportGit);
    }
    if contract.writes_heddle_refs {
        effects.push(CommandSideEffect::WritesHeddleRefs);
    }
    if contract.writes_git_refs {
        effects.push(CommandSideEffect::WritesGitRefs);
    }
    if contract.writes_worktree {
        effects.push(CommandSideEffect::WritesWorktree);
    } else if contract.may_write_worktree {
        effects.push(CommandSideEffect::MayWriteWorktree);
    }
    if contract.writes_metadata {
        effects.push(CommandSideEffect::WritesMetadata);
    }
    if contract.writes_config {
        effects.push(CommandSideEffect::WritesConfig);
    }
    if contract.writes_hooks {
        effects.push(CommandSideEffect::WritesHooks);
    }
    if contract.network_io {
        effects.push(CommandSideEffect::NetworkIo);
    }
    if contract.daemon_process {
        effects.push(CommandSideEffect::DaemonProcess);
    }
    if contract.object_gc {
        effects.push(CommandSideEffect::ObjectGc);
    }
    if contract.external_command {
        effects.push(CommandSideEffect::ExternalCommand);
    }
    if contract.destructive_requires_force {
        effects.push(CommandSideEffect::DestructiveRequiresForce);
    }
    if contract.destructive_data {
        effects.push(CommandSideEffect::DestructiveData);
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
        hidden: arg.is_hide_set(),
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
        hidden: false,
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
            contract_documented_schema_verbs(entry.contract)
                .any(|documented| documented == schema_verb)
        })
        .flat_map(|entry| contract_documented_schema_verbs(entry.contract))
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

/// The contracts advertised to agents: every [`active_command_contract_entries`]
/// entry (present in the compiled clap tree) PLUS any `feature_gated` contract
/// whose clap subcommand is compiled out of this build. The hosted surfaces
/// (`auth`, `support`, `presence`) are `client`-gated, so a default
/// `cargo install heddle-cli` omits their clap nodes — but their schema verbs
/// and `output_kind` discriminators are a wire-format-stable promise that must
/// stay advertised in the catalog regardless of build features. Distinct from
/// the active set, which the clap-tree-equivalence invariants pin exactly.
fn advertised_command_contract_entries() -> &'static [&'static CommandContractEntry] {
    ADVERTISED_COMMAND_CONTRACT_ENTRIES
        .get_or_init(|| {
            let command = Cli::command();
            let mut entries = active_command_contract_entries().to_vec();
            for entry in CONTRACTS.iter() {
                if entry.contract.feature_gate.is_some()
                    && !clap_command_path_exists(&command, entry.path)
                {
                    entries.push(entry);
                }
            }
            entries
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
    advertised_command_contract_entries()
        .iter()
        .copied()
        .flat_map(|entry| {
            contract_json_discriminators(entry.contract)
                .map(move |discriminator| json_discriminator_metadata(entry.path, &discriminator))
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
    advertised_command_contract_entries()
        .iter()
        .copied()
        .filter(|entry| {
            contract_json_discriminators(entry.contract)
                .any(|discriminator| discriminator.schema_verb == Some(schema_verb))
        })
        .flat_map(|entry| {
            let schema_verbs = contract_schema_verbs(entry.contract).collect::<Vec<_>>();
            let include_same_command_siblings =
                schema_verbs.len() == 1 && schema_verbs[0] == schema_verb;
            contract_json_discriminators(entry.contract).filter_map(move |discriminator| {
                if discriminator.schema_verb == Some(schema_verb)
                    || (include_same_command_siblings && discriminator.schema_verb.is_none())
                {
                    Some(json_discriminator_metadata(entry.path, &discriminator))
                } else {
                    None
                }
            })
        })
        .collect()
}

fn json_discriminators_for_path<'a>(
    path: impl IntoIterator<Item = &'a str>,
) -> Vec<CommandJsonDiscriminator> {
    let path = path.into_iter().collect::<Vec<_>>();
    advertised_command_contract_entries()
        .iter()
        .copied()
        .filter(|entry| entry.path == path.as_slice())
        .flat_map(|entry| {
            contract_json_discriminators(entry.contract)
                .map(move |discriminator| json_discriminator_metadata(entry.path, &discriminator))
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

pub(crate) fn command_runtime_contract_for_schema_verb(
    schema_verb: &str,
) -> Option<CommandRuntimeContract> {
    command_runtime_contract(schema_verb)
        .or_else(|| command_runtime_contract(&schema_verb_without_flags(schema_verb)))
}

pub(crate) fn schema_verb_without_flags(schema_verb: &str) -> String {
    schema_verb
        .split_whitespace()
        .filter(|part| !part.starts_with('-'))
        .collect::<Vec<_>>()
        .join(" ")
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
/// registration, while `automation` / `admin` / `git_projection` commands
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
        "git_projection" => "git-interop",
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
    let mut verbs = collect_schema_verbs(contract_schema_verbs);
    verbs.push("error");
    verbs
}

pub(crate) fn documented_schema_verbs() -> Vec<&'static str> {
    let mut verbs = collect_schema_verbs(contract_documented_schema_verbs);
    verbs.push("error");
    verbs
}

pub(crate) fn opaque_schema_verbs() -> Vec<&'static str> {
    collect_schema_verbs(|contract| contract.opaque_schema_verbs.iter().copied())
}

fn collect_schema_verbs<I>(select: impl Fn(CommandContract) -> I) -> Vec<&'static str>
where
    I: IntoIterator<Item = &'static str>,
{
    let mut verbs = Vec::new();
    for entry in advertised_command_contract_entries().iter().copied() {
        for verb in select(entry.contract) {
            if !verbs.contains(&verb) {
                verbs.push(verb);
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
            Some(DoctorCommands::Schemas(_)) => vec!["doctor", "schemas"],
        },
        #[cfg(feature = "git-overlay")]
        Commands::GitOverlay => vec!["git-overlay"],
        Commands::Schemas { .. } => vec!["schemas"],
        Commands::Start(_) => vec!["start"],
        Commands::Try(_) => vec!["try"],
        Commands::Run(_) => vec!["run"],
        Commands::Sync(args) => {
            #[cfg(feature = "git-overlay")]
            {
                if matches!(args.command, Some(SyncCommands::Git { .. })) {
                    return vec!["sync", "git"];
                }
            }
            vec!["sync"]
        }
        Commands::Continue => vec!["continue"],
        Commands::Abort => vec!["abort"],
        Commands::Land(_) => vec!["land"],
        Commands::Ready(_) => vec!["ready"],
        Commands::Capture(_) => vec!["capture"],
        Commands::Commit(_) => vec!["commit"],
        Commands::Checkpoint(_) => vec!["checkpoint"],
        Commands::Log(_) => vec!["log"],
        Commands::Show { .. } => vec!["show"],
        Commands::Retro(_) => vec!["retro"],
        Commands::Clean { .. } => vec!["clean"],
        Commands::Diff(_) => vec!["diff"],
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
            RedactCommands::Purge(command) => match command {
                PurgeCommands::Apply(_) => vec!["redact", "purge", "apply"],
                PurgeCommands::List(_) => vec!["redact", "purge", "list"],
            },
        },
        Commands::Visibility { command } => match command {
            VisibilityCommands::Set(_) => vec!["visibility", "set"],
            VisibilityCommands::Promote(_) => vec!["visibility", "promote"],
            VisibilityCommands::Show(_) => vec!["visibility", "show"],
            VisibilityCommands::List(_) => vec!["visibility", "list"],
        },
        Commands::Revert(_) => vec!["revert"],
        Commands::Undo(_) => vec!["undo"],
        Commands::Collapse(_) => vec!["collapse"],
        Commands::Expand(_) => vec!["expand"],
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
            ThreadCommands::Marker { command } => match command {
                ThreadMarkerCommands::List { .. } => vec!["thread", "marker", "list"],
                ThreadMarkerCommands::Create { .. } => {
                    vec!["thread", "marker", "create"]
                }
                ThreadMarkerCommands::Delete { .. } => {
                    vec!["thread", "marker", "delete"]
                }
                ThreadMarkerCommands::Show { .. } => vec!["thread", "marker", "show"],
            },
        },
        Commands::Timeline(args) => match &args.command {
            TimelineCommands::Status(_) => vec!["timeline", "status"],
            TimelineCommands::RecordStart(_) => vec!["timeline", "record-start"],
            TimelineCommands::RecordFinish(_) => vec!["timeline", "record-finish"],
            TimelineCommands::Fork(_) => vec!["timeline", "fork"],
            TimelineCommands::Reset(_) => vec!["timeline", "reset"],
            TimelineCommands::Recover(_) => vec!["timeline", "recover"],
        },
        Commands::Shell { command } => match command {
            ShellCommands::Init { .. } => vec!["shell", "init"],
            ShellCommands::Completion { .. } => vec!["shell", "completion"],
            ShellCommands::Prompt => vec!["shell", "prompt"],
        },
        Commands::Complete { .. } => vec!["complete"],
        Commands::Merge(_) => vec!["merge"],
        Commands::Resolve(_) => vec!["resolve"],
        Commands::Fsck { .. } => vec!["fsck"],
        #[cfg(feature = "git-overlay")]
        Commands::Import { command } => match command {
            ImportCommands::Git { .. } => vec!["import", "git"],
        },
        #[cfg(feature = "git-overlay")]
        Commands::Export { command } => match command {
            ExportCommands::Git { .. } => vec!["export", "git"],
        },
        Commands::Oplog { command } => match command {
            OplogCommands::Recover => vec!["oplog", "recover"],
        },
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
            #[cfg(all(feature = "git-overlay", feature = "ingest"))]
            ContextCommands::Reason { command } => match command {
                crate::cli::cli_args::ContextReasonCommands::Git(_) => {
                    vec!["context", "reason", "git"]
                }
            },
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
        #[cfg(feature = "client")]
        Commands::Spool { command } => match command {
            SpoolCommands::Attach(_) => vec!["spool", "attach"],
            SpoolCommands::Detach(_) => vec!["spool", "detach"],
            SpoolCommands::Children(_) => vec!["spool", "children"],
            SpoolCommands::Governance(_) => vec!["spool", "governance"],
            SpoolCommands::Membership(_) => vec!["spool", "membership"],
        },
        #[cfg(feature = "client")]
        Commands::Prove(args) => match &args.command {
            Some(ProveCommands::Submit(_)) => vec!["prove", "submit"],
            Some(ProveCommands::List(_)) => vec!["prove", "list"],
            None => vec!["prove"],
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
            AgentCommands::Task(command) => match command {
                AgentTaskCommands::Create(_) => vec!["agent", "task", "create"],
                AgentTaskCommands::List(_) => vec!["agent", "task", "list"],
                AgentTaskCommands::Show(_) => vec!["agent", "task", "show"],
                AgentTaskCommands::Update(_) => vec!["agent", "task", "update"],
            },
            AgentCommands::Fanout(command) => match command {
                AgentFanoutCommands::Plan(_) => vec!["agent", "fanout", "plan"],
                AgentFanoutCommands::Start(_) => vec!["agent", "fanout", "start"],
            },
        },
        Commands::Maintenance { command } => match command {
            MaintenanceCommands::Inspect => vec!["maintenance", "inspect"],
            MaintenanceCommands::Run => vec!["maintenance", "run"],
            MaintenanceCommands::Gc { .. } => vec!["maintenance", "gc"],
            MaintenanceCommands::Index { .. } => vec!["maintenance", "index"],
            MaintenanceCommands::Monitor { .. } => vec!["maintenance", "monitor"],
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
mod tests;
