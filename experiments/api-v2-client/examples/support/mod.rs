// SPDX-License-Identifier: Apache-2.0
//! Loopback contract peer, not a Weft implementation. It verifies request PoP
//! against an ephemeral pinned fixture key. It does not implement Biscuit/root
//! attachment verification, policy enforcement, or durable transfer storage.
use std::{
    net::Ipv4Addr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use api::{
    framing,
    heddle::api::v1alpha1::{CallContext, RequestProof},
    v2::{MethodDescriptor, client::Rpc},
};
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use heddle_thread_client::{
    Remote,
    contract::*,
    rpc,
    transport::{Authorize, Error, IrohTransport},
};
use iroh::{
    Endpoint, RelayMode,
    endpoint::{RecvStream, SendStream, presets},
};
use prost::Message;
use tokio::{sync::watch, task::JoinHandle};

#[derive(Clone, Copy, Default)]
#[allow(dead_code)] // Failure scenarios are selected by integration tests.
pub enum Scenario {
    #[default]
    Normal,
    Interrupted,
    OversizedHeader,
    WrongRevision,
    Reset,
    OversizedBatch,
}

pub struct FixtureAuth(SigningKey);
impl Authorize for FixtureAuth {
    async fn context(
        &self,
        method: &'static MethodDescriptor,
        body: &[u8],
    ) -> Result<CallContext, Error> {
        let identity = format!(
            "principal:device-key:{}",
            hex::encode(self.0.verifying_key().as_bytes())
        );
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| Error::Io(e.to_string()))?
            .as_millis() as i64;
        let mut nonce = [0_u8; 16];
        getrandom::fill(&mut nonce).map_err(|e| Error::Io(e.to_string()))?;
        let signature = self.0.sign(&api::signing::unary_bytes(
            &identity,
            method.path,
            timestamp,
            &nonce,
            body,
        ));
        Ok(CallContext {
            request_proof: Some(RequestProof {
                algorithm: "ed25519".into(),
                signing_identity: identity,
                timestamp_millis: timestamp,
                nonce: nonce.to_vec(),
                signature: signature.to_bytes().to_vec(),
            }),
            ..Default::default()
        })
    }
}

pub type Client = Remote<IrohTransport<FixtureAuth>>;

pub struct Peer {
    server: Endpoint,
    browser: Endpoint,
    task: JoinHandle<()>,
    signer: SigningKey,
    pub calls: Arc<Mutex<Vec<String>>>,
    pub active: Arc<AtomicUsize>,
}

pub fn thread_ref() -> ThreadRef {
    ThreadRef {
        spool: Some(SpoolRef {
            id: "11111111-1111-4111-8111-111111111111".into(),
        }),
        id: Some(ThreadId { value: vec![3; 32] }),
    }
}

pub fn revision() -> RevisionRef {
    RevisionRef {
        spool: thread_ref().spool,
        revision: Some(revision_ref::Revision::GitCommitOid(
            "2a67bcaf7446def0bd01e4b9b79b2cbb9e203401".into(),
        )),
    }
}

fn overview(version: u8, outcome: &str) -> ThreadOverview {
    ThreadOverview {
        r#ref: Some(thread_ref()),
        name: "api-v2-client".into(),
        version: vec![version],
        intent: Some(ThreadIntent {
            outcome: outcome.into(),
            version: vec![version],
            ..Default::default()
        }),
        tip: Some(revision()),
        capture_count: 1,
        ..Default::default()
    }
}

pub fn read_budget() -> ReadBudget {
    ReadBudget {
        max_items: 32,
        max_frame_bytes: 16 * 1024,
        max_snapshot_bytes: 128 * 1024,
    }
}

impl Peer {
    pub async fn start(scenario: Scenario) -> Result<Self> {
        let server = endpoint(true).await?;
        let browser = endpoint(false).await?;
        let mut seed = [0_u8; 32];
        getrandom::fill(&mut seed).map_err(|e| anyhow::anyhow!("fixture entropy: {e}"))?;
        let signer = SigningKey::from_bytes(&seed);
        let calls = Arc::new(Mutex::new(vec![]));
        let active = Arc::new(AtomicUsize::new(0));
        let description = DescribeEndpointResponse {
            endpoint: Some(EndpointRef {
                public_key: server.id().as_bytes().to_vec(),
                kind: EndpointKind::Weft as i32,
            }),
            supported_packages: vec!["heddle.api.v2alpha1".into()],
            implemented_methods: vec![
                rpc::EndpointServiceDescribeEndpoint::METHOD.path.into(),
                rpc::ThreadServiceObserveThread::METHOD.path.into(),
                rpc::ThreadServiceReviseIntent::METHOD.path.into(),
                rpc::ContentServiceReadContent::METHOD.path.into(),
            ],
            default_read_budget: Some(read_budget()),
            maximum_read_budget: Some(read_budget()),
            max_pending_batch_bytes: 128 * 1024,
            max_cursor_bytes: 4096,
            max_concurrent_streams_per_connection: 2048,
            ..Default::default()
        };
        let state = Arc::new(State {
            scenario,
            description,
            key: signer.verifying_key(),
            calls: calls.clone(),
            active: active.clone(),
            thread: watch::channel(overview(1, "Exercise a Thread-shaped client")).0,
        });
        let listener = server.clone();
        let task = tokio::spawn(async move {
            while let Some(incoming) = listener.accept().await {
                let state = state.clone();
                tokio::spawn(async move {
                    let Ok(connection) = incoming.await else {
                        return;
                    };
                    while let Ok((send, recv)) = connection.accept_bi().await {
                        let state = state.clone();
                        tokio::spawn(async move {
                            // Client cancellation deliberately resets streams.
                            let _ = serve(send, recv, state).await;
                        });
                    }
                });
            }
        });
        Ok(Self {
            server,
            browser,
            task,
            signer,
            calls,
            active,
        })
    }

    pub async fn connect(&self) -> Result<Client> {
        let connection = self
            .browser
            .connect(self.server.addr(), api::HOSTED_ALPN_V1)
            .await?;
        let key = *connection.remote_id().as_bytes();
        let transport = IrohTransport::new(
            connection,
            FixtureAuth(self.signer.clone()),
            16 * 1024,
            Duration::from_secs(5),
        )?;
        Ok(Remote::discover(transport, key, EndpointKind::Weft).await?)
    }

    pub fn routes(&self) -> Result<Vec<String>> {
        Ok(self
            .calls
            .lock()
            .map_err(|_| anyhow::anyhow!("call recorder poisoned"))?
            .clone())
    }
    pub fn active_streams(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }
    pub async fn close(self) {
        self.browser.close().await;
        self.server.close().await;
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn endpoint(accept: bool) -> Result<Endpoint> {
    Ok(Endpoint::builder(presets::Minimal)
        .transport_config(
            iroh::endpoint::QuicTransportConfig::builder()
                .max_concurrent_bidi_streams(2048u32.into())
                .stream_receive_window((64u32 * 1024).into())
                .receive_window((16u32 * 1024 * 1024).into())
                .build(),
        )
        .alpns(if accept {
            vec![api::HOSTED_ALPN_V1.to_vec()]
        } else {
            vec![]
        })
        .relay_mode(RelayMode::Disabled)
        .bind_addr((Ipv4Addr::LOCALHOST, 0))?
        .bind()
        .await?)
}

struct State {
    scenario: Scenario,
    description: DescribeEndpointResponse,
    key: VerifyingKey,
    calls: Arc<Mutex<Vec<String>>>,
    active: Arc<AtomicUsize>,
    thread: watch::Sender<ThreadOverview>,
}

async fn serve(mut send: SendStream, mut recv: RecvStream, state: Arc<State>) -> Result<()> {
    let bytes = recv.read_to_end(128 * 1024).await?;
    let request = framing::decode_request_frame(&bytes)?;
    let proof = request
        .context
        .request_proof
        .context("fixture requires request PoP")?;
    ensure!(
        proof.algorithm == "ed25519" && proof.nonce.len() == 16,
        "proof shape"
    );
    let identity = format!("principal:device-key:{}", hex::encode(state.key.as_bytes()));
    ensure!(proof.signing_identity == identity, "fixture key identity");
    let signed = api::signing::unary_bytes(
        &identity,
        request.method,
        proof.timestamp_millis,
        &proof.nonce,
        request.body,
    );
    let signature = ed25519_dalek::Signature::from_slice(&proof.signature)?;
    state.key.verify_strict(&signed, &signature)?;
    let method = api::v2::method_descriptor(request.method).context("v2 route required")?;
    ensure!(
        method
            .client_operation_id(request.body)?
            .unwrap_or_default()
            == request.context.client_operation_id,
        "context/body operation identity"
    );
    state
        .calls
        .lock()
        .map_err(|_| anyhow::anyhow!("recorder poisoned"))?
        .push(request.method.into());
    match method.route {
        api::v2::MethodRoute::EndpointServiceDescribeEndpoint => {
            send.write_all(&framing::encode_success_response(
                &state.description.encode_to_vec(),
            )?)
            .await?;
        }
        api::v2::MethodRoute::ThreadServiceObserveThread => {
            let request = ObserveThreadRequest::decode(request.body)?;
            ensure!(
                request.thread == Some(thread_ref()),
                "stable Thread reference"
            );
            return observe(send, request, state).await;
        }
        api::v2::MethodRoute::ThreadServiceReviseIntent => {
            let request = ReviseIntentRequest::decode(request.body)?;
            ensure!(request.thread == Some(thread_ref()), "mutation Thread");
            let current = state.thread.borrow().clone();
            ensure!(
                request.expected_intent_version == current.version,
                "intent CAS"
            );
            let proposed = request.proposed_intent.context("proposed intent")?;
            let updated = overview(current.version[0] + 1, &proposed.outcome);
            state.thread.send_replace(updated.clone());
            let response = ThreadMutationResponse {
                receipt: Some(MutationReceipt {
                    client_operation_id: request.client_operation_id,
                    endpoint: state.description.endpoint.clone(),
                    outcome: Some(mutation_receipt::Outcome::Applied(Applied::default())),
                    ..Default::default()
                }),
                thread: Some(updated),
            };
            send.write_all(&framing::encode_success_response(
                &response.encode_to_vec(),
            )?)
            .await?;
        }
        api::v2::MethodRoute::ContentServiceReadContent => {
            let request = ReadContentRequest::decode(request.body)?;
            ensure!(request.revision == Some(revision()), "exact revision");
            for selection in request.selections {
                let mut revision = revision();
                if matches!(state.scenario, Scenario::WrongRevision) {
                    revision.spool = None;
                }
                let event = ContentEvent {
                    selection_id: selection.selection_id.clone(),
                    revision: Some(revision.clone()),
                    payload: Some(content_event::Payload::Blob(BlobChunk {
                        data: b"hello\n".to_vec(),
                        total_size: 6,
                        object_hash: vec![9; 32],
                        range_complete: true,
                        offset: 0,
                    })),
                };
                send_message(&mut send, &event).await?;
                if !matches!(state.scenario, Scenario::Interrupted) {
                    send_message(
                        &mut send,
                        &ContentEvent {
                            selection_id: selection.selection_id,
                            revision: Some(revision.clone()),
                            payload: Some(content_event::Payload::SelectionComplete(
                                SectionStatus {
                                    section: "blob".into(),
                                    coverage: Coverage::Complete as i32,
                                    computed_for: Some(revision),
                                    ..Default::default()
                                },
                            )),
                        },
                    )
                    .await?;
                }
            }
        }
        _ => anyhow::bail!("unimplemented fixture route"),
    }
    send.finish()?;
    Ok(())
}

async fn send_message(send: &mut SendStream, message: &impl Message) -> Result<()> {
    // Fragment every header across writes, exercising incremental framing on a
    // real Iroh stream instead of a Vec-backed fake transport.
    let bytes = framing::encode_stream_message(&message.encode_to_vec())?;
    send.write_all(&bytes[..2]).await?;
    send.write_all(&bytes[2..]).await?;
    Ok(())
}

async fn observe(
    mut send: SendStream,
    request: ObserveThreadRequest,
    state: Arc<State>,
) -> Result<()> {
    struct Active(Arc<AtomicUsize>);
    impl Drop for Active {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    state.active.fetch_add(1, Ordering::SeqCst);
    let _active = Active(state.active.clone());
    if matches!(state.scenario, Scenario::OversizedHeader) {
        send.write_all(&[0, 0, 1, 0, 0]).await?; // 64 KiB, no body; reject immediately.
        let _ = send.stopped().await;
        return Ok(());
    }
    let options = request.observe.context("observation options")?;
    if matches!(state.scenario, Scenario::Reset) {
        send_message(
            &mut send,
            &ThreadEvent {
                frame: Some(StreamFrame {
                    sequence: 1,
                    body: Some(stream_frame::Body::Reset(StreamReset {
                        reason: StreamResetReason::CursorExpired as i32,
                    })),
                }),
                payload: None,
            },
        )
        .await?;
        send.finish()?;
        return Ok(());
    }
    let mut updates = state.thread.subscribe();
    let mut sequence = 1;
    let open = ThreadEvent {
        frame: Some(StreamFrame {
            sequence,
            body: Some(stream_frame::Body::Open(StreamOpen {
                source: state.description.endpoint.clone(),
                binding_digest: vec![5; 32],
                resumed_from: options.after_cursor.clone(),
                accepted_budget: options.budget,
                authority_valid_until: None,
            })),
        }),
        payload: None,
    };
    send_message(&mut send, &open).await?;
    let mut previous = options.after_cursor;
    loop {
        let current = updates.borrow_and_update().clone();
        let repetitions = if matches!(state.scenario, Scenario::OversizedBatch) {
            33
        } else {
            1
        };
        for _ in 0..repetitions {
            sequence += 1;
            send_message(
                &mut send,
                &ThreadEvent {
                    frame: Some(StreamFrame {
                        sequence,
                        body: Some(stream_frame::Body::Data(StreamData {
                            kind: if previous.is_empty() {
                                StreamDataKind::Snapshot
                            } else {
                                StreamDataKind::Upsert
                            } as i32,
                        })),
                    }),
                    payload: Some(thread_event::Payload::Overview(current.clone())),
                },
            )
            .await?;
        }
        if matches!(state.scenario, Scenario::Interrupted) {
            send.finish()?;
            return Ok(());
        }
        sequence += 1;
        let cursor = current.version.clone();
        send_message(
            &mut send,
            &ThreadEvent {
                frame: Some(StreamFrame {
                    sequence,
                    body: Some(stream_frame::Body::Checkpoint(StreamCheckpoint {
                        cursor: cursor.clone(),
                        previous_cursor: previous.clone(),
                        snapshot_complete: previous.is_empty(),
                        page: None,
                    })),
                }),
                payload: None,
            },
        )
        .await?;
        previous = cursor;
        if options.mode == ObservationMode::Once as i32 {
            sequence += 1;
            send_message(
                &mut send,
                &ThreadEvent {
                    frame: Some(StreamFrame {
                        sequence,
                        body: Some(stream_frame::Body::Complete(StreamComplete {
                            cursor: previous,
                        })),
                    }),
                    payload: None,
                },
            )
            .await?;
            send.finish()?;
            return Ok(());
        }
        tokio::select! {
            change = updates.changed() => { change?; },
            _ = send.stopped() => return Ok(()),
        }
    }
}
