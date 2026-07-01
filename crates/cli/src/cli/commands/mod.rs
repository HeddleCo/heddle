// SPDX-License-Identifier: Apache-2.0
//! CLI command implementations.

mod action_line;
mod actor_cmd;
mod adopt;
mod advice;
mod agent;
mod agent_cmd;
mod auto_capture;
mod blame;
#[cfg(feature = "git-overlay")]
mod bridge;
mod checkpoint;
mod cherry_pick;
mod child_env;
mod clean;
mod clone;
mod collapse;
mod command_catalog;
pub(crate) mod compact;
mod completion;
mod context;
mod daemon;
mod diagnose;
mod diff;
mod discuss;
mod doctor_docs;
mod doctor_schemas;
mod error_envelope;
mod expand;
mod fetch;
mod ff_record;
mod fsck;
mod gc;
mod git_adapter;
mod git_overlay_health;
mod git_overlay_txn;
mod goto;
pub(crate) mod heddleignore_defaults;
mod history_target;
mod hook;
mod import_progress;
mod index;
mod init;
mod integration;
mod log;
mod maintenance;
mod marker;
mod merge;
mod monitor;
mod mount_lifecycle;
mod next_action;
mod operator_core;
mod operator_loop;
mod oplog;
mod oss;
mod purge;
mod query;
mod ready_cmd;
mod rebase;
pub(crate) mod redact;
mod remote;
mod resolve;
mod retro;
mod revert;
mod review;
mod run_cmd;
mod schemas;
#[cfg(feature = "semantic")]
mod semantic_cmd;
mod session;
mod shell;
mod show;
pub(crate) mod snapshot;
mod start_atomic;
mod stash;
mod stash_ops;
mod status;
mod thread;
#[cfg(feature = "client")]
mod thread_approval;
mod thread_cmd;
mod thread_landing;
mod thread_shaping;
mod timeline_cmd;
mod transaction;
mod try_cmd;
mod undo;
mod undo_apply;
mod verify;
mod visibility;
mod watch;
mod workflow;
pub(crate) mod worktree_cmd;
mod worktree_safety;

pub use actor_cmd::{
    cmd_actor_done, cmd_actor_explain, cmd_actor_list, cmd_actor_show, cmd_actor_spawn,
};
pub use adopt::cmd_adopt;
pub use advice::RecoveryAdvice;
// `agent` (singular) is main's local-daemon dispatcher (`heddle agent
// serve|status|stop`). `agent_cmd` is the reservation/orchestration
// API (`heddle agent reserve|capture|ready|release|list`). They share
// a top-level CLI namespace; the dispatcher in main.rs disambiguates
// by subcommand. See [docs/AGENT_API.md] (TODO once docs land) for
// the boundary.
pub use agent::run as cmd_agent;
pub use agent_cmd::{
    agent_api_schema, cmd_agent_capture, cmd_agent_heartbeat, cmd_agent_list, cmd_agent_ready,
    cmd_agent_release, cmd_agent_reserve,
};
#[cfg(feature = "git-overlay")]
pub use bridge::cmd_bridge_git;
pub use checkpoint::run as cmd_checkpoint;
pub use cherry_pick::cmd_cherry_pick;
pub use clean::cmd_clean;
pub use clone::{
    CLONE_CONNECTION_OUTPUT_KIND, CLONE_OUTPUT_KIND, GitOverlayBlobHydrator, cmd_clone,
    register_git_overlay_factory,
};
pub use collapse::cmd_collapse;
pub use command_catalog::{
    CommandCatalogOutput, advanced_help_groups, build_command_catalog, command_canonical_command,
    command_contract_root_commands, command_help_tier, command_help_visibility, command_path,
    command_persists_op_id, command_runtime_contract, command_runtime_contract_for_command,
    command_supports_json_for_command, command_supports_op_id, command_supports_op_id_for_command,
    command_surface, command_uses_bootstrap_op_id_store, observe_only_root_commands,
    operator_envelope_verbs, root_commands_for_advanced_help, root_commands_for_help_visibility,
};
pub use completion::{cmd_complete, cmd_completion};
pub use context::{
    cmd_context_audit, cmd_context_check, cmd_context_edit, cmd_context_get, cmd_context_history,
    cmd_context_list, cmd_context_rm, cmd_context_set, cmd_context_suggest, cmd_context_supersede,
};
#[allow(unused_imports)]
pub(crate) use daemon::client as daemon_client;
pub use daemon::{cmd_daemon_serve, cmd_daemon_status, cmd_daemon_stop};
pub use diagnose::cmd_diagnose;
pub use diff::cmd_diff;
pub use discuss::run as cmd_discuss;
pub use doctor_docs::cmd_doctor_docs;
pub use doctor_schemas::{cmd_doctor_schemas, documented_samples_with_bound_verbs};
pub use error_envelope::{
    print_error_with_hint, print_error_with_hint_with_config, print_parse_error_json_envelope,
};
pub use expand::cmd_expand;
pub use fetch::cmd_fetch;
pub use fsck::cmd_fsck;
pub use gc::cmd_gc;
pub use git_adapter::{cmd_commit_git_adapter, cmd_switch_git_adapter};
#[cfg(feature = "client")]
pub use heddle_client::cmd_auth;
#[cfg(feature = "client")]
pub use heddle_client::cmd_support;
#[cfg(feature = "client")]
pub use heddle_client::{
    PublisherConfig, cmd_presence_publish, resolve_publisher_config, run_publisher,
};
pub use hook::cmd_hook;
pub use index::cmd_index;
pub use init::cmd_init;
pub use integration::{
    cmd_integration, maybe_prompt_init_install, perform_init_install, prompt_init_install_decision,
};
pub use log::{LogCommandOptions, cmd_log};
pub use maintenance::cmd_maintenance;
pub use merge::cmd_merge;
pub(crate) use merge::{bench_detect_renames, bench_find_merge_base, bench_three_way_merge};
pub use monitor::cmd_monitor;
pub use operator_core::operator_emission_output_kinds;
pub use operator_loop::{cmd_abort, cmd_continue, cmd_sync_smart};
pub use oplog::cmd_oplog;
#[cfg(feature = "git-overlay")]
pub use oss::cmd_git_overlay_guide;
pub use purge::cmd_purge;
pub use query::run as cmd_query;
pub use ready_cmd::cmd_ready;
pub use rebase::cmd_rebase;
pub use redact::cmd_redact;
pub use remote::{cmd_pull, cmd_push, cmd_remote};
pub use resolve::cmd_resolve;
pub use retro::{RetroCommandOptions, cmd_retro};
pub use revert::cmd_revert;
pub use review::run as cmd_review;
pub use run_cmd::cmd_run;
pub use schemas::{cmd_schemas, documented_schema_verbs, schema_for_verb, schema_verbs};
#[cfg(feature = "semantic")]
pub use semantic_cmd::cmd_semantic;
pub use session::{
    cmd_session_end, cmd_session_list, cmd_session_segment, cmd_session_show, cmd_session_start,
};
pub use shell::cmd_shell;
pub use show::cmd_show;
pub use snapshot::{SnapshotAgentOverrides, cmd_snapshot};
pub use stash::cmd_stash;
pub use status::cmd_status;
pub use thread::{cmd_start, cmd_thread_show};
pub use thread_cmd::cmd_thread;
pub use thread_shaping::{
    cmd_capture_split, cmd_thread_absorb, cmd_thread_move, cmd_thread_resolve,
};
pub use timeline_cmd::cmd_timeline;
pub use transaction::run as cmd_transaction;
pub use try_cmd::cmd_try;
pub use undo::{cmd_redo, cmd_undo};
pub use verify::cmd_verify;
pub use visibility::cmd_visibility;
pub use watch::cmd_watch;
pub use workflow::{cmd_land, cmd_sync};
