// SPDX-License-Identifier: Apache-2.0
//! Local adapter from the pre-0.23 discussion RPCs to collaboration V1.

use std::time::SystemTime;

use grpc::heddle::v1::{
    AppendTurnRequest, Discussion as ProtoDiscussion,
    DiscussionResolution as ProtoDiscussionResolution, DiscussionTurn as ProtoDiscussionTurn,
    GetDiscussionRequest, ListDiscussionsByStateRequest, ListDiscussionsBySymbolRequest,
    ListDiscussionsResponse, OpenDiscussionRequest, PathSymbolRef, ResolveDiscussionRequest,
    discussion_service_server::DiscussionService,
};
use objects::object::{
    CollabOpId, CollaborationAnchor, CollaborationIdempotencyKey, CollaborationOperationBodyV1,
    CollaborationOperationEnvelope, CollaborationResolution, DiscussionRecordId, DiscussionTurnV1,
    MaterializedDiscussion, OperationId, StateId, VisibilityTier,
};
use prost::Message;
use repo::{CollaborationStore, Repository, migrate_legacy_discussions_once};
use tonic::{Request, Response, Status};

use super::{GrpcLocalService, to_status, with_idempotency};

#[derive(Clone)]
pub struct LocalDiscussionService {
    inner: GrpcLocalService,
}

impl LocalDiscussionService {
    pub fn new(inner: GrpcLocalService) -> Self {
        Self { inner }
    }
}

fn open_store(repository: &Repository) -> Result<CollaborationStore, Status> {
    let store = CollaborationStore::open(repository.heddle_dir()).map_err(to_status)?;
    migrate_legacy_discussions_once(
        repository,
        &store,
        repository.get_attribution().map_err(to_status)?,
    )
    .map_err(to_status)?;
    Ok(store)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn idempotency_key(value: &str) -> Result<CollaborationIdempotencyKey, Status> {
    CollaborationIdempotencyKey::new(if value.is_empty() {
        OperationId::new().to_string()
    } else {
        value.to_string()
    })
    .map_err(Status::invalid_argument)
}

fn parse_discussion_id(value: &str) -> Result<DiscussionRecordId, Status> {
    value.parse().map_err(|error| {
        Status::invalid_argument(format!("invalid discussion id {value:?}: {error}"))
    })
}

fn parse_visibility(value: &str, default: VisibilityTier) -> Result<VisibilityTier, Status> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(default);
    }
    match value {
        "public" => Ok(VisibilityTier::Public),
        "internal" => Ok(VisibilityTier::Internal),
        _ if value.starts_with("team:") => labelled_visibility(value, "team:", |label| {
            VisibilityTier::TeamScoped { team_id: label }
        }),
        _ if value.starts_with("restricted:") => {
            labelled_visibility(value, "restricted:", |label| VisibilityTier::Restricted {
                scope_label: label,
            })
        }
        _ if value.starts_with("private:") => labelled_visibility(value, "private:", |label| {
            VisibilityTier::Private { scope_label: label }
        }),
        _ => Err(Status::invalid_argument(format!(
            "unsupported discussion visibility {value:?}"
        ))),
    }
}

fn labelled_visibility(
    value: &str,
    prefix: &str,
    build: impl FnOnce(String) -> VisibilityTier,
) -> Result<VisibilityTier, Status> {
    let label = value.trim_start_matches(prefix).trim();
    if label.is_empty() {
        return Err(Status::invalid_argument(format!(
            "discussion visibility {prefix}<label> requires a label"
        )));
    }
    Ok(build(label.to_string()))
}

fn visibility_token(value: &VisibilityTier) -> String {
    match value {
        VisibilityTier::Public => "public".to_string(),
        VisibilityTier::Internal => "internal".to_string(),
        VisibilityTier::TeamScoped { team_id } => format!("team:{team_id}"),
        VisibilityTier::Restricted { scope_label } => format!("restricted:{scope_label}"),
        VisibilityTier::Private { scope_label } => format!("private:{scope_label}"),
    }
}

fn anchor_state(anchor: &CollaborationAnchor) -> Option<StateId> {
    match anchor {
        CollaborationAnchor::State { state_id }
        | CollaborationAnchor::Path { state_id, .. }
        | CollaborationAnchor::Symbol { state_id, .. } => Some(*state_id),
        CollaborationAnchor::Repository | CollaborationAnchor::Change { .. } => None,
    }
}

fn symbol_anchor(anchor: &CollaborationAnchor) -> Option<(&str, &str)> {
    match anchor {
        CollaborationAnchor::Symbol { path, symbol, .. } => Some((path, symbol)),
        _ => None,
    }
}

fn status_matches(discussion: &MaterializedDiscussion, status: &str) -> bool {
    match status {
        "open" => discussion.resolution.is_none() && discussion.conflict_operations.is_empty(),
        "resolved" => discussion.resolution.is_some() && discussion.conflict_operations.is_empty(),
        "conflicted" => !discussion.conflict_operations.is_empty(),
        "all" | "" => true,
        _ => false,
    }
}

fn materialized_to_proto(
    store: &CollaborationStore,
    discussion: &MaterializedDiscussion,
) -> Result<ProtoDiscussion, Status> {
    if !discussion.conflict_operations.is_empty() {
        return Err(Status::failed_precondition(format!(
            "discussion {} has a resolution conflict; inspect it with the native CLI",
            discussion.discussion_id
        )));
    }
    let (file, symbol) = symbol_anchor(&discussion.anchor).unwrap_or(("", ""));
    let state_id = anchor_state(&discussion.anchor);
    let mut turns = Vec::with_capacity(discussion.turns.len());
    let mut opened_at_ms = 0;
    for (operation_id, turn) in &discussion.turns {
        let decoded = store
            .read_operation(operation_id)
            .map_err(to_status)?
            .ok_or_else(|| Status::internal(format!("missing operation {operation_id}")))?;
        if opened_at_ms == 0 {
            opened_at_ms = decoded.operation.occurred_at_ms;
        }
        turns.push(ProtoDiscussionTurn {
            author_name: decoded.operation.author.principal.name,
            author_email: decoded.operation.author.principal.email,
            body: turn.body.clone(),
            posted_at: Some(prost_types::Timestamp {
                seconds: decoded.operation.occurred_at_ms / 1000,
                nanos: ((decoded.operation.occurred_at_ms % 1000) * 1_000_000) as i32,
            }),
        });
    }
    Ok(ProtoDiscussion {
        id: discussion.discussion_id.to_string(),
        anchor: Some(PathSymbolRef {
            file: file.to_string(),
            symbol: symbol.to_string(),
        }),
        opened_against_state: state_id
            .map(|value| value.as_bytes().to_vec())
            .unwrap_or_default(),
        opened_at: Some(prost_types::Timestamp {
            seconds: opened_at_ms / 1000,
            nanos: ((opened_at_ms % 1000) * 1_000_000) as i32,
        }),
        thread_ref: String::new(),
        turns,
        resolution: Some(resolution_to_proto(discussion.resolution.as_ref())?),
        body_changed_since_open: false,
        orphaned: false,
        visibility: visibility_token(&discussion.visibility),
        resolved_annotation_id: match &discussion.resolution {
            Some(CollaborationResolution::Annotation { annotation_id }) => annotation_id.clone(),
            _ => String::new(),
        },
    })
}

fn resolution_to_proto(
    resolution: Option<&CollaborationResolution>,
) -> Result<ProtoDiscussionResolution, Status> {
    use grpc::heddle::v1::discussion_resolution::{
        Dismissed, Open, ResolvedByEdit, ResolvedIntoAnnotation, State,
    };
    let state = match resolution {
        None => State::Open(Open {}),
        Some(CollaborationResolution::AddressedByState { state_id }) => {
            State::ByEdit(ResolvedByEdit {
                state_id: state_id.as_bytes().to_vec(),
            })
        }
        Some(CollaborationResolution::Dismissed { reason }) => State::Dismissed(Dismissed {
            reason: reason.clone(),
        }),
        Some(CollaborationResolution::Annotation { annotation_id }) => {
            State::IntoAnnotation(ResolvedIntoAnnotation {
                annotation_id: annotation_id.clone(),
            })
        }
        Some(CollaborationResolution::AddressedByChange { .. }) => {
            return Err(Status::failed_precondition(
                "the pre-0.23 discussion RPC cannot represent a change-anchored resolution",
            ));
        }
    };
    Ok(ProtoDiscussionResolution { state: Some(state) })
}

fn write_descendant(
    repository: &Repository,
    store: &CollaborationStore,
    discussion_id: DiscussionRecordId,
    key: CollaborationIdempotencyKey,
    body: CollaborationOperationBodyV1,
) -> Result<ProtoDiscussion, Status> {
    let discussion = store
        .materialize_discussion(&discussion_id)
        .map_err(to_status)?
        .ok_or_else(|| Status::not_found(format!("discussion {discussion_id} not found")))?;
    let operation = CollaborationOperationEnvelope::new(
        discussion_id,
        discussion.heads.iter().copied().collect(),
        key,
        repository.get_attribution().map_err(to_status)?,
        now_ms(),
        body,
    )
    .map_err(|error| Status::invalid_argument(error.to_string()))?;
    store.write_operation(&operation).map_err(to_status)?;
    let discussion = store
        .materialize_discussion(&discussion_id)
        .map_err(to_status)?
        .ok_or_else(|| Status::internal("discussion disappeared after write"))?;
    materialized_to_proto(store, &discussion)
}

#[tonic::async_trait]
impl DiscussionService for LocalDiscussionService {
    async fn open_discussion(
        &self,
        request: Request<OpenDiscussionRequest>,
    ) -> Result<Response<ProtoDiscussion>, Status> {
        let req = request.into_inner();
        let req_bytes = req.encode_to_vec();
        let client_op_id = req.client_operation_id.clone();
        let inner = self.inner.clone();
        let result = with_idempotency(
            &self.inner,
            &client_op_id,
            "discussion.open",
            &req_bytes,
            move || {
                let req = req.clone();
                let inner = inner.clone();
                async move {
                    let repository = inner.repo();
                    let store = open_store(repository)?;
                    let anchor = req
                        .anchor
                        .ok_or_else(|| Status::invalid_argument("anchor is required"))?;
                    if anchor.file.trim().is_empty() || anchor.symbol.trim().is_empty() {
                        return Err(Status::invalid_argument(
                            "anchor.file and anchor.symbol are required",
                        ));
                    }
                    let state_id = StateId::try_from_slice(&req.state_id).map_err(|error| {
                        Status::invalid_argument(format!("invalid state_id: {error}"))
                    })?;
                    let title = req
                        .body
                        .lines()
                        .map(str::trim)
                        .find(|line| !line.is_empty())
                        .unwrap_or(&anchor.symbol)
                        .to_string();
                    let discussion_id = DiscussionRecordId::generate();
                    let operation = CollaborationOperationEnvelope::new(
                        discussion_id,
                        Vec::new(),
                        idempotency_key(&req.client_operation_id)?,
                        repository.get_attribution().map_err(to_status)?,
                        now_ms(),
                        CollaborationOperationBodyV1::Open {
                            title,
                            anchor: CollaborationAnchor::Symbol {
                                state_id,
                                path: anchor.file,
                                symbol: anchor.symbol,
                            },
                            visibility: parse_visibility(
                                &req.visibility,
                                repository.resolve_capture_default_visibility(),
                            )?,
                            turn: DiscussionTurnV1::new(req.body)
                                .map_err(|error| Status::invalid_argument(error.to_string()))?,
                        },
                    )
                    .map_err(|error| Status::invalid_argument(error.to_string()))?;
                    store.write_operation(&operation).map_err(to_status)?;
                    let discussion = store
                        .materialize_discussion(&discussion_id)
                        .map_err(to_status)?
                        .ok_or_else(|| Status::internal("discussion did not materialize"))?;
                    materialized_to_proto(&store, &discussion)
                }
            },
        )
        .await?;
        Ok(Response::new(result))
    }

    async fn append_turn(
        &self,
        request: Request<AppendTurnRequest>,
    ) -> Result<Response<ProtoDiscussion>, Status> {
        let req = request.into_inner();
        let req_bytes = req.encode_to_vec();
        let client_op_id = req.client_operation_id.clone();
        let inner = self.inner.clone();
        let result = with_idempotency(
            &self.inner,
            &client_op_id,
            "discussion.append_turn",
            &req_bytes,
            move || {
                let req = req.clone();
                let inner = inner.clone();
                async move {
                    let repository = inner.repo();
                    let store = open_store(repository)?;
                    write_descendant(
                        repository,
                        &store,
                        parse_discussion_id(&req.discussion_id)?,
                        idempotency_key(&req.client_operation_id)?,
                        CollaborationOperationBodyV1::AppendTurn {
                            turn: DiscussionTurnV1::new(req.body)
                                .map_err(|error| Status::invalid_argument(error.to_string()))?,
                        },
                    )
                }
            },
        )
        .await?;
        Ok(Response::new(result))
    }

    async fn resolve_discussion(
        &self,
        request: Request<ResolveDiscussionRequest>,
    ) -> Result<Response<ProtoDiscussion>, Status> {
        let req = request.into_inner();
        let req_bytes = req.encode_to_vec();
        let client_op_id = req.client_operation_id.clone();
        let inner = self.inner.clone();
        let result = with_idempotency(
            &self.inner,
            &client_op_id,
            "discussion.resolve",
            &req_bytes,
            move || {
                let req = req.clone();
                let inner = inner.clone();
                async move {
                    use grpc::heddle::v1::resolve_discussion_request::Resolution;
                    let resolution = match req
                        .resolution
                        .ok_or_else(|| Status::invalid_argument("resolution is required"))?
                    {
                        Resolution::ByEdit(value) => CollaborationResolution::AddressedByState {
                            state_id: StateId::try_from_slice(&value.state_id).map_err(|error| {
                                Status::invalid_argument(format!("invalid state_id: {error}"))
                            })?,
                        },
                        Resolution::Dismissed(value) => CollaborationResolution::Dismissed {
                            reason: value.reason,
                        },
                        Resolution::IntoAnnotation(_) => {
                            return Err(Status::failed_precondition(
                                "resolving into context requires the future cross-domain transaction surface",
                            ));
                        }
                    };
                    let repository = inner.repo();
                    let store = open_store(repository)?;
                    write_descendant(
                        repository,
                        &store,
                        parse_discussion_id(&req.discussion_id)?,
                        idempotency_key(&req.client_operation_id)?,
                        CollaborationOperationBodyV1::Resolve { resolution },
                    )
                }
            },
        )
        .await?;
        Ok(Response::new(result))
    }

    async fn list_by_state(
        &self,
        request: Request<ListDiscussionsByStateRequest>,
    ) -> Result<Response<ListDiscussionsResponse>, Status> {
        let req = request.into_inner();
        let state_id = StateId::try_from_slice(&req.state_id)
            .map_err(|error| Status::invalid_argument(format!("invalid state_id: {error}")))?;
        let store = open_store(self.inner.repo())?;
        let collaboration = store.materialize().map_err(to_status)?;
        let discussions = collaboration
            .discussions
            .into_values()
            .filter(|discussion| anchor_state(&discussion.anchor) == Some(state_id))
            .filter(|discussion| status_matches(discussion, &req.status))
            .map(|discussion| materialized_to_proto(&store, &discussion))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Response::new(ListDiscussionsResponse { discussions }))
    }

    async fn list_by_symbol(
        &self,
        request: Request<ListDiscussionsBySymbolRequest>,
    ) -> Result<Response<ListDiscussionsResponse>, Status> {
        let req = request.into_inner();
        let anchor = req
            .anchor
            .ok_or_else(|| Status::invalid_argument("anchor is required"))?;
        let store = open_store(self.inner.repo())?;
        let collaboration = store.materialize().map_err(to_status)?;
        let discussions = collaboration
            .discussions
            .into_values()
            .filter(|discussion| {
                symbol_anchor(&discussion.anchor)
                    .is_some_and(|value| value == (anchor.file.as_str(), anchor.symbol.as_str()))
            })
            .filter(|discussion| status_matches(discussion, &req.status))
            .map(|discussion| materialized_to_proto(&store, &discussion))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Response::new(ListDiscussionsResponse { discussions }))
    }

    async fn get_discussion(
        &self,
        request: Request<GetDiscussionRequest>,
    ) -> Result<Response<ProtoDiscussion>, Status> {
        let req = request.into_inner();
        let discussion_id = parse_discussion_id(&req.discussion_id)?;
        let store = open_store(self.inner.repo())?;
        let discussion = store
            .materialize_discussion(&discussion_id)
            .map_err(to_status)?
            .ok_or_else(|| Status::not_found(format!("discussion {discussion_id} not found")))?;
        if !req.state_id.is_empty() {
            let requested = StateId::try_from_slice(&req.state_id)
                .map_err(|error| Status::invalid_argument(format!("invalid state_id: {error}")))?;
            if anchor_state(&discussion.anchor) != Some(requested) {
                return Err(Status::not_found(format!(
                    "discussion {discussion_id} is not anchored to {requested}"
                )));
            }
        }
        Ok(Response::new(materialized_to_proto(&store, &discussion)?))
    }
}
