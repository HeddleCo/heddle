// SPDX-License-Identifier: Apache-2.0
//! HEAD reference definition.

use objects::object::{StateId, ThreadName};

/// Parse error for HEAD text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid HEAD: {0}")]
pub struct HeadParseError(pub String);

/// HEAD reference - points to current state.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Head {
    Attached { thread: ThreadName },
    Detached { state: StateId },
}

impl Head {
    pub fn parse(contents: &str) -> Result<Self, HeadParseError> {
        let contents = objects::name_encoding::strip_ref_line_ending(contents);
        if let Some(thread) = contents.strip_prefix("ref: ") {
            ThreadName::from_git_branch(&objects::name_encoding::git_name(thread))
                .map_err(|_| HeadParseError(contents.to_owned()))?;
            super::name::validate_ref_name(thread)
                .map_err(|_| HeadParseError(contents.to_owned()))?;
            Ok(Head::Attached {
                thread: ThreadName::new(thread),
            })
        } else if let Ok(id) = StateId::parse(contents) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_keeps_unicode_whitespace_and_rejects_invalid_framing() {
        assert!(Head::parse("ref: a\n\n").is_err());
        for name in ["trailing\u{a0}", "literal\u{fffd}", "@", "a,b"] {
            let head = Head::Attached {
                thread: ThreadName::new(name),
            };
            assert_eq!(Head::parse(&head.to_text()).expect("LF"), head);
            assert_eq!(
                Head::parse(&format!("ref: {name}\r\n")).expect("CRLF"),
                head
            );
        }
    }
}
