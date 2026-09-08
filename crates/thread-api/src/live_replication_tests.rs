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
            nonce: vec![91],
        };
        let replica = ThreadReplica::create(
            repo.heddle_dir(),
            &SignedGenesis::sign(&genesis, &signer).expect("proof"),
        )
        .expect("replica");
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
                        buffer: (activity == Activity::Work).then(|| Count::acquire(&work_buffers)),
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
        nonce: vec![91],
    };
    let replica = ThreadReplica::create(
        repo.heddle_dir(),
        &SignedGenesis::sign(&genesis, &signer).expect("proof"),
    )
    .expect("replica");
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
                    let reservation = if activity == Activity::Work {
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
