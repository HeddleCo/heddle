//! Signature verification for explicit current acceptance of unchanged work.
//! A verified signature is not current permission or historical authorization.
use heddle_object_model::object::original_boundary_acceptance::{
    FORMAT, OriginalBoundaryAcceptance,
};
use serde::{Deserialize, Serialize};

use crate::{Ed25519Signer, Signer, thread_operation::Error};
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SignedBoundaryAcceptance {
    pub canonical: Vec<u8>,
    pub signature: Vec<u8>,
}
impl SignedBoundaryAcceptance {
    pub fn sign(value: &OriginalBoundaryAcceptance, signer: &impl Signer) -> Result<Self, Error> {
        if signer.public_key() != value.accepting_publisher {
            return Err(Error::Publisher);
        }
        let canonical = value.encode()?;
        let signature = signer.sign(&signing_bytes(&canonical))?;
        Ok(Self {
            canonical,
            signature,
        })
    }
    pub fn verify_signature(&self) -> Result<OriginalBoundaryAcceptance, Error> {
        let value = OriginalBoundaryAcceptance::decode(&self.canonical)?;
        Ed25519Signer::verify_with_public_key(
            &signing_bytes(&self.canonical),
            &value.accepting_publisher,
            &self.signature,
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
#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use heddle_object_model::object::{
        CollaborationActor, ContentHash, original_boundary_acceptance::BoundaryOriginalKind,
        thread_replication::SourceAuthor,
    };

    use super::*;
    #[test]
    fn boundary_acceptance_signature_is_distinct_and_binds_original_manifest() {
        let signer = Ed25519Signer::from_seed(&[33; 32]).expect("signer");
        let account = "00000000-0000-0000-0000-000000000001"
            .parse()
            .expect("account");
        let value = OriginalBoundaryAcceptance {
            version: 1,
            publication_intent: ContentHash::compute(b"intent"),
            originals_manifest: ContentHash::compute(b"manifest"),
            original_account: account,
            kinds: BTreeSet::from([BoundaryOriginalKind::Source]),
            accepting_publisher: signer.public_key().try_into().expect("key"),
            accepting_author: SourceAuthor::account(
                "00000000-0000-0000-0000-000000000002"
                    .parse()
                    .expect("Spool"),
                CollaborationActor {
                    principal_id: account,
                    agent_id: None,
                },
                vec![3],
            )
            .expect("authority claim"),
        };
        let signed = SignedBoundaryAcceptance::sign(&value, &signer).expect("signed acceptance");
        assert_eq!(
            signed.verify_signature().expect("verified signature"),
            value
        );
        let mut changed = value.clone();
        changed.originals_manifest = ContentHash::compute(b"changed manifest");
        assert!(
            SignedBoundaryAcceptance {
                canonical: changed.encode().expect("canonical changed"),
                signature: signed.signature.clone()
            }
            .verify_signature()
            .is_err(),
            "manifest must be signed"
        );
        assert!(
            SignedBoundaryAcceptance {
                canonical: signed.canonical.clone(),
                signature: signer
                    .sign(&signed.canonical)
                    .expect("wrong-domain signature")
            }
            .verify_signature()
            .is_err(),
            "plain original signature cannot become acceptance"
        );
    }
}
