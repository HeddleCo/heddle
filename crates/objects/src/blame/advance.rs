// SPDX-License-Identifier: Apache-2.0
//! Advance one frontier record and return a complete bounded result.

use std::path::Path;

use crate::{
    object::ObjectSource,
    util::{ResourceBudget, ResourceKind, ResourceUsage},
};

use super::{
    lookup::{load_blob_within_budget, lookup_blob_at_path},
    mapping::{
        blob_line_count_matches_frontier, finalize_unmoved, mapped_lines_claimed,
        mappings_fit_state_lines,
    },
    parent::{claim_parent, parent_record, ParentClaim},
    types::{
        origin_from_state, BlameFrontierGroup, BlameSliceAdvance, BlameSliceError,
        BlameSliceLimits, OriginRange,
    },
};

/// Compute one deterministic slice. On success the result is complete; on
/// error callers must not persist half-emitted mutations.
pub fn advance_file_blame_slice<S: ObjectSource>(
    source: &S,
    path: &Path,
    mut frontier_group: BlameFrontierGroup,
    limits: BlameSliceLimits,
) -> Result<BlameSliceAdvance, BlameSliceError> {
    let mut budget = ResourceBudget::new(ResourceUsage {
        scratch_bytes: limits.scratch_bytes,
        lines: limits.lines,
        work: limits.diff_work,
        states: limits.states,
        decoded_bytes: limits.decoded_bytes,
    });

    let Some(entry) = frontier_group.pop() else {
        return Ok(BlameSliceAdvance::Complete {
            finalized: Vec::new(),
            usage: budget.used(),
        });
    };
    mappings_fit_state_lines(&entry.mappings, entry.state_line_count)?;

    budget.consume(ResourceKind::States, 1)?;
    let Some(entry_state) = source.get_state(&entry.state_id())? else {
        return Err(BlameSliceError::MissingObject {
            kind: "state",
            id: entry.state_id().to_string(),
        });
    };
    heddle_perf_contract::record_ancestors_visited(1);

    let expected_origin = origin_from_state(&entry_state);
    if entry.origin != expected_origin {
        return Err(BlameSliceError::InvalidFrontier(
            "origin does not match loaded state".into(),
        ));
    }
    let Some(tree_blob) = lookup_blob_at_path(source, &entry_state.tree, path)? else {
        return Err(BlameSliceError::InvalidFrontier(
            "path is absent from entry state tree".into(),
        ));
    };
    if tree_blob != entry.blob_hash {
        return Err(BlameSliceError::InvalidFrontier(
            "frontier blob does not belong to entry state".into(),
        ));
    }

    let entry_blob = load_blob_within_budget(source, &entry.blob_hash, &mut budget)?;
    let entry_bytes = entry_blob.content();
    let line_count = blob_line_count_matches_frontier(entry_bytes, entry.state_line_count)?;
    budget.consume(ResourceKind::Lines, line_count as u64)?;

    let mut moved = vec![false; line_count];
    if entry_state.parents.is_empty() {
        let finalized = finalize_unmoved(&entry.mappings, &moved, &entry.origin);
        return finish(frontier_group, finalized, budget.used());
    }

    let mut next_parents = Vec::new();
    for parent_id in &entry_state.parents {
        budget.consume(ResourceKind::States, 1)?;
        let Some(parent) = source.get_state(parent_id)? else {
            return Err(BlameSliceError::MissingObject {
                kind: "state",
                id: parent_id.to_string(),
            });
        };
        heddle_perf_contract::record_ancestors_visited(1);
        match claim_parent(
            source,
            super::parent::ParentClaimInput {
                path,
                parent: &parent,
                entry: &entry,
                entry_bytes,
                moved: &mut moved,
            },
            &mut budget,
        )? {
            ParentClaim::MissingPath => {}
            ParentClaim::SameBlob { maps } => {
                if let Some(record) =
                    parent_record(&parent, entry.blob_hash, entry.state_line_count, maps)
                {
                    next_parents.push(record);
                }
                break;
            }
            ParentClaim::Aligned {
                maps,
                blob_hash,
                line_count,
            } => {
                if let Some(record) = parent_record(&parent, blob_hash, line_count, maps) {
                    next_parents.push(record);
                }
            }
        }
        if mapped_lines_claimed(&entry.mappings, &moved) {
            break;
        }
    }

    let finalized = finalize_unmoved(&entry.mappings, &moved, &entry.origin);
    for record in next_parents {
        frontier_group.push(record);
    }
    finish(frontier_group, finalized, budget.used())
}

fn finish(
    next: BlameFrontierGroup,
    finalized: Vec<OriginRange>,
    usage: ResourceUsage,
) -> Result<BlameSliceAdvance, BlameSliceError> {
    if next.is_empty() {
        Ok(BlameSliceAdvance::Complete { finalized, usage })
    } else {
        Ok(BlameSliceAdvance::Progress {
            next,
            finalized,
            usage,
        })
    }
}
