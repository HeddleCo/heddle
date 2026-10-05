use crypto::{
    Ed25519Signer, Signer, thread_ownership_claim::SignedOwnershipClaim,
    thread_ownership_resolution::SignedOwnershipResolution,
};
use native::{
    ownership_claim::ThreadOwnershipClaim, ownership_resolution::ThreadOwnershipResolution,
};
use objects::object::CollaborationActor;

use super::*;

fn decisions() -> (
    BTreeMap<ContentHash, BTreeMap<ContentHash, ThreadOwnershipClaim>>,
    BTreeMap<ContentHash, ThreadOwnershipResolution>,
) {
    let local = Ed25519Signer::from_seed(&[31; 32]).expect("local");
    let account = Ed25519Signer::from_seed(&[32; 32]).expect("account");
    let thread = ContentHash::from_bytes([33; 32]);
    let frontier: std::collections::BTreeSet<_> = [ContentHash::from_bytes([34; 32])].into();
    let mut claims = BTreeMap::new();
    for proof in [vec![35; 32], vec![36; 32]] {
        let value = ThreadOwnershipClaim {
            version: 1,
            thread,
            prior_local_key: local.public_key().try_into().expect("key"),
            accepting_publisher: account.public_key().try_into().expect("key"),
            acceptance: SourceAuthor::account(
                uuid::Uuid::from_u128(22),
                CollaborationActor {
                    principal_id: uuid::Uuid::from_u128(23),
                    agent_id: None,
                },
                proof,
            )
            .expect("author"),
            source_frontier: frontier.clone(),
        };
        let verified = SignedOwnershipClaim::sign(&value, &local, &account)
            .expect("signed claim")
            .verify()
            .expect("both signatures");
        claims.insert(verified.id().expect("id"), verified);
    }
    assert_eq!(claims.len(), 2);
    let winner = claims.first_key_value().expect("winner");
    let r = ThreadOwnershipResolution {
        version: 1,
        spool: uuid::Uuid::from_u128(22),
        thread,
        winning_claim: *winner.0,
        conflicting_claims: claims.keys().copied().collect(),
        frontier,
        local_owner: local.public_key().try_into().expect("key"),
        accepting_publisher: account.public_key().try_into().expect("key"),
        acceptance: winner.1.acceptance.clone(),
        occurred_at_ms: 1000,
    };
    let r = SignedOwnershipResolution::sign(&r, &local, &account)
        .expect("signed resolution")
        .verify(winner.1)
        .expect("both signatures and winner");
    (
        BTreeMap::from([(thread, claims)]),
        BTreeMap::from([(thread, r)]),
    )
}
#[test]
fn native_resolution_requires_the_complete_admitted_conflict_set() {
    let (claims, resolutions) = decisions();
    ownership_cutoffs(&claims, &resolutions).expect("complete decision");
    let thread = *claims.first_key_value().expect("thread").0;
    for missing in claims[&thread].keys() {
        let mut incomplete = claims.clone();
        incomplete.get_mut(&thread).expect("claims").remove(missing);
        assert!(
            matches!(
                ownership_cutoffs(&incomplete, &resolutions),
                Err(Error::Hybrid(Reject::Scope))
            ),
            "incomplete admitted conflict set must reject"
        );
    }
}
#[test]
fn native_resolution_without_claims_rejects() {
    let (claims, resolutions) = decisions();
    ownership_cutoffs(&claims, &resolutions).expect("signed control");
    assert!(
        matches!(
            ownership_cutoffs(&BTreeMap::new(), &resolutions),
            Err(Error::Hybrid(Reject::Scope))
        ),
        "resolution without admitted claims must reject"
    );
}
#[test]
fn native_unresolved_same_frontier_claims_reject() {
    let (claims, resolutions) = decisions();
    ownership_cutoffs(&claims, &resolutions).expect("signed resolved control");
    assert!(
        matches!(
            ownership_cutoffs(&claims, &BTreeMap::new()),
            Err(Error::Hybrid(Reject::Scope))
        ),
        "same-frontier ownership conflict must reject"
    );
}
#[test]
fn native_duplicate_genesis_admission_rejects() {
    let signer = Ed25519Signer::from_seed(&[31; 32]).expect("creator");
    let g = native::ThreadGenesis {
        version: 1,
        spool: uuid::Uuid::from_u128(22).to_string(),
        owner: GenesisOwner::LocalKey(signer.public_key().try_into().expect("key")),
        creator: signer.public_key().try_into().expect("key"),
        parent: None,
        base: objects::object::StateId::from_bytes([34; 32]),
        name: "duplicate".into(),
        intent: String::new(),
        nonce: vec![1],
    };
    let signed = crypto::thread_operation::SignedGenesis::sign(&g, &signer).expect("original");
    let id = signed.verify().expect("native signature").id().expect("id");
    let mut geneses = BTreeMap::new();
    admit_genesis(&mut geneses, id, signed.clone(), &[]).expect("sole admission");
    assert!(
        matches!(
            admit_genesis(&mut geneses, id, signed, &[]),
            Err(Error::Hybrid(Reject::SlotConflict))
        ),
        "duplicate genesis admission must reject"
    );
}
