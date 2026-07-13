// SPDX-License-Identifier: Apache-2.0
//! Pure collapse/expand planning (single decision type).

/// Collapse requires non-empty source states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollapsePlan {
    Proceed,
    StatesRequired,
}

impl CollapsePlan {
    pub fn plan(state_count: usize) -> Self {
        if state_count > 0 {
            Self::Proceed
        } else {
            Self::StatesRequired
        }
    }

    /// Advice kind when collapse is invoked without sources.
    pub fn states_required_kind() -> &'static str {
        "collapse_states_required"
    }
}

/// Whether collapse has at least one source state.
pub fn collapse_has_source_states(state_count: usize) -> bool {
    matches!(CollapsePlan::plan(state_count), CollapsePlan::Proceed)
}

/// Advice kind when collapse is invoked without sources.
pub fn collapse_states_required_kind() -> &'static str {
    CollapsePlan::states_required_kind()
}

pub fn plan_collapse(state_count: usize) -> CollapsePlan {
    CollapsePlan::plan(state_count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapse_plan_requires_states() {
        assert_eq!(plan_collapse(0), CollapsePlan::StatesRequired);
        assert_eq!(plan_collapse(2), CollapsePlan::Proceed);
        assert!(!collapse_has_source_states(0));
        assert!(collapse_has_source_states(1));
        assert_eq!(collapse_states_required_kind(), "collapse_states_required");
    }
}
