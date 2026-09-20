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
        blob_frames(),
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
        blob_frames(),
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
