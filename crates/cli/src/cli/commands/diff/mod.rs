// SPDX-License-Identifier: Apache-2.0
//! Diff command.

mod diff_compute;
mod diff_output;
mod diff_types;

pub use diff_compute::{cmd_diff, compute_state_diff};
pub use diff_types::DiffOutput;