// SPDX-License-Identifier: Apache-2.0
//! Continuous, bounded replication on caller-authenticated protobuf streams.
//! One feed is shared by every observer/replicator of a local Thread. Incoming
//! metadata writes never move a checkout or install unrequested source blobs.
use std::{future::Future, sync::Arc, time::Duration};

use api::v2::client::{MessageReader, MessageWriter};
use heddle_object_model::object::ContentHash;
use prost::Message;
#[cfg(feature = "native")]
use repo::thread_replication::ThreadReplica;
use tokio::{
    sync::{Mutex, Notify, mpsc, oneshot, watch},
    task::{AbortHandle, JoinHandle},
};
use tracing::{Instrument, instrument::WithSubscriber};

use crate::{
    contract::*,
    replication::{Frame, Outbound, Session, store::ReplicaStore},
    transport,
};

#[derive(Debug, thiserror::Error)]
pub enum Error<E: std::error::Error + 'static> {
    #[error(transparent)]
    Transport(#[from] transport::Error),
    #[error("replica store: {0}")]
    Store(#[source] E),
    #[error(transparent)]
    Protocol(#[from] crate::replication::Error),
    #[error("replication worker: {0}")]
    Worker(String),
    #[error("replication response budget exhausted; reopen from durable state")]
    Backpressure,
    #[error("Thread change feed stopped")]
    FeedClosed,
    #[error("Thread policy changed; reconnect from durable receipts")]
    PolicyChanged,
}
pub type Result<T, E> = std::result::Result<T, Error<E>>;

impl<E: std::error::Error + 'static> From<crate::replication::StoreError<E>> for Error<E> {
    fn from(error: crate::replication::StoreError<E>) -> Self {
        match error {
            crate::replication::StoreError::Store(error) => Self::Store(error),
            crate::replication::StoreError::Protocol(error) => Self::Protocol(error),
        }
    }
}

struct AbortOnDrop(AbortHandle);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Create once per Thread, then clone. This observes writes from other local
/// processes using the durable generation; it does not poll once per stream.
#[derive(Clone)]
pub struct Feed {
    thread: ContentHash,
    changes: watch::Receiver<Option<i64>>,
    _task: Option<Arc<AbortOnDrop>>,
}
impl Feed {
    /// The host shares one durable-generation watcher per Thread. Subscribe
    /// before the initial announcement; missed notifications trigger a fresh
    /// generation check, never an assumed accepted frontier.
    pub fn from_changes(thread: ContentHash, changes: watch::Receiver<Option<i64>>) -> Self {
        Self {
            thread,
            changes,
            _task: None,
        }
    }

    #[cfg(feature = "native")]
    pub async fn new(
        replica: ThreadReplica,
    ) -> std::result::Result<Self, crate::replication::native::Error> {
        let thread = replica.thread_id();
        let initial = replica.clone();
        let generation = tokio::task::spawn_blocking(move || initial.generation())
            .await
            .map_err(crate::replication::native::Error::from)??;
        let (sender, changes) = watch::channel(Some(generation));
        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(200));
            loop {
                interval.tick().await;
                if sender.is_closed() {
                    break;
                }
                let replica = replica.clone();
                let result = tokio::task::spawn_blocking(move || replica.generation()).await;
                match result {
                    Ok(Ok(generation)) => {
                        sender.send_if_modified(|old| {
                            if *old == Some(generation) {
                                false
                            } else {
                                *old = Some(generation);
                                true
                            }
                        });
                    }
                    _ => {
                        let _ = sender.send(None);
                        break;
                    }
                }
            }
        });
        Ok(Self {
            thread,
            changes,
            _task: Some(Arc::new(AbortOnDrop(task.abort_handle()))),
        })
    }
}

#[derive(Clone, Copy)]
pub enum Side {
    Initiator,
    Acceptor,
}
impl Side {
    fn decode<E: std::error::Error + 'static>(self, bytes: &[u8]) -> Result<Frame, E> {
        input::reservation(bytes, 64)?;
        Ok(match self {
            Self::Initiator => Frame::from_response(
                ReplicateThreadResponse::decode(bytes).map_err(transport::Error::from)?,
            )?,
            Self::Acceptor => Frame::from_request(
                ReplicateThreadRequest::decode(bytes).map_err(transport::Error::from)?,
            )?,
        })
    }
    fn encode(self, frame: Frame) -> Vec<u8> {
        match self {
            Self::Initiator => frame.request().encode_to_vec(),
            Self::Acceptor => frame.response().encode_to_vec(),
        }
    }
}

enum Event {
    Incoming(Frame),
    Announce,
    Maintain,
}

struct Queued {
    item: Outbound,
    immediate_receipt: bool,
    generation: u64,
    delivered: Option<oneshot::Sender<()>>,
}
async fn completion<E: std::error::Error + 'static>(
    queue: &mpsc::Sender<Queued>,
    item: Outbound,
    generation: u64,
) -> Result<(), E> {
    let (sent, received) = oneshot::channel();
    queue
        .send(Queued {
            item,
            immediate_receipt: true,
            generation,
            delivered: Some(sent),
        })
        .await
        .map_err(|_| Error::Backpressure)?;
    received
        .await
        .map_err(|_| Error::Worker("replication sender stopped before receipt flush".into()))
}
fn requires_disclosure_fence<E: std::error::Error + 'static>(frame: &Frame) -> Result<bool, E> {
    use heddle_object_model::object::thread_replication::{ThreadFacet, ThreadOperation};
    let Frame::Operations(batch) = frame else {
        return Ok(false);
    };
    for record in &batch.operations {
        let operation = ThreadOperation::decode(&record.canonical_record)
            .map_err(crate::replication::Error::from)?;
        // Same-facet causal parents are mandatory in both stores. Any Metadata
        // parent may settle an already pending policy descendant, so fence the
        // whole facet, not just a policy-shaped incoming body.
        if operation.facet() == ThreadFacet::Metadata {
            return Ok(true);
        }
    }
    Ok(false)
}
#[path = "live_replication_input.rs"]
pub mod input;
fn retained_record_bytes(record: &SignedRecord) -> usize {
    record.format.capacity()
        + record.canonical_record.capacity()
        + record.signatures.capacity() * std::mem::size_of::<RecordSignature>()
        + record
            .signatures
            .iter()
            .map(|s| s.public_key.capacity() + s.signature.capacity())
            .sum::<usize>()
}
fn retained_frame_bytes(frame: &Frame) -> usize {
    match frame {
        Frame::Operations(batch) => {
            (batch.operations.capacity() + batch.authority_admissions.capacity())
                * std::mem::size_of::<SignedRecord>()
                + batch
                    .operations
                    .iter()
                    .chain(&batch.authority_admissions)
                    .map(retained_record_bytes)
                    .sum::<usize>()
        }
        _ => 0,
    }
}

/// A permission-only recheck must not wait for an output memory reservation
/// already retained by this stream. Work may produce one bounded output frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activity {
    /// Idle clock tick: verify locally known identity, revocation and time
    /// caveats only. Hosts must not query the store or reserve output memory.
    /// Every actual input and disclosure still uses the fresh gates below.
    Idle,
    /// Shrink retained input accounting after consumed decoded allocations have
    /// been dropped. This is an accounting callback, not authorization.
    InputConsumed {
        remaining_bytes: usize,
    },
    Check,
    /// Advance input admission and bounded control queues. The reader already
    /// accounts for the input; waiting for output memory here could deadlock
    /// every receiver while it holds the memory needed by those producers.
    Receive,
    /// Load and encode one output frame, retaining its memory through delivery.
    Work,
    /// Encode the immediate receipt for this session's just-admitted input.
    ReceiptWork,
    /// Recheck only that immediate receipt, never prepared source disclosure.
    ReceiptCheck,
    /// Post-receipt peer bookkeeping for an already admitted pending input.
    Bookkeeping,
}

/// Hosts can charge database work and output memory separately. Finishing the
/// work releases execution slots while the returned lease covers delivery.
/// Devices without a shared work scheduler can keep returning `()`.
pub trait ActivityGuard: Send {
    type Retained: Send;
    fn finish(self, encoded_bytes: usize) -> std::result::Result<Self::Retained, transport::Error>;
}
impl ActivityGuard for () {
    type Retained = ();
    fn finish(self, _: usize) -> std::result::Result<(), transport::Error> {
        Ok(())
    }
}

/// Call after validating the opening, endpoint bindings, Thread, and facets.
/// `authorize` rechecks the live host permission, including expiry/revocation.
/// It runs before every admission and output, including queued output. Readers
/// must enforce the negotiated frame bound before allocating message bodies.
pub async fn run<B, R, W, G, F, A>(
    session: Session<B>,
    reader: R,
    writer: W,
    side: Side,
    feed: &Feed,
    authorize: G,
) -> Result<(), B::Error>
where
    B: ReplicaStore,
    R: MessageReader<Error = transport::Error>,
    W: MessageWriter<Error = transport::Error> + 'static,
    G: Fn(Activity) -> F + Clone + Send + Sync + 'static,
    F: Future<Output = std::result::Result<A, transport::Error>> + Send,
    A: ActivityGuard,
{
    run_with_idle_clock(
        session,
        reader,
        writer,
        side,
        feed,
        authorize,
        tokio::time::interval(Duration::from_secs(1)),
    )
    .await
}

// Keep the idle clock injectable inside the driver so progress tests can prove
// that queue wakeups work without a heartbeat rescuing a missed notification.
async fn run_with_idle_clock<B, R, W, G, F, A>(
    mut session: Session<B>,
    mut reader: R,
    mut writer: W,
    side: Side,
    feed: &Feed,
    authorize: G,
    mut heartbeat: tokio::time::Interval,
) -> Result<(), B::Error>
where
    B: ReplicaStore,
    R: MessageReader<Error = transport::Error>,
    W: MessageWriter<Error = transport::Error> + 'static,
    G: Fn(Activity) -> F + Clone + Send + Sync + 'static,
    F: Future<Output = std::result::Result<A, transport::Error>> + Send,
    A: ActivityGuard,
{
    if feed.thread != session.replica.thread_id() {
        return Err(transport::Error::Protocol("change feed belongs to another Thread").into());
    }
    drop(authorize(Activity::Check).await?);
    let mut changes = feed.changes.clone();
    let (queue, mut outgoing) = mpsc::channel::<Queued>(256);
    let progress = Arc::new(Notify::new());
    let sender_progress = progress.clone();
    let (completions, mut incoming_completions) = mpsc::channel::<Queued>(1);
    let sender_session = session.clone();
    let sender_authorize = authorize.clone();
    let delivery = Arc::new(Mutex::new(()));
    let sender_delivery = delivery.clone();
    let (disclosures, mut disclosure_changes) = watch::channel(0u64);
    let mut disclosure_generation = 0u64;
    let mut sender: JoinHandle<Result<(), B::Error>> = tokio::spawn(
        (async move {
            let mut deferred = None;
            let mut priority = None;
            let mut priority_closed = false;
            loop {
                let next = if let Some(item) = priority.take() { Some(item) }
                    else if let Ok(item) = incoming_completions.try_recv() { Some(item) }
                    else if let Some(item) = deferred.take() { Some(item) }
                    else { tokio::select! {
                        biased;
                        item = incoming_completions.recv(), if !priority_closed => match item { Some(item) => Some(item), None => { priority_closed = true; continue; } },
                        item = outgoing.recv() => item,
                    }};
                let Some(queued) = next else { break; };
                sender_progress.notify_one();
                let generation = queued.generation;
                let immediate_receipt = queued.immediate_receipt;
                if generation != *disclosure_changes.borrow_and_update() {
                    continue;
                }
                let session = sender_session.clone();
                let gate = sender_authorize.clone();
                // Only cancel an unacquired work reservation. Once admitted,
                // store implementations may own non-cancellable blocking work;
                // keep its lease until it completes, then discard stale output.
                let activity = if immediate_receipt { gate(Activity::ReceiptWork).await? } else {
                    tokio::select! {
                        biased;
                        changed = disclosure_changes.changed() => {
                            changed.map_err(|_| Error::FeedClosed)?;
                            continue;
                        }
                        received = incoming_completions.recv(), if !priority_closed => {
                            if let Some(received) = received {
                                deferred = Some(queued);
                                priority = Some(received);
                                continue;
                            }
                            priority_closed = true;
                            deferred = Some(queued);
                            continue;
                        }
                        activity = gate(Activity::Work) => activity?,
                    }
                };
                let Queued { item, delivered, .. } = queued;
                let prepared = async {
                    let frame = match item {
                        Outbound::Operation(id) => session.export_operation(id).await?,
                        Outbound::Frame(frame) => {
                            if let Frame::Have(have) = &frame {
                                let allowed = session.export_facets().await?;
                                for frontier in &have.frontiers {
                                    if !allowed.contains(&crate::replication::native_facet(
                                        frontier.facet,
                                    )?) {
                                        return Err(transport::Error::Protocol(
                                            "sharing policy changed before disclosure",
                                        )
                                        .into());
                                    }
                                }
                            }
                            frame
                        }
                    };
                    let encoded = side.encode(frame);
                    let retained = activity.finish(encoded.len())?;
                    Ok::<_, Error<B::Error>>((encoded, retained))
                }.await;
                if generation != *disclosure_changes.borrow_and_update() { continue; }
                let (encoded, retained) = prepared?;
                let delivery_guard = sender_delivery.lock().await;
                if generation != *disclosure_changes.borrow_and_update() {
                    continue;
                }
                // Permission-only checks cannot acquire work slots while this
                // mutex is held; an input may already own the same work pool.
                drop(gate(if immediate_receipt { Activity::ReceiptCheck } else { Activity::Check }).await?);
                writer.send(encoded).await?;
                drop(delivery_guard);
                drop(retained);
                if let Some(delivered) = delivered { let _ = delivered.send(()); }
            }
            writer.finish().await?;
            Ok(())
        })
        .in_current_span()
        .with_current_subscriber(),
    );
    // Dropping a JoinHandle detaches it. Abort explicitly so cancellation drops
    // the transport writer too, including while its peer is applying pressure.
    let _sender_guard = AbortOnDrop(sender.abort_handle());
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut announce = true;
    let mut maintain = true;
    loop {
        let event = tokio::select! {
            result = &mut sender => return result.map_err(worker)?,
            result = reader.next() => match result? {
                Some(bytes) => Event::Incoming(side.decode(&bytes)?),
                None => break,
            },
            result = changes.changed() => {
                result.map_err(|_| Error::FeedClosed)?;
                if changes.borrow_and_update().is_none() { return Err(Error::FeedClosed); }
                announce = true;
                maintain = true;
                continue
            },
            _ = std::future::ready(()), if announce && queue.capacity() > 128 => Event::Announce,
            _ = std::future::ready(()), if maintain && queue.capacity() > 128 => Event::Maintain,
            _ = progress.notified() => continue,
            _ = heartbeat.tick() => {
                drop(authorize(Activity::Idle).await?);
                continue
            }
        };
        let event = match event {
            Event::Incoming(frame) => {
                maintain = true;
                let preflight = authorize(Activity::Receive).await?;
                let units = session.input_units(frame)?;
                drop(preflight);
                let container_bytes = units.capacity() * std::mem::size_of::<Frame>();
                let mut units = units.into_iter();
                while let Some(frame) = units.next() {
                    let immediate_receipt = matches!(&frame, Frame::Operations(_));
                    let policy = requires_disclosure_fence(&frame)?;
                    let activity = authorize(Activity::Receive).await?;
                    let delivery_guard = if policy {
                        let guard = delivery.lock().await;
                        disclosure_generation = disclosure_generation
                            .checked_add(1)
                            .ok_or(Error::Backpressure)?;
                        disclosures.send_replace(disclosure_generation);
                        Some(guard)
                    } else {
                        None
                    };
                    let output = session.handle_input(frame).await?;
                    drop(activity);
                    drop(delivery_guard);
                    let remaining_bytes = if units.len() == 0 {
                        0
                    } else {
                        container_bytes
                            + units
                                .as_slice()
                                .iter()
                                .map(retained_frame_bytes)
                                .sum::<usize>()
                    };
                    // IntoIter retains its backing allocation until dropped,
                    // including after its final element has been consumed.
                    if units.len() == 0 {
                        drop(units);
                        units = Vec::new().into_iter();
                    }
                    drop(authorize(Activity::InputConsumed { remaining_bytes }).await?);
                    for item in output {
                        if immediate_receipt && matches!(&item, Outbound::Frame(Frame::Receipt(_)))
                        {
                            completion(&completions, item, disclosure_generation).await?;
                        } else {
                            queue
                                .try_send(Queued {
                                    item,
                                    immediate_receipt: false,
                                    generation: disclosure_generation,
                                    delivered: None,
                                })
                                .map_err(|_| Error::Backpressure)?;
                        }
                    }
                    if session.has_input_bookkeeping() {
                        let bookkeeping = authorize(Activity::Bookkeeping).await?;
                        session.finish_input_bookkeeping().await?;
                        drop(bookkeeping);
                    }
                    if policy {
                        return Err(Error::PolicyChanged);
                    }
                }
                continue;
            }
            other => other,
        };
        let activity = authorize(Activity::Receive).await?;
        let output = match event {
            Event::Incoming(frame) => {
                maintain = true;
                session.handle_input(frame).await?
            }
            Event::Announce => {
                let frame = session.announcement().await?;
                announce = frame.is_some();
                frame.into_iter().map(Outbound::Frame).collect()
            }
            Event::Maintain => {
                let frame = session.control().await?;
                maintain = frame.is_some();
                frame.into_iter().map(Outbound::Frame).collect()
            }
        };
        drop(activity);
        for item in output {
            queue
                .try_send(Queued {
                    item,
                    immediate_receipt: false,
                    generation: disclosure_generation,
                    delivered: None,
                })
                .map_err(|_| Error::Backpressure)?;
        }
    }
    drop(queue);
    drop(completions);
    sender.await.map_err(worker)?
}

fn worker<E: std::error::Error + 'static>(error: tokio::task::JoinError) -> Error<E> {
    Error::Worker(error.to_string())
}

#[cfg(all(test, feature = "native"))]
#[path = "live_replication_tests.rs"]
mod tests;

#[cfg(all(test, feature = "native"))]
#[path = "live_replication_schedule_tests.rs"]
mod schedule_tests;
