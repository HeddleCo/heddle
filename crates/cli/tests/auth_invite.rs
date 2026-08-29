// SPDX-License-Identifier: Apache-2.0

#![cfg(feature = "client")]

use std::process::Command;

fn heddle(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_heddle"))
        .args(args)
        .output()
        .expect("run built heddle binary")
}

#[test]
fn auth_invite_help_exposes_create_and_list_surface() {
    let output = heddle(&["auth", "invite", "--help"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Usage: heddle auth invite"));
    assert!(stdout.contains("--email"));
    assert!(stdout.contains("list"));
}

#[test]
fn unknown_auth_subcommands_exit_usage_instead_of_showing_parent_help() {
    for args in [
        &["auth", "garbage"][..],
        &["auth", "garbage", "--help"],
        &["auth", "create-invite", "--help"],
    ] {
        let output = heddle(args);
        assert_eq!(
            output.status.code(),
            Some(64),
            "{args:?} must be an EX_USAGE error; stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stdout.is_empty(),
            "invalid auth input must not print parent help"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("unrecognized subcommand"),
            "clap should identify the unknown nested command"
        );
    }
}
