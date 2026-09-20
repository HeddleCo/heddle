//! Transport scheduling oracles. The store is deliberately a small scripted
//! durable boundary; original-author admission is covered by native/hosted tests.
use std::sync::{
    Mutex as StdMutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use crypto::{Ed25519Signer, Signer, thread_operation::SignedOperation};
use heddle_object_model::object::{
    Attribution, CollaborationActor, Principal, State, Tree,
    thread_replication::{
        Admission, AuthoredCapture, OPERATION_FORMAT, ThreadFacet, ThreadOperation,
        ThreadOperationBody,
        metadata::{
            AUTHORITY_FORMAT, Control, ThreadControl,
            retention::{MaterialRetention, RetentionPolicy},
        },
    },
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::*;
use crate::replication::store::ReceivedOperation;

#[derive(Clone)]
struct Store {
    thread: ContentHash,
    accepted: Arc<StdMutex<Vec<ContentHash>>>,
    pending: bool,
    receipt_written: Arc<AtomicBool>,
}
impl ReplicaStore for Store {
    type Error = transport::Error;
    fn thread_id(&self) -> ContentHash {
        self.thread
    }
    async fn generation(&self) -> std::result::Result<i64, Self::Error> {
        Ok(self.accepted.lock().expect("test lock").len() as i64)
    }
    async fn sharing(
        &self,
        _: [u8; 32],
    ) -> std::result::Result<std::collections::BTreeSet<ThreadFacet>, Self::Error> {
        Ok(ThreadFacet::ALL.into())
    }
    async fn frontier_page(
        &self,
        _: ThreadFacet,
        after: Option<ContentHash>,
        _: usize,
    ) -> std::result::Result<Vec<ContentHash>, Self::Error> {
        Ok(if after.is_none() {
            vec![ContentHash::from_bytes([9; 32])]
        } else {
            vec![]
        })
    }
    async fn operation(
        &self,
        _: ContentHash,
    ) -> std::result::Result<Option<(ReceivedOperation, Admission)>, Self::Error> {
        Ok(None)
    }
    async fn receive(
        &self,
        received: ReceivedOperation,
    ) -> std::result::Result<Admission, Self::Error> {
        let id = received
            .original
            .verify()
            .expect("signed transport fixture")
            .id()
            .expect("ID");
        self.accepted.lock().expect("test lock").push(id);
        Ok(if self.pending {
            Admission::Pending
        } else {
            Admission::Accepted
        })
    }
    async fn remember_peer_heads(
        &self,
        _: [u8; 32],
        _: Vec<(ThreadFacet, ContentHash)>,
    ) -> std::result::Result<(), Self::Error> {
        assert!(
            self.receipt_written.load(Ordering::SeqCst),
            "pending bookkeeping must follow the durable receipt"
        );
        Err(transport::Error::Protocol(
            "injected peer bookkeeping failure after commit",
        ))
    }
    async fn record_peer_receipt(
        &self,
        _: [u8; 32],
        _: ContentHash,
        _: Admission,
    ) -> std::result::Result<(), Self::Error> {
        Ok(())
    }
    async fn settled_peer_heads(
        &self,
        _: [u8; 32],
        _: std::collections::BTreeSet<ThreadFacet>,
        _: usize,
    ) -> std::result::Result<Vec<(ContentHash, Admission)>, Self::Error> {
        Ok(vec![])
    }
    async fn needed_from_peer(
        &self,
        _: [u8; 32],
        _: std::collections::BTreeSet<ThreadFacet>,
        _: usize,
    ) -> std::result::Result<Vec<ContentHash>, Self::Error> {
        Ok(vec![])
    }
}
struct Reader {
    bytes: Option<Vec<u8>>,
    waiting: Arc<Notify>,
    _input: Arc<StdMutex<Option<OwnedSemaphorePermit>>>,
    receives: Arc<AtomicUsize>,
    initial_receives: Arc<AtomicUsize>,
    half_close: bool,
    eof: Arc<AtomicBool>,
    eof_changed: Arc<Notify>,
}
impl MessageReader for Reader {
    type Error = transport::Error;
    async fn next(&mut self) -> std::result::Result<Option<Vec<u8>>, Self::Error> {
        if self.bytes.is_some() {
            self.waiting.notified().await;
            self.initial_receives
                .store(self.receives.load(Ordering::SeqCst), Ordering::SeqCst);
            return Ok(self.bytes.take());
        }
        if self.half_close {
            self.eof.store(true, Ordering::SeqCst);
            self.eof_changed.notify_waiters();
            return Ok(None);
        }
        std::future::pending().await
    }
    fn cancel(&mut self) {}
}
struct Writer {
    frames: mpsc::Sender<Vec<u8>>,
    receipt: Arc<AtomicBool>,
    receives: Arc<AtomicUsize>,
    receipt_receives: Arc<AtomicUsize>,
    ordinary: Arc<AtomicUsize>,
}
impl MessageWriter for Writer {
    type Error = transport::Error;
    async fn send(&mut self, bytes: Vec<u8>) -> std::result::Result<(), Self::Error> {
        if matches!(
            Side::Initiator
                .decode::<transport::Error>(&bytes)
                .expect("response"),
            Frame::Receipt(_)
        ) {
            self.receipt.store(true, Ordering::SeqCst);
            self.receipt_receives
                .store(self.receives.load(Ordering::SeqCst), Ordering::SeqCst);
        }
        if matches!(
            Side::Initiator
                .decode::<transport::Error>(&bytes)
                .expect("response"),
            Frame::Have(_)
        ) {
            self.ordinary.fetch_add(1, Ordering::SeqCst);
        }
        self.frames
            .send(bytes)
            .await
            .map_err(|_| transport::Error::Protocol("test output closed"))
    }
    async fn finish(&mut self) -> std::result::Result<(), Self::Error> {
        Ok(())
    }
    fn abort(&mut self) {}
}
struct Guard {
    bytes: Option<OwnedSemaphorePermit>,
    work: Option<OwnedSemaphorePermit>,
}
impl ActivityGuard for Guard {
    type Retained = Option<OwnedSemaphorePermit>;
    fn finish(mut self, bytes: usize) -> std::result::Result<Self::Retained, transport::Error> {
        self.work.take();
        if let Some(permit) = &mut self.bytes {
            assert!(
                bytes <= permit.num_permits(),
                "encoded output exceeds acquired quota"
            );
            let release = permit.num_permits() - bytes;
            drop(permit.split(release));
        }
        Ok(self.bytes.take())
    }
}
fn operation(thread: ContentHash, metadata: bool, nonce: u8) -> SignedRecord {
    let signer = Ed25519Signer::from_seed(&[17; 32]).expect("test signer");
    let body = if metadata {
        // Canonical signing shape only; the scripted store is not an authority
        // verifier and this envelope is never submitted to a real endpoint.
        let authority = b"transport scheduler fixture";
        ThreadOperationBody::Metadata(
            ThreadControl {
                version: 1,
                spool: uuid::Uuid::from_bytes([3; 16]),
                actor: CollaborationActor {
                    principal_id: uuid::Uuid::from_bytes([4; 16]),
                    agent_id: None,
                },
                authority_digest: ContentHash::compute_typed(AUTHORITY_FORMAT, authority),
                authority_envelope: authority.to_vec(),
                client_operation_id: uuid::Uuid::from_bytes([nonce; 16]),
                occurred_at_ms: 1,
                control: Control::Retention(RetentionPolicy {
                    source: MaterialRetention::Retain,
                    collaboration: MaterialRetention::Retain,
                    evidence: MaterialRetention::Discard,
                    scrubbed_timeline: MaterialRetention::Retain,
                    raw_transcripts: MaterialRetention::Discard,
                }),
            }
            .encode()
            .expect("canonical metadata"),
        )
    } else {
        let state = State::new_snapshot(
            Tree::new().hash(),
            vec![heddle_object_model::object::StateId::from_content_hash(
                ContentHash::from_bytes([nonce; 32]),
            )],
            Attribution::human(Principal::new("fixture", "fixture@example.test")),
        );
        ThreadOperationBody::Capture(AuthoredCapture::local(
            state.encode_current_msgpack().expect("state").into(),
        ))
    };
    let original = ThreadOperation {
        version: 1,
        thread,
        parents: Default::default(),
        publisher: signer.public_key().try_into().expect("key"),
        body,
    };
    let signed = SignedOperation::sign(&original, &signer).expect("signed fixture");
    SignedRecord {
        format: OPERATION_FORMAT.into(),
        canonical_record: signed.canonical,
        signatures: vec![RecordSignature {
            public_key: original.publisher.to_vec(),
            signature: signed.signature,
        }],
    }
}
async fn saturated_case(metadata: bool, pending: bool, count: usize, half_close: bool) {
    for _ in 0..2 {
        let thread = ContentHash::from_bytes([1; 32]);
        let records = (1..=count)
            .map(|n| operation(thread, metadata, n as u8))
            .collect();
        let input = Frame::Operations(ReplicationOperations {
            operations: records,
            authority_admissions: vec![],
            boundary_acceptances: vec![],
        })
        .request()
        .encode_to_vec();
        let capacity = input::reservation(&input, 64).expect("bounded protobuf input");
        let memory = Arc::new(Semaphore::new(capacity));
        let input_permit = Arc::new(StdMutex::new(Some(
            memory
                .clone()
                .acquire_many_owned(capacity as u32)
                .await
                .expect("entire shared byte pool retained by input"),
        )));
        let work = Arc::new(Semaphore::new(1));
        let waiting = Arc::new(Notify::new());
        let receipt_written = Arc::new(AtomicBool::new(false));
        let store = Store {
            thread,
            accepted: Arc::new(StdMutex::new(vec![])),
            pending,
            receipt_written: receipt_written.clone(),
        };
        let accepted = store.accepted.clone();
        let session = Session::new(
            store,
            [8; 32],
            [if metadata {
                ThreadFacet::Metadata
            } else {
                ThreadFacet::Source
            }]
            .into(),
            64,
        )
        .expect("session");
        let (_changes, receiver) = watch::channel(Some(0));
        let feed = Feed::from_changes(thread, receiver);
        let (frames, mut received) = mpsc::channel(8);
        let gate_memory = memory.clone();
        let gate_input = input_permit.clone();
        let gate_work = work.clone();
        let gate_waiting = waiting.clone();
        let remainders = Arc::new(StdMutex::new(vec![]));
        let measured = remainders.clone();
        let receives = Arc::new(AtomicUsize::new(0));
        let measured_receives = receives.clone();
        let initial_receives = Arc::new(AtomicUsize::new(0));
        let reader_initial = initial_receives.clone();
        let reader_receives = receives.clone();
        let writer_receives = receives.clone();
        let receipt_receives = Arc::new(AtomicUsize::new(0));
        let writer_measurement = receipt_receives.clone();
        let ordinary = Arc::new(AtomicUsize::new(0));
        let writer_ordinary = ordinary.clone();
        let eof = Arc::new(AtomicBool::new(false));
        let reader_eof = eof.clone();
        let eof_changed = Arc::new(Notify::new());
        let reader_eof_changed = eof_changed.clone();
        let task = tokio::spawn(async move {
            run(
                session,
                Reader {
                    bytes: Some(input),
                    waiting,
                    _input: input_permit,
                    receives: reader_receives,
                    initial_receives: reader_initial,
                    half_close,
                    eof: reader_eof,
                    eof_changed: reader_eof_changed,
                },
                Writer {
                    frames,
                    receipt: receipt_written,
                    receives: writer_receives,
                    receipt_receives: writer_measurement,
                    ordinary: writer_ordinary,
                },
                Side::Acceptor,
                &feed,
                move |activity| {
                    let memory = gate_memory.clone();
                    let input = gate_input.clone();
                    let work = gate_work.clone();
                    let waiting = gate_waiting.clone();
                    let measured = measured.clone();
                    let receives = measured_receives.clone();
                    let eof = eof.clone();
                    let eof_changed = eof_changed.clone();
                    async move {
                        if let Activity::InputConsumed { remaining_bytes } = activity {
                            measured.lock().expect("test lock").push(remaining_bytes);
                            let mut input = input.lock().expect("test lock");
                            let permit = input.as_mut().expect("retained input");
                            assert!(
                                remaining_bytes <= permit.num_permits(),
                                "unprocessed allocations cannot grow beyond input quota"
                            );
                            let release = permit.num_permits() - remaining_bytes;
                            drop(permit.split(release));
                            return Ok(Guard {
                                bytes: None,
                                work: None,
                            });
                        }
                        if activity == Activity::Receive {
                            receives.fetch_add(1, Ordering::SeqCst);
                        }
                        let bytes = if activity == Activity::Work {
                            waiting.notify_one();
                            let permit = memory
                                .acquire_many_owned(capacity as u32)
                                .await
                                .expect("output bytes");
                            if half_close {
                                loop {
                                    let changed = eof_changed.notified();
                                    if eof.load(Ordering::SeqCst) {
                                        break;
                                    }
                                    changed.await;
                                }
                            }
                            Some(permit)
                        } else if activity == Activity::ReceiptWork {
                            Some(
                                memory
                                    .acquire_many_owned(512)
                                    .await
                                    .expect("bounded receipt bytes"),
                            )
                        } else {
                            None
                        };
                        let work = if matches!(
                            activity,
                            Activity::Work
                                | Activity::ReceiptWork
                                | Activity::Receive
                                | Activity::Bookkeeping
                        ) {
                            Some(
                                work.acquire_owned()
                                    .await
                                    .expect("one work slot, acquired after memory"),
                            )
                        } else {
                            None
                        };
                        Ok(Guard { bytes, work })
                    }
                },
            )
            .await
        });
        let receipts = tokio::time::timeout(Duration::from_secs(3), async {
            let mut receipts = vec![];
            while let Some(bytes) = received.recv().await {
                if let Frame::Receipt(receipt) = Side::Initiator
                    .decode::<transport::Error>(&bytes)
                    .expect("response")
                {
                    receipts.push(receipt);
                    if receipts.len() == count {
                        return receipts;
                    }
                }
            }
            panic!("committed input lost receipt");
        })
        .await
        .expect("receipts must progress with a full shared byte pool and one work slot");
        assert_eq!(accepted.lock().expect("test lock").len(), count);
        assert!(receipts.iter().all(|receipt| if pending {
            receipt.pending_operation_ids.len() == 1
        } else {
            receipt.accepted_operation_ids.len() == 1
        }));
        if half_close {
            assert!(
                tokio::time::timeout(Duration::from_secs(3), task)
                    .await
                    .expect("half-close drains queued source frontier")
                    .expect("task")
                    .is_ok()
            );
            assert!(
                ordinary.load(Ordering::SeqCst) > 0,
                "priority channel closure must not discard deferred ordinary output"
            );
        } else if metadata || pending {
            assert!(
                tokio::time::timeout(Duration::from_secs(3), task)
                    .await
                    .expect("bounded reset")
                    .expect("task")
                    .is_err()
            );
        } else {
            task.abort();
            assert!(
                task.await
                    .expect_err("cancel normal live session")
                    .is_cancelled()
            );
        }
        assert_eq!(
            memory.available_permits(),
            capacity,
            "all payload reservations released"
        );
        assert_eq!(work.available_permits(), 1, "work reservation released");
        let remainders = remainders.lock().expect("test lock");
        assert_eq!(remainders.last(), Some(&0));
        if count > 1 {
            assert!(
                remainders[0] > 0,
                "later originals and vector backing remain accounted"
            );
        }
        assert_eq!(
            receipt_receives.load(Ordering::SeqCst) - initial_receives.load(Ordering::SeqCst),
            count + 1,
            "one batch preflight plus one admission per input; no empty bookkeeping gate"
        );
    }
}
#[tokio::test]
async fn queued_metadata_progresses_when_input_owns_all_output_bytes() {
    saturated_case(true, false, 1, false).await;
}
#[tokio::test]
async fn source_batch_receipts_bypass_unacquired_output_without_dropping_data() {
    saturated_case(false, false, 2, false).await;
}
#[tokio::test]
async fn pending_commit_receipt_precedes_failed_peer_bookkeeping() {
    saturated_case(false, true, 1, false).await;
}

#[tokio::test]
async fn half_closed_input_drains_deferred_ordinary_source_output() {
    saturated_case(false, false, 1, true).await;
}

#[test]
fn policy_fence_tracks_canonical_property_causality() {
    use heddle_object_model::object::{
        StateId,
        thread_replication::{GenesisOwner, ThreadGenesis},
    };
    let record = operation(ContentHash::from_bytes([1; 32]), true, 1);
    let mut policy = ThreadOperation::decode(&record.canonical_record).expect("policy");
    let ThreadOperationBody::Metadata(bytes) = &policy.body else {
        panic!("metadata")
    };
    let control = ThreadControl::decode(bytes).expect("control");
    let mut name = control.clone();
    name.control = Control::Name("ordinary name".into());
    policy.body = ThreadOperationBody::Metadata(name.encode().expect("name"));
    let mut name_record = record.clone();
    name_record.canonical_record = policy.encode().expect("ordinary operation");
    let frame = |record: SignedRecord| {
        InputUnit::Operation(ReceivedOperation::from(SignedOperation {
            canonical: record.canonical_record,
            signature: record.signatures[0].signature.clone(),
        }))
    };
    assert!(
        !requires_disclosure_fence::<transport::Error>(&frame(name_record)).expect("ordinary name")
    );
    assert!(requires_disclosure_fence::<transport::Error>(&frame(record)).expect("retention"));
    let genesis = ThreadGenesis {
        version: 1,
        spool: control.spool.to_string(),
        parent: None,
        base: StateId::from_content_hash(ContentHash::from_bytes([2; 32])),
        name: "fixture".into(),
        intent: String::new(),
        owner: GenesisOwner::LocalKey(policy.publisher),
        creator: policy.publisher,
        nonce: vec![],
    };
    assert!(
        control.validate_parents(&genesis, &[policy]).is_err(),
        "ordinary Name cannot activate pending Retention"
    );
}
#[test]
fn retained_acceptance_allocations_include_spare_capacity_once_until_last_user() {
    use crypto::{
        original_boundary_acceptance::SignedBoundaryAcceptance,
        thread_authority_admission::SignedAuthorityAdmission,
    };
    // Allocation accounting is independent of signature admission. These small
    // receipt buffers represent the retained canonical containers only.
    let mut canonical = vec![1; 64];
    canonical.reserve(4096);
    let canonical_capacity = canonical.capacity();
    let evidence = Arc::new(SignedBoundaryAcceptance {
        canonical,
        signature: vec![2; 64],
    });
    let make_unit = || {
        let record = operation(ContentHash::from_bytes([1; 32]), false, 1);
        InputUnit::Operation(ReceivedOperation {
            original: SignedOperation {
                canonical: record.canonical_record,
                signature: record.signatures[0].signature.clone(),
            },
            authority_admission: Some(SignedAuthorityAdmission {
                canonical: vec![3; 64],
                signature: vec![4; 64],
                boundary_acceptance: Some(evidence.clone()),
            }),
        })
    };
    let units = [make_unit(), make_unit()];
    let one = retained_unit_bytes(&units[..1]);
    let both = retained_unit_bytes(&units);
    assert!(
        one >= canonical_capacity,
        "acceptance spare capacity must remain reserved"
    );
    assert!(
        both < 2 * one,
        "shared acceptance must not be charged once per operation"
    );
    assert!(
        retained_unit_bytes(&units[1..]) >= canonical_capacity,
        "consuming first original cannot refund evidence retained by successor"
    );
    assert_eq!(retained_unit_bytes(&[]), 0);
}
