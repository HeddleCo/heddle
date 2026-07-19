// SPDX-License-Identifier: Apache-2.0
//! CLI command implementations.

mod action_line;
mod adopt;
mod advice;
mod agent;
mod agent_cmd;
mod agent_presence;
mod agent_provenance;
mod auto_capture;
mod blame;
mod checkpoint;
mod child_env;
mod clone;
mod collapse;
mod command_catalog;
mod commit;
pub(crate) mod compact;
mod completion;
pub(crate) mod context;
mod daemon;
mod diff;
mod discuss;
pub(crate) mod dry_run;
mod doctor;
mod doctor_docs;
mod doctor_schemas;
mod error_envelope;
mod expand;
mod ff_record;
mod fsck;
mod gc;
mod git_overlay_txn;
#[cfg(feature = "git-overlay")]
mod git_projection_io;
pub(crate) mod heddleignore_defaults;
mod history_target;
mod hook;
mod import_progress;
mod init;
mod integration;
mod log;
mod maintenance;
mod marker;
mod merge;
mod mount_lifecycle;
mod next_action;
mod operator_core;
mod operator_loop;
mod oplog;
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
mod shell;
mod show;
pub(crate) mod snapshot;
mod start_atomic;
mod status;
mod thread;
#[cfg(feature = "client")]
mod thread_approval;
mod thread_cmd;
mod thread_landing;
mod thread_shaping;
mod timeline_cmd;
mod try_cmd;
mod undo;
mod undo_apply;
mod verification_health;
mod verify;
mod visibility;
mod watch;
mod workflow;
pub(crate) mod worktree_cmd;
mod worktree_safety;

pub use adopt::cmd_adopt;
pub use advice::RecoveryAdvice;
pub use agent::run as cmd_agent;
pub use agent_cmd::{
    agent_api_schema, cmd_agent_capture, cmd_agent_heartbeat, cmd_agent_list, cmd_agent_ready,
    cmd_agent_release, cmd_agent_reserve,
};
pub use clone::{
    CLONE_CONNECTION_OUTPUT_KIND, CLONE_OUTPUT_KIND, GitOverlayBlobHydrator, cmd_clone,
    register_git_overlay_factory,
};
pub use collapse::cmd_collapse;
pub use command_catalog::{
    CommandCatalogOutput, CommandRuntimeContract, advanced_help_groups, build_command_catalog,
    command_canonical_command, command_contract_root_commands, command_help_tier,
    command_help_visibility, command_path, command_persists_op_id, command_runtime_contract,
    command_runtime_contract_for_command, command_supports_json_for_command,
    command_supports_op_id, command_supports_op_id_for_command, command_surface,
    command_uses_bootstrap_op_id_store, observe_only_root_commands, operator_envelope_verbs,
    root_commands_for_advanced_help, root_commands_for_help_visibility,
};
pub use commit::cmd_commit;
pub use completion::{cmd_complete, cmd_completion};
pub use context::{
    cmd_context_audit, cmd_context_check, cmd_context_edit, cmd_context_get, cmd_context_history,
    cmd_context_list, cmd_context_rm, cmd_context_set, cmd_context_suggest, cmd_context_supersede,
};
#[allow(unused_imports)]
pub(crate) use daemon::client as daemon_client;
pub use daemon::{cmd_daemon_serve, cmd_daemon_status, cmd_daemon_stop};
pub use diff::cmd_diff;
pub use discuss::run as cmd_discuss;
pub use doctor::cmd_doctor;
pub use doctor_docs::cmd_doctor_docs;
pub use doctor_schemas::{cmd_doctor_schemas, documented_samples_with_bound_verbs};
pub use error_envelope::{
    print_error_with_hint, print_error_with_hint_with_config, print_parse_error_json_envelope,
};
pub use expand::cmd_expand;
pub use fsck::{cmd_fsck, cmd_fsck_repair_git};
pub use gc::cmd_gc;
#[cfg(all(feature = "git-overlay", feature = "ingest"))]
pub use git_projection_io::cmd_context_reason_git;
#[cfg(feature = "git-overlay")]
pub use git_projection_io::{cmd_export_git, cmd_import_git, cmd_sync_git};
#[cfg(feature = "client")]
pub use heddle_client::cmd_auth;
pub use hook::cmd_hook;
pub use init::cmd_init;
pub use integration::{
    cmd_integration, maybe_prompt_init_install, perform_init_install, prompt_init_install_decision,
};
pub use log::{LogCommandOptions, cmd_log};
pub use maintenance::cmd_maintenance;
pub(crate) use merge::{bench_detect_renames, bench_find_merge_base, bench_three_way_merge};
pub use operator_core::operator_emission_output_kinds;
pub use operator_loop::{cmd_abort, cmd_continue, cmd_sync_smart};
pub use oplog::cmd_oplog;
pub use purge::cmd_purge;
pub use query::run as cmd_query;
pub use ready_cmd::cmd_ready;
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
pub use shell::cmd_shell;
pub use show::cmd_show;
pub use snapshot::{SnapshotAgentOverrides, cmd_snapshot};
pub use status::cmd_status;
pub use thread::{cmd_start, cmd_thread_show};
pub use thread_cmd::cmd_thread;
pub use thread_shaping::{
    cmd_capture_split, cmd_thread_absorb, cmd_thread_move, cmd_thread_resolve,
};
pub use timeline_cmd::cmd_timeline;
pub use try_cmd::cmd_try;
pub use undo::{cmd_redo, cmd_undo, cmd_undo_recover};
pub use verify::cmd_verify;
pub use visibility::cmd_visibility;
pub use watch::cmd_watch;
pub use workflow::recover_incomplete_land_if_present;
pub use workflow::{cmd_land, cmd_sync};
