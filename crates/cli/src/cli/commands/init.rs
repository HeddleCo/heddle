// SPDX-License-Identifier: Apache-2.0
//! Initialize command.

use std::{
    io::{self, Write},
    path::{Path, PathBuf},
};

use anyhow::{Result, bail};
use objects::object::{Principal, ThreadName, Tree};
use refs::{Head, validate_ref_name};
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
    // Resolve the repo's on-disk config the way `Repository::open` does
    // (following an objectstore pointer to the shared dir for a materialized
    // checkout) so the confirmation prompt's format matches the final
    // render. A repo whose `[output].format` is `json` must not get a text
    // prompt before a JSON envelope.
    let repo_config = resolve_existing_repo_config(path);
    let json = should_output_json(cli, repo_config.as_ref());

    // A detached Git HEAD has no branch for the checkpoint to advance.
    // `create_git_checkpoint` refuses it, but only AFTER the import/capture
    // have written `.heddle/` state — leaving a half-initialized repo.
    // Refuse here, before any write, so a detached HEAD leaves the directory
    // exactly as it was found.
    if has_git && git_head_is_detached(path) {
        bail!(quickstart_detached_head_advice());
    }

    // Validate the requested thread name BEFORE any write, using the same
    // ref-name rules the thread machinery enforces when the ref is actually
    // created. A bad name (`.bad`, `a..b`, …) must fail here rather than
    // after init/bootstrap/import have already written `.heddle/` data,
    // leaving a half-initialized repo for a pure argument error.
    let thread = args.quickstart_thread.as_deref().unwrap_or("quickstart");
    if validate_ref_name(thread).is_err() {
        bail!(RecoveryAdvice::invalid_usage(
            "quickstart_thread_name_invalid",
            format!("'{thread}' is not a valid thread name"),
            "Choose a thread name without '..', a leading '.', a trailing '/' or '.lock', backslashes, or control characters.",
            "heddle init --quickstart --quickstart-thread <name>",
        ));
    }

    // A Git-overlay quickstart creates a real `refs/heads/<name>`. Git's
    // ref-name rules are stricter than Heddle's `validate_ref_name` (they
    // reject a space, `~`, `^`, `:`, `?`, `*`, `[`, …), so a name Heddle
    // accepts but Git rejects would pass preflight and then fail when the
    // branch is created — after `create_snapshot` has written Heddle state.
    // Validate against Git's rules here too so it fails before any write.
    // Native (non-Git) quickstarts keep Heddle's rules only.
    if has_git
        && gix::refs::FullName::try_from(format!("refs/heads/{thread}").as_str()).is_err()
    {
        bail!(RecoveryAdvice::invalid_usage(
            "quickstart_thread_name_invalid",
            format!("'{thread}' is not a valid Git branch name"),
            "Choose a thread name Git accepts as a branch: no spaces, '~', '^', ':', '?', '*', '[', backslashes, or control characters.",
            "heddle init --quickstart --quickstart-thread <name>",
        ));
    }

    // Confirmation gate before touching a directory that already holds
    // work. Truly fresh directories skip straight through.
    let heddle_exists = path.join(".heddle").exists();
    let git_nonempty = has_git && git_has_commits(path);
    if (heddle_exists || git_nonempty) && !args.yes {
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
/// `--principal-*` flags → an already-resolvable identity (env, repo
/// config, Git config, or user config) → an interactive prompt. Returns
/// the `(name, email)` to persist when it came from flags or the prompt,
/// or `None` when an identity is already available without writing. Fails
/// fast (no placeholder) when nothing usable is resolvable and there is no
/// TTY to prompt.
///
/// Every path that yields an identity is checked against the SAME sentinel
/// predicate (`principal_is_unconfigured`) the real capture uses, over the
/// SAME precedence `resolve_principal` walks — see
/// [`resolve_quickstart_principal`]. This is the single source of truth:
/// flags, prompt, repo config, user config, and Git config (and a
/// higher-precedence sentinel shadowing a lower valid source) are all
/// caught HERE, before any write, instead of by `build_attribution` after
/// `.heddle/` already exists.
fn resolve_quickstart_identity(
    cli: &Cli,
    args: &InitArgs,
    path: &Path,
    has_git: bool,
    json: bool,
) -> Result<Option<(String, String)>> {
    // `resolve_principal` lets env win OUTRIGHT — it returns the env identity
    // even when it is the sentinel, before considering repo config (where
    // flags land), Git, or user config. So a sentinel env identity shadows a
    // valid `--principal-*` flag: the capture would still be attributed to the
    // env sentinel and rejected by `build_attribution`, but only AFTER init has
    // written `.heddle/config.toml` (and quickstart may have written
    // QUICKSTART.md). Reject the env sentinel here, before any write, instead
    // of persisting lower-precedence flags that env will shadow.
    if let Some(env_principal) = Principal::from_env()
        && principal_is_unconfigured(&env_principal)
    {
        bail!(quickstart_identity_required_advice());
    }

    // Explicit flags become the repo-level `[principal]` — the highest
    // precedence source after env in `resolve_principal`. Validate them
    // against the sentinel here so `--principal-name Unknown
    // --principal-email unknown@example.com` fails before any write rather
    // than being persisted and then rejected by `build_attribution`.
    let flag_principal = match (args.principal_name.clone(), args.principal_email.clone()) {
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
    if let Some((name, email)) = flag_principal {
        if principal_is_unconfigured(&Principal::new(&name, &email)) {
            bail!(quickstart_identity_required_advice());
        }
        return Ok(Some((name, email)));
    }

    // No flags: ask the REAL resolution (mirroring `resolve_principal`'s
    // stop-at-first-present precedence) what the capture would be
    // attributed to. A usable identity → proceed without writing one. A
    // sentinel result — including a higher-precedence sentinel that shadows
    // a lower valid source — means the capture would be rejected, so we
    // must prompt (if interactive) or fail before any write.
    let resolved = resolve_quickstart_principal(path, has_git);
    if !principal_is_unconfigured(&resolved) {
        return Ok(None);
    }

    if is_tty() && !cli.quiet && !json {
        let name = prompt_line("Your name: ")?;
        let email = prompt_line("Your email: ")?;
        // Validate the collected identity with the same sentinel predicate
        // so a prompted `Unknown / unknown@example.com` is rejected before
        // any write, exactly like the flag path.
        if principal_is_unconfigured(&Principal::new(&name, &email)) {
            bail!(quickstart_identity_required_advice());
        }
        return Ok(Some((name, email)));
    }

    bail!(quickstart_identity_required_advice())
}

/// Resolve the ambient principal the quickstart capture WOULD be attributed
/// to (with no `--principal-*` flags), at preflight time, before the repo
/// exists. This is the single source of truth for "is there a usable
/// identity?": it mirrors `resolve_principal` (snapshot.rs) EXACTLY — same
/// sources, same order, and crucially the same STOP-at-first-present
/// semantics — so the preflight can never diverge from the real resolution
/// the way a fall-through check can. (Flag/prompt identities are validated
/// separately at their source in `resolve_quickstart_identity`, since they
/// occupy the repo-config slot by being written there before the capture.)
///
/// Precedence (identical to `resolve_principal`): env → repo
/// `.heddle/config.toml` `[principal]` → Git config (only when it isn't the
/// sentinel, matching `resolve_principal`'s fall-through) → user config →
/// the `Unknown` sentinel.
///
/// Returns the resolved principal (possibly the sentinel); callers apply
/// `principal_is_unconfigured` to decide whether to fail. STOP semantics are
/// the whole point: a higher-precedence sentinel (e.g. a repo config pinning
/// `Unknown <unknown@example.com>`) shadows a lower valid source here just as
/// it would in `resolve_principal`, so the preflight rejects what the capture
/// would reject.
fn resolve_quickstart_principal(path: &Path, has_git: bool) -> Principal {
    // env wins outright — `resolve_principal` returns it unconditionally
    // (even when it is the sentinel), so mirror that: stop here.
    if let Some(principal) = Principal::from_env() {
        return principal;
    }
    // Repo-level config slot: stop at the on-disk repo `[principal]` if
    // present, even the sentinel — `resolve_principal` does. (This is the
    // "already has .heddle/" quickstart path.) Resolve config the way
    // `Repository::open` does: in a materialized checkout the local
    // `.heddle/` is just an objectstore pointer and the real `[principal]`
    // lives in the SHARED dir it points at, so a local-only probe would
    // wrongly report "no identity" there.
    if let Some(repo_config) = resolve_existing_repo_config(path)
        && let Some(config) = &repo_config.principal
    {
        return Principal::new(&config.name, &config.email);
    }
    // Git config: `resolve_principal` falls through to user config when
    // Git's identity is the sentinel, so only a non-sentinel Git identity
    // stops here.
    if has_git
        && let Ok(Some(identity)) = git_config_identity_with_global_fallback(path)
    {
        let principal = Principal::new(&identity.name, &identity.email);
        if !principal_is_unconfigured(&principal) {
            return principal;
        }
    }
    if let Ok(user_config) = UserConfig::load_default()
        && let Some(config) = &user_config.principal
    {
        return Principal::new(&config.name, &config.email);
    }
    Principal::new("Unknown", "unknown@example.com")
}

fn prompt_line(label: &str) -> Result<String> {
    print!("{label}");
    io::stdout().flush().ok();
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim().to_string())
}

/// Whether the discovered Git repository at `path` has any commits on ANY
/// local ref — not just the current HEAD. A repo with commits on another
/// ref (e.g. after `git switch --orphan scratch`, where the current HEAD is
/// unborn but `main` still carries history) must be treated as having
/// existing history so the quickstart confirms AND imports rather than
/// acting as if the repo were empty (which would leave partial/wrong state).
fn git_has_commits(path: &Path) -> bool {
    let Ok(repo) = gix::discover(path) else {
        return false;
    };
    if repo.head_id().is_ok() {
        return true;
    }
    let Ok(platform) = repo.references() else {
        return false;
    };
    let Ok(refs) = platform.all() else {
        return false;
    };
    for reference in refs.filter_map(Result::ok) {
        let mut reference = reference;
        if let Ok(id) = reference.peel_to_id()
            && repo
                .find_object(id.detach())
                .map(|object| object.kind == gix::objs::Kind::Commit)
                .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

/// Whether the discovered Git repository at `path` has a detached HEAD
/// (HEAD points directly at a commit instead of an attached branch). An
/// unborn HEAD reads as not-detached.
fn git_head_is_detached(path: &Path) -> bool {
    gix::discover(path)
        .ok()
        .and_then(|repo| repo.head().ok().map(|head| head.is_detached()))
        .unwrap_or(false)
}

/// Resolve the on-disk repo config the way `Repository::open` does: when the
/// local `.heddle/` is a worktree pointer (`.heddle/objectstore`), the real
/// config lives in the shared dir it points at; otherwise it is the local
/// `.heddle/config.toml`. Returns `None` when there is no readable `.heddle`
/// config yet (a fresh directory).
fn resolve_existing_repo_config(path: &Path) -> Option<repo::RepoConfig> {
    let heddle_dir = path.join(".heddle");
    if !heddle_dir.is_dir() {
        return None;
    }
    let pointer = heddle_dir.join("objectstore");
    let config_path = if pointer.is_file() {
        let content = std::fs::read_to_string(&pointer).ok()?;
        let shared = parse_objectstore_pointer(&content)?;
        shared.canonicalize().ok()?.join("config.toml")
    } else {
        heddle_dir.join("config.toml")
    };
    repo::RepoConfig::load(&config_path).ok()
}

/// Minimal mirror of the repo crate's objectstore pointer parse: the file
/// holds a line of the form `objectstore: <absolute path>`.
fn parse_objectstore_pointer(content: &str) -> Option<PathBuf> {
    content.lines().find_map(|line| {
        line.strip_prefix("objectstore:")
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
    })
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

/// Create (or repoint) the named quickstart thread and attach HEAD to it.
/// Idempotent: a re-run that is already on the thread is a no-op.
///
/// When a current state exists (a freshly-seeded native repo, a Git overlay
/// whose history we just imported, or simply another thread the user is
/// currently on) the quickstart thread is pointed AT that current state, so
/// the subsequent capture's parent is the current worktree's state. This
/// covers two cases that must behave identically:
///   - the thread does not exist yet → create it at the current state;
///   - the thread already exists but is NOT the one we're attached to →
///     repoint it to the current state. Otherwise `write_head` would attach
///     to the thread's STALE tip without checking out its tree, and the
///     capture would record the current worktree as a child of that stale
///     tip — the wrong parent (corrupting history when `--quickstart --yes`
///     is rerun after switching away from an existing quickstart thread).
///
/// When already attached to the thread, its tip already IS the current
/// state, so it is left untouched (the idempotent no-op rerun).
///
/// An unborn Git overlay has NO current state yet: we must NOT fabricate a
/// bootstrap snapshot here, or the quickstart would land an extra empty
/// parent commit before `QUICKSTART.md` is even written — breaking the
/// promised single initial capture/checkpoint. In that case we just attach
/// HEAD to the thread; the subsequent quickstart capture creates the
/// thread's first (root) state and advances the ref.
fn ensure_quickstart_thread(repo: &Repository, name: &str) -> Result<()> {
    let target = ThreadName::new(name);
    let already_attached =
        matches!(repo.head_ref()?, Head::Attached { thread } if thread == target);
    if !already_attached
        && let Some(state) = repo.current_state()?
    {
        repo.refs().set_thread(&target, &state.change_id)?;
    }
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

fn quickstart_detached_head_advice() -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "quickstart_detached_head",
        "Refusing to run --quickstart on a detached Git HEAD",
        "Attach a branch first with `git switch -c <branch>` (or `git switch <branch>`), then re-run `heddle init --quickstart`.",
        "Git HEAD points directly at a commit instead of an attached branch",
        "quickstart would import history and write a Git checkpoint through a branch, but a detached HEAD has no branch to advance and could reattach or move the wrong ref",
        "no repository objects, refs, metadata, or worktree files were changed",
        "git switch -c <branch>",
        vec![
            "git switch -c <branch>".to_string(),
            "heddle init --quickstart".to_string(),
        ],
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
