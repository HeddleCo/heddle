// SPDX-License-Identifier: Apache-2.0
//! Shared in-band representation for Git gitlinks/submodules.

use std::fmt::Display;

pub const SUBMODULE_PREFIX: &str = "heddle-submodule:";

pub fn gitlink_blob_content(oid: impl Display) -> Vec<u8> {
    format!("{SUBMODULE_PREFIX} {oid}").into_bytes()
}
