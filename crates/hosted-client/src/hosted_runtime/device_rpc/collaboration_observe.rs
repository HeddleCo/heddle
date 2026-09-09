//! Shared push views over indexed collaboration records and original causal proofs.
use anyhow::{Context, Result, bail, ensure};
use api::heddle::api::v2alpha1::*;
use objects::object::{
    CollaborationAnchorStatus, CollaborationOperationBodyV1 as Body,
    CollaborationOperationEnvelope, CollaborationResolution, CollaborationScope, ContentHash,
    thread_replication::ThreadOperationBody,
};
use prost::Message;
use repo::thread_replication::{
    ThreadReplica,
    collaboration::{self, Position, Record},
};

use super::{DeviceRpc, auth::Session};
type Payload = collaboration_event::Payload;
impl super::stream::Event for CollaborationEvent {
    fn frame(&mut self, frame: StreamFrame) {
        self.frame = Some(frame);
    }
}
impl DeviceRpc {
    pub(super) async fn observe_collaboration(
        &self,
        session: &Session,
        body: &[u8],
        send: iroh::endpoint::SendStream,
    ) -> Result<()> {
        let request = ObserveCollaborationRequest::decode(body)?;
        super::checkout::same_spool(session, request.spool.as_ref())?;
        ensure!(
            request.anchors.len() <= 128
                && request.contexts.len() <= 128
                && request.discussions.len() <= 128,
            "collaboration selector count exceeds budget"
        );
        for reference in request.contexts.iter().chain(&request.discussions) {
            super::checkout::same_spool(session, reference.spool.as_ref())?;
        }
        let mut query = request.clone();
        query.observe = None;
        if let Some(page) = query.page.as_mut() {
            page.after_page.clear();
        }
        self.observe_view(
            session,
            "/heddle.api.v2alpha1.CollaborationService/ObserveCollaboration",
            &query.encode_to_vec(),
            request.observe.clone().unwrap_or_default(),
            send,
            |budget, binding| self.collaboration_snapshot(session, &request, budget, binding),
            || Ok(collaboration::generation(&session.spool.heddle_dir)?),
        )
        .await
    }
    pub(super) fn collaboration_snapshot(
        &self,
        session: &Session,
        request: &ObserveCollaborationRequest,
        budget: &ReadBudget,
        binding: &[u8],
    ) -> Result<(Vec<(String, CollaborationEvent)>, PageInfo, Vec<u8>)> {
        let generation = collaboration::generation(&session.spool.heddle_dir)?;
        let spool = request.spool.clone().context("spool required")?;
        let mut after = decode(
            request
                .page
                .as_ref()
                .map(|page| page.after_page.as_slice())
                .unwrap_or_default(),
            binding,
        )?;
        let size = request
            .page
            .as_ref()
            .map(|page| page.size)
            .filter(|size| *size > 0)
            .unwrap_or(50)
            .min(budget.max_items.saturating_sub(1)) as usize;
        ensure!(
            size > 0,
            "collaboration page needs payload and checkpoint capacity"
        );
        let query = request
            .annotations
            .as_ref()
            .map(thread_api::collaboration::annotation_query)
            .transpose()?;
        let mut events = Vec::new();
        let mut exhausted = false;
        let mut work = 0;
        while events.len() < size && work < 1024 {
            let rows = collaboration::candidate_page(
                &session.spool.heddle_dir,
                after.as_ref(),
                request.include_history,
                request.include_operations,
                1,
                budget.max_snapshot_bytes as usize,
            )?;
            let Some(row) = rows.into_iter().next() else {
                exhausted = true;
                break;
            };
            work += 1;
            after = Some(row.position.clone());
            let operation = row.signed.verify()?;
            ensure!(
                operation.id()? == row.position.operation
                    && operation.thread == row.position.thread,
                "collaboration candidate index mismatch"
            );
            let replica = ThreadReplica::open(&session.spool.heddle_dir, operation.thread)?;
            ensure!(
                replica.genesis()?.spool == spool.id,
                "collaboration belongs to another Spool"
            );
            let scope = CollaborationScope {
                spool: session.spool.id,
                thread: Some(operation.thread),
            };
            let reference = |id: String| RecordRef {
                spool: Some(spool.clone()),
                id,
            };
            let selected = |id: &str, is_context: bool| {
                let filters = if is_context {
                    &request.contexts
                } else {
                    &request.discussions
                };
                filters.is_empty() || filters.iter().any(|record| record.id == id)
            };
            let payload = match row.position.kind {
                0 | 1 => {
                    if query.is_some() {
                        continue;
                    }
                    let ThreadOperationBody::Discussion(bytes) = &operation.body else {
                        bail!("discussion index facet mismatch")
                    };
                    let record = CollaborationOperationEnvelope::decode(bytes)?.operation;
                    if !selected(&record.discussion_id.to_string(), false) {
                        continue;
                    }
                    let summary = replica.discussion_summary(
                        record.discussion_id,
                        budget.max_snapshot_bytes as usize,
                    )?;
                    let discussion = &summary.discussion;
                    let status = if discussion.anchor_status == CollaborationAnchorStatus::Orphaned
                    {
                        discussion_record::Status::Orphaned
                    } else if discussion.resolution.is_some() {
                        discussion_record::Status::Resolved
                    } else {
                        discussion_record::Status::Open
                    };
                    if !request.statuses.is_empty() && !request.statuses.contains(&(status as i32))
                    {
                        continue;
                    }
                    let mut resolved_anchor = discussion.anchor.clone();
                    let (_, anchor_scope) = super::collaboration_targets::project(
                        session,
                        &replica,
                        &scope,
                        &mut resolved_anchor,
                        &mut [],
                    )?;
                    let anchor =
                        thread_api::collaboration::anchor_ref(&resolved_anchor, &anchor_scope)?;
                    if !anchor_matches(request, &anchor) {
                        continue;
                    }
                    if row.position.kind == 0 {
                        let (audience, label) =
                            thread_api::collaboration::audience(&discussion.visibility)
                                .context("discussion audience unavailable")?;
                        Payload::Discussion(DiscussionRecord {
                            r#ref: Some(reference(record.discussion_id.to_string())),
                            version: summary.heads.version,
                            anchor: Some(anchor),
                            title: discussion.title.clone(),
                            status: status as i32,
                            blocking: discussion.blocking,
                            turn_count: summary.turn_count,
                            extracted_context: match &discussion.resolution {
                                Some(CollaborationResolution::IntoContext { context }) => {
                                    Some(reference(context.id.to_string()))
                                }
                                _ => None,
                            },
                            causal_heads: summary
                                .heads
                                .parents
                                .iter()
                                .map(|id| id.as_bytes().to_vec())
                                .collect(),
                            audience: audience as i32,
                            audience_label: label,
                        })
                    } else {
                        let turn = match &record.body {
                            Body::Open { turn, .. } | Body::AppendTurn { turn } => turn,
                            _ => bail!("native turn page requires individually signed turns"),
                        };
                        let metadata = record.metadata.context("portable turn actor required")?;
                        Payload::Turn(DiscussionTurn {
                            r#ref: Some(reference(format!(
                                "turn-{}-0",
                                hex::encode(row.position.operation.as_bytes())
                            ))),
                            discussion: Some(reference(record.discussion_id.to_string())),
                            body: turn.body.clone(),
                            principal_id: metadata.actor.principal_id.to_string(),
                            agent_id: metadata.actor.agent_id.unwrap_or_default(),
                            mentions: metadata
                                .mentions
                                .iter()
                                .map(thread_api::collaboration::mention_ref)
                                .collect(),
                            created_at: Some(prost_types::Timestamp {
                                seconds: record.occurred_at_ms.div_euclid(1000),
                                nanos: (record.occurred_at_ms.rem_euclid(1000) * 1_000_000) as i32,
                            }),
                            causal_id: row.position.operation.as_bytes().to_vec(),
                            causal_parents: operation
                                .parents
                                .iter()
                                .map(|id| id.as_bytes().to_vec())
                                .collect(),
                        })
                    }
                }
                2 => {
                    let mut record = operation
                        .context_revision()?
                        .context("context candidate missing context")?;
                    if !selected(&record.id.to_string(), true) {
                        continue;
                    }
                    let (anchor_coverage, anchor_scope) = super::collaboration_targets::project(
                        session,
                        &replica,
                        &scope,
                        &mut record.anchor,
                        &mut record.tags,
                    )?;
                    if query
                        .as_ref()
                        .is_some_and(|query| !query.matches(&record.tags))
                    {
                        continue;
                    }
                    let anchor =
                        thread_api::collaboration::anchor_ref(&record.anchor, &anchor_scope)?;
                    if !anchor_matches(request, &anchor) {
                        continue;
                    }
                    let heads = replica.collaboration_heads(&Record {
                        kind: 2,
                        id: record.id.to_string(),
                    })?;
                    Payload::Context(ContextRecord {
                        r#ref: Some(reference(record.id.to_string())),
                        version: heads.version,
                        anchor: Some(anchor),
                        content: record.content,
                        tags: record
                            .tags
                            .iter()
                            .map(thread_api::collaboration::annotation_tag_ref)
                            .collect(),
                        principal_id: record.metadata.actor.principal_id.to_string(),
                        agent_id: record.metadata.actor.agent_id.unwrap_or_default(),
                        supersedes: record.supersedes.map(|id| reference(id.to_string())),
                        extracted_from: record.extracted_from.map(|id| reference(id.to_string())),
                        anchor_coverage: anchor_coverage as i32,
                        superseded: replica.context_superseded(record.id)?,
                        causal_id: row.position.operation.as_bytes().to_vec(),
                        causal_parents: operation
                            .parents
                            .iter()
                            .map(|id| id.as_bytes().to_vec())
                            .collect(),
                        causal_heads: heads
                            .parents
                            .iter()
                            .map(|id| id.as_bytes().to_vec())
                            .collect(),
                    })
                }
                3 => {
                    if let Some(mut context) = operation.context_revision()? {
                        if !selected(&context.id.to_string(), true) {
                            continue;
                        }
                        let (_, selected_scope) = super::collaboration_targets::project(
                            session,
                            &replica,
                            &scope,
                            &mut context.anchor,
                            &mut context.tags,
                        )?;
                        if query
                            .as_ref()
                            .is_some_and(|query| !query.matches(&context.tags))
                        {
                            continue;
                        }
                        let anchor = thread_api::collaboration::anchor_ref(
                            &context.anchor,
                            &selected_scope,
                        )?;
                        if !anchor_matches(request, &anchor) {
                            continue;
                        }
                    } else if let ThreadOperationBody::Discussion(bytes) = &operation.body {
                        if query.is_some() {
                            continue;
                        }
                        let discussion = CollaborationOperationEnvelope::decode(bytes)?.operation;
                        if !selected(&discussion.discussion_id.to_string(), false) {
                            continue;
                        }
                        let summary = replica.discussion_summary(
                            discussion.discussion_id,
                            budget.max_snapshot_bytes as usize,
                        )?;
                        let status = if summary.discussion.anchor_status
                            == CollaborationAnchorStatus::Orphaned
                        {
                            discussion_record::Status::Orphaned
                        } else if summary.discussion.resolution.is_some() {
                            discussion_record::Status::Resolved
                        } else {
                            discussion_record::Status::Open
                        };
                        if !request.statuses.is_empty()
                            && !request.statuses.contains(&(status as i32))
                        {
                            continue;
                        }
                        let mut resolved_anchor = summary.discussion.anchor.clone();
                        let (_, selected_scope) = super::collaboration_targets::project(
                            session,
                            &replica,
                            &scope,
                            &mut resolved_anchor,
                            &mut [],
                        )?;
                        let anchor = thread_api::collaboration::anchor_ref(
                            &resolved_anchor,
                            &selected_scope,
                        )?;
                        if !anchor_matches(request, &anchor) {
                            continue;
                        }
                    }
                    Payload::Operation(super::thread::signed_record(&row.signed)?)
                }
                _ => bail!("unknown collaboration page kind"),
            };
            let event = CollaborationEvent {
                frame: None,
                payload: Some(payload),
            };
            ensure!(
                event.encoded_len() <= budget.max_frame_bytes as usize / 2,
                "collaboration record exceeds frame budget"
            );
            events.push((
                format!(
                    "{}:{}:{}",
                    row.position.kind,
                    hex::encode(row.position.thread.as_bytes()),
                    hex::encode(row.position.operation.as_bytes())
                ),
                event,
            ));
        }
        if collaboration::generation(&session.spool.heddle_dir)? != generation {
            return Err(super::stream::SnapshotChanged.into());
        }
        let page = PageInfo {
            exhausted,
            next_page: if exhausted {
                vec![]
            } else {
                after
                    .as_ref()
                    .map(|position| encode(position, binding))
                    .unwrap_or_default()
            },
            ..Default::default()
        };
        Ok((events, page, generation))
    }
}
fn anchor_matches(request: &ObserveCollaborationRequest, anchor: &CollaborationAnchor) -> bool {
    request.anchors.is_empty() || request.anchors.iter().any(|selected|selected==anchor || matches!((&selected.target,&anchor.target),(Some(collaboration_anchor::Target::Spool(a)),Some(collaboration_anchor::Target::Source(b))) if b.thread.as_ref().and_then(|thread|thread.spool.as_ref())==Some(a)) || matches!((&selected.target,&anchor.target),(Some(collaboration_anchor::Target::Spool(a)),Some(collaboration_anchor::Target::Thread(b))) if b.spool.as_ref()==Some(a)) || matches!((&selected.target,&anchor.target),(Some(collaboration_anchor::Target::Thread(a)),Some(collaboration_anchor::Target::Source(b))) if b.thread.as_ref()==Some(a)))
}
fn encode(position: &Position, binding: &[u8]) -> Vec<u8> {
    [
        binding,
        position.kind.to_be_bytes().as_slice(),
        position.thread.as_bytes(),
        position.operation.as_bytes(),
    ]
    .concat()
}
fn decode(bytes: &[u8], binding: &[u8]) -> Result<Option<Position>> {
    if bytes.is_empty() {
        return Ok(None);
    }
    ensure!(
        bytes.len() == binding.len() + 68 && bytes[..binding.len()] == *binding,
        "collaboration page token differs from query or authority"
    );
    let bytes = &bytes[binding.len()..];
    Ok(Some(Position {
        kind: i32::from_be_bytes(bytes[..4].try_into()?),
        thread: ContentHash::from_bytes(bytes[4..36].try_into()?),
        operation: ContentHash::from_bytes(bytes[36..].try_into()?),
    }))
}
