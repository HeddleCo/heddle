// SPDX-License-Identifier: Apache-2.0
//! Attach live context annotations to an existing [`DiffReport`].
//!
//! `context set` writes a Context state-attachment. `diff --context` must
//! populate the existing report fields from that attachment — the same
//! store `context get` reads — without a second view RPC.

use anyhow::Result;
use objects::{
    object::{Annotation, AnnotationStatus, ContextTarget, State, StateAttachmentBody},
    store::ObjectStore,
};
use repo::{Repository, StateAttachmentKind};

use super::types::{ContextSnippet, DiffReport, FileContextEntry};

/// Fill `report.context` and `report.broader_guidance` from the state's
/// latest Context attachment.
///
/// When the report has changed paths, only those files are annotated
/// (plus a renamed file's old path). When the change set is empty and
/// `include_unanchored` is true, every active file annotation rides the
/// report so `context set` is visible on a clean `diff --context`.
pub fn attach_show_context(
    repo: &Repository,
    report: &mut DiffReport,
    state: &State,
    include_unanchored: bool,
) -> Result<()> {
    let mut paths: Vec<String> = report
        .changes
        .iter()
        .flat_map(|change| std::iter::once(change.path.clone()).chain(change.old_path.clone()))
        .collect();
    paths.sort();
    paths.dedup();

    report.context = Some(collect_file_context(
        repo,
        state,
        &paths,
        include_unanchored && paths.is_empty(),
    )?);
    report.broader_guidance = Some(collect_state_guidance(repo, state)?);
    Ok(())
}

/// HEAD / current-state used by worktree diffs.
pub fn worktree_context_state(repo: &Repository) -> Result<Option<State>> {
    if let Some(state) = repo.current_state()? {
        return Ok(Some(state));
    }
    let Some(id) = repo.head()? else {
        return Ok(None);
    };
    Ok(repo.store().get_state(&id)?)
}

pub(crate) fn summarize_context(content: &str) -> String {
    let first_line = content
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    let char_count = first_line.chars().count();
    if char_count <= 88 {
        first_line.to_string()
    } else {
        format!("{}...", first_line.chars().take(85).collect::<String>())
    }
}

fn context_root_for_state(
    repo: &Repository,
    state: &State,
) -> Result<Option<objects::object::ContentHash>> {
    Ok(repo
        .latest_state_attachment(&state.state_id, StateAttachmentKind::Context)?
        .and_then(|attachment| match attachment.body {
            StateAttachmentBody::Context(hash) => Some(hash),
            _ => None,
        }))
}

fn collect_file_context(
    repo: &Repository,
    state: &State,
    paths: &[String],
    include_unanchored: bool,
) -> Result<Vec<FileContextEntry>> {
    let Some(context_root) = context_root_for_state(repo, state)? else {
        return Ok(Vec::new());
    };

    let paths = if include_unanchored && paths.is_empty() {
        let mut listed = Vec::new();
        for entry in repo.list_context_entries(&context_root, None)? {
            if let ContextTarget::File { path } = entry.target {
                listed.push(path);
            }
        }
        listed
    } else {
        paths.to_vec()
    };

    let mut entries = Vec::new();
    for path in paths {
        let Ok(target) = ContextTarget::file(path.clone()) else {
            continue;
        };
        let Some(blob) = repo.get_context_blob(&context_root, &target)? else {
            continue;
        };
        let annotations = active_snippets(&blob.annotations);
        if !annotations.is_empty() {
            entries.push(FileContextEntry { path, annotations });
        }
    }
    Ok(entries)
}

fn collect_state_guidance(repo: &Repository, state: &State) -> Result<Vec<ContextSnippet>> {
    let Some(context_root) = context_root_for_state(repo, state)? else {
        return Ok(Vec::new());
    };
    let target = ContextTarget::state(state.state_id);
    let Some(blob) = repo.get_context_blob(&context_root, &target)? else {
        return Ok(Vec::new());
    };
    Ok(active_snippets(&blob.annotations))
}

fn active_snippets(annotations: &[Annotation]) -> Vec<ContextSnippet> {
    annotations
        .iter()
        .filter(|annotation| annotation.status == AnnotationStatus::Active)
        .filter_map(|annotation| {
            annotation
                .current_revision()
                .map(|revision| ContextSnippet {
                    annotation_id: annotation.annotation_id.clone(),
                    kind: revision.kind.to_string(),
                    content: summarize_context(&revision.content),
                    revision_count: annotation.revisions.len(),
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use objects::object::{
        Annotation, AnnotationKind, AnnotationScope, Attribution, ContextBlob, ContextTarget,
        Principal, StateAttachment, StateAttachmentBody,
    };
    use repo::Repository;
    use tempfile::TempDir;

    use super::{attach_show_context, summarize_context};
    use crate::diff::types::{DiffReport, FileChange};

    fn annotate_state(repo: &Repository, path: &str, content: &str) -> objects::object::State {
        std::fs::write(repo.root().join(path), "seed\n").expect("write seed file");
        let state = repo
            .snapshot(Some("seed".into()), None)
            .expect("snapshot seed");
        let target = ContextTarget::file(path).expect("file target");
        let blob = ContextBlob::new(vec![Annotation::new(
            AnnotationScope::File,
            AnnotationKind::Invariant,
            content.to_string(),
            Vec::new(),
            "test@example.com".to_string(),
            1_700_000_000,
            None,
            Some(state.state_id),
        )]);
        let root = repo
            .set_context_blob(None, &target, &blob)
            .expect("store context blob");
        repo.put_state_attachment(&StateAttachment {
            state_id: state.state_id,
            body: StateAttachmentBody::Context(root),
            attribution: Attribution::human(Principal::new("test", "test@example.com")),
            created_at: chrono::Utc::now(),
            supersedes: None,
        })
        .expect("attach context");
        state
    }

    fn report_with(paths: &[&str]) -> DiffReport {
        let changes = paths
            .iter()
            .map(|path| FileChange {
                path: (*path).to_string(),
                kind: "modified".to_string(),
                ..FileChange::default()
            })
            .collect();
        DiffReport::new(Some("HEAD".to_string()), None, changes, None, None, None)
    }

    #[test]
    fn context_set_attachment_rides_diff_for_changed_path() {
        let temp = TempDir::new().expect("tempdir");
        let repo = Repository::init_default(temp.path()).expect("init");
        let state = annotate_state(&repo, "lib.rs", "must stay lowercase");
        let mut report = report_with(&["lib.rs"]);

        attach_show_context(&repo, &mut report, &state, false).expect("attach");

        let context = report.context.expect("context field");
        assert_eq!(context.len(), 1);
        assert_eq!(context[0].path, "lib.rs");
        assert_eq!(context[0].annotations.len(), 1);
        assert_eq!(context[0].annotations[0].kind, "invariant");
        assert_eq!(context[0].annotations[0].content, "must stay lowercase");
    }

    #[test]
    fn context_set_attachment_rides_clean_diff_when_unanchored() {
        let temp = TempDir::new().expect("tempdir");
        let repo = Repository::init_default(temp.path()).expect("init");
        let state = annotate_state(&repo, "lib.rs", "visible without a file change");
        let mut report = report_with(&[]);

        attach_show_context(&repo, &mut report, &state, true).expect("attach");

        let context = report.context.expect("context field");
        assert_eq!(context.len(), 1);
        assert_eq!(context[0].path, "lib.rs");
        assert_eq!(
            context[0].annotations[0].content,
            "visible without a file change"
        );
    }

    #[test]
    fn path_filter_does_not_dump_unrelated_annotations() {
        let temp = TempDir::new().expect("tempdir");
        let repo = Repository::init_default(temp.path()).expect("init");
        let state = annotate_state(&repo, "lib.rs", "not this path");
        let mut report = report_with(&[]);

        attach_show_context(&repo, &mut report, &state, false).expect("attach");

        assert!(
            report
                .context
                .as_ref()
                .is_none_or(|entries| entries.is_empty()),
            "filtered empty change set must not list every annotation"
        );
    }

    #[test]
    fn summarize_context_truncates_on_char_boundary_not_byte_index() {
        let first_line = format!("{}中中", "a".repeat(83));
        assert!(first_line.len() > 88);
        assert!(!first_line.is_char_boundary(85));
        let summary = summarize_context(&format!("{first_line}\nsecond line"));
        assert_eq!(summary, first_line);
    }

    #[test]
    fn summarize_context_char_cap_truncates_multibyte_line() {
        let first_line = format!("{}中中中", "a".repeat(86));
        assert!(first_line.chars().count() > 88);
        let summary = summarize_context(&first_line);
        let expected = format!("{}...", "a".repeat(85));
        assert_eq!(summary, expected);
    }

    #[test]
    fn summarize_context_ascii_truncation_unchanged() {
        let line = "b".repeat(90);
        let summary = summarize_context(&line);
        assert_eq!(summary, format!("{}...", "b".repeat(85)));
    }
}
