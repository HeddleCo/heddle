// SPDX-License-Identifier: Apache-2.0
//! Current harness identity cursor (`provider`, `model`, `thought_level`,
//! `session`, `parent`) written by install hooks and frozen onto each capture.
//!
//! This is live cursor state, not “the model of the thread”. Mid-thread
//! `/model` or `/effort` updates the cursor only; already-captured states
//! keep the pair they froze.

use std::{
    fs::{self, OpenOptions},
    io,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::harness_json::{first_value_string, value_string};

/// Sidecar file name under `.heddle/` (not `identity.toml`, the signing key).
pub const IDENTITY_CURSOR_FILE: &str = "identity";

/// ACP-named current identity. Omit unpublished fields.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityCursor {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought_level: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

impl IdentityCursor {
    /// Drop empty / `unknown` placeholders so unpublished fields stay omitted.
    pub fn omit_unpublished(mut self) -> Self {
        self.provider = published_owned(self.provider);
        self.model = published_owned(self.model);
        self.thought_level = published_owned(self.thought_level);
        self.session = published_owned(self.session);
        self.parent = published_owned(self.parent);
        self
    }

    /// Merge an event patch: missing incoming fields keep the last cursor value.
    ///
    /// `parent` is the exception:
    /// - incoming `parent` equal to incoming `session` is the main agent
    ///   (Claude omits `agent_id`) and clears a stale subagent parent
    /// - incoming `session` equal to the stored parent (Codex parent-session)
    ///   also clears it
    pub fn merge_event(&self, patch: &IdentityCursor) -> Self {
        Self {
            provider: published_owned(patch.provider.clone()).or_else(|| self.provider.clone()),
            model: published_owned(patch.model.clone()).or_else(|| self.model.clone()),
            thought_level: published_owned(patch.thought_level.clone())
                .or_else(|| self.thought_level.clone()),
            session: published_owned(patch.session.clone()).or_else(|| self.session.clone()),
            parent: merge_parent(self, patch),
        }
        .omit_unpublished()
    }

    pub fn is_empty(&self) -> bool {
        self.provider.is_none()
            && self.model.is_none()
            && self.thought_level.is_none()
            && self.session.is_none()
            && self.parent.is_none()
    }

    /// Compact JSON (~200 bytes) for the sidecar hot path.
    pub fn to_vec(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }
}

/// Treat empty / `unknown` as unpublished.
pub fn published_field(value: Option<&str>) -> Option<&str> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("unknown"))
}

fn published_owned(value: Option<String>) -> Option<String> {
    published_field(value.as_deref()).map(str::to_string)
}

fn merge_parent(current: &IdentityCursor, patch: &IdentityCursor) -> Option<String> {
    let incoming_parent = published_owned(patch.parent.clone());
    let incoming_session = published_owned(patch.session.clone());
    if incoming_parent
        .as_ref()
        .zip(incoming_session.as_ref())
        .is_some_and(|(parent, session)| parent == session)
    {
        return None;
    }
    if let Some(incoming) = incoming_parent {
        return Some(incoming);
    }
    if incoming_session
        .as_ref()
        .zip(current.parent.as_ref())
        .is_some_and(|(session, parent)| session == parent)
    {
        return None;
    }
    current.parent.clone()
}

/// Workspace sidecar path.
///
/// Directory checkout: `.heddle/identity`. Pointer checkout (`.heddle` is a
/// file): `.heddle.identity` beside it so the cursor stays workspace-local.
pub fn identity_cursor_path(repo_root: &Path) -> std::path::PathBuf {
    let marker = repo_root.join(".heddle");
    if marker.is_file() {
        repo_root.join(".heddle.identity")
    } else {
        marker.join(IDENTITY_CURSOR_FILE)
    }
}

/// Read the current cursor. Missing or unreadable → empty cursor.
pub fn read_identity_cursor(repo_root: &Path) -> IdentityCursor {
    read_identity_cursor_unlocked(repo_root)
}

fn read_identity_cursor_unlocked(repo_root: &Path) -> IdentityCursor {
    let path = identity_cursor_path(repo_root);
    let Ok(bytes) = fs::read(&path) else {
        return IdentityCursor::default();
    };
    serde_json::from_slice::<IdentityCursor>(&bytes)
        .unwrap_or_default()
        .omit_unpublished()
}

/// Atomic rename of a reconstructible sidecar. No fsync — next hook rewrites.
pub fn write_identity_cursor(repo_root: &Path, cursor: &IdentityCursor) -> io::Result<()> {
    let dest = identity_cursor_path(repo_root);
    let _guard = acquire_identity_lock(&dest)?;
    write_identity_cursor_unlocked(repo_root, cursor)
}

fn write_identity_cursor_unlocked(repo_root: &Path, cursor: &IdentityCursor) -> io::Result<()> {
    let dest = identity_cursor_path(repo_root);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let tmp = dest.with_file_name(format!(
        ".identity.tmp.{}.{}",
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let body = cursor
        .clone()
        .omit_unpublished()
        .to_vec()
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    fs::write(&tmp, &body)?;
    let renamed = fs::rename(&tmp, dest);
    if renamed.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    renamed
}

/// Merge `patch` onto the on-disk cursor and publish it.
///
/// Holds an exclusive sidecar lock for the read-merge-write so StatusLine
/// (model) and PreToolUse (effort) cannot clobber each other.
pub fn stamp_identity_cursor(
    repo_root: &Path,
    patch: &IdentityCursor,
) -> io::Result<IdentityCursor> {
    let dest = identity_cursor_path(repo_root);
    let _guard = acquire_identity_lock(&dest)?;
    let current = read_identity_cursor_unlocked(repo_root);
    let next = current.merge_event(patch);
    write_identity_cursor_unlocked(repo_root, &next)?;
    Ok(next)
}

/// Drop the live cursor so a later human/Cursor capture cannot freeze a dead session.
pub fn expire_identity_cursor(repo_root: &Path) -> io::Result<()> {
    let dest = identity_cursor_path(repo_root);
    let _guard = acquire_identity_lock(&dest)?;
    match fs::remove_file(&dest) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

struct IdentityWriteGuard {
    file: fs::File,
}

fn acquire_identity_lock(dest: &Path) -> io::Result<IdentityWriteGuard> {
    let lock_path = dest.with_file_name(".identity.lock");
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)?;
    file.lock_exclusive()?;
    Ok(IdentityWriteGuard { file })
}

impl Drop for IdentityWriteGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// Walk `path` for a string, or an object's `id` / `level` field.
pub fn value_string_or_named(value: &Value, path: &[&str], object_keys: &[&str]) -> Option<String> {
    let mut current = value;
    for segment in path {
        current = current.get(*segment)?;
    }
    match current {
        Value::String(s) => published_owned(Some(s.clone())),
        Value::Bool(v) => Some(v.to_string()),
        Value::Number(v) => Some(v.to_string()),
        Value::Object(obj) => object_keys
            .iter()
            .find_map(|key| obj.get(*key).and_then(Value::as_str))
            .and_then(|s| published_owned(Some(s.to_string()))),
        _ => None,
    }
}

/// Claude / Codex / OpenCode thought_level mapping (ACP name, not wire).
pub fn thought_level_from_payload(payload: &Value) -> Option<String> {
    value_string_or_named(payload, &["effort"], &["level"])
        .or_else(|| first_value_string(payload, &[&["thought_level"], &["reasoning_effort"]]))
        .or_else(|| value_string(payload, &["model", "variant"]))
        .or_else(|| value_string(payload, &["turn_context", "effort"]))
        .or_else(|| value_string(payload, &["turn_context", "reasoning_effort"]))
        .and_then(|s| published_owned(Some(s)))
}

/// Exact ancestor basename → harness kind token. Never a model.
pub fn harness_kind_from_basename(argv0: &str) -> Option<&'static str> {
    crate::harness_policy::detect_harness_kind(Some(argv0), &Default::default()).as_str()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn merge_keeps_last_cursor_when_event_omits_field() {
        let current = IdentityCursor {
            provider: Some("anthropic".into()),
            model: Some("opus".into()),
            thought_level: Some("high".into()),
            session: Some("sess-1".into()),
            parent: Some("agent-1".into()),
        };
        let next = current.merge_event(&IdentityCursor {
            thought_level: Some("low".into()),
            ..IdentityCursor::default()
        });
        assert_eq!(next.model.as_deref(), Some("opus"));
        assert_eq!(next.thought_level.as_deref(), Some("low"));
        assert_eq!(next.session.as_deref(), Some("sess-1"));
        assert_eq!(next.parent.as_deref(), Some("agent-1"));
    }

    #[test]
    fn parent_session_event_clears_stale_subagent_parent() {
        let current = IdentityCursor {
            provider: Some("openai".into()),
            model: Some("gpt-5.4".into()),
            thought_level: None,
            session: Some("sub-1".into()),
            parent: Some("parent-1".into()),
        };
        let back_on_parent = current.merge_event(&IdentityCursor {
            session: Some("parent-1".into()),
            ..IdentityCursor::default()
        });
        assert_eq!(back_on_parent.session.as_deref(), Some("parent-1"));
        assert!(
            back_on_parent.parent.is_none(),
            "parent-session event must not keep the subagent parent"
        );

        let still_subagent = current.merge_event(&IdentityCursor {
            session: Some("sub-1".into()),
            thought_level: Some("low".into()),
            ..IdentityCursor::default()
        });
        assert_eq!(still_subagent.parent.as_deref(), Some("parent-1"));
        assert_eq!(still_subagent.thought_level.as_deref(), Some("low"));
    }

    #[test]
    fn claude_main_agent_event_clears_subagent_parent() {
        let current = IdentityCursor {
            provider: Some("anthropic".into()),
            model: Some("opus".into()),
            thought_level: None,
            session: Some("sess-1".into()),
            parent: Some("agent-sub".into()),
        };
        let back_on_main = current.merge_event(&IdentityCursor {
            session: Some("sess-1".into()),
            parent: Some("sess-1".into()),
            ..IdentityCursor::default()
        });
        assert_eq!(back_on_main.session.as_deref(), Some("sess-1"));
        assert!(
            back_on_main.parent.is_none(),
            "main-agent event (parent == session) must clear the subagent parent"
        );
    }

    #[test]
    fn omit_unpublished_drops_unknown_and_empty() {
        let cursor = IdentityCursor {
            provider: Some("anthropic".into()),
            model: Some("unknown".into()),
            thought_level: Some("".into()),
            session: Some("  ".into()),
            parent: None,
        }
        .omit_unpublished();
        assert_eq!(cursor.provider.as_deref(), Some("anthropic"));
        assert!(cursor.model.is_none());
        assert!(cursor.thought_level.is_none());
        assert!(cursor.session.is_none());
    }

    #[test]
    fn effort_level_object_maps_to_thought_level() {
        let payload = json!({"effort": {"level": "high"}, "session_id": "s1"});
        assert_eq!(
            thought_level_from_payload(&payload).as_deref(),
            Some("high")
        );
        assert_eq!(
            value_string_or_named(&payload, &["effort"], &["level"]).as_deref(),
            Some("high")
        );
        assert!(value_string(&payload, &["effort"]).is_none());
    }

    #[test]
    fn basename_kind_is_exact_not_path_contains() {
        assert_eq!(
            harness_kind_from_basename("/home/u/dev/codex/target/debug/heddle"),
            None
        );
        assert_eq!(harness_kind_from_basename("/usr/bin/codex"), Some("codex"));
        assert_eq!(
            harness_kind_from_basename("/usr/local/bin/claude"),
            Some("claude-code")
        );
    }

    #[test]
    fn write_and_read_roundtrip_omits_unpublished() {
        let dir = tempfile::TempDir::new().unwrap();
        let written = stamp_identity_cursor(
            dir.path(),
            &IdentityCursor {
                provider: Some("anthropic".into()),
                model: Some("opus".into()),
                thought_level: None,
                session: Some("s1".into()),
                parent: None,
            },
        )
        .unwrap();
        assert!(written.thought_level.is_none());
        let raw = fs::read_to_string(identity_cursor_path(dir.path())).unwrap();
        assert!(!raw.contains("thought_level"));
        assert!(!raw.contains("parent"));
        assert_eq!(read_identity_cursor(dir.path()), written);
    }

    #[test]
    fn concurrent_stamps_use_unique_tmp_and_leave_valid_cursor() {
        let dir = tempfile::TempDir::new().unwrap();
        std::thread::scope(|scope| {
            for i in 0..8 {
                let root = dir.path();
                scope.spawn(move || {
                    stamp_identity_cursor(
                        root,
                        &IdentityCursor {
                            provider: Some("anthropic".into()),
                            model: Some(format!("m{i}")),
                            ..IdentityCursor::default()
                        },
                    )
                    .unwrap();
                });
            }
        });
        let cursor = read_identity_cursor(dir.path());
        assert_eq!(cursor.provider.as_deref(), Some("anthropic"));
        assert!(
            cursor
                .model
                .as_deref()
                .is_some_and(|model| model.starts_with('m')),
            "last writer must leave a published model, got {:?}",
            cursor.model
        );
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name())
            .filter(|name| name.to_string_lossy().starts_with(".identity.tmp."))
            .collect();
        assert!(
            leftovers.is_empty(),
            "unique tmp files must be renamed away"
        );
    }

    #[test]
    fn two_writer_merge_keeps_model_and_thought_level() {
        let dir = tempfile::TempDir::new().unwrap();
        stamp_identity_cursor(
            dir.path(),
            &IdentityCursor {
                provider: Some("anthropic".into()),
                ..IdentityCursor::default()
            },
        )
        .unwrap();
        let ready = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                ready.wait();
                for _ in 0..40 {
                    stamp_identity_cursor(
                        dir.path(),
                        &IdentityCursor {
                            model: Some("opus".into()),
                            ..IdentityCursor::default()
                        },
                    )
                    .unwrap();
                }
            });
            scope.spawn(|| {
                ready.wait();
                for _ in 0..40 {
                    stamp_identity_cursor(
                        dir.path(),
                        &IdentityCursor {
                            thought_level: Some("high".into()),
                            ..IdentityCursor::default()
                        },
                    )
                    .unwrap();
                }
            });
        });
        let cursor = read_identity_cursor(dir.path());
        assert_eq!(cursor.provider.as_deref(), Some("anthropic"));
        assert_eq!(
            cursor.model.as_deref(),
            Some("opus"),
            "StatusLine model must survive a concurrent PreToolUse effort stamp"
        );
        assert_eq!(
            cursor.thought_level.as_deref(),
            Some("high"),
            "PreToolUse effort must survive a concurrent StatusLine model stamp"
        );
    }

    #[test]
    fn expire_removes_cursor_so_later_read_is_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        stamp_identity_cursor(
            dir.path(),
            &IdentityCursor {
                provider: Some("anthropic".into()),
                model: Some("opus".into()),
                ..IdentityCursor::default()
            },
        )
        .unwrap();
        expire_identity_cursor(dir.path()).unwrap();
        assert!(read_identity_cursor(dir.path()).is_empty());
        assert!(!identity_cursor_path(dir.path()).exists());
    }
}
