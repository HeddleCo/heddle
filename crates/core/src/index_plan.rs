// SPDX-License-Identifier: Apache-2.0
//! Pure worktree-index message assembly (no FS load / dump I/O).
//!
//! Owns the human message when the index file is absent on dump.
//! Loading, dumping, and JSON profile stats stay CLI-owned.

/// Human line when dump is requested but the index file is missing.
pub fn index_missing_message(path_display: &str) -> String {
    format!("No index found at {path_display}. Run a snapshot or status command first.")
}

/// Whether the human dump path should report absence (no load attempt needed).
///
/// Pure gate: dump requested and index not present on disk (caller supplies fact).
pub fn plan_index_absent_dump(dump: bool, present: bool) -> bool {
    dump && !present
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_message() {
        let msg = index_missing_message("/repo/.heddle/state/index.bin");
        assert!(msg.contains("/repo/.heddle/state/index.bin"));
        assert!(msg.contains("No index found"));
        assert!(msg.contains("snapshot or status"));
    }

    #[test]
    fn absent_dump_gate() {
        assert!(plan_index_absent_dump(true, false));
        assert!(!plan_index_absent_dump(true, true));
        assert!(!plan_index_absent_dump(false, false));
        assert!(!plan_index_absent_dump(false, true));
    }
}
