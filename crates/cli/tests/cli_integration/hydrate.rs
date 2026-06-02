// SPDX-License-Identifier: Apache-2.0
//! End-to-end coverage of `heddle start --hydrate` + the atomic start
//! rollback (heddle#302 / heddle#356).
//!
//! `--hydrate` symlinks the origin checkout's top-level ignored
//! dependency directories (`node_modules`, `.venv`, …) into a fresh
//! isolated checkout so it's immediately buildable. The whole `start`
//! write-path runs as one atomic transaction on the heddle#330 primitive,
//! so a failure mid-materialize (or mid-hydrate) rewinds every applied
//! effect — with precise directory + symlink rewind — back to the exact
//! pre-start state. These tests run the built `heddle` binary inside temp
//! dirs and inspect what `start` leaves on disk.

use super::*;

/// Seed a project whose dependencies live in ignored directories.
/// `.heddleignore` is itself tracked (no rule covers it), so it
/// materializes into every thread checkout and the same ignore rules
/// apply there.
fn init_deps_in_ignored_dir_project(dir: &std::path::Path) {
    std::fs::write(dir.join(".heddleignore"), "node_modules/\n.venv/\n").unwrap();
    // A tracked source file so the checkout has real captured content.
    std::fs::write(dir.join("index.ts"), "export const x = 1;\n").unwrap();

    // Populated ignored dependency dirs in the origin.
    let node_modules = dir.join("node_modules");
    std::fs::create_dir_all(node_modules.join("left-pad")).unwrap();
    std::fs::write(
        node_modules.join("left-pad").join("index.js"),
        "module.exports = () => {};\n",
    )
    .unwrap();

    let venv = dir.join(".venv");
    std::fs::create_dir_all(venv.join("bin")).unwrap();
    std::fs::write(venv.join("bin").join("python"), "#!/bin/sh\n").unwrap();
}

/// Set up a git-overlay heddle repo whose dependency dirs are ignored
/// ONLY via `.gitignore` (no `.heddleignore` anywhere) — the common
/// drop-in git-overlay setup. The origin's effective ignore set includes
/// `.gitignore` because the repo is in git-overlay mode; the isolated
/// checkout reopened as a native heddle repo does NOT read `.gitignore`.
fn init_gitignore_only_overlay_project(dir: &std::path::Path) {
    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("git should run");
        assert!(status.success(), "git {args:?} should succeed");
    };
    git(&["init"]);
    git(&["config", "user.name", "Heddle Test"]);
    git(&["config", "user.email", "heddle@example.com"]);
    git(&["checkout", "-b", "main"]);

    // Deps ignored ONLY via .gitignore — deliberately no .heddleignore.
    std::fs::write(dir.join(".gitignore"), "node_modules/\n.venv/\n").unwrap();
    std::fs::write(dir.join("index.ts"), "export const x = 1;\n").unwrap();

    let node_modules = dir.join("node_modules");
    std::fs::create_dir_all(node_modules.join("left-pad")).unwrap();
    std::fs::write(
        node_modules.join("left-pad").join("index.js"),
        "module.exports = () => {};\n",
    )
    .unwrap();
}

#[test]
fn hydrate_preserves_gitignore_only_ignores_in_isolated_checkout() {
    // P1 (cid 3327140634): in a git-overlay repo that ignores deps ONLY
    // via `.gitignore`, the isolated checkout is reopened as a native
    // heddle repo whose ignore resolution reads `.heddleignore`, not
    // `.gitignore`. hydrate must materialize the ignore rule into the
    // checkout's native source so the symlinked dep dirs stay ignored —
    // otherwise they surface as added symlinks and capture fails on the
    // absolute, out-of-checkout link target.
    let temp = TempDir::new().unwrap();
    init_gitignore_only_overlay_project(temp.path());
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(&["capture", "-m", "main"], Some(temp.path())).unwrap();

    // A git-overlay repo refuses to start a thread inside itself, so the
    // isolated checkout lives in a sibling temp dir.
    let checkout_root = TempDir::new().unwrap();
    let thread_path = checkout_root.path().join("iso");
    heddle(
        &[
            "start",
            "iso",
            "--path",
            thread_path.to_str().unwrap(),
            "--hydrate",
        ],
        Some(temp.path()),
    )
    .expect("start --hydrate should succeed");

    // The dep dir is hydrated as a symlink...
    let linked = thread_path.join("node_modules");
    assert!(
        std::fs::symlink_metadata(&linked)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false),
        "node_modules should be hydrated as a symlink"
    );

    // ...and IGNORED in the isolated checkout even though the origin
    // expressed the rule only via `.gitignore`: `status` from the
    // checkout must not surface it as added worktree content.
    let status = heddle(&["status"], Some(&thread_path))
        .expect("status should run from the hydrated checkout");
    assert!(
        !status.contains("node_modules"),
        "hydrated node_modules must stay ignored in a .gitignore-only overlay; got:\n{status}"
    );

    // Capture must not choke on the absolute, out-of-checkout link
    // target — the ignored link is pruned before capture follows it.
    heddle(&["capture", "-m", "iso work"], Some(&thread_path))
        .expect("capture in the hydrated checkout must succeed");
}

#[test]
fn hydrate_does_not_dirty_a_tracked_heddleignore() {
    // heddle#356 cid 3333881577: a git-overlay repo that tracks a root
    // `.heddleignore` (covering unrelated paths) while ignoring deps via
    // `.gitignore`. The isolated checkout materializes that TRACKED
    // `.heddleignore`, so `existing` is `Some`. hydrate must record the
    // dep-ignore rule in the worktree-local, never-captured exclude file —
    // NOT by appending to the tracked `.heddleignore` — so a successful
    // `start --hydrate` leaves the checkout's tracked tree clean.
    let temp = TempDir::new().unwrap();
    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(temp.path())
            .status()
            .expect("git should run");
        assert!(status.success(), "git {args:?} should succeed");
    };
    git(&["init"]);
    git(&["config", "user.name", "Heddle Test"]);
    git(&["config", "user.email", "heddle@example.com"]);
    git(&["checkout", "-b", "main"]);

    // Deps ignored via `.gitignore`; a TRACKED `.heddleignore` with an
    // unrelated rule that does NOT cover the deps.
    std::fs::write(temp.path().join(".gitignore"), "node_modules/\n").unwrap();
    std::fs::write(temp.path().join(".heddleignore"), "*.log\n").unwrap();
    std::fs::write(temp.path().join("index.ts"), "export const x = 1;\n").unwrap();
    let node_modules = temp.path().join("node_modules");
    std::fs::create_dir_all(node_modules.join("left-pad")).unwrap();
    std::fs::write(
        node_modules.join("left-pad").join("index.js"),
        "module.exports = () => {};\n",
    )
    .unwrap();

    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(&["capture", "-m", "main"], Some(temp.path())).unwrap();

    let checkout_root = TempDir::new().unwrap();
    let thread_path = checkout_root.path().join("iso");
    heddle(
        &[
            "start",
            "iso",
            "--path",
            thread_path.to_str().unwrap(),
            "--hydrate",
        ],
        Some(temp.path()),
    )
    .expect("start --hydrate should succeed");

    // node_modules is hydrated as a symlink...
    assert!(
        std::fs::symlink_metadata(thread_path.join("node_modules"))
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false),
        "node_modules should be hydrated as a symlink"
    );

    // ...and the TRACKED `.heddleignore` is byte-for-byte UNCHANGED (pre-fix
    // hydrate appended `node_modules/` to it, dirtying the tracked tree).
    let checkout_ignore = std::fs::read_to_string(thread_path.join(".heddleignore")).unwrap();
    assert_eq!(
        checkout_ignore, "*.log\n",
        "hydrate must not modify the tracked .heddleignore"
    );

    // The dep-ignore rule landed in the worktree-local, never-captured exclude.
    let exclude = std::fs::read_to_string(
        thread_path.join(".heddle").join("info").join("exclude"),
    )
    .expect("hydrate should write the worktree-local exclude");
    assert!(
        exclude.contains("node_modules"),
        "the dep-ignore rule must live in the worktree-local exclude; got:\n{exclude}"
    );

    // `status` from the checkout is clean: the dep stays ignored, and the
    // tracked `.heddleignore` is not reported as modified.
    let status = heddle(&["status"], Some(&thread_path))
        .expect("status should run from the hydrated checkout");
    assert!(
        !status.contains("node_modules"),
        "hydrated node_modules must stay ignored in the checkout; got:\n{status}"
    );

    // Capture must not choke on the absolute, out-of-checkout link target.
    heddle(&["capture", "-m", "iso work"], Some(&thread_path))
        .expect("capture in the hydrated checkout must succeed");
}

#[test]
fn hydrate_symlinks_ignored_dep_dirs_into_checkout() {
    let temp = TempDir::new().unwrap();
    init_deps_in_ignored_dir_project(temp.path());
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(&["capture", "-m", "main"], Some(temp.path())).unwrap();

    let thread_path = temp.path().join("iso");
    heddle(
        &[
            "start",
            "iso",
            "--path",
            thread_path.to_str().unwrap(),
            "--hydrate",
        ],
        Some(temp.path()),
    )
    .expect("start --hydrate should succeed");

    // node_modules is a symlink in the checkout, pointing at the origin.
    let linked = thread_path.join("node_modules");
    let meta = std::fs::symlink_metadata(&linked)
        .unwrap_or_else(|e| panic!("expected node_modules symlink at {}: {e}", linked.display()));
    assert!(
        meta.file_type().is_symlink(),
        "node_modules must be a symlink, not a real dir"
    );
    let target = std::fs::read_link(&linked).unwrap();
    assert!(
        target.is_absolute(),
        "hydrate link target should be absolute, got {}",
        target.display()
    );

    // Dependency content is reachable through the link — the whole point
    // is that the checkout is buildable without reinstalling.
    let dep_file = linked.join("left-pad").join("index.js");
    assert!(
        dep_file.is_file(),
        "dependency file must be reachable through the link: {}",
        dep_file.display()
    );

    // .venv is hydrated too (multiple ignored dep dirs).
    assert!(
        std::fs::symlink_metadata(thread_path.join(".venv"))
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false),
        ".venv should also be hydrated"
    );

    // Tracked source still materializes as a real file, not a link.
    let src = thread_path.join("index.ts");
    assert!(src.is_file());
    assert!(
        !std::fs::symlink_metadata(&src)
            .unwrap()
            .file_type()
            .is_symlink(),
        "tracked source must be a real captured file, not a symlink"
    );
}

#[test]
fn hydrate_symlink_failure_leaves_no_partial_thread() {
    // P2 (cid 3327247262): on a host/filesystem that REJECTS directory
    // symlinks (Windows without the privilege, or an FS that doesn't
    // support dir symlinks), the symlink step fails AFTER the thread
    // ref + checkout were created — leaving a half-started thread.
    // `start --hydrate` must be atomic: either it fully succeeds, or it
    // fails cleanly with NO partial thread/checkout left behind.
    //
    // We simulate the unsupported-symlink host with the
    // `hydrate_symlink_dir` fault checkpoint so the contract is
    // exercised even on a platform that natively supports dir symlinks.
    let temp = TempDir::new().unwrap();
    init_deps_in_ignored_dir_project(temp.path());
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(&["capture", "-m", "main"], Some(temp.path())).unwrap();

    let thread_path = temp.path().join("iso");
    let output = heddle_output_with_env(
        &[
            "start",
            "iso",
            "--path",
            thread_path.to_str().unwrap(),
            "--hydrate",
        ],
        Some(temp.path()),
        &[("HEDDLE_FAULT_INJECT", "hydrate_symlink_dir")],
    )
    .expect("the heddle binary should run");

    // (a) The command fails cleanly...
    assert!(
        !output.status.success(),
        "start --hydrate must fail when directory symlinks are rejected"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("directory symlink"),
        "error must name the platform/FS limitation (directory symlinks); got:\n{stderr}"
    );

    // (b) ...and leaves NO half-started thread or checkout — the repo is
    // as if `start` never ran.
    assert!(
        std::fs::symlink_metadata(&thread_path).is_err(),
        "a hydrate failure must not leave the partially-materialized checkout at {}",
        thread_path.display()
    );
    let list = heddle(&["thread", "list"], Some(temp.path()))
        .expect("thread list should run after the rolled-back start");
    assert!(
        !list.contains("iso"),
        "a hydrate failure must not leave a dangling thread ref; got:\n{list}"
    );
}

#[test]
fn hydrate_symlink_failure_removes_self_created_target_dir() {
    // Case (a): `--path` points at a directory that did NOT exist before
    // this invocation. A symlink failure rolls back by removing the dir
    // entirely — it was created by this `start`, so "didn't exist" is the
    // correct pre-start state to restore.
    let temp = TempDir::new().unwrap();
    init_deps_in_ignored_dir_project(temp.path());
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(&["capture", "-m", "main"], Some(temp.path())).unwrap();

    let thread_path = temp.path().join("iso-new");
    assert!(
        std::fs::symlink_metadata(&thread_path).is_err(),
        "precondition: target dir must not exist before start"
    );

    let output = heddle_output_with_env(
        &[
            "start",
            "iso-new",
            "--path",
            thread_path.to_str().unwrap(),
            "--hydrate",
        ],
        Some(temp.path()),
        &[("HEDDLE_FAULT_INJECT", "hydrate_symlink_dir")],
    )
    .expect("the heddle binary should run");

    assert!(
        !output.status.success(),
        "start --hydrate must fail when directory symlinks are rejected"
    );
    assert!(
        std::fs::symlink_metadata(&thread_path).is_err(),
        "a self-created target dir must be removed entirely on rollback: {}",
        thread_path.display()
    );
}

#[test]
fn hydrate_symlink_failure_preserves_preexisting_empty_target_dir() {
    // Case (b): `--path` points at an empty directory the USER created
    // before running `start`. A symlink failure must roll back the
    // contents this invocation materialized WITHOUT destroying the
    // user-provided directory — restoring it to the empty dir they gave us
    // (cid 3327521537). Blanket-deleting the dir would obliterate user
    // state this command never created.
    let temp = TempDir::new().unwrap();
    init_deps_in_ignored_dir_project(temp.path());
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(&["capture", "-m", "main"], Some(temp.path())).unwrap();

    // The user pre-creates an empty dir and hands it to `--path`.
    let thread_path = temp.path().join("iso-existing");
    std::fs::create_dir(&thread_path).unwrap();

    let output = heddle_output_with_env(
        &[
            "start",
            "iso-existing",
            "--path",
            thread_path.to_str().unwrap(),
            "--hydrate",
        ],
        Some(temp.path()),
        &[("HEDDLE_FAULT_INJECT", "hydrate_symlink_dir")],
    )
    .expect("the heddle binary should run");

    assert!(
        !output.status.success(),
        "start --hydrate must fail when directory symlinks are rejected"
    );

    // The user's directory must STILL EXIST...
    let meta = std::fs::symlink_metadata(&thread_path).unwrap_or_else(|e| {
        panic!(
            "a pre-existing user dir must NOT be deleted on rollback ({}): {e}",
            thread_path.display()
        )
    });
    assert!(
        meta.is_dir(),
        "the pre-existing target must remain a directory after rollback"
    );

    // ...and be EMPTY again — every entry this invocation materialized
    // (the .heddle metadata, the checkout, any partial symlinks) cleared.
    let remaining: Vec<_> = std::fs::read_dir(&thread_path)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        remaining.is_empty(),
        "rollback must clear materialized contents from a pre-existing dir, leaving it empty; \
         found: {remaining:?}"
    );

    // And no dangling thread ref, same as the self-created case.
    let list = heddle(&["thread", "list"], Some(temp.path()))
        .expect("thread list should run after the rolled-back start");
    assert!(
        !list.contains("iso-existing"),
        "a hydrate failure must not leave a dangling thread ref; got:\n{list}"
    );
}

#[test]
fn hydrate_does_not_link_admin_dirs() {
    let temp = TempDir::new().unwrap();
    init_deps_in_ignored_dir_project(temp.path());
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(&["capture", "-m", "main"], Some(temp.path())).unwrap();

    let thread_path = temp.path().join("iso");
    heddle(
        &[
            "start",
            "iso",
            "--path",
            thread_path.to_str().unwrap(),
            "--hydrate",
        ],
        Some(temp.path()),
    )
    .unwrap();

    // `.heddle` is ignored at the origin but must never be hydrated —
    // linking it into a checkout would cross-wire two repos' metadata.
    let heddle_link = thread_path.join(".heddle");
    if let Ok(meta) = std::fs::symlink_metadata(&heddle_link) {
        assert!(
            !meta.file_type().is_symlink(),
            ".heddle must never be hydrated as a symlink"
        );
    }
}

#[test]
fn no_hydrate_flag_means_no_links() {
    let temp = TempDir::new().unwrap();
    init_deps_in_ignored_dir_project(temp.path());
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(&["capture", "-m", "main"], Some(temp.path())).unwrap();

    let thread_path = temp.path().join("plain");
    heddle(
        &["start", "plain", "--path", thread_path.to_str().unwrap()],
        Some(temp.path()),
    )
    .unwrap();

    assert!(
        std::fs::symlink_metadata(thread_path.join("node_modules")).is_err(),
        "without --hydrate, node_modules must not be linked into the checkout"
    );
}

#[test]
fn hydrated_deps_stay_ignored_in_checkout() {
    // AC: hydrated deps are not captured into heddle. The symlinked
    // node_modules name matches the checkout's own `.heddleignore` rule,
    // so `status` from inside the checkout must not surface it as
    // uncaptured worktree content.
    let temp = TempDir::new().unwrap();
    init_deps_in_ignored_dir_project(temp.path());
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(&["capture", "-m", "main"], Some(temp.path())).unwrap();

    let thread_path = temp.path().join("iso");
    heddle(
        &[
            "start",
            "iso",
            "--path",
            thread_path.to_str().unwrap(),
            "--hydrate",
        ],
        Some(temp.path()),
    )
    .unwrap();

    let status = heddle(&["status"], Some(&thread_path))
        .expect("status should run from the hydrated checkout");
    assert!(
        !status.contains("node_modules"),
        "hydrated node_modules must stay ignored (not reported by status); got:\n{status}"
    );
}

#[test]
fn start_partial_materialize_rolls_back_self_created_dir() {
    // heddle#356: a failure PARTWAY THROUGH the checkout materialize (the
    // `.heddle` metadata is on disk but the tree bytes are not) must rewind
    // the whole created checkout — the self-created target dir is removed
    // wholesale, and no `.heddle` is left behind. Pre-migration this left a
    // half-written checkout + a dangling thread ref; post-migration the repo
    // is as if `start` never ran.
    let temp = TempDir::new().unwrap();
    init_deps_in_ignored_dir_project(temp.path());
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(&["capture", "-m", "main"], Some(temp.path())).unwrap();

    let thread_path = temp.path().join("iso");
    let output = heddle_output_with_env(
        &["start", "iso", "--path", thread_path.to_str().unwrap()],
        Some(temp.path()),
        &[("HEDDLE_FAULT_INJECT", "start_materialize_checkout")],
    )
    .expect("the heddle binary should run");

    assert!(
        !output.status.success(),
        "a mid-materialize fault must fail the start"
    );
    // The created checkout dir (and its partial `.heddle`) is fully removed.
    assert!(
        std::fs::symlink_metadata(&thread_path).is_err(),
        "a partial materialize must remove the self-created checkout dir: {}",
        thread_path.display()
    );
    // No dangling thread ref.
    let list = heddle(&["thread", "list"], Some(temp.path()))
        .expect("thread list should run after the rolled-back start");
    assert!(
        !list.contains("iso"),
        "a materialize fault must not leave a dangling thread ref; got:\n{list}"
    );
}

#[test]
fn start_partial_materialize_preserves_preexisting_empty_dir() {
    // heddle#356, mirror of the hydrate case: a mid-materialize fault into a
    // USER-supplied empty `--path` dir must clear only the contents this
    // invocation wrote (the partial `.heddle`), never delete the directory
    // the user created.
    let temp = TempDir::new().unwrap();
    init_deps_in_ignored_dir_project(temp.path());
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(&["capture", "-m", "main"], Some(temp.path())).unwrap();

    let thread_path = temp.path().join("iso-existing");
    std::fs::create_dir(&thread_path).unwrap();

    let output = heddle_output_with_env(
        &[
            "start",
            "iso-existing",
            "--path",
            thread_path.to_str().unwrap(),
        ],
        Some(temp.path()),
        &[("HEDDLE_FAULT_INJECT", "start_materialize_checkout")],
    )
    .expect("the heddle binary should run");

    assert!(
        !output.status.success(),
        "a mid-materialize fault must fail the start"
    );
    let meta = std::fs::symlink_metadata(&thread_path).unwrap_or_else(|e| {
        panic!(
            "a pre-existing user dir must NOT be deleted on rollback ({}): {e}",
            thread_path.display()
        )
    });
    assert!(meta.is_dir(), "the pre-existing target must remain a directory");
    let remaining: Vec<_> = std::fs::read_dir(&thread_path)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        remaining.is_empty(),
        "rollback must clear materialized contents from a pre-existing dir; found: {remaining:?}"
    );
    let list = heddle(&["thread", "list"], Some(temp.path()))
        .expect("thread list should run after the rolled-back start");
    assert!(
        !list.contains("iso-existing"),
        "a materialize fault must not leave a dangling thread ref; got:\n{list}"
    );
}
