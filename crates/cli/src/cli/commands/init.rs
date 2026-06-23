// SPDX-License-Identifier: Apache-2.0
//! Initialize command.

use std::{
    io::{self, Write},
    path::{Path, PathBuf},
};

use anyhow::{Result, bail};
use ingest::ImportOptions;
use objects::object::{Principal, ThreadName, Tree};
use refs::Head;
use repo::{Repository, RepositoryCapability, RepositoryProbe, RepositoryProbeTarget, ThreadId};
use serde::Serialize;
use sley::{FullName, Repository as SleyRepository};
use tracing::{debug, info};

use super::{
    RecoveryAdvice,
    action_line::print_next,
    checkpoint::create_git_checkpoint,
    git_overlay_health::{RepositoryVerificationState, build_repository_verification_state},
    snapshot::{
        SnapshotAgentOverrides, create_snapshot, is_placeholder_principal,
        placeholder_principal_warning,
    },
};
use crate::{
    bridge::{GitBridge, WriteThroughOutcome, git_ingest::import_git_history},
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
    #[serde(skip)]
    placeholder_principal_warning: Option<String>,
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
    attachment: QuickstartAttachmentPlan,
    git_has_commits: bool,
    /// Harnesses explicitly selected for connection before any write.
    /// Installed only after the repo is created so scope errors leave the
    /// directory untouched.
    harness_install: Vec<String>,
}

impl Default for QuickstartPreflight {
    fn default() -> Self {
        Self {
            proceed: true,
            persist_principal: None,
            attachment: QuickstartAttachmentPlan::SkipUnborn,
            git_has_commits: false,
            harness_install: Vec::new(),
        }
    }
}

/// Pre-capture Git checkout attachment plan, computed read-only in preflight.
///
/// The write path must not re-discover these preconditions after bootstrap or
/// import has already written `.heddle/`: every edge that decides whether it is
/// safe and meaningful to call `write_through_thread_checkout` belongs here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QuickstartAttachmentPlan {
    /// Current Git HEAD has an exportable commit state and the requested branch
    /// is absent or already points at that commit.
    Attach,
    /// Current Git HEAD has no exportable state yet (fresh/unborn/orphan), so
    /// the first capture should establish the requested thread state.
    SkipUnborn,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QuickstartAttachmentDecision {
    Attach,
    SkipUnborn,
    RefuseCollision,
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
    let has_git = SleyRepository::discover(&path).is_ok();

    // Resolve the single quickstart root ONCE, by read-only repository
    // discovery, so the preflight and write path operate on the SAME
    // directory without triggering `Repository::open` writes.
    let probe = if args.quickstart {
        Some(Repository::probe(&path)?)
    } else {
        None
    };

    // Quickstart confirms and resolves identity BEFORE any write so a
    // Ctrl-C (or declined prompt) leaves the directory untouched — no
    // half-written `.heddle/`. The preflight reads only: it never opens the
    // repo (whose HEAD-sync would write), so a refused/declined quickstart
    // performs zero writes.
    let preflight = match probe.as_ref() {
        Some(probe) => quickstart_preflight(cli, &args, probe)?,
        None => QuickstartPreflight::default(),
    };
    if !preflight.proceed {
        return Ok(());
    }

    // Writes begin here — only after the preflight returned `proceed`, and on
    // the SAME root it validated. For quickstart, branch on the resolved
    // target so a subdirectory invocation opens the discovered repo / boots the
    // discovered Git root rather than creating a nested repo at the cwd.
    let repo = match probe.as_ref().map(|probe| probe.target()) {
        Some(RepositoryProbeTarget::Existing) => {
            Repository::open(probe.as_ref().unwrap().resolved_root())?
        }
        Some(RepositoryProbeTarget::FreshGitOverlay) => {
            Repository::bootstrap_git_overlay(probe.as_ref().unwrap().resolved_root())?
        }
        Some(RepositoryProbeTarget::FreshNative) => {
            Repository::init_default(probe.as_ref().unwrap().resolved_root())?
        }
        None if has_git => Repository::bootstrap_git_overlay(&path)?,
        None => Repository::init_default(&path)?,
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
    let repo_root = repo.root().to_path_buf();
    if args.quickstart {
        if let Some((name, email)) = &preflight.persist_principal {
            let config_path = repo.heddle_dir().join("config.toml");
            let mut repo_config = repo::RepoConfig::load(&config_path).unwrap_or_default();
            repo_config.set_principal(name.clone(), email.clone());
            repo_config.save(&config_path)?;
            info!(principal_name = %name, principal_email = %email, "Principal configured");
            debug!(config_path = %config_path.display(), "Repo config updated");
            repo = Repository::open(&repo_root)?;
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

    let quickstart = if args.quickstart {
        // Capture FIRST, then install harnesses. The initial capture must
        // record the user's own first state; installing harness scaffolding
        // (`.claude/settings.json`, …) before the capture would make
        // `ensure_capturable_content` treat that scaffolding as the user's
        // content — skipping the `QUICKSTART.md` placeholder and recording
        // integration files as the first state. The install decision was made
        // up front in the preflight; only the write runs here, post-capture.
        let summary = run_quickstart_actions(
            &repo,
            &args,
            preflight.attachment,
            preflight.git_has_commits,
        )?;
        super::perform_init_install(cli, &repo, &args, &preflight.harness_install)?;
        Some(summary)
    } else {
        super::maybe_prompt_init_install(cli, &repo, &args)?;
        None
    };

    // Output reflects the repo that was actually created/opened. For quickstart
    // that is the resolved target's capability (a subdirectory invocation may
    // have opened a native repo even though Git discovery finds an ancestor
    // Git checkout); the non-quickstart path keeps its prior `has_git` framing.
    let repo_is_git_overlay = if args.quickstart {
        repo.capability() == RepositoryCapability::GitOverlay
    } else {
        has_git
    };
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
    // After a quickstart the user has a captured state to inspect, so
    // point them at `heddle log` regardless of the trust-derived action.
    //
    // A non-quickstart init must never end without a next step
    // (heddle#644). When the repo has existing Git history the trust
    // state already recommends the exact adopt/import command; when it
    // doesn't (fresh native repo, or a Git checkout with no commits),
    // trust has nothing to flag, so point at the first save — `heddle
    // commit` records the first state (and, in Git-overlay repos, the
    // matching Git checkpoint).
    let next_action = if quickstart.is_some() {
        Some("heddle log".to_string())
    } else if !trust.recommended_action.is_empty() {
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
        git_detected: repo_is_git_overlay,
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
        if let Some(warning) = output.placeholder_principal_warning.as_deref() {
            eprintln!("{}", style::warn(warning));
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
/// resolve the principal identity. Everything here is READ-ONLY — it never
/// opens the repository (whose HEAD-sync would write) nor touches the
/// filesystem — so a Ctrl-C, a declined prompt, or any refusal leaves the
/// directory exactly as it was found.
fn quickstart_preflight(
    cli: &Cli,
    args: &InitArgs,
    probe: &RepositoryProbe,
) -> Result<QuickstartPreflight> {
    // The quickstart preflight is a READ-ONLY DRY-RUN of the real init/identity
    // path: every viability decision below shares the SAME predicates the write
    // path uses — capability and identity come from `Repository::probe`, the
    // thread name via the ref/branch validators, and the harness scope via the
    // install path's `IntegrationScope::parse`. It must NOT call
    // `Repository::open`: for a Git-overlay repo `open` synchronizes
    // `.heddle/HEAD` to Git's HEAD — a write that would fire before a refusal.
    // The single resolved probe target means the write path opens that SAME
    // root, so the read-only probe and the eventual open never disagree on
    // which dir or whether it is a Git overlay.
    let root = probe.resolved_root();
    let is_git_overlay = probe.capability() == RepositoryCapability::GitOverlay;
    let git_facts = probe.git_facts();

    // Honor the repo's on-disk `[output].format` so a `json`-configured repo
    // never gets a text confirmation prompt before a JSON envelope. Read it
    // off disk (following an objectstore pointer to the shared dir) without
    // opening — `None` for a fresh directory.
    let json = should_output_json(cli, probe.existing_config());

    // A detached Git HEAD has no branch for the checkpoint to advance, and
    // `create_git_checkpoint` refuses it only AFTER the import/capture have
    // written `.heddle/` state. Refuse here, before any write — but ONLY when
    // the repo will actually run as a Git overlay. A native repo nested inside
    // an ancestor Git checkout creates no checkpoint, so it must not be refused
    // for the ancestor's detached HEAD.
    if is_git_overlay && git_facts.head_is_detached() {
        bail!(quickstart_detached_head_advice());
    }

    // A shallow Git checkout (`.git/shallow`) can't be imported until full
    // ancestry is available — `import_all` refuses it, but only AFTER
    // `bootstrap_git_overlay` has created `.heddle/` and edited the Git
    // excludes, leaving a half-initialized sidecar. Detect it here, read-only,
    // and refuse before any write — but only when the repo will run as a Git
    // overlay AND has history to import (the exact condition under which
    // `run_quickstart_actions` calls `import_all`). Mirrors `import_all`'s own
    // `git_dir()/shallow` probe (cid 3329409826).
    if is_git_overlay && git_facts.has_commits() && git_facts.is_shallow() {
        bail!(quickstart_shallow_clone_advice());
    }

    // Validate the requested thread name BEFORE any write, using the SAME
    // centralized rule every thread-creation boundary enforces ([`ThreadId::new`]
    // / `validate_thread_id`) — one rule, not an ad-hoc copy. A bad name
    // (`a..b`, `my feature`, a shell metacharacter, …) must fail here rather
    // than after init/bootstrap/import have already written `.heddle/` data,
    // leaving a half-initialized repo for a pure argument error.
    let thread = args.quickstart_thread.as_deref().unwrap_or("quickstart");
    if let Err(err) = ThreadId::new(thread) {
        bail!(RecoveryAdvice::invalid_usage(
            "quickstart_thread_name_invalid",
            err.to_string(),
            "Choose a thread name using only letters, digits, and _ - . / @ : + = \
             (no spaces or shell metacharacters).",
            "heddle init --quickstart --quickstart-thread <name>",
        ));
    }

    // A Git-overlay quickstart creates a real `refs/heads/<thread>`, so the
    // name must additionally satisfy Git's BRANCH-shorthand rules. These are
    // stricter than validating the assembled `refs/heads/<thread>` full ref:
    // a full ref accepts names Git refuses as a branch (e.g. `HEAD`, a leading
    // `-`). Validate the shorthand here so such a name fails before any write
    // rather than after `create_snapshot` has written Heddle state. Native
    // (non-Git) quickstarts keep Heddle's rules only.
    if is_git_overlay && !git_branch_name_is_valid(thread) {
        bail!(RecoveryAdvice::invalid_usage(
            "quickstart_thread_name_invalid",
            format!("'{thread}' is not a valid Git branch name"),
            "Choose a thread name Git accepts as a branch: no spaces, '~', '^', ':', '?', '*', '[', backslashes, control characters, a leading '-', or the reserved name 'HEAD'.",
            "heddle init --quickstart --quickstart-thread <name>",
        ));
    }

    // Decide the whole pre-capture Git checkout attachment path up front,
    // read-only. This is the single gate for the attachment class: attach only
    // when current HEAD has an exportable commit and the target branch is
    // absent or already at that commit; skip for unborn/no-state HEADs so the
    // first capture establishes the thread; refuse divergent target branches
    // before any `.heddle/` writes.
    let git_nonempty = is_git_overlay && git_facts.has_commits();
    let attachment =
        match quickstart_attachment_decision(root, is_git_overlay, thread, git_nonempty) {
            QuickstartAttachmentDecision::Attach => QuickstartAttachmentPlan::Attach,
            QuickstartAttachmentDecision::SkipUnborn => QuickstartAttachmentPlan::SkipUnborn,
            QuickstartAttachmentDecision::RefuseCollision => {
                bail!(quickstart_thread_branch_collision_advice(thread));
            }
        };

    // Confirmation gate before touching a directory that already holds
    // work. Truly fresh directories skip straight through.
    let heddle_exists = root.join(".heddle").exists();
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

    let persist_principal = resolve_quickstart_identity(cli, args, probe, json)?;
    // Resolve explicit harness installs before any write, at the SAME
    // resolved root the install writes to (`repo.root()` in `cmd_init`),
    // not the raw cwd. The install itself runs post-write in `cmd_init`.
    let harness_install = super::prompt_init_install_decision(cli, root, args, json)?;
    Ok(QuickstartPreflight {
        proceed: true,
        persist_principal,
        attachment,
        git_has_commits: git_facts.has_commits(),
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
/// [`Repository::probe`]. This is the single source of truth:
/// flags, prompt, repo config, user config, and Git config (and a
/// higher-precedence sentinel shadowing a lower valid source) are all
/// caught HERE, before any write, instead of by `build_attribution` after
/// `.heddle/` already exists. It resolves identity READ-ONLY — without opening
/// the repository — so a refusal (sentinel env/flag, or no resolvable identity
/// with no TTY) never triggers `Repository::open`'s HEAD-sync write.
fn resolve_quickstart_identity(
    cli: &Cli,
    args: &InitArgs,
    probe: &RepositoryProbe,
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
    if let Some(env_principal) = Principal::from_env()
        && !principal_is_unconfigured(&env_principal)
    {
        return Ok(None);
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

    // No flags: ask what the capture would be attributed to, READ-ONLY. The
    // probe mirrors the repository-scoped identity sources; user config remains
    // outside repository discovery and is checked here.
    if let Some(resolved) = probe.repo_principal_candidate() {
        if principal_is_unconfigured(resolved) {
            bail!(quickstart_identity_required_advice());
        }
        return Ok(None);
    }
    if let Ok(user_config) = UserConfig::load_default()
        && let Some(config) = &user_config.principal
    {
        let principal = Principal::new(&config.name, &config.email);
        if !principal_is_unconfigured(&principal) {
            return Ok(None);
        }
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

fn prompt_line(label: &str) -> Result<String> {
    print!("{label}");
    io::stdout().flush().ok();
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim().to_string())
}

/// Read-only decision for the Git-overlay quickstart's pre-capture checkout
/// attachment.
///
/// After `import_all`, `ensure_quickstart_thread` may point the requested
/// Heddle thread at the CURRENT state and `write_through_thread_checkout` would
/// then write that state to `refs/heads/<thread>`. That is valid only when the
/// current Git HEAD resolves to a real commit and an existing target branch is
/// already at that same commit. An unborn/orphan current HEAD has no exportable
/// state yet, so attachment must be deferred to the first capture. A divergent
/// target branch must refuse before any write, because attachment would move
/// the user's branch onto unrelated history (cid 3335757978, cid 3336080241).
fn quickstart_attachment_decision(
    path: &Path,
    is_git_overlay: bool,
    thread: &str,
    git_has_commits: bool,
) -> QuickstartAttachmentDecision {
    if !is_git_overlay || !path.join(".git").exists() || !git_has_commits {
        return QuickstartAttachmentDecision::SkipUnborn;
    }

    let Ok(repo) = SleyRepository::discover(path) else {
        return QuickstartAttachmentDecision::SkipUnborn;
    };
    let Some(head) = repo.head().ok().and_then(|head| head.oid) else {
        return QuickstartAttachmentDecision::SkipUnborn;
    };
    let Ok(Some(reference)) = repo.find_reference(&format!("refs/heads/{thread}")) else {
        return QuickstartAttachmentDecision::Attach;
    };
    let Ok(Some(branch_tip)) = reference.peeled_oid(&repo) else {
        return QuickstartAttachmentDecision::Attach;
    };
    if head == branch_tip {
        QuickstartAttachmentDecision::Attach
    } else {
        QuickstartAttachmentDecision::RefuseCollision
    }
}

/// Whether `name` is valid as a Git BRANCH — the shorthand written under
/// `refs/heads/` — matching `git check-ref-format --branch`. This is stricter
/// than validating the assembled `refs/heads/<name>` full ref: a syntactically
/// valid full ref can still name an unusable branch. `git check-ref-format
/// refs/heads/HEAD` accepts the full ref, but `git check-ref-format --branch
/// HEAD` rejects it; the same holds for a leading `-` or a bare `@`. The Git
/// checkpoint write-through points `.git/HEAD` at `refs/heads/<name>`, so
/// reject here exactly what Git's porcelain would refuse there.
fn git_branch_name_is_valid(name: &str) -> bool {
    if FullName::try_from(format!("refs/heads/{name}").as_str()).is_err() {
        return false;
    }
    // Branch-shorthand rules `--branch` adds on top of full-ref syntax: not
    // the reserved `HEAD`, not a bare `@`, and no leading `-`.
    !(name == "HEAD" || name == "@" || name.starts_with('-'))
}

/// The write batch of `--quickstart`: start the thread, make one
/// capture, and (Git-overlay only) one checkpoint. Runs only after the
/// preflight has confirmed and resolved identity, so every fallible
/// prompt is already behind us.
fn run_quickstart_actions(
    repo: &Repository,
    args: &InitArgs,
    attachment: QuickstartAttachmentPlan,
    git_has_commits: bool,
) -> Result<QuickstartSummary> {
    // A Git-overlay repo that already has commits must have that history
    // imported into Heddle before a capture/checkpoint has a base to
    // build on — the same import `heddle adopt` performs. Fresh/empty
    // Git repos (no commits) and native repos skip this.
    if repo.capability() == RepositoryCapability::GitOverlay && git_has_commits {
        let mut bridge = GitBridge::new(repo);
        import_git_history(
            &mut bridge,
            Some(repo.root()),
            &[],
            ImportOptions::default(),
            None,
        )?;
    }

    let thread = args
        .quickstart_thread
        .clone()
        .unwrap_or_else(|| "quickstart".to_string());
    ensure_quickstart_thread(repo, &thread)?;

    // On a Git overlay with imported history, `head_ref()` deliberately
    // resolves back to the live Git branch (e.g. `main`) — so merely writing
    // `.heddle/HEAD = Attached{thread}` above is NOT enough: the capture and
    // checkpoint below both target `head_ref()`, and would advance the Git
    // branch while the quickstart thread stays at the imported tip, even though
    // the output says `Thread: <thread>` (cid 3329409824). The preflight has
    // already decided whether all attachment preconditions hold; execute that
    // plan here without re-discovering partial after-the-fact guards.
    match attachment {
        QuickstartAttachmentPlan::Attach => {
            let mut bridge = GitBridge::new(repo);
            if let WriteThroughOutcome::Skipped(reason) =
                bridge.write_through_thread_checkout(&thread)?
            {
                bail!(RecoveryAdvice::safety_refusal(
                    "quickstart_thread_checkout_skipped",
                    format!("Could not attach the Git checkout to thread '{thread}': {reason}"),
                    "Resolve the Git checkout issue and re-run `heddle init --quickstart`.",
                    reason.to_string(),
                    "quickstart would capture and checkpoint on the requested thread, but the Git checkout could not be attached to its branch",
                    "the current Heddle state was preserved",
                    "heddle init --quickstart",
                    vec!["heddle init --quickstart".to_string()],
                ));
            }
        }
        QuickstartAttachmentPlan::SkipUnborn => {}
    }

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
    if !already_attached && let Some(state) = repo.current_state()? {
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

fn quickstart_thread_branch_collision_advice(thread: &str) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "quickstart_thread_branch_collision",
        format!(
            "Refusing to run --quickstart: a Git branch named '{thread}' already exists at a different commit than the current checkout"
        ),
        format!(
            "Pass `--quickstart-thread <name>` to use a different thread name, or switch to '{thread}' (`git switch {thread}`) and run the normal capture flow."
        ),
        format!(
            "a Git branch '{thread}' already exists and points at history unrelated to the current branch"
        ),
        format!(
            "quickstart would attach the '{thread}' thread to the current branch's state and move refs/heads/{thread} onto it, silently discarding the existing branch's history"
        ),
        "no repository objects, refs, metadata, or worktree files were changed",
        "heddle init --quickstart --quickstart-thread <name>",
        vec![
            "heddle init --quickstart --quickstart-thread <name>".to_string(),
            format!("git switch {thread}"),
        ],
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

fn quickstart_shallow_clone_advice() -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "quickstart_shallow_clone",
        "Refusing to run --quickstart on a shallow Git clone",
        "Fetch full history first with `git fetch --unshallow`, then re-run `heddle init --quickstart`.",
        "the Git checkout is shallow (.git/shallow is present)",
        "quickstart would import Git history, but Heddle cannot import a shallow clone until its full ancestry is available",
        "no repository objects, refs, metadata, or worktree files were changed",
        "git fetch --unshallow",
        vec![
            "git fetch --unshallow".to_string(),
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
