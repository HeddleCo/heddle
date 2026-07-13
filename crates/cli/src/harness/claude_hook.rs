// SPDX-License-Identifier: Apache-2.0
//! Claude Code hook handlers beyond session tracking.
//!
//! Behaviours wired here:
//!
//! * **PreToolUse** for file-touching tools (Read/Edit/Write/MultiEdit/NotebookEdit)
//!   surfaces active Heddle annotations for the target file as `additionalContext`
//!   in the hook's `hookSpecificOutput` JSON. Claude Code injects that string
//!   into the tool-call context so constraints/invariants/rationale travel with
//!   the code. When the current thread is part of a stack with siblings or
//!   descendants, the stack's `next_action` verdict and member list are
//!   appended so the agent knows whether it's safe to ship or whether a
//!   parent is blocked.
//! * **Stop** (and **SubagentStop**) captures a Heddle state from the worktree
//!   with agent attribution derived from the hook payload, then returns. The
//!   capture is skipped when the worktree tree matches HEAD so repeated Stop
//!   events do not create redundant states.
//! * **SubagentStop** additionally marks the child `AgentEntry` as `Complete`
//!   so `heddle agent list` distinguishes finished subagents from live ones.
//! * **UserPromptSubmit** rotates the Heddle session segment so each user
//!   prompt becomes a distinct attribution segment on subsequent captures.
//!
//! All handlers are best-effort. Errors are logged via `tracing` and
//! swallowed so a transient failure cannot block the harness.

use std::path::{Path, PathBuf};

use anyhow::Result;
use objects::{
    object::{AnnotationKind, AnnotationScope, AnnotationStatus, ContextTarget},
    store::{AgentRegistry, AgentStatus, ObjectStore},
};
use refs::Head;
use repo::{Repository, RepositorySnapshot, SessionManager, StackNextAction, StateAttachmentKind};
use serde_json::{Value, json};
use tracing::debug;

use crate::{
    cli::commands::snapshot::{SnapshotAgentOverrides, create_snapshot},
    config::UserConfig,
};

/// PreToolUse dispatcher.
///
/// Only file-reading/editing tools trigger context injection. Unknown or
/// non-file tools are ignored.
pub(crate) fn handle_pre_tool_use(repo: &Repository, payload: &Value) -> Result<()> {
    let tool_name = payload
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !is_file_tool(tool_name) {
        return Ok(());
    }
    let Some(path_str) = payload
        .get("tool_input")
        .and_then(|v| v.get("file_path"))
        .and_then(Value::as_str)
    else {
        return Ok(());
    };
    let Some(rel_path) = relative_to_repo(repo.root(), path_str) else {
        return Ok(());
    };
    let annotations = match load_active_annotations(repo, &rel_path) {
        Ok(list) => list,
        Err(err) => {
            debug!(?err, "heddle context lookup failed in PreToolUse hook");
            return Ok(());
        }
    };
    let stack_context = stack_context_for_current_thread(repo);
    if annotations.is_empty() && stack_context.is_none() {
        return Ok(());
    }
    let mut body = String::new();
    if !annotations.is_empty() {
        body.push_str(&format_annotations(&rel_path, &annotations));
    }
    if let Some(stack_body) = stack_context {
        if !body.is_empty() {
            body.push_str("\n\n");
        }
        body.push_str(&stack_body);
    }
    emit_hook_specific_output("PreToolUse", &body);
    Ok(())
}

/// Render the current thread's stack context as additional context for
/// the agent. Returns `None` when no current thread is attached or the
/// thread isn't part of a multi-member stack (single-thread stacks add
/// no signal worth burning context on).
///
/// All errors are swallowed and logged — best-effort by contract.
fn stack_context_for_current_thread(repo: &Repository) -> Option<String> {
    let current = match repo.head_ref().ok()? {
        Head::Attached { thread } => thread,
        Head::Detached { .. } => return None,
    };
    let snapshot = match RepositorySnapshot::capture(repo) {
        Ok(s) => s,
        Err(err) => {
            debug!(?err, "stack snapshot capture failed in PreToolUse hook");
            return None;
        }
    };
    let stack = snapshot.stack_containing(&current)?;
    if stack.member_count() < 2 {
        return None;
    }
    let action = snapshot.next_action_for(&current).ok()?;
    Some(format_stack_context(&current, stack, &action))
}

fn format_stack_context(
    current: &str,
    stack: &repo::ThreadStack,
    action: &StackNextAction,
) -> String {
    let mut body = format!(
        "Heddle stack: `{current}` is part of `{root}` ({size} threads, depth {depth}).\n",
        root = stack.root_name(),
        size = stack.member_count(),
        depth = stack.depth()
    );
    body.push_str("Members (root-first): ");
    let members: Vec<String> = stack
        .member_names()
        .into_iter()
        .map(|name| {
            if name == current {
                format!("**{name}**")
            } else {
                name.to_string()
            }
        })
        .collect();
    body.push_str(&members.join(" → "));
    body.push('\n');
    let verdict = match action {
        StackNextAction::Ready => "next-action: ready — every thread is shippable.".to_string(),
        StackNextAction::Blocked { thread } => {
            format!(
                "next-action: blocked — `{thread}` is in Blocked state; resolve before shipping."
            )
        }
        StackNextAction::WaitingOnReview { thread } => {
            format!("next-action: waiting-on-review — `{thread}` is still in flight.")
        }
        StackNextAction::Unknown => {
            "next-action: unknown — stack state is exotic; run `heddle status` for details."
                .to_string()
        }
    };
    body.push_str(&verdict);
    body
}

/// Stop / SubagentStop capture.
///
/// Creates a Heddle state from the current worktree, attributed to the agent
/// described in the payload. Returns `Ok(())` whether or not a capture
/// happened (clean worktrees are silently skipped).
pub(crate) fn handle_stop_capture(
    repo: &Repository,
    user_config: &UserConfig,
    payload: &Value,
    intent_hint: &str,
) -> Result<()> {
    if !worktree_dirty(repo)? {
        return Ok(());
    }
    let overrides = SnapshotAgentOverrides {
        provider: Some("anthropic".to_string()),
        model: resolve_model(payload),
        session: payload
            .get("session_id")
            .and_then(Value::as_str)
            .map(|s| s.to_string()),
        segment: None,
        policy: None,
        no_policy: false,
        no_agent: false,
    };
    let intent = payload
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| payload.get("stop_reason").and_then(Value::as_str))
        .map(|s| s.to_string())
        .unwrap_or_else(|| intent_hint.to_string());
    let output = create_snapshot(repo, user_config, Some(intent), None, overrides)?;
    debug!(state_id = %output.state_id, "heddle stop-hook captured state");
    Ok(())
}

fn is_file_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "Read" | "Edit" | "Write" | "NotebookEdit" | "MultiEdit"
    )
}

fn relative_to_repo(root: &Path, raw: &str) -> Option<PathBuf> {
    let raw_path = PathBuf::from(raw);
    if raw_path.is_absolute() {
        raw_path.strip_prefix(root).ok().map(Path::to_path_buf)
    } else {
        Some(raw_path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveAnnotation {
    kind: AnnotationKind,
    scope: AnnotationScope,
    content: String,
    attribution: String,
}

fn load_active_annotations(repo: &Repository, rel_path: &Path) -> Result<Vec<ActiveAnnotation>> {
    let Some(head_id) = repo.head()? else {
        return Ok(Vec::new());
    };
    let Some(state) = repo.store().get_state(&head_id)? else {
        return Ok(Vec::new());
    };
    let Some(ctx_root) = repo
        .latest_state_attachment(&state.state_id, StateAttachmentKind::Context)?
        .and_then(|attachment| match attachment.body {
            objects::object::StateAttachmentBody::Context(root) => Some(root),
            _ => None,
        })
    else {
        return Ok(Vec::new());
    };
    let path_str = rel_path.to_string_lossy().to_string();
    let Ok(target) = ContextTarget::file(&path_str) else {
        return Ok(Vec::new());
    };
    let Some(blob) = repo.get_context_blob(&ctx_root, &target)? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for annotation in &blob.annotations {
        if annotation.status != AnnotationStatus::Active {
            continue;
        }
        let Some(rev) = annotation.current_revision() else {
            continue;
        };
        out.push(ActiveAnnotation {
            kind: rev.kind,
            scope: annotation.scope.clone(),
            content: rev.content.clone(),
            attribution: rev.attribution.clone(),
        });
    }
    Ok(out)
}

fn format_annotations(rel_path: &Path, annotations: &[ActiveAnnotation]) -> String {
    let count = annotations.len();
    let mut out = format!(
        "Heddle carries {count} active annotation{plural} for `{path}` from prior work:\n",
        plural = if count == 1 { "" } else { "s" },
        path = rel_path.display()
    );
    for a in annotations {
        let kind_tag = match a.kind {
            AnnotationKind::Constraint => "constraint",
            AnnotationKind::Invariant => "invariant",
            AnnotationKind::Rationale => "rationale",
        };
        let scope_tag = match &a.scope {
            AnnotationScope::File => "file".to_string(),
            AnnotationScope::Symbol { name, .. } => format!("symbol:{name}"),
            AnnotationScope::Lines(a, b) => format!("lines:{a}-{b}"),
        };
        out.push_str(&format!(
            "- [{kind_tag} · {scope_tag}] {} (via {})\n",
            a.content.trim(),
            a.attribution
        ));
    }
    out.push_str(
        "\nThese annotations encode rules, invariants, and design rationale captured alongside the code. \
         Respect them, or supersede them with `heddle context supersede` before capturing a change that invalidates them.",
    );
    out
}

fn worktree_dirty(repo: &Repository) -> Result<bool> {
    let Some(head_id) = repo.head()? else {
        return Ok(true);
    };
    let Some(head_state) = repo.store().get_state(&head_id)? else {
        return Ok(true);
    };
    let tree = repo.build_tree(repo.root())?;
    let tree_hash = repo.store().put_tree(&tree)?;
    Ok(tree_hash != head_state.tree)
}

fn resolve_model(payload: &Value) -> Option<String> {
    if let Some(display) = payload
        .get("model")
        .and_then(|m| m.get("display_name"))
        .and_then(Value::as_str)
    {
        return Some(display.to_string());
    }
    if let Some(id) = payload
        .get("model")
        .and_then(|m| m.get("id"))
        .and_then(Value::as_str)
    {
        return Some(id.to_string());
    }
    payload
        .get("model")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
}

fn emit_hook_specific_output(event: &str, context_body: &str) {
    let value = json!({
        "hookSpecificOutput": {
            "hookEventName": event,
            "additionalContext": context_body,
        }
    });
    if serde_json::to_writer(std::io::stdout(), &value).is_ok() {
        println!();
    }
}

/// UserPromptSubmit handler: rotate the current Heddle session segment so each
/// user prompt becomes a distinct segment boundary in subsequent attribution.
///
/// `open_session`'s `should_rotate_segment` only rotates on identity change
/// (provider/model). Prompt boundaries are a separate, semantic signal:
/// rotating here gives every captured state a stable back-pointer to the
/// prompt that produced it.
pub(crate) fn handle_user_prompt_segment_rotate(
    repo: &Repository,
    heddle_session_id: &str,
    payload: &Value,
) -> Result<()> {
    if heddle_session_id.is_empty() {
        return Ok(());
    }
    let provider = "anthropic".to_string();
    let model = resolve_model(payload).unwrap_or_else(|| "unknown".to_string());
    let mut sessions = SessionManager::new(repo.root());
    match sessions.add_segment(heddle_session_id, provider, model, None) {
        Ok(segment) => {
            debug!(segment_id = %segment.id, heddle_session_id, "rotated segment on UserPromptSubmit");
            Ok(())
        }
        Err(err) => {
            debug!(?err, "segment rotation skipped (session may have ended)");
            Ok(())
        }
    }
}

/// SubagentStop handler: mark the child `AgentEntry` as `Complete` so
/// `heddle agent list` reflects the distinction between live and finished
/// subagents. The capture itself is handled by `handle_stop_capture`.
pub(crate) fn mark_subagent_complete(repo: &Repository, payload: &Value) -> Result<()> {
    // Claude Code identifies a subagent by its `agent_id`. Top-level sessions
    // have no `agent_id`; we must not touch the root entry here.
    let Some(agent_id) = payload.get("agent_id").and_then(Value::as_str) else {
        return Ok(());
    };
    let native_actor_key = format!("claude-code:agent:{agent_id}");
    let registry = AgentRegistry::new(repo.heddle_dir());
    let Some(entry) = registry.find_active_by_native_actor_key(&native_actor_key)? else {
        debug!(%native_actor_key, "no active subagent entry to mark complete");
        return Ok(());
    };
    registry.update_status(&entry.session_id, AgentStatus::Complete)?;
    debug!(session_id = %entry.session_id, %native_actor_key, "marked subagent AgentEntry Complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use objects::object::{
        Annotation, AnnotationKind, AnnotationRevision, AnnotationScope, AnnotationStatus,
        ContextBlob, ContextTarget,
    };

    use super::*;

    fn annotation(kind: AnnotationKind, scope: AnnotationScope, content: &str) -> Annotation {
        Annotation {
            annotation_id: "ann-1".to_string(),
            scope,
            status: AnnotationStatus::Active,
            revisions: vec![AnnotationRevision {
                revision_id: "rev-1".to_string(),
                kind,
                content: content.to_string(),
                tags: vec![],
                attribution: "alice <alice@example.com>".to_string(),
                created_at: 0,
                source_hash: None,
                created_at_state: None,
            }],
            supersedes_annotation_id: None,
            supersedes_rewrite_pct: None,
            visibility: objects::object::VisibilityTier::default(),
            resolved_from_discussion: None,
        }
    }

    #[test]
    fn is_file_tool_recognizes_edit_family() {
        assert!(is_file_tool("Read"));
        assert!(is_file_tool("Edit"));
        assert!(is_file_tool("Write"));
        assert!(is_file_tool("MultiEdit"));
        assert!(is_file_tool("NotebookEdit"));
        assert!(!is_file_tool("Bash"));
        assert!(!is_file_tool("Task"));
        assert!(!is_file_tool(""));
    }

    #[test]
    fn relative_to_repo_strips_absolute_prefix() {
        let root = Path::new("/home/me/proj");
        assert_eq!(
            relative_to_repo(root, "/home/me/proj/src/lib.rs"),
            Some(PathBuf::from("src/lib.rs")),
        );
        assert_eq!(
            relative_to_repo(root, "src/lib.rs"),
            Some(PathBuf::from("src/lib.rs")),
        );
        assert_eq!(relative_to_repo(root, "/other/path"), None);
    }

    #[test]
    fn resolve_model_prefers_display_then_id_then_flat() {
        let display = serde_json::json!({"model": {"display_name": "Claude Opus 4.7", "id": "claude-opus-4-7"}});
        assert_eq!(resolve_model(&display).as_deref(), Some("Claude Opus 4.7"));
        let id = serde_json::json!({"model": {"id": "claude-opus-4-7"}});
        assert_eq!(resolve_model(&id).as_deref(), Some("claude-opus-4-7"));
        let flat = serde_json::json!({"model": "claude-sonnet-4-6"});
        assert_eq!(resolve_model(&flat).as_deref(), Some("claude-sonnet-4-6"));
        let none = serde_json::json!({});
        assert_eq!(resolve_model(&none), None);
    }

    #[test]
    fn format_annotations_includes_all_three_kinds() {
        let rel = PathBuf::from("src/lib.rs");
        let input = vec![
            ActiveAnnotation {
                kind: AnnotationKind::Constraint,
                scope: AnnotationScope::File,
                content: "must be idempotent".to_string(),
                attribution: "alice".to_string(),
            },
            ActiveAnnotation {
                kind: AnnotationKind::Invariant,
                scope: AnnotationScope::Lines(10, 20),
                content: "never calls baz()".to_string(),
                attribution: "bob".to_string(),
            },
            ActiveAnnotation {
                kind: AnnotationKind::Rationale,
                scope: AnnotationScope::Symbol {
                    name: "foo".to_string(),
                    resolved_lines: None,
                },
                content: "inlined for hot path".to_string(),
                attribution: "claude-sonnet-4-6".to_string(),
            },
        ];
        let rendered = format_annotations(&rel, &input);
        assert!(rendered.contains("3 active annotations"));
        assert!(rendered.contains("[constraint · file]"));
        assert!(rendered.contains("[invariant · lines:10-20]"));
        assert!(rendered.contains("[rationale · symbol:foo]"));
        assert!(rendered.contains("via alice"));
        assert!(rendered.contains("supersede"));
    }

    #[test]
    fn format_stack_context_highlights_current_thread_and_verdict() {
        use repo::{StackNode, ThreadStack};
        let stack = ThreadStack {
            root: StackNode {
                name: "feature-a".to_string(),
                children: vec![StackNode {
                    name: "feature-b".to_string(),
                    children: vec![StackNode {
                        name: "feature-c".to_string(),
                        children: vec![],
                    }],
                }],
            },
        };
        let body = format_stack_context(
            "feature-b",
            &stack,
            &StackNextAction::WaitingOnReview {
                thread: "feature-c".to_string(),
            },
        );
        assert!(body.contains("`feature-b`"));
        assert!(body.contains("`feature-a`"));
        assert!(body.contains("3 threads, depth 2"));
        assert!(body.contains("feature-a → **feature-b** → feature-c"));
        assert!(body.contains("waiting-on-review"));
        assert!(body.contains("`feature-c`"));
    }

    #[test]
    fn context_blob_round_trips_active_annotations() {
        // Dogfood the storage format: decode what encode produces and verify
        // filtering only surfaces Active revisions.
        let mut active = annotation(
            AnnotationKind::Constraint,
            AnnotationScope::File,
            "x must be non-negative",
        );
        active.status = AnnotationStatus::Active;
        let mut superseded = annotation(
            AnnotationKind::Rationale,
            AnnotationScope::File,
            "old reason",
        );
        superseded.status = AnnotationStatus::Superseded;
        let blob = ContextBlob::new(vec![active, superseded]);
        let encoded = blob.encode().unwrap();
        let decoded = ContextBlob::decode(&encoded).unwrap();
        let live: Vec<_> = decoded
            .annotations
            .iter()
            .filter(|a| a.status == AnnotationStatus::Active)
            .collect();
        assert_eq!(live.len(), 1);
        assert_eq!(
            live[0].current_revision().unwrap().content,
            "x must be non-negative"
        );
        // ContextTarget::file round-trips a relative path:
        let t = ContextTarget::file("src/lib.rs").unwrap();
        assert_eq!(t.path(), Some("src/lib.rs"));
    }
}
