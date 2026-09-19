// SPDX-License-Identifier: Apache-2.0
use crate::legacy_v1::{
    AnnotatedFile, AnnotationScope, ContextAnnotation, ContextAnnotationKind, ContextRevision,
    ReviseContextResponse, SetContextResponse, StateContextEntry, SupersedeContextResponse,
    SymbolScope, annotation_scope,
};
use api::heddle::api::v1alpha2::{
    AnnotationQuery, ContextRecord, ObserveCollaborationRequest, RecordRef, annotation_tag,
    collaboration_anchor, collaboration_event, revision_ref,
};
use objects::object::StateId;
use wire::ProtocolError;

use super::HostedClient;

const PUT_CONTEXT: &str = "heddle.api.v1alpha2.CollaborationService/PutContext";

fn parse_state(value: Option<&str>) -> Option<StateId> {
    value.and_then(|value| StateId::parse(value).ok())
}

fn scope_path_symbol(scope: &AnnotationScope) -> (String, String) {
    match scope.scope.as_ref() {
        Some(annotation_scope::Scope::File(_)) => (String::new(), String::new()),
        Some(annotation_scope::Scope::Symbol(symbol)) => (String::new(), symbol.name.clone()),
        Some(annotation_scope::Scope::Lines(_)) => (String::new(), String::new()),
        None => (String::new(), String::new()),
    }
}

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
                annotations: Some(AnnotationQuery::default()),
                ..Default::default()
            })
            .await?;
        let mut files: Vec<AnnotatedFile> = Vec::new();
        let mut states: Vec<StateContextEntry> = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for record in collect_context_records(events) {
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

    #[allow(clippy::too_many_arguments)]
    pub async fn set_context(
        &mut self,
        repo_path: &str,
        path: &str,
        target_state_id: Option<&str>,
        scope: AnnotationScope,
        _kind: ContextAnnotationKind,
        tags: Vec<String>,
        content: &str,
        _agent_provider: Option<&str>,
        _agent_model: Option<&str>,
        client_operation_id: String,
        annotation_id: &str,
    ) -> Result<SetContextResponse, ProtocolError> {
        let (_, symbol) = scope_path_symbol(&scope);
        let response = self
            .put_context_record(
                repo_path,
                None,
                annotation_id,
                parse_state(target_state_id),
                path,
                &symbol,
                content,
                tags,
                client_operation_id,
                None,
            )
            .await?;
        let _ = (response, PUT_CONTEXT);
        Ok(SetContextResponse {
            annotation_id: annotation_id.to_string(),
            ..Default::default()
        })
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
                ..Default::default()
            })
            .await?;
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

    #[allow(clippy::too_many_arguments)]
    pub async fn revise_context(
        &mut self,
        repo_path: &str,
        annotation_id: &str,
        content: &str,
        tags: Vec<String>,
        _agent_provider: Option<&str>,
        _agent_model: Option<&str>,
        _kind: ContextAnnotationKind,
        client_operation_id: String,
    ) -> Result<ReviseContextResponse, ProtocolError> {
        self.put_context_record(
            repo_path,
            None,
            annotation_id,
            None,
            "",
            "",
            content,
            tags,
            client_operation_id,
            None,
        )
        .await?;
        Ok(ReviseContextResponse::default())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn supersede_context(
        &mut self,
        repo_path: &str,
        _annotation_id: &str,
        path: Option<&str>,
        target_state_id: Option<&str>,
        scope: AnnotationScope,
        tags: Vec<String>,
        content: &str,
        _agent_provider: Option<&str>,
        _agent_model: Option<&str>,
        _kind: ContextAnnotationKind,
        client_operation_id: String,
    ) -> Result<SupersedeContextResponse, ProtocolError> {
        let (_, symbol) = scope_path_symbol(&scope);
        let new_id = uuid::Uuid::now_v7().to_string();
        let superseded = uuid::Uuid::parse_str(_annotation_id.trim_start_matches("ann-"))
            .or_else(|_| uuid::Uuid::parse_str(_annotation_id))
            .ok();
        self.put_context_record(
            repo_path,
            None,
            &new_id,
            parse_state(target_state_id),
            path.unwrap_or(""),
            &symbol,
            content,
            tags,
            client_operation_id,
            superseded,
        )
        .await?;
        Ok(SupersedeContextResponse {
            new_annotation_id: new_id,
            ..Default::default()
        })
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
        let id = "01999999-aaaa-7bbb-cccc-ddddeeeeffff";
        assert!(context_ids_match(id, id));
        assert!(context_ids_match(&format!("ann-{id}"), id));
        assert_eq!(context_record_id(&format!("ann-{id}")), id);
    }

    #[test]
    fn empty_context_ref_adopts_causal_id() {
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
