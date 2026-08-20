// SPDX-License-Identifier: Apache-2.0
//! First-parent-wins merge policy against one parent of the current frontier.

use std::path::Path;

use crate::{
    object::{ContentHash, ObjectSource, State},
    util::{scratch_bytes_for_line_counts, visit_lcs_equal_runs, ResourceBudget, ResourceKind},
};

use super::{
    lookup::{load_blob_within_budget, lookup_blob_at_path},
    mapping::{claim_equal_run, claim_same_blob_maps},
    types::{origin_from_state, BlameFrontierRecord, BlameLineMap, BlameSliceError},
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
    } = input;
    let Some(parent_blob_hash) = lookup_blob_at_path(source, &parent.tree, path)? else {
        return Ok(ParentClaim::MissingPath);
    };

    if parent_blob_hash == entry.blob_hash {
        return Ok(ParentClaim::SameBlob {
            maps: claim_same_blob_maps(&entry.mappings, moved),
        });
    }

    let parent_blob = load_blob_within_budget(source, &parent_blob_hash, budget)?;
    let parent_bytes = parent_blob.content();
    let parent_lines = match std::str::from_utf8(parent_bytes) {
        Ok(text) => text.lines().count(),
        Err(_) => return Ok(ParentClaim::MissingPath),
    };
    budget.consume(ResourceKind::Lines, parent_lines as u64)?;

    let entry_lines = std::str::from_utf8(entry_bytes)
        .map(|text| text.lines().count())
        .unwrap_or(0);
    let needed = scratch_bytes_for_line_counts(parent_lines, entry_lines);
    budget.require(ResourceKind::ScratchBytes, needed as u64)?;
    let mut scratch = vec![0u8; needed];
    let mut maps = Vec::new();
    visit_lcs_equal_runs(parent_bytes, entry_bytes, &mut scratch, budget, |run| {
        claim_equal_run(run, entry, moved, &mut maps);
        Ok::<(), std::convert::Infallible>(())
    })?;

    let line_count = u32::try_from(parent_lines).map_err(|_| BlameSliceError::Unblamable)?;
    Ok(ParentClaim::Aligned {
        maps,
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
