// SPDX-License-Identifier: Apache-2.0
//! `heddle env run` injects broker-unwrapped slots into a child only.

use std::path::Path;
use std::process::{Command, Output};

use crypto::Ed25519Signer;
use objects::object::{Attribution, Principal};
use repo::Repository;
use runtime_profile::{RuntimeProfileStore, SlotWrite};
use tempfile::TempDir;

fn isolated_heddle(cwd: &Path, home: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_heddle"));
    command
        .args(args)
        .current_dir(cwd)
        .env("HOME", home)
        .env("HEDDLE_HOME", home)
        .env("NO_COLOR", "1")
        .env_remove("HEDDLE_CONFIG")
        .env_remove("HEDDLE_CREDENTIAL")
        .env_remove("CLICOLOR_FORCE")
        .env_remove("FORCE_COLOR");
    command
}

fn run_heddle(cwd: &Path, home: &Path, args: &[&str]) -> Output {
    isolated_heddle(cwd, home, args)
        .output()
        .expect("run heddle")
}

fn seed_profile(repo: &Path, name: &str, slot: &str, value: &str) {
    let opened = Repository::open(repo).expect("open");
    let store = RuntimeProfileStore::open(opened.heddle_dir()).expect("store");
    let signer = Ed25519Signer::generate().expect("signer");
    let (recipient, _secret) = store
        .create_software_recipient(&signer, 1)
        .expect("recipient");
    store
        .create_profile(
            name,
            vec![SlotWrite {
                name: slot.to_string(),
                value: value.as_bytes().to_vec(),
            }],
            recipient.recipient_id,
            Attribution::human(Principal::new("Ada", "ada@example.com")),
            &signer,
        )
        .expect("create");
}

#[test]
fn env_run_injects_slot_and_leaves_no_worktree_plaintext() {
    let temp = TempDir::new().expect("temp repo");
    let home = temp.path().join("home");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(&repo).expect("repo");
    Repository::init_default(&repo).expect("init");
    let secret = "postgres://user:hunter2@localhost/app";
    seed_profile(&repo, "production", "DATABASE_URL", secret);

    let output = run_heddle(
        &repo,
        &home,
        &[
            "env",
            "run",
            "--profile",
            "production",
            "--",
            "printenv",
            "DATABASE_URL",
        ],
    );
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), secret);
    assert!(!repo.join(".env").exists());
    assert!(!repo.join(".env.local").exists());

    let listed = run_heddle(&repo, &home, &["env", "list", "--output", "json"]);
    assert!(listed.status.success());
    let json = String::from_utf8_lossy(&listed.stdout);
    assert!(json.contains("\"output_kind\":\"env_list\"") || json.contains("env_list"));
    assert!(!json.contains(secret), "plaintext leaked into env list JSON");
    assert!(!String::from_utf8_lossy(&listed.stderr).contains(secret));
}

#[test]
fn env_create_then_run_uses_cli_and_leaves_no_plaintext() {
    let temp = TempDir::new().expect("temp repo");
    let home = temp.path().join("home");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(&repo).expect("repo");
    Repository::init_default(&repo).expect("init");
    let secret = "cli-create-secret-value";

    let created = isolated_heddle(
        &repo,
        &home,
        &["env", "create", "--name", "local", "--from-env", "TOKEN"],
    )
    .env("TOKEN", secret)
    .output()
    .expect("env create");
    assert!(
        created.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&created.stderr)
    );
    let create_text = format!(
        "{}{}",
        String::from_utf8_lossy(&created.stdout),
        String::from_utf8_lossy(&created.stderr)
    );
    assert!(!create_text.contains(secret), "create leaked plaintext");

    let output = run_heddle(
        &repo,
        &home,
        &["env", "run", "--profile", "local", "--", "printenv", "TOKEN"],
    );
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), secret);
    assert!(!repo.join(".env").exists());
}

#[test]
fn env_run_refuses_wrong_profile_expired_ttl_and_json() {
    let temp = TempDir::new().expect("temp repo");
    let home = temp.path().join("home");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(&repo).expect("repo");
    Repository::init_default(&repo).expect("init");
    seed_profile(&repo, "production", "TOKEN", "abc");

    let missing = run_heddle(
        &repo,
        &home,
        &["env", "run", "--profile", "missing", "--", "true"],
    );
    assert!(!missing.status.success());

    let expired = run_heddle(
        &repo,
        &home,
        &[
            "env",
            "run",
            "--profile",
            "production",
            "--ttl",
            "0",
            "--",
            "true",
        ],
    );
    assert!(!expired.status.success());

    let json = run_heddle(
        &repo,
        &home,
        &[
            "env",
            "run",
            "--output",
            "json",
            "--profile",
            "production",
            "--",
            "true",
        ],
    );
    assert_eq!(json.status.code(), Some(65));
}

#[test]
fn capture_refuses_reserved_env_file() {
    let temp = TempDir::new().expect("temp repo");
    let home = temp.path().join("home");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(&repo).expect("repo");
    Repository::init_default(&repo).expect("init");
    std::fs::write(repo.join(".env"), b"LEAK=1").expect("write");
    let output = run_heddle(&repo, &home, &["capture", "-m", "should fail"]);
    assert!(!output.status.success());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("reserved") || combined.contains(".env"),
        "{combined}"
    );
}
