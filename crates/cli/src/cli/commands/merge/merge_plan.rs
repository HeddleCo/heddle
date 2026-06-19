// SPDX-License-Identifier: Apache-2.0
//! Shared merge planning seam for preview and apply flows.

use anyhow::{Result, anyhow};
use objects::{object::ChangeId, store::BlockingObjectStore};
use repo::{CommitGraphIndex, Repository};

use super::{
    merge_algo::{ConflictLabels, MergeResult, three_way_merge_with_labels},
    merge_relation::{MergeRelation, MergeRelationKind},
};
use crate::cli::commands::RecoveryAdvice;

pub(crate) struct MergePlan {
    relation: MergeRelation,
    merge_result: Option<MergeResult>,
}

impl MergePlan {
    pub(crate) fn for_merge_command(
        repo: &Repository,
        graph: &mut CommitGraphIndex<'_>,
        current_change_id: &ChangeId,
        target_change_id: &ChangeId,
        labels: ConflictLabels<'_>,
    ) -> Result<Self> {
        Self::build(
            repo,
            graph,
            current_change_id,
            target_change_id,
            MergeRelationKind::AlreadyUpToDate,
            labels,
        )
    }

    pub(crate) fn for_thread_preview(
        repo: &Repository,
        graph: &mut CommitGraphIndex<'_>,
        target_change_id: &ChangeId,
        thread_change_id: &ChangeId,
        labels: ConflictLabels<'_>,
    ) -> Result<Self> {
        Self::build(
            repo,
            graph,
            target_change_id,
            thread_change_id,
            MergeRelationKind::AlreadyIntegrated,
            labels,
        )
    }

    pub(crate) fn relation(&self) -> &MergeRelation {
        &self.relation
    }

    pub(crate) fn merge_result(&self) -> Option<&MergeResult> {
        self.merge_result.as_ref()
    }

    fn build(
        repo: &Repository,
        graph: &mut CommitGraphIndex<'_>,
        current_change_id: &ChangeId,
        target_change_id: &ChangeId,
        integrated_kind: MergeRelationKind,
        labels: ConflictLabels<'_>,
    ) -> Result<Self> {
        if graph.is_ancestor(target_change_id, current_change_id)? {
            return Ok(Self {
                relation: MergeRelation::new(
                    integrated_kind,
                    *current_change_id,
                    *target_change_id,
                    None,
                    0,
                ),
                merge_result: None,
            });
        }

        if graph.is_ancestor(current_change_id, target_change_id)? {
            return Ok(Self {
                relation: MergeRelation::new(
                    MergeRelationKind::FastForward,
                    *current_change_id,
                    *target_change_id,
                    None,
                    0,
                ),
                merge_result: None,
            });
        }

        let merge_base_id = graph
            .find_merge_base(current_change_id, target_change_id)?
            .ok_or_else(|| {
                anyhow!(RecoveryAdvice::merge_no_common_ancestor(
                    &current_change_id.short(),
                    &target_change_id.short(),
                ))
            })?;
        let base_tree = load_tree(repo, &merge_base_id)?;
        let current_tree = load_tree(repo, current_change_id)?;
        let target_tree = load_tree(repo, target_change_id)?;
        let merge_result =
            three_way_merge_with_labels(repo, &base_tree, &current_tree, &target_tree, labels)?;
        let relation_kind = if merge_result.conflicts.is_empty() {
            MergeRelationKind::CleanApply
        } else {
            MergeRelationKind::Conflicted
        };

        Ok(Self {
            relation: MergeRelation::new(
                relation_kind,
                *current_change_id,
                *target_change_id,
                Some(merge_base_id),
                merge_result.conflicts.len(),
            ),
            merge_result: Some(merge_result),
        })
    }
}

fn load_tree(repo: &Repository, change_id: &ChangeId) -> Result<objects::object::Tree> {
    let state = repo
        .store()
        .get_state(change_id)?
        .ok_or_else(|| anyhow!("State '{}' not found", change_id.short()))?;
    // `state.tree` is recorded by the state object — the tree MUST be
    // present in the store for that state to be meaningful. Coercing
    // `Ok(None)` to `Tree::default()` here meant a corrupt store
    // produced a clean merge against an empty side, silently erasing
    // every tracked file. Surface the corruption instead.
    repo.store().get_tree(&state.tree)?.ok_or_else(|| {
        anyhow!(RecoveryAdvice::merge_integrity_refusal(
            format!(
                "State {} references missing tree {}",
                change_id.short(),
                state.tree
            ),
            format!(
                "state {} references tree {} but the object store has no such tree",
                change_id.short(),
                state.tree
            ),
            "merging against a missing tree would silently treat that side as empty and could erase tracked files",
            "merge stopped before writing refs, metadata, or worktree files",
        ))
    })
}
