//! Receiving a foreign account's original admission never enrolls that account
//! or its roots on this device. HYBRID installation requires a complete witness
//! set and public authority bundle through `install_hybrid_import`.
use std::path::Path;

use crypto::{thread_genesis_admission::SignedGenesisAdmission, thread_operation::SignedGenesis};
use objects::object::thread_replication::integration::TrustedHostedExecutor;

use super::{Result, ThreadReplica};
impl ThreadReplica {
    pub fn create_from_genesis_admission(
        directory: &Path,
        original: &SignedGenesis,
        envelope: &[u8],
        admission: &SignedGenesisAdmission,
        trust: &TrustedHostedExecutor,
    ) -> Result<Self> {
        let _ = (directory, original, envelope, trust);
        admission.verify_signature()?;
        Err(super::Error::WitnessEvidenceRequired)
    }
}
impl ThreadReplica {
    /// A stored executor pin supplies no authority. Use `install_hybrid_import`
    /// with selected lineage, a fresh witness set and all original dependencies.
    pub fn create_from_pinned_genesis_admission(
        directory: &Path,
        original: &SignedGenesis,
        envelope: &[u8],
        admission: &SignedGenesisAdmission,
    ) -> Result<Self> {
        let _ = (directory, original, envelope);
        admission.verify_signature()?;
        Err(super::Error::WitnessEvidenceRequired)
    }
}

#[cfg(test)]
mod tests {
    use crypto::{Ed25519Signer, Signer};
    use objects::object::{
        ContentHash,
        thread_genesis_admission::{ENVELOPE_FORMAT, ThreadGenesisAdmission},
        thread_replication::{GenesisOwner, ThreadGenesis},
    };

    use super::*;
    #[test]
    fn bare_foreign_genesis_receipt_cannot_enroll_roots_or_install() {
        let directory = tempfile::tempdir().expect("repository");
        let repository = crate::Repository::init_default(directory.path()).expect("repository");
        let creator = Ed25519Signer::from_seed(&[13; 32]).expect("creator");
        let executor = Ed25519Signer::from_seed(&[14; 32]).expect("executor");
        let spool = uuid::Uuid::from_u128(8);
        let owner = uuid::Uuid::from_u128(7);
        let genesis = ThreadGenesis {
            version: 1,
            owner: GenesisOwner::Account(owner),
            spool: spool.to_string(),
            parent: None,
            base: repository.head().expect("head").expect("initial"),
            name: "foreign".into(),
            intent: "original author".into(),
            creator: creator.public_key().try_into().expect("key"),
            nonce: vec![],
        };
        let original = SignedGenesis::sign(&genesis, &creator).expect("original");
        let envelope = b"exact independently admitted original envelope";
        let trust = TrustedHostedExecutor {
            spool,
            spool_genesis: ContentHash::from_bytes([15; 32]),
            executor: executor.public_key().try_into().expect("key"),
        };
        let receipt = ThreadGenesisAdmission {
            version: 2,
            basis: objects::object::original_boundary_acceptance::AdmissionBasis::OriginalAuthority,
            spool,
            spool_genesis: trust.spool_genesis,
            thread: genesis.id().expect("id"),
            owner,
            creator: genesis.creator,
            authority_digest: ContentHash::compute_typed(ENVELOPE_FORMAT, envelope),
            executor: trust.executor,
            admitted_at_ms: 10,
        };
        let signed = SignedGenesisAdmission::sign(&receipt, &executor).expect("receipt");
        assert!(
            ThreadReplica::create(repository.heddle_dir(), &original).is_err(),
            "foreign account cannot use local genesis constructor"
        );
        assert!(
            ThreadReplica::create_from_pinned_genesis_admission(
                repository.heddle_dir(),
                &original,
                envelope,
                &signed
            )
            .is_err(),
            "carried executor never creates a pin"
        );
        signed
            .verify(&original, envelope, &trust)
            .expect("valid surrounding original signatures");
        assert!(matches!(
            ThreadReplica::create_from_genesis_admission(
                repository.heddle_dir(),
                &original,
                envelope,
                &signed,
                &trust
            ),
            Err(super::super::Error::WitnessEvidenceRequired)
        ));
        assert!(ThreadReplica::open(repository.heddle_dir(), genesis.id().expect("id")).is_err());
        assert!(
            !repository
                .heddle_dir()
                .join("owner-authorization.bin")
                .exists()
        );
    }
}
