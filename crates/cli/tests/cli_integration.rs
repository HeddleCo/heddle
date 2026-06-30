// SPDX-License-Identifier: Apache-2.0
//! CLI integration tests.
//!
//! These tests exercise the CLI commands end-to-end using temporary directories.
//!
//! NOTE: These tests run the built binary via CARGO_BIN_EXE_heddle so they can
//! execute from temporary directories without relying on `cargo run`.

use std::{
    io::Write,
    path::Path,
    process::{Command, Output, Stdio},
    str,
};

use objects::store::ObjectStore;
use repo::Repository;
use serde_json::Value;
use sley::{
    CommitObject, EntryKind, GitObjectType, GitTime, ObjectId, RefPrecondition, ReferenceTarget,
    Repository as SleyRepository, Signature, TagObject,
    plumbing::{
        sley_core::ByteString as GitByteString, sley_object::EncodedObject, sley_refs::ReflogEntry,
    },
};
use tempfile::TempDir;

trait SleyIntegrationRepoExt {
    fn find_commit(&self, oid: ObjectId) -> Result<TestCommit, String>;
    fn find_object(&self, oid: ObjectId) -> Result<(), String>;
    fn head_id(&self) -> Result<ObjectId, String>;
    fn head_commit(&self) -> Result<TestCommit, String>;
    fn merge_base(&self, a: ObjectId, b: ObjectId) -> Result<ObjectId, String>;
    fn rev_walk<I>(&self, starts: I) -> TestRevWalk
    where
        I: IntoIterator<Item = ObjectId>;
}

impl SleyIntegrationRepoExt for SleyRepository {
    fn find_commit(&self, oid: ObjectId) -> Result<TestCommit, String> {
        let commit = self.read_commit(&oid).map_err(|err| err.to_string())?;
        let tree = self
            .read_tree(&commit.tree)
            .map_err(|err| err.to_string())?;
        Ok(TestCommit {
            id: oid,
            commit,
            tree,
        })
    }

    fn find_object(&self, oid: ObjectId) -> Result<(), String> {
        self.read_object(&oid)
            .map(|_| ())
            .map_err(|err| err.to_string())
    }

    fn head_id(&self) -> Result<ObjectId, String> {
        self.head()
            .map_err(|err| err.to_string())?
            .oid
            .ok_or_else(|| "HEAD is unborn".to_string())
    }

    fn head_commit(&self) -> Result<TestCommit, String> {
        let oid = self.head_id()?;
        self.find_commit(oid)
    }

    fn merge_base(&self, a: ObjectId, b: ObjectId) -> Result<ObjectId, String> {
        let ancestors = commit_ancestors(self, a);
        let mut pending = std::collections::VecDeque::from([b]);
        let mut seen = std::collections::HashSet::new();
        while let Some(oid) = pending.pop_front() {
            if ancestors.contains(&oid) {
                return Ok(oid);
            }
            if !seen.insert(oid) {
                continue;
            }
            if let Ok(commit) = self.read_commit(&oid) {
                pending.extend(commit.parents);
            }
        }
        Err(format!("no merge base for {a} and {b}"))
    }

    fn rev_walk<I>(&self, starts: I) -> TestRevWalk
    where
        I: IntoIterator<Item = ObjectId>,
    {
        let mut pending: std::collections::VecDeque<ObjectId> = starts.into_iter().collect();
        let mut seen = std::collections::HashSet::new();
        let mut items = Vec::new();
        while let Some(oid) = pending.pop_front() {
            if !seen.insert(oid) {
                continue;
            }
            match self.read_commit(&oid) {
                Ok(commit) => {
                    pending.extend(commit.parents.iter().copied());
                    items.push(Ok(TestCommitInfo { id: oid }));
                }
                Err(err) => items.push(Err(err.to_string())),
            }
        }
        TestRevWalk { items }
    }
}

fn commit_ancestors(repo: &SleyRepository, start: ObjectId) -> std::collections::HashSet<ObjectId> {
    let mut pending = std::collections::VecDeque::from([start]);
    let mut seen = std::collections::HashSet::new();
    while let Some(oid) = pending.pop_front() {
        if !seen.insert(oid) {
            continue;
        }
        if let Ok(commit) = repo.read_commit(&oid) {
            pending.extend(commit.parents);
        }
    }
    seen
}

struct TestCommit {
    id: ObjectId,
    commit: CommitObject,
    tree: sley::TreeObject,
}

struct TestCommitInfo {
    id: ObjectId,
}

struct TestRevWalk {
    items: Vec<Result<TestCommitInfo, String>>,
}

impl TestRevWalk {
    fn all(self) -> Result<std::vec::IntoIter<Result<TestCommitInfo, String>>, String> {
        Ok(self.items.into_iter())
    }
}

struct TestTag {
    id: ObjectId,
}

impl TestTag {
    fn id(&self) -> ObjectId {
        self.id
    }
}

impl TestCommit {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn tree_id(&self) -> Option<ObjectId> {
        Some(self.commit.tree)
    }

    fn tree(&self) -> Result<TestTree, String> {
        Ok(TestTree {
            entries: self
                .tree
                .entries
                .iter()
                .map(|entry| entry.name.as_bytes().to_vec())
                .collect(),
        })
    }

    fn message_raw_sloppy(&self) -> TestRawMessage<'_> {
        TestRawMessage(&self.commit.message)
    }
}

struct TestTree {
    entries: Vec<Vec<u8>>,
}

impl TestTree {
    fn lookup_entry_by_path(&self, path: &str) -> Result<Option<()>, String> {
        if path.contains('/') {
            return Err(format!(
                "nested lookup not implemented for test path {path}"
            ));
        }
        Ok(self
            .entries
            .iter()
            .any(|entry| entry.as_slice() == path.as_bytes())
            .then_some(()))
    }
}

struct TestRawMessage<'a>(&'a [u8]);

impl std::fmt::Display for TestRawMessage<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", String::from_utf8_lossy(self.0))
    }
}

fn find_reference<'a>(repo: &'a SleyRepository, name: &str) -> Result<TestReference<'a>, String> {
    let full_name = if name == "HEAD" || name.starts_with("refs/") {
        name.to_string()
    } else {
        format!("refs/heads/{name}")
    };
    let target = repo
        .references()
        .read_ref(&full_name)
        .map_err(|err| err.to_string())?
        .ok_or_else(|| format!("reference {full_name} not found"))?;
    Ok(TestReference { repo, target })
}

struct TestReference<'a> {
    repo: &'a SleyRepository,
    target: ReferenceTarget,
}

impl TestReference<'_> {
    fn peel_to_id(&mut self) -> Result<ObjectId, String> {
        let oid = match &self.target {
            ReferenceTarget::Direct(oid) => *oid,
            ReferenceTarget::Symbolic(name) => {
                let reference = find_reference(self.repo, name)?;
                match reference.target {
                    ReferenceTarget::Direct(oid) => oid,
                    ReferenceTarget::Symbolic(_) => {
                        return Err(format!("nested symbolic reference {name} is unsupported"));
                    }
                }
            }
        };
        Ok(oid)
    }

    fn target(&self) -> TestReferenceTarget<'_> {
        TestReferenceTarget {
            target: &self.target,
        }
    }
}

struct TestReferenceTarget<'a> {
    target: &'a ReferenceTarget,
}

impl<'a> TestReferenceTarget<'a> {
    fn try_id(&self) -> Option<&'a ObjectId> {
        match self.target {
            ReferenceTarget::Direct(oid) => Some(oid),
            ReferenceTarget::Symbolic(_) => None,
        }
    }
}

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
#[path = "cli_integration/discuss_carry_forward.rs"]
mod discuss_carry_forward;
#[path = "cli_integration/doctor_docs.rs"]
mod doctor_docs;
#[path = "cli_integration/error_envelope_lint.rs"]
mod error_envelope_lint;
#[path = "cli_integration/exit_codes.rs"]
mod exit_codes;
#[path = "cli_integration/fault_injection.rs"]
mod fault_injection;
#[path = "cli_integration/git_overlay_fixtures.rs"]
mod git_overlay_fixtures;
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
#[path = "cli_integration/harness_error_surface.rs"]
mod harness_error_surface;
#[path = "cli_integration/hooks.rs"]
mod hooks;
#[path = "cli_integration/hydrate.rs"]
mod hydrate;
#[path = "cli_integration/misc.rs"]
mod misc;
#[path = "cli_integration/next_action_contract.rs"]
mod next_action_contract;
#[path = "cli_integration/oplog_salvage.rs"]
mod oplog_salvage;
#[path = "cli_integration/oss_cli_polish.rs"]
mod oss_cli_polish;
#[path = "cli_integration/output_kind_invariant.rs"]
mod output_kind_invariant;
#[path = "cli_integration/output_kind_runtime.rs"]
mod output_kind_runtime;
#[path = "cli_integration/output_mode_no_auto.rs"]
mod output_mode_no_auto;
#[path = "cli_integration/perf_core_loop.rs"]
mod perf_core_loop;
#[path = "cli_integration/perf_trace.rs"]
mod perf_trace;
#[path = "cli_integration/placeholder_identity.rs"]
mod placeholder_identity;
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
#[path = "cli_integration/timeline.rs"]
mod timeline;
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

/// Render the help that `heddle <args>` would print, **in-process**, without
/// spawning the binary.
///
/// `heddle <verb> --help`, `heddle help <topic>`, and `heddle capture
/// --help-agent` are pure presentation: they render text from the clap command
/// tree + curated topic strings with no repo, cwd, or env dependence, and exit
/// before any command body runs. So they don't need a real subprocess — calling
/// `cli::cli::help::render_for_args` gives the byte-identical stdout the binary
/// produces (the binary's `print_*` helpers are `write_stdout(&render_*(..))`
/// wrappers; see `help_render_matches_spawned_binary` for the equivalence
/// guard). This skips one process spawn per help assertion (HeddleCo/heddle#381).
///
/// Help output carries no ANSI styling here (matching the non-TTY piped
/// subprocess the spawn helper used), so substring assertions transfer
/// unchanged.
fn heddle_help(args: &[&str]) -> String {
    cli::cli::help::render_for_args(args).unwrap_or_else(|| {
        panic!(
            "`heddle {}` is not an in-process help request",
            args.join(" ")
        )
    })
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
    cmd.env("HOME", default_test_home_path(&dir));

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

fn canonical_path_string(path: &std::path::Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

fn heddle_output_with_env(
    args: &[&str],
    cwd: Option<&std::path::Path>,
    envs: &[(&str, &str)],
) -> Result<Output, String> {
    heddle_output_with_env_removed(args, cwd, envs, &[])
}

fn heddle_output_with_env_removed(
    args: &[&str],
    cwd: Option<&std::path::Path>,
    envs: &[(&str, &str)],
    remove_envs: &[&str],
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
    cmd.env("HOME", default_test_home_path(&dir));
    cmd.env_remove("NO_COLOR");
    for key in remove_envs {
        cmd.env_remove(key);
    }
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
    cmd.env("HOME", default_test_home_path(cwd));
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

fn open_git(path: impl AsRef<Path>) -> Result<SleyRepository, String> {
    SleyRepository::open(path.as_ref())
        .or_else(|_| SleyRepository::discover(path.as_ref()))
        .map_err(|err| err.to_string())
}

fn git_test_signature() -> Signature {
    Signature {
        name: GitByteString::new(b"Heddle Test".to_vec()),
        email: GitByteString::new(b"heddle@test".to_vec()),
        time: GitTime::new(0, 0),
        raw: b"Heddle Test <heddle@test> 0 +0000".to_vec(),
    }
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
    std::env::temp_dir().join(format!(
        "heddle-cli-test-user-{}-{:016x}.toml",
        std::process::id(),
        test_path_hash(cwd)
    ))
}

fn default_test_home_path(cwd: &std::path::Path) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "heddle-cli-test-home-{}-{:016x}",
        std::process::id(),
        test_path_hash(cwd)
    ))
}

fn test_path_hash(path: &std::path::Path) -> u64 {
    use std::{
        collections::hash_map::DefaultHasher,
        hash::{Hash, Hasher},
    };

    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    hasher.finish()
}

fn git_empty_tree_oid(repo: &SleyRepository) -> ObjectId {
    repo.write_tree(sley::TreeEditor::new())
        .expect("write empty tree")
}

fn git_set_reference(repo: &SleyRepository, name: &str, target: ObjectId) {
    let sig = git_test_signature();
    let refs = repo.references();
    let old_oid = match refs.read_ref(name).expect("read ref") {
        Some(ReferenceTarget::Direct(oid)) => oid,
        _ => ObjectId::null(repo.object_format()),
    };
    let mut tx = refs.transaction();
    tx.update_to(
        name.to_string(),
        ReferenceTarget::Direct(target),
        RefPrecondition::Any,
        Some(ReflogEntry {
            old_oid,
            new_oid: target,
            committer: sig.to_ident_bytes(),
            message: b"test: update ref".to_vec(),
        }),
    );
    tx.commit().expect("update ref");
}

fn git_commit_with_tree(
    repo: &SleyRepository,
    reference: Option<&str>,
    tree_oid: ObjectId,
    message: &str,
    parents: &[ObjectId],
) -> ObjectId {
    let sig = git_test_signature();
    let commit = CommitObject {
        tree: tree_oid,
        parents: parents.to_vec(),
        author: sig.to_ident_bytes(),
        committer: sig.to_ident_bytes(),
        encoding: None,
        message: message.as_bytes().to_vec(),
    };
    let commit_id = repo
        .write_object(EncodedObject::new(GitObjectType::Commit, commit.write()))
        .expect("commit");
    if let Some(reference) = reference {
        git_set_reference(repo, reference, commit_id);
    }
    commit_id
}

fn git_create_annotated_tag(
    repo: &SleyRepository,
    name: &str,
    target: ObjectId,
    object_type: GitObjectType,
    message: &str,
    precondition: RefPrecondition,
) -> TestTag {
    let tag = TagObject {
        object: target,
        object_type,
        name: name.as_bytes().to_vec(),
        tagger: Some(git_test_signature().to_ident_bytes()),
        message: message.as_bytes().to_vec(),
        raw_body: None,
    };
    let tag_id = repo
        .write_object(EncodedObject::new(GitObjectType::Tag, tag.write()))
        .expect("write annotated tag");
    let refs = repo.references();
    let mut tx = refs.transaction();
    tx.update_to(
        format!("refs/tags/{name}"),
        ReferenceTarget::Direct(tag_id),
        precondition,
        None,
    );
    tx.commit().expect("update tag ref");
    TestTag { id: tag_id }
}
