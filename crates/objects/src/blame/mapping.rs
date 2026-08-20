// SPDX-License-Identifier: Apache-2.0
//! Compact line maps and finalized origin ranges.

use crate::object::Origin;
use crate::util::EqualRun;

use super::types::{BlameFrontierRecord, BlameLineMap, BlameSliceError, OriginRange};

pub(super) fn identity_mapping(line_count: u32) -> Vec<BlameLineMap> {
    if line_count == 0 {
        return Vec::new();
    }
    vec![BlameLineMap {
        state_start: 0,
        target_start: 0,
        len: line_count,
    }]
}

pub(super) fn target_at(maps: &[BlameLineMap], state_index: u32) -> Option<u32> {
    maps.iter().find_map(|map| {
        let end = map.state_start.saturating_add(map.len);
        if state_index >= map.state_start && state_index < end {
            Some(map.target_start + (state_index - map.state_start))
        } else {
            None
        }
    })
}

pub(super) fn append_map_run(
    maps: &mut Vec<BlameLineMap>,
    state_start: u32,
    target_start: u32,
    len: u32,
) {
    if len == 0 {
        return;
    }
    match maps.last_mut() {
        Some(last)
            if last.state_start + last.len == state_start
                && last.target_start + last.len == target_start =>
        {
            last.len += len;
        }
        _ => maps.push(BlameLineMap {
            state_start,
            target_start,
            len,
        }),
    }
}

pub(super) fn claim_same_blob_maps(maps: &[BlameLineMap], moved: &mut [bool]) -> Vec<BlameLineMap> {
    let mut claimed = Vec::new();
    for map in maps {
        let mut offset = 0u32;
        while offset < map.len {
            let idx = (map.state_start + offset) as usize;
            if moved.get(idx).copied().unwrap_or(true) {
                offset += 1;
                continue;
            }
            let start = offset;
            offset += 1;
            while offset < map.len {
                let next = (map.state_start + offset) as usize;
                if moved.get(next).copied().unwrap_or(true) {
                    break;
                }
                offset += 1;
            }
            let len = offset - start;
            for step in 0..len {
                moved[(map.state_start + start + step) as usize] = true;
            }
            append_map_run(
                &mut claimed,
                map.state_start + start,
                map.target_start + start,
                len,
            );
        }
    }
    claimed
}

pub(super) fn claim_equal_run(
    run: EqualRun,
    entry: &BlameFrontierRecord,
    moved: &mut [bool],
    maps: &mut Vec<BlameLineMap>,
) {
    let mut offset = 0usize;
    while offset < run.len {
        let entry_index = (run.new_start + offset) as u32;
        let parent_index = (run.old_start + offset) as u32;
        let idx = entry_index as usize;
        if moved.get(idx).copied().unwrap_or(true) {
            offset += 1;
            continue;
        }
        let Some(target) = target_at(&entry.mappings, entry_index) else {
            offset += 1;
            continue;
        };
        let start = offset;
        offset += 1;
        while offset < run.len {
            let next_entry = (run.new_start + offset) as u32;
            let next_parent = (run.old_start + offset) as u32;
            let next_idx = next_entry as usize;
            if moved.get(next_idx).copied().unwrap_or(true) {
                break;
            }
            let Some(next_target) = target_at(&entry.mappings, next_entry) else {
                break;
            };
            if next_parent != parent_index + (offset - start) as u32
                || next_target != target + (offset - start) as u32
            {
                break;
            }
            offset += 1;
        }
        let len = (offset - start) as u32;
        for step in 0..len {
            moved[(entry_index + step) as usize] = true;
        }
        append_map_run(maps, parent_index, target, len);
    }
}

pub(super) fn finalize_unmoved(
    maps: &[BlameLineMap],
    moved: &[bool],
    origin: &Origin,
) -> Vec<OriginRange> {
    let mut targets = Vec::new();
    for map in maps {
        for offset in 0..map.len {
            let state_index = map.state_start + offset;
            if moved.get(state_index as usize).copied().unwrap_or(true) {
                continue;
            }
            targets.push(map.target_start + offset);
        }
    }
    targets.sort_unstable();
    coalesce_targets(&targets, origin)
}

pub(super) fn mappings_fit_state_lines(
    mappings: &[BlameLineMap],
    state_line_count: u32,
    target_line_count: u32,
) -> Result<(), BlameSliceError> {
    if mappings.is_empty() {
        if state_line_count == 0 {
            return Ok(());
        }
        return Err(BlameSliceError::InvalidFrontier(
            "nonempty state has no mappings".into(),
        ));
    }
    let mut prev_state_end = 0u32;
    let mut prev_target_end = 0u32;
    for mapping in mappings {
        if mapping.len == 0 {
            return Err(BlameSliceError::InvalidFrontier(
                "zero-length mapping".into(),
            ));
        }
        let state_end = mapping
            .state_start
            .checked_add(mapping.len)
            .ok_or_else(|| BlameSliceError::InvalidFrontier("mapping state end overflow".into()))?;
        let target_end = mapping
            .target_start
            .checked_add(mapping.len)
            .ok_or_else(|| {
                BlameSliceError::InvalidFrontier("mapping target end overflow".into())
            })?;
        if mapping.state_start < prev_state_end {
            return Err(BlameSliceError::InvalidFrontier(
                "overlapping or unordered state mapping".into(),
            ));
        }
        if mapping.target_start < prev_target_end {
            return Err(BlameSliceError::InvalidFrontier(
                "overlapping or unordered target mapping".into(),
            ));
        }
        if state_end > state_line_count {
            return Err(BlameSliceError::InvalidFrontier(format!(
                "mapping end {state_end} exceeds state_line_count {state_line_count}"
            )));
        }
        if target_end > target_line_count {
            return Err(BlameSliceError::InvalidFrontier(format!(
                "mapping target end {target_end} exceeds target_line_count {target_line_count}"
            )));
        }
        prev_state_end = state_end;
        prev_target_end = target_end;
    }
    Ok(())
}

/// True when every state line covered by `maps` is already claimed.
pub(super) fn mapped_lines_claimed(maps: &[BlameLineMap], moved: &[bool]) -> bool {
    maps.iter().all(|map| {
        (0..map.len).all(|offset| {
            moved
                .get((map.state_start + offset) as usize)
                .copied()
                .unwrap_or(true)
        })
    })
}

pub(super) fn blob_line_count_matches_frontier(
    blob_bytes: &[u8],
    state_line_count: u32,
) -> Result<usize, BlameSliceError> {
    let blob_line_count = std::str::from_utf8(blob_bytes)
        .map_err(|_| BlameSliceError::Unblamable)?
        .lines()
        .count();
    if blob_line_count != state_line_count as usize {
        return Err(BlameSliceError::InvalidFrontier(format!(
            "state_line_count {state_line_count} != blob lines {blob_line_count}"
        )));
    }
    Ok(blob_line_count)
}

fn coalesce_targets(targets: &[u32], origin: &Origin) -> Vec<OriginRange> {
    let mut ranges = Vec::new();
    let mut index = 0;
    while index < targets.len() {
        let start = targets[index];
        let mut len = 1u32;
        index += 1;
        while index < targets.len() && targets[index] == start + len {
            len += 1;
            index += 1;
        }
        ranges.push(OriginRange {
            target_start: start,
            len,
            origin: origin.clone(),
        });
    }
    ranges
}
