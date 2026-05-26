// SPDX-License-Identifier: Apache-2.0
//! Types used by diff command output.

use objects::object::SemanticChange;
use serde::Serialize;

use crate::cli::commands::semantic_change_output::{
    SemanticChangeEntryFields, semantic_change_entry_fields,
};

#[derive(Clone, Debug, Serialize)]
pub struct DiffOutput {
    pub output_kind: &'static str,
    pub status: &'static str,
    pub from_state: Option<String>,
    pub to_state: Option<String>,
    pub changed_path_count: usize,
    pub stats: DiffStats,
    pub changes: Vec<FileChange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub semantic_changes: Option<Vec<SemanticChangeEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<Vec<FileContextEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub broader_guidance: Option<Vec<ContextSnippet>>,
}

impl DiffOutput {
    pub fn new(
        from_state: Option<String>,
        to_state: Option<String>,
        changes: Vec<FileChange>,
        semantic_changes: Option<Vec<SemanticChangeEntry>>,
        context: Option<Vec<FileContextEntry>>,
        broader_guidance: Option<Vec<ContextSnippet>>,
    ) -> Self {
        let stats = DiffStats::from_changes(&changes, semantic_changes.as_deref());
        Self::with_stats(
            from_state,
            to_state,
            changes,
            semantic_changes,
            context,
            broader_guidance,
            stats,
        )
    }

    pub fn with_stats(
        from_state: Option<String>,
        to_state: Option<String>,
        changes: Vec<FileChange>,
        semantic_changes: Option<Vec<SemanticChangeEntry>>,
        context: Option<Vec<FileContextEntry>>,
        broader_guidance: Option<Vec<ContextSnippet>>,
        stats: DiffStats,
    ) -> Self {
        Self {
            output_kind: "diff",
            status: "completed",
            changed_path_count: changes.len(),
            from_state,
            to_state,
            stats,
            changes,
            semantic_changes,
            context,
            broader_guidance,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct DiffStats {
    pub files_changed: usize,
    pub additions: usize,
    pub modifications: usize,
    pub deletions: usize,
    pub renames: usize,
}

impl DiffStats {
    pub(crate) fn from_changes(
        changes: &[FileChange],
        semantic_changes: Option<&[SemanticChangeEntry]>,
    ) -> Self {
        let mut stats = Self {
            files_changed: changes.len(),
            ..Self::default()
        };
        for change in changes {
            let line_counts = change_line_counts(change.lines.as_deref());
            stats.additions += line_counts.added;
            stats.modifications += line_counts.modified;
            stats.deletions += line_counts.deleted;

            match change.kind.as_str() {
                "added" if change.lines.is_none() => stats.additions += 1,
                "modified" if change.lines.is_none() => stats.modifications += 1,
                "deleted" if change.lines.is_none() => stats.deletions += 1,
                "renamed" => stats.renames += 1,
                _ => {}
            }
        }
        if let Some(semantic) = semantic_changes {
            stats.renames += semantic
                .iter()
                .filter(|change| change.change_type == "file_renamed")
                .count();
        }
        stats
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct FileChange {
    pub path: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    #[serde(skip)]
    pub binary: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines: Option<Vec<LineDiff>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct LineDiff {
    pub prefix: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_line: Option<usize>,
}

impl LineDiff {
    pub fn new(prefix: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
            content: content.into(),
            old_line: None,
            new_line: None,
        }
    }

    pub fn with_lines(
        prefix: impl Into<String>,
        content: impl Into<String>,
        old_line: Option<usize>,
        new_line: Option<usize>,
    ) -> Self {
        Self {
            prefix: prefix.into(),
            content: content.into(),
            old_line,
            new_line,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct FileContextEntry {
    pub path: String,
    pub annotations: Vec<ContextSnippet>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ContextSnippet {
    pub annotation_id: String,
    pub kind: String,
    pub content: String,
    pub revision_count: usize,
}

#[derive(Default)]
pub(crate) struct LineCounts {
    pub added: usize,
    pub modified: usize,
    pub deleted: usize,
}

pub(crate) fn change_line_counts(lines: Option<&[LineDiff]>) -> LineCounts {
    let mut counts = LineCounts::default();
    let mut index = 0usize;
    let lines = lines.unwrap_or_default();
    while index < lines.len() {
        let line = &lines[index];
        if line.prefix == "-"
            && let Some(next) = lines.get(index + 1)
            && next.prefix == "+"
            && should_render_modified_pair(&line.content, &next.content)
        {
            counts.modified += 1;
            index += 2;
            continue;
        }
        match line.prefix.as_str() {
            "+" => counts.added += 1,
            "-" => counts.deleted += 1,
            _ => {}
        }
        index += 1;
    }
    counts
}

#[derive(Clone, Debug, Serialize)]
pub struct SemanticChangeEntry {
    pub change_type: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub importance: Option<String>,
}

impl From<SemanticChange> for SemanticChangeEntry {
    fn from(change: SemanticChange) -> Self {
        semantic_change_entry_fields(change).into()
    }
}

impl From<SemanticChangeEntryFields> for SemanticChangeEntry {
    fn from(fields: SemanticChangeEntryFields) -> Self {
        Self {
            change_type: fields.change_type,
            description: fields.description,
            path: fields.path,
            from_path: fields.from_path,
            to_path: fields.to_path,
            old_name: fields.old_name,
            new_name: fields.new_name,
            importance: fields.importance,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use objects::object::{ChangeImportance, SemanticChange};
    use serde_json::Value;

    use super::SemanticChangeEntry;

    #[test]
    fn semantic_change_json_uses_importance_field_not_old_name() {
        let entry = SemanticChangeEntry::from(SemanticChange::FileModified {
            path: PathBuf::from("src/lib.rs"),
            classification: None,
            importance: Some(ChangeImportance::Medium),
            confidence: None,
        });
        let json = serde_json::to_value(entry).expect("semantic entry serializes");

        assert_eq!(json["importance"], "medium");
        assert!(json.get("old_name").is_none(), "{json}");
    }

    #[test]
    fn semantic_rename_json_uses_path_fields() {
        let entry = SemanticChangeEntry::from(SemanticChange::FileRenamed {
            from: PathBuf::from("src/old.rs"),
            to: PathBuf::from("src/new.rs"),
        });
        let json = serde_json::to_value(entry).expect("semantic rename serializes");

        assert_eq!(json["change_type"], "file_renamed");
        assert_eq!(json["from_path"], "src/old.rs");
        assert_eq!(json["to_path"], "src/new.rs");
        assert!(json.get("old_name").is_none(), "{json}");
        assert!(matches!(json["from_path"], Value::String(_)));
        assert!(matches!(json["to_path"], Value::String(_)));
    }
}

pub(crate) fn should_render_modified_pair(removed: &str, added: &str) -> bool {
    let prefix_len = common_prefix_boundary(removed, added);
    let suffix_len = common_suffix_boundary(&removed[prefix_len..], &added[prefix_len..]);
    let shared_len = prefix_len + suffix_len;
    let max_len = removed.len().max(added.len());

    // The `~` row is a review affordance for one logical line edit.
    // If two adjacent delete/add lines barely overlap, keeping the
    // normal two-line patch shape is clearer and avoids visually
    // gluing unrelated code together.
    shared_len >= 4 && shared_len * 3 >= max_len
}

fn common_prefix_boundary(left: &str, right: &str) -> usize {
    let mut boundary = 0;
    for ((left_index, left_char), (_, right_char)) in left.char_indices().zip(right.char_indices())
    {
        if left_char != right_char {
            break;
        }
        boundary = left_index + left_char.len_utf8();
    }
    boundary
}

fn common_suffix_boundary(left_tail: &str, right_tail: &str) -> usize {
    let mut boundary = 0;
    for ((left_index, left_char), (_, right_char)) in left_tail
        .char_indices()
        .rev()
        .zip(right_tail.char_indices().rev())
    {
        if left_char != right_char {
            break;
        }
        boundary = left_tail.len() - left_index;
    }
    boundary
}
