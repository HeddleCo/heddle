// SPDX-License-Identifier: Apache-2.0
//! Run command implementation.

use std::process::{Command, Stdio};

use anyhow::{Result, anyhow};
use heddle_core::run_plan::{
    plan_run_command_empty, run_command_required_example, run_command_required_hint,
    run_command_required_kind, run_command_required_summary, run_failure_message, transcript_token,
    transport_token,
};
use repo::SessionManager;

use super::{
    advice::RecoveryAdvice,
    child_env::sanitized_child_env,
    thread_cmd::{current_thread, load_thread},
};
use crate::{cli::Cli, config::UserConfig};

pub fn cmd_run(cli: &Cli, thread: Option<String>, command: Vec<String>) -> Result<()> {
    if plan_run_command_empty(command.is_empty()) {
        return Err(anyhow!(RecoveryAdvice::invalid_usage(
            run_command_required_kind(),
            run_command_required_summary(),
            run_command_required_hint(),
            run_command_required_example(),
        )));
    }

    let repo = cli.open_repo()?;
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
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if let Ok(Some(session)) = SessionManager::new(repo.root()).get_current_session_id() {
        child.env("HEDDLE_SESSION_ID", session);
    }
    if let Ok(Some(segment)) = SessionManager::new(repo.root()).get_current_segment_id() {
        child.env("HEDDLE_SESSION_SEGMENT", segment);
    }
    let user_config = UserConfig::load_default().unwrap_or_default();
    child.env(
        "HEDDLE_HARNESS_TRANSPORT",
        transport_token(match user_config.harness.transport {
            crate::config::HarnessTransport::Spool => "spool",
            crate::config::HarnessTransport::Direct => "direct",
            crate::config::HarnessTransport::End => "end",
        }),
    );
    child.env(
        "HEDDLE_HARNESS_TRANSCRIPT",
        transcript_token(match user_config.harness.transcript {
            crate::config::HarnessTranscriptMode::Off => "off",
            crate::config::HarnessTranscriptMode::Summary => "summary",
            crate::config::HarnessTranscriptMode::Full => "full",
        }),
    );

    let status = child.status()?;

    if status.success() {
        Ok(())
    } else {
        Err(anyhow!(run_failure_message(
            program,
            &thread.id,
            status.code()
        )))
    }
}
