use api::heddle::api::{
    v1alpha1::{
        AnnotatedFile, AnnotationScope, ContextAnnotation, ContextAnnotationKind, ContextRevision,
        ListContextSuggestionsResponse, ReviseContextResponse, SetContextResponse,
        StateContextEntry, SupersedeContextResponse, annotation_scope,
    },
    v2alpha1::{
        ObservationMode, ObserveCollaborationRequest, ObserveOptions, collaboration_anchor,
        collaboration_event, revision_ref,
    },
};
use objects::object::StateId;
use thread_api::rpc;
use wire::ProtocolError;

use super::HostedClient;

const PUT_CONTEXT: &str = "heddle.api.v2alpha1.CollaborationService/PutContext";

fn native_error(error: impl std::fmt::Display) -> ProtocolError {
    ProtocolError::InvalidState(error.to_string())
}

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

impl HostedClient {
    pub async fn list_context(
        &mut self,
        repo_path: &str,
        _ref: Option<&str>,
        prefix: Option<&str>,
        tag_filter: Option<&str>,
    ) -> Result<(Vec<AnnotatedFile>, Vec<StateContextEntry>), ProtocolError> {
        let spool = self.resolve_spool_ref(repo_path).await?;
        let remote = self.native().await.map_err(native_error)?;
        let mut observation = remote
            .observe::<rpc::CollaborationServiceObserveCollaboration>(
                ObserveCollaborationRequest {
                    spool: Some(spool),
                    include_history: true,
                    observe: Some(ObserveOptions {
                        mode: ObservationMode::Once as i32,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                None,
            )
            .await
            .map_err(native_error)?;
        let mut files: Vec<AnnotatedFile> = Vec::new();
        let mut states: Vec<StateContextEntry> = Vec::new();
        while let Some(batch) = observation.next_commit().await.map_err(native_error)? {
            for change in batch.changes {
                let collaboration_event::Payload::Context(record) = change else {
                    continue;
                };
                let _ = tag_filter;
                let tags: Vec<String> = Vec::new();
                let (path, _symbol, state) = match record.anchor.and_then(|anchor| anchor.target) {
                    Some(collaboration_anchor::Target::Source(source)) => {
                        let state = match source.revision.and_then(|revision| revision.revision) {
                            Some(revision_ref::Revision::State(id)) if id.value.len() == 32 => {
                                id.value.as_slice().try_into().ok().map(StateId::from_bytes)
                            }
                            _ => None,
                        };
                        (source.path, source.symbol_id, state)
                    }
                    _ => (String::new(), String::new(), None),
                };
                if let Some(prefix) = prefix
                    && !path.starts_with(prefix)
                {
                    continue;
                }
                let annotation = ContextAnnotation {
                    id: record
                        .r#ref
                        .as_ref()
                        .map(|value| value.id.clone())
                        .unwrap_or_default(),
                    content: record.content,
                    tags,
                    attribution: record.principal_id,
                    ..Default::default()
                };
                if path.is_empty() {
                    if let Some(state_id) = state {
                        states.push(StateContextEntry {
                            state_id: Some(api::heddle::api::v1alpha1::StateId {
                                value: state_id.as_bytes().to_vec(),
                            }),
                            annotations: vec![annotation],
                        });
                    }
                } else {
                    files.push(AnnotatedFile {
                        path,
                        annotations: vec![annotation],
                    });
                }
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
        r#ref: Option<&str>,
        annotation_id: &str,
    ) -> Result<Vec<ContextRevision>, ProtocolError> {
        let (files, states) = self.list_context(repo_path, r#ref, None, None).await?;
        let mut revisions = Vec::new();
        for file in files {
            for annotation in file.annotations {
                if annotation.id == annotation_id {
                    revisions.push(ContextRevision {
                        revision_id: annotation.id,
                        content: annotation.content,
                        tags: annotation.tags,
                        attribution: annotation.attribution,
                        ..Default::default()
                    });
                }
            }
        }
        for state in states {
            for annotation in state.annotations {
                if annotation.id == annotation_id {
                    revisions.push(ContextRevision {
                        revision_id: annotation.id,
                        content: annotation.content,
                        tags: annotation.tags,
                        attribution: annotation.attribution,
                        ..Default::default()
                    });
                }
            }
        }
        Ok(revisions)
    }

    pub async fn list_context_suggestions(
        &mut self,
        _repo_path: &str,
        _ref: Option<&str>,
        _limit: u32,
    ) -> Result<ListContextSuggestionsResponse, ProtocolError> {
        Ok(ListContextSuggestionsResponse::default())
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
        )
        .await?;
        Ok(SupersedeContextResponse {
            new_annotation_id: new_id,
            ..Default::default()
        })
    }
}
