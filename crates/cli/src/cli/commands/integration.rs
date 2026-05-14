// SPDX-License-Identifier: Apache-2.0
//! Harness integration install and relay commands.

use std::{
    collections::BTreeSet,
    env, fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow};
use objects::fs_atomic::write_file_atomic;
use repo::Repository;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    cli::{
        Cli, IntegrationCommands, IntegrationInstallArgs, IntegrationRelayArgs,
        IntegrationTargetArgs, is_tty, should_output_json,
    },
    config::UserConfig,
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
        match value {
            "repo" => Ok(Self::Repo),
            "user" => Ok(Self::User),
            other => Err(anyhow!("invalid integration scope: {other}")),
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

/// Resolved heddle invocation token to splice into the generated hook command.
/// Either the literal string `heddle` (PATH-relative) or a shell-escaped absolute path.
struct HeddleInvocation(String);

impl HeddleInvocation {
    fn resolve(mode: PathMode) -> Result<Self> {
        Ok(match mode {
            PathMode::Relative => HeddleInvocation("heddle".to_string()),
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
            PathMode::Relative => "heddle".to_string(),
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
    path_mode: String,
}

pub fn cmd_integration(cli: &Cli, command: IntegrationCommands) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    match command {
        IntegrationCommands::List => list_integrations(cli, &repo),
        IntegrationCommands::Install(args) => install_integrations(cli, &repo, args),
        IntegrationCommands::Doctor => doctor_integrations(cli, &repo),
        IntegrationCommands::Uninstall(args) => uninstall_integrations(cli, &repo, args),
        IntegrationCommands::Upgrade(args) => upgrade_integrations(cli, &repo, args),
        IntegrationCommands::Relay(args) => relay_integration(&repo, args),
    }
}

pub fn maybe_prompt_init_install(
    cli: &Cli,
    repo: &Repository,
    args: &crate::cli::InitArgs,
) -> Result<()> {
    if should_output_json(cli, Some(repo.config()))
        || cli.quiet
        || !is_tty()
        || args.no_harness_install
    {
        if let Some(selection) = &args.install_harnesses {
            let harnesses = resolve_selection(repo, selection)?;
            if !harnesses.is_empty() {
                install_selected(
                    cli,
                    repo,
                    &harnesses,
                    IntegrationScope::parse(&args.harness_install_scope)?,
                    args.harness_install_force,
                    PathMode::Relative,
                )?;
            }
        }
        return Ok(());
    }

    let detected = detect_harnesses(repo)?;
    let harnesses = if let Some(selection) = &args.install_harnesses {
        resolve_selection(repo, selection)?
    } else {
        detected
    };
    if harnesses.is_empty() {
        return Ok(());
    }

    println!(
        "Connect Heddle to detected harnesses for ambient actor tracking? [{}] [y/N]",
        harnesses.join(", ")
    );
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    if !matches!(input.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        return Ok(());
    }

    install_selected(
        cli,
        repo,
        &harnesses,
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
        println!("No Heddle-managed harness integrations.");
    } else {
        for status in statuses {
            println!(
                "{} [{}] {} ({})",
                status.harness, status.scope, status.status, status.method
            );
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
    let path_mode = if args.absolute_path {
        PathMode::Absolute
    } else {
        PathMode::Relative
    };
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
            other => return Err(anyhow!("unsupported harness: {other}")),
        }
    }
    save_manifest(repo, &manifest)?;
    if !should_output_json(cli, Some(repo.config())) {
        println!(
            "Installed Heddle harness integrations for: {}",
            harnesses.join(", ")
        );
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
        println!("No Heddle-managed harness integrations.");
    } else {
        for status in statuses {
            println!(
                "{} [{}] (path: {}): {}",
                status.harness,
                status.scope,
                status.path_mode,
                if status.healthy {
                    "healthy"
                } else {
                    &status.status
                }
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
        println!(
            "Uninstalled Heddle harness integrations for: {}",
            targets.join(", ")
        );
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
        // field), trust the actual installed config, not the default. We
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
            other => return Err(anyhow!("unsupported harness: {other}")),
        }
    }
    save_manifest(repo, &manifest)?;
    if !should_output_json(cli, Some(repo.config())) {
        println!(
            "Upgraded Heddle harness integrations for: {}",
            targets.join(", ")
        );
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
            Some(classify_command_path_mode(&cmd))
        }
        "codex" => {
            // notify is `["/bin/sh", "-lc", "<cmd>"]` — read the third arg.
            let value: toml::Value = toml::from_str(&contents).ok()?;
            let arr = value.get("notify")?.as_array()?;
            let cmd = arr.get(2)?.as_str()?;
            Some(classify_command_path_mode(cmd))
        }
        "opencode" => {
            // Plugin script: the spawn invocation is the first quoted token in
            // `Bun.spawnSync([...])`. We look for either `"heddle"` (relative) or
            // a quoted absolute path. Probe the literal we emit at install time.
            if contents.contains("Bun.spawnSync([\"heddle\"")
                || contents.contains("Bun.spawnSync(['heddle'")
            {
                Some(PathMode::Relative)
            } else if contents.contains("Bun.spawnSync([\"/")
                || contents.contains("Bun.spawnSync(['/")
            {
                Some(PathMode::Absolute)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// A command line is "PATH-relative" iff its first whitespace-delimited
/// token is exactly `heddle`. Anything else (an absolute path, a
/// shell-escaped absolute path) classifies as Absolute.
fn classify_command_path_mode(cmd: &str) -> PathMode {
    let first = cmd
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_matches('\'');
    if first == "heddle" {
        PathMode::Relative
    } else {
        PathMode::Absolute
    }
}

fn relay_integration(repo: &Repository, args: IntegrationRelayArgs) -> Result<()> {
    let mut payload = String::new();
    io::stdin().read_to_string(&mut payload)?;
    let user_config = UserConfig::load_default().unwrap_or_default();
    harness::relay_harness_event(repo, &user_config, &args.harness, &args.event, &payload)
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
    let mut status = "healthy".to_string();
    for path in &entry.paths {
        if !Path::new(path).exists() {
            healthy = false;
            status = "missing".to_string();
        }
    }
    if healthy && entry.harness == "claude-code" {
        let settings = entry.paths.first().map(PathBuf::from);
        if let Some(path) = settings
            && fs::read_to_string(&path)
                .map(|contents| !contents.contains("heddle integration relay claude-code"))
                .unwrap_or(true)
        {
            healthy = false;
            status = "drifted".to_string();
        }
    }
    if healthy && entry.harness == "codex" {
        let path = entry.paths.first().map(PathBuf::from);
        if let Some(path) = path
            && fs::read_to_string(&path)
                .map(|contents| !contents.contains("integration relay codex notify"))
                .unwrap_or(true)
        {
            healthy = false;
            status = "drifted".to_string();
        }
    }
    Ok(IntegrationStatus {
        harness: entry.harness.clone(),
        scope: match entry.scope {
            IntegrationScope::Repo => "repo".to_string(),
            IntegrationScope::User => "user".to_string(),
        },
        method: entry.method.clone(),
        status,
        healthy,
        paths: entry.paths.clone(),
        path_mode: match entry.path_mode {
            PathMode::Relative => "relative".to_string(),
            PathMode::Absolute => "absolute".to_string(),
        },
    })
}

fn detect_harnesses(repo: &Repository) -> Result<Vec<String>> {
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
    if repo.root().join(".claude").exists() {
        found.insert("claude-code".to_string());
    }
    if repo.root().join(".opencode").exists() {
        found.insert("opencode".to_string());
    }
    Ok(found.into_iter().collect())
}

fn command_on_path(bin: &str) -> bool {
    env::var_os("PATH")
        .map(|paths| env::split_paths(&paths).collect::<Vec<_>>())
        .into_iter()
        .flatten()
        .any(|dir| dir.join(bin).exists())
}

fn resolve_selection(repo: &Repository, selection: &str) -> Result<Vec<String>> {
    match selection {
        "none" => Ok(Vec::new()),
        "auto" => detect_harnesses(repo),
        value => normalize_harnesses(value.split(',').map(|item| item.to_string()).collect()),
    }
}

fn normalize_harnesses(harnesses: Vec<String>) -> Result<Vec<String>> {
    let mut seen = BTreeSet::new();
    for harness in harnesses {
        let normalized = match harness.trim() {
            "" => continue,
            "claude" => "claude-code",
            "codex" => "codex",
            "claude-code" => "claude-code",
            "opencode" => "opencode",
            other => return Err(anyhow!("unsupported harness: {other}")),
        };
        seen.insert(normalized.to_string());
    }
    Ok(seen.into_iter().collect())
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

fn install_codex(
    repo: &Repository,
    manifest: &mut IntegrationManifest,
    scope: &IntegrationScope,
    force: bool,
    path_mode: PathMode,
) -> Result<()> {
    if *scope != IntegrationScope::User {
        return Err(anyhow!("codex integration currently requires --scope user"));
    }
    let home = env::var("HOME").context("HOME is required for codex integration install")?;
    let config_path = PathBuf::from(home).join(".codex").join("config.toml");
    let existing = if config_path.exists() {
        fs::read_to_string(&config_path)?
    } else {
        String::new()
    };
    if existing.contains("notify =")
        && !existing.contains("integration relay codex notify")
        && !force
    {
        return Err(anyhow!(
            "codex config already defines a non-Heddle notify command; rerun with --force after manual review"
        ));
    }
    let mut value = if existing.trim().is_empty() {
        toml::Value::Table(toml::map::Map::new())
    } else {
        existing.parse::<toml::Value>()?
    };
    let heddle = HeddleInvocation::resolve(path_mode)?;
    let command = format!(
        "{} --repo {} integration relay codex notify",
        heddle,
        shell_escape(repo.root())
    );
    let table = value
        .as_table_mut()
        .ok_or_else(|| anyhow!("codex config root must be a TOML table"))?;
    table.insert(
        "notify".to_string(),
        toml::Value::Array(vec![
            toml::Value::String("/bin/sh".to_string()),
            toml::Value::String("-lc".to_string()),
            toml::Value::String(command),
        ]),
    );
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)?;
    }
    write_file_atomic(&config_path, toml::to_string_pretty(&value)?.as_bytes())?;
    upsert_manifest(
        manifest,
        InstalledIntegration {
            harness: "codex".to_string(),
            scope: scope.clone(),
            method: "notify".to_string(),
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
        let command = format!(
            "{} --repo {} integration relay claude-code {}",
            heddle,
            shell_escape(repo.root()),
            event
        );
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
        let exists = groups
            .iter()
            .any(|group| group.to_string().contains("integration relay claude-code"));
        if !exists {
            groups.push(group);
        }
    }
    let root_obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("claude settings root must be a JSON object"))?;
    let install_status_line = match root_obj.get("statusLine") {
        None => true,
        Some(value) => value
            .as_object()
            .and_then(|obj| obj.get("command"))
            .and_then(Value::as_str)
            .is_some_and(|command| command.contains("integration relay claude-code StatusLine")),
    };
    if install_status_line {
        root_obj.insert(
            "statusLine".to_string(),
            serde_json::json!({
                "type": "command",
                "command": format!(
                    "{} --repo {} integration relay claude-code StatusLine",
                    heddle,
                    shell_escape(repo.root())
                )
            }),
        );
    }
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
    let heddle_raw = HeddleInvocation::raw(path_mode)?;
    let script = format!(
        "const relay = async (event, payload) => {{
  const proc = Bun.spawnSync([{exe:?}, '--repo', {repo:?}, 'integration', 'relay', 'opencode', event], {{
    stdin: JSON.stringify(payload),
  }});
  if (proc.exitCode !== 0) console.error(new TextDecoder().decode(proc.stderr));
}};

export default async function(ctx) {{
  return {{
    event: async (input) => {{
      const event = input?.event?.name || input?.name || 'event';
      const allowed = new Set(['session.created','session.updated','session.diff','file.edited','tool.execute.before','tool.execute.after','permission.asked','permission.replied']);
      if (allowed.has(event)) {{
        await relay(event, input);
      }}
    }},
  }};
}}",
        exe = heddle_raw,
        repo = repo.root().display().to_string(),
    );
    write_file_atomic(&plugin_path, script.as_bytes())?;
    upsert_manifest(
        manifest,
        InstalledIntegration {
            harness: "opencode".to_string(),
            scope: scope.clone(),
            method: "plugin".to_string(),
            paths: vec![plugin_path.display().to_string()],
            status: "installed".to_string(),
            heddle_version: env!("CARGO_PKG_VERSION").to_string(),
            path_mode,
        },
    );
    Ok(())
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
    match harness {
        "codex" => {
            if let Some(path) = existing.paths.first() {
                let config_path = PathBuf::from(path);
                if config_path.exists() {
                    let mut value = fs::read_to_string(&config_path)?.parse::<toml::Value>()?;
                    if let Some(table) = value.as_table_mut()
                        && table.get("notify").is_some_and(|notify| {
                            notify
                                .to_string()
                                .contains("integration relay codex notify")
                        })
                    {
                        table.remove("notify");
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
                                    !group.to_string().contains("integration relay claude-code")
                                });
                            }
                        }
                    }
                    if let Some(command) = root
                        .get("statusLine")
                        .and_then(Value::as_object)
                        .and_then(|obj| obj.get("command"))
                        .and_then(Value::as_str)
                        && command.contains("integration relay claude-code StatusLine")
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
    let _ = repo;
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

fn shell_escape(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(contents.contains("integration relay claude-code PreToolUse"));
        assert!(contents.contains("integration relay claude-code PostToolUse"));
        assert!(contents.contains("integration relay claude-code SubagentStop"));
        assert!(contents.contains("integration relay claude-code Stop"));
        assert!(contents.contains("integration relay claude-code StatusLine"));

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

        uninstall_one(&repo, &mut manifest, "opencode").unwrap();
        assert!(!plugin_path.exists());
        assert!(manifest.integrations.is_empty());
    }

    #[test]
    #[serial_test::serial]
    fn codex_user_install_writes_notify_command() {
        // Serialize env-var access across tests. The credential store
        // (in heddle-client when the client feature is enabled) has its own mutex; this is
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
        assert!(contents.contains("integration relay codex notify"));
        // The default codex install must invoke PATH-relative `heddle`, not the
        // absolute path of the current binary.
        assert!(
            contents.contains("\"heddle --repo "),
            "expected PATH-relative `heddle` in codex notify command, got: {contents}"
        );
        assert_eq!(manifest.integrations[0].harness, "codex");
        assert_eq!(manifest.integrations[0].path_mode, PathMode::Relative);
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
    /// file and trust the on-disk command shape over the manifest's
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