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

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;

    fn create_test_repo() -> (TempDir, Repository, HookManager) {
        let temp_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(temp_dir.path()).unwrap();
        let manager = HookManager::new(&repo);
        (temp_dir, repo, manager)
    }

    #[test]
    fn hook_name_mapping_round_trips_known_hooks() {
        for name in Hook::all() {
            let hook = Hook::from_name(name).expect("known hook should map to enum");
            assert_eq!(hook.filename(), *name);
        }

        assert!(Hook::from_name("pre-capture").is_none());
        assert!(Hook::from_name("../pre-push").is_none());
    }

    #[test]
    fn list_hooks_discovers_only_known_hook_names_sorted() {
        let (_temp_dir, _repo, manager) = create_test_repo();
        manager
            .install(Hook::PreSnapshot, "#!/bin/sh\nexit 0\n")
            .unwrap();
        manager
            .install(Hook::PostPush, "#!/bin/sh\nexit 0\n")
            .unwrap();
        fs::write(manager.hooks_dir.join("notes.txt"), "ignored").unwrap();

        assert_eq!(
            manager.list_hooks().unwrap(),
            vec!["post-push".to_string(), "pre-snapshot".to_string()]
        );
    }

    #[test]
    fn install_and_uninstall_are_idempotent() {
        let (_temp_dir, _repo, manager) = create_test_repo();

        manager
            .install(Hook::PrePull, "#!/bin/sh\necho first\n")
            .unwrap();
        manager
            .install(Hook::PrePull, "#!/bin/sh\necho second\n")
            .unwrap();

        assert!(manager.has_hook(Hook::PrePull));
        assert!(
            fs::read_to_string(manager.hook_path(Hook::PrePull))
                .unwrap()
                .contains("second")
        );
        assert!(manager.uninstall(Hook::PrePull).unwrap());
        assert!(!manager.has_hook(Hook::PrePull));
        assert!(!manager.uninstall(Hook::PrePull).unwrap());
    }

    #[test]
    #[cfg(unix)]
    fn run_with_payload_decodes_env_payload_stdin_and_stdout() {
        let (_temp_dir, repo, manager) = create_test_repo();
        manager
            .install(
                Hook::PreSnapshot,
                r#"#!/bin/sh
payload=$(cat)
printf '{"abort":"","custom":"%s","protocol":"%s","repo":"%s","payload":%s}
' "$HEDDLE_CUSTOM" "$HEDDLE_HOOK_PROTOCOL" "$HEDDLE_REPO" "$payload"
"#,
            )
            .unwrap();
        let ctx = HookContext::new(&repo).with_env("HEDDLE_CUSTOM", "present");

        let response = manager
            .run_with_payload(
                Hook::PreSnapshot,
                &ctx,
                &json!({"answer": 42, "path": "src/lib.rs"}),
                Duration::from_secs(1),
            )
            .unwrap()
            .expect("installed hook should run");

        assert_eq!(response.abort, "");
        assert_eq!(response.extra["custom"], "present");
        assert_eq!(response.extra["protocol"], "json");
        assert_eq!(
            response.extra["repo"],
            repo.root().to_string_lossy().as_ref()
        );
        assert_eq!(response.extra["payload"]["answer"], 42);
        assert_eq!(response.extra["payload"]["path"], "src/lib.rs");
    }

    #[test]
    #[cfg(unix)]
    fn run_reports_nonzero_exit() {
        let (_temp_dir, repo, manager) = create_test_repo();
        manager
            .install(Hook::PrePush, "#!/bin/sh\necho failed >&2\nexit 7\n")
            .unwrap();

        let err = manager
            .run(Hook::PrePush, &HookContext::new(&repo))
            .unwrap_err();

        assert!(err.to_string().contains("Hook pre-push failed"));
        assert!(err.to_string().contains("Some(7)"));
    }

    #[test]
    #[cfg(unix)]
    fn run_with_payload_turns_nonzero_exit_into_abort_response() {
        let (_temp_dir, repo, manager) = create_test_repo();
        manager
            .install(Hook::PreMerge, "#!/bin/sh\necho veto >&2\nexit 9\n")
            .unwrap();

        let response = manager
            .run_with_payload(
                Hook::PreMerge,
                &HookContext::new(&repo),
                &json!({"operation": "merge"}),
                Duration::from_secs(1),
            )
            .unwrap()
            .expect("installed hook should return a response");

        assert!(response.abort.contains("veto"));
        assert!(response.extra.is_null());
    }

    #[test]
    #[cfg(unix)]
    fn run_with_payload_defaults_on_invalid_or_empty_stdout() {
        let (_temp_dir, repo, manager) = create_test_repo();
        let ctx = HookContext::new(&repo);

        manager
            .install(Hook::PostPull, "#!/bin/sh\necho not-json\n")
            .unwrap();
        let invalid = manager
            .run_with_payload(
                Hook::PostPull,
                &ctx,
                &json!({"operation": "pull"}),
                Duration::from_secs(1),
            )
            .unwrap()
            .expect("installed hook should return default response");
        assert_eq!(invalid.abort, "");
        assert!(invalid.extra.is_null());

        manager
            .install(Hook::PostPull, "#!/bin/sh\nexit 0\n")
            .unwrap();
        let empty = manager
            .run_with_payload(
                Hook::PostPull,
                &ctx,
                &json!({"operation": "pull"}),
                Duration::from_secs(1),
            )
            .unwrap()
            .expect("installed hook should return default response");
        assert_eq!(empty.abort, "");
        assert!(empty.extra.is_null());
    }

    #[test]
    #[cfg(unix)]
    fn run_with_payload_times_out() {
        let (_temp_dir, repo, manager) = create_test_repo();
        manager
            .install(Hook::PreRebase, "#!/bin/sh\nsleep 5\n")
            .unwrap();

        let err = manager
            .run_with_payload(
                Hook::PreRebase,
                &HookContext::new(&repo),
                &json!({"operation": "rebase"}),
                Duration::from_millis(30),
            )
            .unwrap_err();

        assert!(err.to_string().contains("pre-rebase timed out"));
    }
}
