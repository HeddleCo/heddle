// SPDX-License-Identifier: Apache-2.0
//! Process-lifetime per-key cache.
//!
//! `OnceMap<K, V>` is the shape that the CLI's daemon detection and mount
//! lifecycle code all reach for independently: a process-wide `HashMap`
//! whose entries are computed on first access and held until the process
//! exits. The repeated `OnceLock<Mutex<HashMap<K, V>>>` ceremony at each
//! call site disappears behind a single type.
//!
//! Two construction modes:
//!
//! - [`OnceMap::get_or_init_with`] for synchronous initializers (file-stat
//!   probes, key derivation).
//! - [`OnceMap::get_or_init_async`] for async initializers (gRPC channel
//!   construction, network handshakes). Concurrent inserts for *different*
//!   keys don't serialize because the lock is released across the `await`;
//!   concurrent inserts for the *same* key may both run the init future
//!   (last writer wins) — this matches the behavior of every call site
//!   that previously used this pattern.
//!
//! Values are cloned on read. Use a cheaply-cloneable handle (`Arc<…>`,
//! `tonic::transport::Channel`) when the underlying object is expensive
//! to clone.

use std::{
    collections::HashMap,
    hash::Hash,
    sync::{Mutex, OnceLock},
};

use objects::sync::LockExt;

/// A process-lifetime cache that maps `K → V` and computes each entry on
/// first access. See module docs for semantics.
pub struct OnceMap<K, V> {
    inner: OnceLock<Mutex<HashMap<K, V>>>,
}

impl<K, V> OnceMap<K, V> {
    /// Empty cache, suitable for `static` initializers.
    pub const fn new() -> Self {
        Self {
            inner: OnceLock::new(),
        }
    }

    fn map(&self) -> &Mutex<HashMap<K, V>> {
        self.inner.get_or_init(|| Mutex::new(HashMap::new()))
    }
}

impl<K: Eq + Hash + Clone, V: Clone> OnceMap<K, V> {
    /// Return the value for `key`, computing and caching it with `init`
    /// on first access. The lock is held across `init`, so concurrent
    /// callers for the same key serialize.
    pub fn get_or_init_with<F>(&self, key: &K, init: F) -> V
    where
        F: FnOnce() -> V,
    {
        let mut guard = self.map().lock_or_poisoned();
        if let Some(v) = guard.get(key) {
            return v.clone();
        }
        let v = init();
        guard.insert(key.clone(), v.clone());
        v
    }

    /// Async variant of [`Self::get_or_init_with`]. The lock is released
    /// across the await, so different keys don't serialize. Two callers
    /// for the same key may both run `init`; the last write wins.
    pub async fn get_or_init_async<F, Fut>(&self, key: &K, init: F) -> V
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = V>,
    {
        if let Some(v) = self.get(key) {
            return v;
        }
        let v = init().await;
        self.map().lock_or_poisoned().insert(key.clone(), v.clone());
        v
    }

    /// Read without computing. Returns `None` if the key was never inserted.
    pub fn get(&self, key: &K) -> Option<V> {
        self.map().lock_or_poisoned().get(key).cloned()
    }

    /// Direct insert. Returns the previous value if any.
    pub fn insert(&self, key: K, value: V) -> Option<V> {
        self.map().lock_or_poisoned().insert(key, value)
    }

    /// Remove and return the value for `key`, if any. Used by call
    /// sites that need to tear down a cached resource (the mount
    /// registry hands the handle back so the caller can unmount it).
    pub fn remove(&self, key: &K) -> Option<V> {
        self.map().lock_or_poisoned().remove(key)
    }
}

impl<K, V> Default for OnceMap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_once_per_key() {
        let map: OnceMap<String, i32> = OnceMap::new();
        let mut calls = 0;
        let mut init = |v: i32| {
            calls += 1;
            v
        };
        let a = map.get_or_init_with(&"a".to_string(), || init(1));
        let a_again = map.get_or_init_with(&"a".to_string(), || init(2));
        let b = map.get_or_init_with(&"b".to_string(), || init(3));
        assert_eq!(a, 1);
        assert_eq!(a_again, 1);
        assert_eq!(b, 3);
        assert_eq!(calls, 2);
    }

    #[test]
    fn get_returns_none_when_missing() {
        let map: OnceMap<String, i32> = OnceMap::new();
        assert!(map.get(&"missing".to_string()).is_none());
        map.insert("present".to_string(), 7);
        assert_eq!(map.get(&"present".to_string()), Some(7));
    }

    #[tokio::test]
    async fn async_init_caches_value() {
        let map: OnceMap<String, i32> = OnceMap::new();
        let v = map
            .get_or_init_async(&"k".to_string(), || async { 42 })
            .await;
        assert_eq!(v, 42);
        assert_eq!(map.get(&"k".to_string()), Some(42));
    }
}
