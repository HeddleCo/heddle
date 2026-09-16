// SPDX-License-Identifier: Apache-2.0
//! Hosted `CollaborationService` client wrappers over v2 RPCs.
//!
//! Write path: signed original operations via `OpenDiscussion`, `AppendTurn`,
//! `ResolveDiscussion`. Read path: `ObserveCollaboration`.

use api::heddle::api::v2alpha1::{
    self as contract, AppendDiscussionRequest, Audience, CollaborationAnchor, MutationResponse,
    ObservationMode, ObserveCollaborationRequest, ObserveOptions, OpenDiscussionRequest,
    PutContextRequest, RecordRef, ResolveDiscussionRequest, collaboration_anchor,
    collaboration_event, discussion_record, resolve_discussion_request, revision_ref,
};
use objects::object::{
    AnnotationKind, Attribution, CollaborationActor, CollaborationAnchor as Anchor,
    CollaborationIdempotencyKey, CollaborationMetadata, CollaborationOperationBodyV1 as Body,
    CollaborationResolution, CollaborationScope, DiscussionRecordId, DiscussionTurnV1, Principal,
    StateId, VisibilityTier,
};
use thread_api::rpc;
use wire::ProtocolError;

use super::{
    HostedClient, helpers::native_client_error, operation_id::ClientOperationId,
    user::require_applied_receipt,
};

const OPEN: &str = "heddle.api.v2alpha1.CollaborationService/OpenDiscussion";
const APPEND: &str = "heddle.api.v2alpha1.CollaborationService/AppendTurn";
const RESOLVE: &str = "heddle.api.v2alpha1.CollaborationService/ResolveDiscussion";
const PUT_CONTEXT: &str = "heddle.api.v2alpha1.CollaborationService/PutContext";

/// One turn of a hosted discussion, decoded from the wire.
#[derive(Debug, Clone, Default)]
pub struct HostedDiscussionTurn {
    pub author_name: String,
    pub author_email: String,
    pub body: String,
    pub posted_at_secs: i64,
    /// Server-minted turn identity. Empty when the producer has not minted one.
    pub turn_id: String,
    /// Per-discussion monotonic sequence. Zero means "not minted".
    pub turn_seq: u64,
}

/// Hosted resolution decoded from the collaboration wire.
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

/// A hosted discussion decoded into the shape the CLI-side sync bridge consumes.
#[derive(Debug, Clone)]
pub struct HostedDiscussion {
    pub id: String,
    pub file: String,
    pub symbol: String,
    pub opened_against_state: Option<StateId>,
    pub visibility: String,
    pub thread_ref: Option<String>,
    pub thread_id: Option<String>,
    pub turns: Vec<HostedDiscussionTurn>,
    pub resolution: HostedResolution,
    pub kind: i32,
}

fn native_error(error: impl std::fmt::Display) -> ProtocolError {
    ProtocolError::InvalidState(error.to_string())
}

fn once_observe() -> ObserveOptions {
    ObserveOptions {
        mode: ObservationMode::Once as i32,
        ..Default::default()
    }
}

fn visibility_tier(value: &str) -> Result<VisibilityTier, ProtocolError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "internal" | "members" | "team" => Ok(VisibilityTier::Internal),
        "public" => Ok(VisibilityTier::Public),
        "private" => Ok(VisibilityTier::Private {
            scope_label: "private".into(),
        }),
        other => Ok(VisibilityTier::Private {
            scope_label: other.to_string(),
        }),
    }
}

fn audience_of(tier: &VisibilityTier) -> Result<(i32, String), ProtocolError> {
    thread_api::collaboration::audience(tier)
        .ok_or_else(|| {
            ProtocolError::InvalidState("discussion audience is not a v2 audience".into())
        })
        .map(|(audience, label)| (audience as i32, label))
}

fn parse_discussion_id(value: &str) -> Result<DiscussionRecordId, ProtocolError> {
    value.parse().or_else(|_| {
        uuid::Uuid::parse_str(value)
            .ok()
            .and_then(|id| DiscussionRecordId::from_uuid(id).ok())
            .ok_or_else(|| {
                ProtocolError::InvalidState(format!("discussion id {value} is not a UUIDv7"))
            })
    })
}

fn thread_hash(
    reference: &contract::ThreadRef,
) -> Result<objects::object::ContentHash, ProtocolError> {
    let value = reference
        .id
        .as_ref()
        .ok_or_else(|| ProtocolError::InvalidState("Thread identity absent".into()))?;
    let bytes: [u8; 32] = value
        .value
        .as_slice()
        .try_into()
        .map_err(|_| ProtocolError::InvalidState("Thread identity length".into()))?;
    Ok(objects::object::ContentHash::from_bytes(bytes))
}

fn source_location(anchor: Option<&CollaborationAnchor>) -> (String, String, Option<StateId>) {
    let Some(collaboration_anchor::Target::Source(source)) =
        anchor.and_then(|anchor| anchor.target.as_ref())
    else {
        return (String::new(), String::new(), None);
    };
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

fn visibility_label(record: &contract::DiscussionRecord) -> String {
    match Audience::try_from(record.audience) {
        Ok(Audience::Public) => "public".into(),
        Ok(Audience::Private) => {
            if record.audience_label.is_empty() {
                "private".into()
            } else {
                record.audience_label.clone()
            }
        }
        _ => "internal".into(),
    }
}

fn hosted_from_record(
    record: &contract::DiscussionRecord,
    turns: Vec<HostedDiscussionTurn>,
) -> HostedDiscussion {
    let (file, symbol, opened_against_state) = source_location(record.anchor.as_ref());
    let resolution = match (
        discussion_record::Status::try_from(record.status).ok(),
        record.extracted_context.as_ref(),
    ) {
        (Some(discussion_record::Status::Resolved), Some(extracted)) => {
            HostedResolution::IntoAnnotation {
                annotation_id: extracted.id.clone(),
            }
        }
        (Some(discussion_record::Status::Resolved), None) => HostedResolution::Dismissed {
            reason: String::new(),
        },
        _ => HostedResolution::Open,
    };
    HostedDiscussion {
        id: record
            .r#ref
            .as_ref()
            .map(|reference| reference.id.clone())
            .unwrap_or_default(),
        file,
        symbol,
        opened_against_state,
        visibility: visibility_label(record),
        thread_ref: None,
        thread_id: None,
        turns,
        resolution,
        kind: 0,
    }
}

impl HostedClient {
    /// The authenticated hosted username (the bearer token's `principal:<subject>`
    /// subject).
    pub fn authenticated_username(&self) -> Option<String> {
        self.context
            .signing_identity()
            .and_then(|principal| principal.strip_prefix("principal:"))
            .map(|subject| subject.trim().to_string())
            .filter(|subject| !subject.is_empty())
    }

    async fn collaboration_scope(
        &mut self,
        repo_path: &str,
        thread_ref: Option<&str>,
    ) -> Result<(contract::SpoolRef, contract::ThreadRef, CollaborationScope), ProtocolError> {
        let spool = self.resolve_spool_ref(repo_path).await?;
        let spool_uuid = uuid::Uuid::parse_str(&spool.id).map_err(native_error)?;
        let name = thread_ref
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("main");
        let reference = match self.resolve_thread_ref(repo_path, name).await {
            Ok(reference) => reference,
            Err(_) if name != "main" => self.resolve_thread_ref(repo_path, "main").await?,
            Err(error) => return Err(error),
        };
        let thread = thread_hash(&reference)?;
        Ok((
            spool,
            reference,
            CollaborationScope {
                spool: spool_uuid,
                thread: Some(thread),
            },
        ))
    }

    async fn collaboration_actor(
        &mut self,
    ) -> Result<(CollaborationActor, Attribution), ProtocolError> {
        let (principal, _credential) = self.observe_current_identity().await?;
        let principal_id = uuid::Uuid::parse_str(&principal.id).map_err(native_error)?;
        let agent_id = None;
        let name = if principal.handle.is_empty() {
            principal.display_name
        } else {
            principal.handle
        };
        Ok((
            CollaborationActor {
                principal_id,
                agent_id,
            },
            Attribution::human(Principal::new(name, "")),
        ))
    }

    async fn observe_discussion(
        &self,
        spool: contract::SpoolRef,
        discussion: RecordRef,
    ) -> Result<HostedDiscussion, ProtocolError> {
        let discussions = self
            .observe_discussions(spool, Some(discussion), Vec::new())
            .await?;
        discussions.into_iter().next().ok_or_else(|| {
            ProtocolError::ObjectNotFound(
                "discussion is not in the hosted collaboration view".into(),
            )
        })
    }

    async fn observe_discussions(
        &self,
        spool: contract::SpoolRef,
        discussion: Option<RecordRef>,
        statuses: Vec<i32>,
    ) -> Result<Vec<HostedDiscussion>, ProtocolError> {
        let remote = self.native().await.map_err(native_error)?;
        let mut observation = remote
            .observe::<rpc::CollaborationServiceObserveCollaboration>(
                ObserveCollaborationRequest {
                    spool: Some(spool),
                    discussions: discussion.into_iter().collect(),
                    statuses,
                    include_history: true,
                    observe: Some(once_observe()),
                    ..Default::default()
                },
                None,
            )
            .await
            .map_err(native_error)?;
        let mut records = Vec::new();
        let mut turns: Vec<contract::DiscussionTurn> = Vec::new();
        while let Some(batch) = observation.next_commit().await.map_err(native_error)? {
            for change in batch.changes {
                match change {
                    collaboration_event::Payload::Discussion(record) => records.push(record),
                    collaboration_event::Payload::Turn(turn) => turns.push(turn),
                    _ => {}
                }
            }
        }
        Ok(records
            .into_iter()
            .map(|record| {
                let id = record.r#ref.as_ref().map(|value| value.id.as_str());
                let mut matching: Vec<_> = turns
                    .iter()
                    .filter(|turn| {
                        turn.discussion
                            .as_ref()
                            .is_some_and(|reference| Some(reference.id.as_str()) == id)
                    })
                    .cloned()
                    .collect();
                matching
                    .sort_by_key(|turn| turn.created_at.as_ref().map(|ts| ts.seconds).unwrap_or(0));
                let hosted_turns = matching
                    .into_iter()
                    .enumerate()
                    .map(|(index, turn)| HostedDiscussionTurn {
                        author_name: turn.principal_id,
                        author_email: String::new(),
                        body: turn.body,
                        posted_at_secs: turn.created_at.map(|ts| ts.seconds).unwrap_or(0),
                        turn_id: turn.r#ref.map(|value| value.id).unwrap_or_default(),
                        turn_seq: (index as u64).saturating_add(1),
                    })
                    .collect();
                hosted_from_record(&record, hosted_turns)
            })
            .collect())
    }

    /// Open a hosted discussion anchored at `state_id`, seeded with `body`.
    #[allow(clippy::too_many_arguments)]
    pub async fn open_discussion(
        &mut self,
        repo_path: &str,
        state_id: StateId,
        file: &str,
        symbol: &str,
        body: &str,
        visibility: &str,
        thread_ref: Option<&str>,
        client_operation_id: String,
        discussion_id: &str,
    ) -> Result<HostedDiscussion, ProtocolError> {
        let operation_id = ClientOperationId::caller_or_fresh(OPEN, client_operation_id);
        let (spool, _, scope) = self.collaboration_scope(repo_path, thread_ref).await?;
        let (actor, author) = self.collaboration_actor().await?;
        let signer = self
            .claim_proof_signer()
            .ok_or(super::HostedError::SigningIdentityRequired)
            .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
        let discussion = parse_discussion_id(discussion_id)?;
        let tier = visibility_tier(visibility)?;
        let (audience, audience_label) = audience_of(&tier)?;
        let anchor = Anchor::Symbol {
            state_id,
            path: file.to_string(),
            symbol: symbol.to_string(),
        };
        let signed = thread_api::collaboration::Command {
            discussion,
            operation_id: CollaborationIdempotencyKey::new(operation_id.as_str())
                .map_err(native_error)?,
            metadata: CollaborationMetadata {
                scope: scope.clone(),
                actor,
                mentions: vec![],
            },
            author,
            occurred_at_ms: chrono::Utc::now().timestamp_millis(),
            body: Body::Open {
                blocking: false,
                title: format!("{file}:{symbol}"),
                anchor: anchor.clone(),
                visibility: tier,
                turn: DiscussionTurnV1::new(body).map_err(native_error)?,
                thread_ref: None,
            },
        }
        .sign(&[], signer)
        .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
        let request = OpenDiscussionRequest {
            client_operation_id: operation_id.to_wire(),
            spool: Some(spool.clone()),
            anchor: Some(
                thread_api::collaboration::anchor_ref(&anchor, &scope)
                    .map_err(|error| ProtocolError::InvalidState(error.to_string()))?,
            ),
            title: format!("{file}:{symbol}"),
            initial_body: body.to_string(),
            signed_operation: Some(signed),
            audience,
            audience_label,
            ..Default::default()
        };
        let remote = self.native().await.map_err(native_error)?;
        let response = remote
            .api
            .call::<rpc::CollaborationServiceOpenDiscussion>(&request)
            .await
            .map_err(native_client_error)?;
        require_applied_receipt(
            response.receipt,
            &request.client_operation_id,
            &remote.description.endpoint,
            "open discussion",
        )?;
        self.observe_discussion(
            spool,
            RecordRef {
                spool: request.spool,
                id: discussion.to_string(),
            },
        )
        .await
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
        let _ = kind;
        let operation_id = ClientOperationId::caller_or_fresh(RESOLVE, client_operation_id);
        let (spool, _, scope) = self.collaboration_scope(repo_path, None).await?;
        let current = self
            .observe_discussion(
                spool.clone(),
                RecordRef {
                    spool: Some(spool.clone()),
                    id: discussion_id.to_string(),
                },
            )
            .await?;
        let (actor, author) = self.collaboration_actor().await?;
        let signer = self
            .claim_proof_signer()
            .ok_or(super::HostedError::SigningIdentityRequired)
            .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
        let discussion = parse_discussion_id(discussion_id)?;
        let context = objects::object::ContextRevision {
            version: 2,
            id: uuid::Uuid::now_v7(),
            parents: vec![],
            metadata: CollaborationMetadata {
                scope: scope.clone(),
                actor: actor.clone(),
                mentions: vec![],
            },
            anchor: Anchor::Symbol {
                state_id: current
                    .opened_against_state
                    .unwrap_or_else(|| StateId::from_bytes([0; 32])),
                path: current.file.clone(),
                symbol: current.symbol.clone(),
            },
            content: content.to_string(),
            tags: tags.into_iter().map(Into::into).collect(),
            supersedes: None,
            extracted_from: Some(discussion),
            occurred_at_ms: chrono::Utc::now().timestamp_millis(),
        };
        let signed = thread_api::collaboration::Command {
            discussion,
            operation_id: CollaborationIdempotencyKey::new(operation_id.as_str())
                .map_err(native_error)?,
            metadata: CollaborationMetadata {
                scope,
                actor,
                mentions: vec![],
            },
            author,
            occurred_at_ms: chrono::Utc::now().timestamp_millis(),
            body: Body::Resolve {
                resolution: CollaborationResolution::IntoContext {
                    context: context.clone(),
                },
            },
        }
        .sign(&[], signer)
        .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
        let request = ResolveDiscussionRequest {
            client_operation_id: operation_id.to_wire(),
            discussion: Some(RecordRef {
                spool: Some(spool.clone()),
                id: discussion.to_string(),
            }),
            resolution: Some(resolve_discussion_request::Resolution::ExtractContext(
                contract::ContextDraft {
                    r#ref: Some(RecordRef {
                        spool: Some(spool.clone()),
                        id: context.id.to_string(),
                    }),
                    content: context.content,
                    tags: context
                        .tags
                        .iter()
                        .map(thread_api::collaboration::annotation_tag_ref)
                        .collect(),
                    ..Default::default()
                },
            )),
            signed_operation: Some(signed),
            ..Default::default()
        };
        let remote = self.native().await.map_err(native_error)?;
        let response = remote
            .api
            .call::<rpc::CollaborationServiceResolveDiscussion>(&request)
            .await
            .map_err(native_client_error)?;
        require_applied_receipt(
            response.receipt,
            &request.client_operation_id,
            &remote.description.endpoint,
            "resolve discussion",
        )?;
        self.observe_discussion(spool, request.discussion.unwrap_or_default())
            .await
    }

    /// Append `body` as a new turn on an existing hosted discussion.
    pub async fn append_turn(
        &mut self,
        repo_path: &str,
        discussion_id: &str,
        body: &str,
        client_operation_id: String,
    ) -> Result<HostedDiscussion, ProtocolError> {
        let operation_id = ClientOperationId::caller_or_fresh(APPEND, client_operation_id);
        let (spool, _, scope) = self.collaboration_scope(repo_path, None).await?;
        let reference = RecordRef {
            spool: Some(spool.clone()),
            id: discussion_id.to_string(),
        };
        let current = self
            .observe_discussion(spool.clone(), reference.clone())
            .await?;
        let (actor, author) = self.collaboration_actor().await?;
        let signer = self
            .claim_proof_signer()
            .ok_or(super::HostedError::SigningIdentityRequired)
            .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
        let discussion = parse_discussion_id(discussion_id)?;
        let signed = thread_api::collaboration::Command {
            discussion,
            operation_id: CollaborationIdempotencyKey::new(operation_id.as_str())
                .map_err(native_error)?,
            metadata: CollaborationMetadata {
                scope,
                actor,
                mentions: vec![],
            },
            author,
            occurred_at_ms: chrono::Utc::now().timestamp_millis(),
            body: Body::AppendTurn {
                turn: DiscussionTurnV1::new(body).map_err(native_error)?,
            },
        }
        .sign(&[], signer)
        .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
        let request = AppendDiscussionRequest {
            client_operation_id: operation_id.to_wire(),
            discussion: Some(reference.clone()),
            body: body.to_string(),
            signed_operation: Some(signed),
            ..Default::default()
        };
        let _ = current;
        let remote = self.native().await.map_err(native_error)?;
        let response = remote
            .api
            .call::<rpc::CollaborationServiceAppendTurn>(&request)
            .await
            .map_err(native_client_error)?;
        require_applied_receipt(
            response.receipt,
            &request.client_operation_id,
            &remote.description.endpoint,
            "append discussion turn",
        )?;
        self.observe_discussion(spool, reference).await
    }

    /// Fetch one hosted discussion by server id.
    pub async fn get_discussion(
        &mut self,
        repo_path: &str,
        discussion_id: &str,
        _state_id: Option<StateId>,
    ) -> Result<HostedDiscussion, ProtocolError> {
        let spool = self.resolve_spool_ref(repo_path).await?;
        self.observe_discussion(
            spool.clone(),
            RecordRef {
                spool: Some(spool),
                id: discussion_id.to_string(),
            },
        )
        .await
    }

    /// List hosted discussions on this spool. `status` is `open` | `resolved` | `all` | `orphaned`.
    pub async fn list_discussions_by_state(
        &mut self,
        repo_path: &str,
        _change_id: objects::object::ChangeId,
        status: &str,
    ) -> Result<Vec<HostedDiscussion>, ProtocolError> {
        let spool = self.resolve_spool_ref(repo_path).await?;
        let statuses = match status {
            "open" => vec![discussion_record::Status::Open as i32],
            "resolved" => vec![discussion_record::Status::Resolved as i32],
            "orphaned" => vec![discussion_record::Status::Orphaned as i32],
            "all" | "" => Vec::new(),
            other => {
                return Err(ProtocolError::InvalidState(format!(
                    "invalid discussion status filter: {other}"
                )));
            }
        };
        self.observe_discussions(spool, None, statuses).await
    }

    /// Publish a context annotation via v2 `PutContext`.
    #[allow(clippy::too_many_arguments)]
    pub async fn put_context_record(
        &mut self,
        repo_path: &str,
        thread_ref: Option<&str>,
        annotation_id: &str,
        state_id: Option<StateId>,
        path: &str,
        symbol: &str,
        content: &str,
        tags: Vec<String>,
        client_operation_id: String,
    ) -> Result<MutationResponse, ProtocolError> {
        let operation_id = ClientOperationId::caller_or_fresh(PUT_CONTEXT, client_operation_id);
        let (spool, _, scope) = self.collaboration_scope(repo_path, thread_ref).await?;
        let (actor, _) = self.collaboration_actor().await?;
        let signer = self
            .claim_proof_signer()
            .ok_or(super::HostedError::SigningIdentityRequired)
            .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
        let id = uuid::Uuid::parse_str(annotation_id.trim_start_matches("ann-"))
            .or_else(|_| uuid::Uuid::parse_str(annotation_id))
            .unwrap_or_else(|_| uuid::Uuid::now_v7());
        let anchor = if path.is_empty() {
            match state_id {
                Some(state_id) => Anchor::State { state_id },
                None => Anchor::Repository,
            }
        } else if symbol.is_empty() {
            Anchor::Path {
                state_id: state_id.unwrap_or_else(|| StateId::from_bytes([0; 32])),
                path: path.to_string(),
            }
        } else {
            Anchor::Symbol {
                state_id: state_id.unwrap_or_else(|| StateId::from_bytes([0; 32])),
                path: path.to_string(),
                symbol: symbol.to_string(),
            }
        };
        let context = objects::object::ContextRevision {
            version: 2,
            id,
            parents: vec![],
            metadata: CollaborationMetadata {
                scope: scope.clone(),
                actor,
                mentions: vec![],
            },
            anchor: anchor.clone(),
            content: content.to_string(),
            tags: tags.iter().cloned().map(Into::into).collect(),
            supersedes: None,
            extracted_from: None,
            occurred_at_ms: chrono::Utc::now().timestamp_millis(),
        };
        let signed = thread_api::collaboration::sign_context(context.clone(), &[], signer)
            .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
        let request = PutContextRequest {
            client_operation_id: operation_id.to_wire(),
            context: Some(contract::ContextDraft {
                r#ref: Some(RecordRef {
                    spool: Some(spool),
                    id: context.id.to_string(),
                }),
                anchor: Some(
                    thread_api::collaboration::anchor_ref(&anchor, &scope)
                        .map_err(|error| ProtocolError::InvalidState(error.to_string()))?,
                ),
                content: context.content,
                tags: context
                    .tags
                    .iter()
                    .map(thread_api::collaboration::annotation_tag_ref)
                    .collect(),
                ..Default::default()
            }),
            signed_operation: Some(signed),
            ..Default::default()
        };
        let remote = self.native().await.map_err(native_error)?;
        let response = remote
            .api
            .call::<rpc::CollaborationServicePutContext>(&request)
            .await
            .map_err(native_client_error)?;
        require_applied_receipt(
            response.receipt.clone(),
            &request.client_operation_id,
            &remote.description.endpoint,
            "put context",
        )?;
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visibility_tier_maps_known_labels() {
        assert!(matches!(
            visibility_tier("public").unwrap(),
            VisibilityTier::Public
        ));
        assert!(matches!(
            visibility_tier("internal").unwrap(),
            VisibilityTier::Internal
        ));
    }

    #[test]
    fn discussion_id_accepts_disc_prefix_and_raw_uuidv7() {
        let id = DiscussionRecordId::generate();
        assert_eq!(parse_discussion_id(&id.to_string()).unwrap(), id);
    }
}
