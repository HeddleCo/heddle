// SPDX-License-Identifier: Apache-2.0
//! Compare command logic.

use anyhow::Result;
use repo::{DiffKind, Repository};

#[cfg(not(feature = "semantic"))]
use super::super::advice::RecoveryAdvice;
use super::{
    super::{
        history_target::{require_resolved_state, resolve_state_id},
        snapshot::ensure_current_state,
    },
    compare_output::write_output,
    compare_types::{CompareOutput, CompareSummary, FileChange, SemanticChangeEntry},
};
#[cfg(feature = "semantic")]
use crate::semantic::{SemanticDiffOptions, SemanticDiffResult, semantic_diff};
use crate::{
    cli::{Cli, should_output_json},
    config::UserConfig,
};
#[cfg(not(feature = "semantic"))]
struct SemanticDiffResult {
    changes: Vec<objects::object::SemanticChange>,
}

/// Compare two states.
///
/// Shows the differences between two arbitrary states in the repository.
pub fn cmd_compare(cli: &Cli, state_a: String, state_b: String, semantic: bool) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    if repo.current_state()?.is_none()
        && (matches!(state_a.as_str(), "HEAD" | "@") || matches!(state_b.as_str(), "HEAD" | "@"))
    {
        ensure_current_state(
            &repo,
            &UserConfig::load_default().unwrap_or_default(),
            Some("Bootstrap git-overlay before comparing HEAD".to_string()),
        )?;
    }

    let id_a = resolve_state_id(&repo, &state_a)?;
    let id_b = resolve_state_id(&repo, &state_b)?;

    let state_a_obj = require_resolved_state(&repo, &id_a)?;
    let state_b_obj = require_resolved_state(&repo, &id_b)?;

    let semantic_result: Option<SemanticDiffResult> = if semantic {
        #[cfg(not(feature = "semantic"))]
        {
            return Err(anyhow::anyhow!(RecoveryAdvice::feature_unavailable(
                "semantic compare",
                "semantic"
            )));
        }
        #[cfg(feature = "semantic")]
        {
            let options = SemanticDiffOptions::default();
            Some(semantic_diff(
                &repo,
                &state_a_obj.tree,
                &state_b_obj.tree,
                &options,
            )?)
        }
    } else {
        None
    };

    let changes = repo.diff_trees(&state_a_obj.tree, &state_b_obj.tree)?;

    let mut added = 0;
    let mut modified = 0;
    let mut deleted = 0;

    let file_changes: Vec<FileChange> = changes
        .iter()
        .map(|change| {
            match change.kind {
                DiffKind::Added => added += 1,
                DiffKind::Modified => modified += 1,
                DiffKind::Deleted => deleted += 1,
                DiffKind::Unchanged => {}
            }
            FileChange {
                path: change.path.clone(),
                kind: change.kind.to_string(),
            }
        })
        .collect();

    let semantic_changes = semantic_result.map(|r| {
        r.changes
            .into_iter()
            .map(SemanticChangeEntry::from)
            .collect::<Vec<_>>()
    });

    let renamed = semantic_changes
        .as_ref()
        .map(|changes| {
            changes
                .iter()
                .filter(|c| c.change_type == "file_renamed")
                .count()
        })
        .unwrap_or(0);

    let total = if semantic {
        added + modified + deleted + renamed
    } else {
        added + modified + deleted
    };

    let output = CompareOutput {
        state_a: id_a.short(),
        state_b: id_b.short(),
        changes: file_changes,
        semantic_changes,
        summary: CompareSummary {
            added,
            modified,
            deleted,
            renamed,
            total,
        },
    };

    write_output(&output, should_output_json(cli, Some(repo.config())))?;

    Ok(())
}
