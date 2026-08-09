// SPDX-License-Identifier: Apache-2.0
//! Shared process harness for CLI integration-test binaries.
#![allow(dead_code)]

use std::{path::Path, process::Output};

const TEST_PRINCIPAL_NAME: &str = "Heddle Test";
const TEST_PRINCIPAL_EMAIL: &str = "test@heddle.dev";

/// Run the Heddle binary with the deterministic test principal.
///
/// Extra environment values are applied last so tests that exercise identity
/// resolution can intentionally override the defaults without mutating the
/// process-wide environment.
pub fn heddle_output(
    args: &[&str],
    cwd: Option<&Path>,
    envs: &[(&str, &str)],
) -> Result<Output, String> {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_heddle"));
    command
        .args(args)
        .env("HEDDLE_PRINCIPAL_NAME", TEST_PRINCIPAL_NAME)
        .env("HEDDLE_PRINCIPAL_EMAIL", TEST_PRINCIPAL_EMAIL)
        .envs(envs.iter().copied());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    command.output().map_err(|error| error.to_string())
}

/// Run Heddle and return UTF-8-lossy stdout on success or a diagnostic carrying
/// the complete exit status/stdout/stderr contract on failure.
pub fn heddle(args: &[&str], cwd: Option<&Path>, envs: &[(&str, &str)]) -> Result<String, String> {
    let output = heddle_output(args, cwd, envs)?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
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

/// Create a deterministic many-small-files fixture shared by CLI and storage
/// performance tests.
pub fn write_many_small_files(root: &Path, file_count: usize) {
    for index in 0..file_count {
        std::fs::write(
            root.join(format!("file_{index:05}.txt")),
            format!("content {index}\n{}\n", "x".repeat(48)),
        )
        .unwrap();
    }
}
