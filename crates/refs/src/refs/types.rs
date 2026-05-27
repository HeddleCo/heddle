// SPDX-License-Identifier: Apache-2.0
//! Public types for ref operations.

use objects::object::{ChangeId, MarkerName, ThreadName};

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
        expected: RefExpectation<ChangeId>,
        new: Option<ChangeId>,
    },
    Marker {
        name: MarkerName,
        expected: RefExpectation<ChangeId>,
        new: Option<ChangeId>,
    },
    Head {
        expected: RefExpectation<Head>,
        new: Head,
    },
}
