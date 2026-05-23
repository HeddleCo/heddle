// SPDX-License-Identifier: Apache-2.0
//! Text codec helpers for loose ref payloads.

use objects::object::ChangeId;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid change id text: {0}")]
pub struct ChangeIdTextError(pub String);

pub fn parse_change_id_text(contents: &str) -> Result<ChangeId, ChangeIdTextError> {
    let trimmed = contents.trim();
    ChangeId::parse(trimmed).map_err(|_| ChangeIdTextError(trimmed.to_string()))
}

pub fn format_change_id_text(id: &ChangeId) -> String {
    format!("{}\n", id.to_string_full())
}
