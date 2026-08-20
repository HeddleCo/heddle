// SPDX-License-Identifier: Apache-2.0
//! Freeze the current identity cursor onto a capture, rotating the session
//! segment when a published provider / model / thought_level changes.

use std::collections::BTreeMap;

use anyhow::Result;
use heddle_core::{
    IdentityCursor, SegmentRotation, attach_published_segment_fields, cursor_patch_from_child_env,
    cursor_segment_rotation, published_field, read_identity_cursor, stamp_identity_cursor,
};
use objects::{lock::RepositoryLockExt, object::Session};
use repo::{Repository, SessionManager};

/// Cursor values frozen onto one capture (ACP names).
#[derive(Clone, Debug, Default)]
pub struct FrozenIdentity {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub thought_level: Option<String>,
    pub session: Option<String>,
    pub parent: Option<String>,
    pub segment_id: Option<String>,
}

/// Read the workspace cursor, attach child-env fallbacks, persist so later
/// captures are not env-shaped, and rotate the Heddle segment when needed.
pub fn freeze_identity_for_capture(repo: &Repository) -> Result<FrozenIdentity> {
    let _guard = repo.locker().write()?;
    freeze_identity_for_capture_locked(repo)
}

fn freeze_identity_for_capture_locked(repo: &Repository) -> Result<FrozenIdentity> {
    let mut cursor = read_identity_cursor(repo.root());
    let child = cursor_patch_from_child_env(&child_env_hints());
    if !child.is_empty() {
        cursor = stamp_identity_cursor(repo.root(), &child)?;
    }
    let mut manager = SessionManager::new(repo.root());
    let session = manager.get_current_session()?;
    let segment_id = apply_segment_policy(&mut manager, session.as_ref(), &cursor)?;
    Ok(FrozenIdentity {
        provider: cursor.provider,
        model: cursor.model,
        thought_level: cursor.thought_level,
        session: cursor.session,
        parent: cursor.parent,
        segment_id,
    })
}

fn child_env_hints() -> BTreeMap<String, String> {
    std::env::vars()
        .filter(|(key, value)| {
            !value.trim().is_empty()
                && matches!(
                    key.as_str(),
                    "CLAUDE_CODE_SESSION_ID"
                        | "CLAUDE_EFFORT"
                        | "PI_MODEL"
                        | "PI_REASONING_LEVEL"
                        | "PI_SESSION_ID"
                        | "PI_PROVIDER"
                        | "PI_PARENT_ID"
                )
        })
        .collect()
}

fn apply_segment_policy(
    manager: &mut SessionManager,
    session: Option<&Session>,
    cursor: &IdentityCursor,
) -> Result<Option<String>> {
    let Some(session) = session else {
        return Ok(None);
    };
    let current = session.current_segment();
    let rotation = cursor_segment_rotation(
        current.map(|s| s.provider.as_str()),
        current.map(|s| s.model.as_str()),
        current.and_then(|s| s.thought_level.as_deref()),
        cursor.provider.as_deref(),
        cursor.model.as_deref(),
        cursor.thought_level.as_deref(),
    );
    if rotation == SegmentRotation::Rotate {
        let provider = cursor
            .provider
            .clone()
            .or_else(|| current.map(|s| s.provider.clone()))
            .unwrap_or_default();
        let model = cursor
            .model
            .clone()
            .or_else(|| current.map(|s| s.model.clone()))
            .unwrap_or_default();
        if published_field(Some(&provider)).is_some() && published_field(Some(&model)).is_some() {
            let segment = manager.add_segment(&session.id, provider, model, None)?;
            if let Some(thought_level) = cursor.thought_level.clone()
                && let Some(mut updated) = manager.get_session(&session.id)?
            {
                if let Some(current) = updated.current_segment_mut() {
                    current.thought_level = Some(thought_level);
                }
                manager.save_session(&updated)?;
            }
            return Ok(Some(segment.id));
        }
    } else if rotation == SegmentRotation::Attach
        && let Some(mut updated) = manager.get_session(&session.id)?
    {
        if let Some(segment) = updated.current_segment_mut() {
            attach_published_segment_fields(
                segment,
                cursor.provider.as_deref(),
                cursor.model.as_deref(),
                cursor.thought_level.as_deref(),
            );
        }
        manager.save_session(&updated)?;
    }
    Ok(session.current_segment_id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use heddle_core::write_identity_cursor;
    use objects::object::Principal;
    use repo::Repository;

    fn principal() -> Principal {
        Principal::new("Ada", "ada@example.com")
    }

    #[test]
    fn freeze_rotates_on_model_change_and_attaches_thought_level() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let mut manager = SessionManager::new(repo.root());
        manager
            .start_session(principal(), "anthropic".into(), "opus".into(), None)
            .unwrap();
        write_identity_cursor(
            repo.root(),
            &IdentityCursor {
                provider: Some("anthropic".into()),
                model: Some("opus".into()),
                thought_level: Some("high".into()),
                session: Some("claude-sess".into()),
                parent: None,
            },
        )
        .unwrap();
        let first = freeze_identity_for_capture(&repo).unwrap();
        assert_eq!(first.model.as_deref(), Some("opus"));
        assert_eq!(first.thought_level.as_deref(), Some("high"));
        assert_eq!(first.session.as_deref(), Some("claude-sess"));
        let first_seg = first.segment_id.clone();

        write_identity_cursor(
            repo.root(),
            &IdentityCursor {
                provider: Some("anthropic".into()),
                model: Some("sonnet".into()),
                thought_level: Some("high".into()),
                session: Some("claude-sess".into()),
                parent: None,
            },
        )
        .unwrap();
        let second = freeze_identity_for_capture(&repo).unwrap();
        assert_eq!(second.model.as_deref(), Some("sonnet"));
        assert_ne!(second.segment_id, first_seg);
    }

    #[test]
    fn empty_thought_level_to_set_does_not_rotate() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let mut manager = SessionManager::new(repo.root());
        manager
            .start_session(principal(), "anthropic".into(), "opus".into(), None)
            .unwrap();
        write_identity_cursor(
            repo.root(),
            &IdentityCursor {
                provider: Some("anthropic".into()),
                model: Some("opus".into()),
                thought_level: Some("max".into()),
                ..IdentityCursor::default()
            },
        )
        .unwrap();
        let frozen = freeze_identity_for_capture(&repo).unwrap();
        let session = SessionManager::new(repo.root())
            .get_current_session()
            .unwrap()
            .unwrap();
        assert_eq!(session.segments.len(), 1);
        assert_eq!(
            session.current_segment().unwrap().thought_level.as_deref(),
            Some("max")
        );
        assert_eq!(frozen.thought_level.as_deref(), Some("max"));
    }

    #[test]
    fn empty_provider_model_to_set_attaches_without_rotate() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let mut manager = SessionManager::new(repo.root());
        manager
            .start_session(principal(), "unknown".into(), "unknown".into(), None)
            .unwrap();
        write_identity_cursor(
            repo.root(),
            &IdentityCursor {
                provider: Some("anthropic".into()),
                model: Some("opus".into()),
                thought_level: Some("high".into()),
                ..IdentityCursor::default()
            },
        )
        .unwrap();
        let frozen = freeze_identity_for_capture(&repo).unwrap();
        let session = SessionManager::new(repo.root())
            .get_current_session()
            .unwrap()
            .unwrap();
        assert_eq!(session.segments.len(), 1);
        assert_eq!(session.current_segment().unwrap().provider, "anthropic");
        assert_eq!(session.current_segment().unwrap().model, "opus");
        assert_eq!(
            session.current_segment().unwrap().thought_level.as_deref(),
            Some("high")
        );
        assert!(
            frozen.session.is_none(),
            "harness session stays unset; do not pair Heddle Session.id"
        );
    }

    #[test]
    fn freeze_waits_for_repo_write_lock() {
        use std::{sync::mpsc, time::Duration};

        use objects::lock::RepositoryLockExt;

        let temp = tempfile::TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        write_identity_cursor(
            repo.root(),
            &IdentityCursor {
                provider: Some("anthropic".into()),
                model: Some("opus".into()),
                ..IdentityCursor::default()
            },
        )
        .unwrap();
        let hold = repo.locker().write().unwrap();
        let root = repo.root().to_path_buf();
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let repo = Repository::open(root).unwrap();
            let frozen = freeze_identity_for_capture(&repo);
            let _ = tx.send(frozen.is_ok());
        });
        assert!(
            rx.recv_timeout(Duration::from_millis(150)).is_err(),
            "freeze must not mutate sessions until it holds the repo write lock"
        );
        drop(hold);
        let ok = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(ok, "freeze must succeed after the write lock is released");
        worker.join().unwrap();
    }
}
