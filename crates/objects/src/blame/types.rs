// SPDX-License-Identifier: Apache-2.0
//! Persistence-friendly blame slice records.

use serde::{Deserialize, Serialize};

use crate::{
    error::HeddleError,
    object::{ContentHash, Origin, State, StateId},
    util::{BudgetExceeded, LineDiffError, ResourceUsage},
};

/// Caps for one [`super::advance_file_blame_slice`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlameSliceLimits {
    pub states: u64,
    pub decoded_bytes: u64,
    pub lines: u64,
    pub diff_work: u64,
    pub scratch_bytes: u64,
}

impl BlameSliceLimits {
    pub fn unlimited() -> Self {
        Self {
            states: u64::MAX,
            decoded_bytes: u64::MAX,
            lines: u64::MAX,
            diff_work: u64::MAX,
            scratch_bytes: u64::MAX,
        }
    }
}

/// Compact run mapping state-file lines onto target-file lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlameLineMap {
    pub state_start: u32,
    pub target_start: u32,
    pub len: u32,
}

/// Prepared target file a persisted frontier may attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlameTarget {
    pub blob: ContentHash,
    pub line_count: u32,
}

/// One unfinalized hunk still walking toward older ancestors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlameFrontierRecord {
    pub origin: Origin,
    pub blob_hash: ContentHash,
    pub state_line_count: u32,
    pub mappings: Vec<BlameLineMap>,
    pub target: BlameTarget,
}

impl BlameFrontierRecord {
    pub fn state_id(&self) -> StateId {
        self.origin.state_id
    }
}

/// Canonical LIFO frontier group. Replay of the same group is deterministic.
///
/// `target` is the prepared file this group may attribute. A record bound to
/// another job's target is `InvalidFrontier`, not a silent swap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlameFrontierGroup {
    pub target: BlameTarget,
    pub records: Vec<BlameFrontierRecord>,
}

impl BlameFrontierGroup {
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn pop(&mut self) -> Option<BlameFrontierRecord> {
        self.records.pop()
    }

    pub fn push(&mut self, record: BlameFrontierRecord) {
        self.records.push(record);
    }

    /// Fail closed when this group is replayed against a different prepare.
    pub fn require_target(
        &self,
        blob: ContentHash,
        line_count: u32,
    ) -> Result<(), BlameSliceError> {
        if self.target.blob != blob || self.target.line_count != line_count {
            return Err(BlameSliceError::InvalidFrontier(
                "frontier target does not match prepared target".into(),
            ));
        }
        self.require_consistent_target()
    }

    pub fn require_consistent_target(&self) -> Result<(), BlameSliceError> {
        if self
            .records
            .iter()
            .any(|record| record.target != self.target)
        {
            return Err(BlameSliceError::InvalidFrontier(
                "frontier record target does not match group".into(),
            ));
        }
        Ok(())
    }
}

/// Finalized target-line range. Each target line is emitted at most once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OriginRange {
    pub target_start: u32,
    pub len: u32,
    pub origin: Origin,
}

/// Outcome of preparing the target path before any slice walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlamePreparation {
    MissingPath,
    Unblamable,
    Empty {
        file_blob: ContentHash,
        origin: Origin,
    },
    Active {
        file_blob: ContentHash,
        line_count: u32,
        frontier: BlameFrontierGroup,
    },
}

/// Bounded slice result. Callers persist this only on success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlameSliceAdvance {
    Progress {
        next: BlameFrontierGroup,
        finalized: Vec<OriginRange>,
        usage: ResourceUsage,
    },
    Complete {
        finalized: Vec<OriginRange>,
        usage: ResourceUsage,
    },
}

/// Typed slice failure. Missing objects are distinct from budget exhaustion.
#[derive(Debug, thiserror::Error)]
pub enum BlameSliceError {
    #[error("path is absent from the target state")]
    MissingPath,
    #[error("file is binary or otherwise unblamable")]
    Unblamable,
    #[error("missing {kind} {id}")]
    MissingObject { kind: &'static str, id: String },
    #[error(transparent)]
    BudgetExceeded(#[from] BudgetExceeded),
    #[error("invalid frontier: {0}")]
    InvalidFrontier(String),
    #[error("invalid origin coverage")]
    InvalidCoverage,
    #[error(transparent)]
    Store(#[from] HeddleError),
}

impl From<LineDiffError> for BlameSliceError {
    fn from(error: LineDiffError) -> Self {
        match error {
            LineDiffError::InvalidUtf8 => Self::Unblamable,
            LineDiffError::BudgetExceeded(error) => Self::BudgetExceeded(error),
            LineDiffError::Visitor(never) => match never {},
        }
    }
}

pub fn origin_from_state(state: &State) -> Origin {
    Origin {
        state_id: state.id(),
        attribution: state.attribution.clone(),
        created_at: state.created_at,
        authored_at: state.authored_at,
    }
}
