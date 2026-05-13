// SPDX-License-Identifier: Apache-2.0
//! Delta message payloads.

use objects::object::ContentHash;
use serde::{Deserialize, Serialize};

/// Request a delta-encoded object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestDelta {
    pub target: ContentHash,
    pub base: ContentHash,
}

/// Delta-encoded data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeltaData {
    pub target: ContentHash,
    pub delta: Vec<u8>,
    pub full_size: u64,
}