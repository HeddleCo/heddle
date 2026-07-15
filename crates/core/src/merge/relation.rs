// SPDX-License-Identifier: Apache-2.0
//! Merge relationship classification shared by preview and apply flows.

use objects::object::StateId;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MergeRelationKind {
    AlreadyUpToDate,
    FastForward,
    AlreadyIntegrated,
    CleanApply,
    Conflicted,
}

#[derive(Clone, Debug)]
pub struct MergeRelation {
    kind: MergeRelationKind,
    merge_base_id: Option<StateId>,
    conflict_count: usize,
}

impl MergeRelation {
    pub fn new(
        kind: MergeRelationKind,
        _current_state_id: StateId,
        _target_state_id: StateId,
        merge_base_id: Option<StateId>,
        conflict_count: usize,
    ) -> Self {
        Self {
            kind,
            merge_base_id,
            conflict_count,
        }
    }

    pub fn kind(&self) -> MergeRelationKind {
        self.kind
    }

    pub fn merge_base_id(&self) -> Option<StateId> {
        self.merge_base_id
    }

    pub fn conflict_count(&self) -> usize {
        self.conflict_count
    }

    pub fn as_json_value(&self) -> &'static str {
        match self.kind {
            MergeRelationKind::AlreadyUpToDate => "already_up_to_date",
            MergeRelationKind::FastForward => "fast_forward",
            MergeRelationKind::AlreadyIntegrated => "already_integrated",
            MergeRelationKind::CleanApply => "clean_apply",
            MergeRelationKind::Conflicted => "path_conflicts",
        }
    }
}
