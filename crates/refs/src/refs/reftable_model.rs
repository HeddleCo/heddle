// SPDX-License-Identifier: Apache-2.0
//! Prototype reftable-style binary model for the refs spike (HeddleCo/heddle#21).
//!
//! Parallel to [`super::PackedRefsModel`] (line-oriented text) but stored as a
//! binary file with a fixed header, per-section offset indexes, and sorted
//! variable-length records. Designed for O(log N) cold lookup without parsing
//! the whole payload.
//!
//! **Status:** spike prototype. Not wired through `RefManager`; only the
//! in-memory model + serializer + lookup primitives exist, because the spike's
//! deliverable is a ship-or-defer decision (see `docs/design/reftable-spike.md`),
//! not a production backend.

use objects::object::ChangeId;

/// Magic bytes at the start (and end) of a serialized reftable.
pub(super) const MAGIC: &[u8; 8] = b"REFT01\0\0";

/// On-disk header size in bytes: 8 magic + 4 thread_count + 4 marker_count.
pub(super) const HEADER_LEN: usize = 16;

/// On-disk footer size in bytes: 8 magic.
pub(super) const FOOTER_LEN: usize = 8;

#[derive(Debug, thiserror::Error)]
pub enum ReftableError {
    #[error("reftable is truncated or malformed at offset {0}")]
    Truncated(usize),
    #[error("reftable magic bytes missing or wrong")]
    BadMagic,
    #[error("reftable record name is not valid UTF-8")]
    BadUtf8,
}

/// Sorted, binary-format model of repository refs (threads + markers).
///
/// In-memory the records are held as sorted `Vec`s of `(name, ChangeId)` so
/// mutation stays simple. On disk they serialize to the layout described in
/// [`to_bytes`] / [`from_bytes`].
#[derive(Debug)]
pub struct ReftableModel {
    threads: Vec<(String, ChangeId)>,
    markers: Vec<(String, ChangeId)>,
}

impl ReftableModel {
    pub fn new() -> Self {
        Self {
            threads: Vec::new(),
            markers: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.threads.is_empty() && self.markers.is_empty()
    }

    pub fn thread_count(&self) -> usize {
        self.threads.len()
    }

    pub fn marker_count(&self) -> usize {
        self.markers.len()
    }

    /// Insert or replace a thread ref. Keeps `threads` sorted by name.
    pub fn set_thread(&mut self, _name: &str, _id: ChangeId) {
        unimplemented!("reftable_model::set_thread — stub for red commit")
    }

    /// Insert or replace a marker ref. Keeps `markers` sorted by name.
    pub fn set_marker(&mut self, _name: &str, _id: ChangeId) {
        unimplemented!("reftable_model::set_marker — stub for red commit")
    }

    /// Remove a thread ref by name; returns the previous value if present.
    pub fn remove_thread(&mut self, _name: &str) -> Option<ChangeId> {
        unimplemented!("reftable_model::remove_thread — stub for red commit")
    }

    /// Remove a marker ref by name; returns the previous value if present.
    pub fn remove_marker(&mut self, _name: &str) -> Option<ChangeId> {
        unimplemented!("reftable_model::remove_marker — stub for red commit")
    }

    pub fn get_thread(&self, _name: &str) -> Option<ChangeId> {
        unimplemented!("reftable_model::get_thread — stub for red commit")
    }

    pub fn get_marker(&self, _name: &str) -> Option<ChangeId> {
        unimplemented!("reftable_model::get_marker — stub for red commit")
    }

    pub fn list_threads(&self) -> Vec<String> {
        unimplemented!("reftable_model::list_threads — stub for red commit")
    }

    pub fn list_markers(&self) -> Vec<String> {
        unimplemented!("reftable_model::list_markers — stub for red commit")
    }

    /// Serialize to the binary on-disk layout.
    pub fn to_bytes(&self) -> Vec<u8> {
        unimplemented!("reftable_model::to_bytes — stub for red commit")
    }

    /// Deserialize from the binary on-disk layout.
    pub fn from_bytes(_bytes: &[u8]) -> Result<Self, ReftableError> {
        unimplemented!("reftable_model::from_bytes — stub for red commit")
    }

    /// Cold-lookup helper: binary-search a single thread by name directly
    /// against the serialized bytes, without materialising the full model.
    /// Returns `Ok(None)` if the name is not present.
    pub fn lookup_thread_in_bytes(
        _bytes: &[u8],
        _name: &str,
    ) -> Result<Option<ChangeId>, ReftableError> {
        unimplemented!("reftable_model::lookup_thread_in_bytes — stub for red commit")
    }

    /// Cold-lookup helper for markers, see [`lookup_thread_in_bytes`].
    pub fn lookup_marker_in_bytes(
        _bytes: &[u8],
        _name: &str,
    ) -> Result<Option<ChangeId>, ReftableError> {
        unimplemented!("reftable_model::lookup_marker_in_bytes — stub for red commit")
    }
}

impl Default for ReftableModel {
    fn default() -> Self {
        Self::new()
    }
}
