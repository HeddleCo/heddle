// SPDX-License-Identifier: Apache-2.0
//! Portable Thread operation signatures, independent of storage and transport.
//! Verification proves the publisher and exact bytes. The receiving host must
//! still authorize the Thread/facet and validate accepted causal parents.

use heddle_object_model::{
    error::HeddleError,
    object::thread_replication::{OPERATION_FORMAT, ThreadOperation},
};
use serde::{Deserialize, Serialize};

use crate::{Ed25519Signer, Signer, SignerError};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Thread operation: {0}")]
    Operation(#[from] HeddleError),
    #[error("Thread signature: {0}")]
    Signature(#[from] SignerError),
    #[error("publisher differs from signing key")]
    Publisher,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedOperation {
    pub canonical: Vec<u8>,
    pub signature: Vec<u8>,
}

impl SignedOperation {
    pub fn sign(operation: &ThreadOperation, signer: &impl Signer) -> Result<Self, Error> {
        if signer.public_key() != operation.publisher {
            return Err(Error::Publisher);
        }
        let canonical = operation.encode()?;
        let signature = signer.sign(&signing_bytes(&canonical))?;
        Ok(Self {
            canonical,
            signature,
        })
    }

    pub fn verify(&self) -> Result<ThreadOperation, Error> {
        let operation = ThreadOperation::decode(&self.canonical)?;
        Ed25519Signer::verify_with_public_key(
            &signing_bytes(&self.canonical),
            &operation.publisher,
            &self.signature,
        )?;
        Ok(operation)
    }
}

fn signing_bytes(canonical: &[u8]) -> Vec<u8> {
    let mut bytes = OPERATION_FORMAT.as_bytes().to_vec();
    bytes.push(0);
    bytes.extend_from_slice(canonical);
    bytes
}
