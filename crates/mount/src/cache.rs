// SPDX-License-Identifier: Apache-2.0
//! Shared blob cache for `ContentAddressedMount`.
//!
//! The cache holds materialised file bytes keyed by `ContentHash` so a
//! hot kernel `read` syscall is a pointer-bump + slice copy rather than
//! a full `get_blob` + `decompress` chain. It's the difference between
//! mount reads beating `std::fs::read` (warm) and being ~10× slower
//! (cold) — see `benches/mount_read_paths.rs`.
//!
//! Why a separate, shared pool instead of owning the cache inline?
//!
//! Heddle's content-addressed model means two threads forked from the
//! same parent share *every* blob hash on the parts of the tree they
//! haven't diverged on yet. If each mount carries its own LRU, every
//! freshly-opened mount starts cold even when a sibling mount in the
//! same process just decompressed the exact same bytes a millisecond
//! ago. By making the cache an `Arc<BlobCachePool>` the daemon can
//! attach one pool to itself, hand it to every new mount, and every
//! cache-hot blob anywhere in the process is hot for the new mount
//! too. Cap stays the same; hit rate goes up.
//!
//! The pool is byte-bounded, not entry-bounded — a 256 MiB cap holds
//! roughly 25 × 10 MiB blobs or 250 000 × 1 KiB blobs, whichever the
//! workload happens to be. Eviction is LRU. A single blob larger than
//! the cap bypasses the cache entirely so one giant file can't
//! evict the rest of the working set.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex,
};

use bytes::Bytes;
use lru::LruCache;
use objects::object::ContentHash;

/// Default cap. Picked so a typical agent workspace fits in memory
/// without the cache dominating RSS — for daemon deployments the
/// recommended sizing is `min(4 GiB, 25% of physical RAM)`, set via
/// [`BlobCachePool::with_capacity`].
pub const DEFAULT_BLOB_CACHE_BYTES: usize = 256 * 1024 * 1024;

/// Process-shared blob cache. Construct once per `Repository` (or once
/// per daemon process) and hand the same `Arc<BlobCachePool>` to every
/// [`crate::ContentAddressedMount`] that wants to share its warm
/// state with sibling mounts.
///
/// Clone-cheap: it's just `Arc<Inner>` under the hood.
pub struct BlobCachePool {
    inner: Mutex<BlobCacheInner>,
    cap_bytes: usize,
    hits: AtomicU64,
    misses: AtomicU64,
    inserts: AtomicU64,
}

struct BlobCacheInner {
    lru: LruCache<ContentHash, Bytes>,
    /// Running sum of `Bytes.len()` over every live entry. Kept
    /// in sync with `lru` mutations so eviction can stop the moment
    /// the working total drops back under `cap_bytes`.
    bytes: usize,
}

impl BlobCachePool {
    /// Construct a pool with [`DEFAULT_BLOB_CACHE_BYTES`] cap. Suitable
    /// for tests, CLI one-shots, and any caller that doesn't have a
    /// sizing strategy.
    pub fn with_default_capacity() -> Self {
        Self::with_capacity(DEFAULT_BLOB_CACHE_BYTES)
    }

    /// Construct a pool with an explicit byte cap. Daemon callers
    /// should size this from physical RAM (see module docs).
    pub fn with_capacity(cap_bytes: usize) -> Self {
        Self {
            inner: Mutex::new(BlobCacheInner {
                lru: LruCache::unbounded(),
                bytes: 0,
            }),
            cap_bytes,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            inserts: AtomicU64::new(0),
        }
    }

    /// Look up a blob. Returns `Some(Arc::clone)` on hit, `None` on
    /// miss. Bumps the entry to MRU.
    pub(crate) fn get(&self, hash: &ContentHash) -> Option<Bytes> {
        let mut guard = self.inner.lock().expect("blob cache lock");
        match guard.lru.get(hash).cloned() {
            Some(bytes) => {
                self.hits.fetch_add(1, Ordering::Relaxed);
                Some(bytes)
            }
            None => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// Insert a freshly-loaded blob. Evicts LRU entries until the
    /// total falls back below cap. A blob larger than cap bypasses
    /// the cache entirely so one giant file can't displace the rest
    /// of the working set.
    pub(crate) fn insert(&self, hash: ContentHash, bytes: Bytes) {
        let size = bytes.len();
        if size > self.cap_bytes {
            return;
        }
        let mut guard = self.inner.lock().expect("blob cache lock");
        if let Some(prev) = guard.lru.put(hash, bytes) {
            guard.bytes = guard.bytes.saturating_sub(prev.len());
        }
        guard.bytes += size;
        while guard.bytes > self.cap_bytes {
            let Some((_evicted_hash, evicted)) = guard.lru.pop_lru() else {
                break;
            };
            guard.bytes = guard.bytes.saturating_sub(evicted.len());
        }
        self.inserts.fetch_add(1, Ordering::Relaxed);
    }

    /// Drop every cached entry. Used by benchmarks that need to
    /// measure the true cold path.
    pub fn clear(&self) {
        let mut guard = self.inner.lock().expect("blob cache lock");
        guard.lru.clear();
        guard.bytes = 0;
    }

    /// Byte cap configured at construction time.
    pub fn cap_bytes(&self) -> usize {
        self.cap_bytes
    }

    /// Current resident bytes across all entries. Cheap (single lock
    /// acquire) but not free — meant for diagnostics, not hot-path
    /// budgeting.
    pub fn resident_bytes(&self) -> usize {
        self.inner.lock().expect("blob cache lock").bytes
    }

    /// Number of distinct blobs currently cached. Same caveat as
    /// `resident_bytes`.
    pub fn entry_count(&self) -> usize {
        self.inner.lock().expect("blob cache lock").lru.len()
    }

    /// Cumulative hit / miss / insert counters since the pool was
    /// constructed. Useful for sizing decisions: a pool with
    /// `inserts >> hits` is undersized, a pool with `hits >> inserts`
    /// is right-sized for its workload.
    pub fn stats(&self) -> BlobCacheStats {
        BlobCacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            inserts: self.inserts.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlobCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub inserts: u64,
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn h(byte: u8) -> ContentHash {
        let mut buf = [0u8; 32];
        buf.iter_mut().for_each(|b| *b = byte);
        ContentHash::from_bytes(buf)
    }

    fn payload(byte: u8, len: usize) -> Bytes {
        Bytes::from(vec![byte; len])
    }

    #[test]
    fn round_trip_get_hit_miss() {
        let pool = BlobCachePool::with_capacity(1024);
        assert!(pool.get(&h(1)).is_none());
        pool.insert(h(1), payload(0xAA, 64));
        let hit = pool.get(&h(1)).expect("should hit");
        assert_eq!(&hit[..], &vec![0xAA; 64][..]);
        let stats = pool.stats();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.inserts, 1);
    }

    #[test]
    fn byte_bound_evicts_lru_until_under_cap() {
        let pool = BlobCachePool::with_capacity(300);
        pool.insert(h(1), payload(0x01, 100));
        pool.insert(h(2), payload(0x02, 100));
        pool.insert(h(3), payload(0x03, 100));
        assert_eq!(pool.resident_bytes(), 300);
        pool.insert(h(4), payload(0x04, 100));
        // h(1) is the LRU after the inserts; it should have been
        // evicted to keep us under cap.
        assert!(pool.get(&h(1)).is_none());
        assert!(pool.get(&h(4)).is_some());
        assert_eq!(pool.resident_bytes(), 300);
    }

    #[test]
    fn oversized_blob_bypasses_cache() {
        let pool = BlobCachePool::with_capacity(256);
        // Existing entry should survive.
        pool.insert(h(1), payload(0x01, 200));
        // 500 > cap → bypassed, no eviction.
        pool.insert(h(2), payload(0x02, 500));
        assert!(pool.get(&h(1)).is_some());
        assert!(pool.get(&h(2)).is_none());
        assert_eq!(pool.resident_bytes(), 200);
    }

    #[test]
    fn shared_pool_visible_across_clones() {
        let pool = Arc::new(BlobCachePool::with_capacity(1024));
        let a = Arc::clone(&pool);
        let b = Arc::clone(&pool);
        a.insert(h(1), payload(0xAA, 64));
        assert!(b.get(&h(1)).is_some());
    }
}
