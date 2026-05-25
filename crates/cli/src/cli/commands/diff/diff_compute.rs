// SPDX-License-Identifier: Apache-2.0
//! Core diff command logic.

use std::collections::BTreeSet;

use anyhow::{Result, anyhow};
use objects::{
    object::{
        AnnotationStatus, Blob, ChangeId, ContextTarget, DiffKind, FileChangeSet, State, Tree,
    },
    worktree::diff_blobs,
};
use repo::Repository;

#[cfg(not(feature = "semantic"))]
use super::super::advice::RecoveryAdvice;
use super::{
    super::{
        git_overlay_health::{
            PlainGitVerificationProbe, build_plain_git_verification_probe,
            build_repository_verification_state, trust_visible_worktree_status,
        },
        history_target::{require_resolved_state, resolve_state_id},
    },
    diff_output::{print_context, print_diff, print_semantic_changes, print_stat},
    diff_types::{
        ContextSnippet, DiffOutput, DiffStats, FileChange, FileContextEntry, LineDiff,
        SemanticChangeEntry,
    },
};
#[cfg(feature = "semantic")]
use crate::semantic::{
    SemanticDiffOptions, SemanticDiffResult, semantic_diff, semantic_diff_worktree,
};
use crate::{
    cli::{Cli, should_output_json, worktree_status_options},
    config::UserConfig,
};

const BINARY_DIFF_ERROR: &str = "binary file";
#[cfg(not(feature = "semantic"))]
struct SemanticDiffResult {
    changes: Vec<objects::object::SemanticChange>,
    file_changes: FileChangeSet,
}

#[allow(clippy::too_many_arguments)]
pub fn cmd_diff(
    cli: &Cli,
    from: Option<String>,
    to: Option<String>,
    semantic: bool,
    stat: bool,
    name_only: bool,
    unified: usize,
    show_context: bool,
) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let start = cli.repo.as_ref().unwrap_or(&cwd);
    let from_is_head_or_default = from
        .as_deref()
        .map(|spec| matches!(spec, "HEAD" | "@"))
        .unwrap_or(true);
    if to.is_none()
        && from_is_head_or_default
        && let Some(probe) = build_plain_git_verification_probe(start)?
    {
        return render_plain_git_head_diff(cli, &probe, stat, name_only);
    }

    let repo = Repository::open(start)?;
    let trust = build_repository_verification_state(&repo);
    if to.is_none()
        && from_is_head_or_default
        && let Some(status) = trust_visible_worktree_status(&repo, &trust)?
    {
        return render_worktree_status_diff(cli, &status, stat, name_only, true);
    }
    let git_overlay_head_worktree_diff = repo.current_state()?.is_none()
        && to.is_none()
        && matches!(from.as_deref(), Some("HEAD" | "@"));
    if !git_overlay_head_worktree_diff
        && repo.current_state()?.is_none()
        && (matches!(from.as_deref(), Some("HEAD" | "@"))
            || matches!(to.as_deref(), Some("HEAD" | "@")))
    {
        crate::cli::commands::snapshot::ensure_current_state(
            &repo,
            &UserConfig::load_default().unwrap_or_default(),
            Some("Bootstrap git-overlay before diffing HEAD".to_string()),
        )?;
    }

    let from_id = if git_overlay_head_worktree_diff {
        None
    } else if let Some(ref spec) = from {
        Some(resolve_state_id(&repo, spec)?)
    } else {
        repo.head()?
    };

    let from_state = if let Some(id) = from_id {
        Some(require_resolved_state(&repo, &id)?)
    } else {
        None
    };

    let from_tree = if let Some(ref state) = from_state {
        repo.store().get_tree(&state.tree)?
    } else {
        None
    };
    let status_options = worktree_status_options(Some(repo.config()));

    let semantic_result: Option<SemanticDiffResult> = if semantic {
        #[cfg(not(feature = "semantic"))]
        {
            return Err(anyhow!(RecoveryAdvice::feature_unavailable(
                "semantic diff",
                "semantic"
            )));
        }
        #[cfg(feature = "semantic")]
        {
            let options = SemanticDiffOptions::default();

            if let Some(ref to_spec) = to {
                let to_id = resolve_state_id(&repo, to_spec)?;
                let to_state = require_resolved_state(&repo, &to_id)?;

                let from_hash = from_state
                    .as_ref()
                    .map(|s| s.tree)
                    .unwrap_or_else(|| Tree::new().hash());

                Some(semantic_diff(&repo, &from_hash, &to_state.tree, &options)?)
            } else {
                let from_hash = from_state
                    .as_ref()
                    .map(|s| s.tree)
                    .unwrap_or_else(|| Tree::new().hash());

                Some(semantic_diff_worktree(
                    &repo,
                    &from_hash,
                    &options,
                    &status_options,
                )?)
            }
        }
    } else {
        None
    };

    // For state-to-state diffs we need the `to` tree later (to fetch
    // "new" blob bytes for line-diff rendering); the worktree path
    // reads new bytes from disk instead and doesn't need this. Semantic
    // diff is additive: it should not suppress normal unified hunks.
    let mut to_tree: Option<Tree> = None;
    if let Some(ref to_spec) = to {
        let to_id = resolve_state_id(&repo, to_spec)?;
        let to_state = require_resolved_state(&repo, &to_id)?;
        to_tree = repo.store().get_tree(&to_state.tree)?;
    }
    let changes: FileChangeSet = if let Some(ref result) = semantic_result {
        result.file_changes.clone()
    } else if let Some(ref to_spec) = to {
        let to_id = resolve_state_id(&repo, to_spec)?;
        let to_state = require_resolved_state(&repo, &to_id)?;

        let from_hash = from_state
            .as_ref()
            .map(|s| s.tree)
            .unwrap_or_else(|| Tree::new().hash());

        repo.diff_trees(&from_hash, &to_state.tree)?
    } else if git_overlay_head_worktree_diff {
        let status = repo.git_overlay_worktree_status()?.unwrap_or_default();

        let mut changes = FileChangeSet::with_capacity(status.change_count());
        for path in status.modified {
            changes.push_modified(path.display().to_string());
        }
        for path in status.added {
            changes.push_added(path.display().to_string());
        }
        for path in status.deleted {
            changes.push_deleted(path.display().to_string());
        }
        changes
    } else {
        let tree = from_tree.clone().unwrap_or_default();
        let status = repo.compare_worktree_cached_with_options(&tree, &status_options)?;

        let mut changes = FileChangeSet::with_capacity(status.change_count());
        for path in status.modified {
            changes.push_modified(path.display().to_string());
        }
        for path in status.added {
            changes.push_added(path.display().to_string());
        }
        for path in status.deleted {
            changes.push_deleted(path.display().to_string());
        }
        changes
    };

    let file_changes: Vec<FileChange> = if name_only {
        changes
            .iter()
            .map(|change| FileChange {
                path: change.path.clone(),
                kind: change.kind.to_string(),
                old_path: None,
                binary: false,
                lines: None,
            })
            .collect()
    } else {
        changes
            .iter()
            .map(|change| {
                // Three diff modes — pick the right line-fetcher per mode:
                //   1. Semantic: skip text-line diffs entirely; the
                //      semantic_changes block carries the rendering.
                //   2. State-to-state (`to.is_some()`): both sides are
                //      stored blobs in the heddle object store. Use
                //      `get_state_diff`.
                //   3. Worktree (`to.is_none()`): "new" side is the live
                //      filesystem. Use `get_worktree_diff`.
                //
                // Pre-Phase-D bug: case 2 fell through to `lines = None`,
                // and `print_diff` rendered the catch-all
                // "Binary file or unable to diff" — even on plain text.
                let lines_result = if let Some(ref tree) = to_tree {
                    get_state_diff(&repo, from_tree.as_ref(), tree, &change.path, &change.kind)
                } else {
                    get_worktree_diff(&repo, from_tree.as_ref(), &change.path, &change.kind)
                };
                let binary = lines_result
                    .as_ref()
                    .err()
                    .is_some_and(is_binary_diff_error);
                let lines = lines_result.ok().map(|lines| unified_hunks(lines, unified));

                FileChange {
                    path: change.path.clone(),
                    kind: change.kind.to_string(),
                    old_path: None,
                    binary,
                    lines,
                }
            })
            .collect()
    };
    let file_changes = detect_clear_renames(
        &repo,
        from_tree.as_ref(),
        to_tree.as_ref(),
        file_changes,
        !(name_only || stat),
        unified,
    )?;

    let semantic_changes = semantic_result.map(|r| {
        r.changes
            .into_iter()
            .map(SemanticChangeEntry::from)
            .collect()
    });

    let context_state = if show_context {
        if let Some(ref to_spec) = to {
            let to_id = resolve_state_id(&repo, to_spec)?;
            Some(require_resolved_state(&repo, &to_id)?)
        } else if let Some(state) = from_state.clone() {
            Some(state)
        } else {
            repo.current_state()?
        }
    } else {
        None
    };

    let stats = DiffStats::from_changes(&file_changes, semantic_changes.as_deref());
    let file_changes = if stat {
        strip_line_hunks(file_changes)
    } else {
        file_changes
    };
    let output = DiffOutput::with_stats(
        from_id.map(|id| id.short()),
        to.clone(),
        file_changes,
        semantic_changes,
        context_state
            .as_ref()
            .map(|state| collect_file_context(&repo, state, &changes))
            .transpose()?,
        context_state
            .as_ref()
            .map(|state| collect_state_guidance(&repo, state))
            .transpose()?,
        stats,
    );

    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&output)?);
    } else if name_only {
        for change in &output.changes {
            println!("{}", change.path);
        }
    } else if stat {
        print_stat(&output);
    } else {
        if show_context {
            print_context(&output);
        }
        print_diff(&output);
        if let Some(ref semantic) = output.semantic_changes {
            print_semantic_changes(semantic);
        }
    }

    Ok(())
}

fn render_plain_git_head_diff(
    cli: &Cli,
    probe: &PlainGitVerificationProbe,
    stat: bool,
    name_only: bool,
) -> Result<()> {
    render_worktree_status_diff(cli, &probe.changes, stat, name_only, false)
}

fn render_worktree_status_diff(
    cli: &Cli,
    status: &objects::worktree::WorktreeStatus,
    stat: bool,
    name_only: bool,
    detect_renames: bool,
) -> Result<()> {
    let changes = status
        .modified
        .iter()
        .map(|path| FileChange {
            path: path.display().to_string(),
            kind: "modified".to_string(),
            old_path: None,
            binary: false,
            lines: None,
        })
        .chain(status.added.iter().map(|path| FileChange {
            path: path.display().to_string(),
            kind: "added".to_string(),
            old_path: None,
            binary: false,
            lines: None,
        }))
        .chain(status.deleted.iter().map(|path| FileChange {
            path: path.display().to_string(),
            kind: "deleted".to_string(),
            old_path: None,
            binary: false,
            lines: None,
        }))
        .collect::<Vec<_>>();
    let changes = if detect_renames {
        detect_clear_renames_for_worktree_status(cli, changes)?
    } else {
        changes
    };
    let output = DiffOutput::new(Some("HEAD".to_string()), None, changes, None, None, None);

    if should_output_json(cli, None) {
        println!("{}", serde_json::to_string(&output)?);
    } else if name_only {
        for change in &output.changes {
            println!("{}", change.path);
        }
    } else if stat {
        print_stat(&output);
    } else {
        print_diff(&output);
    }
    Ok(())
}

/// Compute a state-to-state diff payload without printing.
///
/// Reuses the same line-rendering pipeline as `cmd_diff`'s state-to-state
/// path: object-store lookups for both sides, `diff_blobs` for modified
/// files, hunk grouping via `unified_hunks`. The result is the same
/// `DiffOutput` shape that `cmd_diff` serializes, so callers can embed
/// it inside their own JSON payload.
///
/// Used by `heddle merge --with-diff` to surface the diff that would
/// land (or just landed) without a separate `heddle diff` invocation.
///
/// `semantic` requests the semantic change list in addition to the
/// line-level hunks. Building with `--features semantic` is required;
/// otherwise this errors out the same way `cmd_diff --semantic` does.
pub fn compute_state_diff(
    repo: &Repository,
    from_change_id: &ChangeId,
    to_change_id: &ChangeId,
    semantic: bool,
    unified: usize,
) -> Result<DiffOutput> {
    let from_state = repo.store().get_state(from_change_id)?;
    let from_tree = if let Some(ref state) = from_state {
        repo.store().get_tree(&state.tree)?
    } else {
        None
    };

    let to_state = require_resolved_state(repo, to_change_id)?;
    let to_tree = repo
        .store()
        .get_tree(&to_state.tree)?
        .ok_or_else(|| anyhow!("Tree not found for state {}", to_change_id.short()))?;

    let from_hash = from_state
        .as_ref()
        .map(|s| s.tree)
        .unwrap_or_else(|| Tree::new().hash());

    let semantic_result: Option<SemanticDiffResult> = if semantic {
        #[cfg(not(feature = "semantic"))]
        {
            return Err(anyhow!(RecoveryAdvice::feature_unavailable(
                "semantic diff",
                "semantic"
            )));
        }
        #[cfg(feature = "semantic")]
        {
            let options = SemanticDiffOptions::default();
            Some(semantic_diff(repo, &from_hash, &to_state.tree, &options)?)
        }
    } else {
        None
    };

    let changes: FileChangeSet = if let Some(ref result) = semantic_result {
        result.file_changes.clone()
    } else {
        repo.diff_trees(&from_hash, &to_state.tree)?
    };

    let file_changes: Vec<FileChange> = changes
        .iter()
        .map(|change| {
            let lines_result = get_state_diff(
                repo,
                from_tree.as_ref(),
                &to_tree,
                &change.path,
                &change.kind,
            );
            let binary = lines_result
                .as_ref()
                .err()
                .is_some_and(is_binary_diff_error);
            let lines = lines_result.ok().map(|lines| unified_hunks(lines, unified));
            FileChange {
                path: change.path.clone(),
                kind: change.kind.to_string(),
                old_path: None,
                binary,
                lines,
            }
        })
        .collect();
    let file_changes = detect_clear_renames(
        repo,
        from_tree.as_ref(),
        Some(&to_tree),
        file_changes,
        true,
        unified,
    )?;

    let semantic_changes = semantic_result.map(|r| {
        r.changes
            .into_iter()
            .map(SemanticChangeEntry::from)
            .collect()
    });

    Ok(DiffOutput::new(
        Some(from_change_id.short()),
        Some(to_change_id.short()),
        file_changes,
        semantic_changes,
        None,
        None,
    ))
}

/// Compute a diff from an existing state to an in-memory tree.
///
/// Merge preview uses this for clean 3-way previews: the tree that would
/// land has been computed, but no state has been committed yet. The top
/// tree is installed in the object store so the existing semantic and
/// rename-aware diff pipeline can address it by hash.
pub fn compute_tree_diff(
    repo: &Repository,
    from_change_id: &ChangeId,
    to_tree: &Tree,
    to_label: impl Into<String>,
    semantic: bool,
    unified: usize,
) -> Result<DiffOutput> {
    let from_state = repo.store().get_state(from_change_id)?;
    let from_tree = if let Some(ref state) = from_state {
        repo.store().get_tree(&state.tree)?
    } else {
        None
    };
    let from_hash = from_state
        .as_ref()
        .map(|s| s.tree)
        .unwrap_or_else(|| Tree::new().hash());

    let to_hash = repo.store().put_tree(to_tree)?;

    let semantic_result: Option<SemanticDiffResult> = if semantic {
        #[cfg(not(feature = "semantic"))]
        {
            return Err(anyhow!(RecoveryAdvice::feature_unavailable(
                "semantic diff",
                "semantic"
            )));
        }
        #[cfg(feature = "semantic")]
        {
            let options = SemanticDiffOptions::default();
            Some(semantic_diff(repo, &from_hash, &to_hash, &options)?)
        }
    } else {
        None
    };

    let changes: FileChangeSet = if let Some(ref result) = semantic_result {
        result.file_changes.clone()
    } else {
        repo.diff_trees(&from_hash, &to_hash)?
    };

    let file_changes: Vec<FileChange> = changes
        .iter()
        .map(|change| {
            let lines_result = get_state_diff(
                repo,
                from_tree.as_ref(),
                to_tree,
                &change.path,
                &change.kind,
            );
            let binary = lines_result
                .as_ref()
                .err()
                .is_some_and(is_binary_diff_error);
            let lines = lines_result.ok().map(|lines| unified_hunks(lines, unified));
            FileChange {
                path: change.path.clone(),
                kind: change.kind.to_string(),
                old_path: None,
                binary,
                lines,
            }
        })
        .collect();
    let file_changes = detect_clear_renames(
        repo,
        from_tree.as_ref(),
        Some(to_tree),
        file_changes,
        true,
        unified,
    )?;

    let semantic_changes = semantic_result.map(|r| {
        r.changes
            .into_iter()
            .map(SemanticChangeEntry::from)
            .collect()
    });

    Ok(DiffOutput::new(
        Some(from_change_id.short()),
        Some(to_label.into()),
        file_changes,
        semantic_changes,
        None,
        None,
    ))
}

fn strip_line_hunks(changes: Vec<FileChange>) -> Vec<FileChange> {
    changes
        .into_iter()
        .map(|mut change| {
            change.lines = None;
            change
        })
        .collect()
}

fn unified_hunks(lines: Vec<LineDiff>, context: usize) -> Vec<LineDiff> {
    if lines.is_empty() || !lines.iter().any(|line| line.prefix != " ") {
        return lines;
    }

    let mut ranges = Vec::<(usize, usize)>::new();
    let mut cursor = 0usize;
    while cursor < lines.len() {
        while cursor < lines.len() && lines[cursor].prefix == " " {
            cursor += 1;
        }
        if cursor >= lines.len() {
            break;
        }

        let start = cursor.saturating_sub(context);
        while cursor < lines.len() && lines[cursor].prefix != " " {
            cursor += 1;
        }
        let mut end = (cursor + context).min(lines.len());

        while cursor < lines.len() && lines[cursor].prefix == " " && cursor < end {
            cursor += 1;
        }
        while cursor < lines.len() && lines[cursor].prefix != " " {
            end = (cursor + 1 + context).min(lines.len());
            cursor += 1;
        }

        if let Some((_, previous_end)) = ranges.last_mut()
            && start <= *previous_end
        {
            *previous_end = end;
            continue;
        }
        ranges.push((start, end));
    }

    let mut output = Vec::new();
    for (start, end) in ranges {
        let (old_start, old_len, new_start, new_len) = hunk_span(&lines, start, end);
        output.push(LineDiff {
            prefix: "@".to_string(),
            content: format!("@ -{},{} +{},{} @@", old_start, old_len, new_start, new_len),
            old_line: None,
            new_line: None,
        });
        output.extend(trim_trailing_added_decorations(&lines[start..end]));
    }
    output
}

fn trim_trailing_added_decorations(lines: &[LineDiff]) -> Vec<LineDiff> {
    let mut trimmed = Vec::with_capacity(lines.len());
    let mut index = 0usize;
    while index < lines.len() {
        if lines[index].prefix == "+"
            && is_visual_decoration_line(&lines[index].content)
            && let Some(next_context) = next_context_line(lines, index + 1)
            && next_context.content == lines[index].content
        {
            let added_block_has_code = lines[index + 1..next_context.index]
                .iter()
                .any(|line| line.prefix == "+" && !is_blank_or_visual_decoration(&line.content));
            if added_block_has_code {
                index += 1;
                continue;
            }
        }
        trimmed.push(lines[index].clone());
        index += 1;
    }
    trimmed
}

struct IndexedLine<'a> {
    index: usize,
    content: &'a str,
}

fn next_context_line(lines: &[LineDiff], start: usize) -> Option<IndexedLine<'_>> {
    lines[start..]
        .iter()
        .enumerate()
        .find(|(_, line)| line.prefix == " ")
        .map(|(offset, line)| IndexedLine {
            index: start + offset,
            content: &line.content,
        })
}

fn is_blank_or_visual_decoration(line: &str) -> bool {
    line.trim().is_empty() || is_visual_decoration_line(line)
}

fn is_visual_decoration_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("#[")
        || trimmed.starts_with("#![")
        || trimmed.starts_with('@')
        || trimmed.starts_with("///")
        || trimmed.starts_with("//!")
}

fn hunk_span(lines: &[LineDiff], start: usize, end: usize) -> (usize, usize, usize, usize) {
    let old_before = lines[..start]
        .iter()
        .filter(|line| line.prefix != "+")
        .count();
    let new_before = lines[..start]
        .iter()
        .filter(|line| line.prefix != "-")
        .count();
    let old_len = lines[start..end]
        .iter()
        .filter(|line| line.prefix != "+")
        .count();
    let new_len = lines[start..end]
        .iter()
        .filter(|line| line.prefix != "-")
        .count();

    let old_start = if old_len == 0 {
        old_before
    } else {
        old_before + 1
    };
    let new_start = if new_len == 0 {
        new_before
    } else {
        new_before + 1
    };
    (old_start, old_len, new_start, new_len)
}

fn collect_file_context(
    repo: &Repository,
    state: &State,
    changes: &FileChangeSet,
) -> Result<Vec<FileContextEntry>> {
    let Some(context_root) = &state.context else {
        return Ok(Vec::new());
    };

    let mut entries = Vec::new();
    for change in changes {
        let target = ContextTarget::file(change.path.clone())?;
        let Some(blob) = repo.get_context_blob(context_root, &target)? else {
            continue;
        };
        let annotations = blob
            .annotations
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
            .collect::<Vec<_>>();
        if !annotations.is_empty() {
            entries.push(FileContextEntry {
                path: change.path.clone(),
                annotations,
            });
        }
    }
    Ok(entries)
}

fn collect_state_guidance(repo: &Repository, state: &State) -> Result<Vec<ContextSnippet>> {
    let Some(context_root) = &state.context else {
        return Ok(Vec::new());
    };
    let target = ContextTarget::state(state.change_id);
    let Some(blob) = repo.get_context_blob(context_root, &target)? else {
        return Ok(Vec::new());
    };
    Ok(blob
        .annotations
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
        .collect())
}

fn summarize_context(content: &str) -> String {
    let first_line = content
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    if first_line.len() <= 88 {
        first_line.to_string()
    } else {
        format!("{}...", &first_line[..85])
    }
}

fn get_worktree_diff(
    repo: &Repository,
    from_tree: Option<&Tree>,
    path: &str,
    kind: &DiffKind,
) -> Result<Vec<LineDiff>> {
    let worktree_path = repo.root().join(path);

    match kind {
        DiffKind::Added => {
            let new_blob = read_worktree_blob_for_diff(&worktree_path)?;
            Ok(number_lines(blob_lines(&new_blob, "+")?))
        }
        DiffKind::Deleted => {
            if let Some(tree) = from_tree
                && let Some(entry) = tree.get(path)
            {
                let blob = repo.require_blob(&entry.hash)?;
                return Ok(number_lines(blob_lines(&blob, "-")?));
            }
            Ok(vec![])
        }
        DiffKind::Modified => {
            let new_blob = read_worktree_blob_for_diff(&worktree_path)?;

            if let Some(tree) = from_tree
                && let Some(old_blob) = find_blob_in_tree(repo, tree, path)?
            {
                ensure_text_diffable(&old_blob)?;
                ensure_text_diffable(&new_blob)?;
                let diff = diff_blobs(&old_blob, &new_blob);
                let lines = diff
                    .iter()
                    .map(|l| LineDiff::new(l.prefix(), l.content()))
                    .collect();
                return Ok(number_lines(lines));
            }

            Ok(number_lines(blob_lines(&new_blob, "+")?))
        }
        DiffKind::Unchanged => Ok(Vec::new()),
    }
}

fn read_worktree_blob_for_diff(path: &std::path::Path) -> Result<Blob> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        let target = std::fs::read_link(path)?;
        return Ok(Blob::new(target.to_string_lossy().as_bytes().to_vec()));
    }
    Ok(Blob::new(std::fs::read(path)?))
}

fn detect_clear_renames_for_worktree_status(
    cli: &Cli,
    changes: Vec<FileChange>,
) -> Result<Vec<FileChange>> {
    let cwd = std::env::current_dir()?;
    let start = cli.repo.as_ref().unwrap_or(&cwd);
    let Ok(repo) = Repository::open(start) else {
        return Ok(changes);
    };
    let from_tree = if let Some(id) = repo.head()? {
        repo.store()
            .get_state(&id)?
            .and_then(|state| repo.store().get_tree(&state.tree).transpose())
            .transpose()?
    } else {
        None
    };
    detect_clear_renames(&repo, from_tree.as_ref(), None, changes, false, 3)
}

fn detect_clear_renames(
    repo: &Repository,
    from_tree: Option<&Tree>,
    to_tree: Option<&Tree>,
    changes: Vec<FileChange>,
    include_lines: bool,
    unified: usize,
) -> Result<Vec<FileChange>> {
    let deleted = changes
        .iter()
        .filter(|change| change.kind == "deleted")
        .map(|change| change.path.as_str())
        .collect::<Vec<_>>();
    let added = changes
        .iter()
        .filter(|change| change.kind == "added")
        .map(|change| change.path.as_str())
        .collect::<Vec<_>>();
    if deleted.is_empty() || added.is_empty() {
        return Ok(changes);
    }

    let mut candidates = Vec::new();
    for old_path in &deleted {
        let Some(old_blob) = blob_from_tree(repo, from_tree, old_path)? else {
            continue;
        };
        for new_path in &added {
            let Some(new_blob) = new_blob_for_rename(repo, to_tree, new_path)? else {
                continue;
            };
            let score = rename_similarity(&old_blob, &new_blob);
            if score >= 0.75 {
                candidates.push((score, (*old_path).to_string(), (*new_path).to_string()));
            }
        }
    }

    candidates.sort_by(|left, right| {
        right
            .0
            .total_cmp(&left.0)
            .then_with(|| left.1.cmp(&right.1))
            .then_with(|| left.2.cmp(&right.2))
    });

    let mut used_old = BTreeSet::new();
    let mut used_new = BTreeSet::new();
    let mut renames = Vec::new();
    for (_, old_path, new_path) in candidates {
        if used_old.insert(old_path.clone()) && used_new.insert(new_path.clone()) {
            renames.push((old_path, new_path));
        }
    }
    if renames.is_empty() {
        return Ok(changes);
    }

    let rename_by_new = renames
        .iter()
        .map(|(old_path, new_path)| (new_path.as_str(), old_path.as_str()))
        .collect::<std::collections::BTreeMap<_, _>>();
    let removed_old = renames
        .iter()
        .map(|(old_path, _)| old_path.as_str())
        .collect::<BTreeSet<_>>();

    let mut output = Vec::with_capacity(changes.len() - renames.len());
    for mut change in changes {
        if change.kind == "deleted" && removed_old.contains(change.path.as_str()) {
            continue;
        }
        if change.kind == "added"
            && let Some(old_path) = rename_by_new.get(change.path.as_str())
        {
            let lines = if include_lines {
                match rename_lines(repo, from_tree, to_tree, old_path, &change.path, unified) {
                    Ok(lines) => lines,
                    Err(error) if is_binary_diff_error(&error) => {
                        change.binary = true;
                        None
                    }
                    Err(error) => return Err(error),
                }
            } else {
                None
            };
            change.kind = "renamed".to_string();
            change.old_path = Some((*old_path).to_string());
            change.lines = lines;
        }
        output.push(change);
    }
    Ok(output)
}

fn rename_lines(
    repo: &Repository,
    from_tree: Option<&Tree>,
    to_tree: Option<&Tree>,
    old_path: &str,
    new_path: &str,
    unified: usize,
) -> Result<Option<Vec<LineDiff>>> {
    let Some(old_blob) = blob_from_tree(repo, from_tree, old_path)? else {
        return Ok(None);
    };
    let Some(new_blob) = new_blob_for_rename(repo, to_tree, new_path)? else {
        return Ok(None);
    };
    ensure_text_diffable(&old_blob)?;
    ensure_text_diffable(&new_blob)?;
    let diff = diff_blobs(&old_blob, &new_blob);
    let lines = diff
        .iter()
        .map(|line| LineDiff::new(line.prefix(), line.content()))
        .collect();
    Ok(Some(unified_hunks(number_lines(lines), unified)))
}

fn blob_from_tree(repo: &Repository, tree: Option<&Tree>, path: &str) -> Result<Option<Blob>> {
    let Some(tree) = tree else {
        return Ok(None);
    };
    find_blob_in_tree(repo, tree, path)
}

fn new_blob_for_rename(
    repo: &Repository,
    to_tree: Option<&Tree>,
    path: &str,
) -> Result<Option<Blob>> {
    if let Some(tree) = to_tree {
        return find_blob_in_tree(repo, tree, path);
    }

    let worktree_path = repo.root().join(path);
    match std::fs::read(worktree_path) {
        Ok(content) => Ok(Some(Blob::new(content))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn rename_similarity(old_blob: &Blob, new_blob: &Blob) -> f64 {
    if old_blob.content() == new_blob.content() {
        return 1.0;
    }
    let (Some(old_text), Some(new_text)) = (old_blob.content_str(), new_blob.content_str()) else {
        return 0.0;
    };
    if old_text.chars().any(is_terminal_hostile_control)
        || new_text.chars().any(is_terminal_hostile_control)
    {
        return 0.0;
    }
    let old_lines = old_text.lines().collect::<Vec<_>>();
    let new_lines = new_text.lines().collect::<Vec<_>>();
    if old_lines.is_empty() || new_lines.is_empty() {
        return 0.0;
    }
    let shared = lcs_len(&old_lines, &new_lines);
    (shared * 2) as f64 / (old_lines.len() + new_lines.len()) as f64
}

fn lcs_len(left: &[&str], right: &[&str]) -> usize {
    let mut previous = vec![0usize; right.len() + 1];
    let mut current = vec![0usize; right.len() + 1];
    for left_line in left {
        for (index, right_line) in right.iter().enumerate() {
            current[index + 1] = if left_line == right_line {
                previous[index] + 1
            } else {
                previous[index + 1].max(current[index])
            };
        }
        std::mem::swap(&mut previous, &mut current);
        current.fill(0);
    }
    previous[right.len()]
}

/// Render line-level diff for a path between two stored states.
///
/// Sister of `get_worktree_diff`, but every blob is loaded from the
/// heddle object store via `find_blob_in_tree` rather than from the
/// live filesystem — which is why this can run from anywhere (not just
/// the current worktree) and why it Just Works for `heddle diff
/// <thread-a> <thread-b>`.
///
/// Returns the same `Vec<LineDiff>` shape `print_diff` already knows
/// how to render, so the only renderer change for state-to-state diffs
/// is "stop falling through to the binary-file catch-all."
fn get_state_diff(
    repo: &Repository,
    from_tree: Option<&Tree>,
    to_tree: &Tree,
    path: &str,
    kind: &DiffKind,
) -> Result<Vec<LineDiff>> {
    match kind {
        DiffKind::Added => {
            let Some(new_blob) = find_blob_in_tree(repo, to_tree, path)? else {
                return Ok(Vec::new());
            };
            Ok(number_lines(blob_lines(&new_blob, "+")?))
        }
        DiffKind::Deleted => {
            let Some(tree) = from_tree else {
                return Ok(Vec::new());
            };
            let Some(old_blob) = find_blob_in_tree(repo, tree, path)? else {
                return Ok(Vec::new());
            };
            Ok(number_lines(blob_lines(&old_blob, "-")?))
        }
        DiffKind::Modified => {
            let Some(new_blob) = find_blob_in_tree(repo, to_tree, path)? else {
                return Ok(Vec::new());
            };
            if let Some(tree) = from_tree
                && let Some(old_blob) = find_blob_in_tree(repo, tree, path)?
            {
                ensure_text_diffable(&old_blob)?;
                ensure_text_diffable(&new_blob)?;
                let diff = diff_blobs(&old_blob, &new_blob);
                let lines = diff
                    .iter()
                    .map(|l| LineDiff::new(l.prefix(), l.content()))
                    .collect();
                return Ok(number_lines(lines));
            }
            // No corresponding blob in `from_tree` — render as all-new.
            Ok(number_lines(blob_lines(&new_blob, "+")?))
        }
        DiffKind::Unchanged => Ok(Vec::new()),
    }
}

fn blob_lines(blob: &Blob, prefix: &str) -> Result<Vec<LineDiff>> {
    let text = text_diff_content(blob)?;
    Ok(text
        .lines()
        .map(|line| LineDiff::new(prefix, line))
        .collect())
}

fn ensure_text_diffable(blob: &Blob) -> Result<()> {
    text_diff_content(blob).map(|_| ())
}

fn text_diff_content(blob: &Blob) -> Result<&str> {
    let Some(text) = blob.content_str() else {
        return Err(anyhow!(BINARY_DIFF_ERROR));
    };
    if text.chars().any(is_terminal_hostile_control) {
        return Err(anyhow!(BINARY_DIFF_ERROR));
    }
    Ok(text)
}

fn is_binary_diff_error(error: &anyhow::Error) -> bool {
    error.to_string() == BINARY_DIFF_ERROR
}

fn is_terminal_hostile_control(ch: char) -> bool {
    ch.is_control() && ch != '\n' && ch != '\t'
}

fn number_lines(lines: Vec<LineDiff>) -> Vec<LineDiff> {
    let mut old_line = 1usize;
    let mut new_line = 1usize;

    lines
        .into_iter()
        .map(|line| {
            let old = if line.prefix != "+" {
                let current = Some(old_line);
                old_line += 1;
                current
            } else {
                None
            };
            let new = if line.prefix != "-" {
                let current = Some(new_line);
                new_line += 1;
                current
            } else {
                None
            };
            LineDiff::with_lines(line.prefix, line.content, old, new)
        })
        .collect()
}

fn find_blob_in_tree(repo: &Repository, tree: &Tree, path: &str) -> Result<Option<Blob>> {
    let parts: Vec<&str> = path.split('/').collect();
    find_blob_recursive(repo, tree, &parts)
}

fn find_blob_recursive(repo: &Repository, tree: &Tree, parts: &[&str]) -> Result<Option<Blob>> {
    if parts.is_empty() {
        return Ok(None);
    }

    let name = parts[0];
    let entry = match tree.get(name) {
        Some(e) => e,
        None => return Ok(None),
    };

    if parts.len() == 1 {
        if entry.is_blob() || entry.entry_type == objects::object::EntryType::Symlink {
            return Ok(Some(repo.require_blob(&entry.hash)?));
        }
    } else if entry.is_tree()
        && let Some(subtree) = repo.store().get_tree(&entry.hash)?
    {
        return find_blob_recursive(repo, &subtree, &parts[1..]);
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::unified_hunks;
    use crate::cli::commands::diff::diff_types::LineDiff;

    #[test]
    fn unified_hunks_keeps_context_decoration_when_added_block_ends_before_matching_item() {
        let lines = vec![
            LineDiff::with_lines("+", "#[test]", None, Some(1)),
            LineDiff::with_lines("+", "fn added() {}", None, Some(2)),
            LineDiff::with_lines(" ", "#[test]", Some(1), Some(3)),
            LineDiff::with_lines(" ", "fn existing() {}", Some(2), Some(4)),
        ];

        let hunk = unified_hunks(lines, 3);

        assert!(
            hunk.iter()
                .filter(|line| line.content == "#[test]")
                .all(|line| line.prefix == " "),
            "existing context attribute should own the decoration: {hunk:?}"
        );
        assert!(
            hunk.iter()
                .any(|line| line.prefix == "+" && line.content == "fn added() {}"),
            "added function body should remain: {hunk:?}"
        );
    }
}
