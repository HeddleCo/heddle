// SPDX-License-Identifier: Apache-2.0
//! Shared utilities and helpers for Git bridge operations.

use ingest::LossyImportEntry;
use objects::object::{State, Status};
use sley::ObjectId as GitObjectId;

use super::git_core::GitBridge;

impl<'a> GitBridge<'a> {
    /// Build a Git commit message from a Heddle state.
    ///
    /// Phase B (post-2026-05) onward: this is just the state's intent text,
    /// verbatim. Heddle metadata (change_id, agent, confidence, status) is
    /// carried out-of-band via `refs/notes/heddle` so that exported commit
    /// SHAs match the SHAs of imported commits — a prerequisite for any
    /// bidirectional sync where heddle and an upstream git host (e.g.
    /// GitHub) need to agree on which commits already exist.
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
    /// Heddle-URL: <remote_url>/state/<id>     (omitted when no remote URL)
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
        Self::build_commit_message_with_footer_with_body(
            state,
            &body,
            hosted_url,
            annotations_omitted,
        )
    }

    pub(crate) fn build_commit_message_with_footer_with_body(
        state: &State,
        body: &str,
        hosted_url: Option<&str>,
        annotations_omitted: u32,
    ) -> String {
        let mut out = body.to_string();
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
///
/// `commits_total` counts the commits that actually land in the
/// destination: it is derived from the same branch/tag ref set
/// (`collect_ref_updates`) that `copy_mirror_to_path` copies, by walking
/// the commit ancestry of those tips. Counting from the copy path — rather
/// than a parallel walk over current Heddle refs — guarantees the reported
/// total equals what's copied, including a stale mirror ref left behind by
/// a dropped thread (export does not prune mirror refs, so that commit
/// still travels and is still counted). `states_exported` is the
/// freshly-minted *subset of that same copied ref set* — both counts are
/// partitions of one walk, so `states_exported + already == commits_total`
/// holds by construction and a state minted into the mirror but reachable
/// from no copied ref (an orphan dropped-thread history) inflates neither.
/// They diverge whenever the destination is already populated: an overlay
/// re-export reports `commits_total = N` and `states_exported = 0` — the
/// signal that surfaces "already in sync" instead of a misleading bare
/// "exported 0 states" (heddle#289, mirroring the import-side
/// `commits_imported`/`states_created` split from heddle#147).
#[derive(Debug, Default)]
pub struct ExportStats {
    /// Freshly-minted git commits that land in the destination — the
    /// subset of the copied ref set's commits minted during this export
    /// (no preserved git_oid). Stays at 0 when every copied commit was
    /// already mapped to an existing commit. A minted commit reachable
    /// from no copied ref is excluded (it never reaches the destination).
    pub states_exported: usize,
    /// Unique commits reachable from the branch/tag tips copied to the
    /// destination, including ones whose commit already existed and any
    /// carried by a stale mirror ref. Mirrors
    /// [`ImportStats::commits_imported`].
    pub commits_total: usize,
    pub threads_synced: usize,
    pub markers_synced: usize,
    /// Branches written to the destination, paired with their tip
    /// commit so the summary can show tip short-SHAs.
    pub branches: Vec<ExportedRef>,
    /// Tags written to the destination, paired with their tip commit.
    pub tags: Vec<ExportedRef>,
}

/// A ref written to the export destination, paired with the commit it
/// points at (so the export summary can render tip short-SHAs).
#[derive(Debug, Clone)]
pub struct ExportedRef {
    pub name: String,
    pub tip: GitObjectId,
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
    /// skipped.
    pub skipped_non_commit_refs: usize,
    /// Git tree entries converted under an explicit lossy import opt-in.
    pub lossy_entries: Vec<LossyImportEntry>,
}

#[cfg(test)]
mod tests {
    // ── R6 — bridge footer ─────────────────────────────────────────────
    use objects::object::{Attribution, ChangeId, ContentHash, Principal};

    use super::*;

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
