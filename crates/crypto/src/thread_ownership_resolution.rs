//! The immutable local owner chooses a surviving claim and the recipient
//! independently accepts it with current account authority. Admission checks
//! that authority and the complete stored conflict/frontier separately.
use heddle_object_model::object::thread_replication::{
    ownership_claim::ThreadOwnershipClaim,
    ownership_resolution::{FORMAT, ThreadOwnershipResolution},
};
use serde::{Deserialize, Serialize};

use crate::{Ed25519Signer, Signer, thread_operation::Error};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedOwnershipResolution {
    pub canonical: Vec<u8>,
    pub local_signature: Vec<u8>,
    pub acceptance_signature: Vec<u8>,
}
impl SignedOwnershipResolution {
    pub fn sign(
        value: &ThreadOwnershipResolution,
        local_owner: &impl Signer,
        acceptor: &impl Signer,
    ) -> Result<Self, Error> {
        if local_owner.public_key() != value.local_owner
            || acceptor.public_key() != value.accepting_publisher
        {
            return Err(Error::Publisher);
        }
        let canonical = value.encode()?;
        let local_signature = local_owner.sign(&signing_bytes(&canonical))?;
        let acceptance_signature = acceptor.sign(&signing_bytes(&canonical))?;
        Ok(Self {
            canonical,
            local_signature,
            acceptance_signature,
        })
    }
    /// This proves both signatures and the selected claim's exact identity.
    /// Repository admission binds genesis, stored claims, frontier and current
    /// recipient capability; none may be inferred from these signatures alone.
    pub fn verify(
        &self,
        winning_claim: &ThreadOwnershipClaim,
    ) -> Result<ThreadOwnershipResolution, Error> {
        let value = ThreadOwnershipResolution::decode(&self.canonical)?;
        if winning_claim.id()? != value.winning_claim
            || winning_claim.prior_local_key != value.local_owner
            || winning_claim.thread != value.thread
            || winning_claim.account()? != value.account()?
        {
            return Err(Error::Publisher);
        }
        Ed25519Signer::verify_with_public_key(
            &signing_bytes(&self.canonical),
            &value.local_owner,
            &self.local_signature,
        )?;
        Ed25519Signer::verify_with_public_key(
            &signing_bytes(&self.canonical),
            &value.accepting_publisher,
            &self.acceptance_signature,
        )?;
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
    use heddle_object_model::object::{
        CollaborationActor, ContentHash,
        thread_replication::{SourceAuthor, ownership_claim::ThreadOwnershipClaim},
    };

    use super::*;

    fn fixture() -> (
        ThreadOwnershipClaim,
        ThreadOwnershipResolution,
        Ed25519Signer,
        Ed25519Signer,
    ) {
        let local = Ed25519Signer::from_seed(&[41; 32]).expect("local key");
        let acceptor = Ed25519Signer::from_seed(&[42; 32]).expect("recipient key");
        let spool = "00000000-0000-0000-0000-000000000022"
            .parse()
            .expect("UUID");
        let acceptance = SourceAuthor::account(
            spool,
            CollaborationActor {
                principal_id: "00000000-0000-0000-0000-000000000023"
                    .parse()
                    .expect("UUID"),
                agent_id: None,
            },
            vec![46; 32],
        )
        .expect("account acceptance");
        let claim = ThreadOwnershipClaim {
            version: 1,
            thread: ContentHash::from_bytes([43; 32]),
            prior_local_key: local.public_key().try_into().expect("key"),
            accepting_publisher: acceptor.public_key().try_into().expect("key"),
            acceptance: acceptance.clone(),
            source_frontier: [ContentHash::from_bytes([47; 32])].into(),
        };
        let winning_claim = claim.id().expect("claim id");
        let resolution = ThreadOwnershipResolution {
            version: 1,
            spool,
            thread: claim.thread,
            winning_claim,
            conflicting_claims: [winning_claim, ContentHash::from_bytes([48; 32])].into(),
            frontier: [ContentHash::from_bytes([49; 32])].into(),
            local_owner: local.public_key().try_into().expect("key"),
            accepting_publisher: acceptor.public_key().try_into().expect("key"),
            acceptance,
            occurred_at_ms: 1,
        };
        (claim, resolution, local, acceptor)
    }

    #[test]
    fn original_owner_and_fresh_recipient_must_both_sign_exact_resolution() {
        let (claim, resolution, local, acceptor) = fixture();
        let signed = SignedOwnershipResolution::sign(&resolution, &local, &acceptor)
            .expect("both signatures");
        assert_eq!(signed.verify(&claim).expect("verified"), resolution);
        let mut missing_local = signed.clone();
        missing_local.local_signature = vec![0; 64];
        assert!(missing_local.verify(&claim).is_err());
        let mut missing_acceptance = signed.clone();
        missing_acceptance.acceptance_signature = vec![0; 64];
        assert!(missing_acceptance.verify(&claim).is_err());
        let mut changed_choice = signed.clone();
        changed_choice.canonical = ThreadOwnershipResolution {
            winning_claim: ContentHash::from_bytes([48; 32]),
            ..resolution.clone()
        }
        .encode()
        .expect("changed choice");
        assert!(changed_choice.verify(&claim).is_err());
        assert!(
            Ed25519Signer::verify_with_public_key(
                &signed.canonical,
                &resolution.local_owner,
                &signed.local_signature,
            )
            .is_err(),
            "domain separation"
        );
    }

    #[test]
    fn recipient_key_is_independent_of_historical_claim_publisher() {
        let (mut claim, mut resolution, local, _) = fixture();
        let current_recipient = Ed25519Signer::from_seed(&[99; 32]).expect("current recipient");
        claim.accepting_publisher = [42; 32];
        resolution.winning_claim = claim.id().expect("changed claim");
        resolution.conflicting_claims =
            [resolution.winning_claim, ContentHash::from_bytes([48; 32])].into();
        resolution.accepting_publisher = current_recipient.public_key().try_into().expect("key");
        let signed = SignedOwnershipResolution::sign(&resolution, &local, &current_recipient)
            .expect("current acceptance");
        assert_eq!(signed.verify(&claim).expect("independent keys"), resolution);
    }
}
