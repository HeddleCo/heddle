// SPDX-License-Identifier: Apache-2.0
//! Action operation types.

use serde::{Deserialize, Serialize};

use super::ChangeId;

/// Type of operation performed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Operation {
    /// Repository initialization.
    Init,
    /// Capture worktree as new state.
    Snapshot,
    /// Move worktree to a different state.
    Goto,
    /// Create a new branch of exploration.
    Fork,
    /// Squash multiple states into one.
    Collapse {
        /// Source states that were collapsed.
        sources: Vec<ChangeId>,
    },
    /// AI-generated merge/reconciliation.
    Synthesize {
        /// Source states that were synthesized.
        sources: Vec<ChangeId>,
    },
    /// Update a thread reference.
    ThreadUpdate {
        /// Name of the thread.
        thread: String,
    },
    /// Import from external source.
    Import {
        /// Description of the source.
        source: String,
    },
}

impl Operation {
    /// Get a short description of the operation.
    pub fn description(&self) -> &'static str {
        match self {
            Operation::Init => "initialize repository",
            Operation::Snapshot => "snapshot",
            Operation::Goto => "goto",
            Operation::Fork => "fork",
            Operation::Collapse { .. } => "collapse",
            Operation::Synthesize { .. } => "synthesize",
            Operation::ThreadUpdate { .. } => "update thread",
            Operation::Import { .. } => "import",
        }
    }
}