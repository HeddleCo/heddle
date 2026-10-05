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
    hybrid::authority::PublicProof,
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
    thread_api::hybrid::authority::SelectedAuthority<F, PublicProof>,
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
        let backend = DeviceReplica {
            device: self.clone(),
            replica: replica.clone(),
            local: LocalReplica::new(replica, Arc::new(FsStore::new(&session.spool.heddle_dir)))
                .with_device_authority(self.home.clone()),
            session: session.clone(),
        };
        let causal = replication::Session::new(backend, peer, negotiated, max_items)?
            .with_protocol(open.protocol.as_ref(), accepted.ready.protocol.as_ref())?;
        self.run_replica(causal, accepted.ready, session, feed, reader, writer)
            .await
    }
    #[cfg(test)]
    pub(super) fn relay(
        &self,
        replica: repo::thread_replication::ThreadReplica,
        local: LocalReplica<FsStore>,
        session: Arc<auth::Session>,
    ) -> impl ReplicaStore<Error = Error> {
        DeviceReplica {
            device: self.clone(),
            replica,
            local,
            session,
        }
    }
    pub(super) async fn refresh_export_bundle(
        &self,
        session: &auth::Session,
        bundle: &mut ImportPublicProofBundleV1,
    ) -> Result<()> {
        use repo::thread_replication::hosted_trust::{HostedTrust, SystemClock};
        let authority = &bundle
            .witness_set
            .as_ref()
            .and_then(|s| s.body.as_ref())
            .context("hosted witness set absent")?
            .deployment_authority;
        let trust = HostedTrust::open(&session.spool.heddle_dir, authority, SystemClock)?;
        let snapshot = trust.snapshot()?;
        self.require_current_root(&snapshot.root)?;
        let config = config::UserConfig::load_default()?.hosted_runtime_config(None)?;
        let lookup = super::super::hosted::descriptor_trust::HostedWitnessLookup::new(
            &snapshot.root.authority,
            &config,
        )?;
        #[cfg(test)]
        let lookup = {
            let mut lookup = lookup;
            lookup.test_responses = self
                .test_witness_responses
                .lock()
                .map_err(|_| anyhow::anyhow!("test lookup poisoned"))?
                .clone();
            lookup
        };
        let jobs = snapshot
            .known_job_associations
            .iter()
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        super::super::hosted::descriptor_trust::refresh_import_proofs(
            &lookup,
            bundle,
            api::witness_trust::SetExpectation {
                authority: &snapshot.root.authority,
                root_id: &snapshot.root.root_id,
                root_public_key: &snapshot.root.public_key,
                root_epoch: snapshot.root_epoch,
                now_unix_millis: chrono::Utc::now().timestamp_millis(),
                clock_floor_unix_millis: snapshot.clock_floor_millis,
                known_job_keys: &jobs,
            },
            snapshot.previous.as_ref(),
        )
        .await?;
        Ok(())
    }
    fn require_current_root(
        &self,
        root: &repo::thread_replication::hosted_trust::RootSelection,
    ) -> Result<()> {
        let config = config::UserConfig::load_default()?.hosted_runtime_config(None)?;
        if let (Some(id), Some(key)) = (config.descriptor_key_id, config.descriptor_public_key) {
            anyhow::ensure!(
                id == root.root_id && key == root.public_key,
                "explicit descriptor root selection changed"
            );
        } else {
            super::super::hosted::descriptor_trust::require_current_pin(
                &self.home.join("descriptor-trust.toml"),
                &root.authority,
                &root.root_id,
                &root.public_key,
            )?;
        }
        Ok(())
    }
    pub(super) async fn refresh_native_export_bundle(
        &self,
        session: &auth::Session,
        bundle: &mut NativePublicProofBundleV1,
    ) -> Result<()> {
        use repo::thread_replication::hosted_trust::{HostedTrust, SystemClock};
        let authority = &bundle
            .witness_set
            .as_ref()
            .and_then(|s| s.body.as_ref())
            .context("hosted witness set absent")?
            .deployment_authority;
        let trust = HostedTrust::open(&session.spool.heddle_dir, authority, SystemClock)?;
        let snapshot = trust.snapshot()?;
        self.require_current_root(&snapshot.root)?;
        let config = config::UserConfig::load_default()?.hosted_runtime_config(None)?;
        let lookup = super::super::hosted::descriptor_trust::HostedWitnessLookup::new(
            &snapshot.root.authority,
            &config,
        )?;
        #[cfg(test)]
        let lookup = {
            let mut lookup = lookup;
            lookup.test_responses = self
                .test_witness_responses
                .lock()
                .map_err(|_| anyhow::anyhow!("test lookup poisoned"))?
                .clone();
            lookup
        };
        let jobs = snapshot
            .known_job_associations
            .iter()
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        super::super::hosted::descriptor_trust::refresh_native_proofs(
            &lookup,
            bundle,
            api::witness_trust::SetExpectation {
                authority: &snapshot.root.authority,
                root_id: &snapshot.root.root_id,
                root_public_key: &snapshot.root.public_key,
                root_epoch: snapshot.root_epoch,
                now_unix_millis: chrono::Utc::now().timestamp_millis(),
                clock_floor_unix_millis: snapshot.clock_floor_millis,
                known_job_keys: &jobs,
            },
            snapshot.previous.as_ref(),
        )
        .await?;
        Ok(())
    }
    #[allow(clippy::type_complexity)]
    pub(crate) fn hosted_backend<B: Into<PublicProof>>(
        &self,
        local: LocalReplica<FsStore>,
        session: Arc<auth::Session>,
        bundle: B,
    ) -> Result<
        DeviceHostedReplica<
            impl Fn(
                &PublicProof,
                i64,
                &repo::thread_replication::hosted_trust::TrustTransaction<'_>,
            ) -> repo::thread_replication::Result<()>
            + Send
            + Sync
            + 'static
            + use<B>,
        >,
    > {
        use repo::thread_replication::hosted_trust::{HostedTrust, SystemClock};
        use thread_api::hybrid::authority::{AcceptedHistory, SelectedAuthority};
        let bundle = bundle.into();
        let repository = repo::Repository::open(&session.spool.root)?;
        let now = chrono::Utc::now().timestamp();
        let (_, pinned) = repository.pinned_owner_observation(now)?;
        let history = AcceptedHistory::from_public(
            &bundle,
            &pinned,
            now,
            heddleco_capability_verifier::VerificationLimits::new(30 * 24 * 60 * 60)?,
        )?;
        // This carrier names a lookup in durable, independently selected trust.
        // HostedTrust::open cannot enroll a root from the incoming bundle.
        let authority = &bundle
            .witness_set()
            .and_then(|s| s.body.as_ref())
            .context("hosted witness set absent")?
            .deployment_authority;
        let trust = Arc::new(HostedTrust::open(
            &session.spool.heddle_dir,
            authority,
            SystemClock,
        )?);
        let root = trust.snapshot()?.root;
        self.require_current_root(&root)?;
        let device = self.clone();
        let home = self.home.clone();
        let directory = session.spool.heddle_dir.clone();
        let export = bundle.clone();
        let authority =
            Arc::new(
                SelectedAuthority::from_proof(
                    history,
                    bundle,
                    move |_: &PublicProof,
                          _: i64,
                          context: &repo::thread_replication::hosted_trust::TrustTransaction<
                        '_,
                    >| {
                        device.require_current_root(&root).map_err(|error| {
                            repo::thread_replication::Error::Invalid(error.to_string())
                        })?;
                        session.check_current_in(&home, context).map_err(|error| {
                            repo::thread_replication::Error::Invalid(error.to_string())
                        })
                    },
                ),
            );
        Ok(local
            .with_hosted_authority(directory, trust, authority)
            .with_export_proof(export))
    }
    #[allow(clippy::too_many_arguments)]
    async fn run_replica<B: ReplicaStore<Error = Error>>(
        &self,
        causal: replication::Session<B>,
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
struct DeviceReplica {
    device: DeviceRpc,
    replica: repo::thread_replication::ThreadReplica,
    local: LocalReplica<FsStore>,
    session: Arc<auth::Session>,
}
impl ReplicaStore for DeviceReplica {
    type Error = Error;
    fn thread_id(&self) -> ContentHash {
        self.local.thread_id()
    }
    async fn generation(&self) -> Result<i64, Error> {
        self.local.generation().await
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
        self.local.frontier_page(facet, after, limit).await
    }
    async fn operation(
        &self,
        id: ContentHash,
    ) -> Result<Option<(ReceivedOperation, Admission)>, Error> {
        let replica = self.replica.clone();
        let (imported, native) = tokio::task::spawn_blocking(move || {
            Ok::<_, repo::thread_replication::Error>((
                replica.hybrid_import_bundle()?,
                replica.hybrid_native_bundle()?,
            ))
        })
        .await??;
        let bundle = match (imported, native) {
            (Some(mut b), None) => {
                self.device
                    .refresh_export_bundle(&self.session, &mut b)
                    .await
                    .map_err(store_error)?;
                PublicProof::from(b)
            }
            (None, Some(mut b)) => {
                self.device
                    .refresh_native_export_bundle(&self.session, &mut b)
                    .await
                    .map_err(store_error)?;
                PublicProof::from(b)
            }
            (None, None) => return self.local.operation(id).await,
            _ => return Err(Error::HostedTrustRequired),
        };
        let backend = self
            .device
            .hosted_backend(self.local.clone(), self.session.clone(), bundle)
            .map_err(store_error)?;
        backend.operation(id).await
    }
    async fn receive(&self, operation: ReceivedOperation) -> Result<Admission, Error> {
        let bundle = match (&operation.import_authority, &operation.native_authority) {
            (Some(b), None) => PublicProof::from(b.as_ref().clone()),
            (None, Some(b)) => PublicProof::from(b.as_ref().clone()),
            (None, None) => {
                let replica = self.replica.clone();
                let hosted = tokio::task::spawn_blocking(move || {
                    Ok::<_, repo::thread_replication::Error>(
                        replica.hybrid_import_bundle()?.is_some()
                            || replica.hybrid_native_bundle()?.is_some(),
                    )
                })
                .await??;
                if hosted {
                    return Err(Error::HostedTrustRequired);
                }
                return self.local.receive(operation).await;
            }
            _ => return Err(Error::HostedTrustRequired),
        };
        let backend = self
            .device
            .hosted_backend(self.local.clone(), self.session.clone(), bundle)
            .map_err(store_error)?;
        backend.receive(operation).await
    }
    async fn remember_peer_heads(
        &self,
        peer: [u8; 32],
        heads: Vec<(ThreadFacet, ContentHash)>,
    ) -> Result<(), Error> {
        self.local.remember_peer_heads(peer, heads).await
    }
    async fn record_peer_receipt(
        &self,
        peer: [u8; 32],
        id: ContentHash,
        admission: Admission,
    ) -> Result<(), Error> {
        self.local.record_peer_receipt(peer, id, admission).await
    }
    async fn settled_peer_heads(
        &self,
        peer: [u8; 32],
        facets: BTreeSet<ThreadFacet>,
        limit: usize,
    ) -> Result<Vec<(ContentHash, Admission)>, Error> {
        self.local.settled_peer_heads(peer, facets, limit).await
    }
    async fn needed_from_peer(
        &self,
        peer: [u8; 32],
        facets: BTreeSet<ThreadFacet>,
        limit: usize,
    ) -> Result<Vec<ContentHash>, Error> {
        self.local.needed_from_peer(peer, facets, limit).await
    }
}

fn store_error(error: impl std::fmt::Display) -> Error {
    Error::Store(repo::thread_replication::Error::Invalid(error.to_string()))
}
