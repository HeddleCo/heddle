// SPDX-License-Identifier: Apache-2.0
//! Legacy state-attached discussion persistence.
//!
//! The operation semantics live in `objects::object::DiscussionOperation`.
//! This module only adapts those semantics to the current local
//! `State::with_discussions` storage shape.

use objects::{
    error::{HeddleError, Result},
    object::{Blob, ChangeId, Discussion, DiscussionOperation, DiscussionsBlob, State},
    store::ObjectStore,
};

use crate::Repository;

impl Repository {
    pub fn read_discussions_for_state(
        &self,
        state_id: &ChangeId,
    ) -> Result<(State, DiscussionsBlob)> {
        let state = self
            .store()
            .get_state(state_id)?
            .ok_or(HeddleError::StateNotFound(*state_id))?;
        let discussions = self.decode_discussions_for_state(&state)?;
        Ok((state, discussions))
    }

    pub fn read_current_discussions(&self) -> Result<(State, DiscussionsBlob)> {
        let head_id = self
            .head()?
            .ok_or_else(|| HeddleError::Conflict("repository has no HEAD".into()))?;
        self.read_discussions_for_state(&head_id)
    }

    pub fn apply_legacy_discussion_operation(
        &self,
        operation: DiscussionOperation,
    ) -> Result<Discussion> {
        let (state, mut discussions) = match &operation {
            DiscussionOperation::Open {
                opened_against_state,
                ..
            } => self.read_discussions_for_state(opened_against_state)?,
            DiscussionOperation::AppendTurn { .. } | DiscussionOperation::Resolve { .. } => {
                self.read_current_discussions()?
            }
        };
        let updated = discussions
            .apply_operation(operation)
            .map_err(|err| match err {
                objects::object::DiscussionError::DiscussionNotFound(id) => {
                    HeddleError::NotFound(format!("discussion {id} not found"))
                }
                other => HeddleError::InvalidObject(other.to_string()),
            })?;
        self.persist_discussions_for_state(&state, &discussions)?;
        Ok(updated)
    }

    fn decode_discussions_for_state(&self, state: &State) -> Result<DiscussionsBlob> {
        let Some(hash) = state.discussions else {
            return Ok(DiscussionsBlob::new(Vec::new()));
        };
        let blob = self
            .store()
            .get_blob(&hash)?
            .ok_or_else(|| HeddleError::MissingObject {
                object_type: "blob".into(),
                id: hash.to_hex(),
            })?;
        DiscussionsBlob::decode(blob.content())
            .map_err(|err| HeddleError::Serialization(format!("decode discussions blob: {err}")))
    }

    fn persist_discussions_for_state(
        &self,
        state: &State,
        discussions: &DiscussionsBlob,
    ) -> Result<State> {
        let bytes = discussions
            .encode()
            .map_err(|err| HeddleError::Serialization(format!("encode discussions blob: {err}")))?;
        let hash = self.store().put_blob(&Blob::new(bytes))?;
        let new_state = state.clone().with_discussions(hash);
        self.store().put_state(&new_state)?;
        Ok(new_state)
    }
}

#[cfg(test)]
mod tests {
    use objects::object::{
        DiscussionOperation, DiscussionResolution, Principal, SymbolAnchor, VisibilityTier,
    };
    use tempfile::TempDir;

    use crate::Repository;

    fn author() -> Principal {
        Principal::new("Dana", "dana@example.com")
    }

    #[test]
    fn discussion_legacy_operations_persist_on_state_blob() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let head = repo.head().unwrap().expect("seeded head");

        let opened = repo
            .apply_legacy_discussion_operation(DiscussionOperation::Open {
                id: "disc-legacy".into(),
                anchor: SymbolAnchor::new("src/lib.rs", "parse"),
                opened_against_state: head,
                opened_at: 10,
                thread_ref: Some("main".into()),
                author: author(),
                body: "Can we simplify this?".into(),
                visibility: VisibilityTier::default(),
            })
            .unwrap();
        assert_eq!(opened.id, "disc-legacy");

        let (_, discussions) = repo.read_discussions_for_state(&head).unwrap();
        assert_eq!(discussions.discussions.len(), 1);

        let appended = repo
            .apply_legacy_discussion_operation(DiscussionOperation::AppendTurn {
                discussion_id: "disc-legacy".into(),
                author: author(),
                body: "Yes, after the parser split.".into(),
                posted_at: 11,
            })
            .unwrap();
        assert_eq!(appended.turns.len(), 2);

        let resolved = repo
            .apply_legacy_discussion_operation(DiscussionOperation::Resolve {
                discussion_id: "disc-legacy".into(),
                resolution: DiscussionResolution::Dismissed {
                    reason: "tracked in follow-up".into(),
                },
            })
            .unwrap();
        assert!(matches!(
            resolved.resolution,
            DiscussionResolution::Dismissed { .. }
        ));

        let (_, discussions) = repo.read_current_discussions().unwrap();
        assert_eq!(discussions.discussions[0].turns.len(), 2);
        assert!(matches!(
            discussions.discussions[0].resolution,
            DiscussionResolution::Dismissed { .. }
        ));
    }
}
