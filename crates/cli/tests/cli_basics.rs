// SPDX-License-Identifier: Apache-2.0
//! CLI basics and status integration tests.
//!
//! One of four themed CLI integration binaries split out of the former
//! single `cli_integration.rs` target (61 modules in one binary made
//! link times and `--test` filters unwieldy). Shared harness lives in
//! `tests/support/mod.rs`.

#[path = "support/mod.rs"]
mod support;
use support::*;

#[path = "cli_integration/basics.rs"]
mod basics;
#[path = "cli_integration/capture_vocabulary.rs"]
mod capture_vocabulary;
#[path = "cli_integration/fault_injection.rs"]
mod fault_injection;
#[path = "cli_integration/first_use_resume_transcript.rs"]
mod first_use_resume_transcript;
#[path = "cli_integration/harness_error_surface.rs"]
mod harness_error_surface;
#[path = "cli_integration/identity_resolution.rs"]
mod identity_resolution;
#[path = "cli_integration/ignore_mechanics.rs"]
mod ignore_mechanics;
#[path = "cli_integration/misc.rs"]
mod misc;
#[path = "cli_integration/perf_adopt.rs"]
mod perf_adopt;
#[path = "cli_integration/perf_core_loop/mod.rs"]
mod perf_core_loop;
#[path = "cli_integration/perf_trace.rs"]
mod perf_trace;
#[path = "cli_integration/placeholder_identity.rs"]
mod placeholder_identity;
#[path = "cli_integration/refs_and_history.rs"]
mod refs_and_history;
#[path = "cli_integration/state_id_acceptance.rs"]
mod state_id_acceptance;
#[path = "cli_integration/submodule_status.rs"]
mod submodule_status;
#[path = "cli_integration/transcript_harness.rs"]
mod transcript_harness;
#[path = "cli_integration/watch.rs"]
mod watch;
