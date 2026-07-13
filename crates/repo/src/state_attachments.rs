// SPDX-License-Identifier: Apache-2.0

use std::{fs, path::PathBuf};

use objects::{
    fs_atomic::{create_dir_all_durable, write_file_atomic},
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
            if attachment.created_at < prior.created_at {
                return Err(HeddleError::InvalidObject(
                    "state attachment cannot predate the record it supersedes".to_string(),
                ));
            }
        }

        let id = attachment.id();
        let path = self.state_attachment_path(&attachment.state_id, &id);
        if path.exists() {
            return match self.get_state_attachment(&attachment.state_id, &id)? {
                Some(existing) if existing == *attachment => Ok(id),
                _ => Err(HeddleError::InvalidObject(
                    "state attachment address collision".to_string(),
                )),
            };
        }
        create_dir_all_durable(&self.state_attachment_dir(&attachment.state_id))?;
        let bytes = rmp_serde::to_vec_named(attachment)?;
        write_file_atomic(&path, &bytes)?;
        Ok(id)
    }

    pub fn get_state_attachment(
        &self,
        state_id: &StateId,
        attachment_id: &StateAttachmentId,
    ) -> Result<Option<StateAttachment>> {
        let path = self.state_attachment_path(state_id, attachment_id);
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let attachment: StateAttachment = rmp_serde::from_slice(&bytes)?;
        if attachment.state_id != *state_id || attachment.id() != *attachment_id {
            return Err(HeddleError::InvalidObject(
                "state attachment address does not match its content".to_string(),
            ));
        }
        Ok(Some(attachment))
    }

    pub fn list_state_attachments(&self, state_id: &StateId) -> Result<Vec<StateAttachment>> {
        let dir = self.state_attachment_dir(state_id);
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut attachments = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|value| value.to_str()) != Some("msgpack") {
                continue;
            }
            let bytes = fs::read(&path)?;
            let attachment: StateAttachment = rmp_serde::from_slice(&bytes)?;
            let expected_id = attachment.id();
            if attachment.state_id != *state_id
                || self
                    .state_attachment_path(state_id, &expected_id)
                    .file_name()
                    != path.file_name()
            {
                return Err(HeddleError::InvalidObject(
                    "state attachment stored under the wrong state".to_string(),
                ));
            }
            attachments.push(attachment);
        }
        attachments.sort_by_key(|attachment| (attachment.created_at, attachment.id()));
        Ok(attachments)
    }

    pub fn latest_state_attachment(
        &self,
        state_id: &StateId,
        kind: StateAttachmentKind,
    ) -> Result<Option<StateAttachment>> {
        Ok(self
            .list_state_attachments(state_id)?
            .into_iter()
            .filter(|attachment| StateAttachmentKind::of(&attachment.body) == kind)
            .max_by_key(|attachment| (attachment.created_at, attachment.id())))
    }

    fn state_attachment_dir(&self, state_id: &StateId) -> PathBuf {
        self.heddle_dir()
            .join("state-attachments")
            .join(state_id.to_string_full())
    }

    fn state_attachment_path(
        &self,
        state_id: &StateId,
        attachment_id: &StateAttachmentId,
    ) -> PathBuf {
        self.state_attachment_dir(state_id)
            .join(format!("{}.msgpack", attachment_id.as_hash().to_hex()))
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
}
