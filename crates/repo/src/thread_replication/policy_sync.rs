//! Local publication consent names an owning account and a Thread once. Later
//! accepted policy revisions from that account govern ongoing native sync;
//! merely receiving another collaborator's policy never authorizes disclosure.
use std::collections::BTreeSet;

use objects::object::{
    ContentHash,
    thread_replication::{
        ThreadFacet, ThreadOperationBody,
        metadata::{Control, Property, SharedFacet, ThreadControl, property_version},
    },
};
use rusqlite::{OptionalExtension, params};

use super::{Error, Result, ThreadReplica};
impl ThreadReplica {
    /// Only an authenticated interactive owner/delegate SetSharingPolicy calls
    /// this. Incoming replication never establishes local publication consent.
    pub fn consent_to_policy_sync(&self, account: uuid::Uuid) -> Result<()> {
        let mut connection = self.connect()?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT account FROM native_policy_consent WHERE thread=?1",
                [self.thread.as_bytes()],
                |row| row.get(0),
            )
            .optional()?;
        if existing
            .as_ref()
            .is_some_and(|old| old != &account.to_string())
        {
            return Err(Error::Invalid(
                "Thread publication consent belongs to another account".into(),
            ));
        }
        let changed = tx.execute(
            "INSERT OR IGNORE INTO native_policy_consent(thread,account) VALUES(?1,?2)",
            params![self.thread.as_bytes(), account.to_string()],
        )?;
        if changed > 0 {
            tx.execute(
                "UPDATE threads SET generation=generation+1 WHERE id=?1",
                [self.thread.as_bytes()],
            )?;
        }
        tx.commit()?;
        self.notify_committed()
    }
    pub(super) fn policy_sync_sharing(
        &self,
        destination: &[u8; 32],
    ) -> Result<Option<(BTreeSet<ThreadFacet>, Option<ContentHash>)>> {
        let mut connection = self.connect()?;
        let tx = connection.transaction()?;
        let account: Option<String> = tx
            .query_row(
                "SELECT account FROM native_policy_consent WHERE thread=?1",
                [self.thread.as_bytes()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(account) = account else {
            return Ok(None);
        };
        // Read only IDs when a field conflicts. A large authority envelope is
        // loaded once only when there is exactly one candidate to apply.
        let ids=tx.prepare("SELECT operation FROM thread_control_heads WHERE thread=?1 AND property='sharing' ORDER BY operation LIMIT 129")?.query_map([self.thread.as_bytes()],|row|row.get::<_,Vec<u8>>(0))?.map(|row|super::hash(&row?)).collect::<Result<BTreeSet<_>>>()?;
        let version = property_version(self.thread, &Property::Sharing, &ids)?;
        let closed = Some((BTreeSet::new(), Some(version)));
        if ids.len() != 1 {
            return Ok(closed);
        }
        let id = ids
            .first()
            .ok_or_else(|| Error::Invalid("sharing head missing".into()))?;
        let signed=tx.query_row("SELECT canonical,signature FROM operations WHERE thread=?1 AND id=?2 AND status=1 AND authority_admitted=1",params![self.thread.as_bytes(),id.as_bytes()],|row|Ok(crypto::thread_operation::SignedOperation{canonical:row.get(0)?,signature:row.get(1)?}))?;
        tx.commit()?;
        let operation = signed.verify()?;
        let ThreadOperationBody::Metadata(bytes) = operation.body else {
            return Err(Error::Invalid("sharing index facet".into()));
        };
        let control = ThreadControl::decode(&bytes)?;
        if control.actor.principal_id.to_string() != account {
            return Ok(closed);
        };
        let Control::Sharing(policy) = control.control else {
            return Err(Error::Invalid("sharing index property".into()));
        };
        if !policy.ongoing {
            return Ok(closed);
        };
        let facets = policy
            .destinations
            .iter()
            .filter(|entry| entry.endpoint == *destination)
            .flat_map(|entry| entry.facets.iter())
            .filter_map(|facet| match facet {
                SharedFacet::Source => Some(ThreadFacet::Source),
                SharedFacet::Collaboration => Some(ThreadFacet::Discussion),
                SharedFacet::Metadata => Some(ThreadFacet::Metadata),
                SharedFacet::Evidence | SharedFacet::ScrubbedTimeline => None,
            })
            .collect();
        Ok(Some((facets, Some(version))))
    }
}

#[cfg(test)]
mod tests {
    use crypto::{
        Ed25519Signer, Signer,
        thread_operation::{SignedGenesis, SignedOperation},
    };
    use objects::object::{
        CollaborationActor,
        thread_replication::{
            ThreadGenesis, ThreadOperation,
            metadata::{AUTHORITY_FORMAT, Destination, EndpointKind, SharingPolicy},
        },
    };

    use super::*;
    #[test]
    fn consent_enables_later_owner_policy_without_enabling_foreign_or_conflicting_policy() {
        let directory = tempfile::tempdir().expect("repo");
        let repository = crate::Repository::init_default(directory.path()).expect("repo");
        let signer = Ed25519Signer::from_seed(&[39; 32]).expect("signer");
        let account = uuid::Uuid::from_u128(12);
        let spool = uuid::Uuid::from_u128(13);
        let genesis = ThreadGenesis {
            version: 1,
            spool: spool.to_string(),
            parent: None,
            base: repository.head().expect("head").expect("base"),
            name: "policy".into(),
            intent: "bounded sync".into(),
            creator: signer.public_key().try_into().expect("key"),
            owner: objects::object::thread_replication::GenesisOwner::LocalKey(
                signer.public_key().try_into().expect("key"),
            ),
            nonce: vec![1],
        };
        let replica = ThreadReplica::create(
            repository.heddle_dir(),
            &SignedGenesis::sign(&genesis, &signer).expect("genesis"),
        )
        .expect("replica");
        let make = |actor, parents: BTreeSet<ContentHash>, endpoint| {
            let proof = b"authority already admitted by the receive gate".to_vec();
            let control = ThreadControl {
                version: 1,
                spool,
                actor: CollaborationActor {
                    principal_id: actor,
                    agent_id: None,
                },
                authority_digest: ContentHash::compute_typed(AUTHORITY_FORMAT, &proof),
                authority_envelope: proof,
                client_operation_id: uuid::Uuid::now_v7(),
                occurred_at_ms: 0,
                control: Control::Sharing(SharingPolicy {
                    ongoing: true,
                    destinations: vec![Destination {
                        endpoint,
                        kind: EndpointKind::Weft,
                        spool,
                        facets: BTreeSet::from([SharedFacet::Source, SharedFacet::Metadata]),
                    }],
                }),
            };
            SignedOperation::sign(
                &ThreadOperation {
                    version: 1,
                    thread: replica.thread_id(),
                    parents,
                    publisher: signer.public_key().try_into().expect("key"),
                    body: ThreadOperationBody::Metadata(control.encode().expect("policy")),
                },
                &signer,
            )
            .expect("signed policy")
        };
        let first = make(account, BTreeSet::new(), [1; 32]);
        replica
            .receive(&first, repository.store(), |_| Ok(()))
            .expect("authorized first policy");
        assert!(
            replica
                .sharing(&[1; 32])
                .expect("private default")
                .0
                .is_empty(),
            "receiving signed policy is not initial device publication consent"
        );
        replica
            .consent_to_policy_sync(account)
            .expect("interactive owner consent");
        assert_eq!(
            replica.sharing(&[1; 32]).expect("consented export").0,
            BTreeSet::from([ThreadFacet::Source, ThreadFacet::Metadata])
        );
        let first_id = first.verify().expect("verify").id().expect("ID");
        let successor = make(account, BTreeSet::from([first_id]), [2; 32]);
        replica
            .receive(&successor, repository.store(), |_| Ok(()))
            .expect("later authorized owner policy");
        assert!(
            replica
                .sharing(&[1; 32])
                .expect("removed destination")
                .0
                .is_empty()
        );
        assert!(
            !replica
                .sharing(&[2; 32])
                .expect("new destination")
                .0
                .is_empty(),
            "later owner policy takes effect without another local consent action"
        );
        let foreign = make(
            uuid::Uuid::from_u128(99),
            BTreeSet::from([successor.verify().expect("verify").id().expect("ID")]),
            [3; 32],
        );
        replica
            .receive(&foreign, repository.store(), |_| Ok(()))
            .expect("collaborator policy admitted for shared metadata");
        assert!(
            replica
                .sharing(&[3; 32])
                .expect("foreign policy")
                .0
                .is_empty(),
            "collaborator authority cannot expand private device consent"
        );
        let concurrent = make(account, BTreeSet::from([first_id]), [4; 32]);
        replica
            .receive(&concurrent, repository.store(), |_| Ok(()))
            .expect("concurrent owner policy");
        assert!(
            replica
                .sharing(&[4; 32])
                .expect("conflicted policy")
                .0
                .is_empty(),
            "concurrent policy candidates do not choose a winner"
        );
        assert!(
            replica
                .consent_to_policy_sync(uuid::Uuid::from_u128(99))
                .is_err(),
            "consent cannot be reassigned to another account"
        );
    }
}
