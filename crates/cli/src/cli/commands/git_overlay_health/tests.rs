// SPDX-License-Identifier: Apache-2.0
//! Tests for the shared repository verification contract.
//!
//! Extracted verbatim from `git_overlay_health.rs` (heddle#609 phase 2):
//! the `mod tests` body moved into this sibling file unchanged (de-indented
//! one level). It referenced the parent via `super::{...}` inline and continues
//! to do so as a sibling module -- pure code movement, no logic change.
use objects::object::ThreadName;
use heddle_core::status::next_action::{
    canonical_bridge_import_ref_command, canonical_bridge_reconcile_ref_preview_command,
    remote_tracking_next_action,
};
use repo::{GitRemoteTrackingStatus, Repository};
use sley::Repository as SleyRepository;
use tempfile::TempDir;

use super::{
    RepositoryVerificationState, VerificationActionPlan, action_template,
    clean_health, machine_contract_coverage, remote_drift_decision,
    repository_setup_guidance, repository_verification_blocked_advice,
};
use crate::cli::commands::build_command_catalog;

fn verification_state(
    recommended_action: impl Into<String>,
    recovery_commands: Vec<String>,
) -> RepositoryVerificationState {
    RepositoryVerificationState {
        verified: false,
        status: "needs_reconcile".to_string(),
        repository_mode: "git_overlay".to_string(),
        heddle_initialized: true,
        git_branch: Some("main".to_string()),
        heddle_thread: Some("main".to_string()),
        worktree_dirty: false,
        worktree_state: "clean".to_string(),
        import_state: "imported".to_string(),
        mapping_state: "needs_reconcile".to_string(),
        remote_drift: "none".to_string(),
        active_operation: None,
        default_remote: None,
        clone_verification: "verified".to_string(),
        machine_contract: "verified".to_string(),
        machine_contract_coverage: machine_contract_coverage(),
        workflow_status: "blocked".to_string(),
        workflow_summary: "Git and Heddle disagree".to_string(),
        summary: "Git and Heddle disagree".to_string(),
        recommended_action: recommended_action.into(),
        recommended_action_template: None,
        recovery_commands,
        recovery_action_templates: Vec::new(),
        checks: Vec::new(),
    }
}

#[test]
fn repository_setup_guidance_distinguishes_init_from_adopt() {
    let mut init = verification_state("heddle init", vec!["heddle init".to_string()]);
    init.status = "needs_init".to_string();
    init.repository_mode = "plain-git".to_string();
    init.heddle_initialized = false;
    init.import_state = "git_backed".to_string();
    init.mapping_state = "git_backed".to_string();

    let guidance = repository_setup_guidance(&init).expect("init guidance");
    assert!(guidance.setup_line.contains("initialize Heddle"));
    assert!(guidance.setup_line.contains("heddle init"));
    assert!(guidance.effect.contains("Git commits stay in Git storage"));

    let mut convert = verification_state(
        "heddle adopt --ref main",
        vec!["heddle adopt --ref main".to_string()],
    );
    convert.status = "needs_import".to_string();
    convert.repository_mode = "git-overlay".to_string();
    convert.import_state = "needs_import".to_string();
    convert.mapping_state = "needs_import".to_string();

    let guidance = repository_setup_guidance(&convert).expect("conversion guidance");
    assert!(
        guidance
            .setup_line
            .contains("connect this branch with heddle adopt --ref main")
    );
    assert!(guidance.effect.contains("adoption imports Git history"));
}

#[test]
fn canonical_git_overlay_ref_commands_quote_parseable_refs() {
    let import = canonical_bridge_import_ref_command("feature with spaces");
    assert_eq!(
        action_template(&import)
            .expect("import command should expose a template")
            .argv_template[1..],
        ["bridge", "git", "import", "--ref", "feature with spaces"]
    );

    let reconcile =
        canonical_bridge_reconcile_ref_preview_command(Some("heddle"), "feature 'quoted'");
    assert_eq!(
        action_template(&reconcile)
            .expect("reconcile command should expose a template")
            .argv_template[1..],
        [
            "bridge",
            "git",
            "reconcile",
            "--prefer",
            "heddle",
            "--ref",
            "feature 'quoted'",
            "--preview"
        ]
    );
}

#[test]
fn repository_verification_blocked_advice_uses_verify_when_no_action_exists() {
    let trust = verification_state("", Vec::new());

    let advice = repository_verification_blocked_advice(
        "repository_verification_blocked",
        "blocked",
        "retrying the operation",
        &trust,
        "unsafe",
        "would change",
        "nothing changed",
        None,
    );

    assert_eq!(advice.primary_command, "heddle verify");
    assert_eq!(advice.recovery_commands, vec!["heddle verify"]);
    assert_eq!(
        advice.hint,
        "Run `heddle verify` before retrying the operation."
    );
}

#[test]
fn repository_verification_blocked_advice_preserves_trust_recovery_commands() {
    let trust = verification_state(
        "heddle bridge git reconcile --ref main --preview",
        vec![
            "heddle bridge git reconcile --ref main --preview".to_string(),
            "heddle verify".to_string(),
        ],
    );

    let advice = repository_verification_blocked_advice(
        "repository_verification_blocked",
        "blocked",
        "retrying the operation",
        &trust,
        "unsafe",
        "would change",
        "nothing changed",
        None,
    );

    assert_eq!(
        advice.primary_command,
        "heddle bridge git reconcile --ref main --preview"
    );
    assert_eq!(advice.recovery_commands, trust.recovery_commands);
}

#[test]
fn repository_verification_blocked_advice_keeps_primary_override_first() {
    let trust = verification_state(
        "heddle bridge git import --ref origin/main",
        vec!["heddle bridge git import --ref origin/main".to_string()],
    );

    let advice = repository_verification_blocked_advice(
        "git_checkpoint_preflight_blocked",
        "blocked",
        "retrying `heddle commit`",
        &trust,
        "unsafe",
        "would change",
        "nothing changed",
        Some("heddle pull origin main --preview".to_string()),
    );

    assert_eq!(advice.primary_command, "heddle pull origin main --preview");
    assert_eq!(
        advice.recovery_commands,
        vec!["heddle pull origin main --preview", "heddle verify"]
    );
}

#[test]
fn verification_action_plan_keeps_blockers_above_guidance() {
    let clean_health = clean_health("clean", Vec::new());

    let machine_gap = VerificationActionPlan::from_parts(
        &clean_health,
        Some("heddle push".to_string()),
        Some("heddle land --thread feature --no-push".to_string()),
        Some("heddle doctor schemas --output json".to_string()),
    );
    assert_eq!(
        machine_gap.primary_action,
        "heddle doctor schemas --output json"
    );
    assert_eq!(
        machine_gap.recovery_commands,
        vec!["heddle doctor schemas --output json"]
    );
    assert_eq!(machine_gap.remote_action.as_deref(), Some("heddle push"));
    assert_eq!(
        machine_gap.workflow_action.as_deref(),
        Some("heddle land --thread feature --no-push")
    );

    let workflow_waiting = VerificationActionPlan::from_parts(
        &clean_health,
        Some("heddle push".to_string()),
        Some("heddle land --thread feature --no-push".to_string()),
        None,
    );
    assert_eq!(
        workflow_waiting.primary_action,
        "heddle land --thread feature --no-push"
    );

    let publish_guidance = VerificationActionPlan::from_parts(
        &clean_health,
        Some("heddle push".to_string()),
        None,
        None,
    );
    assert_eq!(publish_guidance.primary_action, "heddle push");
}

#[test]
fn remote_tracking_next_action_covers_basic_git_states_without_repo_context() {
    assert_eq!(
        remote_tracking_next_action(&remote("main", "origin/main", 0, 1, "heddle pull")).as_deref(),
        Some("heddle pull")
    );
    assert_eq!(
        remote_tracking_next_action(&remote("main", "origin/main", 1, 0, "heddle push")).as_deref(),
        Some("heddle push")
    );
    assert_eq!(
        remote_tracking_next_action(&remote("main", "origin/main", 1, 1, "heddle fetch"))
            .as_deref(),
        Some("heddle bridge git import --ref origin/main")
    );
    assert_eq!(
        remote_tracking_next_action(&remote("main", "", 1, 0, "heddle push")).as_deref(),
        Some("heddle push")
    );
}

#[test]
fn remote_drift_decision_prefers_import_until_upstream_thread_matches_git_tip() {
    let (_temp, repo) = test_repo();
    let diverged = remote("main", "origin/main", 1, 1, "heddle fetch");

    let unimported = remote_drift_decision(&repo, &diverged);
    assert_eq!(unimported.status, "remote_diverged");
    assert_eq!(
        unimported.primary_action.as_deref(),
        Some("heddle bridge git import --ref origin/main")
    );
    assert_eq!(
        unimported.recovery_commands,
        vec![
            "heddle bridge git import --ref origin/main",
            "heddle bridge git reconcile --ref origin/main --preview"
        ]
    );

    let head = repo.head().unwrap().expect("test repo should have a head");
    repo.refs()
        .set_thread(&ThreadName::new("origin/main"), &head)
        .unwrap();
    let stale_thread = remote_drift_decision(&repo, &diverged);
    assert_eq!(
        stale_thread.primary_action.as_deref(),
        Some("heddle bridge git import --ref origin/main")
    );
    assert_eq!(
        stale_thread.recovery_commands,
        vec![
            "heddle bridge git import --ref origin/main",
            "heddle bridge git reconcile --ref origin/main --preview"
        ]
    );
}

#[test]
fn remote_drift_decision_treats_local_only_branch_as_clean_publishable_state() {
    let (_temp, repo) = test_repo();
    let untracked = remote("scratch", "", 0, 0, "heddle push");

    let decision = remote_drift_decision(&repo, &untracked);
    assert_eq!(decision.status, "remote_untracked");
    assert!(decision.verified_as_clean);
    assert_eq!(decision.primary_action.as_deref(), Some("heddle push"));
    assert!(decision.recovery_commands.is_empty());
    assert!(!decision.requires_clean_worktree);
}

fn remote(
    branch: &str,
    upstream: &str,
    ahead: usize,
    behind: usize,
    next_action: &str,
) -> GitRemoteTrackingStatus {
    GitRemoteTrackingStatus {
        branch: branch.to_string(),
        upstream: upstream.to_string(),
        ahead,
        behind,
        local_oid: None,
        upstream_oid: None,
        upstream_is_undone_checkpoint: false,
        message: "remote fixture".to_string(),
        next_action: next_action.to_string(),
    }
}

fn test_repo() -> (TempDir, Repository) {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    (temp, repo)
}

/// `git rm --cached path` keeps the file in the worktree but
/// stages the deletion: Git reports both `D path` (index vs HEAD)
/// and `?? path` (worktree). Both signals must survive the
/// dedup step or downstream code mistakes a tracked removal for a
/// new file.
#[test]
fn plain_git_worktree_status_preserves_staged_removal_alongside_untracked() {
    use std::{path::PathBuf, process::Command};

    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();

    let run_git = |args: &[&str]| -> Option<bool> {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()
            .ok()?;
        Some(output.status.success())
    };

    let Some(true) = run_git(&["init", "--quiet"]) else {
        eprintln!("git not on PATH or init failed — skipping");
        return;
    };
    for cmd in [
        ["config", "user.email", "test@example.com"].as_slice(),
        ["config", "user.name", "Test"].as_slice(),
    ] {
        if !matches!(run_git(cmd), Some(true)) {
            eprintln!("git config failed — skipping");
            return;
        }
    }
    std::fs::write(root.join("file.txt"), "hello").expect("write file");
    for cmd in [
        ["add", "file.txt"].as_slice(),
        ["commit", "-m", "initial", "--quiet"].as_slice(),
        ["rm", "--cached", "--quiet", "file.txt"].as_slice(),
    ] {
        if !matches!(run_git(cmd), Some(true)) {
            eprintln!("git command failed — skipping");
            return;
        }
    }

    let git_repo = SleyRepository::discover(root).expect("open git repo");
    let status = super::plain_git_worktree_status(root, &git_repo).expect("status");

    let target = PathBuf::from("file.txt");
    assert!(
        status.added.iter().any(|path| path == &target),
        "untracked worktree copy must still appear as added: {status:?}"
    );
    assert!(
        status.deleted.iter().any(|path| path == &target),
        "staged removal must not be wiped by the untracked entry: {status:?}"
    );
    assert!(
        !status.modified.iter().any(|path| path == &target),
        "no modified entry for `git rm --cached` path: {status:?}"
    );
}

#[test]
fn machine_contract_coverage_counts_the_same_rows_as_command_catalog() {
    let catalog = build_command_catalog();
    let catalog_json = catalog
        .commands
        .iter()
        .filter(|command| command.supports_json)
        .count();
    let catalog_op_id = catalog
        .commands
        .iter()
        .filter(|command| command.supports_op_id)
        .count();
    let catalog_jsonl = catalog
        .commands
        .iter()
        .filter(|command| command.json_kind == "jsonl" || command.json_kind == "json_or_jsonl")
        .count();
    let catalog_mutating = catalog
        .commands
        .iter()
        .filter(|command| command.mutates)
        .count();
    let json_with_schema = catalog
        .commands
        .iter()
        .filter(|command| {
            command.supports_json
                && command.schema_verbs.iter().any(|verb| {
                    !crate::cli::commands::schemas::opaque_schema_verbs().contains(&verb.as_str())
                })
        })
        .count();
    let mutating_json = catalog
        .commands
        .iter()
        .filter(|command| command.supports_json && command.mutates)
        .count();

    let coverage = machine_contract_coverage();
    assert_eq!(coverage.catalog_commands_total, catalog.commands.len());
    assert_eq!(coverage.catalog_mutating_commands_total, catalog_mutating);
    assert_eq!(coverage.json_commands_total, catalog_json);
    assert_eq!(coverage.json_mutating_commands_total, mutating_json);
    assert_eq!(coverage.json_commands_with_schema, json_with_schema);
    assert!(
        coverage.json_commands_with_accepted_opaque_schema > 0,
        "remaining opaque schemas should be counted separately from concrete coverage"
    );
    assert_eq!(coverage.verified_scope, "everyday_and_agent");
    assert_eq!(coverage.advanced_scope, "advanced_internal_admin");
    assert!(
        coverage.verified_scope_json_commands_total > 0,
        "verified machine scope should include everyday and agent-facing JSON commands"
    );
    assert_eq!(
        coverage.verified_scope_json_commands_with_accepted_opaque_schema, 0,
        "verified machine scope must not rely on opaque schemas"
    );
    assert!(
        coverage.advanced_scope_json_commands_with_accepted_opaque_schema > 0,
        "advanced machine scope should report opaque schemas outside clean verification"
    );
    assert_eq!(
        coverage.verified_scope_json_commands_without_schema, 0,
        "verified machine scope must have registered schemas"
    );
    assert_eq!(coverage.mutating_commands_total, mutating_json);
    assert_eq!(coverage.supports_op_id_total, catalog_op_id);
    assert_eq!(coverage.jsonl_commands_total, catalog_jsonl);
    assert_eq!(coverage.json_commands_without_schema, 0);
    assert_eq!(coverage.mutating_commands_without_schema, 0);
}
