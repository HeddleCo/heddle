// SPDX-License-Identifier: Apache-2.0
//! Compact line maps and finalized origin ranges.

use crate::object::Origin;

use super::types::{BlameLineMap, OriginRange};

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

pub(super) fn compact_pairs(pairs: &[(u32, u32)]) -> Vec<BlameLineMap> {
    let mut maps: Vec<BlameLineMap> = Vec::new();
    for &(state_index, target_index) in pairs {
        match maps.last_mut() {
            Some(last)
                if last.state_start + last.len == state_index
                    && last.target_start + last.len == target_index =>
            {
                last.len += 1;
            }
            _ => maps.push(BlameLineMap {
                state_start: state_index,
                target_start: target_index,
                len: 1,
            }),
        }
    }
    maps
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
