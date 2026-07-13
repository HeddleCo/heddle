// SPDX-License-Identifier: Apache-2.0
//! Initialize command.

use std::{fs, path::PathBuf};

use anyhow::{Result, bail};
use heddle_core::{
    InitPrincipalPlan, OnboardingFacts, OnboardingMode,
    init_side_effects as core_init_side_effects, plan_repository_onboarding, resolve_absolute_path,
    select_init_principal,
};
use objects::object::Principal;
use repo::{Repository, RepositoryCapability};
use serde::Serialize;
use sley::Repository as SleyRepository;
use tracing::{debug, info};

use super::{
    RecoveryAdvice,
    action_line::print_next,
    snapshot::{is_placeholder_principal, placeholder_principal_warning},
    verification_health::{RepositoryVerificationState, build_repository_verification_state},
};
use crate::{
    cli::{Cli, InitArgs, should_output_json, style},
    config::UserConfig,
};

#[derive(Serialize)]
struct InitOutput {
    output_kind: &'static str,
    status: String,
    action: String,
    path: PathBuf,
    repository_mode: String,
    git_detected: bool,
    heddle_initialized: bool,
    installed_heddleignore: bool,
    principal_configured: bool,
    principal_status: String,
    principal_source: Option<String>,
    principal: Option<InitPrincipalOutput>,
    principal_recommended_action: Option<String>,
    #[serde(skip)]
    placeholder_principal_warning: Option<String>,
    side_effects: Vec<String>,
    message: String,
    next_action: Option<String>,
    recommended_action: Option<String>,
    #[allow(dead_code)]
    #[serde(skip_serializing)]
    #[serde(rename = "verification")]
    trust: RepositoryVerificationState,
}

#[derive(Serialize)]
struct InitPrincipalOutput {
    name: String,
    email: String,
}

pub fn cmd_init(cli: &Cli, args: InitArgs) -> Result<()> {
    let path = match (args.path.clone(), cli.repo.clone()) {
        (Some(positional), Some(repo_path)) => {
            if absolute_path(&positional)? != absolute_path(&repo_path)? {
                bail!(RecoveryAdvice::init_path_conflict(
                    &positional.display().to_string(),
                    &repo_path.display().to_string(),
                ));
            }
            positional
        }
        (Some(positional), None) => positional,
        (None, Some(repo_path)) => repo_path,
        (None, None) => std::env::current_dir()
            .map_err(|e| anyhow::anyhow!("Failed to determine current directory: {}", e))?,
    };
    let path = path.canonicalize().unwrap_or(path.clone());

    info!(path = %path.display(), "Initializing repository");

    let mut user_config = UserConfig::load_default()?;
    let principal = match (args.principal_name.clone(), args.principal_email.clone()) {
        (Some(name), Some(email)) => Some((name, email)),
        (Some(_), None) => {
            bail!(RecoveryAdvice::init_principal_field_required(
                "--principal-email"
            ))
        }
        (None, Some(_)) => {
            bail!(RecoveryAdvice::init_principal_field_required(
                "--principal-name"
            ))
        }
        (None, None) => None,
    };
    let staged_principal = if let Some((name, email)) = principal {
        user_config.set_principal(name.clone(), email.clone());
        Some((name, email, user_config.stage_default()?))
    } else {
        None
    };

    let git = SleyRepository::discover(&path).ok();
    let existing_repo = if path.join(".heddle").exists() {
        Some(Repository::open(&path)?)
    } else {
        None
    };
    let heddle_mode = existing_repo.as_ref().map(|repo| match repo.capability() {
        RepositoryCapability::GitOverlay => OnboardingMode::GitOverlay,
        RepositoryCapability::NativeHeddle => OnboardingMode::Native,
    });
    let onboarding = plan_repository_onboarding(OnboardingFacts {
        git_worktree: git.is_some(),
        git_has_commits: git
            .as_ref()
            .and_then(|repo| repo.head().ok())
            .and_then(|head| head.oid)
            .is_some(),
        heddle_mode,
    });
    let git_detected = git.is_some();

    let created_repository = existing_repo.is_none();
    let repo = match existing_repo {
        Some(repo) => repo,
        None => match onboarding.mode {
            OnboardingMode::GitOverlay => Repository::init_git_overlay_sidecar(&path)?,
            OnboardingMode::Native => Repository::init_default(&path)?,
        },
    };

    let principal_configured = staged_principal.is_some();
    if let Some((name, email, staged)) = staged_principal {
        match staged.publish() {
            Ok(config_path) => {
                info!(principal_name = %name, principal_email = %email, "Principal configured");
                debug!(config_path = %config_path.display(), "User config updated");
            }
            Err(error) => {
                let principal_published = UserConfig::load_default()
                    .ok()
                    .and_then(|config| config.principal)
                    .is_some_and(|principal| principal.name == name && principal.email == email);
                if created_repository && !principal_published {
                    rollback_new_repository(repo, &path).map_err(|rollback_error| {
                        anyhow::anyhow!(
                            "failed to save principal config: {error}; repository rollback also failed: {rollback_error}"
                        )
                    })?;
                }
                return Err(error);
            }
        }
    }

    if created_repository && onboarding.mode == OnboardingMode::GitOverlay {
        Repository::ensure_git_overlay_local_excludes(&path)?;
    }

    debug!(heddle_dir = %repo.heddle_dir().display(), "Repository initialized");

    let installed_heddleignore = false;

    super::maybe_prompt_init_install(cli, &repo, &args)?;

    let repo_is_git_overlay = onboarding.mode == OnboardingMode::GitOverlay;
    let message = if repo_is_git_overlay {
        format!(
            "Initialized Heddle data in {} for Git-overlay workflows",
            repo.heddle_dir().display()
        )
    } else {
        format!(
            "Initialized Heddle repository in {}",
            repo.heddle_dir().display()
        )
    };

    let trust = build_repository_verification_state(&repo);
    // Init must never end without a next step (heddle#644). When the repo has
    // existing Git history the trust state recommends the exact setup command;
    // when it doesn't, point at the first save — `heddle commit` records the
    // first state and, in Git-overlay repos, the matching Git checkpoint.
    let next_action = if !trust.recommended_action.is_empty() {
        Some(trust.recommended_action.clone())
    } else {
        Some("heddle commit -m \"...\"".to_string())
    };
    let principal_status = init_principal_status(&repo, &user_config)?;
    let placeholder_principal_warning = principal_status
        .principal
        .as_ref()
        .map(|principal| Principal::new(&principal.name, &principal.email))
        .filter(is_placeholder_principal)
        .map(|principal| placeholder_principal_warning(&principal));

    let output = InitOutput {
        output_kind: "init",
        status: "initialized".to_string(),
        action: "init".to_string(),
        path: repo.heddle_dir().to_path_buf(),
        repository_mode: repo.capability_label().to_string(),
        git_detected,
        heddle_initialized: true,
        installed_heddleignore,
        principal_configured,
        principal_status: principal_status.status,
        principal_source: principal_status.source,
        principal: principal_status.principal,
        principal_recommended_action: principal_status.recommended_action,
        placeholder_principal_warning,
        side_effects: init_side_effects(repo_is_git_overlay, principal_configured),
        message,
        next_action: next_action.clone(),
        recommended_action: next_action,
        trust,
    };

    render_init(&output, should_output_json(cli, Some(repo.config())))
}

fn rollback_new_repository(repo: Repository, root: &std::path::Path) -> Result<()> {
    let heddle_dir = repo.heddle_dir().to_path_buf();
    if heddle_dir != root.join(".heddle") {
        bail!("refusing to roll back non-root Heddle metadata");
    }
    drop(repo);
    fs::remove_dir_all(heddle_dir)?;
    Ok(())
}

fn absolute_path(path: &std::path::Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(resolve_absolute_path(std::path::Path::new(""), path))
    } else {
        let cwd = std::env::current_dir()
            .map_err(|e| anyhow::anyhow!("Failed to determine current directory: {}", e))?;
        Ok(resolve_absolute_path(&cwd, path))
    }
}

fn render_init(output: &InitOutput, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(output)?);
    } else {
        println!("{}", output.message);
        match output.principal.as_ref() {
            Some(principal) => {
                let source = output
                    .principal_source
                    .as_deref()
                    .map(|source| format!(" from {source}"))
                    .unwrap_or_default();
                println!(
                    "Principal: {} <{}>{source}",
                    principal.name, principal.email
                );
            }
            None => {
                println!("Principal: not configured");
                if let Some(action) = output.principal_recommended_action.as_deref() {
                    println!("  set with: {action}");
                }
            }
        }
        if let Some(warning) = output.placeholder_principal_warning.as_deref() {
            eprintln!("{}", style::warn(warning));
        }
        if !output.side_effects.is_empty() {
            println!("Side effects:");
            for effect in &output.side_effects {
                println!("  - {effect}");
            }
        }
        if let Some(next) = output.recommended_action.as_deref() {
            print_next(next);
        }
    }
    Ok(())
}

struct InitPrincipalStatus {
    status: String,
    source: Option<String>,
    principal: Option<InitPrincipalOutput>,
    recommended_action: Option<String>,
}

fn init_principal_status(
    repo: &Repository,
    user_config: &UserConfig,
) -> Result<InitPrincipalStatus> {
    let mut candidates: Vec<(&'static str, Principal)> = Vec::new();
    if let Some(principal) = Principal::from_env() {
        candidates.push(("environment", principal));
    }
    if let Some(config) = &repo.config().principal {
        candidates.push(("repository", Principal::new(&config.name, &config.email)));
    }
    if repo.capability() == RepositoryCapability::GitOverlay {
        candidates.push(("git_config", repo.get_principal()?));
    }
    if let Some(config) = &user_config.principal {
        candidates.push(("user_config", Principal::new(&config.name, &config.email)));
    }
    Ok(init_principal_status_from_plan(select_init_principal(
        &candidates,
    )))
}

fn init_principal_status_from_plan(plan: InitPrincipalPlan) -> InitPrincipalStatus {
    InitPrincipalStatus {
        status: plan.status.to_string(),
        source: plan.source.map(str::to_string),
        principal: match (plan.name, plan.email) {
            (Some(name), Some(email)) => Some(InitPrincipalOutput { name, email }),
            _ => None,
        },
        recommended_action: plan.recommended_action.map(str::to_string),
    }
}

fn init_side_effects(has_git: bool, principal_configured: bool) -> Vec<String> {
    core_init_side_effects(has_git, principal_configured)
}
