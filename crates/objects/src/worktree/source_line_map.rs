// SPDX-License-Identifier: Apache-2.0
//! Shared capture position maps using the existing Histogram diff engine.

use crate::object::{
    Blob,
    source_target::{SourceLineEdit, SourceLineEditMap, SourceTargetError},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceLineMapBuild {
    Ready(SourceLineEditMap),
    UnsupportedEncoding,
    BudgetExceeded,
}

/// Build once for an old/new blob pair, regardless of how many ranges refer to
/// that file. Limits apply before parsing and before admitting the edit map.
/// No elapsed-time fallback changes the resulting coordinate decisions.
pub fn source_line_edit_map(
    old: &Blob,
    new: &Blob,
    max_bytes: usize,
    max_edits: usize,
) -> Result<SourceLineMapBuild, SourceTargetError> {
    if old.content().len().saturating_add(new.content().len()) > max_bytes {
        return Ok(SourceLineMapBuild::BudgetExceeded);
    }
    let (Some(old_text), Some(new_text)) = (old.content_str(), new.content_str()) else {
        return Ok(SourceLineMapBuild::UnsupportedEncoding);
    };
    let diff = similar::TextDiff::configure()
        .algorithm(similar::Algorithm::Histogram)
        .diff_lines(old_text, new_text);
    let mut edits: Vec<SourceLineEdit> = Vec::new();
    for operation in diff.ops() {
        if operation.tag() == similar::DiffTag::Equal {
            continue;
        }
        let old_range = operation.old_range();
        let new_range = operation.new_range();
        let edit = SourceLineEdit {
            old_start: line_count(old_range.start)?,
            old_end: line_count(old_range.end)?,
            new_start: line_count(new_range.start)?,
            new_end: line_count(new_range.end)?,
        };
        if let Some(previous) = edits.last_mut()
            && previous.old_end == edit.old_start
            && previous.new_end == edit.new_start
        {
            previous.old_end = edit.old_end;
            previous.new_end = edit.new_end;
        } else {
            if edits.len() >= max_edits.min(65_536) {
                return Ok(SourceLineMapBuild::BudgetExceeded);
            }
            edits.push(edit);
        }
    }
    Ok(SourceLineMapBuild::Ready(SourceLineEditMap::new(
        line_count(diff.old_len())?,
        line_count(diff.new_len())?,
        edits,
    )?))
}

fn line_count(count: usize) -> Result<u32, SourceTargetError> {
    u32::try_from(count)
        .map_err(|_| SourceTargetError::Invalid("source exceeds line coordinate range".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::source_target::{SourceAffinity, SourceLineRange, SourceRangeProjection};

    #[test]
    fn actual_text_diff_tracks_insertions_and_preserves_original_line_content() {
        let old = Blob::new(b"head\nselected one\nselected two\ntail\n".to_vec());
        let new = Blob::new(b"new head\nhead\nselected one\nselected two\ntail\n".to_vec());
        let SourceLineMapBuild::Ready(map) =
            source_line_edit_map(&old, &new, 1024, 10).expect("map")
        else {
            panic!("small UTF-8 sources must have a map");
        };
        assert_eq!(map.edits().len(), 1);
        let range = SourceLineRange {
            start: 1,
            end: 3,
            start_affinity: SourceAffinity::After,
            end_affinity: SourceAffinity::Before,
        };
        assert_eq!(
            map.project(range).expect("project"),
            SourceRangeProjection::Resolved {
                range: SourceLineRange {
                    start: 2,
                    end: 4,
                    ..range
                },
                changed: false,
            }
        );
        let old_lines = old.content_str().expect("text").lines().collect::<Vec<_>>();
        let new_lines = new.content_str().expect("text").lines().collect::<Vec<_>>();
        assert_eq!(&old_lines[1..3], &new_lines[2..4]);
    }

    #[test]
    fn source_limits_and_binary_content_are_explicit() {
        let old = Blob::new(b"before\n".to_vec());
        let new = Blob::new(b"after\n".to_vec());
        assert_eq!(
            source_line_edit_map(&old, &new, 1, 10).expect("budget"),
            SourceLineMapBuild::BudgetExceeded
        );
        assert_eq!(
            source_line_edit_map(&old, &new, 1024, 0).expect("edit budget"),
            SourceLineMapBuild::BudgetExceeded
        );
        assert_eq!(
            source_line_edit_map(&old, &Blob::new(vec![0xff]), 1024, 10).expect("binary"),
            SourceLineMapBuild::UnsupportedEncoding
        );
        let empty = Blob::new(Vec::new());
        let SourceLineMapBuild::Ready(map) =
            source_line_edit_map(&empty, &old, 1024, 10).expect("empty source")
        else {
            panic!("new text is supported");
        };
        assert_eq!(
            map.edits(),
            &[SourceLineEdit {
                old_start: 0,
                old_end: 0,
                new_start: 0,
                new_end: 1
            }]
        );
    }
}
