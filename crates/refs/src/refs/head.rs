// SPDX-License-Identifier: Apache-2.0
//! HEAD reference definition.

use objects::object::{ChangeId, ThreadName};

/// Parse error for HEAD text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid HEAD: {0}")]
pub struct HeadParseError(pub String);

/// HEAD reference - points to current state.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Head {
    Attached { thread: ThreadName },
    Detached { state: ChangeId },
}

impl Head {
    pub fn parse(contents: &str) -> Result<Self, HeadParseError> {
        let contents = contents.trim();
        if let Some(thread) = contents.strip_prefix("ref: ") {
            Ok(Head::Attached {
                thread: ThreadName::new(thread),
            })
        } else if let Ok(id) = ChangeId::parse(contents) {
            Ok(Head::Detached { state: id })
        } else {
            Err(HeadParseError(contents.to_string()))
        }
    }

    pub fn to_text(&self) -> String {
        match self {
            Head::Attached { thread } => format!("ref: {}\n", thread),
            Head::Detached { state } => format!("{}\n", state.to_string_full()),
        }
    }
}
