// SPDX-License-Identifier: Apache-2.0
//! Local gRPC service for the W2 `DiscussionService`.
//!
//! Reads and writes the `DiscussionsBlob` attached to a state via
//! [`State::with_discussions`]. Open / append / resolve mutations all follow
//! the same pattern: load the current state, decode (or create fresh) the
//! existing blob, mutate, encode back to a new [`Blob`], persist a new
//! `State` with the updated `discussions` content hash.
//!
//! Discovery RPCs (lookup by id, lookup by symbol) currently scan the HEAD
//! state's blob only. A cross-state index is W2 follow-up work and is
//! flagged with `// TODO(W2-followup):` comments.

use grpc::heddle::v1::{
    AppendTurnRequest, Discussion as ProtoDiscussion,
    DiscussionResolution as ProtoDiscussionResolution, DiscussionTurn as ProtoDiscussionTurn,
    GetDiscussionRequest, ListDiscussionsByStateRequest, ListDiscussionsBySymbolRequest,
    ListDiscussionsResponse, OpenDiscussionRequest, PathSymbolRef, ResolveDiscussionRequest,
    discussion_service_server::DiscussionService,
};
use objects::{
    object::{
        Blob, ChangeId, Discussion, DiscussionResolution, DiscussionTurn, DiscussionsBlob,
        Principal, State, SymbolAnchor, VisibilityTier,
    },
    store::BlockingObjectStore,
};
use prost::Message;
use repo::Repository;
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

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Wire vocabulary mirrors `VisibilityTier::as_str`. Empty / unknown
/// strings collapse to `Public` (the proto convention is "empty means
/// default"). `team_scoped` and `restricted` round-trip through this path
/// without an associated label — they're admitted for forward-compat with
/// the namespace policy override path; callers wanting a labelled value
/// must go through a richer surface that doesn't yet exist on this RPC.
fn parse_visibility(s: &str) -> VisibilityTier {
    match s {
        "internal" => VisibilityTier::Internal,
        "team_scoped" => VisibilityTier::TeamScoped {
            team_id: String::new(),
        },
        "restricted" => VisibilityTier::Restricted {
            scope_label: String::new(),
        },
        // "public", "", or anything else.
        _ => VisibilityTier::Public,
    }
}

fn turn_to_proto(turn: &DiscussionTurn) -> ProtoDiscussionTurn {
    ProtoDiscussionTurn {
        author_name: turn.author.name.clone(),
        author_email: turn.author.email.clone(),
        body: turn.body.clone(),
        posted_at: Some(prost_types::Timestamp {
            seconds: turn.posted_at,
            nanos: 0,
        }),
    }
}

fn resolution_to_proto(resolution: &DiscussionResolution) -> ProtoDiscussionResolution {
    use grpc::heddle::v1::discussion_resolution::{
        Dismissed, Open, ResolvedByEdit, ResolvedIntoAnnotation, State,
    };
    let state = match resolution {
        DiscussionResolution::Open => State::Open(Open {}),
        DiscussionResolution::ResolvedIntoAnnotation { annotation_id } => {
            State::IntoAnnotation(ResolvedIntoAnnotation {
                annotation_id: annotation_id.clone(),
            })
        }
        DiscussionResolution::ResolvedByEdit { state_id } => State::ByEdit(ResolvedByEdit {
            state_id: state_id.as_bytes().to_vec(),
        }),
        DiscussionResolution::Dismissed { reason } => State::Dismissed(Dismissed {
            reason: reason.clone(),
        }),
    };
    ProtoDiscussionResolution { state: Some(state) }
}

fn discussion_to_proto(d: &Discussion) -> ProtoDiscussion {
    ProtoDiscussion {
        id: d.id.clone(),
        anchor: Some(PathSymbolRef {
            file: d.anchor.file.clone(),
            symbol: d.anchor.symbol.clone(),
        }),
        opened_against_state: d.opened_against_state.as_bytes().to_vec(),
        opened_at: Some(prost_types::Timestamp {
            seconds: d.opened_at,
            nanos: 0,
        }),
        thread_ref: d.thread_ref.clone().unwrap_or_default(),
        turns: d.turns.iter().map(turn_to_proto).collect(),
        resolution: Some(resolution_to_proto(&d.resolution)),
        body_changed_since_open: d.body_changed_since_open,
        orphaned: d.orphaned,
        visibility: d.visibility.as_str().to_string(),
        resolved_annotation_id: d.resolved_annotation_id.clone().unwrap_or_default(),
    }
}

/// Resolve a `state_id` string to a stored `State`, returning the parsed
/// `ChangeId` and the loaded `State`.
fn load_state(repo: &Repository, state_id: &[u8]) -> Result<(ChangeId, State), Status> {
    let id = ChangeId::try_from_slice(state_id)
        .map_err(|err| Status::invalid_argument(format!("invalid state_id: {err}")))?;
    let state = repo
        .store()
        .get_state(&id)
        .map_err(to_status)?
        .ok_or_else(|| Status::not_found(format!("state {} not found", id.to_string_full())))?;
    Ok((id, state))
}

/// Decode a state's `DiscussionsBlob`, returning an empty blob when the
/// state has no discussions attached yet.
fn decode_blob_for_state(repo: &Repository, state: &State) -> Result<DiscussionsBlob, Status> {
    let Some(hash) = state.discussions else {
        return Ok(DiscussionsBlob::new(Vec::new()));
    };
    let blob = repo
        .store()
        .get_blob(&hash)
        .map_err(to_status)?
        .ok_or_else(|| {
            Status::not_found(format!(
                "discussions blob {} referenced by state {} is missing",
                hash,
                state.change_id.to_string_full()
            ))
        })?;
    DiscussionsBlob::decode(blob.content())
        .map_err(|err| Status::internal(format!("failed to decode discussions blob: {err}")))
}

/// Convenience: load both the state and its decoded `DiscussionsBlob`.
fn load_discussions_blob(
    repo: &Repository,
    state_id: &ChangeId,
) -> Result<(State, DiscussionsBlob), Status> {
    let state = repo
        .store()
        .get_state(state_id)
        .map_err(to_status)?
        .ok_or_else(|| {
            Status::not_found(format!("state {} not found", state_id.to_string_full()))
        })?;
    let blob = decode_blob_for_state(repo, &state)?;
    Ok((state, blob))
}

/// Encode `blob`, persist it under a fresh `ContentHash`, then build and
/// store a new `State` with the updated `discussions` pointer.
fn save_discussions_blob(
    repo: &Repository,
    state: &State,
    blob: &DiscussionsBlob,
) -> Result<State, Status> {
    let bytes = blob
        .encode()
        .map_err(|err| Status::internal(format!("failed to encode discussions blob: {err}")))?;
    let hash = repo
        .store()
        .put_blob(&Blob::new(bytes))
        .map_err(to_status)?;
    let new_state = state.clone().with_discussions(hash);
    repo.store().put_state(&new_state).map_err(to_status)?;
    Ok(new_state)
}

/// Resolve the active principal using the repository's identity chain
/// (env/repo/Git config) and fall back to a placeholder only when that lookup
/// itself fails. We deliberately don't fail here — discussion authorship
/// should never block on missing config.
fn principal_for(repo: &Repository) -> Principal {
    repo.get_principal()
        .unwrap_or_else(|_| Principal::new("<unknown>", ""))
}

/// Resolve the HEAD state. Returns `Status::failed_precondition` when the
/// repository has no HEAD (a fresh repo before any thread is seeded).
fn head_state(repo: &Repository) -> Result<State, Status> {
    let head_id = repo
        .head()
        .map_err(to_status)?
        .ok_or_else(|| Status::failed_precondition("repository has no HEAD"))?;
    repo.store()
        .get_state(&head_id)
        .map_err(to_status)?
        .ok_or_else(|| {
            Status::not_found(format!("HEAD state {} not found", head_id.to_string_full()))
        })
}

/// Status filter for list_by_state / list_by_symbol. Empty / unknown values
/// behave like `"all"`.
fn status_matches(d: &Discussion, status: &str) -> bool {
    match status {
        "open" => d.is_open(),
        "resolved" => !d.is_open(),
        "orphaned" => d.orphaned,
        // "all", "", anything else.
        _ => true,
    }
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
                    let repo = inner.repo();
                    let anchor_proto = req
                        .anchor
                        .clone()
                        .ok_or_else(|| Status::invalid_argument("anchor is required"))?;
                    if anchor_proto.file.is_empty() {
                        return Err(Status::invalid_argument("anchor.file is required"));
                    }
                    if anchor_proto.symbol.is_empty() {
                        return Err(Status::invalid_argument("anchor.symbol is required"));
                    }
                    if req.body.trim().is_empty() {
                        return Err(Status::invalid_argument("body must be non-empty"));
                    }
                    let opened_against =
                        ChangeId::try_from_slice(&req.state_id).map_err(|err| {
                            Status::invalid_argument(format!("invalid state_id: {err}"))
                        })?;
                    let (state, mut blob) = load_discussions_blob(repo, &opened_against)?;
                    let now = now_secs();
                    let principal = principal_for(repo);
                    let discussion = Discussion {
                        id: ChangeId::generate().to_string_full(),
                        anchor: SymbolAnchor::new(anchor_proto.file, anchor_proto.symbol),
                        opened_against_state: opened_against,
                        opened_at: now,
                        thread_ref: (!req.thread_ref.is_empty()).then(|| req.thread_ref.clone()),
                        turns: vec![DiscussionTurn {
                            author: principal,
                            body: req.body.clone(),
                            posted_at: now,
                        }],
                        resolution: DiscussionResolution::Open,
                        body_changed_since_open: false,
                        orphaned: false,
                        visibility: parse_visibility(&req.visibility),
                        resolved_annotation_id: None,
                    };
                    discussion
                        .validate()
                        .map_err(|err| Status::invalid_argument(err.to_string()))?;
                    blob.discussions.push(discussion.clone());
                    save_discussions_blob(repo, &state, &blob)?;
                    Ok(discussion_to_proto(&discussion))
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
                    let repo = inner.repo();
                    if req.discussion_id.is_empty() {
                        return Err(Status::invalid_argument("discussion_id is required"));
                    }
                    if req.body.trim().is_empty() {
                        return Err(Status::invalid_argument("body must be non-empty"));
                    }
                    // TODO(W2-followup): scan all states / oplog instead of HEAD-only.
                    let head = head_state(repo)?;
                    let mut blob = decode_blob_for_state(repo, &head)?;
                    let idx = blob
                        .discussions
                        .iter()
                        .position(|d| d.id == req.discussion_id)
                        .ok_or_else(|| {
                            Status::not_found(format!("discussion {} not found", req.discussion_id))
                        })?;
                    let principal = principal_for(repo);
                    blob.discussions[idx].turns.push(DiscussionTurn {
                        author: principal,
                        body: req.body.clone(),
                        posted_at: now_secs(),
                    });
                    blob.discussions[idx]
                        .validate()
                        .map_err(|err| Status::invalid_argument(err.to_string()))?;
                    let updated = blob.discussions[idx].clone();
                    save_discussions_blob(repo, &head, &blob)?;
                    Ok(discussion_to_proto(&updated))
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
                    let repo = inner.repo();
                    if req.discussion_id.is_empty() {
                        return Err(Status::invalid_argument("discussion_id is required"));
                    }
                    // TODO(W2-followup): scan all states / oplog instead of HEAD-only.
                    let head = head_state(repo)?;
                    let mut blob = decode_blob_for_state(repo, &head)?;
                    let idx = blob
                        .discussions
                        .iter()
                        .position(|d| d.id == req.discussion_id)
                        .ok_or_else(|| {
                            Status::not_found(format!("discussion {} not found", req.discussion_id))
                        })?;

                    use grpc::heddle::v1::resolve_discussion_request::Resolution;
                    let resolution = req
                        .resolution
                        .clone()
                        .ok_or_else(|| Status::invalid_argument("resolution mode is required"))?;
                    match resolution {
                        Resolution::IntoAnnotation(_payload) => {
                            // TODO(W2-followup): R5 wiring will create a real
                            // `Annotation` from the discussion's content,
                            // attribute it, and back-link it. For the first
                            // ship we mint a placeholder id and record the
                            // bidirectional link so the resolution shape is
                            // honest about its terminal state.
                            let annotation_id = ChangeId::generate().to_string_full();
                            blob.discussions[idx].resolution =
                                DiscussionResolution::ResolvedIntoAnnotation {
                                    annotation_id: annotation_id.clone(),
                                };
                            blob.discussions[idx].resolved_annotation_id = Some(annotation_id);
                        }
                        Resolution::ByEdit(payload) => {
                            let state_id =
                                ChangeId::try_from_slice(&payload.state_id).map_err(|err| {
                                    Status::invalid_argument(format!("invalid state_id: {err}"))
                                })?;
                            blob.discussions[idx].resolution =
                                DiscussionResolution::ResolvedByEdit { state_id };
                        }
                        Resolution::Dismissed(payload) => {
                            if payload.reason.trim().is_empty() {
                                return Err(Status::invalid_argument(
                                    "dismissal requires a non-empty reason",
                                ));
                            }
                            blob.discussions[idx].resolution = DiscussionResolution::Dismissed {
                                reason: payload.reason,
                            };
                        }
                    }

                    blob.discussions[idx]
                        .validate()
                        .map_err(|err| Status::invalid_argument(err.to_string()))?;
                    let updated = blob.discussions[idx].clone();
                    save_discussions_blob(repo, &head, &blob)?;
                    Ok(discussion_to_proto(&updated))
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
        let repo = self.inner.repo();
        let (_, state) = load_state(repo, &req.state_id)?;
        let blob = decode_blob_for_state(repo, &state)?;
        let discussions = blob
            .discussions
            .iter()
            .filter(|d| status_matches(d, &req.status))
            .map(discussion_to_proto)
            .collect();
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
        if anchor.file.is_empty() || anchor.symbol.is_empty() {
            return Err(Status::invalid_argument(
                "anchor.file and anchor.symbol are required",
            ));
        }
        // TODO(W2-followup): cross-state symbol index. For now we only
        // surface discussions attached to the HEAD state.
        let repo = self.inner.repo();
        let head = head_state(repo)?;
        let blob = decode_blob_for_state(repo, &head)?;
        let discussions = blob
            .discussions
            .iter()
            .filter(|d| d.anchor.file == anchor.file && d.anchor.symbol == anchor.symbol)
            .filter(|d| status_matches(d, &req.status))
            .map(discussion_to_proto)
            .collect();
        Ok(Response::new(ListDiscussionsResponse { discussions }))
    }

    async fn get_discussion(
        &self,
        request: Request<GetDiscussionRequest>,
    ) -> Result<Response<ProtoDiscussion>, Status> {
        let req = request.into_inner();
        if req.discussion_id.is_empty() {
            return Err(Status::invalid_argument("discussion_id is required"));
        }
        // TODO(W2-followup): scan all states / oplog instead of HEAD-only.
        let repo = self.inner.repo();
        let head = head_state(repo)?;
        let blob = decode_blob_for_state(repo, &head)?;
        let discussion = blob
            .discussions
            .iter()
            .find(|d| d.id == req.discussion_id)
            .ok_or_else(|| {
                Status::not_found(format!("discussion {} not found", req.discussion_id))
            })?;
        Ok(Response::new(discussion_to_proto(discussion)))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use objects::object::{Attribution, Principal};
    use repo::{Repository, operation_dedup::OperationDedupStore};
    use tempfile::TempDir;

    use super::*;

    fn fresh_service() -> (TempDir, ChangeId, LocalDiscussionService) {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        // Take a snapshot so we have a real state to anchor discussions against.
        let attribution = Attribution::human(Principal::new("Tester", "tester@example.com"));
        let state = repo
            .snapshot_with_attribution(Some("seed".into()), None, attribution)
            .unwrap();
        let dedup = OperationDedupStore::open(repo.heddle_dir()).unwrap();
        let inner = GrpcLocalService::new(Arc::new(repo), Arc::new(dedup));
        let svc = LocalDiscussionService::new(inner);
        (temp, state.change_id, svc)
    }

    fn open_request(state_id: &ChangeId, body: &str, op_id: &str) -> OpenDiscussionRequest {
        OpenDiscussionRequest {
            repo_path: String::new(),
            state_id: state_id.as_bytes().to_vec(),
            anchor: Some(PathSymbolRef {
                file: "src/lib.rs".into(),
                symbol: "foo".into(),
            }),
            body: body.into(),
            visibility: String::new(),
            thread_ref: String::new(),
            client_operation_id: op_id.into(),
        }
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn open_then_append_turn_persists_both_turns() {
        let (_t, state_id, svc) = fresh_service();
        let opened = svc
            .open_discussion(Request::new(open_request(&state_id, "first", "")))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(opened.turns.len(), 1);
        assert_eq!(opened.turns[0].body, "first");

        let appended = svc
            .append_turn(Request::new(AppendTurnRequest {
                repo_path: String::new(),
                discussion_id: opened.id.clone(),
                body: "second".into(),
                client_operation_id: String::new(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(appended.turns.len(), 2);
        assert_eq!(appended.turns[0].body, "first");
        assert_eq!(appended.turns[1].body, "second");

        // Confirm the on-disk state actually carries both turns: re-list.
        let listed = svc
            .list_by_state(Request::new(ListDiscussionsByStateRequest {
                repo_path: String::new(),
                state_id: state_id.as_bytes().to_vec(),
                status: "all".into(),
            }))
            .await
            .unwrap()
            .into_inner();
        // The discussion was attached to the original state so list_by_state
        // on that state still finds it.
        assert_eq!(listed.discussions.len(), 1);
        assert_eq!(listed.discussions[0].turns.len(), 2);
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn open_idempotent_returns_same_discussion() {
        let (_t, state_id, svc) = fresh_service();
        let op_id = "11111111-2222-3333-4444-555555555555";
        let first = svc
            .open_discussion(Request::new(open_request(&state_id, "hello", op_id)))
            .await
            .unwrap()
            .into_inner();
        let second = svc
            .open_discussion(Request::new(open_request(&state_id, "hello", op_id)))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(first.id, second.id);
        assert_eq!(first.turns.len(), 1);
        assert_eq!(second.turns.len(), 1);
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn resolve_dismissed_with_empty_reason_is_invalid_argument() {
        let (_t, state_id, svc) = fresh_service();
        let opened = svc
            .open_discussion(Request::new(open_request(&state_id, "why?", "")))
            .await
            .unwrap()
            .into_inner();

        use grpc::heddle::v1::resolve_discussion_request::{Resolution, ResolveDismissed};
        let err = svc
            .resolve_discussion(Request::new(ResolveDiscussionRequest {
                repo_path: String::new(),
                discussion_id: opened.id,
                resolution: Some(Resolution::Dismissed(ResolveDismissed {
                    reason: "   ".into(),
                })),
                client_operation_id: String::new(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn list_by_state_filters_by_status() {
        let (_t, state_id, svc) = fresh_service();
        // Open two discussions, dismiss one of them.
        let a = svc
            .open_discussion(Request::new(open_request(&state_id, "a", "")))
            .await
            .unwrap()
            .into_inner();
        let _b = svc
            .open_discussion(Request::new(open_request(&state_id, "b", "")))
            .await
            .unwrap()
            .into_inner();

        use grpc::heddle::v1::resolve_discussion_request::{Resolution, ResolveDismissed};
        svc.resolve_discussion(Request::new(ResolveDiscussionRequest {
            repo_path: String::new(),
            discussion_id: a.id.clone(),
            resolution: Some(Resolution::Dismissed(ResolveDismissed {
                reason: "no longer relevant".into(),
            })),
            client_operation_id: String::new(),
        }))
        .await
        .unwrap();

        // The dismissal mutates the HEAD state's blob, not the original
        // state's blob. So `list_by_state(state_id, "open")` should still
        // see two open discussions on the *original* state_id (since the
        // resolve wrote to HEAD, which advanced past state_id only when a
        // new snapshot was taken — in our test repo HEAD is still
        // state_id from `seed`).
        //
        // To make a deterministic assertion regardless of HEAD movement
        // we instead query the HEAD state, which is where resolve_*
        // wrote its mutation. We rely on `repo.head()` matching
        // `state_id` because we never took an additional snapshot.
        let head_state_id = state_id.as_bytes().to_vec();
        let open_only = svc
            .list_by_state(Request::new(ListDiscussionsByStateRequest {
                repo_path: String::new(),
                state_id: head_state_id.clone(),
                status: "open".into(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(open_only.discussions.len(), 1);
        assert_eq!(open_only.discussions[0].turns[0].body, "b");

        let resolved_only = svc
            .list_by_state(Request::new(ListDiscussionsByStateRequest {
                repo_path: String::new(),
                state_id: head_state_id.clone(),
                status: "resolved".into(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resolved_only.discussions.len(), 1);
        assert_eq!(resolved_only.discussions[0].turns[0].body, "a");

        let all = svc
            .list_by_state(Request::new(ListDiscussionsByStateRequest {
                repo_path: String::new(),
                state_id: head_state_id,
                status: "all".into(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(all.discussions.len(), 2);
    }
}
