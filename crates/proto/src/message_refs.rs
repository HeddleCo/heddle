// SPDX-License-Identifier: Apache-2.0
use objects::object::ChangeId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListRefs {
    #[serde(default)]
    pub repo_path: Option<String>,
    #[serde(default)]
    pub filter: Option<RefFilter>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefFilter {
    #[serde(default)]
    pub names: Vec<String>,
    #[serde(default)]
    pub patterns: Vec<String>,
    #[serde(default = "default_true")]
    pub include_threads: bool,
    #[serde(default = "default_true")]
    pub include_markers: bool,
    #[serde(default)]
    pub limit: Option<usize>,
}

fn default_true() -> bool {
    true
}

impl Default for RefFilter {
    fn default() -> Self {
        Self {
            names: Vec::new(),
            patterns: Vec::new(),
            include_threads: true,
            include_markers: true,
            limit: None,
        }
    }
}

impl RefFilter {
    pub fn matches(&self, name: &str) -> bool {
        if !self.names.is_empty() && self.names.iter().any(|candidate| candidate == name) {
            return true;
        }

        if self.patterns.is_empty() {
            return self.names.is_empty();
        }

        self.patterns
            .iter()
            .any(|pattern| Self::matches_pattern(name, pattern))
    }

    fn matches_pattern(name: &str, pattern: &str) -> bool {
        if pattern == "*" {
            return true;
        }
        if pattern.starts_with('*') && pattern.ends_with('*') && pattern.len() >= 2 {
            return name.contains(&pattern[1..pattern.len() - 1]);
        }
        if let Some(suffix) = pattern.strip_prefix('*') {
            return name.ends_with(suffix);
        }
        if let Some(prefix) = pattern.strip_suffix('*') {
            return name.starts_with(prefix);
        }
        name == pattern
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HeadInfo {
    Attached { thread: String },
    Detached { state: ChangeId },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefsList {
    pub head: HeadInfo,
    pub head_state: Option<ChangeId>,
    pub refs: Vec<RefEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefEntry {
    pub name: String,
    pub change_id: ChangeId,
    pub is_thread: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateRef {
    pub name: String,
    #[serde(default)]
    pub is_thread: bool,
    pub old_value: Option<ChangeId>,
    pub new_value: ChangeId,
    pub force: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefUpdated {
    pub success: bool,
    pub old_value: Option<ChangeId>,
    pub error: Option<String>,
}