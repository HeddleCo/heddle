use crypto::{Ed25519Signer, Signer, thread_operation::{SignedGenesis, SignedOperation}, thread_ownership_claim::SignedOwnershipClaim};
use objects::{object::{Attribution, Principal, State, CollaborationActor, thread_replication::{AuthoredCapture, GenesisOwner, SourceAuthor, ThreadGenesis, ThreadOperation, ThreadOperationBody, ownership_claim::ThreadOwnershipClaim}}, store::ObjectStore};
use super::*;

fn authority(root: &Ed25519Signer) -> crate::device_authority::DeviceAuthority {
    let recovery = Ed25519Signer::from_seed(&[72;32]).expect("recovery");
    let signed = crate::sign_custodial_owner_root(root,&recovery,[9;16],[5;32]).expect("owner root");
    let binding = crate::sign_custodial_owner_binding(root,&signed,[6;32]).expect("owner binding");
    let verified = heddleco_capability_verifier::verify_owner_root(&signed).expect("root");
    crate::device_authority::DeviceAuthority { owner: api::heddle::api::v2alpha1::OwnerState {
        owner: Some(api::heddle::api::v2alpha1::PrincipalRef{id:uuid::Uuid::from_bytes([9;16]).to_string()}),
        root:Some(signed),binding:Some(binding),version:verified.state_hash().to_vec(),..Default::default()
    },mint_roots:vec![],revoked_ids:vec![],revoked_mint_roots:vec![],revoked_publishers:vec![] }
}
fn acceptance(authority: &crate::device_authority::DeviceAuthority, key:&Ed25519Signer, spool:uuid::Uuid, method:&str, agent:bool) -> SourceAuthor {
    let pair = biscuit_auth::KeyPair::from(&biscuit_auth::PrivateKey::from_bytes(&[71;32],biscuit_auth::Algorithm::Ed25519).expect("mint"));
    let agent_fact=if agent {"agent_provider(\"local\");"} else {""};
    let token=biscuit_auth::Biscuit::builder().code(format!("user(\"{}\"); session(\"claim-agent\"); device_pop_key(\"{}\"); {agent_fact} check if operation(\"{method}\"); check if resource(\"spool\",\"acme/project\"); expires_at(2100-01-01T00:00:00Z);",uuid::Uuid::from_bytes([9;16]),hex::encode(key.public_key()))).expect("facts").build(&pair).expect("actual Biscuit");
    let envelope=metadata::prepare_control_authority(authority,&key.public_key().try_into().expect("key"),&token,100).expect("sealed acceptance");
    SourceAuthor::account(spool,CollaborationActor {principal_id:uuid::Uuid::from_bytes([9;16]),agent_id:agent.then(||"claim-agent".into())},envelope).expect("account actor")
}
#[test]
fn explicit_claim_preserves_identity_cutoff_and_conflicts_fail_closed() {
    let directory=tempfile::tempdir().expect("repo");
    let repository=crate::Repository::init_default(directory.path()).expect("repo");
    let local=Ed25519Signer::from_seed(&[31;32]).expect("local");
    let account=Ed25519Signer::from_seed(&[71;32]).expect("account");
    let authority=authority(&account);
    let spool=uuid::Uuid::from_u128(43);
    let base=repository.head().expect("head").expect("base");
    let home=tempfile::tempdir().expect("isolated device home");
    let local_record=crate::identity::LocalIdentity { public_key:hex::encode(local.public_key()), private_key_pem:local.to_pem().expect("local key material"), created_at:"2026-09-09T00:00:00Z".into() };
    objects::fs_atomic::write_file_atomic_secret(&repository.heddle_dir().join(crate::identity::LOCAL_IDENTITY_FILE),toml::to_string(&local_record).expect("local identity").as_bytes()).expect("retain local key");
    let device=crate::identity::DeviceIdentity { public_key:hex::encode(account.public_key()),private_key_pem:account.to_pem().expect("device key material"),server:"https://test.invalid".into(),linked_at:"2026-09-09T00:00:00Z".into() };
    objects::fs_atomic::write_file_atomic_secret(&home.path().join(crate::identity::DEVICE_IDENTITY_FILE),toml::to_string(&device).expect("device identity").as_bytes()).expect("enrolled different device key");
    let pair=biscuit_auth::KeyPair::from(&biscuit_auth::PrivateKey::from_bytes(&[71;32],biscuit_auth::Algorithm::Ed25519).expect("device root"));
    let token=biscuit_auth::Biscuit::builder().code(format!("user(\"{}\"); session(\"device-source\"); device_pop_key(\"{}\"); expires_at(2100-01-01T00:00:00Z);",uuid::Uuid::from_bytes([9;16]),hex::encode(account.public_key()))).expect("device facts").build(&pair).expect("device Biscuit");
    crate::identity::source_author::publish(home.path(),&authority,&account.public_key().try_into().expect("key"),&account.public_key().try_into().expect("key"),&token,100).expect("retain enrollment proof");

    let genesis=ThreadGenesis {version:1,spool:spool.to_string(),parent:None,base,name:"unclaimed".into(),intent:"offline".into(),owner:GenesisOwner::LocalKey(local.public_key().try_into().expect("key")),creator:local.public_key().try_into().expect("key"),nonce:vec![]};
    let original=SignedGenesis::sign(&genesis,&local).expect("genesis");
    let replica=ThreadReplica::create(repository.heddle_dir(),&original).expect("replica");
    let tree=repository.store().get_state(&base).expect("state").expect("base").tree;
    let make=|intent:&str| {
        let state=State::new_snapshot(tree,vec![base],Attribution::human(Principal::new(intent,"")));
        let operation=ThreadOperation {version:1,thread:replica.thread_id(),parents:Default::default(),publisher:genesis.creator,body:ThreadOperationBody::Capture(AuthoredCapture::local(state.encode_current_msgpack().expect("state").into()))};
        SignedOperation::sign(&operation,&local).expect("source")
    };
    let retained_signer=repository.native_thread_signer_at(&replica,home.path()).expect("local actions after enrollment");
    assert_eq!(retained_signer.public_key(),local.public_key(),"enrollment must not change unclaimed Thread signer");
    assert_eq!(replica.source_author_for_at(&retained_signer.public_key().try_into().expect("key"),home.path()).expect("source author"),SourceAuthor::LocalKey,"enrollment must not claim local Thread authorship");
    let old=make("before claim");
    replica.receive(&old,repository.store(),|_|Ok(())).expect("local source");
    let future_state=State::new_snapshot(tree,vec![base],Attribution::human(Principal::new("future account parent","")));
    let future_operation=ThreadOperation {version:1,thread:replica.thread_id(),parents:Default::default(),publisher:account.public_key().try_into().expect("key"),body:ThreadOperationBody::Capture(AuthoredCapture {
        result:future_state.encode_current_msgpack().expect("future State").into(),
        author:crate::identity::source_author::load(home.path(),&account.public_key().try_into().expect("key"),spool).expect("account source proof"),
    })};
    let future_parent=SignedOperation::sign(&future_operation,&account).expect("future parent");
    let delayed_state=State::new_snapshot(tree,vec![future_state.id()],Attribution::human(Principal::new("preclaim pending local work","")));
    let delayed_operation=ThreadOperation {version:1,thread:replica.thread_id(),parents:[future_operation.id().expect("parent ID")].into(),publisher:genesis.creator,body:ThreadOperationBody::Capture(AuthoredCapture::local(delayed_state.encode_current_msgpack().expect("pending State").into()))};
    let delayed=SignedOperation::sign(&delayed_operation,&local).expect("pending source");
    assert_eq!(replica.receive(&delayed,repository.store(),|_|Ok(())).expect("pending source admitted"),objects::object::thread_replication::Admission::Pending);
    let mut claim=ThreadOwnershipClaim {version:1,thread:replica.thread_id(),prior_local_key:genesis.creator,accepting_publisher:account.public_key().try_into().expect("account key"),acceptance:acceptance(&authority,&account,spool,"ClaimThreadOwnership",true),source_frontier:[old.verify().expect("old").id().expect("id")].into()};
    let denied_acceptance=acceptance(&authority,&account,spool,"PublishContent",true);
    let mut narrowed=claim.clone();narrowed.acceptance=denied_acceptance;
    let narrowed=SignedOwnershipClaim::sign(&narrowed,&local,&account).expect("narrowed claim");
    assert!(replica.claim_ownership(&narrowed,&authority,"acme/project",100).is_err(),"agent attenuation must constrain claim acceptance");
    assert_eq!(replica.effective_owner().expect("unchanged"),genesis.owner);
    let signed=SignedOwnershipClaim::sign(&claim,&local,&account).expect("dual proof");
    let command=crate::device_operations::Command { namespace:"owner/claim-agent", id:"00000000-0000-0000-0000-000000000077".parse().expect("command"), method:objects::object::thread_replication::ownership_claim::METHOD,request_hash:[7;32] };
    assert_eq!(replica.claim_ownership_with_command(&signed,&authority,"acme/project",100,&command,&[7]).expect("delegated account acceptance"),vec![7]);
    let id=claim.id().expect("claim ID");
    assert_eq!(replica.claim_ownership_with_command(&signed,&authority,"acme/project",100,&command,&[8]).expect("exact request reuses original receipt"),vec![7]);
    assert_eq!(replica.thread_id(),genesis.id().expect("immutable ID"));
    assert_eq!(replica.genesis().expect("immutable genesis"),genesis);
    assert_eq!(replica.effective_owner().expect("account owner"),GenesisOwner::Account(uuid::Uuid::from_bytes([9;16])));
    assert!(replica.local_source_author_allowed(&old.verify().expect("old")).expect("historical cutoff"));
    assert!(!replica.local_source_author_allowed(&make("after claim").verify().expect("new")).expect("current owner"),"former local key cannot author outside signed cutoff");
    assert_eq!(replica.receive(&future_parent,repository.store(),|_|Ok(())).expect("authorized account parent after claim"),objects::object::thread_replication::Admission::Accepted);
    assert!(matches!(replica.operation(&delayed_operation.id().expect("delayed ID")).expect("delayed source").expect("known pending").1,
        objects::object::thread_replication::Admission::Rejected(ref reason) if reason.contains("cutoff")),"pending former-key source must not settle after signed cutoff");
    assert!(replica.receive(&make("prepared before claim, delivered later"),repository.store(),|_|Ok(())).expect_err("transaction must recheck claim").to_string().contains("cutoff"),"prepared local source must recheck effective ownership in commit transaction");
    let claimed_signer=repository.native_thread_signer_at(&replica,home.path()).expect("claimed account device signer");
    assert_eq!(claimed_signer.public_key(),account.public_key());
    assert!(matches!(replica.source_author_for_at(&claimed_signer.public_key().try_into().expect("key"),home.path()).expect("claimed source proof"),SourceAuthor::Account { .. }));
    let generation=replica.generation().expect("generation");
    assert_eq!(replica.claim_ownership(&signed,&authority,"wrong/path",i64::MAX).expect("retained exact proof no refresh"),id);
    assert_eq!(replica.generation().expect("replay generation"),generation);
    receipt_backfill(&repository, &replica, &signed, spool);
    claim.acceptance=acceptance(&authority,&account,spool,"ClaimThreadOwnership",false);
    claim.source_frontier=replica.frontier_page(objects::object::thread_replication::ThreadFacet::Source,None,128).expect("current frontier").into_iter().collect();
    let conflicting=SignedOwnershipClaim::sign(&claim,&local,&account).expect("different valid claim");
    assert!(replica.claim_ownership(&conflicting,&authority,"acme/project",100).expect_err("competing explicit claim").to_string().contains("conflicting"));
    assert!(replica.effective_owner().is_err(),"conflicting claims must not select an arbitrary owner");
    for retained in [&signed,&conflicting] {
        assert!(replica.claim_ownership(retained,&authority,"acme/project",100).expect_err("claim replay remains conflicted").to_string().contains("conflicting"),"retained claim replay must not assert resolved ownership");
    }
    assert!(replica.claim_ownership_with_command(&signed,&authority,"acme/project",100,&command,&[7]).expect_err("cached receipt remains conflicted").to_string().contains("conflicting"),"cached ownership receipt must not hide unresolved conflict");
    assert!(!replica.local_source_author_allowed(&old.verify().expect("old")).expect("conflict denies"));
}

fn receipt_backfill(repository: &crate::Repository, replica: &ThreadReplica, signed: &SignedOwnershipClaim, spool: uuid::Uuid) {
    use prost::Message;
    use objects::object::thread_replication::integration::SPOOL_GENESIS_TRUST_FORMAT;
    use objects::object::thread_authority_admission::{ThreadAuthorityAdmission, OriginalAuthoritySubject};
    use crypto::thread_authority_admission::SignedAuthorityAdmission;
    let executor=Ed25519Signer::from_seed(&[83;32]).expect("independent endpoint");
    let owner=Ed25519Signer::from_seed(&[84;32]).expect("Spool owner");
    let genesis=crate::sign_spool_owner_genesis(&owner,*spool.as_bytes()).expect("Spool genesis");
    let statement=signed.verify().expect("claim");
    let SourceAuthor::Account {actor,authority_digest,..}=statement.acceptance.clone() else { panic!("account claim") };
    let record=ThreadAuthorityAdmission {version: 3,
            basis: objects::object::original_boundary_acceptance::AdmissionBasis::OriginalAuthority,spool,
        spool_genesis:objects::object::ContentHash::compute_typed(SPOOL_GENESIS_TRUST_FORMAT,&genesis.genesis.as_ref().expect("Spool statement").encode_to_vec()),
        thread:replica.thread_id(),subject:OriginalAuthoritySubject::OwnershipClaim(statement.id().expect("claim ID")),
        actor,publisher:statement.accepting_publisher,authority_digest,executor:executor.public_key().try_into().expect("key"),admitted_at_ms:2000};
    let receipt=SignedAuthorityAdmission::sign(&record,&executor).expect("actual signed testimony");
    let generation=replica.generation().expect("before untrusted receipt");
    assert!(replica.claim_ownership_with_admission(signed,&receipt).expect_err("retained claim never establishes executor trust").to_string().contains("independently pinned executor"));
    assert_eq!(replica.generation().expect("rejection quiet"),generation);
    assert!(replica.ownership_claim_admission(&statement.id().expect("ID")).expect("no untrusted receipt").is_none());
    repository.verify_and_pin_owner_genesis(2,Some(&genesis),&["selected".into(),"shared".into()]).expect("independent Spool trust");
    repository.pin_thread_hosted_executor(replica,record.executor).expect("independent executor trust");
    replica.claim_ownership_with_admission(signed,&receipt).expect("verified receipt backfill");
    assert_eq!(replica.ownership_claim_admission(&statement.id().expect("ID")).expect("retained receipt"),Some(receipt.clone()));
    let generation=replica.generation().expect("published witness");
    replica.claim_ownership_with_admission(signed,&receipt).expect("same receipt retry");
    assert_eq!(replica.generation().expect("receipt replay quiet"),generation);
    let changed=ThreadAuthorityAdmission {admitted_at_ms:3000,..record};
    replica.claim_ownership_with_admission(signed,&SignedAuthorityAdmission::sign(&changed,&executor).expect("later valid testimony")).expect("later witness does not replace original");
    assert_eq!(replica.ownership_claim_admission(&statement.id().expect("ID")).expect("first witness preserved"),Some(receipt));
    assert_eq!(replica.generation().expect("replacement quiet"),generation);
}
