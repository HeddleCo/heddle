// SPDX-License-Identifier: Apache-2.0
use objects::object::ChangeId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushComplete {
    pub success: bool,
    pub new_state: Option<ChangeId>,
    pub error: Option<String>,
    #[serde(default)]
    pub transfer_id: String,
    #[serde(default)]
    pub transport_mode: String,
    #[serde(default)]
    pub resume_offset: u64,
    #[serde(default)]
    pub chunk_index: u32,
    #[serde(default)]
    pub checkpoint: Vec<u8>,
    #[serde(default)]
    pub is_complete: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullComplete {
    pub success: bool,
    pub final_state: Option<ChangeId>,
    pub error: Option<String>,
    #[serde(default)]
    pub transfer_id: String,
    #[serde(default)]
    pub transport_mode: String,
    #[serde(default)]
    pub resume_offset: u64,
    #[serde(default)]
    pub chunk_index: u32,
    #[serde(default)]
    pub checkpoint: Vec<u8>,
    #[serde(default)]
    pub is_complete: bool,
}
