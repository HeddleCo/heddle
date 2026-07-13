// SPDX-License-Identifier: Apache-2.0

use objects::{
    object::{StateAttachment, StateAttachmentBody, StateAttachmentId, StateId},
    store::ObjectStore,
};

use crate::{HeddleError, Repository, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateAttachmentKind {
    Context,
    RiskSignals,
    ReviewSignatures,
    Discussions,
    StructuredConflicts,
    Signature,
}

impl StateAttachmentKind {
    fn of(body: &StateAttachmentBody) -> Self {
        match body {
            StateAttachmentBody::Context(_) => Self::Context,
            StateAttachmentBody::RiskSignals(_) => Self::RiskSignals,
            StateAttachmentBody::ReviewSignatures(_) => Self::ReviewSignatures,
            StateAttachmentBody::Discussions(_) => Self::Discussions,
            StateAttachmentBody::StructuredConflicts(_) => Self::StructuredConflicts,
            StateAttachmentBody::Signature(_) => Self::Signature,
        }
    }
}

impl Repository {
    pub fn put_state_attachment(&self, attachment: &StateAttachment) -> Result<StateAttachmentId> {
        if !self.store.has_state(&attachment.state_id)? {
            return Err(HeddleError::StateNotFound(attachment.state_id));
        }

        if let Some(prior_id) = attachment.supersedes {
            let prior = self
                .get_state_attachment(&attachment.state_id, &prior_id)?
                .ok_or_else(|| HeddleError::NotFound(format!("state attachment {prior_id}")))?;
            if StateAttachmentKind::of(&prior.body) != StateAttachmentKind::of(&attachment.body) {
                return Err(HeddleError::InvalidObject(
                    "state attachment can only supersede the same attachment kind".to_string(),
                ));
            }
        }

        let id = attachment.id();
        if let Some(existing) = self.store.get_state_attachment(&attachment.state_id, &id)? {
            return match self.get_state_attachment(&attachment.state_id, &id)? {
                Some(_) if existing == *attachment => Ok(id),
                _ => Err(HeddleError::InvalidObject(
                    "state attachment address collision".to_string(),
                )),
            };
        }
        self.store.put_state_attachment(attachment)
    }

    pub fn get_state_attachment(
        &self,
        state_id: &StateId,
        attachment_id: &StateAttachmentId,
    ) -> Result<Option<StateAttachment>> {
        self.store.get_state_attachment(state_id, attachment_id)
    }

    pub fn list_state_attachments(&self, state_id: &StateId) -> Result<Vec<StateAttachment>> {
        let mut attachments = self.store.list_state_attachments(state_id)?;
        attachments.sort_by_key(|attachment| (attachment.created_at, attachment.id()));
        Ok(attachments)
    }

    pub fn latest_state_attachment(
        &self,
        state_id: &StateId,
        kind: StateAttachmentKind,
    ) -> Result<Option<StateAttachment>> {
        let attachments: Vec<_> = self
            .list_state_attachments(state_id)?
            .into_iter()
            .filter(|attachment| StateAttachmentKind::of(&attachment.body) == kind)
            .collect();
        let superseded: std::collections::HashSet<_> = attachments
            .iter()
            .filter_map(|attachment| attachment.supersedes)
            .collect();
        Ok(attachments
            .into_iter()
            .filter(|attachment| !superseded.contains(&attachment.id()))
            .max_by_key(StateAttachment::id))
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};
    use objects::object::{Attribution, ContentHash, Principal, StateAttachment};
    use tempfile::TempDir;

    use super::*;

    fn attachment(state_id: StateId, body: StateAttachmentBody) -> StateAttachment {
        StateAttachment {
            state_id,
            body,
            attribution: Attribution::human(Principal::new("Test", "test@example.com")),
            created_at: Utc::now(),
            supersedes: None,
        }
    }

    #[test]
    fn attachment_history_preserves_state_bytes() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let state_id = repo.head().unwrap().unwrap();
        let before = repo.store().get_state(&state_id).unwrap().unwrap();

        let first = attachment(
            state_id,
            StateAttachmentBody::Context(ContentHash::compute(b"first")),
        );
        let first_id = repo.put_state_attachment(&first).unwrap();
        let mut second = attachment(
            state_id,
            StateAttachmentBody::Context(ContentHash::compute(b"second")),
        );
        second.created_at = first.created_at + Duration::seconds(1);
        second.supersedes = Some(first_id);
        repo.put_state_attachment(&second).unwrap();

        let latest = repo
            .latest_state_attachment(&state_id, StateAttachmentKind::Context)
            .unwrap()
            .unwrap();
        assert_eq!(latest, second);
        assert_eq!(repo.list_state_attachments(&state_id).unwrap().len(), 2);
        assert_eq!(repo.store().get_state(&state_id).unwrap().unwrap(), before);
    }

    #[test]
    fn latest_follows_supersession_heads_not_wall_clock() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let state_id = repo.head().unwrap().unwrap();
        let root = attachment(
            state_id,
            StateAttachmentBody::Context(ContentHash::compute(b"root")),
        );
        let root_id = repo.put_state_attachment(&root).unwrap();

        let mut older_clock = attachment(
            state_id,
            StateAttachmentBody::Context(ContentHash::compute(b"older-clock")),
        );
        older_clock.created_at = root.created_at - Duration::seconds(10);
        older_clock.supersedes = Some(root_id);
        repo.put_state_attachment(&older_clock).unwrap();

        let mut fork = attachment(
            state_id,
            StateAttachmentBody::Context(ContentHash::compute(b"fork")),
        );
        fork.created_at = root.created_at + Duration::seconds(10);
        fork.supersedes = Some(root_id);
        repo.put_state_attachment(&fork).unwrap();

        let expected = [older_clock.clone(), fork.clone()]
            .into_iter()
            .max_by_key(StateAttachment::id)
            .unwrap();
        assert_eq!(
            repo.latest_state_attachment(&state_id, StateAttachmentKind::Context)
                .unwrap()
                .unwrap(),
            expected
        );
    }
}
