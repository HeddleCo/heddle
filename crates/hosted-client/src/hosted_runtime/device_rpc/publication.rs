//! Bounded owned-device source publication. Original proofs and complete packs
//! remain staged until validation; uploading never opts a Thread into Weft sync.
use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use api::{
    heddle::api::{v1alpha1::CallContext, v2alpha1::*},
    v2::client::{MessageReader, MessageWriter},
};
use iroh::endpoint::{RecvStream, SendStream};
use objects::{
    object::{ContentHash, OperationId, StateId, thread_replication::Admission},
    store::ObjectStore,
};
use prost::Message;
use repo::thread_replication::ThreadReplica;
use thread_api::{publication::PublicationOriginals, transport};
use tokio::io::AsyncWriteExt;

use super::{DeviceRpc, auth, checkout};
const FRAME: usize = 256 * 1024;
const BYTES: u64 = 256 * 1024 * 1024;
const METHOD: &str = "/heddle.api.v2alpha1.SyncService/PublishContent";
impl DeviceRpc {
    pub(super) async fn serve_publication_stream(
        &self,
        method: &str,
        context: &CallContext,
        peer: [u8; 32],
        send: SendStream,
        recv: RecvStream,
        budget: &mut super::super::hosted::claim_protocol::CallBudget,
    ) -> Result<()> {
        let descriptor = api::v2::method_descriptor(method).context("publication descriptor")?;
        let (mut writer, mut reader) =
            transport::accepted_stream(send, recv, FRAME, Duration::from_secs(30), descriptor)?;
        let body = reader
            .next()
            .await?
            .context("publication opening required")?;
        let request = PublishContentClientFrame::decode(body.as_slice())?;
        let Some(publish_content_client_frame::Body::Open(open)) = request.body.clone() else {
            bail!("publication requires Open")
        };
        let operation = request.client_operation_id.parse::<OperationId>()?;
        let reference = open.thread.as_ref().context("Thread required")?;
        let spool = reference.spool.as_ref().context("Spool required")?;
        let registered = repo::device_catalog::load(&self.home, uuid::Uuid::parse_str(&spool.id)?)?;
        let session = Arc::new(auth::authorize(
            &self.home, descriptor, context, &body, registered,
        )?);
        ensure!(
            open.destination == Some(self.endpoint()),
            "publication destination differs from receiver"
        );
        ensure!(
            open.source
                .as_ref()
                .is_some_and(|source| source.public_key == peer
                    && source.kind == EndpointKind::Device as i32),
            "publication source differs from owned transport peer"
        );
        ensure!(
            open.checkpoint.is_none(),
            "publication requires a fresh transfer; retry exact operation identity"
        );
        let thread = checkout::thread(&session, Some(reference))?;
        let replica = ThreadReplica::open(&session.spool.heddle_dir, thread)?;
        let revision = selected_revision(&session, &open)?;
        check_policy(&session, &replica, &open)?;
        let inventory = inventory(&open)?;
        let digest =
            ContentHash::compute_typed("thread-source-transfer-v1", &request.encode_to_vec());
        let checkpoint = TransferCheckpoint {
            transfer_id: digest.as_bytes()[..16].to_vec(),
            plan_digest: digest.as_bytes().to_vec(),
            ..Default::default()
        };
        let namespace = session.command_namespace()?;
        let command = repo::device_operations::Command {
            namespace: &namespace,
            id: operation,
            method: METHOD,
            request_hash: *blake3::hash(&body).as_bytes(),
        };
        if let Some(bytes) =
            repo::device_operations::replay_response(&session.spool.heddle_dir, &command)?
        {
            session.check_current(&self.home)?;
            writer
                .send(
                    PublishContentServerFrame {
                        body: Some(publish_content_server_frame::Body::Receipt(
                            PublicationReceipt::decode(bytes.as_slice())?,
                        )),
                    }
                    .encode_to_vec(),
                )
                .await?;
            writer.finish().await?;
            return Ok(());
        }
        let slot = self
            .content_work
            .clone()
            .try_acquire_owned()
            .context("source publication workers busy")?;
        budget.retain().map_err(anyhow::Error::msg)?;
        let scratch = tempfile::Builder::new()
            .prefix("device-publication-")
            .tempdir_in(&session.spool.heddle_dir)?;
        let mut files = [
            tokio::fs::File::create(scratch.path().join("source.pack")).await?,
            tokio::fs::File::create(scratch.path().join("source.idx")).await?,
        ];
        writer
            .send(
                PublishContentServerFrame {
                    body: Some(publish_content_server_frame::Body::Ready(TransferReady {
                        endpoint: Some(self.endpoint()),
                        thread: open.thread.clone(),
                        current: open.revision.clone(),
                        checkpoint: Some(checkpoint.clone()),
                        budget: Some(ReadBudget {
                            max_items: 12_048,
                            max_frame_bytes: FRAME as u32,
                            max_snapshot_bytes: BYTES,
                        }),
                        ..Default::default()
                    })),
                }
                .encode_to_vec(),
            )
            .await?;
        let mut originals = PublicationOriginals {
            geneses: Vec::new(),
            operations: Vec::new(),
        };
        let mut metadata = 0usize;
        let mut operation_count = 0usize;
        let mut offsets = [0u64; 2];
        let mut artifact = 0usize;
        loop {
            session.check_clock()?;
            let bytes = reader
                .next()
                .await?
                .context("publication ended without Finish")?;
            let frame = PublishContentClientFrame::decode(bytes.as_slice())?;
            ensure!(
                frame.client_operation_id == request.client_operation_id,
                "publication command identity changed"
            );
            match frame.body.context("publication body required")? {
                publish_content_client_frame::Body::ThreadGenesis(value) => {
                    ensure!(
                        offsets == [0, 0] && originals.geneses.len() < 128,
                        "original genesis must precede source artifacts within bounds"
                    );
                    metadata = metadata
                        .checked_add(value.encoded_len())
                        .context("original metadata overflow")?;
                    originals.geneses.push(value);
                }
                publish_content_client_frame::Body::Operations(value) => {
                    operation_count = operation_count
                        .checked_add(value.operations.len())
                        .context("source operation count overflow")?;
                    ensure!(
                        !value.operations.is_empty()
                            && value.authority_admissions.len() <= value.operations.len(),
                        "matched source proof batch required"
                    );
                    ensure!(
                        offsets == [0, 0] && operation_count <= 10_000,
                        "original operation must precede source artifacts within bounds"
                    );
                    metadata = metadata
                        .checked_add(value.encoded_len())
                        .context("original metadata overflow")?;
                    originals.operations.push(value);
                }
                publish_content_client_frame::Body::Pack(chunk) => {
                    ensure!(
                        !originals.geneses.is_empty() && !originals.operations.is_empty(),
                        "original source proofs required before artifacts"
                    );
                    while artifact < 2 && offsets[artifact] == open.packs[artifact].length {
                        artifact += 1;
                    }
                    ensure!(artifact < 2, "publication artifact overrun");
                    let extent = chunk.extent.context("chunk extent required")?;
                    let planned = &open.packs[artifact];
                    ensure!(
                        extent.kind == planned.kind
                            && extent.pack == planned.pack
                            && extent.offset == offsets[artifact]
                            && extent.length == chunk.data.len() as u64
                            && !chunk.data.is_empty()
                            && extent.length <= planned.length.saturating_sub(offsets[artifact]),
                        "publication chunk differs from exact ordered extent"
                    );
                    ensure!(
                        extent.extent_digest
                            == Some(ObjectAddress {
                                algorithm: "blake3".into(),
                                digest: blake3::hash(&chunk.data).as_bytes().to_vec()
                            }),
                        "publication chunk digest differs"
                    );
                    files[artifact].write_all(&chunk.data).await?;
                    offsets[artifact] += extent.length;
                }
                publish_content_client_frame::Body::Finish(finish) => {
                    ensure!(
                        finish.checkpoint == Some(checkpoint.clone())
                            && offsets == [open.packs[0].length, open.packs[1].length],
                        "Finish requires exact complete transfer plan"
                    );
                    ensure!(
                        reader.next().await?.is_none(),
                        "publication requires FIN after Finish"
                    );
                    break;
                }
                _ => bail!("source publication forbids sidecars or repeated opening"),
            }
            ensure!(
                metadata <= 16 * 1024 * 1024,
                "publication metadata exceeds 16 MiB"
            );
            writer
                .send(
                    PublishContentServerFrame {
                        body: Some(publish_content_server_frame::Body::Checkpoint(
                            checkpoint.clone(),
                        )),
                    }
                    .encode_to_vec(),
                )
                .await?;
        }
        for file in &mut files {
            file.sync_all().await?;
        }
        drop(files);
        let home = self.home.clone();
        let admitted = session.clone();
        let opening = open.clone();
        let command_id = request.client_operation_id.clone();
        let request_hash = command.request_hash;
        let receipt = tokio::task::spawn_blocking(move || {
            let _slot = slot;
            admitted.check_current(&home)?;
            let validated =
                thread_api::publication::validate_source_artifacts(scratch, &opening, originals)?;
            let repository = repo::Repository::open(&admitted.spool.root)?;
            let mut guards = Vec::new();
            for wrapper in validated.geneses() {
                let original = wrapper
                    .genesis
                    .as_ref()
                    .context("original genesis required")?;
                let genesis = objects::object::thread_replication::ThreadGenesis::decode(
                    &original.canonical_record,
                )?;
                let dependency = ThreadReplica::open(&admitted.spool.heddle_dir, genesis.id()?)?;
                ensure!(
                    dependency.genesis_record()?.genesis.as_ref() == Some(original),
                    "publication genesis differs from independently admitted original"
                );
                admitted.authorize_thread(&repository, &dependency)?;
                guards.push((dependency.clone(), dependency.generation()?));
            }
            check_policy(&admitted, &replica, &opening)?;
            for signed in validated.operations() {
                authorize_original(
                    &guards,
                    signed,
                    validated.authority_admissions(),
                    &admitted,
                    &home,
                )?;
            }
            let paths = validated.artifact_paths();
            repository
                .store()
                .install_pack_streaming(&paths[0], &paths[1])?;
            admitted.check_current(&home)?;
            for (dependency, _) in &guards {
                admitted.authorize_thread(&repository, dependency)?;
            }
            let policy_version = check_policy(&admitted, &replica, &opening)?;
            let receipt = PublicationReceipt {
                client_operation_id: command_id,
                destination: opening.destination.clone(),
                thread: opening.thread.clone(),
                revision: opening.revision.clone(),
                sharing_policy_version: policy_version.as_bytes().to_vec(),
                accepted_inventory: Some(ObjectAddress {
                    algorithm: "blake3".into(),
                    digest: inventory.as_bytes().to_vec(),
                }),
                outcome: Some(publication_receipt::Outcome::Accepted(Applied::default())),
            };
            let bytes = replica.publish_prepared_source(
                validated.operations(),
                validated.authority_admissions(),
                repository.store(),
                revision,
                &guards,
                repo::thread_replication::source_publication::Command {
                    namespace: &namespace,
                    id: operation,
                    method: METHOD,
                    request_hash,
                },
                |_, signed| {
                    authorize_original(
                        &guards,
                        signed,
                        validated.authority_admissions(),
                        &admitted,
                        &home,
                    )
                    .map_err(|error| repo::thread_replication::Error::Invalid(error.to_string()))
                },
                || Ok(receipt.encode_to_vec()),
            )?;
            Ok::<_, anyhow::Error>(PublicationReceipt::decode(bytes.as_slice())?)
        })
        .await??;
        session.check_current(&self.home)?;
        writer
            .send(
                PublishContentServerFrame {
                    body: Some(publish_content_server_frame::Body::Receipt(receipt)),
                }
                .encode_to_vec(),
            )
            .await?;
        writer.finish().await?;
        Ok(())
    }
}
fn selected_revision(session: &auth::Session, open: &PublishContentOpen) -> Result<StateId> {
    let revision = open
        .revision
        .as_ref()
        .context("exact source revision required")?;
    checkout::same_spool(session, revision.spool.as_ref())?;
    let Some(revision_ref::Revision::State(id)) = revision.revision.as_ref() else {
        bail!("exact native State required")
    };
    Ok(StateId::from_bytes(
        id.value
            .as_slice()
            .try_into()
            .context("State identity length")?,
    ))
}
fn check_policy(
    session: &auth::Session,
    replica: &ThreadReplica,
    open: &PublishContentOpen,
) -> Result<ContentHash> {
    use objects::object::thread_replication::metadata::{Property, property_version};
    let repository = repo::Repository::open(&session.spool.root)?;
    session.authorize_thread(&repository, replica)?;
    let heads = replica
        .metadata_frontier(&Property::Sharing)?
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    let version = property_version(replica.thread_id(), &Property::Sharing, &heads)?;
    ensure!(
        open.sharing_policy_version.is_empty() || open.sharing_policy_version == version.as_bytes(),
        "publication sharing policy changed"
    );
    Ok(version)
}

fn inventory(open: &PublishContentOpen) -> Result<ContentHash> {
    ensure!(
        open.packs.len() == 2
            && open.packs[0].kind == pack_extent::Kind::NativePack as i32
            && open.packs[1].kind == pack_extent::Kind::NativeIndex as i32,
        "ordered native artifacts required"
    );
    let mut bytes = Vec::new();
    let mut total = 0u64;
    for extent in &open.packs {
        let address = extent.pack.as_ref().context("artifact address required")?;
        total = total
            .checked_add(extent.length)
            .context("artifact size overflow")?;
        ensure!(
            address.algorithm == "blake3"
                && address.digest.len() == 32
                && extent.offset == 0
                && extent.length > 0
                && extent.extent_digest.as_ref() == Some(address)
                && total <= BYTES,
            "bounded complete native artifacts required"
        );
        extent.encode_length_delimited(&mut bytes)?;
    }
    Ok(ContentHash::compute_typed(
        "thread-source-inventory-v1",
        &bytes,
    ))
}
fn authorize_original(
    guards: &[(ThreadReplica, i64)],
    signed: &crypto::thread_operation::SignedOperation,
    admissions: &std::collections::BTreeMap<
        ContentHash,
        crypto::thread_authority_admission::SignedAuthorityAdmission,
    >,
    session: &auth::Session,
    home: &std::path::Path,
) -> Result<()> {
    let operation = signed.verify()?;
    let replica = &guards
        .iter()
        .find(|(replica, _)| replica.thread_id() == operation.thread)
        .context("original source Thread not independently authorized")?
        .0;
    if let Some((retained, Admission::Accepted)) = replica.operation(&operation.id()?)? {
        ensure!(retained == *signed, "retained source differs from original");
        return Ok(());
    }
    if let Some(receipt) = admissions.get(&operation.id()?) {
        replica.require_authority_admission(signed, receipt)?;
        return Ok(());
    }
    replica.verify_source_executor(&operation)?;
    let now = chrono::Utc::now().timestamp();
    let authority = repo::device_authority::load(home, now)?;
    authority.verify_publisher(&operation.publisher)?;
    if operation.source_author()?.is_some() {
        replica.verify_source_authority(
            &operation,
            &authority,
            &session.spool.capability_path,
            now,
        )?;
    }
    // Hosted execution is separately checked against receiver-owned executor pins
    // by publish_prepared_source before any source operation can commit.
    Ok(())
}
