//! A native relay retains the complete public bundle and rechecks durable trust
//! on receive and export. Current access remains the receiver's callback.
use crypto::thread_operation::SignedOperation;
use objects::{
    object::{Blob, State, StateId, Tree},
    store::{ExternalObjectSource, FsStore},
};
use repo::thread_replication::{
    delegated_import::AcceptedAuthority,
    hosted_trust::{Clock, HostedTrust},
};

use super::*;
use crate::hybrid::authority::{PublicEvidence, PublicProof};

pub struct HostedReplica<C, A> {
    local: LocalReplica<FsStore>,
    directory: PathBuf,
    trust: Arc<HostedTrust<C>>,
    authority: Arc<A>,
    export_bundle: Option<PublicProof>,
}
impl<C, A> Clone for HostedReplica<C, A> {
    fn clone(&self) -> Self {
        Self {
            local: self.local.clone(),
            directory: self.directory.clone(),
            trust: self.trust.clone(),
            authority: self.authority.clone(),
            export_bundle: self.export_bundle.clone(),
        }
    }
}
impl LocalReplica<FsStore> {
    /// The directory, selected root, owner history and current access callback
    /// come from the receiver, independently of an incoming authority bundle.
    pub fn with_hosted_authority<C: Clock, A: AcceptedAuthority>(
        self,
        directory: PathBuf,
        trust: Arc<HostedTrust<C>>,
        authority: Arc<A>,
    ) -> HostedReplica<C, A> {
        HostedReplica {
            local: self,
            directory,
            trust,
            authority,
            export_bundle: None,
        }
    }
}
struct ExistingObjects(Arc<FsStore>);
impl ExternalObjectSource for ExistingObjects {
    fn get_blob(&self, hash: &ContentHash) -> objects::store::Result<Option<Blob>> {
        self.0.get_blob(hash)
    }
    fn get_tree(&self, hash: &ContentHash) -> objects::store::Result<Option<Tree>> {
        self.0.get_tree(hash)
    }
    fn get_state(&self, id: &StateId) -> objects::store::Result<Option<State>> {
        self.0.get_state(id)
    }
    fn list_states(&self) -> objects::store::Result<Vec<StateId>> {
        self.0.list_states()
    }
}
impl<C: Clock + 'static, A: AcceptedAuthority + Send + Sync + 'static> HostedReplica<C, A> {
    /// Prepared receiver metadata remains bound to the selected authority.
    /// Overlay only the public set/proofs; every original byte must agree.
    pub fn with_export_bundle(
        mut self,
        bundle: crate::contract::ImportPublicProofBundleV1,
    ) -> Self {
        self.export_bundle = Some(PublicProof::from(bundle));
        self
    }
    pub fn with_native_export_bundle(
        mut self,
        bundle: crate::contract::NativePublicProofBundleV1,
    ) -> Self {
        self.export_bundle = Some(PublicProof::from(bundle));
        self
    }
    pub fn with_export_proof(mut self, bundle: PublicProof) -> Self {
        self.export_bundle = Some(bundle);
        self
    }
    fn export_bundle(&self, mut bundle: PublicProof) -> Result<PublicProof, Error> {
        if let Some(prepared) = &self.export_bundle {
            bundle
                .replace_receiver_metadata(prepared.clone())
                .map_err(|e| Error::Store(e.into()))?;
        }
        Ok(bundle)
    }
    pub fn publish_source(
        &self,
        source: crate::fetch::StagedSource,
        repository: &repo::Repository,
        now_seconds: i64,
        publication: crate::fetch::hosted::HostedPublication<'_>,
        response: impl FnOnce(
            &repo::thread_replication::hosted_trust::TrustTransaction<'_>,
        ) -> repo::thread_replication::Result<Vec<u8>>,
    ) -> Result<Vec<u8>, crate::fetch::Error> {
        if repository.heddle_dir().canonicalize()? != self.directory.canonicalize()? {
            return Err(api::hybrid_codec::Reject::Root.into());
        }
        source.publish_hosted(
            repository,
            &self.trust,
            self.authority.as_ref(),
            now_seconds,
            publication,
            response,
        )
    }

    /// Recheck retained originals before a source relay. Run this blocking
    /// operation on the caller's storage worker, with its current access gate.
    pub fn recheck_selected(
        &self,
        bundle: &crate::contract::ImportPublicProofBundleV1,
        records: &[crate::contract::SignedRecord],
    ) -> repo::thread_replication::Result<()> {
        self.recheck_proof(&PublicProof::from(bundle.clone()), records)
    }
    pub fn recheck_native_selected(
        &self,
        bundle: &crate::contract::NativePublicProofBundleV1,
        records: &[crate::contract::SignedRecord],
    ) -> repo::thread_replication::Result<()> {
        self.recheck_proof(&PublicProof::from(bundle.clone()), records)
    }
    pub fn recheck_proof(
        &self,
        bundle: &PublicProof,
        records: &[crate::contract::SignedRecord],
    ) -> repo::thread_replication::Result<()> {
        if self.local.objects.root().canonicalize()? != self.directory.canonicalize()? {
            return Err(api::hybrid_codec::Reject::Root.into());
        }
        let scratch = tempfile::tempdir()?;
        let mut staged = FsStore::new(scratch.path());
        staged.set_external_source(Arc::new(ExistingObjects(self.local.objects.clone())));
        staged.init()?;
        if matches!(bundle, PublicProof::Native(_)) {
            ThreadReplica::install_hybrid_native(
                &self.directory,
                &self.trust,
                &bundle.encode_to_vec(),
                records,
                self.authority.as_ref(),
                &staged,
                |artifacts| crate::fetch::hosted::publish_store(staged.root(), artifacts),
            )
        } else {
            ThreadReplica::install_hybrid_import(
                &self.directory,
                &self.trust,
                &bundle.encode_to_vec(),
                records,
                self.authority.as_ref(),
                &staged,
                |artifacts| crate::fetch::hosted::publish_store(staged.root(), artifacts),
            )
        }?;
        self.local.objects.reload_packs()?;
        Ok(())
    }
    async fn admit(
        &self,
        original: SignedOperation,
        bundle: PublicProof,
    ) -> Result<Admission, Error> {
        let receiver = self.clone();
        Ok(tokio::task::spawn_blocking(move || {
            let operation = original.verify()?;
            if operation.thread != receiver.local.replica.thread_id() {
                return Err(repo::thread_replication::Error::Hybrid(
                    api::hybrid_codec::Reject::Scope,
                ));
            }
            if receiver.local.objects.root().canonicalize()? != receiver.directory.canonicalize()? {
                return Err(repo::thread_replication::Error::Hybrid(
                    api::hybrid_codec::Reject::Root,
                ));
            }
            let id = operation.id()?;
            let mut pending = vec![original];
            let mut selected = BTreeSet::new();
            let mut records = Vec::new();
            while let Some(original) = pending.pop() {
                let operation = original.verify()?;
                if !selected.insert(operation.id()?) {
                    continue;
                }
                if selected.len() > 10_000 {
                    return Err(repo::thread_replication::Error::Hybrid(
                        api::hybrid_codec::Reject::Bounds,
                    ));
                }
                for parent in &operation.parents {
                    let (original, status) = receiver
                        .local
                        .replica
                        .operation(parent)?
                        .ok_or(api::hybrid_codec::Reject::Scope)?;
                    if !matches!(status, Admission::Accepted) {
                        return Err(repo::thread_replication::Error::Hybrid(
                            api::hybrid_codec::Reject::Scope,
                        ));
                    }
                    pending.push(original);
                }
                records.push(crate::contract::SignedRecord {
                    format: heddle_object_model::object::thread_replication::OPERATION_FORMAT
                        .into(),
                    canonical_record: original.canonical,
                    signatures: vec![crate::contract::RecordSignature {
                        public_key: operation.publisher.to_vec(),
                        signature: original.signature,
                    }],
                });
            }
            receiver.recheck_proof(&bundle, &records)?;
            receiver
                .local
                .replica
                .operation(&id)?
                .map(|(_, status)| status)
                .ok_or_else(|| api::hybrid_codec::Reject::Scope.into())
        })
        .await??)
    }
}
impl<C: Clock + 'static, A: AcceptedAuthority + Send + Sync + 'static> ReplicaStore
    for HostedReplica<C, A>
{
    type Error = Error;
    fn thread_id(&self) -> ContentHash {
        self.local.thread_id()
    }
    async fn generation(&self) -> Result<i64, Error> {
        self.local.generation().await
    }
    async fn sharing(&self, peer: [u8; 32]) -> Result<BTreeSet<ThreadFacet>, Error> {
        self.local.sharing(peer).await
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
        let stored = self
            .local
            .execute(move |replica, _| {
                Ok((
                    replica.operation_with_authority_admission(&id)?,
                    replica.hybrid_import_bundle()?,
                    replica.hybrid_native_bundle()?,
                ))
            })
            .await?;
        let (Some(stored), imported, native) = stored else {
            return Ok(None);
        };
        let bundle = match (imported, native) {
            (Some(b), None) => PublicProof::from(b),
            (None, Some(b)) => PublicProof::from(b),
            (None, None) => return Err(Error::HostedTrustRequired),
            _ => return Err(Error::HostedTrustRequired),
        };
        if stored.authority_admission.is_some() {
            return Err(Error::HostedTrustRequired);
        }
        let bundle = self.export_bundle(bundle)?;
        let status = self.admit(stored.original.clone(), bundle.clone()).await?;
        Ok(Some((
            ReceivedOperation {
                native_authority: bundle.native().cloned().map(Arc::new),
                original: stored.original,
                authority_admission: None,
                import_authority: bundle.imported().cloned().map(Arc::new),
            },
            status,
        )))
    }
    async fn receive(&self, received: ReceivedOperation) -> Result<Admission, Error> {
        if received.authority_admission.is_some() {
            return Err(Error::HostedTrustRequired);
        }
        let bundle = match (received.import_authority, received.native_authority) {
            (Some(b), None) => PublicProof::from(b.as_ref().clone()),
            (None, Some(b)) => PublicProof::from(b.as_ref().clone()),
            _ => return Err(Error::HostedTrustRequired),
        };
        let bundle = self.export_bundle(bundle)?;
        self.admit(received.original, bundle).await
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
