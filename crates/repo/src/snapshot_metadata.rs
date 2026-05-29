// SPDX-License-Identifier: Apache-2.0
//! Post-snapshot metadata refresh.
//!
//! After a snapshot lands a new state on a thread, the active
//! `Thread` record needs its `changed_paths`, `heavy_impact_paths`,
//! `impact_categories`, summaries, and freshness fields updated so
//! `heddle status`, the dashboard, and the integration-policy
//! evaluator see the new reality.
//!
//! Both `heddle capture` (CLI worktree-snapshot) and
//! `ContentAddressedMount::capture` (mount-snapshot) need to do this
//! same work after their state lands. Putting it here keeps the
//! logic in one place and lets `crates/mount` avoid pulling in
//! `crates/cli` deps.

use objects::store::ObjectStore;
use std::path::Path;

use chrono::Utc;
use objects::{
    error::HeddleError,
    object::{ChangeId, State, ThreadName, Tree, Verification},
};

use crate::{
    repository::Repository, ConfidenceBand, Thread, ThreadConfidenceSummary, ThreadFreshness,
    ThreadImpactCategory, ThreadManager, ThreadState, ThreadVerificationSummary,
};

/// Result of a metadata refresh: derived signal the caller may want
/// to surface (e.g. CLI prints a heavy-impact-change warning if
/// `heavy_impact_paths` is non-empty).
#[derive(Debug, Clone, Default)]
pub struct ThreadMetadataRefresh {
    /// Whether this snapshot landed any heavy-impact paths
    /// (dependencies, build config, generated, public API).
    pub promotion_suggested: bool,
    /// The heavy-impact subset of changed paths.
    pub heavy_impact_paths: Vec<String>,
    /// All paths that changed against the thread's base state. Empty
    /// if there's no active thread to refresh.
    pub changed_paths: Vec<String>,
}

/// Refresh the active thread's metadata after a snapshot has landed.
///
/// `state` is the new state; `tree` is the new state's tree object
/// (passed in to avoid a redundant `get_tree`). The active thread is
/// resolved by `repo.root()` via `ThreadManager::find_by_execution_root`.
///
/// If no active thread record exists (e.g. snapshot from outside any
/// thread), this is a no-op and returns the default report.
pub fn refresh_active_thread_metadata(
    repo: &Repository,
    state: &State,
    tree: &Tree,
) -> Result<ThreadMetadataRefresh, HeddleError> {
    let manager = ThreadManager::new(repo.heddle_dir());
    let Some(mut thread) = manager.find_by_execution_root(repo.root())? else {
        return Ok(ThreadMetadataRefresh::default());
    };
    let base_state = repo
        .resolve_state(&thread.base_state)?
        .and_then(|id| repo.store().get_state(&id).ok().flatten());
    let changed_paths = compute_changed_paths(repo, base_state.as_ref(), state, tree)?;
    let heavy_impact_paths = compute_heavy_impact_paths(&changed_paths);
    let impact_categories = classify_impact_categories(&changed_paths);
    update_thread_state_from_state(&mut thread, state);
    thread.changed_paths = changed_paths.clone();
    thread.heavy_impact_paths = heavy_impact_paths.clone();
    thread.impact_categories = impact_categories;
    thread.promotion_suggested = !heavy_impact_paths.is_empty();
    thread.state = ThreadState::Active;
    thread.updated_at = Utc::now();
    refresh_thread_freshness(repo, &mut thread)?;
    manager.save(&thread)?;
    Ok(ThreadMetadataRefresh {
        promotion_suggested: thread.promotion_suggested,
        heavy_impact_paths,
        changed_paths,
    })
}

/// Mirror the CLI's per-state thread-summary refresh: copy the new
/// state's verification + confidence into the thread record's
/// summaries so callers don't have to reload the state object.
pub fn update_thread_state_from_state(thread: &mut Thread, state: &State) {
    thread.current_state = Some(state.change_id.short());
    thread.verification_summary = summarize_verification(state.verification.as_ref());
    thread.confidence_summary = summarize_confidence(state.confidence);
}

/// Sentinel rendered for an absent (`None`) confidence value in
/// CLI text output. Centralised so `heddle show`, `heddle log`, and
/// `heddle capture` agree on the same glyph and an `Option<f32>`
/// never silently collapses into `0.00`. JSON callers continue to
/// see `null` via `Option<f32>` serialization — this constant is
/// text-mode only.
pub const ABSENT_CONFIDENCE_DISPLAY: &str = "—";

/// Format a confidence value for human-readable CLI output. `None`
/// renders as the [`ABSENT_CONFIDENCE_DISPLAY`] sentinel; `Some(v)`
/// uses the same `{:.2}` shape every renderer used historically.
/// Returns just the value portion — callers prepend their own label
/// (`"Confidence: "`, `"  Confidence: "`, etc.) to preserve the
/// exact indentation each command already used.
pub fn format_confidence(confidence: Option<f32>) -> String {
    match confidence {
        Some(value) => format!("{value:.2}"),
        None => ABSENT_CONFIDENCE_DISPLAY.to_string(),
    }
}

/// Pure derivation: confidence value + band for the dashboard.
pub fn summarize_confidence(confidence: Option<f32>) -> ThreadConfidenceSummary {
    let band = confidence.map(|value| {
        if value >= 0.9 {
            ConfidenceBand::High
        } else if value >= 0.75 {
            ConfidenceBand::Medium
        } else {
            ConfidenceBand::Low
        }
    });
    ThreadConfidenceSummary {
        value: confidence,
        band,
    }
}

/// Pure derivation: verification summary from a state's verification.
pub fn summarize_verification(verification: Option<&Verification>) -> ThreadVerificationSummary {
    let Some(verification) = verification else {
        return ThreadVerificationSummary::default();
    };
    ThreadVerificationSummary {
        tests_passed: verification.tests_passed,
        tests_failed: verification.tests_failed,
        coverage_pct: verification.coverage_pct,
        lint_warnings: verification.lint_warnings,
    }
}

/// Re-evaluate freshness against the thread's target. Mirrors the
/// CLI's `refresh_thread_freshness`.
pub fn refresh_thread_freshness(repo: &Repository, thread: &mut Thread) -> Result<(), HeddleError> {
    thread.freshness = if let Some(target_thread) = thread.target_thread.as_deref() {
        if let Some(target_state) = repo.refs().get_thread(&ThreadName::from(target_thread))? {
            if target_state.short() == thread.base_state {
                ThreadFreshness::Current
            } else {
                ThreadFreshness::Stale
            }
        } else {
            ThreadFreshness::Unknown
        }
    } else {
        ThreadFreshness::Unknown
    };
    Ok(())
}

/// Returns the subset of `paths` that touch dependency, build,
/// generated, or public-API files. Used to drive promotion advice
/// after a snapshot lands.
pub fn compute_heavy_impact_paths(paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .filter(|path| {
            is_dependency_path(path)
                || is_build_path(path)
                || is_generated_path(path)
                || is_public_api_path(path)
        })
        .cloned()
        .collect()
}

/// Tag the thread with the union of impact categories present in the
/// changed paths. `RepoWideRefactor` lights up at >= 20 changed
/// paths regardless of category.
pub fn classify_impact_categories(paths: &[String]) -> Vec<ThreadImpactCategory> {
    let mut categories = Vec::new();
    for path in paths {
        if is_dependency_path(path) && !categories.contains(&ThreadImpactCategory::DependencyGraph)
        {
            categories.push(ThreadImpactCategory::DependencyGraph);
        }
        if is_build_path(path) && !categories.contains(&ThreadImpactCategory::BuildRuntimeConfig) {
            categories.push(ThreadImpactCategory::BuildRuntimeConfig);
        }
        if is_generated_path(path) && !categories.contains(&ThreadImpactCategory::GeneratedOutputs)
        {
            categories.push(ThreadImpactCategory::GeneratedOutputs);
        }
        if is_public_api_path(path) && !categories.contains(&ThreadImpactCategory::PublicApiSurface)
        {
            categories.push(ThreadImpactCategory::PublicApiSurface);
        }
    }
    if paths.len() >= 20 && !categories.contains(&ThreadImpactCategory::RepoWideRefactor) {
        categories.push(ThreadImpactCategory::RepoWideRefactor);
    }
    categories
}

fn compute_changed_paths(
    repo: &Repository,
    base_state: Option<&State>,
    state: &State,
    tree: &Tree,
) -> Result<Vec<String>, HeddleError> {
    if base_state.is_none() {
        let mut changed = Vec::new();
        collect_tree_paths(repo, tree, "", &mut changed)?;
        changed.sort();
        changed.dedup();
        return Ok(changed);
    }
    let empty = Tree::new();
    let empty_hash = repo.store().put_tree(&empty)?;
    let base_hash = base_state.map(|base| base.tree).unwrap_or(empty_hash);
    let diff = repo.diff_trees(&base_hash, &state.tree)?;
    let mut changed = diff
        .into_iter()
        .map(|change| change.path)
        .collect::<Vec<_>>();
    changed.sort();
    changed.dedup();
    Ok(changed)
}

fn collect_tree_paths(
    repo: &Repository,
    tree: &Tree,
    prefix: &str,
    out: &mut Vec<String>,
) -> Result<(), HeddleError> {
    for entry in tree.entries() {
        let path = if prefix.is_empty() {
            entry.name.clone()
        } else {
            format!("{prefix}/{}", entry.name)
        };
        if entry.is_tree() {
            if let Some(subtree) = repo.store().get_tree(&entry.hash)? {
                collect_tree_paths(repo, &subtree, &path, out)?;
            }
        } else {
            out.push(path);
        }
    }
    Ok(())
}

fn is_dependency_path(path: &str) -> bool {
    let file_name = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path);
    matches!(
        file_name,
        "Cargo.toml"
            | "Cargo.lock"
            | "package.json"
            | "package-lock.json"
            | "pnpm-lock.yaml"
            | "yarn.lock"
            | "poetry.lock"
            | "pyproject.toml"
            | "go.mod"
            | "go.sum"
            | "Gemfile"
            | "Gemfile.lock"
            | "composer.json"
            | "composer.lock"
    )
}

fn is_build_path(path: &str) -> bool {
    let file_name = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path);
    matches!(
        file_name,
        "Makefile"
            | "justfile"
            | "rust-toolchain"
            | "rust-toolchain.toml"
            | "flake.nix"
            | "Dockerfile"
    ) || path.contains(".github/workflows/")
        || path.ends_with(".proto")
        || path.ends_with(".sql")
}

fn is_generated_path(path: &str) -> bool {
    path.contains("/generated/")
        || path.contains("/vendor/")
        || path.contains("/dist/")
        || path.contains("/build/")
}

fn is_public_api_path(path: &str) -> bool {
    path.contains("/src/lib.rs")
        || path.contains("/include/")
        || path.ends_with("/mod.rs")
        || path.ends_with("/public.rs")
}

/// Record this snapshot in the oplog, mirroring the worktree-snapshot
/// path. The mount-side `capture()` calls this after planting the
/// new state and updating the thread ref.
pub fn record_snapshot_in_oplog(
    repo: &Repository,
    new_state: &ChangeId,
    prev_head: Option<&ChangeId>,
    thread: Option<&str>,
) -> Result<u64, HeddleError> {
    repo.oplog()
        .record_snapshot(new_state, prev_head, thread, Some(&repo.op_scope()))
}

/// Translate `ChangeId` short-strings into `ChangeId`s if the thread
/// references one. `Repository::resolve_state` already handles the
/// alias forms; this just flattens the option for callers.
pub fn resolve_short_change_id(
    repo: &Repository,
    short: &str,
) -> Result<Option<ChangeId>, HeddleError> {
    repo.resolve_state(short)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `summarize_confidence(None)` must produce a summary whose
    /// rendered form is unambiguously absent — never `0.00` or `0%`.
    /// Down-stream renderers (`heddle show`, `heddle log`,
    /// `heddle capture`) display the em dash `—` for an unset value;
    /// JSON callers see `null` thanks to `Option<f32>` serialization.
    #[test]
    fn summarize_confidence_none_renders_as_absent_marker() {
        let summary = summarize_confidence(None);
        assert!(summary.value.is_none(), "value must remain None");
        assert!(summary.band.is_none(), "band must remain None");

        // Drive the assertion through the shared `format_confidence`
        // helper that `heddle show`, `heddle log`, and `heddle
        // capture` all delegate to — that way this test fails if a
        // renderer ever bypasses the helper and re-introduces a
        // misleading `0.00`.
        let rendered = format!("Confidence: {}", format_confidence(summary.value));
        assert!(
            !rendered.contains("0.00"),
            "absent confidence must not render as 0.00, got {rendered:?}"
        );
        assert!(
            !rendered.contains('%'),
            "absent confidence must not render as a percentage, got {rendered:?}"
        );
        assert!(
            rendered.contains(ABSENT_CONFIDENCE_DISPLAY),
            "absent confidence must surface the canonical sentinel, got {rendered:?}"
        );

        // JSON-side: an `Option<f32>` field with no
        // `skip_serializing_if`/`unwrap_or` should serialize as null,
        // not 0.0. Lock this in here so future serde changes flag it.
        let json = serde_json::to_value(&summary).expect("summary serializes");
        assert!(
            json.get("value").is_some_and(serde_json::Value::is_null),
            "JSON value must be null for absent confidence, got {json}"
        );
    }

    /// A present confidence still classifies and renders normally —
    /// the absent-case fix must not regress the band derivation or
    /// the `{:.2}` formatting.
    #[test]
    fn summarize_confidence_some_renders_with_two_decimals() {
        let summary = summarize_confidence(Some(0.85));
        assert_eq!(summary.value, Some(0.85));
        assert!(matches!(summary.band, Some(ConfidenceBand::Medium)));

        let rendered = format!("Confidence: {}", format_confidence(summary.value));
        assert_eq!(rendered, "Confidence: 0.85");
    }

    /// `format_confidence` is the single source of truth for the
    /// absent sentinel. Lock both branches here so renderer drift is
    /// caught at the helper boundary, not in each command's tests.
    #[test]
    fn format_confidence_branches() {
        assert_eq!(format_confidence(None), ABSENT_CONFIDENCE_DISPLAY);
        assert_eq!(format_confidence(Some(0.0)), "0.00");
        assert_eq!(format_confidence(Some(0.5)), "0.50");
        assert_eq!(format_confidence(Some(1.0)), "1.00");
    }
}
