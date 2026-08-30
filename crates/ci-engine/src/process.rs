// SPDX-License-Identifier: Apache-2.0
//! Deterministic argv execution, bounded capture, and timeout teardown.

use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Seek, SeekFrom},
    process::{Command, Stdio},
    time::Duration,
};

use ci_config::Check;
use wait_timeout::ChildExt;

use crate::{Disposition, ProcGroupRegistry, strip_ansi};

const MAX_CAPTURE_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Default)]
pub(crate) struct RunOutput {
    pub(crate) disposition: Disposition,
    pub(crate) combined_output: String,
}

pub(crate) fn run_process(
    check: &Check,
    workdir: &std::path::Path,
    environment: &BTreeMap<String, String>,
    registry: Option<&ProcGroupRegistry>,
) -> RunOutput {
    let Some((program, args)) = check.command.split_first() else {
        return RunOutput::default();
    };
    let mut capture = match tempfile::tempfile() {
        Ok(file) => file,
        Err(_) => return RunOutput::default(),
    };
    let Ok(stdout) = capture.try_clone() else {
        return RunOutput::default();
    };
    let Ok(stderr) = capture.try_clone() else {
        return RunOutput::default();
    };

    let cwd = if check.working_directory.is_empty() {
        workdir.to_path_buf()
    } else {
        workdir.join(&check.working_directory)
    };
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(environment)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    set_process_group(&mut command);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return RunOutput::default(),
    };
    let process_group = child.id() as i32;
    if registry.is_some_and(|value| !value.register_active(process_group)) {
        ProcGroupRegistry::kill_group(process_group);
        let _ = child.wait();
        return RunOutput::default();
    }

    let disposition = match child.wait_timeout(Duration::from_secs(check.timeout_secs)) {
        Ok(Some(status)) if status.success() => Disposition::Success,
        Ok(Some(status)) => Disposition::Exited(status.code().unwrap_or(-1)),
        Ok(None) => {
            kill_process_tree(&mut child);
            Disposition::TimedOut
        }
        Err(_) => {
            kill_process_tree(&mut child);
            Disposition::InfraError
        }
    };
    if let Some(value) = registry {
        value.unregister(process_group);
    }
    RunOutput {
        disposition,
        combined_output: strip_ansi(&read_capture(&mut capture)),
    }
}

#[cfg(unix)]
fn set_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(not(unix))]
fn set_process_group(_command: &mut Command) {}

#[cfg(unix)]
fn kill_process_tree(child: &mut std::process::Child) {
    ProcGroupRegistry::kill_group(child.id() as i32);
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(not(unix))]
fn kill_process_tree(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn read_capture(file: &mut File) -> String {
    let length = file.seek(SeekFrom::End(0)).unwrap_or(0);
    let truncated = length > MAX_CAPTURE_BYTES;
    let start = length.saturating_sub(MAX_CAPTURE_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut bytes = Vec::new();
    if file
        .take(MAX_CAPTURE_BYTES)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return String::new();
    }
    let mut output = String::from_utf8_lossy(&bytes).into_owned();
    if truncated {
        output.insert_str(0, "… [output truncated; showing tail]\n");
    }
    output
}
