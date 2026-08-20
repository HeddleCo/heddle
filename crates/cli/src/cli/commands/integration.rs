// SPDX-License-Identifier: Apache-2.0
//! Harness integration install and relay commands.

use std::{
    collections::BTreeSet,
    env, fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow};
use heddle_core::integration_plan::{
    HarnessSelectionPlan, IntegrationHarnessError, IntegrationHarnessScopeError,
    IntegrationScopeError, IntegrationScopeKind, PathModeKind, classify_opencode_plugin_path_mode,
    claude_settings_has_relay, codex_config_has_relay, doctor_status_line, drifted_status_token,
    empty_integrations_message, healthy_status_token, installed_message,
    integration_capabilities as plan_integration_capabilities, is_timeline_capability_path,
    list_status_line, missing_status_token, normalize_harness_names, parse_scope,
    path_mode_from_absolute_flag, plan_harness_selection, relative_heddle_invocation,
    uninstalled_message, upgraded_message, validate_harness_scope as plan_validate_harness_scope,
    validate_install_plan as plan_validate_install_plan,
};
use objects::fs_atomic::write_file_atomic;
use repo::Repository;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::advice::RecoveryAdvice;
use crate::{
    cli::{
        Cli, IntegrationCommands, IntegrationInstallArgs, IntegrationRelayArgs,
        IntegrationTargetArgs, should_output_json,
    },
    harness,
};

const MANIFEST_FILE: &str = "integrations.toml";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum IntegrationScope {
    Repo,
    User,
}

impl IntegrationScope {
    fn parse(value: &str) -> Result<Self> {
        parse_scope(value)
            .map(IntegrationScope::from)
            .map_err(|err| match err {
                IntegrationScopeError::Invalid { value } => anyhow!(RecoveryAdvice::invalid_usage(
                    "integration_scope_invalid",
                    format!("invalid integration scope: {value}"),
                    "Use `--scope repo` or `--scope user`.",
                    "heddle integration install --scope repo",
                )),
            })
    }

    fn as_kind(&self) -> IntegrationScopeKind {
        match self {
            Self::Repo => IntegrationScopeKind::Repo,
            Self::User => IntegrationScopeKind::User,
        }
    }
}

impl From<IntegrationScopeKind> for IntegrationScope {
    fn from(kind: IntegrationScopeKind) -> Self {
        match kind {
            IntegrationScopeKind::Repo => Self::Repo,
            IntegrationScopeKind::User => Self::User,
        }
    }
}

/// Whether installed hook commands invoke `heddle` via PATH (relative) or via
/// the absolute path of the heddle binary that performed the install.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
enum PathMode {
    #[default]
    Relative,
    Absolute,
}

impl PathMode {
    fn as_kind(self) -> PathModeKind {
        match self {
            Self::Relative => PathModeKind::Relative,
            Self::Absolute => PathModeKind::Absolute,
        }
    }
}

impl From<PathModeKind> for PathMode {
    fn from(kind: PathModeKind) -> Self {
        match kind {
            PathModeKind::Relative => Self::Relative,
            PathModeKind::Absolute => Self::Absolute,
        }
    }
}

/// Resolved heddle invocation token to splice into the generated hook command.
/// Either the literal string `heddle` (PATH-relative) or a shell-escaped absolute path.
struct HeddleInvocation(String);

impl HeddleInvocation {
    fn resolve(mode: PathMode) -> Result<Self> {
        Ok(match mode {
            PathMode::Relative => HeddleInvocation(relative_heddle_invocation().to_string()),
            PathMode::Absolute => {
                let exe = std::env::current_exe()
                    .context("resolving current executable for integration install")?;
                HeddleInvocation(shell_escape(&exe))
            }
        })
    }

    /// Raw form (unescaped) for embedding in non-shell contexts (e.g. JS strings).
    fn raw(mode: PathMode) -> Result<String> {
        Ok(match mode {
            PathMode::Relative => relative_heddle_invocation().to_string(),
            PathMode::Absolute => std::env::current_exe()
                .context("resolving current executable for integration install")?
                .display()
                .to_string(),
        })
    }
}

impl std::fmt::Display for HeddleInvocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InstalledIntegration {
    harness: String,
    scope: IntegrationScope,
    method: String,
    paths: Vec<String>,
    status: String,
    heddle_version: String,
    /// Whether `paths` reference a PATH-relative `heddle` invocation or an
    /// absolute path baked in at install time. Defaults to `relative` on read
    /// for backward compat with manifests written before this field existed.
    #[serde(default)]
    path_mode: PathMode,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct IntegrationManifest {
    #[serde(default)]
    integrations: Vec<InstalledIntegration>,
}

#[derive(Debug, Serialize)]
struct IntegrationStatus {
    harness: String,
    scope: String,
    method: String,
    status: String,
    healthy: bool,
    paths: Vec<String>,
    capabilities: Vec<String>,
    capability_paths: Vec<String>,
    path_mode: String,
}

pub fn cmd_integration(cli: &Cli, command: IntegrationCommands) -> Result<()> {
    let repo = cli.open_repo()?;
    match command {
        IntegrationCommands::List => list_integrations(cli, &repo),
        IntegrationCommands::Install(args) => install_integrations(cli, &repo, args),
        IntegrationCommands::Doctor => doctor_integrations(cli, &repo),
        IntegrationCommands::Uninstall(args) => uninstall_integrations(cli, &repo, args),
        IntegrationCommands::Upgrade(args) => upgrade_integrations(cli, &repo, args),
        IntegrationCommands::Relay(args) => relay_integration(&repo, args),
        IntegrationCommands::Stamp(args) => stamp_integration(&repo, args),
    }
}

pub fn maybe_prompt_init_install(
    cli: &Cli,
    repo: &Repository,
    args: &crate::cli::InitArgs,
) -> Result<()> {
    let json = should_output_json(cli, Some(repo.config()));
    let harnesses = prompt_init_install_decision(cli, repo.root(), args, json)?;
    perform_init_install(cli, repo, args, &harnesses)
}

/// Pre-write phase of init harness selection: resolve any explicit
/// `--install-harnesses` request WITHOUT writing anything. Returns the
/// harnesses to install once writes are safe.
///
/// Init calls this before installing harnesses so scope errors fail before
/// integration files are written. The install itself is deferred to
/// [`perform_init_install`] after the repository is created. Only the
/// directory `root` is needed, so it works before the repository exists on disk.
pub fn prompt_init_install_decision(
    _cli: &Cli,
    root: &Path,
    args: &crate::cli::InitArgs,
    _json: bool,
) -> Result<Vec<String>> {
    // For now init never asks to install detected harnesses. Only an
    // explicit `--install-harnesses` selection installs anything;
    // detection is still available through `--install-harnesses auto`.
    let harnesses = if args.no_harness_install {
        Vec::new()
    } else {
        match &args.install_harnesses {
            Some(selection) => resolve_selection_for_root(root, selection)?,
            None => Vec::new(),
        }
    };

    // Validate the install plan with the SAME predicates the install path
    // uses, in this pre-write decision phase, so an invalid
    // `--harness-install-scope` OR a harness that rejects the chosen scope
    // (e.g. `codex` requires `--scope user`) fails before any repo is created
    // instead of after repository creation.
    // Only matters when something will actually be installed.
    if !harnesses.is_empty() {
        validate_install_plan(&harnesses, &args.harness_install_scope)?;
    }
    Ok(harnesses)
}

/// Post-write phase: install the harnesses chosen by
/// [`prompt_init_install_decision`]. No prompting happens here, so it is
/// safe to run after the repository has been created.
pub fn perform_init_install(
    cli: &Cli,
    repo: &Repository,
    args: &crate::cli::InitArgs,
    harnesses: &[String],
) -> Result<()> {
    if harnesses.is_empty() {
        return Ok(());
    }
    install_selected(
        cli,
        repo,
        harnesses,
        IntegrationScope::parse(&args.harness_install_scope)?,
        args.harness_install_force,
        PathMode::Relative,
    )
}

fn list_integrations(cli: &Cli, repo: &Repository) -> Result<()> {
    let manifest = load_manifest(repo)?;
    let statuses = manifest
        .integrations
        .into_iter()
        .map(|entry| integration_status(repo, &entry))
        .collect::<Result<Vec<_>>>()?;
    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&statuses)?);
    } else if statuses.is_empty() {
        println!("{}", empty_integrations_message());
    } else {
        for status in statuses {
            println!(
                "{}",
                list_status_line(
                    &status.harness,
                    &status.scope,
                    &status.status,
                    &status.method
                )
            );
            if !status.capabilities.is_empty() {
                println!("  capabilities: {}", status.capabilities.join(", "));
            }
            for path in status.paths {
                println!("  {}", path);
            }
        }
    }
    Ok(())
}

fn install_integrations(cli: &Cli, repo: &Repository, args: IntegrationInstallArgs) -> Result<()> {
    let harnesses = if args.harnesses.is_empty() {
        detect_harnesses(repo)?
    } else {
        normalize_harnesses(args.harnesses)?
    };
    let path_mode = PathMode::from(path_mode_from_absolute_flag(args.absolute_path));
    install_selected(
        cli,
        repo,
        &harnesses,
        IntegrationScope::parse(&args.scope)?,
        args.force,
        path_mode,
    )
}

fn install_selected(
    cli: &Cli,
    repo: &Repository,
    harnesses: &[String],
    scope: IntegrationScope,
    force: bool,
    path_mode: PathMode,
) -> Result<()> {
    let mut manifest = load_manifest(repo)?;
    for harness in harnesses {
        match harness.as_str() {
            "codex" => install_codex(repo, &mut manifest, &scope, force, path_mode)?,
            "claude-code" => install_claude(repo, &mut manifest, &scope, force, path_mode)?,
            "opencode" => install_opencode(repo, &mut manifest, &scope, force, path_mode)?,
            other => return Err(anyhow!(unsupported_harness_advice(other))),
        }
    }
    save_manifest(repo, &manifest)?;
    if !should_output_json(cli, Some(repo.config())) {
        println!("{}", installed_message(harnesses));
    }
    Ok(())
}

fn doctor_integrations(cli: &Cli, repo: &Repository) -> Result<()> {
    let manifest = load_manifest(repo)?;
    let statuses = manifest
        .integrations
        .iter()
        .map(|entry| integration_status(repo, entry))
        .collect::<Result<Vec<_>>>()?;
    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&statuses)?);
    } else if statuses.is_empty() {
        println!("{}", empty_integrations_message());
    } else {
        for status in statuses {
            println!(
                "{}",
                doctor_status_line(
                    &status.harness,
                    &status.scope,
                    &status.path_mode,
                    status.healthy,
                    &status.status,
                )
            );
        }
    }
    Ok(())
}

fn uninstall_integrations(cli: &Cli, repo: &Repository, args: IntegrationTargetArgs) -> Result<()> {
    let mut manifest = load_manifest(repo)?;
    let targets = target_harnesses(&manifest, args.harnesses)?;
    for harness in &targets {
        uninstall_one(repo, &mut manifest, harness)?;
    }
    save_manifest(repo, &manifest)?;
    if !should_output_json(cli, Some(repo.config())) {
        println!("{}", uninstalled_message(&targets));
    }
    Ok(())
}

fn upgrade_integrations(cli: &Cli, repo: &Repository, args: IntegrationTargetArgs) -> Result<()> {
    let mut manifest = load_manifest(repo)?;
    let targets = target_harnesses(&manifest, args.harnesses)?;
    for harness in &targets {
        let existing = manifest
            .integrations
            .iter()
            .find(|entry| &entry.harness == harness)
            .cloned();
        let scope = existing
            .as_ref()
            .map(|entry| entry.scope.clone())
            .unwrap_or(IntegrationScope::Repo);
        // Preserve the existing path mode across upgrades — do not silently flip
        // an absolute-path install back to relative just because the user ran
        // `integration upgrade`. New installs go through `install` and pick up
        // the relative default there.
        //
        // Manifests written before PathMode existed deserialize to the field's
        // Default (Relative). But every pre-PathMode install actually wrote
        // *absolute* paths — that's the codex-flagged regression. So when the
        // serde default fired (i.e. the on-disk manifest had no `path_mode`
        // field), use the actual installed config, not the default. We
        // re-read the harness's installed settings file and probe the first
        // emitted command for a leading `heddle` literal vs an absolute path.
        let path_mode = match existing.as_ref() {
            Some(entry) => detect_path_mode(harness.as_str(), entry).unwrap_or(entry.path_mode),
            None => PathMode::default(),
        };
        match harness.as_str() {
            "codex" => install_codex(repo, &mut manifest, &scope, true, path_mode)?,
            "claude-code" => install_claude(repo, &mut manifest, &scope, true, path_mode)?,
            "opencode" => install_opencode(repo, &mut manifest, &scope, true, path_mode)?,
            other => return Err(anyhow!(unsupported_harness_advice(other))),
        }
    }
    save_manifest(repo, &manifest)?;
    if !should_output_json(cli, Some(repo.config())) {
        println!("{}", upgraded_message(&targets));
    }
    Ok(())
}

/// Inspect the harness's installed config and decide whether the recorded
/// invocation is `heddle` (PATH-relative) or an absolute path. Returns `None`
/// when the file is unreadable, missing, or doesn't carry a recognisable
/// command — the caller falls back to the manifest's stored value (or the
/// default). Pre-PathMode manifests deserialize the field to its `Default`
/// (Relative) but every pre-PathMode install actually wrote absolute paths;
/// this probe lets the upgrade flow recover the real on-disk shape.
fn detect_path_mode(harness: &str, entry: &InstalledIntegration) -> Option<PathMode> {
    let path = PathBuf::from(entry.paths.first()?);
    let contents = fs::read_to_string(&path).ok()?;
    match harness {
        "claude-code" => {
            // Hooks are JSON: walk to the first relay command we emitted.
            let root: Value = serde_json::from_str(&contents).ok()?;
            let cmd = root
                .get("hooks")
                .and_then(Value::as_object)?
                .values()
                .find_map(|groups| {
                    groups.as_array()?.iter().find_map(|group| {
                        group
                            .get("hooks")?
                            .as_array()?
                            .iter()
                            .find_map(|h| h.get("command")?.as_str().map(str::to_string))
                    })
                })
                .or_else(|| {
                    // Fallback: statusLine command, which is also rewritten on install.
                    root.get("statusLine")?
                        .get("command")?
                        .as_str()
                        .map(str::to_string)
                })?;
            Some(path_mode_from_command(&cmd))
        }
        "codex" => {
            let value: toml::Value = toml::from_str(&contents).ok()?;
            if let Some(cmd) = value.get("hooks").and_then(|hooks| {
                hooks.as_table()?.values().find_map(|event| {
                    event.as_array()?.iter().find_map(|group| {
                        group
                            .get("hooks")?
                            .as_array()?
                            .iter()
                            .find_map(|hook| hook.get("command")?.as_str().map(str::to_string))
                    })
                })
            }) {
                return Some(path_mode_from_command(&cmd));
            }
            let arr = value.get("notify")?.as_array()?;
            let cmd = arr.get(2)?.as_str()?;
            Some(path_mode_from_command(cmd))
        }
        "opencode" => classify_opencode_plugin_path_mode(&contents).map(PathMode::from),
        _ => None,
    }
}

/// CLI wrapper: pure classifier lives in `heddle_core::integration_plan`.
fn path_mode_from_command(cmd: &str) -> PathMode {
    PathMode::from(heddle_core::integration_plan::classify_command_path_mode(
        cmd,
    ))
}

/// Test/compat alias used by unit tests that call the historical name.
#[cfg(test)]
fn classify_command_path_mode(cmd: &str) -> PathMode {
    path_mode_from_command(cmd)
}

fn relay_integration(repo: &Repository, args: IntegrationRelayArgs) -> Result<()> {
    let mut payload = String::new();
    io::stdin().read_to_string(&mut payload)?;
    harness::relay_harness_event(repo, &args.harness, &args.event, &payload)
}

fn stamp_integration(repo: &Repository, args: crate::cli::IntegrationStampArgs) -> Result<()> {
    let mut payload = String::new();
    io::stdin().read_to_string(&mut payload)?;
    crate::identity_stamp::stamp_bytes(repo.root(), &args.harness, &payload)?;
    Ok(())
}

fn manifest_path(repo: &Repository) -> PathBuf {
    repo.root().join(".heddle/state").join(MANIFEST_FILE)
}

fn load_manifest(repo: &Repository) -> Result<IntegrationManifest> {
    let path = manifest_path(repo);
    if !path.exists() {
        return Ok(IntegrationManifest::default());
    }
    let contents = fs::read_to_string(path)?;
    Ok(toml::from_str(&contents)?)
}

fn save_manifest(repo: &Repository, manifest: &IntegrationManifest) -> Result<()> {
    let path = manifest_path(repo);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let contents = toml::to_string_pretty(manifest)?;
    write_file_atomic(&path, contents.as_bytes())?;
    Ok(())
}

fn integration_status(
    _repo: &Repository,
    entry: &InstalledIntegration,
) -> Result<IntegrationStatus> {
    let mut healthy = true;
    let mut status = healthy_status_token().to_string();
    for path in &entry.paths {
        if !Path::new(path).exists() {
            healthy = false;
            status = missing_status_token().to_string();
        }
    }
    if healthy && entry.harness == "claude-code" {
        let settings = entry.paths.first().map(PathBuf::from);
        if let Some(path) = settings
            && fs::read_to_string(&path)
                .map(|contents| !claude_settings_has_relay(&contents))
                .unwrap_or(true)
        {
            healthy = false;
            status = drifted_status_token().to_string();
        }
    }
    if healthy && entry.harness == "codex" {
        let path = entry.paths.first().map(PathBuf::from);
        if let Some(path) = path
            && fs::read_to_string(&path)
                .map(|contents| !codex_config_has_relay(&contents))
                .unwrap_or(true)
        {
            healthy = false;
            status = drifted_status_token().to_string();
        }
    }
    let capability_paths = integration_capability_paths(entry);
    Ok(IntegrationStatus {
        harness: entry.harness.clone(),
        scope: entry.scope.as_kind().as_str().to_string(),
        method: entry.method.clone(),
        status,
        healthy,
        paths: entry.paths.clone(),
        path_mode: entry.path_mode.as_kind().as_str().to_string(),
        capabilities: plan_integration_capabilities(&entry.harness, !capability_paths.is_empty()),
        capability_paths,
    })
}

fn integration_capability_paths(entry: &InstalledIntegration) -> Vec<String> {
    entry
        .paths
        .iter()
        .filter(|path| is_timeline_capability_path(path))
        .cloned()
        .collect()
}

fn detect_harnesses(repo: &Repository) -> Result<Vec<String>> {
    Ok(detect_harnesses_for_root(repo.root()))
}

/// Path-based harness detection: PATH lookups for the harness binaries
/// plus `.claude`/`.opencode` directory probes under `root`. Works
/// before the repository exists, which explicit pre-write
/// `--install-harnesses auto` resolution relies on.
fn detect_harnesses_for_root(root: &Path) -> Vec<String> {
    let mut found = BTreeSet::new();
    for harness in ["codex", "claude", "opencode"] {
        if command_on_path(harness) {
            let normalized = match harness {
                "claude" => "claude-code",
                other => other,
            };
            found.insert(normalized.to_string());
        }
    }
    if root.join(".claude").exists() {
        found.insert("claude-code".to_string());
    }
    if root.join(".opencode").exists() {
        found.insert("opencode".to_string());
    }
    found.into_iter().collect()
}

fn command_on_path(bin: &str) -> bool {
    env::var_os("PATH")
        .map(|paths| env::split_paths(&paths).collect::<Vec<_>>())
        .into_iter()
        .flatten()
        .any(|dir| dir.join(bin).exists())
}

fn resolve_selection_for_root(root: &Path, selection: &str) -> Result<Vec<String>> {
    match plan_harness_selection(selection).map_err(harness_error_advice)? {
        HarnessSelectionPlan::None => Ok(Vec::new()),
        HarnessSelectionPlan::Auto => Ok(detect_harnesses_for_root(root)),
        HarnessSelectionPlan::Explicit(names) => Ok(names),
    }
}

fn normalize_harnesses(harnesses: Vec<String>) -> Result<Vec<String>> {
    normalize_harness_names(harnesses).map_err(harness_error_advice)
}

fn harness_error_advice(err: IntegrationHarnessError) -> anyhow::Error {
    match err {
        IntegrationHarnessError::Unsupported { harness } => {
            anyhow!(unsupported_harness_advice(&harness))
        }
    }
}

fn unsupported_harness_advice(harness: &str) -> RecoveryAdvice {
    RecoveryAdvice::invalid_usage(
        "integration_harness_unsupported",
        format!("unsupported harness: {harness}"),
        "Use one of: codex, claude-code, opencode.",
        "heddle integration install codex",
    )
}

fn target_harnesses(manifest: &IntegrationManifest, requested: Vec<String>) -> Result<Vec<String>> {
    if requested.is_empty() {
        return Ok(manifest
            .integrations
            .iter()
            .map(|entry| entry.harness.clone())
            .collect());
    }
    normalize_harnesses(requested)
}

/// Single source of truth for which install scopes a harness accepts. Both the
/// pre-write preflight ([`validate_install_plan`], so a scope a harness will
/// reject fails BEFORE any repository is created) and the actual install path
/// ([`install_codex`] et al.) call this, so the two can never disagree — a
/// future scope-restricted harness adds its rule here and is automatically
/// enforced in the preflight. This closes the class behind cid 3329409818: a
/// `heddle init --install-harnesses codex` with the default `--scope repo`
/// must fail in the preflight, not after integration writes have started.
fn validate_harness_scope(harness: &str, scope: &IntegrationScope) -> Result<()> {
    plan_validate_harness_scope(harness, scope.as_kind()).map_err(|err| match err {
        IntegrationHarnessScopeError::CodexRequiresUser => anyhow!(RecoveryAdvice::invalid_usage(
            "integration_codex_scope_invalid",
            "codex integration currently requires --scope user",
            "Rerun the install with `--scope user`.",
            "heddle integration install codex --scope user",
        )),
    })
}

/// Pre-write validation of a harness-install plan: the scope string parses AND
/// every selected harness accepts that scope. Init runs this before installing
/// anything so a harness/scope combination that the install would reject (e.g.
/// `codex` + `repo`) fails before `install_selected` starts writing files.
fn validate_install_plan(harnesses: &[String], scope_value: &str) -> Result<()> {
    let scope = IntegrationScope::parse(scope_value)?;
    plan_validate_install_plan(harnesses, scope.as_kind()).map_err(|err| match err {
        IntegrationHarnessScopeError::CodexRequiresUser => anyhow!(RecoveryAdvice::invalid_usage(
            "integration_codex_scope_invalid",
            "codex integration currently requires --scope user",
            "Rerun the install with `--scope user`.",
            "heddle integration install codex --scope user",
        )),
    })
}

fn install_codex(
    _repo: &Repository,
    manifest: &mut IntegrationManifest,
    scope: &IntegrationScope,
    force: bool,
    path_mode: PathMode,
) -> Result<()> {
    validate_harness_scope("codex", scope)?;
    let home = env::var("HOME").context("HOME is required for codex integration install")?;
    let config_path = PathBuf::from(home).join(".codex").join("config.toml");
    let existing = if config_path.exists() {
        fs::read_to_string(&config_path)?
    } else {
        String::new()
    };
    if existing.contains("notify =")
        && !existing.contains("integration relay codex")
        && !existing.contains("integration stamp codex")
        && !force
    {
        return Err(anyhow!(
            "codex config already defines a non-Heddle notify command; rerun with --force after manual review"
        ));
    }
    let mut value = if existing.trim().is_empty() {
        toml::Value::Table(toml::map::Map::new())
    } else {
        toml::from_str(&existing)?
    };
    let heddle = HeddleInvocation::resolve(path_mode)?;
    // Workspace sidecar: discover `.heddle` from cwd. Do not bake the
    // install-repo path — a second workspace would stamp the wrong tree.
    let stamp = format!("{heddle} integration stamp codex");
    let table = value
        .as_table_mut()
        .ok_or_else(|| anyhow!("codex config root must be a TOML table"))?;
    let features = table
        .entry("features")
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    features
        .as_table_mut()
        .ok_or_else(|| anyhow!("codex features must be a table"))?
        .insert("hooks".to_string(), toml::Value::Boolean(true));
    let hooks = table
        .entry("hooks")
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    let hooks_table = hooks
        .as_table_mut()
        .ok_or_else(|| anyhow!("codex hooks must be a table"))?;
    for event in ["SessionStart", "SubagentStart", "PreToolUse", "Stop"] {
        let command = if event == "Stop" {
            format!("{stamp} --expire")
        } else {
            stamp.clone()
        };
        upsert_codex_hook_event(hooks_table, event, &command);
    }
    if table.get("notify").is_some_and(|notify| {
        let text = notify.to_string();
        text.contains("integration relay codex") || text.contains("integration stamp codex")
    }) {
        table.remove("notify");
    }
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)?;
    }
    write_file_atomic(&config_path, toml::to_string_pretty(&value)?.as_bytes())?;
    upsert_manifest(
        manifest,
        InstalledIntegration {
            harness: "codex".to_string(),
            scope: scope.clone(),
            method: "hooks".to_string(),
            paths: vec![config_path.display().to_string()],
            status: "installed".to_string(),
            heddle_version: env!("CARGO_PKG_VERSION").to_string(),
            path_mode,
        },
    );
    Ok(())
}

fn install_claude(
    repo: &Repository,
    manifest: &mut IntegrationManifest,
    scope: &IntegrationScope,
    _force: bool,
    path_mode: PathMode,
) -> Result<()> {
    let settings_path = match scope {
        IntegrationScope::Repo => repo.root().join(".claude").join("settings.json"),
        IntegrationScope::User => PathBuf::from(env::var("HOME")?)
            .join(".claude")
            .join("settings.json"),
    };
    let mut root: Value = if settings_path.exists() {
        serde_json::from_str(&fs::read_to_string(&settings_path)?)?
    } else {
        serde_json::json!({})
    };
    let hooks = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("claude settings root must be a JSON object"))?
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));
    let hooks_obj = hooks
        .as_object_mut()
        .ok_or_else(|| anyhow!("claude settings hooks must be an object"))?;

    let heddle = HeddleInvocation::resolve(path_mode)?;
    let stamp = format!(
        "{} --repo {} integration stamp claude-code",
        heddle,
        shell_escape(repo.root())
    );
    for event in [
        "SessionStart",
        "UserPromptSubmit",
        "PreToolUse",
        "PostToolUse",
        "SubagentStart",
        "SubagentStop",
        "Stop",
        "SessionEnd",
    ] {
        let command = if event == "PreToolUse" {
            stamp.clone()
        } else if event == "SessionEnd" {
            format!("{stamp} --expire")
        } else {
            format!(
                "{} --repo {} integration relay claude-code {}",
                heddle,
                shell_escape(repo.root()),
                event
            )
        };
        upsert_claude_hook_group(hooks_obj, event, command)?;
    }
    let root_obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("claude settings root must be a JSON object"))?;
    let existing_status = root_obj
        .get("statusLine")
        .and_then(Value::as_object)
        .and_then(|obj| obj.get("command"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let mut status_cmd = stamp;
    if let Some(original) = status_line_user_command(existing_status.as_deref()) {
        status_cmd.push_str(" --chain ");
        status_cmd.push_str(&shell_escape_arg(&original));
    }
    root_obj.insert(
        "statusLine".to_string(),
        serde_json::json!({
            "type": "command",
            "command": status_cmd
        }),
    );
    if let Some(parent) = settings_path.parent() {
        fs::create_dir_all(parent)?;
    }
    write_file_atomic(
        &settings_path,
        serde_json::to_string_pretty(&root)?.as_bytes(),
    )?;
    upsert_manifest(
        manifest,
        InstalledIntegration {
            harness: "claude-code".to_string(),
            scope: scope.clone(),
            method: "hooks+statusline".to_string(),
            paths: vec![settings_path.display().to_string()],
            status: "installed".to_string(),
            heddle_version: env!("CARGO_PKG_VERSION").to_string(),
            path_mode,
        },
    );
    Ok(())
}

fn install_opencode(
    repo: &Repository,
    manifest: &mut IntegrationManifest,
    scope: &IntegrationScope,
    _force: bool,
    path_mode: PathMode,
) -> Result<()> {
    let plugin_path = match scope {
        IntegrationScope::Repo => repo
            .root()
            .join(".opencode")
            .join("plugins")
            .join("heddle.js"),
        IntegrationScope::User => PathBuf::from(env::var("HOME")?)
            .join(".config")
            .join("opencode")
            .join("plugins")
            .join("heddle.js"),
    };
    if let Some(parent) = plugin_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let timeline_manifest_path = plugin_path.with_file_name("heddle.timeline.json");
    let heddle_raw = HeddleInvocation::raw(path_mode)?;
    let script = opencode_plugin_script(&heddle_raw, &repo.root().display().to_string());
    write_file_atomic(&plugin_path, script.as_bytes())?;
    let capabilities = opencode_timeline_capabilities(repo, &heddle_raw);
    write_file_atomic(
        &timeline_manifest_path,
        serde_json::to_string_pretty(&capabilities)?.as_bytes(),
    )?;
    upsert_manifest(
        manifest,
        InstalledIntegration {
            harness: "opencode".to_string(),
            scope: scope.clone(),
            method: "plugin".to_string(),
            paths: vec![
                plugin_path.display().to_string(),
                timeline_manifest_path.display().to_string(),
            ],
            status: "installed".to_string(),
            heddle_version: env!("CARGO_PKG_VERSION").to_string(),
            path_mode,
        },
    );
    Ok(())
}

fn opencode_plugin_script(exe: &str, repo: &str) -> String {
    format!(
        r#"export default async function() {{
  return {{
    event: async (input) => {{
      const eventObj = input?.event || input;
      const event = eventObj?.type || eventObj?.name || input?.type || input?.name || "event";
      const expire = ["session.deleted","session.closed","session.end","session.idle","SessionEnd"].includes(event);
      const stamp = ["--repo", {repo:?}, "integration", "stamp", "opencode"];
      if (expire) stamp.push("--expire");
      Bun.spawnSync([{exe:?}, ...stamp], {{
        stdin: JSON.stringify(input),
      }});
      const allowed = new Set(["session.created","session.updated","session.diff","file.edited","tool.execute.before","tool.execute.after","permission.asked","permission.replied"]);
      if (allowed.has(event)) {{
        Bun.spawn([{exe:?}, "--repo", {repo:?}, "integration", "relay", "opencode", event], {{
          stdin: JSON.stringify(input),
        }});
      }}
    }},
  }};
}}
"#,
        repo = repo,
        exe = exe,
    )
}

fn opencode_timeline_capabilities(repo: &Repository, heddle_raw: &str) -> Value {
    let repo_path = repo.root().display().to_string();
    serde_json::json!({
        "schema_version": 1,
        "producer": "heddle",
        "harness": "opencode",
        "repo": repo_path,
        "binary": heddle_raw,
        "privacy": {
            "native_payloads": "summaries-and-hashes",
            "raw_payloads_synced_by_default": false
        },
        "timeline": {
            "schema_version": 1,
            "default_thread": "main",
            "default_harness": "opencode",
            "output_kinds": ["timeline_log", "timeline_action"],
            "selectors": {
                "step": ["--step", "<timeline_step_id>"],
                "tool_call": [
                    "--tool-call",
                    "<opencode_tool_call_id>",
                    "--harness",
                    "opencode",
                    "--session",
                    "<opencode_session_id>"
                ],
                "undo": ["--undo"],
                "redo": ["--redo"],
                "current": ["--current"]
            },
            "verbs": [
                {
                    "name": "timeline_log",
                    "verb": "log --timeline",
                    "intent": "Inspect the current timeline cursor, branches, and recent steps.",
                    "argv": ["--repo", repo_path, "log", "--timeline", "--thread", "<thread>", "--output", "json"],
                    "mutates_checkout": false
                },
                {
                    "name": "timeline_fork_from_tool_call",
                    "verb": "timeline fork",
                    "intent": "Create a branch from an OpenCode tool call or timeline step before experimenting.",
                    "argv": ["--repo", repo_path, "timeline", "fork", "--tool-call", "<opencode_tool_call_id>", "--harness", "opencode", "--session", "<opencode_session_id>", "--reason", "fan-out", "--output", "json"],
                    "mutates_checkout": false
                },
                {
                    "name": "timeline_reset_to_tool_call",
                    "verb": "timeline reset",
                    "intent": "Move the timeline cursor to an OpenCode tool call and optionally materialize checkout files.",
                    "argv": ["--repo", repo_path, "timeline", "reset", "--tool-call", "<opencode_tool_call_id>", "--harness", "opencode", "--session", "<opencode_session_id>", "--materialize", "--mode", "fail-if-dirty", "--output", "json"],
                    "mutates_checkout": true
                },
                {
                    "name": "timeline_undo",
                    "verb": "timeline reset",
                    "intent": "Move one reversible timeline step backward.",
                    "argv": ["--repo", repo_path, "timeline", "reset", "--undo", "--materialize", "--mode", "fail-if-dirty", "--output", "json"],
                    "mutates_checkout": true
                },
                {
                    "name": "timeline_redo",
                    "verb": "timeline reset",
                    "intent": "Move one reversible timeline step forward.",
                    "argv": ["--repo", repo_path, "timeline", "reset", "--redo", "--materialize", "--mode", "fail-if-dirty", "--output", "json"],
                    "mutates_checkout": true
                },
                {
                    "name": "timeline_recover",
                    "verb": "timeline recover",
                    "intent": "Inspect or complete recovery after an interrupted timeline materialization.",
                    "argv": ["--repo", repo_path, "timeline", "recover", "--thread", "<thread>", "--output", "json"],
                    "mutates_checkout": false
                }
            ]
        }
    })
}

fn uninstall_one(
    repo: &Repository,
    manifest: &mut IntegrationManifest,
    harness: &str,
) -> Result<()> {
    let Some(existing) = manifest
        .integrations
        .iter()
        .find(|entry| entry.harness == harness)
        .cloned()
    else {
        return Ok(());
    };
    heddle_core::expire_identity_cursor(repo.root()).map_err(anyhow::Error::from)?;
    match harness {
        "codex" => {
            if let Some(path) = existing.paths.first() {
                let config_path = PathBuf::from(path);
                if config_path.exists() {
                    let mut value: toml::Value =
                        toml::from_str(&fs::read_to_string(&config_path)?)?;
                    if let Some(table) = value.as_table_mut() {
                        if let Some(hooks) = table.get_mut("hooks").and_then(|v| v.as_table_mut()) {
                            for event in ["SessionStart", "SubagentStart", "PreToolUse", "Stop"] {
                                if let Some(groups) =
                                    hooks.get_mut(event).and_then(|v| v.as_array_mut())
                                {
                                    groups.retain(|group| !is_heddle_hook_text(&group.to_string()));
                                    if groups.is_empty() {
                                        hooks.remove(event);
                                    }
                                }
                            }
                            if hooks.is_empty() {
                                table.remove("hooks");
                            }
                        }
                        if table
                            .get("notify")
                            .is_some_and(|notify| is_heddle_hook_text(&notify.to_string()))
                        {
                            table.remove("notify");
                        }
                        write_file_atomic(
                            &config_path,
                            toml::to_string_pretty(&value)?.as_bytes(),
                        )?;
                    }
                }
            }
        }
        "claude-code" => {
            if let Some(path) = existing.paths.first() {
                let settings_path = PathBuf::from(path);
                if settings_path.exists() {
                    let mut root: Value =
                        serde_json::from_str(&fs::read_to_string(&settings_path)?)?;
                    if let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) {
                        for groups in hooks.values_mut() {
                            if let Some(array) = groups.as_array_mut() {
                                array.retain(|group| {
                                    let text = group.to_string();
                                    !text.contains("integration relay claude-code")
                                        && !text.contains("integration stamp claude-code")
                                });
                            }
                        }
                    }
                    if let Some(command) = root
                        .get("statusLine")
                        .and_then(Value::as_object)
                        .and_then(|obj| obj.get("command"))
                        .and_then(Value::as_str)
                        && (command.contains("integration relay claude-code StatusLine")
                            || command.contains("integration stamp claude-code"))
                    {
                        root.as_object_mut().map(|obj| obj.remove("statusLine"));
                    }
                    write_file_atomic(
                        &settings_path,
                        serde_json::to_string_pretty(&root)?.as_bytes(),
                    )?;
                }
            }
        }
        "opencode" => {
            for path in &existing.paths {
                let path = PathBuf::from(path);
                if path.exists() {
                    fs::remove_file(path)?;
                }
            }
        }
        _ => {}
    }
    manifest
        .integrations
        .retain(|entry| entry.harness != harness);
    Ok(())
}

fn upsert_manifest(manifest: &mut IntegrationManifest, entry: InstalledIntegration) {
    manifest
        .integrations
        .retain(|existing| existing.harness != entry.harness);
    manifest.integrations.push(entry);
    manifest
        .integrations
        .sort_by(|a, b| a.harness.cmp(&b.harness));
}

fn upsert_claude_hook_group(
    hooks_obj: &mut serde_json::Map<String, Value>,
    event: &str,
    command: String,
) -> Result<()> {
    let group = serde_json::json!({
        "matcher": "*",
        "hooks": [{
            "type": "command",
            "command": command
        }]
    });
    let entry = hooks_obj
        .entry(event.to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    let groups = entry
        .as_array_mut()
        .ok_or_else(|| anyhow!("claude hook event entries must be arrays"))?;
    groups.retain(|group| {
        let text = group.to_string();
        !text.contains("integration relay claude-code")
            && !text.contains("integration stamp claude-code")
    });
    groups.push(group);
    Ok(())
}

fn is_heddle_hook_text(text: &str) -> bool {
    text.contains("integration stamp") || text.contains("integration relay")
}

fn upsert_codex_hook_event(
    hooks_table: &mut toml::map::Map<String, toml::Value>,
    event: &str,
    command: &str,
) {
    let mut groups = hooks_table
        .get(event)
        .and_then(toml::Value::as_array)
        .cloned()
        .unwrap_or_default();
    groups.retain(|group| !is_heddle_hook_text(&group.to_string()));
    let mut hook = toml::map::Map::new();
    hook.insert(
        "type".to_string(),
        toml::Value::String("command".to_string()),
    );
    hook.insert(
        "command".to_string(),
        toml::Value::String(command.to_string()),
    );
    let mut group = toml::map::Map::new();
    group.insert("matcher".to_string(), toml::Value::String("*".to_string()));
    group.insert(
        "hooks".to_string(),
        toml::Value::Array(vec![toml::Value::Table(hook)]),
    );
    groups.push(toml::Value::Table(group));
    hooks_table.insert(event.to_string(), toml::Value::Array(groups));
}

fn status_line_user_command(existing: Option<&str>) -> Option<String> {
    let command = existing?.trim();
    if command.is_empty() {
        return None;
    }
    if let Some(chained) = extract_status_line_chain(command) {
        return Some(chained).filter(|value| !value.trim().is_empty());
    }
    if command.contains("integration stamp claude-code")
        || command.contains("integration relay claude-code StatusLine")
    {
        return None;
    }
    Some(command.to_string())
}

fn extract_status_line_chain(command: &str) -> Option<String> {
    if let Some(rest) = command.split_once(" --chain=").map(|(_, rest)| rest) {
        return Some(unshell_escape_arg(rest.trim()));
    }
    command
        .split_once(" --chain ")
        .map(|(_, rest)| unshell_escape_arg(rest.trim()))
}

fn unshell_escape_arg(value: &str) -> String {
    let value = value.trim();
    if let Some(inner) = value.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        return inner.replace("'\"'\"'", "'");
    }
    if let Some(inner) = value.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        return inner.replace("\\\"", "\"").replace("\\\\", "\\");
    }
    value.to_string()
}

fn shell_escape(path: &Path) -> String {
    shell_escape_arg(&path.display().to_string())
}

fn shell_escape_arg(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::cli::Commands;

    struct HomeEnvGuard(Option<std::ffi::OsString>);

    impl HomeEnvGuard {
        fn set(path: &Path) -> Self {
            let original = std::env::var_os("HOME");
            unsafe {
                std::env::set_var("HOME", path);
            }
            Self(original)
        }
    }

    impl Drop for HomeEnvGuard {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => unsafe { std::env::set_var("HOME", value) },
                None => unsafe { std::env::remove_var("HOME") },
            }
        }
    }

    fn init_repo() -> (tempfile::TempDir, Repository) {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        (temp, repo)
    }

    #[test]
    fn init_harness_selection_does_not_auto_connect_detected_harnesses() {
        let temp = tempfile::TempDir::new().unwrap();
        fs::create_dir(temp.path().join(".claude")).unwrap();
        let cli = Cli::parse_from(["heddle", "init"]);
        let Commands::Init(args) = &cli.command else {
            panic!("expected parsed init command");
        };

        let harnesses = prompt_init_install_decision(&cli, temp.path(), args, false).unwrap();

        assert!(
            harnesses.is_empty(),
            "init must not auto-select detected harnesses without --install-harnesses"
        );
    }

    #[test]
    fn claude_repo_install_writes_project_hooks_and_manifest() {
        let (_temp, repo) = init_repo();
        let mut manifest = IntegrationManifest::default();

        install_claude(
            &repo,
            &mut manifest,
            &IntegrationScope::Repo,
            false,
            PathMode::Relative,
        )
        .unwrap();

        let settings_path = repo.root().join(".claude").join("settings.json");
        let contents = fs::read_to_string(&settings_path).unwrap();
        assert!(contents.contains("integration relay claude-code SessionStart"));
        assert!(contents.contains("integration relay claude-code UserPromptSubmit"));
        assert!(contents.contains("integration stamp claude-code"));
        assert!(!contents.contains("integration relay claude-code PreToolUse"));
        assert!(contents.contains("integration relay claude-code PostToolUse"));
        assert!(contents.contains("integration relay claude-code SubagentStop"));
        assert!(contents.contains("integration relay claude-code Stop"));
        assert!(contents.contains("integration stamp claude-code --expire"));
        assert!(!contents.contains("integration relay claude-code SessionEnd"));
        assert!(contents.contains("statusLine"));

        // Default install must use the PATH-relative literal `heddle` and must
        // NOT bake in an absolute path. We assert the exact command shape so a
        // future regression that resurrects current_exe() trips this test.
        let parsed: Value = serde_json::from_str(&contents).unwrap();
        let session_start_cmd = parsed["hooks"]["SessionStart"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(
            session_start_cmd.starts_with("heddle --repo "),
            "expected PATH-relative `heddle` invocation, got: {session_start_cmd}"
        );
        assert!(
            !session_start_cmd.starts_with('/'),
            "expected no absolute path leading the command, got: {session_start_cmd}"
        );
        let status_line_cmd = parsed["statusLine"]["command"].as_str().unwrap();
        assert!(
            status_line_cmd.starts_with("heddle --repo "),
            "expected PATH-relative `heddle` invocation in statusLine, got: {status_line_cmd}"
        );

        assert_eq!(manifest.integrations.len(), 1);
        assert_eq!(manifest.integrations[0].harness, "claude-code");
        assert_eq!(manifest.integrations[0].path_mode, PathMode::Relative);
    }

    #[test]
    fn claude_repo_install_chains_existing_status_line() {
        let (_temp, repo) = init_repo();
        let settings_path = repo.root().join(".claude").join("settings.json");
        fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        fs::write(
            &settings_path,
            r#"{"statusLine":{"type":"command","command":"echo custom-status"}}"#,
        )
        .unwrap();
        let mut manifest = IntegrationManifest::default();
        install_claude(
            &repo,
            &mut manifest,
            &IntegrationScope::Repo,
            false,
            PathMode::Relative,
        )
        .unwrap();
        let parsed: Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        let status_line_cmd = parsed["statusLine"]["command"].as_str().unwrap();
        assert!(status_line_cmd.contains("integration stamp claude-code"));
        assert!(status_line_cmd.contains("--chain"));
        assert!(status_line_cmd.contains("echo custom-status"));
    }

    #[test]
    fn claude_status_line_reinstall_still_chains() {
        let (_temp, repo) = init_repo();
        let settings_path = repo.root().join(".claude").join("settings.json");
        fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        fs::write(
            &settings_path,
            r#"{"statusLine":{"type":"command","command":"echo custom-status"}}"#,
        )
        .unwrap();
        let mut manifest = IntegrationManifest::default();
        install_claude(
            &repo,
            &mut manifest,
            &IntegrationScope::Repo,
            false,
            PathMode::Relative,
        )
        .unwrap();
        install_claude(
            &repo,
            &mut manifest,
            &IntegrationScope::Repo,
            true,
            PathMode::Relative,
        )
        .unwrap();
        let parsed: Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        let status_line_cmd = parsed["statusLine"]["command"].as_str().unwrap();
        assert!(status_line_cmd.contains("integration stamp claude-code"));
        assert!(status_line_cmd.contains("--chain"));
        assert!(
            status_line_cmd.contains("echo custom-status"),
            "reinstall/upgrade must keep the user's StatusLine, got: {status_line_cmd}"
        );
        assert_eq!(
            status_line_cmd
                .matches("integration stamp claude-code")
                .count(),
            1,
            "must not nest stamp commands: {status_line_cmd}"
        );
    }

    #[test]
    fn claude_repo_install_with_absolute_path_bakes_current_exe() {
        let (_temp, repo) = init_repo();
        let mut manifest = IntegrationManifest::default();

        install_claude(
            &repo,
            &mut manifest,
            &IntegrationScope::Repo,
            false,
            PathMode::Absolute,
        )
        .unwrap();

        let settings_path = repo.root().join(".claude").join("settings.json");
        let contents = fs::read_to_string(&settings_path).unwrap();
        let parsed: Value = serde_json::from_str(&contents).unwrap();

        let exe = std::env::current_exe().unwrap();
        let escaped_exe = shell_escape(&exe);

        let session_start_cmd = parsed["hooks"]["SessionStart"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(
            session_start_cmd.starts_with(&escaped_exe),
            "expected absolute heddle path {escaped_exe} prefix, got: {session_start_cmd}"
        );
        assert!(
            !session_start_cmd.starts_with("heddle "),
            "absolute mode must not emit bare `heddle`, got: {session_start_cmd}"
        );

        let status_line_cmd = parsed["statusLine"]["command"].as_str().unwrap();
        assert!(
            status_line_cmd.starts_with(&escaped_exe),
            "expected absolute heddle path {escaped_exe} prefix in statusLine, got: {status_line_cmd}"
        );

        assert_eq!(manifest.integrations[0].path_mode, PathMode::Absolute);
    }

    #[test]
    fn opencode_repo_install_and_uninstall_manage_plugin_file() {
        let (_temp, repo) = init_repo();
        let mut manifest = IntegrationManifest::default();

        install_opencode(
            &repo,
            &mut manifest,
            &IntegrationScope::Repo,
            false,
            PathMode::Relative,
        )
        .unwrap();
        let plugin_path = repo
            .root()
            .join(".opencode")
            .join("plugins")
            .join("heddle.js");
        assert!(plugin_path.exists());
        let plugin_contents = fs::read_to_string(&plugin_path).unwrap();
        assert!(
            plugin_contents.contains("\"heddle\""),
            "opencode plugin should reference PATH-relative `heddle`, got: {plugin_contents}"
        );
        assert!(
            plugin_contents.contains("event?.type") || plugin_contents.contains("eventObj?.type"),
            "opencode plugin must read event.type, got: {plugin_contents}"
        );
        assert!(
            plugin_contents.contains("\"integration\", \"stamp\", \"opencode\"")
                && plugin_contents.contains("Bun.spawnSync"),
            "opencode writes must go through locked integration stamp, got: {plugin_contents}"
        );
        assert!(
            plugin_contents.contains("session.deleted") && plugin_contents.contains("--expire"),
            "plugin must expire the cursor on session end through stamp --expire"
        );
        assert!(
            !plugin_contents.contains("mergeCursor")
                && !plugin_contents.contains("unlinkSync")
                && !plugin_contents.contains("writeFileSync"),
            "plugin must not unlocked-RMW the identity sidecar, got: {plugin_contents}"
        );
        assert!(
            !plugin_contents.contains("session.get"),
            "plugin must not hunt session.get, got: {plugin_contents}"
        );
        let timeline_manifest_path = repo
            .root()
            .join(".opencode")
            .join("plugins")
            .join("heddle.timeline.json");
        assert!(timeline_manifest_path.exists());
        let timeline_manifest: Value =
            serde_json::from_str(&fs::read_to_string(&timeline_manifest_path).unwrap()).unwrap();
        assert_eq!(timeline_manifest["schema_version"], 1);
        assert_eq!(timeline_manifest["harness"], "opencode");
        assert_eq!(timeline_manifest["timeline"]["schema_version"], 1);
        assert_eq!(timeline_manifest["timeline"]["default_harness"], "opencode");
        assert!(
            timeline_manifest["timeline"]["verbs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|verb| verb["name"] == "timeline_reset_to_tool_call")
        );
        assert!(
            timeline_manifest["timeline"]["verbs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|verb| verb["name"] == "timeline_undo")
        );
        let status = integration_status(&repo, &manifest.integrations[0]).unwrap();
        assert_eq!(status.capabilities, vec!["timeline"]);
        assert_eq!(
            status.capability_paths,
            vec![timeline_manifest_path.display().to_string()]
        );

        heddle_core::write_identity_cursor(
            repo.root(),
            &heddle_core::IdentityCursor {
                provider: Some("opencode".into()),
                model: Some("sonnet".into()),
                ..heddle_core::IdentityCursor::default()
            },
        )
        .unwrap();
        uninstall_one(&repo, &mut manifest, "opencode").unwrap();
        assert!(!plugin_path.exists());
        assert!(!timeline_manifest_path.exists());
        assert!(manifest.integrations.is_empty());
        assert!(
            heddle_core::read_identity_cursor(repo.root()).is_empty(),
            "uninstall must expire the identity cursor"
        );
    }

    #[test]
    #[serial_test::serial]
    fn codex_user_install_writes_workspace_stamp_hooks() {
        // Serialize env-var access across tests. The credential store
        // (in CLI-owned hosted runtime when the client feature is enabled) has its own mutex; this is
        // a local fallback for cli-only builds.
        static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _env_lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (_temp, repo) = init_repo();
        let home = tempfile::TempDir::new().unwrap();
        let _home_guard = HomeEnvGuard::set(home.path());
        let mut manifest = IntegrationManifest::default();

        install_codex(
            &repo,
            &mut manifest,
            &IntegrationScope::User,
            false,
            PathMode::Relative,
        )
        .unwrap();

        let config_path = home.path().join(".codex").join("config.toml");
        let contents = fs::read_to_string(&config_path).unwrap();
        assert!(contents.contains("integration stamp codex"));
        assert!(contents.contains("SessionStart"));
        assert!(contents.contains("PreToolUse"));
        assert!(!contents.contains("notify ="));
        assert!(!contents.contains("/bin/sh"));
        assert!(
            !contents.contains("--repo"),
            "codex hook must discover the workspace from cwd, got: {contents}"
        );
        assert!(
            contents.contains("heddle integration stamp codex"),
            "expected PATH-relative `heddle` in codex hook command, got: {contents}"
        );
        assert_eq!(
            contents
                .matches("heddle integration stamp codex --expire\"")
                .count(),
            1,
            "Codex Stop must expire, same as Claude SessionEnd, got: {contents}"
        );
        assert_eq!(
            contents
                .matches("heddle integration stamp codex\"")
                .count(),
            3,
            "SessionStart/SubagentStart/PreToolUse must stamp without expiring, got: {contents}"
        );
        assert!(
            contents.contains("[[hooks.Stop]]")
                && contents.contains("heddle integration stamp codex --expire"),
            "Stop hook must be the expire command, got: {contents}"
        );
        assert_eq!(manifest.integrations[0].harness, "codex");
        assert_eq!(manifest.integrations[0].path_mode, PathMode::Relative);

        heddle_core::write_identity_cursor(
            repo.root(),
            &heddle_core::IdentityCursor {
                provider: Some("openai".into()),
                model: Some("gpt-5.4".into()),
                ..heddle_core::IdentityCursor::default()
            },
        )
        .unwrap();
        uninstall_one(&repo, &mut manifest, "codex").unwrap();
        assert!(
            heddle_core::read_identity_cursor(repo.root()).is_empty(),
            "codex uninstall must expire the identity cursor"
        );
        let after = fs::read_to_string(&config_path).unwrap();
        assert!(
            !after.contains("integration stamp"),
            "codex uninstall must remove stamp hooks, got: {after}"
        );
    }

    #[test]
    fn upgrade_preserves_path_mode_when_absolute() {
        let (_temp, repo) = init_repo();
        let mut manifest = IntegrationManifest::default();

        // First install with --absolute-path semantics.
        install_claude(
            &repo,
            &mut manifest,
            &IntegrationScope::Repo,
            false,
            PathMode::Absolute,
        )
        .unwrap();
        assert_eq!(manifest.integrations[0].path_mode, PathMode::Absolute);

        // Save and reload the manifest the way upgrade_integrations would, so we
        // exercise the same lookup path (find existing entry, read its path_mode).
        save_manifest(&repo, &manifest).unwrap();
        let mut reloaded = load_manifest(&repo).unwrap();

        // Simulate the upgrade body: look up existing entry, preserve mode, reinstall.
        let existing = reloaded
            .integrations
            .iter()
            .find(|entry| entry.harness == "claude-code")
            .cloned()
            .unwrap();
        install_claude(
            &repo,
            &mut reloaded,
            &existing.scope,
            true,
            existing.path_mode,
        )
        .unwrap();

        assert_eq!(reloaded.integrations[0].path_mode, PathMode::Absolute);

        let settings_path = repo.root().join(".claude").join("settings.json");
        let contents = fs::read_to_string(&settings_path).unwrap();
        let parsed: Value = serde_json::from_str(&contents).unwrap();
        let cmd = parsed["hooks"]["SessionStart"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(
            !cmd.starts_with("heddle "),
            "upgrade must not silently flip an absolute install to relative, got: {cmd}"
        );
    }

    /// Regression for codex feedback on PR #56: pre-PathMode manifests
    /// deserialize the missing `path_mode` field to its `Default`
    /// (Relative). But every pre-PathMode install actually wrote
    /// *absolute* paths. So `integration upgrade` on a legacy manifest
    /// silently flipped the install to PATH-relative — breaking
    /// machines where `heddle` isn't on PATH.
    ///
    /// Fix: when probing the existing install, read the actual settings
    /// file and prefer the on-disk command shape over the manifest's
    /// (defaulted) `path_mode`. Setup: install once with absolute mode,
    /// then drop the `path_mode` field from the manifest TOML to
    /// emulate a pre-PathMode install. The upgrade path must detect the
    /// absolute heddle prefix in `.claude/settings.json` and preserve
    /// absolute mode.
    #[test]
    fn upgrade_preserves_path_mode_for_legacy_manifest_with_absolute_install() {
        let (_temp, repo) = init_repo();
        let mut manifest = IntegrationManifest::default();

        install_claude(
            &repo,
            &mut manifest,
            &IntegrationScope::Repo,
            false,
            PathMode::Absolute,
        )
        .unwrap();

        // Confirm we actually wrote an absolute heddle prefix.
        let settings_path = repo.root().join(".claude").join("settings.json");
        let settings_contents = fs::read_to_string(&settings_path).unwrap();
        assert!(
            !settings_contents.contains("\"heddle --repo "),
            "absolute install must NOT have bare `heddle` prefix"
        );

        // Strip `path_mode` from the manifest entry to emulate a
        // pre-PathMode manifest. Round-tripping it through TOML drops
        // the field and the reload deserializes to the default (Relative)
        // — exactly the legacy shape we want to recover from.
        manifest.integrations[0].path_mode = PathMode::Absolute; // ensure it's there pre-strip
        save_manifest(&repo, &manifest).unwrap();
        let manifest_path = repo.root().join(".heddle/state").join(MANIFEST_FILE);
        let raw = fs::read_to_string(&manifest_path).unwrap();
        // Drop any line containing `path_mode` to simulate the legacy on-disk shape.
        let stripped: String = raw
            .lines()
            .filter(|l| !l.trim_start().starts_with("path_mode"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&manifest_path, stripped).unwrap();

        // Reload — `path_mode` is missing, serde defaults it to Relative.
        let reloaded = load_manifest(&repo).unwrap();
        assert_eq!(
            reloaded.integrations[0].path_mode,
            PathMode::Relative,
            "sanity: legacy manifest must deserialize to the field default"
        );

        // The fix: detect_path_mode reads the actual settings.json and
        // reports Absolute, overriding the (defaulted) manifest field.
        let detected =
            detect_path_mode("claude-code", &reloaded.integrations[0]).expect("detection succeeds");
        assert_eq!(
            detected,
            PathMode::Absolute,
            "detect_path_mode must read the on-disk settings and recognise an absolute install"
        );

        // Drive the same code path as `upgrade_integrations`: pick the
        // detected mode, then re-install. The resulting settings file
        // must still have an absolute prefix — no silent flip.
        let mut working = reloaded;
        let resolved_mode =
            detect_path_mode("claude-code", &working.integrations[0]).unwrap_or(PathMode::Relative);
        let scope = working.integrations[0].scope.clone();
        install_claude(&repo, &mut working, &scope, true, resolved_mode).unwrap();

        let settings_after = fs::read_to_string(&settings_path).unwrap();
        let parsed: Value = serde_json::from_str(&settings_after).unwrap();
        let cmd = parsed["hooks"]["SessionStart"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(
            !cmd.starts_with("heddle "),
            "upgrade of a legacy absolute install must NOT silently flip to PATH-relative, got: {cmd}"
        );
    }

    #[test]
    fn classify_command_path_mode_recognises_relative_and_absolute() {
        // Bare `heddle` literal at the start = relative.
        assert_eq!(
            classify_command_path_mode(
                "heddle --repo /some/path integration relay claude-code Stop"
            ),
            PathMode::Relative,
        );
        // Absolute path = absolute, with or without the shell-escape quotes.
        assert_eq!(
            classify_command_path_mode(
                "/Users/dev/.cargo/bin/heddle --repo /repo integration relay claude-code Stop"
            ),
            PathMode::Absolute,
        );
        assert_eq!(
            classify_command_path_mode(
                "'/Users/dev/.cargo/bin/heddle' --repo /repo integration relay claude-code Stop"
            ),
            PathMode::Absolute,
        );
    }

    #[test]
    fn upgrade_preserves_path_mode_when_relative() {
        let (_temp, repo) = init_repo();
        let mut manifest = IntegrationManifest::default();

        install_claude(
            &repo,
            &mut manifest,
            &IntegrationScope::Repo,
            false,
            PathMode::Relative,
        )
        .unwrap();
        save_manifest(&repo, &manifest).unwrap();

        let mut reloaded = load_manifest(&repo).unwrap();
        let existing = reloaded
            .integrations
            .iter()
            .find(|entry| entry.harness == "claude-code")
            .cloned()
            .unwrap();
        install_claude(
            &repo,
            &mut reloaded,
            &existing.scope,
            true,
            existing.path_mode,
        )
        .unwrap();

        assert_eq!(reloaded.integrations[0].path_mode, PathMode::Relative);
    }
}
