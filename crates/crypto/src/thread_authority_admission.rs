//! Executor-signed testimony with one fixed canonical format, without API or
//! storage dependencies. Neither a signature nor its carried key creates trust.
use heddle_object_model::object::{
    thread_authority_admission::{FORMAT, ThreadAuthorityAdmission},
    thread_replication::integration::TrustedHostedExecutor,
};
use serde::{Deserialize, Serialize};

use crate::{
    Ed25519Signer, Signer,
    thread_operation::{Error, SignedOperation},
};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SignedAuthorityAdmission {
    pub canonical: Vec<u8>,
    pub signature: Vec<u8>,
    pub boundary_acceptance:
        Option<std::sync::Arc<crate::original_boundary_acceptance::SignedBoundaryAcceptance>>,
}
impl SignedAuthorityAdmission {
    pub fn sign(value: &ThreadAuthorityAdmission, signer: &impl Signer) -> Result<Self, Error> {
        if signer.public_key() != value.executor {
            return Err(Error::Publisher);
        }
        let canonical = value.encode()?;
        let signature = signer.sign(&signing_bytes(&canonical))?;
        Ok(Self {
            canonical,
            signature,
            boundary_acceptance: None,
        })
    }
    /// Signature-only verification does not establish independent executor trust.
    pub fn verify_signature(&self) -> Result<ThreadAuthorityAdmission, Error> {
        let value = ThreadAuthorityAdmission::decode(&self.canonical)?;
        Ed25519Signer::verify_with_public_key(
            &signing_bytes(&self.canonical),
            &value.executor,
            &self.signature,
        )?;
        Ok(value)
    }
    pub fn verify(
        &self,
        original: &SignedOperation,
        trust: &TrustedHostedExecutor,
    ) -> Result<ThreadAuthorityAdmission, Error> {
        let value = self.verify_signature()?;
        let evidence = self
            .boundary_acceptance
            .as_ref()
            .map(|value| value.verify_signature())
            .transpose()?;
        value.authorize_with_acceptance(&original.verify()?, trust, evidence.as_ref())?;
        Ok(value)
    }
    pub fn verify_claim(
        &self,
        original: &crate::thread_ownership_claim::SignedOwnershipClaim,
        genesis: &heddle_object_model::object::thread_replication::ThreadGenesis,
        trust: &TrustedHostedExecutor,
    ) -> Result<ThreadAuthorityAdmission, Error> {
        let value = self.verify_signature()?;
        let evidence = self
            .boundary_acceptance
            .as_ref()
            .map(|value| value.verify_signature())
            .transpose()?;
        value.authorize_claim_with_acceptance(
            &original.verify()?,
            genesis,
            trust,
            evidence.as_ref(),
        )?;
        Ok(value)
    }
}
fn signing_bytes(canonical: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(FORMAT.len() + 1 + canonical.len());
    bytes.extend_from_slice(FORMAT.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(canonical);
    bytes
}
