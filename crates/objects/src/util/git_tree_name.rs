// SPDX-License-Identifier: Apache-2.0
//! Git tree-entry name classification shared by import engines.

use crate::object::validate_tree_entry_name;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GitTreeNameClassification {
    Representable(String),
    NeedsLossy(GitTreeNameLossy),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitTreeNameLossy {
    pub name: String,
    pub action: GitTreeNameLossyAction,
    pub reason: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitTreeNameLossyAction {
    Dropped,
    Converted,
}

pub fn classify_git_tree_name(raw_name: &[u8]) -> GitTreeNameClassification {
    let name = match std::str::from_utf8(raw_name) {
        Ok(name) => name.to_string(),
        Err(_) => {
            return GitTreeNameClassification::NeedsLossy(GitTreeNameLossy {
                name: String::from_utf8_lossy(raw_name).into_owned(),
                action: GitTreeNameLossyAction::Converted,
                reason: "tree entry name is not valid UTF-8 and was converted with replacement characters",
            });
        }
    };

    // Defer to the canonical tree-name validator so this classifier's
    // "representable" set can never drift from what Heddle will actually
    // store (path separators '/' and '\', '.'/'..', control bytes, empty).
    match validate_tree_entry_name(&name) {
        Ok(()) => GitTreeNameClassification::Representable(name),
        Err(_) => GitTreeNameClassification::NeedsLossy(GitTreeNameLossy {
            name,
            action: GitTreeNameLossyAction::Dropped,
            reason: "tree entry name is not representable in Heddle",
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Close-the-class guard: the classifier's `Representable` verdict must be
    /// EXACTLY the set `validate_tree_entry_name` accepts. If the two ever
    /// diverge (as they did for `\\` before this fix), this fails.
    #[test]
    fn representable_iff_validator_accepts() {
        let cases = [
            "ok.txt",
            "with space",
            "ünïcödé",
            "",
            ".",
            "..",
            "a/b",
            "a\\b",
            "ctrl\u{0001}",
            "del\u{7f}",
        ];
        for c in cases {
            let classified_representable = matches!(
                classify_git_tree_name(c.as_bytes()),
                GitTreeNameClassification::Representable(_)
            );
            let validator_accepts = validate_tree_entry_name(c).is_ok();
            assert_eq!(
                classified_representable, validator_accepts,
                "classifier/validator disagree on {c:?}"
            );
        }
    }

    #[test]
    fn backslash_name_is_not_representable() {
        assert!(matches!(
            classify_git_tree_name(b"foo\\bar"),
            GitTreeNameClassification::NeedsLossy(_)
        ));
    }

    #[test]
    fn invalid_utf8_is_converted_not_dropped() {
        match classify_git_tree_name(&[b'a', 0xff, b'b']) {
            GitTreeNameClassification::NeedsLossy(lossy) => {
                assert_eq!(lossy.action, GitTreeNameLossyAction::Converted);
            }
            other => panic!("expected NeedsLossy/Converted, got {other:?}"),
        }
    }
}
