// SPDX-License-Identifier: Apache-2.0
use crate::legacy_v1::{
    AnnotatedFile, AnnotationScope, ContextAnnotation, ContextAnnotationKind, ContextRevision,
    StateContextEntry, SymbolScope, annotation_scope,
};
use api::heddle::api::v1alpha2::{
    AnnotationQuery, ContextRecord, ObserveCollaborationRequest, RecordRef, annotation_tag,
    collaboration_anchor, collaboration_event, revision_ref,
};
use objects::object::StateId;
use objects::object::{
    AnnotationKind as NativeKind, CollaborationAnchor as NativeAnchor,
    CollaborationRevision as NativeRevision, ContentHash, ContextRevision as NativeContextRevision,
    thread_replication::ThreadOperationBody,
};
use wire::ProtocolError;

use super::HostedClient;

fn context_record_id(annotation_id: &str) -> String {
    annotation_id.trim_start_matches("ann-").to_string()
}

fn context_ids_match(left: &str, right: &str) -> bool {
    left == right || context_record_id(left) == context_record_id(right)
}

fn context_revision_id(record: &ContextRecord) -> String {
    if !record.causal_id.is_empty() {
        hex::encode(&record.causal_id)
    } else if !record.version.is_empty() {
        hex::encode(&record.version)
    } else {
        record
            .r#ref
            .as_ref()
            .map(|value| value.id.clone())
            .unwrap_or_default()
    }
}

fn context_tag_texts(tags: &[api::heddle::api::v1alpha2::AnnotationTag]) -> Vec<String> {
    tags.iter()
        .filter_map(|tag| match tag.tag.as_ref() {
            Some(annotation_tag::Tag::Text(text)) => Some(text.clone()),
            Some(annotation_tag::Tag::Symbol(symbol)) => Some(symbol.name.clone()),
            _ => None,
        })
        .collect()
}

fn context_scope(symbol: &str) -> Option<AnnotationScope> {
    if symbol.is_empty() {
        Some(AnnotationScope {
            scope: Some(annotation_scope::Scope::File(true)),
        })
    } else {
        Some(AnnotationScope {
            scope: Some(annotation_scope::Scope::Symbol(SymbolScope {
                name: symbol.to_string(),
                ..Default::default()
            })),
        })
    }
}

fn context_annotation_id(record: &ContextRecord) -> String {
    record
        .r#ref
        .as_ref()
        .map(|value| value.id.clone())
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| context_revision_id(record))
}

fn annotation_from_record(record: &ContextRecord) -> ContextAnnotation {
    let symbol = match record
        .anchor
        .as_ref()
        .and_then(|anchor| anchor.target.as_ref())
    {
        Some(collaboration_anchor::Target::Source(source)) => source.symbol_id.as_str(),
        _ => "",
    };
    ContextAnnotation {
        id: context_annotation_id(record),
        content: record.content.clone(),
        tags: context_tag_texts(&record.tags),
        attribution: record.principal_id.clone(),
        revision_count: 1,
        scope: context_scope(symbol),
        status: if record.superseded {
            crate::legacy_v1::ContextAnnotationStatus::Superseded as i32
        } else {
            crate::legacy_v1::ContextAnnotationStatus::Active as i32
        },
        supersedes_annotation_id: record.supersedes.as_ref().map(|value| value.id.clone()),
        ..Default::default()
    }
}

fn revision_from_record(record: &ContextRecord) -> ContextRevision {
    ContextRevision {
        revision_id: context_revision_id(record),
        content: record.content.clone(),
        tags: context_tag_texts(&record.tags),
        attribution: record.principal_id.clone(),
        ..Default::default()
    }
}

fn source_location(record: &ContextRecord) -> (String, String, Option<StateId>) {
    match record
        .anchor
        .as_ref()
        .and_then(|anchor| anchor.target.as_ref())
    {
        Some(collaboration_anchor::Target::Source(source)) => {
            let state = match source
                .revision
                .as_ref()
                .and_then(|revision| revision.revision.as_ref())
            {
                Some(revision_ref::Revision::State(id)) if id.value.len() == 32 => {
                    id.value.as_slice().try_into().ok().map(StateId::from_bytes)
                }
                _ => None,
            };
            (source.path.clone(), source.symbol_id.clone(), state)
        }
        _ => (String::new(), String::new(), None),
    }
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

fn legacy_kind(kind: NativeKind) -> i32 {
    match kind {
        NativeKind::Constraint => ContextAnnotationKind::Constraint as i32,
        NativeKind::Invariant => ContextAnnotationKind::Invariant as i32,
        NativeKind::Rationale => ContextAnnotationKind::Rationale as i32,
    }
}

fn legacy_scope(anchor: &NativeAnchor) -> Option<AnnotationScope> {
    match anchor {
        NativeAnchor::Source { source } if source.start_line.is_some() => Some(AnnotationScope {
            scope: Some(annotation_scope::Scope::Lines(
                crate::legacy_v1::LineRange {
                    start: source.start_line.unwrap_or_default(),
                    end: source.end_line.unwrap_or_default(),
                },
            )),
        }),
        NativeAnchor::Source { source } if !source.symbol_id.is_empty() => Some(AnnotationScope {
            scope: Some(annotation_scope::Scope::Symbol(SymbolScope {
                name: source.symbol_id.clone(),
                resolved_start: source.start_line,
                resolved_end: source.end_line,
            })),
        }),
        NativeAnchor::Source { .. }
        | NativeAnchor::State { .. }
        | NativeAnchor::Path { .. }
        | NativeAnchor::Repository => Some(AnnotationScope {
            scope: Some(annotation_scope::Scope::File(true)),
        }),
        NativeAnchor::Symbol { symbol, .. } => Some(AnnotationScope {
            scope: Some(annotation_scope::Scope::Symbol(SymbolScope {
                name: symbol.clone(),
                ..Default::default()
            })),
        }),
        NativeAnchor::Change { .. } => None,
    }
}

fn native_location(context: &NativeContextRevision) -> (String, Option<StateId>) {
    match &context.anchor {
        NativeAnchor::Source { source } => {
            let state = match source.revision {
                NativeRevision::State { state_id } => Some(state_id),
                NativeRevision::GitCommit { .. } => None,
            };
            (source.path.clone(), state)
        }
        NativeAnchor::State { state_id }
        | NativeAnchor::Path { state_id, .. }
        | NativeAnchor::Symbol { state_id, .. } => {
            let path = match &context.anchor {
                NativeAnchor::Path { path, .. } | NativeAnchor::Symbol { path, .. } => path.clone(),
                _ => String::new(),
            };
            (path, Some(*state_id))
        }
        NativeAnchor::Repository | NativeAnchor::Change { .. } => (String::new(), None),
    }
}

fn native_revision(
    operation_id: ContentHash,
    context: &NativeContextRevision,
) -> Result<ContextRevision, ProtocolError> {
    let provenance = context.provenance.as_ref().ok_or_else(|| {
        ProtocolError::InvalidState(format!(
            "context {} has no authored provenance; replication is incomplete",
            context.id
        ))
    })?;
    Ok(ContextRevision {
        revision_id: if provenance.revision_id.is_empty() {
            operation_id.to_string()
        } else {
            provenance.revision_id.clone()
        },
        kind: legacy_kind(provenance.kind),
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
        created_at: Some(prost_types::Timestamp {
            seconds: context.occurred_at_ms.div_euclid(1000),
            nanos: (context.occurred_at_ms.rem_euclid(1000) * 1_000_000) as i32,
        }),
        source_hash: provenance.source_hash.map(|hash| hash.as_bytes().to_vec()),
        created_at_state: provenance.created_at_state.map(|state| {
            api::heddle::api::common::StateId {
                value: state.as_bytes().to_vec(),
            }
        }),
    })
}

fn native_annotation(
    operation_id: ContentHash,
    context: &NativeContextRevision,
) -> Result<ContextAnnotation, ProtocolError> {
    let revision = native_revision(operation_id, context)?;
    Ok(ContextAnnotation {
        id: context.id.to_string(),
        scope: legacy_scope(&context.anchor),
        content: revision.content,
        tags: revision.tags,
        attribution: revision.attribution,
        created_at: revision.created_at,
        source_hash: revision.source_hash,
        created_at_state: revision.created_at_state,
        status: crate::legacy_v1::ContextAnnotationStatus::Active as i32,
        kind: revision.kind,
        revision_count: 1,
        supersedes_annotation_id: context.supersedes.map(|id| id.to_string()),
        supersedes_rewrite_pct: None,
    })
}

impl HostedClient {
    pub async fn list_context(
        &mut self,
        repo_path: &str,
        _ref: Option<&str>,
        prefix: Option<&str>,
        tag_filter: Option<&str>,
    ) -> Result<(Vec<AnnotatedFile>, Vec<StateContextEntry>), ProtocolError> {
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
            let mut files = Vec::new();
            let mut states = Vec::new();
            for (operation_id, context) in latest.into_values() {
                let annotation = native_annotation(operation_id, &context)?;
                if let Some(tag) = tag_filter
                    && !annotation.tags.iter().any(|value| value == tag)
                {
                    continue;
                }
                let (path, state) = native_location(&context);
                if let Some(prefix) = prefix
                    && !path.starts_with(prefix)
                {
                    continue;
                }
                if path.is_empty() {
                    states.push(StateContextEntry {
                        state_id: state.map(|state_id| api::heddle::api::common::StateId {
                            value: state_id.as_bytes().to_vec(),
                        }),
                        annotations: vec![annotation],
                    });
                } else {
                    files.push(AnnotatedFile {
                        path,
                        annotations: vec![annotation],
                    });
                }
            }
            return Ok((files, states));
        }
        let projected = collect_context_records(events);
        if !projected.is_empty() {
            return Err(ProtocolError::InvalidState(
                "hosted context records omitted their signed operations; replication is incomplete"
                    .into(),
            ));
        }
        let mut files: Vec<AnnotatedFile> = Vec::new();
        let mut states: Vec<StateContextEntry> = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for record in projected {
            let annotation = annotation_from_record(&record);
            if annotation.id.is_empty() || !seen.insert(annotation.id.clone()) {
                continue;
            }
            if let Some(tag) = tag_filter
                && !annotation.tags.iter().any(|value| value == tag)
            {
                continue;
            }
            let (path, _symbol, state) = source_location(&record);
            if let Some(prefix) = prefix
                && !path.starts_with(prefix)
            {
                continue;
            }
            if path.is_empty() {
                // Keep repository-level / unparsed-revision records so pull can
                // attach them against the cloned tip instead of dropping them.
                states.push(StateContextEntry {
                    state_id: state.map(|state_id| api::heddle::api::common::StateId {
                        value: state_id.as_bytes().to_vec(),
                    }),
                    annotations: vec![annotation],
                });
            } else {
                files.push(AnnotatedFile {
                    path,
                    annotations: vec![annotation],
                });
            }
        }
        Ok((files, states))
    }

    pub async fn get_context_history(
        &mut self,
        repo_path: &str,
        _ref: Option<&str>,
        annotation_id: &str,
    ) -> Result<Vec<ContextRevision>, ProtocolError> {
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
            revisions.sort_by_key(|revision| {
                revision
                    .created_at
                    .as_ref()
                    .map(|time| (time.seconds, time.nanos))
            });
            revisions.reverse();
            return Ok(revisions);
        }
        let mut revisions: Vec<ContextRevision> = collect_context_records(events)
            .into_iter()
            .filter(|record| {
                record
                    .r#ref
                    .as_ref()
                    .is_some_and(|reference| context_ids_match(&reference.id, annotation_id))
            })
            .map(|record| revision_from_record(&record))
            .filter(|revision| !revision.revision_id.is_empty())
            .collect();
        // Oldest-first on the wire; GetContextHistory is newest-first.
        revisions.reverse();
        Ok(revisions)
    }
}

#[cfg(test)]
mod tests {
    use api::heddle::api::v1alpha2::AnnotationTag;

    use super::*;

    fn record(id: &str, causal: &[u8], content: &str) -> ContextRecord {
        ContextRecord {
            r#ref: Some(RecordRef {
                id: id.to_string(),
                ..Default::default()
            }),
            causal_id: causal.to_vec(),
            content: content.to_string(),
            tags: vec![AnnotationTag {
                tag: Some(annotation_tag::Tag::Text("review".into())),
            }],
            principal_id: "principal-1".into(),
            ..Default::default()
        }
    }

    #[test]
    fn observe_context_events_map_to_revision_ids() {
        let _process_env_guard = crate::test_process_env::shared_blocking();
        let causal = [7u8; 32];
        let mapped = revision_from_record(&record(
            "aaaaaaaa-bbbb-7ccc-dddd-eeeeeeeeeeee",
            &causal,
            "body",
        ));
        assert_eq!(mapped.revision_id, hex::encode(causal));
        assert_eq!(mapped.content, "body");
        assert_eq!(mapped.tags, vec!["review"]);
        assert_eq!(mapped.attribution, "principal-1");
    }

    #[test]
    fn context_ids_match_ann_prefix_and_raw_uuid() {
        let _process_env_guard = crate::test_process_env::shared_blocking();
        let id = "01999999-aaaa-7bbb-cccc-ddddeeeeffff";
        assert!(context_ids_match(id, id));
        assert!(context_ids_match(&format!("ann-{id}"), id));
        assert_eq!(context_record_id(&format!("ann-{id}")), id);
    }

    #[test]
    fn empty_context_ref_adopts_causal_id() {
        let _process_env_guard = crate::test_process_env::shared_blocking();
        let causal = [9u8; 32];
        let mapped = annotation_from_record(&ContextRecord {
            causal_id: causal.to_vec(),
            content: "body".into(),
            ..Default::default()
        });
        assert_eq!(mapped.id, hex::encode(causal));
        assert_eq!(mapped.content, "body");
    }
}
