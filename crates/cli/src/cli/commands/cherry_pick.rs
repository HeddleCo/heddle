// SPDX-License-Identifier: Apache-2.0
//! Cherry-pick command - apply specific commits.

use anyhow::{Result, anyhow};
use objects::object::Attribution;
use repo::Repository;

use super::worktree_safety::ensure_worktree_clean;
use crate::cli::{Cli, should_output_json};

pub fn cmd_cherry_pick(
    cli: &Cli,
    commit: String,
    message: Option<String>,
    no_commit: bool,
    force: bool,
) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;

    // Cherry-pick rewrites the worktree to match a different tree. Without a
    // dirty-worktree guard, modified-but-unsnapshotted files and untracked
    // files on the cherry-picked paths are silently destroyed: the planner has
    // no record they ever existed, so there is no snapshot to recover from.
    if !force {
        ensure_worktree_clean(&repo, "cherry-pick")?;
    }

    let change_id = repo
        .resolve_state(&commit)?
        .ok_or_else(|| anyhow!("Commit {} not found", commit))?;

    let state = repo
        .store()
        .get_state(&change_id)?
        .ok_or_else(|| anyhow!("Commit {} not found", commit))?;

    // Apply the tree from the cherry-picked commit
    let tree = repo
        .store()
        .get_tree(&state.tree)?
        .ok_or_else(|| anyhow!("Tree not found"))?;

    // Apply the tree to the worktree
    apply_tree_to_worktree(&repo, &tree)?;

    if no_commit {
        if should_output_json(cli, Some(repo.config())) {
            println!(
                "{{\"status\": \"applied\", \"commit\": \"{}\", \"no_commit\": true}}",
                commit
            );
        } else {
            println!("Applied {} (not committed)", commit);
        }
    } else {
        let cherry_message = message.unwrap_or_else(|| format!("Cherry-pick {}", commit));
        let attribution = Attribution::human(repo.get_principal()?);

        let new_state = repo.snapshot_with_attribution(Some(cherry_message), None, attribution)?;

        if should_output_json(cli, Some(repo.config())) {
            println!(
                "{{\"status\": \"committed\", \"commit\": \"{}\", \"new_commit\": \"{}\"}}",
                commit,
                new_state.change_id.short()
            );
        } else {
            println!(
                "Cherry-picked {} as {}",
                commit,
                new_state.change_id.short()
            );
        }
    }

    Ok(())
}

fn apply_tree_to_worktree(repo: &Repository, tree: &objects::object::Tree) -> Result<()> {
    use std::{
        collections::{HashMap, HashSet},
        fs,
        path::Path,
    };

    use objects::object::{EntryType, Tree, TreeEntry};

    use crate::cli::commands::merge::prepare_dir_for_file_replacement;

    // Remove entries that are not in the new tree.
    let current_tree = repo
        .current_state()?
        .and_then(|s| repo.store().get_tree(&s.tree).ok().flatten())
        .unwrap_or_default();

    let current_entries: HashMap<&str, &TreeEntry> = current_tree
        .entries()
        .iter()
        .map(|e| (e.name.as_str(), e))
        .collect();
    let current_names: HashSet<&str> = current_entries.keys().copied().collect();
    let new_names: HashSet<&str> = tree.entries().iter().map(|e| e.name.as_str()).collect();

    let source_subtree_for = |entry: &TreeEntry, name: &str| -> Result<Tree> {
        if entry.entry_type == EntryType::Tree {
            Ok(repo
                .resolve_subtree(&current_tree, Path::new(name))?
                .unwrap_or_default())
        } else {
            Ok(Tree::default())
        }
    };

    for name in current_names.difference(&new_names) {
        let path = repo.root().join(name);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if metadata.is_symlink() || metadata.is_file() {
            fs::remove_file(&path)?;
        } else if metadata.is_dir()
            && let Some(current) = current_entries.get(name)
        {
            // Preserve heddle-ignored siblings (`.git/`, `target/`,
            // `node_modules/`, …) when the cherry-picked tree drops a
            // tracked directory: only tracked descendants are removed.
            // Drive removal off the source-tree subtree so newly-added
            // ignore rules can't silently strand tracked content.
            let source_subtree = source_subtree_for(current, name)?;
            repo.remove_tracked_descendants_with_source(&path, &source_subtree)?;
        }
    }

    // Handle type changes (file→dir or dir→file).
    for entry in tree.entries() {
        let path = repo.root().join(&entry.name);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let is_dir_on_disk = metadata.is_dir();
        let is_tree_entry = entry.entry_type == EntryType::Tree;
        if is_dir_on_disk != is_tree_entry {
            if is_dir_on_disk {
                // dir → file/symlink: strip tracked content from the
                // directory, then explicitly drop the directory itself
                // so the materializer can write the new entry type. If
                // heddle-ignored content is keeping the directory
                // occupied, `prepare_dir_for_file_replacement` errors
                // with a clear message — the alternative is
                // `materialize_blob` blowing up deep in the materializer
                // with a bare "Is a directory" I/O error.
                if let Some(current) = current_entries.get(entry.name.as_str()) {
                    let source_subtree = source_subtree_for(current, &entry.name)?;
                    repo.remove_tracked_descendants_with_source(&path, &source_subtree)?;
                }
                if path.exists() {
                    prepare_dir_for_file_replacement(&path)?;
                }
            } else {
                fs::remove_file(&path)?;
            }
        }
    }

    // Write all entries recursively.
    repo.materialize_tree(tree, repo.root())?;

    Ok(())
}