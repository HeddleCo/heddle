// SPDX-License-Identifier: Apache-2.0
//! Hosted `CollaborationService` client wrappers over v2 RPCs.
//!
//! Write path: signed original operations via `OpenDiscussion`, `AppendTurn`,
//! and `PutContext`. Read path: `ObserveCollaboration`.

use std::collections::BTreeSet;

use api::heddle::api::v1alpha2::{
    self as contract, AppendDiscussionRequest, Audience, CollaborationAnchor, MutationResponse,
    ObservationMode, ObserveCollaborationRequest, ObserveOptions, OpenDiscussionRequest,
    PageRequest, PutContextRequest, RecordRef, collaboration_anchor, collaboration_event,
    discussion_record, revision_ref,
};
use objects::object::{
    AnnotationTag, Attribution, CollaborationActor, CollaborationAnchor as Anchor,
    CollaborationMetadata, CollaborationOperationBodyV1 as Body, CollaborationOperationEnvelope,
    CollaborationScope, ContentHash, ContextProvenance, ContextRevision, DiscussionRecordId,
    Principal, StateId, VisibilityTier, thread_replication::ThreadOperationBody,
};
use thread_api::rpc;
use wire::ProtocolError;

use super::{
    HostedClient, helpers::native_client_error, operation_id::ClientOperationId,
    user::require_applied_receipt,
};

const OPEN: &str = "heddle.api.v1alpha2.CollaborationService/OpenDiscussion";
const APPEND: &str = "heddle.api.v1alpha2.CollaborationService/AppendTurn";
const PUT_CONTEXT: &str = "heddle.api.v1alpha2.CollaborationService/PutContext";

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
    /// Original turn/open operation id (32-byte causal hash). Empty when absent.
    pub causal_id: Vec<u8>,
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
#[derive(Debug, Clone, Default)]
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
    /// Current discussion heads (turn/open op ids). Empty for a freshly decoded
    /// bootstrap snapshot that has no hosted causal graph.
    pub causal_heads: Vec<Vec<u8>>,
    /// Opaque collaboration view version used as `expected_version` on resolve.
    pub version: Vec<u8>,
}

fn native_error(error: impl std::fmt::Display) -> ProtocolError {
    ProtocolError::InvalidState(error.to_string())
}

const COLLABORATION_PAGE_SIZE: u32 = 64;

fn once_observe() -> ObserveOptions {
    ObserveOptions {
        mode: ObservationMode::Once as i32,
        ..Default::default()
    }
}

fn collaboration_page(after_page: Vec<u8>) -> PageRequest {
    PageRequest {
        size: COLLABORATION_PAGE_SIZE,
        after_page,
    }
}

/// Once-mode ObserveCollaboration returns a single page, then Completes.
/// Kind 0/1 (discussions/turns) sort before kind 2 (context), so a missing
/// page size plus no pagination drops context revisions on a busy spool.
fn ensure_observe_page(request: &mut ObserveCollaborationRequest) {
    if request.observe.is_none() {
        request.observe = Some(once_observe());
    }
    if request.page.as_ref().is_none_or(|page| page.size == 0) {
        let after_page = request
            .page
            .as_ref()
            .map(|page| page.after_page.clone())
            .unwrap_or_default();
        request.page = Some(collaboration_page(after_page));
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

fn discussion_ids_match(left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }
    match (parse_discussion_id(left), parse_discussion_id(right)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn parse_context_id(value: &str) -> Result<uuid::Uuid, ProtocolError> {
    uuid::Uuid::parse_str(value.trim_start_matches("ann-"))
        .or_else(|_| uuid::Uuid::parse_str(value))
        .map_err(|_| {
            ProtocolError::InvalidState(format!(
                "context id {value} is not a UUID; replication is incomplete"
            ))
        })
}

fn causal_hashes(
    ids: impl IntoIterator<Item = Vec<u8>>,
) -> Result<Vec<ContentHash>, ProtocolError> {
    let mut unique = std::collections::BTreeSet::new();
    for id in ids {
        if id.is_empty() {
            continue;
        }
        let bytes: [u8; 32] = id.as_slice().try_into().map_err(|_| {
            ProtocolError::InvalidState("causal parent must contain 32 bytes".into())
        })?;
        unique.insert(ContentHash::from_bytes(bytes));
    }
    Ok(unique.into_iter().collect())
}

fn discussion_parent_ids(discussion: &HostedDiscussion) -> Result<Vec<ContentHash>, ProtocolError> {
    let heads = causal_hashes(discussion.causal_heads.iter().cloned())?;
    if !heads.is_empty() {
        return Ok(heads);
    }
    causal_hashes(
        discussion
            .turns
            .iter()
            .filter(|turn| !turn.causal_id.is_empty())
            .map(|turn| turn.causal_id.clone()),
    )
}

/// ObserveCollaboration `causal_heads` / turn `causal_id` are outer Thread
/// operation ids. Envelope parents must be the inner CollabOpId of those
/// originals, so append/resolve sign the observed signed records.
fn parent_scope(record: &contract::SignedRecord) -> Result<CollaborationScope, ProtocolError> {
    let operation = thread_api::collaboration::verify(record).map_err(native_error)?;
    let ThreadOperationBody::Discussion(bytes) = operation.body else {
        return Err(ProtocolError::InvalidState(
            "discussion parent is not a collaboration operation".into(),
        ));
    };
    CollaborationOperationEnvelope::decode(&bytes)
        .map_err(native_error)?
        .operation
        .metadata
        .map(|metadata| metadata.scope)
        .ok_or_else(|| {
            ProtocolError::InvalidState("discussion parent has no collaboration scope".into())
        })
}

fn signed_head_records(
    operations: &[contract::SignedRecord],
    heads: &[ContentHash],
) -> Result<Vec<contract::SignedRecord>, ProtocolError> {
    let wanted: BTreeSet<ContentHash> = heads.iter().copied().collect();
    let mut found = BTreeSet::new();
    let mut matched = Vec::new();
    for record in operations {
        let operation = thread_api::collaboration::verify(record).map_err(native_error)?;
        let id = operation.id().map_err(native_error)?;
        if wanted.contains(&id) && found.insert(id) {
            matched.push(record.clone());
        }
    }
    if found != wanted {
        return Err(ProtocolError::InvalidState(
            "observed discussion heads are missing original signed operations".into(),
        ));
    }
    Ok(matched)
}

fn parent_bytes(ids: &[ContentHash]) -> Vec<Vec<u8>> {
    ids.iter().map(|id| id.as_bytes().to_vec()).collect()
}

/// Weft admits the request only when `anchor(request.anchor) == signed.anchor`.
/// Symbol/Path/State project to `Source` on the wire, so the signed record must
/// use that canonical form rather than the local Symbol/Path variant.
fn canonical_anchor(
    anchor: Anchor,
    scope: &CollaborationScope,
) -> Result<(Anchor, contract::CollaborationAnchor), ProtocolError> {
    let wire = thread_api::collaboration::anchor_ref(&anchor, scope).map_err(native_error)?;
    let canonical = thread_api::collaboration::anchor(&wire, scope).map_err(native_error)?;
    Ok((canonical, wire))
}

fn decoded_discussion(
    signed: &contract::SignedRecord,
) -> Result<CollaborationOperationEnvelope, ProtocolError> {
    let operation = thread_api::collaboration::verify(signed).map_err(native_error)?;
    let ThreadOperationBody::Discussion(bytes) = operation.body else {
        return Err(ProtocolError::InvalidState(
            "signed collaboration is not a discussion".into(),
        ));
    };
    Ok(CollaborationOperationEnvelope::decode(&bytes)
        .map_err(native_error)?
        .operation)
}

fn decoded_context(signed: &contract::SignedRecord) -> Result<ContextRevision, ProtocolError> {
    let operation = thread_api::collaboration::verify(signed).map_err(native_error)?;
    let ThreadOperationBody::Context(bytes) = operation.body else {
        return Err(ProtocolError::InvalidState(
            "signed collaboration is not a context revision".into(),
        ));
    };
    ContextRevision::decode(&bytes).map_err(native_error)
}

fn open_request_from_signed(
    signed: contract::SignedRecord,
    client_operation_id: String,
    spool: contract::SpoolRef,
    scope: &CollaborationScope,
) -> Result<OpenDiscussionRequest, ProtocolError> {
    let record = decoded_discussion(&signed)?;
    let Body::Open {
        blocking,
        title,
        anchor,
        visibility,
        turn,
        ..
    } = record.body
    else {
        return Err(ProtocolError::InvalidState(
            "signed collaboration is not Open".into(),
        ));
    };
    let (audience, audience_label) = audience_of(&visibility)?;
    Ok(OpenDiscussionRequest {
        client_operation_id,
        spool: Some(spool),
        anchor: Some(thread_api::collaboration::anchor_ref(&anchor, scope).map_err(native_error)?),
        title,
        initial_body: turn.body,
        blocking,
        signed_operation: Some(signed),
        audience,
        audience_label,
    })
}

fn context_draft_from_revision(
    context: &ContextRevision,
    spool: contract::SpoolRef,
) -> Result<contract::ContextDraft, ProtocolError> {
    let reference = |id: String| RecordRef {
        spool: Some(spool.clone()),
        id,
    };
    Ok(contract::ContextDraft {
        r#ref: Some(reference(context.id.to_string())),
        anchor: Some(
            thread_api::collaboration::anchor_ref(&context.anchor, &context.metadata.scope)
                .map_err(native_error)?,
        ),
        content: context.content.clone(),
        tags: context
            .tags
            .iter()
            .map(thread_api::collaboration::annotation_tag_ref)
            .collect(),
        supersedes: context.supersedes.map(|id| reference(id.to_string())),
        extracted_from: context
            .extracted_from
            .as_ref()
            .map(|id| reference(id.to_string())),
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

fn hosted_thread_fields(record: &contract::DiscussionRecord) -> (Option<String>, Option<String>) {
    let title = record.title.trim();
    if title.is_empty() || title.contains(':') {
        return (None, None);
    }
    match title.split_once('\x1f') {
        Some((thread_ref, thread_id)) => (
            Some(thread_ref.to_string()).filter(|value| !value.is_empty()),
            Some(thread_id.to_string()).filter(|value| !value.is_empty()),
        ),
        None => (Some(title.to_string()), None),
    }
}

fn hosted_thread_ref(record: &contract::DiscussionRecord) -> Option<String> {
    hosted_thread_fields(record).0
}

fn hosted_thread_id(record: &contract::DiscussionRecord) -> Option<String> {
    hosted_thread_fields(record).1
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
            reason: record.title.clone(),
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
        thread_ref: hosted_thread_ref(record),
        thread_id: hosted_thread_id(record),
        turns,
        resolution,
        kind: 0,
        causal_heads: record.causal_heads.clone(),
        version: record.version.clone(),
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
        let reference = self
            .resolve_thread_ref(repo_path, name)
            .await
            .map_err(|error| {
                ProtocolError::InvalidState(format!(
                    "cannot resolve requested collaboration scope {name:?}: {error}; no collaboration write was submitted"
                ))
            })?;
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
        let (principal, credential) = self.observe_current_identity().await?;
        let principal_id = uuid::Uuid::parse_str(&principal.account_id)
            .or_else(|_| uuid::Uuid::parse_str(&principal.id))
            .map_err(native_error)?;
        let agent_id = if credential.acting_agent_id.is_empty() {
            None
        } else {
            Some(credential.acting_agent_id)
        };
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

    /// Bind a locally authored open operation to its exact hosted scope and
    /// actor, then sign the complete authored body. Callers persist the
    /// returned record before delivery so retries transmit identical bytes.
    pub(crate) async fn prepare_open_discussion_operation(
        &mut self,
        repo_path: &str,
        default_thread_ref: &str,
        authored: &CollaborationOperationEnvelope,
    ) -> Result<contract::SignedRecord, ProtocolError> {
        let Body::Open {
            blocking,
            title,
            anchor,
            visibility,
            turn,
            thread_ref,
        } = &authored.body
        else {
            return Err(ProtocolError::InvalidState(
                "native discussion preparation requires an authored Open operation".into(),
            ));
        };
        let requested_thread_ref = thread_ref.as_deref().unwrap_or(default_thread_ref);
        let (_, _, scope) = self
            .collaboration_scope(repo_path, Some(requested_thread_ref))
            .await?;
        let (actor, _) = self.collaboration_actor().await?;
        let signer = self
            .claim_proof_signer()
            .ok_or(super::HostedError::SigningIdentityRequired)
            .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
        let (anchor, _) = canonical_anchor(anchor.clone(), &scope)?;
        thread_api::collaboration::Command {
            discussion: authored.discussion_id,
            operation_id: authored.idempotency_key.clone(),
            metadata: CollaborationMetadata {
                scope,
                actor,
                mentions: vec![],
            },
            author: authored.author.clone(),
            occurred_at_ms: authored.occurred_at_ms,
            body: Body::Open {
                blocking: *blocking,
                title: title.clone(),
                anchor,
                visibility: visibility.clone(),
                turn: turn.clone(),
                thread_ref: thread_ref.clone(),
            },
        }
        .sign(&[], signer)
        .map_err(native_error)
    }

    /// Deliver an already prepared Open operation without reconstructing or
    /// restamping any authored field.
    pub(crate) async fn publish_open_discussion_operation(
        &self,
        signed: contract::SignedRecord,
        client_operation_id: String,
    ) -> Result<HostedDiscussion, ProtocolError> {
        let operation_id = ClientOperationId::caller_or_fresh(OPEN, client_operation_id);
        let record = decoded_discussion(&signed)?;
        let metadata = record.metadata.as_ref().ok_or_else(|| {
            ProtocolError::InvalidState("signed discussion metadata is absent".into())
        })?;
        let spool = contract::SpoolRef {
            id: metadata.scope.spool.to_string(),
        };
        let request = open_request_from_signed(
            signed,
            operation_id.to_wire(),
            spool.clone(),
            &metadata.scope,
        )?;
        let discussion = request
            .signed_operation
            .as_ref()
            .map(decoded_discussion)
            .transpose()?
            .ok_or_else(|| ProtocolError::InvalidState("signed discussion is absent".into()))?
            .discussion_id;
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

    pub(crate) async fn observe_collaboration_events(
        &self,
        mut request: ObserveCollaborationRequest,
    ) -> Result<Vec<collaboration_event::Payload>, ProtocolError> {
        ensure_observe_page(&mut request);
        let remote = self.native().await.map_err(native_error)?;
        let mut payloads = Vec::new();
        loop {
            let mut observation = remote
                .observe::<rpc::CollaborationServiceObserveCollaboration>(request.clone(), None)
                .await
                .map_err(native_error)?;
            let mut exhausted = true;
            let mut next_page = Vec::new();
            while let Some(batch) = observation.next_commit().await.map_err(native_error)? {
                if let Some(page) = &batch.page {
                    exhausted = page.exhausted;
                    next_page = page.next_page.clone();
                }
                payloads.extend(batch.changes);
            }
            if exhausted {
                break;
            }
            if next_page.is_empty()
                || request
                    .page
                    .as_ref()
                    .is_some_and(|page| page.after_page == next_page)
            {
                return Err(ProtocolError::InvalidState(
                    "collaboration page cursor did not advance".into(),
                ));
            }
            request.page.get_or_insert_default().after_page = next_page;
        }
        Ok(payloads)
    }

    fn state_anchor(state_id: Option<StateId>) -> Vec<contract::CollaborationAnchor> {
        let Some(state_id) = state_id else {
            return Vec::new();
        };
        vec![contract::CollaborationAnchor {
            target: Some(collaboration_anchor::Target::Source(
                contract::SourceAnchor {
                    revision: Some(contract::RevisionRef {
                        revision: Some(revision_ref::Revision::State(
                            api::heddle::api::common::StateId {
                                value: state_id.as_bytes().to_vec(),
                            },
                        )),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )),
        }]
    }

    async fn observe_discussion(
        &self,
        spool: contract::SpoolRef,
        discussion: RecordRef,
    ) -> Result<HostedDiscussion, ProtocolError> {
        self.observe_discussion_at(spool, discussion, None).await
    }

    async fn observe_discussion_at(
        &self,
        spool: contract::SpoolRef,
        discussion: RecordRef,
        state_id: Option<StateId>,
    ) -> Result<HostedDiscussion, ProtocolError> {
        let (discussions, _) = self
            .observe_discussions(
                spool,
                Some(discussion),
                Vec::new(),
                Self::state_anchor(state_id),
                false,
            )
            .await?;
        discussions.into_iter().next().ok_or_else(|| {
            ProtocolError::ObjectNotFound(
                "discussion is not in the hosted collaboration view".into(),
            )
        })
    }

    async fn observe_discussion_heads(
        &self,
        spool: contract::SpoolRef,
        discussion: RecordRef,
    ) -> Result<(HostedDiscussion, Vec<contract::SignedRecord>), ProtocolError> {
        let (discussions, operations) = self
            .observe_discussions(spool, Some(discussion), Vec::new(), Vec::new(), true)
            .await?;
        let hosted = discussions.into_iter().next().ok_or_else(|| {
            ProtocolError::ObjectNotFound(
                "discussion is not in the hosted collaboration view".into(),
            )
        })?;
        Ok((hosted, operations))
    }

    async fn observe_discussions(
        &self,
        spool: contract::SpoolRef,
        discussion: Option<RecordRef>,
        statuses: Vec<i32>,
        anchors: Vec<contract::CollaborationAnchor>,
        include_operations: bool,
    ) -> Result<(Vec<HostedDiscussion>, Vec<contract::SignedRecord>), ProtocolError> {
        let events = self
            .observe_collaboration_events(ObserveCollaborationRequest {
                spool: Some(spool),
                discussions: discussion.into_iter().collect(),
                statuses,
                include_history: true,
                include_operations,
                anchors,
                observe: Some(once_observe()),
                ..Default::default()
            })
            .await?;
        let mut records = Vec::new();
        let mut turns: Vec<contract::DiscussionTurn> = Vec::new();
        let mut operations = Vec::new();
        for change in events {
            match change {
                collaboration_event::Payload::Discussion(record) => records.push(record),
                collaboration_event::Payload::Turn(turn) => turns.push(turn),
                collaboration_event::Payload::Operation(operation) => operations.push(operation),
                _ => {}
            }
        }
        let discussions = records
            .into_iter()
            .map(|record| {
                let id = record.r#ref.as_ref().map(|value| value.id.as_str());
                let mut matching: Vec<_> = turns
                    .iter()
                    .filter(|turn| {
                        turn.discussion.as_ref().is_some_and(|reference| {
                            Some(reference.id.as_str()) == id
                                || discussion_ids_match(
                                    reference.id.as_str(),
                                    id.unwrap_or_default(),
                                )
                        })
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
                        causal_id: turn.causal_id,
                    })
                    .collect();
                hosted_from_record(&record, hosted_turns)
            })
            .collect();
        Ok((discussions, operations))
    }

    async fn observe_context_heads(
        &self,
        spool: contract::SpoolRef,
        annotation_id: &str,
    ) -> Result<(Vec<ContentHash>, Vec<u8>), ProtocolError> {
        let id = annotation_id.trim_start_matches("ann-").to_string();
        let events = self
            .observe_collaboration_events(ObserveCollaborationRequest {
                spool: Some(spool.clone()),
                contexts: vec![RecordRef {
                    spool: Some(spool),
                    id: id.clone(),
                }],
                include_history: true,
                observe: Some(once_observe()),
                ..Default::default()
            })
            .await?;
        let Some(record) = events.into_iter().rev().find_map(|change| match change {
            collaboration_event::Payload::Context(record)
                if record.r#ref.as_ref().is_some_and(|reference| {
                    reference.id == id || reference.id == annotation_id
                }) =>
            {
                Some(record)
            }
            _ => None,
        }) else {
            return Ok((Vec::new(), Vec::new()));
        };
        let mut heads = causal_hashes(record.causal_heads)?;
        if heads.is_empty() && record.causal_id.len() == 32 {
            heads = causal_hashes(std::iter::once(record.causal_id))?;
        }
        Ok((heads, record.version))
    }

    /// Prepare an authored append against the observed native frontier.
    pub(crate) async fn prepare_append_discussion_operation(
        &mut self,
        repo_path: &str,
        discussion_id: &str,
        authored: &CollaborationOperationEnvelope,
    ) -> Result<contract::SignedRecord, ProtocolError> {
        let Body::AppendTurn { turn } = &authored.body else {
            return Err(ProtocolError::InvalidState(
                "unsupported authored discussion operation; native replication is incomplete"
                    .into(),
            ));
        };
        let spool = self.resolve_spool_ref(repo_path).await?;
        let reference = RecordRef {
            spool: Some(spool.clone()),
            id: discussion_id.to_string(),
        };
        let (current, operations) = self
            .observe_discussion_heads(spool.clone(), reference.clone())
            .await?;
        let parents = discussion_parent_ids(&current)?;
        if parents.is_empty() {
            return Err(ProtocolError::InvalidState(
                "non-root collaboration operation requires a parent".into(),
            ));
        }
        let parent_records = signed_head_records(&operations, &parents)?;
        let scope = parent_scope(&parent_records[0])?;
        let (actor, _) = self.collaboration_actor().await?;
        let signer = self
            .claim_proof_signer()
            .ok_or(super::HostedError::SigningIdentityRequired)
            .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
        let discussion = parse_discussion_id(discussion_id)?;
        if discussion != authored.discussion_id {
            return Err(ProtocolError::InvalidState(
                "authored turn belongs to another discussion".into(),
            ));
        }
        let signed = thread_api::collaboration::Command {
            discussion,
            operation_id: authored.idempotency_key.clone(),
            metadata: CollaborationMetadata {
                scope,
                actor,
                mentions: vec![],
            },
            author: authored.author.clone(),
            occurred_at_ms: authored.occurred_at_ms,
            body: Body::AppendTurn { turn: turn.clone() },
        }
        .sign(&parent_records, signer)
        .map_err(native_error)?;
        Ok(signed)
    }

    /// Deliver a prepared append operation unchanged.
    pub(crate) async fn publish_append_discussion_operation(
        &self,
        signed: contract::SignedRecord,
        client_operation_id: String,
    ) -> Result<HostedDiscussion, ProtocolError> {
        let operation_id = ClientOperationId::caller_or_fresh(APPEND, client_operation_id);
        let outer = thread_api::collaboration::verify(&signed).map_err(native_error)?;
        let parents: Vec<_> = outer.parents.iter().copied().collect();
        let record = decoded_discussion(&signed)?;
        let Body::AppendTurn { turn } = &record.body else {
            return Err(ProtocolError::InvalidState(
                "prepared native discussion operation is not an append".into(),
            ));
        };
        let metadata = record.metadata.as_ref().ok_or_else(|| {
            ProtocolError::InvalidState("signed discussion metadata is absent".into())
        })?;
        let spool = contract::SpoolRef {
            id: metadata.scope.spool.to_string(),
        };
        let reference = RecordRef {
            spool: Some(spool.clone()),
            id: record.discussion_id.to_string(),
        };
        let request = AppendDiscussionRequest {
            client_operation_id: operation_id.to_wire(),
            discussion: Some(reference.clone()),
            body: turn.body.clone(),
            signed_operation: Some(signed),
            causal_parents: parent_bytes(&parents),
            mentions: Vec::new(),
        };
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
        state_id: Option<StateId>,
    ) -> Result<HostedDiscussion, ProtocolError> {
        let spool = self.resolve_spool_ref(repo_path).await?;
        self.observe_discussion_at(
            spool.clone(),
            RecordRef {
                spool: Some(spool),
                id: discussion_id.to_string(),
            },
            state_id,
        )
        .await
    }

    /// List hosted discussions on this spool via ObserveCollaboration.
    /// `status` is `open` | `resolved` | `all` | `orphaned`.
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
        let (discussions, _) = self
            .observe_discussions(spool, None, statuses, Vec::new(), false)
            .await?;
        Ok(discussions)
    }

    /// Return the verified transport originals used by native replication.
    /// Projection-only discussion records are intentionally excluded.
    pub(crate) async fn list_discussion_operations(
        &mut self,
        repo_path: &str,
    ) -> Result<Vec<contract::SignedRecord>, ProtocolError> {
        let spool = self.resolve_spool_ref(repo_path).await?;
        let (_, operations) = self
            .observe_discussions(spool, None, Vec::new(), Vec::new(), true)
            .await?;
        Ok(operations)
    }

    pub(crate) async fn list_context_operations(
        &mut self,
        repo_path: &str,
    ) -> Result<Vec<contract::SignedRecord>, ProtocolError> {
        let spool = self.resolve_spool_ref(repo_path).await?;
        let events = self
            .observe_collaboration_events(ObserveCollaborationRequest {
                spool: Some(spool),
                include_history: true,
                include_operations: true,
                annotations: Some(contract::AnnotationQuery::default()),
                observe: Some(once_observe()),
                ..Default::default()
            })
            .await?;
        Ok(events
            .into_iter()
            .filter_map(|event| match event {
                collaboration_event::Payload::Operation(record) => Some(record),
                _ => None,
            })
            .collect())
    }

    /// Prepare and sign a native context revision. Callers persist both the
    /// signed record and expected version before delivery.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn prepare_native_context_record(
        &mut self,
        repo_path: &str,
        thread_ref: Option<&str>,
        annotation_id: &str,
        anchor: Anchor,
        content: &str,
        tags: Vec<AnnotationTag>,
        provenance: ContextProvenance,
        occurred_at_ms: i64,
        supersedes: Option<uuid::Uuid>,
    ) -> Result<(contract::SignedRecord, Vec<u8>), ProtocolError> {
        let (spool, _, scope) = self.collaboration_scope(repo_path, thread_ref).await?;
        let (actor, _) = self.collaboration_actor().await?;
        let signer = self
            .claim_proof_signer()
            .ok_or(super::HostedError::SigningIdentityRequired)
            .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
        let id = parse_context_id(annotation_id)?;
        let (anchor, _) = canonical_anchor(anchor, &scope)?;
        let (parent_ids, expected_version) = self
            .observe_context_heads(spool.clone(), annotation_id)
            .await?;
        let context = ContextRevision {
            version: 2,
            id,
            parents: vec![],
            metadata: CollaborationMetadata {
                scope: scope.clone(),
                actor,
                mentions: vec![],
            },
            anchor,
            content: content.to_string(),
            tags,
            supersedes,
            extracted_from: None,
            occurred_at_ms,
            provenance: Some(provenance),
        };
        let signed =
            thread_api::collaboration::sign_context_parent_ids(context, &parent_ids, signer)
                .map_err(native_error)?;
        Ok((signed, expected_version))
    }

    /// Deliver a prepared context revision without resolving scope or
    /// reconstructing authored fields.
    pub(crate) async fn publish_native_context_record(
        &self,
        signed: contract::SignedRecord,
        expected_version: Vec<u8>,
        client_operation_id: String,
    ) -> Result<MutationResponse, ProtocolError> {
        let operation_id = ClientOperationId::caller_or_fresh(PUT_CONTEXT, client_operation_id);
        let record = decoded_context(&signed)?;
        let spool = contract::SpoolRef {
            id: record.metadata.scope.spool.to_string(),
        };
        let request = PutContextRequest {
            client_operation_id: operation_id.to_wire(),
            context: Some(context_draft_from_revision(&record, spool)?),
            signed_operation: Some(signed),
            expected_version,
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
    use objects::object::{
        AnnotationKind, CollaborationIdempotencyKey, ContentHash, DiscussionTurnV1,
    };
    use uuid::Uuid;

    use super::*;

    #[test]
    fn discussion_id_accepts_disc_prefix_and_raw_uuidv7() {
        let _process_env_guard = crate::test_process_env::shared_blocking();
        let id = DiscussionRecordId::generate();
        assert_eq!(parse_discussion_id(&id.to_string()).unwrap(), id);
        let raw = id.to_string().trim_start_matches("disc-").to_string();
        assert!(discussion_ids_match(&id.to_string(), &raw));
    }

    #[test]
    fn discussion_parent_ids_are_sorted_unique_heads() {
        let _process_env_guard = crate::test_process_env::shared_blocking();
        let discussion = HostedDiscussion {
            causal_heads: vec![vec![2; 32], vec![1; 32], vec![2; 32], vec![]],
            turns: vec![HostedDiscussionTurn {
                causal_id: vec![9; 32],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(
            discussion_parent_ids(&discussion).unwrap(),
            vec![
                ContentHash::from_bytes([1; 32]),
                ContentHash::from_bytes([2; 32])
            ]
        );
        let from_turns = HostedDiscussion {
            turns: vec![
                HostedDiscussionTurn {
                    causal_id: vec![4; 32],
                    ..Default::default()
                },
                HostedDiscussionTurn {
                    causal_id: vec![3; 32],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            discussion_parent_ids(&from_turns).unwrap(),
            vec![
                ContentHash::from_bytes([3; 32]),
                ContentHash::from_bytes([4; 32])
            ]
        );
    }

    #[test]
    fn missing_observe_page_is_filled() {
        let _process_env_guard = crate::test_process_env::shared_blocking();
        let mut request = ObserveCollaborationRequest::default();
        ensure_observe_page(&mut request);
        assert_eq!(
            request.observe.as_ref().map(|options| options.mode),
            Some(ObservationMode::Once as i32)
        );
        assert_eq!(
            request.page.as_ref().map(|page| page.size),
            Some(COLLABORATION_PAGE_SIZE)
        );
    }

    fn proof_scope() -> CollaborationScope {
        CollaborationScope {
            spool: Uuid::from_u128(1),
            thread: Some(ContentHash::from_bytes([2; 32])),
        }
    }

    fn proof_metadata(scope: CollaborationScope) -> CollaborationMetadata {
        CollaborationMetadata {
            scope,
            actor: CollaborationActor {
                principal_id: Uuid::from_u128(3),
                agent_id: None,
            },
            mentions: vec![],
        }
    }

    fn proof_signer() -> crypto::Ed25519Signer {
        crypto::Ed25519Signer::from_seed(&[7; 32]).expect("signer")
    }

    /// Weft `OpenDiscussion` field check, named so a mismatch prints the
    /// exact request vs signed values instead of a boolean OR.
    fn open_field_diffs(
        request: &OpenDiscussionRequest,
        signed: &contract::SignedRecord,
        scope: &CollaborationScope,
    ) -> Vec<String> {
        let operation = thread_api::collaboration::verify(signed).expect("verify");
        let ThreadOperationBody::Discussion(bytes) = &operation.body else {
            panic!("expected discussion operation");
        };
        let record = CollaborationOperationEnvelope::decode(bytes)
            .expect("decode")
            .operation;
        let Body::Open {
            blocking,
            title,
            anchor,
            visibility,
            turn,
            ..
        } = &record.body
        else {
            panic!("expected Open");
        };
        let mut diffs = Vec::new();
        if request.title != *title {
            diffs.push(format!(
                "title request={:?} signed={:?}",
                request.title, title
            ));
        }
        if request.initial_body != turn.body {
            diffs.push(format!(
                "initial_body request={:?} signed_turn.body={:?}",
                request.initial_body, turn.body
            ));
        }
        if request.blocking != *blocking {
            diffs.push(format!(
                "blocking request={} signed={}",
                request.blocking, blocking
            ));
        }
        let request_visibility =
            thread_api::collaboration::visibility(request.audience, &request.audience_label)
                .expect("audience");
        if request_visibility != *visibility {
            diffs.push(format!(
                "audience/visibility request={request_visibility:?} signed={visibility:?}"
            ));
        }
        let request_anchor = thread_api::collaboration::anchor(
            request.anchor.as_ref().expect("anchor required"),
            scope,
        )
        .expect("request anchor");
        if request_anchor != *anchor {
            diffs.push(format!(
                "anchor request={request_anchor:?} signed={anchor:?}"
            ));
        }
        diffs
    }

    fn context_field_diffs(
        draft: &contract::ContextDraft,
        signed: &contract::SignedRecord,
        spool: &contract::SpoolRef,
    ) -> Vec<String> {
        let operation = thread_api::collaboration::verify(signed).expect("verify");
        let ThreadOperationBody::Context(bytes) = &operation.body else {
            panic!("expected context operation");
        };
        let record = ContextRevision::decode(bytes).expect("decode");
        let reference = |id: String| RecordRef {
            spool: Some(spool.clone()),
            id,
        };
        let mut diffs = Vec::new();
        if draft.r#ref.as_ref() != Some(&reference(record.id.to_string())) {
            diffs.push(format!(
                "ref request={:?} signed={}",
                draft.r#ref, record.id
            ));
        }
        if draft.content != record.content {
            diffs.push(format!(
                "content request={:?} signed={:?}",
                draft.content, record.content
            ));
        }
        let request_tags = thread_api::collaboration::annotation_tags(&draft.tags).expect("tags");
        if request_tags != record.tags {
            diffs.push(format!(
                "tags request={request_tags:?} signed={:?}",
                record.tags
            ));
        }
        let signed_supersedes = record.supersedes.map(|id| reference(id.to_string()));
        if draft.supersedes != signed_supersedes {
            diffs.push(format!(
                "supersedes request={:?} signed={:?}",
                draft.supersedes, signed_supersedes
            ));
        }
        let signed_extracted = record.extracted_from.map(|id| reference(id.to_string()));
        if draft.extracted_from != signed_extracted {
            diffs.push(format!(
                "extracted_from request={:?} signed={:?}",
                draft.extracted_from, signed_extracted
            ));
        }
        let request_anchor = thread_api::collaboration::anchor(
            draft.anchor.as_ref().expect("context anchor required"),
            &record.metadata.scope,
        )
        .expect("request anchor");
        if request_anchor != record.anchor {
            diffs.push(format!(
                "anchor request={request_anchor:?} signed={:?}",
                record.anchor
            ));
        }
        diffs
    }

    #[test]
    fn parallel_open_reconstruction_diffs_only_on_symbol_vs_source_anchor() {
        let _process_env_guard = crate::test_process_env::shared_blocking();
        // Pre-fix construction: sign `Anchor::Symbol`, put `anchor_ref(Symbol)`
        // (a Source) on the request. Title/body/blocking/audience already match.
        let scope = proof_scope();
        let file = "src/main.rs";
        let symbol = "run";
        let body = "please review";
        let tier = VisibilityTier::Internal;
        let (audience, audience_label) = audience_of(&tier).unwrap();
        let anchor = Anchor::Symbol {
            state_id: StateId::from_bytes([9; 32]),
            path: file.to_string(),
            symbol: symbol.to_string(),
        };
        let signed = thread_api::collaboration::Command {
            discussion: DiscussionRecordId::generate(),
            operation_id: CollaborationIdempotencyKey::new("op-open").unwrap(),
            metadata: proof_metadata(scope.clone()),
            author: Attribution::human(Principal::new("alice", "")),
            occurred_at_ms: 1,
            body: Body::Open {
                blocking: false,
                title: format!("{file}:{symbol}"),
                anchor: anchor.clone(),
                visibility: tier,
                turn: DiscussionTurnV1::new(body).unwrap(),
                thread_ref: None,
            },
        }
        .sign(&[], &proof_signer())
        .unwrap();
        let request = OpenDiscussionRequest {
            client_operation_id: "op-open".into(),
            spool: Some(contract::SpoolRef {
                id: scope.spool.to_string(),
            }),
            anchor: Some(thread_api::collaboration::anchor_ref(&anchor, &scope).unwrap()),
            title: format!("{file}:{symbol}"),
            initial_body: body.to_string(),
            signed_operation: Some(signed.clone()),
            audience,
            audience_label,
            ..Default::default()
        };
        let diffs = open_field_diffs(&request, &signed, &scope);
        assert_eq!(diffs.len(), 1, "unexpected extra diffs: {diffs:?}");
        assert!(
            diffs[0].starts_with("anchor request=Source"),
            "expected Symbol vs Source, got {diffs:?}"
        );
        assert!(
            diffs[0].contains("signed=Symbol"),
            "expected signed Symbol, got {diffs:?}"
        );
    }

    #[test]
    fn open_discussion_request_fields_match_signed_record() {
        let _process_env_guard = crate::test_process_env::shared_blocking();
        let scope = proof_scope();
        let file = "src/main.rs";
        let symbol = "run";
        let body = "please review";
        let (anchor, _) = canonical_anchor(
            Anchor::Symbol {
                state_id: StateId::from_bytes([9; 32]),
                path: file.to_string(),
                symbol: symbol.to_string(),
            },
            &scope,
        )
        .unwrap();
        let signed = thread_api::collaboration::Command {
            discussion: DiscussionRecordId::generate(),
            operation_id: CollaborationIdempotencyKey::new("op-open").unwrap(),
            metadata: proof_metadata(scope.clone()),
            author: Attribution::human(Principal::new("alice", "")),
            occurred_at_ms: 1,
            body: Body::Open {
                blocking: false,
                title: format!("{file}:{symbol}"),
                anchor,
                visibility: VisibilityTier::Internal,
                turn: DiscussionTurnV1::new(body).unwrap(),
                thread_ref: None,
            },
        }
        .sign(&[], &proof_signer())
        .unwrap();
        let request = open_request_from_signed(
            signed.clone(),
            "op-open".into(),
            contract::SpoolRef {
                id: scope.spool.to_string(),
            },
            &scope,
        )
        .unwrap();
        let diffs = open_field_diffs(&request, &signed, &scope);
        assert!(
            diffs.is_empty(),
            "OpenDiscussion fields differ from signed record: {diffs:?}"
        );
    }

    #[test]
    fn parallel_context_reconstruction_drops_supersedes_and_symbol_anchor() {
        let _process_env_guard = crate::test_process_env::shared_blocking();
        let scope = proof_scope();
        let superseded = Uuid::from_u128(11);
        let context = ContextRevision {
            version: 2,
            id: Uuid::from_u128(9),
            parents: vec![],
            metadata: proof_metadata(scope.clone()),
            anchor: Anchor::Symbol {
                state_id: StateId::from_bytes([9; 32]),
                path: "src/lib.rs".into(),
                symbol: "entry".into(),
            },
            content: "the decision".into(),
            tags: vec!["decision".into()],
            supersedes: Some(superseded),
            extracted_from: None,
            occurred_at_ms: 1,
            provenance: None,
        };
        let signed =
            thread_api::collaboration::sign_context(context.clone(), &[], &proof_signer()).unwrap();
        let spool = contract::SpoolRef {
            id: scope.spool.to_string(),
        };
        let draft = contract::ContextDraft {
            r#ref: Some(RecordRef {
                spool: Some(spool.clone()),
                id: context.id.to_string(),
            }),
            anchor: Some(thread_api::collaboration::anchor_ref(&context.anchor, &scope).unwrap()),
            content: context.content,
            tags: context
                .tags
                .iter()
                .map(thread_api::collaboration::annotation_tag_ref)
                .collect(),
            ..Default::default()
        };
        let diffs = context_field_diffs(&draft, &signed, &spool);
        assert!(
            diffs.iter().any(|diff| diff.starts_with("supersedes ")),
            "expected supersedes drop, got {diffs:?}"
        );
        assert!(
            diffs
                .iter()
                .any(|diff| diff.starts_with("anchor request=Source")),
            "expected Symbol vs Source, got {diffs:?}"
        );
    }

    #[test]
    fn put_context_draft_fields_match_signed_revision() {
        let _process_env_guard = crate::test_process_env::shared_blocking();
        let scope = proof_scope();
        let (anchor, _) = canonical_anchor(
            Anchor::Symbol {
                state_id: StateId::from_bytes([9; 32]),
                path: "src/lib.rs".into(),
                symbol: "entry".into(),
            },
            &scope,
        )
        .unwrap();
        let context = ContextRevision {
            version: 2,
            id: Uuid::from_u128(9),
            parents: vec![],
            metadata: proof_metadata(scope.clone()),
            anchor,
            content: "the decision".into(),
            tags: vec!["decision".into()],
            supersedes: Some(Uuid::from_u128(11)),
            extracted_from: None,
            occurred_at_ms: 1,
            provenance: None,
        };
        let signed =
            thread_api::collaboration::sign_context(context, &[], &proof_signer()).unwrap();
        let spool = contract::SpoolRef {
            id: scope.spool.to_string(),
        };
        let record = decoded_context(&signed).unwrap();
        let draft = context_draft_from_revision(&record, spool.clone()).unwrap();
        let diffs = context_field_diffs(&draft, &signed, &spool);
        assert!(
            diffs.is_empty(),
            "context fields differ from signed revision: {diffs:?}"
        );
    }

    #[test]
    fn native_discussion_open_preserves_every_supported_anchor_and_authored_evidence() {
        let _process_env_guard = crate::test_process_env::shared_blocking();
        let scope = proof_scope();
        let state = StateId::from_bytes([9; 32]);
        let anchors = vec![
            Anchor::Repository,
            Anchor::Path {
                state_id: state,
                path: "src/path.rs".into(),
            },
            Anchor::Source {
                source: objects::object::CollaborationSourceAnchor {
                    revision: objects::object::CollaborationRevision::State { state_id: state },
                    path: "src/line.rs".into(),
                    symbol_id: String::new(),
                    start_line: Some(17),
                    end_line: Some(19),
                    target: None,
                },
            },
            Anchor::Symbol {
                state_id: state,
                path: "src/symbol.rs".into(),
                symbol: "entry".into(),
            },
        ];
        for local_anchor in anchors {
            let (anchor, _) = canonical_anchor(local_anchor, &scope).expect("canonical anchor");
            let author = Attribution::human(Principal::new("Original Author", "author@test"));
            let signed = thread_api::collaboration::Command {
                discussion: DiscussionRecordId::generate(),
                operation_id: CollaborationIdempotencyKey::new("authored-operation")
                    .expect("idempotency"),
                metadata: proof_metadata(scope.clone()),
                author: author.clone(),
                occurred_at_ms: 1_726_000_123_456,
                body: Body::Open {
                    blocking: true,
                    title: "Authored title".into(),
                    anchor: anchor.clone(),
                    visibility: VisibilityTier::Internal,
                    turn: DiscussionTurnV1::new("authored body").expect("turn"),
                    thread_ref: Some("feature/provenance".into()),
                },
            }
            .sign(&[], &proof_signer())
            .expect("signed");
            let request = open_request_from_signed(
                signed.clone(),
                Uuid::now_v7().to_string(),
                contract::SpoolRef {
                    id: scope.spool.to_string(),
                },
                &scope,
            )
            .expect("request");
            assert!(
                open_field_diffs(&request, &signed, &scope).is_empty(),
                "request must be a projection of the signed original"
            );
            let decoded = decoded_discussion(&signed).expect("decode signed discussion");
            assert_eq!(decoded.author, author);
            assert_eq!(decoded.occurred_at_ms, 1_726_000_123_456);
            assert_eq!(decoded.metadata.expect("metadata").scope, scope);
            let Body::Open {
                title,
                anchor: decoded_anchor,
                thread_ref,
                ..
            } = decoded.body
            else {
                panic!("open")
            };
            assert_eq!(title, "Authored title");
            assert_eq!(decoded_anchor, anchor);
            assert_eq!(thread_ref.as_deref(), Some("feature/provenance"));
        }
    }

    #[test]
    fn native_context_edit_preserves_anchor_scope_creation_state_and_time() {
        let _process_env_guard = crate::test_process_env::shared_blocking();
        let scope = proof_scope();
        let state = StateId::from_bytes([9; 32]);
        let (anchor, _) = canonical_anchor(
            Anchor::Source {
                source: objects::object::CollaborationSourceAnchor {
                    revision: objects::object::CollaborationRevision::State { state_id: state },
                    path: "src/line.rs".into(),
                    symbol_id: "entry".into(),
                    start_line: Some(21),
                    end_line: Some(24),
                    target: None,
                },
            },
            &scope,
        )
        .expect("canonical anchor");
        let provenance = ContextProvenance {
            revision_id: Uuid::now_v7().to_string(),
            kind: AnnotationKind::Invariant,
            attribution: "Original Author <author@test> (via codex/model)".into(),
            source_hash: Some(ContentHash::from_bytes([8; 32])),
            created_at_state: Some(state),
        };
        let first = ContextRevision {
            version: 2,
            id: Uuid::now_v7(),
            parents: vec![],
            metadata: proof_metadata(scope.clone()),
            anchor: anchor.clone(),
            content: "first".into(),
            tags: vec!["tag".into()],
            supersedes: None,
            extracted_from: None,
            occurred_at_ms: 1_726_000_123_456,
            provenance: Some(provenance.clone()),
        };
        let signed_first =
            thread_api::collaboration::sign_context(first.clone(), &[], &proof_signer())
                .expect("first");
        let mut edit = first;
        edit.content = "edited".into();
        edit.occurred_at_ms += 77;
        edit.provenance.as_mut().expect("provenance").revision_id = Uuid::now_v7().to_string();
        let signed_edit =
            thread_api::collaboration::sign_context(edit.clone(), &[signed_first], &proof_signer())
                .expect("edit");
        let decoded = decoded_context(&signed_edit).expect("decode edit");
        assert_eq!(decoded.anchor, anchor);
        assert_eq!(decoded.metadata.scope, scope);
        assert_eq!(decoded.occurred_at_ms, edit.occurred_at_ms);
        assert_eq!(decoded.provenance, edit.provenance);
        assert_eq!(
            decoded
                .provenance
                .as_ref()
                .and_then(|value| value.created_at_state),
            Some(state)
        );
    }

    #[tokio::test]
    async fn requested_scope_failure_never_retargets_main() {
        let _process_env_guard = crate::test_process_env::shared().await;
        use crate::hosted_runtime::hosted::test_server;
        use api::heddle::api::common::CallFailureCode;

        for failure in [
            None,
            Some(CallFailureCode::PermissionDenied),
            Some(CallFailureCode::Unavailable),
        ] {
            let fixture = test_server::ThreadListingFixture {
                overviews: Vec::new(),
                page_size: 64,
                requests: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                resolution_failure: failure,
                resolution_requests: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            };
            let (mut client, server, captured) =
                test_server::start_with_thread_listing(fixture).await;
            let authored = CollaborationOperationEnvelope::new(
                DiscussionRecordId::generate(),
                Vec::new(),
                CollaborationIdempotencyKey::new("exact-scope").expect("key"),
                Attribution::human(Principal::new("Original Author", "author@test")),
                1_726_000_123_456,
                Body::Open {
                    blocking: false,
                    title: "exact scope".into(),
                    anchor: Anchor::Repository,
                    visibility: VisibilityTier::Internal,
                    turn: DiscussionTurnV1::new("body").expect("turn"),
                    thread_ref: None,
                },
            )
            .expect("authored operation");
            let error = client
                .prepare_open_discussion_operation("acme/widgets", "feature/missing", &authored)
                .await
                .expect_err("the requested scope must fail");
            let message = error.to_string();
            assert!(message.contains("feature/missing"), "{message}");
            assert!(
                message.contains("no collaboration write was submitted"),
                "{message}"
            );
            assert_eq!(
                *captured
                    .resolution_requests
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner()),
                vec!["feature/missing".to_string()],
                "scope resolution must never retry main"
            );
            client.close().await;
            server.await.expect("server");
        }
    }
}
