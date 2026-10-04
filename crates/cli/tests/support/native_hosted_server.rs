// SPDX-License-Identifier: Apache-2.0
//! Minimal v2 Weft fixture for publication and bounded Thread review tests.
// Shared fixtures expose entry points used by different test binaries.
#![allow(dead_code)]

use std::{
    net::Ipv4Addr,
    sync::{Arc, Mutex},
};

use api::{
    StreamingShape,
    framing::{
        StreamFrame, decode_request_frame, decode_request_prelude, decode_stream_frame,
        encode_stream_message, encode_success_response,
    },
    heddle::api::v1alpha2 as v2,
    method_descriptor,
};
use base64::Engine as _;
use crypto::{Ed25519Signer, Signer};
use hosted_client::hosted_runtime::hosted::HostedClient;
#[path = "native_hosted_https.rs"]
pub mod https;
#[path = "../../../thread-api/tests/support/native_witness.rs"]
pub mod witnessing;
use iroh::{Endpoint, RelayMode, endpoint::presets};
use prost::Message;
use tokio::task::JoinHandle;

const PUBLICATION_FRAME_BYTES: usize = 512 * 1024;
const ORIGINAL_BATCH_BYTES: usize = 256 * 1024;
const ORIGINAL_BATCH_OPERATIONS: usize = 128;
const PUBLICATION_OPERATIONS: usize = 10_000;
const PUBLICATION_METADATA_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Default)]
pub struct PublicationCapture {
    pub import_requests: Vec<v2::ImportSourceRequest>,
    pub calls: Vec<String>,
    /// Run Weft's shared verifier with request-time operation/resource facts.
    pub enforce_scope: bool,
    pub revision: Option<v2::RevisionRef>,
    pub thread_genesis: Option<v2::ThreadGenesisRecord>,
    pub native_authority: Option<v2::NativePublicProofBundleV1>,
    pub operations: Vec<v2::ReplicationOperations>,
    pub pack_data: Vec<u8>,
    pub index_data: Vec<u8>,
    pub started: Vec<v2::ThreadOverview>,
    pub evidence: Vec<v2::RecordEvidenceRequest>,
    /// Context commands the service accepted.
    pub contexts: Vec<v2::PutContextRequest>,
    /// Every context command delivered, accepted or not.
    pub context_attempts: Vec<v2::PutContextRequest>,
    /// Drop the next context command before applying it, as a transport
    /// failure would, so the client must redeliver its prepared record.
    pub interrupt_next_context: bool,
    /// Misbehave: answer every requested context with this signed operation
    /// as its only causal head, as a hostile remote redirecting the record to
    /// another record or Thread would.
    pub hostile_context_head: Option<v2::SignedRecord>,
    /// Misbehave: replace a requested discussion frontier with these originals.
    pub hostile_discussion_heads: Option<Vec<v2::SignedRecord>>,
    pub discussions: Vec<v2::OpenDiscussionRequest>,
    pub appends: Vec<v2::AppendDiscussionRequest>,
    pub resolutions: Vec<v2::ResolveDiscussionRequest>,
    pub discussion_operations: Vec<v2::SignedRecord>,
    /// Every signed discussion operation received, including rejected deliveries.
    pub received_discussion_operations: Vec<v2::SignedRecord>,
    /// Every discussion command ID the server admitted, replays included.
    pub delivered_command_ids: Vec<String>,
    /// Inject a publication failure to verify push's partial-result contract.
    pub reject_discussions: bool,
    /// Apply the next discussion command, then lose its receipt, so the
    /// client must redeliver an operation the server already holds.
    pub lose_next_discussion_receipt: bool,
    /// Every accepted source publication on the fixture Thread, in arrival
    /// order. Like Weft, the Thread's source heads are the published
    /// revisions no other publication names in its ancestry, so two writers
    /// publishing from one base leave two heads until one of them publishes a
    /// capture naming both.
    pub published: Vec<PublishedSource>,
}

#[derive(Clone, Debug)]
pub struct PublishedSource {
    pub thread: v2::ThreadRef,
    pub revision: v2::RevisionRef,
    pub thread_genesis: v2::ThreadGenesisRecord,
    pub native_authority: v2::NativePublicProofBundleV1,
    pub operations: Vec<v2::ReplicationOperations>,
    pub pack_data: Vec<u8>,
    pub index_data: Vec<u8>,
    /// Source revisions the publication's signed operations carry, its own
    /// included.
    pub ancestry: std::collections::BTreeSet<Vec<u8>>,
}

impl PublicationCapture {
    /// Non-dominated published revisions: Weft's `source_heads`.
    pub fn source_heads(&self) -> Vec<v2::RevisionRef> {
        self.source_heads_for(None)
    }

    fn source_heads_for(&self, thread: Option<&v2::ThreadRef>) -> Vec<v2::RevisionRef> {
        let mut heads: Vec<v2::RevisionRef> = Vec::new();
        for (index, published) in self.published.iter().enumerate() {
            if thread.is_some_and(|thread| thread != &published.thread) {
                continue;
            }
            let own = revision_state(&published.revision);
            let dominated = self.published.iter().enumerate().any(|(other, candidate)| {
                other != index
                    && candidate.thread == published.thread
                    && revision_state(&candidate.revision) != own
                    && candidate.ancestry.contains(&own)
            });
            if !dominated && !heads.contains(&published.revision) {
                heads.push(published.revision.clone());
            }
        }
        heads
    }
}

fn revision_state(revision: &v2::RevisionRef) -> Vec<u8> {
    match revision.revision.as_ref() {
        Some(v2::revision_ref::Revision::State(id)) => id.value.clone(),
        _ => panic!("fixture publications carry State revisions"),
    }
}

fn published_ancestry(
    operations: &[v2::ReplicationOperations],
) -> std::collections::BTreeSet<Vec<u8>> {
    operations
        .iter()
        .flat_map(|batch| &batch.operations)
        .filter_map(|record| {
            objects::object::thread_replication::ThreadOperation::decode(&record.canonical_record)
                .expect("published operation")
                .source_state()
                .expect("published source state")
                .map(|state| state.id().as_bytes().to_vec())
        })
        .collect()
}

#[derive(Clone)]
struct Fixture {
    spool: uuid::Uuid,
    thread_name: String,
    thread_id: Vec<u8>,
    owner_genesis: v2::SignedSpoolOwnerGenesis,
    owner: v2::OwnerState,
    witness_set: api::heddle::api::common::SignedHostedWitnessSetV1,
    _https: Arc<https::TestHttpsServer>,
    captured: Arc<Mutex<PublicationCapture>>,
}

pub fn enroll_device(spool: uuid::Uuid, home: &std::path::Path) {
    let (_, owner) = witnessing::ownership(spool);
    repo::device_authority::publish(
        home,
        &repo::device_authority::DeviceAuthority {
            owner,
            mint_roots: vec![],
            revoked_ids: vec![],
            revoked_mint_roots: vec![],
            revoked_publishers: vec![],
        },
        chrono::Utc::now().timestamp(),
    )
    .expect("independent test account enrollment");
}

pub fn enroll_source_author(home: &std::path::Path, publisher: &[u8; 32]) {
    use heddle_biscuit_verifier::signature_v1::BiscuitBuilderV1Ext as _;

    let now = chrono::Utc::now();
    let authority = repo::device_authority::load(home, now.timestamp()).expect("enrolled owner");
    let mint = biscuit_auth::KeyPair::from(
        &biscuit_auth::PrivateKey::from_bytes(&[71; 32], biscuit_auth::Algorithm::Ed25519)
            .expect("owner mint"),
    );
    let token = biscuit_auth::Biscuit::builder()
        .fact(format!("user(\"{}\")", uuid::Uuid::from_u128(2)).as_str())
        .expect("account")
        .fact(format!("device_pop_key(\"{}\")", hex::encode(publisher)).as_str())
        .expect("source proof key")
        .fact("session(\"offline-source-fixture\")")
        .expect("session")
        .fact(
            format!(
                "expires_at({})",
                (now + chrono::Duration::days(30)).to_rfc3339()
            )
            .as_str(),
        )
        .expect("expiry")
        .build_v1(&mint)
        .expect("device credential");
    repo::identity::source_author::publish(
        home,
        &authority,
        &mint.public().to_bytes().try_into().expect("mint key"),
        publisher,
        &token,
        now.timestamp(),
    )
    .expect("retain original account source proof");
}

pub async fn start(
    spool: uuid::Uuid,
    thread_name: impl Into<String>,
    thread_id: [u8; 32],
) -> (HostedClient, JoinHandle<()>, Arc<Mutex<PublicationCapture>>) {
    let (client, task, captured, _, _, _) = start_inner(spool, thread_name, thread_id, false).await;
    (client, task, captured)
}

pub async fn start_routed(
    spool: uuid::Uuid,
    thread_name: impl Into<String>,
    thread_id: [u8; 32],
) -> (
    HostedClient,
    JoinHandle<()>,
    Arc<Mutex<PublicationCapture>>,
    iroh::EndpointAddr,
    iroh::SecretKey,
    Arc<https::TestHttpsServer>,
) {
    start_inner(spool, thread_name, thread_id, true).await
}

async fn start_inner(
    spool: uuid::Uuid,
    thread_name: impl Into<String>,
    thread_id: [u8; 32],
    routed: bool,
) -> (
    HostedClient,
    JoinHandle<()>,
    Arc<Mutex<PublicationCapture>>,
    iroh::EndpointAddr,
    iroh::SecretKey,
    Arc<https::TestHttpsServer>,
) {
    let captured = Arc::new(Mutex::new(PublicationCapture::default()));
    let canonical_path: Vec<String> = if routed {
        vec!["spool".into(), "acme".into()]
    } else {
        vec!["acme".into(), "widgets".into()]
    };
    let (owner_genesis, owner) = witnessing::ownership_for(spool, &canonical_path);
    let mut secret_bytes = [0; 32];
    getrandom::fill(&mut secret_bytes).expect("fixture endpoint key");
    let secret = iroh::SecretKey::from_bytes(&secret_bytes);
    let server = Endpoint::builder(presets::Minimal)
        .secret_key(secret.clone())
        .alpns(vec![api::HOSTED_ALPN_V1.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .expect("hosted test bind address")
        .bind()
        .await
        .expect("hosted test endpoint");
    let server_addr = server.addr();
    let server_key = server.id().as_bytes().to_vec();
    let root = Ed25519Signer::from_seed(&[7; 32]).expect("independent deployment root");
    let ephemeral = Ed25519Signer::from_seed(&secret.to_bytes()).expect("endpoint signer");
    let direct = server_addr
        .ip_addrs()
        .next()
        .expect("direct address")
        .to_string();
    let descriptor =
        https::signed_descriptor(&server_addr.id.to_string(), &direct, &root, &ephemeral);
    let witness_metadata = Arc::new(Mutex::new(None));
    let generated = Arc::clone(&witness_metadata);
    let https = Arc::new(https::TestHttpsServer::start_with(|authority| {
        use std::collections::{HashMap, VecDeque};
        let set = witnessing::witness_set_for(&format!("https://{authority}"), "clone-test-key");
        *generated.lock().expect("witness set") = Some(set.clone());
        HashMap::from([
            (
                "/.well-known/heddle/iroh-endpoint".into(),
                VecDeque::from(vec![descriptor; 256]),
            ),
            (
                "/.well-known/heddle/hosted-witnesses".into(),
                VecDeque::from(vec![set.encode_to_vec(); 256]),
            ),
        ])
    }));
    let witness_set = witness_metadata
        .lock()
        .expect("witness set")
        .clone()
        .expect("generated set");
    let fixture = Fixture {
        spool,
        thread_name: thread_name.into(),
        thread_id: thread_id.to_vec(),
        owner_genesis,
        owner,
        witness_set,
        _https: Arc::clone(&https),
        captured: Arc::clone(&captured),
    };
    let server_task = tokio::spawn(async move {
        while let Some(incoming) = server.accept().await {
            let connection = incoming.await.expect("hosted test handshake");
            let fixture = fixture.clone();
            let server_key = server_key.clone();
            let connection_task = async move {
                while let Ok((send, recv)) = connection.accept_bi().await {
                    tokio::spawn(serve_call(send, recv, fixture.clone(), server_key.clone()));
                }
            };
            if routed {
                tokio::spawn(connection_task);
            } else {
                connection_task.await;
                server.close().await;
                break;
            }
        }
    });
    use heddle_biscuit_verifier::signature_v1::BiscuitBuilderV1Ext as _;
    let signer = Ed25519Signer::from_seed(&[71; 32]).expect("client device");
    let mint = biscuit_auth::KeyPair::from(
        &biscuit_auth::PrivateKey::from_bytes(&[71; 32], biscuit_auth::Algorithm::Ed25519)
            .expect("owner mint"),
    );
    let token = biscuit_auth::Biscuit::builder()
        .fact(r#"user("clone-test")"#)
        .expect("subject")
        .fact(format!("device_pop_key(\"{}\")", hex::encode(signer.public_key())).as_str())
        .expect("proof")
        .fact(r#"session("native-fixture")"#)
        .expect("session")
        .fact(
            format!(
                "expires_at({})",
                (chrono::Utc::now() + chrono::Duration::days(30)).to_rfc3339()
            )
            .as_str(),
        )
        .expect("expiry")
        .build_v1(&mint)
        .expect("client credential")
        .to_base64()
        .expect("token");
    let configuration = config::ClientConfig::default()
        .with_token(wire::AuthToken::new(token, "clone-test"))
        .with_auth_proof_key_pem(signer.to_pem().expect("device PEM"))
        .with_authenticated_principal("clone-test")
        .with_descriptor_trust(
            "clone-test-key",
            root.public_key().try_into().expect("root key"),
        )
        .with_tls_ca_certificate_pem(https.certificate_pem.clone());
    let client = HostedClient::connect_server(&https.authority, &configuration)
        .await
        .expect("trusted hosted fixture");
    (client, server_task, captured, server_addr, secret, https)
}

async fn serve_call(
    mut send: iroh::endpoint::SendStream,
    mut recv: iroh::endpoint::RecvStream,
    fixture: Fixture,
    server_key: Vec<u8>,
) {
    let mut request = Vec::new();
    let (method, context, prelude_len) = loop {
        let chunk = recv
            .read_chunk(api::framing::MAX_CONTROL_BODY + 6)
            .await
            .expect("read request prelude")
            .expect("request prelude");
        request.extend_from_slice(&chunk);
        if let Some((prelude, consumed)) =
            decode_request_prelude(&request).expect("decode request prelude")
        {
            break (prelude.method.to_string(), prelude.context, consumed);
        }
    };
    fixture
        .captured
        .lock()
        .expect("capture calls")
        .calls
        .push(method.clone());
    let streaming = method_descriptor(&method)
        .map(|descriptor| descriptor.streaming)
        .or_else(|| api::v2::method_descriptor(&method).map(|descriptor| descriptor.streaming))
        .expect("registered hosted method");
    if fixture
        .captured
        .lock()
        .expect("scope enforcement")
        .enforce_scope
    {
        let root = biscuit_auth::KeyPair::from(
            &biscuit_auth::PrivateKey::from_bytes(&[71; 32], biscuit_auth::Algorithm::Ed25519)
                .expect("fixture root"),
        );
        let operation = method.rsplit('/').next().expect("method name");
        // The fixture serves one canonical spool. Like Weft, caller identity
        // and account ownership carry no spool resource fact.
        let resource = match operation {
            "DescribeEndpoint" | "GetIdentity" | "ObserveIdentity" => None,
            "ObserveOwnership" => {
                read_request_body(&mut recv, &mut request).await;
                let request = v2::ObserveOwnershipRequest::decode(
                    decode_request_frame(&request).expect("owner frame").body,
                )
                .expect("owner request");
                request.spool.map(|_| ("spool", "spool/acme"))
            }
            _ => Some(("spool", "spool/acme")),
        };
        let token = base64::engine::general_purpose::URL_SAFE.encode(&context.bearer_capability);
        if let Err(error) = heddle_biscuit_verifier::verify_at_with_resource(
            &token,
            &[root.public()],
            &[],
            operation,
            resource,
            chrono::Utc::now(),
        ) {
            println!("scope admission refused {operation}: {error}");
            let failure = api::heddle::api::common::CallFailure {
                code: api::heddle::api::common::CallFailureCode::Unauthenticated as i32,
                message: "invalid or revoked bearer capability".into(),
                ..Default::default()
            };
            let bytes = if streaming == StreamingShape::Unary {
                api::framing::encode_failure_response(&failure)
            } else {
                api::framing::encode_stream_failure(&failure)
            }
            .expect("scope failure");
            send.write_all(&bytes).await.expect("scope rejection");
            send.finish().expect("scope response");
            return;
        }
    }
    match streaming {
        StreamingShape::Unary | StreamingShape::ClientStreaming => match method.as_str() {
            "/heddle.api.v1alpha2.EndpointService/DescribeEndpoint" => {
                let response = v2::DescribeEndpointResponse {
                    endpoint: Some(v2::EndpointRef {
                        kind: v2::EndpointKind::Weft as i32,
                        public_key: server_key,
                    }),
                    supported_packages: vec!["heddle.api.v1alpha2".into()],
                    implemented_methods: vec![
                        "/heddle.api.v1alpha2.WorkspaceService/ResolveResources".into(),
                        "/heddle.api.v1alpha2.SpoolService/ObserveSpool".into(),
                        "/heddle.api.v1alpha2.OwnerAuthorizationService/ObserveOwnership".into(),
                        "/heddle.api.v1alpha2.IdentityService/ObserveIdentity".into(),
                        "/heddle.api.v1alpha2.IntegrationService/ImportSource".into(),
                        "/heddle.api.v1alpha2.ThreadService/ObserveThreads".into(),
                        "/heddle.api.v1alpha2.ThreadService/ObserveThread".into(),
                        "/heddle.api.v1alpha2.SyncService/PublishContent".into(),
                        "/heddle.api.v1alpha2.SyncService/Fetch".into(),
                        "/heddle.api.v1alpha2.ThreadService/StartThread".into(),
                        "/heddle.api.v1alpha2.IdentityService/GetIdentity".into(),
                        "/heddle.api.v1alpha2.EvidenceService/RecordEvidence".into(),
                        "/heddle.api.v1alpha2.CollaborationService/ObserveCollaboration".into(),
                        "/heddle.api.v1alpha2.CollaborationService/OpenDiscussion".into(),
                        "/heddle.api.v1alpha2.CollaborationService/PutContext".into(),
                        "/heddle.api.v1alpha2.CollaborationService/AppendTurn".into(),
                        "/heddle.api.v1alpha2.CollaborationService/ResolveDiscussion".into(),
                    ],
                    default_read_budget: Some(v2::ReadBudget {
                        max_items: 64,
                        max_frame_bytes: 64 * 1024,
                        max_snapshot_bytes: 1024 * 1024,
                    }),
                    max_pending_batch_bytes: 1024 * 1024,
                    protocol: Some(thread_api::hybrid::protocol()),
                    understood_signed_record_formats: vec![
                        objects::object::thread_replication::GENESIS_FORMAT.into(),
                        objects::object::thread_replication::OPERATION_FORMAT.into(),
                        objects::object::thread_replication::ownership_claim::FORMAT.into(),
                    ],
                    ..Default::default()
                };
                write_unary(&mut send, &response).await;
            }
            "/heddle.api.v1alpha2.WorkspaceService/ResolveResources" => {
                read_request_body(&mut recv, &mut request).await;
                let body = decode_request_frame(&request)
                    .expect("decode ResolveResources frame")
                    .body;
                let request = v2::ResolveResourcesRequest::decode(body)
                    .expect("decode ResolveResources request");
                let thread_name = (request.selectors.len() == 1)
                    .then(|| request.selectors[0].selector.as_ref())
                    .flatten()
                    .and_then(|selector| match selector {
                        v2::resource_selector::Selector::ThreadName(value) => {
                            Some(value.name.as_str())
                        }
                        _ => None,
                    });
                let spool = v2::SpoolRef {
                    id: fixture.spool.to_string(),
                };
                let entity = if let Some(name) = thread_name {
                    let started = fixture
                        .captured
                        .lock()
                        .expect("started Threads")
                        .started
                        .iter()
                        .find(|thread| thread.name == name)
                        .and_then(|thread| thread.r#ref.as_ref())
                        .and_then(|reference| reference.id.as_ref())
                        .map(|id| id.value.clone());
                    let thread_id = if name == fixture.thread_name {
                        fixture.thread_id.clone()
                    } else if let Some(started) = started {
                        started
                    } else {
                        assert_eq!(name, "main", "fixture's landing target");
                        vec![24; 32]
                    };
                    v2::entity_ref::Entity::Thread(v2::ThreadRef {
                        spool: Some(spool),
                        id: Some(v2::ThreadId { value: thread_id }),
                    })
                } else {
                    v2::entity_ref::Entity::Spool(spool)
                };
                write_unary(
                    &mut send,
                    &v2::ResolveResourcesResponse {
                        results: vec![v2::ResourceResolution {
                            resource: Some(v2::EntityRef {
                                entity: Some(entity),
                            }),
                            coverage: v2::Coverage::Complete as i32,
                            ..Default::default()
                        }],
                        ..Default::default()
                    },
                )
                .await;
            }
            "/heddle.api.v1alpha2.IntegrationService/ImportSource" => {
                read_request_body(&mut recv, &mut request).await;
                let body = v2::ImportSourceRequest::decode(
                    decode_request_frame(&request).expect("import frame").body,
                )
                .expect("import request");
                fixture
                    .captured
                    .lock()
                    .expect("capture import")
                    .import_requests
                    .push(body.clone());
                write_unary(
                    &mut send,
                    &v2::MutationResponse {
                        receipt: Some(v2::MutationReceipt {
                            client_operation_id: body.client_operation_id.clone(),
                            endpoint: Some(v2::EndpointRef {
                                kind: v2::EndpointKind::Weft as i32,
                                public_key: server_key,
                            }),
                            outcome: Some(v2::mutation_receipt::Outcome::PendingOperation(
                                v2::RecordRef {
                                    spool: body.destination,
                                    id: body.client_operation_id,
                                },
                            )),
                            ..Default::default()
                        }),
                    },
                )
                .await;
            }
            "/heddle.api.v1alpha2.ThreadService/StartThread" => {
                read_request_body(&mut recv, &mut request).await;
                let body = v2::StartThreadRequest::decode(
                    decode_request_frame(&request).expect("start frame").body,
                )
                .expect("start request");
                let record = body.thread_genesis.expect("original genesis");
                api::native_witness::verify_genesis_authority(
                    body.native_genesis_authority
                        .as_ref()
                        .expect("client binding"),
                    &record,
                    &body.creator_authority,
                )
                .expect("client binding before request PoP");
                let original = record;
                let signed = crypto::thread_operation::SignedGenesis {
                    canonical: original.canonical_record,
                    signature: original.signatures[0].signature.clone(),
                };
                let genesis = signed.verify().expect("verify original genesis");
                assert_eq!(genesis.spool, fixture.spool.to_string());
                let overview = v2::ThreadOverview {
                    name: genesis.name.clone(),
                    r#ref: Some(v2::ThreadRef {
                        spool: Some(v2::SpoolRef {
                            id: genesis.spool.clone(),
                        }),
                        id: Some(v2::ThreadId {
                            value: genesis.id().expect("Thread id").as_bytes().to_vec(),
                        }),
                    }),
                    ..Default::default()
                };
                fixture
                    .captured
                    .lock()
                    .expect("started Threads")
                    .started
                    .push(overview.clone());
                write_unary(
                    &mut send,
                    &v2::ThreadMutationResponse {
                        thread: Some(overview),
                        ..Default::default()
                    },
                )
                .await;
            }
            "/heddle.api.v1alpha2.IdentityService/GetIdentity" => {
                read_request_body(&mut recv, &mut request).await;
                let frame = decode_request_frame(&request).expect("identity frame");
                let root = biscuit_auth::KeyPair::from(
                    &biscuit_auth::PrivateKey::from_bytes(
                        &[71; 32],
                        biscuit_auth::Algorithm::Ed25519,
                    )
                    .expect("root"),
                );
                let token = heddle_biscuit_verifier::signature_v1::verify(
                    &frame.context.bearer_capability,
                    root.public(),
                )
                .expect("client capability");
                let credential =
                    heddle_biscuit_verifier::inspect_verified_credential(&token, &root.public())
                        .expect("caller identity");
                write_unary(
                    &mut send,
                    &v2::GetIdentityResponse {
                        identity: Some(v2::PrincipalRecord {
                            id: "principal-test".into(),
                            account_id: uuid::Uuid::from_u128(2).to_string(),
                            ..Default::default()
                        }),
                        current_credential: Some(v2::CurrentCredentialRecord {
                            kind: v2::CredentialKind::Device as i32,
                            subject: "clone-test".into(),
                            proof_public_key: credential.proof_public_key,
                            acting_agent_id: credential.agent_id.unwrap_or_default(),
                            thread_control_authority: witnessing::envelope(&fixture.owner, &token),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                )
                .await;
            }
            "/heddle.api.v1alpha2.EvidenceService/RecordEvidence" => {
                read_request_body(&mut recv, &mut request).await;
                let body = v2::RecordEvidenceRequest::decode(
                    decode_request_frame(&request).expect("evidence frame").body,
                )
                .expect("evidence request");
                let projection = body.evidence.as_ref().expect("signed evidence");
                let original = thread_api::evidence::verify_evidence(
                    projection.evidence.as_ref().expect("evidence original"),
                )
                .expect("verify evidence");
                assert_eq!(original.spool, fixture.spool);
                assert_eq!(original.thread.as_bytes().as_slice(), fixture.thread_id);
                let reference = projection.r#ref.clone().expect("evidence ref");
                fixture
                    .captured
                    .lock()
                    .expect("record evidence")
                    .evidence
                    .push(body.clone());
                write_unary(
                    &mut send,
                    &v2::MutationResponse {
                        receipt: Some(v2::MutationReceipt {
                            client_operation_id: body.client_operation_id,
                            outcome: Some(v2::mutation_receipt::Outcome::Applied(v2::Applied {
                                resulting_versions: vec![v2::ExpectedVersion {
                                    resource: Some(v2::EntityRef {
                                        entity: Some(v2::entity_ref::Entity::Evidence(reference)),
                                    }),
                                    ..Default::default()
                                }],
                            })),
                            ..Default::default()
                        }),
                    },
                )
                .await;
            }
            "/heddle.api.v1alpha2.CollaborationService/OpenDiscussion" => {
                read_request_body(&mut recv, &mut request).await;
                let body = v2::OpenDiscussionRequest::decode(
                    decode_request_frame(&request)
                        .expect("discussion frame")
                        .body,
                )
                .expect("discussion request");
                admit_discussion(
                    &mut send,
                    &fixture,
                    &body.client_operation_id,
                    body.signed_operation.as_ref().expect("signed discussion"),
                    server_key,
                    |capture| capture.discussions.push(body.clone()),
                )
                .await;
            }
            "/heddle.api.v1alpha2.CollaborationService/AppendTurn" => {
                read_request_body(&mut recv, &mut request).await;
                let body = v2::AppendDiscussionRequest::decode(
                    decode_request_frame(&request).expect("append frame").body,
                )
                .expect("append request");
                admit_discussion(
                    &mut send,
                    &fixture,
                    &body.client_operation_id,
                    body.signed_operation.as_ref().expect("signed append"),
                    server_key,
                    |capture| capture.appends.push(body.clone()),
                )
                .await;
            }
            "/heddle.api.v1alpha2.CollaborationService/ResolveDiscussion" => {
                read_request_body(&mut recv, &mut request).await;
                let body = v2::ResolveDiscussionRequest::decode(
                    decode_request_frame(&request).expect("resolve frame").body,
                )
                .expect("resolve request");
                admit_discussion(
                    &mut send,
                    &fixture,
                    &body.client_operation_id,
                    body.signed_operation.as_ref().expect("signed resolve"),
                    server_key,
                    |capture| capture.resolutions.push(body.clone()),
                )
                .await;
            }
            "/heddle.api.v1alpha2.CollaborationService/PutContext" => {
                read_request_body(&mut recv, &mut request).await;
                let body = v2::PutContextRequest::decode(
                    decode_request_frame(&request).expect("context frame").body,
                )
                .expect("context request");
                admit_context(&mut send, &fixture, body, server_key).await;
            }
            other => panic!("unexpected hosted unary method: {other}"),
        },
        StreamingShape::ServerStreaming => {
            match method.as_str() {
                "/heddle.api.v1alpha2.ThreadService/ObserveThreads" => {
                    serve_observe_threads(&mut send, server_key, &fixture).await
                }
                "/heddle.api.v1alpha2.ThreadService/ObserveThread" => {
                    read_request_body(&mut recv, &mut request).await;
                    let body = v2::ObserveThreadRequest::decode(
                        decode_request_frame(&request).expect("thread frame").body,
                    )
                    .expect("thread request");
                    if body.sections.contains(&(v2::ThreadSection::Review as i32)) {
                        hosted_client::hosted_runtime::hosted::test_server::serve_native_thread_review(&mut send, &mut recv, &mut request, server_key).await;
                    } else {
                        let capture = fixture.captured.lock().expect("source view").clone();
                        for frame in snapshot_frames(server_key) {
                            let payload = matches!(
                                frame.body,
                                Some(v2::stream_frame::Body::Data(_))
                            )
                            .then(|| {
                                v2::thread_event::Payload::Overview(v2::ThreadOverview {
                                    r#ref: body.thread.clone(),
                                    name: fixture.thread_name.clone(),
                                    source_heads: capture.source_heads_for(body.thread.as_ref()),
                                    ..Default::default()
                                })
                            });
                            write_message(
                                &mut send,
                                &v2::ThreadEvent {
                                    frame: Some(frame),
                                    payload,
                                },
                            )
                            .await;
                        }
                    }
                }
                "/heddle.api.v1alpha2.SpoolService/ObserveSpool" => {
                    for frame in snapshot_frames(server_key) {
                        let payload = matches!(frame.body, Some(v2::stream_frame::Body::Data(_)))
                            .then(|| {
                                v2::spool_event::Payload::Spool(v2::SpoolOverview {
                                    r#ref: Some(v2::SpoolRef {
                                        id: fixture.spool.to_string(),
                                    }),
                                    version: vec![7; 32],
                                    owner_genesis: Some(fixture.owner_genesis.clone()),
                                    path_segments: fixture
                                        .owner
                                        .resource_keyring
                                        .as_ref()
                                        .expect("keyring")
                                        .canonical_spool_path_segments
                                        .clone(),
                                    ..Default::default()
                                })
                            });
                        write_message(
                            &mut send,
                            &v2::SpoolEvent {
                                frame: Some(frame),
                                payload,
                            },
                        )
                        .await;
                    }
                }
                "/heddle.api.v1alpha2.OwnerAuthorizationService/ObserveOwnership" => {
                    for frame in snapshot_frames(server_key) {
                        let payload = matches!(frame.body, Some(v2::stream_frame::Body::Data(_)))
                            .then(|| v2::ownership_event::Payload::Owner(fixture.owner.clone()));
                        write_message(
                            &mut send,
                            &v2::OwnershipEvent {
                                frame: Some(frame),
                                payload,
                            },
                        )
                        .await;
                    }
                }
                "/heddle.api.v1alpha2.IdentityService/ObserveIdentity" => {
                    let root = biscuit_auth::KeyPair::from(
                        &biscuit_auth::PrivateKey::from_bytes(
                            &[71; 32],
                            biscuit_auth::Algorithm::Ed25519,
                        )
                        .expect("root"),
                    );
                    let token = heddle_biscuit_verifier::signature_v1::verify(
                        &context.bearer_capability,
                        root.public(),
                    )
                    .expect("caller capability");
                    let mut sequence = 1;
                    for mut frame in snapshot_frames(server_key) {
                        let data = matches!(frame.body, Some(v2::stream_frame::Body::Data(_)));
                        if data {
                            write_message(
                                &mut send,
                                &v2::IdentityEvent {
                                    frame: Some(v2::StreamFrame {
                                        sequence,
                                        body: frame.body.clone(),
                                    }),
                                    payload: Some(v2::identity_event::Payload::Identity(
                                        v2::PrincipalRecord {
                                            id: "principal-test".into(),
                                            account_id: uuid::Uuid::from_u128(2).to_string(),
                                            personal_spool: Some(v2::SpoolAddress {
                                                r#ref: Some(v2::SpoolRef {
                                                    id: fixture.spool.to_string(),
                                                }),
                                                path_segments: vec!["personal".into()],
                                            }),
                                            ..Default::default()
                                        },
                                    )),
                                },
                            )
                            .await;
                            sequence += 1;
                        }
                        frame.sequence = sequence;
                        sequence += 1;
                        let payload = data.then(|| {
                            v2::identity_event::Payload::CurrentCredential(
                                v2::CurrentCredentialRecord {
                                    kind: v2::CredentialKind::Device as i32,
                                    subject: "clone-test".into(),
                                    thread_control_authority: witnessing::envelope(
                                        &fixture.owner,
                                        &token,
                                    ),
                                    ..Default::default()
                                },
                            )
                        });
                        write_message(
                            &mut send,
                            &v2::IdentityEvent {
                                frame: Some(frame),
                                payload,
                            },
                        )
                        .await;
                    }
                }
                "/heddle.api.v1alpha2.CollaborationService/ObserveCollaboration" => {
                    read_request_body(&mut recv, &mut request).await;
                    let body = v2::ObserveCollaborationRequest::decode(
                        decode_request_frame(&request)
                            .expect("observe collaboration frame")
                            .body,
                    )
                    .expect("observe collaboration request");
                    let payloads = collaboration_payloads(&fixture, &body);
                    let mut sequence = 1;
                    for mut frame in snapshot_frames(server_key) {
                        if matches!(frame.body, Some(v2::stream_frame::Body::Data(_))) {
                            for payload in &payloads {
                                frame.sequence = sequence;
                                sequence += 1;
                                write_message(
                                    &mut send,
                                    &v2::CollaborationEvent {
                                        frame: Some(frame.clone()),
                                        payload: Some(payload.clone()),
                                    },
                                )
                                .await;
                            }
                        } else {
                            frame.sequence = sequence;
                            sequence += 1;
                            write_message(
                                &mut send,
                                &v2::CollaborationEvent {
                                    frame: Some(frame),
                                    ..Default::default()
                                },
                            )
                            .await;
                        }
                    }
                }
                other => panic!("unexpected hosted observation: {other}"),
            }
        }
        StreamingShape::Bidirectional => {
            let buffered = request.split_off(prelude_len);
            match method.as_str() {
                "/heddle.api.v1alpha2.SyncService/PublishContent" => {
                    serve_publication(send, recv, buffered, fixture).await;
                    return;
                }
                "/heddle.api.v1alpha2.SyncService/Fetch" => {
                    serve_fetch(send, recv, buffered, fixture, server_key).await;
                    return;
                }
                other => panic!("unexpected hosted bidirectional method: {other}"),
            }
        }
    }
    send.finish().expect("finish hosted response");
}

/// Verify exactly the command identity authenticated by the native original,
/// before recording any collaboration mutation. Replays retain one original.
async fn admit_discussion(
    send: &mut iroh::endpoint::SendStream,
    fixture: &Fixture,
    command_id: &str,
    signed: &v2::SignedRecord,
    server_key: Vec<u8>,
    capture_request: impl FnOnce(&mut PublicationCapture),
) {
    let operation = thread_api::collaboration::verify(signed).expect("verify discussion");
    let objects::object::thread_replication::ThreadOperationBody::Discussion(bytes) =
        operation.body
    else {
        panic!("discussion original")
    };
    let envelope = objects::object::CollaborationOperationEnvelope::decode(&bytes)
        .expect("original discussion command");
    use api::heddle::api::common::CallFailureCode;
    // Weft's order: the command ID must be the signed key before anything is
    // recorded; then an identical redelivery replays and a different command
    // under the same ID conflicts.
    let failure = {
        let mut capture = fixture.captured.lock().expect("discussion admission");
        capture.received_discussion_operations.push(signed.clone());
        if command_id != envelope.operation.idempotency_key.as_str() {
            Some((
                CallFailureCode::InvalidArgument,
                "command ID differs from signed operation",
            ))
        } else if capture.reject_discussions {
            Some((
                CallFailureCode::InvalidArgument,
                "injected discussion rejection",
            ))
        } else if let Some(previous) = capture.discussion_operations.iter().find(|previous| {
            let original = decode_discussion(previous);
            original.operation.idempotency_key == envelope.operation.idempotency_key
        }) {
            if previous == signed {
                capture.delivered_command_ids.push(command_id.into());
                None
            } else {
                Some((
                    CallFailureCode::FailedPrecondition,
                    "operation ID names another command",
                ))
            }
        } else {
            capture_request(&mut capture);
            capture.discussion_operations.push(signed.clone());
            capture.delivered_command_ids.push(command_id.into());
            if std::mem::take(&mut capture.lose_next_discussion_receipt) {
                Some((CallFailureCode::Unavailable, "receipt lost after apply"))
            } else {
                None
            }
        }
    };
    if let Some((code, message)) = failure {
        send.write_all(
            &api::framing::encode_failure_response(&api::heddle::api::common::CallFailure {
                code: code as i32,
                message: message.into(),
                ..Default::default()
            })
            .expect("command failure"),
        )
        .await
        .expect("discussion rejection");
        return;
    }
    write_unary(
        send,
        &v2::MutationResponse {
            receipt: Some(v2::MutationReceipt {
                client_operation_id: command_id.into(),
                endpoint: Some(v2::EndpointRef {
                    kind: v2::EndpointKind::Weft as i32,
                    public_key: server_key,
                }),
                outcome: Some(v2::mutation_receipt::Outcome::Applied(
                    v2::Applied::default(),
                )),
                ..Default::default()
            }),
        },
    )
    .await;
}

/// One accepted context revision, keyed the way Weft partitions it: by the
/// Thread its signed scope names.
struct AcceptedContext {
    thread: objects::object::ContentHash,
    id: objects::object::ContentHash,
    parents: std::collections::BTreeSet<objects::object::ContentHash>,
    context: uuid::Uuid,
}

fn accepted_context(signed: &v2::SignedRecord) -> AcceptedContext {
    let operation = thread_api::collaboration::verify(signed).expect("verify context original");
    let context = operation
        .context_revision()
        .expect("decode context revision")
        .expect("context original");
    AcceptedContext {
        thread: operation.thread,
        id: operation.id().expect("context operation id"),
        parents: operation.parents.clone(),
        context: context.id,
    }
}

/// Weft's `heads()`: the current frontier of one context within one Thread.
/// A context RecordRef is bound to a single Thread, so the same record has
/// no frontier at all in any other Thread.
fn context_heads(
    capture: &PublicationCapture,
    thread: objects::object::ContentHash,
    context: uuid::Uuid,
) -> (Vec<objects::object::ContentHash>, Vec<u8>) {
    let accepted: Vec<_> = capture
        .contexts
        .iter()
        .filter_map(|request| request.signed_operation.as_ref())
        .map(accepted_context)
        .filter(|record| record.thread == thread && record.context == context)
        .collect();
    let mut heads: Vec<_> = accepted
        .iter()
        .filter(|record| {
            !accepted
                .iter()
                .any(|child| child.parents.contains(&record.id))
        })
        .map(|record| record.id)
        .collect();
    heads.sort();
    let version = heads.iter().flat_map(|id| id.as_bytes().to_vec()).collect();
    (heads, version)
}

/// Admit a context command with Weft's order of checks: dedup on the command
/// ID, the thread-scoped expected-version check, the record's Thread binding,
/// then causal parents. Nothing is recorded on rejection.
async fn admit_context(
    send: &mut iroh::endpoint::SendStream,
    fixture: &Fixture,
    body: v2::PutContextRequest,
    server_key: Vec<u8>,
) {
    use api::heddle::api::common::CallFailureCode;
    let signed = body.signed_operation.clone().expect("signed context");
    let record = accepted_context(&signed);
    let failure = {
        let mut capture = fixture.captured.lock().expect("context admission");
        capture.context_attempts.push(body.clone());
        let replay = capture
            .contexts
            .iter()
            .find(|previous| previous.client_operation_id == body.client_operation_id)
            .map(|previous| previous.signed_operation.as_ref() == Some(&signed));
        let (heads, version) = context_heads(&capture, record.thread, record.context);
        let parents: std::collections::BTreeSet<_> = heads.iter().copied().collect();
        let bound_elsewhere = capture
            .contexts
            .iter()
            .filter_map(|request| request.signed_operation.as_ref())
            .map(accepted_context)
            .any(|other| other.context == record.context && other.thread != record.thread);
        let known_parents = capture
            .contexts
            .iter()
            .filter_map(|request| request.signed_operation.as_ref())
            .map(accepted_context)
            .filter(|other| other.thread == record.thread)
            .map(|other| other.id)
            .collect::<std::collections::BTreeSet<_>>();
        (if std::mem::take(&mut capture.interrupt_next_context) {
            Some((CallFailureCode::Unavailable, "context delivery interrupted"))
        } else if let Some(identical) = replay {
            (!identical).then_some((
                CallFailureCode::FailedPrecondition,
                "operation ID names another command",
            ))
        } else if body.expected_version.is_empty() {
            (!heads.is_empty() || !record.parents.is_empty()).then_some((
                CallFailureCode::AlreadyExists,
                "context already exists; use observed version",
            ))
        } else if body.expected_version != version
            || parents != record.parents
            || heads.len() != record.parents.len()
        {
            Some((
                CallFailureCode::Aborted,
                "context changed; refresh before revising",
            ))
        } else {
            None
        })
        .or_else(|| {
            bound_elsewhere.then_some((
                CallFailureCode::Internal,
                "collaboration RecordRef is bound to another Thread",
            ))
        })
        .or_else(|| {
            (!record.parents.is_subset(&known_parents)).then_some((
                CallFailureCode::FailedPrecondition,
                "operation requires accepted causal parents",
            ))
        })
        .or_else(|| {
            if replay.is_none() {
                capture.contexts.push(body.clone());
            }
            None
        })
    };
    if let Some((code, message)) = failure {
        send.write_all(
            &api::framing::encode_failure_response(&api::heddle::api::common::CallFailure {
                code: code as i32,
                message: message.into(),
                ..Default::default()
            })
            .expect("context failure"),
        )
        .await
        .expect("context rejection");
        return;
    }
    write_unary(
        send,
        &v2::MutationResponse {
            receipt: Some(v2::MutationReceipt {
                client_operation_id: body.client_operation_id,
                endpoint: Some(v2::EndpointRef {
                    kind: v2::EndpointKind::Weft as i32,
                    public_key: server_key,
                }),
                outcome: Some(v2::mutation_receipt::Outcome::Applied(
                    v2::Applied::default(),
                )),
                ..Default::default()
            }),
        },
    )
    .await;
}

fn decode_discussion(signed: &v2::SignedRecord) -> objects::object::DecodedCollaborationOperation {
    let operation = thread_api::collaboration::verify(signed).expect("verify original");
    let objects::object::thread_replication::ThreadOperationBody::Discussion(bytes) =
        operation.body
    else {
        panic!("discussion original")
    };
    objects::object::CollaborationOperationEnvelope::decode(&bytes).expect("decode original")
}

fn collaboration_payloads(
    fixture: &Fixture,
    request: &v2::ObserveCollaborationRequest,
) -> Vec<v2::collaboration_event::Payload> {
    use objects::object::{
        CollaborationOperationBodyV1 as Body, materialize_repository_collaboration,
    };
    use v2::collaboration_event::Payload;
    let capture = fixture.captured.lock().expect("collaboration snapshot");
    let originals: Vec<_> = capture
        .discussion_operations
        .iter()
        .map(decode_discussion)
        .collect();
    let materialized = materialize_repository_collaboration(originals.clone())
        .expect("materialize hosted discussions");
    let mut payloads = Vec::new();
    for (id, discussion) in materialized.discussions {
        if !request.discussions.is_empty()
            && !request.discussions.iter().any(|r| r.id == id.to_string())
        {
            continue;
        }
        if request.annotations.is_some() {
            continue;
        }
        let scope = originals
            .iter()
            .find(|o| o.operation.discussion_id == id)
            .expect("discussion original")
            .operation
            .metadata
            .as_ref()
            .expect("discussion metadata")
            .scope
            .clone();
        let reference = v2::RecordRef {
            spool: Some(v2::SpoolRef {
                id: fixture.spool.to_string(),
            }),
            id: id.to_string(),
        };
        let operations: Vec<_> = capture
            .discussion_operations
            .iter()
            .zip(&originals)
            .filter(|(_, o)| o.operation.discussion_id == id)
            .collect();
        let heads: Vec<_> = operations
            .iter()
            .filter(|(_, o)| discussion.heads.contains(&o.operation_id))
            .map(|(s, _)| {
                thread_api::collaboration::verify(s)
                    .expect("head")
                    .id()
                    .expect("head id")
                    .as_bytes()
                    .to_vec()
            })
            .collect();
        if !request.discussions.is_empty()
            && let Some(hostile) = &capture.hostile_discussion_heads
        {
            let heads: Vec<_> = hostile
                .iter()
                .map(|record| {
                    thread_api::collaboration::operation_id(record)
                        .expect("hostile discussion head")
                        .as_bytes()
                        .to_vec()
                })
                .collect();
            payloads.push(Payload::Discussion(v2::DiscussionRecord {
                r#ref: Some(reference),
                version: heads.concat(),
                causal_heads: heads,
                ..Default::default()
            }));
            if request.include_operations {
                payloads.extend(hostile.iter().cloned().map(Payload::Operation));
            }
            continue;
        }
        let (audience, label) =
            thread_api::collaboration::audience(&discussion.visibility).expect("audience");
        payloads.push(Payload::Discussion(v2::DiscussionRecord {
            r#ref: Some(reference.clone()),
            version: heads.concat(),
            anchor: Some(
                thread_api::collaboration::anchor_ref(&discussion.anchor, &scope).expect("anchor"),
            ),
            title: discussion.title,
            blocking: discussion.blocking,
            turn_count: discussion.turns.len() as u64,
            status: if discussion.resolution.is_some() {
                v2::discussion_record::Status::Resolved as i32
            } else {
                v2::discussion_record::Status::Open as i32
            },
            causal_heads: heads,
            audience: audience as i32,
            audience_label: label,
            ..Default::default()
        }));
        for (signed, decoded) in &operations {
            let record = &decoded.operation;
            if let Body::Open { turn, .. } | Body::AppendTurn { turn } = &record.body {
                let outer = thread_api::collaboration::verify(signed).expect("turn");
                payloads.push(Payload::Turn(v2::DiscussionTurn {
                    r#ref: Some(v2::RecordRef {
                        id: decoded.operation_id.to_string_full(),
                        spool: reference.spool.clone(),
                    }),
                    discussion: Some(reference.clone()),
                    body: turn.body.clone(),
                    principal_id: record
                        .metadata
                        .as_ref()
                        .expect("turn metadata")
                        .actor
                        .principal_id
                        .to_string(),
                    created_at: Some(prost_types::Timestamp {
                        seconds: record.occurred_at_ms.div_euclid(1000),
                        nanos: (record.occurred_at_ms.rem_euclid(1000) * 1_000_000) as i32,
                    }),
                    causal_id: outer.id().expect("turn id").as_bytes().to_vec(),
                    ..Default::default()
                }));
            }
            if request.include_operations {
                payloads.push(Payload::Operation((*signed).clone()));
            }
        }
    }
    for context in &capture.contexts {
        let record = accepted_context(context.signed_operation.as_ref().expect("context original"));
        if !request.contexts.is_empty()
            && !request
                .contexts
                .iter()
                .any(|r| r.id == record.context.to_string())
        {
            continue;
        }
        if request.include_operations {
            payloads.push(Payload::Operation(
                context.signed_operation.clone().expect("context original"),
            ));
        }
    }
    // A requested context projects its current frontier within the Thread
    // the record is bound to, as Weft's view does.
    for requested in &request.contexts {
        if let Some(hostile) = &capture.hostile_context_head {
            let head = thread_api::collaboration::operation_id(hostile)
                .expect("hostile head ID")
                .as_bytes()
                .to_vec();
            payloads.push(Payload::Context(v2::ContextRecord {
                r#ref: Some(v2::RecordRef {
                    spool: Some(v2::SpoolRef {
                        id: fixture.spool.to_string(),
                    }),
                    id: requested.id.clone(),
                }),
                version: head.clone(),
                causal_id: head.clone(),
                causal_heads: vec![head],
                ..Default::default()
            }));
            if request.include_operations {
                payloads.push(Payload::Operation(hostile.clone()));
            }
            continue;
        }
        let Some(latest) = capture
            .contexts
            .iter()
            .filter_map(|context| context.signed_operation.as_ref())
            .map(accepted_context)
            .rfind(|record| record.context.to_string() == requested.id)
        else {
            continue;
        };
        let (heads, version) = context_heads(&capture, latest.thread, latest.context);
        payloads.push(Payload::Context(v2::ContextRecord {
            r#ref: Some(v2::RecordRef {
                spool: Some(v2::SpoolRef {
                    id: fixture.spool.to_string(),
                }),
                id: latest.context.to_string(),
            }),
            version,
            causal_id: latest.id.as_bytes().to_vec(),
            causal_parents: latest
                .parents
                .iter()
                .map(|id| id.as_bytes().to_vec())
                .collect(),
            causal_heads: heads.iter().map(|id| id.as_bytes().to_vec()).collect(),
            ..Default::default()
        }));
    }
    payloads
}

fn snapshot_frames(server_key: Vec<u8>) -> Vec<v2::StreamFrame> {
    vec![
        v2::stream_frame::Body::Open(v2::StreamOpen {
            source: Some(v2::EndpointRef {
                kind: v2::EndpointKind::Weft as i32,
                public_key: server_key,
            }),
            binding_digest: vec![8; 32],
            authority_valid_until: Some(prost_types::Timestamp {
                seconds: chrono::Utc::now().timestamp() + 240,
                nanos: 0,
            }),
            accepted_budget: Some(v2::ReadBudget {
                max_items: 64,
                max_frame_bytes: 65536,
                max_snapshot_bytes: 1048576,
            }),
            ..Default::default()
        }),
        v2::stream_frame::Body::Data(v2::StreamData {
            kind: v2::StreamDataKind::Snapshot as i32,
        }),
        v2::stream_frame::Body::Checkpoint(v2::StreamCheckpoint {
            cursor: vec![1],
            snapshot_complete: true,
            page: Some(v2::PageInfo {
                exhausted: true,
                ..Default::default()
            }),
            ..Default::default()
        }),
        v2::stream_frame::Body::Complete(v2::StreamComplete { cursor: vec![1] }),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, body)| v2::StreamFrame {
        sequence: (index + 1) as u64,
        body: Some(body),
    })
    .collect()
}

async fn read_request_body(recv: &mut iroh::endpoint::RecvStream, request: &mut Vec<u8>) {
    while let Some(chunk) = recv
        .read_chunk(api::framing::MAX_CONTROL_BODY + 6)
        .await
        .expect("read request body")
    {
        request.extend_from_slice(&chunk);
    }
}

async fn write_unary<M: Message>(send: &mut iroh::endpoint::SendStream, response: &M) {
    send.write_chunk(
        encode_success_response(&response.encode_to_vec())
            .expect("encode unary response")
            .into(),
    )
    .await
    .expect("write unary response");
}

async fn serve_observe_threads(
    send: &mut iroh::endpoint::SendStream,
    server_key: Vec<u8>,
    fixture: &Fixture,
) {
    let source = v2::EndpointRef {
        kind: v2::EndpointKind::Weft as i32,
        public_key: server_key,
    };
    let overview = v2::ThreadOverview {
        name: fixture.thread_name.clone(),
        r#ref: Some(thread_ref(fixture)),
        source_heads: fixture
            .captured
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .source_heads_for(Some(&thread_ref(fixture))),
        ..Default::default()
    };
    let capture = fixture.captured.lock().expect("started listing").clone();
    let mut overviews = capture.started.clone();
    if capture.thread_genesis.is_some() && !overviews.iter().any(|v| v.r#ref == overview.r#ref) {
        overviews.push(overview);
    }
    for overview in &mut overviews {
        overview.source_heads = fixture
            .captured
            .lock()
            .expect("sources")
            .source_heads_for(overview.r#ref.as_ref());
    }
    let first = if overviews.is_empty() {
        None
    } else {
        Some(overviews.remove(0))
    };
    let events = [
        v2::ThreadListEvent {
            frame: Some(v2::StreamFrame {
                sequence: 1,
                body: Some(v2::stream_frame::Body::Open(v2::StreamOpen {
                    source: Some(source),
                    binding_digest: vec![9; 32],
                    accepted_budget: Some(v2::ReadBudget {
                        max_items: 64,
                        max_frame_bytes: 64 * 1024,
                        max_snapshot_bytes: 1024 * 1024,
                    }),
                    ..Default::default()
                })),
            }),
            ..Default::default()
        },
        v2::ThreadListEvent {
            frame: Some(v2::StreamFrame {
                sequence: 2,
                body: Some(v2::stream_frame::Body::Data(v2::StreamData {
                    kind: v2::StreamDataKind::Snapshot as i32,
                })),
            }),
            payload: first.map(v2::thread_list_event::Payload::Thread),
        },
        v2::ThreadListEvent {
            frame: Some(v2::StreamFrame {
                sequence: 3,
                body: Some(v2::stream_frame::Body::Checkpoint(v2::StreamCheckpoint {
                    cursor: vec![1],
                    snapshot_complete: true,
                    page: Some(v2::PageInfo {
                        exhausted: true,
                        ..Default::default()
                    }),
                    ..Default::default()
                })),
            }),
            ..Default::default()
        },
        v2::ThreadListEvent {
            frame: Some(v2::StreamFrame {
                sequence: 4,
                body: Some(v2::stream_frame::Body::Complete(v2::StreamComplete {
                    cursor: vec![1],
                })),
            }),
            ..Default::default()
        },
    ];
    let mut sequence = 1;
    for mut event in events {
        if event.payload.is_none()
            && matches!(
                event.frame.as_ref().and_then(|f| f.body.as_ref()),
                Some(v2::stream_frame::Body::Data(_))
            )
        {
            continue;
        }
        if matches!(
            event.frame.as_ref().and_then(|f| f.body.as_ref()),
            Some(v2::stream_frame::Body::Checkpoint(_))
        ) {
            for overview in overviews.drain(..) {
                write_message(
                    send,
                    &v2::ThreadListEvent {
                        frame: Some(v2::StreamFrame {
                            sequence,
                            body: Some(v2::stream_frame::Body::Data(v2::StreamData {
                                kind: v2::StreamDataKind::Snapshot as i32,
                            })),
                        }),
                        payload: Some(v2::thread_list_event::Payload::Thread(overview)),
                    },
                )
                .await;
                sequence += 1;
            }
        }
        event.frame.as_mut().expect("listing frame").sequence = sequence;
        sequence += 1;
        write_message(send, &event).await;
    }
}

fn thread_ref(fixture: &Fixture) -> v2::ThreadRef {
    v2::ThreadRef {
        spool: Some(v2::SpoolRef {
            id: fixture.spool.to_string(),
        }),
        id: Some(v2::ThreadId {
            value: fixture.thread_id.clone(),
        }),
    }
}

async fn serve_publication(
    mut send: iroh::endpoint::SendStream,
    mut recv: iroh::endpoint::RecvStream,
    mut buffered: Vec<u8>,
    fixture: Fixture,
) {
    let opening: v2::PublishContentClientFrame = read_message(&mut recv, &mut buffered).await;
    if opening.encoded_len() > PUBLICATION_FRAME_BYTES {
        reject_publication(&mut send, "publication frame budget exceeded").await;
        return;
    }
    let Some(v2::publish_content_client_frame::Body::Open(open)) = opening.body.clone() else {
        panic!("native publication must start with Open");
    };
    assert!(
        open.thread == Some(thread_ref(&fixture))
            || fixture
                .captured
                .lock()
                .expect("started publication")
                .started
                .iter()
                .any(|t| t.r#ref == open.thread)
    );
    let mut logical = opening.clone();
    if let Some(v2::publish_content_client_frame::Body::Open(open)) = logical.body.as_mut() {
        open.checkpoint = None;
    }
    let digest = typed_digest("thread-source-transfer-v1", &logical.encode_to_vec());
    let checkpoint = v2::TransferCheckpoint {
        transfer_id: digest[..16].to_vec(),
        plan_digest: digest.to_vec(),
        resume_token: Vec::new(),
        committed_bytes: 0,
    };
    write_message(
        &mut send,
        &v2::PublishContentServerFrame {
            body: Some(v2::publish_content_server_frame::Body::Ready(
                v2::TransferReady {
                    protocol: open.protocol.clone(),
                    endpoint: open.destination.clone(),
                    thread: open.thread.clone(),
                    current: open.revision.clone(),
                    checkpoint: Some(checkpoint.clone()),
                    budget: Some(v2::ReadBudget {
                        max_items: 10_000,
                        max_frame_bytes: PUBLICATION_FRAME_BYTES as u32,
                        max_snapshot_bytes: 16 * 1024 * 1024,
                    }),
                    ..Default::default()
                },
            )),
        },
    )
    .await;

    let mut original_bytes = 0usize;
    let mut operation_count = 0usize;
    let mut accepted = PublicationCapture {
        revision: open.revision.clone(),
        ..Default::default()
    };
    loop {
        let frame: v2::PublishContentClientFrame = read_message(&mut recv, &mut buffered).await;
        assert_eq!(frame.client_operation_id, opening.client_operation_id);
        let failure = if frame.encoded_len() > PUBLICATION_FRAME_BYTES {
            Some("publication frame budget exceeded")
        } else {
            match &frame.body {
                Some(v2::publish_content_client_frame::Body::ThreadGenesis(genesis)) => {
                    original_bytes += genesis.encoded_len();
                    (genesis.encoded_len() > ORIGINAL_BATCH_BYTES)
                        .then_some("publication original metadata budget exceeded")
                }
                Some(v2::publish_content_client_frame::Body::Operations(batch)) => {
                    original_bytes += batch.encoded_len();
                    operation_count += batch.operations.len();
                    if batch.operations.is_empty()
                        || batch.operations.len() > ORIGINAL_BATCH_OPERATIONS
                        || batch.authority_admissions.len() > ORIGINAL_BATCH_OPERATIONS
                    {
                        Some("original authority batch exceeds bounds")
                    } else if batch.encoded_len() > ORIGINAL_BATCH_BYTES {
                        Some("publication original metadata budget exceeded")
                    } else {
                        thread_api::authority_admission::match_batch(batch)
                            .err()
                            .map(|_| "invalid original authority batch")
                    }
                }
                _ => None,
            }
        };
        let failure = failure.or({
            if operation_count > PUBLICATION_OPERATIONS {
                Some("publication operation budget exceeded")
            } else if original_bytes > PUBLICATION_METADATA_BYTES {
                Some("publication original metadata budget exceeded")
            } else {
                None
            }
        });
        if let Some(message) = failure {
            reject_publication(&mut send, message).await;
            return;
        }
        match frame.body {
            Some(v2::publish_content_client_frame::Body::ThreadGenesis(genesis)) => {
                assert!(accepted.thread_genesis.replace(genesis).is_none());
                write_checkpoint(&mut send, &checkpoint).await;
            }
            Some(v2::publish_content_client_frame::Body::Operations(operations)) => {
                accepted.operations.push(operations);
                write_checkpoint(&mut send, &checkpoint).await;
            }
            Some(v2::publish_content_client_frame::Body::Pack(chunk)) => {
                let extent = chunk.extent.as_ref().expect("native pack chunk extent");
                let destination = if extent.kind == v2::pack_extent::Kind::NativePack as i32 {
                    &mut accepted.pack_data
                } else {
                    assert_eq!(extent.kind, v2::pack_extent::Kind::NativeIndex as i32);
                    &mut accepted.index_data
                };
                assert_eq!(extent.offset, destination.len() as u64);
                assert_eq!(extent.length, chunk.data.len() as u64);
                destination.extend_from_slice(&chunk.data);
                write_checkpoint(&mut send, &checkpoint).await;
            }
            Some(v2::publish_content_client_frame::Body::Finish(finish)) => {
                assert_eq!(finish.checkpoint, Some(checkpoint));
                assert_eq!(accepted.pack_data.len() as u64, open.packs[0].length);
                assert_eq!(accepted.index_data.len() as u64, open.packs[1].length);
                assert!(accepted.thread_genesis.is_some());
                assert!(!accepted.operations.is_empty());
                let previous = fixture
                    .captured
                    .lock()
                    .expect("previous publication")
                    .published
                    .iter()
                    .rev()
                    .find(|p| Some(&p.thread) == open.thread.as_ref())
                    .map(|p| p.native_authority.clone());
                let proof = witnessing::bundle(
                    &fixture.owner_genesis,
                    &fixture.owner,
                    accepted.thread_genesis.as_ref().expect("client original"),
                    &accepted.operations,
                    &fixture.witness_set,
                    previous,
                );
                accepted.native_authority = Some(proof.clone());
                for batch in &mut accepted.operations {
                    batch.native_authority = Some(proof.clone());
                }
                let inventory = inventory_digest(&open.packs);
                {
                    let mut capture = fixture.captured.lock().expect("accepted publication");
                    accepted.calls = std::mem::take(&mut capture.calls);
                    accepted.started = std::mem::take(&mut capture.started);
                    accepted.evidence = std::mem::take(&mut capture.evidence);
                    accepted.contexts = std::mem::take(&mut capture.contexts);
                    accepted.context_attempts = std::mem::take(&mut capture.context_attempts);
                    accepted.interrupt_next_context = capture.interrupt_next_context;
                    accepted.hostile_context_head = capture.hostile_context_head.clone();
                    accepted.hostile_discussion_heads = capture.hostile_discussion_heads.clone();
                    accepted.discussions = std::mem::take(&mut capture.discussions);
                    accepted.appends = std::mem::take(&mut capture.appends);
                    accepted.resolutions = std::mem::take(&mut capture.resolutions);
                    accepted.discussion_operations =
                        std::mem::take(&mut capture.discussion_operations);
                    accepted.received_discussion_operations =
                        std::mem::take(&mut capture.received_discussion_operations);
                    accepted.delivered_command_ids =
                        std::mem::take(&mut capture.delivered_command_ids);
                    accepted.reject_discussions = capture.reject_discussions;
                    accepted.lose_next_discussion_receipt = capture.lose_next_discussion_receipt;
                    accepted.published = std::mem::take(&mut capture.published);
                    accepted.published.push(PublishedSource {
                        thread: open.thread.clone().expect("published Thread"),
                        revision: open.revision.clone().expect("published revision"),
                        thread_genesis: accepted.thread_genesis.clone().expect("published genesis"),
                        native_authority: proof.clone(),
                        operations: accepted.operations.clone(),
                        pack_data: accepted.pack_data.clone(),
                        index_data: accepted.index_data.clone(),
                        ancestry: published_ancestry(&accepted.operations),
                    });
                    *capture = accepted;
                }
                write_message(
                    &mut send,
                    &v2::PublishContentServerFrame {
                        body: Some(v2::publish_content_server_frame::Body::Receipt(
                            v2::PublicationReceipt {
                                native_authority: Some(proof),
                                import_authority: None,
                                client_operation_id: opening.client_operation_id,
                                destination: open.destination,
                                thread: open.thread,
                                revision: open.revision,
                                sharing_policy_version: if open.sharing_policy_version.is_empty() {
                                    vec![3; 32]
                                } else {
                                    open.sharing_policy_version
                                },
                                accepted_inventory: Some(v2::ObjectAddress {
                                    algorithm: "blake3".into(),
                                    digest: inventory.to_vec(),
                                }),
                                outcome: Some(v2::publication_receipt::Outcome::Accepted(
                                    v2::Applied::default(),
                                )),
                            },
                        )),
                    },
                )
                .await;
                send.finish().expect("finish native publication response");
                return;
            }
            other => panic!("unexpected native publication frame: {other:?}"),
        }
    }
}

async fn reject_publication(send: &mut iroh::endpoint::SendStream, message: &str) {
    send.write_chunk(
        api::framing::encode_stream_failure(&api::heddle::api::common::CallFailure {
            code: api::heddle::api::common::CallFailureCode::InvalidArgument as i32,
            message: message.into(),
            ..Default::default()
        })
        .expect("encode publication failure")
        .into(),
    )
    .await
    .expect("write publication failure");
    send.finish().expect("finish rejected publication");
}

async fn serve_fetch(
    mut send: iroh::endpoint::SendStream,
    mut recv: iroh::endpoint::RecvStream,
    mut buffered: Vec<u8>,
    fixture: Fixture,
    server_key: Vec<u8>,
) {
    let opening: v2::FetchClientFrame = read_message(&mut recv, &mut buffered).await;
    let Some(v2::fetch_client_frame::Body::Open(open)) = opening.body else {
        panic!("native fetch must start with Open");
    };
    let reference = open.thread.clone().expect("fetch Thread");
    // Serve the publication whose revision was requested: each head of a
    // multi-head Thread is fetched on its own, as from Weft.
    let accepted = {
        let capture = fixture
            .captured
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        match open.revision.as_ref() {
            Some(requested) => capture
                .published
                .iter()
                .rev()
                .find(|published| &published.revision == requested && published.thread == reference)
                .cloned()
                .expect("requested revision was published"),
            None => capture
                .published
                .last()
                .cloned()
                .expect("published revision"),
        }
    };
    let genesis = accepted.thread_genesis.clone();
    let revision = accepted.revision.clone();
    let artifacts = [&accepted.pack_data, &accepted.index_data];
    let packs = pack_extents(artifacts);
    let checkpoint = v2::TransferCheckpoint {
        transfer_id: vec![7; 16],
        plan_digest: inventory_digest(&packs).to_vec(),
        resume_token: Vec::new(),
        committed_bytes: 0,
    };
    write_message(
        &mut send,
        &v2::FetchServerFrame {
            body: Some(v2::fetch_server_frame::Body::Ready(v2::TransferReady {
                protocol: open.protocol.clone(),
                native_authority: Some(accepted.native_authority.clone()),
                endpoint: Some(v2::EndpointRef {
                    kind: v2::EndpointKind::Weft as i32,
                    public_key: server_key,
                }),
                thread: Some(reference),
                current: Some(revision.clone()),
                owner_genesis: Some(fixture.owner_genesis),
                ownership: Some(fixture.owner),
                thread_genesis: Some(genesis),
                packs: packs.clone(),
                checkpoint: Some(checkpoint.clone()),
                budget: Some(v2::ReadBudget {
                    max_items: 10_000,
                    max_frame_bytes: PUBLICATION_FRAME_BYTES as u32,
                    max_snapshot_bytes: 16 * 1024 * 1024,
                }),
                full_closure_available: true,
                ..Default::default()
            })),
        },
    )
    .await;
    for operations in accepted.operations {
        write_message(
            &mut send,
            &v2::FetchServerFrame {
                body: Some(v2::fetch_server_frame::Body::Operations(operations)),
            },
        )
        .await;
    }
    for (planned, data) in packs.iter().zip(artifacts) {
        for (offset, chunk) in data.chunks(32 * 1024).enumerate() {
            let mut extent = planned.clone();
            extent.offset = (offset * 32 * 1024) as u64;
            extent.length = chunk.len() as u64;
            extent.extent_digest = Some(v2::ObjectAddress {
                algorithm: "blake3".into(),
                digest: blake3::hash(chunk).as_bytes().to_vec(),
            });
            write_message(
                &mut send,
                &v2::FetchServerFrame {
                    body: Some(v2::fetch_server_frame::Body::Pack(v2::PackChunk {
                        extent: Some(extent),
                        data: chunk.to_vec(),
                    })),
                },
            )
            .await;
        }
    }
    let mut complete_checkpoint = checkpoint;
    complete_checkpoint.committed_bytes = artifacts.iter().map(|data| data.len() as u64).sum();
    write_message(
        &mut send,
        &v2::FetchServerFrame {
            body: Some(v2::fetch_server_frame::Body::Complete(v2::FetchComplete {
                revision: Some(revision),
                checkpoint: Some(complete_checkpoint),
                closure: v2::Coverage::Complete as i32,
                missing: Vec::new(),
            })),
        },
    )
    .await;
    send.finish().expect("finish native fetch response");
}

async fn read_message<M: Message + Default>(
    recv: &mut iroh::endpoint::RecvStream,
    buffered: &mut Vec<u8>,
) -> M {
    loop {
        if let Some((frame, consumed)) = decode_stream_frame(buffered).expect("native frame") {
            let StreamFrame::Message(body) = frame else {
                panic!("expected native message frame, got {frame:?}");
            };
            let body = body.to_vec();
            buffered.drain(..consumed);
            return M::decode(body.as_slice()).expect("native protobuf message");
        }
        let chunk = recv
            .read_chunk(api::framing::MAX_CONTROL_BODY + 5)
            .await
            .expect("read native frame")
            .expect("native message frame");
        buffered.extend_from_slice(&chunk);
    }
}

async fn write_message<M: Message>(send: &mut iroh::endpoint::SendStream, message: &M) {
    send.write_chunk(
        encode_stream_message(&message.encode_to_vec())
            .expect("encode native frame")
            .into(),
    )
    .await
    .expect("write native frame");
}

async fn write_checkpoint(
    send: &mut iroh::endpoint::SendStream,
    checkpoint: &v2::TransferCheckpoint,
) {
    write_message(
        send,
        &v2::PublishContentServerFrame {
            body: Some(v2::publish_content_server_frame::Body::Checkpoint(
                checkpoint.clone(),
            )),
        },
    )
    .await;
}

fn pack_extents(artifacts: [&Vec<u8>; 2]) -> Vec<v2::PackExtent> {
    artifacts
        .into_iter()
        .enumerate()
        .map(|(index, bytes)| {
            assert!(!bytes.is_empty());
            let address = v2::ObjectAddress {
                algorithm: "blake3".into(),
                digest: blake3::hash(bytes).as_bytes().to_vec(),
            };
            v2::PackExtent {
                pack: Some(address.clone()),
                kind: if index == 0 {
                    v2::pack_extent::Kind::NativePack
                } else {
                    v2::pack_extent::Kind::NativeIndex
                } as i32,
                offset: 0,
                length: bytes.len() as u64,
                extent_digest: Some(address),
            }
        })
        .collect()
}

fn inventory_digest(packs: &[v2::PackExtent]) -> [u8; 32] {
    let mut inventory = Vec::new();
    for extent in packs {
        extent
            .encode_length_delimited(&mut inventory)
            .expect("encode native inventory");
    }
    typed_digest("thread-source-inventory-v1", &inventory)
}

fn typed_digest(kind: &str, bytes: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(kind.as_bytes());
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(&[0]);
    hasher.update(bytes);
    *hasher.finalize().as_bytes()
}
