// SPDX-License-Identifier: Apache-2.0
use objects::object::StateId;
use serde::{Deserialize, Serialize};

mod kind;

pub use kind::{AdvertisedRef, AdvertisedRefError, RefKind};

/// Filter applied when listing repository refs.
///
/// Retained as part of the published `heddle-wire` 0.11 surface while Weft
/// migrates to the generated hosted API types.
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
    /// Synthetic frontier roots are an internal embargo mechanism and stay
    /// hidden unless a caller opts in.
    #[serde(default)]
    pub include_synthetic: bool,
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
            include_synthetic: false,
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

    pub fn includes_kind(&self, kind: RefKind) -> bool {
        match kind {
            RefKind::Thread => self.include_threads,
            RefKind::Marker => self.include_markers,
            RefKind::SyntheticFrontierRoot => self.include_synthetic,
        }
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

/// Repository HEAD shape returned alongside a ref listing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HeadInfo {
    Attached { thread: String },
    Detached { state: StateId },
}

/// Published repository-ref listing response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefsList {
    pub head: HeadInfo,
    pub head_state: Option<StateId>,
    pub refs: Vec<RefEntry>,
}

/// One advertised repository ref.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefEntry {
    pub name: String,
    pub state_id: StateId,
    pub kind: RefKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefUpdated {
    pub success: bool,
    pub old_value: Option<StateId>,
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_filter_matches_union_of_names_and_patterns() {
        let filter = RefFilter {
            names: vec!["refs/heads/main".to_string()],
            patterns: vec!["refs/tags/v*".to_string()],
            ..RefFilter::default()
        };

        assert!(filter.matches("refs/heads/main"));
        assert!(filter.matches("refs/tags/v1.0.0"));
        assert!(!filter.matches("refs/heads/feature"));
        assert!(!filter.matches("refs/tags/nightly"));
    }

    #[test]
    fn ref_filter_without_names_or_patterns_matches_everything() {
        let filter = RefFilter::default();

        assert!(filter.matches("refs/heads/main"));
        assert!(filter.matches("refs/tags/v1.0.0"));
        assert!(filter.matches("threads/alice"));
        assert!(!filter.includes_kind(RefKind::SyntheticFrontierRoot));
        assert!(filter.includes_kind(RefKind::Thread));
        assert!(filter.includes_kind(RefKind::Marker));
    }

    #[test]
    fn ref_filter_exact_names_do_not_expand_without_patterns() {
        let filter = RefFilter {
            names: vec!["refs/heads/main".to_string()],
            patterns: Vec::new(),
            ..RefFilter::default()
        };

        assert!(filter.matches("refs/heads/main"));
        assert!(!filter.matches("refs/heads/mainline"));
    }
}
