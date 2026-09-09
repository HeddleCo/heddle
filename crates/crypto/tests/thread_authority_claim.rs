use crypto::{Ed25519Signer, Signer, thread_authority_admission::SignedAuthorityAdmission, thread_ownership_claim::SignedOwnershipClaim};
use heddle_object_model::object::{CollaborationActor, ContentHash, StateId, thread_authority_admission::{OriginalAuthoritySubject, ThreadAuthorityAdmission}, thread_replication::{GenesisOwner, SourceAuthor, ThreadGenesis, integration::TrustedHostedExecutor, ownership_claim::ThreadOwnershipClaim}};

#[test]
fn retained_claim_testimony_binds_subject_account_and_both_original_signatures() {
    let local = Ed25519Signer::from_seed(&[91;32]).expect("local owner");
    let accepting = Ed25519Signer::from_seed(&[92;32]).expect("account acceptor");
    let executor = Ed25519Signer::from_seed(&[93;32]).expect("independent executor");
    let trust = TrustedHostedExecutor { spool: "00000000-0000-0000-0000-000000000094".parse().expect("Spool"), spool_genesis:ContentHash::from_bytes([97;32]), executor:executor.public_key().try_into().expect("executor") };
    let spool = trust.spool;
    let actor = CollaborationActor { principal_id: "00000000-0000-0000-0000-000000000095".parse().expect("account"), agent_id: Some("delegated-agent".into()) };
    let key = local.public_key().try_into().expect("owner key");
    let genesis = ThreadGenesis { version:1, spool:spool.to_string(), parent:None, base:StateId::from_bytes([96;32]), name:"private".into(), intent:"claim".into(), creator:key, owner:GenesisOwner::LocalKey(key), nonce:vec![] };
    let claim = ThreadOwnershipClaim { version:1, thread:genesis.id().expect("Thread"), prior_local_key:key, accepting_publisher:accepting.public_key().try_into().expect("acceptor"), acceptance:SourceAuthor::account(spool,actor.clone(),b"independently checked at original admission".to_vec()).expect("acceptance"), source_frontier:Default::default() };
    let signed = SignedOwnershipClaim::sign(&claim,&local,&accepting).expect("both original signatures");
    let SourceAuthor::Account { authority_digest, .. } = &claim.acceptance else { panic!("account") };
    let statement = ThreadAuthorityAdmission { version:2, spool, spool_genesis:trust.spool_genesis, thread:claim.thread, subject:OriginalAuthoritySubject::OwnershipClaim(claim.id().expect("claim ID")), actor, publisher:claim.accepting_publisher, authority_digest:*authority_digest, executor:trust.executor, admitted_at_ms:1 };
    let receipt = SignedAuthorityAdmission::sign(&statement,&executor).expect("first admission");
    assert_eq!(receipt.verify_claim(&signed,&genesis,&trust).expect("retained exact original"),statement);
    let mut changed = statement.clone();
    changed.subject = OriginalAuthoritySubject::Operation(claim.id().expect("same digest, wrong kind"));
    assert!(SignedAuthorityAdmission::sign(&changed,&executor).expect("signed wrong subject").verify_claim(&signed,&genesis,&trust).is_err(),"an operation receipt cannot authorize ownership");
    let mut changed = statement.clone(); changed.actor.agent_id = None;
    assert!(SignedAuthorityAdmission::sign(&changed,&executor).expect("signed relabeling").verify_claim(&signed,&genesis,&trust).is_err(),"delegated acceptor identity cannot be relabeled");
    for local_signature in [true,false] {
        let mut damaged=signed.clone();
        if local_signature { damaged.local_signature[0]^=1; } else { damaged.acceptance_signature[0]^=1; }
        assert!(receipt.verify_claim(&damaged,&genesis,&trust).is_err(),"historical receipt cannot replace an original signature");
    }
    let mut wrong_trust=trust.clone(); wrong_trust.executor=[98;32];
    assert!(receipt.verify_claim(&signed,&genesis,&wrong_trust).is_err(),"receipt issuer never establishes its own trust");
}
