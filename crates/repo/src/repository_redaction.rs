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
use crypto::verify_payload_signature;
use objects::{
    fs_atomic::write_file_atomic,
    object::{ChangeId, ContentHash, Principal, Redaction, RedactionsBlob, Tree},
};

use crate::repository::Repository;

/// Why a wire-side redaction was rejected. Surfaces in sync error
/// messages so operators can diagnose the bad record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireRejection {
    /// Cross-replica redactions must be signed. An unsigned record is
    /// refused so secrets can't be propagated by an attacker who's
    /// merely on the wire (without an authoring identity).
    Unsigned,
    /// The record carries a signature, but it doesn't verify against
    /// the canonical signing payload. Tampering or wrong key.
    Tampered,
    /// The signature verifies, but the public key isn't on this
    /// receiver's `[redact] trusted_keys` list. Signing proves *who*
    /// declared the redaction; the trust list proves the receiver
    /// has *authorized* that operator to act on this workspace.
    /// Without this gate an attacker can mint and sign their own
    /// redaction and pass verification trivially.
    UntrustedKey {
        algorithm: String,
        public_key: String,
    },
}

/// Outcome of an `accept_wire_redactions` call. Receiver-side sync
/// handlers surface this in their stats summary so operators see
/// exactly what propagation did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WireAcceptOutcome {
    /// Number of new redaction records added to the local store.
    /// Idempotent re-pulls of the same record count `0`.
    pub redactions_added: usize,
    /// Number of blobs whose local bytes were purged because an
    /// incoming redaction carried `purged_at: Some(_)`.
    pub blobs_purged: usize,
    /// Number of incoming redactions that were byte-identical to a
    /// local record and skipped (idempotency path).
    pub skipped_existing: usize,
}

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

    /// Accept a wire-transferred `RedactionsBlob` for a specific blob
    /// hash. Verifies every signature, refuses unsigned records, then
    /// merges new records into the local sidecar (idempotent on
    /// content-addressed duplicates). If any incoming record carries
    /// `purged_at: Some(_)`, the local blob bytes are dropped via
    /// `purge_blob`.
    ///
    /// `bytes` is the rmp-encoded `RedactionsBlob` payload that arrived
    /// over the wire; it must decode and every contained `Redaction`
    /// must match `blob` in its `redacted_blob` field.
    ///
    /// Errors:
    /// - [`WireRejection::Unsigned`] if any incoming redaction has no
    ///   signature. The whole blob is refused (atomic — partial accept
    ///   would let an unsigned sibling smuggle in via a signed peer).
    /// - [`WireRejection::Tampered`] if any signature fails to verify
    ///   against the canonical payload.
    /// - [`WireRejection::UntrustedKey`] if the signer's public key is
    ///   not on this receiver's `[redact] trusted_keys` list. The list
    ///   is fail-closed: an empty list rejects every incoming signed
    ///   redaction, forcing operators to make an explicit trust
    ///   decision before secrets-scrubbing primitives are accepted.
    /// - Other errors propagate as `anyhow::Error` (decode failure, io,
    ///   crypto subsystem failure).
    pub fn accept_wire_redactions(
        &self,
        blob: ContentHash,
        bytes: &[u8],
    ) -> Result<WireAcceptOutcome> {
        let incoming = RedactionsBlob::decode(bytes)
            .with_context(|| format!("decode incoming redactions for blob {}", blob.short()))?;

        // Snapshot the trust list once. Cheap to clone (operator key
        // counts are O(individuals), not O(blobs)).
        let trusted_keys = self.config().redact.trusted_keys.clone();

        // Pre-validate every entry before we touch the local store —
        // an all-or-nothing accept keeps the audit trail honest.
        for redaction in &incoming.redactions {
            if redaction.redacted_blob != blob {
                anyhow::bail!(
                    "incoming redaction claims blob {} but was transferred under {}",
                    redaction.redacted_blob.short(),
                    blob.short()
                );
            }
            verify_wire_redaction(redaction, &trusted_keys)
                .with_context(|| format!("verify incoming redaction for blob {}", blob.short()))?;
        }

        let mut outcome = WireAcceptOutcome::default();
        let mut any_purged = false;

        for redaction in incoming.redactions {
            let was_purged = redaction.is_purged();
            let id_before = redaction_content_hash(&redaction)?;
            // Snapshot the existing record set so we can tell whether
            // `put_redaction` was a no-op (idempotent re-pull).
            let existing_count = self
                .get_redactions_for_blob(&blob)
                .map(|r| r.redactions.len())
                .unwrap_or(0);
            let id_after = self.put_redaction(redaction)?;
            debug_assert_eq!(
                id_before, id_after,
                "put_redaction must preserve content-addressed id"
            );
            let new_count = self
                .get_redactions_for_blob(&blob)
                .map(|r| r.redactions.len())
                .unwrap_or(0);
            if new_count > existing_count {
                outcome.redactions_added += 1;
            } else {
                outcome.skipped_existing += 1;
            }
            if was_purged {
                any_purged = true;
            }
        }

        if any_purged {
            // The incoming record asserts the bytes should be gone.
            // Replay locally — `purge_blob` is idempotent and refuses
            // only when no redaction exists, which we just guaranteed.
            //
            // `_purger` is the on-wire principal of the *redactor*;
            // since the redaction's identity is carried in the record
            // itself, use that — receiver doesn't have its own
            // operator context here.
            let purger = Principal {
                name: "<wire-replay>".to_string(),
                email: "".to_string(),
            };
            let purge_outcome = self.purge_blob(&blob, &purger)?;
            if purge_outcome.blob_bytes_removed {
                outcome.blobs_purged += 1;
            }
        }

        Ok(outcome)
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

/// Wire-side signature gate. Returns `Ok(())` for a verified,
/// trusted redaction. Rejection variants:
///
/// - [`WireRejection::Unsigned`] when no signature is present.
/// - [`WireRejection::UntrustedKey`] when the signer's public key is
///   not on the trust list. Checked *before* `verify_payload_signature`
///   so an attacker can't probe the trust list via a timing oracle —
///   the cryptographic cost is paid only for keys we trust.
/// - [`WireRejection::Tampered`] when verification fails.
///
/// `trusted_keys` is the snapshot from `RepoConfig::redact::trusted_keys`.
/// An empty list rejects every signed redaction (fail-closed).
fn verify_wire_redaction(redaction: &Redaction, trusted_keys: &[crate::TrustedKey]) -> Result<()> {
    let Some(signature) = &redaction.signature else {
        anyhow::bail!(WireRejection::Unsigned);
    };
    if !key_is_trusted(trusted_keys, &signature.algorithm, &signature.public_key) {
        anyhow::bail!(WireRejection::UntrustedKey {
            algorithm: signature.algorithm.clone(),
            public_key: signature.public_key.clone(),
        });
    }
    let payload = redaction.canonical_signing_payload();
    let public_key = hex::decode(&signature.public_key)
        .with_context(|| "decode incoming redaction signature public key")?;
    let sig_bytes = hex::decode(&signature.signature)
        .with_context(|| "decode incoming redaction signature bytes")?;
    if verify_payload_signature(&payload, &signature.algorithm, &public_key, &sig_bytes).is_err() {
        anyhow::bail!(WireRejection::Tampered);
    }
    Ok(())
}

/// Whether `(algorithm, public_key)` is in the configured trust list.
/// Hex comparison is case-insensitive to match the storage format
/// produced by `hex::encode` (lowercase) without surprising operators
/// who copy-paste keys that happen to be uppercase.
fn key_is_trusted(trusted: &[crate::TrustedKey], algorithm: &str, public_key_hex: &str) -> bool {
    trusted.iter().any(|t| {
        t.algorithm.eq_ignore_ascii_case(algorithm)
            && t.public_key.eq_ignore_ascii_case(public_key_hex)
    })
}

impl std::fmt::Display for WireRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireRejection::Unsigned => f.write_str(
                "redaction has no signature — cross-replica redactions must be \
                 signed with `--sign-with` to propagate",
            ),
            WireRejection::Tampered => f.write_str(
                "redaction signature failed to verify — the canonical payload \
                 was modified after signing or the wrong key was used",
            ),
            WireRejection::UntrustedKey {
                algorithm,
                public_key,
            } => write!(
                f,
                "redaction signed by an untrusted operator key ({algorithm}:{public_key}) — \
                 add the key to `[redact] trusted_keys` in `.heddle/config.toml` to accept it",
            ),
        }
    }
}

impl std::error::Error for WireRejection {}

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
    use crypto::Signer;
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

    fn signed_sample_redaction(signer: &dyn crypto::Signer) -> Redaction {
        let mut r = sample_redaction();
        let payload = r.canonical_signing_payload();
        let sig = signer.sign(&payload).expect("sign payload");
        r.signature = Some(objects::object::StateSignature {
            algorithm: signer.algorithm().to_string(),
            public_key: hex::encode(signer.public_key()),
            signature: hex::encode(&sig),
        });
        r
    }

    /// Initialize a repo and add `signer`'s public key to its
    /// `[redact] trusted_keys` list. The signed-path acceptance tests
    /// use this so the receiver actually trusts the signer (the
    /// fail-closed default rejects every key).
    ///
    /// Round-trips the config through toml::Value rather than
    /// string-appending a `[redact]` section: the default emit may or
    /// may not already include `[redact]` (depending on
    /// serde-skip-empty behaviour), and appending a duplicate header
    /// is a TOML parse error.
    fn fresh_repo_trusting(signer: &dyn crypto::Signer) -> (TempDir, Repository) {
        let (dir, _) = fresh_repo();
        let config_path = dir.path().join(".heddle/config.toml");
        let raw = std::fs::read_to_string(&config_path).expect("read default config");
        let mut value: toml::Value = toml::from_str(&raw).expect("parse default config");
        let entry: toml::Value = toml::Value::try_from(crate::TrustedKey {
            algorithm: signer.algorithm().to_string(),
            public_key: hex::encode(signer.public_key()),
            label: Some("test-fixture".to_string()),
        })
        .expect("encode trusted key");
        let table = value
            .as_table_mut()
            .expect("config root must be a TOML table");
        let redact = table
            .entry("redact".to_string())
            .or_insert_with(|| toml::Value::Table(Default::default()))
            .as_table_mut()
            .expect("[redact] must be a table");
        redact.insert("trusted_keys".to_string(), toml::Value::Array(vec![entry]));
        let serialized = toml::to_string(&value).expect("serialize patched config");
        std::fs::write(&config_path, serialized).expect("write config");
        let reopened = Repository::open(dir.path()).expect("re-open repo");
        (dir, reopened)
    }

    #[test]
    fn accept_wire_redactions_refuses_unsigned() {
        let (_dir, repo) = fresh_repo();
        let unsigned = sample_redaction();
        let payload = RedactionsBlob::new(vec![unsigned]).encode().unwrap();
        let err = repo
            .accept_wire_redactions(sample_blob(), &payload)
            .expect_err("unsigned redaction must be refused");
        let chain: Vec<String> = err.chain().map(|e| e.to_string()).collect();
        assert!(
            chain.iter().any(|m| m.contains("no signature")),
            "rejection reason must explain unsigned, got chain: {chain:?}"
        );
        // Local store must remain empty — refusal is atomic.
        let stored = repo.get_redactions_for_blob(&sample_blob()).unwrap();
        assert!(stored.redactions.is_empty());
    }

    #[test]
    fn accept_wire_redactions_refuses_untrusted_signer_even_with_valid_signature() {
        // The codex-flagged spoof vector: an attacker mints a redaction,
        // signs it with their own key, and ships it. Signature
        // verification by itself passes (the math is fine — the key
        // matches the signature) but the receiver MUST reject because
        // the key isn't on the trust list. Fail-closed default: an
        // empty trust list rejects every signed key.
        let (_dir, repo) = fresh_repo();
        let attacker = crypto::Ed25519Signer::generate().expect("keygen");
        let forged = signed_sample_redaction(&attacker);
        let payload = RedactionsBlob::new(vec![forged]).encode().unwrap();
        let err = repo
            .accept_wire_redactions(sample_blob(), &payload)
            .expect_err("untrusted signer must be refused");
        let chain: Vec<String> = err.chain().map(|e| e.to_string()).collect();
        assert!(
            chain.iter().any(|m| m.contains("untrusted operator key")),
            "rejection reason must explain untrusted-key, got chain: {chain:?}"
        );
        // Local store must remain empty — refusal is atomic.
        let stored = repo.get_redactions_for_blob(&sample_blob()).unwrap();
        assert!(stored.redactions.is_empty());
    }

    #[test]
    fn accept_wire_redactions_persists_signed_redaction_idempotently() {
        let signer = crypto::Ed25519Signer::generate().expect("keygen");
        let (_dir, repo) = fresh_repo_trusting(&signer);
        let signed = signed_sample_redaction(&signer);
        let payload = RedactionsBlob::new(vec![signed]).encode().unwrap();

        let first = repo
            .accept_wire_redactions(sample_blob(), &payload)
            .expect("first accept");
        assert_eq!(first.redactions_added, 1);
        assert_eq!(first.skipped_existing, 0);
        assert_eq!(first.blobs_purged, 0);

        let second = repo
            .accept_wire_redactions(sample_blob(), &payload)
            .expect("second accept idempotent");
        assert_eq!(second.redactions_added, 0);
        assert_eq!(second.skipped_existing, 1);
    }

    #[test]
    fn accept_wire_redactions_rejects_tampered_signature() {
        let signer = crypto::Ed25519Signer::generate().expect("keygen");
        let (_dir, repo) = fresh_repo_trusting(&signer);
        let mut tampered = signed_sample_redaction(&signer);
        // Mutate the reason AFTER signing — the canonical payload
        // changes but the signature does not, so verification fails.
        tampered.reason = "post-signing tampered reason".to_string();
        let payload = RedactionsBlob::new(vec![tampered]).encode().unwrap();
        let err = repo
            .accept_wire_redactions(sample_blob(), &payload)
            .expect_err("tampered signature must be refused");
        let chain: Vec<String> = err.chain().map(|e| e.to_string()).collect();
        assert!(
            chain.iter().any(|m| m.contains("failed to verify")),
            "rejection reason must explain tamper, got chain: {chain:?}"
        );
    }

    #[test]
    fn accept_wire_redactions_with_purged_at_drives_local_purge() {
        let signer = crypto::Ed25519Signer::generate().expect("keygen");
        let (_dir, repo) = fresh_repo_trusting(&signer);
        let mut signed = signed_sample_redaction(&signer);
        // Sender purged before propagation: mark the record purged.
        signed.purged_at = Some(Utc.with_ymd_and_hms(2026, 5, 12, 9, 0, 0).unwrap());
        // Re-sign over the new canonical payload (purged_at is excluded
        // per the signing contract, so no actual change in signature
        // bytes — but be precise about this).
        let payload_bytes = signed.canonical_signing_payload();
        let sig = signer.sign(&payload_bytes).unwrap();
        signed.signature = Some(objects::object::StateSignature {
            algorithm: signer.algorithm().to_string(),
            public_key: hex::encode(signer.public_key()),
            signature: hex::encode(&sig),
        });
        let wire = RedactionsBlob::new(vec![signed]).encode().unwrap();

        let outcome = repo
            .accept_wire_redactions(sample_blob(), &wire)
            .expect("accept wire purge");
        assert_eq!(outcome.redactions_added, 1);
        // Local store records the purge.
        let stored = repo.get_redactions_for_blob(&sample_blob()).unwrap();
        assert!(
            stored.redactions.iter().all(|r| r.is_purged()),
            "redaction must be persisted with purged_at"
        );
    }
}
