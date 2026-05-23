// SPDX-License-Identifier: Apache-2.0
//! Machine-readable command catalog.

use std::io::{self, Write};

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
    ShellCommands, StashCommands, StoreCommands, ThreadCommands, WorkspaceCommands,
    cli_args::{ConflictCommands, DiscussCommands, ReviewCommands, TransactionCommands},
    should_output_json, style,
};
#[cfg(feature = "client")]
use crate::cli::{AuthCommands, PresenceCommands, SupportCommands};
#[cfg(feature = "git-overlay")]
use crate::cli::{BridgeCommands, GitCommands};

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CommandCatalogOutput {
    pub commands: Vec<CommandCatalogEntry>,
    pub global_options: Vec<CommandCatalogOption>,
    pub recommended_action_placeholders: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CommandCatalogEntry {
    pub path: Vec<String>,
    pub display: String,
    pub tier: String,
    pub summary: String,
    pub has_subcommands: bool,
    pub supports_json: bool,
    pub mutates: bool,
    pub supports_op_id: bool,
    pub persists_op_id: bool,
    pub observe_only: bool,
    pub may_initialize: bool,
    pub may_import_git: bool,
    pub may_write_worktree: bool,
    pub may_move_ref: bool,
    pub destructive_requires_force: bool,
    pub side_effect_class: String,
    pub first_run_behavior: String,
    pub json_kind: String,
    pub schema_verbs: Vec<String>,
    pub documented_schema_verbs: Vec<String>,
    pub options: Vec<CommandCatalogOption>,
    pub arguments: Vec<CommandCatalogArgument>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CommandCatalogOption {
    pub id: String,
    pub long: Option<String>,
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

#[derive(Debug, Clone, Copy)]
pub(crate) struct CommandContract {
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
    json_kind: &'static str,
    schema_verbs: &'static [&'static str],
    documented_schema_verbs: &'static [&'static str],
    help_tier: &'static str,
}

#[derive(Debug, Clone, Copy)]
struct CommandContractEntry {
    path: &'static [&'static str],
    contract: CommandContract,
}

const RECOMMENDED_ACTION_PLACEHOLDERS: &[&str] = &[
    // Choice placeholders: the user must choose one command and fill
    // in the state after inspecting the bisect result.
    "heddle bisect good <state> or heddle bisect bad <state>",
    // Raw-Git recovery is only surfaced while recovering an active
    // raw Git operation Heddle did not start.
    "git bisect good or git bisect bad",
    "git add <files> && heddle continue",
    // Remote setup requires filling in a real name and URL after
    // inspecting current configuration.
    "heddle remote add <name> <url>",
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
    json_kind: "json",
    schema_verbs: &[],
    documented_schema_verbs: &[],
    help_tier: "advanced",
};

const READ_TEXT: CommandContract = CommandContract {
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
    json_kind: "json",
    schema_verbs: &[],
    documented_schema_verbs: &[],
    help_tier: "advanced",
};

const MUTATING_NO_OP_ID: CommandContract = CommandContract {
    supports_op_id: false,
    ..MUTATING
};

const INIT: CommandContract = CommandContract {
    may_initialize: true,
    may_move_ref: false,
    ..MUTATING_NO_OP_ID
};

const CAPTURE: CommandContract = CommandContract {
    may_initialize: true,
    ..MUTATING
};

const WORKTREE_MUTATION: CommandContract = CommandContract {
    may_write_worktree: true,
    ..MUTATING
};

const DESTRUCTIVE_WORKTREE_MUTATION: CommandContract = CommandContract {
    destructive_requires_force: true,
    ..WORKTREE_MUTATION
};

const IMPORTING_MUTATION: CommandContract = CommandContract {
    may_import_git: true,
    ..MUTATING
};

const fn persistent_op_id(contract: CommandContract) -> CommandContract {
    CommandContract {
        persists_op_id: true,
        ..contract
    }
}

const fn schemas(
    contract: CommandContract,
    schema_verbs: &'static [&'static str],
) -> CommandContract {
    CommandContract {
        schema_verbs,
        ..contract
    }
}

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

const fn everyday(contract: CommandContract) -> CommandContract {
    CommandContract {
        help_tier: "everyday",
        ..contract
    }
}

const fn hidden(contract: CommandContract) -> CommandContract {
    CommandContract {
        help_tier: "hidden",
        ..contract
    }
}

const CONTRACTS: &[CommandContractEntry] = &[
    entry(&["abort"], MUTATING),
    entry(&["actor"], MUTATING),
    entry(&["actor", "spawn"], MUTATING),
    entry(&["actor", "list"], READ_JSON),
    entry(&["actor", "show"], READ_JSON),
    entry(&["actor", "explain"], READ_JSON),
    entry(&["actor", "done"], MUTATING),
    entry(&["agent"], MUTATING),
    entry(&["agent", "serve"], MUTATING_NO_OP_ID),
    entry(&["agent", "status"], READ_JSON),
    entry(&["agent", "stop"], MUTATING_NO_OP_ID),
    entry(&["agent", "reserve"], MUTATING),
    entry(&["agent", "heartbeat"], MUTATING),
    entry(&["agent", "capture"], CAPTURE),
    entry(&["agent", "ready"], CAPTURE),
    entry(&["agent", "release"], MUTATING),
    entry(&["agent", "list"], READ_JSON),
    entry(&["attempt"], MUTATING),
    #[cfg(feature = "client")]
    entry(&["auth"], MUTATING),
    #[cfg(feature = "client")]
    entry(&["auth", "login"], MUTATING_NO_OP_ID),
    #[cfg(feature = "client")]
    entry(&["auth", "logout"], MUTATING_NO_OP_ID),
    #[cfg(feature = "client")]
    entry(&["auth", "status"], READ_JSON),
    #[cfg(feature = "client")]
    entry(&["auth", "create-service-token"], MUTATING_NO_OP_ID),
    entry(&["bisect"], WORKTREE_MUTATION),
    entry(&["bisect", "start"], WORKTREE_MUTATION),
    entry(&["bisect", "good"], WORKTREE_MUTATION),
    entry(&["bisect", "bad"], WORKTREE_MUTATION),
    entry(&["bisect", "reset"], WORKTREE_MUTATION),
    entry(&["blame"], READ_JSON),
    entry(&["branch"], MUTATING),
    entry(&["bridge"], everyday(MUTATING)),
    entry(&["bridge", "git"], MUTATING),
    entry(
        &["bridge", "git", "status"],
        documented_schemas(READ_JSON, &["bridge git status"]),
    ),
    entry(
        &["bridge", "git", "init"],
        documented_schemas(INIT, &["bridge git init"]),
    ),
    entry(
        &["bridge", "git", "export"],
        documented_schemas(MUTATING_NO_OP_ID, &["bridge git export"]),
    ),
    entry(
        &["bridge", "git", "import"],
        documented_schemas(IMPORTING_MUTATION, &["bridge git import"]),
    ),
    entry(
        &["bridge", "git", "sync"],
        documented_schemas(IMPORTING_MUTATION, &["bridge git sync"]),
    ),
    entry(&["bridge", "git", "reconcile"], IMPORTING_MUTATION),
    entry(
        &["bridge", "git", "push"],
        documented_schemas(MUTATING, &["bridge git push"]),
    ),
    entry(
        &["bridge", "git", "pull"],
        documented_schemas(WORKTREE_MUTATION, &["bridge git pull"]),
    ),
    entry(&["bridge", "git", "ingest"], IMPORTING_MUTATION),
    entry(&["bridge", "git", "reason"], MUTATING),
    entry(
        &["capture"],
        everyday(schemas(persistent_op_id(CAPTURE), &["capture"])),
    ),
    entry(&["checkpoint"], schemas(CAPTURE, &["checkpoint"])),
    entry(&["checkout"], WORKTREE_MUTATION),
    entry(&["cherry-pick"], WORKTREE_MUTATION),
    entry(&["clean"], DESTRUCTIVE_WORKTREE_MUTATION),
    entry(
        &["clone"],
        everyday(schemas(
            CommandContract {
                may_initialize: true,
                may_write_worktree: true,
                may_move_ref: true,
                ..MUTATING_NO_OP_ID
            },
            &["clone"],
        )),
    ),
    entry(&["collapse"], MUTATING),
    entry(&["commit"], schemas(CAPTURE, &["commit"])),
    entry(&["commands"], documented_schemas(READ_JSON, &["commands"])),
    entry(&["compare"], READ_JSON),
    entry(&["completion"], READ_TEXT),
    entry(&["conflict"], READ_JSON),
    entry(&["conflict", "list"], READ_JSON),
    entry(&["conflict", "show"], READ_JSON),
    entry(&["continue"], MUTATING),
    entry(&["context"], MUTATING),
    entry(&["context", "set"], MUTATING),
    entry(&["context", "get"], READ_JSON),
    entry(&["context", "list"], READ_JSON),
    entry(&["context", "history"], READ_JSON),
    entry(&["context", "edit"], MUTATING),
    entry(&["context", "supersede"], MUTATING),
    entry(&["context", "rm"], MUTATING),
    entry(&["context", "check"], READ_JSON),
    entry(&["context", "suggest"], READ_JSON),
    entry(&["context", "audit"], READ_JSON),
    entry(&["daemon"], MUTATING_NO_OP_ID),
    entry(&["daemon", "serve"], MUTATING_NO_OP_ID),
    entry(&["daemon", "status"], READ_JSON),
    entry(&["daemon", "stop"], MUTATING_NO_OP_ID),
    entry(&["delegate"], MUTATING),
    entry(&["diagnose"], documented_schemas(READ_JSON, &["diagnose"])),
    entry(&["diff"], everyday(schemas(READ_JSON, &["diff"]))),
    entry(&["discuss"], MUTATING),
    entry(&["discuss", "open"], MUTATING),
    entry(&["discuss", "append"], MUTATING),
    entry(&["discuss", "resolve"], MUTATING),
    entry(&["discuss", "list"], READ_JSON),
    entry(&["discuss", "show"], READ_JSON),
    entry(&["doctor"], everyday(READ_JSON)),
    entry(&["doctor", "docs"], READ_JSON),
    entry(&["doctor", "schemas"], READ_JSON),
    entry(&["fetch"], schemas(MUTATING, &["fetch"])),
    entry(&["fork"], MUTATING),
    entry(&["fsck"], MUTATING),
    entry(&["gc"], hidden(MUTATING)),
    entry(&["git-overlay"], READ_JSON),
    entry(&["goto"], WORKTREE_MUTATION),
    entry(&["harness-bridge"], hidden(READ_JSONL)),
    entry(&["help"], everyday(READ_TEXT)),
    entry(&["hook"], MUTATING),
    entry(&["hook", "list"], READ_JSON),
    entry(&["hook", "install"], MUTATING),
    entry(&["hook", "uninstall"], MUTATING),
    entry(&["hook", "events"], READ_JSON),
    entry(&["index"], hidden(READ_JSON)),
    entry(&["init"], everyday(INIT)),
    entry(&["inspect"], READ_JSON),
    entry(&["integration"], MUTATING),
    entry(&["integration", "list"], READ_JSON),
    entry(&["integration", "install"], MUTATING),
    entry(&["integration", "doctor"], READ_JSON),
    entry(&["integration", "uninstall"], MUTATING),
    entry(&["integration", "upgrade"], MUTATING),
    entry(&["integration", "relay"], MUTATING),
    entry(
        &["log"],
        everyday(documented_schemas(READ_JSON, &["log", "log --reflog"])),
    ),
    entry(&["maintenance"], MUTATING),
    entry(&["maintenance", "inspect"], READ_JSON),
    entry(&["maintenance", "run"], MUTATING),
    entry(&["maintenance", "gc"], MUTATING),
    entry(&["maintenance", "index"], READ_JSON),
    entry(&["maintenance", "monitor"], READ_JSON),
    entry(&["marker"], MUTATING),
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
        everyday(schemas(WORKTREE_MUTATION, &["merge --preview"])),
    ),
    entry(&["monitor"], hidden(READ_JSON)),
    #[cfg(feature = "client")]
    entry(&["presence"], READ_JSON),
    #[cfg(feature = "client")]
    entry(&["presence", "publish"], READ_JSON),
    entry(&["pull"], schemas(WORKTREE_MUTATION, &["pull"])),
    entry(
        &["purge"],
        CommandContract {
            destructive_requires_force: true,
            ..MUTATING
        },
    ),
    entry(
        &["purge", "apply"],
        CommandContract {
            destructive_requires_force: true,
            ..MUTATING
        },
    ),
    entry(&["purge", "list"], READ_JSON),
    entry(&["push"], schemas(MUTATING, &["push"])),
    entry(&["query"], READ_JSON),
    entry(&["ready"], everyday(schemas(CAPTURE, &["ready"]))),
    entry(&["rebase"], WORKTREE_MUTATION),
    entry(&["redact"], MUTATING),
    entry(&["redact", "apply"], MUTATING),
    entry(&["redact", "list"], READ_JSON),
    entry(&["redact", "show"], READ_JSON),
    entry(&["redact", "trust"], MUTATING),
    entry(&["redact", "trust", "add"], MUTATING),
    entry(&["redact", "trust", "list"], READ_JSON),
    entry(&["redact", "trust", "remove"], MUTATING),
    entry(&["redo"], WORKTREE_MUTATION),
    entry(&["remote"], MUTATING),
    entry(&["remote", "list"], schemas(READ_JSON, &["remote list"])),
    entry(&["remote", "add"], MUTATING),
    entry(&["remote", "remove"], MUTATING),
    entry(&["remote", "set-default"], MUTATING),
    entry(&["remote", "show"], schemas(READ_JSON, &["remote show"])),
    entry(&["resolve"], everyday(MUTATING)),
    entry(&["retro"], READ_JSON),
    entry(&["revert"], MUTATING),
    entry(&["review"], MUTATING),
    entry(
        &["review", "show"],
        documented_schemas(READ_JSON, &["review show"]),
    ),
    entry(
        &["review", "sign"],
        documented_schemas(persistent_op_id(MUTATING), &["review sign"]),
    ),
    entry(
        &["review", "next"],
        documented_schemas(READ_JSON, &["review next"]),
    ),
    entry(
        &["review", "health"],
        documented_schemas(READ_JSON, &["review health"]),
    ),
    entry(&["run"], MUTATING_NO_OP_ID),
    entry(&["schemas"], READ_JSON),
    entry(&["semantic"], READ_JSON),
    entry(&["semantic", "hot"], READ_JSON),
    entry(&["session"], MUTATING),
    entry(&["session", "start"], MUTATING),
    entry(&["session", "segment"], MUTATING),
    entry(&["session", "end"], MUTATING),
    entry(&["session", "show"], READ_JSON),
    entry(&["session", "list"], READ_JSON),
    entry(&["shell"], READ_TEXT),
    entry(&["shell", "init"], READ_TEXT),
    entry(&["ship"], MUTATING),
    entry(
        &["show"],
        everyday(documented_schemas(READ_JSON, &["show"])),
    ),
    entry(&["start"], everyday(MUTATING)),
    entry(&["stash"], WORKTREE_MUTATION),
    entry(&["stash", "push"], WORKTREE_MUTATION),
    entry(&["stash", "list"], READ_JSON),
    entry(&["stash", "pop"], DESTRUCTIVE_WORKTREE_MUTATION),
    entry(&["stash", "apply"], WORKTREE_MUTATION),
    entry(&["stash", "drop"], DESTRUCTIVE_WORKTREE_MUTATION),
    entry(&["stash", "clear"], DESTRUCTIVE_WORKTREE_MUTATION),
    entry(&["stash", "show"], READ_JSON),
    entry(
        &["status"],
        everyday(documented_schemas(READ_JSON_OR_JSONL, &["status"])),
    ),
    entry(&["store"], MUTATING),
    entry(&["store", "warm"], MUTATING),
    #[cfg(feature = "client")]
    entry(&["support"], MUTATING),
    #[cfg(feature = "client")]
    entry(&["support", "grant"], MUTATING_NO_OP_ID),
    #[cfg(feature = "client")]
    entry(&["support", "list"], READ_JSON),
    #[cfg(feature = "client")]
    entry(&["support", "revoke"], MUTATING_NO_OP_ID),
    entry(&["switch"], WORKTREE_MUTATION),
    entry(&["sync"], MUTATING),
    entry(&["thread"], everyday(MUTATING)),
    entry(&["thread", "create"], MUTATING),
    entry(&["thread", "current"], READ_JSON),
    entry(&["thread", "switch"], WORKTREE_MUTATION),
    entry(&["thread", "cd"], READ_TEXT),
    entry(
        &["thread", "list"],
        documented_schemas(READ_JSON, &["thread list"]),
    ),
    entry(&["thread", "show"], READ_JSON_OR_JSONL),
    entry(&["thread", "captures"], READ_JSON),
    entry(&["thread", "rename"], MUTATING),
    entry(&["thread", "refresh"], WORKTREE_MUTATION),
    entry(&["thread", "move"], MUTATING),
    entry(&["thread", "absorb"], MUTATING),
    entry(&["thread", "resolve"], MUTATING),
    entry(&["thread", "promote"], WORKTREE_MUTATION),
    entry(&["thread", "drop"], DESTRUCTIVE_WORKTREE_MUTATION),
    entry(&["thread", "approve"], MUTATING),
    entry(&["thread", "approvals"], READ_JSON),
    entry(&["thread", "revoke-approval"], MUTATING),
    entry(&["thread", "check-merge"], READ_JSON),
    entry(&["thread", "cleanup"], DESTRUCTIVE_WORKTREE_MUTATION),
    entry(&["transaction"], hidden(MUTATING)),
    entry(&["transaction", "begin"], MUTATING),
    entry(
        &["transaction", "commit"],
        documented_schemas(MUTATING, &["transaction commit"]),
    ),
    entry(&["transaction", "abort"], MUTATING),
    entry(&["transaction", "status"], READ_JSON),
    entry(
        &["trust"],
        everyday(documented_schemas(READ_JSON, &["trust"])),
    ),
    entry(&["try"], MUTATING),
    entry(&["undo"], everyday(WORKTREE_MUTATION)),
    entry(&["version"], READ_JSON),
    entry(&["watch"], READ_JSONL),
    entry(&["workspace"], everyday(READ_JSON)),
    entry(
        &["workspace", "show"],
        documented_schemas(READ_JSON_OR_JSONL, &["workspace show"]),
    ),
];

const fn entry(path: &'static [&'static str], contract: CommandContract) -> CommandContractEntry {
    CommandContractEntry { path, contract }
}

pub fn cmd_commands(cli: &Cli) -> Result<()> {
    let output = build_command_catalog();
    if should_output_json(cli, None) {
        write_stdout(&format!("{}\n", serde_json::to_string(&output)?))?;
        return Ok(());
    }

    let mut rendered = String::new();
    rendered.push_str(&format!("{}\n", style::bold("Command catalog")));
    rendered.push_str("Use `heddle commands --output json` for flags, arguments, and tiers.\n\n");
    for tier in ["everyday", "advanced"] {
        rendered.push_str(&format!("{}:\n", style::bold(tier)));
        for command in output
            .commands
            .iter()
            .filter(|command| command.tier == tier && command.path.len() == 1)
        {
            rendered.push_str(&format!("  {:<14}  {}\n", command.display, command.summary));
        }
        rendered.push('\n');
    }
    write_stdout(&rendered)?;
    Ok(())
}

fn write_stdout(text: &str) -> Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    match out.write_all(text.as_bytes()) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(err) => Err(err.into()),
    }
}

pub fn build_command_catalog() -> CommandCatalogOutput {
    debug_assert!(
        RECOMMENDED_ACTION_PLACEHOLDERS
            .iter()
            .all(|action| validate_recommended_action(action).is_ok())
    );

    let command = Cli::command();
    let global_options = command
        .get_arguments()
        .filter(|arg| arg.is_global_set() && !arg.is_hide_set())
        .map(catalog_option)
        .collect();

    let mut commands = Vec::new();
    walk_commands(&command, &mut Vec::new(), &mut commands);
    CommandCatalogOutput {
        commands,
        global_options,
        recommended_action_placeholders: RECOMMENDED_ACTION_PLACEHOLDERS
            .iter()
            .map(|action| (*action).to_string())
            .collect(),
    }
}

fn walk_commands(
    command: &clap::Command,
    prefix: &mut Vec<String>,
    out: &mut Vec<CommandCatalogEntry>,
) {
    for subcommand in command.get_subcommands().filter(|cmd| !cmd.is_hide_set()) {
        prefix.push(subcommand.get_name().to_string());
        out.push(catalog_entry(subcommand, prefix));
        walk_commands(subcommand, prefix, out);
        prefix.pop();
    }
}

fn catalog_entry(command: &clap::Command, path: &[String]) -> CommandCatalogEntry {
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
    CommandCatalogEntry {
        path: path.to_vec(),
        display: path.join(" "),
        tier: command_help_tier_for_path(path).to_string(),
        summary: command
            .get_about()
            .or_else(|| command.get_long_about())
            .map(|about| about.to_string().lines().next().unwrap_or("").to_string())
            .unwrap_or_default(),
        has_subcommands: command.get_subcommands().any(|cmd| !cmd.is_hide_set()),
        supports_json: contract.supports_json,
        mutates: contract.mutates,
        supports_op_id: contract.supports_op_id,
        persists_op_id: contract.persists_op_id,
        observe_only: contract.observe_only,
        may_initialize: contract.may_initialize,
        may_import_git: contract.may_import_git,
        may_write_worktree: contract.may_write_worktree,
        may_move_ref: contract.may_move_ref,
        destructive_requires_force: contract.destructive_requires_force,
        side_effect_class: side_effect_class(contract).to_string(),
        first_run_behavior: first_run_behavior(contract).to_string(),
        json_kind: contract.json_kind.to_string(),
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
    }
}

fn side_effect_class(contract: CommandContract) -> &'static str {
    if contract.observe_only {
        "observe_only"
    } else if contract.destructive_requires_force {
        "destructive_worktree_mutation"
    } else if contract.may_write_worktree {
        "worktree_mutation"
    } else if contract.may_import_git {
        "git_import"
    } else if contract.may_initialize {
        "initialize"
    } else if contract.may_move_ref {
        "ref_mutation"
    } else if contract.mutates {
        "mutation"
    } else {
        "none"
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

pub(crate) fn command_contract(path: &[String]) -> CommandContract {
    command_contract_for_path(path.iter().map(String::as_str))
        .unwrap_or_else(|| panic!("missing command contract for `{}`", path.join(" ")))
}

pub(crate) fn command_contract_for_path<'a>(
    path: impl IntoIterator<Item = &'a str>,
) -> Option<CommandContract> {
    let path = path.into_iter().collect::<Vec<_>>();
    CONTRACTS
        .iter()
        .find(|entry| entry.path == path.as_slice())
        .map(|entry| entry.contract)
}

pub fn command_supports_op_id(command_name: &str) -> bool {
    command_contract_for_path(command_name.split_whitespace())
        .map(|contract| contract.supports_op_id)
        .unwrap_or(false)
}

pub fn command_persists_op_id(command_name: &str) -> bool {
    command_contract_for_path(command_name.split_whitespace())
        .map(|contract| contract.persists_op_id)
        .unwrap_or(false)
}

pub fn command_supports_op_id_for_command(command: &Commands) -> bool {
    let path = command_path(command);
    command_contract_for_path(path)
        .map(|contract| contract.supports_op_id)
        .unwrap_or(false)
}

pub fn command_supports_json_for_command(command: &Commands) -> bool {
    let path = command_path(command);
    command_contract_for_path(path)
        .map(|contract| contract.supports_json)
        .unwrap_or(false)
}

pub fn command_help_tier(command_name: &str) -> &'static str {
    command_contract_for_path(command_name.split_whitespace())
        .map(|contract| contract.help_tier)
        .unwrap_or("advanced")
}

fn command_help_tier_for_path(path: &[String]) -> &'static str {
    command_contract(path).help_tier
}

pub fn observe_only_root_commands() -> Vec<&'static str> {
    CONTRACTS
        .iter()
        .filter(|entry| {
            entry.path.len() == 1 && entry.contract.observe_only && !entry.contract.mutates
        })
        .map(|entry| entry.path[0])
        .collect()
}

pub fn command_contract_root_commands() -> Vec<&'static str> {
    CONTRACTS
        .iter()
        .filter(|entry| entry.path.len() == 1)
        .map(|entry| entry.path[0])
        .collect()
}

pub(crate) fn validate_recommended_action(action: &str) -> std::result::Result<(), String> {
    let trimmed = action.trim();
    if trimmed.is_empty() || RECOMMENDED_ACTION_PLACEHOLDERS.contains(&trimmed) {
        return Ok(());
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

fn split_recommended_action(action: &str) -> std::result::Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut chars = action.chars().peekable();
    let mut in_double_quote = false;

    while let Some(ch) = chars.next() {
        match ch {
            '"' => in_double_quote = !in_double_quote,
            '\\' if in_double_quote => match chars.next() {
                Some(next) => current.push(next),
                None => current.push('\\'),
            },
            ch if ch.is_whitespace() && !in_double_quote => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            ch => current.push(ch),
        }
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

fn collect_schema_verbs(
    select: impl Fn(CommandContract) -> &'static [&'static str],
) -> Vec<&'static str> {
    let mut verbs = Vec::new();
    for entry in CONTRACTS {
        for verb in select(entry.contract) {
            if !verbs.contains(verb) {
                verbs.push(*verb);
            }
        }
    }
    verbs
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use clap::Parser;

    use super::*;

    #[test]
    fn recommended_actions_parse_through_clap_or_registered_placeholders() {
        for action in [
            "",
            "heddle init",
            "heddle bridge git import --ref main",
            "heddle capture -m \"...\"",
            "heddle stash push -m \"...\"",
            "heddle thread promote main",
            "heddle bridge git reconcile --prefer heddle --ref main --preview",
            "heddle bisect good <state> or heddle bisect bad <state>",
            "git bisect good or git bisect bad",
            "git add <files> && heddle continue",
        ] {
            validate_recommended_action(action)
                .unwrap_or_else(|err| panic!("expected `{action}` to validate: {err}"));
        }
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
    fn command_contract_table_matches_clap_command_tree() {
        let contract_paths = CONTRACTS
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
            contract_paths.len(),
            CONTRACTS.len(),
            "command contract table contains duplicate paths"
        );

        let mut clap_paths = BTreeSet::new();
        collect_clap_command_paths(&Cli::command(), &mut Vec::new(), &mut clap_paths);

        let missing_contracts = clap_paths
            .difference(&contract_paths)
            .map(|path| path.join(" "))
            .collect::<Vec<_>>();
        assert!(
            missing_contracts.is_empty(),
            "Clap exposes command path(s) with no command contract: {missing_contracts:?}"
        );

        let stale_contracts = contract_paths
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
                        && !contract.destructive_requires_force,
                    "`{display}` observe-only commands must not advertise write side effects"
                );
            }
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
            if !contract.schema_verbs.is_empty() {
                assert!(
                    contract.supports_json,
                    "`{display}` registers JSON schema verbs but does not support JSON"
                );
                assert_ne!(
                    contract.json_kind, "jsonl",
                    "`{display}` JSONL command cannot have a single-value schema"
                );
            }
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
    fn command_contract_table_drives_help_tiers() {
        let catalog = build_command_catalog();
        for (display, tier) in [
            ("status", "everyday"),
            ("trust", "everyday"),
            ("commit", "advanced"),
        ] {
            let entry = catalog
                .commands
                .iter()
                .find(|entry| entry.display == display)
                .unwrap_or_else(|| panic!("missing command catalog entry for `{display}`"));
            assert_eq!(entry.tier, tier);
            assert_eq!(command_help_tier(display), tier);
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
    fn op_id_persistence_reads_contract_table() {
        let catalog = build_command_catalog();
        for (display, persists) in [
            ("capture", true),
            ("review sign", true),
            ("commit", false),
            ("status", false),
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
                command_persists_op_id(display),
                persists,
                "`{display}` runtime op-id persistence must come from the contract table"
            );
            if persists {
                assert!(
                    entry.supports_op_id,
                    "`{display}` cannot persist op-ids unless it supports op-id replay"
                );
            }
        }
    }
}

pub fn command_path(command: &Commands) -> Vec<&'static str> {
    match command {
        Commands::Init(_) => vec!["init"],
        Commands::Help { .. } => vec!["help"],
        Commands::Status { .. } => vec!["status"],
        Commands::Watch(_) => vec!["watch"],
        Commands::Diagnose(_) => vec!["diagnose"],
        Commands::Trust => vec!["trust"],
        Commands::Doctor(args) => match &args.command {
            None => vec!["doctor"],
            Some(DoctorCommands::Docs(_)) => vec!["doctor", "docs"],
            Some(DoctorCommands::Schemas) => vec!["doctor", "schemas"],
        },
        #[cfg(feature = "git-overlay")]
        Commands::GitOverlay => vec!["git-overlay"],
        Commands::Schemas { .. } => vec!["schemas"],
        Commands::Version => vec!["version"],
        Commands::Commands => vec!["commands"],
        Commands::Start(_) => vec!["start"],
        Commands::Try(_) => vec!["try"],
        Commands::Attempt(_) => vec!["attempt"],
        Commands::Run(_) => vec!["run"],
        Commands::Sync(_) => vec!["sync"],
        Commands::Continue => vec!["continue"],
        Commands::Abort => vec!["abort"],
        Commands::Ship(_) => vec!["ship"],
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
