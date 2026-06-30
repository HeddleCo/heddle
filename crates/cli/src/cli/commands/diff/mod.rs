// SPDX-License-Identifier: Apache-2.0
//! Diff command.

mod diff_compute;
mod diff_output;

pub use diff_compute::cmd_diff;
pub(crate) use diff_output::{print_diff, print_stat};
pub use heddle_core::{DiffReport, SemanticChangeEntry, compute_state_diff, compute_tree_diff};
