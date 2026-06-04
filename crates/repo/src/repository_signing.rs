// SPDX-License-Identifier: Apache-2.0
//! State signing operations for Repository.

use std::sync::Arc;

use crypto::{Signer, StateSigningExt, load_signer, verify_state_signature_bytes};
use objects::object::{ChangeId, SignatureStatus, State, StateSignature};
use objects::store::ObjectStore;
use tracing::{debug, instrument};

use super::{HeddleError, Repository, Result};

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

            let state = repo.snapshot(Some("first".to_string()), None).expect("capture");

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
            let local_state = repo.snapshot(Some("local".to_string()), None).expect("capture");
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
            let device_state = repo2.snapshot(Some("device".to_string()), None).expect("capture");
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

    #[test]
    fn signature_survives_semantic_merge() {
        let home = TempDir::new().expect("home temp");
        with_signing_home(home.path(), || {
            let (temp, repo) = setup_repo();

            // Build a fork: A -> B on one side, A -> C on the other.
            std::fs::write(temp.path().join("file.txt"), "a").expect("write");
            let state_a = repo.snapshot(Some("a".to_string()), None).expect("capture a");
            std::fs::write(temp.path().join("file.txt"), "b").expect("write");
            let state_b = repo.snapshot(Some("b".to_string()), None).expect("capture b");

            repo.goto(&state_a.change_id).expect("goto a");
            std::fs::write(temp.path().join("side.txt"), "c").expect("write");
            let state_c = repo.snapshot(Some("c".to_string()), None).expect("capture c");

            // Merge B into head C -> a real two-parent merge state.
            let attribution = Attribution::human(Principal::new("Merger", "merge@example.com"));
            let merge = repo
                .snapshot_merge_with_attribution(
                    &state_b.change_id,
                    Some("merge".to_string()),
                    None,
                    attribution,
                    None,
                )
                .expect("merge");

            // The merge state carries its own signature.
            assert!(merge.signature.is_some(), "merge state must be signed");
            assert_eq!(
                repo.verify_state_signature(&merge.change_id).expect("verify"),
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
            let first = repo.snapshot(Some("a".to_string()), None).expect("capture a");
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
            let exposed = repo.snapshot(Some("b".to_string()), None).expect("capture b");
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
            let resecured = repo.snapshot(Some("c".to_string()), None).expect("capture c");
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
