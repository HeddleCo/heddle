// SPDX-License-Identifier: Apache-2.0
//! One flow-controlled, all-history native Git acceptance exchange. No intermediate source
//! receipt authorizes a Git ACK; the outer receipt proves the receiver's atomic head CAS.
use super::*;
use api::v2::client::{MessageWriter, Sender};

pub struct GitHistoryUpload<R> {
    pub opening: PublishContentClientFrame,
    pub originals: PublicationOriginals,
    pub artifacts: [R; 2],
}

/// Bind every exact signed original uploaded by the all-history command, independent of
/// batching and duplicate historical evidence. This verifies identities, not current rights.
pub fn git_originals_digest<'a>(
    originals: impl IntoIterator<Item = &'a PublicationOriginals>,
) -> Result<[u8; 32], Error> {
    let mut verified = Vec::new();
    let mut bytes = 0usize;
    for record in originals
        .into_iter()
        .flat_map(|o| &o.operations)
        .flat_map(|b| &b.operations)
    {
        bytes = bytes
            .checked_add(record.encoded_len())
            .ok_or(Error::Invalid("Git originals size overflow"))?;
        if bytes > 16 * 1024 * 1024 {
            return Err(Error::Invalid("Git originals metadata limit"));
        }
        let signed = crate::replication::decode_record(record.clone())
            .map_err(|_| Error::Invalid("invalid Git original encoding"))?;
        let id = signed
            .verify()
            .map_err(|_| Error::Invalid("invalid Git original signature"))?
            .id()
            .map_err(|_| Error::Invalid("invalid Git original identity"))?;
        verified.push((id, signed));
    }
    api::git_acceptance::originals_digest(verified.iter().map(|(id, signed)| {
        api::git_acceptance::OriginalDigestEntry {
            operation_id: id.as_bytes(),
            canonical: &signed.canonical,
            signature: &signed.signature,
        }
    }))
    .ok_or(Error::Invalid("invalid complete Git originals manifest"))
}

fn checkpoint(opening: &PublishContentClientFrame) -> TransferCheckpoint {
    let mut logical = opening.clone();
    if let Some(publish_content_client_frame::Body::Open(open)) = logical.body.as_mut() {
        open.checkpoint = None;
    }
    let digest = typed_digest("thread-source-transfer-v1", &logical.encode_to_vec());
    TransferCheckpoint {
        transfer_id: digest[..16].to_vec(),
        plan_digest: digest.to_vec(),
        resume_token: Vec::new(),
        committed_bytes: 0,
    }
}
fn opening(frame: &PublishContentClientFrame) -> Result<&PublishContentOpen, Error> {
    if let Some(publish_content_client_frame::Body::Open(open)) = &frame.body {
        Ok(open)
    } else {
        Err(Error::Invalid("Git publication Open required"))
    }
}
fn wrapped(
    operation: &str,
    index: Option<u32>,
    body: publish_content_client_frame::Body,
) -> Result<PublishContentClientFrame, Error> {
    use git_push_history_frame::Body as H;
    use publish_content_client_frame::Body as B;
    let body = if let Some(index) = index {
        let inner = match body {
            B::Pack(v) => H::Pack(v),
            B::Sidecar(v) => H::Sidecar(v),
            B::Finish(v) => H::Finish(v),
            B::Operations(v) => H::Operations(v),
            B::ThreadGenesis(v) => H::ThreadGenesis(v),
            _ => return Err(Error::Invalid("invalid nested Git history frame")),
        };
        B::GitHistory(GitPushHistoryFrame {
            index,
            body: Some(inner),
        })
    } else {
        body
    };
    Ok(PublishContentClientFrame {
        client_operation_id: operation.into(),
        body: Some(body),
    })
}
async fn send_revision<W: MessageWriter<Error = transport::Error>, R: AsyncRead + Unpin + Send>(
    sender: &mut Sender<W, PublishContentClientFrame>,
    operation: &str,
    index: Option<u32>,
    revision: &mut GitHistoryUpload<R>,
    frame_limit: usize,
) -> Result<(), Error> {
    let originals = batching::bounded_originals(
        &revision.originals,
        operation,
        frame_limit.saturating_sub(32),
    )?;
    originals.validate_bounds()?;
    for body in originals
        .geneses
        .iter()
        .cloned()
        .map(publish_content_client_frame::Body::ThreadGenesis)
        .chain(
            originals
                .operations
                .iter()
                .cloned()
                .map(publish_content_client_frame::Body::Operations),
        )
    {
        let frame = wrapped(operation, index, body)?;
        if frame.encoded_len() > frame_limit {
            return Err(Error::Invalid("Git history original frame limit"));
        }
        sender.send(&frame).await?;
    }
    let open = opening(&revision.opening)?;
    for (artifact, planned) in revision.artifacts.iter_mut().zip(&open.packs) {
        let mut offset = 0;
        let mut digest = blake3::Hasher::new();
        let mut buffer = vec![0; frame_limit / 2];
        while offset < planned.length {
            let length = (planned.length - offset).min(buffer.len() as u64) as usize;
            let read = artifact.read(&mut buffer[..length]).await?;
            if read == 0 {
                return Err(Error::Invalid("Git history artifact truncated"));
            }
            let data = &buffer[..read];
            digest.update(data);
            let frame = wrapped(
                operation,
                index,
                publish_content_client_frame::Body::Pack(PackChunk {
                    extent: Some(PackExtent {
                        pack: planned.pack.clone(),
                        kind: planned.kind,
                        offset,
                        length: read as u64,
                        extent_digest: Some(ObjectAddress {
                            algorithm: "blake3".into(),
                            digest: blake3::hash(data).as_bytes().to_vec(),
                        }),
                    }),
                    data: data.to_vec(),
                }),
            )?;
            if frame.encoded_len() > frame_limit {
                return Err(Error::Invalid("Git history chunk frame limit"));
            }
            sender.send(&frame).await?;
            offset += read as u64;
        }
        if artifact.read(&mut buffer[..1]).await? != 0
            || planned
                .pack
                .as_ref()
                .is_none_or(|p| p.digest != digest.finalize().as_bytes())
        {
            return Err(Error::Invalid(
                "Git history artifact differs from declared bytes",
            ));
        }
    }
    sender
        .send(&wrapped(
            operation,
            index,
            publish_content_client_frame::Body::Finish(PublishContentFinish {
                checkpoint: Some(checkpoint(&revision.opening)),
            }),
        )?)
        .await?;
    Ok(())
}
impl<T: RpcTransport<Error = transport::Error>> Remote<T> {
    pub async fn publish_git_content<R: AsyncRead + Unpin + Send>(
        &self,
        mut tip: GitHistoryUpload<R>,
        mut history: Vec<GitHistoryUpload<R>>,
        actor_account_id: &str,
    ) -> Result<PublicationReceipt, Error> {
        let open = opening(&tip.opening)?;
        let git = open
            .git_acceptance
            .as_ref()
            .ok_or(Error::Invalid("native Git acceptance required"))?;
        if git.expected_revision.len() != 32
            || git.expected_git_commit.len() != 20
            || git.accepted_git_commit.len() != 20
            || git.expected_git_commit == git.accepted_git_commit
            || git.gateway_publisher.len() != 32
            || git.transport_token.is_empty()
            || git.scope.is_none()
            || git.expected_native_generation.is_none_or(|n| n < 0)
            || git.originals_digest.len() != 32
            || git.history.len() != history.len()
            || history.len() >= 128
            || actor_account_id.is_empty()
            || open.destination != self.description.endpoint
            || open.sharing_policy_version.len() != 32
        {
            return Err(Error::Invalid(
                "exact bounded Git acceptance fence required",
            ));
        }
        if git_originals_digest(
            history
                .iter()
                .chain(std::iter::once(&tip))
                .map(|r| &r.originals),
        )?
        .as_slice()
            != git.originals_digest
        {
            return Err(Error::Invalid("Git command originals manifest differs"));
        }
        if open
            .protocol
            .as_ref()
            .is_none_or(|p| p.protocol_version != 2 || p.mandatory_features != [1, 2])
            || self
                .description
                .protocol
                .as_ref()
                .is_none_or(|p| p.protocol_version != 2 || p.mandatory_features != [1, 2])
        {
            return Err(Error::Invalid(
                "peer must explicitly support atomic Git acceptance",
            ));
        }
        let mut bytes = 0u64;
        let mut metadata = tip.opening.encoded_len();
        let mut states = std::collections::BTreeSet::new();
        let mut ids = std::collections::BTreeSet::new();
        for (index, revision) in history.iter().chain(std::iter::once(&tip)).enumerate() {
            let child = opening(&revision.opening)?;
            crate::hybrid::publish_open(child).map_err(Error::Invalid)?;
            revision.originals.validate_bounds()?;
            if child.thread != open.thread
                || child.source != open.source
                || child.destination != open.destination
                || child.sharing_policy_version != open.sharing_policy_version
                || !child.semantic_indexes.is_empty()
                || !states.insert(
                    child
                        .revision
                        .as_ref()
                        .ok_or(Error::Invalid("history revision absent"))?
                        .encode_to_vec(),
                )
                || !ids.insert(revision.opening.client_operation_id.clone())
            {
                return Err(Error::Invalid(
                    "history differs from exact Git publication scope",
                ));
            }
            if index < history.len()
                && (child.git_acceptance.is_some()
                    || git.history[index].client_operation_id
                        != revision.opening.client_operation_id
                    || git.history[index].open.as_ref() != Some(child))
            {
                return Err(Error::Invalid(
                    "history opening differs from Git acceptance inventory",
                ));
            }
            inventory_digest(&child.packs)?;
            for extent in &child.packs {
                bytes = bytes
                    .checked_add(extent.length)
                    .ok_or(Error::Invalid("Git history size overflow"))?;
            }
            for length in revision
                .originals
                .geneses
                .iter()
                .map(Message::encoded_len)
                .chain(
                    revision
                        .originals
                        .operations
                        .iter()
                        .map(Message::encoded_len),
                )
            {
                metadata = metadata
                    .checked_add(length)
                    .ok_or(Error::Invalid("Git history metadata overflow"))?;
            }
        }
        if bytes > 64 * 1024 * 1024
            || metadata > 16 * 1024 * 1024
            || tip.opening.encoded_len() > 512 * 1024
        {
            return Err(Error::Invalid("combined Git history budget exceeded"));
        }
        if open.protocol.is_some() {
            api::import_authority::require_hybrid_peer(self.description.protocol.as_ref())
                .map_err(|_| Error::Invalid("peer does not support Git publication protocol"))?;
        }
        let expected = checkpoint(&tip.opening);
        let (mut sender, mut responses) = self
            .api
            .exchange::<rpc::SyncServicePublishContent>(&tip.opening)
            .await?;
        let first = responses
            .next()
            .await?
            .ok_or(Error::Invalid("Git publication ended before admission"))?;
        let ready = match first.body {
            Some(publish_content_server_frame::Body::Receipt(receipt)) => {
                return validate_git_receipt(receipt, &tip, &history, actor_account_id);
            }
            Some(publish_content_server_frame::Body::Ready(ready)) => ready,
            _ => {
                return Err(Error::Invalid(
                    "Git publication Ready or replay receipt required",
                ));
            }
        };
        crate::hybrid::transfer_ready(&ready).map_err(Error::Invalid)?;
        crate::hybrid::negotiated(open.protocol.as_ref(), ready.protocol.as_ref())
            .map_err(Error::Invalid)?;
        if ready.endpoint != open.destination
            || ready.thread != open.thread
            || ready.current != open.revision
            || ready.checkpoint.as_ref() != Some(&expected)
        {
            return Err(Error::Invalid(
                "Git admission differs from exact publication plan",
            ));
        }
        let frame_limit = ready
            .budget
            .ok_or(Error::Invalid("Git frame budget required"))?
            .max_frame_bytes as usize;
        if !(1024..=512 * 1024).contains(&frame_limit) {
            return Err(Error::Invalid("Git frame budget unsupported"));
        }
        let operation = tip.opening.client_operation_id.clone();
        // Retain immutable receipt-validation inputs while mutable artifact readers are consumed.
        let validation_tip = (tip.opening.clone(), tip.originals.clone());
        let validation_history: Vec<_> = history
            .iter()
            .map(|r| (r.opening.clone(), r.originals.clone()))
            .collect();
        let upload = async {
            for (index, revision) in history.iter_mut().enumerate() {
                send_revision(
                    &mut sender,
                    &operation,
                    Some(index as u32),
                    revision,
                    frame_limit,
                )
                .await?;
            }
            send_revision(&mut sender, &operation, None, &mut tip, frame_limit).await?;
            sender.finish().await?;
            Ok::<_, Error>(())
        };
        let receive = async {
            while let Some(frame) = responses.next().await? {
                match frame.body {
                    Some(publish_content_server_frame::Body::Checkpoint(checkpoint))
                        if checkpoint == expected => {}
                    Some(publish_content_server_frame::Body::Receipt(receipt)) => {
                        return Ok(receipt);
                    }
                    _ => return Err(Error::Invalid("unexpected Git publication response")),
                }
            }
            Err(Error::Invalid(
                "Git publication ended without durable acceptance",
            ))
        };
        let (_, receipt) = tokio::try_join!(upload, receive)?;
        validate_git_receipt_frames(
            receipt,
            &validation_tip.0,
            &validation_history.iter().map(|r| &r.0).collect::<Vec<_>>(),
            actor_account_id,
        )
    }
}
fn validate_git_receipt<R>(
    receipt: PublicationReceipt,
    tip: &GitHistoryUpload<R>,
    history: &[GitHistoryUpload<R>],
    actor: &str,
) -> Result<PublicationReceipt, Error> {
    validate_git_receipt_frames(
        receipt,
        &tip.opening,
        &history.iter().map(|r| &r.opening).collect::<Vec<_>>(),
        actor,
    )
}
fn validate_git_receipt_frames(
    receipt: PublicationReceipt,
    tip: &PublishContentClientFrame,
    history: &[&PublishContentClientFrame],
    actor: &str,
) -> Result<PublicationReceipt, Error> {
    let open = opening(tip)?;
    let git = open
        .git_acceptance
        .as_ref()
        .ok_or(Error::Invalid("Git acceptance missing"))?;
    let receipt = validate_receipt(receipt, tip, &inventory_digest(&open.packs)?)?;
    let accepted = receipt.git_acceptance.as_ref().ok_or(Error::Invalid(
        "ordinary source receipt cannot acknowledge Git",
    ))?;
    if accepted.expected_revision != git.expected_revision
        || accepted.expected_git_commit != git.expected_git_commit
        || accepted.accepted_git_commit != git.accepted_git_commit
        || accepted.gateway_publisher != git.gateway_publisher
        || accepted.actor_account_id != actor
        || accepted.actor_credential_digest.len() != 32
        || api::git_acceptance::request_digest(tip, actor)
            .as_ref()
            .is_none_or(|expected| accepted.request_digest != expected.as_slice())
        || Some(accepted.previous_native_generation) != git.expected_native_generation
        || accepted.previous_native_generation < 0
        || accepted.native_generation <= accepted.previous_native_generation
        || accepted.blob_ids.len() > 20_000
        || accepted.blob_ids.iter().any(|id| id.len() != 32)
        || accepted.blob_ids.windows(2).any(|ids| ids[0] >= ids[1])
        || accepted.history.len() != history.len() + 1
        || accepted.retained_history.len() != accepted.history.len()
    {
        return Err(Error::Invalid(
            "receiver Git acceptance differs from authenticated command",
        ));
    }
    for (receipt, opening) in accepted
        .history
        .iter()
        .zip(history.iter().copied().chain(std::iter::once(tip)))
    {
        if receipt.git_acceptance.is_some() {
            return Err(Error::Invalid("nested Git acceptance receipt"));
        }
        validate_receipt(
            receipt.clone(),
            opening,
            &inventory_digest(&self::opening(opening)?.packs)?,
        )?;
    }
    // Upload receipts stay exact: the authenticated request digest above binds every
    // uploaded extent and original. A redundant historical repack may nevertheless
    // have a different physical inventory from the receiver's already retained source.
    // That separate, receiver-derived mapping is for later native retention checks.
    for (retained, uploaded) in accepted.retained_history.iter().zip(&accepted.history) {
        if retained.thread != uploaded.thread
            || retained.revision != uploaded.revision
            || retained.sharing_policy_version.len() != 32
            || retained
                .accepted_inventory
                .as_ref()
                .is_none_or(|address| address.algorithm != "blake3" || address.digest.len() != 32)
        {
            return Err(Error::Invalid(
                "retained Git history differs from accepted States",
            ));
        }
    }
    Ok(receipt)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (
        PublishContentClientFrame,
        PublishContentClientFrame,
        PublicationReceipt,
    ) {
        let (_, mut child, _) = super::super::tests::fixture(false);
        child.client_operation_id = "child-operation".into();
        let mut tip = child.clone();
        tip.client_operation_id = "tip-operation".into();
        let child_open = opening(&child).unwrap().clone();
        let publish_content_client_frame::Body::Open(open) = tip.body.as_mut().unwrap() else {
            unreachable!()
        };
        open.revision = Some(RevisionRef {
            revision: Some(revision_ref::Revision::State(
                api::heddle::api::common::StateId { value: vec![9; 32] },
            )),
            ..Default::default()
        });
        open.git_acceptance = Some(GitPushAcceptance {
            expected_revision: vec![1; 32],
            expected_git_commit: vec![2; 20],
            accepted_git_commit: vec![3; 20],
            gateway_publisher: vec![4; 32],
            expected_native_generation: Some(7),
            originals_digest: vec![5; 32],
            history: vec![GitPushHistoryRevision {
                client_operation_id: child.client_operation_id.clone(),
                open: Some(child_open),
            }],
            ..Default::default()
        });
        let ordinary = |frame: &PublishContentClientFrame| {
            let open = opening(frame).unwrap();
            PublicationReceipt {
                client_operation_id: frame.client_operation_id.clone(),
                destination: open.destination.clone(),
                thread: open.thread.clone(),
                revision: open.revision.clone(),
                sharing_policy_version: vec![6; 32],
                accepted_inventory: Some(ObjectAddress {
                    algorithm: "blake3".into(),
                    digest: inventory_digest(&open.packs).unwrap().to_vec(),
                }),
                outcome: Some(publication_receipt::Outcome::Accepted(Applied::default())),
                ..Default::default()
            }
        };
        let history = vec![ordinary(&child), ordinary(&tip)];
        let retained_history = history
            .iter()
            .map(|r| GitHistoryRevision {
                thread: r.thread.clone(),
                revision: r.revision.clone(),
                accepted_inventory: r.accepted_inventory.clone(),
                sharing_policy_version: r.sharing_policy_version.clone(),
            })
            .collect();
        let mut receipt = ordinary(&tip);
        let git = opening(&tip).unwrap().git_acceptance.as_ref().unwrap();
        receipt.git_acceptance = Some(GitPushAcceptanceReceipt {
            expected_revision: git.expected_revision.clone(),
            expected_git_commit: git.expected_git_commit.clone(),
            accepted_git_commit: git.accepted_git_commit.clone(),
            gateway_publisher: git.gateway_publisher.clone(),
            actor_account_id: "verified-actor".into(),
            actor_credential_digest: vec![8; 32],
            request_digest: api::git_acceptance::request_digest(&tip, "verified-actor")
                .unwrap()
                .to_vec(),
            native_generation: 8,
            previous_native_generation: 7,
            history,
            retained_history,
            ..Default::default()
        });
        (tip, child, receipt)
    }

    #[test]
    fn historical_repack_keeps_exact_upload_receipt_and_distinct_retained_inventory() {
        let (tip, child, mut receipt) = fixture();
        let retained = &mut receipt.git_acceptance.as_mut().unwrap().retained_history[0];
        retained.accepted_inventory.as_mut().unwrap().digest = vec![99; 32];
        retained.sharing_policy_version = vec![98; 32];
        validate_git_receipt_frames(receipt.clone(), &tip, &[&child], "verified-actor").unwrap();
        receipt.git_acceptance.as_mut().unwrap().history[0]
            .accepted_inventory
            .as_mut()
            .unwrap()
            .digest = vec![99; 32];
        assert!(validate_git_receipt_frames(receipt, &tip, &[&child], "verified-actor").is_err());
    }

    #[test]
    fn retained_history_cannot_replace_upload_fences_or_change_order_shape_and_scope() {
        let (tip, child, receipt) = fixture();
        for mutation in 0..9 {
            let mut changed = receipt.clone();
            let acceptance = changed.git_acceptance.as_mut().unwrap();
            match mutation {
                0 => {
                    acceptance.retained_history.pop();
                }
                1 => acceptance.retained_history.reverse(),
                2 => acceptance.retained_history[0].thread = None,
                3 => acceptance.retained_history[0].accepted_inventory = None,
                4 => {
                    acceptance.retained_history[0]
                        .accepted_inventory
                        .as_mut()
                        .unwrap()
                        .algorithm = "sha256".into()
                }
                5 => {
                    acceptance.retained_history[0]
                        .accepted_inventory
                        .as_mut()
                        .unwrap()
                        .digest
                        .pop();
                }
                6 => acceptance.retained_history[0]
                    .sharing_policy_version
                    .clear(),
                7 => acceptance.request_digest[0] ^= 1,
                8 => changed.accepted_inventory.as_mut().unwrap().digest[0] ^= 1,
                _ => unreachable!(),
            }
            assert!(
                validate_git_receipt_frames(changed, &tip, &[&child], "verified-actor").is_err(),
                "mutation {mutation}"
            );
        }
    }
}
