// SPDX-License-Identifier: Apache-2.0
//! Text codec helpers for loose ref payloads.

use objects::object::StateId;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid state id text: {0}")]
pub struct StateIdTextError(pub String);

pub fn parse_state_id_text(contents: &str) -> Result<StateId, StateIdTextError> {
    let trimmed = contents.trim();
    StateId::parse(trimmed).map_err(|_| StateIdTextError(trimmed.to_string()))
}

pub fn format_state_id_text(id: &StateId) -> String {
    format!("{}\n", id.to_string_full())
}
