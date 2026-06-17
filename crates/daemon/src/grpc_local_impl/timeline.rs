// SPDX-License-Identifier: Apache-2.0
//! Local gRPC service for agent timeline operation objects.

use std::time::{SystemTime, UNIX_EPOCH};

use grpc::heddle::v1::{
    AgentTimelineBranchCreated, AgentTimelineCursorMoved, AgentTimelineNativeToolCall,
    AgentTimelineOperationDraft, AgentTimelineOperationRecord, AgentTimelineStateSummary,
    AgentTimelineStatus, AgentTimelineStepSummary, AgentTimelineToolCallFinished,
    AgentTimelineToolCallStarted, AgentTimelineToolPayload, CreateTimelineBranchRequest,
    CreateTimelineBranchResponse, GetTimelineOperationRequest, GetTimelineStatusRequest,
    ListTimelineStepsRequest, ListTimelineStepsResponse, RecordTimelineOperationRequest,
    ResolveNativeToolCallRequest, SeekTimelineToNativeToolCallRequest, SeekTimelineToStepRequest,
    TimelineCursorMoveResponse, TimelineCursorRequest, agent_timeline_operation_draft,
    agent_timeline_operation_record, timeline_service_server::TimelineService,
};
use objects::object::{
    BranchCreatedV1, ChangeId, ContentHash, CursorMovedV1, NativeToolCallRefV1, TimelineBranchId,
    TimelineBranchReason, TimelineCursorMoveReason, TimelineLabel, TimelineOperationBodyV1,
    TimelineOperationEnvelope, TimelineOperationId, TimelineStepId, TimelineToolCallStatus,
    TimelineToolPayloadMetadata, ToolCallFinishedV1, ToolCallStartedV1,
};
use prost::Message;
use repo::{
    TimelineCursorMoveRecord, TimelineNativeToolKey, TimelineSeekTarget, TimelineStepSummary,
    TimelineStore, TimelineThreadStatus, TimelineView,
};
use tonic::{Request, Response, Status};

use super::{GrpcLocalService, to_status, with_idempotency};

#[derive(Clone)]
pub struct LocalTimelineService {
    inner: GrpcLocalService,
}

impl LocalTimelineService {
    pub fn new(inner: GrpcLocalService) -> Self {
        Self { inner }
    }
}

#[tonic::async_trait]
impl TimelineService for LocalTimelineService {
    async fn record_operation(
        &self,
        request: Request<RecordTimelineOperationRequest>,
    ) -> Result<Response<AgentTimelineOperationRecord>, Status> {
        let req = request.into_inner();
        let body = req.encode_to_vec();
        let client_op = req.client_operation_id.clone();
        let inner = self.inner.clone();

        let response = with_idempotency(
            &self.inner,
            &client_op,
            "TimelineService.RecordOperation",
            &body,
            move || async move {
                let draft = req
                    .operation
                    .ok_or_else(|| Status::invalid_argument("operation is required"))?;
                let envelope = draft_to_envelope(draft)?;
                let bytes = envelope
                    .encode()
                    .map_err(|err| Status::invalid_argument(err.to_string()))?;
                let store = TimelineStore::open(inner.repo().heddle_dir()).map_err(to_status)?;
                let id = store.write_operation_bytes(&bytes).map_err(to_status)?;
                record_from_envelope(id, envelope, bytes)
            },
        )
        .await?;

        Ok(Response::new(response))
    }

    async fn get_operation(
        &self,
        request: Request<GetTimelineOperationRequest>,
    ) -> Result<Response<AgentTimelineOperationRecord>, Status> {
        let req = request.into_inner();
        let id = TimelineOperationId::try_from_slice(&req.operation_id)
            .map_err(|err| Status::invalid_argument(format!("invalid operation_id: {err}")))?;
        let store = TimelineStore::open(self.inner.repo().heddle_dir()).map_err(to_status)?;
        let bytes = store
            .read_operation_bytes(&id)
            .map_err(to_status)?
            .ok_or_else(|| Status::not_found(format!("timeline operation {}", id.short())))?;
        let envelope = TimelineOperationEnvelope::decode(&bytes)
            .map_err(|err| Status::internal(format!("decode stored timeline operation: {err}")))?;
        Ok(Response::new(record_from_envelope(id, envelope, bytes)?))
    }

    async fn get_timeline_status(
        &self,
        request: Request<GetTimelineStatusRequest>,
    ) -> Result<Response<AgentTimelineStatus>, Status> {
        let req = request.into_inner();
        let (_store, view) = open_timeline_store_and_view(&self.inner)?;
        Ok(Response::new(status_for_thread(&view, &req.thread)))
    }

    async fn list_timeline_steps(
        &self,
        request: Request<ListTimelineStepsRequest>,
    ) -> Result<Response<ListTimelineStepsResponse>, Status> {
        let req = request.into_inner();
        let (_store, view) = open_timeline_store_and_view(&self.inner)?;
        let branch_id = if req.branch_id.is_empty() {
            view.status(&req.thread)
                .and_then(|status| status.current_branch_id.clone())
        } else {
            Some(TimelineBranchId::new(req.branch_id.clone()))
        };
        let mut steps = branch_id
            .as_ref()
            .map(|branch_id| {
                view.list_branch_steps(&req.thread, branch_id)
                    .into_iter()
                    .map(step_summary_to_proto)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if req.limit > 0 && steps.len() > req.limit as usize {
            let keep_from = steps.len() - req.limit as usize;
            steps = steps.split_off(keep_from);
        }
        Ok(Response::new(ListTimelineStepsResponse {
            steps,
            status: Some(status_for_thread(&view, &req.thread)),
        }))
    }

    async fn resolve_native_tool_call(
        &self,
        request: Request<ResolveNativeToolCallRequest>,
    ) -> Result<Response<AgentTimelineStepSummary>, Status> {
        let req = request.into_inner();
        let (_store, view) = open_timeline_store_and_view(&self.inner)?;
        let native = native_key_from_resolve_request(&req);
        let step = view
            .find_step_by_native_call(&req.thread, &native)
            .ok_or_else(|| Status::not_found("native tool call not found"))?;
        Ok(Response::new(step_summary_to_proto(step)))
    }

    async fn seek_to_step(
        &self,
        request: Request<SeekTimelineToStepRequest>,
    ) -> Result<Response<TimelineCursorMoveResponse>, Status> {
        let req = request.into_inner();
        let body = req.encode_to_vec();
        let client_op = req.client_operation_id.clone();
        let inner = self.inner.clone();

        let response = with_idempotency(
            &self.inner,
            &client_op,
            "TimelineService.SeekToStep",
            &body,
            move || async move { seek_to_step_impl(&inner, req).await },
        )
        .await?;

        Ok(Response::new(response))
    }

    async fn seek_to_native_tool_call(
        &self,
        request: Request<SeekTimelineToNativeToolCallRequest>,
    ) -> Result<Response<TimelineCursorMoveResponse>, Status> {
        let req = request.into_inner();
        let body = req.encode_to_vec();
        let client_op = req.client_operation_id.clone();
        let inner = self.inner.clone();

        let response = with_idempotency(
            &self.inner,
            &client_op,
            "TimelineService.SeekToNativeToolCall",
            &body,
            move || async move { seek_to_native_tool_call_impl(&inner, req).await },
        )
        .await?;

        Ok(Response::new(response))
    }

    async fn undo_tool_call(
        &self,
        request: Request<TimelineCursorRequest>,
    ) -> Result<Response<TimelineCursorMoveResponse>, Status> {
        let req = request.into_inner();
        let body = req.encode_to_vec();
        let client_op = req.client_operation_id.clone();
        let inner = self.inner.clone();

        let response = with_idempotency(
            &self.inner,
            &client_op,
            "TimelineService.UndoToolCall",
            &body,
            move || async move { move_cursor_by_delta_impl(&inner, req, -1).await },
        )
        .await?;

        Ok(Response::new(response))
    }

    async fn redo_tool_call(
        &self,
        request: Request<TimelineCursorRequest>,
    ) -> Result<Response<TimelineCursorMoveResponse>, Status> {
        let req = request.into_inner();
        let body = req.encode_to_vec();
        let client_op = req.client_operation_id.clone();
        let inner = self.inner.clone();

        let response = with_idempotency(
            &self.inner,
            &client_op,
            "TimelineService.RedoToolCall",
            &body,
            move || async move { move_cursor_by_delta_impl(&inner, req, 1).await },
        )
        .await?;

        Ok(Response::new(response))
    }

    async fn create_timeline_branch(
        &self,
        request: Request<CreateTimelineBranchRequest>,
    ) -> Result<Response<CreateTimelineBranchResponse>, Status> {
        let req = request.into_inner();
        let body = req.encode_to_vec();
        let client_op = req.client_operation_id.clone();
        let inner = self.inner.clone();

        let response = with_idempotency(
            &self.inner,
            &client_op,
            "TimelineService.CreateTimelineBranch",
            &body,
            move || async move { create_timeline_branch_impl(&inner, req).await },
        )
        .await?;

        Ok(Response::new(response))
    }
}

async fn seek_to_step_impl(
    inner: &GrpcLocalService,
    req: SeekTimelineToStepRequest,
) -> Result<TimelineCursorMoveResponse, Status> {
    let (store, view) = open_timeline_store_and_view(inner)?;
    let target = view
        .resolve_seek_target(&req.thread, &TimelineStepId::new(req.step_id.clone()))
        .ok_or_else(|| Status::not_found("timeline step not found"))?;
    if !req.branch_id.is_empty() && target.branch_id.as_str() != req.branch_id {
        return Err(Status::failed_precondition(
            "timeline step belongs to a different branch",
        ));
    }
    write_cursor_move(&store, &target, &view, parse_seek_reason(&req.reason)?)
}

async fn seek_to_native_tool_call_impl(
    inner: &GrpcLocalService,
    req: SeekTimelineToNativeToolCallRequest,
) -> Result<TimelineCursorMoveResponse, Status> {
    let (store, view) = open_timeline_store_and_view(inner)?;
    let target = view
        .resolve_seek_to_native_call(&req.thread, &native_key_from_parts(&req))
        .ok_or_else(|| Status::not_found("native tool call not found"))?
        .clone();
    write_cursor_move(&store, &target, &view, parse_seek_reason(&req.reason)?)
}

async fn move_cursor_by_delta_impl(
    inner: &GrpcLocalService,
    req: TimelineCursorRequest,
    delta: i32,
) -> Result<TimelineCursorMoveResponse, Status> {
    let (store, view) = open_timeline_store_and_view(inner)?;
    if !req.branch_id.is_empty()
        && view
            .status(&req.thread)
            .and_then(|status| status.current_branch_id.as_ref())
            .is_some_and(|branch_id| branch_id.as_str() != req.branch_id)
    {
        return Err(Status::failed_precondition(
            "timeline cursor is on a different branch",
        ));
    }
    let target = if delta < 0 {
        view.resolve_undo_target(&req.thread)
    } else {
        view.resolve_redo_target(&req.thread)
    }
    .ok_or_else(|| Status::failed_precondition("timeline cursor is not initialized"))?;
    write_cursor_move(
        &store,
        &target,
        &view,
        if delta < 0 {
            TimelineCursorMoveReason::Undo
        } else {
            TimelineCursorMoveReason::Redo
        },
    )
}

async fn create_timeline_branch_impl(
    inner: &GrpcLocalService,
    req: CreateTimelineBranchRequest,
) -> Result<CreateTimelineBranchResponse, Status> {
    let (store, view) = open_timeline_store_and_view(inner)?;
    let target = if req.from_step_id.is_empty() {
        let status = view.status(&req.thread).ok_or_else(|| {
            Status::failed_precondition("from_step_id is required when the cursor has no step")
        })?;
        let branch_id = status.current_branch_id.clone().ok_or_else(|| {
            Status::failed_precondition("from_step_id is required when the cursor has no branch")
        })?;
        let state = status.current_state.ok_or_else(|| {
            Status::failed_precondition("from_step_id is required when the cursor has no state")
        })?;
        TimelineSeekTarget {
            thread: req.thread.clone(),
            branch_id,
            step_id: status.current_step_id.clone(),
            state,
        }
    } else {
        view.resolve_seek_target(&req.thread, &TimelineStepId::new(req.from_step_id.clone()))
            .ok_or_else(|| Status::not_found("timeline step not found"))?
    };
    let branch_id = if req.branch_id.is_empty() {
        TimelineBranchId::generate()
    } else {
        TimelineBranchId::new(req.branch_id)
    };
    let body = BranchCreatedV1 {
        thread: req.thread.clone(),
        branch_id: branch_id.clone(),
        parent_branch_id: Some(target.branch_id.clone()),
        from_step_id: target.step_id.clone(),
        from_state: target.state,
        reason: parse_branch_reason_or_default(&req.reason)?,
        created_at_ms: now_ms(),
    };
    let record = write_timeline_envelope(
        &store,
        TimelineOperationEnvelope::new(TimelineOperationBodyV1::BranchCreated(body), Vec::new()),
    )?;
    let view = TimelineView::rebuild(&store).map_err(to_status)?;
    Ok(CreateTimelineBranchResponse {
        status: Some(status_for_thread(&view, &req.thread)),
        branch_id: branch_id.to_string(),
        parent_branch_id: target.branch_id.to_string(),
        from_step_id: target.step_id.map(|id| id.to_string()).unwrap_or_default(),
        operation: Some(record),
    })
}

fn write_cursor_move(
    store: &TimelineStore,
    target: &TimelineSeekTarget,
    view: &TimelineView,
    reason: TimelineCursorMoveReason,
) -> Result<TimelineCursorMoveResponse, Status> {
    let status = view.status(&target.thread);
    let from_step_id = status.and_then(|status| status.current_step_id.clone());
    let from_state = status
        .and_then(|status| status.current_state)
        .unwrap_or(target.state);
    let id = store
        .record_cursor_move(TimelineCursorMoveRecord {
            thread: target.thread.clone(),
            branch_id: target.branch_id.clone(),
            from_step_id,
            to_step_id: target.step_id.clone(),
            from_state,
            to_state: target.state,
            reason,
            moved_at_ms: now_ms(),
            labels: Vec::new(),
        })
        .map_err(to_status)?;
    let record = record_from_store(store, id)?;
    let view = TimelineView::rebuild(store).map_err(to_status)?;
    Ok(TimelineCursorMoveResponse {
        status: Some(status_for_thread(&view, &target.thread)),
        operation: Some(record),
    })
}

fn write_timeline_envelope(
    store: &TimelineStore,
    envelope: TimelineOperationEnvelope,
) -> Result<AgentTimelineOperationRecord, Status> {
    let bytes = envelope
        .encode()
        .map_err(|err| Status::invalid_argument(err.to_string()))?;
    let id = store.write_operation_bytes(&bytes).map_err(to_status)?;
    record_from_envelope(id, envelope, bytes)
}

fn parse_seek_reason(value: &str) -> Result<TimelineCursorMoveReason, Status> {
    if value.is_empty() || value == "seek-step" {
        return Ok(TimelineCursorMoveReason::SeekToolCall);
    }
    parse_cursor_reason(value)
}

fn parse_branch_reason_or_default(value: &str) -> Result<TimelineBranchReason, Status> {
    if value.is_empty() {
        return Ok(TimelineBranchReason::ExplicitFork);
    }
    parse_branch_reason(value)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

fn open_timeline_store_and_view(
    inner: &GrpcLocalService,
) -> Result<(TimelineStore, TimelineView), Status> {
    let store = TimelineStore::open(inner.repo().heddle_dir()).map_err(to_status)?;
    let view = TimelineView::rebuild(&store).map_err(to_status)?;
    Ok((store, view))
}

fn record_from_store(
    store: &TimelineStore,
    id: TimelineOperationId,
) -> Result<AgentTimelineOperationRecord, Status> {
    let bytes = store
        .read_operation_bytes(&id)
        .map_err(to_status)?
        .ok_or_else(|| {
            Status::internal(format!(
                "timeline operation {} was not persisted",
                id.short()
            ))
        })?;
    let envelope = TimelineOperationEnvelope::decode(&bytes)
        .map_err(|err| Status::internal(format!("decode stored timeline operation: {err}")))?;
    record_from_envelope(id, envelope, bytes)
}

fn status_for_thread(view: &TimelineView, thread: &str) -> AgentTimelineStatus {
    let status = view.status(thread);
    AgentTimelineStatus {
        thread: thread.to_string(),
        current_branch_id: status
            .and_then(|status| status.current_branch_id.as_ref())
            .map(|id| id.to_string())
            .unwrap_or_default(),
        current_step_id: status
            .and_then(|status| status.current_step_id.as_ref())
            .map(|id| id.to_string())
            .unwrap_or_default(),
        current_state: status.and_then(|status| state_summary_for_status(view, status)),
        branch_count: view.branch_count(thread) as u32,
        step_count: view.step_count(thread) as u32,
    }
}

fn state_summary_for_status(
    view: &TimelineView,
    status: &TimelineThreadStatus,
) -> Option<AgentTimelineStateSummary> {
    let state = status.current_state?;
    let step = status
        .current_step_id
        .as_ref()
        .and_then(|step_id| view.step(&status.thread, step_id));
    Some(AgentTimelineStateSummary {
        state_id: state.as_bytes().to_vec(),
        display_id: state.to_string_full(),
        source_step_id: status
            .current_step_id
            .as_ref()
            .map(|id| id.to_string())
            .unwrap_or_default(),
        payload: step.and_then(step_payload_to_proto),
    })
}

fn step_summary_to_proto(step: &TimelineStepSummary) -> AgentTimelineStepSummary {
    AgentTimelineStepSummary {
        thread: step.thread.clone(),
        step_id: step.step_id.to_string(),
        branch_id: step.branch_id.to_string(),
        parent_step_id: step
            .parent_step_id
            .as_ref()
            .map(|id| id.to_string())
            .unwrap_or_default(),
        native: step.native.clone().map(native_to_proto),
        tool_name: step.tool_name.clone().unwrap_or_default(),
        status: step
            .status
            .as_ref()
            .map(tool_call_status_to_wire)
            .unwrap_or_default()
            .to_string(),
        changed: step.changed.unwrap_or(false),
        touched_paths: step.touched_paths.clone(),
        before_state: step
            .before_state
            .map(|state| state.as_bytes().to_vec())
            .unwrap_or_default(),
        after_state: step
            .after_state
            .map(|state| state.as_bytes().to_vec())
            .unwrap_or_default(),
        capture_state: step
            .capture_state
            .map(|state| state.as_bytes().to_vec())
            .unwrap_or_default(),
        payload: step_payload_to_proto(step),
        labels: step.labels.iter().map(label_to_wire).collect(),
        started_at_ms: step.started_at_ms.unwrap_or_default(),
        finished_at_ms: step.finished_at_ms.unwrap_or_default(),
        operation_ids: step
            .operation_ids
            .iter()
            .map(|id| id.as_bytes().to_vec())
            .collect(),
        operation_display_ids: step
            .operation_ids
            .iter()
            .map(TimelineOperationId::to_string_full)
            .collect(),
        capture_oplog_batch_id: step.capture_oplog_batch_id,
    }
}

fn step_payload_to_proto(step: &TimelineStepSummary) -> Option<AgentTimelineToolPayload> {
    if step.payload_summary.is_none() && step.payload_hash.is_none() {
        return None;
    }
    Some(AgentTimelineToolPayload {
        summary: step.payload_summary.clone().unwrap_or_default(),
        hash: step
            .payload_hash
            .map(|hash| hash.as_bytes().to_vec())
            .unwrap_or_default(),
    })
}

fn native_key_from_parts(req: &SeekTimelineToNativeToolCallRequest) -> TimelineNativeToolKey {
    TimelineNativeToolKey {
        harness: req.harness.clone(),
        session_id: non_empty(req.session_id.clone()),
        message_id: non_empty(req.message_id.clone()),
        tool_call_id: req.tool_call_id.clone(),
    }
}

fn native_key_from_resolve_request(req: &ResolveNativeToolCallRequest) -> TimelineNativeToolKey {
    TimelineNativeToolKey {
        harness: req.harness.clone(),
        session_id: non_empty(req.session_id.clone()),
        message_id: non_empty(req.message_id.clone()),
        tool_call_id: req.tool_call_id.clone(),
    }
}

fn draft_to_envelope(
    draft: AgentTimelineOperationDraft,
) -> Result<TimelineOperationEnvelope, Status> {
    let labels = draft
        .labels
        .iter()
        .map(|label| parse_label(label))
        .collect::<Result<Vec<_>, _>>()?;
    let body = match draft
        .body
        .ok_or_else(|| Status::invalid_argument("operation body is required"))?
    {
        agent_timeline_operation_draft::Body::ToolCallStarted(body) => {
            TimelineOperationBodyV1::ToolCallStarted(tool_call_started_from_proto(body)?)
        }
        agent_timeline_operation_draft::Body::ToolCallFinished(body) => {
            TimelineOperationBodyV1::ToolCallFinished(tool_call_finished_from_proto(body)?)
        }
        agent_timeline_operation_draft::Body::CursorMoved(body) => {
            TimelineOperationBodyV1::CursorMoved(cursor_moved_from_proto(body)?)
        }
        agent_timeline_operation_draft::Body::BranchCreated(body) => {
            TimelineOperationBodyV1::BranchCreated(branch_created_from_proto(body)?)
        }
    };
    Ok(TimelineOperationEnvelope::new(body, labels))
}

fn record_from_envelope(
    id: TimelineOperationId,
    envelope: TimelineOperationEnvelope,
    bytes: Vec<u8>,
) -> Result<AgentTimelineOperationRecord, Status> {
    let body = match envelope.body {
        TimelineOperationBodyV1::ToolCallStarted(body) => {
            agent_timeline_operation_record::Body::ToolCallStarted(tool_call_started_to_proto(body))
        }
        TimelineOperationBodyV1::ToolCallFinished(body) => {
            agent_timeline_operation_record::Body::ToolCallFinished(tool_call_finished_to_proto(
                body,
            ))
        }
        TimelineOperationBodyV1::CursorMoved(body) => {
            agent_timeline_operation_record::Body::CursorMoved(cursor_moved_to_proto(body))
        }
        TimelineOperationBodyV1::BranchCreated(body) => {
            agent_timeline_operation_record::Body::BranchCreated(branch_created_to_proto(body))
        }
    };
    Ok(AgentTimelineOperationRecord {
        operation_id: id.as_bytes().to_vec(),
        display_id: id.to_string_full(),
        schema_version: envelope.schema_version.into(),
        kind: envelope.kind.as_str().to_string(),
        labels: envelope.labels.iter().map(label_to_wire).collect(),
        body: Some(body),
        envelope: bytes,
    })
}

fn tool_call_started_from_proto(
    body: AgentTimelineToolCallStarted,
) -> Result<ToolCallStartedV1, Status> {
    Ok(ToolCallStartedV1 {
        thread: body.thread,
        step_id: TimelineStepId::new(body.step_id),
        branch_id: TimelineBranchId::new(body.branch_id),
        parent_step_id: optional_step_id(body.parent_step_id),
        native: native_from_proto(body.native)?,
        tool_name: body.tool_name,
        before_state: change_id_from_bytes(&body.before_state, "before_state")?,
        payload: payload_from_proto(body.payload)?,
        started_at_ms: body.started_at_ms,
    })
}

fn tool_call_finished_from_proto(
    body: AgentTimelineToolCallFinished,
) -> Result<ToolCallFinishedV1, Status> {
    Ok(ToolCallFinishedV1 {
        thread: body.thread,
        step_id: TimelineStepId::new(body.step_id),
        branch_id: TimelineBranchId::new(body.branch_id),
        native: native_from_proto(body.native)?,
        status: parse_tool_call_status(&body.status)?,
        before_state: change_id_from_bytes(&body.before_state, "before_state")?,
        after_state: change_id_from_bytes(&body.after_state, "after_state")?,
        capture_state: optional_change_id(body.capture_state, "capture_state")?,
        capture_oplog_batch_id: body.capture_oplog_batch_id,
        changed: body.changed,
        touched_paths: body.touched_paths,
        payload: payload_from_proto(body.payload)?,
        finished_at_ms: body.finished_at_ms,
    })
}

fn cursor_moved_from_proto(body: AgentTimelineCursorMoved) -> Result<CursorMovedV1, Status> {
    Ok(CursorMovedV1 {
        thread: body.thread,
        branch_id: TimelineBranchId::new(body.branch_id),
        from_step_id: optional_step_id(body.from_step_id),
        to_step_id: optional_step_id(body.to_step_id),
        from_state: change_id_from_bytes(&body.from_state, "from_state")?,
        to_state: change_id_from_bytes(&body.to_state, "to_state")?,
        reason: parse_cursor_reason(&body.reason)?,
        moved_at_ms: body.moved_at_ms,
    })
}

fn branch_created_from_proto(body: AgentTimelineBranchCreated) -> Result<BranchCreatedV1, Status> {
    Ok(BranchCreatedV1 {
        thread: body.thread,
        branch_id: TimelineBranchId::new(body.branch_id),
        parent_branch_id: optional_branch_id(body.parent_branch_id),
        from_step_id: optional_step_id(body.from_step_id),
        from_state: change_id_from_bytes(&body.from_state, "from_state")?,
        reason: parse_branch_reason(&body.reason)?,
        created_at_ms: body.created_at_ms,
    })
}

fn tool_call_started_to_proto(body: ToolCallStartedV1) -> AgentTimelineToolCallStarted {
    AgentTimelineToolCallStarted {
        thread: body.thread,
        step_id: body.step_id.to_string(),
        branch_id: body.branch_id.to_string(),
        parent_step_id: body
            .parent_step_id
            .map(|id| id.to_string())
            .unwrap_or_default(),
        native: Some(native_to_proto(body.native)),
        tool_name: body.tool_name,
        before_state: body.before_state.as_bytes().to_vec(),
        payload: payload_to_proto(body.payload),
        started_at_ms: body.started_at_ms,
    }
}

fn tool_call_finished_to_proto(body: ToolCallFinishedV1) -> AgentTimelineToolCallFinished {
    AgentTimelineToolCallFinished {
        thread: body.thread,
        step_id: body.step_id.to_string(),
        branch_id: body.branch_id.to_string(),
        native: Some(native_to_proto(body.native)),
        status: tool_call_status_to_wire(&body.status).to_string(),
        before_state: body.before_state.as_bytes().to_vec(),
        after_state: body.after_state.as_bytes().to_vec(),
        capture_state: body
            .capture_state
            .map(|state| state.as_bytes().to_vec())
            .unwrap_or_default(),
        capture_oplog_batch_id: body.capture_oplog_batch_id,
        changed: body.changed,
        touched_paths: body.touched_paths,
        payload: payload_to_proto(body.payload),
        finished_at_ms: body.finished_at_ms,
    }
}

fn cursor_moved_to_proto(body: CursorMovedV1) -> AgentTimelineCursorMoved {
    AgentTimelineCursorMoved {
        thread: body.thread,
        branch_id: body.branch_id.to_string(),
        from_step_id: body
            .from_step_id
            .map(|id| id.to_string())
            .unwrap_or_default(),
        to_step_id: body.to_step_id.map(|id| id.to_string()).unwrap_or_default(),
        from_state: body.from_state.as_bytes().to_vec(),
        to_state: body.to_state.as_bytes().to_vec(),
        reason: cursor_reason_to_wire(&body.reason).to_string(),
        moved_at_ms: body.moved_at_ms,
    }
}

fn branch_created_to_proto(body: BranchCreatedV1) -> AgentTimelineBranchCreated {
    AgentTimelineBranchCreated {
        thread: body.thread,
        branch_id: body.branch_id.to_string(),
        parent_branch_id: body
            .parent_branch_id
            .map(|id| id.to_string())
            .unwrap_or_default(),
        from_step_id: body
            .from_step_id
            .map(|id| id.to_string())
            .unwrap_or_default(),
        from_state: body.from_state.as_bytes().to_vec(),
        reason: branch_reason_to_wire(&body.reason).to_string(),
        created_at_ms: body.created_at_ms,
    }
}

fn optional_step_id(value: String) -> Option<TimelineStepId> {
    (!value.is_empty()).then(|| TimelineStepId::new(value))
}

fn optional_branch_id(value: String) -> Option<TimelineBranchId> {
    (!value.is_empty()).then(|| TimelineBranchId::new(value))
}

fn native_from_proto(
    native: Option<AgentTimelineNativeToolCall>,
) -> Result<NativeToolCallRefV1, Status> {
    let native = native.ok_or_else(|| Status::invalid_argument("native tool call is required"))?;
    if native.harness.trim().is_empty() {
        return Err(Status::invalid_argument("native.harness must not be empty"));
    }
    if native.tool_call_id.trim().is_empty() {
        return Err(Status::invalid_argument(
            "native.tool_call_id must not be empty",
        ));
    }
    Ok(NativeToolCallRefV1 {
        harness: native.harness,
        session_id: non_empty(native.session_id),
        message_id: non_empty(native.message_id),
        tool_call_id: native.tool_call_id,
    })
}

fn native_to_proto(native: NativeToolCallRefV1) -> AgentTimelineNativeToolCall {
    AgentTimelineNativeToolCall {
        harness: native.harness,
        session_id: native.session_id.unwrap_or_default(),
        message_id: native.message_id.unwrap_or_default(),
        tool_call_id: native.tool_call_id,
    }
}

fn payload_from_proto(
    payload: Option<AgentTimelineToolPayload>,
) -> Result<Option<TimelineToolPayloadMetadata>, Status> {
    let Some(payload) = payload else {
        return Ok(None);
    };
    Ok(Some(TimelineToolPayloadMetadata {
        summary: non_empty(payload.summary),
        hash: optional_content_hash(payload.hash, "payload.hash")?,
    }))
}

fn payload_to_proto(
    payload: Option<TimelineToolPayloadMetadata>,
) -> Option<AgentTimelineToolPayload> {
    payload.map(|payload| AgentTimelineToolPayload {
        summary: payload.summary.unwrap_or_default(),
        hash: payload
            .hash
            .map(|hash| hash.as_bytes().to_vec())
            .unwrap_or_default(),
    })
}

fn change_id_from_bytes(bytes: &[u8], field: &str) -> Result<ChangeId, Status> {
    ChangeId::try_from_slice(bytes)
        .map_err(|err| Status::invalid_argument(format!("invalid {field}: {err}")))
}

fn optional_change_id(bytes: Vec<u8>, field: &str) -> Result<Option<ChangeId>, Status> {
    if bytes.is_empty() {
        return Ok(None);
    }
    change_id_from_bytes(&bytes, field).map(Some)
}

fn optional_content_hash(bytes: Vec<u8>, field: &str) -> Result<Option<ContentHash>, Status> {
    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.len() != 32 {
        return Err(Status::invalid_argument(format!(
            "invalid {field}: expected 32 bytes"
        )));
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Ok(Some(ContentHash::from_bytes(arr)))
}

fn non_empty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

fn parse_label(value: &str) -> Result<TimelineLabel, Status> {
    match value {
        "repo-reversible" => Ok(TimelineLabel::RepoReversible),
        "external-side-effects-unknown" => Ok(TimelineLabel::ExternalSideEffectsUnknown),
        "ignored-path-touched" => Ok(TimelineLabel::IgnoredPathTouched),
        "outside-repo-touched" => Ok(TimelineLabel::OutsideRepoTouched),
        "purge-boundary" => Ok(TimelineLabel::PurgeBoundary),
        "capture-failed" => Ok(TimelineLabel::CaptureFailed),
        other => Err(Status::invalid_argument(format!(
            "unknown timeline label '{other}'"
        ))),
    }
}

fn label_to_wire(label: &TimelineLabel) -> String {
    match label {
        TimelineLabel::RepoReversible => "repo-reversible",
        TimelineLabel::ExternalSideEffectsUnknown => "external-side-effects-unknown",
        TimelineLabel::IgnoredPathTouched => "ignored-path-touched",
        TimelineLabel::OutsideRepoTouched => "outside-repo-touched",
        TimelineLabel::PurgeBoundary => "purge-boundary",
        TimelineLabel::CaptureFailed => "capture-failed",
    }
    .to_string()
}

fn parse_tool_call_status(value: &str) -> Result<TimelineToolCallStatus, Status> {
    match value {
        "succeeded" => Ok(TimelineToolCallStatus::Succeeded),
        "failed" => Ok(TimelineToolCallStatus::Failed),
        "cancelled" | "canceled" => Ok(TimelineToolCallStatus::Cancelled),
        other => Err(Status::invalid_argument(format!(
            "unknown timeline tool call status '{other}'"
        ))),
    }
}

fn tool_call_status_to_wire(status: &TimelineToolCallStatus) -> &'static str {
    match status {
        TimelineToolCallStatus::Succeeded => "succeeded",
        TimelineToolCallStatus::Failed => "failed",
        TimelineToolCallStatus::Cancelled => "cancelled",
    }
}

fn parse_cursor_reason(value: &str) -> Result<TimelineCursorMoveReason, Status> {
    match value {
        "seek-tool-call" => Ok(TimelineCursorMoveReason::SeekToolCall),
        "undo" => Ok(TimelineCursorMoveReason::Undo),
        "redo" => Ok(TimelineCursorMoveReason::Redo),
        "reset" => Ok(TimelineCursorMoveReason::Reset),
        "auto-advance" => Ok(TimelineCursorMoveReason::AutoAdvance),
        other => Err(Status::invalid_argument(format!(
            "unknown timeline cursor move reason '{other}'"
        ))),
    }
}

fn cursor_reason_to_wire(reason: &TimelineCursorMoveReason) -> &'static str {
    match reason {
        TimelineCursorMoveReason::SeekToolCall => "seek-tool-call",
        TimelineCursorMoveReason::Undo => "undo",
        TimelineCursorMoveReason::Redo => "redo",
        TimelineCursorMoveReason::Reset => "reset",
        TimelineCursorMoveReason::AutoAdvance => "auto-advance",
    }
}

fn parse_branch_reason(value: &str) -> Result<TimelineBranchReason, Status> {
    match value {
        "edit-from-rewound-cursor" => Ok(TimelineBranchReason::EditFromRewoundCursor),
        "explicit-fork" => Ok(TimelineBranchReason::ExplicitFork),
        "retry" => Ok(TimelineBranchReason::Retry),
        "fan-out" => Ok(TimelineBranchReason::FanOut),
        other => Err(Status::invalid_argument(format!(
            "unknown timeline branch reason '{other}'"
        ))),
    }
}

fn branch_reason_to_wire(reason: &TimelineBranchReason) -> &'static str {
    match reason {
        TimelineBranchReason::EditFromRewoundCursor => "edit-from-rewound-cursor",
        TimelineBranchReason::ExplicitFork => "explicit-fork",
        TimelineBranchReason::Retry => "retry",
        TimelineBranchReason::FanOut => "fan-out",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use grpc::heddle::v1::{
        AgentTimelineNativeToolCall, AgentTimelineOperationDraft, AgentTimelineToolCallFinished,
        AgentTimelineToolCallStarted, GetTimelineOperationRequest, GetTimelineStatusRequest,
        ListTimelineStepsRequest, RecordTimelineOperationRequest, ResolveNativeToolCallRequest,
        SeekTimelineToNativeToolCallRequest, SeekTimelineToStepRequest,
        agent_timeline_operation_draft, timeline_service_server::TimelineService,
    };
    use repo::{Repository, operation_dedup::OperationDedupStore};
    use tempfile::TempDir;
    use tonic::Request;

    use super::*;

    fn fresh_service() -> (TempDir, LocalTimelineService) {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let dedup = OperationDedupStore::open(repo.heddle_dir()).unwrap();
        let inner = GrpcLocalService::new(Arc::new(repo), Arc::new(dedup));
        (temp, LocalTimelineService::new(inner))
    }

    async fn record_finished_step(
        service: &LocalTimelineService,
        step_id: &str,
        tool_call_id: &str,
        before: u8,
        after: u8,
        finished_at_ms: i64,
    ) {
        service
            .record_operation(Request::new(RecordTimelineOperationRequest {
                repo_path: String::new(),
                operation: Some(AgentTimelineOperationDraft {
                    labels: vec!["repo-reversible".to_string()],
                    body: Some(agent_timeline_operation_draft::Body::ToolCallFinished(
                        AgentTimelineToolCallFinished {
                            thread: "main".to_string(),
                            step_id: step_id.to_string(),
                            branch_id: "tlb-main".to_string(),
                            native: Some(AgentTimelineNativeToolCall {
                                harness: "opencode".to_string(),
                                session_id: "session-1".to_string(),
                                message_id: "message-1".to_string(),
                                tool_call_id: tool_call_id.to_string(),
                            }),
                            status: "succeeded".to_string(),
                            before_state: vec![before; 16],
                            after_state: vec![after; 16],
                            capture_state: vec![after; 16],
                            capture_oplog_batch_id: Some(finished_at_ms as u64),
                            changed: true,
                            touched_paths: vec![format!("src/{step_id}.rs")],
                            payload: None,
                            finished_at_ms,
                        },
                    )),
                }),
                client_operation_id: String::new(),
            }))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn record_operation_stores_canonical_timeline_object() {
        let (_temp, service) = fresh_service();
        let response = service
            .record_operation(Request::new(RecordTimelineOperationRequest {
                repo_path: String::new(),
                operation: Some(AgentTimelineOperationDraft {
                    labels: vec!["repo-reversible".to_string()],
                    body: Some(agent_timeline_operation_draft::Body::ToolCallStarted(
                        AgentTimelineToolCallStarted {
                            thread: "main".to_string(),
                            step_id: "tls-step".to_string(),
                            branch_id: "tlb-main".to_string(),
                            parent_step_id: String::new(),
                            native: Some(AgentTimelineNativeToolCall {
                                harness: "opencode".to_string(),
                                session_id: "session-1".to_string(),
                                message_id: "message-1".to_string(),
                                tool_call_id: "call-1".to_string(),
                            }),
                            tool_name: "shell".to_string(),
                            before_state: vec![1; 16],
                            payload: None,
                            started_at_ms: 1_700_000_000_000,
                        },
                    )),
                }),
                client_operation_id: String::new(),
            }))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(response.kind, "tool_call_started");
        assert_eq!(response.labels, vec!["repo-reversible"]);
        assert_eq!(response.schema_version, 1);
        assert_eq!(response.operation_id.len(), 32);
        assert!(!response.envelope.is_empty());

        let read = service
            .get_operation(Request::new(GetTimelineOperationRequest {
                repo_path: String::new(),
                operation_id: response.operation_id.clone(),
            }))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(read.operation_id, response.operation_id);
        assert_eq!(read.envelope, response.envelope);
        assert_eq!(read.display_id, response.display_id);
    }

    #[tokio::test]
    async fn list_status_and_seek_follow_recorded_tool_calls() {
        let (_temp, service) = fresh_service();
        record_finished_step(&service, "tls-step-1", "call-1", 1, 2, 1_700_000_000_000).await;
        record_finished_step(&service, "tls-step-2", "call-2", 2, 3, 1_700_000_000_100).await;

        let status = service
            .get_timeline_status(Request::new(GetTimelineStatusRequest {
                repo_path: String::new(),
                thread: "main".to_string(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(status.current_branch_id, "tlb-main");
        assert_eq!(status.current_step_id, "tls-step-2");
        assert_eq!(status.current_state.unwrap().state_id, vec![3; 16]);
        assert_eq!(status.step_count, 2);

        let listed = service
            .list_timeline_steps(Request::new(ListTimelineStepsRequest {
                repo_path: String::new(),
                thread: "main".to_string(),
                branch_id: "tlb-main".to_string(),
                limit: 0,
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(listed.steps.len(), 2);
        assert_eq!(listed.steps[0].step_id, "tls-step-1");
        assert_eq!(listed.steps[1].step_id, "tls-step-2");
        assert_eq!(listed.steps[1].status, "succeeded");
        assert_eq!(listed.steps[1].touched_paths, vec!["src/tls-step-2.rs"]);

        let resolved = service
            .resolve_native_tool_call(Request::new(ResolveNativeToolCallRequest {
                repo_path: String::new(),
                thread: "main".to_string(),
                harness: "opencode".to_string(),
                session_id: "session-1".to_string(),
                message_id: "message-1".to_string(),
                tool_call_id: "call-1".to_string(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resolved.step_id, "tls-step-1");

        let moved = service
            .seek_to_step(Request::new(SeekTimelineToStepRequest {
                repo_path: String::new(),
                thread: "main".to_string(),
                branch_id: "tlb-main".to_string(),
                step_id: "tls-step-1".to_string(),
                reason: "seek-step".to_string(),
                client_operation_id: String::new(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(moved.operation.unwrap().kind, "cursor_moved");
        let moved_status = moved.status.unwrap();
        assert_eq!(moved_status.current_step_id, "tls-step-1");
        assert_eq!(moved_status.current_state.unwrap().state_id, vec![2; 16]);

        let moved = service
            .seek_to_native_tool_call(Request::new(SeekTimelineToNativeToolCallRequest {
                repo_path: String::new(),
                thread: "main".to_string(),
                harness: "opencode".to_string(),
                session_id: "session-1".to_string(),
                message_id: "message-1".to_string(),
                tool_call_id: "call-2".to_string(),
                reason: String::new(),
                client_operation_id: String::new(),
            }))
            .await
            .unwrap()
            .into_inner();
        let moved_status = moved.status.unwrap();
        assert_eq!(moved_status.current_step_id, "tls-step-2");
        assert_eq!(moved_status.current_state.unwrap().state_id, vec![3; 16]);
    }
}
