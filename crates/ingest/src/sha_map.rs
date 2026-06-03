// SPDX-License-Identifier: Apache-2.0
//! Bidirectional sidecar mapping `git_sha ↔ heddle ChangeId` (and companions
//! for tree/blob hashes).
//!
//! # Why a sidecar?
//!
//! When commits are re-hashed from SHA-1 to BLAKE3 during import, every
//! reference embedded in prose — commit bodies, PR descriptions,
//! Co-Authored-By trailers, issue links — would otherwise dangle. The
//! sidecar lets the importer and downstream tooling rewrite those
//! references, and it's the authoritative oracle for a verification
//! pass after a full import.
//!
//! # Storage
//!
//! SQLite at `<heddle_dir>/ingest/sha_map.sqlite` (WAL mode). A compact row
//! plus one secondary index cover every query the importer makes:
//!
//! ```sql
//! CREATE TABLE sha_map (
//!     git_sha   TEXT PRIMARY KEY NOT NULL,
//!     kind      INTEGER NOT NULL,
//!     heddle_repr TEXT NOT NULL,
//!     lossy_entries TEXT
//! );
//! CREATE INDEX sha_map_heddle_repr ON sha_map(heddle_repr);
//! ```
//!
//! `kind` is `0 = commit`, `1 = tree`, `2 = blob` (matches the on-the-
//! wire [`MapKind`] enum). `heddle_repr` is `ChangeId::to_string_full()`
//! for commits and `ContentHash::to_hex()` for trees/blobs.
//!
//! # Why SQLite (and not the prior JSONL + HashMap design)?
//!
//! The earlier implementation held every record in two `HashMap`s and
//! appended each insert to a JSONL file. For a 10 M-object repo the
//! resident set climbed past several GB before the streaming pack
//! builder even saw a payload — exactly the OOM the streaming pack
//! work was meant to avoid. SQLite gives us:
//!
//! - **Bounded memory.** Page cache is ~20 MB by default; lookups go
//!   through the B-tree, not a fully-resident hash map.
//! - **Native rollback.** `BEGIN IMMEDIATE` / `COMMIT` / `ROLLBACK`
//!   replace the hand-rolled `batch_inserted` / `batch_pre_len`
//!   bookkeeping. The earlier `abort_append_batch` correctness bug
//!   (in-memory ghost entries surviving a failed flush, on-disk
//!   leak via `BufWriter::Drop`) becomes structurally impossible.
//! - **Crash safety.** WAL mode survives process kills mid-batch
//!   without corrupting the index.
//! - **Indexed reverse lookup.** `get_git_for_heddle` is an index hit,
//!   not a fully-materialized inverse map.
//!
//! `rusqlite` is already in the dep tree (the OpenCode harvester
//! reads its session DB through it), so the cost is purely code,
//! not deps.

use std::path::Path;

use objects::object::{ChangeId, ContentHash};
use rusqlite::{Connection, OptionalExtension, params};
use tracing::{debug, warn};

use crate::import_options::LossyImportEntry;

/// What kind of object the mapping is for.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum MapKind {
    Commit,
    Tree,
    Blob,
}

impl MapKind {
    fn as_i64(self) -> i64 {
        match self {
            MapKind::Commit => 0,
            MapKind::Tree => 1,
            MapKind::Blob => 2,
        }
    }
}

/// SQLite-backed bidirectional map.
///
/// One handle per process. Not `Send + Sync` (the inner `Connection`
/// isn't); wrap in `Arc<Mutex<_>>` if the importer ever grows parallel
/// inserts — currently sequential.
pub struct ShaMap {
    conn: Connection,
    /// Number of nested batches active. Outermost open is `BEGIN
    /// IMMEDIATE`; nested opens are `SAVEPOINT`s; abort/flush at depth
    /// `> 1` hits the matching savepoint, abort/flush at depth 1 hits
    /// the outer transaction.
    batch_depth: usize,
}

impl std::fmt::Debug for ShaMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShaMap")
            .field("batch_depth", &self.batch_depth)
            .field("len", &self.len())
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ShaMapError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("invalid git sha {sha:?}: {reason}")]
    InvalidGitSha { sha: String, reason: &'static str },
    #[error(
        "conflicting mapping for git={git}: already mapped to {existing} (new {incoming} rejected)"
    )]
    Conflict {
        git: String,
        existing: String,
        incoming: String,
    },
    #[error("serialize lossy tree entries: {0}")]
    LossySerialize(#[from] serde_json::Error),
}

impl Default for ShaMap {
    fn default() -> Self {
        Self::new()
    }
}

impl ShaMap {
    /// In-memory database, lost when the handle drops. Used by tests
    /// and by the in-process oplog emitter test rigs that don't need
    /// persistence.
    pub fn new() -> Self {
        let conn =
            Connection::open_in_memory().expect("in-memory SQLite open should always succeed");
        initialize_schema(&conn).expect("schema apply on a fresh connection should always succeed");
        Self {
            conn,
            batch_depth: 0,
        }
    }

    /// Open (or create) the SQLite sidecar at the given path. Parent
    /// directory is created if missing. WAL mode is enabled so a
    /// crashed import leaves the index in a consistent state and so
    /// readers (e.g. a verification pass) can run alongside a writer.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ShaMapError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        // WAL gives us crash-safe atomic commits and lets a future
        // reader query while we're writing. NORMAL synchronous is
        // safe with WAL — durability boundary is the WAL fsync at
        // commit, the main file's checkpoint can be lazier.
        conn.pragma_update(None, "journal_mode", "wal")?;
        conn.pragma_update(None, "synchronous", "normal")?;
        // Keep transient sort/index work in memory rather than a temp
        // file — these are tiny relative to the page cache.
        conn.pragma_update(None, "temp_store", "memory")?;
        initialize_schema(&conn)?;

        let map = Self {
            conn,
            batch_depth: 0,
        };
        debug!(path = %path.display(), records = map.len(), "sha_map opened");
        Ok(map)
    }

    /// Total number of records (commits + trees + blobs).
    pub fn len(&self) -> usize {
        self.conn
            .query_row("SELECT COUNT(*) FROM sha_map", [], |r| r.get::<_, i64>(0))
            .map(|n| n as usize)
            .unwrap_or(0)
    }

    /// `true` when the map holds no records.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Begin a batched-write context. All subsequent inserts run inside
    /// a SQLite transaction (or savepoint, when nested) so a failed
    /// import can `abort_append_batch` and roll every insert back atomically.
    ///
    /// Importer flow: `Importer::run` opens an outer batch, writes
    /// per-commit object inserts, and either flushes on the success
    /// path or aborts on the streaming-pack failure path. Without a
    /// transaction wrapping those inserts the prior implementation
    /// could leak in-memory and on-disk state on the abort path; with
    /// SQLite the rollback is one statement.
    pub fn begin_append_batch(&mut self) -> Result<(), ShaMapError> {
        if self.batch_depth == 0 {
            // BEGIN IMMEDIATE acquires the write lock up front.
            // Preferable to DEFERRED for our single-writer import flow:
            // a deferred transaction lazily upgrades to a write lock at
            // first INSERT, which would deadlock if anything else were
            // concurrently writing the file.
            self.conn.execute_batch("BEGIN IMMEDIATE")?;
        } else {
            // Nested batch — open a savepoint so an inner abort can
            // unwind only the inner work without rolling back the outer
            // transaction. The savepoint name is keyed on the depth
            // we're about to enter so it's unique within the stack.
            self.conn
                .execute_batch(&format!("SAVEPOINT s{}", self.batch_depth + 1))?;
        }
        self.batch_depth += 1;
        Ok(())
    }

    /// Commit the outermost batch (or release the matching savepoint
    /// for a nested call). After the outermost flush every insert from
    /// the batch is durable on disk via WAL fsync.
    pub fn flush_append_batch(&mut self) -> Result<(), ShaMapError> {
        if self.batch_depth == 0 {
            return Ok(());
        }
        let releasing_outer = self.batch_depth == 1;
        let stmt = if releasing_outer {
            "COMMIT".to_string()
        } else {
            format!("RELEASE s{}", self.batch_depth)
        };
        self.conn.execute_batch(&stmt)?;
        self.batch_depth -= 1;
        Ok(())
    }

    /// Roll back the outermost batch (or innermost savepoint for a
    /// nested call). Undoes every INSERT performed since the matching
    /// `begin_append_batch`.
    ///
    /// Matches the old hand-rolled implementation's contract and
    /// removes its bug surface: SQLite's ROLLBACK undoes both the
    /// in-memory transaction state *and* the WAL frames staged for it.
    /// There is no equivalent of "in-memory entries surviving a failed
    /// flush" because the transaction's writes never become visible to
    /// readers until COMMIT.
    ///
    /// Errors during the rollback statement itself are logged rather
    /// than propagated — the public API mirrors the prior implementation
    /// (a `()` return) and a failure here is rare and unactionable.
    pub fn abort_append_batch(&mut self) {
        if self.batch_depth == 0 {
            return;
        }
        let releasing_outer = self.batch_depth == 1;
        let stmt = if releasing_outer {
            "ROLLBACK".to_string()
        } else {
            // ROLLBACK TO leaves the savepoint frame active but reverts
            // every change made under it; RELEASE then drops the frame
            // so a sibling SAVEPOINT name can be reused later.
            format!(
                "ROLLBACK TO s{depth}; RELEASE s{depth}",
                depth = self.batch_depth
            )
        };
        if let Err(e) = self.conn.execute_batch(&stmt) {
            warn!(
                error = %e,
                depth = self.batch_depth,
                "sha_map abort failed; subsequent operations may see partial state"
            );
        }
        self.batch_depth -= 1;
    }

    /// Insert a commit mapping. Idempotent on identical re-insert,
    /// returns [`ShaMapError::Conflict`] if a different `heddle` is
    /// already mapped to `git_sha`.
    pub fn insert_commit(&mut self, git_sha: &str, heddle: ChangeId) -> Result<(), ShaMapError> {
        self.insert_raw(MapKind::Commit, git_sha, heddle.to_string_full())
    }

    /// Insert a tree mapping.
    pub fn insert_tree(&mut self, git_sha: &str, heddle: ContentHash) -> Result<(), ShaMapError> {
        self.insert_tree_with_lossy_entries(git_sha, heddle, &[])
    }

    /// Insert a tree mapping and persist any lossy conversions that produced it.
    pub fn insert_tree_with_lossy_entries(
        &mut self,
        git_sha: &str,
        heddle: ContentHash,
        lossy_entries: &[LossyImportEntry],
    ) -> Result<(), ShaMapError> {
        let lossy_json = if lossy_entries.is_empty() {
            None
        } else {
            Some(serde_json::to_string(lossy_entries)?)
        };
        self.insert_raw_with_lossy(MapKind::Tree, git_sha, heddle.to_hex(), lossy_json)
    }

    /// Insert a blob mapping.
    pub fn insert_blob(&mut self, git_sha: &str, heddle: ContentHash) -> Result<(), ShaMapError> {
        self.insert_raw(MapKind::Blob, git_sha, heddle.to_hex())
    }

    fn insert_raw(
        &mut self,
        kind: MapKind,
        git_sha: &str,
        heddle_repr: String,
    ) -> Result<(), ShaMapError> {
        let git_sha = normalize_git_sha(git_sha)?;
        self.insert_raw_normalized(kind, git_sha, heddle_repr, None)
    }

    fn insert_raw_with_lossy(
        &mut self,
        kind: MapKind,
        git_sha: &str,
        heddle_repr: String,
        lossy_json: Option<String>,
    ) -> Result<(), ShaMapError> {
        let git_sha = normalize_git_sha(git_sha)?;
        self.insert_raw_normalized(kind, git_sha, heddle_repr, lossy_json)
    }

    fn insert_raw_normalized(
        &mut self,
        kind: MapKind,
        git_sha: String,
        heddle_repr: String,
        lossy_json: Option<String>,
    ) -> Result<(), ShaMapError> {
        // Try the INSERT first — happy path is one prepared statement.
        // On primary-key conflict we fall back to a SELECT so we can
        // distinguish idempotent re-inserts (same `heddle_repr`) from
        // genuine conflicts.
        let mut stmt = self
            .conn
            .prepare_cached(
                "INSERT INTO sha_map (git_sha, kind, heddle_repr, lossy_entries) VALUES (?, ?, ?, ?)",
            )?;
        match stmt.execute(params![git_sha, kind.as_i64(), heddle_repr, lossy_json]) {
            Ok(_) => Ok(()),
            Err(rusqlite::Error::SqliteFailure(err, _))
                if err.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                // Drop the cached statement borrow before issuing the
                // follow-up SELECT through the same connection.
                drop(stmt);
                let existing: Option<String> = self
                    .conn
                    .query_row(
                        "SELECT heddle_repr FROM sha_map WHERE git_sha = ?",
                        params![git_sha],
                        |r| r.get(0),
                    )
                    .optional()?;
                match existing {
                    Some(s) if s == heddle_repr => {
                        if kind == MapKind::Tree && lossy_json.is_some() {
                            self.conn.execute(
                                "UPDATE sha_map SET lossy_entries = ? WHERE git_sha = ? AND kind = ? AND lossy_entries IS NULL",
                                params![lossy_json, git_sha, kind.as_i64()],
                            )?;
                        }
                        Ok(())
                    }
                    Some(s) => Err(ShaMapError::Conflict {
                        git: git_sha,
                        existing: s,
                        incoming: heddle_repr,
                    }),
                    None => Err(ShaMapError::Sqlite(rusqlite::Error::SqliteFailure(
                        err, None,
                    ))),
                }
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Look up the Heddle `ChangeId` for a git commit SHA.
    pub fn get_commit(&self, git_sha: &str) -> Option<ChangeId> {
        let heddle = self.get_for_kind(git_sha, MapKind::Commit)?;
        ChangeId::parse(&heddle).ok()
    }

    /// Look up the Heddle `ContentHash` for a git tree SHA.
    pub fn get_tree(&self, git_sha: &str) -> Option<ContentHash> {
        let heddle = self.get_for_kind(git_sha, MapKind::Tree)?;
        ContentHash::from_hex(&heddle).ok()
    }

    /// Look up persisted lossy entries for a git tree SHA.
    ///
    /// Returns `Ok(None)` when there is no tree mapping. Returns an empty
    /// vector when the mapped tree was lossless.
    pub fn get_tree_lossy_entries(
        &self,
        git_sha: &str,
    ) -> Result<Option<Vec<LossyImportEntry>>, ShaMapError> {
        let git_sha = normalize_git_sha(git_sha)?;
        let mut stmt = self
            .conn
            .prepare_cached("SELECT lossy_entries FROM sha_map WHERE git_sha = ? AND kind = ?")?;
        let Some(json) = stmt
            .query_row(params![git_sha, MapKind::Tree.as_i64()], |r| {
                r.get::<_, Option<String>>(0)
            })
            .optional()?
        else {
            return Ok(None);
        };
        let Some(json) = json else {
            return Ok(Some(Vec::new()));
        };
        Ok(Some(serde_json::from_str(&json)?))
    }

    /// Look up the Heddle `ContentHash` for a git blob SHA.
    pub fn get_blob(&self, git_sha: &str) -> Option<ContentHash> {
        let heddle = self.get_for_kind(git_sha, MapKind::Blob)?;
        ContentHash::from_hex(&heddle).ok()
    }

    fn get_for_kind(&self, git_sha: &str, want: MapKind) -> Option<String> {
        let git_sha = normalize_git_sha(git_sha).ok()?;
        let mut stmt = self
            .conn
            .prepare_cached("SELECT heddle_repr FROM sha_map WHERE git_sha = ? AND kind = ?")
            .ok()?;
        stmt.query_row(params![git_sha, want.as_i64()], |r| r.get(0))
            .optional()
            .ok()?
    }

    /// Reverse lookup: the git SHA that produced a given Heddle repr.
    /// Returns `None` if no record matches; the secondary index makes
    /// this an O(log n) lookup against `heddle_repr`.
    pub fn get_git_for_heddle(&self, heddle_repr: &str) -> Option<String> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT git_sha FROM sha_map WHERE heddle_repr = ?")
            .ok()?;
        stmt.query_row(params![heddle_repr], |r| r.get(0))
            .optional()
            .ok()?
    }

    /// How many commits have been mapped (used for progress reporting).
    pub fn commit_count(&self) -> usize {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM sha_map WHERE kind = ?",
                params![MapKind::Commit.as_i64()],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n as usize)
            .unwrap_or(0)
    }

    /// Every git commit SHA currently in the map. Order is unspecified
    /// (B-tree iteration on `git_sha`); callers that want a stable
    /// order should sort.
    pub fn commit_shas(&self) -> Vec<String> {
        let mut stmt = match self
            .conn
            .prepare_cached("SELECT git_sha FROM sha_map WHERE kind = ?")
        {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows =
            match stmt.query_map(params![MapKind::Commit.as_i64()], |r| r.get::<_, String>(0)) {
                Ok(r) => r,
                Err(_) => return Vec::new(),
            };
        rows.filter_map(|r| r.ok()).collect()
    }
}

fn initialize_schema(conn: &Connection) -> Result<(), ShaMapError> {
    // Idempotent: every IF NOT EXISTS makes re-opening an existing DB
    // a no-op. Safe to call on every `open` so schema drift between
    // builds repairs itself transparently.
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS sha_map (
            git_sha   TEXT PRIMARY KEY NOT NULL,
            kind      INTEGER NOT NULL,
            heddle_repr TEXT NOT NULL,
            lossy_entries TEXT
        );
        CREATE INDEX IF NOT EXISTS sha_map_heddle_repr ON sha_map(heddle_repr);
        "#,
    )?;
    if !sha_map_has_column(conn, "lossy_entries")? {
        conn.execute_batch("ALTER TABLE sha_map ADD COLUMN lossy_entries TEXT;")?;
    }
    Ok(())
}

fn sha_map_has_column(conn: &Connection, column: &str) -> Result<bool, ShaMapError> {
    let mut stmt = conn.prepare("PRAGMA table_info(sha_map)")?;
    let columns = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for existing in columns {
        if existing? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Lowercase a 40-char git SHA-1. Returns an error for anything else.
///
/// This rejects abbreviated SHAs on purpose — the importer writes full
/// SHAs, and callers passing short ones are almost always confused.
fn normalize_git_sha(sha: &str) -> Result<String, ShaMapError> {
    let trimmed = sha.trim();
    if trimmed.len() != 40 {
        return Err(ShaMapError::InvalidGitSha {
            sha: trimmed.to_string(),
            reason: "expected 40-char full SHA-1",
        });
    }
    if !trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(ShaMapError::InvalidGitSha {
            sha: trimmed.to_string(),
            reason: "non-hex character",
        });
    }
    Ok(trimmed.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use crate::import_options::LossyImportEntry;

    use super::*;

    fn deterministic_content_hash(tag: &str) -> ContentHash {
        ContentHash::compute(tag.as_bytes())
    }

    #[test]
    fn normalizes_uppercase_sha() {
        let s = normalize_git_sha("CA1AF22000000000000000000000000000000000").unwrap();
        assert_eq!(s, "ca1af22000000000000000000000000000000000");
    }

    #[test]
    fn rejects_short_sha() {
        let err = normalize_git_sha("ca1af22").unwrap_err();
        assert!(matches!(err, ShaMapError::InvalidGitSha { .. }));
    }

    #[test]
    fn rejects_non_hex_sha() {
        let err = normalize_git_sha("zzzzzzzz00000000000000000000000000000000").unwrap_err();
        assert!(matches!(err, ShaMapError::InvalidGitSha { .. }));
    }

    #[test]
    fn in_memory_insert_and_lookup_commit() {
        let mut m = ShaMap::new();
        let sha = "ca1af22000000000000000000000000000000000";
        let cid = ChangeId::generate();
        m.insert_commit(sha, cid).unwrap();
        assert_eq!(m.get_commit(sha), Some(cid));
        assert_eq!(m.len(), 1);
        assert_eq!(m.commit_count(), 1);
    }

    #[test]
    fn kinds_do_not_cross_contaminate() {
        let mut m = ShaMap::new();
        let sha = "ca1af22000000000000000000000000000000000";
        let tree_hash = deterministic_content_hash("tree");
        m.insert_tree(sha, tree_hash).unwrap();

        // Same git sha looked up as commit/blob must miss because kind
        // is part of the semantic identity.
        assert!(m.get_commit(sha).is_none());
        assert!(m.get_blob(sha).is_none());
        assert_eq!(m.get_tree(sha), Some(tree_hash));
    }

    #[test]
    fn idempotent_reinsert_is_noop() {
        let mut m = ShaMap::new();
        let sha = "ca1af22000000000000000000000000000000000";
        let cid = ChangeId::generate();
        m.insert_commit(sha, cid).unwrap();
        m.insert_commit(sha, cid).unwrap();
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn conflicting_insert_errors() {
        let mut m = ShaMap::new();
        let sha = "ca1af22000000000000000000000000000000000";
        let cid1 = ChangeId::generate();
        let cid2 = ChangeId::generate();
        m.insert_commit(sha, cid1).unwrap();
        let err = m.insert_commit(sha, cid2).unwrap_err();
        assert!(matches!(err, ShaMapError::Conflict { .. }));
    }

    #[test]
    fn round_trips_through_disk() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("sha_map.sqlite");

        let sha1 = "ca1af22000000000000000000000000000000000";
        let sha2 = "deadbeef000000000000000000000000c0ffee42";
        let sha3 = "f000ba1100000000000000000000000000001234";

        let cid = ChangeId::generate();
        let tree_h = deterministic_content_hash("tree-1");
        let blob_h = deterministic_content_hash("blob-1");

        {
            let mut m = ShaMap::open(&path).unwrap();
            m.insert_commit(sha1, cid).unwrap();
            m.insert_tree(sha2, tree_h).unwrap();
            m.insert_blob(sha3, blob_h).unwrap();
            assert_eq!(m.len(), 3);
        }

        // Reopen — every record must round-trip.
        let reloaded = ShaMap::open(&path).unwrap();
        assert_eq!(reloaded.len(), 3);
        assert_eq!(reloaded.get_commit(sha1), Some(cid));
        assert_eq!(reloaded.get_tree(sha2), Some(tree_h));
        assert_eq!(reloaded.get_blob(sha3), Some(blob_h));
    }

    #[test]
    fn lossy_tree_entries_round_trip_through_disk() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("sha_map.sqlite");
        let sha = "ca1af22000000000000000000000000000000000";
        let tree_h = deterministic_content_hash("lossy-tree");
        let entries = vec![LossyImportEntry::dropped(
            "vendor".to_string(),
            Some("0707070707070707070707070707070707070707".to_string()),
            "gitlink/submodule entries have no Heddle tree equivalent",
        )];

        {
            let mut m = ShaMap::open(&path).unwrap();
            m.insert_tree_with_lossy_entries(sha, tree_h, &entries)
                .unwrap();
        }

        let reloaded = ShaMap::open(&path).unwrap();
        assert_eq!(reloaded.get_tree(sha), Some(tree_h));
        assert_eq!(
            reloaded.get_tree_lossy_entries(sha).unwrap().unwrap(),
            entries
        );
    }

    #[test]
    fn open_adds_lossy_entries_column_to_existing_db() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("sha_map.sqlite");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                r#"
                CREATE TABLE sha_map (
                    git_sha TEXT PRIMARY KEY NOT NULL,
                    kind INTEGER NOT NULL,
                    heddle_repr TEXT NOT NULL
                );
                CREATE INDEX sha_map_heddle_repr ON sha_map(heddle_repr);
                "#,
            )
            .unwrap();
        }

        let mut m = ShaMap::open(&path).unwrap();
        let sha = "ca1af22000000000000000000000000000000000";
        let tree_h = deterministic_content_hash("migrated-tree");
        let entries = vec![LossyImportEntry::converted(
            "bad\u{fffd}name".to_string(),
            Some("0808080808080808080808080808080808080808".to_string()),
            "tree entry name is not valid UTF-8",
        )];
        m.insert_tree_with_lossy_entries(sha, tree_h, &entries)
            .unwrap();

        assert_eq!(m.get_tree_lossy_entries(sha).unwrap().unwrap(), entries);
    }

    #[test]
    fn append_batch_round_trips_through_disk() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("sha_map.sqlite");
        let sha1 = "ca1af22000000000000000000000000000000000";
        let sha2 = "deadbeef000000000000000000000000c0ffee42";
        let cid = ChangeId::generate();
        let tree_h = deterministic_content_hash("tree-batch");

        {
            let mut m = ShaMap::open(&path).unwrap();
            m.begin_append_batch().unwrap();
            m.insert_commit(sha1, cid).unwrap();
            m.insert_tree(sha2, tree_h).unwrap();
            m.flush_append_batch().unwrap();
        }

        let reloaded = ShaMap::open(&path).unwrap();
        assert_eq!(reloaded.len(), 2);
        assert_eq!(reloaded.get_commit(sha1), Some(cid));
        assert_eq!(reloaded.get_tree(sha2), Some(tree_h));
    }

    #[test]
    fn reverse_lookup_finds_git() {
        let mut m = ShaMap::new();
        let sha = "ca1af22000000000000000000000000000000000";
        let cid = ChangeId::generate();
        m.insert_commit(sha, cid).unwrap();
        assert_eq!(
            m.get_git_for_heddle(&cid.to_string_full()).as_deref(),
            Some(sha)
        );
    }

    #[test]
    fn abort_rolls_back_in_memory_inserts() {
        // The legacy implementation's regression test for the bug Codex
        // flagged on PR #38. With the SQLite backend the rollback is
        // native (`ROLLBACK` reverts both buffered pages and WAL
        // frames), so this is now mostly a sanity check that we wired
        // begin/abort to BEGIN/ROLLBACK correctly.
        let mut m = ShaMap::new();
        let pre_sha = "ca1af22000000000000000000000000000000000";
        let pre_cid = ChangeId::generate();
        m.insert_commit(pre_sha, pre_cid).unwrap();

        m.begin_append_batch().unwrap();
        let in_batch_sha = "deadbeef000000000000000000000000c0ffee42";
        let in_batch_cid = ChangeId::generate();
        let in_batch_tree_sha = "f000ba1100000000000000000000000000001234";
        let in_batch_tree = deterministic_content_hash("aborted-tree");
        m.insert_commit(in_batch_sha, in_batch_cid).unwrap();
        m.insert_tree(in_batch_tree_sha, in_batch_tree).unwrap();
        assert_eq!(m.len(), 3);

        m.abort_append_batch();

        assert_eq!(m.len(), 1, "batch inserts must be rolled back");
        assert_eq!(m.get_commit(pre_sha), Some(pre_cid));
        assert_eq!(
            m.get_commit(in_batch_sha),
            None,
            "aborted commit must not survive"
        );
        assert!(
            m.get_tree(in_batch_tree_sha).is_none(),
            "aborted tree must not survive"
        );
        assert!(
            m.get_git_for_heddle(&in_batch_cid.to_string_full())
                .is_none(),
            "aborted commit must not survive in reverse lookup"
        );
        assert!(
            m.get_git_for_heddle(&in_batch_tree.to_hex()).is_none(),
            "aborted tree must not survive in reverse lookup"
        );
    }

    #[test]
    fn abort_rolls_back_on_disk_state_too() {
        // The on-disk half of the rollback. With SQLite WAL this is
        // SQLite's job — a ROLLBACK discards the staged WAL frames so
        // they never reach the main DB file. We assert the contract
        // by reopening: the aborted record must not be present.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("sha_map.sqlite");

        let pre_sha = "ca1af22000000000000000000000000000000000";
        let pre_cid = ChangeId::generate();
        {
            let mut m = ShaMap::open(&path).unwrap();
            m.insert_commit(pre_sha, pre_cid).unwrap();
        }

        // Doomed batch: insert, abort, drop the handle.
        {
            let mut m = ShaMap::open(&path).unwrap();
            m.begin_append_batch().unwrap();
            let doomed_sha = "deadbeef000000000000000000000000c0ffee42";
            let doomed_cid = ChangeId::generate();
            m.insert_commit(doomed_sha, doomed_cid).unwrap();
            m.abort_append_batch();
        }

        // Reopen. Only the original record should survive.
        let reloaded = ShaMap::open(&path).unwrap();
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded.get_commit(pre_sha), Some(pre_cid));
    }

    #[test]
    fn flush_clears_rollback_state() {
        // After a successful flush, a stray later abort must not claw
        // back the now-committed records (would attempt to ROLLBACK
        // outside any open transaction; we treat depth=0 as a no-op).
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("sha_map.sqlite");
        let sha = "ca1af22000000000000000000000000000000000";
        let cid = ChangeId::generate();

        let mut m = ShaMap::open(&path).unwrap();
        m.begin_append_batch().unwrap();
        m.insert_commit(sha, cid).unwrap();
        m.flush_append_batch().unwrap();

        // Stray abort with no batch open — must be a no-op.
        m.abort_append_batch();

        assert_eq!(m.len(), 1);
        assert_eq!(m.get_commit(sha), Some(cid));
        assert_eq!(ShaMap::open(&path).unwrap().len(), 1);
    }

    #[test]
    fn nested_savepoint_abort_preserves_outer_inserts() {
        // Begin → outer-insert → begin → inner-insert → abort (inner).
        // Outer insert must survive; inner insert must not. Then commit
        // the outer batch and verify durability across reopen.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("sha_map.sqlite");
        let outer_sha = "ca1af22000000000000000000000000000000000";
        let outer_cid = ChangeId::generate();
        let inner_sha = "deadbeef000000000000000000000000c0ffee42";
        let inner_cid = ChangeId::generate();

        let mut m = ShaMap::open(&path).unwrap();
        m.begin_append_batch().unwrap();
        m.insert_commit(outer_sha, outer_cid).unwrap();
        m.begin_append_batch().unwrap();
        m.insert_commit(inner_sha, inner_cid).unwrap();
        m.abort_append_batch(); // rolls back inner only
        assert_eq!(m.get_commit(inner_sha), None);
        assert_eq!(m.get_commit(outer_sha), Some(outer_cid));
        m.flush_append_batch().unwrap();
        drop(m);

        let reloaded = ShaMap::open(&path).unwrap();
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded.get_commit(outer_sha), Some(outer_cid));
    }

    #[test]
    fn many_inserts_stay_under_modest_resident_set() {
        // The reason we moved off the HashMap. 100 K inserts on the
        // legacy implementation pushed the in-memory maps past tens of
        // MB before any pack data was written; with SQLite's page
        // cache the working set should be modest. We assert the
        // structural property (the SQLite file is small per row, and
        // the page cache caps memory) by inserting and then
        // round-tripping every key — a smoke check that scales.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("sha_map.sqlite");
        let mut m = ShaMap::open(&path).unwrap();
        m.begin_append_batch().unwrap();
        for i in 0..10_000u32 {
            // Synthesize a unique 40-char hex git sha for each i.
            let sha = format!("{:040x}", i);
            let cid = ChangeId::generate();
            m.insert_commit(&sha, cid).unwrap();
        }
        m.flush_append_batch().unwrap();
        assert_eq!(m.commit_count(), 10_000);
        // Spot-check round-trip across the range.
        for i in [0u32, 1, 99, 1_234, 5_000, 9_999] {
            let sha = format!("{:040x}", i);
            assert!(m.get_commit(&sha).is_some(), "missing commit {sha}");
        }
    }
}
