// SPDX-License-Identifier: Apache-2.0
//! CLI integration tests.
//!
//! These tests exercise the CLI commands end-to-end using temporary directories.
//!
//! NOTE: These tests run the built binary via CARGO_BIN_EXE_heddle so they can
//! execute from temporary directories without relying on `cargo run`.

use objects::store::ObjectStore;
use std::{
    io::Write,
    process::{Command, Output, Stdio},
    str,
};

use git_substrate::{
    GitRepo, ObjectId, ObjectType, RefConstraint, Tag, TreeEntryInput, TreeEntryMode,
    actor_suffix_bytes, empty_tree_sha1, set_reference, write_simple_commit,
};
use repo::Repository;
use serde_json::Value;
use tempfile::TempDir;

#[path = "cli_integration/attempt.rs"]
mod attempt;
#[path = "cli_integration/basics.rs"]
mod basics;
#[path = "cli_integration/bridge.rs"]
mod bridge;
#[path = "cli_integration/cli_help_consistency.rs"]
mod cli_help_consistency;
#[path = "cli_integration/cli_premium_output.rs"]
mod cli_premium_output;
#[path = "cli_integration/compact_output.rs"]
mod compact_output;
#[path = "cli_integration/context_recovery_advice.rs"]
mod context_recovery_advice;
#[path = "cli_integration/current_context_advice.rs"]
mod current_context_advice;
#[path = "cli_integration/diff_patch_conformance.rs"]
mod diff_patch_conformance;
#[path = "cli_integration/doctor_docs.rs"]
mod doctor_docs;
#[path = "cli_integration/error_envelope_lint.rs"]
mod error_envelope_lint;
#[path = "cli_integration/exit_codes.rs"]
mod exit_codes;
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
#[path = "cli_integration/hydrate.rs"]
mod hydrate;
#[path = "cli_integration/misc.rs"]
mod misc;
#[path = "cli_integration/oss_cli_polish.rs"]
mod oss_cli_polish;
#[path = "cli_integration/output_kind_invariant.rs"]
mod output_kind_invariant;
#[path = "cli_integration/output_kind_runtime.rs"]
mod output_kind_runtime;
#[path = "cli_integration/output_mode_no_auto.rs"]
mod output_mode_no_auto;
#[path = "cli_integration/next_action_contract.rs"]
mod next_action_contract;
#[path = "cli_integration/perf_core_loop.rs"]
mod perf_core_loop;
#[path = "cli_integration/quickstart.rs"]
mod quickstart;
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
#[path = "cli_integration/stdout_stderr_split.rs"]
mod stdout_stderr_split;
#[path = "cli_integration/thread_cleanup.rs"]
mod thread_cleanup;
#[path = "cli_integration/thread_default_current.rs"]
mod thread_default_current;
#[path = "cli_integration/try_cmd.rs"]
mod try_cmd;
#[path = "cli_integration/unrelated_histories_recovery.rs"]
mod unrelated_histories_recovery;
#[path = "cli_integration/visibility.rs"]
mod visibility;
#[path = "cli_integration/watch.rs"]
mod watch;
#[path = "cli_integration/worktree_target_advice.rs"]
mod worktree_target_advice;

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

pub(crate) fn assert_json_recovery_advice_fields(envelope: &Value, context: &str) {
    for field in [
        "unsafe_condition",
        "would_change",
        "preserved",
        "primary_command",
        "recovery_commands",
        "hint",
    ] {
        assert!(
            envelope[field]
                .as_str()
                .is_some_and(|value| !value.trim().is_empty())
                || envelope[field]
                    .as_array()
                    .is_some_and(|value| !value.is_empty()),
            "JSON recovery advice should expose `{field}` through structured fields: {context}"
        );
    }
    assert!(
        envelope["error"].as_str().is_some_and(|error| {
            !error.contains("Unsafe:")
                && !error.contains("Would change:")
                && !error.contains("Preserved:")
                && !error.contains("Primary recovery:")
                && !error.contains("Other recovery:")
        }),
        "JSON `error` should stay concise; recovery detail belongs in structured fields: {context}"
    );
    assert!(
        envelope
            .get("primary_command_template")
            .is_some_and(|template| template.is_null() || template.is_object()),
        "JSON recovery advice should expose `primary_command_template` as object or null: {context}"
    );
    assert!(
        envelope["recovery_action_templates"]
            .as_array()
            .is_some_and(|templates| templates.iter().all(|template| template.is_object())),
        "JSON recovery advice should expose `recovery_action_templates` as an array of template objects: {context}"
    );
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
    let config_path = default_test_user_config_path(&dir);
    seed_default_test_user_config(&config_path, &dir)?;
    cmd.env("HEDDLE_CONFIG", config_path);

    cmd.output().map_err(|e| e.to_string())
}

fn heddle_argv_json<I, S>(args: I) -> Value
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let argv = std::iter::once(env!("CARGO_BIN_EXE_heddle").to_string())
        .chain(args.into_iter().map(|arg| arg.as_ref().to_string()))
        .collect::<Vec<_>>();
    serde_json::json!(argv)
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
    let config_path = default_test_user_config_path(&dir);
    seed_default_test_user_config(&config_path, &dir)?;
    cmd.env("HEDDLE_CONFIG", config_path);
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
    let config_path = default_test_user_config_path(cwd);
    seed_default_test_user_config(&config_path, cwd)?;
    cmd.env("HEDDLE_CONFIG", config_path);
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
    let output = heddle(&["status", "--output", "json"], Some(path)).unwrap();
    serde_json::from_str(&output).expect("status output should be JSON")
}

/// Run `git <args>` in `dir` under a fully isolated environment, asserting
/// success. Hermetic *by construction*: the child's environment is wiped with
/// [`Command::env_clear`] and rebuilt from a minimal explicit allowlist, so no
/// inherited variable — `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`,
/// `GIT_OBJECT_DIRECTORY`, or any other `GIT_*` / ambient var — can leak in and
/// flake the tests. A blocklist would only ever chase the next leaking var;
/// clearing the slate and opting variables back in closes the whole class.
/// Identity is pinned via `-c` so commits don't depend on a global
/// `user.name`/`user.email`. Shared by the exit-code and error-envelope
/// fixtures (HeddleCo/heddle#252) so the isolation lives in one place.
pub(crate) fn git_hermetic(args: &[&str], dir: &std::path::Path) {
    let mut command = Command::new("git");
    command.env_clear();
    // Minimal allowlist — everything the child legitimately needs, nothing else.
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    command
        .env("HOME", dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .env("TERM", "dumb")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir);
    let status = command
        .status()
        .unwrap_or_else(|err| panic!("spawn git {args:?}: {err}"));
    assert!(status.success(), "git {args:?} failed in {}", dir.display());
}

fn git_test_actor() -> Vec<u8> {
    actor_suffix_bytes(b"Heddle Test", b"heddle@test", 0, 0)
}

fn seed_default_test_user_config(
    config_path: &std::path::Path,
    cwd: &std::path::Path,
) -> Result<(), String> {
    if config_path.exists() {
        return Ok(());
    }
    if cwd.join(".git").exists() {
        return Ok(());
    }
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    std::fs::write(
        config_path,
        "[principal]\nname = \"Heddle Test\"\nemail = \"heddle@example.com\"\n",
    )
    .map_err(|err| err.to_string())
}

fn default_test_user_config_path(cwd: &std::path::Path) -> std::path::PathBuf {
    use std::{
        collections::hash_map::DefaultHasher,
        hash::{Hash, Hasher},
    };

    let mut hasher = DefaultHasher::new();
    cwd.hash(&mut hasher);
    std::env::temp_dir().join(format!(
        "heddle-cli-test-user-{}-{:016x}.toml",
        std::process::id(),
        hasher.finish()
    ))
}

fn git_empty_tree_oid(_repo: &GitRepo) -> ObjectId {
    empty_tree_sha1()
}

fn git_set_reference(repo: &GitRepo, name: &str, target: ObjectId) {
    set_reference(
        repo.git_dir(),
        repo.object_format(),
        name,
        &target,
        RefConstraint::Any,
        "test: update ref",
    )
    .expect("update ref");
}

fn git_tree_with_file(repo: &GitRepo, path: &str, content: &[u8]) -> ObjectId {
    let blob = repo.write_blob(content).expect("write git blob");
    let mut entries = vec![TreeEntryInput {
        mode: TreeEntryMode::Blob,
        name: path.to_string(),
        oid: blob,
    }];
    repo.write_tree(&mut entries).expect("write git tree")
}

fn git_commit_with_tree(
    repo: &GitRepo,
    reference: Option<&str>,
    tree_oid: ObjectId,
    message: &str,
    parents: &[ObjectId],
) -> ObjectId {
    let actor = git_test_actor();
    let commit_oid = write_simple_commit(
        repo.git_dir(),
        repo.object_format(),
        &tree_oid,
        parents,
        &actor,
        &actor,
        message.as_bytes(),
    )
    .expect("commit");
    if let Some(reference) = reference {
        git_set_reference(repo, reference, commit_oid.clone());
    }
    commit_oid
}

fn git_head_oid(path: &std::path::Path) -> String {
    GitRepo::open(path)
        .or_else(|_| GitRepo::discover(path))
        .expect("open git repo")
        .read_ref_oid("HEAD")
        .expect("head")
        .expect("peel")
        .to_hex()
}

fn git_rev_list_count(path: &std::path::Path, rev: &str) -> usize {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-list", "--count", rev])
        .output()
        .expect("spawn git rev-list");
    assert!(
        output.status.success(),
        "git rev-list --count {rev} failed in {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .expect("rev-list count should parse")
}

fn git_create_annotated_tag(
    repo: &GitRepo,
    name: &str,
    target: &ObjectId,
    message: &str,
) -> ObjectId {
    let tag = Tag {
        object: target.clone(),
        object_type: ObjectType::Commit,
        name: name.as_bytes().to_vec(),
        tagger: Some(git_test_actor()),
        message: message.as_bytes().to_vec(),
    };
    let tag_id = repo.write_tag(&tag).expect("write tag");
    set_reference(
        repo.git_dir(),
        repo.object_format(),
        &format!("refs/tags/{name}"),
        &tag_id,
        RefConstraint::Any,
        "test: create tag",
    )
    .expect("set tag ref");
    tag_id
}
