//! Direct replication uses the native causal driver. An independently admitted
//! owner may read their private data without creating a hosted sharing grant.
use std::{collections::BTreeSet, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use api::{
    heddle::api::{v1alpha1::CallContext, v2alpha1::*},
    v2::client::{MessageReader, MessageWriter},
};
use iroh::endpoint::{RecvStream, SendStream};
use objects::{
    object::{
        ContentHash,
        thread_replication::{Admission, ThreadFacet},
    },
    store::FsStore,
};
use prost::Message;
use thread_api::{
    live_replication::{self, Activity, Side},
    replication::{
        self,
        native::{Error, LocalReplica},
        opening,
        store::{ReceivedOperation, ReplicaStore},
    },
    transport,
};
use tracing::{Instrument, instrument::WithSubscriber};

use super::{DeviceRpc, auth, checkout};

struct AbortTask(tokio::task::AbortHandle);
impl Drop for AbortTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl DeviceRpc {
    pub(crate) async fn serve_stream(
        &self,
        method: &str,
        context: &CallContext,
        peer: [u8; 32],
        send: SendStream,
        recv: RecvStream,
        budget: &mut super::super::hosted::claim_protocol::CallBudget,
    ) -> Result<()> {
        let descriptor = api::v2::method_descriptor(method).context("unknown device stream")?;
        let (mut writer, mut reader) = thread_api::transport::accepted_stream(
            send,
            recv,
            opening::FRAME_LIMIT,
            Duration::from_secs(10),
            descriptor,
        )?;
        let body = reader
            .next()
            .await?
            .context("replication opening required")?;
        let request = ReplicateThreadRequest::decode(body.as_slice())?;
        let Some(replicate_thread_request::Body::Open(open)) = request.body else {
            bail!("replication requires Open");
        };
        let reference = open.thread.as_ref().context("Thread required")?;
        let spool = reference.spool.as_ref().context("Spool required")?;
        let registered = repo::device_catalog::load(&self.home, uuid::Uuid::parse_str(&spool.id)?)?;
        let session = Arc::new(auth::authorize(
            &self.home, descriptor, context, &body, registered,
        )?);
        budget.retain().map_err(anyhow::Error::msg)?;
        let facets = BTreeSet::from(ThreadFacet::ALL);
        let accepted = opening::accept(
            &open,
            reference,
            &self.endpoint(),
            peer,
            &facets,
            Vec::new(),
        )?;
        // Lookup is side-effect free. Installing a new original genesis is an
        // explicit publication mutation, handled by StartThread/PublishContent.
        let replica = repo::thread_replication::ThreadReplica::open(
            &session.spool.heddle_dir,
            checkout::thread(&session, Some(reference))?,
        )?;
        if let Some(genesis) = accepted.genesis
            && genesis != replica.genesis()?
        {
            bail!("opening genesis differs from local Thread");
        }
        let negotiated = opening::parse_facets(&accepted.ready.facets)?;
        let max_items = accepted
            .ready
            .budget
            .as_ref()
            .context("negotiated budget required")?
            .max_items as usize;
        let shared = self.feed(&session)?;
        let mut changes = shared.changes.subscribe();
        let (updates, receiver) = tokio::sync::watch::channel(Some(0));
        // One Spool watcher observes durable post-commit markers for every
        // subscriber. This adapter performs no generation query while idle.
        let notifier = tokio::spawn(
            async move {
                let _shared = shared;
                while changes.changed().await.is_ok() {
                    let value = *changes.borrow_and_update();
                    if value == u64::MAX {
                        let _ = updates.send(None);
                        break;
                    }
                    if updates
                        .send(Some(i64::try_from(value).unwrap_or(i64::MAX)))
                        .is_err()
                    {
                        break;
                    }
                }
            }
            .in_current_span()
            .with_current_subscriber(),
        );
        let _notifier = AbortTask(notifier.abort_handle());
        let feed = live_replication::Feed::from_changes(replica.thread_id(), receiver);
        let backend = OwnedReplica(
            LocalReplica::new(replica, Arc::new(FsStore::new(&session.spool.heddle_dir)))
                .with_device_authority(self.home.clone()),
        );
        let causal = replication::Session::new(backend, peer, negotiated, max_items)?;
        session.check_current(&self.home)?;
        writer
            .send(
                ReplicateThreadResponse {
                    body: Some(replicate_thread_response::Body::Ready(accepted.ready)),
                }
                .encode_to_vec(),
            )
            .await?;
        let home = self.home.clone();
        live_replication::run(
            causal,
            reader,
            writer,
            Side::Acceptor,
            &feed,
            move |activity| {
                let session = session.clone();
                let home = home.clone();
                async move {
                    let checked = if matches!(activity, Activity::Idle) {
                        session.check_clock()
                    } else {
                        session.check_current(&home)
                    };
                    checked.map_err(|error| transport::Error::Io(error.to_string()))
                }
            },
        )
        .await?;
        Ok(())
    }
}

/// Only constructed after verifying the owner's locally admitted mint root,
/// exact method/resource caveats and request proof. Hosted publication policy
/// does not restrict an owner's direct access to their own private device.
#[derive(Clone)]
struct OwnedReplica(LocalReplica<FsStore>);
impl ReplicaStore for OwnedReplica {
    type Error = Error;
    fn thread_id(&self) -> ContentHash {
        self.0.thread_id()
    }
    async fn generation(&self) -> Result<i64, Error> {
        self.0.generation().await
    }
    async fn sharing(&self, _destination: [u8; 32]) -> Result<BTreeSet<ThreadFacet>, Error> {
        Ok(BTreeSet::from(ThreadFacet::ALL))
    }
    async fn frontier_page(
        &self,
        facet: ThreadFacet,
        after: Option<ContentHash>,
        limit: usize,
    ) -> Result<Vec<ContentHash>, Error> {
        self.0.frontier_page(facet, after, limit).await
    }
    async fn operation(
        &self,
        id: ContentHash,
    ) -> Result<Option<(ReceivedOperation, Admission)>, Error> {
        self.0.operation(id).await
    }
    async fn receive(&self, operation: ReceivedOperation) -> Result<Admission, Error> {
        self.0.receive(operation).await
    }
    async fn remember_peer_heads(
        &self,
        peer: [u8; 32],
        heads: Vec<(ThreadFacet, ContentHash)>,
    ) -> Result<(), Error> {
        self.0.remember_peer_heads(peer, heads).await
    }
    async fn record_peer_receipt(
        &self,
        peer: [u8; 32],
        id: ContentHash,
        admission: Admission,
    ) -> Result<(), Error> {
        self.0.record_peer_receipt(peer, id, admission).await
    }
    async fn settled_peer_heads(
        &self,
        peer: [u8; 32],
        facets: BTreeSet<ThreadFacet>,
        limit: usize,
    ) -> Result<Vec<(ContentHash, Admission)>, Error> {
        self.0.settled_peer_heads(peer, facets, limit).await
    }
    async fn needed_from_peer(
        &self,
        peer: [u8; 32],
        facets: BTreeSet<ThreadFacet>,
        limit: usize,
    ) -> Result<Vec<ContentHash>, Error> {
        self.0.needed_from_peer(peer, facets, limit).await
    }
}
