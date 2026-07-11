// SPDX-License-Identifier: Apache-2.0
//! Pure collapse/expand planning helpers.

/// Whether collapse has at least one source state.
pub fn collapse_has_source_states(state_count: usize) -> bool {
    state_count > 0
}

/// Advice kind when collapse is invoked without sources.
pub fn collapse_states_required_kind() -> &'static str {
    "collapse_states_required"
}

/// Collapse requires non-empty source states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollapsePlan {
    Proceed,
    StatesRequired,
}

pub fn plan_collapse(state_count: usize) -> CollapsePlan {
    if collapse_has_source_states(state_count) {
        CollapsePlan::Proceed
    } else {
        CollapsePlan::StatesRequired
    }
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
    }
}
