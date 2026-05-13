// SPDX-License-Identifier: Apache-2.0
//! Core diff command logic.

use anyhow::{Result, anyhow};
use objects::{
    object::{
        AnnotationStatus, Blob, ChangeId, ContextTarget, DiffKind, FileChangeSet, State, Tree,
    },
    worktree::diff_blobs,
};
use repo::Repository;

use super::{
    super::history_target::resolve_state_id,
    diff_output::{print_context, print_diff, print_semantic_changes, print_stat},
    diff_types::{
        ContextSnippet, DiffOutput, FileChange, FileContextEntry, LineDiff, SemanticChangeEntry,
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
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
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
        repo.store().get_state(&id)?
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
            anyhow::bail!("semantic diff requires building heddle with --features semantic");
        }
        #[cfg(feature = "semantic")]
        {
            let options = SemanticDiffOptions::default();

            if let Some(ref to_spec) = to {
                let to_id = resolve_state_id(&repo, to_spec)?;
                let to_state = repo
                    .store()
                    .get_state(&to_id)?
                    .ok_or_else(|| anyhow!("State not found: {}", to_spec))?;

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
        let to_state = repo
            .store()
            .get_state(&to_id)?
            .ok_or_else(|| anyhow!("State not found: {}", to_spec))?;
        to_tree = repo.store().get_tree(&to_state.tree)?;
    }
    let changes: FileChangeSet = if let Some(ref result) = semantic_result {
        result.file_changes.clone()
    } else if let Some(ref to_spec) = to {
        let to_id = resolve_state_id(&repo, to_spec)?;
        let to_state = repo
            .store()
            .get_state(&to_id)?
            .ok_or_else(|| anyhow!("State not found: {}", to_spec))?;

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

    let file_changes: Vec<FileChange> = if name_only || stat {
        changes
            .iter()
            .map(|change| FileChange {
                path: change.path.clone(),
                kind: change.kind.to_string(),
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
                let lines = if let Some(ref tree) = to_tree {
                    get_state_diff(&repo, from_tree.as_ref(), tree, &change.path, &change.kind).ok()
                } else {
                    get_worktree_diff(&repo, from_tree.as_ref(), &change.path, &change.kind).ok()
                };
                let lines = lines.map(|lines| unified_hunks(lines, unified));

                FileChange {
                    path: change.path.clone(),
                    kind: change.kind.to_string(),
                    lines,
                }
            })
            .collect()
    };

    let semantic_changes = semantic_result.map(|r| {
        r.changes
            .into_iter()
            .map(SemanticChangeEntry::from)
            .collect()
    });

    let context_state = if show_context {
        if let Some(ref to_spec) = to {
            let to_id = resolve_state_id(&repo, to_spec)?;
            repo.store().get_state(&to_id)?
        } else if let Some(state) = from_state.clone() {
            Some(state)
        } else {
            repo.current_state()?
        }
    } else {
        None
    };

    let output = DiffOutput {
        from_state: from_id.map(|id| id.short()),
        to_state: to.clone(),
        changes: file_changes,
        semantic_changes,
        context: context_state
            .as_ref()
            .map(|state| collect_file_context(&repo, state, &changes))
            .transpose()?,
        broader_guidance: context_state
            .as_ref()
            .map(|state| collect_state_guidance(&repo, state))
            .transpose()?,
    };

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

    let to_state = repo
        .store()
        .get_state(to_change_id)?
        .ok_or_else(|| anyhow!("State not found: {}", to_change_id.short()))?;
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
            anyhow::bail!("semantic diff requires building heddle with --features semantic");
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
            let lines = get_state_diff(
                repo,
                from_tree.as_ref(),
                &to_tree,
                &change.path,
                &change.kind,
            )
            .ok();
            let lines = lines.map(|lines| unified_hunks(lines, unified));
            FileChange {
                path: change.path.clone(),
                kind: change.kind.to_string(),
                lines,
            }
        })
        .collect();

    let semantic_changes = semantic_result.map(|r| {
        r.changes
            .into_iter()
            .map(SemanticChangeEntry::from)
            .collect()
    });

    Ok(DiffOutput {
        from_state: Some(from_change_id.short()),
        to_state: Some(to_change_id.short()),
        changes: file_changes,
        semantic_changes,
        context: None,
        broader_guidance: None,
    })
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
            let content = std::fs::read(&worktree_path)?;
            let new_blob = Blob::new(content);
            let lines = new_blob
                .content_str()
                .map(|s| s.lines().map(|l| LineDiff::new("+", l)).collect())
                .unwrap_or_default();
            Ok(number_lines(lines))
        }
        DiffKind::Deleted => {
            if let Some(tree) = from_tree
                && let Some(entry) = tree.get(path)
            {
                let blob = repo.require_blob(&entry.hash)?;
                let lines = blob
                    .content_str()
                    .map(|s| s.lines().map(|l| LineDiff::new("-", l)).collect())
                    .unwrap_or_default();
                return Ok(number_lines(lines));
            }
            Ok(vec![])
        }
        DiffKind::Modified => {
            let content = std::fs::read(&worktree_path)?;
            let new_blob = Blob::new(content);

            if let Some(tree) = from_tree
                && let Some(old_blob) = find_blob_in_tree(repo, tree, path)?
            {
                let diff = diff_blobs(&old_blob, &new_blob);
                let lines = diff
                    .iter()
                    .map(|l| LineDiff::new(l.prefix(), l.content()))
                    .collect();
                return Ok(number_lines(lines));
            }

            let lines = new_blob
                .content_str()
                .map(|s| s.lines().map(|l| LineDiff::new("+", l)).collect())
                .unwrap_or_default();
            Ok(number_lines(lines))
        }
        DiffKind::Unchanged => Ok(Vec::new()),
    }
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
            Ok(new_blob
                .content_str()
                .map(|s| s.lines().map(|l| LineDiff::new("+", l)).collect())
                .map(number_lines)
                .unwrap_or_default())
        }
        DiffKind::Deleted => {
            let Some(tree) = from_tree else {
                return Ok(Vec::new());
            };
            let Some(old_blob) = find_blob_in_tree(repo, tree, path)? else {
                return Ok(Vec::new());
            };
            Ok(old_blob
                .content_str()
                .map(|s| s.lines().map(|l| LineDiff::new("-", l)).collect())
                .map(number_lines)
                .unwrap_or_default())
        }
        DiffKind::Modified => {
            let Some(new_blob) = find_blob_in_tree(repo, to_tree, path)? else {
                return Ok(Vec::new());
            };
            if let Some(tree) = from_tree
                && let Some(old_blob) = find_blob_in_tree(repo, tree, path)?
            {
                let diff = diff_blobs(&old_blob, &new_blob);
                let lines = diff
                    .iter()
                    .map(|l| LineDiff::new(l.prefix(), l.content()))
                    .collect();
                return Ok(number_lines(lines));
            }
            // No corresponding blob in `from_tree` — render as all-new.
            Ok(new_blob
                .content_str()
                .map(|s| s.lines().map(|l| LineDiff::new("+", l)).collect())
                .map(number_lines)
                .unwrap_or_default())
        }
        DiffKind::Unchanged => Ok(Vec::new()),
    }
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
        if entry.is_blob() {
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