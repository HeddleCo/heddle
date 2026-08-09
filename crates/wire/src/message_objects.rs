// SPDX-License-Identifier: Apache-2.0
use objects::object::ContentHash;
use serde::{Deserialize, Serialize};

use crate::{ObjectId, ObjectType};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectRequest {
    pub id: ObjectId,
    pub have_base: Option<ContentHash>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectData {
    pub id: ObjectId,
    pub obj_type: ObjectType,
    pub data: Vec<u8>,
    pub is_delta: bool,
}
