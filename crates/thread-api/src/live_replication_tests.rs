use std::sync::atomic::{AtomicUsize, Ordering};

use crypto::{Ed25519Signer, Signer, thread_operation::SignedGenesis};
use heddle_object_model::object::thread_replication::{ThreadFacet, ThreadGenesis};
use repo::Repository;
use tokio::sync::Notify;

use super::*;
use crate::replication::native::LocalReplica;

struct Count(Arc<AtomicUsize>);
impl Count {
    fn acquire(count: &Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self(count.clone())
    }
}
impl Drop for Count {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
struct Work {
    _active: Count,
    buffer: Option<Count>,
}
impl ActivityGuard for Work {
    type Retained = Option<Count>;
    fn finish(self, _: usize) -> std::result::Result<Self::Retained, transport::Error> {
        Ok(self.buffer)
    }
}
struct QuietReader;
impl MessageReader for QuietReader {
    type Error = transport::Error;
    async fn next(&mut self) -> std::result::Result<Option<Vec<u8>>, Self::Error> {
        std::future::pending().await
    }
    fn cancel(&mut self) {}
}
struct StalledWriter(Arc<Notify>);
impl MessageWriter for StalledWriter {
    type Error = transport::Error;
    async fn send(&mut self, _: Vec<u8>) -> std::result::Result<(), Self::Error> {
        self.0.notify_one();
        std::future::pending().await
    }
    async fn finish(&mut self) -> std::result::Result<(), Self::Error> {
        Ok(())
    }
    fn abort(&mut self) {}
}

#[tokio::test]
async fn stalled_delivery_retains_memory_releases_work_and_refunds_on_cancellation() {
    for _ in 0..2 {
        let directory = tempfile::TempDir::new().expect("local replica");
        let repo = Repository::init_default(directory.path()).expect("repository");
        let signer = Ed25519Signer::from_seed(&[17; 32]).expect("creator");
        let genesis = ThreadGenesis {
            version: 1,
            spool: "01980000-0000-7000-8000-000000000001".into(),
            parent: None,
            base: repo.head().expect("HEAD").expect("initial state"),
            name: "stream".into(),
            intent: "separate work from delivery".into(),
            creator: signer.public_key().try_into().expect("key"),
            owner: heddle_object_model::object::thread_replication::GenesisOwner::LocalKey(
                signer.public_key().try_into().expect("key"),
            ),
            nonce: vec![91],
        };
        let replica = ThreadReplica::create(
            repo.heddle_dir(),
            &SignedGenesis::sign(&genesis, &signer).expect("proof"),
        )
        .expect("replica");
        seed_frontiers(&replica, &repo, &signer, &genesis, 1);
        let feed = Feed::new(replica.clone()).await.expect("shared feed");
        let session = Session::new(
            LocalReplica::new(replica, Arc::new(repo.store().clone())),
            [7; 32],
            [ThreadFacet::Source].into(),
            1,
        )
        .expect("session");
        let active = Arc::new(AtomicUsize::new(0));
        let buffers = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(Notify::new());
        let work_active = active.clone();
        let work_buffers = buffers.clone();
        let writer = StalledWriter(started.clone());
        let task = tokio::spawn(async move {
            run(
                session,
                QuietReader,
                writer,
                Side::Acceptor,
                &feed,
                move |activity| {
                    std::future::ready(Ok(Work {
                        _active: Count::acquire(&work_active),
                        buffer: (matches!(activity, Activity::Work | Activity::ReceiptWork))
                            .then(|| Count::acquire(&work_buffers)),
                    }))
                },
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(2), started.notified())
            .await
            .expect("writer receives an announcement");
        tokio::time::timeout(Duration::from_millis(300), async {
            while active.load(Ordering::SeqCst) != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("delivery cannot retain a work slot");
        assert_eq!(
            buffers.load(Ordering::SeqCst),
            1,
            "stalled output must retain its memory allowance"
        );
        task.abort();
        assert!(task.await.expect_err("task cancelled").is_cancelled());
        tokio::time::timeout(Duration::from_millis(300), async {
            while active.load(Ordering::SeqCst) != 0 || buffers.load(Ordering::SeqCst) != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("all work and memory allowances refunded");
    }
}

struct InputReader {
    frame: Option<Vec<u8>>,
    input_memory: Option<tokio::sync::OwnedSemaphorePermit>,
}
impl MessageReader for InputReader {
    type Error = transport::Error;
    async fn next(&mut self) -> std::result::Result<Option<Vec<u8>>, Self::Error> {
        if let Some(frame) = self.frame.take() {
            return Ok(Some(frame));
        }
        self.input_memory.take();
        std::future::pending().await
    }
    fn cancel(&mut self) {
        self.input_memory.take();
    }
}
struct MemoryGuard(Option<tokio::sync::OwnedSemaphorePermit>);
impl ActivityGuard for MemoryGuard {
    type Retained = Option<tokio::sync::OwnedSemaphorePermit>;
    fn finish(self, _: usize) -> std::result::Result<Self::Retained, transport::Error> {
        Ok(self.0)
    }
}

#[tokio::test]
async fn admitted_input_can_finish_while_the_output_memory_pool_is_full() {
    let directory = tempfile::TempDir::new().expect("local replica");
    let repo = Repository::init_default(directory.path()).expect("repository");
    let signer = Ed25519Signer::from_seed(&[17; 32]).expect("creator");
    let genesis = ThreadGenesis {
        version: 1,
        spool: "01980000-0000-7000-8000-000000000001".into(),
        parent: None,
        base: repo.head().expect("HEAD").expect("initial state"),
        name: "stream".into(),
        intent: "separate work from delivery".into(),
        creator: signer.public_key().try_into().expect("key"),
        owner: heddle_object_model::object::thread_replication::GenesisOwner::LocalKey(
            signer.public_key().try_into().expect("key"),
        ),
        nonce: vec![91],
    };
    let replica = ThreadReplica::create(
        repo.heddle_dir(),
        &SignedGenesis::sign(&genesis, &signer).expect("proof"),
    )
    .expect("replica");
    seed_frontiers(&replica, &repo, &signer, &genesis, 1);
    let feed = Feed::new(replica.clone()).await.expect("shared feed");
    let session = Session::new(
        LocalReplica::new(replica, Arc::new(repo.store().clone())),
        [7; 32],
        [ThreadFacet::Source].into(),
        1,
    )
    .expect("session");

    let memory = Arc::new(tokio::sync::Semaphore::new(1));
    let reader = InputReader {
        frame: Some(
            ReplicateThreadRequest {
                body: Some(replicate_thread_request::Body::Have(
                    ReplicationHave::default(),
                )),
            }
            .encode_to_vec(),
        ),
        input_memory: Some(
            memory
                .clone()
                .acquire_owned()
                .await
                .expect("admitted input memory"),
        ),
    };
    let sent = Arc::new(Notify::new());
    let writer = StalledWriter(sent.clone());
    let activity_memory = memory.clone();
    let task = tokio::spawn(async move {
        run(
            session,
            reader,
            writer,
            Side::Acceptor,
            &feed,
            move |activity| {
                let memory = activity_memory.clone();
                async move {
                    let reservation = if matches!(activity, Activity::Work | Activity::ReceiptWork)
                    {
                        Some(
                            memory
                                .acquire_owned()
                                .await
                                .map_err(|_| transport::Error::Protocol("memory closed"))?,
                        )
                    } else {
                        None
                    };
                    Ok(MemoryGuard(reservation))
                }
            },
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(1), sent.notified()).await.expect("input processing must release its existing memory before output needs another reservation");
    task.abort();
    assert!(task.await.expect_err("cancelled").is_cancelled());
    tokio::time::timeout(Duration::from_millis(300), async {
        while memory.available_permits() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("input and output memory returned after cancellation");
}

struct RecordingWriter(mpsc::Sender<Vec<u8>>);
impl MessageWriter for RecordingWriter {
    type Error = transport::Error;
    async fn send(&mut self, bytes: Vec<u8>) -> std::result::Result<(), Self::Error> {
        self.0
            .send(bytes)
            .await
            .map_err(|_| transport::Error::Protocol("test receiver closed"))
    }
    async fn finish(&mut self) -> std::result::Result<(), Self::Error> {
        Ok(())
    }
    fn abort(&mut self) {}
}

fn seed_frontiers(
    replica: &ThreadReplica,
    repository: &Repository,
    signer: &Ed25519Signer,
    genesis: &ThreadGenesis,
    count: usize,
) {
    use objects::object::{
        Attribution, Principal, State, Tree,
        thread_replication::{ThreadOperation, ThreadOperationBody},
    };
    replica
        .set_sharing([7; 32], &[ThreadFacet::Source].into())
        .expect("explicit source sharing");
    for index in 0..count {
        let state = State::new_snapshot(
            Tree::new().hash(),
            vec![genesis.base],
            Attribution::human(Principal::new(
                format!("Agent {index}"),
                "agent@example.test",
            )),
        );
        let operation = ThreadOperation {
            version: 1,
            thread: replica.thread_id(),
            parents: Default::default(),
            publisher: genesis.creator,
            body: ThreadOperationBody::Capture(
                objects::object::thread_replication::AuthoredCapture::local(
                    state.encode_current_msgpack().expect("source").into(),
                ),
            ),
        };
        replica
            .receive(
                &crypto::thread_operation::SignedOperation::sign(&operation, signer)
                    .expect("signed capture"),
                repository.store(),
                |_| Ok(()),
            )
            .expect("accepted capture");
    }
}

fn fixture(count: usize) -> (tempfile::TempDir, Repository, ThreadReplica) {
    let directory = tempfile::TempDir::new().expect("local replica");
    let repository = Repository::init_default(directory.path()).expect("repository");
    let signer = Ed25519Signer::from_seed(&[17; 32]).expect("creator");
    let genesis = ThreadGenesis {
        version: 1,
        spool: "01980000-0000-7000-8000-000000000001".into(),
        parent: None,
        base: repository.head().expect("HEAD").expect("initial state"),
        name: "idle".into(),
        intent: "push on durable changes".into(),
        creator: signer.public_key().try_into().expect("key"),
        owner: heddle_object_model::object::thread_replication::GenesisOwner::LocalKey(
            signer.public_key().try_into().expect("key"),
        ),
        nonce: vec![93],
    };
    let replica = ThreadReplica::create(
        repository.heddle_dir(),
        &SignedGenesis::sign(&genesis, &signer).expect("proof"),
    )
    .expect("replica");
    seed_frontiers(&replica, &repository, &signer, &genesis, count);
    (directory, repository, replica)
}

#[tokio::test]
async fn unchanged_replication_sends_no_frames_or_database_activity_after_initial_frontiers() {
    let (_directory, repository, replica) = fixture(1);
    let (_changes, receiver) = watch::channel(Some(1));
    let feed = Feed::from_changes(replica.thread_id(), receiver);
    let session = Session::new(
        LocalReplica::new(replica, Arc::new(repository.store().clone())),
        [7; 32],
        [ThreadFacet::Source].into(),
        1,
    )
    .expect("session");
    let (writer, mut frames) = mpsc::channel(4);
    let work = Arc::new(AtomicUsize::new(0));
    let measured = work.clone();
    let task = tokio::spawn(async move {
        run(
            session,
            QuietReader,
            RecordingWriter(writer),
            Side::Acceptor,
            &feed,
            move |activity| {
                if matches!(
                    activity,
                    Activity::Check
                        | Activity::Receive
                        | Activity::Work
                        | Activity::ReceiptWork
                        | Activity::ReceiptCheck
                ) {
                    measured.fetch_add(1, Ordering::SeqCst);
                }
                std::future::ready(Ok(()))
            },
        )
        .await
    });
    let frame = tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("initial frontier")
        .expect("frame");
    let Frame::Have(have) = Side::Initiator
        .decode::<crate::replication::native::Error>(&frame)
        .expect("frontier frame")
    else {
        panic!("Have")
    };
    assert_eq!(
        have.frontiers
            .iter()
            .map(|frontier| frontier.heads.len())
            .sum::<usize>(),
        1
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    let baseline = work.load(Ordering::SeqCst);
    assert!(
        tokio::time::timeout(Duration::from_millis(2200), frames.recv())
            .await
            .is_err(),
        "unchanged peers must not send empty Have heartbeats"
    );
    assert_eq!(
        work.load(Ordering::SeqCst),
        baseline,
        "idle time must not enter any database-work authorization gate"
    );
    task.abort();
    assert!(task.await.expect_err("cancelled").is_cancelled());
}

struct PausedWriter {
    resume: Option<Arc<Notify>>,
    frames: mpsc::Sender<Vec<u8>>,
}
impl MessageWriter for PausedWriter {
    type Error = transport::Error;
    async fn send(&mut self, bytes: Vec<u8>) -> std::result::Result<(), Self::Error> {
        if let Some(resume) = self.resume.take() {
            resume.notified().await;
        }
        self.frames
            .send(bytes)
            .await
            .map_err(|_| transport::Error::Protocol("test receiver closed"))
    }
    async fn finish(&mut self) -> std::result::Result<(), Self::Error> {
        Ok(())
    }
    fn abort(&mut self) {}
}
struct CompletedReceive {
    count: Option<Arc<AtomicUsize>>,
    changed: Arc<Notify>,
}
impl Drop for CompletedReceive {
    fn drop(&mut self) {
        if let Some(count) = &self.count {
            count.fetch_add(1, Ordering::SeqCst);
            self.changed.notify_one();
        }
    }
}
impl ActivityGuard for CompletedReceive {
    type Retained = ();
    fn finish(self, _: usize) -> std::result::Result<(), transport::Error> {
        Ok(())
    }
}

#[tokio::test]
async fn bounded_sender_progress_drains_more_than_256_frontiers_without_idle_clock_or_input() {
    let (_directory, repository, replica) = fixture(300);
    let (_changes, receiver) = watch::channel(Some(1));
    let feed = Feed::from_changes(replica.thread_id(), receiver);
    let session = Session::new(
        LocalReplica::new(replica, Arc::new(repository.store().clone())),
        [7; 32],
        [ThreadFacet::Source].into(),
        1,
    )
    .expect("session");
    let (writer, mut frames) = mpsc::channel(1);
    let release = Arc::new(Notify::new());
    let paused = PausedWriter {
        resume: Some(release.clone()),
        frames: writer,
    };
    let completed = Arc::new(AtomicUsize::new(0));
    let observed = completed.clone();
    let changed = Arc::new(Notify::new());
    let notify = changed.clone();
    let first_idle = tokio::time::Instant::now() + Duration::from_secs(60);
    let task = tokio::spawn(async move {
        run_with_idle_clock(
            session,
            QuietReader,
            paused,
            Side::Acceptor,
            &feed,
            move |activity| {
                std::future::ready(Ok(CompletedReceive {
                    count: (activity == Activity::Receive).then(|| observed.clone()),
                    changed: notify.clone(),
                }))
            },
            tokio::time::interval_at(first_idle, Duration::from_secs(60)),
        )
        .await
    });
    // The writer retains page one. One initial empty control check and 129
    // completed one-item announcements leave 128 pages queued, forcing the
    // producer to suspend. Wait for that state rather than guessing a delay.
    tokio::time::timeout(Duration::from_secs(10), async {
        while completed.load(Ordering::SeqCst) < 130 {
            changed.notified().await;
        }
    })
    .await
    .expect("bounded output queue must fill behind paused writer");
    release.notify_one();
    let mut heads = std::collections::BTreeSet::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        while heads.len() < 300 {
            let frame = frames.recv().await.expect("complete paged announcement");
            let Frame::Have(have) = Side::Initiator
                .decode::<crate::replication::native::Error>(&frame)
                .expect("frontier")
            else {
                panic!("Have")
            };
            assert_eq!(have.frontiers.len(), 1);
            assert_eq!(
                have.frontiers[0].heads.len(),
                1,
                "negotiated one-item pages"
            );
            assert!(
                heads.insert(have.frontiers[0].heads[0].clone()),
                "each frontier is sent once"
            );
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("sender progress must wake blocked announcement without a clock tick");
    task.abort();
    assert!(task.await.expect_err("cancelled").is_cancelled());
}

#[tokio::test]
async fn idle_clock_enforces_local_expiration_without_inbound_or_store_work() {
    let (_directory, repository, replica) = fixture(0);
    let (_changes, receiver) = watch::channel(Some(1));
    let feed = Feed::from_changes(replica.thread_id(), receiver);
    let session = Session::new(
        LocalReplica::new(replica, Arc::new(repository.store().clone())),
        [7; 32],
        [ThreadFacet::Source].into(),
        1,
    )
    .expect("session");
    let (writer, mut frames) = mpsc::channel(1);
    let deadline = tokio::time::Instant::now() + Duration::from_millis(150);
    let idle_checks = Arc::new(AtomicUsize::new(0));
    let measured = idle_checks.clone();
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        run(
            session,
            QuietReader,
            RecordingWriter(writer),
            Side::Acceptor,
            &feed,
            move |activity| {
                if activity == Activity::Idle {
                    measured.fetch_add(1, Ordering::SeqCst);
                }
                std::future::ready(if tokio::time::Instant::now() >= deadline {
                    Err(transport::Error::Protocol("local capability expired"))
                } else {
                    Ok(())
                })
            },
        ),
    )
    .await
    .expect("idle expiry must stop a silent peer")
    .expect_err("expired capability");
    assert!(matches!(
        result,
        Error::Transport(transport::Error::Protocol("local capability expired"))
    ));
    assert!(idle_checks.load(Ordering::SeqCst) > 0);
    assert_eq!(
        frames.recv().await,
        None,
        "empty replica never emits a fabricated heartbeat"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replication_sender_preserves_rpc_subscriber_and_span() {
    use tracing::{Instrument, instrument::WithSubscriber};
    use tracing_subscriber::{Layer, layer::SubscriberExt};

    struct SenderEvents {
        observed: Arc<AtomicUsize>,
        parented: Arc<AtomicUsize>,
    }
    impl<S> Layer<S> for SenderEvents
    where
        S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            context: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if event.metadata().target() != "replication_sender_oracle" {
                return;
            }
            self.observed.fetch_add(1, Ordering::SeqCst);
            if context.event_scope(event).is_some_and(|scope| {
                scope
                    .from_root()
                    .any(|span| span.name() == "replication_rpc_oracle")
            }) {
                self.parented.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    let (_directory, repository, replica) = fixture(1);
    let (_changes, receiver) = watch::channel(Some(1));
    let feed = Feed::from_changes(replica.thread_id(), receiver);
    let session = Session::new(
        LocalReplica::new(replica, Arc::new(repository.store().clone())),
        [7; 32],
        [ThreadFacet::Source].into(),
        1,
    )
    .expect("session");
    let observed = Arc::new(AtomicUsize::new(0));
    let parented = Arc::new(AtomicUsize::new(0));
    let subscriber = tracing_subscriber::registry().with(SenderEvents {
        observed: observed.clone(),
        parented: parented.clone(),
    });
    let dispatch = tracing::Dispatch::new(subscriber);
    let span = tracing::dispatcher::with_default(&dispatch, || {
        tracing::info_span!("replication_rpc_oracle")
    });
    let (writer, mut frames) = mpsc::channel(4);
    let task = tokio::spawn(
        async move {
            run(
                session,
                QuietReader,
                RecordingWriter(writer),
                Side::Acceptor,
                &feed,
                |activity| {
                    // Work only executes in the separately spawned sender.
                    if matches!(activity, Activity::Work | Activity::ReceiptWork) {
                        tracing::info!(target: "replication_sender_oracle", "export work");
                    }
                    std::future::ready(Ok(()))
                },
            )
            .await
        }
        .instrument(span)
        .with_subscriber(dispatch),
    );
    tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("initial frontier delivered")
        .expect("frame");
    task.abort();
    assert!(task.await.expect_err("cancelled").is_cancelled());
    assert!(
        observed.load(Ordering::SeqCst) > 0,
        "sender lost the RPC subscriber"
    );
    assert_eq!(
        parented.load(Ordering::SeqCst),
        observed.load(Ordering::SeqCst),
        "sender work must remain inside the originating RPC span"
    );
}

struct CloseFeedAfterCommit {
    replica: ThreadReplica,
    initial: i64,
    changes: watch::Sender<Option<i64>>,
    invalidated: Arc<std::sync::atomic::AtomicBool>,
    receive: bool,
}
impl Drop for CloseFeedAfterCommit {
    fn drop(&mut self) {
        if self.receive && self.replica.generation().expect("durable generation") > self.initial {
            self.invalidated.store(true, Ordering::SeqCst);
            self.changes.send_replace(None);
        }
    }
}
impl ActivityGuard for CloseFeedAfterCommit {
    type Retained = ();
    fn finish(self, _: usize) -> std::result::Result<(), transport::Error> {
        Ok(())
    }
}
#[tokio::test]
async fn committed_input_receipt_flushes_before_invalidated_feed_closes() {
    committed_receipt_case(false).await;
}
#[tokio::test]
async fn committed_prefix_receipt_precedes_denied_next_operation() {
    committed_receipt_case(true).await;
}
async fn committed_receipt_case(with_successor: bool) {
    use objects::object::{
        Attribution, Principal, State, Tree,
        thread_replication::{
            AuthoredCapture, OPERATION_FORMAT, ThreadOperation, ThreadOperationBody,
        },
    };
    let (_directory, repository, replica) = fixture(0);
    let initial = replica.generation().expect("initial generation");
    let signer = Ed25519Signer::from_seed(&[17; 32]).expect("creator");
    let state = State::new_snapshot(
        Tree::new().hash(),
        vec![replica.genesis().expect("genesis").base],
        Attribution::human(Principal::new("Author", "author@example.test")),
    );
    let operation = ThreadOperation {
        version: 1,
        thread: replica.thread_id(),
        parents: Default::default(),
        publisher: signer.public_key().try_into().expect("key"),
        body: ThreadOperationBody::Capture(AuthoredCapture::local(
            state.encode_current_msgpack().expect("State").into(),
        )),
    };
    let id = operation.id().expect("operation ID");
    let signed = crypto::thread_operation::SignedOperation::sign(&operation, &signer)
        .expect("signed original");
    let mut records = vec![SignedRecord {
        format: OPERATION_FORMAT.into(),
        canonical_record: signed.canonical,
        signatures: vec![RecordSignature {
            public_key: operation.publisher.to_vec(),
            signature: signed.signature,
        }],
    }];
    let successor_id = if with_successor {
        let mut successor = operation.clone();
        successor.parents.insert(id);
        let successor_id = successor.id().expect("successor ID");
        let signed = crypto::thread_operation::SignedOperation::sign(&successor, &signer)
            .expect("signed successor");
        records.push(SignedRecord {
            format: OPERATION_FORMAT.into(),
            canonical_record: signed.canonical,
            signatures: vec![RecordSignature {
                public_key: successor.publisher.to_vec(),
                signature: signed.signature,
            }],
        });
        Some(successor_id)
    } else {
        None
    };
    let bytes = Frame::Operations(ReplicationOperations {
        operations: records,
        authority_admissions: vec![],
    })
    .request()
    .encode_to_vec();
    let (changes, receiver) = watch::channel(Some(initial));
    let feed = Feed::from_changes(replica.thread_id(), receiver);
    let session = Session::new(
        LocalReplica::new(replica.clone(), Arc::new(repository.store().clone())),
        [7; 32],
        [ThreadFacet::Source].into(),
        2,
    )
    .expect("session");
    let (writer, mut frames) = mpsc::channel(8);
    let invalidated = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let gate_invalidated = invalidated.clone();
    let gate_replica = replica.clone();
    let task = tokio::spawn(async move {
        run(
            session,
            InputReader {
                frame: Some(bytes),
                input_memory: None,
            },
            RecordingWriter(writer),
            Side::Acceptor,
            &feed,
            move |activity| {
                std::future::ready(
                    if gate_invalidated.load(Ordering::SeqCst)
                        && !matches!(
                            activity,
                            Activity::ReceiptWork
                                | Activity::ReceiptCheck
                                | Activity::InputConsumed { .. }
                        )
                    {
                        Err(transport::Error::Protocol("old content epoch invalidated"))
                    } else {
                        Ok(CloseFeedAfterCommit {
                            replica: gate_replica.clone(),
                            initial,
                            changes: changes.clone(),
                            invalidated: gate_invalidated.clone(),
                            receive: activity == Activity::Receive,
                        })
                    },
                )
            },
        )
        .await
    });
    let receipt = tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(bytes) = frames.recv().await {
            if let Frame::Receipt(receipt) = Side::Initiator
                .decode::<crate::replication::native::Error>(&bytes)
                .expect("response")
            {
                return Some(receipt);
            }
        }
        None
    })
    .await
    .expect("bounded acknowledgement")
    .expect("committed input must receive its receipt before feed closure");
    assert_eq!(receipt.accepted_operation_ids, vec![id.as_bytes().to_vec()]);
    assert!(replica.operation(&id).expect("durable original").is_some());
    if let Some(successor_id) = successor_id {
        assert!(
            replica
                .operation(&successor_id)
                .expect("successor lookup")
                .is_none(),
            "next operation must not commit after authority invalidation"
        );
    }
    assert!(invalidated.load(Ordering::SeqCst));
    assert!(
        tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .expect("clean reconnect after receipt")
            .expect("driver task")
            .is_err()
    );
}
