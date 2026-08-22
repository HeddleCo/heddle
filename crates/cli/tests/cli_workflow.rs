// SPDX-License-Identifier: Apache-2.0
//! Thread and workflow integration tests.
//!
//! One of four themed CLI integration binaries split out of the former
//! single `cli_integration.rs` target (61 modules in one binary made
//! link times and `--test` filters unwieldy). Shared harness lives in
//! `tests/support/mod.rs`.

#[path = "support/mod.rs"]
mod support;
use support::*;

#[path = "cli_integration/context_recovery_advice.rs"]
mod context_recovery_advice;
#[path = "cli_integration/current_context_advice.rs"]
mod current_context_advice;
#[path = "cli_integration/diff_patch_conformance.rs"]
mod diff_patch_conformance;
#[path = "cli_integration/discuss_carry_forward.rs"]
mod discuss_carry_forward;
#[path = "cli_integration/dry_run_preview.rs"]
mod dry_run_preview;
#[path = "cli_integration/hooks.rs"]
mod hooks;
#[path = "cli_integration/interactive_selection.rs"]
mod interactive_selection;
#[path = "cli_integration/land_current_thread.rs"]
mod land_current_thread;
#[path = "cli_integration/oplog_salvage.rs"]
mod oplog_salvage;
#[path = "cli_integration/oss_cli_polish.rs"]
mod oss_cli_polish;
#[path = "cli_integration/redact_purge.rs"]
mod redact_purge;
#[cfg(feature = "telemetry")]
#[path = "cli_integration/telemetry.rs"]
mod telemetry;
#[path = "cli_integration/thread_cleanup.rs"]
mod thread_cleanup;
#[path = "cli_integration/thread_default_current.rs"]
mod thread_default_current;
#[path = "cli_integration/timeline.rs"]
mod timeline;
#[path = "cli_integration/visibility.rs"]
mod visibility;
#[path = "cli_integration/worktree_target_advice.rs"]
mod worktree_target_advice;
