// SPDX-License-Identifier: Apache-2.0
//! Utilities for first-class Gitlink tree entries.

use sley::ObjectId as GitObjectId;

pub fn gitlink_placeholder_bytes(target: &GitObjectId) -> Vec<u8> {
    format!("Heddle Gitlink placeholder\nTarget: {target}\n").into_bytes()
}
