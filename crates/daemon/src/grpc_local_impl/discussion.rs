// SPDX-License-Identifier: Apache-2.0
//! Local gRPC service for the W2 `DiscussionService`.
//!
//! Reads and writes the `DiscussionsBlob` attached to an immutable state.
//!
//! State-scoped discovery reads the requested state's blob. Repository-wide
//! discovery walks reachable states and deduplicates by discussion id, preferring
//! the current HEAD copy when a carried-forward discussion exists in multiple
//! states.

use std::collections::HashSet;

use grpc::heddle::v1::{
    AppendTurnRequest, ContextAnnotationKind, Discussion as ProtoDiscussion,
    DiscussionResolution as ProtoDiscussionResolution, DiscussionTurn as ProtoDiscussionTurn,
    GetDiscussionRequest, ListDiscussionsByStateRequest, ListDiscussionsBySymbolRequest,
    ListDiscussionsResponse, OpenDiscussionRequest, PathSymbolRef, ResolveDiscussionRequest,
    discussion_service_server::DiscussionService,
    resolve_discussion_request::ResolveIntoAnnotation,
};
use objects::{
    lock::RepositoryLockExt,
    object::{
        Annotation, AnnotationKind, AnnotationScope, Blob, ContentHash, ContextBlob, ContextTarget,
        Discussion, DiscussionResolution, DiscussionTurn, DiscussionsBlob, Principal, State,
        StateAttachment, StateAttachmentBody, StateId, SymbolAnchor, VisibilityTier,
    },
    store::ObjectStore,
};
use prost::Message;
use repo::{Repository, StateAttachmentKind};
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

/// Flat discussion RPC visibility vocabulary. Labelled tiers carry their label
/// in the token itself (`team:<id>`, `restricted:<label>`,
/// `private:<label>`) so the service never has to manufacture an empty label.
fn parse_visibility(s: &str) -> Result<VisibilityTier, Status> {
    let trimmed = s.trim();
    match trimmed {
        "" | "public" => return Ok(VisibilityTier::Public),
        "internal" => return Ok(VisibilityTier::Internal),
        _ => {}
    }

    if let Some(team_id) = trimmed.strip_prefix("team:") {
        let team_id = team_id.trim();
        if team_id.is_empty() {
            return Err(Status::invalid_argument(
                "discussion visibility team:<id> requires a non-empty id",
            ));
        }
        return Ok(VisibilityTier::TeamScoped {
            team_id: team_id.to_string(),
        });
    }
    if let Some(scope_label) = trimmed.strip_prefix("restricted:") {
        let scope_label = scope_label.trim();
        if scope_label.is_empty() {
            return Err(Status::invalid_argument(
                "discussion visibility restricted:<label> requires a non-empty label",
            ));
        }
        return Ok(VisibilityTier::Restricted {
            scope_label: scope_label.to_string(),
        });
    }
    if let Some(scope_label) = trimmed.strip_prefix("private:") {
        let scope_label = scope_label.trim();
        if scope_label.is_empty() {
            return Err(Status::invalid_argument(
                "discussion visibility private:<label> requires a non-empty label",
            ));
        }
        return Ok(VisibilityTier::Private {
            scope_label: scope_label.to_string(),
        });
    }
    Err(Status::invalid_argument(format!(
        "unsupported discussion visibility {trimmed:?}; expected public, internal, team:<id>, restricted:<label>, or private:<label>"
    )))
}

fn visibility_to_wire(visibility: &VisibilityTier) -> String {
    match visibility {
        VisibilityTier::Public | VisibilityTier::Internal => visibility.as_str().to_string(),
        VisibilityTier::TeamScoped { team_id } => format!("team:{team_id}"),
        VisibilityTier::Restricted { scope_label } => format!("restricted:{scope_label}"),
        VisibilityTier::Private { scope_label } => format!("private:{scope_label}"),
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
        visibility: visibility_to_wire(&d.visibility),
        resolved_annotation_id: d.resolved_annotation_id.clone().unwrap_or_default(),
    }
}

/// Resolve a `state_id` string to a stored `State`, returning the parsed
/// `StateId` and the loaded `State`.
fn load_state(repo: &Repository, state_id: &[u8]) -> Result<(StateId, State), Status> {
    let id = StateId::try_from_slice(state_id)
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
    let Some(attachment) = repo
        .latest_state_attachment(&state.state_id, StateAttachmentKind::Discussions)
        .map_err(to_status)?
    else {
        return Ok(DiscussionsBlob::new(Vec::new()));
    };
    let StateAttachmentBody::Discussions(hash) = attachment.body else {
        unreachable!()
    };
    let blob = repo
        .store()
        .get_blob(&hash)
        .map_err(to_status)?
        .ok_or_else(|| {
            Status::not_found(format!(
                "discussions blob {} referenced by state {} is missing",
                hash,
                state.state_id.to_string_full()
            ))
        })?;
    DiscussionsBlob::decode(blob.content())
        .map_err(|err| Status::internal(format!("failed to decode discussions blob: {err}")))
}

/// Convenience: load both the state and its decoded `DiscussionsBlob`.
fn load_discussions_blob(
    repo: &Repository,
    state_id: &StateId,
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

fn save_discussions_blob(
    repo: &Repository,
    state: &State,
    blob: &DiscussionsBlob,
) -> Result<(), Status> {
    let hash = put_discussions_blob(repo, blob)?;
    let prior = repo
        .latest_state_attachment(&state.state_id, StateAttachmentKind::Discussions)
        .map_err(to_status)?;
    let attachment = StateAttachment {
        state_id: state.state_id,
        body: StateAttachmentBody::Discussions(hash),
        attribution: repo.get_attribution().map_err(to_status)?,
        created_at: chrono::Utc::now(),
        supersedes: prior.map(|attachment| attachment.id()),
    };
    repo.put_state_attachment(&attachment).map_err(to_status)?;
    Ok(())
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

fn put_discussions_blob(repo: &Repository, blob: &DiscussionsBlob) -> Result<ContentHash, Status> {
    let bytes = blob
        .encode()
        .map_err(|err| Status::internal(format!("failed to encode discussions blob: {err}")))?;
    repo.store().put_blob(&Blob::new(bytes)).map_err(to_status)
}

fn reachable_discussions(repo: &Repository) -> Result<Vec<(StateId, Discussion)>, Status> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    if let Some(head_id) = repo.head().map_err(to_status)?
        && let Some(head) = repo.store().get_state(&head_id).map_err(to_status)?
    {
        push_discussions_from_state(repo, head_id, &head, &mut seen, &mut out)?;
    }

    for state_id in repo
        .reachable_states()
        .map_err(|err| Status::internal(format!("walk reachable states: {err}")))?
    {
        let Some(state) = repo.store().get_state(&state_id).map_err(to_status)? else {
            continue;
        };
        push_discussions_from_state(repo, state_id, &state, &mut seen, &mut out)?;
    }

    Ok(out)
}

fn push_discussions_from_state(
    repo: &Repository,
    state_id: StateId,
    state: &State,
    seen: &mut HashSet<String>,
    out: &mut Vec<(StateId, Discussion)>,
) -> Result<(), Status> {
    let blob = decode_blob_for_state(repo, state)?;
    for discussion in blob.discussions {
        if seen.insert(discussion.id.clone()) {
            out.push((state_id, discussion));
        }
    }
    Ok(())
}

fn annotation_kind_from_proto(kind: i32) -> Result<AnnotationKind, Status> {
    match ContextAnnotationKind::try_from(kind)
        .map_err(|_| Status::invalid_argument(format!("unknown annotation kind tag {kind}")))?
    {
        ContextAnnotationKind::Unspecified | ContextAnnotationKind::Rationale => {
            Ok(AnnotationKind::Rationale)
        }
        ContextAnnotationKind::Constraint => Ok(AnnotationKind::Constraint),
        ContextAnnotationKind::Invariant => Ok(AnnotationKind::Invariant),
    }
}

fn resolve_discussion_into_annotation(
    repo: &Repository,
    head: &State,
    discussions: &mut DiscussionsBlob,
    discussion_index: usize,
    payload: ResolveIntoAnnotation,
) -> Result<Discussion, Status> {
    if payload.content.trim().is_empty() {
        return Err(Status::invalid_argument(
            "into-annotation resolution requires non-empty content",
        ));
    }
    let kind = annotation_kind_from_proto(payload.kind)?;
    let attribution = repo
        .get_attribution()
        .map_err(|err| Status::internal(format!("resolve attribution: {err}")))?;

    let discussion = discussions
        .discussions
        .get(discussion_index)
        .ok_or_else(|| Status::internal("discussion index out of range"))?
        .clone();
    let target = ContextTarget::file(discussion.anchor.file.clone())
        .map_err(|err| Status::invalid_argument(err.to_string()))?;
    let mut scope = AnnotationScope::Symbol {
        name: discussion.anchor.symbol.clone(),
        resolved_lines: None,
    };
    target
        .validate_scope(&scope)
        .map_err(|err| Status::invalid_argument(err.to_string()))?;
    let source = target.path().and_then(|path| {
        std::fs::read(repo.root().join(path))
            .ok()
            .map(|bytes| (path.to_string(), bytes))
    });
    scope = resolve_annotation_scope(
        source
            .as_ref()
            .map(|(path, bytes)| (path.as_str(), bytes.as_slice())),
        scope,
    );
    let source_hash =
        compute_annotation_source_hash(source.as_ref().map(|(_, bytes)| bytes.as_slice()), &scope);

    let mut annotation = Annotation::new(
        scope,
        kind,
        payload.content,
        payload.tags,
        attribution.to_string(),
        now_secs(),
        source_hash,
        Some(head.state_id),
    );
    annotation.resolved_from_discussion = Some(discussion.id.clone());
    annotation.visibility = discussion.visibility.clone();
    let annotation_id = annotation.annotation_id.clone();

    let context_attachment = repo
        .latest_state_attachment(&head.state_id, StateAttachmentKind::Context)
        .map_err(to_status)?;
    let context_root = context_attachment.as_ref().map(|attachment| {
        let StateAttachmentBody::Context(root) = &attachment.body else {
            unreachable!()
        };
        *root
    });
    let mut context = match context_root {
        Some(root) => repo
            .get_context_blob(&root, &target)
            .map_err(to_status)?
            .unwrap_or_else(|| ContextBlob::new(Vec::new())),
        None => ContextBlob::new(Vec::new()),
    };
    context.annotations.push(annotation);
    let context_root = repo
        .set_context_blob(context_root.as_ref(), &target, &context)
        .map_err(to_status)?;

    let updated = discussions
        .discussions
        .get_mut(discussion_index)
        .ok_or_else(|| Status::internal("discussion index out of range"))?;
    updated.resolution = DiscussionResolution::ResolvedIntoAnnotation {
        annotation_id: annotation_id.clone(),
    };
    updated.resolved_annotation_id = Some(annotation_id);
    updated
        .validate()
        .map_err(|err| Status::invalid_argument(err.to_string()))?;
    let updated = updated.clone();

    let context_record = StateAttachment {
        state_id: head.state_id,
        body: StateAttachmentBody::Context(context_root),
        attribution: attribution.clone(),
        created_at: chrono::Utc::now(),
        supersedes: context_attachment.map(|attachment| attachment.id()),
    };
    repo.put_state_attachment(&context_record)
        .map_err(to_status)?;
    save_discussions_blob(repo, head, discussions)?;

    Ok(updated)
}

fn resolve_annotation_scope(
    source: Option<(&str, &[u8])>,
    scope: AnnotationScope,
) -> AnnotationScope {
    let AnnotationScope::Symbol {
        name,
        resolved_lines: None,
    } = scope
    else {
        return scope;
    };
    let Some((path, source)) = source else {
        return AnnotationScope::Symbol {
            name,
            resolved_lines: None,
        };
    };
    #[cfg(feature = "semantic")]
    {
        match repo::symbol_resolver::resolve_symbol_lines(source, std::path::Path::new(path), &name)
        {
            Ok((start, end)) => AnnotationScope::Symbol {
                name,
                resolved_lines: Some((start, end)),
            },
            Err(_) => AnnotationScope::Symbol {
                name,
                resolved_lines: None,
            },
        }
    }
    #[cfg(not(feature = "semantic"))]
    {
        let _ = path;
        let _ = source;
        AnnotationScope::Symbol {
            name,
            resolved_lines: None,
        }
    }
}

fn compute_annotation_source_hash(
    source: Option<&[u8]>,
    scope: &AnnotationScope,
) -> Option<ContentHash> {
    let source = source?;
    let scoped = match scope {
        AnnotationScope::Lines(start, end) => extract_line_range(source, *start, *end),
        AnnotationScope::Symbol {
            resolved_lines: Some((start, end)),
            ..
        } => extract_line_range(source, *start, *end),
        _ => source.to_vec(),
    };
    Some(ContentHash::compute(&scoped))
}

fn extract_line_range(source: &[u8], start: u32, end: u32) -> Vec<u8> {
    let start_line = start.max(1);
    let end_line = end.max(start_line);
    let mut current_line = 1;
    let mut start_byte = (start_line == 1).then_some(0);
    let mut end_byte = None;

    for (idx, byte) in source.iter().enumerate() {
        if *byte != b'\n' {
            continue;
        }
        if current_line == end_line {
            end_byte = Some(idx + 1);
            break;
        }
        current_line += 1;
        if current_line == start_line {
            start_byte = Some(idx + 1);
        }
    }

    let Some(start_byte) = start_byte else {
        return Vec::new();
    };
    let end_byte = end_byte.unwrap_or(source.len());
    if start_byte > end_byte || start_byte > source.len() {
        return Vec::new();
    }
    source[start_byte..end_byte].to_vec()
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
                    let opened_against = StateId::try_from_slice(&req.state_id).map_err(|err| {
                        Status::invalid_argument(format!("invalid state_id: {err}"))
                    })?;
                    let now = now_secs();
                    let principal = principal_for(repo);
                    let visibility = if req.visibility.trim().is_empty() {
                        repo.resolve_capture_default_visibility()
                    } else {
                        parse_visibility(&req.visibility)?
                    };
                    let discussion = Discussion {
                        id: objects::object::generate_discussion_id(),
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
                        visibility,
                        resolved_annotation_id: None,
                    };
                    discussion
                        .validate()
                        .map_err(|err| Status::invalid_argument(err.to_string()))?;
                    let _lock = repo
                        .locker()
                        .write()
                        .map_err(|err| Status::internal(err.to_string()))?;
                    let (state, mut blob) = load_discussions_blob(repo, &opened_against)?;
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
                    // This local mutation API is HEAD-scoped because the
                    // request carries no state_id. It must not pretend to find
                    // and mutate discussions across the whole repository.
                    let principal = principal_for(repo);
                    let _lock = repo
                        .locker()
                        .write()
                        .map_err(|err| Status::internal(err.to_string()))?;
                    let head = head_state(repo)?;
                    let mut blob = decode_blob_for_state(repo, &head)?;
                    let idx = blob
                        .discussions
                        .iter()
                        .position(|d| d.id == req.discussion_id)
                        .ok_or_else(|| {
                            Status::not_found(format!("discussion {} not found", req.discussion_id))
                        })?;
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
                    // This local mutation API is HEAD-scoped because the
                    // request carries no state_id. It must not pretend to find
                    // and mutate discussions across the whole repository.
                    use grpc::heddle::v1::resolve_discussion_request::Resolution;
                    let resolution = req
                        .resolution
                        .clone()
                        .ok_or_else(|| Status::invalid_argument("resolution mode is required"))?;
                    if let Resolution::Dismissed(ref payload) = resolution
                        && payload.reason.trim().is_empty()
                    {
                        return Err(Status::invalid_argument(
                            "dismissal requires a non-empty reason",
                        ));
                    }

                    let _lock = repo
                        .locker()
                        .write()
                        .map_err(|err| Status::internal(err.to_string()))?;
                    let head = head_state(repo)?;
                    let mut blob = decode_blob_for_state(repo, &head)?;
                    let idx = blob
                        .discussions
                        .iter()
                        .position(|d| d.id == req.discussion_id)
                        .ok_or_else(|| {
                            Status::not_found(format!("discussion {} not found", req.discussion_id))
                        })?;

                    match resolution {
                        Resolution::IntoAnnotation(payload) => {
                            let updated = resolve_discussion_into_annotation(
                                repo, &head, &mut blob, idx, payload,
                            )?;
                            return Ok(discussion_to_proto(&updated));
                        }
                        Resolution::ByEdit(payload) => {
                            let state_id =
                                StateId::try_from_slice(&payload.state_id).map_err(|err| {
                                    Status::invalid_argument(format!("invalid state_id: {err}"))
                                })?;
                            blob.discussions[idx].resolution =
                                DiscussionResolution::ResolvedByEdit { state_id };
                        }
                        Resolution::Dismissed(payload) => {
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
        let repo = self.inner.repo();
        let discussions = reachable_discussions(repo)?
            .into_iter()
            .map(|(_, discussion)| discussion)
            .filter(|discussion| {
                discussion.anchor.file == anchor.file
                    && discussion.anchor.symbol == anchor.symbol
                    && status_matches(discussion, &req.status)
            })
            .map(|discussion| discussion_to_proto(&discussion))
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
        // Default: HEAD first, then reachable states. Optional `state_id`
        // (#836) resolves the discussion against a specific prior state.
        let repo = self.inner.repo();
        if req.state_id.is_empty() {
            let discussion = reachable_discussions(repo)?
                .into_iter()
                .map(|(_, discussion)| discussion)
                .find(|discussion| discussion.id == req.discussion_id)
                .ok_or_else(|| {
                    Status::not_found(format!("discussion {} not found", req.discussion_id))
                })?;
            return Ok(Response::new(discussion_to_proto(&discussion)));
        }

        let state = load_state(repo, &req.state_id)?.1;
        let blob = decode_blob_for_state(repo, &state)?;
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

    fn fresh_service() -> (TempDir, StateId, LocalDiscussionService) {
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
        (temp, state.state_id, svc)
    }

    fn open_request(state_id: &StateId, body: &str, op_id: &str) -> OpenDiscussionRequest {
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
    async fn open_discussion_serializes_concurrent_appends() {
        let (_t, state_id, svc) = fresh_service();
        let op_a = objects::object::OperationId::new().to_string();
        let op_b = objects::object::OperationId::new().to_string();
        let mut req_a = open_request(&state_id, "body a", &op_a);
        req_a.anchor.as_mut().unwrap().symbol = "sym_a".into();
        let mut req_b = open_request(&state_id, "body b", &op_b);
        req_b.anchor.as_mut().unwrap().symbol = "sym_b".into();

        let svc_a = svc.clone();
        let svc_b = svc.clone();
        let (a, b) = tokio::join!(
            svc_a.open_discussion(Request::new(req_a)),
            svc_b.open_discussion(Request::new(req_b)),
        );
        a.expect("first open_discussion");
        b.expect("second open_discussion");

        let listed = svc
            .list_by_state(Request::new(ListDiscussionsByStateRequest {
                repo_path: String::new(),
                state_id: state_id.as_bytes().to_vec(),
                status: "all".into(),
            }))
            .await
            .expect("list_by_state");
        assert_eq!(
            listed.get_ref().discussions.len(),
            2,
            "both concurrent discussions must land — neither should be lost \
             to a stale-blob clobber"
        );
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn open_rejects_labelled_visibility_tiers_without_labels() {
        let (_t, state_id, svc) = fresh_service();

        for visibility in [
            "team",
            "team:",
            "team_scoped",
            "restricted",
            "restricted:",
            "private",
            "private:",
            "unknown",
        ] {
            let mut req = open_request(&state_id, "hello", "");
            req.visibility = visibility.into();
            let err = svc.open_discussion(Request::new(req)).await.unwrap_err();
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
        }
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn open_round_trips_supported_visibility_tiers() {
        let (_t, state_id, svc) = fresh_service();

        for (visibility, expected) in [
            ("", "public"),
            ("public", "public"),
            ("internal", "internal"),
            ("team:platform", "team:platform"),
            ("restricted:legal", "restricted:legal"),
            ("private:embargo-x", "private:embargo-x"),
        ] {
            let mut req = open_request(&state_id, "hello", "");
            req.visibility = visibility.into();
            let opened = svc
                .open_discussion(Request::new(req))
                .await
                .unwrap()
                .into_inner();
            assert_eq!(opened.visibility, expected);
        }
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn open_empty_visibility_uses_repo_discussion_default() {
        let temp = TempDir::new().unwrap();
        Repository::init_default(temp.path()).unwrap();
        std::fs::write(
            temp.path().join(".heddle/config.toml"),
            "[repository]\nversion = 1\n\n[review.discussion]\ndefault_visibility = \"Internal\"\n",
        )
        .unwrap();
        let repo = Repository::open(temp.path()).unwrap();
        let attribution = Attribution::human(Principal::new("Tester", "tester@example.com"));
        let state = repo
            .snapshot_with_attribution(Some("seed".into()), None, attribution)
            .unwrap();
        let dedup = OperationDedupStore::open(repo.heddle_dir()).unwrap();
        let inner = GrpcLocalService::new(Arc::new(repo), Arc::new(dedup));
        let svc = LocalDiscussionService::new(inner);

        let opened = svc
            .open_discussion(Request::new(open_request(&state.state_id, "hello", "")))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(opened.visibility, "internal");
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
    async fn resolve_into_annotation_creates_context_and_resolves_discussion() {
        let (_t, state_id, svc) = fresh_service();
        let opened = svc
            .open_discussion(Request::new(open_request(&state_id, "why?", "")))
            .await
            .unwrap()
            .into_inner();

        use grpc::heddle::v1::resolve_discussion_request::{Resolution, ResolveIntoAnnotation};
        let resolved = svc
            .resolve_discussion(Request::new(ResolveDiscussionRequest {
                repo_path: String::new(),
                discussion_id: opened.id.clone(),
                resolution: Some(Resolution::IntoAnnotation(ResolveIntoAnnotation {
                    kind: ContextAnnotationKind::Rationale as i32,
                    content: "capture this".into(),
                    tags: vec!["todo".into()],
                })),
                client_operation_id: String::new(),
            }))
            .await
            .unwrap()
            .into_inner();
        let annotation_id = resolved.resolved_annotation_id.clone();
        assert!(
            !annotation_id.is_empty(),
            "into-annotation resolution should return the created annotation id"
        );

        let repo = svc.inner.repo();
        let head_id = repo.head().unwrap().unwrap();
        assert_eq!(head_id, state_id);
        let head = repo.store().get_state(&head_id).unwrap().unwrap();
        let context_attachment = repo
            .latest_state_attachment(&head_id, StateAttachmentKind::Context)
            .unwrap()
            .expect("state should carry a context attachment");
        let StateAttachmentBody::Context(context_root) = context_attachment.body else {
            panic!("expected context attachment")
        };
        let (target, context, index) = repo
            .find_annotation(&context_root, &annotation_id)
            .unwrap()
            .expect("created annotation should be indexed in the context tree");
        assert_eq!(target.path(), Some("src/lib.rs"));
        let annotation = &context.annotations[index];
        assert_eq!(
            annotation.resolved_from_discussion.as_deref(),
            Some(opened.id.as_str())
        );
        assert_eq!(
            annotation.current_revision().unwrap().content,
            "capture this"
        );
        assert_eq!(
            annotation.current_revision().unwrap().tags,
            vec!["todo".to_string()]
        );

        let discussion_blob = decode_blob_for_state(repo, &head).unwrap();
        let stored = discussion_blob
            .discussions
            .iter()
            .find(|discussion| discussion.id == opened.id)
            .expect("resolved discussion should still be present on new HEAD");
        assert_eq!(
            stored.resolved_annotation_id.as_deref(),
            Some(annotation_id.as_str())
        );
        assert!(matches!(
            stored.resolution,
            DiscussionResolution::ResolvedIntoAnnotation { .. }
        ));
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn list_by_symbol_finds_reachable_discussions() {
        let (_t, state_id, svc) = fresh_service();
        let opened = svc
            .open_discussion(Request::new(open_request(&state_id, "symbol thread", "")))
            .await
            .unwrap()
            .into_inner();

        let listed = svc
            .list_by_symbol(Request::new(ListDiscussionsBySymbolRequest {
                repo_path: String::new(),
                anchor: Some(PathSymbolRef {
                    file: "src/lib.rs".into(),
                    symbol: "foo".into(),
                }),
                status: "all".into(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(listed.discussions.len(), 1);
        assert_eq!(listed.discussions[0].id, opened.id);
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn get_discussion_without_state_scans_reachable_discussions() {
        let (temp, state_id, svc) = fresh_service();
        std::fs::write(temp.path().join("later.txt"), "later\n").unwrap();
        svc.inner
            .repo()
            .snapshot(Some("later".into()), None)
            .expect("advance HEAD");

        let opened = svc
            .open_discussion(Request::new(open_request(&state_id, "old state", "")))
            .await
            .unwrap()
            .into_inner();

        let fetched = svc
            .get_discussion(Request::new(GetDiscussionRequest {
                repo_path: String::new(),
                discussion_id: opened.id.clone(),
                state_id: Vec::new(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(fetched.id, opened.id);
        assert_eq!(fetched.turns[0].body, "old state");
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
