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
                let target = String::from_utf8(entry.body.clone()).unwrap();
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
            assert_eq!(
                target.to_string_lossy().as_bytes(),
                entry.body.as_slice(),
                "`{}` symlink target mismatch",
                entry.path
            );
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

// ---------------------------------------------------------------------------
// Native-path cell runner (heddle init + capture + worktree mutate)
// ---------------------------------------------------------------------------

fn json_patch_field(cwd: &Path) -> Option<String> {
    let out = heddle_output(&["--output", "json", "diff"], Some(cwd)).expect("heddle json diff");
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

    // JSON `.patch` is the same render as `--patch` and must be present
    // even without the flag — structured consumers rely on it.
    let json_patch = json_patch_field(h.path());
    assert_eq!(
        json_patch.as_deref(),
        Some(patch.as_str()),
        "JSON `.patch` field must equal the `--patch` stdout"
    );

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
// Matrix — binary content: heddle refuses, must not corrupt the patch
// ---------------------------------------------------------------------------

#[test]
fn binary_modify_is_refused_not_emitted() {
    let h = TempDir::new().unwrap();
    heddle(&["init"], Some(h.path())).unwrap();
    write_entry(h.path(), &normal("data.bin", "text\n"));
    write_entry(h.path(), &normal("notes.txt", "keep\n"));
    heddle(&["capture", "-m", "v1"], Some(h.path())).unwrap();
    // Turn data.bin binary (embedded NUL) and make a real text edit too.
    std::fs::write(h.path().join("data.bin"), [0u8, 1, 2, 0, 255]).unwrap();
    write_entry(h.path(), &normal("notes.txt", "edited\n"));

    let patch = heddle(&["diff", "--patch"], Some(h.path())).unwrap();
    // The text edit still renders; the binary file is omitted rather than
    // emitted as a corrupt text hunk.
    assert!(
        patch.contains("notes.txt") && patch.contains("+edited"),
        "the text edit must still render:\n{patch}"
    );
    assert!(
        !patch.contains("data.bin"),
        "binary file must be refused, not emitted as a hunk:\n{patch}"
    );

    // The text-only remainder must still round-trip.
    apply_oracle(
        &[normal("notes.txt", "keep\n")],
        &patch,
        &[Expect::Present(normal("notes.txt", "edited\n"))],
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
    let json_patch = json_patch_field(h.path());
    assert_eq!(
        json_patch.as_deref(),
        Some(patch.as_str()),
        "plain-Git JSON `.patch` must equal `--patch` stdout"
    );
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

        // JSON `.patch` must mirror the `--patch` render.
        let json_patch = json_patch_field(h.path());
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
