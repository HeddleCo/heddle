// SPDX-License-Identifier: Apache-2.0
//! Shared semantic-change formatting data for CLI output.

use objects::object::SemanticChange;

pub(crate) struct SemanticChangeEntryFields {
    pub change_type: String,
    pub description: String,
    pub path: Option<String>,
    pub from_path: Option<String>,
    pub to_path: Option<String>,
    pub old_name: Option<String>,
    pub new_name: Option<String>,
    pub importance: Option<String>,
}

pub(crate) fn semantic_change_entry_fields(change: SemanticChange) -> SemanticChangeEntryFields {
    match change {
        SemanticChange::FileAdded { path } => SemanticChangeEntryFields {
            change_type: "file_added".to_string(),
            description: format!("File added: {}", path.display()),
            path: Some(path.display().to_string()),
            from_path: None,
            to_path: None,
            old_name: None,
            new_name: None,
            importance: None,
        },
        SemanticChange::FileDeleted { path } => SemanticChangeEntryFields {
            change_type: "file_deleted".to_string(),
            description: format!("File deleted: {}", path.display()),
            path: Some(path.display().to_string()),
            from_path: None,
            to_path: None,
            old_name: None,
            new_name: None,
            importance: None,
        },
        SemanticChange::FileModified {
            path,
            classification,
            importance,
            ..
        } => SemanticChangeEntryFields {
            change_type: if let Some(cls) = classification {
                format!("file_modified:{:?}", cls).to_lowercase()
            } else {
                "file_modified".to_string()
            },
            description: if let Some(cls) = classification {
                format!("File modified ({:?}): {}", cls, path.display())
            } else {
                format!("File modified: {}", path.display())
            },
            path: Some(path.display().to_string()),
            from_path: None,
            to_path: None,
            old_name: None,
            new_name: None,
            importance: importance.map(|i| format!("{i:?}").to_lowercase()),
        },
        SemanticChange::FunctionDeleted {
            file,
            name,
            importance,
        } => SemanticChangeEntryFields {
            change_type: "function_deleted".to_string(),
            description: format!("Function deleted: {} in {}", name, file.display()),
            path: Some(file.display().to_string()),
            from_path: None,
            to_path: None,
            old_name: Some(name),
            new_name: None,
            importance: importance.map(|i| format!("{i:?}").to_lowercase()),
        },
        SemanticChange::SignatureChanged {
            file,
            name,
            old_signature,
            new_signature,
            importance,
        } => SemanticChangeEntryFields {
            change_type: "signature_changed".to_string(),
            description: format!("Signature changed: {} in {}", name, file.display()),
            path: Some(file.display().to_string()),
            from_path: None,
            to_path: None,
            old_name: Some(old_signature),
            new_name: Some(new_signature),
            importance: importance.map(|i| format!("{i:?}").to_lowercase()),
        },
        SemanticChange::FileRenamed { from, to } => SemanticChangeEntryFields {
            change_type: "file_renamed".to_string(),
            description: format!("File renamed: {} -> {}", from.display(), to.display()),
            path: None,
            from_path: Some(from.display().to_string()),
            to_path: Some(to.display().to_string()),
            old_name: None,
            new_name: None,
            importance: None,
        },
        SemanticChange::FunctionAdded {
            file,
            name,
            importance,
        } => SemanticChangeEntryFields {
            change_type: "function_added".to_string(),
            description: format!("Function added: {} in {}", name, file.display()),
            path: Some(file.display().to_string()),
            from_path: None,
            to_path: None,
            old_name: None,
            new_name: Some(name),
            importance: importance.map(|i| format!("{i:?}").to_lowercase()),
        },
        SemanticChange::FunctionExtracted {
            file,
            name,
            source_file,
            source_name,
            importance,
        } => SemanticChangeEntryFields {
            change_type: "function_extracted".to_string(),
            description: if let Some(source_name) = &source_name {
                let source_file = source_file.as_ref().unwrap_or(&file);
                format!(
                    "Function extracted: {} from {} in {}",
                    name,
                    source_name,
                    source_file.display()
                )
            } else {
                format!("Function extracted: {} in {}", name, file.display())
            },
            path: Some(file.display().to_string()),
            from_path: source_file.map(|path| path.display().to_string()),
            to_path: None,
            old_name: source_name,
            new_name: Some(name),
            importance: importance.map(|i| format!("{i:?}").to_lowercase()),
        },
        SemanticChange::FunctionRenamed {
            file,
            old_name,
            new_name,
            importance,
        } => SemanticChangeEntryFields {
            change_type: "function_renamed".to_string(),
            description: format!(
                "Function renamed: {} -> {} in {}",
                old_name,
                new_name,
                file.display()
            ),
            path: Some(file.display().to_string()),
            from_path: None,
            to_path: None,
            old_name: Some(old_name),
            new_name: Some(new_name),
            importance: importance.map(|i| format!("{i:?}").to_lowercase()),
        },
        SemanticChange::FunctionModified {
            file,
            name,
            importance,
        } => SemanticChangeEntryFields {
            change_type: "function_modified".to_string(),
            description: format!("Function modified: {} in {}", name, file.display()),
            path: Some(file.display().to_string()),
            from_path: None,
            to_path: None,
            old_name: Some(name),
            new_name: None,
            importance: importance.map(|i| format!("{i:?}").to_lowercase()),
        },
        SemanticChange::FunctionMoved {
            file,
            name,
            old_start_line,
            new_start_line,
            importance,
        } => SemanticChangeEntryFields {
            change_type: "function_moved".to_string(),
            description: format!(
                "Function moved: {} in {} ({} -> {})",
                name,
                file.display(),
                old_start_line + 1,
                new_start_line + 1
            ),
            path: Some(file.display().to_string()),
            from_path: None,
            to_path: None,
            old_name: Some(name),
            new_name: None,
            importance: importance.map(|i| format!("{i:?}").to_lowercase()),
        },
        SemanticChange::DependencyAdded { name, version } => SemanticChangeEntryFields {
            change_type: "dependency_added".to_string(),
            description: format!("Dependency added: {}@{}", name, version),
            path: None,
            from_path: None,
            to_path: None,
            old_name: None,
            new_name: Some(name),
            importance: None,
        },
        SemanticChange::DependencyRemoved { name } => SemanticChangeEntryFields {
            change_type: "dependency_removed".to_string(),
            description: format!("Dependency removed: {}", name),
            path: None,
            from_path: None,
            to_path: None,
            old_name: Some(name),
            new_name: None,
            importance: None,
        },
        SemanticChange::Custom { change_type, .. } => SemanticChangeEntryFields {
            change_type: format!("custom:{}", change_type),
            description: format!("Custom change: {}", change_type),
            path: None,
            from_path: None,
            to_path: None,
            old_name: None,
            new_name: None,
            importance: None,
        },
    }
}
