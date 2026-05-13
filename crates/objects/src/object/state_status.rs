// SPDX-License-Identifier: Apache-2.0
//! Lifecycle status for states.

use serde::{Deserialize, Serialize};

/// Lifecycle status of a state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Status {
    /// Draft state, freely rewritable.
    #[default]
    Draft,
    /// Published state, history is frozen.
    Published,
}

impl Status {
    /// Convert to byte for serialization.
    pub fn to_byte(&self) -> u8 {
        match self {
            Status::Draft => 0,
            Status::Published => 1,
        }
    }

    /// Parse from byte.
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(Status::Draft),
            1 => Some(Status::Published),
            _ => None,
        }
    }
}