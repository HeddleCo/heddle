// SPDX-License-Identifier: Apache-2.0
//! Types used by diff command output.

use objects::object::SemanticChange;
use serde::Serialize;

use crate::cli::commands::semantic_change_output::{
    SemanticChangeEntryFields, semantic_change_entry_fields,
};

#[derive(Clone, Debug, Serialize)]
pub struct DiffOutput {
    pub from_state: Option<String>,
    pub to_state: Option<String>,
    pub changes: Vec<FileChange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub semantic_changes: Option<Vec<SemanticChangeEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<Vec<FileContextEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub broader_guidance: Option<Vec<ContextSnippet>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct FileChange {
    pub path: String,
    pub kind: String,
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
        }
    }
}
