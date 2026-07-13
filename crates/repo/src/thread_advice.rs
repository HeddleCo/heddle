// SPDX-License-Identifier: Apache-2.0
//! Shared thread recommendation and health helpers.

use serde::Serialize;

use crate::{Thread, ThreadFreshness, ThreadState};

/// Shell-quote an argument for inclusion in a recommended-command string so a
/// value containing whitespace or shell metacharacters yields a runnable
/// command (and tokenizes correctly through the CLI's next_action validator).
///
/// Applied defensively at EVERY breadcrumb construction site — to thread ids
/// as well as file paths — because not every id reaching a breadcrumb is a
/// freshly-validated [`crate::ThreadId`]. [`ThreadId::new_unchecked`] (used for
/// `Deserialize` and `ThreadRecord::thread_id`), historical/persisted records,
/// and `heddle agent reserve --thread` all bypass [`crate::validate_thread_id`].
/// Creation-time validation stays as a UX / early-reject layer, but the SAFETY
/// guarantee is the emit-quoting here: a clean slug (`feature/x`) passes through
/// bare and renders unchanged, while an unsafe id is single-quoted into one
/// token so it can never split into extra args and break the breadcrumb.
pub fn shell_quote(arg: &str) -> String {
    let safe = !arg.is_empty()
        && arg.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(b, b'_' | b'-' | b'.' | b'/' | b'@' | b':' | b'+' | b'=')
        });
    if safe {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecommendedAction {
    Capture,
    Ready,
    Sync,
    Land,
    Resolve,
    Review,
    Promote,
}

impl RecommendedAction {
    pub fn command(&self, thread_id: &str) -> Option<String> {
        // Quote `thread_id` defensively: not every id reaching this function is
        // a freshly-validated `ThreadId`. `new_unchecked` (Deserialize /
        // `ThreadRecord::thread_id`), historical/persisted records, and
        // `heddle agent reserve --thread` all bypass `validate_thread_id`, so an
        // unsafe id (whitespace / shell metacharacter) can reach here. A clean
        // slug passes through bare (unchanged); an unsafe one becomes a single
        // quoted token so the breadcrumb stays runnable and survives the CLI's
        // next_action validator. (heddle#464 — defense-in-depth: quote at the
        // emit boundary; creation-time validation stays as a UX/early-reject
        // layer, but safety does not depend on it being covered everywhere.)
        match self {
            Self::Capture => Some("heddle capture -m \"...\"".to_string()),
            Self::Ready => Some(format!("heddle ready {}", thread_flag(thread_id))),
            Self::Sync => Some(format!("heddle sync {}", thread_flag(thread_id))),
            Self::Land => Some(format!("heddle land {}", thread_flag(thread_id))),
            Self::Resolve => Some("heddle resolve --list".to_string()),
            Self::Review => None,
            Self::Promote => Some(format!(
                "heddle thread promote {}",
                positional_value(thread_id)
            )),
        }
    }
}

/// Render `--thread <id>` so a leading-dash id (e.g. a historical/`new_unchecked`
/// thread literally named `-foo`) is bound via the `=` form. clap parses
/// `--thread=-foo` as the flag's value, whereas `--thread -foo` parses `-foo` as
/// another option and breaks the breadcrumb (clap has no `allow_hyphen_values`
/// here). Shell-quoting still applies for whitespace/metacharacters.
pub fn thread_flag(thread_id: &str) -> String {
    let q = shell_quote(thread_id);
    if thread_id.starts_with('-') {
        format!("--thread={q}")
    } else {
        format!("--thread {q}")
    }
}

/// Render a trailing POSITIONAL value. A leading-dash value needs the `--`
/// end-of-options separator (positionals can't use the `=` form), so
/// `heddle thread promote -foo` becomes `heddle thread promote -- -foo`.
fn positional_value(value: &str) -> String {
    let q = shell_quote(value);
    if value.starts_with('-') {
        format!("-- {q}")
    } else {
        q
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ThreadAdvice {
    pub thread_health: String,
    pub blockers: Vec<String>,
    pub recommended_action: String,
}

pub fn describe_thread_advice(
    thread: &Thread,
    worktree_dirty: bool,
    conflicts: usize,
    clean_ready_merges_to_apply: bool,
) -> ThreadAdvice {
    describe_thread_advice_with_initial(
        thread,
        worktree_dirty,
        conflicts,
        clean_ready_merges_to_apply,
        false,
    )
}

/// Variant that distinguishes a worktree that diverges from the
/// seeded genesis state (no user capture has happened yet) from one
/// that has accumulated changes since a real capture.
///
/// When `initial_state` is true and the worktree is dirty, the thread
/// is labeled `"uncaptured"` instead of `"dirty_worktree"`. The
/// recommended action stays `heddle capture` — only the label
/// changes, so the user-facing first impression matches the actual
/// situation (nothing has been captured yet) rather than implying
/// that something has drifted. See heddle#160.
pub fn describe_thread_advice_with_initial(
    thread: &Thread,
    worktree_dirty: bool,
    conflicts: usize,
    clean_ready_merges_to_apply: bool,
    initial_state: bool,
) -> ThreadAdvice {
    if matches!(thread.state, ThreadState::Abandoned | ThreadState::Merged) {
        return ThreadAdvice {
            thread_health: thread.state.to_string(),
            blockers: Vec::new(),
            recommended_action: String::new(),
        };
    }

    // A freshly-initialized active thread with no work, no conflicts, no
    // merges pending, and no promotion warning is healthy. The advice
    // cascade below otherwise falls through to a misleading
    // "needs_attention" + "heddle ready" recommendation for repos that have
    // genuinely nothing to do yet.
    let fresh_and_idle = !worktree_dirty
        && conflicts == 0
        && !clean_ready_merges_to_apply
        && thread.state == ThreadState::Active
        && thread.freshness != ThreadFreshness::Stale
        && thread.changed_paths.is_empty()
        && !thread.promotion_suggested;
    if fresh_and_idle {
        return ThreadAdvice {
            thread_health: "clean".to_string(),
            blockers: Vec::new(),
            recommended_action: String::new(),
        };
    }

    let mut blockers = Vec::new();
    let action = if worktree_dirty {
        RecommendedAction::Capture
    } else if thread.freshness == ThreadFreshness::Stale {
        blockers.push(format!(
            "Thread '{}' is stale against '{}'",
            thread.id,
            thread
                .target_thread
                .as_deref()
                .unwrap_or("its target thread")
        ));
        if conflicts > 0 {
            blockers.push(format!(
                "{} path conflict(s) need manual resolution after refresh",
                conflicts
            ));
        }
        RecommendedAction::Sync
    } else if thread.promotion_suggested && !thread.heavy_impact_paths.is_empty() {
        blockers.push(format!(
            "Heavy-impact change: {} — review broader impact before merging",
            preview_paths(&thread.heavy_impact_paths)
        ));
        RecommendedAction::Review
    } else if conflicts > 0 || thread.state == ThreadState::Blocked {
        if conflicts > 0 {
            blockers.push(format!(
                "{} path conflict(s) need manual resolution",
                conflicts
            ));
        } else if blockers.is_empty() {
            blockers.push("Thread needs attention before integration".to_string());
        }
        // `land` — not `resolve --list`. This is a metadata-only function; it
        // is always called from non-materialized contexts (status passes
        // conflicts=0, the only conflicts>0 caller is the merge dry-run
        // preview), so no merge state exists for `resolve` to read here and a
        // `resolve --list` breadcrumb dies with `no_merge_in_progress`. `land`
        // re-drives the thread: it materializes a real conflict (then surfaces
        // `continue`) or re-reports the specific blocker with its own
        // recommendation. (heddle#464 close-the-class.)
        RecommendedAction::Land
    } else if thread.state == ThreadState::Ready
        && thread.integration_policy_result.status.as_deref() == Some("previewed")
    {
        return ThreadAdvice {
            thread_health: "ready".to_string(),
            blockers,
            recommended_action: format!("heddle land {}", thread_flag(&thread.id)),
        };
    } else if clean_ready_merges_to_apply || thread.state == ThreadState::Ready {
        RecommendedAction::Land
    } else {
        RecommendedAction::Ready
    };

    let thread_health = if worktree_dirty && initial_state {
        "uncaptured"
    } else if worktree_dirty {
        "dirty_worktree"
    } else if !blockers.is_empty() {
        "blocked"
    } else if thread.state == ThreadState::Ready {
        "ready"
    } else if thread.freshness == ThreadFreshness::Current {
        "active"
    } else {
        "needs_attention"
    }
    .to_string();

    ThreadAdvice {
        thread_health,
        blockers,
        recommended_action: action.command(&thread.id).unwrap_or_default(),
    }
}

/// Format a path list for inclusion in a one-line blocker message.
///
/// Keeps the first few names and tags the rest as `… +N more`. Without this,
/// a repo with hundreds of changed files would push a 1.5-screen-wide line
/// into `heddle status` / `heddle thread drop` / `heddle merge --preview`.
/// The full list still lives in the JSON form of every advice-emitting verb.
fn preview_paths(paths: &[String]) -> String {
    const PREVIEW: usize = 3;
    let visible: Vec<&str> = paths.iter().take(PREVIEW).map(String::as_str).collect();
    let suffix = if paths.len() > visible.len() {
        format!(", … +{} more", paths.len() - visible.len())
    } else {
        String::new()
    };
    format!("{}{suffix}", visible.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread_json(state: &str) -> Thread {
        serde_json::from_value(serde_json::json!({
            "id": "feature/x",
            "thread": "feature/x",
            "target_thread": "main",
            "mode": "materialized",
            "state": state,
            "base_state": "aaaa",
            "base_root": "bbbb",
            "freshness": "current",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
        }))
        .expect("thread fixture should deserialize")
    }

    // heddle#464 close-the-class: `describe_thread_advice` is metadata-only —
    // it is never called from a context that has materialized a merge (status
    // passes conflicts=0; the lone conflicts>0 caller is the dry-run merge
    // preview). So it must never emit `heddle resolve --list`, which would die
    // with `no_merge_in_progress`. A blocked thread re-drives through `land`.
    #[test]
    fn blocked_thread_recommends_land_not_dead_resolve_breadcrumb() {
        let advice = describe_thread_advice(&thread_json("blocked"), false, 0, false);
        assert_eq!(advice.thread_health, "blocked");
        assert_ne!(advice.recommended_action, "heddle resolve --list");
        assert_eq!(advice.recommended_action, "heddle land --thread feature/x");
    }

    // Even when a preview reports conflicts, the merge is a dry run with no
    // materialized state, so the breadcrumb must drive materialization (land),
    // never a dead `resolve --list`.
    #[test]
    fn previewed_conflicts_recommend_land_not_dead_resolve_breadcrumb() {
        let advice = describe_thread_advice(&thread_json("active"), false, 2, false);
        assert_ne!(advice.recommended_action, "heddle resolve --list");
        assert_eq!(advice.recommended_action, "heddle land --thread feature/x");
    }

    // A clean slug renders bare in every breadcrumb (no regression): quoting is
    // a no-op for the safe set, so existing copy-pasteable commands are
    // unchanged.
    #[test]
    fn command_interpolates_safe_slug_bare() {
        assert_eq!(
            RecommendedAction::Sync.command("feature/x").as_deref(),
            Some("heddle sync --thread feature/x")
        );
        assert_eq!(
            RecommendedAction::Land.command("feature/x").as_deref(),
            Some("heddle land --thread feature/x")
        );
        assert_eq!(
            RecommendedAction::Promote.command("team@scope").as_deref(),
            Some("heddle thread promote team@scope")
        );
    }

    // heddle#464 defense-in-depth: an UNVALIDATED id that bypassed
    // `ThreadId::new` — exactly what `ThreadId::new_unchecked` models for a
    // deserialized / historical record or a `heddle agent reserve --thread`
    // value — must still render as a single quoted shell token through
    // `command()`, never bare. This is the close-the-class proof: safety does
    // not depend on the creation boundary being covered.
    #[test]
    fn command_quotes_unsafe_unvalidated_thread_id() {
        for unsafe_id in ["bad;echo pwn", "my feature", "a$(x)"] {
            // Construct the id the way a persisted/historical record does —
            // straight through `new_unchecked`, skipping validation.
            let historical = crate::ThreadId::new_unchecked(unsafe_id);
            let rendered = RecommendedAction::Sync
                .command(historical.as_str())
                .expect("sync breadcrumb");
            assert_eq!(rendered, format!("heddle sync --thread '{unsafe_id}'"));
            // Guard: the offending id must not appear bare (the P1 bug).
            assert!(
                !rendered.contains(&format!("--thread {unsafe_id}")),
                "the unsafe id must be quoted, not interpolated bare: {rendered}"
            );
        }
    }

    // `shell_quote` leaves the safe slug set bare and single-quotes anything
    // else — thread ids AND the file PATHS that legitimately CAN contain spaces.
    #[test]
    fn shell_quote_quotes_whitespace_and_metacharacters() {
        // Safe slugs / ordinary paths pass through bare...
        assert_eq!(shell_quote("src/lib.rs"), "src/lib.rs");
        assert_eq!(shell_quote("feature/x"), "feature/x");
        assert_eq!(shell_quote("team@scope"), "team@scope");
        // ...but whitespace / shell metacharacters are single-quoted so the
        // recommended command is runnable and tokenizes correctly in the
        // next_action validator (e.g. `heddle resolve 'my file.txt'`).
        assert_eq!(shell_quote("my file.txt"), "'my file.txt'");
        assert_eq!(shell_quote("bad;echo pwn"), "'bad;echo pwn'");
        assert_eq!(shell_quote("a'b"), r"'a'\''b'");
    }

    // heddle#464 round 8: a leading-dash id (e.g. a historical `-foo` that
    // `validate_thread_id` now rejects, but that can still arrive via
    // `new_unchecked`) is in `shell_quote`'s safe set, so quoting alone leaves it
    // bare and clap parses `-foo` as a flag. The flag form must use `=` (clap
    // binds the value); the positional form needs the `--` end-of-options marker.
    #[test]
    fn leading_dash_thread_ids_use_equals_and_separator_forms() {
        let id = crate::ThreadId::new_unchecked("-foo");
        assert_eq!(
            RecommendedAction::Sync.command(id.as_str()).as_deref(),
            Some("heddle sync --thread=-foo")
        );
        assert_eq!(
            RecommendedAction::Ready.command(id.as_str()).as_deref(),
            Some("heddle ready --thread=-foo")
        );
        assert_eq!(
            RecommendedAction::Land.command(id.as_str()).as_deref(),
            Some("heddle land --thread=-foo")
        );
        assert_eq!(
            RecommendedAction::Promote.command(id.as_str()).as_deref(),
            Some("heddle thread promote -- -foo")
        );
        // Clean slugs are unchanged (space form, bare).
        assert_eq!(
            RecommendedAction::Sync.command("feature/x").as_deref(),
            Some("heddle sync --thread feature/x")
        );
    }
}
