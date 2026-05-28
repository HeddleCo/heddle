// SPDX-License-Identifier: Apache-2.0
//! Core diff command logic.

use std::{collections::BTreeSet, path::Path};

use anyhow::{Result, anyhow};
use objects::{
    object::{
        AnnotationStatus, Blob, ChangeId, ContextTarget, DiffKind, EntryType, FileChangeSet,
        FileMode, State, Tree, TreeEntry,
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
            build_repository_verification_state, plain_git_setup_advice,
            trust_visible_worktree_status,
        },
        history_target::{require_resolved_state, resolve_state_id},
    },
    diff_output::{
        print_context, print_diff, print_diff_patch, print_semantic_changes, print_stat,
        render_diff_patch,
    },
    diff_types::{
        ContextSnippet, DiffOutput, DiffStats, FileChange, FileContextEntry, FileEolState,
        LineDiff, SemanticChangeEntry, change_line_counts,
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
    patch: bool,
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
        if probe.changes.is_clean() {
            return Err(anyhow!(plain_git_setup_advice(&probe, "diff", None)));
        }
        return render_plain_git_head_diff(cli, &probe, stat, name_only, patch, unified);
    }

    let repo = Repository::open(start)?;
    let trust = build_repository_verification_state(&repo);
    if to.is_none()
        && from_is_head_or_default
        && let Some(status) = trust_visible_worktree_status(&repo, &trust)?
    {
        return render_worktree_status_diff(
            cli,
            &status,
            stat,
            name_only,
            true,
            patch,
            unified,
            Some(&repo),
        );
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
                ..Default::default()
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
                let diff_result = if let Some(ref tree) = to_tree {
                    get_state_diff(&repo, from_tree.as_ref(), tree, &change.path, &change.kind)
                } else {
                    get_worktree_diff(&repo, from_tree.as_ref(), &change.path, &change.kind)
                };
                let binary = diff_result
                    .as_ref()
                    .err()
                    .is_some_and(is_binary_diff_error);
                let (raw_lines, eol) = match diff_result {
                    Ok((lines, eol)) => (Some(lines), eol),
                    Err(_) => (None, FileEolState::default()),
                };
                // `--stat` only needs the per-file tally; the unified
                // hunks would be allocated only for `strip_line_hunks`
                // to throw them away. Count once and drop the vector
                // immediately so a 10MB diff costs ~24 bytes/file in
                // retained memory instead of Vec<LineDiff>-per-file.
                let (lines, line_counts) = if stat {
                    let counts = change_line_counts(raw_lines.as_deref());
                    (None, Some(counts))
                } else {
                    (
                        raw_lines.map(|lines| unified_hunks(lines, unified, &eol)),
                        None,
                    )
                };

                let kind = change.kind.to_string();
                let mode = change_file_mode(
                    &repo,
                    from_tree.as_ref(),
                    to_tree.as_ref(),
                    &change.path,
                    &kind,
                );
                FileChange {
                    path: change.path.clone(),
                    kind,
                    binary,
                    lines,
                    line_counts,
                    eol,
                    mode,
                    ..Default::default()
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
    let mut output = DiffOutput::with_stats(
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
    populate_patch_text(&mut output);

    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&output)?);
    } else if name_only {
        for change in &output.changes {
            println!("{}", change.path);
        }
    } else if stat {
        print_stat(&output);
    } else if patch {
        print_diff_patch(&output);
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

/// Render and stash the standard unified-diff text on the output payload
/// when there is any line-level data to render. JSON consumers always
/// need a patch-compatible field; the rest of the print path consults
/// the same renderer.
fn populate_patch_text(output: &mut DiffOutput) {
    if !output.changes.iter().any(|change| change.lines.is_some()) {
        return;
    }
    let text = render_diff_patch(output);
    if !text.is_empty() {
        output.patch = Some(text);
    }
}

fn render_plain_git_head_diff(
    cli: &Cli,
    probe: &PlainGitVerificationProbe,
    stat: bool,
    name_only: bool,
    patch: bool,
    unified: usize,
) -> Result<()> {
    // The plain-Git fast path has no heddle Repository, so there is
    // no in-tree blob source `get_worktree_diff` can read from. When
    // `--patch` is requested we read the HEAD blobs through `gix`
    // and feed them through the same `diff_blobs` + renderer pipeline
    // the heddle paths use — that way the `\ No newline at end of
    // file` handling stays in one place.
    //
    // JSON output carries a `.patch` field whenever a repo is available,
    // regardless of the `--patch` flag, so we inflate hunks for JSON too.
    let json = should_output_json(cli, None);
    if (patch || json) && !stat && !name_only {
        let changes = plain_git_file_changes_with_hunks(probe, unified)?;
        return render_status_changes(cli, changes, stat, name_only, patch);
    }
    render_worktree_status_diff(
        cli,
        &probe.changes,
        stat,
        name_only,
        false,
        patch,
        unified,
        None,
    )
}

fn render_status_changes(
    cli: &Cli,
    changes: Vec<FileChange>,
    stat: bool,
    name_only: bool,
    patch: bool,
) -> Result<()> {
    let mut output = DiffOutput::new(Some("HEAD".to_string()), None, changes, None, None, None);
    populate_patch_text(&mut output);

    if should_output_json(cli, None) {
        println!("{}", serde_json::to_string(&output)?);
    } else if name_only {
        for change in &output.changes {
            println!("{}", change.path);
        }
    } else if stat {
        print_stat(&output);
    } else if patch {
        print_diff_patch(&output);
    } else {
        print_diff(&output);
    }
    Ok(())
}

/// Build one `FileChange` per status entry in the plain-Git probe,
/// computing real hunks against the gix-read HEAD blobs so `--patch`
/// emits a body the regular renderer can stamp newline markers onto.
///
/// Unborn HEAD (plain `git init` + staged file, no commit yet) has
/// no tree to read; in that case we pass `None` and the add-only path
/// in `compute_plain_git_hunks` renders against `/dev/null`. Without
/// this check, `head_tree()?` propagates a "no HEAD commit" error and
/// the whole `--patch` render fails, even though the only honest diff
/// is "everything is new."
fn plain_git_file_changes_with_hunks(
    probe: &PlainGitVerificationProbe,
    unified: usize,
) -> Result<Vec<FileChange>> {
    let git_repo = gix::discover(&probe.root)?;
    let mut head_tree = if git_repo.head()?.is_unborn() {
        None
    } else {
        Some(git_repo.head_tree()?)
    };
    let mut changes = Vec::with_capacity(probe.changes.change_count());
    for path in &probe.changes.modified {
        changes.push(plain_git_file_change(
            &git_repo,
            head_tree.as_mut(),
            &probe.root,
            path,
            "modified",
            DiffKind::Modified,
            unified,
        )?);
    }
    for path in &probe.changes.added {
        changes.push(plain_git_file_change(
            &git_repo,
            head_tree.as_mut(),
            &probe.root,
            path,
            "added",
            DiffKind::Added,
            unified,
        )?);
    }
    for path in &probe.changes.deleted {
        changes.push(plain_git_file_change(
            &git_repo,
            head_tree.as_mut(),
            &probe.root,
            path,
            "deleted",
            DiffKind::Deleted,
            unified,
        )?);
    }
    Ok(changes)
}

#[allow(clippy::too_many_arguments)]
fn plain_git_file_change(
    git_repo: &gix::Repository,
    head_tree: Option<&mut gix::Tree<'_>>,
    root: &Path,
    path: &std::path::Path,
    kind: &str,
    diff_kind: DiffKind,
    unified: usize,
) -> Result<FileChange> {
    let (old_blob, old_mode) = match (head_tree, &diff_kind) {
        (Some(tree), DiffKind::Modified | DiffKind::Deleted) => {
            match plain_git_lookup_blob_and_mode(git_repo, tree, path)? {
                Some((blob, mode)) => (Some(blob), Some(mode)),
                None => (None, None),
            }
        }
        _ => (None, None),
    };
    let new_blob = match diff_kind {
        DiffKind::Added | DiffKind::Modified => {
            // A read error here means the file vanished between the
            // status scan and the diff attempt — fall back to status-
            // only so the rendered patch at least names the path.
            read_worktree_blob_for_diff(&root.join(path)).ok()
        }
        _ => None,
    };
    // Added files take their mode from the live worktree; deleted files
    // from the HEAD tree entry resolved above. (A pure mode change on a
    // modify isn't surfaced as a header here.)
    let mode = match diff_kind {
        DiffKind::Added => worktree_file_mode(&root.join(path)),
        DiffKind::Deleted => old_mode,
        _ => None,
    };
    let (lines, eol, binary) = compute_plain_git_hunks(
        old_blob.as_ref(),
        new_blob.as_ref(),
        &diff_kind,
        unified,
    );
    Ok(FileChange {
        path: path.display().to_string(),
        kind: kind.to_string(),
        binary,
        lines,
        eol,
        mode,
        ..Default::default()
    })
}

fn plain_git_lookup_blob_and_mode(
    git_repo: &gix::Repository,
    tree: &mut gix::Tree<'_>,
    path: &std::path::Path,
) -> Result<Option<(Blob, FileMode)>> {
    let Some(entry) = tree.peel_to_entry_by_path(path)? else {
        return Ok(None);
    };
    let entry_mode = entry.mode();
    if !entry_mode.is_blob_or_symlink() {
        return Ok(None);
    }
    let mode = if entry_mode.is_link() {
        FileMode::Symlink
    } else if entry_mode.is_executable() {
        FileMode::Executable
    } else {
        FileMode::Normal
    };
    let object = git_repo.find_object(entry.object_id())?;
    Ok(Some((Blob::new(object.data.clone()), mode)))
}

fn compute_plain_git_hunks(
    old: Option<&Blob>,
    new: Option<&Blob>,
    diff_kind: &DiffKind,
    unified: usize,
) -> (Option<Vec<LineDiff>>, FileEolState, bool) {
    let attempt = || -> Result<(Vec<LineDiff>, FileEolState)> {
        match diff_kind {
            DiffKind::Added => {
                let Some(new) = new else {
                    return Ok((Vec::new(), FileEolState::default()));
                };
                ensure_text_diffable(new)?;
                let eol = eol_for_added(new);
                Ok((number_lines(blob_lines(new, "+")?), eol))
            }
            DiffKind::Deleted => {
                let Some(old) = old else {
                    return Ok((Vec::new(), FileEolState::default()));
                };
                ensure_text_diffable(old)?;
                let eol = eol_for_deleted(old);
                Ok((number_lines(blob_lines(old, "-")?), eol))
            }
            DiffKind::Modified => match (old, new) {
                (Some(old), Some(new)) => {
                    ensure_text_diffable(old)?;
                    ensure_text_diffable(new)?;
                    let eol = eol_for_modified(old, new);
                    let diff = diff_blobs(old, new);
                    let lines = diff
                        .iter()
                        .map(|l| LineDiff::new(l.prefix(), l.content()))
                        .collect();
                    Ok((number_lines(lines), eol))
                }
                (None, Some(new)) => {
                    ensure_text_diffable(new)?;
                    let eol = eol_for_added(new);
                    Ok((number_lines(blob_lines(new, "+")?), eol))
                }
                (Some(old), None) => {
                    ensure_text_diffable(old)?;
                    let eol = eol_for_deleted(old);
                    Ok((number_lines(blob_lines(old, "-")?), eol))
                }
                (None, None) => Ok((Vec::new(), FileEolState::default())),
            },
            DiffKind::Unchanged => Ok((Vec::new(), FileEolState::default())),
        }
    };
    match attempt() {
        Ok((lines, eol)) => (Some(unified_hunks(lines, unified, &eol)), eol, false),
        Err(error) if is_binary_diff_error(&error) => {
            (None, FileEolState::default(), true)
        }
        Err(_) => (None, FileEolState::default(), false),
    }
}

#[allow(clippy::too_many_arguments)]
fn render_worktree_status_diff(
    cli: &Cli,
    status: &objects::worktree::WorktreeStatus,
    stat: bool,
    name_only: bool,
    detect_renames: bool,
    patch: bool,
    unified: usize,
    repo: Option<&Repository>,
) -> Result<()> {
    // `--patch` and JSON output are the two consumers that actually
    // need the hunk vector: `--patch` prints it, and JSON always carries
    // a `.patch` field when a repo is available (regardless of the CLI
    // flag). The other printers — `--stat`, `--name-only`, the default
    // pretty printer — read only kind/path off the status entries, so we
    // keep the cheap status-only construction for them.
    let json = should_output_json(cli, None);
    let want_hunks = (patch || json)
        && !stat
        && !name_only
        && repo.is_some();
    let from_tree = if want_hunks && let Some(repo) = repo {
        head_from_tree(repo)?
    } else {
        None
    };

    let changes = file_changes_from_status(status, want_hunks, repo, from_tree.as_ref(), unified);
    let changes = if detect_renames {
        detect_clear_renames_for_worktree_status(cli, changes)?
    } else {
        changes
    };
    let mut output = DiffOutput::new(Some("HEAD".to_string()), None, changes, None, None, None);
    populate_patch_text(&mut output);

    if should_output_json(cli, None) {
        println!("{}", serde_json::to_string(&output)?);
    } else if name_only {
        for change in &output.changes {
            println!("{}", change.path);
        }
    } else if stat {
        print_stat(&output);
    } else if patch {
        print_diff_patch(&output);
    } else {
        print_diff(&output);
    }
    Ok(())
}

/// Build `FileChange` entries from a `WorktreeStatus`, optionally
/// computing the per-file hunk vector (with EOL metadata) so the
/// patch renderer has something to render. When `want_hunks` is
/// false the entries are status-only — same as the old behaviour.
fn file_changes_from_status(
    status: &objects::worktree::WorktreeStatus,
    want_hunks: bool,
    repo: Option<&Repository>,
    from_tree: Option<&Tree>,
    unified: usize,
) -> Vec<FileChange> {
    let mut changes = Vec::with_capacity(status.change_count());
    for path in &status.modified {
        changes.push(make_status_file_change(
            path,
            "modified",
            DiffKind::Modified,
            want_hunks,
            repo,
            from_tree,
            unified,
        ));
    }
    for path in &status.added {
        changes.push(make_status_file_change(
            path,
            "added",
            DiffKind::Added,
            want_hunks,
            repo,
            from_tree,
            unified,
        ));
    }
    for path in &status.deleted {
        changes.push(make_status_file_change(
            path,
            "deleted",
            DiffKind::Deleted,
            want_hunks,
            repo,
            from_tree,
            unified,
        ));
    }
    changes
}

#[allow(clippy::too_many_arguments)]
fn make_status_file_change(
    path: &std::path::Path,
    kind: &str,
    diff_kind: DiffKind,
    want_hunks: bool,
    repo: Option<&Repository>,
    from_tree: Option<&Tree>,
    unified: usize,
) -> FileChange {
    let path_str = path.display().to_string();
    let (lines, eol, binary, mode) = if want_hunks && let Some(repo) = repo {
        // Worktree status diffs have no `to_tree`; the add mode comes
        // from the live worktree, the delete mode from `from_tree`.
        let mode = change_file_mode(repo, from_tree, None, &path_str, kind);
        match get_worktree_diff(repo, from_tree, &path_str, &diff_kind) {
            Ok((raw, eol)) => (Some(unified_hunks(raw, unified, &eol)), eol, false, mode),
            Err(error) if is_binary_diff_error(&error) => {
                (None, FileEolState::default(), true, mode)
            }
            // Worktree read errors on a status-listed file mean the
            // file vanished between the status scan and the diff
            // attempt. Fall back to status-only; the renderer prints
            // the file header without a body, matching git's
            // behaviour for transient races.
            Err(_) => (None, FileEolState::default(), false, mode),
        }
    } else {
        (None, FileEolState::default(), false, None)
    };
    FileChange {
        path: path_str,
        kind: kind.to_string(),
        binary,
        lines,
        eol,
        mode,
        ..Default::default()
    }
}

fn head_from_tree(repo: &Repository) -> Result<Option<Tree>> {
    let Some(head_id) = repo.head()? else {
        return Ok(None);
    };
    let Some(state) = repo.store().get_state(&head_id)? else {
        return Ok(None);
    };
    Ok(repo.store().get_tree(&state.tree)?)
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
            let diff_result = get_state_diff(
                repo,
                from_tree.as_ref(),
                &to_tree,
                &change.path,
                &change.kind,
            );
            let binary = diff_result
                .as_ref()
                .err()
                .is_some_and(is_binary_diff_error);
            let (lines, eol) = match diff_result {
                Ok((lines, eol)) => (Some(unified_hunks(lines, unified, &eol)), eol),
                Err(_) => (None, FileEolState::default()),
            };
            FileChange {
                path: change.path.clone(),
                kind: change.kind.to_string(),
                binary,
                lines,
                eol,
                ..Default::default()
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
            let diff_result = get_state_diff(
                repo,
                from_tree.as_ref(),
                to_tree,
                &change.path,
                &change.kind,
            );
            let binary = diff_result
                .as_ref()
                .err()
                .is_some_and(is_binary_diff_error);
            let (lines, eol) = match diff_result {
                Ok((lines, eol)) => (Some(unified_hunks(lines, unified, &eol)), eol),
                Err(_) => (None, FileEolState::default()),
            };
            FileChange {
                path: change.path.clone(),
                kind: change.kind.to_string(),
                binary,
                lines,
                eol,
                ..Default::default()
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

fn unified_hunks(lines: Vec<LineDiff>, context: usize, eol: &FileEolState) -> Vec<LineDiff> {
    if lines.is_empty() {
        return lines;
    }
    if !lines.iter().any(|line| line.prefix != " ") {
        // No `+`/`-` lines. The only way an all-context diff is still a
        // real change is a trailing-newline-only edit (`hello\n` <->
        // `hello`): `diff_blobs` strips terminators, so the changed tail
        // line collapses to shared context. Synthesize a single tail
        // hunk so the renderer can split it and attach the
        // `\ No newline at end of file` marker. Otherwise it's a genuine
        // no-op — return the lines untouched (no hunk header).
        if eol.old_has_final_newline == eol.new_has_final_newline {
            return lines;
        }
        return eol_only_tail_hunk(lines, context);
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

/// Build a single hunk anchored on the file's last line for a
/// trailing-newline-only change. The body is `context` lines plus the
/// tail (all shared context); the renderer (`render_patch_hunks`) splits
/// the tail into a `-`/`+` pair and attaches the no-newline marker to
/// the side that lacks the terminator. Mirrors `git diff`'s hunk for an
/// EOL-only edit (e.g. `@@ -2,4 +2,4 @@` for a 5-line file at context 3).
fn eol_only_tail_hunk(lines: Vec<LineDiff>, context: usize) -> Vec<LineDiff> {
    let end = lines.len();
    let start = end.saturating_sub(context + 1);
    let (old_start, old_len, new_start, new_len) = hunk_span(&lines, start, end);
    let mut output = Vec::with_capacity(end - start + 1);
    output.push(LineDiff {
        prefix: "@".to_string(),
        content: format!("@ -{},{} +{},{} @@", old_start, old_len, new_start, new_len),
        old_line: None,
        new_line: None,
    });
    output.extend_from_slice(&lines[start..end]);
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
) -> Result<(Vec<LineDiff>, FileEolState)> {
    let worktree_path = repo.root().join(path);

    match kind {
        DiffKind::Added => {
            let new_blob = read_worktree_blob_for_diff(&worktree_path)?;
            let eol = eol_for_added(&new_blob);
            Ok((number_lines(blob_lines(&new_blob, "+")?), eol))
        }
        DiffKind::Deleted => {
            // `find_blob_in_tree` walks the path component by component;
            // a root-only `tree.get(path)` misses nested deletions like
            // `src/nested/file.txt` and would drop the deletion hunk.
            if let Some(tree) = from_tree
                && let Some(blob) = find_blob_in_tree(repo, tree, path)?
            {
                let eol = eol_for_deleted(&blob);
                return Ok((number_lines(blob_lines(&blob, "-")?), eol));
            }
            Ok((vec![], FileEolState::default()))
        }
        DiffKind::Modified => {
            let new_blob = read_worktree_blob_for_diff(&worktree_path)?;

            if let Some(tree) = from_tree
                && let Some(old_blob) = find_blob_in_tree(repo, tree, path)?
            {
                ensure_text_diffable(&old_blob)?;
                ensure_text_diffable(&new_blob)?;
                let eol = eol_for_modified(&old_blob, &new_blob);
                let diff = diff_blobs(&old_blob, &new_blob);
                let lines = diff
                    .iter()
                    .map(|l| LineDiff::new(l.prefix(), l.content()))
                    .collect();
                return Ok((number_lines(lines), eol));
            }

            let eol = eol_for_added(&new_blob);
            Ok((number_lines(blob_lines(&new_blob, "+")?), eol))
        }
        DiffKind::Unchanged => Ok((Vec::new(), FileEolState::default())),
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
    let mut renames: Vec<(String, String, f64)> = Vec::new();
    for (score, old_path, new_path) in candidates {
        if used_old.insert(old_path.clone()) && used_new.insert(new_path.clone()) {
            renames.push((old_path, new_path, score));
        }
    }
    if renames.is_empty() {
        return Ok(changes);
    }

    let rename_by_new = renames
        .iter()
        .map(|(old_path, new_path, score)| (new_path.as_str(), (old_path.as_str(), *score)))
        .collect::<std::collections::BTreeMap<_, _>>();
    let removed_old = renames
        .iter()
        .map(|(old_path, _, _)| old_path.as_str())
        .collect::<BTreeSet<_>>();

    let mut output = Vec::with_capacity(changes.len() - renames.len());
    for mut change in changes {
        if change.kind == "deleted" && removed_old.contains(change.path.as_str()) {
            continue;
        }
        if change.kind == "added"
            && let Some((old_path, score)) = rename_by_new.get(change.path.as_str()).copied()
        {
            let (lines, eol) = if include_lines {
                match rename_lines(repo, from_tree, to_tree, old_path, &change.path, unified) {
                    Ok(Some((lines, eol))) => (Some(lines), eol),
                    Ok(None) => (None, FileEolState::default()),
                    Err(error) if is_binary_diff_error(&error) => {
                        change.binary = true;
                        (None, FileEolState::default())
                    }
                    Err(error) => return Err(error),
                }
            } else {
                (None, FileEolState::default())
            };
            change.kind = "renamed".to_string();
            change.old_path = Some(old_path.to_string());
            change.similarity_score = Some(score);
            change.lines = lines;
            change.eol = eol;
            // The original `added` carried a stat-path tally that
            // counted the file as a pure insertion; after we collapse
            // the (added, deleted) pair into one rename, those line
            // counts double-count the move. Drop them so DiffStats
            // falls back to walking the (possibly None) `lines`
            // payload chosen above.
            change.line_counts = None;
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
) -> Result<Option<(Vec<LineDiff>, FileEolState)>> {
    let Some(old_blob) = blob_from_tree(repo, from_tree, old_path)? else {
        return Ok(None);
    };
    let Some(new_blob) = new_blob_for_rename(repo, to_tree, new_path)? else {
        return Ok(None);
    };
    ensure_text_diffable(&old_blob)?;
    ensure_text_diffable(&new_blob)?;
    let eol = eol_for_modified(&old_blob, &new_blob);
    let diff = diff_blobs(&old_blob, &new_blob);
    let lines = diff
        .iter()
        .map(|line| LineDiff::new(line.prefix(), line.content()))
        .collect();
    Ok(Some((unified_hunks(number_lines(lines), unified, &eol), eol)))
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
) -> Result<(Vec<LineDiff>, FileEolState)> {
    match kind {
        DiffKind::Added => {
            let Some(new_blob) = find_blob_in_tree(repo, to_tree, path)? else {
                return Ok((Vec::new(), FileEolState::default()));
            };
            let eol = eol_for_added(&new_blob);
            Ok((number_lines(blob_lines(&new_blob, "+")?), eol))
        }
        DiffKind::Deleted => {
            let Some(tree) = from_tree else {
                return Ok((Vec::new(), FileEolState::default()));
            };
            let Some(old_blob) = find_blob_in_tree(repo, tree, path)? else {
                return Ok((Vec::new(), FileEolState::default()));
            };
            let eol = eol_for_deleted(&old_blob);
            Ok((number_lines(blob_lines(&old_blob, "-")?), eol))
        }
        DiffKind::Modified => {
            let Some(new_blob) = find_blob_in_tree(repo, to_tree, path)? else {
                return Ok((Vec::new(), FileEolState::default()));
            };
            if let Some(tree) = from_tree
                && let Some(old_blob) = find_blob_in_tree(repo, tree, path)?
            {
                ensure_text_diffable(&old_blob)?;
                ensure_text_diffable(&new_blob)?;
                let eol = eol_for_modified(&old_blob, &new_blob);
                let diff = diff_blobs(&old_blob, &new_blob);
                let lines = diff
                    .iter()
                    .map(|l| LineDiff::new(l.prefix(), l.content()))
                    .collect();
                return Ok((number_lines(lines), eol));
            }
            // No corresponding blob in `from_tree` — render as all-new.
            let eol = eol_for_added(&new_blob);
            Ok((number_lines(blob_lines(&new_blob, "+")?), eol))
        }
        DiffKind::Unchanged => Ok((Vec::new(), FileEolState::default())),
    }
}

/// Trailing-newline state for a one-sided change (added or deleted).
/// The absent side is reported as "has newline" so the patch renderer
/// never tries to emit a marker for content that doesn't exist.
fn eol_for_added(new_blob: &Blob) -> FileEolState {
    let (new_eol, new_count) = blob_eol_meta(new_blob);
    FileEolState {
        old_has_final_newline: true,
        new_has_final_newline: new_eol,
        old_line_count: 0,
        new_line_count: new_count,
    }
}

fn eol_for_deleted(old_blob: &Blob) -> FileEolState {
    let (old_eol, old_count) = blob_eol_meta(old_blob);
    FileEolState {
        old_has_final_newline: old_eol,
        new_has_final_newline: true,
        old_line_count: old_count,
        new_line_count: 0,
    }
}

fn eol_for_modified(old_blob: &Blob, new_blob: &Blob) -> FileEolState {
    let (old_eol, old_count) = blob_eol_meta(old_blob);
    let (new_eol, new_count) = blob_eol_meta(new_blob);
    FileEolState {
        old_has_final_newline: old_eol,
        new_has_final_newline: new_eol,
        old_line_count: old_count,
        new_line_count: new_count,
    }
}

/// `diff_blobs` strips line terminators before the renderer sees the
/// hunks, so the per-side trailing-newline state has to come from the
/// raw blob bytes. Empty blobs are treated as "no marker needed":
/// there's nothing to lack a newline.
fn blob_eol_meta(blob: &Blob) -> (bool, usize) {
    let content = blob.content();
    if content.is_empty() {
        return (true, 0);
    }
    let has_eol = content.ends_with(b"\n");
    let line_count = blob
        .content_str()
        .map(|text| text.lines().count())
        .unwrap_or(0);
    (has_eol, line_count)
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
    match find_entry_in_tree(repo, tree, path)? {
        Some(entry) => Ok(Some(repo.require_blob(&entry.hash)?)),
        None => Ok(None),
    }
}

/// Resolve a path to its `TreeEntry`, descending through subtrees.
///
/// `Tree::get` binary-searches a single tree's direct children only, so
/// a nested path like `src/nested/file.txt` must be walked component by
/// component — a root-level `tree.get("src/nested/file.txt")` always
/// misses. Returns the entry for a blob or symlink leaf; `None` for a
/// missing path or a directory leaf.
fn find_entry_in_tree(repo: &Repository, tree: &Tree, path: &str) -> Result<Option<TreeEntry>> {
    let parts: Vec<&str> = path.split('/').collect();
    find_entry_recursive(repo, tree, &parts)
}

fn find_entry_recursive(
    repo: &Repository,
    tree: &Tree,
    parts: &[&str],
) -> Result<Option<TreeEntry>> {
    if parts.is_empty() {
        return Ok(None);
    }

    let name = parts[0];
    let entry = match tree.get(name) {
        Some(e) => e,
        None => return Ok(None),
    };

    if parts.len() == 1 {
        if entry.is_blob() || entry.entry_type == EntryType::Symlink {
            return Ok(Some(entry.clone()));
        }
    } else if entry.is_tree()
        && let Some(subtree) = repo.store().get_tree(&entry.hash)?
    {
        return find_entry_recursive(repo, &subtree, &parts[1..]);
    }

    Ok(None)
}

/// Resolve a worktree path's git file mode for patch headers. A symlink
/// reports `120000`; a regular file with any executable bit set reports
/// `100755`; everything else `100644`. Read failures fall back to `None`
/// (the renderer then emits the regular-file default).
fn worktree_file_mode(path: &Path) -> Option<FileMode> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() {
        return Some(FileMode::Symlink);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 != 0 {
            return Some(FileMode::Executable);
        }
    }
    Some(FileMode::Normal)
}

/// Pick the git file mode the patch renderer stamps on an add/delete.
/// Added files take the content-side mode (the `to_tree` entry for a
/// state-to-state diff, otherwise the live worktree); deleted files take
/// the old `from_tree` entry's mode. Other kinds carry no mode header.
fn change_file_mode(
    repo: &Repository,
    from_tree: Option<&Tree>,
    to_tree: Option<&Tree>,
    path: &str,
    kind: &str,
) -> Option<FileMode> {
    match kind {
        "added" => match to_tree {
            Some(tree) => find_entry_in_tree(repo, tree, path)
                .ok()
                .flatten()
                .map(|entry| entry.mode),
            None => worktree_file_mode(&repo.root().join(path)),
        },
        "deleted" => from_tree
            .and_then(|tree| find_entry_in_tree(repo, tree, path).ok().flatten())
            .map(|entry| entry.mode),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::unified_hunks;
    use crate::cli::commands::diff::diff_types::{
        DiffStats, FileChange, FileEolState, LineCounts, LineDiff, change_line_counts,
    };

    fn stat_change(kind: &str, counts: LineCounts) -> FileChange {
        FileChange {
            path: "notes.txt".to_string(),
            kind: kind.to_string(),
            line_counts: Some(counts),
            ..Default::default()
        }
    }

    /// The stat-only branch is supposed to count once and then drop
    /// the hunk vector. `DiffStats` must read the pre-computed tally
    /// off the FileChange so a 10MB diff renders as
    /// "1 files changed, 1 additions, 0 modifications" even though
    /// `lines` is `None`. Regressing this re-introduces the cheap-
    /// branch behaviour that treated the file like name-only.
    #[test]
    fn diff_stats_reads_line_counts_when_hunks_dropped() {
        let changes = vec![stat_change(
            "modified",
            LineCounts {
                added: 1,
                modified: 0,
                deleted: 0,
            },
        )];

        let stats = DiffStats::from_changes(&changes, None);

        assert_eq!(stats.files_changed, 1);
        assert_eq!(stats.additions, 1);
        assert_eq!(stats.modifications, 0);
        assert_eq!(stats.deletions, 0);
        assert_eq!(stats.renames, 0);
    }

    /// The file-level kind fallback must not fire when a stat-path
    /// FileChange has an empty `line_counts` payload — empty means
    /// "we counted and there were no eligible lines" (the binary or
    /// empty-diff case), not "we never counted".
    #[test]
    fn diff_stats_treats_zero_line_counts_as_authoritative() {
        let changes = vec![stat_change(
            "modified",
            LineCounts {
                added: 0,
                modified: 0,
                deleted: 0,
            },
        )];

        let stats = DiffStats::from_changes(&changes, None);

        assert_eq!(stats.modifications, 0);
        assert_eq!(stats.additions, 0);
        assert_eq!(stats.deletions, 0);
    }

    /// Sanity-check the underlying counter so the stat closure that
    /// feeds `line_counts` produces matching output.
    #[test]
    fn change_line_counts_pairs_modified_lines() {
        let lines = vec![
            LineDiff::with_lines("-", "alpha", Some(1), None),
            LineDiff::with_lines("+", "alpha-changed", None, Some(1)),
            LineDiff::with_lines("+", "fresh", None, Some(2)),
        ];
        let counts = change_line_counts(Some(&lines));
        assert_eq!(counts.modified, 1);
        assert_eq!(counts.added, 1);
        assert_eq!(counts.deleted, 0);
    }

    #[test]
    fn unified_hunks_keeps_context_decoration_when_added_block_ends_before_matching_item() {
        let lines = vec![
            LineDiff::with_lines("+", "#[test]", None, Some(1)),
            LineDiff::with_lines("+", "fn added() {}", None, Some(2)),
            LineDiff::with_lines(" ", "#[test]", Some(1), Some(3)),
            LineDiff::with_lines(" ", "fn existing() {}", Some(2), Some(4)),
        ];

        let hunk = unified_hunks(lines, 3, &FileEolState::default());

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
