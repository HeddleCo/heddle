//! The browser authorizes one exact claim; the owned device supplies only the
//! matching retained local-owner signature.
use super::*;
use crypto::{Ed25519Signer, Signer, thread_ownership_claim::SignedOwnershipAcceptance};
use objects::object::{CollaborationActor, thread_replication::{GenesisOwner, SourceAuthor, ThreadFacet, ownership_claim::ThreadOwnershipClaim}};

pub(super) async fn claim(
    remote: &thread_api::Remote<thread_api::transport::IrohTransport<thread_api::credentials::Credentials>>,
    _repository: &repo::Repository,
    replica: &repo::thread_replication::ThreadReplica,
) {
    let genesis = replica.genesis().expect("original genesis");
    let GenesisOwner::LocalKey(prior_local_key) = genesis.owner else { panic!("fixture starts locally owned") };
    let spool: uuid::Uuid = genesis.spool.parse().expect("Spool UUID");
    let signer = Ed25519Signer::from_seed(&[71; 32]).expect("account acceptor");
    let now = chrono::Utc::now().timestamp();
    let home = repo::identity::heddle_home_dir();
    let authority = repo::device_authority::load(&home, now).expect("independent device owner");
    let minted = crate::hosted_runtime::root_mint::mint_agent_root(&[71; 32]).expect("actual account capability");
    let mint = biscuit_verifier::PublicKey::from_bytes(signer.public_key(), biscuit_auth::Algorithm::Ed25519).expect("mint");
    let token = biscuit_verifier::parse_token(&minted.token, &[mint]).expect("sealed account token");
    let envelope = repo::thread_replication::metadata::prepare_control_authority(&authority, &signer.public_key().try_into().expect("key"), &token, now).expect("portable account acceptance");
    let statement = ThreadOwnershipClaim {
        version: 1, thread: replica.thread_id(), prior_local_key,
        accepting_publisher: signer.public_key().try_into().expect("acceptor"),
        acceptance: SourceAuthor::account(spool, CollaborationActor { principal_id: uuid::Uuid::from_bytes([9;16]), agent_id: None }, envelope).expect("account author"),
        source_frontier: replica.frontier_page(ThreadFacet::Source, None, 128).expect("source cutoff").into_iter().collect(),
    };
    let acceptance = SignedOwnershipAcceptance::sign(&statement, &signer).expect("accept exact claim");
    let request = ClaimThreadOwnershipRequest {
        client_operation_id: uuid::Uuid::new_v4().to_string(),
        thread: Some(ThreadRef { spool: Some(SpoolRef { id: genesis.spool.clone() }), id: Some(ThreadId { value: replica.thread_id().as_bytes().to_vec() }) }),
        claim: Some(thread_api::thread_ownership::encode_acceptance(&acceptance).expect("wire acceptance")),
    };
    let generation = replica.generation().expect("before rejected acceptor");
    let foreign = Ed25519Signer::from_seed(&[78;32]).expect("different acceptor");
    let mut foreign_statement = statement.clone();
    foreign_statement.accepting_publisher = foreign.public_key().try_into().expect("foreign key");
    let mut rejected = request.clone();
    rejected.client_operation_id = uuid::Uuid::new_v4().to_string();
    rejected.claim = Some(thread_api::thread_ownership::encode_acceptance(&SignedOwnershipAcceptance::sign(&foreign_statement, &foreign).expect("valid foreign signature")).expect("foreign acceptance"));
    let error = remote.api.call::<thread_api::rpc::ThreadServiceClaimThreadOwnership>(&rejected).await.expect_err("acceptor must match original account capability");
    assert!(error.to_string().contains("publisher or account differs"), "reject exact original acceptor mismatch: {error}");
    assert!(replica.ownership_claims().expect("no claim on rejection").is_empty());
    assert_eq!(replica.generation().expect("no rejected claim mutation"), generation);
    let response = remote.api.call::<thread_api::rpc::ThreadServiceClaimThreadOwnership>(&request).await.expect("device co-signs explicit ownership claim");
    let repeated = remote.api.call::<thread_api::rpc::ThreadServiceClaimThreadOwnership>(&request).await.expect("exact claim command replay");
    assert_eq!(response, repeated, "claim replay preserves original durable response");
    let thread_api::thread_ownership::ClaimProof::Complete(proof) = thread_api::thread_ownership::decode(response.claim.as_ref().expect("dual proof")).expect("verify returned claim") else { panic!("device must return both signatures") };
    assert_eq!(proof.verify().expect("canonical claim"), statement);
    assert_eq!(replica.genesis().expect("unchanged genesis"), genesis);
    assert_eq!(replica.effective_owner().expect("effective owner"), GenesisOwner::Account(uuid::Uuid::from_bytes([9;16])));
}
