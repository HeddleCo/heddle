// SPDX-License-Identifier: Apache-2.0
//! Types used by compare output.

use objects::object::SemanticChange;
use serde::Serialize;

use crate::cli::commands::semantic_change_output::{
    SemanticChangeEntryFields, semantic_change_entry_fields,
};

#[derive(Serialize)]
pub struct CompareOutput {
    pub state_a: String,
    pub state_b: String,
    pub changes: Vec<FileChange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub semantic_changes: Option<Vec<SemanticChangeEntry>>,
    pub summary: CompareSummary,
}

#[derive(Serialize)]
pub struct FileChange {
    pub path: String,
    pub kind: String,
}

#[derive(Serialize)]
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

#[derive(Serialize)]
pub struct CompareSummary {
    pub added: usize,
    pub modified: usize,
    pub deleted: usize,
    pub renamed: usize,
    pub total: usize,
}
