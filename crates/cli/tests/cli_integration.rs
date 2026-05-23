// SPDX-License-Identifier: Apache-2.0
//! CLI integration tests.
//!
//! These tests exercise the CLI commands end-to-end using temporary directories.
//!
//! NOTE: These tests run the built binary via CARGO_BIN_EXE_heddle so they can
//! execute from temporary directories without relying on `cargo run`.

use std::{
    io::Write,
    process::{Command, Output, Stdio},
    str,
};

use gix::refs::transaction::PreviousValue;
use repo::Repository;
use serde_json::Value;
use tempfile::TempDir;

#[path = "cli_integration/attempt.rs"]
mod attempt;
#[path = "cli_integration/basics.rs"]
mod basics;
#[path = "cli_integration/bridge.rs"]
mod bridge;
#[path = "cli_integration/cli_premium_output.rs"]
mod cli_premium_output;
#[path = "cli_integration/doctor_docs.rs"]
mod doctor_docs;
#[path = "cli_integration/fault_injection.rs"]
mod fault_injection;
#[path = "cli_integration/git_overlay_interop_matrix.rs"]
mod git_overlay_interop_matrix;
#[path = "cli_integration/git_overlay_matrix.rs"]
mod git_overlay_matrix;
#[path = "cli_integration/git_overlay_remote_ref_import.rs"]
mod git_overlay_remote_ref_import;
#[path = "cli_integration/git_overlay_sync_adoption.rs"]
mod git_overlay_sync_adoption;
#[path = "cli_integration/git_replacement_matrix.rs"]
mod git_replacement_matrix;
#[path = "cli_integration/hooks.rs"]
mod hooks;
#[path = "cli_integration/misc.rs"]
mod misc;
#[path = "cli_integration/oss_cli_polish.rs"]
mod oss_cli_polish;
#[path = "cli_integration/perf_core_loop.rs"]
mod perf_core_loop;
#[path = "cli_integration/realworld_git.rs"]
mod realworld_git;
#[path = "cli_integration/redact_purge.rs"]
mod redact_purge;
#[path = "cli_integration/refs_and_history.rs"]
mod refs_and_history;
#[path = "cli_integration/remotes.rs"]
mod remotes;
#[path = "cli_integration/shared_target.rs"]
mod shared_target;
#[path = "cli_integration/state_id_acceptance.rs"]
mod state_id_acceptance;
#[path = "cli_integration/thread_cleanup.rs"]
mod thread_cleanup;
#[path = "cli_integration/thread_default_current.rs"]
mod thread_default_current;
#[path = "cli_integration/try_cmd.rs"]
mod try_cmd;
#[path = "cli_integration/unrelated_histories_recovery.rs"]
mod unrelated_histories_recovery;
#[path = "cli_integration/watch.rs"]
mod watch;

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
        // Legacy flat bridge form (`heddle bridge import`, `bridge
        // export`, etc.) — main wrapped these under `BridgeCommands::Git`,
        // so insert the `git` token between `bridge` and the
        // sub-verb. Tests stay readable; production CLI follows main's
        // canonical wrapped form.
        ["bridge", verb, rest_args @ ..]
            if matches!(
                *verb,
                "import" | "export" | "sync" | "push" | "pull" | "init" | "ingest" | "reason"
            ) =>
        {
            let mut translated: Vec<String> = vec!["bridge".into(), "git".into(), (*verb).into()];
            translated.extend(rest_args.iter().map(|arg| (*arg).to_string()));
            translated
        }
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

    let temp;
    let dir = if let Some(dir) = cwd {
        dir.to_path_buf()
    } else {
        temp = TempDir::new().map_err(|e| e.to_string())?;
        temp.path().to_path_buf()
    };
    cmd.current_dir(&dir);
    cmd.env("HEDDLE_CONFIG", dir.join(".heddle-user/config.toml"));

    cmd.output().map_err(|e| e.to_string())
}

fn heddle_output_with_env(
    args: &[&str],
    cwd: Option<&std::path::Path>,
    envs: &[(&str, &str)],
) -> Result<Output, String> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_heddle"));
    cmd.args(translate_legacy_args(args));

    let temp;
    let dir = if let Some(dir) = cwd {
        dir.to_path_buf()
    } else {
        temp = TempDir::new().map_err(|e| e.to_string())?;
        temp.path().to_path_buf()
    };
    cmd.current_dir(&dir);
    cmd.env("HEDDLE_CONFIG", dir.join(".heddle-user/config.toml"));
    cmd.env_remove("NO_COLOR");
    for (key, value) in envs {
        cmd.env(key, value);
    }

    cmd.output().map_err(|e| e.to_string())
}

fn heddle_output_with_stdin(
    args: &[&str],
    cwd: &std::path::Path,
    stdin: &str,
) -> Result<Output, String> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_heddle"));
    cmd.args(translate_legacy_args(args));
    cmd.current_dir(cwd);
    cmd.env("HEDDLE_CONFIG", cwd.join(".heddle-user/config.toml"));
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let mut stdin_pipe = child
        .stdin
        .take()
        .ok_or_else(|| "missing stdin pipe".to_string())?;
    stdin_pipe
        .write_all(stdin.as_bytes())
        .map_err(|e| e.to_string())?;
    drop(stdin_pipe);

    child.wait_with_output().map_err(|e| e.to_string())
}

fn state_chain_ids(path: &std::path::Path, count: usize) -> Vec<String> {
    let repo = Repository::open(path).expect("repo should open");
    let mut ids = Vec::new();
    let mut current = repo.head().expect("head should resolve");

    while let Some(id) = current {
        ids.push(id.to_string_full());
        if ids.len() >= count {
            break;
        }
        let state = repo
            .store()
            .get_state(&id)
            .expect("state lookup should work")
            .expect("state should exist");
        current = state.first_parent().copied();
    }

    ids
}

fn status_json(path: &std::path::Path) -> Value {
    let output = heddle(&["status", "--json"], Some(path)).unwrap();
    serde_json::from_str(&output).expect("status output should be JSON")
}

fn git_test_signature() -> gix::actor::Signature {
    gix::actor::Signature {
        name: "Heddle Test".into(),
        email: "heddle@test".into(),
        time: gix::date::Time {
            seconds: 0,
            offset: 0,
        },
    }
}

fn git_empty_tree_oid(repo: &gix::Repository) -> gix::hash::ObjectId {
    repo.empty_tree().id
}

fn git_set_reference(repo: &gix::Repository, name: &str, target: gix::hash::ObjectId) {
    let sig = git_test_signature();
    let mut time_buf = gix::date::parse::TimeBuf::default();
    let edit = gix::refs::transaction::RefEdit {
        change: gix::refs::transaction::Change::Update {
            log: gix::refs::transaction::LogChange {
                mode: gix::refs::transaction::RefLog::AndReference,
                force_create_reflog: false,
                message: "test: update ref".into(),
            },
            expected: PreviousValue::Any,
            new: gix::refs::Target::Object(target),
        },
        name: name.try_into().expect("valid ref name"),
        deref: false,
    };
    repo.edit_references_as([edit], Some(sig.to_ref(&mut time_buf)))
        .expect("update ref");
}

fn git_commit_with_tree(
    repo: &gix::Repository,
    reference: Option<&str>,
    tree_oid: gix::hash::ObjectId,
    message: &str,
    parents: &[gix::hash::ObjectId],
) -> gix::hash::ObjectId {
    let sig = git_test_signature();
    let mut committer_buf = gix::date::parse::TimeBuf::default();
    let mut author_buf = gix::date::parse::TimeBuf::default();
    let commit = repo
        .new_commit_as(
            sig.to_ref(&mut committer_buf),
            sig.to_ref(&mut author_buf),
            message,
            tree_oid,
            parents.to_vec(),
        )
        .expect("commit");
    if let Some(reference) = reference {
        git_set_reference(repo, reference, commit.id);
    }
    commit.id
}
