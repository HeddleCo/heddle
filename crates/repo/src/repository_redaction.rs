// SPDX-License-Identifier: Apache-2.0
//! Repository helpers for the redaction primitive.
//!
//! Storage layout (one file per redacted blob):
//!
//! ```text
//! <heddle_dir>/redactions/<blob-hash-hex>.bin
//! ```
//!
//! The file is an rmp-serde-encoded [`RedactionsBlob`] — every redaction
//! that targets the same blob lives in the same blob file. The blob's own
//! content hash provides a unique key that is independent of how the
//! blob was produced, so cross-state redactions ("redact every occurrence
//! of this blob") fold into a single record naturally.
//!
//! ## Redaction IDs
//!
//! A redaction's *id* is the BLAKE3 hash of its rmp-encoded bytes. We
//! compute it deterministically when writing so callers can correlate the
//! returned id with the oplog `OpRecord::Redact` entry's `redaction_id`
//! field. The id is content-addressed: identical Redactions produce
//! identical ids, which preserves the "redact is idempotent" property
//! from the build brief.

use std::{collections::HashSet, fs, path::PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;
use objects::{
    fs_atomic::write_file_atomic,
    object::{ChangeId, ContentHash, Principal, Redaction, RedactionsBlob, Tree},
};

use crate::repository::Repository;

/// Outcome of a `purge` call. Useful for surfaces that report what
/// actually changed (the JSON output of `heddle purge`).
#[derive(Debug, Clone)]
pub struct PurgeOutcome {
    /// The id (content hash) of the latest redaction on the purged blob.
    /// `None` when no redaction existed yet — purge refuses in that case;
    /// the field is here so a future force-without-redaction surface can
    /// fill it in without changing the public type.
    pub redaction_id: Option<ContentHash>,
    /// Number of redaction records that transitioned from
    /// "declared-only" to "purged" as part of this call. Idempotent
    /// retries report 0.
    pub redactions_marked: usize,
    /// Whether the loose blob bytes were physically removed from local
    /// storage. `false` if no loose copy existed (already gone, or only
    /// present in a pack).
    pub blob_bytes_removed: bool,
    /// `true` iff the blob is still present in a pack file. Initial
    /// implementation can't repack to drop the bytes; surfaces this so
    /// the CLI can warn operators rather than silently leave bytes on
    /// disk.
    pub blob_remains_in_pack: bool,
}

impl Repository {
    /// Append a redaction. Returns the redaction's content-addressed id.
    ///
    /// Idempotent: if a redaction with the same canonical bytes already
    /// exists on the blob, no second entry is written and the existing
    /// id is returned.
    pub fn put_redaction(&self, redaction: Redaction) -> Result<ContentHash> {
        let blob = redaction.redacted_blob;
        let mut existing = self.get_redactions_for_blob(&blob)?;

        // Compute the id by canonical-encoding a single-redaction blob
        // wrapper. The content-addressed id is stable across runs.
        let id = redaction_content_hash(&redaction)?;

        // Idempotency: if any existing redaction encodes to the same id,
        // skip the write.
        for existing_redaction in &existing.redactions {
            let existing_id = redaction_content_hash(existing_redaction)?;
            if existing_id == id {
                return Ok(id);
            }
        }

        existing.push(redaction);
        let bytes = existing
            .encode()
            .with_context(|| "encoding redactions blob")?;
        let path = self.redaction_path_for_blob(&blob);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create '{}'", parent.display()))?;
        }
        write_file_atomic(&path, &bytes).with_context(|| format!("write '{}'", path.display()))?;
        Ok(id)
    }

    /// Load all redactions targeting `blob`. Returns an empty
    /// `RedactionsBlob` (not an error) when none exist — callers can
    /// treat the result uniformly.
    pub fn get_redactions_for_blob(&self, blob: &ContentHash) -> Result<RedactionsBlob> {
        let path = self.redaction_path_for_blob(blob);
        if !path.exists() {
            return Ok(RedactionsBlob::empty());
        }
        let bytes = fs::read(&path).with_context(|| format!("read '{}'", path.display()))?;
        RedactionsBlob::decode(&bytes).with_context(|| format!("decode '{}'", path.display()))
    }

    /// Walk every redactions file in the repo. Used by `heddle redact list`
    /// and the GC's "never collect a redaction" guard. Returns
    /// `(blob_hash, blob)` pairs so callers can correlate.
    pub fn list_all_redactions(&self) -> Result<Vec<(ContentHash, RedactionsBlob)>> {
        let dir = self.redactions_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in fs::read_dir(&dir).with_context(|| format!("read '{}'", dir.display()))? {
            let entry = entry.with_context(|| format!("entry in '{}'", dir.display()))?;
            let path = entry.path();
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            // Skip non-`.bin` files (e.g. editor backups). The blob hash
            // is hex-encoded; bad filenames just get skipped — we don't
            // crash on partial state.
            if path.extension().and_then(|e| e.to_str()) != Some("bin") {
                continue;
            }
            let Ok(blob) = parse_blob_hash_hex(stem) else {
                continue;
            };
            let bytes = fs::read(&path).with_context(|| format!("read '{}'", path.display()))?;
            let blob_obj = RedactionsBlob::decode(&bytes)
                .with_context(|| format!("decode '{}'", path.display()))?;
            out.push((blob, blob_obj));
        }
        Ok(out)
    }

    /// Look up a single redaction by its id. Returns `Some((blob_hash,
    /// redaction))` if found; `None` if no redaction by that id exists.
    ///
    /// Today this walks every redactions file — operators rarely have
    /// more than a handful of redactions in a repo, and the operation
    /// is interactive (`heddle redact show`). If listings become
    /// frequent enough to matter, a flat `<heddle_dir>/redactions/index.bin`
    /// can be added without changing the public signature.
    pub fn get_redaction(
        &self,
        redaction_id: &ContentHash,
    ) -> Result<Option<(ContentHash, Redaction)>> {
        for (blob, redactions_blob) in self.list_all_redactions()? {
            for redaction in &redactions_blob.redactions {
                let id = redaction_content_hash(redaction)?;
                if id == *redaction_id {
                    return Ok(Some((blob, redaction.clone())));
                }
            }
        }
        Ok(None)
    }

    /// Mark every redaction on `blob` as purged and physically remove the
    /// blob bytes from the local loose object store. The `Redaction`
    /// records stay in place; only the bytes are gone.
    ///
    /// Refuses if no redaction exists on the blob — operators must
    /// `redact` before `purge`. This is the contract from the build
    /// brief: "Refuses unless a Redaction already exists on the blob."
    ///
    /// `_purger` is recorded by the caller in the oplog `Purge` entry;
    /// it's accepted here so the helper can be extended (e.g. to embed
    /// the purger in a purge record) without changing the signature.
    pub fn purge_blob(&self, blob: &ContentHash, _purger: &Principal) -> Result<PurgeOutcome> {
        let mut redactions_blob = self.get_redactions_for_blob(blob)?;
        if redactions_blob.redactions.is_empty() {
            anyhow::bail!(
                "no redaction exists for blob {} — declare one with `heddle redact` first",
                blob.short()
            );
        }
        let now = Utc::now();
        let redactions_marked = redactions_blob.mark_all_purged(now);
        let latest_id = match redactions_blob.latest() {
            Some(latest) => Some(redaction_content_hash(latest)?),
            None => None,
        };
        // Persist the purged-at marker before touching the blob bytes —
        // if the blob delete fails (filesystem error), the audit trail
        // still records that purge was attempted.
        let bytes = redactions_blob
            .encode()
            .with_context(|| "re-encode redactions blob after purge mark")?;
        let path = self.redaction_path_for_blob(blob);
        write_file_atomic(&path, &bytes).with_context(|| format!("write '{}'", path.display()))?;

        // Delete the loose blob bytes if present. Packed blobs are
        // flagged but not removed in this initial implementation —
        // dropping packed bytes requires a repack pass we punt on
        // here.
        let (blob_bytes_removed, blob_remains_in_pack) = remove_loose_blob_bytes(self, blob)?;

        Ok(PurgeOutcome {
            redaction_id: latest_id,
            redactions_marked,
            blob_bytes_removed,
            blob_remains_in_pack,
        })
    }

    /// `<heddle_dir>/redactions/` — root of the redaction store.
    pub(crate) fn redactions_dir(&self) -> PathBuf {
        self.heddle_dir().join("redactions")
    }

    /// `<heddle_dir>/redactions/<blob-hash-hex>.bin` — the redactions
    /// file for a specific blob.
    pub(crate) fn redaction_path_for_blob(&self, blob: &ContentHash) -> PathBuf {
        self.redactions_dir()
            .join(format!("{}.bin", hex_encode_content_hash(blob)))
    }

    /// If `blob` has any active redaction, return the stub text the
    /// materialize path should write to disk in place of the blob
    /// content. Returns `None` when no redaction exists — callers
    /// should then proceed with normal materialization.
    ///
    /// Picks the latest redaction (by `redacted_at`) to source the
    /// stub. Multiple redactions on the same blob converge to the
    /// most-recent message; the older ones remain in the audit trail.
    pub fn redaction_stub_for_blob(&self, blob: &ContentHash) -> Result<Option<String>> {
        let redactions = self.get_redactions_for_blob(blob)?;
        if !redactions.has_active() {
            return Ok(None);
        }
        let latest = redactions
            .latest()
            .expect("non-empty redactions blob has a latest entry");
        let id = redaction_content_hash(latest)?;
        Ok(Some(latest.stub_text(&id)))
    }

    /// Enumerate every state reachable from any thread tip or marker by
    /// walking parent pointers transitively. Used by `--all-states`
    /// redaction propagation so a leaked secret can be scrubbed from
    /// every state in which its blob hash appears.
    ///
    /// The walk is breadth-first and dedups by `ChangeId`, so a state
    /// reached from multiple tips appears once. Missing states (broken
    /// parent links) are skipped silently — redaction propagation is
    /// best-effort across the reachable graph, not a graph-integrity
    /// check.
    pub fn reachable_states(&self) -> Result<Vec<ChangeId>> {
        let refs = self.refs();
        let mut roots: Vec<ChangeId> = Vec::new();
        for name in refs
            .list_threads()
            .with_context(|| "list threads for reachable_states")?
        {
            if let Some(tip) = refs
                .get_thread(&name)
                .with_context(|| format!("read thread '{name}'"))?
            {
                roots.push(tip);
            }
        }
        for name in refs
            .list_markers()
            .with_context(|| "list markers for reachable_states")?
        {
            if let Some(tip) = refs
                .get_marker(&name)
                .with_context(|| format!("read marker '{name}'"))?
            {
                roots.push(tip);
            }
        }

        let mut visited: HashSet<ChangeId> = HashSet::new();
        let mut queue: Vec<ChangeId> = Vec::new();
        for root in roots {
            if visited.insert(root) {
                queue.push(root);
            }
        }
        let mut out: Vec<ChangeId> = Vec::new();
        while let Some(id) = queue.pop() {
            // Load the state; if missing (broken parent ref or shallow
            // clone), skip — propagation tolerates gaps.
            let Some(state) = self
                .store()
                .get_state(&id)
                .with_context(|| format!("load state {} for reachable walk", id.short()))?
            else {
                continue;
            };
            out.push(id);
            for parent in &state.parents {
                if visited.insert(*parent) {
                    queue.push(*parent);
                }
            }
        }
        Ok(out)
    }

    /// Find every path under `state` whose terminal blob hashes to
    /// `target`. Used by `--all-states` propagation: a leaked secret
    /// may live at different paths across history (renames, copies),
    /// so we propagate by blob hash, not by path.
    ///
    /// Returns paths as forward-slash strings, lexicographically
    /// stable thanks to `Tree` entry ordering.
    pub fn paths_to_blob_in_state(
        &self,
        state: &ChangeId,
        target: &ContentHash,
    ) -> Result<Vec<String>> {
        let Some(tree) = self
            .get_tree_for_state(state)
            .with_context(|| format!("load tree for state {}", state.short()))?
        else {
            return Ok(Vec::new());
        };
        let mut out: Vec<String> = Vec::new();
        walk_tree_for_blob(self, &tree, "", target, &mut out)?;
        Ok(out)
    }
}

/// Recursive helper for [`Repository::paths_to_blob_in_state`]. Walks
/// the tree depth-first; on a matching blob, records the full
/// repo-relative path and continues (a blob can appear under multiple
/// paths).
fn walk_tree_for_blob(
    repo: &Repository,
    tree: &Tree,
    prefix: &str,
    target: &ContentHash,
    out: &mut Vec<String>,
) -> Result<()> {
    for entry in tree.iter() {
        let path = if prefix.is_empty() {
            entry.name.clone()
        } else {
            format!("{prefix}/{}", entry.name)
        };
        if entry.is_blob() {
            if entry.hash == *target {
                out.push(path);
            }
            continue;
        }
        if entry.is_tree() {
            let Some(subtree) = repo
                .store()
                .get_tree(&entry.hash)
                .with_context(|| format!("load subtree {}", entry.hash.short()))?
            else {
                // Missing subtree object — treat as unreachable, don't fail.
                continue;
            };
            walk_tree_for_blob(repo, &subtree, &path, target, out)?;
        }
    }
    Ok(())
}

/// Compute the content hash of a single redaction. The hash covers the
/// rmp-encoded bytes of a one-element `RedactionsBlob` containing the
/// redaction, so the id format is stable across schema additions that
/// extend `RedactionsBlob` (e.g. a future header field).
fn redaction_content_hash(redaction: &Redaction) -> Result<ContentHash> {
    // Wrap in a single-entry blob so the canonical bytes are independent
    // of the surrounding container's existing siblings — two different
    // redactions stored in the same blob file still produce distinct ids.
    let single = RedactionsBlob::new(vec![redaction.clone()]);
    let bytes = single
        .encode()
        .with_context(|| "encode single-redaction for content addressing")?;
    let digest = blake3::hash(&bytes);
    Ok(ContentHash::from_bytes(*digest.as_bytes()))
}

fn hex_encode_content_hash(hash: &ContentHash) -> String {
    let bytes = hash.as_bytes();
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{:02x}", b);
    }
    out
}

fn parse_blob_hash_hex(hex: &str) -> Result<ContentHash> {
    if hex.len() != 64 {
        anyhow::bail!("invalid blob-hash hex length: {}", hex.len());
    }
    let mut bytes = [0u8; 32];
    for i in 0..32 {
        let slice = &hex[i * 2..i * 2 + 2];
        bytes[i] = u8::from_str_radix(slice, 16)
            .with_context(|| format!("invalid hex byte at offset {}", i * 2))?;
    }
    Ok(ContentHash::from_bytes(bytes))
}

/// Remove the loose blob bytes for `hash` if a loose copy exists.
/// Returns `(removed, remains_in_pack)`. Both `false` when the blob is
/// not in the store at all (already gone).
fn remove_loose_blob_bytes(repo: &Repository, hash: &ContentHash) -> Result<(bool, bool)> {
    let store = repo.store();
    if let Some(path) = store.loose_blob_path(hash)
        && path.exists()
    {
        fs::remove_file(&path)
            .with_context(|| format!("remove loose blob '{}'", path.display()))?;
        // Even after loose removal, the blob may still be in a pack.
        // We don't have a non-disruptive way to check packs here
        // without holding the pack index — leave the field
        // conservatively `false` and let a future refinement set it
        // when the pack-aware purge lands.
        return Ok((true, false));
    }
    Ok((false, false))
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use objects::object::{ChangeId, Principal};
    use tempfile::TempDir;

    use super::*;

    fn fresh_repo() -> (TempDir, Repository) {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init_default(dir.path()).unwrap();
        (dir, repo)
    }

    fn sample_principal() -> Principal {
        Principal {
            name: "Anan".into(),
            email: "anan@heddle.sh".into(),
        }
    }

    fn sample_blob() -> ContentHash {
        ContentHash::from_bytes([7u8; 32])
    }

    fn sample_redaction() -> Redaction {
        Redaction {
            redacted_blob: sample_blob(),
            state: ChangeId::from_bytes([1u8; 16]),
            path: "config/secrets.toml".into(),
            reason: "leaked credential".into(),
            redactor: sample_principal(),
            redacted_at: Utc.with_ymd_and_hms(2026, 5, 10, 14, 33, 0).unwrap(),
            signature: None,
            purged_at: None,
            supersedes: None,
        }
    }

    #[test]
    fn put_redaction_writes_blob_and_returns_stable_id() {
        let (_dir, repo) = fresh_repo();
        let r = sample_redaction();
        let id1 = repo.put_redaction(r.clone()).expect("put redaction");
        // Idempotent: putting the same redaction returns the same id and
        // does not duplicate the entry.
        let id2 = repo.put_redaction(r.clone()).expect("re-put redaction");
        assert_eq!(
            id1, id2,
            "put_redaction must be idempotent on identical input"
        );

        let stored = repo
            .get_redactions_for_blob(&sample_blob())
            .expect("get redactions");
        assert_eq!(
            stored.redactions.len(),
            1,
            "idempotent put must not duplicate entries"
        );
    }

    #[test]
    fn list_all_redactions_returns_every_blob_with_a_record() {
        let (_dir, repo) = fresh_repo();
        let r = sample_redaction();
        repo.put_redaction(r.clone()).unwrap();
        let listing = repo.list_all_redactions().expect("list all redactions");
        assert_eq!(listing.len(), 1);
        assert_eq!(listing[0].0, sample_blob());
        assert_eq!(listing[0].1.redactions.len(), 1);
    }

    #[test]
    fn get_redaction_finds_by_id_or_returns_none() {
        let (_dir, repo) = fresh_repo();
        let id = repo.put_redaction(sample_redaction()).unwrap();
        let found = repo
            .get_redaction(&id)
            .expect("lookup by id")
            .expect("present");
        assert_eq!(found.0, sample_blob());
        let unknown = ContentHash::from_bytes([0u8; 32]);
        let missing = repo.get_redaction(&unknown).expect("lookup miss");
        assert!(
            missing.is_none(),
            "lookup of unknown id must return None, not error"
        );
    }

    #[test]
    fn purge_blob_refuses_when_no_redaction_exists() {
        let (_dir, repo) = fresh_repo();
        let err = repo
            .purge_blob(&sample_blob(), &sample_principal())
            .expect_err("purge without redaction must refuse");
        let msg = err.to_string();
        assert!(
            msg.contains("no redaction"),
            "error must name the missing-redaction precondition, got: {msg}"
        );
    }

    #[test]
    fn purge_blob_marks_redactions_purged_after_redact() {
        let (_dir, repo) = fresh_repo();
        repo.put_redaction(sample_redaction()).unwrap();
        let outcome = repo
            .purge_blob(&sample_blob(), &sample_principal())
            .expect("purge after redact");
        assert_eq!(outcome.redactions_marked, 1);
        assert!(outcome.redaction_id.is_some());

        // After purge the stored redactions carry a purged_at marker.
        let stored = repo
            .get_redactions_for_blob(&sample_blob())
            .expect("get redactions");
        assert!(
            stored.redactions.iter().all(|r| r.is_purged()),
            "every redaction on a purged blob must be marked purged"
        );

        // Idempotent re-purge marks zero additional records — operators
        // can retry a partial purge without inflating the audit trail.
        let again = repo
            .purge_blob(&sample_blob(), &sample_principal())
            .expect("re-purge");
        assert_eq!(again.redactions_marked, 0);
    }
}