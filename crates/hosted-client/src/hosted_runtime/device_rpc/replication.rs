//! Direct replication uses the native causal driver. An independently admitted
//! owner may read their private data without creating a hosted sharing grant.
use std::{collections::BTreeSet, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use api::{
    heddle::api::{common::CallContext, v1alpha2::*},
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

type DeviceHostedReplica<F> = thread_api::replication::native::HostedReplica<
    repo::thread_replication::hosted_trust::SystemClock,
    thread_api::hybrid::authority::SelectedAuthority<F>,
>;

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
        if method == "/heddle.api.v1alpha2.SyncService/PublishContent" {
            return self
                .serve_publication_stream(method, context, peer, send, recv, budget)
                .await;
        }
        if method == "/heddle.api.v1alpha2.SyncService/Fetch" {
            return self
                .serve_fetch_stream(method, context, peer, send, recv, budget)
                .await;
        }
        let descriptor = api::v2::method_descriptor(method).context("unknown device stream")?;
        let (writer, mut reader) = thread_api::transport::accepted_stream(
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
        let bundle = accepted
            .import_authority
            .clone()
            .or(replica.hybrid_import_bundle()?);
        let backend = LocalReplica::new(replica, Arc::new(FsStore::new(&session.spool.heddle_dir)))
            .with_device_authority(self.home.clone());
        if let Some(bundle) = bundle {
            let backend = self.hosted_backend(backend, session.clone(), bundle)?;
            let causal =
                replication::Session::new(OwnedReplica(backend), peer, negotiated, max_items)?
                    .with_protocol(open.protocol.as_ref(), accepted.ready.protocol.as_ref())?;
            self.run_replica(causal, accepted.ready, session, feed, reader, writer)
                .await
        } else {
            let causal =
                replication::Session::new(OwnedReplica(backend), peer, negotiated, max_items)?
                    .with_protocol(open.protocol.as_ref(), accepted.ready.protocol.as_ref())?;
            self.run_replica(causal, accepted.ready, session, feed, reader, writer)
                .await
        }
    }
    pub(super) fn hosted_backend(
        &self,
        local: LocalReplica<FsStore>,
        session: Arc<auth::Session>,
        bundle: ImportPublicProofBundleV1,
    ) -> Result<
        DeviceHostedReplica<
            impl Fn(&ImportPublicProofBundleV1, i64) -> repo::thread_replication::Result<()>
            + Send
            + Sync
            + 'static
            + use<>,
        >,
    > {
        use repo::thread_replication::hosted_trust::{HostedTrust, SystemClock};
        use thread_api::hybrid::authority::{AcceptedHistory, SelectedAuthority};
        let repository = repo::Repository::open(&session.spool.root)?;
        let now = chrono::Utc::now().timestamp();
        let (_, pinned) = repository.pinned_owner_observation(now)?;
        let history = AcceptedHistory::from_selected_spool(
            &bundle,
            &pinned,
            now,
            heddleco_capability_verifier::VerificationLimits::new(30 * 24 * 60 * 60)?,
        )?;
        // This carrier names a lookup in durable, independently selected trust.
        // HostedTrust::open cannot enroll a root from the incoming bundle.
        let authority = &bundle
            .witness_set
            .as_ref()
            .and_then(|s| s.body.as_ref())
            .context("hosted witness set absent")?
            .deployment_authority;
        let trust = Arc::new(HostedTrust::open(
            &session.spool.heddle_dir,
            authority,
            SystemClock,
        )?);
        let home = self.home.clone();
        let directory = session.spool.heddle_dir.clone();
        let authority = Arc::new(SelectedAuthority::new(
            history,
            bundle,
            move |_: &ImportPublicProofBundleV1, _: i64| {
                session
                    .check_current(&home)
                    .map_err(|error| repo::thread_replication::Error::Invalid(error.to_string()))
            },
        ));
        Ok(local.with_hosted_authority(directory, trust, authority))
    }
    #[allow(clippy::too_many_arguments)]
    async fn run_replica<B: ReplicaStore<Error = Error>>(
        &self,
        causal: replication::Session<OwnedReplica<B>>,
        ready: ReplicationReady,
        session: Arc<auth::Session>,
        feed: live_replication::Feed,
        reader: transport::Reader,
        mut writer: transport::Writer,
    ) -> Result<()> {
        session.check_current(&self.home)?;
        writer
            .send(
                ReplicateThreadResponse {
                    body: Some(replicate_thread_response::Body::Ready(ready)),
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
                    if matches!(activity, Activity::InputConsumed { .. }) {
                        return Ok(());
                    }
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
struct OwnedReplica<B>(B);
impl<B: ReplicaStore<Error = Error>> ReplicaStore for OwnedReplica<B> {
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
