// SPDX-License-Identifier: Apache-2.0

/// Cheap storage facts used to decide whether a repack is worthwhile.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RepackInventory {
    /// Objects currently stored loose.
    pub loose_objects: u64,
    /// On-disk bytes occupied by loose objects.
    pub loose_bytes: u64,
    /// Active pack count.
    pub pack_count: u64,
    /// Bytes occupied by active pack and index files.
    pub pack_bytes: u64,
    /// Pack entries whose identity also occurs in another active pack.
    pub duplicate_objects: u64,
    /// Total entries across active packs, including duplicates.
    pub packed_objects: u64,
}

impl RepackInventory {
    /// Duplicate-entry fragmentation in basis points (`10_000 == 100%`).
    pub fn fragmentation_bps(self) -> u16 {
        if self.packed_objects == 0 {
            return 0;
        }
        let bps = self
            .duplicate_objects
            .saturating_mul(10_000)
            .checked_div(self.packed_objects)
            .unwrap_or(0);
        bps.min(10_000) as u16
    }
}

/// The policy signal that caused a background repack.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepackReason {
    /// An operator explicitly requested a repack.
    Manual,
    /// The loose-object threshold was crossed.
    LooseObjects { count: u64 },
    /// The active pack-count threshold was crossed.
    PackCount { count: u64 },
    /// Multiple packs crossed the combined-size threshold.
    PackBytes { bytes: u64 },
    /// Duplicate entries crossed the fragmentation threshold.
    Fragmentation { basis_points: u16 },
}

/// Configurable automatic repack thresholds.
///
/// `None` disables a signal. Size only triggers when at least two packs exist:
/// rewriting one already-consolidated large pack would otherwise thrash forever.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepackPolicy {
    /// Trigger after this many loose objects.
    pub loose_object_threshold: Option<u64>,
    /// Trigger after this many active packs.
    pub pack_count_threshold: Option<u64>,
    /// Trigger when two or more packs occupy at least this many bytes.
    pub pack_bytes_threshold: Option<u64>,
    /// Trigger at this duplicate-entry ratio in basis points.
    pub fragmentation_threshold_bps: Option<u16>,
}

impl Default for RepackPolicy {
    fn default() -> Self {
        Self {
            loose_object_threshold: Some(10_000),
            pack_count_threshold: Some(8),
            pack_bytes_threshold: Some(1024 * 1024 * 1024),
            fragmentation_threshold_bps: Some(1_500),
        }
    }
}

impl RepackPolicy {
    /// Return the first policy signal crossed by `inventory`.
    pub fn evaluate(self, inventory: RepackInventory) -> Option<RepackReason> {
        if self
            .loose_object_threshold
            .is_some_and(|threshold| inventory.loose_objects >= threshold)
        {
            return Some(RepackReason::LooseObjects {
                count: inventory.loose_objects,
            });
        }
        if self
            .pack_count_threshold
            .is_some_and(|threshold| inventory.pack_count >= threshold)
        {
            return Some(RepackReason::PackCount {
                count: inventory.pack_count,
            });
        }
        if inventory.pack_count > 1
            && self
                .pack_bytes_threshold
                .is_some_and(|threshold| inventory.pack_bytes >= threshold)
        {
            return Some(RepackReason::PackBytes {
                bytes: inventory.pack_bytes,
            });
        }
        let fragmentation = inventory.fragmentation_bps();
        if self
            .fragmentation_threshold_bps
            .is_some_and(|threshold| fragmentation >= threshold)
        {
            return Some(RepackReason::Fragmentation {
                basis_points: fragmentation,
            });
        }
        None
    }
}
