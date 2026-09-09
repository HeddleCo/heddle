// SPDX-License-Identifier: Apache-2.0
#[path = "../examples/support/mod.rs"]
mod support;

use api::v2::client::ClientError;
use heddle_thread_api::{content::BlobSource, contract::*, observation::Error, rpc, transport};
use support::{Peer, Scenario};

#[tokio::test]
async fn a_persisted_checkpoint_resumes_committed_changes_without_replacement() {
    let peer = Peer::start(Scenario::Normal).await.expect("peer");
    let remote = peer.connect().await.expect("discovery");
    let thread = remote.thread(support::thread_ref());
    let mut view = thread
        .observe(&[ThreadSection::Overview], ObservationMode::Follow, None)
        .await
        .expect("observe");
    let batch = view
        .next_commit()
        .await
        .expect("protocol")
        .expect("snapshot");
    let saved = batch.resume.encode();
    view.cancel();
    let thread_event::Payload::Overview(overview) = &batch.changes[0] else {
        panic!("overview");
    };
    thread
        .revise_intent(
            &peer
                .prepare_intent(overview, uuid::Uuid::from_u128(2), "after disconnect")
                .expect("prepare original edit"),
        )
        .await
        .expect("edit");
    let resumed =
        heddle_thread_api::observation::Resume::decode(&saved).expect("persisted checkpoint");
    let mut view = thread
        .observe(
            &[ThreadSection::Overview],
            ObservationMode::Follow,
            Some(resumed),
        )
        .await
        .expect("resume");
    let update = view
        .next_commit()
        .await
        .expect("resumed protocol")
        .expect("delta");
    assert!(!update.replace);
    let thread_event::Payload::Overview(overview) = &update.changes[0] else {
        panic!("updated overview");
    };
    assert_eq!(
        overview.intent.as_ref().expect("updated intent").outcome,
        "after disconnect"
    );
    assert_eq!(peer.routes().expect("routes").len(), 4);
    view.cancel();
    peer.close().await;
}

#[tokio::test]
async fn snapshot_edit_and_live_update_need_no_lookup_or_refetch() {
    let peer = Peer::start(Scenario::Normal).await.expect("loopback peer");
    let remote = peer.connect().await.expect("discovery");
    let thread = remote.thread(support::thread_ref());
    let mut view = thread
        .observe(&[ThreadSection::Overview], ObservationMode::Follow, None)
        .await
        .expect("observe");
    let batch = view
        .next_commit()
        .await
        .expect("snapshot protocol")
        .expect("snapshot");
    assert!(batch.replace);
    assert_eq!(peer.active_streams(), 1);
    let thread_event::Payload::Overview(overview) = &batch.changes[0] else {
        panic!("overview");
    };
    let receipt = thread
        .revise_intent(
            &peer
                .prepare_intent(overview, uuid::Uuid::from_u128(1), "new intent")
                .expect("prepare original edit"),
        )
        .await
        .expect("edit");
    assert_eq!(
        receipt.receipt.expect("receipt").client_operation_id,
        uuid::Uuid::from_u128(1).to_string()
    );
    let update = view
        .next_commit()
        .await
        .expect("update protocol")
        .expect("update");
    assert!(!update.replace);
    let thread_event::Payload::Overview(updated) = &update.changes[0] else {
        panic!("updated overview");
    };
    assert_eq!(
        updated.intent.as_ref().expect("updated intent").outcome,
        "new intent"
    );
    let routes = peer.routes().expect("routes");
    assert_eq!(routes.len(), 3, "discovery + observation + mutation");
    assert!(
        routes
            .iter()
            .all(|r| r.starts_with("/heddle.api.v2alpha1."))
    );
    view.cancel();
    peer.close().await;
}

#[tokio::test]
async fn paths_and_hashes_share_one_exact_revision_read() {
    let peer = Peer::start(Scenario::Normal).await.expect("peer");
    let remote = peer.connect().await.expect("discovery");
    let blobs = remote
        .read_blobs(
            support::revision(),
            vec![
                BlobSource::Path("README.md".into()),
                BlobSource::ObjectHash(vec![9; 32]),
            ],
        )
        .await
        .expect("blobs");
    assert_eq!(blobs.len(), 2);
    assert!(blobs.iter().all(|b| b.bytes == b"hello\n"));
    assert_eq!(peer.routes().expect("routes").len(), 2);
    peer.close().await;
}

#[tokio::test]
async fn fin_before_checkpoint_never_delivers_a_snapshot() {
    let peer = Peer::start(Scenario::Interrupted).await.expect("peer");
    let remote = peer.connect().await.expect("discovery");
    let mut view = remote
        .thread(support::thread_ref())
        .observe(&[ThreadSection::Overview], ObservationMode::Once, None)
        .await
        .expect("observe");
    assert!(matches!(view.next_commit().await, Err(Error::Interrupted)));
    assert!(view.next_commit().await.expect("terminal").is_none());
    peer.close().await;
}

#[tokio::test]
async fn blob_range_without_selection_completion_is_interrupted() {
    let peer = Peer::start(Scenario::Interrupted).await.expect("peer");
    let remote = peer.connect().await.expect("discovery");
    assert!(matches!(
        remote
            .read_blobs(
                support::revision(),
                vec![BlobSource::Path("README.md".into())]
            )
            .await,
        Err(Error::Interrupted)
    ));
    peer.close().await;
}

#[tokio::test]
async fn a_blob_from_another_revision_is_rejected() {
    let peer = Peer::start(Scenario::WrongRevision).await.expect("peer");
    let remote = peer.connect().await.expect("discovery");
    assert!(matches!(
        remote
            .read_blobs(
                support::revision(),
                vec![BlobSource::Path("README.md".into())]
            )
            .await,
        Err(Error::Invalid("content revision mismatch"))
    ));
    peer.close().await;
}

#[tokio::test]
async fn oversized_header_is_rejected_without_waiting_for_body() {
    let peer = Peer::start(Scenario::OversizedHeader).await.expect("peer");
    let remote = peer.connect().await.expect("discovery");
    let mut view = remote
        .thread(support::thread_ref())
        .observe(&[ThreadSection::Overview], ObservationMode::Once, None)
        .await
        .expect("observe");
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), view.next_commit())
        .await
        .expect("must reject header immediately");
    assert!(matches!(
        result,
        Err(Error::Client(ClientError::Transport(
            transport::Error::Protocol("response exceeds frame budget")
        )))
    ));
    peer.close().await;
}

#[tokio::test]
async fn uncommitted_batches_are_bounded_independently_of_frames() {
    let peer = Peer::start(Scenario::OversizedBatch).await.expect("peer");
    let remote = peer.connect().await.expect("discovery");
    let mut view = remote
        .thread(support::thread_ref())
        .observe(&[ThreadSection::Overview], ObservationMode::Once, None)
        .await
        .expect("observe");
    assert!(matches!(
        view.next_commit().await,
        Err(Error::Invalid("uncommitted batch budget exceeded"))
    ));
    peer.close().await;
}

#[tokio::test]
async fn reset_before_open_is_a_typed_terminal_outcome() {
    let peer = Peer::start(Scenario::Reset).await.expect("peer");
    let remote = peer.connect().await.expect("discovery");
    let mut view = remote
        .thread(support::thread_ref())
        .observe(&[ThreadSection::Overview], ObservationMode::Once, None)
        .await
        .expect("observe");
    assert!(
        matches!(view.next_commit().await, Err(Error::Reset(reason)) if reason == StreamResetReason::CursorExpired as i32)
    );
    peer.close().await;
}

#[tokio::test]
async fn a_cursor_cannot_cross_thread_projections_and_unknown_handlers_stay_local() {
    let peer = Peer::start(Scenario::Normal).await.expect("peer");
    let remote = peer.connect().await.expect("discovery");
    let thread = remote.thread(support::thread_ref());
    let mut view = thread
        .observe(&[ThreadSection::Overview], ObservationMode::Once, None)
        .await
        .expect("observe");
    let batch = view
        .next_commit()
        .await
        .expect("protocol")
        .expect("snapshot");
    assert!(view.next_commit().await.expect("Complete").is_none());
    assert!(matches!(
        thread
            .observe(
                &[ThreadSection::Review],
                ObservationMode::Once,
                Some(batch.resume)
            )
            .await,
        Err(Error::Invalid(
            "resume belongs to a different source or projection"
        ))
    ));
    assert!(matches!(
        remote
            .api
            .call::<rpc::ThreadServiceStartThread>(&StartThreadRequest::default())
            .await,
        Err(ClientError::NotImplemented(_))
    ));
    assert_eq!(
        peer.routes().expect("routes").len(),
        2,
        "rejected calls must not open RPCs"
    );
    peer.close().await;
}
