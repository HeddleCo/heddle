// SPDX-License-Identifier: Apache-2.0
//! Initialize command.

use std::{fs::OpenOptions, io::Write, path::PathBuf};

use anyhow::Result;
use repo::Repository;
use serde::Serialize;
use tracing::{debug, info};

use crate::{
    cli::{Cli, InitArgs, should_output_json},
    config::UserConfig,
};

#[derive(Serialize)]
struct InitOutput {
    path: PathBuf,
    message: String,
}

pub fn cmd_init(cli: &Cli, args: InitArgs) -> Result<()> {
    let path = match args.path.clone() {
        Some(p) => p,
        None => std::env::current_dir()
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
    let installed_heddleignore = maybe_install_default_heddleignore(repo.root())?;

    if args.principal_name.is_some() || args.principal_email.is_some() {
        let name = args
            .principal_name
            .clone()
            .ok_or_else(|| anyhow::anyhow!("--principal-name is required"))?;
        let email = args
            .principal_email
            .clone()
            .ok_or_else(|| anyhow::anyhow!("--principal-email is required"))?;
        let mut config = UserConfig::load_default()?;
        config.set_principal(name.clone(), email.clone());
        let config_path = config.save_default()?;
        info!(principal_name = %name, principal_email = %email, "Principal configured");
        debug!(config_path = %config_path.display(), "User config updated");
    }

    super::maybe_prompt_init_install(cli, &repo, &args)?;

    let mut message = if has_git {
        format!(
            "Initialized Heddle sidecar in {} for Git-overlay workflows",
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

    let output = InitOutput {
        path: repo.heddle_dir().to_path_buf(),
        message,
    };

    render_init(&output, should_output_json(cli, Some(repo.config())))
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
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut f) => {
            f.write_all(super::heddleignore_defaults::DEFAULT_HEDDLEIGNORE.as_bytes())
                .map_err(|e| anyhow::anyhow!("failed to write default .heddleignore: {}", e))?;
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
    }
    Ok(())
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
        assert_eq!(body, super::super::heddleignore_defaults::DEFAULT_HEDDLEIGNORE);
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
