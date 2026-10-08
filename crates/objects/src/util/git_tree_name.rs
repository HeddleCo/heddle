// SPDX-License-Identifier: Apache-2.0
//! Git tree-entry name classification shared by import engines.

use crate::object::{ReservedMetadataName, reserved_tree_entry_name, validate_tree_entry_name};

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

/// Classify one Git tree entry name for import. `at_root` says the entry is
/// a direct child of a commit's root tree, where `.heddle` is reserved too;
/// `symlink` says it is a symlink, which `.gitmodules` may not be (see
/// [`reserved_tree_entry_name`]).
///
/// A reserved name is an `Err`: import must fail, and not even `--lossy`
/// may drop it, because the tree is hostile rather than unrepresentable.
pub fn classify_git_tree_name(
    raw_name: &[u8],
    at_root: bool,
    symlink: bool,
) -> Result<GitTreeNameClassification, ReservedMetadataName> {
    // Checked on the raw bytes, before any lossy conversion could hide or
    // create an alias.
    match reserved_tree_entry_name(raw_name, at_root, symlink) {
        Some(reason) => Err(reason),
        None => Ok(classify_git_tree_name_representable(raw_name)),
    }
}

/// Classify only whether a Git tree entry name is representable, without
/// the reserved-name check. For read-only projections of Git history, which
/// never write a worktree (checkout still refuses reserved names).
pub fn classify_git_tree_name_representable(raw_name: &[u8]) -> GitTreeNameClassification {
    let (name, utf8_lossy) = match std::str::from_utf8(raw_name) {
        Ok(name) => (name.to_string(), false),
        Err(_) => (String::from_utf8_lossy(raw_name).into_owned(), true),
    };

    // Validate the FINAL name (after any UTF-8 replacement) against the
    // canonical tree-name validator, so this classifier's representable set
    // can never drift from what Heddle will actually store (path separators
    // '/' and '\', '.'/'..', control bytes, empty). Critically, a name that
    // is invalid UTF-8 AND otherwise unrepresentable (e.g. `bad\<0xff>` ->
    // lossy `bad\<U+FFFD>` still containing a backslash) must be Dropped, not
    // silently persisted as Converted.
    match (validate_tree_entry_name(&name), utf8_lossy) {
        (Ok(()), false) => GitTreeNameClassification::Representable(name),
        (Ok(()), true) => GitTreeNameClassification::NeedsLossy(GitTreeNameLossy {
            name,
            action: GitTreeNameLossyAction::Converted,
            reason: "tree entry name is not valid UTF-8 and was converted with replacement characters",
        }),
        (Err(_), _) => GitTreeNameClassification::NeedsLossy(GitTreeNameLossy {
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
            ".git",
            ".heddle",
        ];
        for c in cases {
            let classified_representable = matches!(
                classify_git_tree_name_representable(c.as_bytes()),
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
            classify_git_tree_name_representable(b"foo\\bar"),
            GitTreeNameClassification::NeedsLossy(_)
        ));
    }

    #[test]
    fn invalid_utf8_is_converted_not_dropped() {
        match classify_git_tree_name_representable(&[b'a', 0xff, b'b']) {
            GitTreeNameClassification::NeedsLossy(lossy) => {
                assert_eq!(lossy.action, GitTreeNameLossyAction::Converted);
            }
            other => panic!("expected NeedsLossy/Converted, got {other:?}"),
        }
    }

    #[test]
    fn invalid_utf8_that_stays_unrepresentable_after_conversion_is_dropped() {
        // `bad\<0xff>`: invalid UTF-8 AND contains a backslash. Lossy UTF-8
        // conversion replaces the 0xff but the backslash survives, so the
        // converted name is still rejected by validate_tree_entry_name and
        // must be Dropped — never silently persisted as Converted.
        match classify_git_tree_name_representable(b"bad\\\xff") {
            GitTreeNameClassification::NeedsLossy(lossy) => {
                assert_eq!(lossy.action, GitTreeNameLossyAction::Dropped);
            }
            other => panic!("expected NeedsLossy/Dropped, got {other:?}"),
        }
    }

    #[test]
    fn reserved_names_fail_rather_than_go_lossy() {
        let cases: [(&[u8], bool, bool); 10] = [
            (b".git", false, false),
            (b".GIT", false, false),
            (b".git::$INDEX_ALLOCATION", false, false),
            (b"git~1", false, false),
            (b".git\\hooks", false, false),
            (b".git\xff", false, false),
            (b".heddle", true, false),
            (b".HEDDLE", true, false),
            (b".gitmodules", false, true),
            (b"GITMOD~1", false, true),
        ];
        for (raw, at_root, symlink) in cases {
            assert!(
                classify_git_tree_name(raw, at_root, symlink).is_err(),
                "{raw:?}"
            );
        }
        // A nested `.heddle` is ordinary content.
        assert_eq!(
            classify_git_tree_name(b".heddle", false, false),
            Ok(GitTreeNameClassification::Representable(
                ".heddle".to_string()
            ))
        );
        // A regular `.gitmodules` file is ordinary.
        assert_eq!(
            classify_git_tree_name(b".gitmodules", true, false),
            Ok(GitTreeNameClassification::Representable(
                ".gitmodules".to_string()
            ))
        );
    }
}
