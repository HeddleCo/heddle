//! Original-signed collaboration commands; delivery identity is independently checked.
use anyhow::{Context, Result, bail, ensure};
use api::heddle::api::v2alpha1::*;
use objects::object::{
    CollaborationOperationBodyV1 as Body, CollaborationOperationEnvelope,
    CollaborationResolution as Resolution, ContextRevision,
    thread_replication::ThreadOperationBody,
};
use prost::Message;
use repo::thread_replication::{
    ThreadReplica,
    collaboration::{Command, Precondition},
};

use super::{DeviceRpc, auth::Session, checkout};

impl DeviceRpc {
    pub(super) fn collaboration_command(
        &self,
        session: &Session,
        method: &str,
        body: &[u8],
    ) -> Result<Vec<u8>> {
        let descriptor = api::v2::method_descriptor(method).context("collaboration method")?;
        let id = uuid::Uuid::parse_str(
            descriptor
                .client_operation_id(body)?
                .context("command ID required")?,
        )?;
        let (signed, spool) = match method.rsplit('/').next().context("method")? {
            "OpenDiscussion" => {
                let r = OpenDiscussionRequest::decode(body)?;
                (r.signed_operation, r.spool)
            }
            "AppendTurn" => {
                let r = AppendDiscussionRequest::decode(body)?;
                (r.signed_operation, r.discussion.and_then(|v| v.spool))
            }
            "ResolveDiscussion" => {
                let r = ResolveDiscussionRequest::decode(body)?;
                (r.signed_operation, r.discussion.and_then(|v| v.spool))
            }
            "ReopenDiscussion" => {
                let r = ReopenDiscussionRequest::decode(body)?;
                (r.signed_operation, r.discussion.and_then(|v| v.spool))
            }
            "PutContext" => {
                let r = PutContextRequest::decode(body)?;
                (
                    r.signed_operation,
                    r.context.and_then(|v| v.r#ref).and_then(|v| v.spool),
                )
            }
            _ => bail!("unknown collaboration command"),
        };
        checkout::same_spool(session, spool.as_ref())?;
        let spool = spool.context("spool")?;
        let signed = signed.context("original signed collaboration operation required")?;
        let operation = thread_api::collaboration::verify(&signed)?;
        let (metadata, discussion, context) = match &operation.body {
            ThreadOperationBody::Discussion(bytes) => {
                let record = CollaborationOperationEnvelope::decode(bytes)?.operation;
                ensure!(
                    record.idempotency_key.as_str() == id.to_string(),
                    "signed command ID differs from request"
                );
                (
                    record.metadata.clone().context("portable actor required")?,
                    Some(record),
                    None,
                )
            }
            ThreadOperationBody::Context(bytes) => {
                let record = ContextRevision::decode(bytes)?;
                (record.metadata.clone(), None, Some(record))
            }
            _ => bail!("expected collaboration record"),
        };
        ensure!(
            metadata.scope.spool == session.spool.id
                && metadata.scope.thread == Some(operation.thread),
            "signed collaboration scope differs from request"
        );
        // An independently admitted courier cannot overwrite the original author.
        // Records without portable delegated proof require direct author delivery.
        ensure!(
            operation.publisher == session.publisher
                && metadata.actor.principal_id.to_string() == session.principal
                && metadata.actor.agent_id == session.agent_id,
            "original collaboration author differs from verified delivery authority"
        );
        let records = repo::thread_replication::collaboration::records(&operation)?;
        let record = records.first().context("record identity")?.clone();
        let reference = RecordRef {
            spool: Some(spool.clone()),
            id: record.id.clone(),
        };
        let precondition = match (
            method.rsplit('/').next(),
            discussion.as_ref(),
            context.as_ref(),
        ) {
            (Some("OpenDiscussion"), Some(record), None) => {
                let request = OpenDiscussionRequest::decode(body)?;
                let Body::Open {
                    title,
                    blocking,
                    anchor,
                    visibility,
                    turn,
                    ..
                } = &record.body
                else {
                    bail!("OpenDiscussion requires signed Open")
                };
                ensure!(
                    request.title == *title
                        && request.blocking == *blocking
                        && request.initial_body == turn.body
                        && thread_api::collaboration::visibility(
                            request.audience,
                            &request.audience_label
                        )? == *visibility
                        && thread_api::collaboration::anchor(
                            request.anchor.as_ref().context("anchor required")?,
                            &metadata.scope
                        )? == *anchor,
                    "OpenDiscussion fields differ from signed record"
                );
                Precondition::New
            }
            (Some("AppendTurn"), Some(record), None) => {
                let request = AppendDiscussionRequest::decode(body)?;
                let Body::AppendTurn { turn } = &record.body else {
                    bail!("AppendTurn requires signed turn")
                };
                let parents = request
                    .causal_parents
                    .iter()
                    .map(|id| {
                        Ok(objects::object::ContentHash::from_bytes(
                            id.as_slice().try_into()?,
                        ))
                    })
                    .collect::<Result<std::collections::BTreeSet<_>>>()?;
                let mentions = request
                    .mentions
                    .iter()
                    .map(thread_api::collaboration::mention)
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                ensure!(
                    request.discussion.as_ref() == Some(&reference)
                        && request.body == turn.body
                        && parents == operation.parents
                        && parents.len() == request.causal_parents.len()
                        && mentions == metadata.mentions,
                    "AppendTurn fields differ from signed record"
                );
                Precondition::Append
            }
            (Some("ResolveDiscussion"), Some(record), None) => {
                let request = ResolveDiscussionRequest::decode(body)?;
                let Body::Resolve { resolution } = &record.body else {
                    bail!("ResolveDiscussion requires signed resolution")
                };
                let matches = match (&request.resolution, resolution) {
                    (
                        Some(resolve_discussion_request::Resolution::DismissalReason(reason)),
                        Resolution::Dismissed { reason: signed },
                    ) => reason == signed,
                    (
                        Some(resolve_discussion_request::Resolution::ResolvedByEdit(revision)),
                        Resolution::AddressedByState { state_id },
                    ) => checkout::revision(session, Some(revision))? == *state_id,
                    (
                        Some(resolve_discussion_request::Resolution::ExtractContext(draft)),
                        Resolution::IntoContext { context },
                    ) => draft_matches(draft, context, &spool)?,
                    _ => false,
                };
                ensure!(
                    request.discussion.as_ref() == Some(&reference) && matches,
                    "resolution differs from signed record"
                );
                Precondition::Exact(request.expected_version)
            }
            (Some("ReopenDiscussion"), Some(record), None) => {
                let request = ReopenDiscussionRequest::decode(body)?;
                let Body::Reopen { reason } = &record.body else {
                    bail!("ReopenDiscussion requires signed reopen")
                };
                ensure!(
                    request.discussion.as_ref() == Some(&reference) && request.reason == *reason,
                    "reopen differs from signed record"
                );
                Precondition::Exact(request.expected_version)
            }
            (Some("PutContext"), None, Some(record)) => {
                let request = PutContextRequest::decode(body)?;
                ensure!(
                    draft_matches(
                        request.context.as_ref().context("context required")?,
                        record,
                        &spool
                    )?,
                    "context fields differ from signed record"
                );
                if request.expected_version.is_empty() {
                    Precondition::New
                } else {
                    Precondition::Exact(request.expected_version)
                }
            }
            _ => bail!("command differs from signed operation kind"),
        };
        let repository = repo::Repository::open(&session.spool.root)?;
        let replica = ThreadReplica::open(&session.spool.heddle_dir, operation.thread)?;
        let signed = thread_api::replication::decode_record(signed)?;
        let digest = blake3::hash(&[session.actor.as_bytes(), body].concat());
        Ok(replica.collaboration_command(
            &signed,
            repository.store(),
            Command {
                id,
                method,
                request_hash: *digest.as_bytes(),
                record,
                precondition,
            },
            |_| {
                session
                    .check_current(&self.home)
                    .map_err(|error| repo::thread_replication::Error::Invalid(error.to_string()))
            },
            |heads| {
                let mut receipt = self.receipt(&id.to_string());
                receipt.outcome = Some(mutation_receipt::Outcome::Applied(Applied {
                    resulting_versions: heads
                        .iter()
                        .map(|(record, heads)| ExpectedVersion {
                            resource: Some(EntityRef {
                                entity: Some(if record.kind == 1 {
                                    entity_ref::Entity::Discussion(RecordRef {
                                        spool: Some(spool.clone()),
                                        id: record.id.clone(),
                                    })
                                } else {
                                    entity_ref::Entity::Context(RecordRef {
                                        spool: Some(spool.clone()),
                                        id: record.id.clone(),
                                    })
                                }),
                            }),
                            version: heads.version.clone(),
                        })
                        .collect(),
                }));
                Ok(MutationResponse {
                    receipt: Some(receipt),
                }
                .encode_to_vec())
            },
        )?)
    }
}
fn draft_matches(draft: &ContextDraft, record: &ContextRevision, spool: &SpoolRef) -> Result<bool> {
    let reference = |id: String| RecordRef {
        spool: Some(spool.clone()),
        id,
    };
    Ok(
        draft.r#ref.as_ref() == Some(&reference(record.id.to_string()))
            && draft.content == record.content
            && thread_api::collaboration::annotation_tags(&draft.tags)? == record.tags
            && draft.supersedes == record.supersedes.map(|id| reference(id.to_string()))
            && draft.extracted_from == record.extracted_from.map(|id| reference(id.to_string()))
            && thread_api::collaboration::anchor(
                draft.anchor.as_ref().context("context anchor required")?,
                &record.metadata.scope,
            )? == record.anchor,
    )
}
