// SPDX-License-Identifier: Apache-2.0
//! Shared repository trust contract.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
};

use objects::worktree::WorktreeStatus;
use repo::{GitOverlayBranchTip, GitOverlayImportHint, GitRemoteTrackingStatus, Repository};
use schemars::JsonSchema;
use serde::Serialize;

use crate::{cli::worktree_status_options, remote::RemoteConfig};

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub(crate) struct GitOverlayHealth {
    pub status: String,
    pub clean: bool,
    pub summary: String,
    pub recovery_commands: Vec<String>,
    pub checks: Vec<GitOverlayHealthCheck>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub(crate) struct GitOverlayHealthCheck {
    pub name: String,
    pub status: String,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub(crate) struct RepositoryTrustState {
    pub trusted: bool,
    pub status: String,
    pub repository_mode: String,
    pub heddle_initialized: bool,
    pub git_branch: Option<String>,
    pub heddle_thread: Option<String>,
    pub worktree_dirty: bool,
    pub import_state: String,
    pub mapping_state: String,
    pub remote_drift: String,
    pub active_operation: Option<String>,
    pub default_remote: Option<String>,
    pub clone_verification: String,
    pub machine_contract: String,
    pub summary: String,
    pub recommended_action: String,
    pub recovery_commands: Vec<String>,
    pub checks: Vec<TrustCheck>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub(crate) struct TrustCheck {
    pub name: String,
    pub status: String,
    pub clean: bool,
    pub summary: String,
    pub recommended_action: Option<String>,
    pub recovery_commands: Vec<String>,
    #[serde(default)]
    pub details: BTreeMap<String, String>,
}

#[derive(Debug)]
pub(crate) struct PlainGitTrustProbe {
    pub root: PathBuf,
    pub git_branch: Option<String>,
    pub git_branches: Vec<String>,
    pub changes: WorktreeStatus,
    pub trust: RepositoryTrustState,
}

impl GitOverlayHealth {
    pub(crate) fn clean(summary: impl Into<String>, checks: Vec<GitOverlayHealthCheck>) -> Self {
        Self {
            status: "clean".to_string(),
            clean: true,
            summary: summary.into(),
            recovery_commands: Vec::new(),
            checks,
        }
    }

    pub(crate) fn primary_recovery_command(&self) -> Option<&str> {
        self.recovery_commands.first().map(String::as_str)
    }
}

impl RepositoryTrustState {
    pub(crate) fn from_health(repo: &Repository, health: GitOverlayHealth) -> Self {
        let git_branch = repo.git_overlay_current_branch().ok().flatten();
        let heddle_thread = repo.current_lane().ok().flatten();
        let active_operation = repo.operation_status().ok().flatten().map(|operation| {
            format!(
                "{} {} ({})",
                operation.scope, operation.kind, operation.state
            )
        });
        let remote_drift = repo
            .git_remote_tracking_status()
            .ok()
            .flatten()
            .map(|remote| {
                if remote.ahead == 0 && remote.behind == 0 {
                    "clean".to_string()
                } else {
                    health.status.clone()
                }
            })
            .unwrap_or_else(|| "clean".to_string());
        let import_state = health
            .checks
            .iter()
            .find(|check| check.name == "import")
            .map(|check| check.status.clone())
            .unwrap_or_else(|| "clean".to_string());
        let mapping_state = health
            .checks
            .iter()
            .find(|check| check.name == "head_mapping")
            .map(|check| check.status.clone())
            .unwrap_or_else(|| "clean".to_string());
        let worktree_dirty = health
            .checks
            .iter()
            .any(|check| check.status == "dirty_worktree");
        let recommended_action = health
            .primary_recovery_command()
            .unwrap_or(if health.clean { "" } else { "heddle doctor" })
            .to_string();
        let is_git_overlay = repo.capability() == repo::RepositoryCapability::GitOverlay;
        let checks = trust_checks_from_health(&health, &recommended_action, is_git_overlay);
        Self {
            trusted: health.clean,
            status: health.status.clone(),
            repository_mode: repo.capability_label().to_string(),
            heddle_initialized: true,
            git_branch,
            heddle_thread,
            worktree_dirty,
            import_state,
            mapping_state,
            remote_drift,
            active_operation,
            default_remote: default_remote_name(repo),
            clone_verification: "not_applicable".to_string(),
            machine_contract: "available".to_string(),
            summary: health.summary.clone(),
            recommended_action,
            recovery_commands: health.recovery_commands.clone(),
            checks,
        }
    }
}

fn trust_checks_from_health(
    health: &GitOverlayHealth,
    recommended_action: &str,
    is_git_overlay: bool,
) -> Vec<TrustCheck> {
    vec![
        if is_git_overlay {
            trust_check(
                "Git",
                true,
                "clean",
                "Git overlay repository is present",
                None,
                Vec::new(),
            )
        } else {
            trust_check(
                "Git",
                true,
                "not_applicable",
                "repository is not using the Git overlay",
                None,
                Vec::new(),
            )
        },
        trust_check(
            "Heddle",
            true,
            "clean",
            "Heddle sidecar is initialized",
            None,
            Vec::new(),
        ),
        mapping_trust_check(health, recommended_action, is_git_overlay),
        worktree_trust_check(health, recommended_action),
        remote_trust_check(health, recommended_action),
        operation_trust_check(health, recommended_action),
        trust_check(
            "Machine contract",
            true,
            "available",
            "command catalog, JSON, and schema contracts are available",
            None,
            Vec::new(),
        ),
        trust_check(
            "Clone",
            true,
            "not_applicable",
            "clone verification is not applicable to this checkout",
            None,
            Vec::new(),
        ),
    ]
}

fn mapping_trust_check(
    health: &GitOverlayHealth,
    recommended_action: &str,
    is_git_overlay: bool,
) -> TrustCheck {
    if !is_git_overlay {
        return trust_check(
            "Mapping",
            true,
            "not_applicable",
            "Git/Heddle mapping is not applicable outside Git overlay mode",
            None,
            Vec::new(),
        );
    }
    if let Some(import) = find_health_check(health, "import")
        && import.status != "clean"
    {
        return trust_check_from_health("Mapping", import, recommended_action, health);
    }
    if let Some(mapping) = find_health_check(health, "head_mapping") {
        return trust_check_from_health("Mapping", mapping, recommended_action, health);
    }
    if let Some(import) = find_health_check(health, "import") {
        return trust_check_from_health("Mapping", import, recommended_action, health);
    }
    trust_check(
        "Mapping",
        true,
        "clean",
        "Git branch tips map to imported Heddle state",
        None,
        Vec::new(),
    )
}

fn worktree_trust_check(health: &GitOverlayHealth, recommended_action: &str) -> TrustCheck {
    for name in ["worktree", "heddle_worktree"] {
        if let Some(check) = find_health_check(health, name)
            && check.status != "clean"
        {
            return trust_check_from_health("Worktree", check, recommended_action, health);
        }
    }
    if let Some(check) = find_health_check(health, "worktree") {
        return trust_check_from_health("Worktree", check, recommended_action, health);
    }
    trust_check(
        "Worktree",
        true,
        "clean",
        "worktree has no uncommitted Git/Heddle disagreement",
        None,
        Vec::new(),
    )
}

fn remote_trust_check(health: &GitOverlayHealth, recommended_action: &str) -> TrustCheck {
    if let Some(check) = find_health_check(health, "remote_tracking") {
        return trust_check_from_health("Remote", check, recommended_action, health);
    }
    trust_check(
        "Remote",
        true,
        "clean",
        "no unresolved remote drift detected",
        None,
        Vec::new(),
    )
}

fn operation_trust_check(health: &GitOverlayHealth, recommended_action: &str) -> TrustCheck {
    if let Some(check) = find_health_check(health, "operation") {
        return trust_check_from_health("Operation", check, recommended_action, health);
    }
    trust_check(
        "Operation",
        true,
        "clean",
        "no Git or Heddle operation in progress",
        None,
        Vec::new(),
    )
}

fn trust_check_from_health(
    public_name: &str,
    check: &GitOverlayHealthCheck,
    recommended_action: &str,
    health: &GitOverlayHealth,
) -> TrustCheck {
    let clean = check.status == "clean";
    trust_check(
        public_name,
        clean,
        &check.status,
        &check.summary,
        (!clean && !recommended_action.is_empty()).then(|| recommended_action.to_string()),
        if clean {
            Vec::new()
        } else {
            health.recovery_commands.clone()
        },
    )
}

fn find_health_check<'a>(
    health: &'a GitOverlayHealth,
    name: &str,
) -> Option<&'a GitOverlayHealthCheck> {
    health.checks.iter().find(|check| check.name == name)
}

fn trust_check(
    name: &str,
    clean: bool,
    status: &str,
    summary: &str,
    recommended_action: Option<String>,
    recovery_commands: Vec<String>,
) -> TrustCheck {
    TrustCheck {
        name: name.to_string(),
        status: status.to_string(),
        clean,
        summary: summary.to_string(),
        recommended_action,
        recovery_commands,
        details: BTreeMap::new(),
    }
}

pub(crate) fn build_repository_trust_state(repo: &Repository) -> RepositoryTrustState {
    let health = build_git_overlay_health(repo);
    RepositoryTrustState::from_health(repo, health)
}

pub(crate) fn build_plain_git_trust_probe(
    start: &Path,
) -> anyhow::Result<Option<PlainGitTrustProbe>> {
    let root_output = Command::new("git")
        .arg("-C")
        .arg(start)
        .args(["rev-parse", "--show-toplevel"])
        .output();
    let root_output = match root_output {
        Ok(output) if output.status.success() => output,
        _ => return Ok(None),
    };
    let root = PathBuf::from(
        String::from_utf8_lossy(&root_output.stdout)
            .trim()
            .to_string(),
    );
    if root.join(".heddle").exists() {
        return Ok(None);
    }

    let git_branch = git_probe_stdout(&root, &["branch", "--show-current"])?
        .map(|branch| branch.trim().to_string())
        .filter(|branch| !branch.is_empty());
    let git_branches = git_probe_stdout(&root, &["branch", "--format", "%(refname:short)"])?
        .map(|stdout| {
            stdout
                .lines()
                .map(str::trim)
                .filter(|branch| !branch.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut changes = WorktreeStatus::default();
    if let Some(stdout) =
        git_probe_stdout(&root, &["status", "--porcelain", "--untracked-files=all"])?
    {
        for line in stdout.lines() {
            if line.len() < 3 {
                continue;
            }
            let code = &line[..2];
            let raw_path = &line[3..];
            if code.contains('R') {
                if let Some((old_path, new_path)) = raw_path.split_once(" -> ") {
                    changes.deleted.push(PathBuf::from(old_path));
                    changes.added.push(PathBuf::from(new_path));
                } else {
                    changes.modified.push(PathBuf::from(raw_path));
                }
            } else if code == "??" || code.contains('A') {
                changes.added.push(PathBuf::from(raw_path));
            } else if code.contains('D') {
                changes.deleted.push(PathBuf::from(raw_path));
            } else {
                changes.modified.push(PathBuf::from(raw_path));
            }
        }
    }

    let default_remote = git_default_remote_name(&root);
    let import = git_branch
        .as_ref()
        .map(|branch| format!("heddle bridge git import --ref {branch}"))
        .unwrap_or_else(|| "heddle bridge git import".to_string());
    let mut details = BTreeMap::new();
    details.insert("path".to_string(), root.display().to_string());
    if let Some(branch) = &git_branch {
        details.insert("git_branch".to_string(), branch.clone());
    }
    if let Some(remote) = &default_remote {
        details.insert("default_remote".to_string(), remote.clone());
    }
    let mut checks = vec![
        TrustCheck {
            name: "Git".to_string(),
            status: "present".to_string(),
            clean: true,
            summary: "plain Git repository found".to_string(),
            recommended_action: None,
            recovery_commands: Vec::new(),
            details,
        },
        TrustCheck {
            name: "Heddle".to_string(),
            status: "needs_init".to_string(),
            clean: false,
            summary: "Heddle sidecar is not initialized".to_string(),
            recommended_action: Some("heddle init".to_string()),
            recovery_commands: vec!["heddle init".to_string()],
            details: BTreeMap::new(),
        },
        TrustCheck {
            name: "Mapping".to_string(),
            status: "needs_import".to_string(),
            clean: false,
            summary: "Git history has not been imported into Heddle".to_string(),
            recommended_action: Some(import.clone()),
            recovery_commands: vec![import.clone()],
            details: BTreeMap::new(),
        },
    ];
    checks.push(trust_check(
        "Worktree",
        changes.is_clean(),
        if changes.is_clean() {
            "clean"
        } else {
            "dirty_worktree"
        },
        if changes.is_clean() {
            "Git worktree is clean"
        } else {
            "Git worktree has uncommitted changes"
        },
        None,
        Vec::new(),
    ));
    checks.push(trust_check(
        "Remote",
        false,
        "unknown",
        "remote drift is checked after Heddle initialization",
        None,
        Vec::new(),
    ));
    checks.push(trust_check(
        "Operation",
        true,
        "clean",
        "no Heddle operation in progress",
        None,
        Vec::new(),
    ));
    checks.push(trust_check(
        "Machine contract",
        true,
        "available",
        "command catalog, JSON, and schema contracts are available",
        None,
        Vec::new(),
    ));
    checks.push(trust_check(
        "Clone",
        true,
        "not_applicable",
        "clone verification is not applicable to this checkout",
        None,
        Vec::new(),
    ));
    let trust = RepositoryTrustState {
        trusted: false,
        status: "needs_init".to_string(),
        repository_mode: "plain-git".to_string(),
        heddle_initialized: false,
        git_branch: git_branch.clone(),
        heddle_thread: None,
        worktree_dirty: !changes.is_clean(),
        import_state: "needs_init".to_string(),
        mapping_state: "needs_init".to_string(),
        remote_drift: "unknown".to_string(),
        active_operation: None,
        default_remote,
        clone_verification: "not_applicable".to_string(),
        machine_contract: "available".to_string(),
        summary: "Git repository has not been initialized for Heddle".to_string(),
        recommended_action: "heddle init".to_string(),
        recovery_commands: vec!["heddle init".to_string(), import],
        checks,
    };
    Ok(Some(PlainGitTrustProbe {
        root,
        git_branch,
        git_branches,
        changes,
        trust,
    }))
}

fn default_remote_name(repo: &Repository) -> Option<String> {
    RemoteConfig::open(repo)
        .ok()
        .and_then(|cfg| cfg.default_name().map(str::to_string))
        .or_else(|| {
            (repo.capability() == repo::RepositoryCapability::GitOverlay)
                .then(|| git_default_remote_name(repo.root()))
                .flatten()
        })
}

fn git_default_remote_name(root: &Path) -> Option<String> {
    let stdout = git_probe_stdout(root, &["remote"]).ok().flatten()?;
    let mut remotes = stdout
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty());
    remotes.find(|name| *name == "origin").map(str::to_string)
}

fn git_probe_stdout(root: &Path, args: &[&str]) -> anyhow::Result<Option<String>> {
    let output = Command::new("git").arg("-C").arg(root).args(args).output();
    match output {
        Ok(output) if output.status.success() => {
            Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()))
        }
        Ok(_) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn build_git_overlay_health(repo: &Repository) -> GitOverlayHealth {
    if repo.capability() != repo::RepositoryCapability::GitOverlay {
        return GitOverlayHealth::clean(
            "Repository is not using the Git overlay",
            vec![GitOverlayHealthCheck {
                name: "capability".to_string(),
                status: "clean".to_string(),
                summary: "native Heddle repository".to_string(),
            }],
        );
    }

    let mut checks = Vec::new();

    match repo.operation_status() {
        Ok(Some(operation)) => {
            checks.push(GitOverlayHealthCheck {
                name: "operation".to_string(),
                status: "operation_in_progress".to_string(),
                summary: operation.message.clone(),
            });
            return GitOverlayHealth {
                status: "operation_in_progress".to_string(),
                clean: false,
                summary: operation.message,
                recovery_commands: vec![operation.next_action],
                checks,
            };
        }
        Ok(None) => checks.push(GitOverlayHealthCheck {
            name: "operation".to_string(),
            status: "clean".to_string(),
            summary: "no Git or Heddle operation in progress".to_string(),
        }),
        Err(error) => {
            checks.push(GitOverlayHealthCheck {
                name: "operation".to_string(),
                status: "degraded".to_string(),
                summary: error.to_string(),
            });
            return degraded(checks, "Could not inspect in-progress operations");
        }
    }

    match repo.git_overlay_worktree_status() {
        Ok(Some(status)) if !status.is_clean() => {
            let changed = status.modified.len() + status.added.len() + status.deleted.len();
            checks.push(GitOverlayHealthCheck {
                name: "worktree".to_string(),
                status: "dirty_worktree".to_string(),
                summary: format!("{changed} Git worktree path(s) have uncommitted changes"),
            });
            return GitOverlayHealth {
                status: "dirty_worktree".to_string(),
                clean: false,
                summary: format!("{changed} Git worktree path(s) have uncommitted changes"),
                recovery_commands: vec![
                    "heddle capture".to_string(),
                    "heddle stash push -m \"...\"".to_string(),
                ],
                checks,
            };
        }
        Ok(Some(_)) => checks.push(GitOverlayHealthCheck {
            name: "worktree".to_string(),
            status: "clean".to_string(),
            summary: "Git worktree is clean".to_string(),
        }),
        Ok(None) => checks.push(GitOverlayHealthCheck {
            name: "worktree".to_string(),
            status: "clean".to_string(),
            summary: "Git worktree status is not available; Heddle status remains authoritative"
                .to_string(),
        }),
        Err(error) => {
            checks.push(GitOverlayHealthCheck {
                name: "worktree".to_string(),
                status: "degraded".to_string(),
                summary: error.to_string(),
            });
            return degraded(checks, "Could not inspect Git worktree status");
        }
    }

    if let Ok(Some(state)) = repo.current_state()
        && let Ok(tree) = repo.require_tree(&state.tree)
        && let Ok(status) = repo.compare_worktree_cached_with_options(
            &tree,
            &worktree_status_options(Some(repo.config())),
        )
        && !status.is_clean()
    {
        let changed = status.modified.len() + status.added.len() + status.deleted.len();
        checks.push(GitOverlayHealthCheck {
            name: "heddle_worktree".to_string(),
            status: "dirty_worktree".to_string(),
            summary: format!("{changed} Heddle worktree path(s) differ from the current state"),
        });
        return GitOverlayHealth {
            status: "dirty_worktree".to_string(),
            clean: false,
            summary: format!("{changed} Heddle worktree path(s) differ from the current state"),
            recovery_commands: vec![
                "heddle capture".to_string(),
                "heddle stash push -m \"...\"".to_string(),
            ],
            checks,
        };
    }

    match repo.git_overlay_import_hint() {
        Ok(Some(hint)) => return needs_import(checks, hint),
        Ok(None) => checks.push(GitOverlayHealthCheck {
            name: "import".to_string(),
            status: "clean".to_string(),
            summary: "Git branch tips have been imported into Heddle".to_string(),
        }),
        Err(error) => {
            checks.push(GitOverlayHealthCheck {
                name: "import".to_string(),
                status: "degraded".to_string(),
                summary: error.to_string(),
            });
            return degraded(checks, "Could not inspect Git import state");
        }
    }

    match repo.git_remote_tracking_status() {
        Ok(Some(remote)) => return remote_drift(checks, remote),
        Ok(None) => checks.push(GitOverlayHealthCheck {
            name: "remote_tracking".to_string(),
            status: "clean".to_string(),
            summary: "No Git upstream drift detected".to_string(),
        }),
        Err(error) => {
            checks.push(GitOverlayHealthCheck {
                name: "remote_tracking".to_string(),
                status: "degraded".to_string(),
                summary: error.to_string(),
            });
            return degraded(checks, "Could not inspect Git upstream drift");
        }
    }

    match current_branch_tip(repo) {
        Ok(Some(tip)) if !tip.history_imported => {
            checks.push(GitOverlayHealthCheck {
                name: "head_mapping".to_string(),
                status: "git_heddle_mismatch".to_string(),
                summary: format!(
                    "Git branch '{}' points at commit {} that is not mapped to the active Heddle state",
                    tip.branch, tip.git_commit
                ),
            });
            return GitOverlayHealth {
                status: "needs_import".to_string(),
                clean: false,
                summary: format!(
                    "Git branch '{}' points at a commit that has not been imported into Heddle",
                    tip.branch
                ),
                recovery_commands: vec![format!("heddle bridge git import --ref {}", tip.branch)],
                checks,
            };
        }
        Ok(Some(tip)) => checks.push(GitOverlayHealthCheck {
            name: "head_mapping".to_string(),
            status: "clean".to_string(),
            summary: format!("Git branch '{}' maps to imported Heddle state", tip.branch),
        }),
        Ok(None) => checks.push(GitOverlayHealthCheck {
            name: "head_mapping".to_string(),
            status: "clean".to_string(),
            summary: "No attached Git branch to map".to_string(),
        }),
        Err(error) => {
            checks.push(GitOverlayHealthCheck {
                name: "head_mapping".to_string(),
                status: "degraded".to_string(),
                summary: error.to_string(),
            });
            return degraded(checks, "Could not inspect Git/Heddle branch mapping");
        }
    }

    GitOverlayHealth::clean("Git overlay and Heddle agree", checks)
}

fn needs_import(
    mut checks: Vec<GitOverlayHealthCheck>,
    hint: GitOverlayImportHint,
) -> GitOverlayHealth {
    checks.push(GitOverlayHealthCheck {
        name: "import".to_string(),
        status: "needs_import".to_string(),
        summary: format!(
            "{} Git branch tip(s) still need Heddle import",
            hint.missing_branch_count
        ),
    });
    GitOverlayHealth {
        status: "needs_import".to_string(),
        clean: false,
        summary: format!(
            "{} Git branch tip(s) still need Heddle import",
            hint.missing_branch_count
        ),
        recovery_commands: vec![hint.recommended_command],
        checks,
    }
}

fn remote_drift(
    mut checks: Vec<GitOverlayHealthCheck>,
    remote: GitRemoteTrackingStatus,
) -> GitOverlayHealth {
    let status = match (remote.ahead, remote.behind) {
        (0, _) => "remote_behind",
        (_, 0) => "remote_ahead",
        _ => "remote_diverged",
    };
    let command = match (remote.ahead, remote.behind) {
        (0, _) => "heddle pull".to_string(),
        (_, 0) => "heddle push".to_string(),
        _ => "heddle sync".to_string(),
    };
    checks.push(GitOverlayHealthCheck {
        name: "remote_tracking".to_string(),
        status: status.to_string(),
        summary: remote.message.clone(),
    });
    GitOverlayHealth {
        status: status.to_string(),
        clean: false,
        summary: remote.message,
        recovery_commands: vec![command],
        checks,
    }
}

fn degraded(mut checks: Vec<GitOverlayHealthCheck>, summary: &str) -> GitOverlayHealth {
    checks.push(GitOverlayHealthCheck {
        name: "contract".to_string(),
        status: "degraded".to_string(),
        summary: "health could not be proven clean".to_string(),
    });
    GitOverlayHealth {
        status: "degraded".to_string(),
        clean: false,
        summary: summary.to_string(),
        recovery_commands: vec!["heddle doctor".to_string()],
        checks,
    }
}

fn current_branch_tip(repo: &Repository) -> anyhow::Result<Option<GitOverlayBranchTip>> {
    let Some(branch) = repo.git_overlay_current_branch()? else {
        return Ok(None);
    };
    repo.git_overlay_branch_tip(&branch).map_err(Into::into)
}
