// SPDX-License-Identifier: Apache-2.0

#![cfg(feature = "client")]

use std::{
    path::Path,
    process::{Command, Output, Stdio},
};

use tempfile::TempDir;

fn isolated_command(home: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_heddle"));
    command
        .args(args)
        .current_dir(home)
        .stdin(Stdio::null())
        .env("HOME", home)
        .env("HEDDLE_HOME", home)
        .env_remove("HEDDLE_CONFIG")
        .env_remove("HEDDLE_CREDENTIAL")
        .env("NO_COLOR", "1");
    command
}

fn output_text(output: &Output) -> String {
    String::from_utf8_lossy(&[output.stdout.as_slice(), output.stderr.as_slice()].concat())
        .into_owned()
}

#[test]
fn auth_login_non_tty_fails_closed_with_invite_next() {
    let temp = TempDir::new().expect("tempdir");
    let output = isolated_command(temp.path(), &["auth", "login"])
        .output()
        .expect("run auth login");
    assert!(
        !output.status.success(),
        "non-TTY login must fail closed:\n{}",
        output_text(&output)
    );
    let text = output_text(&output);
    assert!(
        text.contains("heddle auth login --invite <code>"),
        "fail-closed must name the invite recovery:\n{text}"
    );
    assert!(
        text.contains("Next:"),
        "fail-closed must print a Next: line:\n{text}"
    );
    assert_eq!(
        output.status.code(),
        Some(78),
        "missing-precondition login must be Config (78):\n{text}"
    );
}

#[test]
fn identity_command_is_gone() {
    let temp = TempDir::new().expect("tempdir");
    let output = isolated_command(temp.path(), &["identity", "--help"])
        .output()
        .expect("run identity --help");
    assert!(
        !output.status.success(),
        "identity must not be a CLI command:\n{}",
        output_text(&output)
    );
    let text = output_text(&output);
    assert!(
        !text.contains("identity ensure") && !text.contains("claim-link"),
        "identity help must not survive:\n{text}"
    );
}

#[test]
fn whoami_help_does_not_offer_login() {
    let temp = TempDir::new().expect("tempdir");
    let output = isolated_command(temp.path(), &["whoami", "--help"])
        .output()
        .expect("run whoami --help");
    assert!(
        output.status.success(),
        "whoami --help failed:\n{}",
        output_text(&output)
    );
    let text = output_text(&output);
    assert!(
        !text.contains("whoami --login") && !text.contains("--login"),
        "whoami must not grow a login flag:\n{text}"
    );
}
