// SPDX-License-Identifier: Apache-2.0
use api::heddle::api::v1alpha2::{
    AnnotationQuery, ContextRecord, ObserveCollaborationRequest, RecordRef, collaboration_event,
};
use objects::object::{
    Annotation, AnnotationRevision, AnnotationScope, AnnotationStatus,
    CollaborationAnchor as NativeAnchor, CollaborationRevision as NativeRevision, ContentHash,
    ContextRevision as NativeContextRevision, ContextTarget,
    thread_replication::ThreadOperationBody,
};
use wire::ProtocolError;

use super::HostedClient;

fn context_record_id(annotation_id: &str) -> String {
    annotation_id.trim_start_matches("ann-").to_string()
}

fn collect_context_records(events: Vec<collaboration_event::Payload>) -> Vec<ContextRecord> {
    events
        .into_iter()
        .filter_map(|change| match change {
            collaboration_event::Payload::Context(record) => Some(record),
            _ => None,
        })
        .collect()
}

fn native_context_operations(
    events: &[collaboration_event::Payload],
) -> Result<Vec<(ContentHash, NativeContextRevision)>, ProtocolError> {
    events
        .iter()
        .filter_map(|change| match change {
            collaboration_event::Payload::Operation(record) => Some(record),
            _ => None,
        })
        .map(|record| {
            let operation = thread_api::collaboration::verify(record)
                .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
            let id = operation
                .id()
                .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
            let ThreadOperationBody::Context(bytes) = operation.body else {
                return Err(ProtocolError::InvalidState(
                    "ObserveCollaboration returned a non-context operation for a context query"
                        .into(),
                ));
            };
            let context = NativeContextRevision::decode(&bytes)
                .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
            Ok((id, context))
        })
        .collect()
}

fn native_scope(anchor: &NativeAnchor) -> AnnotationScope {
    match anchor {
        NativeAnchor::Source { source } if source.start_line.is_some() => AnnotationScope::Lines(
            source.start_line.unwrap_or_default(),
            source.end_line.unwrap_or_default(),
        ),
        NativeAnchor::Source { source } if !source.symbol_id.is_empty() => {
            AnnotationScope::Symbol {
                name: source.symbol_id.clone(),
                resolved_lines: source.start_line.zip(source.end_line),
            }
        }
        NativeAnchor::Symbol { symbol, .. } => AnnotationScope::Symbol {
            name: symbol.clone(),
            resolved_lines: None,
        },
        _ => AnnotationScope::File,
    }
}

fn native_target(context: &NativeContextRevision) -> Option<ContextTarget> {
    match &context.anchor {
        NativeAnchor::Source { source } if !source.path.is_empty() => {
            ContextTarget::file(&source.path).ok()
        }
        NativeAnchor::Source { source } => match source.revision {
            NativeRevision::State { state_id } => Some(ContextTarget::state(state_id)),
            NativeRevision::GitCommit { .. } => None,
        },
        NativeAnchor::State { state_id } => Some(ContextTarget::state(*state_id)),
        NativeAnchor::Path { path, .. } | NativeAnchor::Symbol { path, .. } => {
            ContextTarget::file(path).ok()
        }
        NativeAnchor::Repository | NativeAnchor::Change { .. } => None,
    }
}

fn native_revision(
    operation_id: ContentHash,
    context: &NativeContextRevision,
) -> Result<AnnotationRevision, ProtocolError> {
    let provenance = context.provenance.as_ref().ok_or_else(|| {
        ProtocolError::InvalidState(format!(
            "context {} has no authored provenance; replication is incomplete",
            context.id
        ))
    })?;
    Ok(AnnotationRevision {
        revision_id: if provenance.revision_id.is_empty() {
            operation_id.to_string()
        } else {
            provenance.revision_id.clone()
        },
        kind: provenance.kind,
        content: context.content.clone(),
        tags: context
            .tags
            .iter()
            .filter_map(|tag| match tag {
                objects::object::AnnotationTag::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect(),
        attribution: provenance.attribution.clone(),
        created_at: context.occurred_at_ms.div_euclid(1000),
        source_hash: provenance.source_hash,
        created_at_state: provenance.created_at_state,
    })
}

fn native_annotation(
    operation_id: ContentHash,
    context: &NativeContextRevision,
) -> Result<Annotation, ProtocolError> {
    Ok(Annotation {
        annotation_id: context.id.to_string(),
        scope: native_scope(&context.anchor),
        status: AnnotationStatus::Active,
        revisions: vec![native_revision(operation_id, context)?],
        supersedes_annotation_id: context.supersedes.map(|id| id.to_string()),
        supersedes_rewrite_pct: None,
        visibility: objects::object::VisibilityTier::default(),
        resolved_from_discussion: None,
        anchor_status: objects::object::AnnotationAnchorStatus::default(),
    })
}
impl HostedClient {
    pub async fn list_context(
        &mut self,
        repo_path: &str,
        _ref: Option<&str>,
        prefix: Option<&str>,
        tag_filter: Option<&str>,
    ) -> Result<Vec<(Option<ContextTarget>, Annotation)>, ProtocolError> {
        let spool = self.resolve_spool_ref(repo_path).await?;
        // Presence of AnnotationQuery selects context revisions only, so
        // discussion/turn pages cannot starve the context list. History must
        // be included: without it, once-mode pages kind 0/1 and the query
        // skips those, leaving an empty context list.
        let events = self
            .observe_collaboration_events(ObserveCollaborationRequest {
                spool: Some(spool),
                include_history: true,
                include_operations: true,
                annotations: Some(AnnotationQuery::default()),
                ..Default::default()
            })
            .await?;
        let native = native_context_operations(&events)?;
        if !native.is_empty() {
            let mut latest = std::collections::BTreeMap::new();
            for (operation_id, context) in native {
                latest
                    .entry(context.id)
                    .and_modify(|current: &mut (ContentHash, NativeContextRevision)| {
                        if context.occurred_at_ms >= current.1.occurred_at_ms {
                            *current = (operation_id, context.clone());
                        }
                    })
                    .or_insert((operation_id, context));
            }
            let mut annotations = Vec::new();
            for (operation_id, context) in latest.into_values() {
                let annotation = native_annotation(operation_id, &context)?;
                if let Some(tag) = tag_filter
                    && !annotation
                        .revisions
                        .iter()
                        .flat_map(|revision| &revision.tags)
                        .any(|value| value == tag)
                {
                    continue;
                }
                let target = native_target(&context);
                if let Some(prefix) = prefix
                    && !matches!(&target, Some(ContextTarget::File { path }) if path.starts_with(prefix))
                {
                    continue;
                }
                annotations.push((target, annotation));
            }
            return Ok(annotations);
        }
        let projected = collect_context_records(events);
        if !projected.is_empty() {
            return Err(ProtocolError::InvalidState(
                "hosted context records omitted their signed operations; replication is incomplete"
                    .into(),
            ));
        }
        Ok(Vec::new())
    }

    pub async fn get_context_history(
        &mut self,
        repo_path: &str,
        _ref: Option<&str>,
        annotation_id: &str,
    ) -> Result<Vec<AnnotationRevision>, ProtocolError> {
        let spool = self.resolve_spool_ref(repo_path).await?;
        let id = context_record_id(annotation_id);
        let events = self
            .observe_collaboration_events(ObserveCollaborationRequest {
                spool: Some(spool.clone()),
                contexts: vec![RecordRef {
                    spool: Some(spool),
                    id: id.clone(),
                }],
                include_history: true,
                include_operations: true,
                ..Default::default()
            })
            .await?;
        let native = native_context_operations(&events)?;
        if !native.is_empty() {
            let mut revisions = native
                .into_iter()
                .filter(|(_, context)| context.id.to_string() == id)
                .map(|(operation, context)| native_revision(operation, &context))
                .collect::<Result<Vec<_>, _>>()?;
            revisions.sort_by_key(|revision| revision.created_at);
            revisions.reverse();
            return Ok(revisions);
        }
        if !collect_context_records(events).is_empty() {
            return Err(ProtocolError::InvalidState(
                "hosted context records omitted their signed operations; replication is incomplete"
                    .into(),
            ));
        }
        Ok(Vec::new())
    }
}
