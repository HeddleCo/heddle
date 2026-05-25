// SPDX-License-Identifier: Apache-2.0
//! Run command implementation.

use std::process::{Command, Stdio};

use anyhow::{Result, anyhow};
use repo::{Repository, SessionManager};

use super::{
    advice::RecoveryAdvice,
    thread_cmd::{current_thread, load_thread},
};
use crate::{cli::Cli, config::UserConfig};

pub fn cmd_run(cli: &Cli, thread: Option<String>, command: Vec<String>) -> Result<()> {
    if command.is_empty() {
        return Err(anyhow!(RecoveryAdvice::invalid_usage(
            "run_command_required",
            "Usage: heddle run --thread <name> -- <cmd...>",
            "Pass a command after `--` so Heddle knows what to execute in the thread checkout.",
            "heddle run --thread <name> -- <cmd...>",
        )));
    }

    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let thread = match thread {
        Some(thread_id) => load_thread(&repo, &thread_id)?,
        None => current_thread(&repo)?.ok_or_else(|| {
            anyhow!(RecoveryAdvice::no_current_thread(
                "run",
                Some("--thread"),
                "heddle run --thread <name> -- <cmd...>",
            ))
        })?,
    };

    let program = &command[0];
    let args = &command[1..];
    let mut child = Command::new(program);
    child
        .args(args)
        .current_dir(&thread.execution_path)
        .env_clear()
        .envs(sanitized_child_env())
        .env("HEDDLE_THREAD_ID", &thread.id)
        .env("HEDDLE_THREAD_NAME", &thread.thread)
        .env("HEDDLE_HARNESS_BRIDGE_REPO", repo.root())
        .env("HEDDLE_HARNESS_BRIDGE_SUBCOMMAND", "harness-bridge")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if let Ok(path) = std::env::current_exe() {
        child.env("HEDDLE_HARNESS_BRIDGE_BIN", path);
    }
    if let Ok(Some(session)) = SessionManager::new(repo.root()).get_current_session_id() {
        child.env("HEDDLE_SESSION_ID", session);
    }
    if let Ok(Some(segment)) = SessionManager::new(repo.root()).get_current_segment_id() {
        child.env("HEDDLE_SESSION_SEGMENT", segment);
    }
    let user_config = UserConfig::load_default().unwrap_or_default();
    child.env(
        "HEDDLE_HARNESS_TRANSPORT",
        match user_config.harness.transport {
            crate::config::HarnessTransport::Spool => "spool",
            crate::config::HarnessTransport::Direct => "direct",
            crate::config::HarnessTransport::End => "end",
        },
    );
    child.env(
        "HEDDLE_HARNESS_TRANSCRIPT",
        match user_config.harness.transcript {
            crate::config::HarnessTranscriptMode::Off => "off",
            crate::config::HarnessTranscriptMode::Summary => "summary",
            crate::config::HarnessTranscriptMode::Full => "full",
        },
    );

    let status = child.status()?;

    if status.success() {
        Ok(())
    } else {
        Err(anyhow!(
            "Command '{}' failed in thread '{}' with status {}",
            program,
            thread.id,
            status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "signal".to_string())
        ))
    }
}

fn sanitized_child_env() -> Vec<(String, String)> {
    std::env::vars()
        .filter(|(key, _)| {
            matches!(
                key.as_str(),
                "PATH" | "HOME" | "USER" | "LOGNAME" | "SHELL" | "TMPDIR" | "TEMP" | "TMP" | "LANG"
            ) || key.starts_with("LC_")
        })
        .collect()
}
