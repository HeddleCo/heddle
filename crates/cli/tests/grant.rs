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
fn grant_help_exposes_create_list_delete_and_stays_off_signup_invite() {
    let parent = heddle(&["grant", "--help"]);
    assert_eq!(parent.status.code(), Some(0));
    let parent_out = String::from_utf8_lossy(&parent.stdout);
    assert!(parent_out.contains("Usage: heddle grant"));
    assert!(parent_out.contains("create"));
    assert!(parent_out.contains("list"));
    assert!(parent_out.contains("delete"));
    assert!(
        parent_out.contains("signup-only") || parent_out.contains("signup"),
        "grant help must separate itself from signup invite:\n{parent_out}"
    );
    assert!(
        parent_out.contains("writer or below")
            && (parent_out.contains("admin and owner")
                || parent_out.contains("Admin and owner")),
        "grant help must state the agent grant ceiling:\n{parent_out}"
    );
    assert!(
        !parent_out.contains("auth invite --email"),
        "grant help must not overload auth invite:\n{parent_out}"
    );

    let create = heddle(&["grant", "create", "--help"]);
    assert_eq!(create.status.code(), Some(0));
    let create_out = String::from_utf8_lossy(&create.stdout);
    assert!(create_out.contains("--spool"));
    assert!(create_out.contains("--principal"));
    assert!(create_out.contains("--role"));
    assert!(create_out.contains("contributor"));
    assert!(
        create_out.contains("signup") || create_out.contains("auth invite"),
        "grant create help must say this is not a signup invite:\n{create_out}"
    );
    assert!(
        create_out.contains("writer or below")
            && create_out.contains("Admin and owner")
            && (create_out.contains("human-verified") || create_out.contains("human verification")),
        "grant create help must state the agent grant ceiling:\n{create_out}"
    );

    let list = heddle(&["grant", "list", "--help"]);
    assert_eq!(list.status.code(), Some(0));
    let list_out = String::from_utf8_lossy(&list.stdout);
    assert!(list_out.contains("--spool"));

    let delete = heddle(&["grant", "delete", "--help"]);
    assert_eq!(delete.status.code(), Some(0));
    let delete_out = String::from_utf8_lossy(&delete.stdout);
    assert!(delete_out.contains("<ID>"));
    assert!(delete_out.contains("--spool"));
    assert!(
        delete_out.contains("writer-or-below") || delete_out.contains("writer or below"),
        "grant delete help must state the agent grant ceiling:\n{delete_out}"
    );
}

#[test]
fn auth_invite_help_stays_signup_only() {
    let output = heddle(&["auth", "invite", "--help"]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Usage: heddle auth invite"));
    assert!(
        stdout.to_ascii_lowercase().contains("signup"),
        "auth invite help must stay signup-only:\n{stdout}"
    );
    assert!(
        stdout.contains("heddle grant"),
        "auth invite help should point collaborators at grant:\n{stdout}"
    );
    assert!(
        !stdout.lines().any(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with("--spool") && !trimmed.contains("heddle grant")
        }),
        "auth invite must not grow a --spool flag of its own:\n{stdout}"
    );
}
