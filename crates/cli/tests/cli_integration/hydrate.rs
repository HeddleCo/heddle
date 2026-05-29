// SPDX-License-Identifier: Apache-2.0
//! End-to-end coverage of `heddle start --hydrate` (heddle#302).
//!
//! `--hydrate` symlinks the origin checkout's top-level ignored
//! dependency directories (`node_modules`, `.venv`, …) into a fresh
//! isolated checkout so it's immediately buildable. These tests run the
//! built `heddle` binary inside temp dirs and inspect what `start`
//! leaves on disk: the links must exist, point back at the origin, and
//! stay ignored (never captured).

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
        !std::fs::symlink_metadata(&src).unwrap().file_type().is_symlink(),
        "tracked source must be a real captured file, not a symlink"
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
