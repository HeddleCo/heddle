//! Injected weft v2 authority-change signals must reopen a fresh exact
//! selection. A bare resend of the same stale stream would stay Aborted.
use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use api::{
    heddle::api::common::{CallFailure, CallFailureCode},
    v2::{
        MethodDescriptor,
        client::{Client, MessageReader, MessageWriter, RpcTransport},
    },
};
use heddle_thread_api::{
    Remote, Rpc, content::BlobSource, contract::*, is_reopen_retryable, observation::Error, rpc,
    transport,
};
use prost::Message;

struct Reader {
    frames: VecDeque<Result<Option<Vec<u8>>, transport::Error>>,
}

impl MessageReader for Reader {
    type Error = transport::Error;
    async fn next(&mut self) -> Result<Option<Vec<u8>>, Self::Error> {
        self.frames.pop_front().unwrap_or(Ok(None))
    }
    fn cancel(&mut self) {
        self.frames.clear();
    }
}

struct Writer;
impl MessageWriter for Writer {
    type Error = transport::Error;
    async fn send(&mut self, _: Vec<u8>) -> Result<(), Self::Error> {
        Err(transport::Error::Protocol("read-only fixture"))
    }
    async fn finish(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
    fn abort(&mut self) {}
}

struct Transport {
    observe_calls: Arc<AtomicUsize>,
    first: CallFailure,
    success: Vec<Vec<u8>>,
    method: &'static str,
}

impl Transport {
    fn new(
        observe_calls: Arc<AtomicUsize>,
        first: CallFailure,
        success: Vec<Vec<u8>>,
        method: &'static str,
    ) -> Self {
        Self {
            observe_calls,
            first,
            success,
            method,
        }
    }
}

impl RpcTransport for Transport {
    type Error = transport::Error;
    type Reader = Reader;
    type Writer = Writer;
    async fn unary(
        &self,
        _: &'static MethodDescriptor,
        _: Vec<u8>,
    ) -> Result<Vec<u8>, Self::Error> {
        Err(transport::Error::Protocol("observation only"))
    }
    async fn observe(
        &self,
        method: &'static MethodDescriptor,
        _: Vec<u8>,
    ) -> Result<Self::Reader, Self::Error> {
        assert_eq!(method.path, self.method);
        let attempt = self.observe_calls.fetch_add(1, Ordering::SeqCst);
        if attempt == 0 {
            return Ok(Reader {
                frames: VecDeque::from([Err(transport::Error::Remote(self.first.clone().into()))]),
            });
        }
        Ok(Reader {
            frames: self
                .success
                .iter()
                .cloned()
                .map(|frame| Ok(Some(frame)))
                .chain(std::iter::once(Ok(None)))
                .collect(),
        })
    }
    async fn exchange(
        &self,
        _: &'static MethodDescriptor,
        _: Vec<u8>,
    ) -> Result<(Self::Writer, Self::Reader), Self::Error> {
        Err(transport::Error::Protocol("observation only"))
    }
}

fn endpoint() -> EndpointRef {
    EndpointRef {
        public_key: vec![7; 32],
        kind: EndpointKind::Weft as i32,
    }
}

fn budget() -> ReadBudget {
    ReadBudget {
        max_items: 16,
        max_frame_bytes: 64 * 1024,
        max_snapshot_bytes: 1024 * 1024,
    }
}

fn remote(transport: Transport, method: &str) -> Remote<Transport> {
    Remote {
        api: Client::new(transport, [method.to_string()]),
        description: DescribeEndpointResponse {
            endpoint: Some(endpoint()),
            default_read_budget: Some(budget()),
            max_pending_batch_bytes: 1024 * 1024,
            implemented_methods: vec![method.to_string()],
            ..Default::default()
        },
    }
}

fn aborted(message: &str) -> CallFailure {
    CallFailure {
        code: CallFailureCode::Aborted as i32,
        message: message.into(),
        ..Default::default()
    }
}

fn thread() -> ThreadRef {
    ThreadRef {
        spool: Some(SpoolRef { id: "spool".into() }),
        id: Some(ThreadId { value: vec![1; 32] }),
    }
}

fn revision() -> RevisionRef {
    RevisionRef {
        spool: Some(SpoolRef { id: "spool".into() }),
        revision: Some(revision_ref::Revision::State(
            api::heddle::api::common::StateId { value: vec![2; 32] },
        )),
    }
}

fn blob_frames() -> Vec<Vec<u8>> {
    let hash = vec![9; 32];
    vec![
        ContentEvent {
            selection_id: "0".into(),
            revision: Some(revision()),
            payload: Some(content_event::Payload::Blob(BlobChunk {
                data: b"hello\n".to_vec(),
                total_size: 6,
                object_hash: hash.clone(),
                range_complete: true,
                offset: 0,
            })),
        }
        .encode_to_vec(),
        ContentEvent {
            selection_id: "0".into(),
            revision: Some(revision()),
            payload: Some(content_event::Payload::SelectionComplete(SectionStatus {
                section: "blob".into(),
                coverage: Coverage::Complete as i32,
                computed_for: Some(revision()),
                ..Default::default()
            })),
        }
        .encode_to_vec(),
    ]
}

fn budget_echo(accepted: ReadBudget) -> Vec<u8> {
    ContentEvent {
        payload: Some(content_event::Payload::AcceptedBudget(accepted)),
        ..Default::default()
    }
    .encode_to_vec()
}

async fn read_content_frames(
    frames: Vec<Vec<u8>>,
    requested: ReadBudget,
) -> Result<Vec<heddle_thread_api::content::Blob>, Error> {
    let method = rpc::ContentServiceReadContent::METHOD.path;
    // Start after the injected retryable failure: these cases exercise one stream.
    let calls = Arc::new(AtomicUsize::new(1));
    let mut remote = remote(
        Transport::new(calls.clone(), aborted("unused"), frames, method),
        method,
    );
    remote.description.default_read_budget = Some(requested);
    let result = remote
        .read_blobs(
            thread(),
            revision(),
            vec![BlobSource::ObjectHash(vec![9; 32])],
        )
        .await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "protocol failures must not retry"
    );
    result
}

#[tokio::test]
async fn content_accepts_initial_budget_echo_before_domain_events() {
    for revision in [None, Some(revision())] {
        let echo = ContentEvent {
            revision,
            payload: Some(content_event::Payload::AcceptedBudget(budget())),
            ..Default::default()
        };
        let mut frames = vec![echo.encode_to_vec()];
        frames.extend(blob_frames());
        let blobs = read_content_frames(frames, budget())
            .await
            .expect("alpha.22 content stream");
        assert_eq!(blobs[0].bytes, b"hello\n");
        assert_eq!(blobs[0].object_hash, vec![9; 32]);
    }
}

#[tokio::test]
async fn content_rejects_missing_budget_echo() {
    for frames in [vec![], blob_frames()] {
        assert!(matches!(
            read_content_frames(frames, budget()).await,
            Err(Error::Invalid(_))
        ));
    }
}

#[tokio::test]
async fn content_rejects_duplicate_budget_echo() {
    for position in [1, 2, 3] {
        let mut frames = vec![budget_echo(budget())];
        frames.extend(blob_frames());
        frames.insert(position, budget_echo(budget()));
        assert!(matches!(
            read_content_frames(frames, budget()).await,
            Err(Error::Invalid(_))
        ));
    }
}

#[tokio::test]
async fn content_rejects_budget_echo_after_domain_event() {
    let mut frames = blob_frames();
    frames.insert(1, budget_echo(budget()));
    assert!(matches!(
        read_content_frames(frames, budget()).await,
        Err(Error::Invalid(_))
    ));
}

#[tokio::test]
async fn content_rejects_widened_zero_or_below_floor_budget_echo() {
    for accepted in [
        ReadBudget {
            max_items: 17,
            ..budget()
        },
        ReadBudget {
            max_frame_bytes: budget().max_frame_bytes + 1,
            ..budget()
        },
        ReadBudget {
            max_snapshot_bytes: budget().max_snapshot_bytes + 1,
            ..budget()
        },
        ReadBudget {
            max_items: 0,
            ..budget()
        },
        ReadBudget {
            max_frame_bytes: 0,
            ..budget()
        },
        ReadBudget {
            max_snapshot_bytes: 0,
            ..budget()
        },
        ReadBudget {
            max_items: 15,
            ..budget()
        },
        ReadBudget {
            max_frame_bytes: budget().max_frame_bytes - 1,
            ..budget()
        },
        ReadBudget {
            max_snapshot_bytes: budget().max_snapshot_bytes - 1,
            ..budget()
        },
    ] {
        let mut frames = vec![budget_echo(accepted)];
        frames.extend(blob_frames());
        assert!(
            matches!(
                read_content_frames(frames, budget()).await,
                Err(Error::Invalid(_))
            ),
            "accepted {accepted:?}"
        );
    }
}

#[tokio::test]
async fn content_rejects_selection_scoped_budget_echo_and_unknown_payload() {
    for frame in [
        ContentEvent {
            selection_id: "0".into(),
            payload: Some(content_event::Payload::AcceptedBudget(budget())),
            ..Default::default()
        }
        .encode_to_vec(),
        ContentEvent {
            revision: Some(RevisionRef {
                spool: None,
                ..revision()
            }),
            payload: Some(content_event::Payload::AcceptedBudget(budget())),
            ..Default::default()
        }
        .encode_to_vec(),
    ] {
        let mut frames = vec![frame];
        frames.extend(blob_frames());
        assert!(matches!(
            read_content_frames(frames, budget()).await,
            Err(Error::Invalid(_))
        ));
    }
    let mut frames = vec![
        budget_echo(budget()),
        ContentEvent {
            selection_id: "0".into(),
            revision: Some(revision()),
            payload: None,
        }
        .encode_to_vec(),
    ];
    frames.extend(blob_frames());
    assert!(matches!(
        read_content_frames(frames, budget()).await,
        Err(Error::Invalid(_))
    ));
}

#[tokio::test]
async fn content_charges_echo_and_completion_against_item_budget() {
    for max_items in [1, 2, 3] {
        let requested = ReadBudget {
            max_items,
            ..budget()
        };
        let mut frames = vec![budget_echo(requested)];
        frames.extend(blob_frames());
        assert_eq!(
            read_content_frames(frames, requested).await.is_ok(),
            max_items == 3
        );
    }
}

#[tokio::test]
async fn content_counts_transport_framing_in_frame_and_snapshot_budgets() {
    let mut domain = blob_frames();
    let mut chunk = ContentEvent::decode(domain[0].as_slice()).expect("chunk");
    let Some(content_event::Payload::Blob(ref mut blob)) = chunk.payload else {
        panic!("blob")
    };
    blob.data = vec![42; 1800];
    blob.total_size = 1800;
    domain[0] = chunk.encode_to_vec();
    let frame_bytes = domain[0].len() as u32 + 5;
    for max_frame_bytes in [frame_bytes - 1, frame_bytes] {
        let requested = ReadBudget {
            max_frame_bytes,
            ..budget()
        };
        let mut frames = vec![budget_echo(requested)];
        frames.extend(domain.clone());
        assert_eq!(
            read_content_frames(frames, requested).await.is_ok(),
            max_frame_bytes == frame_bytes
        );
    }
    // The snapshot fits only if both the echo and every 5-byte stream header are charged.
    let mut requested = ReadBudget {
        max_frame_bytes: frame_bytes,
        max_snapshot_bytes: 4096,
        ..budget()
    };
    let total = budget_echo(requested).len() + domain.iter().map(Vec::len).sum::<usize>() + 15;
    for max_snapshot_bytes in [total as u64 - 1, total as u64] {
        requested.max_snapshot_bytes = max_snapshot_bytes;
        let mut frames = vec![budget_echo(requested)];
        frames.extend(domain.clone());
        assert_eq!(
            read_content_frames(frames, requested).await.is_ok(),
            max_snapshot_bytes == total as u64
        );
    }
}

fn identity_frames() -> Vec<Vec<u8>> {
    let open = IdentityEvent {
        frame: Some(StreamFrame {
            sequence: 1,
            body: Some(stream_frame::Body::Open(StreamOpen {
                source: Some(endpoint()),
                binding_digest: vec![9; 32],
                accepted_budget: Some(budget()),
                ..Default::default()
            })),
        }),
        payload: None,
    };
    let change = IdentityEvent {
        frame: Some(StreamFrame {
            sequence: 2,
            body: Some(stream_frame::Body::Data(StreamData {
                kind: StreamDataKind::Snapshot as i32,
            })),
        }),
        payload: Some(identity_event::Payload::Identity(PrincipalRecord {
            id: "human".into(),
            ..Default::default()
        })),
    };
    let checkpoint = IdentityEvent {
        frame: Some(StreamFrame {
            sequence: 3,
            body: Some(stream_frame::Body::Checkpoint(StreamCheckpoint {
                cursor: vec![1],
                snapshot_complete: true,
                previous_cursor: vec![],
                page: Some(PageInfo {
                    exhausted: true,
                    ..Default::default()
                }),
            })),
        }),
        payload: None,
    };
    vec![
        open.encode_to_vec(),
        change.encode_to_vec(),
        checkpoint.encode_to_vec(),
    ]
}

#[test]
fn classifier_is_the_shared_retry_gate() {
    assert!(is_reopen_retryable(&aborted(
        "material authority changed; reopen exact selection"
    )));
    assert!(!is_reopen_retryable(&CallFailure {
        code: CallFailureCode::PermissionDenied as i32,
        message: "material authority changed; reopen exact selection".into(),
        ..Default::default()
    }));
}

#[tokio::test]
async fn authority_change_reopens_a_fresh_content_selection_and_succeeds() {
    let method = rpc::ContentServiceReadContent::METHOD.path;
    let observe_calls = Arc::new(AtomicUsize::new(0));
    let transport = Transport::new(
        Arc::clone(&observe_calls),
        aborted("material authority changed; reopen exact selection"),
        std::iter::once(budget_echo(budget()))
            .chain(blob_frames())
            .collect(),
        method,
    );
    let remote = remote(transport, method);
    let blobs = remote
        .read_blobs(
            thread(),
            revision(),
            vec![BlobSource::ObjectHash(vec![9; 32])],
        )
        .await
        .expect("reopen must swallow the retryable signal");
    assert_eq!(blobs[0].bytes, b"hello\n");
    assert_eq!(
        observe_calls.load(Ordering::SeqCst),
        2,
        "must reopen a fresh exact selection, not resend on the aborted stream"
    );
}

#[tokio::test]
async fn authority_change_reopens_a_fresh_observation_and_succeeds() {
    let method = rpc::IdentityServiceObserveIdentity::METHOD.path;
    let observe_calls = Arc::new(AtomicUsize::new(0));
    let transport = Transport::new(
        Arc::clone(&observe_calls),
        aborted("material authority changed; reopen exact selection"),
        identity_frames(),
        method,
    );
    let remote = remote(transport, method);
    let mut view = remote
        .observe::<rpc::IdentityServiceObserveIdentity>(ObserveIdentityRequest::default(), None)
        .await
        .expect("reopen must swallow the retryable Open failure");
    let batch = view
        .next_commit()
        .await
        .expect("checkpoint")
        .expect("snapshot");
    assert!(matches!(
        &batch.changes[0],
        identity_event::Payload::Identity(p) if p.id == "human"
    ));
    assert_eq!(
        observe_calls.load(Ordering::SeqCst),
        2,
        "must reopen a fresh exact selection, not resend on the aborted stream"
    );
}

#[tokio::test]
async fn non_transient_content_failure_is_not_retried() {
    let method = rpc::ContentServiceReadContent::METHOD.path;
    let observe_calls = Arc::new(AtomicUsize::new(0));
    let transport = Transport::new(
        Arc::clone(&observe_calls),
        CallFailure {
            code: CallFailureCode::PermissionDenied as i32,
            message: "hidden source".into(),
            ..Default::default()
        },
        std::iter::once(budget_echo(budget()))
            .chain(blob_frames())
            .collect(),
        method,
    );
    let remote = remote(transport, method);
    let error = match remote
        .read_blobs(
            thread(),
            revision(),
            vec![BlobSource::ObjectHash(vec![9; 32])],
        )
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("non-transient failure must surface"),
    };
    assert!(matches!(
        error,
        Error::Client(api::v2::client::ClientError::Transport(
            transport::Error::Remote(ref failure)
        )) if failure.code == CallFailureCode::PermissionDenied as i32
            && failure.message == "hidden source"
    ));
    assert_eq!(
        observe_calls.load(Ordering::SeqCst),
        1,
        "non-transient failure must not reopen"
    );
}

#[tokio::test]
async fn non_transient_observation_failure_is_not_retried() {
    let method = rpc::IdentityServiceObserveIdentity::METHOD.path;
    let observe_calls = Arc::new(AtomicUsize::new(0));
    let transport = Transport::new(
        Arc::clone(&observe_calls),
        CallFailure {
            code: CallFailureCode::NotFound as i32,
            message: "unknown principal".into(),
            ..Default::default()
        },
        identity_frames(),
        method,
    );
    let remote = remote(transport, method);
    let mut view = remote
        .observe::<rpc::IdentityServiceObserveIdentity>(ObserveIdentityRequest::default(), None)
        .await
        .expect("non-retryable opening is returned as a primed observation");
    let error = match view.next_commit().await {
        Err(error) => error,
        Ok(_) => panic!("non-transient failure must surface"),
    };
    assert!(matches!(
        error,
        Error::Client(api::v2::client::ClientError::Transport(
            transport::Error::Remote(ref failure)
        )) if failure.code == CallFailureCode::NotFound as i32
    ));
    assert_eq!(observe_calls.load(Ordering::SeqCst), 1);
}
