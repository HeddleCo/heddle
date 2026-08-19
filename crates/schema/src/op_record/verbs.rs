// SPDX-License-Identifier: Apache-2.0
//! Public aliases for the oplog verb catalog.
//!
//! The durable catalog name for a capture is `snapshot` (see
//! [`super::OpRecord::verb`]). The CLI verb is `capture`. Query filters
//! accept the public name, case-insensitively, and still match the stored
//! catalog verb.

use super::OpRecord;

impl OpRecord {
    /// Resolve a user-supplied query/log filter to a catalog verb.
    ///
    /// `capture` (any case) maps to `snapshot`. Other inputs match
    /// [`OpRecord::verbs`] case-insensitively. Unknown names return `None`
    /// so callers can fail closed instead of silently widening the filter.
    pub fn resolve_verb(input: &str) -> Option<&'static str> {
        let needle = input.trim();
        if needle.is_empty() {
            return None;
        }
        if needle.eq_ignore_ascii_case("capture") {
            return Some("snapshot");
        }
        Self::verbs(true)
            .into_iter()
            .find(|verb| verb.eq_ignore_ascii_case(needle))
    }

    /// Human-facing name for a stored catalog verb.
    pub fn public_verb_name(verb: &str) -> &str {
        if verb == "snapshot" { "capture" } else { verb }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_aliases_snapshot_case_insensitively() {
        assert_eq!(OpRecord::resolve_verb("capture"), Some("snapshot"));
        assert_eq!(OpRecord::resolve_verb("Capture"), Some("snapshot"));
        assert_eq!(OpRecord::resolve_verb("CAPTURE"), Some("snapshot"));
        assert_eq!(OpRecord::resolve_verb(" snapshot "), Some("snapshot"));
    }

    #[test]
    fn catalog_verbs_resolve_case_insensitively() {
        assert_eq!(
            OpRecord::resolve_verb("Transaction_Commit"),
            Some("transaction_commit")
        );
        assert_eq!(OpRecord::resolve_verb("CHECKPOINT"), Some("checkpoint"));
    }

    #[test]
    fn unknown_verb_is_none() {
        assert_eq!(OpRecord::resolve_verb("captur"), None);
        assert_eq!(OpRecord::resolve_verb(""), None);
        assert_eq!(OpRecord::resolve_verb("   "), None);
    }

    #[test]
    fn public_name_rewrites_snapshot_only() {
        assert_eq!(OpRecord::public_verb_name("snapshot"), "capture");
        assert_eq!(
            OpRecord::public_verb_name("transaction_commit"),
            "transaction_commit"
        );
    }
}
