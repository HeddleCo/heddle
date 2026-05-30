// SPDX-License-Identifier: Apache-2.0
//! Initialize command.

use std::{
    io::{self, Write},
    path::{Path, PathBuf},
};

use anyhow::{Result, bail};
use objects::object::{Principal, ThreadName, Tree};
use refs::Head;
use repo::{Repository, RepositoryCapability};
use serde::Serialize;
use tracing::{debug, info};

use super::{
    RecoveryAdvice,
    action_line::print_next,
    checkpoint::create_git_checkpoint,
    git_overlay_health::{RepositoryVerificationState, build_repository_verification_state},
    snapshot::{SnapshotAgentOverrides, create_snapshot},
};
use crate::{
    bridge::{
        GitBridge, git_core::git_config_identity_with_global_fallback, git_import::import_all,
    },
    cli::{Cli, InitArgs, is_tty, should_output_json, style, worktree_status_options},
    config::UserConfig,
};

/// Short pointer file Heddle writes (and captures) when `--quickstart`
/// runs in a directory with no capturable user files yet, so the first
/// `heddle log` has a user-visible state to show.
const QUICKSTART_PLACEHOLDER: &str = "\
# Quickstart

This repository was bootstrapped with `heddle init --quickstart`.

Heddle captured this file as your first state so `heddle log` has
something to show. Replace it with your own work and run
`heddle capture -m \"...\"` to record your next step.

Next:
  heddle log       # see the history Heddle is tracking
  heddle status    # check what changed
";

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
    side_effects: Vec<String>,
    message: String,
    next_action: Option<String>,
    recommended_action: Option<String>,
    /// Quickstart actions (thread/capture/checkpoint). Render-only;
    /// excluded from the JSON contract so the `init` output schema is
    /// unchanged whether or not `--quickstart` was passed.
    #[serde(skip)]
    quickstart: Option<QuickstartSummary>,
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

/// What `--quickstart` did after the normal init steps.
struct QuickstartSummary {
    thread: String,
    change_id: String,
    git_commit: Option<String>,
    wrote_placeholder: bool,
}

/// Outcome of the pre-write quickstart phase (confirmation + identity).
/// Resolved before any filesystem write so a Ctrl-C at a prompt leaves
/// the directory exactly as it was found.
struct QuickstartPreflight {
    proceed: bool,
    persist_principal: Option<(String, String)>,
    /// Harnesses the user agreed to connect, decided at the prompt before
    /// any write. Installed only after the repo is created so a Ctrl-C at
    /// the harness prompt leaves the directory untouched.
    harness_install: Vec<String>,
}

impl Default for QuickstartPreflight {
    fn default() -> Self {
        Self {
            proceed: true,
            persist_principal: None,
            harness_install: Vec::new(),
        }
    }
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

    // Quickstart confirms and resolves identity BEFORE any write so a
    // Ctrl-C (or declined prompt) leaves the directory untouched — no
    // half-written `.heddle/`.
    let preflight = if args.quickstart {
        quickstart_preflight(cli, &args, &path, has_git)?
    } else {
        QuickstartPreflight::default()
    };
    if !preflight.proceed {
        return Ok(());
    }

    let repo = if args.quickstart && path.join(".heddle").exists() {
        // Confirmed (or `--yes`) quickstart over a directory that already
        // has Heddle data: open it and run the quickstart actions rather
        // than re-initializing (which would refuse).
        Repository::open(&path)?
    } else if has_git {
        Repository::bootstrap_git_overlay(&path)?
    } else {
        Repository::init_default(&path)?
    };

    debug!(heddle_dir = %repo.heddle_dir().display(), "Repository initialized");

    let installed_heddleignore = false;

    let mut user_config = UserConfig::load_default()?;
    let mut principal_configured = false;
    // Quickstart writes the resolved identity to the *repo* config
    // (`.heddle/config.toml`) rather than the global user config: the
    // flag/prompt identity must win over an ambient Git `user.*`, and
    // `resolve_principal`'s precedence ranks repo config above Git
    // config but ranks user config below it. Re-open the repo afterwards
    // so the in-memory `repo.config()` the capture reads reflects it.
    let mut repo = repo;
    if args.quickstart {
        if let Some((name, email)) = &preflight.persist_principal {
            let config_path = repo.heddle_dir().join("config.toml");
            let mut repo_config = repo::RepoConfig::load(&config_path).unwrap_or_default();
            repo_config.set_principal(name.clone(), email.clone());
            repo_config.save(&config_path)?;
            info!(principal_name = %name, principal_email = %email, "Principal configured");
            debug!(config_path = %config_path.display(), "Repo config updated");
            repo = Repository::open(&path)?;
            principal_configured = true;
        }
    } else if args.principal_name.is_some() || args.principal_email.is_some() {
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

    if args.quickstart {
        // Decision was made up front in the preflight; only the install
        // (a write) runs here, after the repo exists.
        super::perform_init_install(cli, &repo, &args, &preflight.harness_install)?;
    } else {
        super::maybe_prompt_init_install(cli, &repo, &args)?;
    }

    let quickstart = if args.quickstart {
        Some(run_quickstart_actions(&repo, &args)?)
    } else {
        None
    };

    let message = if has_git {
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
    // After a quickstart the user has a captured state to inspect, so
    // point them at `heddle log` regardless of the trust-derived action.
    let next_action = if quickstart.is_some() {
        Some("heddle log".to_string())
    } else {
        (!trust.recommended_action.is_empty()).then(|| trust.recommended_action.clone())
    };
    let principal_status = init_principal_status(&repo, &user_config)?;
    let output = InitOutput {
        output_kind: "init",
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
        side_effects: init_side_effects(has_git, principal_configured),
        message,
        next_action: next_action.clone(),
        recommended_action: next_action,
        quickstart,
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
        if let Some(quickstart) = output.quickstart.as_ref() {
            if quickstart.wrote_placeholder {
                println!(
                    "Wrote {} and captured it as your first state.",
                    style::accent("QUICKSTART.md")
                );
            }
            println!("Thread: {}", style::bold(&quickstart.thread));
            println!("Captured: {}", style::change_id(&quickstart.change_id));
            if let Some(commit) = quickstart.git_commit.as_deref() {
                println!(
                    "Checkpoint: {}",
                    style::dim(&commit[..commit.len().min(12)])
                );
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

fn init_side_effects(has_git: bool, principal_configured: bool) -> Vec<String> {
    let mut side_effects = Vec::new();
    if has_git {
        side_effects.push("created Heddle sidecar for the existing Git repository".to_string());
        side_effects.push("updated .git/info/exclude for Heddle metadata".to_string());
        side_effects.push("left Git-tracked files untouched".to_string());
    } else {
        side_effects.push("created Heddle repository metadata".to_string());
    }
    if principal_configured {
        side_effects.push("updated default principal attribution".to_string());
    }
    side_effects
}

/// Pre-write phase of `--quickstart`: run the confirmation gate and
/// resolve the principal identity. Everything here happens before the
/// first filesystem write so a Ctrl-C (or a declined prompt) leaves the
/// directory exactly as it was found.
fn quickstart_preflight(
    cli: &Cli,
    args: &InitArgs,
    path: &Path,
    has_git: bool,
) -> Result<QuickstartPreflight> {
    let json = should_output_json(cli, None);

    // Confirmation gate before touching a directory that already holds
    // work. Truly fresh directories skip straight through.
    let heddle_exists = path.join(".heddle").exists();
    let git_nonempty = has_git && git_has_commits(path);
    if (heddle_exists || git_nonempty) && !args.yes {
        let thread = args.quickstart_thread.as_deref().unwrap_or("quickstart");
        if !json {
            println!(
                "{}",
                style::warn(
                    "heddle init --quickstart would act on a directory that already has work:"
                )
            );
            if heddle_exists {
                println!("  - existing .heddle/ data is present");
            }
            if git_nonempty {
                println!("  - this Git repository already has commits");
            }
            println!(
                "It would resolve your identity, start the '{thread}' thread, capture once, and (on Git-overlay) checkpoint once."
            );
            println!("Existing files are not modified.");
        }
        // No interactive terminal to confirm at: require an explicit
        // `--yes` rather than proceeding silently.
        if json || cli.quiet || !is_tty() {
            bail!(quickstart_needs_confirmation_advice());
        }
        print!("Proceed? [y/N] ");
        io::stdout().flush().ok();
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        if !matches!(input.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("Aborted; no changes made.");
            return Ok(QuickstartPreflight {
                proceed: false,
                ..QuickstartPreflight::default()
            });
        }
    }

    let persist_principal = resolve_quickstart_identity(cli, args, path, has_git, json)?;
    // The harness-install prompt is the LAST interactive gate, decided
    // here before any write so Ctrl-C at it leaves the directory
    // untouched. The install itself runs post-write in `cmd_init`.
    let harness_install = super::prompt_init_install_decision(cli, path, args, json)?;
    Ok(QuickstartPreflight {
        proceed: true,
        persist_principal,
        harness_install,
    })
}

/// Resolve the principal for `--quickstart`. Priority: explicit
/// `--principal-*` flags → an already-resolvable identity (env, user
/// config, or Git config) → an interactive prompt. Returns the
/// `(name, email)` to persist when it came from flags or the prompt, or
/// `None` when an identity is already available without writing. Fails
/// fast (no placeholder) when nothing is resolvable and there is no TTY
/// to prompt.
fn resolve_quickstart_identity(
    cli: &Cli,
    args: &InitArgs,
    path: &Path,
    has_git: bool,
    json: bool,
) -> Result<Option<(String, String)>> {
    match (args.principal_name.clone(), args.principal_email.clone()) {
        (Some(name), Some(email)) => return Ok(Some((name, email))),
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
        (None, None) => {}
    }

    if quickstart_identity_available(path, has_git) {
        return Ok(None);
    }

    if is_tty() && !cli.quiet && !json {
        let name = prompt_line("Your name: ")?;
        let email = prompt_line("Your email: ")?;
        if name.is_empty() || email.is_empty() {
            bail!(quickstart_identity_required_advice());
        }
        return Ok(Some((name, email)));
    }

    bail!(quickstart_identity_required_advice())
}

/// Whether a real (non-placeholder) principal is already resolvable
/// without writing one. Mirrors `resolve_principal`'s precedence so the
/// preflight never refuses a repo whose capture would in fact be
/// attributable: environment, then the repo-level
/// `.heddle/config.toml` `[principal]` (which outranks user and Git
/// config in `resolve_principal`), then the user config, then — in a Git
/// repo — Git's own `user.name`/`user.email`.
fn quickstart_identity_available(path: &Path, has_git: bool) -> bool {
    if let Some(principal) = Principal::from_env()
        && !principal_is_unconfigured(&principal)
    {
        return true;
    }
    let repo_config_path = path.join(".heddle").join("config.toml");
    if let Ok(repo_config) = repo::RepoConfig::load(&repo_config_path)
        && let Some(config) = &repo_config.principal
        && !principal_is_unconfigured(&Principal::new(&config.name, &config.email))
    {
        return true;
    }
    if let Ok(user_config) = UserConfig::load_default()
        && let Some(config) = &user_config.principal
        && !principal_is_unconfigured(&Principal::new(&config.name, &config.email))
    {
        return true;
    }
    if has_git
        && git_config_identity_with_global_fallback(path)
            .ok()
            .flatten()
            .is_some()
    {
        return true;
    }
    false
}

fn prompt_line(label: &str) -> Result<String> {
    print!("{label}");
    io::stdout().flush().ok();
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim().to_string())
}

/// Whether the discovered Git repository at `path` already has at least
/// one commit (a non-empty history). An unborn branch reads as empty.
fn git_has_commits(path: &Path) -> bool {
    gix::discover(path)
        .ok()
        .map(|repo| repo.head_id().is_ok())
        .unwrap_or(false)
}

/// The write batch of `--quickstart`: start the thread, make one
/// capture, and (Git-overlay only) one checkpoint. Runs only after the
/// preflight has confirmed and resolved identity, so every fallible
/// prompt is already behind us.
fn run_quickstart_actions(repo: &Repository, args: &InitArgs) -> Result<QuickstartSummary> {
    // A Git-overlay repo that already has commits must have that history
    // imported into Heddle before a capture/checkpoint has a base to
    // build on — the same import `heddle adopt` performs. Fresh/empty
    // Git repos (no commits) and native repos skip this.
    if repo.capability() == RepositoryCapability::GitOverlay && git_has_commits(repo.root()) {
        let mut bridge = GitBridge::new(repo);
        import_all(&mut bridge, Some(repo.root()))?;
    }

    let thread = args
        .quickstart_thread
        .clone()
        .unwrap_or_else(|| "quickstart".to_string());
    ensure_quickstart_thread(repo, &thread)?;

    let user_config = UserConfig::load_default().unwrap_or_default();
    let wrote_placeholder = ensure_capturable_content(repo)?;
    let snapshot = create_snapshot(
        repo,
        &user_config,
        Some("quickstart: initial capture".to_string()),
        None,
        SnapshotAgentOverrides {
            provider: None,
            model: None,
            session: None,
            segment: None,
            policy: None,
            no_policy: false,
            no_agent: false,
        },
    )?;

    // Checkpoint is Git-overlay only; on native repos the capture above
    // is the user-visible "first commit".
    let git_commit = if repo.capability() == RepositoryCapability::GitOverlay {
        let record = create_git_checkpoint(
            repo,
            Some("quickstart: first commit"),
            worktree_status_options(Some(repo.config())),
        )?;
        Some(record.git_commit)
    } else {
        None
    };

    Ok(QuickstartSummary {
        thread,
        change_id: snapshot.change_id,
        git_commit,
        wrote_placeholder,
    })
}

/// Create the named quickstart thread and attach HEAD to it. Idempotent:
/// a re-run that is already on the thread is a no-op.
///
/// When a current state already exists (a freshly-seeded native repo, or
/// a Git overlay whose history we just imported) the thread is pointed at
/// it. An unborn Git overlay has NO current state yet: we must NOT
/// fabricate a bootstrap snapshot here, or the quickstart would land an
/// extra empty parent commit before `QUICKSTART.md` is even written —
/// breaking the promised single initial capture/checkpoint. Instead we
/// just attach HEAD to the thread; the subsequent quickstart capture
/// creates the thread's first (root) state and advances the ref.
fn ensure_quickstart_thread(repo: &Repository, name: &str) -> Result<()> {
    let target = ThreadName::new(name);
    if let Some(state) = repo.current_state()?
        && repo.refs().get_thread(&target)?.is_none()
    {
        repo.refs().set_thread(&target, &state.change_id)?;
    }
    let already_attached =
        matches!(repo.head_ref()?, Head::Attached { thread } if thread == target);
    if !already_attached {
        repo.refs().write_head(&Head::Attached { thread: target })?;
    }
    Ok(())
}

/// Ensure there is something user-visible to capture. When the worktree
/// has no capturable files (a fresh empty directory), write the
/// root-level `QUICKSTART.md` pointer and report that we did. The
/// root-level path matters: the default ignore list excludes `.heddle/`
/// (`repo_config::default_ignore`), so a placeholder under `.heddle/`
/// would be silently dropped by the capture walk. Non-destructive: an
/// existing `QUICKSTART.md` is left untouched.
fn ensure_capturable_content(repo: &Repository) -> Result<bool> {
    let options = worktree_status_options(Some(repo.config()));
    let (status, _) = repo.compare_worktree_cached_profiled_with_options(&Tree::new(), &options)?;
    if !status.added.is_empty() {
        return Ok(false);
    }
    let placeholder = repo.root().join("QUICKSTART.md");
    if !placeholder.exists() {
        std::fs::write(&placeholder, QUICKSTART_PLACEHOLDER)?;
    }
    Ok(true)
}

fn quickstart_needs_confirmation_advice() -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "quickstart_needs_confirmation",
        "Refusing to run --quickstart non-interactively against a directory that already has Heddle data or Git history",
        "Re-run with `--yes` to confirm, or run `heddle init --quickstart` in an interactive terminal to answer the prompt.",
        "the target directory already has .heddle/ data or non-empty Git history and no interactive terminal is available to confirm",
        "quickstart would start a thread and capture in a directory that already holds work",
        "no repository objects, refs, metadata, or worktree files were changed",
        "heddle init --quickstart --yes",
        vec!["heddle init --quickstart --yes".to_string()],
    )
}

fn quickstart_identity_required_advice() -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "quickstart_identity_required",
        "Refusing to run --quickstart without an accountable identity",
        "Pass `--principal-name <name> --principal-email <email>`, configure identity first, or run in an interactive terminal to be prompted.",
        "no principal was resolvable from flags, environment, user config, or Git config, and no interactive terminal is available to prompt",
        "quickstart would capture history attributed to Unknown <unknown@example.com>",
        "no repository objects, refs, metadata, or worktree files were changed",
        "heddle init --quickstart --principal-name <name> --principal-email <email>",
        vec![
            "heddle init --quickstart --principal-name <name> --principal-email <email>"
                .to_string(),
        ],
    )
}
