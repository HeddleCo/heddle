// SPDX-License-Identifier: Apache-2.0
//! Persist the live collaboration op-log as a per-state `Discussions` attachment.
//!
//! Local discussions live in `.heddle/collaboration`. Hosted pull bootstrap
//! (`discussions_from_pack`) reads `StateAttachmentKind::Discussions` on the
//! pulled tip. This snapshot is how they travel over `heddle push` / `heddle
//! pull` — they are not Git-projected.

use chrono::Utc;
use objects::{
    object::{
        Blob, CollaborationAnchor, CollaborationAnchorStatus, CollaborationResolution,
        ContentHash, Discussion, DiscussionResolution, DiscussionTurn, DiscussionsBlob,
        MaterializedDiscussion, StateAttachment, StateAttachmentBody, StateId, SymbolAnchor,
    },
    store::ObjectStore,
};
use oplog::OpLogBackend;
use refs::RefBackend;

use crate::{CollaborationStore, HeddleError, Repository, Result, StateAttachmentKind};

impl<R, O, S> Repository<R, O, S>
where
    R: RefBackend,
    O: OpLogBackend,
    S: ObjectStore,
{
    /// Encode current symbol-anchored discussions as a blob and attach it to
    /// `state_id`. Returns the blob hash when a snapshot was written, or `None`
    /// when the op-log has nothing to send.
    pub fn persist_discussions_snapshot(&self, state_id: StateId) -> Result<Option<ContentHash>> {
        let Some(hash) = self.encode_collaboration_discussions_blob()? else {
            return Ok(None);
        };
        let prior = self.latest_state_attachment(&state_id, StateAttachmentKind::Discussions)?;
        if let Some(prior_attachment) = &prior
            && let StateAttachmentBody::Discussions(existing) = &prior_attachment.body
            && *existing == hash
        {
            return Ok(Some(hash));
        }
        if !self.store().has_state(&state_id)? {
            return Err(HeddleError::StateNotFound(state_id));
        }
        let state = self
            .store()
            .get_state(&state_id)?
            .ok_or(HeddleError::StateNotFound(state_id))?;
        let created_at = prior
            .as_ref()
            .map(|attachment| attachment.created_at + chrono::Duration::nanoseconds(1))
            .map_or_else(Utc::now, |minimum| minimum.max(Utc::now()));
        self.put_state_attachment(&StateAttachment {
            state_id,
            body: StateAttachmentBody::Discussions(hash),
            attribution: state.attribution.clone(),
            created_at,
            supersedes: prior.map(|attachment| attachment.id()),
        })?;
        Ok(Some(hash))
    }

    /// Encode the live op-log as a `DiscussionsBlob` without attaching it.
    /// Snapshot travel uses the hash; push attaches it to the tip.
    pub(crate) fn encode_collaboration_discussions_blob(
        &self,
    ) -> Result<Option<ContentHash>> {
        if !self.heddle_dir().join("collaboration").exists() {
            return Ok(None);
        }
        let store = CollaborationStore::open(self.heddle_dir())?;
        let discussions = collaboration_discussions_blob(&store)?;
        if discussions.is_empty() {
            return Ok(None);
        }
        let bytes = DiscussionsBlob::new(discussions).encode().map_err(|err| {
            HeddleError::Serialization(format!("encode discussions blob: {err}"))
        })?;
        Ok(Some(self.store().put_blob(&Blob::new(bytes))?))
    }
}

fn collaboration_discussions_blob(store: &CollaborationStore) -> Result<Vec<Discussion>> {
    let materialized = store.materialize()?;
    let mut discussions = Vec::new();
    for discussion in materialized.discussions.values() {
        if let Some(converted) = discussion_from_materialized(store, discussion)? {
            discussions.push(converted);
        }
    }
    discussions.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(discussions)
}

fn discussion_from_materialized(
    store: &CollaborationStore,
    discussion: &MaterializedDiscussion,
) -> Result<Option<Discussion>> {
    let CollaborationAnchor::Symbol {
        state_id,
        path,
        symbol,
    } = &discussion.anchor
    else {
        return Ok(None);
    };
    let mut turns = Vec::with_capacity(discussion.turns.len());
    let mut opened_at = 0i64;
    for (index, (operation_id, turn)) in discussion.turns.iter().enumerate() {
        let decoded = store.read_operation(operation_id)?.ok_or_else(|| {
            HeddleError::InvalidObject(format!(
                "discussion {} references missing operation {operation_id}",
                discussion.discussion_id
            ))
        })?;
        let posted_at = millis_to_secs(decoded.operation.occurred_at_ms);
        if index == 0 {
            opened_at = posted_at;
        }
        turns.push(DiscussionTurn {
            author: decoded.operation.author.principal,
            body: turn.body.clone(),
            posted_at,
            references: Vec::new(),
        });
    }
    if turns.is_empty() {
        return Ok(None);
    }
    let (resolution, resolved_annotation_id) = blob_resolution(&discussion.resolution);
    Ok(Some(Discussion {
        id: discussion.discussion_id.to_string(),
        anchor: SymbolAnchor::new(path, symbol),
        opened_against_state: *state_id,
        opened_at,
        thread_ref: discussion.thread_ref.clone(),
        turns,
        resolution,
        body_changed_since_open: discussion.body_changed_since_open,
        anchor_ambiguous: matches!(discussion.anchor_status, CollaborationAnchorStatus::Ambiguous),
        orphaned: matches!(discussion.anchor_status, CollaborationAnchorStatus::Orphaned),
        visibility: discussion.visibility.clone(),
        resolved_annotation_id,
    }))
}

fn blob_resolution(
    resolution: &Option<CollaborationResolution>,
) -> (DiscussionResolution, Option<String>) {
    match resolution {
        None => (DiscussionResolution::Open, None),
        Some(CollaborationResolution::AddressedByState { state_id }) => {
            (DiscussionResolution::ResolvedByEdit { state_id: *state_id }, None)
        }
        Some(CollaborationResolution::Dismissed { reason }) => (
            DiscussionResolution::Dismissed {
                reason: reason.clone(),
            },
            None,
        ),
        Some(CollaborationResolution::Annotation { annotation_id }) => (
            DiscussionResolution::ResolvedIntoAnnotation {
                annotation_id: annotation_id.clone(),
            },
            Some(annotation_id.clone()),
        ),
        // Hosted blob form has no ChangeId pin and cannot name an annotation
        // that has not been minted yet. Keep the discussion visible as open.
        Some(CollaborationResolution::AddressedByChange { .. })
        | Some(CollaborationResolution::IntoAnnotation { .. }) => (DiscussionResolution::Open, None),
    }
}

fn millis_to_secs(ms: i64) -> i64 {
    if ms > 0 {
        ms / 1000
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use objects::object::{
        Attribution, CollaborationAnchor, CollaborationIdempotencyKey,
        CollaborationOperationBodyV1, CollaborationOperationEnvelope, DiscussionRecordId,
        DiscussionTurnV1, Principal, VisibilityTier,
    };
    use tempfile::TempDir;

    use super::*;

    fn write_open(
        repo: &Repository,
        store: &CollaborationStore,
        path: &str,
        symbol: &str,
        body: &str,
        thread_ref: Option<&str>,
    ) -> (DiscussionRecordId, StateId) {
        let state_id = repo.head().unwrap().unwrap();
        let discussion_id = DiscussionRecordId::generate();
        let author = Attribution::human(Principal::new("Alice", "alice@example.com"));
        let operation = CollaborationOperationEnvelope::new(
            discussion_id,
            Vec::new(),
            CollaborationIdempotencyKey::new("open-1").unwrap(),
            author,
            1_700_000_000_000,
            CollaborationOperationBodyV1::Open {
                title: symbol.to_string(),
                anchor: CollaborationAnchor::Symbol {
                    state_id,
                    path: path.to_string(),
                    symbol: symbol.to_string(),
                },
                visibility: VisibilityTier::Internal,
                turn: DiscussionTurnV1::new(body).unwrap(),
                thread_ref: thread_ref.map(str::to_string),
            },
        )
        .unwrap();
        store.write_operation(&operation).unwrap();
        (discussion_id, state_id)
    }

    #[test]
    fn persist_writes_discussions_attachment_and_blob() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        std::fs::write(temp.path().join("greet.py"), "def greet():\n    return 'hi'\n").unwrap();
        let state = repo
            .snapshot_with_attribution(
                Some("seed".to_string()),
                None,
                Attribution::human(Principal::new("Alice", "alice@example.com")),
            )
            .unwrap();
        let store = CollaborationStore::open(repo.heddle_dir()).unwrap();
        let (discussion_id, _) = write_open(&repo, &store, "greet.py", "greet", "please check", Some("alice"));

        let hash = repo
            .persist_discussions_snapshot(state.id())
            .unwrap()
            .expect("snapshot hash");
        let attachment = repo
            .latest_state_attachment(&state.id(), StateAttachmentKind::Discussions)
            .unwrap()
            .expect("discussions attachment");
        let StateAttachmentBody::Discussions(attached) = attachment.body else {
            panic!("wrong attachment kind");
        };
        assert_eq!(attached, hash);
        let blob = repo.store().get_blob(&hash).unwrap().expect("blob");
        let decoded = DiscussionsBlob::decode(blob.content()).unwrap();
        assert_eq!(decoded.discussions.len(), 1);
        assert_eq!(decoded.discussions[0].id, discussion_id.to_string());
        assert_eq!(decoded.discussions[0].anchor.file, "greet.py");
        assert_eq!(decoded.discussions[0].anchor.symbol, "greet");
        assert_eq!(decoded.discussions[0].thread_ref.as_deref(), Some("alice"));
        assert_eq!(decoded.discussions[0].turns[0].body, "please check");

        assert_eq!(
            repo.persist_discussions_snapshot(state.id()).unwrap(),
            Some(hash),
            "identical snapshot must not mint a new blob"
        );
        assert_eq!(
            repo.list_state_attachments(&state.id())
                .unwrap()
                .into_iter()
                .filter(|attachment| attachment.body.kind() == StateAttachmentKind::Discussions)
                .count(),
            1
        );
    }

    #[test]
    fn persist_skips_when_op_log_has_no_symbol_discussions() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let state_id = repo.head().unwrap().unwrap();
        assert_eq!(repo.persist_discussions_snapshot(state_id).unwrap(), None);
        assert!(
            repo.latest_state_attachment(&state_id, StateAttachmentKind::Discussions)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn persist_ignores_repository_anchored_discussions() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let state_id = repo.head().unwrap().unwrap();
        let store = CollaborationStore::open(repo.heddle_dir()).unwrap();
        let discussion_id = DiscussionRecordId::generate();
        let operation = CollaborationOperationEnvelope::new(
            discussion_id,
            Vec::new(),
            CollaborationIdempotencyKey::new("repo-open").unwrap(),
            Attribution::human(Principal::new("Alice", "alice@example.com")),
            1,
            CollaborationOperationBodyV1::Open {
                title: "repo".to_string(),
                anchor: CollaborationAnchor::Repository,
                visibility: VisibilityTier::Internal,
                turn: DiscussionTurnV1::new("whole repo").unwrap(),
                thread_ref: None,
            },
        )
        .unwrap();
        store.write_operation(&operation).unwrap();
        assert_eq!(repo.persist_discussions_snapshot(state_id).unwrap(), None);
    }
}
