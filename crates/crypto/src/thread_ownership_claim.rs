//! Both parties sign the same canonical ownership statement. A courier signature
//! is never substituted for either the current local owner or account acceptor.
use serde::{Deserialize, Serialize};
use heddle_object_model::object::thread_replication::ownership_claim::{FORMAT, ThreadOwnershipClaim};
use crate::{Ed25519Signer, Signer, thread_operation::Error};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedOwnershipClaim {
    pub canonical: Vec<u8>,
    pub local_signature: Vec<u8>,
    pub acceptance_signature: Vec<u8>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedOwnershipAcceptance {
    pub canonical: Vec<u8>,
    pub signature: Vec<u8>,
}
impl SignedOwnershipAcceptance {
    pub fn sign(value: &ThreadOwnershipClaim, acceptor: &impl Signer) -> Result<Self, Error> {
        if acceptor.public_key() != value.accepting_publisher { return Err(Error::Publisher); }
        let canonical = value.encode()?;
        let signature = acceptor.sign(&signing_bytes(&canonical))?;
        Ok(Self { canonical, signature })
    }
    pub fn verify(&self) -> Result<ThreadOwnershipClaim, Error> {
        let value = ThreadOwnershipClaim::decode(&self.canonical)?;
        Ed25519Signer::verify_with_public_key(&signing_bytes(&self.canonical), &value.accepting_publisher, &self.signature)?;
        Ok(value)
    }
    /// Caller must independently admit target-account authority and exact local
    /// ownership/frontier before invoking this narrowly typed co-sign operation.
    pub fn cosign(&self, local: &impl Signer) -> Result<SignedOwnershipClaim, Error> {
        let value = self.verify()?;
        if local.public_key() != value.prior_local_key { return Err(Error::Publisher); }
        Ok(SignedOwnershipClaim { canonical: self.canonical.clone(),
            local_signature: local.sign(&signing_bytes(&self.canonical))?,
            acceptance_signature: self.signature.clone(),
        })
    }
}
impl SignedOwnershipClaim {
    pub fn sign(value: &ThreadOwnershipClaim, local: &impl Signer, acceptor: &impl Signer) -> Result<Self, Error> {
        if local.public_key() != value.prior_local_key || acceptor.public_key() != value.accepting_publisher {
            return Err(Error::Publisher);
        }
        let canonical = value.encode()?;
        let bytes = signing_bytes(&canonical);
        Ok(Self { canonical, local_signature: local.sign(&bytes)?, acceptance_signature: acceptor.sign(&bytes)? })
    }
    pub fn verify(&self) -> Result<ThreadOwnershipClaim, Error> {
        let value = ThreadOwnershipClaim::decode(&self.canonical)?;
        let bytes = signing_bytes(&self.canonical);
        Ed25519Signer::verify_with_public_key(&bytes, &value.prior_local_key, &self.local_signature)?;
        Ed25519Signer::verify_with_public_key(&bytes, &value.accepting_publisher, &self.acceptance_signature)?;
        Ok(value)
    }
}
pub fn signing_bytes(canonical: &[u8]) -> Vec<u8> {
    let mut bytes = FORMAT.as_bytes().to_vec();
    bytes.push(0);
    bytes.extend_from_slice(canonical);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use heddle_object_model::object::{CollaborationActor, ContentHash, thread_replication::SourceAuthor};
    fn fixture() -> (ThreadOwnershipClaim, Ed25519Signer, Ed25519Signer) {
        let local = Ed25519Signer::from_seed(&[31; 32]).expect("local key");
        let acceptor = Ed25519Signer::from_seed(&[32; 32]).expect("account key");
        let claim = ThreadOwnershipClaim {
            version: 1, thread: ContentHash::from_bytes([33; 32]),
            prior_local_key: local.public_key().try_into().expect("key"),
            accepting_publisher: acceptor.public_key().try_into().expect("key"),
            acceptance: SourceAuthor::account("00000000-0000-0000-0000-000000000022".parse().expect("UUID"), CollaborationActor {
                principal_id: "00000000-0000-0000-0000-000000000023".parse().expect("UUID"), agent_id: Some("delegated-agent".into()),
            }, vec![36; 32]).expect("account acceptance"),
            source_frontier: [ContentHash::from_bytes([37;32])].into(),
        };
        (claim, local, acceptor)
    }
    #[test]
    fn ownership_claim_browser_vector() {
        let (claim, local, acceptor) = fixture();
        let proof = SignedOwnershipClaim::sign(&claim, &local, &acceptor).expect("independent Rust fixture");
        assert_eq!(proof.verify().expect("verified vector"), claim);
        if std::env::var_os("HEDDLE_EXPORT_OWNERSHIP_VECTOR").is_some() {
            println!("OWNERSHIP_VECTOR {{\"canonical\":{:?},\"acceptance_signature\":{:?}}}",proof.canonical,proof.acceptance_signature);
        }
    }
    #[test]
    fn ownership_claim_requires_both_exact_statement_signatures() {
        let (claim, local, acceptor) = fixture();
        let signed = SignedOwnershipClaim::sign(&claim, &local, &acceptor).expect("both signatures");
        assert_eq!(signed.verify().expect("verified"), claim);
        let mut missing_owner = signed.clone();
        missing_owner.local_signature = vec![0;64];
        assert!(missing_owner.verify().is_err(), "local owner signature must be verified");
        let mut missing_acceptor = signed.clone();
        missing_acceptor.acceptance_signature = vec![0;64];
        assert!(missing_acceptor.verify().is_err(), "accepting account signature must be verified");
        for mutate in 0..5 {
            let mut changed = claim.clone();
            match mutate {
                0 => changed.thread = ContentHash::from_bytes([38;32]),
                1 => changed.source_frontier.clear(),
                2 => changed.prior_local_key = acceptor.public_key().try_into().expect("key"),
                3 => changed.accepting_publisher = local.public_key().try_into().expect("key"),
                _ => { let SourceAuthor::Account { actor, .. } = &mut changed.acceptance else { panic!("account"); }; actor.principal_id = "00000000-0000-0000-0000-000000000027".parse().expect("UUID"); }
            }
            let tampered = SignedOwnershipClaim { canonical: changed.encode().expect("valid different statement"), ..signed.clone() };
            assert!(tampered.verify().is_err(), "both signatures bind Thread/account/keys/cutoff");
        }
    }
    #[test]
    fn local_integration_signature_binds_original_account_author() {
        use heddle_object_model::object::{State, Tree, Attribution, Principal, VisibilityTier,
            thread_replication::{ThreadOperation, ThreadOperationBody, local_integration::LocalIntegration}};
        use crate::thread_operation::SignedOperation;
        let (claim, _, signer) = fixture();
        let state = State::new_snapshot(Tree::new().hash(), vec![], Attribution::human(Principal::new("source author", "")));
        let receipt = LocalIntegration {
            version: 1, spool: "00000000-0000-0000-0000-000000000022".parse().expect("Spool"),
            device: signer.public_key().try_into().expect("key"), author: claim.acceptance,
            source_thread: ContentHash::from_bytes([41;32]), source_operation: ContentHash::from_bytes([42;32]),
            source_revision: state.id(), target_thread: ContentHash::from_bytes([43;32]),
            expected_target_frontier: Default::default(), result: state.encode_current_msgpack().expect("State").into(),
            result_visibility: VisibilityTier::Internal, initiating_request_proof: ContentHash::from_bytes([44;32]),
            local_policy_version: ContentHash::from_bytes([45;32]), executed_at_ms: 1,
        };
        let operation = ThreadOperation { version: 1, thread: receipt.target_thread,
            parents: Default::default(), publisher: receipt.device,
            body: ThreadOperationBody::LocalIntegration(receipt.encode().expect("authored integration")),
        };
        let signed = SignedOperation::sign(&operation, &signer).expect("original signature");
        assert_eq!(signed.verify().expect("signed integration").source_author().expect("author"), Some(receipt.author.clone()));
        let mut changed = receipt;
        changed.author = SourceAuthor::LocalKey;
        let changed_operation = ThreadOperation { body: ThreadOperationBody::LocalIntegration(changed.encode().expect("different author")), ..operation };
        let substituted = SignedOperation { canonical: changed_operation.encode().expect("different source claim"), ..signed };
        assert!(substituted.verify().is_err(), "integration original author must be signed");
    }

}
