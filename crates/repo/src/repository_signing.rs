// SPDX-License-Identifier: Apache-2.0
//! State signing operations for Repository.

use crypto::{Signer, StateSigningExt, load_signer, verify_state_signature_bytes};
use objects::object::{ChangeId, SignatureStatus, StateSignature};
use objects::store::ObjectStore;
use tracing::{debug, instrument};

use super::{HeddleError, Repository, Result};

impl Repository {
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
}
