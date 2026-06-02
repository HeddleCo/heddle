// SPDX-License-Identifier: Apache-2.0
//! Import policy shared by git tree translators.

/// Policy knobs for mechanical git imports.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ImportOptions {
    /// Accept git tree entries Heddle cannot represent losslessly.
    ///
    /// When false, the importer fails at the first unrepresentable entry.
    /// When true, the importer restores the historical drop/convert behavior
    /// and records every affected entry in the import summary.
    pub lossy: bool,
}

/// One git tree entry that was dropped or converted because Heddle cannot
/// represent it losslessly yet.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LossyImportEntry {
    pub path: String,
    pub git_object: Option<String>,
    pub action: LossyImportAction,
    pub reason: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LossyImportAction {
    Dropped,
    Converted,
}

impl LossyImportAction {
    pub fn as_str(self) -> &'static str {
        match self {
            LossyImportAction::Dropped => "dropped",
            LossyImportAction::Converted => "converted",
        }
    }
}

impl LossyImportEntry {
    pub fn dropped(path: String, git_object: Option<String>, reason: impl Into<String>) -> Self {
        Self {
            path,
            git_object,
            action: LossyImportAction::Dropped,
            reason: reason.into(),
        }
    }

    pub fn converted(path: String, git_object: Option<String>, reason: impl Into<String>) -> Self {
        Self {
            path,
            git_object,
            action: LossyImportAction::Converted,
            reason: reason.into(),
        }
    }

    pub fn summary_line(&self) -> String {
        match &self.git_object {
            Some(object) => format!(
                "{} {} ({}): {}",
                self.action.as_str(),
                self.path,
                object,
                self.reason
            ),
            None => format!("{} {}: {}", self.action.as_str(), self.path, self.reason),
        }
    }
}

pub(crate) fn join_tree_path(prefix: &str, name: &str) -> String {
    let name = display_tree_name(name);
    if prefix.is_empty() {
        name
    } else {
        format!("{prefix}/{name}")
    }
}

pub(crate) fn display_tree_name(name: &str) -> String {
    if name.bytes().any(|b| b < 0x20 || b == 0x7f) {
        name.escape_debug().to_string()
    } else {
        name.to_string()
    }
}

pub(crate) fn rebase_lossy_entry(prefix: &str, entry: &LossyImportEntry) -> LossyImportEntry {
    let mut rebased = entry.clone();
    if !prefix.is_empty() {
        rebased.path = format!("{prefix}/{}", entry.path);
    }
    rebased
}

pub(crate) fn entry_relative_to_prefix(prefix: &str, entry: &LossyImportEntry) -> LossyImportEntry {
    if prefix.is_empty() {
        return entry.clone();
    }

    let mut relative = entry.clone();
    if let Some(stripped) = entry.path.strip_prefix(prefix) {
        relative.path = stripped.trim_start_matches('/').to_string();
    }
    relative
}
