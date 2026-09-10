//! Pinned executor testimony, not a simulated current credential authorization.
//! These tests deliberately need no original bearer or account root enrollment.
use std::collections::BTreeSet;

use crypto::{
    Ed25519Signer, Signer,
    original_boundary_acceptance::SignedBoundaryAcceptance,
    thread_authority_admission::SignedAuthorityAdmission,
    thread_genesis_admission::SignedGenesisAdmission,
    thread_operation::{SignedGenesis, SignedOperation},
    thread_ownership_claim::SignedOwnershipClaim,
};
use objects::{
    object::{
        Attribution, CollaborationActor, ContentHash, Principal, State,
        original_boundary_acceptance::{
            AdmissionBasis, BoundaryOriginalKind, OriginalBoundaryAcceptance,
        },
        thread_authority_admission::{OriginalAuthoritySubject, ThreadAuthorityAdmission},
        thread_genesis_admission::{ENVELOPE_FORMAT, ThreadGenesisAdmission},
        thread_replication::{
            AuthoredCapture, GenesisOwner, SourceAuthor, ThreadGenesis, ThreadOperation,
            ThreadOperationBody, integration::TrustedHostedExecutor,
            ownership_claim::ThreadOwnershipClaim,
        },
    },
    store::ObjectStore,
};

use super::*;

fn key(seed: u8) -> Ed25519Signer {
    Ed25519Signer::from_seed(&[seed; 32]).expect("fixture key")
}
fn evidence(
    spool: uuid::Uuid,
    account: uuid::Uuid,
    kind: BoundaryOriginalKind,
) -> SignedBoundaryAcceptance {
    let signer = key(19);
    SignedBoundaryAcceptance::sign(
        &OriginalBoundaryAcceptance {
            version: 1,
            publication_intent: ContentHash::compute(b"exact original publication intent"),
            originals_manifest: ContentHash::compute(
                b"exact original manifest committed by pinned executor",
            ),
            original_account: account,
            kinds: [kind].into(),
            accepting_publisher: signer.public_key().try_into().expect("key"),
            accepting_author: SourceAuthor::account(
                spool,
                CollaborationActor {
                    principal_id: account,
                    agent_id: Some("current-acceptor".into()),
                },
                vec![71; 64 * 1024],
            )
            .expect("acceptance author"),
        },
        &signer,
    )
    .expect("signed explicit acceptance")
}
fn basis(evidence: &SignedBoundaryAcceptance) -> AdmissionBasis {
    AdmissionBasis::BoundaryAcceptance {
        acceptance: evidence
            .verify_signature()
            .expect("acceptance")
            .id()
            .expect("ID"),
    }
}
fn pin(replica: &ThreadReplica, trust: &TrustedHostedExecutor) {
    // Explicit test fixture pin, never extracted from an incoming record.
    replica
        .connect()
        .expect("connection")
        .execute(
            "INSERT OR IGNORE INTO hosted_executor_pins(spool,genesis,executor) VALUES(?1,?2,?3)",
            rusqlite::params![
                trust.spool.to_string(),
                trust.spool_genesis.as_bytes(),
                trust.executor
            ],
        )
        .expect("independent fixture pin");
}
fn setup() -> (
    tempfile::TempDir,
    crate::Repository,
    ThreadGenesis,
    SignedGenesis,
    TrustedHostedExecutor,
) {
    let directory = tempfile::tempdir().expect("repository");
    let repository = crate::Repository::init_default(directory.path()).expect("repo");
    let creator = key(17);
    let executor = key(18);
    let spool = uuid::Uuid::from_u128(741);
    let account = uuid::Uuid::from_u128(742);
    let genesis = ThreadGenesis {
        version: 1,
        spool: spool.to_string(),
        parent: None,
        base: repository.head().expect("head").expect("base"),
        name: "retained acceptance".into(),
        intent: "unchanged original".into(),
        creator: creator.public_key().try_into().expect("key"),
        owner: GenesisOwner::Account(account),
        nonce: vec![1],
    };
    let original = SignedGenesis::sign(&genesis, &creator).expect("original genesis");
    let trust = TrustedHostedExecutor {
        spool,
        spool_genesis: ContentHash::compute(b"independently selected Spool genesis"),
        executor: executor.public_key().try_into().expect("key"),
    };
    (directory, repository, genesis, original, trust)
}
fn genesis_receipt(
    genesis: &ThreadGenesis,
    trust: &TrustedHostedExecutor,
) -> SignedGenesisAdmission {
    let GenesisOwner::Account(account) = genesis.owner else {
        panic!("account genesis")
    };
    let evidence = evidence(trust.spool, account, BoundaryOriginalKind::AccountGenesis);
    let statement = ThreadGenesisAdmission {
        version: 2,
        basis: basis(&evidence),
        spool: trust.spool,
        spool_genesis: trust.spool_genesis,
        thread: genesis.id().expect("id"),
        owner: account,
        creator: genesis.creator,
        authority_digest: ContentHash::compute_typed(
            ENVELOPE_FORMAT,
            b"original expired genesis envelope",
        ),
        executor: trust.executor,
        admitted_at_ms: 9000,
    };
    let mut signed = SignedGenesisAdmission::sign(&statement, &key(18)).expect("executor receipt");
    signed.boundary_acceptance = Some(std::sync::Arc::new(evidence));
    signed
}
#[test]
fn boundary_genesis_receipt_requires_evidence_and_survives_reopen() {
    let (_directory, repository, genesis, original, trust) = setup();
    let receipt = genesis_receipt(&genesis, &trust);
    assert!(
        receipt
            .verify_signature()
            .expect("signed basis")
            .authorize(&genesis, b"original expired genesis envelope", &trust)
            .is_err(),
        "signature-only genesis path must reject boundary basis"
    );
    let mut missing = receipt.clone();
    missing.boundary_acceptance = None;
    assert!(
        ThreadReplica::create_from_genesis_admission(
            repository.heddle_dir(),
            &original,
            b"original expired genesis envelope",
            &missing,
            &trust
        )
        .err()
        .expect("missing evidence")
        .to_string()
        .contains("basis evidence")
    );
    assert!(ThreadReplica::open(repository.heddle_dir(), genesis.id().expect("id")).is_err());
    let mut wrong = trust.clone();
    wrong.executor = [33; 32];
    assert!(
        receipt
            .verify(&original, b"original expired genesis envelope", &wrong)
            .is_err(),
        "carried evidence must not establish executor trust"
    );
    let replica = ThreadReplica::create_from_genesis_admission(
        repository.heddle_dir(),
        &original,
        b"original expired genesis envelope",
        &receipt,
        &trust,
    )
    .expect("independently pinned genesis admission");
    let reopen = ThreadReplica::open(repository.heddle_dir(), replica.thread_id()).expect("reopen");
    let wrapper = reopen.genesis_record().expect("export exact evidence");
    assert_eq!(wrapper.boundary_acceptances.len(), 1);
    assert_eq!(
        wrapper.boundary_acceptances[0].canonical_record,
        receipt
            .boundary_acceptance
            .as_ref()
            .expect("acceptance")
            .canonical
    );
    assert_eq!(
        wrapper.genesis.expect("original").canonical_record,
        original.canonical
    );
    let mut changed = receipt.verify_signature().expect("statement");
    changed.admitted_at_ms += 1;
    let mut changed = SignedGenesisAdmission::sign(&changed, &key(18)).expect("later statement");
    changed.boundary_acceptance = receipt.boundary_acceptance.clone();
    assert!(
        ThreadReplica::create_from_genesis_admission(
            repository.heddle_dir(),
            &original,
            b"original expired genesis envelope",
            &changed,
            &trust
        )
        .is_err(),
        "first genesis receipt stays immutable"
    );
}
#[test]
fn boundary_source_receipt_is_atomic_and_relayable_without_original_authority() {
    let (_directory, repository, genesis, original_genesis, trust) = setup();
    let replica = ThreadReplica::create_from_genesis_admission(
        repository.heddle_dir(),
        &original_genesis,
        b"original expired genesis envelope",
        &genesis_receipt(&genesis, &trust),
        &trust,
    )
    .expect("genesis");
    let GenesisOwner::Account(account) = genesis.owner else {
        panic!("account")
    };
    let base = repository
        .store()
        .get_state(&genesis.base)
        .expect("store")
        .expect("base");
    let state = State::new_snapshot(
        base.tree,
        vec![genesis.base],
        Attribution::human(Principal::new("offline original", "")),
    );
    let author = SourceAuthor::account(
        trust.spool,
        CollaborationActor {
            principal_id: account,
            agent_id: Some("revoked-original".into()),
        },
        b"unchanged expired or revoked credential".to_vec(),
    )
    .expect("original provenance");
    let SourceAuthor::Account {
        actor,
        authority_digest,
        ..
    } = &author
    else {
        panic!("account")
    };
    let operation = ThreadOperation {
        version: 1,
        thread: replica.thread_id(),
        parents: BTreeSet::new(),
        publisher: genesis.creator,
        body: ThreadOperationBody::Capture(AuthoredCapture {
            result: state.encode_current_msgpack().expect("state").into(),
            author: author.clone(),
        }),
    };
    let original = SignedOperation::sign(&operation, &key(17)).expect("old original signature");
    let acceptance = evidence(trust.spool, account, BoundaryOriginalKind::Source);
    let statement = ThreadAuthorityAdmission {
        version: 3,
        basis: basis(&acceptance),
        spool: trust.spool,
        spool_genesis: trust.spool_genesis,
        thread: replica.thread_id(),
        subject: OriginalAuthoritySubject::Operation(operation.id().expect("id")),
        actor: actor.clone(),
        publisher: operation.publisher,
        authority_digest: *authority_digest,
        executor: trust.executor,
        admitted_at_ms: 9001,
    };
    let mut receipt = SignedAuthorityAdmission::sign(&statement, &key(18)).expect("receipt");
    receipt.boundary_acceptance = Some(std::sync::Arc::new(acceptance));
    assert!(
        statement.authorize(&operation, &trust).is_err(),
        "signature-only old authorize path must reject boundary basis"
    );
    assert!(
        replica
            .receive_with_authority_admission(&original, &receipt, repository.store(), |_| Ok(()))
            .expect_err("no self enrolled executor")
            .to_string()
            .contains("independently pinned")
    );
    pin(&replica, &trust);
    let mut missing = receipt.clone();
    missing.boundary_acceptance = None;
    assert!(
        replica
            .receive_with_authority_admission(&original, &missing, repository.store(), |_| Ok(()))
            .expect_err("missing evidence")
            .to_string()
            .contains("basis evidence")
    );
    let mut damaged = receipt.clone();
    std::sync::Arc::make_mut(damaged.boundary_acceptance.as_mut().expect("evidence")).signature
        [0] ^= 1;
    assert!(
        replica
            .receive_with_authority_admission(&original, &damaged, repository.store(), |_| Ok(()))
            .is_err(),
        "evidence signature required"
    );
    replica.connect().expect("connection").execute_batch("CREATE TRIGGER boundary_rollback BEFORE UPDATE OF generation ON threads BEGIN SELECT RAISE(ABORT,'boundary transaction rollback'); END;").expect("rollback seam");
    assert!(
        replica
            .receive_with_authority_admission(&original, &receipt, repository.store(), |_| Ok(()))
            .expect_err("rollback")
            .to_string()
            .contains("boundary transaction rollback")
    );
    assert!(
        replica
            .operation(&operation.id().expect("id"))
            .expect("lookup")
            .is_none()
    );
    let count: i64 = replica
        .connect()
        .expect("connection")
        .query_row(
            "SELECT count(*) FROM boundary_admission_evidence WHERE receipt=?1",
            [receipt.canonical.as_slice()],
            |row| row.get(0),
        )
        .expect("evidence count");
    assert_eq!(count, 0, "failed source leaves no acceptance reference");
    replica
        .connect()
        .expect("connection")
        .execute_batch("DROP TRIGGER boundary_rollback")
        .expect("restore trigger");
    assert_eq!(
        replica
            .receive_with_authority_admission(&original, &receipt, repository.store(), |_| Ok(()))
            .expect("admit pinned testimony"),
        Admission::Accepted
    );
    let reopened =
        ThreadReplica::open(repository.heddle_dir(), replica.thread_id()).expect("reopen");
    let stored = reopened
        .operation_with_authority_admission(&operation.id().expect("id"))
        .expect("lookup")
        .expect("retained");
    assert_eq!(stored.original, original);
    assert_eq!(stored.authority_admission, Some(receipt.clone()));
    let ancestry = reopened
        .source_ancestry(operation.id().expect("id"), 128, 1024 * 1024)
        .expect("bounded export");
    assert_eq!(ancestry[0].authority_admission, Some(receipt.clone()));
    let mut later = statement.clone();
    later.admitted_at_ms += 1;
    let mut later =
        SignedAuthorityAdmission::sign(&later, &key(18)).expect("later valid statement");
    later.boundary_acceptance = receipt.boundary_acceptance.clone();
    reopened
        .receive_with_authority_admission(&original, &later, repository.store(), |_| Ok(()))
        .expect("replay");
    assert_eq!(
        reopened
            .operation_with_authority_admission(&operation.id().expect("id"))
            .expect("lookup")
            .expect("stored")
            .authority_admission,
        Some(receipt.clone())
    );
    let mut prior_id = operation.id().expect("first ID");
    let mut prior_revision = state.id();
    for index in 1..128 {
        let next = State::new_snapshot(
            base.tree,
            vec![prior_revision],
            Attribution::human(Principal::new(format!("offline {index}"), "")),
        );
        let operation = ThreadOperation {
            version: 1,
            thread: replica.thread_id(),
            parents: [prior_id].into(),
            publisher: genesis.creator,
            body: ThreadOperationBody::Capture(AuthoredCapture {
                result: next.encode_current_msgpack().expect("state").into(),
                author: author.clone(),
            }),
        };
        let signed = SignedOperation::sign(&operation, &key(17)).expect("source signature");
        let statement = ThreadAuthorityAdmission {
            subject: OriginalAuthoritySubject::Operation(operation.id().expect("ID")),
            ..statement.clone()
        };
        let mut next_receipt =
            SignedAuthorityAdmission::sign(&statement, &key(18)).expect("receipt");
        next_receipt.boundary_acceptance = receipt.boundary_acceptance.clone();
        assert_eq!(
            reopened
                .receive_with_authority_admission(
                    &signed,
                    &next_receipt,
                    repository.store(),
                    |_| Ok(())
                )
                .expect("next accepted original"),
            Admission::Accepted
        );
        prior_id = operation.id().expect("ID");
        prior_revision = next.id();
    }
    let shared = reopened
        .source_ancestry(prior_id, 128, 1024 * 1024)
        .expect("128 originals with one distinct64KiB acceptance fit1MiB");
    assert_eq!(shared.len(), 128);
    let first = shared[0]
        .authority_admission
        .as_ref()
        .expect("receipt")
        .boundary_acceptance
        .as_ref()
        .expect("evidence");
    for item in &shared {
        assert!(
            std::sync::Arc::ptr_eq(
                first,
                item.authority_admission
                    .as_ref()
                    .expect("receipt")
                    .boundary_acceptance
                    .as_ref()
                    .expect("evidence")
            ),
            "SQL hydration must own shared acceptance bytes only once"
        );
    }
    assert!(
        reopened.source_ancestry(prior_id, 128, 1024).is_err(),
        "aggregate source/evidence budget remains enforced"
    );
}
#[test]
fn boundary_claim_receipt_preserves_dual_signatures_and_explicit_cutoff() {
    let (_directory, repository, mut genesis, _, trust) = setup();
    genesis.owner = GenesisOwner::LocalKey(genesis.creator);
    let original = SignedGenesis::sign(&genesis, &key(17)).expect("local original");
    let replica = ThreadReplica::create(repository.heddle_dir(), &original).expect("unclaimed");
    let account = uuid::Uuid::from_u128(742);
    let claim = ThreadOwnershipClaim {
        version: 1,
        thread: replica.thread_id(),
        prior_local_key: genesis.creator,
        accepting_publisher: key(20)
            .public_key()
            .try_into()
            .expect("original claim acceptor"),
        acceptance: SourceAuthor::account(
            trust.spool,
            CollaborationActor {
                principal_id: account,
                agent_id: Some("old-claim-actor".into()),
            },
            b"old acceptance authority".to_vec(),
        )
        .expect("original acceptance"),
        source_frontier: BTreeSet::new(),
    };
    let signed =
        SignedOwnershipClaim::sign(&claim, &key(17), &key(20)).expect("dual original signatures");
    let SourceAuthor::Account {
        actor,
        authority_digest,
        ..
    } = &claim.acceptance
    else {
        panic!("account")
    };
    let acceptance = evidence(trust.spool, account, BoundaryOriginalKind::OwnershipClaim);
    let statement = ThreadAuthorityAdmission {
        version: 3,
        basis: basis(&acceptance),
        spool: trust.spool,
        spool_genesis: trust.spool_genesis,
        thread: replica.thread_id(),
        subject: OriginalAuthoritySubject::OwnershipClaim(claim.id().expect("claim id")),
        actor: actor.clone(),
        publisher: claim.accepting_publisher,
        authority_digest: *authority_digest,
        executor: trust.executor,
        admitted_at_ms: 9010,
    };
    let mut receipt = SignedAuthorityAdmission::sign(&statement, &key(18)).expect("receipt");
    receipt.boundary_acceptance = Some(std::sync::Arc::new(acceptance));
    assert!(
        statement.authorize_claim(&claim, &genesis, &trust).is_err(),
        "signature-only claim path rejects boundary basis"
    );
    pin(&replica, &trust);
    let mut missing = receipt.clone();
    missing.boundary_acceptance = None;
    assert!(
        replica
            .claim_ownership_with_admission(&signed, &missing)
            .is_err()
    );
    assert_eq!(replica.effective_owner().expect("unclaimed"), genesis.owner);
    replica
        .claim_ownership_with_admission(&signed, &receipt)
        .expect("explicit accepted claim");
    let reopened =
        ThreadReplica::open(repository.heddle_dir(), replica.thread_id()).expect("reopen");
    assert_eq!(
        reopened
            .ownership_claims_with_admission()
            .expect("retained"),
        vec![(signed.clone(), Some(receipt.clone()))]
    );
    let wrapper = reopened.genesis_record().expect("claim transfer");
    assert_eq!(wrapper.boundary_acceptances.len(), 1);
    assert_eq!(wrapper.ownership_claims[0].signatures.len(), 2);
    assert_eq!(reopened.genesis().expect("immutable genesis"), genesis);
    assert_eq!(
        reopened.effective_owner().expect("explicit account"),
        GenesisOwner::Account(account)
    );
    let mut wrong = signed;
    wrong.local_signature[0] ^= 1;
    assert!(
        reopened
            .claim_ownership_with_admission(&wrong, &receipt)
            .is_err(),
        "receipt does not replace original local signature"
    );
}
