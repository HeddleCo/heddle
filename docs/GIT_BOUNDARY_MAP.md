# Git Boundary Map

Status: current Heddle boundary contract. This map complements
`docs/SLEY_INTEGRATION.md` by naming which Git-shaped work is owned by Sley,
which Heddle work is waiting on a Sley facade, and where subprocess `git` is
allowed as a test oracle.

## Boundary Rule

Production Heddle code must not depend on a `git` executable on `PATH`.
Git-format reads, writes, transport, status, refs, index work, notes, and bridge
operations belong behind Sley-backed APIs or an explicitly named Sley facade
gap. Heddle may inspect or write Heddle-owned sidecar files, but it should not
add new Git plumbing in Heddle when the right owner is Sley.

Tests may shell out to `git` when they need Git itself as an oracle, fixture
builder, or compatibility witness. Keep those calls in test crates,
`#[cfg(test)]` modules, `*_tests.rs` files, or scripts whose purpose is fixture
construction or conformance checking. Production code must not call test-only
Git helpers.

The mechanical gate is `crates/cli/tests/git_process_lint.rs`. It scans runtime
source directories for production `Command::new("git")` sites, skips test-only
modules, and keeps the production subprocess allowlist reviewed.

Current production subprocess allowlist: empty.

## Sley-backed

These production areas already route Git-format work through Sley and should
stay there.

| Area | Heddle files | Boundary |
|---|---|---|
| Git overlay discovery and repository handles | `crates/repo/src/repository.rs` | `Repository::git_overlay_sley_repository`, overlay bootstrap, plain-Git detection, and Git root inspection use `sley::Repository` rather than spawning Git. |
| Git overlay status and remote drift | `crates/repo/src/repository.rs`, `crates/cli/src/cli/commands/status.rs`, `crates/cli/src/cli/commands/thread.rs`, `crates/cli/src/cli/commands/git_overlay_health/mod.rs` | Worktree/index status, branch tips, tag tips, current branch, detached HEAD state, remote names, and ahead/behind checks are Sley-backed or should flow through the repository helpers that are. |
| Git bridge import/export/sync | `crates/cli/src/bridge/git_core.rs`, `crates/cli/src/bridge/git_ingest.rs`, `crates/cli/src/bridge/git_export.rs`, `crates/cli/src/bridge/git_sync.rs`, `crates/cli/src/bridge/git_notes.rs`, `crates/cli/src/bridge/git_reconstruct.rs` | Bridge commands read/write Git objects, notes, refs, commits, trees, tags, and sync markers with Sley. Native Git transport in `git_core.rs` also uses Sley remote APIs. |
| Ingest Git walking | `crates/ingest/src/git_walk.rs`, `crates/ingest/src/importer.rs`, `crates/ingest/src/transcript/mod.rs` | Production ingest opens Git repositories through Sley and translates commit/tree/ref data into Heddle records. Subprocess Git in these files is test-only fixture setup. |
| Merge with Git checkpoint commit | `crates/cli/src/cli/commands/merge/git_commit.rs` | `merge --git-commit` validates Git state, writes trees/commit objects, updates refs, and appends reflog records through Sley. |
| Git-overlay clone/import/fetch/push | `crates/cli/src/cli/commands/clone.rs`, `crates/cli/src/bridge/git_core.rs` | Supported Git-overlay clone/import/fetch/push paths use Sley. Unsupported lazy/filter Git-overlay clone cases fail closed instead of shelling out. |

## Sley facade gap

These areas are production Git-shaped behavior where Heddle currently has local
adapter code or compensating logic because the ideal Sley facade is not exposed
yet. They are not permission to spawn `git`; they are cleanup targets once Sley
adds the facade named in `docs/SLEY_INTEGRATION.md`.

| Gap | Heddle areas | Current shape | Desired end state |
|---|---|---|---|
| Exact repository open | `crates/cli/src/bridge/git_core.rs` `open_repo`, `copy_local_repo_to_bare`, clone/import scratch paths | Heddle chooses between `SleyRepository::discover`, `open`, and `init_bare` depending on whether a checkout or bare repo is expected. | Sley exposes an exact bare/open option so Heddle callers express discovery vs exact-open intent directly. |
| Missing-reference ergonomics | `crates/repo/src/repository.rs` `git_find_reference`, `git_resolve_oid`; `crates/cli/src/bridge/git_sync.rs` | Heddle maps Sley errors into `Option`/required-reference behavior at call sites. | Sley exposes `reference_exists` and `require_reference` style APIs. |
| Ref target vs peeled object/commit | `crates/repo/src/repository.rs`, `crates/cli/src/bridge/git_export.rs`, `crates/cli/src/bridge/git_sync.rs` | Heddle helpers choose whether to preserve a direct tag object or peel to a commit for ancestry/frontier decisions. | Sley exposes explicit object-peel and commit-peel APIs so call sites cannot blur the two operations. |
| Attached HEAD reflog parity | `crates/cli/src/bridge/git_core.rs`, `crates/cli/src/bridge/git_sync.rs`, `crates/cli/src/cli/commands/merge/git_commit.rs`, `crates/cli/src/cli/commands/undo_apply/mod.rs` | Heddle performs ref transactions and, in some attached-HEAD cases, appends direct `HEAD` reflog entries itself. | Sley exposes a porcelain branch update or transaction option that mirrors attached `HEAD` reflog behavior. |
| Git config stack editing | `crates/cli/src/cli/commands/remote/remote_ops.rs`, `crates/cli/src/cli/commands/remote/mod.rs`, `crates/repo/src/repository.rs` | Heddle reads Sley config snapshots and owns editable-origin checks for remote mutation. | Sley exposes a Git-default config stack with includes, worktree config policy, and editable origin metadata. |
| Lazy blob hydration | `crates/cli/src/cli/commands/clone.rs`, `crates/repo/src/lazy_hydrator.rs`, `crates/repo/src/repository.rs` | Local Git-overlay lazy/filter clone paths fail closed or use a Heddle hydrator boundary because native promisor blob fetching is not yet exposed. | Sley exposes a blob read-or-fetch boundary for promisor objects. |
| Reusable status/index plans | `crates/repo/src/repository.rs`, `crates/cli/src/cli/commands/status.rs`, `crates/cli/src/cli/commands/diff`, `crates/cli/src/cli/commands/git_overlay_health/mod.rs` | Heddle calls Sley status APIs, then carries local caching and command-specific reuse patterns. | Sley exposes reusable status/index work plans that Heddle commands can share. |

## Test oracle

Subprocess Git belongs in tests when Git itself is the fixture creator or
compatibility oracle. These sites should not be migrated to production helpers.
They should remain hermetic: set test identities, clear global/system config
where practical, and use temporary repositories.

| Test class | Examples | What belongs here |
|---|---|---|
| Bridge conformance | `crates/cli/tests/git_bridge_integration.rs`, `crates/cli/tests/roundtrip_fidelity.rs`, `crates/cli/tests/cli_integration/bridge.rs` | Create Git repositories, refs, tags, notes, worktrees, and commits that exercise byte-fidelity and round-trip behavior. |
| Git-overlay CLI compatibility | `crates/cli/tests/cli_integration/git_overlay_matrix.rs`, `crates/cli/tests/cli_integration/git_replacement_matrix.rs`, `crates/cli/tests/cli_integration/git_overlay_interop_matrix.rs` | Compare Heddle behavior against Git porcelain, prepare external operation states, and verify no-`git` runtime replacement coverage. |
| Patch/diff oracles | `crates/cli/tests/cli_integration/diff_patch_conformance.rs`, `crates/semantic/src/diff/diff_tests.rs`, inline tests in `crates/cli/src/cli/commands/diff/diff_output.rs` | Ask Git to apply, check, or produce canonical patch behavior for tests. |
| Ingest fixtures | `crates/ingest/src/git_walk.rs`, `crates/ingest/src/importer.rs`, `crates/ingest/src/reasoning_pipeline.rs`, `crates/ingest/src/transcript/mod.rs` test modules | Construct small or pathological repositories whose raw objects, refs, reflogs, tags, and worktrees validate the Sley-backed importer. |
| Runtime lint and no-Git gates | `crates/cli/tests/git_process_lint.rs`, `crates/cli/tests/cli_integration/git_replacement_matrix.rs` | Prove production code does not spawn Git and that supported Git-overlay workflows run without a Git binary. |

## Intentional subprocess

Current state: none.

A production Git subprocess should be exceptional. Prefer a Sley facade gap or
a user handoff that prints the external command instead of running it. If a
subprocess is still necessary, the boundary map and
`crates/cli/tests/git_process_lint.rs` must agree on the entry. Each entry must
carry an owner, a reason, and a desired end state. The expected reason should be
a temporary Sley facade gap with an issue or roadmap row and a deletion plan.

Any allowed subprocess must clear inherited environment where practical and
re-add only the minimum variables it needs. `GIT_*` variables must not leak into
child processes unless the entry explains why that exact variable is required.

## Enforcement Checklist

When adding, moving, or removing a Git-shaped operation:

1. Decide which category above owns it.
2. If it is production behavior, prefer Sley-backed APIs or add a Sley facade
   gap. Do not shell out to Git as a shortcut.
3. If it is a test oracle, keep it in a test-only file or module and make the
   fixture hermetic.
4. If it is an intentional production subprocess, add one reviewed allowlist
   entry to `crates/cli/tests/git_process_lint.rs` with owner, reason, category,
   and desired end state, then document the same file/function in this map.
5. Run `cargo test --locked -p heddle-cli --test git_process_lint -- --nocapture`.
