// SPDX-License-Identifier: Apache-2.0
//! Session tracking for multi-provider agent workflows.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::Principal;

pub fn generate_session_id() -> String {
    let random_bytes: [u8; 10] = rand::random();
    format!(
        "sess-{}",
        base32::encode(base32::Alphabet::Rfc4648 { padding: false }, &random_bytes).to_lowercase()
    )
}

pub fn generate_segment_id(session_id: &str, segment_number: u32) -> String {
    format!("{}-seg-{}", session_id, segment_number)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub principal: Principal,
    pub created_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub segments: Vec<SessionSegment>,
    pub current_segment_id: Option<String>,
}

impl Session {
    pub fn new(
        id: String,
        principal: Principal,
        provider: String,
        model: String,
        policy_id: Option<String>,
    ) -> Self {
        let segment_id = generate_segment_id(&id, 1);
        let segment = SessionSegment {
            id: segment_id.clone(),
            provider,
            model,
            started_at: Utc::now(),
            policy_id,
        };
        Self {
            id,
            principal,
            created_at: Utc::now(),
            ended_at: None,
            segments: vec![segment],
            current_segment_id: Some(segment_id),
        }
    }

    pub fn is_active(&self) -> bool {
        self.ended_at.is_none()
    }

    pub fn current_segment(&self) -> Option<&SessionSegment> {
        self.current_segment_id
            .as_ref()
            .and_then(|id| self.segments.iter().find(|s| &s.id == id))
    }

    pub fn add_segment(
        &mut self,
        provider: String,
        model: String,
        policy_id: Option<String>,
    ) -> &SessionSegment {
        let segment_number = self.segments.len() as u32 + 1;
        let segment_id = generate_segment_id(&self.id, segment_number);
        let segment = SessionSegment {
            id: segment_id.clone(),
            provider,
            model,
            started_at: Utc::now(),
            policy_id,
        };
        self.segments.push(segment);
        self.current_segment_id = Some(segment_id);
        self.segments.last().expect("segment was just pushed")
    }

    pub fn end(&mut self) {
        self.ended_at = Some(Utc::now());
        self.current_segment_id = None;
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSegment {
    pub id: String,
    pub provider: String,
    pub model: String,
    pub started_at: DateTime<Utc>,
    pub policy_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_creation() {
        let principal = Principal::new("Test User", "test@example.com");
        let session = Session::new(
            "sess-test123".to_string(),
            principal.clone(),
            "anthropic".to_string(),
            "claude-opus-4".to_string(),
            None,
        );

        assert_eq!(session.id, "sess-test123");
        assert_eq!(session.principal, principal);
        assert!(session.is_active());
        assert!(session.ended_at.is_none());
        assert_eq!(session.segments.len(), 1);
        assert!(session.current_segment_id.is_some());
    }

    #[test]
    fn test_segment_id_format() {
        let segment_id = generate_segment_id("sess-test123", 1);
        assert_eq!(segment_id, "sess-test123-seg-1");

        let segment_id = generate_segment_id("sess-test123", 2);
        assert_eq!(segment_id, "sess-test123-seg-2");
    }

    #[test]
    fn test_current_segment() {
        let principal = Principal::new("Test User", "test@example.com");
        let session = Session::new(
            "sess-test123".to_string(),
            principal,
            "anthropic".to_string(),
            "claude-opus-4".to_string(),
            None,
        );

        let segment = session.current_segment().unwrap();
        assert_eq!(segment.provider, "anthropic");
        assert_eq!(segment.model, "claude-opus-4");
    }

    #[test]
    fn test_add_segment() {
        let principal = Principal::new("Test User", "test@example.com");
        let mut session = Session::new(
            "sess-test123".to_string(),
            principal,
            "anthropic".to_string(),
            "claude-opus-4".to_string(),
            None,
        );

        let segment = session.add_segment(
            "openai".to_string(),
            "gpt-4".to_string(),
            Some("policy-123".to_string()),
        );

        let segment_id = segment.id.clone();
        let segment_provider = segment.provider.clone();
        let segment_model = segment.model.clone();
        let segment_policy_id = segment.policy_id.clone();

        assert_eq!(session.segments.len(), 2);
        assert_eq!(segment_id, "sess-test123-seg-2");
        assert_eq!(segment_provider, "openai");
        assert_eq!(segment_model, "gpt-4");
        assert_eq!(segment_policy_id, Some("policy-123".to_string()));

        let current = session.current_segment().unwrap();
        assert_eq!(current.id, "sess-test123-seg-2");
    }

    #[test]
    fn test_end_session() {
        let principal = Principal::new("Test User", "test@example.com");
        let mut session = Session::new(
            "sess-test123".to_string(),
            principal,
            "anthropic".to_string(),
            "claude-opus-4".to_string(),
            None,
        );

        assert!(session.is_active());

        session.end();

        assert!(!session.is_active());
        assert!(session.ended_at.is_some());
        assert!(session.current_segment_id.is_none());
    }

    #[test]
    fn test_session_serialization() {
        let principal = Principal::new("Test User", "test@example.com");
        let session = Session::new(
            "sess-test123".to_string(),
            principal,
            "anthropic".to_string(),
            "claude-opus-4".to_string(),
            Some("policy-abc".to_string()),
        );

        let json = serde_json::to_string(&session).unwrap();
        let deserialized: Session = serde_json::from_str(&json).unwrap();

        assert_eq!(session, deserialized);
    }
}
