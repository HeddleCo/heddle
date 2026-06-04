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
//! returns `false` when the effective tier is public. Only resolutions more
//! restrictive than public are persisted. This is *enforced* at the write
//! boundary — [`Repository::put_state_visibility`] normalizes a
//! `VisibilityTier::Public` put to public-by-absence rather than trusting
//! callers to keep public off disk — so the absence genuinely *is* the
//! public signal.

use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use objects::{
    fs_atomic::write_file_atomic,
    lock::RepositoryLockExt,
    object::{ChangeId, ContentHash, StateVisibility, StateVisibilityBlob, VisibilityTier},
};

use crate::repository::Repository;

impl Repository {
    /// Record a visibility declaration for its state. Returns the record's
    /// content-addressed id.
    ///
    /// The whole read → dedupe → write sequence runs under the repository
    /// write lock, so two concurrent puts on the same state can't both load
    /// the same blob and have the second `write_file_atomic` clobber the
    /// first (a lost update would silently drop an embargo or a promotion).
    /// This mirrors the `SignState` handler, which serializes its per-state
    /// read-modify-write the same way — reusing the existing repo lock, not
    /// a new one.
    ///
    /// Idempotent: if a record with the same canonical bytes already exists
    /// on the state, no second entry is written and the existing id is
    /// returned.
    ///
    /// **Absence ≡ public, enforced here.** When the *effective* (latest)
    /// tier resolves to [`VisibilityTier::Public`], the state's sidecar is
    /// removed so it returns to public-by-absence and
    /// [`has_visibility_for_state`](Self::has_visibility_for_state) reports
    /// `false`. This holds whether the Public put is fresh or supersedes a
    /// prior private record — no caller can leave a Public record that makes
    /// a public state read as non-public.
    pub fn put_state_visibility(&self, record: StateVisibility) -> Result<ContentHash> {
        record
            .validate()
            .with_context(|| "validate state-visibility record before put")?;
        let state = record.state;

        // Serialize the full read-modify-write behind the repo write lock so
        // concurrent appends on the same state can't clobber each other.
        let _lock = self
            .locker()
            .write()
            .with_context(|| "acquire repo write lock for state-visibility put")?;

        let id = state_visibility_content_hash(&record)?;
        let mut existing = self.get_state_visibility_for_state(&state)?;

        for existing_record in &existing.records {
            if state_visibility_content_hash(existing_record)? == id {
                return Ok(id);
            }
        }

        existing.push(record);

        let path = self.state_visibility_path_for_state(&state);

        // Absence ≡ public: if the effective (latest) tier is public, drop the
        // state back to public-by-absence by removing the sidecar rather than
        // persisting a record that would classify a public state as non-public.
        let effective_public = match existing.latest() {
            Some(latest) => latest.tier == VisibilityTier::Public,
            None => true,
        };
        if effective_public {
            if path.exists() {
                fs::remove_file(&path).with_context(|| {
                    format!("remove state-visibility sidecar '{}'", path.display())
                })?;
            }
            return Ok(id);
        }

        let bytes = existing
            .encode()
            .with_context(|| "encoding state-visibility blob")?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create '{}'", parent.display()))?;
        }
        write_file_atomic(&path, &bytes).with_context(|| format!("write '{}'", path.display()))?;
        Ok(id)
    }

    /// Return the raw rmp-encoded `StateVisibilityBlob` bytes for the given
    /// state, or `Ok(None)` if no sidecar exists. The bytes are the
    /// wire-transfer payload, not a re-serialized view.
    pub fn get_state_visibility_bytes_for_state(
        &self,
        state: &ChangeId,
    ) -> Result<Option<Vec<u8>>> {
        let path = self.state_visibility_path_for_state(state);
        match fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err).with_context(|| format!("read '{}'", path.display())),
        }
    }

    /// Accept a wire-transferred `StateVisibilityBlob` for a specific state.
    /// The payload must decode, every contained record must target `state`,
    /// and each record is persisted through `put_state_visibility` so
    /// validation and public-by-absence normalization run at the same
    /// boundary as local writes.
    pub fn accept_wire_state_visibility(&self, state: ChangeId, bytes: &[u8]) -> Result<()> {
        let incoming = StateVisibilityBlob::decode(bytes).with_context(|| {
            format!(
                "decode incoming state visibility for state {}",
                state.to_string_full()
            )
        })?;

        for record in &incoming.records {
            if record.state != state {
                anyhow::bail!(
                    "incoming state visibility claims state {} but was transferred under {}",
                    record.state.to_string_full(),
                    state.to_string_full()
                );
            }
            record
                .validate()
                .with_context(|| "validate incoming state-visibility record")?;
        }

        for record in incoming.records {
            self.put_state_visibility(record)?;
        }
        Ok(())
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

    /// Whether `state` resolves to a **non-public** effective tier. `false`
    /// means **public-by-absence** — either no record exists, or the latest
    /// declaration resolves to [`VisibilityTier::Public`]. By construction
    /// (see [`put_state_visibility`](Self::put_state_visibility)) a public
    /// resolution is never persisted, so this is equivalent to "a record
    /// exists"; computing it from the effective tier keeps the keystone
    /// invariant — true iff the effective tier is non-public — explicit and
    /// robust against any blob a future path might introduce. This is the
    /// query the serve-side gate keys off.
    pub fn has_visibility_for_state(&self, state: &ChangeId) -> Result<bool> {
        Ok(self
            .get_state_visibility_for_state(state)?
            .latest()
            .is_some_and(|r| r.tier != VisibilityTier::Public))
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
    use std::{sync::Arc, thread};

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
    fn put_rejects_invalid_records_before_persisting_and_round_trips_valid_record() {
        let (_dir, repo) = fresh_repo();
        let team_state = ChangeId::from_bytes([31u8; 16]);
        let restricted_state = ChangeId::from_bytes([32u8; 16]);

        let empty_team = sample_record(
            team_state,
            VisibilityTier::TeamScoped {
                team_id: String::new(),
            },
        );
        let team_err = repo
            .put_state_visibility(empty_team)
            .expect_err("empty team id must be rejected");
        let team_err_chain = format!("{team_err:#}");
        assert!(
            team_err_chain.contains("team_scoped"),
            "unexpected error: {team_err}"
        );
        assert!(
            !repo.state_visibility_path_for_state(&team_state).exists(),
            "invalid team-scoped record must not persist a sidecar"
        );

        let empty_scope = sample_record(
            restricted_state,
            VisibilityTier::Restricted {
                scope_label: " ".into(),
            },
        );
        let scope_err = repo
            .put_state_visibility(empty_scope)
            .expect_err("empty restricted scope must be rejected");
        let scope_err_chain = format!("{scope_err:#}");
        assert!(
            scope_err_chain.contains("restricted"),
            "unexpected error: {scope_err}"
        );
        assert!(
            !repo
                .state_visibility_path_for_state(&restricted_state)
                .exists(),
            "invalid restricted record must not persist a sidecar"
        );

        let valid = sample_record(
            restricted_state,
            VisibilityTier::Restricted {
                scope_label: "security-embargo".into(),
            },
        );
        repo.put_state_visibility(valid.clone())
            .expect("valid put must persist");
        let stored = repo
            .get_state_visibility_for_state(&restricted_state)
            .expect("read valid record");
        assert_eq!(stored.records, vec![valid]);
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

    #[test]
    fn concurrent_puts_on_same_state_do_not_lose_updates() {
        // Finding 1: the read → dedupe → write must be serialized behind the
        // repo write lock. Eight threads each append a *distinct* non-public
        // record to the SAME state. Without the lock, concurrent appends load
        // the same base blob and the last `write_file_atomic` wins, silently
        // dropping the others. With it, every record survives.
        let (_dir, repo) = fresh_repo();
        let repo = Arc::new(repo);
        let state = ChangeId::from_bytes([42u8; 16]);

        const N: u32 = 8;
        let mut handles = Vec::new();
        for i in 0..N {
            let repo = Arc::clone(&repo);
            handles.push(thread::spawn(move || {
                // Distinct declared_at per thread → distinct content hash, so
                // each append accretes (none is a dedup no-op).
                let record = StateVisibility {
                    declared_at: Utc.with_ymd_and_hms(2026, 6, 1, 12, i, 0).unwrap(),
                    ..sample_record(state, VisibilityTier::Internal)
                };
                repo.put_state_visibility(record).expect("concurrent put");
            }));
        }
        for h in handles {
            h.join().expect("join put thread");
        }

        let stored = repo
            .get_state_visibility_for_state(&state)
            .expect("read back");
        assert_eq!(
            stored.records.len() as u32,
            N,
            "every concurrent append on the same state must survive — no lost update"
        );
        assert!(
            repo.has_visibility_for_state(&state)
                .expect("has visibility"),
            "a non-public effective tier must report has_visibility_for_state == true"
        );
    }

    #[test]
    fn public_put_resolves_to_public_by_absence() {
        // Finding 2: a Public put must not persist a record. The state stays
        // public-by-absence, so has_visibility_for_state is false and get
        // resolves to an empty (public) blob.
        let (_dir, repo) = fresh_repo();
        let state = ChangeId::from_bytes([20u8; 16]);
        repo.put_state_visibility(sample_record(state, VisibilityTier::Public))
            .expect("public put");

        assert!(
            !repo
                .has_visibility_for_state(&state)
                .expect("has visibility"),
            "a Public put must resolve to public-by-absence (has_visibility_for_state == false)"
        );
        assert!(
            !repo.state_visibility_path_for_state(&state).exists(),
            "a Public put must not persist a sidecar file"
        );
        assert!(
            repo.get_state_visibility_for_state(&state)
                .expect("read back")
                .records
                .is_empty(),
            "get must resolve a Public state to an empty (public) blob"
        );
    }

    #[test]
    fn supersede_with_public_drops_back_to_public_by_absence() {
        // Finding 2 (supersede arm): a private record makes the state
        // non-public; superseding it with a later Public declaration must
        // drop the whole state back to public-by-absence (record removed),
        // not leave a lingering non-public classification.
        let (_dir, repo) = fresh_repo();
        let state = ChangeId::from_bytes([21u8; 16]);
        let private = sample_record(
            state,
            VisibilityTier::Restricted {
                scope_label: "embargo".into(),
            },
        );
        let private_id = repo.put_state_visibility(private).expect("put private");
        assert!(
            repo.has_visibility_for_state(&state)
                .expect("has visibility"),
            "an embargo/private record must report has_visibility_for_state == true"
        );

        let public = StateVisibility {
            declared_at: Utc.with_ymd_and_hms(2026, 6, 2, 9, 0, 0).unwrap(),
            supersedes: Some(private_id),
            ..sample_record(state, VisibilityTier::Public)
        };
        repo.put_state_visibility(public)
            .expect("supersede with public");

        assert!(
            !repo
                .has_visibility_for_state(&state)
                .expect("has visibility"),
            "supersede-to-Public must restore public-by-absence (has_visibility_for_state == false)"
        );
        assert!(
            !repo.state_visibility_path_for_state(&state).exists(),
            "supersede-to-Public must remove the sidecar entirely"
        );
    }
}
