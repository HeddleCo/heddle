//! Executor-signed testimony with one fixed canonical format, without API or
//! storage dependencies. Neither a signature nor its carried key creates trust.
use heddle_object_model::object::{
    thread_genesis_admission::{FORMAT, ThreadGenesisAdmission},
    thread_replication::integration::TrustedHostedExecutor,
};
use serde::{Deserialize, Serialize};

use crate::{
    Ed25519Signer, Signer,
    thread_operation::{Error, SignedGenesis},
};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SignedGenesisAdmission {
    pub canonical: Vec<u8>,
    pub signature: Vec<u8>,
}
impl SignedGenesisAdmission {
    pub fn sign(value: &ThreadGenesisAdmission, signer: &impl Signer) -> Result<Self, Error> {
        if signer.public_key() != value.executor {
            return Err(Error::Publisher);
        }
        let canonical = value.encode()?;
        let signature = signer.sign(&signing_bytes(&canonical))?;
        Ok(Self {
            canonical,
            signature,
        })
    }
    /// Signature-only verification does not establish independent executor trust.
    pub fn verify_signature(&self) -> Result<ThreadGenesisAdmission, Error> {
        let value = ThreadGenesisAdmission::decode(&self.canonical)?;
        Ed25519Signer::verify_with_public_key(
            &signing_bytes(&self.canonical),
            &value.executor,
            &self.signature,
        )?;
        Ok(value)
    }
    pub fn verify(
        &self,
        original: &SignedGenesis,
        envelope: &[u8],
        trust: &TrustedHostedExecutor,
    ) -> Result<ThreadGenesisAdmission, Error> {
        let value = self.verify_signature()?;
        value.authorize(&original.verify()?, envelope, trust)?;
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

#[cfg(test)]
mod tests {
    use heddle_object_model::object::{
        ContentHash, StateId,
        thread_genesis_admission::ENVELOPE_FORMAT,
        thread_replication::{GenesisOwner, ThreadGenesis},
    };

    use super::*;
    #[test]
    fn genesis_admission_binds_original_owner_envelope_and_independent_executor() {
        let creator = Ed25519Signer::from_seed(&[3; 32]).expect("creator");
        let executor = Ed25519Signer::from_seed(&[4; 32]).expect("executor");
        let owner = "00000000-0000-0000-0000-000000000007"
            .parse()
            .expect("owner UUID");
        let spool = "00000000-0000-0000-0000-000000000008";
        let genesis = ThreadGenesis {
            version: 1,
            owner: GenesisOwner::Account(owner),
            spool: spool.to_string(),
            parent: None,
            base: StateId::from_bytes([1; 32]),
            name: "original".into(),
            intent: "authority".into(),
            creator: creator.public_key().try_into().expect("creator key"),
            nonce: vec![],
        };
        let original = SignedGenesis::sign(&genesis, &creator).expect("original signature");
        let envelope = b"exact original authority envelope";
        let trust = TrustedHostedExecutor {
            spool: spool.parse().expect("Spool UUID"),
            spool_genesis: ContentHash::from_bytes([5; 32]),
            executor: executor.public_key().try_into().expect("executor key"),
        };
        let value = ThreadGenesisAdmission {
            version: 1,
            spool: spool.parse().expect("Spool UUID"),
            spool_genesis: trust.spool_genesis,
            thread: genesis.id().expect("Thread"),
            owner,
            creator: genesis.creator,
            authority_digest: ContentHash::compute_typed(ENVELOPE_FORMAT, envelope),
            executor: trust.executor,
            admitted_at_ms: 1,
        };
        let signed = SignedGenesisAdmission::sign(&value, &executor).expect("admission");
        assert_eq!(
            signed
                .verify(&original, envelope, &trust)
                .expect("verified"),
            value
        );
        assert!(
            signed
                .verify(&original, b"another envelope", &trust)
                .is_err()
        );
        let mut foreign = trust.clone();
        foreign.executor = [9; 32];
        assert!(signed.verify(&original, envelope, &foreign).is_err());
        let mut altered = genesis.clone();
        altered.owner = GenesisOwner::Account(
            "00000000-0000-0000-0000-000000000009"
                .parse()
                .expect("other owner"),
        );
        let altered = SignedGenesis::sign(&altered, &creator).expect("different owner signed");
        assert!(signed.verify(&altered, envelope, &trust).is_err());
        let mut altered = original.clone();
        altered.signature[0] ^= 1;
        assert!(signed.verify(&altered, envelope, &trust).is_err());
        let mut altered = signed.clone();
        altered.signature[0] ^= 1;
        assert!(altered.verify(&original, envelope, &trust).is_err());
    }
}
