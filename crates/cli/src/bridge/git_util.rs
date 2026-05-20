// SPDX-License-Identifier: Apache-2.0
//! Shared utilities and helpers for Git bridge operations.

use std::collections::HashMap;

use objects::object::{State, Status};

use super::git_core::GitBridge;

impl<'a> GitBridge<'a> {
    /// Parse trailers from a commit message.
    pub(crate) fn parse_trailers(message: &str) -> HashMap<String, String> {
        let mut trailers = HashMap::new();

        for line in message.lines().rev() {
            if line.is_empty() {
                break;
            }

            if let Some(pos) = line.find(':') {
                let key = &line[..pos];
                let value = line[pos + 1..].trim();

                if key.starts_with("Heddle-") {
                    trailers.insert(key.to_string(), value.to_string());
                }
            } else if !line.trim().is_empty() {
                break;
            }
        }

        trailers
    }

    /// Extract intent (commit subject) from message.
    pub(crate) fn extract_intent(message: &str) -> Option<String> {
        let lines: Vec<&str> = message.lines().collect();

        for line in &lines {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed.starts_with("Heddle-") && trimmed.contains(':') {
                break;
            }
            return Some(trimmed.to_string());
        }

        None
    }

    /// Convert Heddle state attribution to Git signature.
    pub(crate) fn state_to_signature(state: &State) -> gix::actor::Signature {
        gix::actor::Signature {
            name: state.attribution.principal.name.as_str().into(),
            email: state.attribution.principal.email.as_str().into(),
            time: gix::date::Time {
                seconds: state.created_at.timestamp(),
                offset: 0,
            },
        }
    }

    /// Build a Git commit message from a Heddle state.
    ///
    /// Phase B (post-2026-05) onward: this is just the state's intent text,
    /// verbatim. Heddle metadata (change_id, agent, confidence, status) is
    /// carried out-of-band via `refs/notes/heddle` so that exported commit
    /// SHAs match the SHAs of imported commits — a prerequisite for any
    /// bidirectional sync where heddle and an upstream git host (e.g.
    /// GitHub) need to agree on which commits already exist.
    ///
    /// The legacy `Heddle-Change-Id:` / `Heddle-Status:` / `Heddle-Agent:` /
    /// `Heddle-Confidence:` trailers are no longer written. The parser
    /// (`parse_trailers`) is retained so historical commits that still
    /// carry trailers can be read; see `git_import::resolve_identity`.
    pub(crate) fn build_commit_message(state: &State) -> String {
        // Status is intentionally not surfaced here — published-vs-draft
        // belongs in heddle's note, not the commit message body, since
        // including it would change the commit SHA whenever a user toggles
        // the status field.
        let _ = Status::Draft;
        state
            .intent
            .clone()
            .unwrap_or_else(|| "No intent specified".to_string())
    }

    /// Build a commit message that includes the W2 footer (R6).
    ///
    /// Footer layout (always emitted, last block of the message):
    ///
    /// ```text
    /// <body>
    ///
    /// Heddle-State: <hex change-id>
    /// Heddle-URL: <hosted_url>/state/<id>     (omitted when no hosted URL)
    /// Heddle-Annotations-Omitted: <count>
    /// ```
    ///
    /// The footer is the durable record — every reader on every host gets
    /// it regardless of remote configuration. Richer per-scope metadata
    /// rides on the opt-in git note (see [`super::git_notes`]).
    pub(crate) fn build_commit_message_with_footer(
        state: &State,
        hosted_url: Option<&str>,
        annotations_omitted: u32,
    ) -> String {
        let body = Self::build_commit_message(state);
        let mut out = body;
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
        out.push_str(&format!(
            "Heddle-State: {}\n",
            state.change_id.to_string_full()
        ));
        if let Some(url) = hosted_url
            && !url.is_empty()
        {
            let trimmed = url.trim_end_matches('/');
            out.push_str(&format!(
                "Heddle-URL: {trimmed}/state/{}\n",
                state.change_id.to_string_full()
            ));
        }
        out.push_str(&format!(
            "Heddle-Annotations-Omitted: {annotations_omitted}\n"
        ));
        out
    }
}

/// Statistics for export operation.
#[derive(Debug, Default)]
pub struct ExportStats {
    pub states_exported: usize,
    pub threads_synced: usize,
    pub markers_synced: usize,
}

/// Statistics for import operation.
///
/// `commits_imported` counts every commit visited by the ancestry walk;
/// `states_created` counts only the commits whose heddle state did not
/// yet exist in the store. They diverge whenever a ref is re-imported
/// (the second `bridge git import --ref X` against the same source
/// reports `commits_imported = N` and `states_created = 0`) — that
/// distinction is what surfaces "already in sync" instead of leaving
/// the operator staring at a misleading `commits_imported: 0`
/// (heddle#147).
#[derive(Debug, Default)]
pub struct ImportStats {
    /// Total commits walked from the source refs, including ones whose
    /// heddle state was already present. Mirrors what `bridge git
    /// ingest` reports so the two verbs read the same way.
    pub commits_imported: usize,
    /// New state objects written to the heddle store during this
    /// import. Stays at 0 when every visited commit already had a
    /// heddle state — that's the signal the bridge is in sync.
    pub states_created: usize,
    pub branches_synced: usize,
    pub tags_synced: usize,
    /// Refs (typically annotated tags) that point at a non-commit object —
    /// most often a blob (e.g. `git/git`'s `refs/tags/junio-gpg-pub`
    /// pointing at the maintainer's GPG public key blob) or a tree
    /// (e.g. `git-lfs`'s `refs/tags/core-gpg-keys`).
    ///
    /// These are skipped during walk because heddle's marker model
    /// currently requires the target to be a commit. The full-fidelity
    /// fix is to extend the marker model with a non-commit-ref variant;
    /// until then we record them here so callers can surface what was
    /// skipped (and so a future export can restore them by reading the
    /// preserved git mirror).
    pub skipped_non_commit_refs: Vec<SkippedRef>,
    /// Refs whose object reachability could not be fully copied into
    /// the bridge mirror — see [`PartialMirrorRef`]. SHA-stable export
    /// is degraded for these refs.
    pub partial_mirror_refs: Vec<PartialMirrorRef>,
}

/// A ref that pointed at a non-commit object during import.
#[derive(Debug, Clone)]
pub struct SkippedRef {
    pub name: String,
    pub peeled_oid: String,
    pub peeled_kind: String,
}

/// A ref whose object reachability could not be fully copied into the
/// bridge mirror — typically because the source ODB is missing some
/// object referenced from the ref's commit graph (a real-world failure
/// mode in repos like `expressjs/express` and `git-lfs/git-lfs`, where
/// pack data references objects that aren't actually present and that
/// `git fsck` doesn't catch because they're not reachable from any
/// other ref).
///
/// SHA-stable export will fall back to recreating commits from heddle
/// state for the affected refs; their git_oids in the destination will
/// be heddle-derived rather than verbatim copies.
#[derive(Debug, Clone)]
pub struct PartialMirrorRef {
    pub name: String,
    pub error: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_trailers() {
        let message = r#"Add feature X

This is the body.

Heddle-Change-Id: hd-abc123
Heddle-Agent: anthropic/claude
Heddle-Confidence: 0.95
"#;

        let trailers = GitBridge::parse_trailers(message);
        assert_eq!(
            trailers.get("Heddle-Change-Id"),
            Some(&"hd-abc123".to_string())
        );
        assert_eq!(
            trailers.get("Heddle-Agent"),
            Some(&"anthropic/claude".to_string())
        );
        assert_eq!(trailers.get("Heddle-Confidence"), Some(&"0.95".to_string()));
    }

    #[test]
    fn test_extract_intent() {
        let message = "Add feature X\n\nBody here\n\nHeddle-Change-Id: hd-abc123";
        assert_eq!(
            GitBridge::extract_intent(message),
            Some("Add feature X".to_string())
        );

        let message2 = "Heddle-Change-Id: hd-abc123";
        assert_eq!(GitBridge::extract_intent(message2), None);
    }

    // ── R6 — bridge footer ─────────────────────────────────────────────

    use objects::object::{Attribution, ChangeId, ContentHash, Principal};

    fn sample_state() -> State {
        State::new_snapshot(
            ContentHash::compute(b"tree"),
            vec![],
            Attribution::human(Principal::new("Alice", "alice@example.com")),
        )
        .with_intent("ship the auth rewrite")
    }

    #[test]
    fn footer_emits_state_id_and_zero_omitted_when_no_url() {
        let state = sample_state();
        let msg = GitBridge::build_commit_message_with_footer(&state, None, 0);
        assert!(msg.contains(&format!(
            "Heddle-State: {}",
            state.change_id.to_string_full()
        )));
        assert!(msg.contains("Heddle-Annotations-Omitted: 0"));
        assert!(!msg.contains("Heddle-URL:"));
    }

    #[test]
    fn footer_emits_url_when_hosted_configured() {
        let state = sample_state();
        let msg =
            GitBridge::build_commit_message_with_footer(&state, Some("https://heddle.test/"), 3);
        assert!(msg.contains(&format!(
            "Heddle-URL: https://heddle.test/state/{}",
            state.change_id.to_string_full()
        )));
        assert!(msg.contains("Heddle-Annotations-Omitted: 3"));
    }

    // The state_id from `change_id.to_string_full()` is referenced via
    // `ChangeId` for the bound on `state.change_id` — keep the import.
    #[test]
    fn change_id_round_trips_through_footer() {
        let state = sample_state();
        let id_str = state.change_id.to_string_full();
        let _: ChangeId = ChangeId::parse(&id_str).expect("round-trip parse");
    }
}