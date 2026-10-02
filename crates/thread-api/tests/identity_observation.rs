#![cfg(feature = "iroh")]
use std::collections::VecDeque;

use api::v2::{
    MethodDescriptor,
    client::{Client, MessageReader, MessageWriter, RpcTransport},
};
use heddle_thread_api::{Remote, contract::*, observation::Error, transport};
use prost::Message;

struct Reader(VecDeque<Vec<u8>>);
impl MessageReader for Reader {
    type Error = transport::Error;
    async fn next(&mut self) -> Result<Option<Vec<u8>>, Self::Error> {
        Ok(self.0.pop_front())
    }
    fn cancel(&mut self) {
        self.0.clear();
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
struct Transport(Vec<IdentityEvent>);
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
        assert_eq!(
            method.path,
            "/heddle.api.v1alpha2.IdentityService/ObserveIdentity"
        );
        Ok(Reader(self.0.iter().map(Message::encode_to_vec).collect()))
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
fn remote(events: Vec<IdentityEvent>) -> Remote<Transport> {
    Remote {
        api: Client::new(
            Transport(events),
            ["/heddle.api.v1alpha2.IdentityService/ObserveIdentity".into()],
        ),
        description: DescribeEndpointResponse {
            endpoint: Some(endpoint()),
            default_read_budget: Some(budget()),
            max_pending_batch_bytes: 1024 * 1024,
            ..Default::default()
        },
    }
}
fn control(sequence: u64, body: stream_frame::Body) -> IdentityEvent {
    IdentityEvent {
        frame: Some(StreamFrame {
            sequence,
            body: Some(body),
        }),
        payload: None,
    }
}
fn data(sequence: u64, kind: StreamDataKind, payload: identity_event::Payload) -> IdentityEvent {
    IdentityEvent {
        frame: Some(StreamFrame {
            sequence,
            body: Some(stream_frame::Body::Data(StreamData { kind: kind as i32 })),
        }),
        payload: Some(payload),
    }
}
fn open() -> IdentityEvent {
    control(
        1,
        stream_frame::Body::Open(StreamOpen {
            source: Some(endpoint()),
            binding_digest: vec![9; 32],
            accepted_budget: Some(budget()),
            ..Default::default()
        }),
    )
}
fn change(sequence: u64, id: &str, kind: StreamDataKind) -> IdentityEvent {
    data(
        sequence,
        kind,
        identity_event::Payload::Identity(PrincipalRecord {
            id: id.into(),
            ..Default::default()
        }),
    )
}
fn checkpoint(sequence: u64, cursor: u8, previous: Vec<u8>) -> IdentityEvent {
    control(
        sequence,
        stream_frame::Body::Checkpoint(StreamCheckpoint {
            cursor: vec![cursor],
            snapshot_complete: previous.is_empty(),
            previous_cursor: previous,
            page: Some(PageInfo {
                exhausted: true,
                ..Default::default()
            }),
        }),
    )
}

#[tokio::test]
async fn identity_observation_delivers_only_committed_identity_and_rejects_cross_method_resume() {
    let remote = remote(vec![
        open(),
        change(2, "human", StreamDataKind::Snapshot),
        checkpoint(3, 1, vec![]),
    ]);
    let mut view = remote
        .observe::<heddle_thread_api::rpc::IdentityServiceObserveIdentity>(
            ObserveIdentityRequest::default(),
            None,
        )
        .await
        .expect("identity observation");
    let batch = view
        .next_commit()
        .await
        .expect("checkpoint")
        .expect("snapshot");
    assert!(batch.replace);
    assert!(matches!(&batch.changes[0], identity_event::Payload::Identity(p) if p.id == "human"));
    assert!(matches!(view.next_commit().await, Err(Error::Interrupted)));
    let result = remote
        .observe::<heddle_thread_api::rpc::RunServiceObserveRuns>(
            ObserveRunsRequest::default(),
            Some(batch.resume),
        )
        .await;
    assert!(matches!(
        result,
        Err(Error::Invalid(
            "resume belongs to a different source or projection"
        ))
    ));
}

#[tokio::test]
async fn stream_open_declaring_hybrid_support_is_rejected_not_ignored() {
    let hybrid_opens = [
        StreamOpen {
            protocol: Some(api::heddle::api::common::ProtocolCompatibility::default()),
            ..Default::default()
        },
        StreamOpen {
            witness_set: Some(api::heddle::api::common::SignedHostedWitnessSetV1::default()),
            ..Default::default()
        },
    ];
    for hybrid in hybrid_opens {
        let stream_frame::Body::Open(base) = open().frame.expect("frame").body.expect("body")
        else {
            panic!("open fixture")
        };
        let opening = control(
            1,
            stream_frame::Body::Open(StreamOpen {
                protocol: hybrid.protocol,
                witness_set: hybrid.witness_set,
                ..base
            }),
        );
        let remote = remote(vec![
            opening,
            change(2, "human", StreamDataKind::Snapshot),
            checkpoint(3, 1, vec![]),
        ]);
        let result = match remote
            .observe::<heddle_thread_api::rpc::IdentityServiceObserveIdentity>(
                ObserveIdentityRequest::default(),
                None,
            )
            .await
        {
            Ok(mut view) => view.next_commit().await.map(|_| ()),
            Err(error) => Err(error),
        };
        assert!(
            matches!(&result, Err(Error::Invalid(message)) if message.contains("api#307")),
            "HYBRID stream opening must be refused: {result:?}"
        );
    }
}
