// SPDX-License-Identifier: Apache-2.0
//! Resolve the target path into the first frontier or a terminal reason.

use std::path::Path;

use crate::{
    object::{ObjectSource, State},
    util::{ResourceBudget, ResourceKind, ResourceUsage},
};

use super::{
    lookup::{load_blob_within_budget, lookup_blob_at_path},
    mapping::identity_mapping,
    types::{
        origin_from_state, BlameFrontierGroup, BlameFrontierRecord, BlamePreparation,
        BlameSliceError, BlameSliceLimits,
    },
};

// Local line count so prepare does not own strings.
fn count_lines(bytes: &[u8]) -> Result<usize, BlameSliceError> {
    std::str::from_utf8(bytes)
        .map(|text| text.lines().count())
        .map_err(|_| BlameSliceError::Unblamable)
}

/// Build the first frontier for `path` at `state` without walking parents.
pub fn prepare_file_blame<S: ObjectSource>(
    source: &S,
    state: &State,
    path: &Path,
    limits: BlameSliceLimits,
) -> Result<BlamePreparation, BlameSliceError> {
    let mut budget = ResourceBudget::new(ResourceUsage {
        scratch_bytes: limits.scratch_bytes,
        lines: limits.lines,
        work: limits.diff_work,
        states: limits.states,
        decoded_bytes: limits.decoded_bytes,
    });
    budget.consume(ResourceKind::States, 1)?;

    let Some(blob_hash) = lookup_blob_at_path(source, &state.tree, path)? else {
        return Ok(BlamePreparation::MissingPath);
    };
    let blob = load_blob_within_budget(source, &blob_hash, &mut budget)?;
    let Ok(line_count) = count_lines(blob.content()) else {
        return Ok(BlamePreparation::Unblamable);
    };
    budget.require(ResourceKind::Lines, line_count as u64)?;

    let origin = origin_from_state(state);
    if line_count == 0 {
        return Ok(BlamePreparation::Empty {
            file_blob: blob_hash,
            origin,
        });
    }

    let line_count = u32::try_from(line_count).map_err(|_| BlameSliceError::Unblamable)?;
    Ok(BlamePreparation::Active {
        file_blob: blob_hash,
        line_count,
        frontier: BlameFrontierGroup {
            records: vec![BlameFrontierRecord {
                origin,
                blob_hash,
                state_line_count: line_count,
                mappings: identity_mapping(line_count),
            }],
        },
    })
}
