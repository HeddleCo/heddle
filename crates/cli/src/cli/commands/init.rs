// SPDX-License-Identifier: Apache-2.0
//! Initialize command.

use std::{fs::OpenOptions, io::Write, path::PathBuf};

use anyhow::{Result, bail};
use objects::object::Principal;
use repo::{Repository, RepositoryCapability};
use serde::Serialize;
use tracing::{debug, info};

use super::{
    RecoveryAdvice,
    git_overlay_health::{RepositoryVerificationState, build_repository_verification_state},
};
use crate::{
    cli::{Cli, InitArgs, should_output_json},
    config::UserConfig,
};

#[derive(Serialize)]
struct InitOutput {
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
    side_effects: Vec<String>,
    message: String,
    next_action: Option<String>,
    recommended_action: Option<String>,
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

    // If the directory already has a `.git` (or is inside one), leave the
    // `main` thread unseeded: the user almost certainly wants to import from
    // Git next, and pre-seeding would make `main` point at a throwaway
    // empty-tree snapshot. Otherwise, seed `main` so the repo is immediately
    // usable for snapshot/history/etc.
    let has_git = gix::discover(&path).is_ok();
    let repo = if has_git {
        Repository::bootstrap_git_overlay(&path)?
    } else {
        Repository::init_default(&path)?
    };

    debug!(heddle_dir = %repo.heddle_dir().display(), "Repository initialized");

    // Install the default `.heddleignore` if the repo doesn't ship
    // one yet. Auto-install (no prompt) is the explicit UX call: the
    // friction we're paying is `heddle merge` refusals on day-one
    // `.DS_Store` / `xcuserdata/` noise, and a prompt would just
    // delay that suppression to whenever the user noticed. The file
    // is plain text the user can edit or delete afterwards, so the
    // blast radius of "wrong choice" is small.
    let installed_heddleignore = if has_git {
        false
    } else {
        maybe_install_default_heddleignore(repo.root())?
    };

    let mut user_config = UserConfig::load_default()?;
    let mut principal_configured = false;
    if args.principal_name.is_some() || args.principal_email.is_some() {
        let name = args.principal_name.clone().ok_or_else(|| {
            anyhow::anyhow!(RecoveryAdvice::init_principal_field_required(
                "--principal-name"
            ))
        })?;
        let email = args.principal_email.clone().ok_or_else(|| {
            anyhow::anyhow!(RecoveryAdvice::init_principal_field_required(
                "--principal-email"
            ))
        })?;
        user_config.set_principal(name.clone(), email.clone());
        let config_path = user_config.save_default()?;
        info!(principal_name = %name, principal_email = %email, "Principal configured");
        debug!(config_path = %config_path.display(), "User config updated");
        principal_configured = true;
    }

    super::maybe_prompt_init_install(cli, &repo, &args)?;

    let mut message = if has_git {
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
    if installed_heddleignore {
        message.push_str("\nWrote default .heddleignore (edit to customize)");
    }

    let trust = build_repository_verification_state(&repo);
    let next_action =
        (!trust.recommended_action.is_empty()).then(|| trust.recommended_action.clone());
    let principal_status = init_principal_status(&repo, &user_config)?;
    let output = InitOutput {
        status: "initialized".to_string(),
        action: "init".to_string(),
        path: repo.heddle_dir().to_path_buf(),
        repository_mode: repo.capability_label().to_string(),
        git_detected: has_git,
        heddle_initialized: true,
        installed_heddleignore,
        principal_configured,
        principal_status: principal_status.status,
        principal_source: principal_status.source,
        principal: principal_status.principal,
        principal_recommended_action: principal_status.recommended_action,
        side_effects: init_side_effects(has_git, installed_heddleignore, principal_configured),
        message,
        next_action: next_action.clone(),
        recommended_action: next_action,
        trust,
    };

    render_init(&output, should_output_json(cli, Some(repo.config())))
}

fn absolute_path(path: &std::path::Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()
            .map_err(|e| anyhow::anyhow!("Failed to determine current directory: {}", e))?
            .join(path))
    }
}

/// Write the bundled default `.heddleignore` into the worktree root
/// if (and only if) no `.heddleignore` already exists there. Returns
/// whether a write actually happened so the caller can surface a
/// single-line notice to the user.
///
/// Failure to write is non-fatal: a freshly-initialized repo without
/// the default template is still a valid Heddle repo, and a noisy
/// failure here would block init for paths the user can recreate by
/// hand. We propagate I/O errors only for the genuinely unexpected
/// cases (permission denied with the file *not* present, etc.).
pub(crate) fn maybe_install_default_heddleignore(root: &std::path::Path) -> Result<bool> {
    let path = root.join(".heddleignore");
    // Atomic create-or-fail: the prior `path.exists()` + `fs::write`
    // shape was a TOCTOU window where a concurrent `heddle init` (or
    // a user dropping their own `.heddleignore` between the two
    // syscalls) could see "absent" and then have its file silently
    // overwritten. `O_CREAT | O_EXCL` collapses the check and the
    // write into one kernel-enforced step.
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut f) => {
            // If `write_all` fails after `create_new` already
            // landed an empty file (ENOSPC, EIO, ...), a naïve
            // bail-out would leave the zero-byte file on disk —
            // and a retried `heddle init` would then hit the
            // `AlreadyExists` arm and silently report success
            // without ever installing the template. Remove the
            // partial file so the retry path can recreate it.
            if let Err(e) =
                f.write_all(super::heddleignore_defaults::DEFAULT_HEDDLEIGNORE.as_bytes())
            {
                drop(f);
                let _ = std::fs::remove_file(&path);
                return Err(anyhow::anyhow!(
                    "failed to write default .heddleignore: {}",
                    e
                ));
            }
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(anyhow::anyhow!(
            "failed to create default .heddleignore: {}",
            e
        )),
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
        if !output.side_effects.is_empty() {
            println!("Side effects:");
            for effect in &output.side_effects {
                println!("  - {effect}");
            }
        }
        if let Some(next) = output.recommended_action.as_deref() {
            println!("Next: {next}");
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
    if let Some(principal) = Principal::from_env()
        && !principal_is_unconfigured(&principal)
    {
        return Ok(configured_principal_status("environment", principal));
    }

    if let Some(config) = &repo.config().principal {
        let principal = Principal::new(&config.name, &config.email);
        if !principal_is_unconfigured(&principal) {
            return Ok(configured_principal_status("repository", principal));
        }
    }

    if repo.capability() == RepositoryCapability::GitOverlay {
        let principal = repo.get_principal()?;
        if !principal_is_unconfigured(&principal) {
            return Ok(configured_principal_status("git_config", principal));
        }
    }

    if let Some(config) = &user_config.principal {
        let principal = Principal::new(&config.name, &config.email);
        if !principal_is_unconfigured(&principal) {
            return Ok(configured_principal_status("user_config", principal));
        }
    }

    Ok(InitPrincipalStatus {
        status: "not_configured".to_string(),
        source: None,
        principal: None,
        recommended_action: Some(set_principal_command().to_string()),
    })
}

fn configured_principal_status(source: &str, principal: Principal) -> InitPrincipalStatus {
    InitPrincipalStatus {
        status: "configured".to_string(),
        source: Some(source.to_string()),
        principal: Some(InitPrincipalOutput {
            name: principal.name,
            email: principal.email,
        }),
        recommended_action: None,
    }
}

fn principal_is_unconfigured(principal: &Principal) -> bool {
    principal.name.trim().is_empty()
        || principal.email.trim().is_empty()
        || (principal.name.trim() == "Unknown" && principal.email.trim() == "unknown@example.com")
}

fn set_principal_command() -> &'static str {
    "heddle init --principal-name <name> --principal-email <email>"
}

fn init_side_effects(
    has_git: bool,
    installed_heddleignore: bool,
    principal_configured: bool,
) -> Vec<String> {
    let mut side_effects = Vec::new();
    if has_git {
        side_effects.push("created Heddle sidecar for the existing Git repository".to_string());
        side_effects.push("left Git-tracked files and `git status --short` untouched".to_string());
    } else {
        side_effects.push("created Heddle repository metadata".to_string());
        if installed_heddleignore {
            side_effects.push("wrote default .heddleignore".to_string());
        }
    }
    if principal_configured {
        side_effects.push("updated default principal attribution".to_string());
    }
    side_effects
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_writes_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let wrote = maybe_install_default_heddleignore(dir.path()).unwrap();
        assert!(wrote);
        let body = std::fs::read_to_string(dir.path().join(".heddleignore")).unwrap();
        assert_eq!(
            body,
            super::super::heddleignore_defaults::DEFAULT_HEDDLEIGNORE
        );
    }

    #[test]
    fn install_preserves_existing_via_create_new() {
        // The atomic `O_CREAT | O_EXCL` path must NOT overwrite a
        // curated `.heddleignore`. Pre-create one with custom content,
        // then confirm `maybe_install_default_heddleignore` returns
        // `false` and leaves the body untouched.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".heddleignore");
        let curated = "# curated\nsecrets/\n";
        std::fs::write(&path, curated).unwrap();

        let wrote = maybe_install_default_heddleignore(dir.path()).unwrap();
        assert!(!wrote);
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body, curated);
    }
}
