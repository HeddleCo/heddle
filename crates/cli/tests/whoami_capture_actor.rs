// SPDX-License-Identifier: Apache-2.0

#![cfg(feature = "client")]

use std::{
    path::Path,
    process::{Command, Output},
};

use serde_json::Value;
use tempfile::TempDir;

fn isolated_command(cwd: &Path, home: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_heddle"));
    command
        .args(args)
        .current_dir(cwd)
        .env("HOME", home)
        .env("HEDDLE_HOME", home)
        .env_remove("HEDDLE_CONFIG")
        .env_remove("HEDDLE_CREDENTIAL")
        .env_remove("HEDDLE_PRINCIPAL_NAME")
        .env_remove("HEDDLE_PRINCIPAL_EMAIL")
        .env_remove("HEDDLE_AGENT_PROVIDER")
        .env_remove("HEDDLE_AGENT_MODEL")
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .env_remove("FORCE_COLOR");
    command
}

fn output_text(output: &Output) -> String {
    String::from_utf8_lossy(&[output.stdout.as_slice(), output.stderr.as_slice()].concat())
        .into_owned()
}

fn assert_success(output: &Output, command: &str) {
    assert!(
        output.status.success(),
        "{command} failed with {:?}:\n{}",
        output.status.code(),
        output_text(output)
    );
}

#[test]
fn whoami_reports_init_principal_then_unauthenticated_hosted() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(&workspace).expect("workspace");

    let init = isolated_command(
        &workspace,
        &home,
        &[
            "init",
            "tiny-notes",
            "--principal-name",
            "Luke",
            "--principal-email",
            "luke@example.com",
        ],
    )
    .output()
    .expect("run init");
    assert_success(&init, "init tiny-notes");

    let repo = workspace.join("tiny-notes");
    let status = isolated_command(&repo, &home, &["status"])
        .output()
        .expect("run status");
    assert_success(&status, "status");
    assert!(
        output_text(&status).contains("Luke <luke@example.com> from user_config"),
        "status must show the init principal:\n{}",
        output_text(&status)
    );

    let whoami = isolated_command(&repo, &home, &["whoami"])
        .output()
        .expect("run whoami");
    assert_success(&whoami, "whoami");
    assert_eq!(whoami.status.code(), Some(0));
    let text = output_text(&whoami);
    let actor_at = text
        .find("Capture actor: Luke <luke@example.com>")
        .expect("whoami must report the capture actor first");
    let hosted_at = text
        .find("Hosted auth:")
        .expect("whoami must keep hosted auth as a second stanza");
    assert!(
        actor_at < hosted_at,
        "capture actor must precede hosted auth:\n{text}"
    );
    assert!(
        text.contains("Not authenticated"),
        "hosted stanza must stay clearly unauthenticated:\n{text}"
    );

    let json = isolated_command(&repo, &home, &["whoami", "--output", "json"])
        .output()
        .expect("run whoami json");
    assert_success(&json, "whoami --output json");
    assert_eq!(json.status.code(), Some(0));
    let value: Value = serde_json::from_slice(&json.stdout).unwrap_or_else(|error| {
        panic!(
            "whoami json did not parse: {error}; stdout={}",
            String::from_utf8_lossy(&json.stdout)
        )
    });
    assert_eq!(value["output_kind"], "whoami");
    assert_eq!(value["capture_actor"]["name"], "Luke");
    assert_eq!(value["capture_actor"]["email"], "luke@example.com");
    assert_eq!(value["capture_actor"]["source"], "user_config");
    assert_eq!(value["authenticated"], false);
    assert_eq!(value["source"], "none");
}

#[test]
fn whoami_help_names_capture_actor_and_hosted_auth_as_different_objects() {
    let temp = TempDir::new().expect("tempdir");
    let help = isolated_command(temp.path(), temp.path(), &["whoami", "--help"])
        .output()
        .expect("run whoami --help");
    assert_success(&help, "whoami --help");
    let text = output_text(&help);
    assert!(
        text.contains("capture actor"),
        "help must name the capture actor:\n{text}"
    );
    assert!(
        text.contains("hosted auth") || text.contains("Hosted auth"),
        "help must name hosted auth:\n{text}"
    );
    assert!(
        text.contains("different objects"),
        "help must say these are different objects:\n{text}"
    );
    assert!(
        text.contains("identity ensure"),
        "help must distinguish identity ensure from the local actor:\n{text}"
    );
}
