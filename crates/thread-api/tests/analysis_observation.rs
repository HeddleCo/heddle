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
struct Transport(Vec<AnalysisEvent>);
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
            "/heddle.api.v2alpha1.AnalysisService/ObserveAnalysis"
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
fn remote(events: Vec<AnalysisEvent>) -> Remote<Transport> {
    Remote {
        api: Client::new(
            Transport(events),
            ["/heddle.api.v2alpha1.AnalysisService/ObserveAnalysis".into()],
        ),
        description: DescribeEndpointResponse {
            endpoint: Some(endpoint()),
            default_read_budget: Some(budget()),
            max_pending_batch_bytes: 1024 * 1024,
            ..Default::default()
        },
    }
}
fn control(sequence: u64, body: stream_frame::Body) -> AnalysisEvent {
    AnalysisEvent {
        frame: Some(StreamFrame {
            sequence,
            body: Some(body),
        }),
        payload: None,
    }
}
fn data(sequence: u64, kind: StreamDataKind, payload: analysis_event::Payload) -> AnalysisEvent {
    AnalysisEvent {
        frame: Some(StreamFrame {
            sequence,
            body: Some(stream_frame::Body::Data(StreamData { kind: kind as i32 })),
        }),
        payload: Some(payload),
    }
}
fn open() -> AnalysisEvent {
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
fn change(sequence: u64, id: &str, kind: StreamDataKind) -> AnalysisEvent {
    data(
        sequence,
        kind,
        analysis_event::Payload::BehaviorChange(BehaviorChange {
            id: id.into(),
            ..Default::default()
        }),
    )
}
fn checkpoint(sequence: u64, cursor: u8, previous: Vec<u8>) -> AnalysisEvent {
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
async fn analysis_commits_typed_replacement_and_removal_with_the_shared_state_machine() {
    let remote = remote(vec![
        open(),
        change(2, "old", StreamDataKind::Snapshot),
        checkpoint(3, 1, vec![]),
        data(
            4,
            StreamDataKind::Upsert,
            analysis_event::Payload::ReplaceSection(SectionReplacement {
                section: "behavior_changes".into(),
            }),
        ),
        change(5, "new", StreamDataKind::Upsert),
        data(
            6,
            StreamDataKind::Remove,
            analysis_event::Payload::BehaviorRemoval(BehaviorChangeRemoval {
                change_id: "old".into(),
                analysis: None,
            }),
        ),
        checkpoint(7, 2, vec![1]),
        control(
            8,
            stream_frame::Body::Complete(StreamComplete { cursor: vec![2] }),
        ),
    ]);
    let mut view = remote
        .observe_analysis(ObserveAnalysisRequest::default(), None)
        .await
        .expect("observe");
    let first = view
        .next_commit()
        .await
        .expect("snapshot protocol")
        .expect("snapshot");
    assert!(first.replace);
    assert_eq!(first.changes.len(), 1);
    let second = view
        .next_commit()
        .await
        .expect("delta protocol")
        .expect("delta");
    assert!(!second.replace);
    assert_eq!(second.changes.len(), 3);
    assert!(
        matches!(&second.changes[2], analysis_event::Payload::BehaviorRemoval(r) if r.change_id == "old")
    );
    assert!(second.page.expect("page").exhausted);
    assert!(view.next_commit().await.expect("Complete").is_none());
}

#[tokio::test]
async fn incomplete_analysis_never_becomes_an_empty_complete_result() {
    for events in [
        vec![open(), change(2, "pending", StreamDataKind::Snapshot)],
        vec![open()],
    ] {
        let remote = remote(events);
        let mut view = remote
            .observe_analysis(ObserveAnalysisRequest::default(), None)
            .await
            .expect("observe");
        assert!(matches!(view.next_commit().await, Err(Error::Interrupted)));
    }
}

#[tokio::test]
async fn behavior_removal_requires_a_remove_frame() {
    let remote = remote(vec![
        open(),
        change(2, "old", StreamDataKind::Snapshot),
        checkpoint(3, 1, vec![]),
        data(
            4,
            StreamDataKind::Upsert,
            analysis_event::Payload::BehaviorRemoval(BehaviorChangeRemoval {
                change_id: "old".into(),
                analysis: None,
            }),
        ),
    ]);
    let mut view = remote
        .observe_analysis(ObserveAnalysisRequest::default(), None)
        .await
        .expect("observe");
    assert!(view.next_commit().await.expect("snapshot").is_some());
    assert!(matches!(
        view.next_commit().await,
        Err(Error::Invalid("removal payload/kind mismatch"))
    ));
}
