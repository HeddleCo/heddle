// SPDX-License-Identifier: Apache-2.0
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuthToken {
    pub id: String,
    pub user: String,
}

impl AuthToken {
    pub fn new(id: impl Into<String>, user: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            user: user.into(),
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        rmp_serde::to_vec(self).expect("auth token serialization should not fail")
    }

    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        rmp_serde::from_slice(bytes).ok()
    }
}
