// SPDX-License-Identifier: Apache-2.0
//! Signing extension trait for core State.

use objects::object::State;

use crate::{
    Signer,
    state_signature::{
        StateSignatureError, state_signature_from_signer, verify_state_signature_bytes,
    },
};

pub trait StateSigningExt {
    fn sign(&mut self, signer: &dyn Signer) -> Result<(), StateSignatureError>;
    fn verify_signature(&self) -> Result<(), StateSignatureError>;
}

impl StateSigningExt for State {
    fn sign(&mut self, signer: &dyn Signer) -> Result<(), StateSignatureError> {
        let hash = self.compute_hash();
        self.signature = Some(state_signature_from_signer(&hash, signer)?);
        Ok(())
    }

    fn verify_signature(&self) -> Result<(), StateSignatureError> {
        match &self.signature {
            Some(signature) => verify_state_signature_bytes(signature, &self.compute_hash()),
            None => Err(StateSignatureError::InvalidSignature(
                "state has no signature".to_string(),
            )),
        }
    }
}