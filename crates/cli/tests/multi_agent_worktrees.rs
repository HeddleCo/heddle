// SPDX-License-Identifier: Apache-2.0
//! Integration tests for multi-agent parallel worktrees.
//!
//! Covers three features implemented together:
//!
//! 1. **Object store pointer** — `.heddle` as a file that points to a shared store.
//! 2. **`heddle worktree add`** — create a filesystem-isolated agent checkout.
//! 3. **Actor registry** — `heddle actor spawn / list / done`.

use std::{
    fs,
    process::{Command, Output},
    str,
};

use serde_json::Value;
use tempfile::TempDir;

#[path = "multi_agent_worktrees/agent_registry.rs"]
mod agent_registry;
#[cfg(target_os = "linux")]
#[path = "multi_agent_worktrees/daemon_lifecycle.rs"]
mod daemon_lifecycle;
#[path = "multi_agent_worktrees/e2e.rs"]
mod e2e;
#[path = "multi_agent_worktrees/materialized_threads_e2e.rs"]
mod materialized_threads_e2e;
#[path = "multi_agent_worktrees/objectstore_pointer.rs"]
mod objectstore_pointer;
#[path = "multi_agent_worktrees/thread_create.rs"]
mod thread_create;
#[cfg(target_os = "linux")]
#[path = "multi_agent_worktrees/virtualized_mount.rs"]
mod virtualized_mount;
#[path = "multi_agent_worktrees/worktree_add.rs"]
mod worktree_add;

fn translate_legacy_args(args: &[&str]) -> Vec<String> {
    let mut prefix = Vec::new();
    let mut i = 0;
    while i < args.len() && args[i].starts_with("--") {
        prefix.push(args[i].to_string());
        i += 1;
    }
    let rest = &args[i..];
    let translated = match rest {
        ["thread", "delete", name] => vec![
            "thread".into(),
            "drop".into(),
            (*name).into(),
            "--delete-thread".into(),
        ],
        _ => rest.iter().map(|arg| (*arg).to_string()).collect(),
    };
    prefix.extend(translated);
    prefix
}

fn heddle(args: &[&str], cwd: Option<&std::path::Path>) -> Result<String, String> {
    let output = heddle_output(args, cwd)?;
    let stdout = str::from_utf8(&output.stdout).unwrap_or("").to_string();
    let stderr = str::from_utf8(&output.stderr).unwrap_or("").to_string();
    if output.status.success() {
        Ok(stdout)
    } else {
        Err(format!(
            "Exit code: {:?}\nstdout: {}\nstderr: {}",
            output.status.code(),
            stdout,
            stderr
        ))
    }
}

fn heddle_output(args: &[&str], cwd: Option<&std::path::Path>) -> Result<Output, String> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_heddle"));
    cmd.args(translate_legacy_args(args));
    cmd.env("HEDDLE_PRINCIPAL_NAME", "Heddle Test")
        .env("HEDDLE_PRINCIPAL_EMAIL", "test@heddle.dev");
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd.output().map_err(|e| e.to_string())
}

fn heddle_argv_json<I, S>(args: I) -> Value
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    serde_json::json!(
        std::iter::once(env!("CARGO_BIN_EXE_heddle").to_string())
            .chain(args.into_iter().map(|arg| arg.as_ref().to_string()))
            .collect::<Vec<_>>()
    )
}

fn heddle_output_with_env(
    args: &[&str],
    cwd: Option<&std::path::Path>,
    envs: &[(&str, &str)],
) -> Result<Output, String> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_heddle"));
    cmd.args(translate_legacy_args(args));
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    for (key, value) in envs {
        cmd.env(key, value);
    }
    cmd.output().map_err(|e| e.to_string())
}

/// RAII wrapper around a per-test repo `TempDir` that also tears down
/// any heddled daemon spawned for this repo's state path.
///
/// The daemon's endpoint and registry files already live under the
/// per-test tempdir at `<tmp>/.heddle/state/`, so the registry path
/// is per-test by construction. This guard closes the remaining gap:
/// the daemon *process* itself is detached and survives the tempdir's
/// removal until its 300 s idle exit fires. Without this, parallel
/// CI runs accumulate orphan `heddled` processes.
///
/// `Drop` is best-effort:
/// * If no daemon ever ran for this repo, `heddle daemon stop` is a
///   cheap no-op success.
/// * If the daemon is alive, the stop verb drains live mounts and
///   exits cleanly.
/// * Failure is intentionally swallowed — a panicking test should not
///   compound a teardown error into a double-panic.
pub struct RepoFixture {
    /// `Option` so `Drop` can take the inner value before the tempdir
    /// is removed. After `take`, the path is gone and we won't run
    /// the daemon-stop step.
    inner: Option<TempDir>,
}

impl RepoFixture {
    fn from_temp(temp: TempDir) -> Self {
        Self { inner: Some(temp) }
    }
}

impl std::ops::Deref for RepoFixture {
    type Target = TempDir;

    fn deref(&self) -> &TempDir {
        self.inner
            .as_ref()
            .expect("RepoFixture inner TempDir taken before Drop")
    }
}

impl Drop for RepoFixture {
    fn drop(&mut self) {
        if let Some(temp) = self.inner.take() {
            // Best-effort daemon shutdown. Ignore the result: the
            // common case is "no daemon ever ran here", which still
            // returns success today, and a true failure shouldn't
            // mask whatever the test was actually asserting.
            let _ = heddle(&["daemon", "stop"], Some(temp.path()));
            // `temp` drops at end of scope, removing the tempdir
            // (including the now-irrelevant endpoint/registry files).
        }
    }
}

fn setup_repo(filename: &str, content: &str) -> RepoFixture {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join(filename), content).unwrap();
    heddle(&["capture", "-m", "init"], Some(temp.path())).unwrap();
    RepoFixture::from_temp(temp)
}

fn head_track(path: &std::path::Path) -> String {
    let out = heddle(&["--output", "json", "status"], Some(path)).unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    v["thread"].as_str().unwrap_or("").to_string()
}

/// Mutation `--output json` replies no longer embed `verification`
/// (the verification-claim gate still consults it in-memory, but it
/// is omitted from the wire). This helper grafts the proof back onto
/// the returned value for test ergonomics by invoking
/// `heddle verify --output json` after the original call.
pub(crate) fn inject_post_verification_at(cwd: &std::path::Path, mut value: Value) -> Value {
    let obj = match value.as_object_mut() {
        Some(obj) => obj,
        None => return value,
    };
    if obj.contains_key("verification") {
        return value;
    }
    let verify_out = match heddle_output(&["--output", "json", "verify"], Some(cwd)) {
        Ok(out) => out,
        Err(_) => return value,
    };
    let stream = if !verify_out.status.success() {
        verify_out.stderr
    } else {
        verify_out.stdout
    };
    let text = std::str::from_utf8(&stream).unwrap_or("");
    let parsed: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return value,
    };
    let verification = if parsed.get("kind") == Some(&Value::String("verify_failed".to_string())) {
        parsed.get("verification").cloned().unwrap_or(Value::Null)
    } else {
        let mut obj_map = parsed.as_object().cloned().unwrap_or_default();
        obj_map.remove("output_kind");
        obj_map.remove("repository_label");
        obj_map.remove("repository_context");
        obj_map.remove("clean");
        Value::Object(obj_map)
    };
    obj.insert("verification".to_string(), verification);
    value
}
