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
use chrono::Utc;
use objects::{
    fs_atomic::write_file_atomic,
    lock::RepositoryLockExt,
    object::{ChangeId, ContentHash, StateVisibility, StateVisibilityBlob, VisibilityTier},
};
use oplog::{OpLogBackend, OpRecord, VisibilitySidecarSnapshots};

use crate::namespace_policy::{VisibilityResolutionContext, resolve_default_visibility};
use crate::repository::Repository;

/// Outcome of a visibility put that captured its before/after images
/// **atomically under the write lock** (heddle#317 / PR #529 P1 r5). The
/// before-image is the sidecar the put actually overwrote — read inside the
/// same critical section as the write — so no caller can read a stale prior
/// before locking and record a before-image that a racing put has already
/// invalidated (which would make undo delete the racer's record). `id` is the
/// content id of the persisted record; `prior_sidecar`/`new_sidecar` are the
/// full per-state sidecar bytes before/after (`None` = public-by-absence),
/// feeding the oplog's undo/redo targets directly.
#[derive(Debug, Clone)]
pub struct PutVisibilityOutcome {
    pub id: ContentHash,
    pub prior_sidecar: Option<Vec<u8>>,
    pub new_sidecar: Option<Vec<u8>>,
}

/// Which audit op a [`Repository::commit_state_visibility`] emits — and thus
/// how it resolves the record under the write lock.
#[derive(Debug, Clone, Copy)]
pub enum VisibilityCommitKind {
    /// `heddle visibility set`: a fresh declaration. Emits
    /// `OpRecord::StateVisibilitySet`; always commits.
    Set,
    /// `heddle visibility promote`: supersede the current latest record with a
    /// less-restrictive tier. Emits `OpRecord::StateVisibilityPromote`; resolves
    /// the superseded id **under the same lock** as the put (heddle#317 r5), so
    /// the supersede pointer and the captured before-image describe one
    /// consistent snapshot of the sidecar — never a torn read across a racing
    /// append. A public-by-absence state has nothing to promote, so the commit
    /// yields `Ok(None)`.
    Promote,
}

/// Outcome of a [`Repository::commit_state_visibility`]: the sidecar put's
/// before/after images and content id, plus the content id of the record a
/// promotion superseded (`None` for a `Set`).
#[derive(Debug, Clone)]
pub struct VisibilityCommitOutcome {
    pub put: PutVisibilityOutcome,
    pub superseded: Option<ContentHash>,
}

/// The automatic capture-time default-visibility binding, staged for folding
/// into a snapshot's own commit batch (heddle#317 / PR #529 P1). Produced by
/// [`Repository::stage_default_visibility_binding`]; the sidecar has already
/// been written when this is returned.
pub struct DefaultVisibilityBinding {
    /// The [`OpRecord::StateVisibilitySet`] audit record to append to the
    /// snapshot's batch so undo/redo of that batch restores the sidecar.
    pub record: OpRecord,
    /// The per-state sidecar bytes BEFORE the binding (always `None` for a
    /// freshly created state). The `SnapshotMutation` rewind restores the
    /// sidecar to this image if the snapshot batch fails to commit.
    pub prior_sidecar: Option<Vec<u8>>,
}

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
    pub fn put_state_visibility(&self, record: StateVisibility) -> Result<PutVisibilityOutcome> {
        // Serialize the full read-modify-write behind the repo write lock so
        // concurrent appends on the same state can't clobber each other.
        let _lock = self
            .locker()
            .write()
            .with_context(|| "acquire repo write lock for state-visibility put")?;
        self.put_state_visibility_locked(record)
    }

    /// Like [`put_state_visibility`](Self::put_state_visibility) but a no-op when
    /// the state already carries any visibility record. The existence test and
    /// the write are ONE locked critical section, so a concurrent `visibility
    /// set` landing between "is it absent?" and "write the default" cannot be
    /// clobbered — the guard sees the racer's record and skips (heddle#317 r5).
    /// Returns `Ok(None)` when a record already existed, else the put outcome.
    /// This is the atomic primitive the capture-time default binding stands on.
    pub fn put_state_visibility_if_absent(
        &self,
        record: StateVisibility,
    ) -> Result<Option<PutVisibilityOutcome>> {
        let _lock = self
            .locker()
            .write()
            .with_context(|| "acquire repo write lock for if-absent state-visibility put")?;
        self.put_state_visibility_locked_if_absent(record)
    }

    /// Lock-free body of [`put_state_visibility`](Self::put_state_visibility).
    ///
    /// The caller **must already hold the repo write lock**. The repo lock is an
    /// OS file lock (`flock`) that does NOT nest within one process — re-taking
    /// it on a thread that already holds it deadlocks — so the snapshot
    /// chokepoint, which writes the capture-time default-visibility binding
    /// while still holding the snapshot write lock (heddle#317 / PR #529 P1),
    /// calls this directly instead of the lock-taking wrapper.
    ///
    /// **Atomic before-image capture (PR #529 P1 r5).** This reads the existing
    /// sidecar bytes (the before-image), appends the record, and writes the new
    /// bytes — all in one critical section the lock holder owns — then returns
    /// both images in a [`PutVisibilityOutcome`]. Recording the before-image
    /// here, rather than from a pre-lock read at the call site, closes the
    /// TOCTOU where a racing put invalidates a stale pre-read prior and undo
    /// then deletes the racer's record.
    pub(crate) fn put_state_visibility_locked(
        &self,
        record: StateVisibility,
    ) -> Result<PutVisibilityOutcome> {
        record
            .validate()
            .with_context(|| "validate state-visibility record before put")?;
        let state = record.state;
        let path = self.state_visibility_path_for_state(&state);

        // Capture the before-image UNDER THE LOCK: this is the record the put
        // actually overwrites, the oplog's undo target. No call site reads it
        // before locking, so it can never be a stale pre-read.
        let prior_sidecar = self.get_state_visibility_bytes_for_state(&state)?;

        let id = state_visibility_content_hash(&record)?;
        let mut existing = self.get_state_visibility_for_state(&state)?;

        for existing_record in &existing.records {
            if state_visibility_content_hash(existing_record)? == id {
                // Dedup no-op: nothing written, so before == after.
                return Ok(PutVisibilityOutcome {
                    id,
                    new_sidecar: prior_sidecar.clone(),
                    prior_sidecar,
                });
            }
        }

        existing.push(record);

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
            return Ok(PutVisibilityOutcome {
                id,
                prior_sidecar,
                new_sidecar: None,
            });
        }

        let bytes = existing
            .encode()
            .with_context(|| "encoding state-visibility blob")?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create '{}'", parent.display()))?;
        }
        write_file_atomic(&path, &bytes).with_context(|| format!("write '{}'", path.display()))?;
        Ok(PutVisibilityOutcome {
            id,
            prior_sidecar,
            new_sidecar: Some(bytes),
        })
    }

    /// Lock-free body of
    /// [`put_state_visibility_if_absent`](Self::put_state_visibility_if_absent).
    /// The caller **must already hold the repo write lock** (see
    /// [`put_state_visibility_locked`](Self::put_state_visibility_locked) for why
    /// the snapshot chokepoint calls the locked body directly). The existence
    /// test runs inside the caller's critical section, immediately before the
    /// write, so the absent → write decision is atomic.
    pub(crate) fn put_state_visibility_locked_if_absent(
        &self,
        record: StateVisibility,
    ) -> Result<Option<PutVisibilityOutcome>> {
        if self
            .get_state_visibility_for_state(&record.state)
            .with_context(|| "read existing visibility for if-absent put")?
            .has_record()
        {
            return Ok(None);
        }
        Ok(Some(self.put_state_visibility_locked(record)?))
    }

    /// Commit a visibility mutation as ONE serialized unit: write the per-state
    /// sidecar AND append its `OpRecord` audit entry **while holding a single
    /// repo write lock** (heddle#317 / PR #529 P1 r6).
    ///
    /// Pre-r6 the CLI wrote the sidecar under the repo lock, RELEASED it, then
    /// appended the oplog entry under the separate `oplog.lock`. Two overlapping
    /// `visibility set`/`promote` commands could therefore append their oplog
    /// records in the OPPOSITE order to their sidecar writes: if B's sidecar
    /// landed after A's, but B's oplog append raced ahead of A's, undo would
    /// treat A as the latest op and restore A's (`None`) before-image — deleting
    /// B's record and silently dropping the state to public-by-absence. Holding
    /// ONE lock across both steps totally orders concurrent mutations, so the
    /// oplog-append order always matches the sidecar-write order.
    ///
    /// **Lock ordering (heddle#317 r6).** This acquires the repo write lock
    /// FIRST, then the oplog append takes `oplog.lock` — the same nesting order
    /// the snapshot chokepoint uses (`snapshot_with_attribution_profiled_locked`
    /// holds the repo write lock across `apply`'s sidecar write and the atomic
    /// batch commit). No path holds `oplog.lock` across a repo-lock acquisition
    /// (the oplog crate sits below `repo` and never reaches for the repo lock),
    /// so the nesting cannot deadlock.
    ///
    /// `Set` always commits. `Promote` resolves the superseded record under the
    /// lock and returns `Ok(None)` on a public-by-absence state (nothing to
    /// promote); the caller maps that to a user-facing error.
    pub fn commit_state_visibility(
        &self,
        record: StateVisibility,
        kind: VisibilityCommitKind,
    ) -> Result<Option<VisibilityCommitOutcome>> {
        // ONE write lock spans the sidecar write AND the oplog append below, so
        // concurrent visibility mutations are totally ordered.
        let _lock = self
            .locker()
            .write()
            .with_context(|| "acquire repo write lock for state-visibility commit")?;

        let mut record = record;
        let superseded = match kind {
            VisibilityCommitKind::Set => None,
            VisibilityCommitKind::Promote => {
                let existing = self.get_state_visibility_for_state(&record.state)?;
                let Some(latest) = existing.latest() else {
                    // Public-by-absence: nothing to promote, no sidecar write,
                    // no oplog append.
                    return Ok(None);
                };
                let superseded = state_visibility_content_hash(latest)?;
                record.supersedes = Some(superseded);
                Some(superseded)
            }
        };

        let state = record.state;
        let tier = record.tier.clone();
        let put = self.put_state_visibility_locked(record)?;

        // Append the audit entry WHILE STILL HOLDING the repo write lock — never
        // after releasing it. This is the r6 invariant: a concurrent mutation
        // cannot interleave its own sidecar write + append between this put and
        // this append, so the two logs stay in the same order.
        let scope = self.op_scope();
        let snapshots = VisibilitySidecarSnapshots {
            prior: put.prior_sidecar.clone(),
            new: put.new_sidecar.clone(),
        };
        match superseded {
            None => self.oplog().record_state_visibility_set(
                &state,
                &put.id,
                &tier,
                snapshots,
                Some(&scope),
            ),
            Some(superseded) => self.oplog().record_state_visibility_promote(
                &state,
                &superseded,
                &put.id,
                &tier,
                snapshots,
                Some(&scope),
            ),
        }
        .with_context(|| "append state-visibility audit entry under repo write lock")?;

        Ok(Some(VisibilityCommitOutcome { put, superseded }))
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

    /// Canonical content id of a [`StateVisibility`] record — the same id
    /// [`Self::put_state_visibility`] assigns. Exposed so callers (e.g. the
    /// `visibility promote` verb) can name an existing record to supersede
    /// without re-deriving the hashing scheme and drifting out of sync.
    pub fn state_visibility_record_id(&self, record: &StateVisibility) -> Result<ContentHash> {
        state_visibility_content_hash(record)
    }

    /// Resolve the default visibility tier a freshly captured state inherits.
    ///
    /// Runs the [`resolve_default_visibility`] chain with the repo-wide default
    /// from `[review.discussion] default_visibility`. That field defaults to
    /// [`VisibilityTier::Public`], so an unconfigured repo resolves public and
    /// the common case stays record-free. Namespace policies are not yet loaded
    /// from config, so the namespace tier of the chain is currently unused here.
    pub fn resolve_capture_default_visibility(&self) -> VisibilityTier {
        let ctx = VisibilityResolutionContext {
            repo_default: Some(self.config().review.discussion.default_visibility.clone()),
            namespace: None,
        };
        resolve_default_visibility(&ctx)
    }

    /// Invariant A — immutable-at-creation (spike #266 §5.4).
    ///
    /// Bind the inherited default tier to a brand-new state *at creation time*:
    /// resolve the chain once, and persist any resolution more restrictive than
    /// public as the state's initial [`StateVisibility`] record (plus an
    /// `OpRecord::StateVisibilitySet` audit entry). A public resolution stays
    /// record-free (absence ≡ public). Resolving here — not at first serve —
    /// means a `[namespace]`/repo default that later drifts more-open cannot
    /// retroactively expose an already-created state.
    ///
    /// This is the single decision site for default visibility. Every snapshot
    /// creator funnels through the snapshot chokepoint
    /// ([`snapshot_with_attribution_profiled`](Self::snapshot_with_attribution_profiled)
    /// / [`snapshot_tree_with_attribution_profiled`](Self::snapshot_tree_with_attribution_profiled),
    /// plus the mount capture path), so capture, cherry-pick, revert, and
    /// daemon/mount captures all inherit the configured default by construction
    /// rather than each call site re-binding (and one of them leaking when it
    /// forgets to).
    ///
    /// Idempotent: a state that already carries a visibility record is left
    /// untouched, so a re-capture that mints the same `ChangeId` never
    /// double-binds, and a caller that explicitly set a tier before this runs is
    /// respected. Returns the binding to fold when one was written.
    ///
    /// **Folds into the snapshot's own batch (PR #529 P1).** The returned
    /// [`OpRecord::StateVisibilitySet`] is appended to the *same* oplog batch as
    /// the snapshot that triggered the binding — never a separate trailing batch
    /// — so a single `heddle undo` reverts the snapshot AND its auto-applied
    /// default tier together. The old separate-batch binding made the first
    /// `undo` after a capture restore only the sidecar, leaving the snapshot in
    /// place (undo took two presses). Explicit user actions
    /// (`heddle visibility set`/`promote`) stay their own undoable batch — only
    /// this automatic, snapshot-time default binding folds in.
    ///
    /// `lock_held` declares whether the caller already holds the repo write lock.
    /// The snapshot chokepoint's `SnapshotMutation::apply` runs under the snapshot
    /// write lock and passes `true` (the sidecar write must not re-enter the
    /// non-reentrant `flock` repo lock); the mount capture path holds no lock and
    /// passes `false` (the sidecar write takes the lock itself).
    pub fn stage_default_visibility_binding(
        &self,
        state: &ChangeId,
        lock_held: bool,
    ) -> Result<Option<DefaultVisibilityBinding>> {
        let tier = self.resolve_capture_default_visibility();
        if tier == VisibilityTier::Public {
            return Ok(None);
        }

        let declarer = self
            .get_principal()
            .with_context(|| "resolve principal for capture-time visibility binding")?;
        let record = StateVisibility {
            state: *state,
            tier: tier.clone(),
            embargo_until: None,
            declarer,
            declared_at: Utc::now(),
            signature: None,
            supersedes: None,
        };
        // Bind ONLY if the state is record-free, with the existence test and the
        // write fused into one locked critical section (heddle#317 r5). A racing
        // `visibility set` that lands between the two would otherwise be clobbered
        // by an unconditional default-bind; `if_absent` skips instead, and a skip
        // (Ok(None)) stages no oplog entry. The captured before-image is the one
        // the locked write actually overwrote — for a record-free state, None.
        let outcome = if lock_held {
            self.put_state_visibility_locked_if_absent(record)?
        } else {
            self.put_state_visibility_if_absent(record)?
        };
        let Some(outcome) = outcome else {
            return Ok(None);
        };
        Ok(Some(DefaultVisibilityBinding {
            record: OpRecord::StateVisibilitySet {
                state: *state,
                record_id: outcome.id,
                tier,
                prior_sidecar: outcome.prior_sidecar.clone(),
                new_sidecar: outcome.new_sidecar,
            },
            prior_sidecar: outcome.prior_sidecar,
        }))
    }

    /// Restore the per-state visibility sidecar to an absolute snapshot:
    /// rewrite `snapshot`'s bytes, or remove the sidecar when `snapshot` is
    /// `None` (public-by-absence). Absolute (write-or-delete), so re-running it
    /// on a rollback path is idempotent. This is the undo/redo restore point —
    /// undo passes the op's `prior_sidecar`, redo its `new_sidecar`. Mirrors
    /// [`restore_redaction_sidecar`](Self::restore_redaction_sidecar).
    pub fn restore_state_visibility_sidecar(
        &self,
        state: &ChangeId,
        snapshot: Option<Vec<u8>>,
    ) -> Result<()> {
        let path = self.state_visibility_path_for_state(state);
        match snapshot {
            Some(bytes) => {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("create '{}'", parent.display()))?;
                }
                write_file_atomic(&path, &bytes)
                    .with_context(|| format!("write '{}'", path.display()))?;
            }
            None => {
                if path.exists() {
                    fs::remove_file(&path).with_context(|| {
                        format!("remove state-visibility sidecar '{}'", path.display())
                    })?;
                }
            }
        }
        Ok(())
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

    /// A repo whose `[review.discussion] default_visibility` is pinned to
    /// `tier_toml`, so the capture-time binding resolves a non-public default.
    fn repo_with_default(tier_toml: &str) -> (TempDir, Repository) {
        let dir = TempDir::new().unwrap();
        Repository::init_default(dir.path()).unwrap();
        std::fs::write(
            dir.path().join(".heddle/config.toml"),
            format!(
                "[repository]\nversion = 1\n\n[review.discussion]\ndefault_visibility = {tier_toml}\n"
            ),
        )
        .unwrap();
        let repo = Repository::open(dir.path()).unwrap();
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
        let id1 = repo.put_state_visibility(record.clone()).expect("put").id;
        let id2 = repo.put_state_visibility(record).expect("re-put").id;
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
        let first_id = repo.put_state_visibility(first).expect("put first").id;
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
        let private_id = repo.put_state_visibility(private).expect("put private").id;
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

    #[test]
    fn racing_visibility_sets_capture_true_prior_under_lock() {
        // The TOCTOU this closes: a call site that read its before-image BEFORE
        // taking the write lock could record a stale prior. Sequence A-reads /
        // B-writes / A-writes then has A record prior == None, so undoing A
        // DELETES B's independent record. The primitive now captures the
        // before-image UNDER the lock — it is the record actually present when
        // the locked write runs — so A's recorded prior is B's record, not a
        // stale None. Deterministic: drive the two puts through the primitive in
        // order; the returned prior is what the call site records in the oplog.
        let (_dir, repo) = fresh_repo();
        let state = ChangeId::from_bytes([99u8; 16]);

        // B's write lands first, onto a record-free state.
        let b = StateVisibility {
            declared_at: Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap(),
            ..sample_record(
                state,
                VisibilityTier::TeamScoped {
                    team_id: "infra".into(),
                },
            )
        };
        let b_out = repo.put_state_visibility(b).expect("B put");
        assert!(
            b_out.prior_sidecar.is_none(),
            "B writes onto a fresh state, so its before-image is public-by-absence"
        );
        let after_b = repo
            .get_state_visibility_bytes_for_state(&state)
            .expect("read after B");
        assert_eq!(
            b_out.new_sidecar, after_b,
            "B's after-image must equal the bytes actually on disk after B's write"
        );

        // A's write lands AFTER B's. Its recorded prior must be B's record.
        let a = StateVisibility {
            declared_at: Utc.with_ymd_and_hms(2026, 6, 1, 12, 1, 0).unwrap(),
            ..sample_record(state, VisibilityTier::Internal)
        };
        let a_out = repo.put_state_visibility(a).expect("A put");
        assert_eq!(
            a_out.prior_sidecar, after_b,
            "A's recorded prior must be the record actually present (B's), captured \
             under the lock — never a stale pre-read None"
        );
        assert!(
            a_out.prior_sidecar.is_some(),
            "A's before-image must not be the stale pre-read None the TOCTOU produced"
        );

        // Prove the payoff: undoing A (restore its prior) leaves B's record
        // intact rather than deleting the whole sidecar.
        repo.restore_state_visibility_sidecar(&state, a_out.prior_sidecar.clone())
            .expect("undo A by restoring its captured prior");
        let restored = repo
            .get_state_visibility_for_state(&state)
            .expect("read after undo");
        assert_eq!(
            restored.records.len(),
            1,
            "undoing A must leave exactly B's record — not delete B's sidecar"
        );
        assert_eq!(
            restored.latest().unwrap().tier,
            VisibilityTier::TeamScoped {
                team_id: "infra".into()
            },
            "the record surviving undo-of-A must be B's team-scoped declaration"
        );
    }

    #[test]
    fn bind_default_visibility_if_absent_is_atomic() {
        // The capture-time default binding must NOT overwrite a record that
        // already exists when it runs, and the existence test must live inside
        // the write lock (heddle#317 r5). Models a `visibility set` that landed
        // before the bind: bind sees the record, skips, and stages no oplog
        // entry. Driven end-to-end through `stage_default_visibility_binding`.
        let (_dir, repo) = repo_with_default("\"Internal\"");
        let state = ChangeId::from_bytes([77u8; 16]);

        // A user (or a racer) already set a team-scoped tier on this state.
        let existing = sample_record(
            state,
            VisibilityTier::TeamScoped {
                team_id: "sec".into(),
            },
        );
        repo.put_state_visibility(existing.clone())
            .expect("seed existing record");
        let bytes_before = repo
            .get_state_visibility_bytes_for_state(&state)
            .expect("read existing bytes");

        let staged = repo
            .stage_default_visibility_binding(&state, false)
            .expect("bind on an already-recorded state");
        assert!(
            staged.is_none(),
            "bind must not bind over an existing record — and stages no oplog entry"
        );
        assert_eq!(
            bytes_before,
            repo.get_state_visibility_bytes_for_state(&state)
                .expect("read bytes after bind"),
            "the user's existing record must be untouched by a skipped bind"
        );
        let stored = repo
            .get_state_visibility_for_state(&state)
            .expect("read stored");
        assert_eq!(stored.records.len(), 1, "no spurious second record appended");
        assert_eq!(
            stored.records[0], existing,
            "bind must not overwrite the user's declared tier"
        );

        // On a genuinely record-free state, bind DOES write and stage a record
        // whose before-image is public-by-absence.
        let fresh = ChangeId::from_bytes([78u8; 16]);
        let staged = repo
            .stage_default_visibility_binding(&fresh, false)
            .expect("bind on a fresh state")
            .expect("a record-free state must bind the default");
        assert!(
            staged.prior_sidecar.is_none(),
            "a fresh state's before-image is public-by-absence (None)"
        );
        assert!(
            repo.has_visibility_for_state(&fresh)
                .expect("has visibility on bound state"),
            "the default tier must now be bound on the fresh state"
        );
        match staged.record {
            OpRecord::StateVisibilitySet {
                tier, prior_sidecar, ..
            } => {
                assert_eq!(tier, VisibilityTier::Internal, "bound the configured default");
                assert!(
                    prior_sidecar.is_none(),
                    "the staged oplog record's before-image is None for a fresh state"
                );
            }
            other => panic!("bind must stage a StateVisibilitySet record, got {other:?}"),
        }
    }

    /// A `StateVisibilitySet` audit entry's identity + sidecar snapshots, pulled
    /// from the oplog for ordering assertions.
    struct RecordedVisibilitySet {
        record_id: ContentHash,
        prior: Option<Vec<u8>>,
        new: Option<Vec<u8>>,
    }

    /// Collect every `StateVisibilitySet` audit entry, in chronological (entry
    /// id) order.
    fn recorded_visibility_sets(repo: &Repository) -> Vec<RecordedVisibilitySet> {
        let mut entries = repo.oplog().recent(1000).expect("read oplog");
        entries.sort_by_key(|e| e.id);
        entries
            .iter()
            .filter_map(|e| match &e.operation {
                OpRecord::StateVisibilitySet {
                    record_id,
                    prior_sidecar,
                    new_sidecar,
                    ..
                } => Some(RecordedVisibilitySet {
                    record_id: *record_id,
                    prior: prior_sidecar.clone(),
                    new: new_sidecar.clone(),
                }),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn concurrent_visibility_sets_oplog_order_matches_sidecar_order() {
        // The r6 invariant: the combined primitive writes the sidecar AND appends
        // the oplog entry under ONE repo write lock, so two `visibility set`
        // commands committed in order A-then-B append their oplog records in that
        // SAME order. Pre-r6 the sidecar write and the oplog append took separate
        // locks, so B's append could race ahead of A's; undo would then treat A as
        // the latest op, restore A's `None` before-image, and DELETE B's record.
        // Deterministic: sequence the two commits through the primitive; assert the
        // recorded oplog order is [A, B] (B latest) and B's before-image is A's
        // record, so undoing B restores A — not public-by-absence.
        let (_dir, repo) = fresh_repo();
        let state = ChangeId::from_bytes([55u8; 16]);

        // A commits first, onto a record-free state.
        let a = StateVisibility {
            declared_at: Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap(),
            ..sample_record(
                state,
                VisibilityTier::TeamScoped {
                    team_id: "infra".into(),
                },
            )
        };
        let a_out = repo
            .commit_state_visibility(a, VisibilityCommitKind::Set)
            .expect("commit A")
            .expect("a set always commits");

        // B commits second, onto A's record.
        let b = StateVisibility {
            declared_at: Utc.with_ymd_and_hms(2026, 6, 1, 12, 1, 0).unwrap(),
            ..sample_record(state, VisibilityTier::Internal)
        };
        let b_out = repo
            .commit_state_visibility(b, VisibilityCommitKind::Set)
            .expect("commit B")
            .expect("a set always commits");

        // The oplog-append order matches the sidecar-write order: A first, B second.
        let sets = recorded_visibility_sets(&repo);
        assert_eq!(sets.len(), 2, "exactly two visibility-set audit entries");
        assert_eq!(
            sets[0].record_id, a_out.put.id,
            "A's audit entry is recorded first"
        );
        assert_eq!(
            sets[1].record_id, b_out.put.id,
            "B's audit entry is recorded second (B latest)"
        );

        // B's recorded before-image is A's on-disk record — never a stale `None`.
        assert_eq!(
            sets[1].prior, a_out.put.new_sidecar,
            "B's oplog before-image must be A's record, not a stale public-by-absence"
        );
        assert!(
            sets[1].prior.is_some(),
            "B's before-image must not be the stale None the out-of-order race produced"
        );
        assert_eq!(
            b_out.put.prior_sidecar, a_out.put.new_sidecar,
            "B's put captured A's record as its before-image, under the same lock"
        );

        // Undo B by restoring its captured prior: A survives, the state stays
        // A's tier — undoing B does NOT delete A's record.
        repo.restore_state_visibility_sidecar(&state, b_out.put.prior_sidecar.clone())
            .expect("undo B by restoring its captured prior");
        let restored = repo
            .get_state_visibility_for_state(&state)
            .expect("read after undo");
        assert_eq!(
            restored.records.len(),
            1,
            "undoing B must leave exactly A's record — not public-by-absence"
        );
        assert_eq!(
            restored.latest().unwrap().tier,
            VisibilityTier::TeamScoped {
                team_id: "infra".into()
            },
            "the record surviving undo-of-B must be A's team-scoped declaration"
        );
    }

    #[test]
    fn visibility_set_sidecar_and_oplog_are_one_locked_section() {
        // The combined primitive returns only after BOTH the sidecar write and the
        // oplog append are durable — there is no observable window where the
        // sidecar exists but its audit entry is missing (the r6 single-locked-section
        // guarantee). The lock window can't be opened without a real race, so assert
        // the post-return invariant: once the primitive returns, the sidecar is on
        // disk AND a matching StateVisibilitySet audit entry exists alongside it.
        let (_dir, repo) = fresh_repo();
        let state = ChangeId::from_bytes([56u8; 16]);

        // Before the commit: no sidecar, no audit entry.
        assert!(
            !repo.state_visibility_path_for_state(&state).exists(),
            "no sidecar before the commit"
        );
        assert!(
            recorded_visibility_sets(&repo).is_empty(),
            "no visibility-set audit entry before the commit"
        );

        let record = sample_record(
            state,
            VisibilityTier::Restricted {
                scope_label: "embargo".into(),
            },
        );
        let outcome = repo
            .commit_state_visibility(record, VisibilityCommitKind::Set)
            .expect("commit")
            .expect("a set always commits");

        // After return: the sidecar is durable and equals the put's after-image.
        let on_disk = repo
            .get_state_visibility_bytes_for_state(&state)
            .expect("read sidecar bytes");
        assert!(
            on_disk.is_some(),
            "the sidecar must be on disk once the primitive returns"
        );
        assert_eq!(
            on_disk, outcome.put.new_sidecar,
            "the durable sidecar equals the put's after-image"
        );

        // AND the matching audit entry is durable in the SAME observable state:
        // the primitive never returns with the sidecar written but the oplog entry
        // absent — the two are one locked unit.
        let sets = recorded_visibility_sets(&repo);
        assert_eq!(
            sets.len(),
            1,
            "exactly one visibility-set audit entry must accompany the sidecar"
        );
        assert_eq!(
            sets[0].record_id, outcome.put.id,
            "the audit entry names the put"
        );
        assert_eq!(
            sets[0].new, on_disk,
            "the audit entry's after-image equals the durable sidecar"
        );
    }
}
