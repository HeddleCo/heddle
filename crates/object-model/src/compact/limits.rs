// SPDX-License-Identifier: Apache-2.0

use super::{Result, invalid};

/// Matches the packer writer split (`FRAME_LIMIT` in `objects`).
///
/// A legitimate compact frame is at most 12 MiB, so a 1-byte item cannot
/// exceed this many declarations. Combined with [`admit_count`], a hostile
/// 1 GiB pack object cannot force a `Vec` reservation from an attacker
/// count near the decompress cap.
pub(super) const MAX_COMPACT_COUNT: usize = 12 * 1024 * 1024;

/// In-memory `State` values are much larger than their compact columns.
/// Cap declared state-frame counts so `blank_state()` cannot approach
/// process-aborting allocation. The writer batches states by 12 MiB of
/// messagepack, well below this many objects.
pub(super) const MAX_COMPACT_STATE_COUNT: usize = 1024 * 1024;

/// Empty blob bodies are valid; each length is at least a 1-byte varint.
pub(super) const MIN_BLOB_ITEM_BYTES: usize = 1;

/// An empty tree is a single zero entry-count varint.
pub(super) const MIN_TREE_ITEM_BYTES: usize = 1;

/// Smallest valid tree entry: mode + kind + 1-byte name + SHA-1 gitlink.
pub(super) const MIN_TREE_ENTRY_BYTES: usize = 1 + 1 + 2 + 21;

/// Columnar floor after dictionaries: change_id, tree, and the per-state
/// tags/varints a minimal state always writes.
pub(super) const MIN_STATE_COLUMN_BYTES: usize = 16 + 32 + 18;

pub(super) const MIN_STATE_PARENT_BYTES: usize = 32;
pub(super) const MIN_EXTRA_HEADER_BYTES: usize = 2;
pub(super) const MIN_LINEAGE_BYTES: usize = 1 + 16 + 32;
pub(super) const MIN_VERIFICATION_CUSTOM_BYTES: usize = 2;
pub(super) const MIN_PRINCIPAL_BYTES: usize = 2;
pub(super) const MIN_AGENT_BYTES: usize = 5;

/// Reject a declared count before any `Vec::with_capacity` or `blank_state`.
pub(super) fn admit_count(
    field: &str,
    count: usize,
    remaining: usize,
    min_item_bytes: usize,
    max_count: usize,
) -> Result<()> {
    if count > max_count {
        return Err(invalid(format!(
            "{field} count {count} exceeds maximum {max_count}"
        )));
    }
    let min_item_bytes = min_item_bytes.max(1);
    match count.checked_mul(min_item_bytes) {
        Some(needed) if needed <= remaining => Ok(()),
        _ => Err(invalid(format!(
            "{field} count {count} exceeds remaining frame bytes {remaining} at {min_item_bytes} bytes per item"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_output_cap_declaration_fails_before_any_alloc() {
        let remaining = 1024 * 1024 * 1024;
        let error = admit_count("blob frame", remaining, remaining, 1, MAX_COMPACT_COUNT)
            .expect_err("1 GiB declared count must not be admitted");
        assert!(
            error.to_string().contains("exceeds maximum"),
            "count gate must fire before with_capacity, got {error}"
        );
    }

    #[test]
    fn encoded_floor_rejects_count_that_fits_remaining_bytes() {
        let error = admit_count(
            "tree entry",
            5,
            100,
            MIN_TREE_ENTRY_BYTES,
            MAX_COMPACT_COUNT,
        )
        .expect_err("1-byte remaining() floor is not enough");
        assert!(error.to_string().contains("bytes per item"), "got {error}");
    }

    #[test]
    fn encoded_floor_accepts_count_that_fits() {
        admit_count(
            "tree entry",
            4,
            4 * MIN_TREE_ENTRY_BYTES,
            MIN_TREE_ENTRY_BYTES,
            MAX_COMPACT_COUNT,
        )
        .expect("exact floor must be admitted");
    }
}
