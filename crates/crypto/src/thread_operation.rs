// SPDX-License-Identifier: Apache-2.0
//! Portable Thread operation signatures, independent of storage and transport.
//! Verification proves the publisher and exact bytes. The receiving host must
//! still authorize the Thread/facet and validate accepted causal parents.

use heddle_object_model::{
    error::HeddleError,
    object::thread_replication::{
        GENESIS_FORMAT, OPERATION_FORMAT, ThreadGenesis, ThreadOperation,
    },
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
        let signature = signer.sign(&signing_bytes(OPERATION_FORMAT, &canonical))?;
        Ok(Self {
            canonical,
            signature,
        })
    }

    pub fn verify(&self) -> Result<ThreadOperation, Error> {
        let operation = ThreadOperation::decode(&self.canonical)?;
        Ed25519Signer::verify_with_public_key(
            &signing_bytes(OPERATION_FORMAT, &self.canonical),
            &operation.publisher,
            &self.signature,
        )?;
        Ok(operation)
    }
}

fn signing_bytes(format: &str, canonical: &[u8]) -> Vec<u8> {
    let mut bytes = format.as_bytes().to_vec();
    bytes.push(0);
    bytes.extend_from_slice(canonical);
    bytes
}

/// Original creation record, signed once by its creator. A later publisher can
/// relay this record without changing Thread identity or claiming authorship.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedGenesis {
    pub canonical: Vec<u8>,
    pub signature: Vec<u8>,
}
impl SignedGenesis {
    pub fn sign(genesis: &ThreadGenesis, signer: &impl Signer) -> Result<Self, Error> {
        if signer.public_key() != genesis.creator {
            return Err(Error::Publisher);
        }
        let canonical = genesis.encode()?;
        let signature = signer.sign(&signing_bytes(GENESIS_FORMAT, &canonical))?;
        Ok(Self {
            canonical,
            signature,
        })
    }
    pub fn verify(&self) -> Result<ThreadGenesis, Error> {
        let genesis = ThreadGenesis::decode(&self.canonical)?;
        Ed25519Signer::verify_with_public_key(
            &signing_bytes(GENESIS_FORMAT, &self.canonical),
            &genesis.creator,
            &self.signature,
        )?;
        Ok(genesis)
    }
}
