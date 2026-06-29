// SPDX-License-Identifier: Apache-2.0
//! In-memory semantic parse cache keyed by stable content identity.

use std::{
    collections::{HashMap, VecDeque, hash_map::Entry},
    sync::{Arc, Mutex, OnceLock},
};

use objects::object::ContentHash;

use crate::parser::{Language, ParsedFile};

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct ParseCacheKey {
    content_hash: ContentHash,
    language: Language,
}

/// Parse cache counters for warm/cold benchmarking.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SemanticParseCacheStats {
    /// Number of successful cache hits.
    pub hits: usize,
    /// Number of cache misses.
    pub misses: usize,
    /// Number of entries inserted into the cache.
    pub stores: usize,
}

#[derive(Debug, Default)]
struct SemanticParseCacheInner {
    entries: HashMap<ParseCacheKey, Option<Arc<ParsedFile>>>,
    order: VecDeque<ParseCacheKey>,
    stats: SemanticParseCacheStats,
}

/// Shared cache for parsed semantic artifacts.
#[derive(Debug)]
pub struct SemanticParseCache {
    inner: Mutex<SemanticParseCacheInner>,
    max_entries: usize,
}

impl SemanticParseCache {
    const DEFAULT_MAX_ENTRIES: usize = 256;

    /// Create a bounded parse cache.
    pub fn new(max_entries: usize) -> Self {
        Self {
            inner: Mutex::new(SemanticParseCacheInner::default()),
            max_entries,
        }
    }

    /// Returns the process-wide cache instance.
    pub fn shared() -> &'static Self {
        static CACHE: OnceLock<SemanticParseCache> = OnceLock::new();
        CACHE.get_or_init(Self::default)
    }

    /// Parse a source file, reusing a cached AST when available.
    pub fn parse(&self, source: &str, language: Language) -> Option<Arc<ParsedFile>> {
        let key = ParseCacheKey {
            content_hash: ContentHash::compute(source.as_bytes()),
            language,
        };

        if let Some(parsed) = self.lookup(key) {
            return parsed;
        }

        let parsed =
            ParsedFile::parse_with_hash(Arc::<str>::from(source), language, key.content_hash)
                .map(Arc::new);
        self.store(key, parsed.clone());
        parsed
    }

    /// Returns current cache counters.
    pub fn stats(&self) -> SemanticParseCacheStats {
        lock_inner(&self.inner).stats
    }

    /// Clears cached entries and counters.
    pub fn clear(&self) {
        let mut inner = lock_inner(&self.inner);
        inner.entries.clear();
        inner.order.clear();
        inner.stats = SemanticParseCacheStats::default();
    }

    fn lookup(&self, key: ParseCacheKey) -> Option<Option<Arc<ParsedFile>>> {
        let mut inner = lock_inner(&self.inner);
        let parsed = inner.entries.get(&key).cloned();
        if parsed.is_some() {
            promote_key(&mut inner.order, key);
            inner.stats.hits += 1;
        } else {
            inner.stats.misses += 1;
        }
        parsed
    }

    fn store(&self, key: ParseCacheKey, parsed: Option<Arc<ParsedFile>>) {
        let mut inner = lock_inner(&self.inner);
        if self.max_entries == 0 {
            inner.stats.stores += 1;
            return;
        }

        if let Entry::Occupied(mut entry) = inner.entries.entry(key) {
            entry.insert(parsed);
            promote_key(&mut inner.order, key);
            inner.stats.stores += 1;
            return;
        }

        while inner.entries.len() >= self.max_entries {
            let Some(evicted) = inner.order.pop_front() else {
                break;
            };
            inner.entries.remove(&evicted);
        }

        inner.entries.insert(key, parsed);
        inner.order.push_back(key);
        inner.stats.stores += 1;
    }
}

impl Default for SemanticParseCache {
    fn default() -> Self {
        Self::new(Self::DEFAULT_MAX_ENTRIES)
    }
}

fn lock_inner(
    mutex: &Mutex<SemanticParseCacheInner>,
) -> std::sync::MutexGuard<'_, SemanticParseCacheInner> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn promote_key(order: &mut VecDeque<ParseCacheKey>, key: ParseCacheKey) {
    if let Some(position) = order.iter().position(|existing| *existing == key) {
        order.remove(position);
    }
    order.push_back(key);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caches_successful_parse_results() {
        let cache = SemanticParseCache::default();
        let source = "fn hello() {}";

        let first = cache.parse(source, Language::Rust);
        let second = cache.parse(source, Language::Rust);

        assert!(first.is_some());
        assert!(second.is_some());
        let stats = cache.stats();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.stores, 1);
    }

    #[test]
    fn caches_failed_parse_results() {
        let cache = SemanticParseCache::default();
        let source = "not valid";

        assert!(cache.parse(source, Language::Unknown).is_none());
        assert!(cache.parse(source, Language::Unknown).is_none());

        let stats = cache.stats();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.stores, 1);
    }

    #[test]
    fn evicts_least_recently_used_entries_when_bound_is_reached() {
        let cache = SemanticParseCache::new(2);

        let first = "fn first() {}";
        let second = "fn second() {}";
        let third = "fn third() {}";

        assert!(cache.parse(first, Language::Rust).is_some());
        assert!(cache.parse(second, Language::Rust).is_some());
        assert!(cache.parse(first, Language::Rust).is_some());
        assert!(cache.parse(third, Language::Rust).is_some());

        let stats_after_warm = cache.stats();
        assert_eq!(stats_after_warm.hits, 1);

        assert!(cache.parse(second, Language::Rust).is_some());
        let stats = cache.stats();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 4);
        assert_eq!(stats.stores, 4);
    }
}
