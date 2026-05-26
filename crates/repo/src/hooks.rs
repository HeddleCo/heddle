// SPDX-License-Identifier: Apache-2.0
//! Hooks system for Heddle.

use std::{
    fs,
    io::{Read, Write},
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::Repository;

/// Typed hook response. Captures the JSON object the hook
/// emits on stdout when invoked via [`HookManager::run_with_payload`].
/// Per-event richer response shapes (e.g.
/// `pre_capture::Response { extra_signals, abort }`) decode off
/// `extra` without further wiring here.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HookResponse {
    /// When non-empty, the operation aborts with this string as the
    /// reason. Universal veto channel.
    #[serde(default)]
    pub abort: String,
    /// Per-event extension fields.
    #[serde(flatten, default)]
    pub extra: serde_json::Value,
}

#[derive(Debug, Clone, Copy)]
pub enum Hook {
    PreSnapshot,
    PostSnapshot,
    PrePush,
    PostPush,
    PrePull,
    PostPull,
    PreMerge,
    PostMerge,
    PreRebase,
    PostRebase,
}

impl Hook {
    pub fn filename(&self) -> &'static str {
        match self {
            Hook::PreSnapshot => "pre-snapshot",
            Hook::PostSnapshot => "post-snapshot",
            Hook::PrePush => "pre-push",
            Hook::PostPush => "post-push",
            Hook::PrePull => "pre-pull",
            Hook::PostPull => "post-pull",
            Hook::PreMerge => "pre-merge",
            Hook::PostMerge => "post-merge",
            Hook::PreRebase => "pre-rebase",
            Hook::PostRebase => "post-rebase",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "pre-snapshot" => Some(Hook::PreSnapshot),
            "post-snapshot" => Some(Hook::PostSnapshot),
            "pre-push" => Some(Hook::PrePush),
            "post-push" => Some(Hook::PostPush),
            "pre-pull" => Some(Hook::PrePull),
            "post-pull" => Some(Hook::PostPull),
            "pre-merge" => Some(Hook::PreMerge),
            "post-merge" => Some(Hook::PostMerge),
            "pre-rebase" => Some(Hook::PreRebase),
            "post-rebase" => Some(Hook::PostRebase),
            _ => None,
        }
    }

    pub fn all() -> &'static [&'static str] {
        &[
            "pre-snapshot",
            "post-snapshot",
            "pre-push",
            "post-push",
            "pre-pull",
            "post-pull",
            "pre-merge",
            "post-merge",
            "pre-rebase",
            "post-rebase",
        ]
    }
}

#[derive(Debug)]
pub struct HookContext {
    pub repo_path: PathBuf,
    pub env: Vec<(String, String)>,
}

impl HookContext {
    pub fn new(repo: &Repository) -> Self {
        Self {
            repo_path: repo.root().to_path_buf(),
            env: Vec::new(),
        }
    }

    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }
}

pub struct HookManager {
    hooks_dir: PathBuf,
}

impl HookManager {
    pub fn new(repo: &Repository) -> Self {
        Self {
            hooks_dir: repo.heddle_dir().join("hooks"),
        }
    }

    pub fn hook_path(&self, hook: Hook) -> PathBuf {
        self.hooks_dir.join(hook.filename())
    }

    pub fn has_hook(&self, hook: Hook) -> bool {
        self.hook_path(hook).exists()
    }

    pub fn list_hooks(&self) -> Result<Vec<String>> {
        if !self.hooks_dir.exists() {
            return Ok(Vec::new());
        }

        let mut hooks = Vec::new();
        for entry in fs::read_dir(&self.hooks_dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if Hook::from_name(&name).is_some() {
                hooks.push(name);
            }
        }

        hooks.sort();
        Ok(hooks)
    }

    pub fn run(&self, hook: Hook, ctx: &HookContext) -> Result<bool> {
        let hook_path = self.hook_path(hook);

        if !hook_path.exists() {
            debug!("Hook {} does not exist, skipping", hook.filename());
            return Ok(false);
        }

        debug!("Running hook: {}", hook.filename());

        let mut cmd = Command::new(&hook_path);
        cmd.env_clear()
            .current_dir(&ctx.repo_path)
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", std::env::var("HOME").unwrap_or_default())
            .env("HEDDLE_REPO", &ctx.repo_path);

        for (key, value) in &ctx.env {
            cmd.env(key, value);
        }

        let status = cmd
            .status()
            .map_err(|e| anyhow!("Failed to execute hook {}: {}", hook.filename(), e))?;

        if !status.success() {
            return Err(anyhow!(
                "Hook {} failed with exit code {:?}",
                hook.filename(),
                status.code()
            ));
        }

        debug!("Hook {} completed successfully", hook.filename());
        Ok(true)
    }

    /// JSON-over-stdio hook invocation.
    ///
    /// Spawns the hook with the payload written to stdin (UTF-8
    /// JSON), reads stdout (also UTF-8 JSON) up to `timeout`, and
    /// decodes into a [`HookResponse`]. Non-zero exit collapses to
    /// `abort = stderr_text` so a crashed hook can't pretend to vote
    /// "ok". `Ok(None)` = hook not installed (treat as no-op).
    pub fn run_with_payload(
        &self,
        hook: Hook,
        ctx: &HookContext,
        payload: &serde_json::Value,
        timeout: Duration,
    ) -> Result<Option<HookResponse>> {
        let hook_path = self.hook_path(hook);
        if !hook_path.exists() {
            return Ok(None);
        }
        let mut cmd = Command::new(&hook_path);
        cmd.env_clear()
            .current_dir(&ctx.repo_path)
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", std::env::var("HOME").unwrap_or_default())
            .env("HEDDLE_REPO", &ctx.repo_path)
            .env("HEDDLE_HOOK_PROTOCOL", "json")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in &ctx.env {
            cmd.env(key, value);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow!("failed to spawn hook {}: {e}", hook.filename()))?;
        let payload_bytes =
            serde_json::to_vec(payload).map_err(|e| anyhow!("encode hook payload: {e}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(&payload_bytes);
        }
        let started = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let mut stdout = String::new();
                    if let Some(mut out) = child.stdout.take() {
                        let _ = out.read_to_string(&mut stdout);
                    }
                    let mut stderr = String::new();
                    if let Some(mut err) = child.stderr.take() {
                        let _ = err.read_to_string(&mut stderr);
                    }
                    if !status.success() {
                        return Ok(Some(HookResponse {
                            abort: if stderr.trim().is_empty() {
                                format!(
                                    "hook {} exited {}",
                                    hook.filename(),
                                    status.code().unwrap_or(-1)
                                )
                            } else {
                                stderr
                            },
                            extra: serde_json::Value::Null,
                        }));
                    }
                    if stdout.trim().is_empty() {
                        return Ok(Some(HookResponse::default()));
                    }
                    // Tolerate non-JSON stdout so legacy hooks that
                    // just `echo` informational text continue to work
                    // — they get treated as an implicit "no objections,
                    // no extras" response. Strict JSON-protocol hooks
                    // are still parsed when they emit valid JSON.
                    let response: HookResponse =
                        serde_json::from_str(stdout.trim()).unwrap_or_default();
                    return Ok(Some(response));
                }
                Ok(None) => {
                    if started.elapsed() > timeout {
                        let _ = child.kill();
                        return Err(anyhow!(
                            "hook {} timed out after {:?}",
                            hook.filename(),
                            timeout
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => {
                    return Err(anyhow!("wait on hook {} failed: {e}", hook.filename()));
                }
            }
        }
    }

    pub fn run_optional(&self, hook: Hook, ctx: &HookContext) -> bool {
        match self.run(hook, ctx) {
            Ok(ran) => ran,
            Err(e) => {
                warn!("Hook {} failed: {}", hook.filename(), e);
                false
            }
        }
    }

    pub fn install(&self, hook: Hook, content: &str) -> Result<()> {
        if !self.hooks_dir.exists() {
            fs::create_dir_all(&self.hooks_dir)?;
        }

        let hook_path = self.hook_path(hook);
        fs::write(&hook_path, content)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o755))?;
        }

        Ok(())
    }

    pub fn uninstall(&self, hook: Hook) -> Result<bool> {
        let hook_path = self.hook_path(hook);
        if hook_path.exists() {
            fs::remove_file(&hook_path)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}
