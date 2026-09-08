// SPDX-License-Identifier: Apache-2.0
use biscuit_auth::{Biscuit, KeyPair};
use crypto::{Ed25519Signer, Signer, thread_operation::SignedOperation};
use iroh::{Endpoint, RelayMode, endpoint::presets};
use objects::object::{
    Attribution, ContentHash, Principal, State, Tree,
    thread_replication::{Admission, ThreadGenesis, ThreadOperation, ThreadOperationBody},
};
use repo::Repository;

use super::*;
use crate::authority::CredentialSigner;

fn credential(root: &KeyPair) -> CredentialSigner {
    let signer = Ed25519Signer::from_seed(&[17; 32]).expect("test signer");
    let token = Biscuit::builder()
        .fact("user(\"owner\")")
        .expect("subject")
        .fact("session(\"local-agent\")")
        .expect("session")
        .fact("right(\"spool\", \"/owned/project\", \"write\")")
        .expect("right")
        .fact(format!("device_pop_key(\"{}\")", hex::encode(signer.public_key())).as_str())
        .expect("proof key")
        .build(root)
        .expect("owner-minted Biscuit");
    CredentialSigner {
        signer,
        bearer: token.to_base64().expect("token encoding"),
        grant_envelope: vec![],
    }
}

fn genesis(repository: &Repository) -> ThreadGenesis {
    ThreadGenesis {
        version: 1,
        spool: "owned".into(),
        parent: None,
        base: repository.head().expect("HEAD").expect("base"),
        name: "live".into(),
        intent: "live source metadata".into(),
        creator: Ed25519Signer::from_seed(&[17; 32])
            .expect("creator")
            .public_key()
            .try_into()
            .expect("public key"),
        nonce: vec![],
    }
}

fn capture(
    repository: &Repository,
    replica: &ThreadReplica,
    genesis: &ThreadGenesis,
) -> ContentHash {
    let signer = Ed25519Signer::from_seed(&[17; 32]).expect("publisher");
    let state = State::new_snapshot(
        Tree::new().hash(),
        vec![genesis.base],
        Attribution::human(Principal::new("Agent", "agent@example.test")),
    );
    let operation = ThreadOperation {
        version: 1,
        thread: replica.thread_id(),
        parents: BTreeSet::new(),
        publisher: signer.public_key().try_into().expect("public key"),
        body: ThreadOperationBody::Capture(state.encode_current_msgpack().expect("state")),
    };
    let id = operation.id().expect("ID");
    assert_eq!(
        replica
            .receive(
                &SignedOperation::sign(&operation, &signer).expect("signature"),
                repository.store(),
                |_| Ok(())
            )
            .expect("local admission"),
        Admission::Accepted
    );
    id
}

async fn endpoint() -> Endpoint {
    Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .alpns(vec![api::HOSTED_ALPN_V1.to_vec()])
        .bind_addr((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("loopback")
        .bind()
        .await
        .expect("Iroh endpoint")
}

async fn accepted(replica: &ThreadReplica, id: ContentHash) {
    tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            if replica
                .operation(&id)
                .expect("lookup")
                .is_some_and(|(_, status)| status == Admission::Accepted)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("operation must arrive without reopening RPC");
}

#[tokio::test]
async fn exact_body_pop_and_durable_nonce_guard_the_owner_boundary() {
    let dir = tempfile::TempDir::new().expect("directory");
    let repository = Repository::init_default(dir.path()).expect("repository");
    let genesis = genesis(&repository);
    let replica = ThreadReplica::open(repository.heddle_dir(), &genesis).expect("replica");
    let root = KeyPair::new();
    let authority =
        RootAuthority::new(vec![root.public()], "/owned/project".into()).expect("authority");
    let signer = credential(&root);
    let method = rpc::SyncServiceReplicateThread::METHOD;
    let context = signer.context(method, b"bound opening").await.expect("PoP");
    assert!(matches!(
        authority.verify(&context, method, b"another opening", "write", &replica),
        Err(transport::Error::Protocol("invalid request signature"))
    ));
    let verified = authority
        .verify(&context, method, b"bound opening", "write", &replica)
        .expect("attached owner proof");
    let reopened = ThreadReplica::open(repository.heddle_dir(), &genesis).expect("restart");
    assert!(matches!(
        authority.verify(&context, method, b"bound opening", "write", &reopened),
        Err(transport::Error::Protocol("request nonce already consumed"))
    ));
    authority.recheck(&verified).expect("live authority");
    authority.replace_roots(vec![]).expect("detach root");
    assert!(matches!(
        authority.recheck(&verified),
        Err(transport::Error::Protocol(
            "Biscuit does not authorize this operation"
        ))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn irohs_open_stream_syncs_later_writes_honors_opt_in_and_stops_on_root_detachment() {
    let left_dir = tempfile::TempDir::new().expect("left directory");
    let right_dir = tempfile::TempDir::new().expect("right directory");
    let left_repo = Repository::init_default(left_dir.path()).expect("left repo");
    let right_repo = Repository::init_default(right_dir.path()).expect("right repo");
    let genesis = genesis(&left_repo);
    let left = ThreadReplica::open(left_repo.heddle_dir(), &genesis).expect("left replica");
    let right = ThreadReplica::open(right_repo.heddle_dir(), &genesis).expect("right replica");
    let left_endpoint = endpoint().await;
    let right_endpoint = endpoint().await;
    let left_key = *left_endpoint.id().as_bytes();
    let right_key = *right_endpoint.id().as_bytes();
    let facets = BTreeSet::from([ThreadFacet::Source]);
    left.set_sharing(right_key, &facets)
        .expect("ongoing opt-in");
    let root = KeyPair::new();
    let authority = Arc::new(
        RootAuthority::new(vec![root.public()], "/owned/project".into()).expect("owner authority"),
    );
    let left_peer = Peer::new(
        left.clone(),
        EndpointRef {
            public_key: left_key.to_vec(),
            kind: EndpointKind::Device as i32,
        },
        facets.clone(),
    )
    .expect("left peer")
    .with_genesis(
        opening::sign_genesis(
            &genesis,
            &Ed25519Signer::from_seed(&[17; 32]).expect("origin key"),
        )
        .expect("signed creation record"),
    )
    .expect("original genesis travels with publication");
    let right_peer = Peer::new(
        right.clone(),
        EndpointRef {
            public_key: right_key.to_vec(),
            kind: EndpointKind::Device as i32,
        },
        facets.clone(),
    )
    .expect("right peer");
    let (outgoing, incoming) = tokio::join!(
        left_endpoint.connect(right_endpoint.addr(), api::HOSTED_ALPN_V1),
        async {
            right_endpoint
                .accept()
                .await
                .expect("incoming")
                .await
                .expect("connection")
        }
    );
    let outgoing = outgoing.expect("outgoing connection");
    let left_feed = Feed::new(left.clone()).await.expect("left feed");
    let right_feed = Feed::new(right.clone()).await.expect("right feed");
    let server = tokio::spawn({
        let peer = right_peer.clone();
        let authority = authority.clone();
        let store = Arc::new(right_repo.store().clone());
        let feed = right_feed.clone();
        let connection = incoming.clone();
        async move { peer.accept(connection, authority, store, &feed).await }
    });
    let client = tokio::spawn({
        let peer = left_peer.clone();
        let store = Arc::new(left_repo.store().clone());
        let feed = left_feed.clone();
        let connection = outgoing.clone();
        let signer = credential(&root);
        async move {
            peer.connect(
                connection,
                EndpointKind::Device,
                signer,
                store,
                &feed,
                || async {
                    tokio::task::yield_now().await;
                    Ok(())
                },
            )
            .await
        }
    });
    let first = capture(&left_repo, &left, &genesis);
    accepted(&right, first).await;
    // Another process can open the same durable database. It need not hold a
    // handle to the RPC or notify it explicitly when recording new work.
    let reopened =
        ThreadReplica::open(left_repo.heddle_dir(), &genesis).expect("independent writer");
    let later = capture(&left_repo, &reopened, &genesis);
    accepted(&right, later).await;
    let private = capture(&right_repo, &right, &genesis);
    // Prove the reverse direction is active, while the private operation stays
    // behind policy, by waiting for the remote acceptance of the later write.
    tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            if left.peer_receipt(right_key, later).expect("receipt") == Some(Admission::Accepted) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("acceptance on the live stream");
    assert!(left.operation(&private).expect("private lookup").is_none());
    right
        .set_sharing(left_key, &facets)
        .expect("explicit opt-in after capture");
    accepted(&left, private).await;
    assert!(
        !server.is_finished() && !client.is_finished(),
        "one continuous exchange must still be open"
    );
    authority
        .replace_roots(vec![])
        .expect("owner detaches root");
    let result = tokio::time::timeout(Duration::from_secs(4), server)
        .await
        .expect("live revocation must stop idle stream")
        .expect("server task");
    assert!(matches!(
        result,
        Err(live_replication::Error::Transport(
            transport::Error::Protocol("Biscuit does not authorize this operation")
        ))
    ));
    assert!(
        tokio::time::timeout(Duration::from_secs(4), client)
            .await
            .expect("peer notices closure")
            .expect("client task")
            .is_err()
    );
    let offline = capture(&left_repo, &left, &genesis);
    assert!(right.operation(&offline).expect("offline lookup").is_none());
    authority
        .replace_roots(vec![root.public()])
        .expect("owner attaches root again");
    let server = tokio::spawn({
        let peer = right_peer;
        let store = Arc::new(right_repo.store().clone());
        let feed = right_feed;
        async move { peer.accept(incoming, authority, store, &feed).await }
    });
    let client = tokio::spawn({
        let peer = left_peer;
        let store = Arc::new(left_repo.store().clone());
        let feed = left_feed;
        let signer = credential(&root);
        async move {
            peer.connect(
                outgoing,
                EndpointKind::Device,
                signer,
                store,
                &feed,
                || async {
                    tokio::task::yield_now().await;
                    Ok(())
                },
            )
            .await
        }
    });
    accepted(&right, offline).await;
    assert_eq!(
        left.view().expect("left view").source_heads,
        right.view().expect("right view").source_heads
    );
    client.abort();
    server.abort();
    assert!(client.await.expect_err("client cancelled").is_cancelled());
    assert!(server.await.expect_err("server cancelled").is_cancelled());
    left_endpoint.close().await;
    right_endpoint.close().await;
}

#[tokio::test]
async fn call_context_carries_the_serialized_biscuit_returned_by_credential_ceremonies() {
    use crate::transport::Authorize;
    let root = KeyPair::new();
    let credential = credential(&root);
    let method = api::v2::method_descriptor("/heddle.api.v2alpha1.SyncService/ReplicateThread")
        .expect("replication descriptor");
    let context = credential
        .context(method, &[])
        .await
        .expect("signed context");
    Biscuit::from(&context.bearer_capability, root.public())
        .expect("CallContext carries raw serialized Biscuit bytes");
}
