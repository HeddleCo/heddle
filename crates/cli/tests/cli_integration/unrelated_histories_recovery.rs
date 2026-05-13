// SPDX-License-Identifier: Apache-2.0
//! Recovery from severed-parent histories.
//!
//! Some bootstrap workflows produce a synthetic root commit that
//! carries an upstream tree but no parent edge — Codex Cloud's
//! "sync a tree into a fresh worktree" mode is the motivating
//! example, but the same shape arises from `git archive` /
//! `git format-patch --root` exports, squash-merged forks where the
//! merge commit is the "first" commit on the new branch, and any
//! workflow that materializes content from a tarball before adding
//! a remote.
//!
//! `git merge` refuses these histories (`refusing to merge unrelated
//! histories`); forcing it with `--allow-unrelated-histories` produces
//! one `add/add` conflict per shared file because the trees have no
//! common base. The conflict count scales with the codebase, not the
//! drift — it's diagnostic noise, not real merge work.
//!
//! The recovery primitive is **tree-hash content matching**: every
//! commit identifies a tree, and a commit on `origin/main` whose tree
//! exactly equals the synthetic root's tree IS the canonical ancestor
//! the snapshot was taken from. Once we find it, we can graft the
//! divergent branch onto that commit and proceed with a normal rebase.
//!
//! This test reproduces the full recovery end-to-end — fixture
//! synthesis → severed-history detection → tree-hash discovery →
//! squash + replay → rebase across upstream PRs → invariant checks.
//!
//! ## Heddle product opportunity
//!
//! The manual recovery is six conceptual steps, four of which are
//! pure git plumbing. They could become two heddle commands:
//!
//! - `heddle bridge find-ancestor [--branch BRANCH] <synthetic-commit>` —
//!   walks `BRANCH`'s history (default `origin/main`), reports any
//!   commit whose tree hash matches the synthetic commit's tree
//!   hash. Output is a structured JSON list with confidence scores
//!   (exact tree match, near match, no match).
//!
//! - `heddle bridge graft --onto <ancestor> <synthetic-branch>` —
//!   squashes the synthetic-branch commits into one diff against the
//!   ancestor's tree, applies as a single commit on a new branch,
//!   then optionally rebases onto a target tip.
//!
//! Together, these would turn this 6-step manual recovery into:
//!
//! ```text
//! $ heddle bridge find-ancestor HEAD
//! { "matches": [{ "commit": "87199280", "tree": "9db723dc...",
//!                 "confidence": "exact" }] }
//! $ heddle bridge graft --onto 87199280 HEAD
//! ```

use super::*;

/// Build a tree containing a single file. Local helper because the
/// sibling modules that already define this keep theirs private.
fn git_tree_with_file(repo: &gix::Repository, path: &str, content: &[u8]) -> gix::hash::ObjectId {
    let blob = repo.write_blob(content).expect("write git blob").detach();
    let empty = repo.empty_tree().id;
    let mut editor = repo.edit_tree(empty).expect("edit git tree");
    editor
        .upsert(path, gix::object::tree::EntryKind::Blob, blob)
        .expect("add file to git tree");
    editor.write().expect("write git tree").detach()
}

/// Build a fixture mirroring the Codex Cloud bootstrap pattern, then
/// walk every step of the manual recovery and assert the invariants
/// hold at each transition.
///
/// Origin layout (canonical lineage):
/// ```text
///     A ─ B ─ C       (refs/heads/main)
/// ```
///
/// Synthetic worktree (severed-parent layout, what Codex Cloud
/// produces and what `tarball + git init` produces):
/// ```text
///     B'─ X ─ Y       (refs/heads/work)
/// ```
/// where `B'` carries B's tree exactly but has no parent, and
/// `X`/`Y` are local iteration commits. The work branch is now 2
/// upstream commits behind (`C` exists on origin but not on work)
/// and has 2 local commits (`X`, `Y`). The "right" answer after
/// recovery is a branch whose graph reads
/// `A ─ B ─ C ─ X' ─ Y'` (X' and Y' are X and Y replayed onto C).
#[test]
#[ignore = "nightly real-world matrix: codex-cloud-style snapshot recovery"]
fn unrelated_histories_recover_via_tree_hash_match() {
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let origin_repo = gix::init_bare(&origin).expect("init origin");

    // Build A → B → C on origin's main.
    let tree_a = git_tree_with_file(&origin_repo, "core.rs", b"pub fn a() {}\n");
    let commit_a = git_commit_with_tree(&origin_repo, Some("refs/heads/main"), tree_a, "A", &[]);
    let tree_b = git_tree_with_file(&origin_repo, "core.rs", b"pub fn a() {}\npub fn b() {}\n");
    let commit_b = git_commit_with_tree(
        &origin_repo,
        Some("refs/heads/main"),
        tree_b,
        "B",
        &[commit_a],
    );
    let tree_c = git_tree_with_file(
        &origin_repo,
        "core.rs",
        b"pub fn a() {}\npub fn b() {}\npub fn c() {}\n",
    );
    let _commit_c = git_commit_with_tree(
        &origin_repo,
        Some("refs/heads/main"),
        tree_c,
        "C",
        &[commit_b],
    );

    // The "Codex Cloud sync" — take B's tree and stamp it as a root
    // commit (no parent). This is exactly what
    // `lastSyncedTreeRef: <tree_b_hash>` in
    // `.git/worktrees/<name>/codex-synced-branch.json` produces.
    let snapshot_tree = tree_b;
    let synthetic_root = git_commit_with_tree(
        &origin_repo,
        Some("refs/heads/work"),
        snapshot_tree,
        "Add tag-scoped git-overlay history guidance",
        &[], // no parent — this is the load-bearing severance
    );

    // Two local iteration commits on `work`. Stand-ins for codex's
    // "Polish CLI operator feedback after shakedown" /
    // "Implement native Git-overlay replacement workflows" etc.
    let tree_x = git_tree_with_file(
        &origin_repo,
        "core.rs",
        b"pub fn a() {}\npub fn b() {}\npub fn local_x() {}\n",
    );
    let commit_x = git_commit_with_tree(
        &origin_repo,
        Some("refs/heads/work"),
        tree_x,
        "iter X",
        &[synthetic_root],
    );
    let tree_y = git_tree_with_file(
        &origin_repo,
        "core.rs",
        b"pub fn a() {}\npub fn b() {}\npub fn local_x() {}\npub fn local_y() {}\n",
    );
    let commit_y = git_commit_with_tree(
        &origin_repo,
        Some("refs/heads/work"),
        tree_y,
        "iter Y",
        &[commit_x],
    );

    // ─── Step 1: confirm the histories are severed ──────────────
    //
    // `git merge-base origin/main work` produces no output — the
    // graphs share no ancestor. This is the load-bearing diagnostic.
    let merge_base = origin_repo
        .merge_base(commit_y, _commit_c)
        .ok()
        .map(|m| m.detach());
    assert!(
        merge_base.is_none(),
        "synthetic root must have no common ancestor with origin/main; \
         got merge-base {merge_base:?}"
    );

    // ─── Step 2: discover the canonical ancestor by tree-hash ───
    //
    // Walk every commit reachable from `origin/main` and find one
    // whose tree hash equals the synthetic root's tree hash. That's
    // the commit Codex Cloud (or `git archive`, or whoever) sourced
    // the snapshot from.
    let synthetic_root_tree = origin_repo
        .find_commit(synthetic_root)
        .expect("find synthetic root")
        .tree_id()
        .expect("synthetic root tree id")
        .detach();
    let mut canonical_ancestor: Option<gix::hash::ObjectId> = None;
    for info in origin_repo
        .rev_walk([_commit_c])
        .all()
        .expect("rev-walk origin/main")
    {
        let info = info.expect("walk step");
        let oid = info.id;
        let commit = origin_repo.find_commit(oid).expect("find commit");
        let tree_id = commit.tree_id().expect("tree id").detach();
        if tree_id == synthetic_root_tree {
            canonical_ancestor = Some(oid);
            break;
        }
    }
    let canonical = canonical_ancestor.expect(
        "the synthetic root's tree must match SOME commit on origin/main; \
         that commit IS the canonical ancestor",
    );
    assert_eq!(
        canonical, commit_b,
        "the synced tree was B's tree, so B is the canonical ancestor"
    );

    // ─── Step 3: graft `work` onto the canonical ancestor ───────
    //
    // The graft = "take work's tip tree (which already incorporates
    // the iterations X and Y) and apply it as a single commit on top
    // of the canonical ancestor." That gives us
    // `A ─ B ─ {graft = synthetic + X + Y squashed}` — a branch
    // with proper graph lineage that's content-equivalent to the
    // pre-recovery work tip.
    let work_tip_tree = origin_repo
        .find_commit(commit_y)
        .expect("find work tip")
        .tree_id()
        .expect("work tip tree id")
        .detach();
    let grafted = git_commit_with_tree(
        &origin_repo,
        None,
        work_tip_tree,
        "graft: replay work onto canonical ancestor",
        &[canonical],
    );
    let grafted_tree = origin_repo
        .find_commit(grafted)
        .expect("find grafted")
        .tree_id()
        .expect("grafted tree id")
        .detach();
    assert_eq!(
        grafted_tree, work_tip_tree,
        "grafted commit's tree must exactly match the original work tip's tree \
         — anything else is silent data loss in the recovery"
    );

    // ─── Step 4: invariant — graft is now mergeable with origin/main ──
    //
    // `git merge-base grafted origin/main` should now resolve to the
    // canonical ancestor. The phantom-conflict storm (one AA conflict
    // per shared file) is gone because there's a real common base.
    let recovered_base = origin_repo
        .merge_base(grafted, _commit_c)
        .expect("recovery merge-base resolves")
        .detach();
    assert_eq!(
        recovered_base, canonical,
        "post-graft, the merge-base of the recovered branch and origin/main \
         must be the canonical ancestor we discovered in step 2"
    );

    // ─── Step 5: simulated rebase across the upstream gap ───────
    //
    // After recovery, applying `_commit_c`'s diff (relative to its
    // parent `commit_b` = canonical) onto `grafted` should produce a
    // tree containing every file from both sides. We simulate the
    // rebase by hand here: take grafted's tree (work's content), add
    // C's distinguishing change, assert no information lost.
    //
    // In production this is `git rebase origin/main` on the recovered
    // branch — the rebase has a real common base now, so it produces
    // a focused conflict report (only files genuinely changed on both
    // sides), not the 222-conflict storm we started with.
    let rebased_tree_bytes =
        b"pub fn a() {}\npub fn b() {}\npub fn c() {}\npub fn local_x() {}\npub fn local_y() {}\n";
    let rebased_tree = git_tree_with_file(&origin_repo, "core.rs", rebased_tree_bytes);
    let rebased = git_commit_with_tree(
        &origin_repo,
        Some("refs/heads/recovered"),
        rebased_tree,
        "recovered: work tip rebased onto origin/main",
        &[grafted],
    );

    // ─── Step 6: final invariant — the recovered branch contains
    //              every distinguishing change from both lineages.
    let final_tree = origin_repo
        .find_commit(rebased)
        .expect("find recovered tip")
        .tree_id()
        .expect("final tree id")
        .detach();
    let final_root_tree = origin_repo
        .find_object(final_tree)
        .expect("read final tree")
        .into_tree();
    let core_entry = final_root_tree
        .iter()
        .find_map(|entry| {
            let entry = entry.expect("tree entry");
            (entry.filename() == b"core.rs".as_slice()).then(|| entry.oid().to_owned())
        })
        .expect("core.rs in final tree");
    let core_blob = origin_repo
        .find_object(core_entry)
        .expect("find core.rs blob");
    let core_bytes = core_blob.data.clone();
    for token in [
        b"pub fn a()" as &[u8],
        b"pub fn b()",
        b"pub fn c()",
        b"pub fn local_x()",
        b"pub fn local_y()",
    ] {
        assert!(
            core_bytes
                .windows(token.len())
                .any(|window| window == token),
            "recovered tree must preserve every distinguishing change; \
             missing {:?} in {}",
            std::str::from_utf8(token).unwrap_or("<non-utf8>"),
            std::str::from_utf8(&core_bytes).unwrap_or("<non-utf8>")
        );
    }
}

/// Diagnostic-only check: a single tree-hash lookup against a real
/// origin should narrow the candidate ancestor set to exactly one
/// commit when the synthetic root was a verbatim snapshot. Hermetic,
/// runs in milliseconds, and exercises the inner primitive of the
/// recovery without the full end-to-end machinery.
///
/// This is the test that would back a future
/// `heddle bridge find-ancestor` command.
#[test]
#[ignore = "nightly real-world matrix: tree-hash ancestor lookup primitive"]
fn synthetic_root_tree_resolves_to_unique_canonical_ancestor() {
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let origin_repo = gix::init_bare(&origin).expect("init origin");

    let tree_a = git_tree_with_file(&origin_repo, "core.rs", b"// a\n");
    let a = git_commit_with_tree(&origin_repo, Some("refs/heads/main"), tree_a, "A", &[]);
    let tree_b = git_tree_with_file(&origin_repo, "core.rs", b"// a\n// b\n");
    let b = git_commit_with_tree(&origin_repo, Some("refs/heads/main"), tree_b, "B", &[a]);
    let tree_c = git_tree_with_file(&origin_repo, "core.rs", b"// a\n// b\n// c\n");
    let c = git_commit_with_tree(&origin_repo, Some("refs/heads/main"), tree_c, "C", &[b]);

    // The synced tree pretends to be tree_b; we don't even need to
    // create the synthetic commit — the question is "given this tree
    // hash, which origin/main commit produced it?"
    let synced_tree = tree_b;

    let mut matches = Vec::new();
    for info in origin_repo.rev_walk([c]).all().expect("rev-walk") {
        let info = info.expect("walk step");
        let commit = origin_repo.find_commit(info.id).expect("find commit");
        let tree_id = commit.tree_id().expect("tree id").detach();
        if tree_id == synced_tree {
            matches.push(info.id);
        }
    }
    assert_eq!(
        matches.len(),
        1,
        "exactly one origin/main commit should produce a given synced tree; \
         got {} matches: {matches:?}",
        matches.len()
    );
    assert_eq!(matches[0], b, "the unique match is the source commit");
}