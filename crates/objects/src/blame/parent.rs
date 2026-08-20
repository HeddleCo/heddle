// SPDX-License-Identifier: Apache-2.0
//! First-parent-wins merge policy against one parent of the current frontier.

use std::path::Path;

use crate::{
    object::{ContentHash, ObjectSource, State},
    util::{
        LineDiffLimits, ResourceBudget, ResourceKind, scratch_bytes_for_line_counts,
        visit_lcs_equal_runs,
    },
};

use super::{
    lookup::lookup_blob_at_path,
    mapping::{compact_pairs, target_at},
    types::{
        BlameFrontierRecord, BlameLineMap, BlameSliceError, BlameSliceLimits, origin_from_state,
    },
};

pub(super) enum ParentClaim {
    MissingPath,
    SameBlob {
        maps: Vec<BlameLineMap>,
    },
    Aligned {
        maps: Vec<BlameLineMap>,
        blob_hash: ContentHash,
        line_count: u32,
    },
}

pub(super) struct ParentClaimInput<'a> {
    pub path: &'a Path,
    pub parent: &'a State,
    pub entry: &'a BlameFrontierRecord,
    pub entry_bytes: &'a [u8],
    pub moved: &'a mut [bool],
    pub limits: BlameSliceLimits,
}

pub(super) fn claim_parent<S: ObjectSource>(
    source: &S,
    input: ParentClaimInput<'_>,
    budget: &mut ResourceBudget,
) -> Result<ParentClaim, BlameSliceError> {
    let ParentClaimInput {
        path,
        parent,
        entry,
        entry_bytes,
        moved,
        limits,
    } = input;
    let Some(parent_blob_hash) = lookup_blob_at_path(source, &parent.tree, path)? else {
        return Ok(ParentClaim::MissingPath);
    };

    if parent_blob_hash == entry.blob_hash {
        let mut pairs = Vec::new();
        for map in &entry.mappings {
            for offset in 0..map.len {
                let state_index = map.state_start + offset;
                let idx = state_index as usize;
                if moved.get(idx).copied().unwrap_or(true) {
                    continue;
                }
                pairs.push((state_index, map.target_start + offset));
                moved[idx] = true;
            }
        }
        return Ok(ParentClaim::SameBlob {
            maps: compact_pairs(&pairs),
        });
    }

    let Some(parent_blob) = source.get_blob(&parent_blob_hash)? else {
        return Ok(ParentClaim::MissingPath);
    };
    budget.consume(
        ResourceKind::DecodedBytes,
        parent_blob.content().len() as u64,
    )?;
    let parent_bytes = parent_blob.content();
    let parent_lines = match std::str::from_utf8(parent_bytes) {
        Ok(text) => text.lines().count(),
        Err(_) => return Ok(ParentClaim::MissingPath),
    };
    budget.require(ResourceKind::Lines, parent_lines as u64)?;

    let needed = scratch_bytes_for_line_counts(
        parent_lines,
        std::str::from_utf8(entry_bytes)
            .map(|text| text.lines().count())
            .unwrap_or(0),
    );
    budget.require(ResourceKind::ScratchBytes, needed as u64)?;
    let scratch_len = needed.min(limits.scratch_bytes as usize);
    let mut scratch = vec![0u8; scratch_len];
    let mut pairs = Vec::new();
    visit_lcs_equal_runs(
        parent_bytes,
        entry_bytes,
        &mut scratch,
        LineDiffLimits {
            scratch_bytes: scratch_len as u64,
            max_lines: limits.lines,
            max_work: limits.diff_work,
        },
        |run| {
            for offset in 0..run.len {
                let parent_index = (run.old_start + offset) as u32;
                let entry_index = (run.new_start + offset) as u32;
                let idx = entry_index as usize;
                if moved.get(idx).copied().unwrap_or(true) {
                    continue;
                }
                let Some(target) = target_at(&entry.mappings, entry_index) else {
                    continue;
                };
                pairs.push((parent_index, target));
                moved[idx] = true;
            }
            Ok::<(), std::convert::Infallible>(())
        },
    )?;

    let line_count = u32::try_from(parent_lines).map_err(|_| BlameSliceError::Unblamable)?;
    Ok(ParentClaim::Aligned {
        maps: compact_pairs(&pairs),
        blob_hash: parent_blob_hash,
        line_count,
    })
}

pub(super) fn parent_record(
    parent: &State,
    blob_hash: ContentHash,
    line_count: u32,
    maps: Vec<BlameLineMap>,
) -> Option<BlameFrontierRecord> {
    if maps.is_empty() {
        return None;
    }
    Some(BlameFrontierRecord {
        origin: origin_from_state(parent),
        blob_hash,
        state_line_count: line_count,
        mappings: maps,
    })
}
