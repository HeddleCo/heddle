//! Receiving a foreign account's original admission never enrolls that account
//! or its roots on this device. The caller supplies independent executor trust.
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
        admission.verify(original, envelope, trust)?;
        Self::create_with_proof(directory, original, envelope, Some(admission))
    }
}
impl ThreadReplica {
    /// A direct device may relay a foreign original receipt only when this
    /// receiver already pinned its hosted executor through an independent path.
    pub fn create_from_pinned_genesis_admission(
        directory: &Path,
        original: &SignedGenesis,
        envelope: &[u8],
        admission: &SignedGenesisAdmission,
    ) -> Result<Self> {
        use rusqlite::{OptionalExtension, params};
        let value = admission.verify_signature()?;
        let connection = crate::local_metadata::open(directory)?;
        let genesis: Option<Vec<u8>> = connection
            .query_row(
                "SELECT genesis FROM hosted_executor_pins WHERE spool=?1 AND executor=?2",
                params![value.spool.to_string(), value.executor],
                |row| row.get(0),
            )
            .optional()?;
        let genesis = genesis.ok_or_else(|| {
            super::Error::Invalid(
                "original account genesis requires independently pinned hosted executor".into(),
            )
        })?;
        let trust = TrustedHostedExecutor {
            spool: value.spool,
            spool_genesis: objects::object::ContentHash::from_bytes(
                genesis.as_slice().try_into().map_err(|_| {
                    super::Error::Invalid("invalid hosted executor genesis pin".into())
                })?,
            ),
            executor: value.executor,
        };
        Self::create_from_genesis_admission(directory, original, envelope, admission, &trust)
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
    fn foreign_genesis_receipt_persists_exactly_without_enrolling_account_roots() {
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
            version: 1,
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
        let replica = ThreadReplica::create_from_genesis_admission(
            repository.heddle_dir(),
            &original,
            envelope,
            &signed,
            &trust,
        )
        .expect("independent admission");
        let record = replica.genesis_record().expect("retained original proof");
        assert_eq!(record.creator_authority, envelope);
        assert_eq!(
            record
                .admission
                .as_ref()
                .expect("retained receipt")
                .canonical_record,
            signed.canonical
        );
        let replay = ThreadReplica::create_from_genesis_admission(
            repository.heddle_dir(),
            &original,
            envelope,
            &signed,
            &trust,
        )
        .expect("exact replay");
        assert_eq!(replay.genesis_record().expect("replay proof"), record);
        let mut conflicting = receipt.clone();
        conflicting.admitted_at_ms += 1;
        let conflicting = SignedGenesisAdmission::sign(&conflicting, &executor)
            .expect("different first admission");
        assert!(
            ThreadReplica::create_from_genesis_admission(
                repository.heddle_dir(),
                &original,
                envelope,
                &conflicting,
                &trust
            )
            .is_err(),
            "first admission cannot change on replay"
        );
        assert_eq!(
            ThreadReplica::open(repository.heddle_dir(), replica.thread_id())
                .expect("reopen")
                .genesis_record()
                .expect("proof after rejected replay"),
            record
        );
        let connection = crate::local_metadata::open(repository.heddle_dir()).expect("metadata");
        let pins: i64 = connection
            .query_row("SELECT count(*) FROM hosted_executor_pins", [], |r| {
                r.get(0)
            })
            .expect("pin count");
        assert_eq!(
            pins, 0,
            "genesis delivery does not create global executor pins"
        );
    }
}
