// SPDX-License-Identifier: Apache-2.0
use objects::object::ChangeId;
use serde::{Deserialize, Serialize};

use crate::{ObjectId, ObjectInfo};

pub const TRANSPORT_MODE_NATIVE_PACK: &str = "native-pack";

#[allow(dead_code)]
pub const PARTIAL_FETCH_DISABLED: &str = "disabled";
#[allow(dead_code)]
pub const PARTIAL_FETCH_ENABLED: &str = "enabled";
#[allow(dead_code)]
pub const PARTIAL_FETCH_REQUIRED: &str = "required";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushRequest {
    #[serde(default)]
    pub repo_path: Option<String>,
    pub target_thread: String,
    pub local_state: ChangeId,
    pub create_thread: bool,
    pub force: bool,
    pub objects: Vec<ObjectInfo>,
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
    #[serde(default)]
    pub partial_fetch_status: String,
    #[serde(default)]
    pub allow_partial_fetch: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushReady {
    pub remote_head: Option<ChangeId>,
    pub have_objects: Vec<ObjectId>,
    pub want_objects: Vec<ObjectId>,
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
    #[serde(default)]
    pub partial_fetch_status: String,
    #[serde(default)]
    pub missing_objects: Vec<ObjectId>,
}

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
pub struct PullRequest {
    #[serde(default)]
    pub repo_path: Option<String>,
    pub remote_thread: String,
    pub local_thread: Option<String>,
    pub target_state: Option<ChangeId>,
    #[serde(default)]
    pub depth: Option<u32>,
    #[serde(default)]
    pub exclude_states: Vec<ChangeId>,
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
    #[serde(default)]
    pub partial_fetch_status: String,
    #[serde(default)]
    pub allow_partial_fetch: bool,
    #[serde(default)]
    pub fresh_full_pull: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullReady {
    pub remote_state: ChangeId,
    pub objects_to_fetch: Vec<ObjectInfo>,
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
    #[serde(default)]
    pub partial_fetch_status: String,
    #[serde(default)]
    pub missing_objects: Vec<ObjectId>,
    #[serde(default)]
    pub full_closure_available: bool,
    #[serde(default)]
    pub object_count: u32,
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