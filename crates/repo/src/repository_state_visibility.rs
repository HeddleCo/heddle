// SPDX-License-Identifier: Apache-2.0
//! Repository helpers for the per-state visibility sidecar.
//!
//! Storage layout (one file per state that carries a non-public tier):
//!
//! ```text
//! <heddle_dir>/visibility/<change-id>.bin
//! ```
//!
//! The file is an rmp-serde-encoded [`StateVisibilityBlob`] — every
//! visibility declaration on the same state lives in the same file, keyed by
//! the state's `ChangeId`. This mirrors the per-blob redactions sidecar
//! (`crates/repo/src/repository_redaction.rs`), one level up: redaction is
//! keyed by a blob hash, commit visibility by a state id.
//!
//! ## Absence ≡ public
//!
//! The public tier is the default and stays **record-free**: a public
//! resolution never writes a file here, and [`Repository::has_visibility_for_state`]
//! returns `false` when no record exists. Only resolutions more restrictive
//! than public are persisted. Callers must therefore not persist a
//! `VisibilityTier::Public` record — the absence *is* the public signal.

use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use objects::{
    fs_atomic::write_file_atomic,
    object::{ChangeId, ContentHash, StateVisibility, StateVisibilityBlob},
};

use crate::repository::Repository;

impl Repository {
    /// Append a visibility record for its state. Returns the record's
    /// content-addressed id.
    ///
    /// Idempotent: if a record with the same canonical bytes already exists
    /// on the state, no second entry is written and the existing id is
    /// returned.
    ///
    /// Callers must not pass a `VisibilityTier::Public` record — public is
    /// represented by *absence* (see the module docs). Persisting a public
    /// record would make [`has_visibility_for_state`](Self::has_visibility_for_state)
    /// report a state as non-public when it is not.
    pub fn put_state_visibility(&self, record: StateVisibility) -> Result<ContentHash> {
        let state = record.state;
        let mut existing = self.get_state_visibility_for_state(&state)?;

        let id = state_visibility_content_hash(&record)?;
        for existing_record in &existing.records {
            if state_visibility_content_hash(existing_record)? == id {
                return Ok(id);
            }
        }

        existing.push(record);
        let bytes = existing
            .encode()
            .with_context(|| "encoding state-visibility blob")?;
        let path = self.state_visibility_path_for_state(&state);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create '{}'", parent.display()))?;
        }
        write_file_atomic(&path, &bytes).with_context(|| format!("write '{}'", path.display()))?;
        Ok(id)
    }

    /// Load all visibility records targeting `state`. Returns an empty
    /// [`StateVisibilityBlob`] (not an error) when none exist — callers can
    /// treat the result uniformly.
    pub fn get_state_visibility_for_state(&self, state: &ChangeId) -> Result<StateVisibilityBlob> {
        let path = self.state_visibility_path_for_state(state);
        if !path.exists() {
            return Ok(StateVisibilityBlob::empty());
        }
        let bytes = fs::read(&path).with_context(|| format!("read '{}'", path.display()))?;
        StateVisibilityBlob::decode(&bytes)
            .with_context(|| format!("decode '{}'", path.display()))
    }

    /// Whether `state` carries any persisted visibility record. `false`
    /// means **public-by-absence** — the public resolution is record-free,
    /// so a missing (or empty) sidecar resolves to the public tier. This is
    /// the keystone query the serve-side gate keys off.
    pub fn has_visibility_for_state(&self, state: &ChangeId) -> Result<bool> {
        Ok(self.get_state_visibility_for_state(state)?.has_record())
    }

    /// Walk every visibility sidecar file in the repo. Returns
    /// `(state_id, blob)` pairs so callers can correlate. Used by listing
    /// surfaces and the GC's "never collect a visibility record" guard.
    pub fn list_all_state_visibility(&self) -> Result<Vec<(ChangeId, StateVisibilityBlob)>> {
        let dir = self.state_visibility_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in fs::read_dir(&dir).with_context(|| format!("read '{}'", dir.display()))? {
            let entry = entry.with_context(|| format!("entry in '{}'", dir.display()))?;
            let path = entry.path();
            // Only `.bin` files whose stem parses as a ChangeId. Editor
            // backups and partial writes are skipped, never fatal.
            if path.extension().and_then(|e| e.to_str()) != Some("bin") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Ok(state) = ChangeId::parse(stem) else {
                continue;
            };
            let bytes = fs::read(&path).with_context(|| format!("read '{}'", path.display()))?;
            let blob = StateVisibilityBlob::decode(&bytes)
                .with_context(|| format!("decode '{}'", path.display()))?;
            out.push((state, blob));
        }
        Ok(out)
    }

    /// `<heddle_dir>/visibility/` — root of the per-state visibility store.
    pub(crate) fn state_visibility_dir(&self) -> PathBuf {
        self.heddle_dir().join("visibility")
    }

    /// `<heddle_dir>/visibility/<change-id>.bin` — the visibility sidecar
    /// for a specific state.
    pub(crate) fn state_visibility_path_for_state(&self, state: &ChangeId) -> PathBuf {
        self.state_visibility_dir()
            .join(format!("{}.bin", state.to_string_full()))
    }
}

/// Content hash of a single visibility record. The hash covers the
/// rmp-encoded bytes of a one-element [`StateVisibilityBlob`], so the id
/// format is stable across schema additions that extend the container.
fn state_visibility_content_hash(record: &StateVisibility) -> Result<ContentHash> {
    let single = StateVisibilityBlob::new(vec![record.clone()]);
    let bytes = single
        .encode()
        .with_context(|| "encode single state-visibility for content addressing")?;
    let digest = blake3::hash(&bytes);
    Ok(ContentHash::from_bytes(*digest.as_bytes()))
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use objects::object::{Principal, VisibilityTier};
    use tempfile::TempDir;

    use super::*;

    fn fresh_repo() -> (TempDir, Repository) {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init_default(dir.path()).unwrap();
        (dir, repo)
    }

    fn sample_record(state: ChangeId, tier: VisibilityTier) -> StateVisibility {
        StateVisibility {
            state,
            tier,
            embargo_until: None,
            declarer: Principal {
                name: "Grace Hopper".into(),
                email: "grace@example.com".into(),
            },
            declared_at: Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap(),
            signature: None,
            supersedes: None,
        }
    }

    #[test]
    fn put_then_read_back_and_has_visibility_true() {
        let (_dir, repo) = fresh_repo();
        let state = ChangeId::from_bytes([5u8; 16]);
        let record = sample_record(
            state,
            VisibilityTier::Restricted {
                scope_label: "security-embargo".into(),
            },
        );
        repo.put_state_visibility(record.clone())
            .expect("put visibility");

        let stored = repo
            .get_state_visibility_for_state(&state)
            .expect("read back");
        assert_eq!(stored.records.len(), 1);
        assert_eq!(stored.records[0], record);
        assert!(
            repo.has_visibility_for_state(&state)
                .expect("has visibility"),
            "a state with a persisted record must report has_visibility_for_state == true"
        );
    }

    #[test]
    fn absence_is_public_has_visibility_false() {
        // The keystone: a state with no record resolves to public, so the
        // query returns false. No file is written for the public tier.
        let (_dir, repo) = fresh_repo();
        let with_record = ChangeId::from_bytes([1u8; 16]);
        repo.put_state_visibility(sample_record(with_record, VisibilityTier::Internal))
            .expect("put visibility");

        let no_record = ChangeId::from_bytes([2u8; 16]);
        assert!(
            !repo
                .has_visibility_for_state(&no_record)
                .expect("has visibility on record-free state"),
            "a state with no record must be public-by-absence (has_visibility_for_state == false)"
        );
        // And its sidecar load is an empty blob, never an error.
        assert!(
            repo.get_state_visibility_for_state(&no_record)
                .expect("read record-free state")
                .records
                .is_empty()
        );
    }

    #[test]
    fn put_is_idempotent_on_identical_record() {
        let (_dir, repo) = fresh_repo();
        let state = ChangeId::from_bytes([7u8; 16]);
        let record = sample_record(state, VisibilityTier::Internal);
        let id1 = repo.put_state_visibility(record.clone()).expect("put");
        let id2 = repo.put_state_visibility(record).expect("re-put");
        assert_eq!(id1, id2, "identical record must return the same id");

        let stored = repo
            .get_state_visibility_for_state(&state)
            .expect("read back");
        assert_eq!(
            stored.records.len(),
            1,
            "idempotent put must not duplicate the record"
        );
    }

    #[test]
    fn distinct_records_on_same_state_accrete() {
        // A promotion appends a superseding record; both live in the same
        // per-state sidecar file.
        let (_dir, repo) = fresh_repo();
        let state = ChangeId::from_bytes([8u8; 16]);
        let first = sample_record(
            state,
            VisibilityTier::Restricted {
                scope_label: "embargo".into(),
            },
        );
        let first_id = repo.put_state_visibility(first).expect("put first");
        let second = StateVisibility {
            tier: VisibilityTier::Internal,
            declared_at: Utc.with_ymd_and_hms(2026, 6, 2, 9, 0, 0).unwrap(),
            supersedes: Some(first_id),
            ..sample_record(state, VisibilityTier::Internal)
        };
        repo.put_state_visibility(second).expect("put second");

        let stored = repo
            .get_state_visibility_for_state(&state)
            .expect("read back");
        assert_eq!(stored.records.len(), 2);
        // `latest` resolves the effective tier by declared_at.
        assert_eq!(stored.latest().unwrap().tier, VisibilityTier::Internal);
    }

    #[test]
    fn list_all_returns_every_state_with_a_record() {
        let (_dir, repo) = fresh_repo();
        let a = ChangeId::from_bytes([10u8; 16]);
        let b = ChangeId::from_bytes([11u8; 16]);
        repo.put_state_visibility(sample_record(a, VisibilityTier::Internal))
            .unwrap();
        repo.put_state_visibility(sample_record(
            b,
            VisibilityTier::TeamScoped {
                team_id: "infra".into(),
            },
        ))
        .unwrap();

        let mut listed: Vec<ChangeId> = repo
            .list_all_state_visibility()
            .expect("list all")
            .into_iter()
            .map(|(state, _)| state)
            .collect();
        listed.sort_by_key(|c| c.to_string_full());
        let mut want = vec![a, b];
        want.sort_by_key(|c| c.to_string_full());
        assert_eq!(listed, want);
    }
}
