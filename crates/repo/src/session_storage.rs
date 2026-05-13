// SPDX-License-Identifier: Apache-2.0
//! Session storage and management.

use std::{fs, path::PathBuf};

use anyhow::Result;
use objects::{
    fs_atomic::write_file_atomic,
    object::{Principal, Session, SessionSegment},
};
use tracing::{debug, info};

use crate::WorktreeState;

pub struct SessionManager {
    sessions_dir: PathBuf,
    state_path: PathBuf,
}

impl SessionManager {
    pub fn new(repo_root: &std::path::Path) -> Self {
        let runtime_root = repo_root.join(".heddle/state");
        Self {
            sessions_dir: runtime_root.join("sessions"),
            state_path: runtime_root.join("worktree.toml"),
        }
    }

    pub fn sessions_dir(&self) -> &std::path::Path {
        &self.sessions_dir
    }

    pub fn session_path(&self, session_id: &str) -> PathBuf {
        self.sessions_dir.join(format!("{}.json", session_id))
    }

    fn load_state(&self) -> Result<WorktreeState> {
        Ok(WorktreeState::load(&self.state_path)?)
    }

    fn save_state(&self, state: &WorktreeState) -> Result<()> {
        state.save(&self.state_path)?;
        Ok(())
    }

    pub fn start_session(
        &mut self,
        principal: Principal,
        provider: String,
        model: String,
        policy_id: Option<String>,
    ) -> Result<Session> {
        fs::create_dir_all(&self.sessions_dir)?;

        let session_id = objects::object::generate_session_id();
        let session = Session::new(session_id, principal, provider, model, policy_id);

        self.save_session(&session)?;

        let segment_id = session.current_segment_id.clone().unwrap_or_default();
        self.set_current_session(&session.id, &segment_id)?;

        info!(session_id = %session.id, "Started new session");
        Ok(session)
    }

    pub fn add_segment(
        &mut self,
        session_id: &str,
        provider: String,
        model: String,
        policy_id: Option<String>,
    ) -> Result<SessionSegment> {
        let mut session = self
            .get_session(session_id)?
            .ok_or_else(|| anyhow::anyhow!("Session not found: {}", session_id))?;

        if !session.is_active() {
            return Err(anyhow::anyhow!("Cannot add segment to ended session"));
        }

        let segment = session.add_segment(provider, model, policy_id).clone();
        self.save_session(&session)?;

        self.set_current_session(&session.id, &segment.id)?;

        debug!(session_id = %session.id, segment_id = %segment.id, "Added segment");
        Ok(segment)
    }

    pub fn end_session(&mut self, session_id: Option<&str>) -> Result<Session> {
        let id = match session_id {
            Some(id) => id.to_string(),
            None => self
                .get_current_session_id()?
                .ok_or_else(|| anyhow::anyhow!("No active session"))?,
        };

        let mut session = self
            .get_session(&id)?
            .ok_or_else(|| anyhow::anyhow!("Session not found: {}", id))?;

        if !session.is_active() {
            return Err(anyhow::anyhow!("Session already ended"));
        }

        session.end();
        self.save_session(&session)?;

        if self.get_current_session_id()?.as_deref() == Some(id.as_str()) {
            self.clear_current_session()?;
        }

        info!(session_id = %session.id, "Ended session");
        Ok(session)
    }

    pub fn get_session(&self, session_id: &str) -> Result<Option<Session>> {
        let path = self.session_path(session_id);
        if !path.exists() {
            return Ok(None);
        }

        let content = fs::read_to_string(path)?;
        let session: Session = serde_json::from_str(&content)?;
        Ok(Some(session))
    }

    pub fn save_session(&self, session: &Session) -> Result<()> {
        fs::create_dir_all(&self.sessions_dir)?;

        let path = self.session_path(&session.id);
        let content = serde_json::to_string_pretty(session)?;
        write_file_atomic(&path, content.as_bytes())?;

        Ok(())
    }

    pub fn list_sessions(&self, active_only: bool) -> Result<Vec<Session>> {
        if !self.sessions_dir.exists() {
            return Ok(Vec::new());
        }

        let mut sessions = Vec::new();
        for entry in fs::read_dir(&self.sessions_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().map(|e| e == "json").unwrap_or(false)
                && let Ok(content) = fs::read_to_string(&path)
                && let Ok(session) = serde_json::from_str::<Session>(&content)
                && (!active_only || session.is_active())
            {
                sessions.push(session);
            }
        }

        sessions.sort_by_key(|a| std::cmp::Reverse(a.created_at));
        Ok(sessions)
    }

    pub fn get_current_session_id(&self) -> Result<Option<String>> {
        let state = self.load_state()?;
        Ok(state.current_session_id)
    }

    pub fn get_current_segment_id(&self) -> Result<Option<String>> {
        let state = self.load_state()?;
        Ok(state.current_segment_id)
    }

    pub fn set_current_session(&mut self, session_id: &str, segment_id: &str) -> Result<()> {
        let mut state = self.load_state()?;
        state.current_session_id = Some(session_id.to_string());
        state.current_segment_id = Some(segment_id.to_string());
        self.save_state(&state)?;
        Ok(())
    }

    pub fn clear_current_session(&mut self) -> Result<()> {
        let mut state = self.load_state()?;
        state.current_session_id = None;
        state.current_segment_id = None;
        self.save_state(&state)?;
        Ok(())
    }

    pub fn get_current_session(&self) -> Result<Option<Session>> {
        let session_id = match self.get_current_session_id()? {
            Some(id) => id,
            None => return Ok(None),
        };

        self.get_session(&session_id)
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn create_test_manager() -> (TempDir, SessionManager) {
        let temp = TempDir::new().unwrap();
        let manager = SessionManager::new(temp.path());
        (temp, manager)
    }

    #[test]
    fn test_start_session() {
        let (temp, mut manager) = create_test_manager();
        let principal = Principal::new("Test User", "test@example.com");

        let session = manager
            .start_session(
                principal,
                "anthropic".to_string(),
                "claude-opus-4".to_string(),
                None,
            )
            .unwrap();

        assert!(session.id.starts_with("sess-"));
        assert!(session.is_active());
        assert_eq!(session.segments.len(), 1);
        assert_eq!(session.segments[0].provider, "anthropic");

        let current_id = manager.get_current_session_id().unwrap();
        assert_eq!(current_id, Some(session.id));
        assert!(temp.path().join(".heddle/state/worktree.toml").exists());
        assert!(!temp.path().join(".heddle/config.toml").exists());
    }

    #[test]
    fn test_add_segment() {
        let (_temp, mut manager) = create_test_manager();
        let principal = Principal::new("Test User", "test@example.com");

        let session = manager
            .start_session(
                principal,
                "anthropic".to_string(),
                "claude-opus-4".to_string(),
                None,
            )
            .unwrap();

        let segment = manager
            .add_segment(
                &session.id,
                "openai".to_string(),
                "gpt-4".to_string(),
                Some("policy-123".to_string()),
            )
            .unwrap();

        assert_eq!(segment.id, format!("{}-seg-2", session.id));
        assert_eq!(segment.provider, "openai");
        assert_eq!(segment.model, "gpt-4");

        let updated = manager.get_session(&session.id).unwrap().unwrap();
        assert_eq!(updated.segments.len(), 2);
    }

    #[test]
    fn test_end_session() {
        let (_temp, mut manager) = create_test_manager();
        let principal = Principal::new("Test User", "test@example.com");

        let session = manager
            .start_session(
                principal,
                "anthropic".to_string(),
                "claude-opus-4".to_string(),
                None,
            )
            .unwrap();

        assert!(session.is_active());

        let ended = manager.end_session(Some(&session.id)).unwrap();
        assert!(!ended.is_active());
        assert!(ended.ended_at.is_some());

        let current_id = manager.get_current_session_id().unwrap();
        assert!(current_id.is_none());
    }

    #[test]
    fn test_list_sessions() {
        let (_temp, mut manager) = create_test_manager();
        let principal = Principal::new("Test User", "test@example.com");

        manager
            .start_session(
                principal.clone(),
                "anthropic".to_string(),
                "claude-opus-4".to_string(),
                None,
            )
            .unwrap();
        manager
            .start_session(principal, "openai".to_string(), "gpt-4".to_string(), None)
            .unwrap();

        let sessions = manager.list_sessions(false).unwrap();
        assert_eq!(sessions.len(), 2);

        let active = manager.list_sessions(true).unwrap();
        assert_eq!(active.len(), 2);
    }

    #[test]
    fn test_session_persistence() {
        let (temp, mut manager) = create_test_manager();
        let principal = Principal::new("Test User", "test@example.com");

        let session = manager
            .start_session(
                principal,
                "anthropic".to_string(),
                "claude-opus-4".to_string(),
                None,
            )
            .unwrap();

        drop(manager);

        let manager2 = SessionManager::new(temp.path());
        let loaded = manager2.get_session(&session.id).unwrap().unwrap();

        assert_eq!(session.id, loaded.id);
        assert_eq!(session.segments.len(), loaded.segments.len());
    }

    #[test]
    fn test_per_worktree_sessions() {
        let temp1 = TempDir::new().unwrap();
        let temp2 = TempDir::new().unwrap();

        let mut manager1 = SessionManager::new(temp1.path());
        let mut manager2 = SessionManager::new(temp2.path());

        let principal = Principal::new("Test User", "test@example.com");

        let session1 = manager1
            .start_session(
                principal.clone(),
                "anthropic".to_string(),
                "claude-opus-4".to_string(),
                None,
            )
            .unwrap();

        let session2 = manager2
            .start_session(principal, "openai".to_string(), "gpt-4".to_string(), None)
            .unwrap();

        assert_ne!(session1.id, session2.id);

        assert_eq!(
            manager1.get_current_session_id().unwrap(),
            Some(session1.id)
        );
        assert_eq!(
            manager2.get_current_session_id().unwrap(),
            Some(session2.id)
        );
    }

    #[test]
    fn test_save_session_writes_atomic_json() {
        let (_temp, mut manager) = create_test_manager();
        let principal = Principal::new("Test User", "test@example.com");

        let session = manager
            .start_session(
                principal,
                "anthropic".to_string(),
                "claude-opus-4".to_string(),
                None,
            )
            .unwrap();

        manager
            .add_segment(
                &session.id,
                "openai".to_string(),
                "gpt-4.1".to_string(),
                None,
            )
            .unwrap();

        let session_path = manager.session_path(&session.id);
        let content = fs::read_to_string(&session_path).unwrap();
        let stored: Session = serde_json::from_str(&content).unwrap();
        assert_eq!(stored.segments.len(), 2);

        let temp_entries = fs::read_dir(manager.sessions_dir())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .count();
        assert_eq!(temp_entries, 0);
    }

    #[test]
    fn test_invalid_state_is_reported() {
        let (temp, manager) = create_test_manager();
        let runtime_dir = temp.path().join(".heddle/state");
        fs::create_dir_all(&runtime_dir).unwrap();
        fs::write(runtime_dir.join("worktree.toml"), "not = [valid").unwrap();

        assert!(manager.get_current_session_id().is_err());
    }
}