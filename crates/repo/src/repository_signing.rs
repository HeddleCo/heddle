// SPDX-License-Identifier: Apache-2.0
//! State signing operations for Repository.

use std::sync::Arc;

use crypto::{Signer, StateSigningExt, load_signer, verify_state_signature_bytes};
use objects::{
    object::{ChangeId, ContentHash, SignatureStatus, State, StateSignature},
    store::ObjectStore,
};
use tracing::{debug, instrument};

use super::{HeddleError, Repository, Result};

/// Outcome of [`Repository::resign_if_owned`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResignOutcome {
    /// The state carried no signature; nothing to preserve.
    Unsigned,
    /// The state was signed by this repo's active signing identity and has
    /// been re-signed over its CURRENT `compute_hash()`, so the signature
    /// stays valid after an authorized rewrite.
    Resigned,
    /// The signature was produced by a key this identity cannot reproduce —
    /// a foreign/third-party key, or no signer is currently resolvable. The
    /// state is left UNMODIFIED; the caller must not persist a rewritten
    /// version of it, since that would ship a signature that no longer
    /// verifies against the new hash.
    Unreproducible,
}

impl Repository {
    /// Path to this repo's per-repo local signing identity (heddle#482).
    fn local_identity_path(&self) -> std::path::PathBuf {
        self.heddle_dir().join(crate::identity::LOCAL_IDENTITY_FILE)
    }

    /// Resolve the machine signing key for THIS sign attempt: the device key
    /// if `heddle auth login` has linked one, otherwise the auto-minted
    /// per-repo local key. Returns `None` only when no key can be produced
    /// (e.g. an unwritable home, or — fail-closed — an identity file whose
    /// permissions have been loosened to group/world-readable), in which case
    /// captures proceed unsigned and surface that status.
    ///
    /// Deliberately NOT cached (heddle#482): the identity file's permissions
    /// are re-validated by `resolve_signer` on every call, so a mid-session
    /// `chmod` that exposes the private key makes the very next sign fail
    /// closed instead of reusing a signer minted while the file was still
    /// `0600`. A long-lived handle (e.g. the mount path) therefore can't keep
    /// signing with a now-exposed key. Resolution is a small file read + PEM
    /// parse — negligible against the tree/blob writes a capture already does.
    pub(crate) fn signing_signer(&self) -> Option<Arc<dyn Signer>> {
        let local = self.local_identity_path();
        let device = crate::identity::device_identity_path();
        crate::identity::resolve_signer(&local, &device).map(Arc::from)
    }

    /// Best-effort auto-sign on the capture/commit/merge path (heddle#482).
    ///
    /// A missing or unreadable key warns and leaves the state unsigned rather
    /// than failing the capture — the unsigned status stays observable via
    /// `state.signature` being `None`. This MUST be the last mutation before
    /// `put_state`: it signs `compute_hash()` over the then-current fields, so
    /// any later field change would invalidate the signature.
    pub(crate) fn sign_state_best_effort(&self, state: &mut State) {
        let Some(signer) = self.signing_signer() else {
            debug!("no signing identity available; state captured unsigned");
            return;
        };
        if let Err(error) = state.sign(&*signer) {
            tracing::warn!(%error, "auto-signing failed; state captured unsigned");
        }
    }

    /// The authored-state write chokepoint (heddle#482): auto-sign
    /// (best-effort) then persist a freshly authored `State`. EVERY writer that
    /// records a *new authored state* routes through here — capture/snapshot,
    /// merge, mount capture, thread materialize, fork, collapse, context
    /// annotation, and both rebase replay paths, across this crate AND the
    /// `mount`/`cli` crates — so no authored state can reach the store
    /// unsigned. Signing is the LAST mutation before the write, so the
    /// signature covers the final field set; any later change would invalidate
    /// it.
    ///
    /// Non-authored writes deliberately do NOT use this path — they keep their
    /// existing signature (or stay legitimately unsigned) rather than minting a
    /// fresh one: replaying/transferring an already-signed state
    /// (`put_state_serialized`, sync/packfile ops), the synthetic init root
    /// (`seed_default_thread`), git-import of foreign history, and server-side
    /// review/signal mutations. Re-signing those would either clobber an
    /// existing signature or falsely attribute foreign content to this device.
    pub fn put_authored_state(&self, state: &mut State) -> Result<()> {
        self.sign_state_best_effort(state);
        self.store.put_state(state)?;
        Ok(())
    }

    /// Resolve a signer THIS repo controls whose public key matches the one
    /// embedded in `signature`, or `None` if the signature is foreign.
    ///
    /// Tries two keys (heddle#570): first the *active* signer (the device key
    /// after an `auth login`, else the per-repo local key), then — if that
    /// misses — the per-repo local key explicitly. The second try matters
    /// because the device key supersedes the local key for *new* states, so a
    /// state signed with the local key BEFORE a device key was linked would
    /// otherwise be misclassified as foreign even though this repo still owns
    /// its key and can legitimately re-sign it.
    fn owning_signer_for(&self, signature: &StateSignature) -> Option<Arc<dyn Signer>> {
        if let Some(signer) = self.signing_signer()
            && hex::encode(signer.public_key()) == signature.public_key
        {
            return Some(signer);
        }
        let local = crate::identity::load_local_signer(&self.local_identity_path())?;
        if hex::encode(local.public_key()) == signature.public_key {
            return Some(Arc::from(local));
        }
        None
    }

    /// Re-sign `state` over its CURRENT `compute_hash()` IFF its existing
    /// signature was both produced by a key this repo controls AND already
    /// valid over one of `prior_hashes` (the hash candidates the signature
    /// could have been made over, BEFORE the caller's rewrite). So an
    /// authorized rewrite (e.g. the #570 fidelity backfill, which re-derives
    /// hash-bearing fields) keeps a valid signature instead of shipping one
    /// that no longer verifies.
    ///
    /// Multiple candidates are accepted because the #565 format bump changed
    /// how `compute_hash` folds the git-fidelity fields: a state signed BEFORE
    /// the bump was signed over its pre-fidelity hash
    /// ([`State::compute_hash_pre_fidelity`]), while one signed after was signed
    /// over the current hash. The backfill passes BOTH so a valid legacy
    /// signature isn't misread as unreproducible just because the new hash
    /// doesn't match (heddle#570).
    ///
    /// Ownership is decided by [`Self::owning_signer_for`], which tries both the
    /// active signer and the per-repo local key — a repo that signed states
    /// with its local key before linking a device key is still recognised as
    /// the owner (heddle#570).
    ///
    /// Returns [`ResignOutcome::Unreproducible`] WITHOUT modifying `state` when:
    /// - the signature belongs to a foreign key (or no owned signer resolves) —
    ///   re-signing foreign content would falsely attribute it to this device; or
    /// - the existing signature does NOT verify against ANY of `prior_hashes` —
    ///   re-signing over it would launder a never-valid signature into a fresh,
    ///   valid-looking one (heddle#570).
    ///
    /// In both cases the caller MUST NOT persist a rewritten version of the state.
    pub fn resign_if_owned(
        &self,
        state: &mut State,
        prior_hashes: &[ContentHash],
    ) -> ResignOutcome {
        let Some(existing) = state.signature.clone() else {
            return ResignOutcome::Unsigned;
        };
        let Some(signer) = self.owning_signer_for(&existing) else {
            return ResignOutcome::Unreproducible;
        };
        // Verify the EXISTING signature against the OLD hash(es) before
        // re-signing. Without this an owned-but-invalid signature (corrupted, or
        // made over different content) would be silently replaced with a valid
        // signature over the new content — laundering a bad signature. A legacy
        // (pre-#565) signature verifies against the pre-fidelity candidate, not
        // the post-bump one, so we accept ANY candidate.
        let verifies = prior_hashes
            .iter()
            .any(|hash| verify_state_signature_bytes(&existing, hash).is_ok());
        if !verifies {
            return ResignOutcome::Unreproducible;
        }
        match state.sign(&*signer) {
            Ok(()) => ResignOutcome::Resigned,
            Err(error) => {
                tracing::warn!(%error, "re-signing an owned state failed; leaving it unmodified");
                ResignOutcome::Unreproducible
            }
        }
    }

    /// Sign a state with the given signer.
    ///
    /// This loads the state, signs it, and stores the updated state.
    ///
    /// # Arguments
    ///
    /// * `state_id` - The change ID of the state to sign
    /// * `signer` - The signer to use
    ///
    /// # Errors
    ///
    /// Returns an error if the state is not found or signing fails.
    #[instrument(skip(self, signer), fields(state_id = %state_id.short()))]
    pub fn sign_state(&self, state_id: &ChangeId, signer: &dyn Signer) -> Result<()> {
        debug!("Signing state");

        let mut state = self
            .store
            .get_state(state_id)?
            .ok_or(HeddleError::StateNotFound(*state_id))?;

        state
            .sign(signer)
            .map_err(|error| HeddleError::Conflict(format!("failed to sign state: {error}")))?;

        self.store.put_state(&state)?;

        debug!(algorithm = signer.algorithm(), "State signed successfully");

        Ok(())
    }

    /// Sign a state using a key file.
    ///
    /// # Arguments
    ///
    /// * `state_id` - The change ID of the state to sign
    /// * `key_path` - Path to the private key file
    /// * `algorithm` - Optional algorithm hint (auto-detected if not specified)
    #[instrument(skip(self), fields(state_id = %state_id.short()))]
    pub fn sign_state_with_key(
        &self,
        state_id: &ChangeId,
        key_path: &std::path::Path,
        algorithm: Option<&str>,
    ) -> Result<()> {
        let signer =
            load_signer(key_path, algorithm).map_err(|e| HeddleError::Conflict(e.to_string()))?;

        self.sign_state(state_id, signer.as_ref())
    }

    /// Verify a state's signature.
    ///
    /// Returns the signature status:
    /// - `SignatureStatus::Valid` if the signature is valid
    /// - `SignatureStatus::Invalid` if the signature is invalid
    /// - `SignatureStatus::Unsigned` if the state has no signature
    ///
    /// # Arguments
    ///
    /// * `state_id` - The change ID of the state to verify
    #[instrument(skip(self), fields(state_id = %state_id.short()))]
    pub fn verify_state_signature(&self, state_id: &ChangeId) -> Result<SignatureStatus> {
        debug!("Verifying state signature");

        let state = self
            .store
            .get_state(state_id)?
            .ok_or(HeddleError::StateNotFound(*state_id))?;

        match &state.signature {
            Some(sig) => {
                let hash = state.compute_hash();
                match verify_state_signature_bytes(sig, &hash) {
                    Ok(()) => {
                        debug!("Signature is valid");
                        Ok(SignatureStatus::Valid)
                    }
                    Err(e) => {
                        debug!(error = %e, "Signature verification error");
                        Ok(SignatureStatus::Invalid)
                    }
                }
            }
            None => {
                debug!("State has no signature");
                Ok(SignatureStatus::Unsigned)
            }
        }
    }

    /// Get the signature of a state.
    ///
    /// Returns `None` if the state is not found or has no signature.
    pub fn get_state_signature(&self, state_id: &ChangeId) -> Result<Option<StateSignature>> {
        let state = self.store.get_state(state_id)?;

        match state {
            Some(s) => Ok(s.signature),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use crypto::Ed25519Signer;
    use objects::object::{Attribution, Principal};
    use tempfile::TempDir;

    use super::*;

    fn setup_repo() -> (TempDir, Repository) {
        let temp = TempDir::new().expect("create temp dir");
        let repo = Repository::init_default(temp.path()).expect("init repo");
        (temp, repo)
    }

    fn create_test_state(repo: &Repository) -> ChangeId {
        use objects::object::Tree;

        let tree = Tree::new();
        let tree_hash = repo.store().put_tree(&tree).expect("put tree");

        let attribution = Attribution::human(Principal::new("Test", "test@example.com"));
        let state = objects::object::State::new(tree_hash, vec![], attribution);
        repo.store().put_state(&state).expect("put state");
        state.change_id
    }

    #[test]
    fn test_sign_state() {
        let (_temp, repo) = setup_repo();
        let state_id = create_test_state(&repo);

        // Initially unsigned
        let status = repo.verify_state_signature(&state_id).expect("verify");
        assert_eq!(status, SignatureStatus::Unsigned);

        // Sign the state
        let signer = Ed25519Signer::generate().expect("generate key");
        repo.sign_state(&state_id, &signer).expect("sign state");

        // Now it should be valid
        let status = repo.verify_state_signature(&state_id).expect("verify");
        assert_eq!(status, SignatureStatus::Valid);
    }

    #[test]
    fn test_verify_invalid_signature() {
        let (_temp, repo) = setup_repo();
        let state_id = create_test_state(&repo);

        // Sign with one key
        let signer1 = Ed25519Signer::generate().expect("generate key");
        repo.sign_state(&state_id, &signer1).expect("sign state");

        // Tamper with the stored signature (simulate corruption)
        let mut state = repo
            .store()
            .get_state(&state_id)
            .expect("get state")
            .expect("state exists");

        if let Some(ref mut sig) = state.signature {
            // Flip a bit in the signature
            let mut sig_bytes = hex::decode(&sig.signature).expect("decode");
            sig_bytes[0] ^= 0xff;
            sig.signature = hex::encode(&sig_bytes);
        }

        repo.store().put_state(&state).expect("put state");

        // Should now be invalid
        let status = repo.verify_state_signature(&state_id).expect("verify");
        assert_eq!(status, SignatureStatus::Invalid);
    }

    #[test]
    fn test_get_state_signature() {
        let (_temp, repo) = setup_repo();
        let state_id = create_test_state(&repo);

        // No signature initially
        let sig = repo.get_state_signature(&state_id).expect("get signature");
        assert!(sig.is_none());

        // Sign the state
        let signer = Ed25519Signer::generate().expect("generate key");
        repo.sign_state(&state_id, &signer).expect("sign state");

        // Should have signature now
        let sig = repo.get_state_signature(&state_id).expect("get signature");
        assert!(sig.is_some());

        let sig = sig.expect("signature exists");
        assert_eq!(sig.algorithm(), "ed25519");
    }

    // ----- heddle#482: automatic state signing on the capture/merge path -----

    use std::sync::Mutex;

    /// Serializes the signing tests below — they manipulate the process-global
    /// `HEDDLE_HOME` to point the device-identity lookup at a per-test temp dir.
    static SIGNING_HOME_LOCK: Mutex<()> = Mutex::new(());

    /// Run `f` with `HEDDLE_HOME` pinned to `home` so the device-identity
    /// resolver reads a per-test directory. Restores the prior value after.
    fn with_signing_home<T>(home: &std::path::Path, f: impl FnOnce() -> T) -> T {
        let _guard = SIGNING_HOME_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::env::var_os("HEDDLE_HOME");
        unsafe {
            std::env::set_var("HEDDLE_HOME", home);
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        match previous {
            Some(value) => unsafe { std::env::set_var("HEDDLE_HOME", value) },
            None => unsafe { std::env::remove_var("HEDDLE_HOME") },
        }
        match result {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    fn sig_pubkey(state: &State) -> String {
        state
            .signature
            .as_ref()
            .expect("state is signed")
            .public_key
            .clone()
    }

    #[test]
    fn capture_auto_signs_with_local_identity() {
        let home = TempDir::new().expect("home temp");
        with_signing_home(home.path(), || {
            let (temp, repo) = setup_repo();
            std::fs::write(temp.path().join("file.txt"), "hello").expect("write file");

            let state = repo
                .snapshot(Some("first".to_string()), None)
                .expect("capture");

            // Every captured state is signed and verifies, with no auth login.
            assert!(state.signature.is_some(), "capture must auto-sign");
            assert_eq!(
                repo.verify_state_signature(&state.change_id)
                    .expect("verify"),
                SignatureStatus::Valid,
            );

            // The signing key is the per-repo auto-minted local identity.
            let local = crate::identity::load_or_mint_local(
                &temp.path().join(".heddle").join("identity.toml"),
            )
            .expect("read local identity");
            assert_eq!(sig_pubkey(&state), local.public_key);
        });
    }

    #[test]
    fn auth_login_reconciles_local_to_device_key() {
        let home = TempDir::new().expect("home temp");
        with_signing_home(home.path(), || {
            let (temp, repo) = setup_repo();

            // Pre-auth capture: signed by the auto-minted local key.
            std::fs::write(temp.path().join("a.txt"), "a").expect("write");
            let local_state = repo
                .snapshot(Some("local".to_string()), None)
                .expect("capture");
            let local_pubkey = sig_pubkey(&local_state);
            assert_eq!(
                repo.verify_state_signature(&local_state.change_id)
                    .expect("verify"),
                SignatureStatus::Valid,
            );

            // Simulate `heddle auth login`: mint + link a distinct device key.
            let device = Ed25519Signer::generate().expect("device key");
            let device_pubkey = hex::encode(device.public_key());
            crate::identity::link_device_key(
                device.public_key(),
                &device.to_pem().expect("device pem"),
                "grpc.example",
            )
            .expect("link device key");
            assert_ne!(device_pubkey, local_pubkey, "device key is distinct");

            // A fresh handle (fresh signer cache) now signs with the device key.
            let repo2 = Repository::open(temp.path()).expect("reopen repo");
            std::fs::write(temp.path().join("b.txt"), "b").expect("write");
            let device_state = repo2
                .snapshot(Some("device".to_string()), None)
                .expect("capture");
            assert_eq!(
                sig_pubkey(&device_state),
                device_pubkey,
                "post-login captures sign with the device key",
            );
            assert_eq!(
                repo2
                    .verify_state_signature(&device_state.change_id)
                    .expect("verify"),
                SignatureStatus::Valid,
            );

            // The prior local-signed state still verifies — reconciliation does
            // not invalidate it (its public key is embedded in the state).
            assert_eq!(
                repo2
                    .verify_state_signature(&local_state.change_id)
                    .expect("verify"),
                SignatureStatus::Valid,
            );
        });
    }

    /// login → capture (device-signed) → logout → capture (local-signed):
    /// `auth logout` unlinks the device identity so the next capture stops
    /// signing with the logged-out device key and falls back to the per-repo
    /// local key (heddle#482). Pre-fix, logout left `device-identity.toml` on
    /// disk, so this post-logout capture would still carry the device key —
    /// the gap this test pins shut. Reuses the same handle to also confirm the
    /// per-sign (uncached) resolution picks up the removal mid-session.
    #[test]
    fn logout_unlinks_device_key_so_capture_falls_back_to_local() {
        let home = TempDir::new().expect("home temp");
        with_signing_home(home.path(), || {
            let (temp, repo) = setup_repo();

            // Simulate `auth login --server grpc.S`: link a device key.
            let device = Ed25519Signer::generate().expect("device key");
            let device_pubkey = hex::encode(device.public_key());
            crate::identity::link_device_key(
                device.public_key(),
                &device.to_pem().expect("device pem"),
                "grpc.S",
            )
            .expect("link device key");

            // Post-login capture signs with the device key.
            std::fs::write(temp.path().join("a.txt"), "a").expect("write");
            let signed_in = repo
                .snapshot(Some("in".to_string()), None)
                .expect("capture");
            assert_eq!(
                sig_pubkey(&signed_in),
                device_pubkey,
                "post-login capture uses the device key",
            );

            // Simulate `auth logout grpc.S`: unlink the device identity.
            let removed = crate::identity::unlink_device_key("grpc.S").expect("unlink device key");
            assert!(
                removed,
                "logout removes the matching-server device identity"
            );

            // Subsequent capture on the SAME handle no longer uses the device
            // key — it falls back to the per-repo local key (a distinct key),
            // which still verifies.
            std::fs::write(temp.path().join("b.txt"), "b").expect("write");
            let signed_out = repo
                .snapshot(Some("out".to_string()), None)
                .expect("capture");
            assert_ne!(
                sig_pubkey(&signed_out),
                device_pubkey,
                "post-logout capture must not sign with the logged-out device key",
            );
            assert_eq!(
                repo.verify_state_signature(&signed_out.change_id)
                    .expect("verify"),
                SignatureStatus::Valid,
            );

            // Logout is idempotent: a second one finds nothing to remove.
            let again = crate::identity::unlink_device_key("grpc.S").expect("idempotent unlink");
            assert!(!again, "second logout finds nothing to remove");
        });
    }

    #[test]
    fn signature_survives_semantic_merge() {
        let home = TempDir::new().expect("home temp");
        with_signing_home(home.path(), || {
            let (temp, repo) = setup_repo();

            // Build a fork: A -> B on one side, A -> C on the other.
            std::fs::write(temp.path().join("file.txt"), "a").expect("write");
            let state_a = repo
                .snapshot(Some("a".to_string()), None)
                .expect("capture a");
            std::fs::write(temp.path().join("file.txt"), "b").expect("write");
            let state_b = repo
                .snapshot(Some("b".to_string()), None)
                .expect("capture b");

            repo.goto(&state_a.change_id).expect("goto a");
            std::fs::write(temp.path().join("side.txt"), "c").expect("write");
            let state_c = repo
                .snapshot(Some("c".to_string()), None)
                .expect("capture c");

            // Merge B into head C -> a real two-parent merge state.
            let attribution = Attribution::human(Principal::new("Merger", "merge@example.com"));
            let merge = repo
                .snapshot_merge_with_attribution(
                    &state_b.change_id,
                    Some("merge".to_string()),
                    None,
                    attribution,
                    None,
                    false,
                )
                .expect("merge");

            // The merge state carries its own signature.
            assert!(merge.signature.is_some(), "merge state must be signed");
            assert_eq!(
                repo.verify_state_signature(&merge.change_id)
                    .expect("verify"),
                SignatureStatus::Valid,
            );

            // Every parent's signature still verifies after the merge — merges
            // do not rewrite ancestors, so attribution survives.
            for parent in [&state_a, &state_b, &state_c] {
                assert_eq!(
                    repo.verify_state_signature(&parent.change_id)
                        .expect("verify parent"),
                    SignatureStatus::Valid,
                    "parent signature must survive the merge",
                );
            }
        });
    }

    #[test]
    fn capture_degrades_gracefully_when_signing_unavailable() {
        let home = TempDir::new().expect("home temp");
        with_signing_home(home.path(), || {
            let (temp, repo) = setup_repo();

            // Force the local-identity mint to fail by occupying its path with a
            // directory, so `read_to_string` errors and no key can be produced.
            std::fs::create_dir(temp.path().join(".heddle").join("identity.toml"))
                .expect("occupy identity path");

            std::fs::write(temp.path().join("file.txt"), "x").expect("write");
            // Capture must still succeed — signing is best-effort.
            let state = repo.snapshot(Some("x".to_string()), None).expect("capture");

            assert!(
                state.signature.is_none(),
                "degraded capture is unsigned, not silently signed",
            );
            assert_eq!(
                repo.verify_state_signature(&state.change_id)
                    .expect("verify"),
                SignatureStatus::Unsigned,
            );
        });
    }

    // ----- heddle#570: the unified re-sign decision (verify-old → try-local
    // -key → resign/skip) used by the fidelity backfill -----

    /// Build an unsigned root state for the re-sign tests.
    fn unsigned_state() -> State {
        use objects::object::Tree;
        let attribution = Attribution::human(Principal::new("Test", "test@example.com"));
        State::new(Tree::new().hash(), vec![], attribution)
    }

    /// An owned signature that was valid over the old hash is re-signed over the
    /// new hash after an authorized rewrite.
    #[test]
    fn resign_if_owned_resigns_active_owned_signature() {
        let home = TempDir::new().expect("home temp");
        with_signing_home(home.path(), || {
            let (_temp, repo) = setup_repo();
            let signer = repo.signing_signer().expect("local signer resolves");

            let mut state = unsigned_state();
            state.sign(&*signer).expect("sign with the repo's own key");
            let old_hash = state.compute_hash();

            // An authorized rewrite changes the content hash.
            state.created_at += chrono::Duration::seconds(1);
            assert_ne!(state.compute_hash(), old_hash, "rewrite changed the hash");

            assert_eq!(
                repo.resign_if_owned(&mut state, &[old_hash]),
                ResignOutcome::Resigned,
            );
            state
                .verify_signature()
                .expect("re-signed state verifies over the new hash");
        });
    }

    /// A state adopted+signed BEFORE the #565 format bump carries a signature
    /// made over its PRE-fidelity hash, not the post-bump `compute_hash()`. The
    /// #570 backfill must verify against that legacy hash (passed alongside the
    /// current one) and re-sign over the new hash — verifying only the post-bump
    /// hash would wrongly reject a valid legacy signature as unreproducible.
    #[test]
    fn resign_if_owned_accepts_legacy_pre_fidelity_signature() {
        let home = TempDir::new().expect("home temp");
        with_signing_home(home.path(), || {
            let (_temp, repo) = setup_repo();
            let signer = repo.signing_signer().expect("local signer resolves");

            // Simulate a pre-bump signature: sign the PRE-fidelity hash directly
            // (what the old code's `compute_hash` produced before #565 appended
            // the git-fidelity block).
            let mut state = unsigned_state();
            let legacy_hash = state.compute_hash_pre_fidelity();
            let post_bump_hash = state.compute_hash();
            assert_ne!(
                legacy_hash, post_bump_hash,
                "pre-fidelity hash differs from the post-bump hash",
            );
            state.signature = Some(
                crypto::state_signature_from_signer(&legacy_hash, &*signer)
                    .expect("sign legacy hash"),
            );

            // The backfill re-derives a fidelity field (here raw_message),
            // changing `compute_hash()`. The pre-fidelity hash is unchanged.
            let before = state.compute_hash();
            let before_pre_fidelity = state.compute_hash_pre_fidelity();
            assert_eq!(before_pre_fidelity, legacy_hash);
            let mut updated = state.clone().with_raw_message(b"legacy commit message\n");

            // Passing the legacy candidate recognises + re-signs the signature.
            assert_eq!(
                repo.resign_if_owned(&mut updated, &[before, before_pre_fidelity]),
                ResignOutcome::Resigned,
            );
            updated
                .verify_signature()
                .expect("re-signed legacy state verifies over the new hash");

            // Regression guard: verifying against ONLY the post-bump hash (the
            // pre-fix behaviour) rejects the valid legacy signature.
            let mut rejected = state.with_raw_message(b"legacy commit message\n");
            assert_eq!(
                repo.resign_if_owned(&mut rejected, &[before]),
                ResignOutcome::Unreproducible,
                "the post-bump hash alone does not verify a legacy signature",
            );
        });
    }

    /// A foreign pre-fidelity signature may be valid over the old hash, but
    /// this repo cannot reproduce the key. The deletion-wave backfill must not
    /// re-sign it as if this device authored it.
    #[test]
    fn resign_if_owned_refuses_foreign_pre_fidelity_signature() {
        let home = TempDir::new().expect("home temp");
        with_signing_home(home.path(), || {
            let (_temp, repo) = setup_repo();
            let foreign = Ed25519Signer::generate().expect("foreign key");

            let mut state = unsigned_state();
            let legacy_hash = state.compute_hash_pre_fidelity();
            state.signature = Some(
                crypto::state_signature_from_signer(&legacy_hash, &foreign)
                    .expect("foreign-sign legacy hash"),
            );
            let original_signature = state.signature.clone();

            let before = state.compute_hash();
            let before_pre_fidelity = state.compute_hash_pre_fidelity();
            let mut updated = state.with_raw_message(b"legacy commit message\n");

            assert_eq!(
                repo.resign_if_owned(&mut updated, &[before, before_pre_fidelity]),
                ResignOutcome::Unreproducible,
                "foreign pre-fidelity signatures need an explicit preserve/reject contract",
            );
            assert_eq!(
                updated.signature, original_signature,
                "foreign signature is left untouched, not re-signed by this repo",
            );
        });
    }

    /// A locally owned pre-fidelity signature that is corrupted must not be
    /// laundered into a fresh valid signature. Ownership alone is not enough:
    /// the old signature also has to verify over one of the caller-supplied old
    /// hash candidates.
    #[test]
    fn resign_if_owned_refuses_corrupted_pre_fidelity_signature() {
        let home = TempDir::new().expect("home temp");
        with_signing_home(home.path(), || {
            let (_temp, repo) = setup_repo();
            let signer = repo.signing_signer().expect("local signer resolves");

            let mut state = unsigned_state();
            let legacy_hash = state.compute_hash_pre_fidelity();
            let mut signature = crypto::state_signature_from_signer(&legacy_hash, &*signer)
                .expect("sign legacy hash");
            let mut bytes = hex::decode(&signature.signature).expect("decode sig");
            bytes[0] ^= 0xff;
            signature.signature = hex::encode(&bytes);
            state.signature = Some(signature.clone());

            let before = state.compute_hash();
            let before_pre_fidelity = state.compute_hash_pre_fidelity();
            let mut updated = state.with_raw_message(b"legacy commit message\n");

            assert_eq!(
                repo.resign_if_owned(&mut updated, &[before, before_pre_fidelity]),
                ResignOutcome::Unreproducible,
                "corrupted pre-fidelity signatures must not be laundered",
            );
            assert_eq!(
                updated.signature,
                Some(signature),
                "the corrupted signature stays visible for preserve/reject handling",
            );
        });
    }

    /// An owned signature that does NOT verify over the old hash (corrupted, or
    /// made over different content) must NOT be re-signed: re-signing would
    /// launder a never-valid signature into a fresh, valid-looking one. The
    /// state is left untouched and reported `Unreproducible` (heddle#570).
    #[test]
    fn resign_if_owned_refuses_to_launder_an_invalid_owned_signature() {
        let home = TempDir::new().expect("home temp");
        with_signing_home(home.path(), || {
            let (_temp, repo) = setup_repo();
            let signer = repo.signing_signer().expect("local signer resolves");

            let mut state = unsigned_state();
            state.sign(&*signer).expect("sign with the repo's own key");

            // Corrupt the signature so it no longer verifies — but keep the
            // public key intact so the OWNERSHIP check still matches.
            let corrupted = {
                let sig = state.signature.as_ref().expect("signed");
                let mut bytes = hex::decode(&sig.signature).expect("decode sig");
                bytes[0] ^= 0xff;
                hex::encode(&bytes)
            };
            if let Some(sig) = state.signature.as_mut() {
                sig.signature = corrupted.clone();
            }
            let old_hash = state.compute_hash();

            // The owner check matches (public key intact) but the old signature
            // does not verify over `old_hash`, so re-signing is refused.
            assert_eq!(
                repo.resign_if_owned(&mut state, &[old_hash]),
                ResignOutcome::Unreproducible,
            );
            assert_eq!(
                state.signature.as_ref().expect("still signed").signature,
                corrupted,
                "the state is left untouched, not re-signed",
            );
        });
    }

    /// A state signed with the per-repo LOCAL key before an `auth login` linked
    /// a device key is still owner-reproducible: the device key now supersedes
    /// the local key for new states, but `owning_signer_for` falls back to the
    /// local key, so the backfill re-signs it rather than declaring ownership
    /// lost (heddle#570).
    #[test]
    fn resign_if_owned_recognizes_local_key_after_device_link() {
        let home = TempDir::new().expect("home temp");
        with_signing_home(home.path(), || {
            let (_temp, repo) = setup_repo();

            // Sign with the local key (no device key linked yet).
            let local = repo.signing_signer().expect("local signer resolves");
            let local_pubkey = hex::encode(local.public_key());
            let mut state = unsigned_state();
            state.sign(&*local).expect("sign with local key");
            let old_hash = state.compute_hash();

            // `auth login`: link a DISTINCT device key, which now supersedes the
            // local key for new states.
            let device = Ed25519Signer::generate().expect("device key");
            let device_pubkey = hex::encode(device.public_key());
            assert_ne!(device_pubkey, local_pubkey, "device key is distinct");
            crate::identity::link_device_key(
                device.public_key(),
                &device.to_pem().expect("device pem"),
                "grpc.example",
            )
            .expect("link device key");
            assert_eq!(
                hex::encode(repo.signing_signer().expect("signer").public_key()),
                device_pubkey,
                "active signer is now the device key",
            );

            // An authorized rewrite, then re-sign: ownership is recognised via
            // the local key fallback and the state is re-signed (with the local
            // key it was originally signed by), keeping a valid signature.
            state.created_at += chrono::Duration::seconds(1);
            assert_eq!(
                repo.resign_if_owned(&mut state, &[old_hash]),
                ResignOutcome::Resigned,
            );
            state
                .verify_signature()
                .expect("re-signed state verifies over the new hash");
            assert_eq!(
                state.signature.as_ref().expect("signed").public_key,
                local_pubkey,
                "re-signed with the owning local key, not the device key",
            );
        });
    }

    /// A foreign signature (a key this repo does not control) is not re-signed
    /// even after trying the local-key fallback: ownership genuinely cannot be
    /// reproduced, so the state is left untouched (heddle#570).
    #[test]
    fn resign_if_owned_refuses_foreign_signature() {
        let home = TempDir::new().expect("home temp");
        with_signing_home(home.path(), || {
            let (_temp, repo) = setup_repo();

            let foreign = Ed25519Signer::generate().expect("foreign key");
            let mut state = unsigned_state();
            state.sign(&foreign).expect("foreign-sign");
            let old_hash = state.compute_hash();

            assert_eq!(
                repo.resign_if_owned(&mut state, &[old_hash]),
                ResignOutcome::Unreproducible,
            );
        });
    }

    /// An unsigned state needs no signature preserved.
    #[test]
    fn resign_if_owned_reports_unsigned() {
        let home = TempDir::new().expect("home temp");
        with_signing_home(home.path(), || {
            let (_temp, repo) = setup_repo();
            let mut state = unsigned_state();
            let old_hash = state.compute_hash();
            assert_eq!(
                repo.resign_if_owned(&mut state, &[old_hash]),
                ResignOutcome::Unsigned,
            );
        });
    }

    /// The permission gate is re-checked before EVERY sign (heddle#482): a
    /// signer minted while the identity file was `0600` must NOT survive a
    /// mid-session `chmod` that exposes the key. The pre-fix handle cached the
    /// signer on first capture, so a later capture on the same handle kept
    /// signing with the now-readable key — this test fails closed instead.
    #[cfg(unix)]
    #[test]
    fn perm_loosening_mid_session_fails_closed_without_cached_signer() {
        use std::os::unix::fs::PermissionsExt;

        let home = TempDir::new().expect("home temp");
        with_signing_home(home.path(), || {
            let (temp, repo) = setup_repo();
            let identity = temp.path().join(".heddle").join("identity.toml");

            // First capture on this handle mints the 0600 local key and signs.
            std::fs::write(temp.path().join("a.txt"), "a").expect("write");
            let first = repo
                .snapshot(Some("a".to_string()), None)
                .expect("capture a");
            assert!(
                first.signature.is_some(),
                "first capture signs with the freshly-minted 0600 key",
            );
            assert_eq!(
                std::fs::metadata(&identity)
                    .expect("identity metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600,
            );

            // Mid-session a restore/chmod exposes the private key.
            std::fs::set_permissions(&identity, std::fs::Permissions::from_mode(0o644))
                .expect("loosen perms");

            // The SAME handle must refuse to reuse a cached signer: the gate is
            // re-validated per sign, so this capture is unsigned-but-marked.
            std::fs::write(temp.path().join("b.txt"), "b").expect("write");
            let exposed = repo
                .snapshot(Some("b".to_string()), None)
                .expect("capture b");
            assert!(
                exposed.signature.is_none(),
                "an exposed key must make the next sign fail closed, not reuse a cached signer",
            );
            assert_eq!(
                repo.verify_state_signature(&exposed.change_id)
                    .expect("verify"),
                SignatureStatus::Unsigned,
            );

            // Re-securing the key restores signing on the very same handle.
            std::fs::set_permissions(&identity, std::fs::Permissions::from_mode(0o600))
                .expect("re-secure perms");
            std::fs::write(temp.path().join("c.txt"), "c").expect("write");
            let resecured = repo
                .snapshot(Some("c".to_string()), None)
                .expect("capture c");
            assert!(
                resecured.signature.is_some(),
                "re-securing the key lets the same handle sign again",
            );
            assert_eq!(
                repo.verify_state_signature(&resecured.change_id)
                    .expect("verify"),
                SignatureStatus::Valid,
            );
        });
    }
}
