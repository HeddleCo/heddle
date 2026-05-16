// SPDX-License-Identifier: Apache-2.0
//! CLI command implementations.

mod actor_cmd;
mod agent;
mod agent_cmd;
mod attempt;
mod bisect;
mod blame;
#[cfg(feature = "git-overlay")]
mod bridge;
mod checkpoint;
mod cherry_pick;
mod clean;
mod clone;
mod collapse;
mod compare;
mod completion;
mod conflict;
mod context;
mod daemon;
mod diagnose;
mod diff;
mod discuss;
mod doctor_docs;
mod doctor_schemas;
mod fetch;
mod fork;
mod fsck;
mod fsck_checks;
mod gc;
mod goto;
mod harness_bridge;
mod history_target;
mod hook;
mod index;
mod init;
mod integration;
mod log;
mod maintenance;
mod marker;
mod merge;
mod monitor;
mod mount_lifecycle;
mod operator_core;
mod operator_loop;
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
mod semantic_change_output;
#[cfg(feature = "semantic")]
mod semantic_cmd;
mod session;
mod show;
pub(crate) mod snapshot;
mod stash;
mod stash_ops;
mod status;
mod store_cmd;
mod thread;
#[cfg(feature = "client")]
mod thread_approval;
mod thread_cmd;
mod thread_shaping;
mod transaction;
mod try_cmd;
mod undo;
mod undo_apply;
mod watch;
mod workflow;
mod workspace;
pub(crate) mod worktree_cmd;
mod worktree_safety;

pub use actor_cmd::{
    cmd_actor_done, cmd_actor_explain, cmd_actor_list, cmd_actor_show, cmd_actor_spawn,
};
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
pub use attempt::cmd_attempt;
pub use bisect::cmd_bisect;
pub use blame::cmd_blame;
#[cfg(feature = "git-overlay")]
pub use bridge::cmd_bridge_git;
pub use checkpoint::run as cmd_checkpoint;
pub use cherry_pick::cmd_cherry_pick;
pub use clean::cmd_clean;
pub use clone::{GitOverlayBlobHydrator, cmd_clone};
pub use collapse::cmd_collapse;
pub use compare::cmd_compare;
pub use completion::cmd_completion;
pub use conflict::run as cmd_conflict;
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
pub use doctor_schemas::cmd_doctor_schemas;
pub use fetch::cmd_fetch;
pub use fork::cmd_fork;
pub use fsck::cmd_fsck;
pub use gc::cmd_gc;
pub use goto::cmd_goto;
pub use harness_bridge::cmd_harness_bridge;
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
pub use integration::{cmd_integration, maybe_prompt_init_install};
pub use log::{LogCommandOptions, cmd_log};
pub use maintenance::cmd_maintenance;
pub use marker::cmd_marker;
pub use merge::cmd_merge;
pub(crate) use merge::{bench_detect_renames, bench_find_merge_base, bench_three_way_merge};
pub use monitor::cmd_monitor;
pub use operator_loop::{cmd_abort, cmd_continue, cmd_sync_smart};
#[cfg(feature = "git-overlay")]
pub use oss::cmd_git_overlay_guide;
pub use oss::cmd_version;
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
pub use schemas::{REGISTERED_VERBS, cmd_schemas, schema_for_verb};
#[cfg(feature = "semantic")]
pub use semantic_cmd::cmd_semantic;
pub use session::{
    cmd_session_end, cmd_session_list, cmd_session_segment, cmd_session_show, cmd_session_start,
};
pub use show::cmd_show;
pub use snapshot::{SnapshotAgentOverrides, cmd_snapshot};
pub use stash::cmd_stash;
pub use status::cmd_status;
pub use store_cmd::cmd_store;
pub use thread::{cmd_start, cmd_thread_show};
pub use thread_cmd::cmd_thread;
pub use thread_shaping::{
    cmd_capture_split, cmd_thread_absorb, cmd_thread_move, cmd_thread_resolve,
};
pub use transaction::run as cmd_transaction;
pub use try_cmd::cmd_try;
pub use undo::{cmd_redo, cmd_undo};
pub use watch::cmd_watch;
pub use workflow::{cmd_delegate, cmd_ship, cmd_sync};
pub use workspace::{cmd_workspace, cmd_workspace_show};
