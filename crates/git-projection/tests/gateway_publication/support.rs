#![allow(dead_code)] // Shared fixtures are used by different integration binaries.
use api::v2::{
    MethodDescriptor,
    client::{Client, MessageReader, MessageWriter, RpcTransport},
};
use crypto::{Ed25519Signer, Signer, thread_operation::SignedOperation};
use heddle_git_projection::gateway_publication::{HistorySelection, PublicationScope};
use objects::{
    object::{
        Attribution, Blob, CollaborationActor, ContentHash, Principal, State, Tree, TreeEntry,
        thread_replication::{
            AuthoredCapture, GenesisOwner, SourceAuthor, ThreadGenesis, ThreadOperation,
            ThreadOperationBody,
        },
    },
    store::{FsStore, ObjectStore},
};
use std::collections::BTreeSet;
use thread_api::{Remote, contract::*, transport::Error};

pub struct NoNetwork;
pub struct NoStream;
impl MessageReader for NoStream {
    type Error = Error;
    async fn next(&mut self) -> Result<Option<Vec<u8>>, Error> {
        panic!("preparation made an RPC")
    }
    fn cancel(&mut self) {}
}
impl MessageWriter for NoStream {
    type Error = Error;
    async fn send(&mut self, _: Vec<u8>) -> Result<(), Error> {
        panic!("preparation sent bytes")
    }
    async fn finish(&mut self) -> Result<(), Error> {
        panic!("preparation finished an RPC")
    }
    fn abort(&mut self) {}
}
impl RpcTransport for NoNetwork {
    type Error = Error;
    type Reader = NoStream;
    type Writer = NoStream;
    async fn unary(&self, _: &'static MethodDescriptor, _: Vec<u8>) -> Result<Vec<u8>, Error> {
        panic!("preparation made an RPC")
    }
    async fn observe(&self, _: &'static MethodDescriptor, _: Vec<u8>) -> Result<NoStream, Error> {
        panic!("preparation made an RPC")
    }
    async fn exchange(
        &self,
        _: &'static MethodDescriptor,
        _: Vec<u8>,
    ) -> Result<(NoStream, NoStream), Error> {
        panic!("preparation made an RPC")
    }
}
pub fn remote() -> Remote<NoNetwork> {
    Remote {
        api: Client::new(
            NoNetwork,
            ["/heddle.api.v1alpha2.SyncService/PublishContent".into()],
        ),
        description: DescribeEndpointResponse {
            endpoint: Some(EndpointRef {
                kind: EndpointKind::Weft as i32,
                public_key: vec![8; 32],
            }),
            ..Default::default()
        },
    }
}
pub struct Fixture {
    pub root: tempfile::TempDir,
    pub store: FsStore,
    pub states: Vec<State>,
    pub blobs: Vec<Blob>,
    pub originals: Vec<SignedOperation>,
    pub genesis: ThreadGenesisRecord,
    pub reference: ThreadRef,
}
pub fn author() -> SourceAuthor {
    // Explicit non-authorizing fixture bytes: tests prove signed provenance and
    // standalone closure, not live account enrollment or capability admission.
    SourceAuthor::account(
        uuid::Uuid::from_u128(501),
        CollaborationActor {
            principal_id: uuid::Uuid::from_u128(502),
            agent_id: Some("fixture-source-actor".into()),
        },
        vec![7; 64],
    )
    .expect("fixture author")
}
pub fn fixture(blob_size: usize) -> Fixture {
    let root = tempfile::tempdir().expect("fixture");
    let store = FsStore::new(root.path().join("source"));
    store.init().expect("source store");
    let signer = Ed25519Signer::from_seed(&[61; 32]).expect("fixture signer");
    let seed =
        objects::object::thread_replication::initial_base::synthetic_initial_base().expect("seed");
    let genesis = ThreadGenesis {
        version: 1,
        spool: uuid::Uuid::from_u128(501).to_string(),
        parent: None,
        base: seed.id(),
        name: "main".into(),
        intent: "isolated publication fixture".into(),
        owner: GenesisOwner::Account(uuid::Uuid::from_u128(502)),
        creator: signer.public_key().try_into().expect("key"),
        nonce: vec![1],
    };
    let thread = genesis.id().expect("Thread");
    let reference = ThreadRef {
        spool: Some(SpoolRef {
            id: genesis.spool.clone(),
        }),
        id: Some(ThreadId {
            value: thread.as_bytes().to_vec(),
        }),
    };
    let mut originals = Vec::new();
    let mut states = Vec::new();
    let mut blobs = Vec::new();
    let mut parent = seed.id();
    let mut causal = BTreeSet::new();
    for byte in [41, 42] {
        let blob = Blob::new(vec![byte; blob_size]);
        let tree = Tree::from_git_entries(vec![
            TreeEntry::file("file.txt", blob.hash(), false).expect("entry"),
        ])
        .expect("tree");
        store.put_blob(&blob).expect("blob");
        store.put_tree(&tree).expect("tree");
        let state = State::new_snapshot(
            tree.hash(),
            vec![parent],
            Attribution::human(Principal::new(
                "Untrusted Git Author",
                "git@example.invalid",
            )),
        );
        store.put_state(&state).expect("State");
        let operation = ThreadOperation {
            version: 1,
            thread,
            parents: causal,
            publisher: genesis.creator,
            body: ThreadOperationBody::Capture(AuthoredCapture {
                result: state.encode_current_msgpack().expect("State bytes").into(),
                author: author(),
            }),
        };
        causal = [operation.id().expect("operation")].into();
        parent = state.id();
        originals.push(SignedOperation::sign(&operation, &signer).expect("signed original"));
        states.push(state);
        blobs.push(blob);
    }
    let genesis = ThreadGenesisRecord {
        genesis: Some(
            thread_api::replication::opening::sign_genesis(&genesis, &signer).expect("genesis"),
        ),
        creator_authority: vec![4; 64],
        ..Default::default()
    };
    Fixture {
        root,
        store,
        states,
        blobs,
        originals,
        genesis,
        reference,
    }
}
impl Fixture {
    pub fn selection(&self) -> HistorySelection<'_> {
        HistorySelection {
            tip: self.states[1].id(),
            genesis: self.genesis.clone(),
            originals: &self.originals,
        }
    }
    pub fn spool_genesis(&self) -> ContentHash {
        ContentHash::compute(b"fixture independently selected Spool")
    }
    pub fn scope(&self) -> PublicationScope {
        PublicationScope {
            thread: self.reference.clone(),
            source: EndpointRef {
                kind: EndpointKind::Device as i32,
                public_key: vec![2; 32],
            },
            spool_genesis: self.spool_genesis(),
            sharing_policy: ContentHash::compute(b"fixture current policy"),
            command: "11111111-1111-1111-1111-111111111111"
                .parse()
                .expect("command"),
        }
    }
}
pub async fn received(source: &thread_api::publication::SourcePack) -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("isolated receiving directory");
    let readers = source.open_artifacts().await.expect("exact artifacts");
    for (mut reader, name) in readers.into_iter().zip(["source.pack", "source.idx"]) {
        let mut output = tokio::fs::File::create(directory.path().join(name))
            .await
            .expect("receiver file");
        tokio::io::copy(&mut reader, &mut output)
            .await
            .expect("actual transfer");
    }
    directory
}
