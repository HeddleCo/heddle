// SPDX-License-Identifier: Apache-2.0
//! Semantic change descriptions.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// What kind of modification was made to a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModificationKind {
    /// Real logic/behaviour change.
    Logic,
    /// Only whitespace or indentation changed.
    WhitespaceOnly,
    /// Only import/use statements changed.
    ImportsOnly,
    /// Only comments changed.
    CommentsOnly,
    /// Formatting pass (tokens identical, layout differs).
    FormattingOnly,
    /// Mix of logic and non-logic changes.
    Mixed,
}

/// How important is this change for review.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ChangeImportance {
    /// Reviewer can safely skip.
    Noise,
    /// Low priority — imports, comments, renames.
    Low,
    /// Medium priority — mixed changes, signature tweaks.
    Medium,
    /// High priority — logic changes, new/deleted functions.
    High,
}

/// A semantic change description.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SemanticChange {
    /// A new file was added.
    FileAdded { path: PathBuf },
    /// A file was deleted.
    FileDeleted { path: PathBuf },
    /// A file was modified.
    FileModified {
        path: PathBuf,
        #[serde(default)]
        classification: Option<ModificationKind>,
        #[serde(default)]
        importance: Option<ChangeImportance>,
        /// Confidence in the classification (0.0–1.0). AST-backed = high, token-fallback = lower.
        #[serde(default)]
        confidence: Option<f64>,
    },
    /// A file was renamed.
    FileRenamed { from: PathBuf, to: PathBuf },
    /// A function was added without enough evidence to call it an extraction.
    FunctionAdded {
        file: PathBuf,
        name: String,
        #[serde(default)]
        importance: Option<ChangeImportance>,
    },
    /// A function was extracted from an existing function.
    FunctionExtracted {
        file: PathBuf,
        name: String,
        #[serde(default)]
        source_file: Option<PathBuf>,
        #[serde(default)]
        source_name: Option<String>,
        #[serde(default)]
        importance: Option<ChangeImportance>,
    },
    /// A function was deleted.
    FunctionDeleted {
        file: PathBuf,
        name: String,
        #[serde(default)]
        importance: Option<ChangeImportance>,
    },
    /// A function was renamed.
    FunctionRenamed {
        file: PathBuf,
        old_name: String,
        new_name: String,
        #[serde(default)]
        importance: Option<ChangeImportance>,
    },
    /// A function body changed without a signature/name change.
    FunctionModified {
        file: PathBuf,
        name: String,
        #[serde(default)]
        importance: Option<ChangeImportance>,
    },
    /// A function moved within the same file without changing its body.
    FunctionMoved {
        file: PathBuf,
        name: String,
        old_start_line: usize,
        new_start_line: usize,
        #[serde(default)]
        importance: Option<ChangeImportance>,
    },
    /// A function signature changed.
    SignatureChanged {
        file: PathBuf,
        name: String,
        old_signature: String,
        new_signature: String,
        #[serde(default)]
        importance: Option<ChangeImportance>,
    },
    /// A dependency was added.
    DependencyAdded { name: String, version: String },
    /// A dependency was removed.
    DependencyRemoved { name: String },
    /// Custom semantic change.
    Custom {
        change_type: String,
        data: serde_json::Value,
    },
}

impl SemanticChange {
    /// Get a short description.
    pub fn description(&self) -> String {
        match self {
            SemanticChange::FileAdded { path } => format!("add {}", path.display()),
            SemanticChange::FileDeleted { path } => format!("delete {}", path.display()),
            SemanticChange::FileModified {
                path,
                classification,
                ..
            } => {
                if let Some(kind) = classification {
                    format!("modify {} ({:?})", path.display(), kind)
                } else {
                    format!("modify {}", path.display())
                }
            }
            SemanticChange::FileRenamed { from, to } => {
                format!("rename {} -> {}", from.display(), to.display())
            }
            SemanticChange::FunctionAdded { file, name, .. } => {
                format!("add function {} in {}", name, file.display())
            }
            SemanticChange::FunctionExtracted {
                file,
                name,
                source_file,
                source_name,
                ..
            } => {
                if let Some(source_name) = source_name {
                    let source_file = source_file.as_ref().unwrap_or(file);
                    format!(
                        "extract function {} from {} in {}",
                        name,
                        source_name,
                        source_file.display()
                    )
                } else {
                    format!("extract function {} in {}", name, file.display())
                }
            }
            SemanticChange::FunctionDeleted { file, name, .. } => {
                format!("delete function {} in {}", name, file.display())
            }
            SemanticChange::FunctionRenamed {
                file,
                old_name,
                new_name,
                ..
            } => {
                format!("rename {} -> {} in {}", old_name, new_name, file.display())
            }
            SemanticChange::FunctionModified { file, name, .. } => {
                format!("modify function {} in {}", name, file.display())
            }
            SemanticChange::FunctionMoved {
                file,
                name,
                old_start_line,
                new_start_line,
                ..
            } => {
                format!(
                    "move function {} in {} ({} -> {})",
                    name,
                    file.display(),
                    old_start_line + 1,
                    new_start_line + 1
                )
            }
            SemanticChange::SignatureChanged { file, name, .. } => {
                format!("change signature of {} in {}", name, file.display())
            }
            SemanticChange::DependencyAdded { name, version } => {
                format!("add dependency {}@{}", name, version)
            }
            SemanticChange::DependencyRemoved { name } => {
                format!("remove dependency {}", name)
            }
            SemanticChange::Custom { change_type, .. } => {
                format!("custom: {}", change_type)
            }
        }
    }
}