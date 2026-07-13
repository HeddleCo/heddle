// SPDX-License-Identifier: Apache-2.0
//! Public types for ref operations.

use objects::object::StateId;

use super::{Head, RefExpectation};

pub(super) fn matches_expectation<T: PartialEq>(
    expectation: &RefExpectation<T>,
    current: Option<&T>,
    exists: bool,
) -> bool {
    match expectation {
        RefExpectation::Any => true,
        RefExpectation::Missing => !exists,
        RefExpectation::Value(value) => current == Some(value),
    }
}

pub(super) fn describe_state_id(value: Option<StateId>) -> String {
    value.map_or_else(|| "missing".to_string(), |id| id.to_string_full())
}

pub(super) fn describe_expectation_state_id(expectation: &RefExpectation<StateId>) -> String {
    match expectation {
        RefExpectation::Any => "any".to_string(),
        RefExpectation::Missing => "missing".to_string(),
        RefExpectation::Value(value) => value.to_string_full(),
    }
}

pub(super) fn describe_head(head: &Head) -> String {
    match head {
        Head::Attached { thread } => format!("ref: {}", thread),
        Head::Detached { state } => state.to_string_full(),
    }
}

pub(super) fn describe_expectation_head(expectation: &RefExpectation<Head>) -> String {
    match expectation {
        RefExpectation::Any => "any".to_string(),
        RefExpectation::Missing => "missing".to_string(),
        RefExpectation::Value(head) => describe_head(head),
    }
}
