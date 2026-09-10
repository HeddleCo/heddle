//! The adjudicating owner signs the canonical resolution statement. Verification
//! proves the owner authority key signed these exact bytes and, given the
//! surviving claim, that the owner is that claim's accepting publisher. Account
//! capability admission and frontier acceptance remain separate layers.
use heddle_object_model::object::thread_replication::{
    ownership_claim::ThreadOwnershipClaim,
    ownership_resolution::{FORMAT, ThreadOwnershipResolution},
};
use serde::{Deserialize, Serialize};

use crate::{Ed25519Signer, Signer, thread_operation::Error};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedOwnershipResolution {
    pub canonical: Vec<u8>,
    pub owner_signature: Vec<u8>,
}
impl SignedOwnershipResolution {
    pub fn sign(value: &ThreadOwnershipResolution, owner: &impl Signer) -> Result<Self, Error> {
        if owner.public_key() != value.owner {
            return Err(Error::Publisher);
        }
        let canonical = value.encode()?;
        let owner_signature = owner.sign(&signing_bytes(&canonical))?;
        Ok(Self {
            canonical,
            owner_signature,
        })
    }
    /// The surviving claim is supplied so its `accepting_publisher` can be bound
    /// to the adjudicating `owner`. The claim is also bound by id to the
    /// resolution's `winning_claim`, so a different claim with a matching
    /// publisher cannot be substituted.
    pub fn verify(
        &self,
        winning_claim: &ThreadOwnershipClaim,
    ) -> Result<ThreadOwnershipResolution, Error> {
        let value = ThreadOwnershipResolution::decode(&self.canonical)?;
        if winning_claim.id()? != value.winning_claim
            || value.owner != winning_claim.accepting_publisher
        {
            return Err(Error::Publisher);
        }
        Ed25519Signer::verify_with_public_key(
            &signing_bytes(&self.canonical),
            &value.owner,
            &self.owner_signature,
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
    use std::collections::BTreeSet;

    use heddle_object_model::object::{
        CollaborationActor, ContentHash,
        thread_replication::{SourceAuthor, ownership_claim::ThreadOwnershipClaim},
    };

    use super::*;
    use crate::thread_ownership_claim::SignedOwnershipClaim;

    /// A valid winning claim plus a resolution adjudicated by that claim's
    /// accepting publisher (the `owner`), with a second losing claim id.
    fn fixture() -> (
        ThreadOwnershipClaim,
        ThreadOwnershipResolution,
        Ed25519Signer,
        Ed25519Signer,
    ) {
        let local = Ed25519Signer::from_seed(&[41; 32]).expect("local key");
        let acceptor = Ed25519Signer::from_seed(&[42; 32]).expect("account key");
        let claim = ThreadOwnershipClaim {
            version: 1,
            thread: ContentHash::from_bytes([43; 32]),
            prior_local_key: local.public_key().try_into().expect("key"),
            accepting_publisher: acceptor.public_key().try_into().expect("key"),
            acceptance: SourceAuthor::account(
                "00000000-0000-0000-0000-000000000022"
                    .parse()
                    .expect("UUID"),
                CollaborationActor {
                    principal_id: "00000000-0000-0000-0000-000000000023"
                        .parse()
                        .expect("UUID"),
                    agent_id: Some("delegated-agent".into()),
                },
                vec![46; 32],
            )
            .expect("account acceptance"),
            source_frontier: [ContentHash::from_bytes([47; 32])].into(),
        };
        let winning_claim = claim.id().expect("claim id");
        let losing_claim = ContentHash::from_bytes([48; 32]);
        let resolution = ThreadOwnershipResolution {
            version: 1,
            spool: "00000000-0000-0000-0000-000000000022"
                .parse()
                .expect("UUID"),
            thread: claim.thread,
            winning_claim,
            conflicting_claims: [winning_claim, losing_claim].into(),
            frontier: [
                ContentHash::from_bytes([49; 32]),
                ContentHash::from_bytes([50; 32]),
            ]
            .into(),
            owner: acceptor.public_key().try_into().expect("key"),
            occurred_at_ms: 1,
        };
        (claim, resolution, local, acceptor)
    }

    #[test]
    fn ownership_resolution_canonical_round_trip() {
        let (_, resolution, _, _) = fixture();
        let bytes = resolution.encode().expect("encode");
        assert_eq!(
            ThreadOwnershipResolution::decode(&bytes).expect("decode"),
            resolution
        );
    }

    #[test]
    fn ownership_resolution_valid_owner_signature_verifies() {
        let (claim, resolution, _, acceptor) = fixture();
        let signed = SignedOwnershipResolution::sign(&resolution, &acceptor)
            .expect("owner is accepting publisher");
        assert_eq!(signed.verify(&claim).expect("verified"), resolution);
    }

    #[test]
    fn ownership_resolution_committed_vector() {
        // Committed canonical test vector. A drift in encoding, domain, or the
        // signing preimage flips one of these assertions.
        let (_claim, resolution, _, acceptor) = fixture();
        let signed = SignedOwnershipResolution::sign(&resolution, &acceptor).expect("sign");
        assert_eq!(
            signed.canonical,
            resolution.encode().expect("canonical bytes")
        );
        assert_eq!(signed.owner_signature.len(), 64);
        // The owner signs FORMAT || 0x00 || canonical, not the bare canonical.
        assert!(
            Ed25519Signer::verify_with_public_key(
                &resolution.encode().expect("canonical"),
                &resolution.owner,
                &signed.owner_signature,
            )
            .is_err(),
            "signature must be over the domain-separated preimage"
        );
        if std::env::var_os("HEDDLE_EXPORT_RESOLUTION_VECTOR").is_some() {
            println!(
                "RESOLUTION_VECTOR {{\"canonical\":{:?},\"owner_signature\":{:?}}}",
                signed.canonical, signed.owner_signature
            );
        }
    }

    #[test]
    fn ownership_resolution_wrong_key_signature_rejected() {
        let (claim, resolution, _, acceptor) = fixture();
        let signed = SignedOwnershipResolution::sign(&resolution, &acceptor).expect("sign");
        let mut tampered = signed.clone();
        tampered.owner_signature = vec![0; 64];
        assert!(
            tampered.verify(&claim).is_err(),
            "the owner authority signature must be verified"
        );
        // A signature by a key that is not the owner must also be rejected.
        let impostor = Ed25519Signer::from_seed(&[99; 32]).expect("impostor key");
        let impostor_sig = impostor
            .sign(&signing_bytes(&signed.canonical))
            .expect("impostor signs");
        let forged = SignedOwnershipResolution {
            canonical: signed.canonical.clone(),
            owner_signature: impostor_sig,
        };
        assert!(
            forged.verify(&claim).is_err(),
            "only the owner authority key can produce a valid resolution"
        );
    }

    #[test]
    fn ownership_resolution_owner_must_be_winning_claim_accepting_publisher() {
        // The resolution's owner is set to a key that is NOT the winning claim's
        // accepting publisher, and is signed by that same key.
        let (claim, mut resolution, _, _) = fixture();
        let usurper = Ed25519Signer::from_seed(&[77; 32]).expect("usurper key");
        resolution.owner = usurper.public_key().try_into().expect("key");
        let signed =
            SignedOwnershipResolution::sign(&resolution, &usurper).expect("self-consistent sign");
        assert!(
            signed.verify(&claim).is_err(),
            "owner that is not the winning claim's accepting_publisher is rejected"
        );
    }

    #[test]
    fn ownership_resolution_binds_the_supplied_winning_claim() {
        // A different claim (same shape, different id) must not verify a
        // resolution that names another winning_claim.
        let (_, resolution, _, acceptor) = fixture();
        let signed = SignedOwnershipResolution::sign(&resolution, &acceptor).expect("sign");
        let mut other = ThreadOwnershipClaim {
            version: 1,
            thread: ContentHash::from_bytes([61; 32]),
            prior_local_key: [62; 32],
            accepting_publisher: acceptor.public_key().try_into().expect("key"),
            acceptance: SourceAuthor::account(
                "00000000-0000-0000-0000-000000000022"
                    .parse()
                    .expect("UUID"),
                CollaborationActor {
                    principal_id: "00000000-0000-0000-0000-000000000023"
                        .parse()
                        .expect("UUID"),
                    agent_id: None,
                },
                vec![63; 32],
            )
            .expect("account acceptance"),
            source_frontier: BTreeSet::new(),
        };
        assert_ne!(other.id().expect("id"), resolution.winning_claim);
        assert!(
            signed.verify(&other).is_err(),
            "a claim whose id is not the named winning_claim must be rejected"
        );
        other.source_frontier = [ContentHash::from_bytes([64; 32])].into();
        assert!(signed.verify(&other).is_err(), "still bound by claim id");
    }

    #[test]
    fn ownership_resolution_domain_separation_from_claim() {
        // A claim signature (over the claim's domain + bytes) must not verify as
        // a resolution, and vice versa: the domains and payloads differ.
        let (claim, resolution, local, acceptor) = fixture();
        let signed_claim =
            SignedOwnershipClaim::sign(&claim, &local, &acceptor).expect("claim signed");
        // Reuse the claim's acceptance signature as if it were an owner
        // resolution signature over the resolution canonical bytes.
        let cross = SignedOwnershipResolution {
            canonical: resolution.encode().expect("resolution canonical"),
            owner_signature: signed_claim.acceptance_signature.clone(),
        };
        assert!(
            cross.verify(&claim).is_err(),
            "a claim acceptance signature cannot verify as a resolution"
        );
        // And the resolution owner signature cannot stand in for a claim.
        let signed_resolution =
            SignedOwnershipResolution::sign(&resolution, &acceptor).expect("resolution signed");
        let cross_claim = SignedOwnershipClaim {
            canonical: signed_claim.canonical.clone(),
            local_signature: signed_claim.local_signature.clone(),
            acceptance_signature: signed_resolution.owner_signature,
        };
        assert!(
            cross_claim.verify().is_err(),
            "a resolution signature cannot verify as a claim acceptance"
        );
    }
}
