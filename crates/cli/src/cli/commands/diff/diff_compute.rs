// SPDX-License-Identifier: Apache-2.0
//! Core diff command logic.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

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
        // Status-only entries, but modes captured before rename detection so
        // the cross-type guard fires here too (cid 3321103601) — see
        // `make_status_only_change`.
        changes
            .iter()
            .map(|change| {
                make_status_only_change(
                    Some(&repo),
                    from_tree.as_ref(),
                    to_tree.as_ref(),
                    &change.path,
                    &change.kind.to_string(),
                )
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
                // Worktree diffs (no `to_tree`) reclassify a `modified`
                // path that is now a directory into a deletion (file→dir
                // type change); state-to-state diffs read from trees and
                // never hit the filesystem, so they keep `change.kind`.
                let effective_kind = if to_tree.is_none() {
                    worktree_modified_type_change(repo.root(), &change.path, change.kind)
                        .map(|(_, diff_kind)| diff_kind)
                        .unwrap_or(change.kind)
                } else {
                    change.kind
                };
                let diff_result = if let Some(ref tree) = to_tree {
                    get_state_diff(&repo, from_tree.as_ref(), tree, &change.path, &effective_kind)
                } else {
                    get_worktree_diff(&repo, from_tree.as_ref(), &change.path, &effective_kind)
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

                let kind = effective_kind.to_string();
                let (old_mode, mode) = change_file_modes(
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
                    old_mode,
                    ..Default::default()
                }
            })
            .collect()
    };
    let file_changes = sort_changes_by_path(file_changes);
    // A type change (dir ↔ file/symlink, or regular ↔ symlink) surfaces as
    // a single `modified` entry that git records as delete-old + add-new.
    // Expand it on both diff surfaces — worktree and state-to-state — so
    // committed diffs round-trip too, before renames are detected.
    let file_changes = expand_type_changes(
        &repo,
        from_tree.as_ref(),
        to_tree.as_ref(),
        file_changes,
        !(name_only || stat),
        unified,
    )?;
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

/// Render and stash the standard unified-diff text on the output payload.
/// JSON consumers always need a patch-compatible field; the rest of the
/// print path consults the same renderer.
///
/// We always run `render_diff_patch` rather than gating on `lines`,
/// because some changes are valid header-only patches with no line body:
/// a pure rename (`rename from`/`rename to`), a mode-only modify
/// (`old mode`/`new mode`), and an empty-file add/delete. `render_diff_patch`
/// already decides per-change what is renderable and returns an empty
/// string when nothing is — so the emptiness check below is the only
/// gate we need.
fn populate_patch_text(output: &mut DiffOutput) {
    let text = render_diff_patch(output);
    if !text.is_empty() {
        output.patch = Some(text);
    }
}

/// Order a state-to-state change list deterministically by path. `diff_trees`
/// yields its change set in hash order, which differs between process
/// invocations — so `heddle diff <a> <b> --patch` and the JSON `.patch` field
/// from a separate run disagree on the order of unrelated files. git emits
/// diff entries in path order; sorting here matches that and keeps every
/// render of the same diff byte-identical. Sort *before* `expand_type_changes`
/// so each type change's local delete-before-add ordering stays intact (the
/// expansion replaces a single entry in place).
fn sort_changes_by_path(mut changes: Vec<FileChange>) -> Vec<FileChange> {
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    changes
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
    // `plain_git_worktree_status` can report the same path as BOTH
    // deleted (index-vs-HEAD) and added (untracked worktree) — e.g.
    // `git rm --cached f` followed by editing the still-present untracked
    // `f`. Emitting an add patch and a separate delete patch for one path
    // produces a conflicting pair `git apply` rejects; git renders that
    // state as a single modify (HEAD content -> worktree content), so we
    // coalesce here.
    let added_set: BTreeSet<&Path> = probe.changes.added.iter().map(PathBuf::as_path).collect();
    let deleted_set: BTreeSet<&Path> =
        probe.changes.deleted.iter().map(PathBuf::as_path).collect();

    let mut changes = Vec::with_capacity(probe.changes.change_count());
    for path in &probe.changes.modified {
        push_plain_git_modified(
            &git_repo,
            &mut head_tree,
            &probe.root,
            path,
            unified,
            &mut changes,
        )?;
    }
    for path in &probe.changes.added {
        if deleted_set.contains(path.as_path()) {
            // Coalesced HEAD→worktree modify (see above): route through the
            // type-change classifier so a coalesced regular↔symlink swap
            // splits into delete+add rather than emitting a cross-type chmod.
            push_plain_git_modified(
                &git_repo,
                &mut head_tree,
                &probe.root,
                path,
                unified,
                &mut changes,
            )?;
        } else {
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
    }
    for path in &probe.changes.deleted {
        // Already emitted as a coalesced modify in the added loop.
        if added_set.contains(path.as_path()) {
            continue;
        }
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
    // from the HEAD tree entry resolved above. A modify carries both: the
    // HEAD-tree old mode and the live-worktree new mode, so a chmod
    // (exec-bit flip) surfaces as `old mode`/`new mode`.
    let (old_mode_field, mode) = match diff_kind {
        DiffKind::Added => (None, worktree_file_mode(&root.join(path))),
        DiffKind::Deleted => (None, old_mode),
        DiffKind::Modified => (old_mode, worktree_file_mode(&root.join(path))),
        DiffKind::Unchanged => (None, None),
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
        old_mode: old_mode_field,
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

/// Classify the HEAD-tree side of a plain-Git path. A tracked entry is a
/// blob or symlink — git records no directory entries — so this returns
/// `Regular` or `Symlink`; an absent entry (unborn HEAD, or a path not in
/// HEAD) is `Absent`, which `is_type_change` treats as no type change so
/// the modify renders as content.
fn plain_git_old_side_kind(
    head_tree: Option<&mut gix::Tree<'_>>,
    path: &std::path::Path,
) -> Result<SideKind> {
    let Some(tree) = head_tree else {
        return Ok(SideKind::Absent);
    };
    let Some(entry) = tree.peel_to_entry_by_path(path)? else {
        return Ok(SideKind::Absent);
    };
    Ok(if entry.mode().is_link() {
        SideKind::Symlink
    } else {
        SideKind::Regular
    })
}

/// Emit the plain-Git `FileChange`(s) for one `modified` (or coalesced-
/// modify) path, splitting a *type change* into the delete+add pair git
/// records rather than a cross-type chmod `git apply` rejects.
///
/// This is the plain-Git mirror of the heddle path's
/// `worktree_modified_type_change` + `expand_type_changes`: it reuses the
/// same `worktree_side_kind` / `is_type_change` decision so both backends
/// classify identical input identically (a regular↔symlink swap splits, a
/// file→dir change downgrades to a deletion whose new leaves arrive as
/// their own `added` entries from status). A tracked old side is always a
/// single blob/symlink, so there is never an old subtree to expand here.
fn push_plain_git_modified(
    git_repo: &gix::Repository,
    head_tree: &mut Option<gix::Tree<'_>>,
    root: &Path,
    path: &std::path::Path,
    unified: usize,
    out: &mut Vec<FileChange>,
) -> Result<()> {
    let new_kind = worktree_side_kind(&root.join(path));
    let old_kind = plain_git_old_side_kind(head_tree.as_mut(), path)?;
    if is_type_change(old_kind, new_kind) {
        out.push(plain_git_file_change(
            git_repo,
            head_tree.as_mut(),
            root,
            path,
            "deleted",
            DiffKind::Deleted,
            unified,
        )?);
        // A new-side directory's leaves arrive as separate `added` status
        // entries; only a non-directory new side adds here.
        if new_kind != SideKind::Dir {
            out.push(plain_git_file_change(
                git_repo,
                head_tree.as_mut(),
                root,
                path,
                "added",
                DiffKind::Added,
                unified,
            )?);
        }
    } else {
        out.push(plain_git_file_change(
            git_repo,
            head_tree.as_mut(),
            root,
            path,
            "modified",
            DiffKind::Modified,
            unified,
        )?);
    }
    Ok(())
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
                (Some(old), Some(new)) => modified_blob_hunks(old, new),
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
    // Expand a type change (dir ↔ file/symlink, regular ↔ symlink) into the
    // delete-old + add-new pair git emits. Worktree surface, so the new side
    // is read from disk (`to_tree = None`).
    let changes = match repo {
        Some(repo) => {
            expand_type_changes(repo, from_tree.as_ref(), None, changes, want_hunks, unified)?
        }
        None => changes,
    };
    let changes = if detect_renames {
        // `want_hunks` (a `--patch`/JSON render) needs the rename pair's
        // edit hunk preserved; pass it through as `include_lines` and
        // the real `unified` context so a rename-with-edits doesn't
        // collapse to a pure rename that drops the content edit.
        detect_clear_renames_for_worktree_status(cli, changes, want_hunks, unified)?
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
    // Reclassify a `modified` path that is now a directory (file→dir type
    // change) into a deletion so the renderer emits `+++ /dev/null` and
    // `git apply` removes the blocking file before the nested adds land.
    let (kind, diff_kind) = match repo
        .and_then(|repo| worktree_modified_type_change(repo.root(), &path_str, diff_kind))
    {
        Some(reclassified) => reclassified,
        None => (kind, diff_kind),
    };
    match repo {
        Some(repo) if want_hunks => {
            build_worktree_change(repo, from_tree, &path_str, kind, diff_kind, unified)
        }
        _ => make_status_only_change(repo, from_tree, None, &path_str, kind),
    }
}

/// Build a status-only `FileChange` (no hunk body) that still carries its
/// `(old_mode, mode)` pair. Modes are cheap metadata that *every* output mode
/// needs, not just `--patch`/JSON: rename detection rejects a cross-type
/// (regular↔symlink) collapse by comparing the two sides' modes, and the
/// renderers stamp rename+mode headers from them. Gating mode capture on the
/// hunk-only flag dropped them on the default/`--stat`/`--name-only` paths, so
/// a cross-type move silently re-collapsed into a rename there while `--patch`
/// (which kept the modes) correctly stayed split (cid 3321103601). This is the
/// single chokepoint every status-only construction site routes through — the
/// worktree-status path, the type-change split, and the `--name-only` builder
/// — so the capture can't diverge between them again. `repo == None` is the
/// plain-Git fast path, which has no object store to resolve modes from (and
/// runs no rename collapse), so it stays modeless.
fn make_status_only_change(
    repo: Option<&Repository>,
    from_tree: Option<&Tree>,
    to_tree: Option<&Tree>,
    path_str: &str,
    kind: &str,
) -> FileChange {
    let (old_mode, mode) = match repo {
        Some(repo) => change_file_modes(repo, from_tree, to_tree, path_str, kind),
        None => (None, None),
    };
    FileChange {
        path: path_str.to_string(),
        kind: kind.to_string(),
        mode,
        old_mode,
        ..Default::default()
    }
}

/// Build a worktree-side `FileChange` with its hunk vector, EOL metadata,
/// and `(old_mode, mode)` pair. Worktree status diffs have no `to_tree`:
/// the new-side mode comes from the live worktree, the old-side mode from
/// `from_tree`.
fn build_worktree_change(
    repo: &Repository,
    from_tree: Option<&Tree>,
    path_str: &str,
    kind: &str,
    diff_kind: DiffKind,
    unified: usize,
) -> FileChange {
    let (old_mode, mode) = change_file_modes(repo, from_tree, None, path_str, kind);
    let (lines, eol, binary) = match get_worktree_diff(repo, from_tree, path_str, &diff_kind) {
        Ok((raw, eol)) => (Some(unified_hunks(raw, unified, &eol)), eol, false),
        Err(error) if is_binary_diff_error(&error) => (None, FileEolState::default(), true),
        // Worktree read errors on a status-listed file mean the file
        // vanished between the status scan and the diff attempt. Fall back
        // to status-only; the renderer prints the file header without a
        // body, matching git's behaviour for transient races.
        Err(_) => (None, FileEolState::default(), false),
    };
    FileChange {
        path: path_str.to_string(),
        kind: kind.to_string(),
        binary,
        lines,
        eol,
        mode,
        old_mode,
        ..Default::default()
    }
}

/// The object kind a path resolves to on one side of a diff.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SideKind {
    Absent,
    Dir,
    /// A regular or executable file (`100644` / `100755`).
    Regular,
    Symlink,
}

/// Classify a path's kind within a tree (the old side of a diff, or the
/// new side of a state-to-state diff). `find_entry_in_tree` resolves blob
/// and symlink leaves; a `None` there means either a directory or a
/// missing path, disambiguated by `dir_subtree_in_tree`.
fn tree_side_kind(repo: &Repository, tree: Option<&Tree>, path: &str) -> Result<SideKind> {
    let Some(tree) = tree else {
        return Ok(SideKind::Absent);
    };
    if let Some(entry) = find_entry_in_tree(repo, tree, path)? {
        return Ok(if entry.entry_type == EntryType::Symlink {
            SideKind::Symlink
        } else {
            SideKind::Regular
        });
    }
    if dir_subtree_in_tree(repo, tree, path)?.is_some() {
        Ok(SideKind::Dir)
    } else {
        Ok(SideKind::Absent)
    }
}

/// Classify a path's new-side kind: the `to_tree` entry for a
/// state-to-state diff, otherwise the live worktree.
fn new_side_kind(repo: &Repository, to_tree: Option<&Tree>, path: &str) -> Result<SideKind> {
    match to_tree {
        Some(tree) => tree_side_kind(repo, Some(tree), path),
        None => Ok(worktree_side_kind(&repo.root().join(path))),
    }
}

/// Classify a worktree path. `symlink_metadata` does not follow links, so
/// a symlink (even one pointing at a directory) reports `Symlink`, not
/// `Dir`. A missing path is `Absent`.
fn worktree_side_kind(path: &Path) -> SideKind {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return SideKind::Absent;
    };
    if meta.file_type().is_symlink() {
        SideKind::Symlink
    } else if meta.is_dir() {
        SideKind::Dir
    } else {
        SideKind::Regular
    }
}

/// A `modified` entry whose two sides are different object *kinds* — git
/// can't represent it as a chmod and `git apply` rejects the attempt.
fn is_type_change(old: SideKind, new: SideKind) -> bool {
    use SideKind::{Dir, Regular, Symlink};
    matches!(
        (old, new),
        (Dir, Regular)
            | (Dir, Symlink)
            | (Regular, Dir)
            | (Symlink, Dir)
            | (Regular, Symlink)
            | (Symlink, Regular)
    )
}

/// Rewrite a `modified` entry that is actually a *type change* into the
/// delete-old + add-new pair `git diff` emits, so `git apply` can swap one
/// object kind for another instead of attempting a cross-type chmod.
///
/// Two shapes need this (both verified against `git diff`):
/// * **dir ↔ file/symlink** — a tracked directory replaced by a file (or
///   the reverse). git emits a deletion of every leaf under the old
///   directory plus an add of the new file (or vice versa); a bare
///   `old mode`/`new mode` chmod cannot turn a directory into a file
///   (cid 3319484717 — the committed-diff side dropped this entirely).
/// * **regular ↔ symlink** — `100644`/`100755` ⇄ `120000`. git emits a
///   delete of the old object and an add of the new; `git apply` rejects
///   the `old mode 100644`/`new mode 120000` chmod form across this
///   boundary (cid 3319484727).
///
/// Shared by the worktree path (`to_tree == None`, new side read from
/// disk) and the state-to-state path (`to_tree == Some`, new side read
/// from the object store) so the split is byte-identical on both — fixing
/// it in only one place would leave committed diffs (`heddle diff HEAD~1
/// HEAD --patch`) emitting the form git rejects.
///
/// The worktree path never sees a *file → dir* `modified` entry here:
/// `worktree_modified_type_change` downgrades it to a deletion upstream
/// and the directory's new leaves arrive as separate `added` entries from
/// status. The state path has no such upstream pass, so both directions
/// are handled below.
fn expand_type_changes(
    repo: &Repository,
    from_tree: Option<&Tree>,
    to_tree: Option<&Tree>,
    changes: Vec<FileChange>,
    want_hunks: bool,
    unified: usize,
) -> Result<Vec<FileChange>> {
    let mut output = Vec::with_capacity(changes.len());
    for change in changes {
        if change.kind != "modified" {
            output.push(change);
            continue;
        }
        let old_kind = tree_side_kind(repo, from_tree, &change.path)?;
        let new_kind = new_side_kind(repo, to_tree, &change.path)?;
        if !is_type_change(old_kind, new_kind) {
            output.push(change);
            continue;
        }

        // Delete the old side: every leaf under a directory, else the
        // single old object.
        if old_kind == SideKind::Dir {
            if let Some(from_tree) = from_tree
                && let Some(subtree) = dir_subtree_in_tree(repo, from_tree, &change.path)?
            {
                let mut nested = Vec::new();
                collect_subtree_blob_paths(repo, &subtree, &change.path, &mut nested)?;
                for nested_path in nested {
                    output.push(make_type_change_part(
                        repo,
                        Some(from_tree),
                        to_tree,
                        &nested_path,
                        DiffKind::Deleted,
                        want_hunks,
                        unified,
                    ));
                }
            }
        } else {
            output.push(make_type_change_part(
                repo,
                from_tree,
                to_tree,
                &change.path,
                DiffKind::Deleted,
                want_hunks,
                unified,
            ));
        }

        // Add the new side: every leaf under a directory, else the single
        // new object. A new-side directory only occurs in the state path
        // (the worktree path reclassifies file→dir upstream), so its
        // leaves come from `to_tree`.
        if new_kind == SideKind::Dir {
            if let Some(to_tree) = to_tree
                && let Some(subtree) = dir_subtree_in_tree(repo, to_tree, &change.path)?
            {
                let mut nested = Vec::new();
                collect_subtree_blob_paths(repo, &subtree, &change.path, &mut nested)?;
                for nested_path in nested {
                    output.push(make_type_change_part(
                        repo,
                        from_tree,
                        Some(to_tree),
                        &nested_path,
                        DiffKind::Added,
                        want_hunks,
                        unified,
                    ));
                }
            }
        } else {
            output.push(make_type_change_part(
                repo,
                from_tree,
                to_tree,
                &change.path,
                DiffKind::Added,
                want_hunks,
                unified,
            ));
        }
    }
    Ok(output)
}

fn make_type_change_part(
    repo: &Repository,
    from_tree: Option<&Tree>,
    to_tree: Option<&Tree>,
    path_str: &str,
    diff_kind: DiffKind,
    want_hunks: bool,
    unified: usize,
) -> FileChange {
    let kind = diff_kind.to_string();
    if !want_hunks {
        return make_status_only_change(Some(repo), from_tree, to_tree, path_str, &kind);
    }
    match to_tree {
        Some(to_tree) => {
            build_state_change(repo, from_tree, to_tree, path_str, &kind, diff_kind, unified)
        }
        None => build_worktree_change(repo, from_tree, path_str, &kind, diff_kind, unified),
    }
}

/// State-to-state analogue of `build_worktree_change`: both sides come
/// from the object store, so the new-side mode and content are read from
/// `to_tree` rather than the live worktree.
fn build_state_change(
    repo: &Repository,
    from_tree: Option<&Tree>,
    to_tree: &Tree,
    path_str: &str,
    kind: &str,
    diff_kind: DiffKind,
    unified: usize,
) -> FileChange {
    let (old_mode, mode) = change_file_modes(repo, from_tree, Some(to_tree), path_str, kind);
    let (lines, eol, binary) = match get_state_diff(repo, from_tree, to_tree, path_str, &diff_kind) {
        Ok((raw, eol)) => (Some(unified_hunks(raw, unified, &eol)), eol, false),
        Err(error) if is_binary_diff_error(&error) => (None, FileEolState::default(), true),
        Err(_) => (None, FileEolState::default(), false),
    };
    FileChange {
        path: path_str.to_string(),
        kind: kind.to_string(),
        binary,
        lines,
        eol,
        mode,
        old_mode,
        ..Default::default()
    }
}

/// Resolve `path` to its subtree if it names a directory in `tree`,
/// descending component by component. Returns `None` for a missing path or
/// a blob/symlink leaf.
fn dir_subtree_in_tree(repo: &Repository, tree: &Tree, path: &str) -> Result<Option<Tree>> {
    let mut current = tree.clone();
    let mut parts = path.split('/').peekable();
    while let Some(name) = parts.next() {
        let Some(entry) = current.get(name) else {
            return Ok(None);
        };
        if !entry.is_tree() {
            return Ok(None);
        }
        let Some(subtree) = repo.store().get_tree(&entry.hash)? else {
            return Ok(None);
        };
        if parts.peek().is_none() {
            return Ok(Some(subtree));
        }
        current = subtree;
    }
    Ok(None)
}

/// Collect every blob/symlink leaf path under `subtree`, prefixed with the
/// subtree's path, so a dir→file type change can emit a deletion per file.
fn collect_subtree_blob_paths(
    repo: &Repository,
    subtree: &Tree,
    prefix: &str,
    out: &mut Vec<String>,
) -> Result<()> {
    for entry in subtree.entries() {
        let child_path = format!("{prefix}/{}", entry.name);
        if entry.is_tree() {
            if let Some(nested) = repo.store().get_tree(&entry.hash)? {
                collect_subtree_blob_paths(repo, &nested, &child_path, out)?;
            }
        } else {
            out.push(child_path);
        }
    }
    Ok(())
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
            build_state_change(
                repo,
                from_tree.as_ref(),
                &to_tree,
                &change.path,
                &change.kind.to_string(),
                change.kind,
                unified,
            )
        })
        .collect();
    let file_changes = sort_changes_by_path(file_changes);
    let file_changes = expand_type_changes(
        repo,
        from_tree.as_ref(),
        Some(&to_tree),
        file_changes,
        true,
        unified,
    )?;
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

    let mut output = DiffOutput::new(
        Some(from_change_id.short()),
        Some(to_change_id.short()),
        file_changes,
        semantic_changes,
        None,
        None,
    );
    populate_patch_text(&mut output);
    Ok(output)
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
            build_state_change(
                repo,
                from_tree.as_ref(),
                to_tree,
                &change.path,
                &change.kind.to_string(),
                change.kind,
                unified,
            )
        })
        .collect();
    let file_changes = sort_changes_by_path(file_changes);
    let file_changes = expand_type_changes(
        repo,
        from_tree.as_ref(),
        Some(to_tree),
        file_changes,
        true,
        unified,
    )?;
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

    let mut output = DiffOutput::new(
        Some(from_change_id.short()),
        Some(to_label.into()),
        file_changes,
        semantic_changes,
        None,
        None,
    );
    populate_patch_text(&mut output);
    Ok(output)
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
        // Emit the hunk body UNTRIMMED. Decoration trimming drops a real
        // `+` line, which is a pretty-display nicety only — applying it
        // here would desync the body from the `@@` header counts computed
        // above (via `hunk_span`) and corrupt the `--patch`/JSON line
        // model so `git apply` rejects or mis-reconstructs the file (cid
        // 3320364905). The trim now lives in `print_diff` alone, via
        // `trim_added_decorations_for_display`.
        output.extend_from_slice(&lines[start..end]);
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

/// Pretty-display transform: drop a leading added "decoration" line
/// (`#[...]`, `///`, `@`, etc.) when an identical context line already
/// follows the inserted block, so the diff anchors on the existing item
/// rather than showing a duplicated attribute.
///
/// DISPLAY ONLY. This drops a real `+` line, so it must never reach the
/// `--patch`/JSON line model — the dropped line is a genuine change and
/// omitting it desyncs the `@@` header counts, corrupting `git apply`
/// (cid 3320364905). `unified_hunks` keeps the canonical (untrimmed)
/// hunk body; `print_diff` calls this purely for human-facing rendering.
///
/// Applied per hunk body (segmented on the `@` header lines) so the
/// decoration match can never cross a hunk boundary into an unrelated
/// context line.
pub(crate) fn trim_added_decorations_for_display(lines: &[LineDiff]) -> Vec<LineDiff> {
    let mut output = Vec::with_capacity(lines.len());
    let mut body_start = 0usize;
    for (index, line) in lines.iter().enumerate() {
        if line.prefix == "@" {
            if body_start < index {
                output.extend(trim_trailing_added_decorations(&lines[body_start..index]));
            }
            output.push(line.clone());
            body_start = index + 1;
        }
    }
    if body_start < lines.len() {
        output.extend(trim_trailing_added_decorations(&lines[body_start..]));
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
                return modified_blob_hunks(&old_blob, &new_blob);
            }

            let eol = eol_for_added(&new_blob);
            Ok((number_lines(blob_lines(&new_blob, "+")?), eol))
        }
        DiffKind::Unchanged => Ok((Vec::new(), FileEolState::default())),
    }
}

/// A tracked file replaced by a directory (`foo` → `foo/bar`) surfaces in
/// heddle's worktree status as a `modified` path whose worktree side is
/// now a directory. `git diff` represents that as a *deletion* of the file
/// (the directory's new files arrive as separate `added` entries), so we
/// reclassify the modify to a deletion: otherwise `read_worktree_blob_for_diff`
/// fails reading the directory, the change collapses to `lines: None`, and
/// the renderer drops it — leaving `git apply` unable to create `foo/bar`
/// over the still-present `foo`. Returns the effective `(kind, DiffKind)`.
///
/// Classification goes through `worktree_side_kind` (`symlink_metadata`, no
/// link following), so only a *real* directory triggers the downgrade. A
/// regular file replaced by a symlink *pointing at* a directory reports
/// `Symlink`, stays a `modified` entry, and is split into delete+add by
/// `expand_type_changes` — `Path::is_dir()` would have followed the link,
/// misread it as a directory, and dropped the `120000` add (cid 3320033195).
fn worktree_modified_type_change(
    repo_root: &Path,
    path: &str,
    diff_kind: DiffKind,
) -> Option<(&'static str, DiffKind)> {
    if matches!(diff_kind, DiffKind::Modified)
        && worktree_side_kind(&repo_root.join(path)) == SideKind::Dir
    {
        Some(("deleted", DiffKind::Deleted))
    } else {
        None
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
    include_lines: bool,
    unified: usize,
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
    detect_clear_renames(&repo, from_tree.as_ref(), None, changes, include_lines, unified)
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

    // Snapshot each side's git mode so a candidate can be rejected when the
    // deleted and added sides differ in git *type class* (regular vs
    // symlink). git never renames across a type boundary: `git apply`
    // rejects a `rename from/to` whose `old mode`/`new mode` cross S_IFMT
    // (e.g. `100644` → `120000`). Such a pair must stay a delete + add,
    // which the cross-path delete/add rendering already round-trips. A
    // regular↔executable move stays *within* the regular class, so it is
    // intentionally still collapsible — git emits it as a rename with an
    // `old mode`/`new mode` pair that `git apply` accepts.
    let deleted_side_modes = changes
        .iter()
        .filter(|change| change.kind == "deleted")
        .map(|change| (change.path.as_str(), change.mode))
        .collect::<std::collections::BTreeMap<&str, Option<FileMode>>>();
    let added_side_modes = changes
        .iter()
        .filter(|change| change.kind == "added")
        .map(|change| (change.path.as_str(), change.mode))
        .collect::<std::collections::BTreeMap<&str, Option<FileMode>>>();

    let mut candidates = Vec::new();
    for old_path in &deleted {
        let Some(old_blob) = blob_from_tree(repo, from_tree, old_path)? else {
            continue;
        };
        for new_path in &added {
            // A delete + add at the *same* path is a type change
            // (regular ↔ symlink), not a rename — `expand_type_changes`
            // emits both halves and collapsing them back into a
            // `foo → foo` rename would drop the type swap.
            if old_path == new_path {
                continue;
            }
            // A cross-*type* move (regular ↔ symlink) at different paths is
            // never a rename either: collapsing it would emit a rename
            // header carrying a mismatched `old mode`/`new mode`, which
            // `git apply` rejects. Leave the pair as a separate delete +
            // add. (Regular↔executable stays compatible — see the
            // mode-snapshot comment above.)
            if !rename_mode_compatible(
                deleted_side_modes.get(old_path).copied().flatten(),
                added_side_modes.get(new_path).copied().flatten(),
            ) {
                continue;
            }
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
    // The deleted entry (whose `mode` carries the rename's *old-side*
    // mode) is dropped below, so snapshot old-side modes keyed by path
    // first. A rename paired with a chmod/type change (`old.sh` -> `new.sh`
    // made executable) needs both modes on the collapsed `renamed` change
    // so the renderer can emit `old mode`/`new mode`.
    let deleted_modes = changes
        .iter()
        .filter(|change| change.kind == "deleted")
        .map(|change| (change.path.clone(), change.mode))
        .collect::<std::collections::BTreeMap<String, Option<FileMode>>>();

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
            // `change.mode` already holds the added (new) side mode; pull
            // the deleted (old) side mode off the snapshot so a rename+chmod
            // surfaces both modes in the patch headers.
            change.old_mode = deleted_modes.get(old_path).copied().flatten();
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

/// Whether a delete + add can be collapsed into a single `renamed` change
/// given the two sides' git file modes. git only renames *within* one
/// S_IFMT type class: regular files (`100644`) and executables (`100755`)
/// share the regular-file type, so a move between them renders as a rename
/// with an `old mode`/`new mode` pair that `git apply` accepts; a symlink
/// (`120000`) is a distinct type, so a regular↔symlink move is never a
/// rename — `git apply` rejects a `rename from/to` whose `new mode
/// (120000)` doesn't match its `old mode (100644)`. A missing mode falls
/// back to the regular-file default the renderer also assumes.
fn rename_mode_compatible(old: Option<FileMode>, new: Option<FileMode>) -> bool {
    let is_symlink = |mode: Option<FileMode>| matches!(mode, Some(FileMode::Symlink));
    is_symlink(old) == is_symlink(new)
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
                return modified_blob_hunks(&old_blob, &new_blob);
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

/// Compute the `(lines, eol)` for a `modified` pair of blobs, applying the
/// identical-content short-circuit shared by every diff-rendering path.
///
/// When the two blobs carry identical bytes the change is a pure mode flip
/// (chmod / exec-bit), even on a binary file: returning an empty body routes
/// the renderer through the `old mode`/`new mode` header instead of the
/// binary-refusal branch, so a binary chmod-only round-trips through `git
/// apply` rather than emitting a placeholder binary patch git rejects.
///
/// Both heddle-backed paths (`get_worktree_diff`, `get_state_diff`) and the
/// plain-Git fast path (`compute_plain_git_hunks`) call this, so the
/// short-circuit + text-diff decision lives in exactly one place — a binary
/// chmod-only behaves identically regardless of backend (cid 3320033191).
fn modified_blob_hunks(old: &Blob, new: &Blob) -> Result<(Vec<LineDiff>, FileEolState)> {
    if old.content() == new.content() {
        return Ok((Vec::new(), FileEolState::default()));
    }
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

/// Resolve the `(old_mode, mode)` pair the patch renderer stamps on a
/// change. `mode` is the field the renderer reads for `new file mode`
/// (adds) / `deleted file mode` (deletes); `old_mode` pairs with it on a
/// `modified` change so a chmod surfaces as `old mode`/`new mode`.
///
/// * **added** — `(None, new-side mode)`: the `to_tree` entry for a
///   state-to-state diff, otherwise the live worktree.
/// * **deleted** — `(None, old-side mode)`: the `from_tree` entry's mode
///   carried in `mode` for the `deleted file mode` header.
/// * **modified** — `(old-side mode, new-side mode)`: `from_tree` entry
///   vs. the `to_tree` entry (state diff) or live worktree.
/// * anything else — `(None, None)`.
fn change_file_modes(
    repo: &Repository,
    from_tree: Option<&Tree>,
    to_tree: Option<&Tree>,
    path: &str,
    kind: &str,
) -> (Option<FileMode>, Option<FileMode>) {
    let old_side = || {
        from_tree
            .and_then(|tree| find_entry_in_tree(repo, tree, path).ok().flatten())
            .map(|entry| entry.mode)
    };
    let new_side = || match to_tree {
        Some(tree) => find_entry_in_tree(repo, tree, path)
            .ok()
            .flatten()
            .map(|entry| entry.mode),
        None => worktree_file_mode(&repo.root().join(path)),
    };
    match kind {
        "added" => (None, new_side()),
        "deleted" => (None, old_side()),
        "modified" => (old_side(), new_side()),
        _ => (None, None),
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

    /// The canonical hunk body (the one `--patch`/JSON consume) must keep
    /// every real `+` line, including a leading `+#[test]` decoration that
    /// duplicates a following context line. Dropping it here desyncs the
    /// `@@` header counts and corrupts `git apply` (cid 3320364905) — the
    /// trim is now a display-only transform, not a property of the model.
    #[test]
    fn unified_hunks_keeps_added_decoration_in_canonical_body() {
        let lines = vec![
            LineDiff::with_lines("+", "#[test]", None, Some(1)),
            LineDiff::with_lines("+", "fn added() {}", None, Some(2)),
            LineDiff::with_lines(" ", "#[test]", Some(1), Some(3)),
            LineDiff::with_lines(" ", "fn existing() {}", Some(2), Some(4)),
        ];

        let hunk = unified_hunks(lines, 3, &FileEolState::default());

        let header = hunk
            .iter()
            .find(|line| line.prefix == "@")
            .expect("hunk should carry an `@@` header");
        // Two added (`+`) lines + two context lines on the new side → +4.
        assert_eq!(
            header.content, "@ -1,2 +1,4 @@",
            "header counts must match the untrimmed body: {hunk:?}"
        );
        assert!(
            hunk.iter()
                .any(|line| line.prefix == "+" && line.content == "#[test]"),
            "added decoration line must survive in the canonical body: {hunk:?}"
        );
        assert!(
            hunk.iter()
                .any(|line| line.prefix == "+" && line.content == "fn added() {}"),
            "added function body should remain: {hunk:?}"
        );
    }

    /// The display transform DOES trim the leading `+#[test]` so the
    /// pretty diff anchors on the existing item — but only the body lines
    /// move; the `@@` header (untrimmed counts) is preserved verbatim.
    #[test]
    fn display_trim_drops_added_decoration_but_keeps_header() {
        use super::trim_added_decorations_for_display;

        let lines = vec![
            LineDiff::with_lines("+", "#[test]", None, Some(1)),
            LineDiff::with_lines("+", "fn added() {}", None, Some(2)),
            LineDiff::with_lines(" ", "#[test]", Some(1), Some(3)),
            LineDiff::with_lines(" ", "fn existing() {}", Some(2), Some(4)),
        ];
        let hunk = unified_hunks(lines, 3, &FileEolState::default());

        let display = trim_added_decorations_for_display(&hunk);

        assert!(
            display
                .iter()
                .filter(|line| line.content == "#[test]")
                .all(|line| line.prefix == " "),
            "display trim should let existing context own the decoration: {display:?}"
        );
        assert!(
            display
                .iter()
                .any(|line| line.prefix == "+" && line.content == "fn added() {}"),
            "added function body should remain after display trim: {display:?}"
        );
        assert_eq!(
            display.iter().find(|line| line.prefix == "@").map(|l| l.content.as_str()),
            Some("@ -1,2 +1,4 @@"),
            "display trim must not rewrite the `@@` header: {display:?}"
        );
    }
}
