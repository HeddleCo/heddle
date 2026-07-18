// SPDX-License-Identifier: Apache-2.0
//! Hosted `CollaborationService` client wrappers.
//!
//! These are the caller-authenticated, PoP-signed RPCs the CLI uses to publish
//! and fetch discussions against a hosted weft. They are the write/read seam
//! for hosted collaboration: local discussions live in the append-only
//! [`repo::CollaborationStore`] op-log, and the CLI-side sync orchestrator
//! ([`heddle_cli`'s `discussion_sync`]) bridges that model to the server's
//! per-state `DiscussionsBlob` shape through these calls.
//!
//! Wire identity note: the canonical `CollaborationService` proto types the
//! discussion anchor state as a 32-byte `StateId`, but the hosted server still
//! resolves the inbound `state_id` field through a 16-byte `ChangeId` decode
//! (`OpenDiscussion`/`ListByState`). We therefore send the **ChangeId** bytes in
//! that field (matching weft's own canonical integration tests), while the
//! server echoes the genuine 32-byte `StateId` back in
//! `Discussion.opened_against_state`.

use grpc::heddle::api::v1alpha1::{
    AppendTurnRequest, Discussion as ProtoDiscussion, ListDiscussionsByStateRequest,
    OpenDiscussionRequest, PathSymbolRef, StateId as ProtoStateId,
};
use objects::object::{ChangeId, StateId};
use tonic::Request;
use wire::ProtocolError;

use super::{HostedGrpcClient, helpers::status_to_protocol_error};

/// One turn of a hosted discussion, decoded from the wire.
#[derive(Debug, Clone)]
pub struct HostedDiscussionTurn {
    pub author_name: String,
    pub author_email: String,
    pub body: String,
    pub posted_at_secs: i64,
}

/// A hosted discussion decoded from the `CollaborationService` wire types into
/// the shape the CLI-side sync bridge consumes.
#[derive(Debug, Clone)]
pub struct HostedDiscussion {
    /// Server-assigned discussion id (opaque string).
    pub id: String,
    pub file: String,
    pub symbol: String,
    /// Genuine 32-byte anchor `StateId` echoed by the server, when present.
    pub opened_against_state: Option<StateId>,
    pub visibility: String,
    pub turns: Vec<HostedDiscussionTurn>,
}

const OPEN_METHOD: &str = "/heddle.api.v1alpha1.CollaborationService/OpenDiscussion";
const APPEND_METHOD: &str = "/heddle.api.v1alpha1.CollaborationService/AppendTurn";
const LIST_BY_STATE_METHOD: &str = "/heddle.api.v1alpha1.CollaborationService/ListByState";

fn decode_discussion(proto: ProtoDiscussion) -> HostedDiscussion {
    let anchor = proto.anchor.unwrap_or_default();
    HostedDiscussion {
        id: proto.id,
        file: anchor.file,
        symbol: anchor.symbol,
        opened_against_state: proto
            .opened_against_state
            .and_then(|state| StateId::try_from_slice(&state.value).ok()),
        visibility: proto.visibility,
        turns: proto
            .turns
            .into_iter()
            .map(|turn| HostedDiscussionTurn {
                author_name: turn.author_name,
                author_email: turn.author_email,
                body: turn.body,
                posted_at_secs: turn.posted_at.map(|ts| ts.seconds).unwrap_or(0),
            })
            .collect(),
    }
}

/// The hosted server decodes the discussion anchor `state_id` field as a
/// 16-byte `ChangeId` (see the module note); wrap the change id in the proto
/// `StateId` message accordingly.
fn change_id_state_field(change_id: ChangeId) -> Option<ProtoStateId> {
    Some(ProtoStateId {
        value: change_id.as_bytes().to_vec(),
    })
}

impl HostedGrpcClient {
    /// Open a hosted discussion anchored at `change_id`'s state, seeded with
    /// `body` as the first turn. Caller-authenticated + PoP-signed.
    #[allow(clippy::too_many_arguments)]
    pub async fn open_discussion(
        &mut self,
        repo_path: &str,
        change_id: ChangeId,
        file: &str,
        symbol: &str,
        body: &str,
        visibility: &str,
        client_operation_id: String,
    ) -> Result<HostedDiscussion, ProtocolError> {
        let mut request = Request::new(OpenDiscussionRequest {
            repo_path: super::helpers::repository_ref(repo_path),
            state_id: change_id_state_field(change_id),
            anchor: Some(PathSymbolRef {
                file: file.to_string(),
                symbol: symbol.to_string(),
            }),
            body: body.to_string(),
            visibility: visibility.to_string(),
            thread_ref: String::new(),
            client_operation_id,
        });
        self.apply_signed_auth(&mut request, OPEN_METHOD)?;
        let response = self
            .collaboration
            .open_discussion(request)
            .await
            .map_err(status_to_protocol_error)?
            .into_inner();
        Ok(decode_discussion(response))
    }

    /// Append `body` as a new turn on an existing hosted discussion.
    /// Caller-authenticated + PoP-signed.
    pub async fn append_turn(
        &mut self,
        repo_path: &str,
        discussion_id: &str,
        body: &str,
        client_operation_id: String,
    ) -> Result<HostedDiscussion, ProtocolError> {
        let mut request = Request::new(AppendTurnRequest {
            repo_path: super::helpers::repository_ref(repo_path),
            discussion_id: discussion_id.to_string(),
            body: body.to_string(),
            client_operation_id,
        });
        self.apply_signed_auth(&mut request, APPEND_METHOD)?;
        let response = self
            .collaboration
            .append_turn(request)
            .await
            .map_err(status_to_protocol_error)?
            .into_inner();
        Ok(decode_discussion(response))
    }

    /// List hosted discussions anchored at `change_id`'s state. `status` is one
    /// of `open` | `resolved` | `all` | `orphaned`.
    pub async fn list_discussions_by_state(
        &mut self,
        repo_path: &str,
        change_id: ChangeId,
        status: &str,
    ) -> Result<Vec<HostedDiscussion>, ProtocolError> {
        let mut request = Request::new(ListDiscussionsByStateRequest {
            repo_path: super::helpers::repository_ref(repo_path),
            state_id: change_id_state_field(change_id),
            status: status.to_string(),
        });
        self.apply_signed_auth(&mut request, LIST_BY_STATE_METHOD)?;
        let response = self
            .collaboration
            .list_by_state(request)
            .await
            .map_err(status_to_protocol_error)?
            .into_inner();
        Ok(response.discussions.into_iter().map(decode_discussion).collect())
    }
}
