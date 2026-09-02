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
//! for `OpenDiscussion`/`ListByState`. We therefore send the **ChangeId** bytes
//! in those fields (matching weft's own canonical integration tests), while the
//! server echoes the genuine 32-byte `StateId` back in
//! `Discussion.opened_against_state`.
//!
//! `GetDiscussion.state_id` is the 32-byte `StateId` (empty = HEAD). Pass
//! `opened_against_state` / the event's `new_state` so a discussion that
//! exists on a prior state is not NotFound.

use api::heddle::api::v1alpha1::{
    AppendTurnRequest, ContextAnnotationKind, Discussion as ProtoDiscussion, DiscussionKind,
    DiscussionSeverity, DiscussionStatusFilter, GetDiscussionRequest,
    ListDiscussionsByStateRequest, OpenDiscussionRequest, PathSymbolRef, ResolveDiscussionRequest,
    StateId as ProtoStateId, discussion_resolution, list_discussions_response,
    resolve_discussion_request,
};
use objects::object::{AnnotationKind, ChangeId, StateId};
use wire::ProtocolError;

use super::{HostedClient, helpers::hosted_to_protocol_error};

/// One turn of a hosted discussion, decoded from the wire.
#[derive(Debug, Clone, Default)]
pub struct HostedDiscussionTurn {
    pub author_name: String,
    pub author_email: String,
    pub body: String,
    pub posted_at_secs: i64,
    /// Server-minted turn identity. Empty when the producer has not minted one
    /// (ListByState snapshots on older weft); live events should carry it.
    pub turn_id: String,
    /// Per-discussion monotonic sequence. Zero means "not minted" — callers
    /// then fall back to list order. Proto documents minted sequences as
    /// 1-based (`turn_seq` is zero only before mint).
    pub turn_seq: u64,
}

/// Hosted resolution decoded from the collaboration wire oneof.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum HostedResolution {
    #[default]
    Open,
    IntoAnnotation {
        annotation_id: String,
    },
    ByEdit {
        state_id: Option<StateId>,
    },
    Dismissed {
        reason: String,
    },
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
    pub thread_ref: Option<String>,
    /// Wire `Discussion.thread_id`. Weft stamps the stable UUID here and the
    /// renameable name in `thread_ref`.
    pub thread_id: Option<String>,
    pub turns: Vec<HostedDiscussionTurn>,
    pub resolution: HostedResolution,
    /// Wire `Discussion.kind`. Unspecified is treated as code-anchored when
    /// file+symbol are present; Coordination has no `PathSymbolRef`.
    pub kind: i32,
}

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
        thread_ref: (!proto.thread_ref.is_empty()).then_some(proto.thread_ref),
        thread_id: (!proto.thread_id.is_empty()).then_some(proto.thread_id),
        kind: proto.kind,
        turns: proto
            .turns
            .into_iter()
            .map(|turn| HostedDiscussionTurn {
                author_name: turn.author_name,
                author_email: turn.author_email,
                body: turn.body,
                posted_at_secs: turn.posted_at.map(|ts| ts.seconds).unwrap_or(0),
                turn_id: turn.turn_id,
                turn_seq: turn.turn_seq,
            })
            .collect(),
        resolution: decode_resolution(proto.resolution),
    }
}

fn decode_resolution(
    resolution: Option<api::heddle::api::v1alpha1::DiscussionResolution>,
) -> HostedResolution {
    match resolution.and_then(|resolution| resolution.state) {
        Some(discussion_resolution::State::IntoAnnotation(annotation)) => {
            HostedResolution::IntoAnnotation {
                annotation_id: annotation.annotation_id,
            }
        }
        Some(discussion_resolution::State::ByEdit(edit)) => HostedResolution::ByEdit {
            state_id: edit
                .state_id
                .and_then(|state| StateId::try_from_slice(&state.value).ok()),
        },
        Some(discussion_resolution::State::Dismissed(dismissed)) => HostedResolution::Dismissed {
            reason: dismissed.reason,
        },
        Some(discussion_resolution::State::Open(_)) | None => HostedResolution::Open,
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

fn get_discussion_request(
    repo_path: &str,
    discussion_id: &str,
    state_id: Option<StateId>,
) -> GetDiscussionRequest {
    GetDiscussionRequest {
        repo_path: super::helpers::repository_ref(repo_path),
        discussion_id: discussion_id.to_string(),
        state_id: state_id.and_then(super::helpers::proto_state_id),
    }
}

impl HostedClient {
    /// The authenticated hosted username (the bearer token's `principal:<subject>`
    /// subject). weft stamps discussion turns with `Principal::new(username, "")`,
    /// so this is the author name our own pushed turns carry server-side — the
    /// identity the discussion-sync bridge uses to recognize turns we published.
    /// `None` for an anonymous/unsigned client.
    pub fn authenticated_username(&self) -> Option<String> {
        self.context
            .signing_identity()
            .and_then(|principal| principal.strip_prefix("principal:"))
            .map(|subject| subject.trim().to_string())
            .filter(|subject| !subject.is_empty())
    }

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
        thread_ref: Option<&str>,
        client_operation_id: String,
        discussion_id: &str,
    ) -> Result<HostedDiscussion, ProtocolError> {
        let request = open_discussion_request(
            repo_path,
            change_id,
            file,
            symbol,
            body,
            visibility,
            thread_ref,
            client_operation_id,
            discussion_id,
        );
        let response = self
            .routes()
            .open_discussion(&request)
            .await
            .map_err(hosted_to_protocol_error)?;
        Ok(decode_discussion(response))
    }

    /// Resolve a hosted discussion by creating and linking a context annotation.
    #[allow(clippy::too_many_arguments)]
    pub async fn resolve_discussion_into_annotation(
        &mut self,
        repo_path: &str,
        discussion_id: &str,
        kind: AnnotationKind,
        content: &str,
        tags: Vec<String>,
        client_operation_id: String,
    ) -> Result<HostedDiscussion, ProtocolError> {
        let request = resolve_into_annotation_request(
            repo_path,
            discussion_id,
            kind,
            content,
            tags,
            client_operation_id,
        );
        let response = self
            .routes()
            .resolve_discussion(&request)
            .await
            .map_err(hosted_to_protocol_error)?;
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
        let request = AppendTurnRequest {
            repo_path: super::helpers::repository_ref(repo_path),
            discussion_id: discussion_id.to_string(),
            body: body.to_string(),
            client_operation_id,
            references: Vec::new(),
        };
        let response = self
            .routes()
            .append_turn(&request)
            .await
            .map_err(hosted_to_protocol_error)?;
        Ok(decode_discussion(response))
    }

    /// Fetch one hosted discussion by server id. `state_id` empty means HEAD;
    /// set it to recover a discussion on a prior state (32-byte `StateId`,
    /// not the ChangeId used on OpenDiscussion/ListByState).
    pub async fn get_discussion(
        &mut self,
        repo_path: &str,
        discussion_id: &str,
        state_id: Option<StateId>,
    ) -> Result<HostedDiscussion, ProtocolError> {
        let request = get_discussion_request(repo_path, discussion_id, state_id);
        let response = self
            .routes()
            .get_discussion(&request)
            .await
            .map_err(hosted_to_protocol_error)?;
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
        let status = discussion_status_filter(status)? as i32;
        let mut discussions = Vec::new();
        let mut page_token = String::new();
        loop {
            let request = ListDiscussionsByStateRequest {
                repo_path: super::helpers::repository_ref(repo_path),
                state_id: change_id_state_field(change_id),
                status,
                page_size: api::MAX_PAGE_SIZE,
                page_token: page_token.clone(),
            };
            let mut stream = self
                .routes()
                .list_discussions_by_state(&request)
                .await
                .map_err(hosted_to_protocol_error)?;
            let mut next_page_token = None;
            while let Some(response) = stream.next().await.map_err(hosted_to_protocol_error)? {
                match response.frame {
                    Some(list_discussions_response::Frame::Item(discussion)) => {
                        discussions.push(decode_discussion(*discussion));
                    }
                    Some(list_discussions_response::Frame::PageEnd(page_end)) => {
                        next_page_token = Some(page_end.next_page_token);
                    }
                    None => {
                        return Err(ProtocolError::InvalidState(
                            "ListByState emitted an empty frame".to_string(),
                        ));
                    }
                }
            }
            let next_page_token = next_page_token.ok_or_else(|| {
                ProtocolError::InvalidState(
                    "ListByState ended without a terminal page frame".to_string(),
                )
            })?;
            if next_page_token.is_empty() {
                return Ok(discussions);
            }
            if next_page_token == page_token {
                return Err(ProtocolError::InvalidState(
                    "ListByState returned a repeated page token".to_string(),
                ));
            }
            page_token = next_page_token;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn open_discussion_request(
    repo_path: &str,
    change_id: ChangeId,
    file: &str,
    symbol: &str,
    body: &str,
    visibility: &str,
    thread_ref: Option<&str>,
    client_operation_id: String,
    discussion_id: &str,
) -> OpenDiscussionRequest {
    OpenDiscussionRequest {
        repo_path: super::helpers::repository_ref(repo_path),
        state_id: change_id_state_field(change_id),
        anchor: Some(PathSymbolRef {
            file: file.to_string(),
            symbol: symbol.to_string(),
        }),
        body: body.to_string(),
        visibility: visibility.to_string(),
        thread_ref: thread_ref.unwrap_or_default().to_string(),
        client_operation_id,
        thread_id: String::new(),
        severity: DiscussionSeverity::Unspecified as i32,
        kind: DiscussionKind::CodeAnchored as i32,
        discussion_id: discussion_id.to_string(),
    }
}

fn resolve_into_annotation_request(
    repo_path: &str,
    discussion_id: &str,
    kind: AnnotationKind,
    content: &str,
    tags: Vec<String>,
    client_operation_id: String,
) -> ResolveDiscussionRequest {
    ResolveDiscussionRequest {
        repo_path: super::helpers::repository_ref(repo_path),
        discussion_id: discussion_id.to_string(),
        resolution: Some(resolve_discussion_request::Resolution::IntoAnnotation(
            resolve_discussion_request::ResolveIntoAnnotation {
                kind: annotation_kind_to_proto(kind) as i32,
                content: content.to_string(),
                tags,
            },
        )),
        client_operation_id,
    }
}

fn annotation_kind_to_proto(kind: AnnotationKind) -> ContextAnnotationKind {
    match kind {
        AnnotationKind::Constraint => ContextAnnotationKind::Constraint,
        AnnotationKind::Invariant => ContextAnnotationKind::Invariant,
        AnnotationKind::Rationale => ContextAnnotationKind::Rationale,
    }
}

fn discussion_status_filter(status: &str) -> Result<DiscussionStatusFilter, ProtocolError> {
    match status {
        "all" => Ok(DiscussionStatusFilter::Unspecified),
        "open" => Ok(DiscussionStatusFilter::Open),
        "resolved" => Ok(DiscussionStatusFilter::Resolved),
        "orphaned" => Ok(DiscussionStatusFilter::Orphaned),
        other => Err(ProtocolError::InvalidState(format!(
            "invalid discussion status filter: {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use api::heddle::api::v1alpha1::{
        DiscussionTurn as ProtoTurn, PathSymbolRef, StateId as ProtoStateId,
        resolve_discussion_request::Resolution,
    };
    use objects::object::{AnnotationKind, ChangeId};

    use super::*;

    #[test]
    fn discussion_status_filter_accepts_known_and_rejects_unknown() {
        assert_eq!(
            discussion_status_filter("all").unwrap(),
            DiscussionStatusFilter::Unspecified
        );
        assert_eq!(
            discussion_status_filter("open").unwrap(),
            DiscussionStatusFilter::Open
        );
        assert_eq!(
            discussion_status_filter("resolved").unwrap(),
            DiscussionStatusFilter::Resolved
        );
        assert_eq!(
            discussion_status_filter("orphaned").unwrap(),
            DiscussionStatusFilter::Orphaned
        );
        assert!(discussion_status_filter("closed").is_err());
    }

    #[test]
    fn change_id_state_field_wraps_16_byte_change_id() {
        let change = ChangeId::from_bytes([0x11; 16]);
        let field = change_id_state_field(change).expect("proto state wrapper");
        assert_eq!(field.value, change.as_bytes().to_vec());
    }

    #[test]
    fn get_discussion_request_uses_32_byte_state_id() {
        let state = StateId::from_bytes([0xab; 32]);
        let request = get_discussion_request("acme/widgets", "disc-1", Some(state));
        assert_eq!(request.discussion_id, "disc-1");
        let sent = request.state_id.expect("state_id should be set");
        assert_eq!(sent.value, vec![0xab; 32]);
        let head = get_discussion_request("acme/widgets", "disc-1", None);
        assert!(head.state_id.is_none());
    }

    #[test]
    fn open_discussion_request_sets_thread_ref() {
        let change = ChangeId::from_bytes([0x11; 16]);
        let request = open_discussion_request(
            "acme/widgets",
            change,
            "src/lib.rs",
            "run",
            "keep this stable",
            "internal",
            Some("refs/heads/feature/run"),
            "open-op".to_string(),
            "disc-018f0000-0000-7000-8000-000000000000",
        );
        assert_eq!(request.thread_ref, "refs/heads/feature/run");
        assert_eq!(request.anchor.unwrap().file, "src/lib.rs");
        assert_eq!(request.body, "keep this stable");
        assert_eq!(request.severity, DiscussionSeverity::Unspecified as i32);
        assert_eq!(request.kind, DiscussionKind::CodeAnchored as i32);
        assert_eq!(
            request.discussion_id,
            "disc-018f0000-0000-7000-8000-000000000000"
        );
    }

    #[test]
    fn resolve_into_annotation_request_uses_the_typed_resolution_variant() {
        let request = resolve_into_annotation_request(
            "acme/widgets",
            "discussion-1",
            AnnotationKind::Invariant,
            "the cache key must include visibility",
            vec!["cache".to_string(), "security".to_string()],
            "resolve-op".to_string(),
        );
        let Some(Resolution::IntoAnnotation(annotation)) = request.resolution else {
            panic!("expected ResolveIntoAnnotation request");
        };
        assert_eq!(annotation.kind, ContextAnnotationKind::Invariant as i32);
        assert_eq!(annotation.content, "the cache key must include visibility");
        assert_eq!(annotation.tags, ["cache", "security"]);
    }

    #[test]
    fn decode_discussion_maps_anchor_turns_and_optional_state() {
        let state = StateId::from_bytes([0x22; 32]);
        let proto = ProtoDiscussion {
            id: "disc-1".into(),
            anchor: Some(PathSymbolRef {
                file: "src/lib.rs".into(),
                symbol: "main".into(),
            }),
            opened_against_state: Some(ProtoStateId {
                value: state.as_bytes().to_vec(),
            }),
            visibility: "team".into(),
            turns: vec![ProtoTurn {
                author_name: "alice".into(),
                author_email: "a@x".into(),
                body: "lgtm".into(),
                references: Vec::new(),
                posted_at: Some(prost_types::Timestamp {
                    seconds: 42,
                    nanos: 0,
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        let decoded = decode_discussion(proto);
        assert_eq!(decoded.id, "disc-1");
        assert_eq!(decoded.file, "src/lib.rs");
        assert_eq!(decoded.symbol, "main");
        assert_eq!(decoded.opened_against_state, Some(state));
        assert_eq!(decoded.visibility, "team");
        assert_eq!(decoded.thread_ref, None);
        assert_eq!(decoded.thread_id, None);
        assert_eq!(decoded.turns.len(), 1);
        assert_eq!(decoded.turns[0].author_name, "alice");
        assert_eq!(decoded.turns[0].body, "lgtm");
        assert_eq!(decoded.turns[0].posted_at_secs, 42);
        assert!(matches!(decoded.resolution, HostedResolution::Open));

        // Missing optional fields collapse cleanly.
        let empty = decode_discussion(ProtoDiscussion {
            id: "empty".into(),
            ..Default::default()
        });
        assert!(empty.file.is_empty());
        assert!(empty.opened_against_state.is_none());
        assert!(empty.turns.is_empty());
        assert!(empty.thread_id.is_none());

        let stamped = decode_discussion(ProtoDiscussion {
            id: "stamped".into(),
            thread_ref: "old-name".into(),
            thread_id: "thr-stable".into(),
            ..Default::default()
        });
        assert_eq!(stamped.thread_ref.as_deref(), Some("old-name"));
        assert_eq!(stamped.thread_id.as_deref(), Some("thr-stable"));
    }
}
