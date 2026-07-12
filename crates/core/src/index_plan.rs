// SPDX-License-Identifier: Apache-2.0
//! Pure worktree-index dump planning (no FS load / dump I/O).
//!
//! Single plan enum — not separate message + gate factories.
//! Loading, dumping, and JSON profile stats stay CLI-owned.

/// Pure plan for a dump request given whether the index file is present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexDumpPlan {
    /// Dump path should report absence (no load attempt needed).
    Absent { path_display: String },
    /// Index is present; CLI may load and dump.
    Proceed,
    /// Dump was not requested.
    NotRequested,
}

impl IndexDumpPlan {
    /// Plan dump from flags and on-disk presence (caller supplies facts).
    pub fn plan(dump: bool, present: bool, path_display: &str) -> Self {
        if !dump {
            Self::NotRequested
        } else if present {
            Self::Proceed
        } else {
            Self::Absent {
                path_display: path_display.to_string(),
            }
        }
    }

    /// Human line when dump is requested but the index file is missing.
    pub fn absent_message(&self) -> Option<String> {
        match self {
            Self::Absent { path_display } => Some(format!(
                "No index found at {path_display}. Run a snapshot or status command first."
            )),
            _ => None,
        }
    }
}

/// Human line when dump is requested but the index file is missing.
pub fn index_missing_message(path_display: &str) -> String {
    IndexDumpPlan::plan(true, false, path_display)
        .absent_message()
        .expect("Absent plan always has a message")
}

/// Whether the human dump path should report absence (no load attempt needed).
pub fn plan_index_absent_dump(dump: bool, present: bool) -> bool {
    matches!(
        IndexDumpPlan::plan(dump, present, ""),
        IndexDumpPlan::Absent { .. }
    )
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
        assert!(matches!(
            IndexDumpPlan::plan(true, false, "p"),
            IndexDumpPlan::Absent { .. }
        ));
        assert_eq!(IndexDumpPlan::plan(true, true, "p"), IndexDumpPlan::Proceed);
    }
}
