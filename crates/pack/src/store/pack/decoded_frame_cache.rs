// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::{HashMap, VecDeque},
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::Arc,
};

use bytes::Bytes;
use heddle_object_model::object::State;

const DEFAULT_DECODED_FRAME_CACHE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug)]
pub(super) struct DecodedFrameKey {
    pack_path: Arc<PathBuf>,
    record_offset: u64,
}

impl DecodedFrameKey {
    pub(super) fn new(pack_path: Arc<PathBuf>, record_offset: usize) -> Self {
        Self {
            pack_path,
            record_offset: record_offset as u64,
        }
    }
}

impl PartialEq for DecodedFrameKey {
    fn eq(&self, other: &Self) -> bool {
        self.record_offset == other.record_offset && self.pack_path == other.pack_path
    }
}

impl Eq for DecodedFrameKey {}

impl Hash for DecodedFrameKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.pack_path.hash(state);
        self.record_offset.hash(state);
    }
}

#[derive(Debug)]
pub(super) struct DecodedFrame {
    pub(super) bytes: Bytes,
    pub(super) states: Option<Vec<State>>,
    resident_bytes: usize,
}

impl DecodedFrame {
    pub(super) fn new(bytes: Bytes, states: Option<Vec<State>>) -> Self {
        let state_bytes = states.as_ref().map_or(0, |values| {
            values
                .len()
                .saturating_mul(std::mem::size_of::<State>())
                // Compact columns hold the variable-width state data. Count
                // one additional frame-sized allowance for the allocations
                // materialised from those columns.
                .saturating_add(bytes.len())
        });
        let resident_bytes = bytes.len().saturating_add(state_bytes);
        Self {
            bytes,
            states,
            resident_bytes,
        }
    }
}

/// Process-local LRU of decompressed compact frames.
///
/// Paths distinguish immutable packs while offsets distinguish physical
/// records within a pack. The byte budget is global to a `PackManager`, so a
/// repository with many packs cannot multiply the allowance per reader.
#[derive(Debug)]
pub(super) struct DecodedFrameCache {
    entries: HashMap<DecodedFrameKey, Arc<DecodedFrame>>,
    lru: VecDeque<DecodedFrameKey>,
    resident_bytes: usize,
    byte_budget: usize,
}

impl DecodedFrameCache {
    pub(super) fn new() -> Self {
        Self::with_byte_budget(DEFAULT_DECODED_FRAME_CACHE_BYTES)
    }

    fn with_byte_budget(byte_budget: usize) -> Self {
        Self {
            entries: HashMap::new(),
            lru: VecDeque::new(),
            resident_bytes: 0,
            byte_budget,
        }
    }

    pub(super) fn get(&mut self, key: &DecodedFrameKey) -> Option<Arc<DecodedFrame>> {
        let frame = Arc::clone(self.entries.get(key)?);
        self.promote(key);
        Some(frame)
    }

    pub(super) fn insert(
        &mut self,
        key: DecodedFrameKey,
        frame: Arc<DecodedFrame>,
    ) -> Arc<DecodedFrame> {
        if frame.resident_bytes > self.byte_budget {
            return frame;
        }
        if let Some(previous) = self.entries.remove(&key) {
            self.resident_bytes = self.resident_bytes.saturating_sub(previous.resident_bytes);
            self.lru.retain(|candidate| candidate != &key);
        }
        while self.resident_bytes.saturating_add(frame.resident_bytes) > self.byte_budget {
            let Some(oldest) = self.lru.pop_front() else {
                break;
            };
            if let Some(evicted) = self.entries.remove(&oldest) {
                self.resident_bytes = self.resident_bytes.saturating_sub(evicted.resident_bytes);
            }
        }
        self.resident_bytes = self.resident_bytes.saturating_add(frame.resident_bytes);
        self.lru.push_back(key.clone());
        self.entries.insert(key, Arc::clone(&frame));
        frame
    }

    fn promote(&mut self, key: &DecodedFrameKey) {
        self.lru.retain(|candidate| candidate != key);
        self.lru.push_back(key.clone());
    }
}

pub(super) fn in_memory_pack_path() -> Arc<PathBuf> {
    Arc::new(Path::new("<memory-pack>").to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_budget_evicts_the_least_recently_used_frame() {
        let path = Arc::new(PathBuf::from("pack"));
        let keys = [
            DecodedFrameKey::new(Arc::clone(&path), 1),
            DecodedFrameKey::new(Arc::clone(&path), 2),
            DecodedFrameKey::new(path, 3),
        ];
        let mut cache = DecodedFrameCache::with_byte_budget(8);
        cache.insert(
            keys[0].clone(),
            Arc::new(DecodedFrame::new(Bytes::from_static(b"aaaa"), None)),
        );
        cache.insert(
            keys[1].clone(),
            Arc::new(DecodedFrame::new(Bytes::from_static(b"bbbb"), None)),
        );
        assert!(cache.get(&keys[0]).is_some());
        cache.insert(
            keys[2].clone(),
            Arc::new(DecodedFrame::new(Bytes::from_static(b"cccc"), None)),
        );

        assert!(cache.get(&keys[0]).is_some());
        assert!(cache.get(&keys[1]).is_none());
        assert!(cache.get(&keys[2]).is_some());
    }
}
