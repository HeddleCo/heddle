// SPDX-License-Identifier: Apache-2.0
//! Pure GC message assembly and dry-run plan tokens (no store I/O).
//!
//! Owns human summary strings and dry-run field tokens for `heddle maintenance gc`
//! that can be decided from counts / flags alone. Packing, pruning, redaction
//! inventory, and Git Projection mirror I/O stay CLI-owned.

/// Stable status token for GC JSON/human outcome (`"dry_run"` | `"ok"`).
pub fn gc_status_token(dry_run: bool) -> &'static str {
    if dry_run { "dry_run" } else { "ok" }
}

/// Dry-run plan fields derived from object inventory counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GcDryRunPlan {
    /// Total objects that would be considered for packing (blobs + trees).
    pub packed_count: u64,
    /// Outcome status token (`"dry_run"`).
    pub status: &'static str,
}

/// Plan dry-run summary tokens from blob/tree inventory sizes.
pub fn plan_gc_dry_run(blob_count: usize, tree_count: usize) -> GcDryRunPlan {
    GcDryRunPlan {
        packed_count: (blob_count + tree_count) as u64,
        status: gc_status_token(true),
    }
}

/// Dry-run pack line: `Would pack N objects (B blobs, T trees)`.
pub fn gc_dry_run_pack_message(blob_count: usize, tree_count: usize) -> String {
    let total = blob_count + tree_count;
    format!("Would pack {total} objects ({blob_count} blobs, {tree_count} trees)")
}

/// Dry-run prune plan line (always emitted on the human dry-run path).
pub fn gc_dry_run_prune_message() -> &'static str {
    "Would prune redundant loose objects after consolidating into a pack"
}

/// Dry-run pinned-redactions line when count is positive.
pub fn gc_pinned_redactions_message(pinned_redactions: usize) -> Option<String> {
    if pinned_redactions > 0 {
        Some(format!(
            "Pinned {pinned_redactions} redaction tombstone(s) — never collected by GC"
        ))
    } else {
        None
    }
}

/// Ordered human lines for a GC dry-run from inventory + redaction counts.
pub fn gc_dry_run_messages(
    blob_count: usize,
    tree_count: usize,
    pinned_redactions: usize,
) -> Vec<String> {
    let mut lines = vec![
        gc_dry_run_pack_message(blob_count, tree_count),
        gc_dry_run_prune_message().to_string(),
    ];
    if let Some(msg) = gc_pinned_redactions_message(pinned_redactions) {
        lines.push(msg);
    }
    lines
}

/// Pack step human message after `pack_objects`.
pub fn gc_pack_message(packed_count: u64, bytes_saved: u64) -> String {
    if packed_count > 0 {
        format!("Packed {packed_count} objects (saved {bytes_saved} bytes)")
    } else {
        "No objects to pack".to_string()
    }
}

/// Loose-object prune human message after `prune_loose_objects`.
pub fn gc_prune_loose_message(removed: u64, bytes_freed: u64) -> String {
    if removed > 0 {
        format!("Pruned {removed} redundant loose objects (freed {bytes_freed} bytes)")
    } else {
        "No loose objects to prune".to_string()
    }
}

/// Post-GC preserved redactions line when count is positive.
pub fn gc_preserved_redactions_message(pinned_redactions: usize) -> Option<String> {
    if pinned_redactions > 0 {
        Some(format!(
            "Preserved {pinned_redactions} redaction tombstone(s) across GC \
             (structurally outside the object store)"
        ))
    } else {
        None
    }
}

/// Git Projection mapping prune line when entries were removed.
pub fn gc_pruned_git_mapping_message(removed: usize) -> Option<String> {
    if removed > 0 {
        Some(format!(
            "Pruned {removed} stale Git Projection Mapping entries"
        ))
    } else {
        None
    }
}

/// Bridge Mirror consolidation line when loose objects were packed.
pub fn gc_consolidated_mirror_message(consolidated: usize) -> Option<String> {
    if consolidated > 0 {
        Some(format!(
            "Consolidated {consolidated} loose Bridge Mirror objects into a pack"
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dry_run_plan_and_tokens() {
        assert_eq!(gc_status_token(true), "dry_run");
        assert_eq!(gc_status_token(false), "ok");
        assert_eq!(
            plan_gc_dry_run(2, 3),
            GcDryRunPlan {
                packed_count: 5,
                status: "dry_run",
            }
        );
        let lines = gc_dry_run_messages(1, 2, 0);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("Would pack 3 objects"));
        assert!(lines[0].contains("1 blobs"));
        assert_eq!(lines[1], gc_dry_run_prune_message());
        let with_pin = gc_dry_run_messages(0, 0, 4);
        assert_eq!(with_pin.len(), 3);
        assert!(with_pin[2].contains("Pinned 4"));
    }

    #[test]
    fn apply_messages_from_counts() {
        assert_eq!(gc_pack_message(0, 0), "No objects to pack");
        assert!(gc_pack_message(3, 100).contains("Packed 3"));
        assert!(gc_pack_message(3, 100).contains("100 bytes"));
        assert_eq!(gc_prune_loose_message(0, 0), "No loose objects to prune");
        assert!(gc_prune_loose_message(2, 50).contains("Pruned 2"));
        assert!(gc_preserved_redactions_message(0).is_none());
        assert!(
            gc_preserved_redactions_message(1)
                .unwrap()
                .contains("Preserved 1")
        );
        assert!(gc_pruned_git_mapping_message(0).is_none());
        assert!(
            gc_pruned_git_mapping_message(5)
                .unwrap()
                .contains("Pruned 5")
        );
        assert!(gc_consolidated_mirror_message(0).is_none());
        assert!(
            gc_consolidated_mirror_message(7)
                .unwrap()
                .contains("Consolidated 7")
        );
    }
}
