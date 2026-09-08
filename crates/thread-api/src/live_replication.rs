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
    sync::{mpsc, watch},
    task::{AbortHandle, JoinHandle},
};

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

/// Call after validating the opening, endpoint bindings, Thread, and facets.
/// `authorize` rechecks the live host permission, including expiry/revocation.
/// It runs before every admission and output, including queued output. Readers
/// must enforce the negotiated frame bound before allocating message bodies.
pub async fn run<B, R, W, G, F>(
    mut session: Session<B>,
    mut reader: R,
    mut writer: W,
    side: Side,
    feed: &Feed,
    authorize: G,
) -> Result<(), B::Error>
where
    B: ReplicaStore,
    R: MessageReader<Error = transport::Error>,
    W: MessageWriter<Error = transport::Error> + 'static,
    G: Fn() -> F + Clone + Send + Sync + 'static,
    F: Future<Output = std::result::Result<(), transport::Error>> + Send,
{
    if feed.thread != session.replica.thread_id() {
        return Err(transport::Error::Protocol("change feed belongs to another Thread").into());
    }
    authorize().await?;
    let mut changes = feed.changes.clone();
    let (queue, mut outgoing) = mpsc::channel::<Outbound>(256);
    let sender_session = session.clone();
    let sender_authorize = authorize.clone();
    let mut sender: JoinHandle<Result<(), B::Error>> = tokio::spawn(async move {
        while let Some(item) = outgoing.recv().await {
            let session = sender_session.clone();
            let gate = sender_authorize.clone();
            gate().await?;
            let frame = match item {
                Outbound::Operation(id) => session.export_operation(id).await?,
                Outbound::Frame(frame) => {
                    if let Frame::Have(have) = &frame {
                        let allowed = session.export_facets().await?;
                        for frontier in &have.frontiers {
                            if !allowed.contains(&crate::replication::native_facet(frontier.facet)?)
                            {
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
            // Store reads can yield; verify live rights again at disclosure.
            gate().await?;
            writer.send(side.encode(frame)).await?;
        }
        writer.finish().await?;
        Ok(())
    });
    // Dropping a JoinHandle detaches it. Abort explicitly so cancellation drops
    // the transport writer too, including while its peer is applying pressure.
    let _sender_guard = AbortOnDrop(sender.abort_handle());
    let mut heartbeat = tokio::time::interval(Duration::from_secs(1));
    let mut announce = true;
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
                Event::Maintain
            },
            _ = std::future::ready(()), if announce && queue.capacity() > 128 => Event::Announce,
            _ = heartbeat.tick() => {
                queue.try_send(Outbound::Frame(Frame::Have(ReplicationHave::default()))).map_err(|_| Error::Backpressure)?;
                Event::Maintain
            }
        };
        authorize().await?;
        let output = match event {
            Event::Incoming(frame) => session.handle(frame).await?,
            Event::Announce => {
                let frame = session.announcement().await?;
                announce = frame.is_some();
                frame.into_iter().map(Outbound::Frame).collect()
            }
            Event::Maintain => session
                .control()
                .await?
                .into_iter()
                .map(Outbound::Frame)
                .collect(),
        };
        for item in output {
            queue.try_send(item).map_err(|_| Error::Backpressure)?;
        }
    }
    drop(queue);
    sender.await.map_err(worker)?
}

fn worker<E: std::error::Error + 'static>(error: tokio::task::JoinError) -> Error<E> {
    Error::Worker(error.to_string())
}

#[cfg(all(test, feature = "native"))]
#[path = "live_replication_tests.rs"]
mod tests;
