//! Exact private source transfer: bounded original causal proofs plus one selected
//! source tree. Dependency access is checked independently before its proof enters
//! the response; a target Thread never grants access to another Thread.
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use api::{
    heddle::api::{
        v1alpha1::{CallContext, CallFailureCode},
        v2alpha1::*,
    },
    v2::client::{MessageReader, MessageWriter},
};
use iroh::endpoint::{RecvStream, SendStream};
use objects::{
    object::{ContentHash, StateId, thread_replication::OPERATION_FORMAT},
    store::ObjectStore,
};
use prost::Message;
use repo::thread_replication::ThreadReplica;
use thread_api::{
    publication::{SourceBudget, VisibleSourcePack},
    transport,
};
use tokio::io::AsyncReadExt;

use super::{DeviceRpc, auth, checkout};

const FRAME: usize = 256 * 1024;
const BYTES: u64 = 256 * 1024 * 1024;
const RECORDS: usize = 10_000;
#[derive(Debug)]
struct SourceSelectionUnavailable;
impl std::fmt::Display for SourceSelectionUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("selected source unavailable")
    }
}
impl std::error::Error for SourceSelectionUnavailable {}
pub(super) struct Prepared {
    pub(super) pack: VisibleSourcePack,
    pub(super) geneses: BTreeMap<ContentHash, ThreadGenesisRecord>,
    pub(super) operations: Vec<repo::thread_replication::admission::StoredOperation>,
    guards: Vec<(ThreadReplica, i64)>,
}
impl DeviceRpc {
    pub(super) async fn serve_fetch_stream(
        &self,
        method: &str,
        context: &CallContext,
        _peer: [u8; 32],
        send: SendStream,
        recv: RecvStream,
        budget: &mut super::super::hosted::claim_protocol::CallBudget,
    ) -> Result<()> {
        let descriptor = api::v2::method_descriptor(method).context("Fetch descriptor missing")?;
        let (mut writer, mut reader) =
            transport::accepted_stream(send, recv, FRAME, Duration::from_secs(30), descriptor)?;
        let outcome = self
            .send_fetch_stream_inner(descriptor, context, &mut writer, &mut reader, budget)
            .await;
        if let Err(error) = outcome {
            let (code, message) = if error.is::<SourceSelectionUnavailable>() {
                (
                    CallFailureCode::NotFound,
                    "selected source unavailable".to_string(),
                )
            } else {
                (CallFailureCode::FailedPrecondition, error.to_string())
            };
            writer.fail(&super::failure(code, message)).await?;
        }
        Ok(())
    }
    async fn send_fetch_stream_inner(
        &self,
        descriptor: &'static api::v2::MethodDescriptor,
        context: &CallContext,
        mut writer: &mut transport::Writer,
        reader: &mut transport::Reader,
        budget: &mut super::super::hosted::claim_protocol::CallBudget,
    ) -> Result<()> {
        let body = reader.next().await?.context("Fetch opening required")?;
        let request = FetchClientFrame::decode(body.as_slice())?;
        let Some(fetch_client_frame::Body::Open(open)) = request.body else {
            bail!("Fetch requires Open")
        };
        let reference = open.thread.as_ref().context("Thread required")?;
        let spool = reference.spool.as_ref().context("Spool required")?;
        let registered = repo::device_catalog::load(&self.home, uuid::Uuid::parse_str(&spool.id)?)?;
        let session = Arc::new(auth::authorize(
            &self.home, descriptor, context, &body, registered,
        )?);
        budget.retain().map_err(anyhow::Error::msg)?;
        if reader.next().await?.is_some() {
            bail!("exact Fetch requires FIN after Open")
        }
        let selection = open
            .selection
            .as_ref()
            .context("source selection required")?;
        if open.checkpoint.is_some()
            || open.proof.is_some()
            || selection.facets != [SharedFacet::Source as i32]
            || !selection.exclude_revisions.is_empty()
            || selection.depth != 0
        {
            bail!("Fetch requires a fresh exact source selection")
        }
        let allow_partial = selection.allow_partial;
        let thread = checkout::thread(&session, Some(reference))?;
        let selected_revision = open.revision.as_ref().context("source revision required")?;
        checkout::same_spool(&session, selected_revision.spool.as_ref())?;
        let revision = match selected_revision.revision.as_ref() {
            Some(revision_ref::Revision::State(id)) => StateId::from_bytes(
                id.value
                    .as_slice()
                    .try_into()
                    .map_err(|_| SourceSelectionUnavailable)?,
            ),
            _ => return Err(SourceSelectionUnavailable.into()),
        };
        let feed = self.feed(&session)?;
        let mut changes = feed.changes.subscribe();
        let slot = self.content_work.clone().acquire_owned().await?;
        let home = self.home.clone();
        let admitted = session.clone();
        let mut prepared = tokio::task::spawn_blocking(move || {
            let _slot = slot;
            admitted.check_current(&home)?;
            let value = prepare(&admitted, thread, revision, allow_partial)
                .map_err(|_| SourceSelectionUnavailable)?;
            admitted.check_current(&home)?;
            Ok::<_, anyhow::Error>(value)
        })
        .await??;
        let mut ready = TransferReady {
            endpoint: Some(self.endpoint()),
            thread: open.thread,
            current: open.revision,
            thread_genesis: Some(
                prepared
                    .geneses
                    .remove(&thread)
                    .context("selected original genesis missing")?,
            ),
            packs: prepared.pack.artifacts().to_vec(),
            full_closure_available: prepared.pack.is_complete(),
            budget: Some(ReadBudget {
                max_items: RECORDS as u32 + 2048,
                max_frame_bytes: FRAME as u32,
                max_snapshot_bytes: BYTES,
            }),
            ..Default::default()
        };
        let checkpoint = TransferCheckpoint {
            transfer_id: uuid::Uuid::now_v7().as_bytes().to_vec(),
            plan_digest: blake3::hash(&ready.encode_to_vec()).as_bytes().to_vec(),
            ..Default::default()
        };
        ready.checkpoint = Some(checkpoint.clone());
        let mut charged = 0u64;
        send_frame(
            &self.home,
            &session,
            &mut writer,
            fetch_server_frame::Body::Ready(ready),
            &mut charged,
            &mut changes,
            &prepared.guards,
        )
        .await?;
        for record in prepared.geneses.into_values() {
            send_frame(
                &self.home,
                &session,
                &mut writer,
                fetch_server_frame::Body::ThreadGenesis(record),
                &mut charged,
                &mut changes,
                &prepared.guards,
            )
            .await?;
        }
        for stored in prepared.operations {
            let signed = stored.original;
            let operation = signed.verify()?;
            let record = SignedRecord {
                format: OPERATION_FORMAT.into(),
                canonical_record: signed.canonical,
                signatures: vec![RecordSignature {
                    public_key: operation.publisher.to_vec(),
                    signature: signed.signature,
                }],
            };
            send_frame(
                &self.home,
                &session,
                &mut writer,
                fetch_server_frame::Body::Operations(ReplicationOperations {
                    boundary_acceptances: thread_api::boundary_acceptance::authority_evidence(
                        stored.authority_admission.as_ref(),
                    )?,
                    operations: vec![record],
                    authority_admissions: stored
                        .authority_admission
                        .as_ref()
                        .map(thread_api::authority_admission::encode)
                        .transpose()?
                        .into_iter()
                        .collect(),
                }),
                &mut charged,
                &mut changes,
                &prepared.guards,
            )
            .await?;
        }
        let mut committed = 0u64;
        for (mut file, extent) in prepared
            .pack
            .open_artifacts()
            .await?
            .into_iter()
            .zip(prepared.pack.artifacts())
        {
            let mut offset = 0;
            while offset < extent.length {
                let length = (extent.length - offset).min((FRAME - 1024) as u64) as usize;
                let mut data = vec![0; length];
                file.read_exact(&mut data).await?;
                let chunk = PackChunk {
                    extent: Some(PackExtent {
                        pack: extent.pack.clone(),
                        kind: extent.kind,
                        offset,
                        length: length as u64,
                        extent_digest: Some(ObjectAddress {
                            algorithm: "blake3".into(),
                            digest: blake3::hash(&data).as_bytes().to_vec(),
                        }),
                    }),
                    data,
                };
                send_frame(
                    &self.home,
                    &session,
                    &mut writer,
                    fetch_server_frame::Body::Pack(chunk),
                    &mut charged,
                    &mut changes,
                    &prepared.guards,
                )
                .await?;
                offset += length as u64;
                committed += length as u64;
            }
        }
        send_frame(
            &self.home,
            &session,
            &mut writer,
            fetch_server_frame::Body::Complete(FetchComplete {
                revision: Some(RevisionRef {
                    spool: Some(SpoolRef {
                        id: session.spool.id.to_string(),
                    }),
                    revision: Some(revision_ref::Revision::State(
                        api::heddle::api::v1alpha1::StateId {
                            value: revision.as_bytes().to_vec(),
                        },
                    )),
                }),
                checkpoint: Some(TransferCheckpoint {
                    committed_bytes: committed,
                    ..checkpoint
                }),
                closure: if prepared.pack.is_complete() {
                    Coverage::Complete as i32
                } else {
                    Coverage::Partial as i32
                },
                missing: vec![],
            }),
            &mut charged,
            &mut changes,
            &prepared.guards,
        )
        .await?;
        writer.finish().await?;
        Ok(())
    }
}
async fn send_frame(
    home: &std::path::Path,
    session: &auth::Session,
    writer: &mut transport::Writer,
    body: fetch_server_frame::Body,
    charged: &mut u64,
    changes: &mut tokio::sync::watch::Receiver<u64>,
    guards: &[(ThreadReplica, i64)],
) -> Result<()> {
    if changes.has_changed()? {
        changes.borrow_and_update();
        for (replica, generation) in guards {
            if replica.generation()? != *generation {
                bail!("source transfer Thread changed; reopen exact selection")
            }
        }
    }
    session.check_current(home)?;
    let frame = FetchServerFrame { body: Some(body) }.encode_to_vec();
    *charged = charged
        .checked_add(frame.len() as u64)
        .context("Fetch byte overflow")?;
    if *charged > BYTES || frame.len() > FRAME {
        bail!("Fetch response budget exceeded")
    };
    writer.send(frame).await?;
    Ok(())
}
pub(super) fn prepare(
    session: &auth::Session,
    thread: ContentHash,
    revision: StateId,
    allow_partial: bool,
) -> Result<Prepared> {
    let repository = repo::Repository::open(&session.spool.root)?;
    let selected = ThreadReplica::open(&session.spool.heddle_dir, thread)?;
    session.authorize_thread(&repository, &selected)?;
    // Signed metadata is not possession of the named global CAS objects.
    // The exact selected Thread is already authorized. Its signed source
    // lineage, including the canonical seed, is checked below. A reverse
    // lookup cannot prove membership for a missing or withheld revision.
    let Some(redactions) = auth::source_content_visibility(
        &repository,
        &selected,
        uuid::Uuid::parse_str(&session.principal)?,
        session.agent_id.as_deref(),
        revision,
    )?
    else {
        bail!("selected source is unavailable to this audience")
    };
    let genesis = selected.genesis()?;
    let seed = objects::object::thread_replication::hosted_import::synthetic_initial_base()?;
    if revision == genesis.base && revision == seed.id() {
        let state = repository
            .store()
            .get_state(&revision)?
            .context("canonical initial source is unavailable")?;
        if state.encode_current_msgpack()? != seed.encode_current_msgpack()? {
            bail!("initial source differs from canonical empty seed")
        }
        let generation = selected.generation()?;
        let scratch = repository.heddle_dir().join("source-transfers");
        objects::fs_atomic::create_private_dir_all(&scratch)?;
        let pack = VisibleSourcePack::prepare(
            repository.store(),
            &state,
            &[],
            &redactions,
            &scratch,
            SourceBudget {
                max_objects: 100_000,
                max_decoded_bytes: BYTES,
            },
        )?;
        if !allow_partial && !pack.is_complete() {
            bail!("selected source has hidden entries; request partial source projection");
        }
        if selected.generation()? != generation {
            bail!("source Thread changed during preparation")
        }
        return Ok(Prepared {
            pack,
            geneses: BTreeMap::from([(thread, selected.genesis_record()?)]),
            operations: Vec::new(),
            guards: vec![(selected, generation)],
        });
    }
    let mut source_owner = selected.clone();
    let mut lineage = BTreeMap::new();
    for _ in 0..128 {
        let current = source_owner.genesis()?;
        if revision != current.base {
            break;
        }
        let parent = current
            .parent
            .context("fork base has no parent source proof")?;
        lineage.insert(source_owner.thread_id(), source_owner.clone());
        source_owner = ThreadReplica::open(&session.spool.heddle_dir, parent)?;
        session.authorize_thread(&repository, &source_owner)?;
        if source_owner.genesis()?.spool != current.spool {
            bail!("fork source parent crosses Spool")
        }
    }
    if revision == source_owner.genesis()?.base {
        bail!("source parent chain exceeds bound")
    }
    let state = source_owner
        .accepted_source_revision(revision)?
        .context("selected revision is not admitted by Thread")?;
    let ids = source_owner.source_operation_page(revision, None, 2)?;
    let [selected_operation] = ids.as_slice() else {
        bail!("select a uniquely admitted source operation")
    };
    let mut pending = BTreeSet::from([(source_owner.thread_id(), *selected_operation)]);
    let mut seen = BTreeSet::new();
    let mut emitted = BTreeSet::new();
    let mut geneses = BTreeMap::new();
    let mut proofs = Vec::new();
    let mut guards = Vec::new();
    for (id, child) in lineage {
        let generation = child.generation()?;
        geneses.insert(id, child.genesis_record()?);
        guards.push((child, generation));
    }
    let mut operations = Vec::new();
    let mut bytes = 0usize;
    while let Some((owner, id)) = pending.pop_first() {
        if !seen.insert((owner, id)) {
            continue;
        }
        if seen.len() + pending.len() > RECORDS {
            bail!("Fetch source ancestry exceeds bound")
        }
        let replica = ThreadReplica::open(&session.spool.heddle_dir, owner)?;
        let generation = replica.generation()?;
        session.authorize_thread(&repository, &replica)?;
        let signed_genesis = replica.signed_genesis()?;
        let genesis = signed_genesis.verify()?;
        if genesis.spool != session.spool.id.to_string() {
            bail!("source dependency crosses Spool")
        }
        if !geneses.contains_key(&owner) {
            if geneses.len() >= 128 {
                bail!("Fetch dependency Thread bound exceeded")
            }
            let wrapper = replica.genesis_record()?;
            bytes += wrapper.encoded_len();
            geneses.insert(owner, wrapper);
            guards.push((replica.clone(), generation));
        }
        let remaining = RECORDS.saturating_sub(operations.len());
        for stored in
            replica.source_ancestry(id, remaining, (16 * 1024 * 1024usize).saturating_sub(bytes))?
        {
            let signed = &stored.original;
            let operation = signed.verify()?;
            let operation_id = operation.id()?;
            if !emitted.insert((owner, operation_id)) {
                continue;
            }
            if operation.thread != owner || operation.source_state()?.is_none() {
                bail!("source proof identity differs")
            }
            seen.insert((owner, operation_id));
            bytes += signed.canonical.len()
                + signed.signature.len()
                + stored.authority_admission.as_ref().map_or(0, |receipt| {
                    receipt.canonical.len() + receipt.signature.len()
                })
                + 128;
            if bytes > 16 * 1024 * 1024 || operations.len() >= RECORDS {
                bail!("source proof metadata budget exceeded")
            }
            if let Some(receipt) = operation.local_integration()?
                && !seen.contains(&(receipt.source_thread, receipt.source_operation))
            {
                pending.insert((receipt.source_thread, receipt.source_operation));
            }
            if let Some(receipt) = operation.integration()?
                && !seen.contains(&(receipt.source_thread, receipt.source_operation))
            {
                pending.insert((receipt.source_thread, receipt.source_operation));
            }
            if let Some(proof) = operation.reference_proof(&genesis)? {
                proofs.push(proof)
            }
            operations.push(stored);
        }
    }

    let scratch = repository.heddle_dir().join("source-transfers");
    objects::fs_atomic::create_private_dir_all(&scratch)?;
    let pack = VisibleSourcePack::prepare(
        repository.store(),
        &state,
        &proofs,
        &redactions,
        &scratch,
        SourceBudget {
            max_objects: 100_000,
            max_decoded_bytes: BYTES,
        },
    )?;
    if !allow_partial && !pack.is_complete() {
        bail!("selected source has hidden entries; request partial source projection");
    }
    for (replica, generation) in &guards {
        if replica.generation()? != *generation {
            bail!("source Thread changed during preparation")
        }
    }
    Ok(Prepared {
        pack,
        geneses,
        operations,
        guards,
    })
}
