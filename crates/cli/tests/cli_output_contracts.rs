// SPDX-License-Identifier: Apache-2.0
//! Output-contract and error-envelope integration tests.
//!
//! One of four themed CLI integration binaries split out of the former
//! single `cli_integration.rs` target (61 modules in one binary made
//! link times and `--test` filters unwieldy). Shared harness lives in
//! `tests/support/mod.rs`.

#[path = "support/mod.rs"]
mod support;
use support::*;

#[path = "cli_integration/cli_help_consistency.rs"]
mod cli_help_consistency;
#[path = "cli_integration/cli_premium_output.rs"]
mod cli_premium_output;
#[path = "cli_integration/compact_output.rs"]
mod compact_output;
#[path = "cli_integration/doctor_docs.rs"]
mod doctor_docs;
#[path = "cli_integration/error_envelope_lint.rs"]
mod error_envelope_lint;
#[path = "cli_integration/exit_codes.rs"]
mod exit_codes;
#[path = "cli_integration/next_action_contract.rs"]
mod next_action_contract;
#[path = "cli_integration/output_kind_invariant.rs"]
mod output_kind_invariant;
#[path = "cli_integration/output_kind_runtime.rs"]
mod output_kind_runtime;
#[path = "cli_integration/output_mode_no_auto.rs"]
mod output_mode_no_auto;
#[path = "cli_integration/stdout_stderr_split.rs"]
mod stdout_stderr_split;
