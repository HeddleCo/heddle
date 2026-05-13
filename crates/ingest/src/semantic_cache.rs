// SPDX-License-Identifier: Apache-2.0
//! Ingest-side parse cache keyed on git blob SHA.
//!
//! When the importer walks a sizable repo, the same blob SHA shows up
//! many times — most files don't change between commits, and reflog
//! entries replay earlier states. Re-parsing the same source text into
//! a tree-sitter AST for every appearance is pure waste.
//!
//! [`semantic::SemanticParseCache`] already memoizes parses keyed
//! on the **BLAKE3 content hash** of the bytes. That's the right key
//! for cross-crate reuse (any two callers hash the same bytes to the
//! same key), but it forces us to hash every blob again — even though
//! the git blob SHA already names the same content.
//!
//! This cache sits in front of `SemanticParseCache` and keys on the
//! git blob SHA the importer already has in hand. Repeat blobs across
//! commits hit the local map in O(1) with no hashing. Cold blobs fall
//! through to the upstream cache, so any downstream consumer sharing
//! the process-wide singleton still benefits from the parse.
//!
//! # Usage
//!
//! ```ignore
//! let mut cache = IngestSemanticCache::new();
//! let parsed = cache.parse_blob(&git_sha, Path::new("src/lib.rs"), &bytes);
//! ```
//!
//! The cache is **not** `Send + Sync` — it mutates its own stats and
//! the internal map on every call. Wrap it in a `Mutex` if you need to
//! share across threads; the typical importer is single-threaded per
//! repo so this is usually fine.

use std::{collections::HashMap, path::Path, sync::Arc};

use semantic::{Language, ParsedFile, SemanticParseCache};

/// Counters the importer can surface to diagnose parse-cache
/// effectiveness. All counters are cumulative and never decrease.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IngestSemanticCacheStats {
    /// Blob SHA already in the local map — returned without re-parsing
    /// or re-hashing the bytes.
    pub sha_hits: usize,
    /// Blob SHA unseen; parse executed (and cached for future hits).
    pub parses: usize,
    /// Blob had an unsupported or unknown language extension — we
    /// don't even attempt to parse. Counted so operators can see how
    /// much of a repo is "opaque" to semantic analysis.
    pub language_skips: usize,
    /// Bytes weren't valid UTF-8 (binary asset, image, etc.). Skipped.
    pub binary_skips: usize,
    /// Parse was attempted but tree-sitter returned `None` (grammar
    /// couldn't read the file — common for files with embedded
    /// template languages or mid-edit syntax errors).
    pub parse_failures: usize,
}

/// Parse cache keyed on git blob SHA. Wraps an inner
/// [`SemanticParseCache`] so cross-module reuse still works — any
/// bytes we hand off to the inner cache are available to other
/// process-wide consumers even if they came in via a different key.
pub struct IngestSemanticCache {
    by_git_sha: HashMap<String, Option<Arc<ParsedFile>>>,
    inner: Arc<SemanticParseCache>,
    stats: IngestSemanticCacheStats,
}

impl Default for IngestSemanticCache {
    fn default() -> Self {
        Self::new()
    }
}

impl IngestSemanticCache {
    /// Build a new cache backed by a fresh inner [`SemanticParseCache`].
    /// Use this when the importer wants its cache isolated from the
    /// rest of the process (e.g. in tests).
    pub fn new() -> Self {
        Self::with_inner(Arc::new(SemanticParseCache::default()))
    }

    /// Build a cache that shares its parse memoization with an external
    /// [`SemanticParseCache`]. Passing `SemanticParseCache::shared()`
    /// here wires the importer into the process-wide singleton.
    pub fn with_inner(inner: Arc<SemanticParseCache>) -> Self {
        Self {
            by_git_sha: HashMap::new(),
            inner,
            stats: IngestSemanticCacheStats::default(),
        }
    }

    /// Parse `bytes` as the given file, returning a shared handle to
    /// the AST. Results are cached per git blob SHA, so subsequent
    /// calls with the same `git_sha` return the prior result without
    /// re-hashing or re-parsing.
    ///
    /// Returns `None` in the same cases the underlying parser does:
    ///
    /// - Unsupported language (language_skips++)
    /// - Non-UTF-8 bytes (binary_skips++)
    /// - tree-sitter parse failed (parse_failures++)
    pub fn parse_blob(
        &mut self,
        git_sha: &str,
        path: &Path,
        bytes: &[u8],
    ) -> Option<Arc<ParsedFile>> {
        // SHA-keyed fast path. Hash lookup is cheap relative to
        // ContentHash::compute + HashMap lookup on a 500KB blob.
        if let Some(cached) = self.by_git_sha.get(git_sha) {
            self.stats.sha_hits += 1;
            return cached.clone();
        }

        let language = Language::from_path(path);
        if matches!(language, Language::Unknown) {
            self.stats.language_skips += 1;
            self.by_git_sha.insert(git_sha.to_string(), None);
            return None;
        }

        let Ok(source) = std::str::from_utf8(bytes) else {
            self.stats.binary_skips += 1;
            self.by_git_sha.insert(git_sha.to_string(), None);
            return None;
        };

        // Delegate to the inner cache so cross-module consumers see the
        // same parse. Inner updates its own (content-hash, language)
        // stats independently.
        let parsed = self.inner.parse(source, language);
        match &parsed {
            Some(_) => self.stats.parses += 1,
            None => self.stats.parse_failures += 1,
        }
        self.by_git_sha.insert(git_sha.to_string(), parsed.clone());
        parsed
    }

    /// Current counters. See [`IngestSemanticCacheStats`].
    pub fn stats(&self) -> IngestSemanticCacheStats {
        self.stats
    }

    /// Number of distinct blob SHAs the cache has seen (including
    /// skipped/failed ones — every call inserts an entry).
    pub fn len(&self) -> usize {
        self.by_git_sha.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_git_sha.is_empty()
    }

    /// Borrow the inner [`SemanticParseCache`] — handy if a caller
    /// wants to surface the BLAKE3-keyed stats alongside our sha-
    /// keyed ones.
    pub fn inner(&self) -> &SemanticParseCache {
        &self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha_hit_returns_without_reparsing() {
        let mut cache = IngestSemanticCache::new();
        let src = "fn main() {}\n";
        // First call → parses.
        let a = cache.parse_blob("sha1", Path::new("m.rs"), src.as_bytes());
        // Second call with the same SHA → sha_hit, skip parse.
        let b = cache.parse_blob("sha1", Path::new("m.rs"), src.as_bytes());
        assert!(a.is_some() && b.is_some());
        assert!(Arc::ptr_eq(&a.unwrap(), &b.unwrap()));
        let s = cache.stats();
        assert_eq!(s.sha_hits, 1);
        assert_eq!(s.parses, 1);
        assert_eq!(s.binary_skips, 0);
        assert_eq!(s.language_skips, 0);
    }

    #[test]
    fn unknown_language_is_cached_as_none() {
        let mut cache = IngestSemanticCache::new();
        // `.xyz` isn't in `Language::from_path` — should skip without
        // reading bytes, and the skip should itself be memoized so a
        // second call is a sha_hit.
        assert!(
            cache
                .parse_blob("sha-xyz", Path::new("file.xyz"), b"irrelevant")
                .is_none()
        );
        assert!(
            cache
                .parse_blob("sha-xyz", Path::new("file.xyz"), b"irrelevant")
                .is_none()
        );
        let s = cache.stats();
        assert_eq!(s.language_skips, 1);
        assert_eq!(s.sha_hits, 1);
        assert_eq!(s.parses, 0);
    }

    #[test]
    fn binary_bytes_skip_without_parsing() {
        let mut cache = IngestSemanticCache::new();
        // 0xFF sequences are not valid UTF-8 in Rust's UTF-8 decoder.
        let bytes: &[u8] = &[0xFF, 0xFE, 0xFD, 0x00];
        let got = cache.parse_blob("sha-bin", Path::new("data.rs"), bytes);
        assert!(got.is_none());
        let s = cache.stats();
        assert_eq!(s.binary_skips, 1);
        assert_eq!(s.parses, 0);
    }

    #[test]
    fn different_shas_same_content_each_parse_once() {
        // Two blobs with different SHAs but identical bytes will each
        // insert a map entry. Upstream `SemanticParseCache` (keyed on
        // content hash) should coalesce — one of the two hits the
        // inner cache instead of running the parser — but our own
        // `parses` counter increments for both because we count
        // sha-level misses, not content-level ones.
        let mut cache = IngestSemanticCache::new();
        let src = "fn a() {}\n";
        let _ = cache.parse_blob("sha-A", Path::new("a.rs"), src.as_bytes());
        let _ = cache.parse_blob("sha-B", Path::new("b.rs"), src.as_bytes());
        let s = cache.stats();
        assert_eq!(s.sha_hits, 0);
        assert_eq!(s.parses, 2);
        // Inner cache should see one miss + one hit.
        let inner_stats = cache.inner().stats();
        assert_eq!(inner_stats.hits, 1);
        assert_eq!(inner_stats.misses, 1);
    }

    #[test]
    fn parse_failures_are_counted_separately_from_skips() {
        // A Rust file with real syntax errors → parser returns None.
        // Should increment parse_failures, not language_skips.
        let mut cache = IngestSemanticCache::new();
        // `fn (` is invalid at tree-sitter level.
        let src = "fn (((( {";
        let got = cache.parse_blob("sha-bad", Path::new("x.rs"), src.as_bytes());
        assert!(got.is_none());
        let s = cache.stats();
        assert_eq!(s.parse_failures, 1);
        assert_eq!(s.language_skips, 0);
        // Second call is a sha_hit even though the first failed.
        let _ = cache.parse_blob("sha-bad", Path::new("x.rs"), src.as_bytes());
        assert_eq!(cache.stats().sha_hits, 1);
    }

    #[test]
    fn with_inner_shares_parse_memoization() {
        // Two ingest caches sharing an inner should both benefit from
        // the first cache's parse.
        let inner = Arc::new(SemanticParseCache::default());
        let src = "fn a() {}\n";
        let mut a = IngestSemanticCache::with_inner(inner.clone());
        let mut b = IngestSemanticCache::with_inner(inner.clone());
        let _ = a.parse_blob("a-sha", Path::new("f.rs"), src.as_bytes());
        let _ = b.parse_blob("b-sha", Path::new("f.rs"), src.as_bytes());
        // Inner parsed once, reused once.
        let s = inner.stats();
        assert_eq!(s.misses, 1);
        assert_eq!(s.hits, 1);
    }
}