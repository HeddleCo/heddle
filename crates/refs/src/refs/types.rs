// SPDX-License-Identifier: Apache-2.0
//! Public types for ref operations.

use objects::object::{MarkerName, StateId, ThreadName};

use super::Head;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefExpectation<T> {
    Any,
    Missing,
    Value(T),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefUpdate {
    Thread {
        name: ThreadName,
        expected: RefExpectation<StateId>,
        new: Option<StateId>,
    },
    Marker {
        name: MarkerName,
        expected: RefExpectation<StateId>,
        new: Option<StateId>,
    },
    Head {
        expected: RefExpectation<Head>,
        new: Head,
    },
}
