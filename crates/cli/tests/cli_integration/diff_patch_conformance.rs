// SPDX-License-Identifier: Apache-2.0
//! Golden-corpus conformance harness for `heddle diff --patch`.
//!
//! This is the drip-ender for the unified-diff format work (heddle#270).
//! Rather than enumerate format edge cases one Codex round at a time,
//! every cell below runs `heddle diff --patch` through *real* `git apply`
//! and asserts the reconstructed worktree, so a missing or malformed
//! header is a red test rather than a future review finding.
//!
//! For each cell we:
//!   1. set up a repo state that produces the change;
//!   2. run `heddle diff --patch` and `heddle diff --output json`,
//!      asserting the JSON `.patch` field equals the `--patch` stdout;
//!   3. pipe the patch through `git apply --check` then `git apply`
//!      against a checkout seeded at the pre-change state;
//!   4. assert apply succeeds AND the resulting files match the
//!      post-change state — content, mode (exec / symlink), existence.
//!
//! A proptest layer feeds random file-trees + edits through the same
//! oracle to catch cells a hand-enumerated matrix misses.
//!
//! **Target: `git apply`-compatible round-trip, not byte-identical to
//! `git diff`.** heddle's blob hashes are not git SHAs, so the
//! `index <sha>..<sha>` header line is omitted; `git apply` does not
//! require it unless `--index`/`-3` is used, and these cells never pass
//! those flags. The behavioural oracle (apply succeeds + tree matches)
//! is the contract, which is exactly what consumers of the patch rely on.

use std::{
    collections::BTreeMap,
    io::Write,
    path::Path,
    process::{Command, Output, Stdio},
};

use proptest::prelude::*;
use serde_json::Value;
use tempfile::TempDir;

use super::{heddle, heddle_output};

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Normal,
    Exec,
    Symlink,
}

/// One file in a repo state. For `Symlink`, `body` is the link target.
#[derive(Clone, Debug)]
struct Entry {
    path: String,
    body: Vec<u8>,
    kind: Kind,
}

fn normal(path: &str, body: &str) -> Entry {
    Entry {
        path: path.to_string(),
        body: body.as_bytes().to_vec(),
        kind: Kind::Normal,
    }
}

#[cfg(unix)]
fn exec(path: &str, body: &str) -> Entry {
    Entry {
        path: path.to_string(),
        body: body.as_bytes().to_vec(),
        kind: Kind::Exec,
    }
}

#[cfg(unix)]
fn symlink(path: &str, target: &str) -> Entry {
    Entry {
        path: path.to_string(),
        body: target.as_bytes().to_vec(),
        kind: Kind::Symlink,
    }
}

/// A symlink whose target is an arbitrary byte sequence (need not be valid
/// UTF-8). Git stores symlink targets as raw bytes; this exercises the path
/// where a lossy `to_string_lossy` conversion would corrupt the target.
#[cfg(unix)]
fn symlink_bytes(path: &str, target: &[u8]) -> Entry {
    Entry {
        path: path.to_string(),
        body: target.to_vec(),
        kind: Kind::Symlink,
    }
}

/// A regular file with arbitrary (binary) bytes. heddle treats a body with
/// embedded NULs as binary, so these exercise the `Binary files … differ`
/// path rather than a text hunk.
fn binary(path: &str, body: &[u8]) -> Entry {
    Entry {
        path: path.to_string(),
        body: body.to_vec(),
        kind: Kind::Normal,
    }
}

#[cfg(unix)]
fn binary_exec(path: &str, body: &[u8]) -> Entry {
    Entry {
        path: path.to_string(),
        body: body.to_vec(),
        kind: Kind::Exec,
    }
}

/// What a path must look like in the applied worktree.
enum Expect {
    Present(Entry),
    Absent(&'static str),
}

// ---------------------------------------------------------------------------
// Filesystem helpers
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_mode(mode);
    std::fs::set_permissions(path, perms).unwrap();
}

#[cfg(not(unix))]
fn set_mode(_: &Path, _: u32) {}

fn write_entry(dir: &Path, entry: &Entry) {
    let full = dir.join(&entry.path);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    match entry.kind {
        Kind::Symlink => {
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStrExt;
                // Git stores symlink targets as raw bytes, which need not be
                // valid UTF-8. Build the target from raw OS bytes so the
                // non-UTF-8 target cell round-trips byte-exactly.
                let target = std::ffi::OsStr::from_bytes(&entry.body);
                // Replace any stale entry so re-materializing a state is
                // idempotent.
                let _ = std::fs::remove_file(&full);
                std::os::unix::fs::symlink(target, &full).unwrap();
            }
            #[cfg(not(unix))]
            {
                let _ = dir;
            }
        }
        Kind::Normal => {
            std::fs::write(&full, &entry.body).unwrap();
            set_mode(&full, 0o644);
        }
        Kind::Exec => {
            std::fs::write(&full, &entry.body).unwrap();
            set_mode(&full, 0o755);
        }
    }
}

fn assert_present(dir: &Path, entry: &Entry) {
    let full = dir.join(&entry.path);
    let meta = std::fs::symlink_metadata(&full)
        .unwrap_or_else(|err| panic!("expected `{}` to be present: {err}", entry.path));
    match entry.kind {
        Kind::Symlink => {
            assert!(
                meta.file_type().is_symlink(),
                "`{}` should be a symlink after apply",
                entry.path
            );
            let target = std::fs::read_link(&full).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStrExt;
                assert_eq!(
                    target.as_os_str().as_bytes(),
                    entry.body.as_slice(),
                    "`{}` symlink target mismatch",
                    entry.path
                );
            }
            #[cfg(not(unix))]
            {
                assert_eq!(
                    target.to_string_lossy().as_bytes(),
                    entry.body.as_slice(),
                    "`{}` symlink target mismatch",
                    entry.path
                );
            }
        }
        Kind::Exec => {
            assert_eq!(
                std::fs::read(&full).unwrap(),
                entry.body,
                "`{}` content mismatch",
                entry.path
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert!(
                    meta.permissions().mode() & 0o111 != 0,
                    "`{}` should keep its executable bit after apply",
                    entry.path
                );
            }
        }
        Kind::Normal => {
            assert_eq!(
                std::fs::read(&full).unwrap(),
                entry.body,
                "`{}` content mismatch",
                entry.path
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert!(
                    meta.permissions().mode() & 0o111 == 0,
                    "`{}` should not be executable after apply",
                    entry.path
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// git helpers (oracle-side; the heddle side uses the shared `heddle()` wrapper)
// ---------------------------------------------------------------------------

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap_or_else(|err| panic!("git {args:?} should run: {err}"));
    assert!(status.success(), "git {args:?} should succeed");
}

fn git_init(dir: &Path) {
    git(dir, &["init", "-q"]);
    git(dir, &["config", "user.name", "Heddle Test"]);
    git(dir, &["config", "user.email", "heddle@example.com"]);
    git(dir, &["checkout", "-q", "-b", "main"]);
}

fn pipe_git(dir: &Path, args: &[&str], patch: &str) -> Output {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("git should spawn");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(patch.as_bytes())
        .unwrap();
    child.wait_with_output().expect("git should finish")
}

/// Seed a git repo with `pre`, then `git apply --check` + `git apply`
/// the heddle-produced `patch`, and assert every `expect` holds on the
/// reconstructed worktree.
fn apply_oracle(pre: &[Entry], patch: &str, expect: &[Expect]) {
    let g = TempDir::new().unwrap();
    git_init(g.path());
    for entry in pre {
        write_entry(g.path(), entry);
    }
    git(g.path(), &["add", "-A"]);
    git(g.path(), &["commit", "-q", "-m", "seed"]);

    let check = pipe_git(g.path(), &["apply", "--check"], patch);
    assert!(
        check.status.success(),
        "git apply --check rejected the patch;\nstderr={}\npatch=\n{patch}",
        String::from_utf8_lossy(&check.stderr)
    );
    let applied = pipe_git(g.path(), &["apply"], patch);
    assert!(
        applied.status.success(),
        "git apply failed;\nstderr={}\npatch=\n{patch}",
        String::from_utf8_lossy(&applied.stderr)
    );

    for exp in expect {
        match exp {
            Expect::Present(entry) => assert_present(g.path(), entry),
            Expect::Absent(path) => assert!(
                !g.path().join(path).exists(),
                "`{path}` should be gone after apply;\npatch=\n{patch}"
            ),
        }
    }
}

/// Seed a git repo with `pre`, then assert `git apply --check` *refuses* the
/// heddle-produced `patch`. This is the F5 fail-loud contract (cid
/// 3319484747): a binary *content* change is emitted as a `Binary files …
/// differ` marker carrying a placeholder `index 0000000..0000000` line. git
/// apply cannot apply a binary patch without a full index, so it refuses the
/// *whole* patch atomically rather than silently skipping the binary block —
/// which would leave stale binary content on disk while reporting success.
fn apply_refusal_oracle(pre: &[Entry], patch: &str) {
    let g = TempDir::new().unwrap();
    git_init(g.path());
    for entry in pre {
        write_entry(g.path(), entry);
    }
    git(g.path(), &["add", "-A"]);
    git(g.path(), &["commit", "-q", "-m", "seed"]);

    let check = pipe_git(g.path(), &["apply", "--check"], patch);
    assert!(
        !check.status.success(),
        "git apply --check accepted a patch carrying a binary content change; \
         it must refuse rather than leave stale binary content (false round-trip);\npatch=\n{patch}"
    );
}

// ---------------------------------------------------------------------------
// Native-path cell runner (heddle init + capture + worktree mutate)
// ---------------------------------------------------------------------------

fn json_patch_field(cwd: &Path) -> Option<String> {
    json_diff_patch_field(cwd, &[])
}

/// Like `json_patch_field` but for a state-to-state diff: `extra` carries the
/// `<from> <to>` revisions so `heddle --output json diff HEAD~1 HEAD` can be
/// asserted to mirror its `--patch` render too.
fn json_diff_patch_field(cwd: &Path, extra: &[&str]) -> Option<String> {
    let mut args = vec!["--output", "json", "diff"];
    args.extend_from_slice(extra);
    let out = heddle_output(&args, Some(cwd)).expect("heddle json diff");
    assert!(
        out.status.success(),
        "heddle --output json diff should succeed; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: Value = serde_json::from_slice(&out.stdout).expect("diff output should be JSON");
    value
        .get("patch")
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

/// The full JSON diff payload for `heddle --output json diff <base> <mode>`.
fn json_diff_value(cwd: &Path, base: &[&str], mode: &[&str]) -> Value {
    let mut args = vec!["--output", "json", "diff"];
    args.extend_from_slice(base);
    args.extend_from_slice(mode);
    let out = heddle_output(&args, Some(cwd)).expect("heddle json diff");
    assert!(
        out.status.success(),
        "heddle --output json diff {base:?} {mode:?} should succeed; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("diff output should be JSON")
}

/// Extract the rename/type-detection signature from a diff's JSON `changes`
/// array: the sorted `(kind, path, old_path)` triples. This is the verdict
/// rename/type detection reaches, independent of how a given mode renders it.
/// A `--stat` render that collapses a cross-type move into a rename (or drops
/// a type-change split) produces a *different* signature here — which is
/// exactly the mode-divergence class we are closing.
fn change_signature(value: &Value) -> Vec<(String, String, String)> {
    // Worktree-mode diffs group `changes` into `{modified, added, deleted}`
    // category arrays; state-to-state diffs keep a flat array. Flatten both
    // to the same triple list so the signature is shape-independent.
    let entries: Vec<&Value> = match value.get("changes") {
        Some(Value::Array(arr)) => arr.iter().collect(),
        Some(Value::Object(map)) => ["modified", "added", "deleted"]
            .iter()
            .filter_map(|key| map.get(*key))
            .filter_map(Value::as_array)
            .flatten()
            .collect(),
        _ => Vec::new(),
    };
    let mut sig: Vec<(String, String, String)> = entries
        .iter()
        .map(|change| {
            let field = |key| {
                change
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            };
            (field("kind"), field("path"), field("old_path"))
        })
        .collect();
    sig.sort();
    sig
}

/// The structural close-the-class assertion (cids 3321875377 + 3321875382):
/// for ONE repo state, every output mode must agree on rename/type detection,
/// and every JSON render — with OR without a summary flag — must carry the
/// round-trippable `.patch` field equal to the `--patch` stdout. Folded into
/// every cell runner so a future mode-divergence (a `--stat`/`--name-only`
/// render that drops `.patch` or collapses a type change differently from
/// `--patch`) is a red test rather than the next Codex round. `base` is the
/// diff's leading revision args (`[]` for the worktree surface, `["HEAD~1",
/// "HEAD"]` for the committed surface).
fn assert_modes_consistent(cwd: &Path, base: &[&str], patch_stdout: &str) {
    let modes: [&[&str]; 4] = [&[], &["--stat"], &["--name-only"], &["--patch"]];
    let reference = change_signature(&json_diff_value(cwd, base, &[]));
    for mode in modes {
        let value = json_diff_value(cwd, base, mode);
        assert_eq!(
            value.get("patch").and_then(Value::as_str),
            Some(patch_stdout),
            "`--output json diff {base:?} {mode:?}` must carry the same `.patch` as `--patch` stdout"
        );
        assert_eq!(
            change_signature(&value),
            reference,
            "`--output json diff {base:?} {mode:?}` rename/type detection diverged from the default render"
        );
    }
}

/// Run one native-path cell: capture `pre`, mutate the worktree, then
/// assert the `--patch` text round-trips through `git apply` to `expect`
/// and that the JSON `.patch` field matches the `--patch` stdout.
fn native_cell(pre: &[Entry], mutate: impl Fn(&Path), expect: &[Expect]) {
    let h = TempDir::new().unwrap();
    heddle(&["init"], Some(h.path())).unwrap();
    for entry in pre {
        write_entry(h.path(), entry);
    }
    heddle(&["capture", "-m", "v1"], Some(h.path())).unwrap();
    mutate(h.path());

    let patch = heddle(&["diff", "--patch"], Some(h.path())).unwrap();
    assert!(
        !patch.trim().is_empty(),
        "native cell produced an empty patch (no change detected?)"
    );

    // Every output mode must agree on rename/type detection, and every JSON
    // render — including `--stat`/`--name-only` — must carry the `.patch`
    // field equal to the `--patch` stdout (cids 3321875377 + 3321875382).
    assert_modes_consistent(h.path(), &[], &patch);

    apply_oracle(pre, &patch, expect);
}

/// Run one native state-to-state cell: capture `pre` as `v1`, mutate, capture
/// `v2`, then assert the committed-diff render (`heddle diff HEAD~1 HEAD
/// --patch`) round-trips through `git apply` to `expect`. This is the
/// committed-tree surface — the one that took the `to_tree`-present branch
/// and dropped type changes in r8 (cid 3319484717) — so every type-change
/// cell that runs here is the regression guard for that path.
fn state_cell(pre: &[Entry], mutate: impl Fn(&Path), expect: &[Expect]) {
    let h = TempDir::new().unwrap();
    heddle(&["init"], Some(h.path())).unwrap();
    for entry in pre {
        write_entry(h.path(), entry);
    }
    heddle(&["capture", "-m", "v1"], Some(h.path())).unwrap();
    mutate(h.path());
    heddle(&["capture", "-m", "v2"], Some(h.path())).unwrap();

    let patch = heddle(&["diff", "HEAD~1", "HEAD", "--patch"], Some(h.path())).unwrap();
    assert!(
        !patch.trim().is_empty(),
        "state cell produced an empty patch (no change detected?)"
    );

    // Committed-surface analogue of the worktree check: every output mode of
    // `heddle diff HEAD~1 HEAD` must agree on rename/type detection and carry
    // `.patch` in every JSON render (cids 3321875377 + 3321875382).
    assert_modes_consistent(h.path(), &["HEAD~1", "HEAD"], &patch);

    apply_oracle(pre, &patch, expect);
}

// ---------------------------------------------------------------------------
// Matrix — add
// ---------------------------------------------------------------------------

#[test]
fn add_nonempty_round_trips() {
    native_cell(
        &[normal("anchor.txt", "anchor\n")],
        |dir| write_entry(dir, &normal("new.txt", "alpha\nbeta\n")),
        &[Expect::Present(normal("new.txt", "alpha\nbeta\n"))],
    );
}

#[test]
fn add_empty_round_trips() {
    native_cell(
        &[normal("anchor.txt", "anchor\n")],
        |dir| write_entry(dir, &normal("empty.txt", "")),
        &[Expect::Present(normal("empty.txt", ""))],
    );
}

#[test]
fn add_no_trailing_newline_round_trips() {
    native_cell(
        &[normal("anchor.txt", "anchor\n")],
        |dir| write_entry(dir, &normal("noeol.txt", "single line no eol")),
        &[Expect::Present(normal("noeol.txt", "single line no eol"))],
    );
}

#[cfg(unix)]
#[test]
fn add_executable_round_trips() {
    native_cell(
        &[normal("anchor.txt", "anchor\n")],
        |dir| write_entry(dir, &exec("run.sh", "#!/bin/sh\necho hi\n")),
        &[Expect::Present(exec("run.sh", "#!/bin/sh\necho hi\n"))],
    );
}

#[cfg(unix)]
#[test]
fn add_symlink_round_trips() {
    native_cell(
        &[normal("anchor.txt", "anchor\n")],
        |dir| write_entry(dir, &symlink("linky", "some/target/path")),
        &[Expect::Present(symlink("linky", "some/target/path"))],
    );
}

// ---------------------------------------------------------------------------
// Matrix — delete
// ---------------------------------------------------------------------------

#[test]
fn delete_nonempty_round_trips() {
    native_cell(
        &[normal("doomed.txt", "gamma\ndelta\n"), normal("keep.txt", "keep\n")],
        |dir| std::fs::remove_file(dir.join("doomed.txt")).unwrap(),
        &[
            Expect::Absent("doomed.txt"),
            Expect::Present(normal("keep.txt", "keep\n")),
        ],
    );
}

#[test]
fn delete_empty_round_trips() {
    native_cell(
        &[normal("willdie.txt", ""), normal("keep.txt", "keep\n")],
        |dir| std::fs::remove_file(dir.join("willdie.txt")).unwrap(),
        &[
            Expect::Absent("willdie.txt"),
            Expect::Present(normal("keep.txt", "keep\n")),
        ],
    );
}

#[test]
fn delete_nested_round_trips() {
    native_cell(
        &[
            normal("src/nested/file.txt", "alpha\nbeta\n"),
            normal("keep.txt", "keep\n"),
        ],
        |dir| std::fs::remove_file(dir.join("src/nested/file.txt")).unwrap(),
        &[
            Expect::Absent("src/nested/file.txt"),
            Expect::Present(normal("keep.txt", "keep\n")),
        ],
    );
}

// ---------------------------------------------------------------------------
// Matrix — modify (content shapes)
// ---------------------------------------------------------------------------

#[test]
fn modify_nonempty_round_trips() {
    native_cell(
        &[normal("f.txt", "l1\nl2\nl3\nl4\nl5\n")],
        |dir| write_entry(dir, &normal("f.txt", "l1\nL2\nl3\nl4\nl5\n")),
        &[Expect::Present(normal("f.txt", "l1\nL2\nl3\nl4\nl5\n"))],
    );
}

#[test]
fn modify_old_side_lacks_newline_round_trips() {
    native_cell(
        &[normal("f.txt", "hello")],
        |dir| write_entry(dir, &normal("f.txt", "hello\nmore\n")),
        &[Expect::Present(normal("f.txt", "hello\nmore\n"))],
    );
}

#[test]
fn modify_new_side_lacks_newline_round_trips() {
    native_cell(
        &[normal("f.txt", "hello\nmore\n")],
        |dir| write_entry(dir, &normal("f.txt", "hello")),
        &[Expect::Present(normal("f.txt", "hello"))],
    );
}

#[test]
fn modify_newline_only_addition_round_trips() {
    native_cell(
        &[normal("f.txt", "hello")],
        |dir| write_entry(dir, &normal("f.txt", "hello\n")),
        &[Expect::Present(normal("f.txt", "hello\n"))],
    );
}

#[test]
fn modify_newline_only_removal_round_trips() {
    native_cell(
        &[normal("f.txt", "hello\n")],
        |dir| write_entry(dir, &normal("f.txt", "hello")),
        &[Expect::Present(normal("f.txt", "hello"))],
    );
}

// ---------------------------------------------------------------------------
// Matrix — mode-only modify (chmod). Covers cid 3318629228.
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn chmod_add_exec_bit_round_trips() {
    let body = "#!/bin/sh\necho hi\n";
    native_cell(
        &[normal("run.sh", body)],
        |dir| set_mode(&dir.join("run.sh"), 0o755),
        &[Expect::Present(exec("run.sh", body))],
    );
}

#[cfg(unix)]
#[test]
fn chmod_remove_exec_bit_round_trips() {
    let body = "#!/bin/sh\necho hi\n";
    native_cell(
        &[exec("run.sh", body)],
        |dir| set_mode(&dir.join("run.sh"), 0o644),
        &[Expect::Present(normal("run.sh", body))],
    );
}

/// A mode-only modify is a header-only patch (`diff --git` + `old mode`/
/// `new mode`, no `@@` body). Pin the exact shape so a regression that
/// drops the headers (the cid 3318629228 bug) trips here.
#[cfg(unix)]
#[test]
fn chmod_only_emits_header_only_patch() {
    let h = TempDir::new().unwrap();
    heddle(&["init"], Some(h.path())).unwrap();
    write_entry(h.path(), &normal("run.sh", "#!/bin/sh\necho hi\n"));
    heddle(&["capture", "-m", "v1"], Some(h.path())).unwrap();
    set_mode(&h.path().join("run.sh"), 0o755);

    let patch = heddle(&["diff", "--patch"], Some(h.path())).unwrap();
    assert!(
        patch.contains("diff --git a/run.sh b/run.sh"),
        "chmod patch must carry the `diff --git` header:\n{patch}"
    );
    assert!(
        patch.contains("old mode 100644") && patch.contains("new mode 100755"),
        "chmod patch must carry `old mode`/`new mode` headers:\n{patch}"
    );
    assert!(
        !patch.contains("@@"),
        "mode-only modify is header-only — no hunk body:\n{patch}"
    );
}

// ---------------------------------------------------------------------------
// Matrix — special-character paths (git C-style header quoting). Covers
// cid 3319049648 — paths with tab/newline/quote/backslash/space/non-ASCII
// must be quoted in every header site so `git apply` parses them.
// ---------------------------------------------------------------------------

/// Names heddle can track in a committed tree (`validate_name` rejects
/// control characters and the `\` path-separator byte, but allows quotes,
/// spaces, and non-ASCII). These exercise the full capture → modify /
/// delete / rename → `git apply` round-trip, so the quoting must hold at
/// every header site for a *tracked* file.
fn capturable_quoting_paths() -> Vec<&'static str> {
    vec![
        "quo\"te.txt",
        " leading.txt",
        "trailing .txt",
        "café_ünïcode.txt",
        "dir with space/child.txt",
    ]
}

/// Names heddle refuses in a committed tree (tab/newline control bytes,
/// backslash) but that still appear in patches as *worktree adds* (and in
/// the plain-Git surface, where git — not heddle — owns the tree). The add
/// path never validates a tree name, so these must still quote correctly.
fn worktree_only_quoting_paths() -> Vec<&'static str> {
    vec![
        "tab\tname.txt",
        "new\nline.txt",
        "back\\slash.txt",
    ]
}

#[test]
fn special_char_path_add_round_trips() {
    let mut paths = capturable_quoting_paths();
    paths.extend(worktree_only_quoting_paths());
    for path in paths {
        native_cell(
            &[normal("anchor.txt", "anchor\n")],
            |dir| write_entry(dir, &normal(path, "alpha\nbeta\n")),
            &[Expect::Present(normal(path, "alpha\nbeta\n"))],
        );
    }
}

#[test]
fn special_char_path_modify_round_trips() {
    for path in capturable_quoting_paths() {
        native_cell(
            &[normal(path, "l1\nl2\nl3\n")],
            |dir| write_entry(dir, &normal(path, "l1\nCHANGED\nl3\n")),
            &[Expect::Present(normal(path, "l1\nCHANGED\nl3\n"))],
        );
    }
}

#[test]
fn special_char_path_delete_round_trips() {
    for path in capturable_quoting_paths() {
        native_cell(
            &[normal(path, "doomed\ncontent\n"), normal("keep.txt", "keep\n")],
            move |dir| std::fs::remove_file(dir.join(path)).unwrap(),
            &[
                Expect::Absent(path),
                Expect::Present(normal("keep.txt", "keep\n")),
            ],
        );
    }
}

#[test]
fn special_char_path_rename_round_trips() {
    // Rename a quote-named file to a unicode-named file: every header site
    // (`diff --git`, `rename from`/`to`) must quote both sides. Both names
    // are heddle-capturable (no control/backslash bytes).
    let body = "alpha\nbeta\ngamma\ndelta\nepsilon\n";
    native_cell(
        &[normal("fro\"m.txt", body)],
        |dir| {
            std::fs::remove_file(dir.join("fro\"m.txt")).unwrap();
            write_entry(dir, &normal("tö nÿ.txt", body));
        },
        &[
            Expect::Absent("fro\"m.txt"),
            Expect::Present(normal("tö nÿ.txt", body)),
        ],
    );
}

/// Control-char / backslash names that heddle can't track but git can:
/// exercise modify + delete on the plain-Git surface so the header quoting
/// holds for the gix-read tree side too.
#[test]
fn plain_git_special_char_path_modify_round_trips() {
    for path in worktree_only_quoting_paths() {
        plain_git_cell(
            &[normal(path, "a\nb\nc\n")],
            false,
            move |dir| write_entry(dir, &normal(path, "a\nB\nc\n")),
            &[Expect::Present(normal(path, "a\nB\nc\n"))],
        );
    }
}

#[test]
fn plain_git_special_char_path_delete_round_trips() {
    for path in worktree_only_quoting_paths() {
        plain_git_cell(
            &[normal(path, "x\ny\n"), normal("keep.txt", "keep\n")],
            true,
            move |dir| std::fs::remove_file(dir.join(path)).unwrap(),
            &[
                Expect::Absent(path),
                Expect::Present(normal("keep.txt", "keep\n")),
            ],
        );
    }
}

// ---------------------------------------------------------------------------
// Matrix — rename
// ---------------------------------------------------------------------------

#[test]
fn pure_rename_round_trips() {
    let body = "alpha\nbeta\ngamma\ndelta\nepsilon\n";
    native_cell(
        &[normal("from.txt", body)],
        |dir| {
            std::fs::remove_file(dir.join("from.txt")).unwrap();
            write_entry(dir, &normal("to.txt", body));
        },
        &[
            Expect::Absent("from.txt"),
            Expect::Present(normal("to.txt", body)),
        ],
    );
}

#[test]
fn rename_with_edit_round_trips() {
    let before = "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n";
    let after = "l1\nl2\nCHANGED\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n";
    native_cell(
        &[normal("source.txt", before)],
        |dir| {
            std::fs::remove_file(dir.join("source.txt")).unwrap();
            write_entry(dir, &normal("target.txt", after));
        },
        &[
            Expect::Absent("source.txt"),
            Expect::Present(normal("target.txt", after)),
        ],
    );
}

/// A pure rename must populate the JSON `.patch` field as a header-only
/// patch even though it has no line body. Covers cid 3318629236.
#[test]
fn pure_rename_populates_json_patch_field() {
    let body = "a\nb\nc\nd\ne\n";
    let h = TempDir::new().unwrap();
    heddle(&["init"], Some(h.path())).unwrap();
    write_entry(h.path(), &normal("from.txt", body));
    heddle(&["capture", "-m", "v1"], Some(h.path())).unwrap();
    std::fs::remove_file(h.path().join("from.txt")).unwrap();
    write_entry(h.path(), &normal("to.txt", body));

    let json_patch = json_patch_field(h.path()).expect("pure rename JSON must carry `.patch`");
    assert!(
        json_patch.contains("rename from from.txt") && json_patch.contains("rename to to.txt"),
        "JSON `.patch` must carry the rename headers:\n{json_patch}"
    );
    // And it must be a patch real git accepts.
    apply_oracle(
        &[normal("from.txt", body)],
        &json_patch,
        &[
            Expect::Absent("from.txt"),
            Expect::Present(normal("to.txt", body)),
        ],
    );
}

// ---------------------------------------------------------------------------
// Matrix — combined ops (rename × mode). Covers cid 3319049643 — a rename
// paired with a chmod/type change must carry `old mode`/`new mode` so the
// permission change round-trips alongside the move.
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn rename_with_chmod_round_trips() {
    let body = "alpha\nbeta\ngamma\ndelta\nepsilon\n";
    native_cell(
        &[normal("old.sh", body)],
        |dir| {
            std::fs::remove_file(dir.join("old.sh")).unwrap();
            write_entry(dir, &exec("new.sh", body));
        },
        &[
            Expect::Absent("old.sh"),
            Expect::Present(exec("new.sh", body)),
        ],
    );
}

/// Pin the header shape: rename+chmod must emit `old mode`/`new mode`
/// before `similarity index`, matching `git diff`. A regression that drops
/// the deleted-side mode (the cid 3319049643 bug) trips here.
#[cfg(unix)]
#[test]
fn rename_with_chmod_emits_mode_headers() {
    let body = "alpha\nbeta\ngamma\ndelta\nepsilon\n";
    let h = TempDir::new().unwrap();
    heddle(&["init"], Some(h.path())).unwrap();
    write_entry(h.path(), &normal("old.sh", body));
    heddle(&["capture", "-m", "v1"], Some(h.path())).unwrap();
    std::fs::remove_file(h.path().join("old.sh")).unwrap();
    write_entry(h.path(), &exec("new.sh", body));

    let patch = heddle(&["diff", "--patch"], Some(h.path())).unwrap();
    assert!(
        patch.contains("rename from old.sh") && patch.contains("rename to new.sh"),
        "rename+chmod must still emit the rename headers:\n{patch}"
    );
    assert!(
        patch.contains("old mode 100644") && patch.contains("new mode 100755"),
        "rename+chmod must carry `old mode`/`new mode`:\n{patch}"
    );
    let old_mode_idx = patch.find("old mode").unwrap();
    let sim_idx = patch.find("similarity index").unwrap();
    assert!(
        old_mode_idx < sim_idx,
        "`old mode` must precede `similarity index` (git order):\n{patch}"
    );
}

#[cfg(unix)]
#[test]
fn rename_with_edit_and_chmod_round_trips() {
    let before = "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n";
    let after = "l1\nl2\nCHANGED\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n";
    native_cell(
        &[normal("src.sh", before)],
        |dir| {
            std::fs::remove_file(dir.join("src.sh")).unwrap();
            write_entry(dir, &exec("dst.sh", after));
        },
        &[
            Expect::Absent("src.sh"),
            Expect::Present(exec("dst.sh", after)),
        ],
    );
}

// ---------------------------------------------------------------------------
// Matrix — non-UTF-8 symlink target × rename (cid 3322251771). Git stores a
// symlink's target as RAW bytes, which need not be valid UTF-8. The worktree
// blob read (`read_worktree_blob_for_diff`) must preserve those bytes; a
// `to_string_lossy` conversion replaces invalid bytes with U+FFFD, so the
// similarity score drops AND the generated patch encodes the corrupted target
// — `git apply` then creates a link pointing at the wrong place. The same
// lossy bug lived in the capture path (the stored blob) and the status hash;
// all now route through one raw-bytes helper. A symlink moved to a new path
// with an identical (non-UTF-8) target scores similarity 1.0 and collapses
// into a header-only rename, exercising the target bytes through the rename
// path on both the worktree surface (`native_cell` — the read that took the
// lossy path) and the committed-tree surface (`state_cell`).
// ---------------------------------------------------------------------------

/// Non-UTF-8 target with a high-bit-set invalid byte sequence. `\xFF\xFE` is
/// never valid UTF-8, so `to_string_lossy` would mangle it.
#[cfg(unix)]
const NON_UTF8_TARGET: &[u8] = b"dest/\xff\xfe/link-target";

/// Worktree surface: a symlink with a non-UTF-8 target moved to a new path.
/// The added side's target is read via `read_worktree_blob_for_diff` — the
/// read that r15 converted with `to_string_lossy`. Raw bytes must survive so
/// the rename collapses and `git apply` reproduces the byte-exact target.
#[cfg(unix)]
#[test]
fn native_non_utf8_symlink_rename_round_trips() {
    native_cell(
        &[symlink_bytes("from-link", NON_UTF8_TARGET), normal("anchor.txt", "anchor\n")],
        |dir| {
            std::fs::remove_file(dir.join("from-link")).unwrap();
            write_entry(dir, &symlink_bytes("to-link", NON_UTF8_TARGET));
        },
        &[
            Expect::Absent("from-link"),
            Expect::Present(symlink_bytes("to-link", NON_UTF8_TARGET)),
            Expect::Present(normal("anchor.txt", "anchor\n")),
        ],
    );
}

/// Committed-tree surface: same non-UTF-8 symlink rename through the
/// `HEAD~1 HEAD` diff path. Stored target bytes must round-trip byte-exactly.
#[cfg(unix)]
#[test]
fn state_non_utf8_symlink_rename_round_trips() {
    state_cell(
        &[symlink_bytes("from-link", NON_UTF8_TARGET), normal("keep.txt", "keep\n")],
        |dir| {
            std::fs::remove_file(dir.join("from-link")).unwrap();
            write_entry(dir, &symlink_bytes("to-link", NON_UTF8_TARGET));
        },
        &[
            Expect::Absent("from-link"),
            Expect::Present(symlink_bytes("to-link", NON_UTF8_TARGET)),
            Expect::Present(normal("keep.txt", "keep\n")),
        ],
    );
}

// ---------------------------------------------------------------------------
// Matrix — non-UTF-8 symlink target IN THE HUNK BODY (cid 3323910729). r16
// closed the rename-similarity class: an identical non-UTF-8 target on both
// sides collapses to a *header-only* rename (no hunk body), so those cells
// never carried raw bytes in a hunk. This round closes the patch-OUTPUT
// class: an add / delete / target-edit of a symlink emits a real text hunk
// whose +/- line IS the target bytes. The old code marked such a change
// `binary` (its target failed `content_str()`), producing a placeholder
// binary stanza that `git apply` REJECTS for a `120000` entry — so a
// non-UTF-8 symlink add/delete/edit never round-tripped. The fix routes the
// target through one byte-preserving path (`render_symlink_change`).
//
// These assert byte-exact `git apply` round-trip from RAW `--patch` stdout
// (a non-UTF-8 target is not a valid `&str`, so the String-capturing
// `native_cell`/`state_cell` can't carry it). Coverage is per surface ×
// every backend: the heddle-overlay worktree path (`native_cell_bytes`), the
// heddle-overlay committed path (`state_cell_bytes`), and the plain-Git fast
// path (`plain_git_cell_bytes`). A regression on ANY surface fails CI here.
//
// The rename surface is regression-guarded by the r16 cells above: a *changed*
// non-UTF-8 target scores similarity 0 (single line, no overlap) and never
// collapses, so it renders as the delete + add these cells already cover; an
// *identical* target collapses to a header-only rename (no bytes in the body).
// ---------------------------------------------------------------------------

/// Pipe a RAW-byte patch into `git <args>` (the byte analogue of `pipe_git`),
/// for patches whose hunk body carries a non-UTF-8 symlink target.
fn pipe_git_bytes(dir: &Path, args: &[&str], patch: &[u8]) -> Output {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("git should spawn");
    child.stdin.as_mut().unwrap().write_all(patch).unwrap();
    child.wait_with_output().expect("git should finish")
}

/// Byte-exact analogue of `apply_oracle`: seed `pre`, then `git apply --check`
/// + `git apply` the RAW-byte `patch`, and assert every `expect` holds.
fn apply_oracle_bytes(pre: &[Entry], patch: &[u8], expect: &[Expect]) {
    let g = TempDir::new().unwrap();
    git_init(g.path());
    for entry in pre {
        write_entry(g.path(), entry);
    }
    git(g.path(), &["add", "-A"]);
    git(g.path(), &["commit", "-q", "-m", "seed"]);

    let check = pipe_git_bytes(g.path(), &["apply", "--check"], patch);
    assert!(
        check.status.success(),
        "git apply --check rejected the patch;\nstderr={}\npatch=\n{}",
        String::from_utf8_lossy(&check.stderr),
        String::from_utf8_lossy(patch),
    );
    let applied = pipe_git_bytes(g.path(), &["apply"], patch);
    assert!(
        applied.status.success(),
        "git apply failed;\nstderr={}\npatch=\n{}",
        String::from_utf8_lossy(&applied.stderr),
        String::from_utf8_lossy(patch),
    );

    for exp in expect {
        match exp {
            Expect::Present(entry) => assert_present(g.path(), entry),
            Expect::Absent(path) => assert!(
                !g.path().join(path).exists(),
                "`{path}` should be gone after apply",
            ),
        }
    }
}

/// Capture `heddle <args>` stdout as RAW bytes (the String-capturing `heddle`
/// wrapper drops a non-UTF-8 patch to `""`). Asserts success + non-empty.
fn patch_bytes(args: &[&str], cwd: &Path) -> Vec<u8> {
    let out = heddle_output(args, Some(cwd)).expect("heddle diff --patch");
    assert!(
        out.status.success(),
        "heddle {args:?} should succeed; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.stdout.is_empty(),
        "cell produced an empty patch (no change detected?)"
    );
    out.stdout
}

/// Worktree-surface byte cell: heddle-overlay backend, `heddle diff --patch`.
fn native_cell_bytes(pre: &[Entry], mutate: impl Fn(&Path), expect: &[Expect]) {
    let h = TempDir::new().unwrap();
    heddle(&["init"], Some(h.path())).unwrap();
    for entry in pre {
        write_entry(h.path(), entry);
    }
    heddle(&["capture", "-m", "v1"], Some(h.path())).unwrap();
    mutate(h.path());

    let patch = patch_bytes(&["diff", "--patch"], h.path());
    // Same all-modes contract as the text cells: the JSON `.patch` field is
    // the LOSSY view of these bytes, so it must equal `from_utf8_lossy(stdout)`
    // in every render mode, and rename/type detection must agree across modes.
    assert_modes_consistent(h.path(), &[], &String::from_utf8_lossy(&patch));
    apply_oracle_bytes(pre, &patch, expect);
}

/// Committed-surface byte cell: heddle-overlay backend, `diff HEAD~1 HEAD`.
fn state_cell_bytes(pre: &[Entry], mutate: impl Fn(&Path), expect: &[Expect]) {
    let h = TempDir::new().unwrap();
    heddle(&["init"], Some(h.path())).unwrap();
    for entry in pre {
        write_entry(h.path(), entry);
    }
    heddle(&["capture", "-m", "v1"], Some(h.path())).unwrap();
    mutate(h.path());
    heddle(&["capture", "-m", "v2"], Some(h.path())).unwrap();

    let patch = patch_bytes(&["diff", "HEAD~1", "HEAD", "--patch"], h.path());
    assert_modes_consistent(h.path(), &["HEAD~1", "HEAD"], &String::from_utf8_lossy(&patch));
    apply_oracle_bytes(pre, &patch, expect);
}

/// Plain-Git fast-path byte cell: no `heddle init`; HEAD read via gix.
fn plain_git_cell_bytes(
    pre: &[Entry],
    stage: bool,
    mutate: impl Fn(&Path),
    expect: &[Expect],
) {
    let h = TempDir::new().unwrap();
    git_init(h.path());
    for entry in pre {
        write_entry(h.path(), entry);
    }
    git(h.path(), &["add", "-A"]);
    git(h.path(), &["commit", "-q", "-m", "seed"]);
    mutate(h.path());
    if stage {
        git(h.path(), &["add", "-A"]);
    }

    let patch = patch_bytes(&["diff", "--patch"], h.path());
    assert_modes_consistent(h.path(), &[], &String::from_utf8_lossy(&patch));
    apply_oracle_bytes(pre, &patch, expect);
}

// --- add ---

#[cfg(unix)]
#[test]
fn native_non_utf8_symlink_add_round_trips() {
    native_cell_bytes(
        &[normal("anchor.txt", "anchor\n")],
        |dir| write_entry(dir, &symlink_bytes("linky", NON_UTF8_TARGET)),
        &[Expect::Present(symlink_bytes("linky", NON_UTF8_TARGET))],
    );
}

#[cfg(unix)]
#[test]
fn state_non_utf8_symlink_add_round_trips() {
    state_cell_bytes(
        &[normal("keep.txt", "keep\n")],
        |dir| write_entry(dir, &symlink_bytes("linky", NON_UTF8_TARGET)),
        &[Expect::Present(symlink_bytes("linky", NON_UTF8_TARGET))],
    );
}

#[cfg(unix)]
#[test]
fn plain_git_non_utf8_symlink_add_round_trips() {
    plain_git_cell_bytes(
        &[normal("anchor.txt", "anchor\n")],
        true,
        |dir| write_entry(dir, &symlink_bytes("linky", NON_UTF8_TARGET)),
        &[Expect::Present(symlink_bytes("linky", NON_UTF8_TARGET))],
    );
}

// --- delete ---

#[cfg(unix)]
#[test]
fn native_non_utf8_symlink_delete_round_trips() {
    native_cell_bytes(
        &[symlink_bytes("doomed", NON_UTF8_TARGET), normal("keep.txt", "keep\n")],
        |dir| std::fs::remove_file(dir.join("doomed")).unwrap(),
        &[
            Expect::Absent("doomed"),
            Expect::Present(normal("keep.txt", "keep\n")),
        ],
    );
}

#[cfg(unix)]
#[test]
fn state_non_utf8_symlink_delete_round_trips() {
    state_cell_bytes(
        &[symlink_bytes("doomed", NON_UTF8_TARGET), normal("keep.txt", "keep\n")],
        |dir| std::fs::remove_file(dir.join("doomed")).unwrap(),
        &[
            Expect::Absent("doomed"),
            Expect::Present(normal("keep.txt", "keep\n")),
        ],
    );
}

#[cfg(unix)]
#[test]
fn plain_git_non_utf8_symlink_delete_round_trips() {
    plain_git_cell_bytes(
        &[symlink_bytes("doomed", NON_UTF8_TARGET), normal("keep.txt", "keep\n")],
        true,
        |dir| std::fs::remove_file(dir.join("doomed")).unwrap(),
        &[
            Expect::Absent("doomed"),
            Expect::Present(normal("keep.txt", "keep\n")),
        ],
    );
}

// --- target edit (to AND from non-UTF-8 bytes) ---

/// A second non-UTF-8 target, distinct from `NON_UTF8_TARGET`, so an edit cell
/// changes the bytes on both sides of the hunk.
#[cfg(unix)]
const NON_UTF8_TARGET_ALT: &[u8] = b"other/\xfe\xff/elsewhere";

#[cfg(unix)]
#[test]
fn native_non_utf8_symlink_edit_round_trips() {
    native_cell_bytes(
        &[symlink_bytes("linky", NON_UTF8_TARGET)],
        |dir| write_entry(dir, &symlink_bytes("linky", NON_UTF8_TARGET_ALT)),
        &[Expect::Present(symlink_bytes("linky", NON_UTF8_TARGET_ALT))],
    );
}

#[cfg(unix)]
#[test]
fn state_non_utf8_symlink_edit_round_trips() {
    state_cell_bytes(
        &[symlink_bytes("linky", NON_UTF8_TARGET)],
        |dir| write_entry(dir, &symlink_bytes("linky", NON_UTF8_TARGET_ALT)),
        &[Expect::Present(symlink_bytes("linky", NON_UTF8_TARGET_ALT))],
    );
}

#[cfg(unix)]
#[test]
fn plain_git_non_utf8_symlink_edit_round_trips() {
    plain_git_cell_bytes(
        &[symlink_bytes("linky", NON_UTF8_TARGET)],
        false,
        |dir| write_entry(dir, &symlink_bytes("linky", NON_UTF8_TARGET_ALT)),
        &[Expect::Present(symlink_bytes("linky", NON_UTF8_TARGET_ALT))],
    );
}

/// Edit a symlink FROM a non-UTF-8 target TO a valid-UTF-8 one (and the
/// add/edit cells above cover the reverse): the `-` line carries raw bytes,
/// the `+` line is plain text, so the byte renderer must mix both in one hunk.
#[cfg(unix)]
#[test]
fn native_non_utf8_symlink_edit_to_utf8_round_trips() {
    native_cell_bytes(
        &[symlink_bytes("linky", NON_UTF8_TARGET)],
        |dir| write_entry(dir, &symlink("linky", "plain/utf8/target")),
        &[Expect::Present(symlink("linky", "plain/utf8/target"))],
    );
}

// ---------------------------------------------------------------------------
// Matrix — rename-candidate × type change (cid 3320838479). A delete + add at
// *different* paths whose bytes are identical scores as a rename (similarity
// 1.0). The rename-collapse must NOT merge such a pair when the two sides
// cross git's regular↔symlink type boundary: collapsing emits a `rename
// from/to` carrying a mismatched `old mode 100644`/`new mode 120000`, which
// `git apply` rejects ("new mode … does not match old mode …"). The pair has
// to stay a separate delete + add. A regular↔executable move stays *within*
// git's regular-file type class, so it is still a legal rename-with-mode-change
// that `git apply` accepts — the guard must not over-block it. These run on
// BOTH heddle rename-collapse backends: the worktree-status path (`native_cell`,
// which reads the added symlink's blob by *following* the link on disk) and the
// committed-tree path (`state_cell`, which reads the symlink's stored target
// bytes). The plain-Git fast path does no rename collapse, so its cross-type
// pair is already a delete + add — `plain_git_*` below is the regression guard
// that it stays that way.
// ---------------------------------------------------------------------------

/// Worktree surface: a tracked regular file deleted at one path and a symlink
/// added at another, both resolving to identical bytes (the link points at an
/// unchanged anchor file with the same content, so the worktree similarity —
/// which follows the link — scores it as a rename). Must stay delete + add.
#[cfg(unix)]
#[test]
fn native_regular_to_symlink_rename_candidate_stays_split() {
    let shared = "shared payload\n";
    native_cell(
        &[normal("mover.txt", shared), normal("anchor.txt", shared)],
        |dir| {
            std::fs::remove_file(dir.join("mover.txt")).unwrap();
            // `linked` -> `anchor.txt`; the worktree blob read follows the
            // link, so the added side's bytes equal the deleted file's bytes
            // and the pair scores as a rename candidate.
            write_entry(dir, &symlink("linked", "anchor.txt"));
        },
        &[
            Expect::Absent("mover.txt"),
            Expect::Present(symlink("linked", "anchor.txt")),
            Expect::Present(normal("anchor.txt", shared)),
        ],
    );
}

/// Committed-tree surface: a regular file whose content equals a symlink's
/// target string. The stored symlink blob is its target bytes, so the deleted
/// regular blob and the added symlink blob are byte-identical → rename
/// candidate → must stay delete + add across the type boundary.
#[cfg(unix)]
#[test]
fn state_regular_to_symlink_rename_candidate_stays_split() {
    state_cell(
        &[normal("mover.txt", "dest/dir/file"), normal("keep.txt", "keep\n")],
        |dir| {
            std::fs::remove_file(dir.join("mover.txt")).unwrap();
            write_entry(dir, &symlink("linked", "dest/dir/file"));
        },
        &[
            Expect::Absent("mover.txt"),
            Expect::Present(symlink("linked", "dest/dir/file")),
            Expect::Present(normal("keep.txt", "keep\n")),
        ],
    );
}

/// The reverse direction: a symlink deleted at one path, a regular file
/// carrying the link's target bytes added at another. Still a cross-type
/// rename candidate (120000 ↔ 100644), still delete + add.
#[cfg(unix)]
#[test]
fn state_symlink_to_regular_rename_candidate_stays_split() {
    state_cell(
        &[symlink("mover", "dest/dir/file"), normal("keep.txt", "keep\n")],
        |dir| {
            std::fs::remove_file(dir.join("mover")).unwrap();
            write_entry(dir, &normal("landed.txt", "dest/dir/file"));
        },
        &[
            Expect::Absent("mover"),
            Expect::Present(normal("landed.txt", "dest/dir/file")),
            Expect::Present(normal("keep.txt", "keep\n")),
        ],
    );
}

/// The companion must-not-over-block guard: a regular→executable move at
/// different paths stays within git's regular-file type class, so it MUST
/// still collapse into a rename carrying an `old mode 100644`/`new mode
/// 100755` pair that `git apply` accepts. Runs on the committed-tree surface
/// (the worktree surface is already covered by `rename_with_chmod_*`).
#[cfg(unix)]
#[test]
fn state_regular_to_exec_rename_candidate_collapses() {
    let body = "alpha\nbeta\ngamma\ndelta\nepsilon\n";
    let h = TempDir::new().unwrap();
    heddle(&["init"], Some(h.path())).unwrap();
    write_entry(h.path(), &normal("old.sh", body));
    heddle(&["capture", "-m", "v1"], Some(h.path())).unwrap();
    std::fs::remove_file(h.path().join("old.sh")).unwrap();
    write_entry(h.path(), &exec("new.sh", body));
    heddle(&["capture", "-m", "v2"], Some(h.path())).unwrap();

    let patch = heddle(&["diff", "HEAD~1", "HEAD", "--patch"], Some(h.path())).unwrap();
    assert!(
        patch.contains("rename from old.sh") && patch.contains("rename to new.sh"),
        "regular→exec move must still collapse into a rename:\n{patch}"
    );
    assert!(
        patch.contains("old mode 100644") && patch.contains("new mode 100755"),
        "regular→exec rename must carry the `old mode`/`new mode` pair:\n{patch}"
    );
    apply_oracle(
        &[normal("old.sh", body)],
        &patch,
        &[
            Expect::Absent("old.sh"),
            Expect::Present(exec("new.sh", body)),
        ],
    );
}

/// Pin the rendered shape, not just the round-trip: a cross-type rename
/// candidate must emit `deleted file mode 100644` + `new file mode 120000`
/// and NEVER a `rename from`, so a regression that round-trips by some other
/// mechanism (or re-introduces the cross-type rename) trips here.
#[cfg(unix)]
#[test]
fn cross_type_rename_candidate_renders_as_split_not_rename() {
    let h = TempDir::new().unwrap();
    heddle(&["init"], Some(h.path())).unwrap();
    write_entry(h.path(), &normal("mover.txt", "dest/dir/file"));
    write_entry(h.path(), &normal("keep.txt", "keep\n"));
    heddle(&["capture", "-m", "v1"], Some(h.path())).unwrap();
    std::fs::remove_file(h.path().join("mover.txt")).unwrap();
    write_entry(h.path(), &symlink("linked", "dest/dir/file"));
    heddle(&["capture", "-m", "v2"], Some(h.path())).unwrap();

    let patch = heddle(&["diff", "HEAD~1", "HEAD", "--patch"], Some(h.path())).unwrap();
    assert!(
        !patch.contains("rename from"),
        "cross-type move must not collapse into a rename:\n{patch}"
    );
    assert!(
        patch.contains("deleted file mode 100644") && patch.contains("new file mode 120000"),
        "cross-type move must render as delete(100644) + add(120000):\n{patch}"
    );
    apply_oracle(
        &[normal("mover.txt", "dest/dir/file"), normal("keep.txt", "keep\n")],
        &patch,
        &[
            Expect::Absent("mover.txt"),
            Expect::Present(symlink("linked", "dest/dir/file")),
            Expect::Present(normal("keep.txt", "keep\n")),
        ],
    );
}

/// Plain-Git both-path guard: the fast path does no rename collapse, so a
/// cross-type delete+add is already two separate stanzas. This pins that it
/// stays delete + add (never a future cross-type rename) and round-trips.
#[cfg(unix)]
#[test]
fn plain_git_regular_to_symlink_rename_candidate_stays_split() {
    plain_git_cell(
        &[normal("mover.txt", "dest/dir/file"), normal("keep.txt", "keep\n")],
        true,
        |dir| {
            std::fs::remove_file(dir.join("mover.txt")).unwrap();
            write_entry(dir, &symlink("linked", "dest/dir/file"));
        },
        &[
            Expect::Absent("mover.txt"),
            Expect::Present(symlink("linked", "dest/dir/file")),
            Expect::Present(normal("keep.txt", "keep\n")),
        ],
    );
}

// ---------------------------------------------------------------------------
// Matrix — symlink↔symlink / symlink→regular rename candidates (cid
// 3322115749). Rename similarity must compare the bytes git stores as the
// blob, per entry type: a regular file → its content, a symlink → its target
// *path* bytes (`read_link`). The worktree rename path used to read the new
// candidate with a blind `std::fs::read`, which *follows* a symlink and reads
// the dereferenced target FILE's content instead of the link's own target
// bytes. A tracked symlink moved to a new symlink whose dereferenced target
// happened to match the old link's target string then collapsed into a pure
// rename — and `git apply` left the OLD link target on disk instead of
// creating the NEW one. These cells run on BOTH backends: the worktree-status
// path (`native_cell`) and the committed-tree path (`state_cell`, which reads
// the stored target bytes via `find_blob_in_tree`). Every cell also flows
// through `assert_modes_consistent`, so all output modes are covered.
// ---------------------------------------------------------------------------

/// The exact cid 3322115749 trap on the worktree backend: `mover` is a symlink
/// whose target STRING equals the *content* of `anchor.txt`, and the new
/// symlink `moved` points AT `anchor.txt`. A blind `fs::read("moved")` follows
/// the link to `anchor.txt` and reads `"dest/dir/file"` — byte-identical to the
/// old link's stored target — scoring a false 1.0 rename that, as a pure
/// header-only rename, would leave `moved → dest/dir/file` (the OLD target)
/// after apply. Reading the link's own target (`anchor.txt`) instead scores
/// low → stays delete + add → `moved` materializes with the correct target.
#[cfg(unix)]
#[test]
fn native_symlink_to_symlink_different_target_round_trips() {
    native_cell(
        &[
            symlink("mover", "dest/dir/file"),
            normal("anchor.txt", "dest/dir/file"),
        ],
        |dir| {
            std::fs::remove_file(dir.join("mover")).unwrap();
            write_entry(dir, &symlink("moved", "anchor.txt"));
        },
        &[
            Expect::Absent("mover"),
            Expect::Present(symlink("moved", "anchor.txt")),
            Expect::Present(normal("anchor.txt", "dest/dir/file")),
        ],
    );
}

/// Committed-tree backend of the same shape: the stored symlink blob is the
/// link's target bytes, so the new candidate is `"anchor.txt"`, never the
/// dereferenced file content — the pair stays split and round-trips with the
/// correct new target.
#[cfg(unix)]
#[test]
fn state_symlink_to_symlink_different_target_round_trips() {
    state_cell(
        &[
            symlink("mover", "dest/dir/file"),
            normal("anchor.txt", "dest/dir/file"),
        ],
        |dir| {
            std::fs::remove_file(dir.join("mover")).unwrap();
            write_entry(dir, &symlink("moved", "anchor.txt"));
        },
        &[
            Expect::Absent("mover"),
            Expect::Present(symlink("moved", "anchor.txt")),
            Expect::Present(normal("anchor.txt", "dest/dir/file")),
        ],
    );
}

/// A symlink moved to a symlink with the SAME target IS a legitimate rename;
/// whether it collapses or stays split, the new link must carry the right
/// target after apply. Reading link-target bytes (not dereferenced content)
/// keeps the target intact on both detection and patch generation.
#[cfg(unix)]
#[test]
fn native_symlink_to_symlink_same_target_round_trips() {
    native_cell(
        &[symlink("mover", "shared/target/path")],
        |dir| {
            std::fs::remove_file(dir.join("mover")).unwrap();
            write_entry(dir, &symlink("moved", "shared/target/path"));
        },
        &[
            Expect::Absent("mover"),
            Expect::Present(symlink("moved", "shared/target/path")),
        ],
    );
}

/// Committed-tree backend of the same-target symlink move.
#[cfg(unix)]
#[test]
fn state_symlink_to_symlink_same_target_round_trips() {
    state_cell(
        &[symlink("mover", "shared/target/path"), normal("keep.txt", "keep\n")],
        |dir| {
            std::fs::remove_file(dir.join("mover")).unwrap();
            write_entry(dir, &symlink("moved", "shared/target/path"));
        },
        &[
            Expect::Absent("mover"),
            Expect::Present(symlink("moved", "shared/target/path")),
            Expect::Present(normal("keep.txt", "keep\n")),
        ],
    );
}

/// Worktree backend of the symlink→regular cross-type candidate: a symlink
/// deleted at one path, a regular file carrying the link's target string added
/// at another. The deleted-side mode is `120000`, the added-side `100644`, so
/// `rename_mode_compatible` keeps the pair split regardless of similarity — the
/// worktree complement of `state_symlink_to_regular_rename_candidate_stays_split`.
#[cfg(unix)]
#[test]
fn native_symlink_to_regular_rename_candidate_stays_split() {
    native_cell(
        &[symlink("mover", "dest/dir/file"), normal("keep.txt", "keep\n")],
        |dir| {
            std::fs::remove_file(dir.join("mover")).unwrap();
            write_entry(dir, &normal("landed.txt", "dest/dir/file"));
        },
        &[
            Expect::Absent("mover"),
            Expect::Present(normal("landed.txt", "dest/dir/file")),
            Expect::Present(normal("keep.txt", "keep\n")),
        ],
    );
}

// ---------------------------------------------------------------------------
// Matrix — rename×type on the STATUS path (cid 3321103601). The cells above
// assert `--patch`/JSON, which capture each side's mode and so feed the
// rename-collapse its cross-type guard. The default, `--stat`, and
// `--name-only` renders take the status-only path, which used to drop modes
// (they were gated on the hunk-only flag) — so the same cross-type move that
// `--patch` keeps split silently re-collapsed into a rename there. These pin
// every status render to the patch render's verdict, on BOTH rename-collapse
// backends: the worktree-status path (`status_renders`) and the committed-tree
// path (`state_status_renders`, `heddle diff HEAD~1 HEAD`).
// ---------------------------------------------------------------------------

/// The three non-`--patch` renders of one repo state, captured together so a
/// single setup exercises every status-path renderer.
struct StatusRenders {
    default: String,
    stat: String,
    name_only: String,
}

/// Worktree-status renders: capture `pre`, mutate the worktree, then run
/// `heddle diff` with no flag / `--stat` / `--name-only` (all read-only, so
/// they share one worktree).
fn status_renders(pre: &[Entry], mutate: impl Fn(&Path)) -> StatusRenders {
    let h = TempDir::new().unwrap();
    heddle(&["init"], Some(h.path())).unwrap();
    for entry in pre {
        write_entry(h.path(), entry);
    }
    heddle(&["capture", "-m", "v1"], Some(h.path())).unwrap();
    mutate(h.path());
    StatusRenders {
        default: heddle(&["diff"], Some(h.path())).unwrap(),
        stat: heddle(&["diff", "--stat"], Some(h.path())).unwrap(),
        name_only: heddle(&["diff", "--name-only"], Some(h.path())).unwrap(),
    }
}

/// Committed-tree renders: capture `pre` as v1, mutate, capture v2, then run
/// `heddle diff HEAD~1 HEAD` with no flag / `--stat` / `--name-only`.
fn state_status_renders(pre: &[Entry], mutate: impl Fn(&Path)) -> StatusRenders {
    let h = TempDir::new().unwrap();
    heddle(&["init"], Some(h.path())).unwrap();
    for entry in pre {
        write_entry(h.path(), entry);
    }
    heddle(&["capture", "-m", "v1"], Some(h.path())).unwrap();
    mutate(h.path());
    heddle(&["capture", "-m", "v2"], Some(h.path())).unwrap();
    StatusRenders {
        default: heddle(&["diff", "HEAD~1", "HEAD"], Some(h.path())).unwrap(),
        stat: heddle(&["diff", "HEAD~1", "HEAD", "--stat"], Some(h.path())).unwrap(),
        name_only: heddle(&["diff", "HEAD~1", "HEAD", "--name-only"], Some(h.path())).unwrap(),
    }
}

/// Assert the three status renders all treat the change as a cross-type
/// delete + add — never a rename — naming both the deleted and added paths.
fn assert_split_not_rename(renders: &StatusRenders, deleted: &str, added: &str) {
    assert!(
        !renders.default.contains("rename from"),
        "default render must keep the cross-type move split, not a rename:\n{}",
        renders.default
    );
    assert!(
        !renders.stat.contains("renamed") && !renders.stat.contains(" -> "),
        "--stat must keep the cross-type move split, not a rename:\n{}",
        renders.stat
    );
    assert!(
        renders.name_only.lines().any(|line| line == deleted)
            && renders.name_only.lines().any(|line| line == added),
        "--name-only must list both `{deleted}` (deleted) and `{added}` (added), \
         not collapse to one renamed path:\n{}",
        renders.name_only
    );
}

/// Worktree surface: a regular file removed at one path and a symlink (whose
/// followed bytes equal the removed file's) added at another scores as a
/// rename candidate, but crosses the regular↔symlink boundary — so every
/// status render must keep it split, matching `--patch`
/// (`native_regular_to_symlink_rename_candidate_stays_split`).
#[cfg(unix)]
#[test]
fn status_regular_to_symlink_rename_candidate_stays_split() {
    let shared = "shared payload\n";
    let renders = status_renders(
        &[normal("mover.txt", shared), normal("anchor.txt", shared)],
        |dir| {
            std::fs::remove_file(dir.join("mover.txt")).unwrap();
            write_entry(dir, &symlink("linked", "anchor.txt"));
        },
    );
    assert_split_not_rename(&renders, "mover.txt", "linked");
}

/// Committed-tree surface: a regular file whose stored bytes equal a symlink's
/// target string. The `--name-only` committed-diff render took its own
/// modeless builder, so the cross-type guard could not fire there either.
#[cfg(unix)]
#[test]
fn state_status_regular_to_symlink_rename_candidate_stays_split() {
    let renders = state_status_renders(
        &[normal("mover.txt", "dest/dir/file"), normal("keep.txt", "keep\n")],
        |dir| {
            std::fs::remove_file(dir.join("mover.txt")).unwrap();
            write_entry(dir, &symlink("linked", "dest/dir/file"));
        },
    );
    assert_split_not_rename(&renders, "mover.txt", "linked");
}

/// The reverse direction (cid 3321875377): the cross-type-ness lives on the
/// DELETED side. A *symlink* is removed and a regular file whose bytes equal
/// the symlink's target string is added — so a render that reads the deleted
/// entry's mode as a (modeless) regular file would collapse the pair into a
/// rename, while `--patch` keeps it split. `status_regular_to_symlink_*` above
/// covers the added-side direction; this pins the deleted-side one.
#[cfg(unix)]
#[test]
fn status_symlink_to_regular_rename_candidate_stays_split() {
    let renders = status_renders(
        &[symlink("mover_link", "dest/dir/file"), normal("keep.txt", "keep\n")],
        |dir| {
            std::fs::remove_file(dir.join("mover_link")).unwrap();
            write_entry(dir, &normal("newreg.txt", "dest/dir/file"));
        },
    );
    assert_split_not_rename(&renders, "mover_link", "newreg.txt");
}

/// Committed-tree surface of the deleted-symlink direction.
#[cfg(unix)]
#[test]
fn state_status_symlink_to_regular_rename_candidate_stays_split() {
    let renders = state_status_renders(
        &[symlink("mover_link", "dest/dir/file"), normal("keep.txt", "keep\n")],
        |dir| {
            std::fs::remove_file(dir.join("mover_link")).unwrap();
            write_entry(dir, &normal("newreg.txt", "dest/dir/file"));
        },
    );
    assert_split_not_rename(&renders, "mover_link", "newreg.txt");
}

/// The must-not-over-block companion: a regular→executable move stays within
/// git's regular-file type class, so every status render MUST still collapse
/// it into a rename (and `--patch` round-trips through `git apply`). Capturing
/// modes for the guard must not start blocking the legal rename+chmod.
#[cfg(unix)]
#[test]
fn status_regular_to_exec_move_still_collapses_to_rename() {
    let body = "alpha\nbeta\ngamma\ndelta\nepsilon\n";
    let renders = status_renders(
        &[normal("old.sh", body)],
        |dir| {
            std::fs::remove_file(dir.join("old.sh")).unwrap();
            write_entry(dir, &exec("new.sh", body));
        },
    );
    assert!(
        renders.stat.contains("renamed") && renders.stat.contains("old.sh -> new.sh"),
        "--stat must show the regular→exec move as a rename:\n{}",
        renders.stat
    );
    assert!(
        renders.default.contains("rename from old.sh")
            && renders.default.contains("rename to new.sh"),
        "default render must show the regular→exec move as a rename:\n{}",
        renders.default
    );
    // `--name-only` collapses to the single new path — `old.sh` is gone.
    assert!(
        renders.name_only.lines().any(|line| line == "new.sh")
            && !renders.name_only.lines().any(|line| line == "old.sh"),
        "--name-only must list only the renamed-to path for a regular→exec move:\n{}",
        renders.name_only
    );
    // And the patch form of the same move still round-trips with its chmod.
    let patch = {
        let h = TempDir::new().unwrap();
        heddle(&["init"], Some(h.path())).unwrap();
        write_entry(h.path(), &normal("old.sh", body));
        heddle(&["capture", "-m", "v1"], Some(h.path())).unwrap();
        std::fs::remove_file(h.path().join("old.sh")).unwrap();
        write_entry(h.path(), &exec("new.sh", body));
        heddle(&["diff", "--patch"], Some(h.path())).unwrap()
    };
    assert!(
        patch.contains("old mode 100644") && patch.contains("new mode 100755"),
        "rename+chmod patch must still carry the mode headers:\n{patch}"
    );
    apply_oracle(
        &[normal("old.sh", body)],
        &patch,
        &[Expect::Absent("old.sh"), Expect::Present(exec("new.sh", body))],
    );
}

/// A *same-path* regular↔symlink type change must split into a delete + add on
/// the status renders too — never surface as a single `modified` chmod. This
/// drives the type-change split (`expand_type_changes` → `make_type_change_part`)
/// in its no-hunk mode (`--stat`/`--name-only`), the sibling status-path
/// construction site, so it also captures modes through the shared helper.
#[cfg(unix)]
#[test]
fn status_same_path_regular_to_symlink_splits_not_modified() {
    let renders = status_renders(
        &[normal("swap", "shared payload\n"), normal("anchor.txt", "shared payload\n")],
        |dir| {
            std::fs::remove_file(dir.join("swap")).unwrap();
            write_entry(dir, &symlink("swap", "anchor.txt"));
        },
    );
    assert!(
        renders.stat.contains("deleted")
            && renders.stat.contains("added")
            && !renders.stat.contains("modified")
            && !renders.stat.contains("renamed"),
        "--stat must split a same-path type change into delete + add:\n{}",
        renders.stat
    );
    // `--name-only` lists the path on both the delete and add halves.
    assert_eq!(
        renders.name_only.lines().filter(|line| *line == "swap").count(),
        2,
        "--name-only must list the split path twice (delete + add):\n{}",
        renders.name_only
    );
}

// ---------------------------------------------------------------------------
// Matrix — rename×type on the GIT-OVERLAY status path (cid 3321875377). The
// cells above run pure-heddle repos, which take the `cmd_diff` path where the
// head tree is always loaded. The git-overlay-trust path — `heddle diff`
// rendering `render_worktree_status_diff` with a repo — is reached only when
// heddle trusts the visible git worktree status (e.g. the git branch advanced
// past the heddle import). There the head tree was loaded ONLY when hunks were
// inflated (`--patch`/JSON), so the default/--stat/--name-only renders saw a
// modeless deleted entry and a symlink→regular move re-collapsed into a rename
// while `--patch` kept it split. These pin every git-overlay status render to
// the `--patch` verdict — the actual surface the Codex finding pointed at.
// ---------------------------------------------------------------------------

/// Set up a git-overlay repo whose git branch has advanced past the heddle
/// import (so `heddle diff` trusts the git worktree status), then capture the
/// four renders of `mutate`'s worktree change plus the `--patch` oracle. `pre`
/// is committed BEFORE `heddle adopt`, so its blobs/modes live in heddle's
/// imported head tree where rename detection resolves them; the advancing
/// commit is unrelated content so it never appears in the worktree diff.
#[cfg(unix)]
fn git_overlay_status_renders(pre: &[Entry], mutate: impl Fn(&Path)) -> (StatusRenders, String) {
    let h = TempDir::new().unwrap();
    git_init(h.path());
    for entry in pre {
        write_entry(h.path(), entry);
    }
    git(h.path(), &["add", "-A"]);
    git(h.path(), &["commit", "-q", "-m", "seed"]);
    heddle(&["adopt"], Some(h.path())).unwrap();
    // Advance the git branch past the import with an UNRELATED commit so heddle
    // flips to trusting the git-overlay worktree status (`git_branch_advanced`).
    write_entry(h.path(), &normal("unrelated_advance.txt", "advance\n"));
    git(h.path(), &["add", "-A"]);
    git(h.path(), &["commit", "-q", "-m", "advance"]);
    mutate(h.path());
    let renders = StatusRenders {
        default: heddle(&["diff"], Some(h.path())).unwrap(),
        stat: heddle(&["diff", "--stat"], Some(h.path())).unwrap(),
        name_only: heddle(&["diff", "--name-only"], Some(h.path())).unwrap(),
    };
    let patch = heddle(&["diff", "--patch"], Some(h.path())).unwrap();
    (renders, patch)
}

/// The git-overlay reproduction of cid 3321875377: a symlink committed into the
/// imported head, removed in the worktree, and a regular file with the same
/// bytes added (so the pair scores as a rename candidate). `--patch` loads the
/// head tree and keeps the cross-type pair split; the default/--stat/
/// --name-only renders must reach the same verdict, not collapse to a rename.
#[cfg(unix)]
#[test]
fn git_overlay_status_symlink_to_regular_rename_candidate_stays_split() {
    let (renders, patch) = git_overlay_status_renders(
        &[symlink("mover_link", "dest/dir/file"), normal("keep.txt", "keep\n")],
        |dir| {
            std::fs::remove_file(dir.join("mover_link")).unwrap();
            write_entry(dir, &normal("newreg.txt", "dest/dir/file"));
        },
    );
    // The oracle: `--patch` must keep the cross-type pair split.
    assert!(
        !patch.contains("rename from"),
        "git-overlay --patch must keep the symlink→regular move split:\n{patch}"
    );
    assert_split_not_rename(&renders, "mover_link", "newreg.txt");
}

// ---------------------------------------------------------------------------
// Matrix — file ↔ directory type changes. Covers cid 3319049665 — a tracked
// file replaced by a directory (or vice-versa) must emit the deletion of the
// blocking path so `git apply` can create the new tree over it.
// ---------------------------------------------------------------------------

#[test]
fn file_to_dir_type_change_round_trips() {
    // `conf` is a tracked file; it becomes a directory `conf/` with a
    // nested file. git represents that as a deletion of `conf` + an add of
    // `conf/nested.txt`. Present(conf/nested.txt) implies `conf` is a dir.
    native_cell(
        &[normal("conf", "old config\n"), normal("keep.txt", "keep\n")],
        |dir| {
            std::fs::remove_file(dir.join("conf")).unwrap();
            write_entry(dir, &normal("conf/nested.txt", "nested\nvalue\n"));
        },
        &[
            Expect::Present(normal("conf/nested.txt", "nested\nvalue\n")),
            Expect::Present(normal("keep.txt", "keep\n")),
        ],
    );
}

#[test]
fn dir_to_file_type_change_round_trips() {
    // The mirror: a tracked `data/item.txt` (so `data` is a directory) is
    // replaced by a regular file `data`. git deletes `data/item.txt` and
    // adds the `data` file.
    native_cell(
        &[normal("data/item.txt", "x\ny\n"), normal("keep.txt", "keep\n")],
        |dir| {
            std::fs::remove_file(dir.join("data/item.txt")).unwrap();
            std::fs::remove_dir(dir.join("data")).unwrap();
            write_entry(dir, &normal("data", "now a file\n"));
        },
        &[
            Expect::Present(normal("data", "now a file\n")),
            Expect::Present(normal("keep.txt", "keep\n")),
        ],
    );
}

// ---------------------------------------------------------------------------
// Matrix — regular ↔ symlink type changes (cid 3319484727). A regular file
// replaced by a symlink (or vice-versa) is NOT a chmod — `git apply` rejects
// an `old mode 100644`/`new mode 120000` flip. It must be split into a delete
// of the old type + an add of the new type with the right `new file mode`.
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn regular_to_symlink_type_change_round_trips() {
    // `node` is a tracked regular file; it becomes a symlink. git represents
    // that as delete(100644 node) + add(120000 node -> target).
    native_cell(
        &[normal("node", "real contents\n"), normal("keep.txt", "keep\n")],
        |dir| {
            std::fs::remove_file(dir.join("node")).unwrap();
            write_entry(dir, &symlink("node", "some/target/path"));
        },
        &[
            Expect::Present(symlink("node", "some/target/path")),
            Expect::Present(normal("keep.txt", "keep\n")),
        ],
    );
}

#[cfg(unix)]
#[test]
fn symlink_to_regular_type_change_round_trips() {
    // The mirror: a tracked symlink becomes a regular file.
    native_cell(
        &[symlink("node", "some/target/path"), normal("keep.txt", "keep\n")],
        |dir| {
            std::fs::remove_file(dir.join("node")).unwrap();
            write_entry(dir, &normal("node", "now real contents\n"));
        },
        &[
            Expect::Present(normal("node", "now real contents\n")),
            Expect::Present(normal("keep.txt", "keep\n")),
        ],
    );
}

/// cid 3320033195 — a tracked regular file replaced by a symlink whose
/// *target is a real directory*. `Path::is_dir()` follows the link and
/// would misclassify the change as a plain deletion, dropping the `120000`
/// add and losing the new path on apply. Symlink-aware metadata
/// (`worktree_side_kind`) keeps it a regular→symlink type change → the
/// modify splits into delete(100644) + add(120000). The symlink target
/// `realdir` exists as a real directory in the worktree, so this trips the
/// `is_dir()`-follows-the-link defect specifically.
#[cfg(unix)]
#[test]
fn regular_to_symlink_pointing_at_dir_type_change_round_trips() {
    native_cell(
        &[
            normal("node", "real contents\n"),
            normal("realdir/keep.txt", "keep\n"),
        ],
        |dir| {
            std::fs::remove_file(dir.join("node")).unwrap();
            write_entry(dir, &symlink("node", "realdir"));
        },
        &[
            Expect::Present(symlink("node", "realdir")),
            Expect::Present(normal("realdir/keep.txt", "keep\n")),
        ],
    );
}

// ---------------------------------------------------------------------------
// Matrix — state-to-state type changes (cid 3319484717). The committed-tree
// surface (`heddle diff HEAD~1 HEAD`) took the `to_tree`-present branch, which
// in r8 skipped the type-change expansion entirely — so a file→dir or
// regular→symlink change between two commits silently dropped. These run the
// same type-change classes through the committed surface via `state_cell`.
// ---------------------------------------------------------------------------

#[test]
fn state_file_to_dir_type_change_round_trips() {
    state_cell(
        &[normal("conf", "old config\n"), normal("keep.txt", "keep\n")],
        |dir| {
            std::fs::remove_file(dir.join("conf")).unwrap();
            write_entry(dir, &normal("conf/nested.txt", "nested\nvalue\n"));
        },
        &[
            Expect::Present(normal("conf/nested.txt", "nested\nvalue\n")),
            Expect::Present(normal("keep.txt", "keep\n")),
        ],
    );
}

#[test]
fn state_dir_to_file_type_change_round_trips() {
    state_cell(
        &[normal("data/item.txt", "x\ny\n"), normal("keep.txt", "keep\n")],
        |dir| {
            std::fs::remove_file(dir.join("data/item.txt")).unwrap();
            std::fs::remove_dir(dir.join("data")).unwrap();
            write_entry(dir, &normal("data", "now a file\n"));
        },
        &[
            Expect::Present(normal("data", "now a file\n")),
            Expect::Present(normal("keep.txt", "keep\n")),
        ],
    );
}

#[cfg(unix)]
#[test]
fn state_regular_to_symlink_type_change_round_trips() {
    state_cell(
        &[normal("node", "real contents\n"), normal("keep.txt", "keep\n")],
        |dir| {
            std::fs::remove_file(dir.join("node")).unwrap();
            write_entry(dir, &symlink("node", "some/target/path"));
        },
        &[
            Expect::Present(symlink("node", "some/target/path")),
            Expect::Present(normal("keep.txt", "keep\n")),
        ],
    );
}

#[cfg(unix)]
#[test]
fn state_symlink_to_regular_type_change_round_trips() {
    state_cell(
        &[symlink("node", "some/target/path"), normal("keep.txt", "keep\n")],
        |dir| {
            std::fs::remove_file(dir.join("node")).unwrap();
            write_entry(dir, &normal("node", "now real contents\n"));
        },
        &[
            Expect::Present(normal("node", "now real contents\n")),
            Expect::Present(normal("keep.txt", "keep\n")),
        ],
    );
}

// ---------------------------------------------------------------------------
// Matrix — binary content (cid 3319484747). heddle has no git binary delta to
// emit (its blob hashes are not git SHAs), so a binary *content* change is
// rendered as git's `Binary files … differ` marker plus a placeholder
// `index 0000000..0000000` line. That index line is load-bearing: it makes
// `git apply` recognize a binary patch and *refuse the whole patch* ("without
// full index line"). Without it git silently treats the marker as empty and
// applies the rest — leaving the binary content stale while reporting success
// (the false round-trip F5 caught). The fail-loud contract is: a patch that
// touches binary content must be *refused*, never partially applied.
// ---------------------------------------------------------------------------

#[test]
fn binary_modify_emits_marker_and_is_refused() {
    let h = TempDir::new().unwrap();
    heddle(&["init"], Some(h.path())).unwrap();
    write_entry(h.path(), &normal("data.bin", "text\n"));
    write_entry(h.path(), &normal("notes.txt", "keep\n"));
    heddle(&["capture", "-m", "v1"], Some(h.path())).unwrap();
    // Turn data.bin binary (embedded NUL) and make a real text edit too.
    std::fs::write(h.path().join("data.bin"), [0u8, 1, 2, 0, 255]).unwrap();
    write_entry(h.path(), &normal("notes.txt", "edited\n"));

    let patch = heddle(&["diff", "--patch"], Some(h.path())).unwrap();
    // The text edit still renders as a normal hunk.
    assert!(
        patch.contains("notes.txt") && patch.contains("+edited"),
        "the text edit must still render:\n{patch}"
    );
    // The binary change is surfaced as a marker + placeholder index, not
    // dropped and not mangled into a text hunk.
    assert!(
        patch.contains("Binary files a/data.bin and b/data.bin differ"),
        "binary modify must emit git's `Binary files … differ` marker:\n{patch}"
    );
    assert!(
        patch.contains("index 0000000..0000000"),
        "binary marker needs the placeholder index line to force refusal:\n{patch}"
    );
    assert!(
        !patch.contains("--- a/data.bin"),
        "binary file must not be rendered as a text hunk:\n{patch}"
    );
    let json_patch = json_patch_field(h.path());
    assert_eq!(
        json_patch.as_deref(),
        Some(patch.as_str()),
        "JSON `.patch` must equal the `--patch` stdout for the binary case too"
    );

    // The whole patch is refused — git apply will not silently skip the
    // binary block and apply the text edit, which would leave stale content
    // while claiming success.
    apply_refusal_oracle(
        &[normal("data.bin", "text\n"), normal("notes.txt", "keep\n")],
        &patch,
    );
}

/// A binary content change *paired with* a mode change must still refuse:
/// the renderer emits `old mode`/`new mode` + placeholder index + marker, so
/// git apply cannot downgrade it to a mode-only chmod that leaves stale binary
/// content (the precise F5 false round-trip).
#[cfg(unix)]
#[test]
fn binary_modify_with_chmod_is_refused() {
    let h = TempDir::new().unwrap();
    heddle(&["init"], Some(h.path())).unwrap();
    write_entry(h.path(), &normal("data.bin", "text\n"));
    heddle(&["capture", "-m", "v1"], Some(h.path())).unwrap();
    // Change content to binary AND flip the exec bit.
    std::fs::write(h.path().join("data.bin"), [0u8, 9, 8, 0, 7]).unwrap();
    set_mode(&h.path().join("data.bin"), 0o755);

    let patch = heddle(&["diff", "--patch"], Some(h.path())).unwrap();
    assert!(
        patch.contains("old mode 100644") && patch.contains("new mode 100755"),
        "binary+chmod must carry the mode headers:\n{patch}"
    );
    assert!(
        patch.contains("Binary files a/data.bin and b/data.bin differ")
            && patch.contains("index 0000000..0000000"),
        "binary+chmod must still emit the binary marker + index, not a bare chmod:\n{patch}"
    );
    apply_refusal_oracle(&[normal("data.bin", "text\n")], &patch);
}

/// The companion guard: a *pure* chmod on a binary file (content byte-identical,
/// only the mode flips) must NOT be refused. The content-equality short-circuit
/// routes it through the mode-only chmod path (no marker), which git apply
/// accepts and which correctly leaves the binary content untouched. Proves the
/// fail-loud refusal fires on content change, not merely on "the file is binary".
#[cfg(unix)]
#[test]
fn binary_pure_chmod_round_trips() {
    let bytes: &[u8] = &[0u8, 1, 2, 0, 255, 0, 42];
    native_cell(
        &[binary("data.bin", bytes)],
        |dir| set_mode(&dir.join("data.bin"), 0o755),
        &[Expect::Present(binary_exec("data.bin", bytes))],
    );
}

// ---------------------------------------------------------------------------
// Surfaces — plain-Git fast path (no `heddle init`)
// ---------------------------------------------------------------------------

/// Run a plain-Git fast-path cell: the worktree is a plain git repo with
/// no `heddle init`. `heddle diff` reads HEAD via gix. The oracle seeds
/// the same committed state.
fn plain_git_cell(pre: &[Entry], stage: bool, mutate: impl Fn(&Path), expect: &[Expect]) {
    let h = TempDir::new().unwrap();
    git_init(h.path());
    for entry in pre {
        write_entry(h.path(), entry);
    }
    git(h.path(), &["add", "-A"]);
    git(h.path(), &["commit", "-q", "-m", "seed"]);
    mutate(h.path());
    if stage {
        // git status reports an untracked add as "added" only once staged.
        git(h.path(), &["add", "-A"]);
    }

    let patch = heddle(&["diff", "--patch"], Some(h.path())).unwrap();
    assert!(
        !patch.trim().is_empty(),
        "plain-Git cell produced an empty patch"
    );
    // Same all-modes contract on the plain-Git backend: `--output json --stat`
    // and `--name-only` must still carry `.patch` (cid 3321875382), and every
    // mode must agree on the change set.
    assert_modes_consistent(h.path(), &[], &patch);
    apply_oracle(pre, &patch, expect);
}

#[test]
fn plain_git_modify_round_trips() {
    plain_git_cell(
        &[normal("f.txt", "a\nb\nc\n")],
        false,
        |dir| write_entry(dir, &normal("f.txt", "a\nB\nc\n")),
        &[Expect::Present(normal("f.txt", "a\nB\nc\n"))],
    );
}

#[test]
fn plain_git_add_round_trips() {
    plain_git_cell(
        &[normal("anchor.txt", "anchor\n")],
        true,
        |dir| write_entry(dir, &normal("new.txt", "alpha\nbeta\n")),
        &[Expect::Present(normal("new.txt", "alpha\nbeta\n"))],
    );
}

#[test]
fn plain_git_delete_round_trips() {
    plain_git_cell(
        &[normal("doomed.txt", "x\ny\n"), normal("keep.txt", "keep\n")],
        true,
        |dir| std::fs::remove_file(dir.join("doomed.txt")).unwrap(),
        &[
            Expect::Absent("doomed.txt"),
            Expect::Present(normal("keep.txt", "keep\n")),
        ],
    );
}

#[cfg(unix)]
#[test]
fn plain_git_chmod_round_trips() {
    let body = "#!/bin/sh\necho hi\n";
    plain_git_cell(
        &[normal("run.sh", body)],
        true,
        |dir| set_mode(&dir.join("run.sh"), 0o755),
        &[Expect::Present(exec("run.sh", body))],
    );
}

// ---------------------------------------------------------------------------
// Plain-Git both-path coverage (cid 3320033191 / 3320033195). The plain-Git
// fast path is a separate diff-rendering backend from the heddle-backed paths;
// for four rounds, bugs were fixed in one backend and missed in the other.
// These cells run the binary-chmod / type-change / symlink classes — the ones
// the heddle paths already cover via `native_cell` / `state_cell` — through the
// plain-Git backend too, so a sibling-path miss fails CI here, not in review.
// ---------------------------------------------------------------------------

/// cid 3320033191 — a pure chmod on a *binary* file in a plain-Git repo. The
/// old/new blob bytes are identical, so it must render as a mode-only header
/// (`old mode`/`new mode`), NOT a `Binary files … differ` placeholder that
/// `git apply` refuses. The identical-content short-circuit (now shared by
/// every backend via `modified_blob_hunks`) routes it through the chmod path.
#[cfg(unix)]
#[test]
fn plain_git_binary_pure_chmod_round_trips() {
    let bytes: &[u8] = &[0u8, 1, 2, 0, 255, 0, 42];
    plain_git_cell(
        &[binary("data.bin", bytes)],
        true,
        |dir| set_mode(&dir.join("data.bin"), 0o755),
        &[Expect::Present(binary_exec("data.bin", bytes))],
    );
}

/// The binary-chmod patch is header-only on the plain-Git path too: no
/// `Binary files … differ` marker, no `@@` body — just the `old mode`/`new
/// mode` pair. Pins the exact shape so a regression that re-introduces the
/// binary branch for a pure chmod trips here.
#[cfg(unix)]
#[test]
fn plain_git_binary_pure_chmod_emits_mode_only_patch() {
    let h = TempDir::new().unwrap();
    git_init(h.path());
    write_entry(h.path(), &binary("data.bin", &[0u8, 1, 2, 0, 255]));
    git(h.path(), &["add", "-A"]);
    git(h.path(), &["commit", "-q", "-m", "seed"]);
    set_mode(&h.path().join("data.bin"), 0o755);

    let patch = heddle(&["diff", "--patch"], Some(h.path())).unwrap();
    assert!(
        patch.contains("old mode 100644") && patch.contains("new mode 100755"),
        "plain-Git binary chmod must carry `old mode`/`new mode`:\n{patch}"
    );
    assert!(
        !patch.contains("Binary files") && !patch.contains("@@"),
        "pure binary chmod is header-only — no binary marker, no hunk:\n{patch}"
    );
}

#[cfg(unix)]
#[test]
fn plain_git_regular_to_symlink_type_change_round_trips() {
    // regular → symlink in a plain-Git repo must split into delete(100644)
    // + add(120000), matching the heddle path's `expand_type_changes`.
    plain_git_cell(
        &[normal("node", "real contents\n"), normal("keep.txt", "keep\n")],
        false,
        |dir| {
            std::fs::remove_file(dir.join("node")).unwrap();
            write_entry(dir, &symlink("node", "some/target/path"));
        },
        &[
            Expect::Present(symlink("node", "some/target/path")),
            Expect::Present(normal("keep.txt", "keep\n")),
        ],
    );
}

#[cfg(unix)]
#[test]
fn plain_git_symlink_to_regular_type_change_round_trips() {
    plain_git_cell(
        &[symlink("node", "some/target/path"), normal("keep.txt", "keep\n")],
        false,
        |dir| {
            std::fs::remove_file(dir.join("node")).unwrap();
            write_entry(dir, &normal("node", "now real contents\n"));
        },
        &[
            Expect::Present(normal("node", "now real contents\n")),
            Expect::Present(normal("keep.txt", "keep\n")),
        ],
    );
}

/// cid 3320033195 on the plain-Git backend: the symlink target is a real
/// directory. `worktree_side_kind` (used by the plain-Git classifier too)
/// reports `Symlink`, never the `Dir` it points at, so the regular→symlink
/// swap still splits into delete+add.
#[cfg(unix)]
#[test]
fn plain_git_regular_to_symlink_pointing_at_dir_type_change_round_trips() {
    plain_git_cell(
        &[
            normal("node", "real contents\n"),
            normal("realdir/keep.txt", "keep\n"),
        ],
        false,
        |dir| {
            std::fs::remove_file(dir.join("node")).unwrap();
            write_entry(dir, &symlink("node", "realdir"));
        },
        &[
            Expect::Present(symlink("node", "realdir")),
            Expect::Present(normal("realdir/keep.txt", "keep\n")),
        ],
    );
}

#[test]
fn plain_git_file_to_dir_type_change_round_trips() {
    // file → dir on the plain-Git path: the modify downgrades to a deletion
    // of the blocking file; the dir's leaf arrives as a separate untracked
    // `added` entry. Without the downgrade the modify renders a chmod+empty
    // body git apply leaves as a file, blocking the nested add.
    plain_git_cell(
        &[normal("conf", "old config\n"), normal("keep.txt", "keep\n")],
        false,
        |dir| {
            std::fs::remove_file(dir.join("conf")).unwrap();
            write_entry(dir, &normal("conf/nested.txt", "nested\nvalue\n"));
        },
        &[
            Expect::Present(normal("conf/nested.txt", "nested\nvalue\n")),
            Expect::Present(normal("keep.txt", "keep\n")),
        ],
    );
}

#[test]
fn plain_git_dir_to_file_type_change_round_trips() {
    // dir → file: the leaf is deleted (worktree path gone) and the new file
    // is added (untracked). Neither lands in the modified set, so this guards
    // that the plain-Git add/delete ordering still round-trips.
    plain_git_cell(
        &[normal("data/item.txt", "x\ny\n"), normal("keep.txt", "keep\n")],
        false,
        |dir| {
            std::fs::remove_file(dir.join("data/item.txt")).unwrap();
            std::fs::remove_dir(dir.join("data")).unwrap();
            write_entry(dir, &normal("data", "now a file\n"));
        },
        &[
            Expect::Present(normal("data", "now a file\n")),
            Expect::Present(normal("keep.txt", "keep\n")),
        ],
    );
}

/// Unborn HEAD: a fresh `git init` with a staged file and no commit. The
/// only honest diff is "every file is new"; the patch must apply against
/// an (otherwise empty) baseline.
#[test]
fn plain_git_unborn_head_add_round_trips() {
    let h = TempDir::new().unwrap();
    git_init(h.path());
    write_entry(h.path(), &normal("first.txt", "alpha\nbeta\n"));
    git(h.path(), &["add", "first.txt"]);

    let patch = heddle(&["diff", "--patch"], Some(h.path())).unwrap();
    assert!(
        patch.contains("new file mode 100644") && patch.contains("--- /dev/null"),
        "unborn-HEAD add must carry the new-file header:\n{patch}"
    );

    // Apply against a baseline that has the file absent.
    apply_oracle(
        &[normal("anchor.txt", "anchor\n")],
        &patch,
        &[Expect::Present(normal("first.txt", "alpha\nbeta\n"))],
    );
}

/// Plain-Git same-path delete+add: `git rm --cached f` removes `f` from the
/// index (HEAD still has it, the worktree copy becomes untracked), then the
/// untracked `f` is edited. `plain_git_worktree_status` reports `f` as BOTH
/// deleted (index-vs-HEAD) and added (untracked worktree); emitting both an
/// add and a delete patch for one path is a pair `git apply` rejects. The
/// pair must coalesce into a single HEAD→worktree modify. Covers cid 3319049659.
#[test]
fn plain_git_rm_cached_then_edit_coalesces() {
    let h = TempDir::new().unwrap();
    git_init(h.path());
    write_entry(h.path(), &normal("f.txt", "v1\nshared\ntail\n"));
    git(h.path(), &["add", "-A"]);
    git(h.path(), &["commit", "-q", "-m", "seed"]);
    // Drop from the index (HEAD keeps it; worktree copy is now untracked),
    // then edit the untracked copy so its body differs from HEAD.
    git(h.path(), &["rm", "--cached", "-q", "f.txt"]);
    write_entry(h.path(), &normal("f.txt", "v2\nshared\ntail\n"));

    let patch = heddle(&["diff", "--patch"], Some(h.path())).unwrap();
    assert!(
        patch.contains("--- a/f.txt") && patch.contains("+++ b/f.txt"),
        "same-path delete+add must coalesce into a single modify:\n{patch}"
    );
    assert!(
        !patch.contains("/dev/null"),
        "coalesced modify must not emit add/delete `/dev/null` headers:\n{patch}"
    );
    let json_patch = json_patch_field(h.path());
    assert_eq!(
        json_patch.as_deref(),
        Some(patch.as_str()),
        "plain-Git JSON `.patch` must equal `--patch` stdout"
    );
    apply_oracle(
        &[normal("f.txt", "v1\nshared\ntail\n")],
        &patch,
        &[Expect::Present(normal("f.txt", "v2\nshared\ntail\n"))],
    );
}

// ---------------------------------------------------------------------------
// Decoration-trim conformance (cid 3320364905). `trim_trailing_added_decorations`
// drops an added "decoration" line (`#[...]`, `///`, `@`, …) when an identical
// context line follows the inserted block, so the PRETTY diff anchors on the
// existing item instead of showing a duplicated attribute. That trim is a
// display-only nicety, but it used to be baked into the canonical hunk body
// while the `@@` header line-counts were computed *before* trimming — so the
// `--patch`/JSON render emitted one fewer `+` line than its header claimed, and
// `git apply` rejected the patch as corrupt ("corrupt patch at line N") or
// reconstructed the wrong file. The fix keeps the canonical body untrimmed (the
// trim now lives in `print_diff` alone). These cells add a `#[test] fn …` block
// ending in a decoration line immediately before an existing `#[test]`, then
// round-trip through real `git apply` with a content assertion — so re-coupling
// the trim to the patch path fails CI here, on every backend, not in review.
//
// The pre/post pair is shaped so the diff is a faithful 3-line insertion that
// `keep_annotations_with_inserted_items` leaves untouched: the trim is the
// *sole* transform that could desync the body from the header.
// ---------------------------------------------------------------------------

const DECORATION_PRE: &str = "mod m {}\n#[test]\nfn existing() {}\n";
const DECORATION_POST: &str =
    "mod m {}\nfn h() {}\n#[test]\nfn added() {}\n#[test]\nfn existing() {}\n";

#[test]
fn native_added_decoration_before_identical_line_round_trips() {
    native_cell(
        &[normal("tests.rs", DECORATION_PRE)],
        |dir| write_entry(dir, &normal("tests.rs", DECORATION_POST)),
        &[Expect::Present(normal("tests.rs", DECORATION_POST))],
    );
}

#[test]
fn state_added_decoration_before_identical_line_round_trips() {
    state_cell(
        &[normal("tests.rs", DECORATION_PRE)],
        |dir| write_entry(dir, &normal("tests.rs", DECORATION_POST)),
        &[Expect::Present(normal("tests.rs", DECORATION_POST))],
    );
}

#[test]
fn plain_git_added_decoration_before_identical_line_round_trips() {
    plain_git_cell(
        &[normal("tests.rs", DECORATION_PRE)],
        false,
        |dir| write_entry(dir, &normal("tests.rs", DECORATION_POST)),
        &[Expect::Present(normal("tests.rs", DECORATION_POST))],
    );
}

// ---------------------------------------------------------------------------
// Surfaces — trust-visible fast path (adopted repo, branch advanced
// outside heddle). Covers cid 3318629234 (rename+edit must keep its hunk).
// ---------------------------------------------------------------------------

#[test]
fn trust_visible_rename_with_edit_keeps_hunk() {
    let baseline = "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n";
    let edited = "l1\nl2\nCHANGED\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n";

    let h = TempDir::new().unwrap();
    git_init(h.path());
    write_entry(h.path(), &normal("source.txt", baseline));
    git(h.path(), &["add", "-A"]);
    git(h.path(), &["commit", "-q", "-m", "seed"]);
    // heddle adopts at `baseline` — this is the diff baseline.
    heddle(&["adopt"], Some(h.path())).unwrap();
    // Advance the git branch outside heddle so `diff` routes through the
    // trust-visible worktree-status fast path. The advance content is
    // irrelevant — the diff is computed against the adopted baseline.
    write_entry(h.path(), &normal("source.txt", &format!("{baseline}l11\n")));
    git(h.path(), &["add", "-A"]);
    git(h.path(), &["commit", "-q", "-m", "advance"]);
    // Worktree: rename source -> target with a one-line edit, relative to
    // the adopted baseline.
    git(h.path(), &["rm", "-q", "source.txt"]);
    write_entry(h.path(), &normal("target.txt", edited));
    git(h.path(), &["add", "-A"]);

    let patch = heddle(&["diff", "--patch"], Some(h.path())).unwrap();
    assert!(
        patch.contains("rename from source.txt") && patch.contains("rename to target.txt"),
        "trust-visible rename must emit the rename headers:\n{patch}"
    );
    // The regression cid 3318629234 dropped this hunk, rendering a pure
    // rename that silently discarded the edit.
    assert!(
        patch.contains("-l3") && patch.contains("+CHANGED"),
        "trust-visible rename+edit must keep its edit hunk:\n{patch}"
    );
    let json_patch = json_patch_field(h.path());
    assert_eq!(
        json_patch.as_deref(),
        Some(patch.as_str()),
        "trust-visible JSON `.patch` must equal `--patch` stdout"
    );

    // Round-trip against the adopted baseline (source.txt at `baseline`).
    apply_oracle(
        &[normal("source.txt", baseline)],
        &patch,
        &[
            Expect::Absent("source.txt"),
            Expect::Present(normal("target.txt", edited)),
        ],
    );
}

// ---------------------------------------------------------------------------
// Surface — native state-to-state (`heddle diff <from> <to>`)
// ---------------------------------------------------------------------------

#[test]
fn state_to_state_add_round_trips() {
    let h = TempDir::new().unwrap();
    heddle(&["init"], Some(h.path())).unwrap();
    write_entry(h.path(), &normal("base.txt", "base\n"));
    heddle(&["capture", "-m", "v1"], Some(h.path())).unwrap();
    write_entry(h.path(), &normal("fresh.txt", "fresh\n"));
    heddle(&["capture", "-m", "v2"], Some(h.path())).unwrap();

    let patch = heddle(&["diff", "HEAD~1", "HEAD", "--patch"], Some(h.path())).unwrap();
    assert!(
        patch.contains("new file mode 100644") && patch.contains("+fresh"),
        "state-to-state add must carry the new-file header + body:\n{patch}"
    );
    apply_oracle(
        &[normal("base.txt", "base\n")],
        &patch,
        &[Expect::Present(normal("fresh.txt", "fresh\n"))],
    );
}

// ---------------------------------------------------------------------------
// Surface — embedded diff payload (`merge --with-diff --output json`). Covers
// cid 3319484733: `compute_state_diff`/`compute_tree_diff` returned a
// `DiffOutput` whose `.patch` defaulted to `None`, so structured consumers of
// the merge preview saw hunks in `.changes` but no applicable patch text.
// ---------------------------------------------------------------------------

#[test]
fn merge_with_diff_json_carries_patch() {
    let h = TempDir::new().unwrap();
    heddle(&["init"], Some(h.path())).unwrap();
    write_entry(h.path(), &normal("base.txt", "base\n"));
    heddle(&["capture", "-m", "v1"], Some(h.path())).unwrap();

    // Advance a thread ahead of main (a clean fast-forward), so the preview
    // diff is main-state → merged-result and reconstructs the thread tree.
    heddle(&["thread", "create", "feature"], Some(h.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(h.path())).unwrap();
    write_entry(h.path(), &normal("base.txt", "base\nfeature\n"));
    write_entry(h.path(), &normal("new.txt", "new\n"));
    heddle(&["capture", "-m", "v2"], Some(h.path())).unwrap();
    heddle(&["thread", "switch", "main"], Some(h.path())).unwrap();

    let out = heddle_output(
        &[
            "--output",
            "json",
            "merge",
            "feature",
            "--preview",
            "--with-diff",
        ],
        Some(h.path()),
    )
    .expect("merge --with-diff should run");
    assert!(
        out.status.success(),
        "merge --with-diff --output json should succeed; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let parsed: Value = serde_json::from_slice(&out.stdout).expect("merge output should be JSON");
    let patch = parsed["diff"]["patch"]
        .as_str()
        .unwrap_or_else(|| panic!("merge preview `.diff.patch` must be populated, not null: {parsed}"));
    assert!(
        patch.contains("+feature") && patch.contains("new.txt"),
        "embedded patch must carry the incoming hunks:\n{patch}"
    );

    // The embedded patch is a real, applicable patch: seeded at main's state
    // it reconstructs the merged (fast-forwarded) tree.
    apply_oracle(
        &[normal("base.txt", "base\n")],
        patch,
        &[
            Expect::Present(normal("base.txt", "base\nfeature\n")),
            Expect::Present(normal("new.txt", "new\n")),
        ],
    );
}

// ---------------------------------------------------------------------------
// Proptest — random tree + random edits through the same oracle
// ---------------------------------------------------------------------------

// Includes special-character names (space, quote, non-ASCII) so random
// runs exercise git's header quoting. Limited to heddle-capturable names
// (`validate_name` rejects control bytes and `\`), and none is a path
// prefix of another, keeping every generated tree filesystem-consistent.
// (Control-char / backslash names + file↔dir type changes get their own
// dedicated cells.)
const NAME_POOL: &[&str] = &[
    "a.txt",
    "b.txt",
    "sub/c.txt",
    "d.txt",
    "g h.txt",
    "i\"j.txt",
    "mün\u{f6}.txt",
];

/// Text content with varied shapes: empty, single line w/o eol, multi
/// line w/ or w/o trailing newline. ASCII-only so heddle never treats it
/// as binary.
fn content_strategy() -> impl Strategy<Value = String> {
    (
        proptest::collection::vec("[a-z]{1,6}", 0..5),
        any::<bool>(),
    )
        .prop_map(|(lines, trailing)| {
            let mut joined = lines.join("\n");
            if !joined.is_empty() && trailing {
                joined.push('\n');
            }
            joined
        })
}

fn tree_strategy() -> impl Strategy<Value = BTreeMap<String, String>> {
    proptest::collection::btree_map(
        proptest::sample::select(NAME_POOL).prop_map(|name| name.to_string()),
        content_strategy(),
        1..=4,
    )
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]

    /// For any pre/post pair of text trees, `heddle diff --patch` must
    /// produce a patch that `git apply` accepts and that reconstructs the
    /// post tree exactly. This is the layer that catches cells a
    /// hand-enumerated matrix misses (the no-eol / empty / nested /
    /// rename-collision interactions).
    #[test]
    fn diff_patch_round_trips_random_tree(
        pre in tree_strategy(),
        post in tree_strategy(),
    ) {
        prop_assume!(pre != post);

        let pre_entries: Vec<Entry> =
            pre.iter().map(|(p, c)| normal(p, c)).collect();

        let h = TempDir::new().unwrap();
        heddle(&["init"], Some(h.path())).unwrap();
        for entry in &pre_entries {
            write_entry(h.path(), entry);
        }
        heddle(&["capture", "-m", "v1"], Some(h.path())).unwrap();

        // Mutate worktree to `post`: delete dropped paths, (re)write the rest.
        for name in pre.keys() {
            if !post.contains_key(name) {
                std::fs::remove_file(h.path().join(name)).ok();
            }
        }
        for (name, content) in &post {
            write_entry(h.path(), &normal(name, content));
        }

        let patch = heddle(&["diff", "--patch"], Some(h.path())).unwrap();
        prop_assert!(
            !patch.trim().is_empty(),
            "non-equal trees must produce a patch; pre={pre:?} post={post:?}"
        );

        // Every output mode must agree on the change set and carry `.patch`
        // in every JSON render — the random-tree layer of the all-modes
        // invariant (cids 3321875377 + 3321875382).
        assert_modes_consistent(h.path(), &[], &patch);

        // Oracle: seed `pre`, apply, assert the worktree equals `post`.
        let g = TempDir::new().unwrap();
        git_init(g.path());
        for entry in &pre_entries {
            write_entry(g.path(), entry);
        }
        git(g.path(), &["add", "-A"]);
        git(g.path(), &["commit", "-q", "-m", "seed"]);

        let check = pipe_git(g.path(), &["apply", "--check"], &patch);
        prop_assert!(
            check.status.success(),
            "git apply --check failed: {}\npatch=\n{patch}",
            String::from_utf8_lossy(&check.stderr)
        );
        let applied = pipe_git(g.path(), &["apply"], &patch);
        prop_assert!(
            applied.status.success(),
            "git apply failed: {}\npatch=\n{patch}",
            String::from_utf8_lossy(&applied.stderr)
        );

        for (name, content) in &post {
            let got = std::fs::read(g.path().join(name)).unwrap();
            prop_assert_eq!(
                &got,
                &content.as_bytes().to_vec(),
                "content mismatch for {} after apply", name
            );
        }
        for name in pre.keys() {
            if !post.contains_key(name) {
                prop_assert!(
                    !g.path().join(name).exists(),
                    "deleted path {} still present after apply", name
                );
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 16, ..ProptestConfig::default() })]

    /// A tracked file replaced by a directory (or the reverse) must
    /// round-trip: the type change emits a deletion of the blocking path
    /// plus the new tree, and `git apply` reconstructs it. Randomizes the
    /// direction and both file bodies — the randomized layer for the
    /// hand-enumerated `file_to_dir_type_change_round_trips` cell
    /// (cid 3319049665).
    #[test]
    fn type_change_round_trips(
        file_to_dir in any::<bool>(),
        body_a in "[a-z]{1,8}\n",
        body_b in "[a-z]{1,8}\n",
    ) {
        // `pre` is the captured baseline; `post_files` is what the worktree
        // (and the applied oracle tree) must look like afterwards.
        let (pre, post_files): (Vec<Entry>, Vec<Entry>) = if file_to_dir {
            (
                vec![normal("node", &body_a), normal("anchor.txt", "anchor\n")],
                vec![normal("node/leaf.txt", &body_b)],
            )
        } else {
            (
                vec![normal("node/leaf.txt", &body_a), normal("anchor.txt", "anchor\n")],
                vec![normal("node", &body_b)],
            )
        };

        let h = TempDir::new().unwrap();
        heddle(&["init"], Some(h.path())).unwrap();
        for entry in &pre {
            write_entry(h.path(), entry);
        }
        heddle(&["capture", "-m", "v1"], Some(h.path())).unwrap();

        // Swap the type in the worktree.
        if file_to_dir {
            std::fs::remove_file(h.path().join("node")).unwrap();
        } else {
            std::fs::remove_file(h.path().join("node/leaf.txt")).unwrap();
            std::fs::remove_dir(h.path().join("node")).unwrap();
        }
        for entry in &post_files {
            write_entry(h.path(), entry);
        }

        let patch = heddle(&["diff", "--patch"], Some(h.path())).unwrap();
        prop_assert!(
            !patch.trim().is_empty(),
            "type change must produce a patch; file_to_dir={file_to_dir}"
        );
        let json_patch = json_patch_field(h.path());
        prop_assert_eq!(json_patch.as_deref(), Some(patch.as_str()));

        // Oracle: seed `pre`, apply, assert the post tree materializes.
        let g = TempDir::new().unwrap();
        git_init(g.path());
        for entry in &pre {
            write_entry(g.path(), entry);
        }
        git(g.path(), &["add", "-A"]);
        git(g.path(), &["commit", "-q", "-m", "seed"]);

        let check = pipe_git(g.path(), &["apply", "--check"], &patch);
        prop_assert!(
            check.status.success(),
            "git apply --check failed: {}\npatch=\n{patch}",
            String::from_utf8_lossy(&check.stderr)
        );
        let applied = pipe_git(g.path(), &["apply"], &patch);
        prop_assert!(
            applied.status.success(),
            "git apply failed: {}\npatch=\n{patch}",
            String::from_utf8_lossy(&applied.stderr)
        );
        for entry in &post_files {
            let got = std::fs::read(g.path().join(&entry.path)).unwrap();
            prop_assert_eq!(
                &got, &entry.body,
                "content mismatch for {} after apply", entry.path
            );
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 16, ..ProptestConfig::default() })]

    /// A regular file replaced by a symlink (or the reverse) must round-trip
    /// as delete(old type) + add(new type), never an `old mode`/`new mode`
    /// flip that `git apply` rejects (cid 3319484727). Randomizes direction,
    /// the file body, and the link target — the randomized layer for the
    /// hand-enumerated `regular_to_symlink_type_change_round_trips` cells.
    #[cfg(unix)]
    #[test]
    fn symlink_type_change_round_trips(
        file_to_link in any::<bool>(),
        body in "[a-z]{1,12}\n",
        target in "[a-z][a-z/]{0,18}[a-z]",
    ) {
        let (pre, post): (Entry, Entry) = if file_to_link {
            (normal("node", &body), symlink("node", &target))
        } else {
            (symlink("node", &target), normal("node", &body))
        };

        let h = TempDir::new().unwrap();
        heddle(&["init"], Some(h.path())).unwrap();
        write_entry(h.path(), &pre);
        write_entry(h.path(), &normal("anchor.txt", "anchor\n"));
        heddle(&["capture", "-m", "v1"], Some(h.path())).unwrap();

        std::fs::remove_file(h.path().join("node")).unwrap();
        write_entry(h.path(), &post);

        let patch = heddle(&["diff", "--patch"], Some(h.path())).unwrap();
        prop_assert!(
            !patch.trim().is_empty(),
            "type change must produce a patch; file_to_link={file_to_link}"
        );
        let json_patch = json_patch_field(h.path());
        prop_assert_eq!(json_patch.as_deref(), Some(patch.as_str()));

        // Oracle: seed `pre`, apply, assert the post type + content materializes.
        let g = TempDir::new().unwrap();
        git_init(g.path());
        write_entry(g.path(), &pre);
        write_entry(g.path(), &normal("anchor.txt", "anchor\n"));
        git(g.path(), &["add", "-A"]);
        git(g.path(), &["commit", "-q", "-m", "seed"]);

        let check = pipe_git(g.path(), &["apply", "--check"], &patch);
        prop_assert!(
            check.status.success(),
            "git apply --check failed: {}\npatch=\n{patch}",
            String::from_utf8_lossy(&check.stderr)
        );
        let applied = pipe_git(g.path(), &["apply"], &patch);
        prop_assert!(
            applied.status.success(),
            "git apply failed: {}\npatch=\n{patch}",
            String::from_utf8_lossy(&applied.stderr)
        );

        let meta = std::fs::symlink_metadata(g.path().join("node")).unwrap();
        if file_to_link {
            prop_assert!(
                meta.file_type().is_symlink(),
                "node should be a symlink after apply"
            );
            let link = std::fs::read_link(g.path().join("node")).unwrap();
            let link = link.to_string_lossy();
            prop_assert_eq!(link.as_bytes(), target.as_bytes());
        } else {
            prop_assert!(
                !meta.file_type().is_symlink(),
                "node should be a regular file after apply"
            );
            prop_assert_eq!(
                std::fs::read(g.path().join("node")).unwrap(),
                body.as_bytes().to_vec()
            );
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 16, ..ProptestConfig::default() })]

    /// The plain-Git backend (no `heddle init`) must split a regular↔symlink
    /// swap into delete+add exactly like the heddle backend does, never the
    /// cross-type `old mode`/`new mode` flip git apply rejects. This is the
    /// random both-path guard for the plain-Git type-change routing
    /// (`push_plain_git_modified`): the heddle surface gets this coverage from
    /// `symlink_type_change_round_trips`, so the plain-Git surface gets the
    /// mirror here rather than waiting for an r11 Codex review (cid 3320033195).
    #[cfg(unix)]
    #[test]
    fn plain_git_symlink_type_change_round_trips(
        file_to_link in any::<bool>(),
        body in "[a-z]{1,12}\n",
        target in "[a-z][a-z/]{0,18}[a-z]",
    ) {
        let (pre, post): (Entry, Entry) = if file_to_link {
            (normal("node", &body), symlink("node", &target))
        } else {
            (symlink("node", &target), normal("node", &body))
        };

        let h = TempDir::new().unwrap();
        git_init(h.path());
        write_entry(h.path(), &pre);
        write_entry(h.path(), &normal("anchor.txt", "anchor\n"));
        git(h.path(), &["add", "-A"]);
        git(h.path(), &["commit", "-q", "-m", "seed"]);

        std::fs::remove_file(h.path().join("node")).unwrap();
        write_entry(h.path(), &post);

        let patch = heddle(&["diff", "--patch"], Some(h.path())).unwrap();
        prop_assert!(
            !patch.trim().is_empty(),
            "plain-Git type change must produce a patch; file_to_link={file_to_link}"
        );
        let json_patch = json_patch_field(h.path());
        prop_assert_eq!(json_patch.as_deref(), Some(patch.as_str()));

        // Oracle: seed `pre`, apply, assert the post type + content materializes.
        let g = TempDir::new().unwrap();
        git_init(g.path());
        write_entry(g.path(), &pre);
        write_entry(g.path(), &normal("anchor.txt", "anchor\n"));
        git(g.path(), &["add", "-A"]);
        git(g.path(), &["commit", "-q", "-m", "seed"]);

        let check = pipe_git(g.path(), &["apply", "--check"], &patch);
        prop_assert!(
            check.status.success(),
            "git apply --check failed: {}\npatch=\n{patch}",
            String::from_utf8_lossy(&check.stderr)
        );
        let applied = pipe_git(g.path(), &["apply"], &patch);
        prop_assert!(
            applied.status.success(),
            "git apply failed: {}\npatch=\n{patch}",
            String::from_utf8_lossy(&applied.stderr)
        );

        let meta = std::fs::symlink_metadata(g.path().join("node")).unwrap();
        if file_to_link {
            prop_assert!(
                meta.file_type().is_symlink(),
                "node should be a symlink after apply"
            );
            let link = std::fs::read_link(g.path().join("node")).unwrap();
            let link = link.to_string_lossy();
            prop_assert_eq!(link.as_bytes(), target.as_bytes());
        } else {
            prop_assert!(
                !meta.file_type().is_symlink(),
                "node should be a regular file after apply"
            );
            prop_assert_eq!(
                std::fs::read(g.path().join("node")).unwrap(),
                body.as_bytes().to_vec()
            );
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 16, ..ProptestConfig::default() })]

    /// A delete + add at *different* paths whose bytes are identical scores as
    /// a rename (similarity 1.0); when the two sides cross git's regular↔symlink
    /// type boundary the rename-collapse must refuse to merge them, leaving a
    /// delete + add `git apply` accepts — never a `rename from/to` carrying a
    /// mismatched `old mode`/`new mode` (cid 3320838479). The committed-tree
    /// surface stores a symlink's blob as its target bytes, so setting the
    /// regular side's content equal to the link target makes every generated
    /// pair a genuine rename candidate. Randomizes the direction and the shared
    /// bytes — the randomized layer for the hand-enumerated cross-type
    /// rename-candidate cells.
    #[cfg(unix)]
    #[test]
    fn cross_type_rename_candidate_stays_split(
        regular_deleted in any::<bool>(),
        shared in "[a-z][a-z/]{0,18}[a-z]",
    ) {
        // `shared` doubles as the symlink target and the regular file's
        // content, so the two blobs are byte-identical (similarity 1.0).
        let (pre, post): (Entry, Entry) = if regular_deleted {
            (normal("mover", &shared), symlink("landed", &shared))
        } else {
            (symlink("mover", &shared), normal("landed", &shared))
        };

        let h = TempDir::new().unwrap();
        heddle(&["init"], Some(h.path())).unwrap();
        write_entry(h.path(), &pre);
        write_entry(h.path(), &normal("anchor.txt", "anchor\n"));
        heddle(&["capture", "-m", "v1"], Some(h.path())).unwrap();
        std::fs::remove_file(h.path().join("mover")).unwrap();
        write_entry(h.path(), &post);
        heddle(&["capture", "-m", "v2"], Some(h.path())).unwrap();

        let patch = heddle(&["diff", "HEAD~1", "HEAD", "--patch"], Some(h.path())).unwrap();
        prop_assert!(!patch.trim().is_empty());
        // The cross-type pair must never become a rename.
        prop_assert!(
            !patch.contains("rename from"),
            "cross-type move collapsed into a rename:\n{patch}"
        );
        let json_patch = json_diff_patch_field(h.path(), &["HEAD~1", "HEAD"]);
        prop_assert_eq!(json_patch.as_deref(), Some(patch.as_str()));

        // Oracle: seed `pre`, apply, assert the post type + content materializes.
        let g = TempDir::new().unwrap();
        git_init(g.path());
        write_entry(g.path(), &pre);
        write_entry(g.path(), &normal("anchor.txt", "anchor\n"));
        git(g.path(), &["add", "-A"]);
        git(g.path(), &["commit", "-q", "-m", "seed"]);

        let check = pipe_git(g.path(), &["apply", "--check"], &patch);
        prop_assert!(
            check.status.success(),
            "git apply --check failed: {}\npatch=\n{patch}",
            String::from_utf8_lossy(&check.stderr)
        );
        let applied = pipe_git(g.path(), &["apply"], &patch);
        prop_assert!(
            applied.status.success(),
            "git apply failed: {}\npatch=\n{patch}",
            String::from_utf8_lossy(&applied.stderr)
        );

        prop_assert!(
            !g.path().join("mover").exists(),
            "the moved-from path should be gone after apply"
        );
        let meta = std::fs::symlink_metadata(g.path().join("landed")).unwrap();
        if regular_deleted {
            prop_assert!(
                meta.file_type().is_symlink(),
                "landed should be a symlink after apply"
            );
            let link = std::fs::read_link(g.path().join("landed")).unwrap();
            let link = link.to_string_lossy();
            prop_assert_eq!(link.as_bytes(), shared.as_bytes());
        } else {
            prop_assert!(
                !meta.file_type().is_symlink(),
                "landed should be a regular file after apply"
            );
            prop_assert_eq!(
                std::fs::read(g.path().join("landed")).unwrap(),
                shared.as_bytes().to_vec()
            );
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]

    /// The committed-tree surface (`heddle diff HEAD~1 HEAD`) must round-trip
    /// the same random text-tree edits as the worktree surface. This is the
    /// `to_tree`-present branch that silently dropped changes in r8
    /// (cid 3319484717); proptesting it keeps the committed and worktree
    /// surfaces in lockstep instead of catching the gap one Codex round later.
    #[test]
    fn state_diff_round_trips_random_tree(
        pre in tree_strategy(),
        post in tree_strategy(),
    ) {
        prop_assume!(pre != post);

        let pre_entries: Vec<Entry> =
            pre.iter().map(|(p, c)| normal(p, c)).collect();

        let h = TempDir::new().unwrap();
        heddle(&["init"], Some(h.path())).unwrap();
        for entry in &pre_entries {
            write_entry(h.path(), entry);
        }
        heddle(&["capture", "-m", "v1"], Some(h.path())).unwrap();

        // Mutate worktree to `post`, then commit it as `v2` so the diff is
        // computed state-to-state rather than against the worktree.
        for name in pre.keys() {
            if !post.contains_key(name) {
                std::fs::remove_file(h.path().join(name)).ok();
            }
        }
        for (name, content) in &post {
            write_entry(h.path(), &normal(name, content));
        }
        heddle(&["capture", "-m", "v2"], Some(h.path())).unwrap();

        let patch = heddle(&["diff", "HEAD~1", "HEAD", "--patch"], Some(h.path())).unwrap();
        prop_assert!(
            !patch.trim().is_empty(),
            "non-equal trees must produce a patch; pre={pre:?} post={post:?}"
        );

        let json_patch = json_diff_patch_field(h.path(), &["HEAD~1", "HEAD"]);
        prop_assert_eq!(json_patch.as_deref(), Some(patch.as_str()));

        // Oracle: seed `pre`, apply, assert the worktree equals `post`.
        let g = TempDir::new().unwrap();
        git_init(g.path());
        for entry in &pre_entries {
            write_entry(g.path(), entry);
        }
        git(g.path(), &["add", "-A"]);
        git(g.path(), &["commit", "-q", "-m", "seed"]);

        let check = pipe_git(g.path(), &["apply", "--check"], &patch);
        prop_assert!(
            check.status.success(),
            "git apply --check failed: {}\npatch=\n{patch}",
            String::from_utf8_lossy(&check.stderr)
        );
        let applied = pipe_git(g.path(), &["apply"], &patch);
        prop_assert!(
            applied.status.success(),
            "git apply failed: {}\npatch=\n{patch}",
            String::from_utf8_lossy(&applied.stderr)
        );

        for (name, content) in &post {
            let got = std::fs::read(g.path().join(name)).unwrap();
            prop_assert_eq!(
                &got,
                &content.as_bytes().to_vec(),
                "content mismatch for {} after apply", name
            );
        }
        for name in pre.keys() {
            if !post.contains_key(name) {
                prop_assert!(
                    !g.path().join(name).exists(),
                    "deleted path {} still present after apply", name
                );
            }
        }
    }
}
