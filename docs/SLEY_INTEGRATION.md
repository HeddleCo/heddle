# Sley Integration Contract

Status: current Heddle hardening note. Heddle now treats Sley as the Git-format
engine for Git overlay, Git bridge, plain-Git inspection, and no-`git` runtime
paths. This page records the Sley-side ergonomics Heddle wants next and the
Heddle-side gates that should stay required while the integration settles.

## Sley Asks

### Exact Repository Open

Heddle has two distinct intents:

- discover a checkout from a user path, such as `heddle status` in a subdir
- open one already-known Git directory exactly, such as a scratch bare repo

The second intent should never discover a parent repo.

Desired Sley shape:

```rust
let git = SleyRepository::open_exact_bare(dest)
    .context("open Git scratch destination")?;
```

or:

```rust
let git = SleyRepository::open_with(
    dest,
    OpenOptions::new().exact_path(true).bare(true),
)?;
```

Heddle would use this in bridge clone/import/export scratch paths instead of
choosing between `open` and `discover` locally.

### Explicit Missing-Reference APIs

`find_reference(name) -> Result<Option<_>>` is correct but easy to misuse in
call sites that only need existence or a typed "required ref" failure.

Desired Sley shape:

```rust
if git.reference_exists("refs/heads/main")? {
    // render the branch as present
}

let head = git.require_reference("refs/remotes/origin/main")?;
```

Heddle would use `reference_exists` in health/verify checks and
`require_reference` in import/export paths where absence is a hard Git boundary.

### Separate Ref Resolution From Object Peeling

Heddle needs to preserve annotated tag object IDs in tag refs, but also often
needs to peel a ref target to the commit it names.

Desired Sley shape:

```rust
let tag_ref = git.require_reference("refs/tags/v1.0")?;
let ref_target = tag_ref.direct_target()?;

let object_oid = git.peel_to_object_oid(ref_target)?;
let commit_oid = git.peel_to_commit_oid(ref_target)?;
```

Heddle would use `peel_to_object_oid` when writing tag refs and
`peel_to_commit_oid` when deciding ancestry or branch-frontier behavior. The
important ergonomic split is that symbolic-ref resolution and annotated-tag
object peeling are visibly different operations.

### Porcelain Ref Updates With Attached-HEAD Reflog Parity

When Heddle advances the currently checked-out branch through Sley, users expect
both the branch reflog and direct `HEAD` reflog to look like ordinary Git
porcelain updated the branch.

Desired Sley shape:

```rust
git.update_branch_checked_out_as_head(
    "main",
    new_oid,
    RefUpdateOptions::new()
        .expect_old(old_oid)
        .reflog("heddle: checkpoint"),
)?;
```

or a transaction option:

```rust
git.refs()
    .transaction()
    .update("refs/heads/main", new_oid)
    .mirror_attached_head_reflog(true)
    .message("heddle: checkpoint")
    .commit()?;
```

Heddle would remove its direct `HEAD` reflog append sites and route Git-overlay
checkpoint, undo/redo, merge `--git-commit`, and branch sync through this
porcelain-level operation.

### Git Config Stack With Editable Origins

Remote mutation needs the same view Git users expect: includes followed,
worktree config included only when `extensions.worktreeConfig = true`, and
each value tied back to the file that defined it so Heddle can refuse unsafe
external include writes.

Desired Sley shape:

```rust
let config = git.config_stack(
    ConfigStackOptions::git_default()
        .follow_includes(true)
        .worktree_config(WorktreeConfig::WhenEnabled)
        .track_origins(true),
)?;

let origin = config.remote("origin")?;
let editable = config.editable_section_file(origin.section_id())?;
```

Heddle would use this in `heddle remote add/remove/set-url` instead of owning
config-layer reconstruction.

### Lazy Blob Hydration Boundary

Heddle can read local Git blobs through Sley today. When a promisor object is
missing, Heddle wants one native Sley boundary that either fetches the promised
blob or returns a typed unsupported/missing-object error.

Desired Sley shape:

```rust
let bytes = git.blobs()
    .read_or_fetch(oid, BlobFetchOptions::from_remote("origin"))
    .await?;
```

Heddle would use this in clone checkout, status/diff patch rendering, and any
future partial-clone materialization path.

### Reusable Status/Index Work Plans

The Git-overlay hot path repeatedly needs "HEAD tree + index + worktree"
classification without rebuilding all maps for every command.

Desired Sley shape:

```rust
let status = git.status_plan()
    .include_untracked(true)
    .reuse_index_cache(cache_key)
    .build()?;

let changes = status.collect()?;
```

Heddle would use this under status, diff, verify, and thread-list health so the
same Sley primitive backs all Git-overlay worktree reads.

## Heddle Hardening Gates

Keep these Heddle-side checks required while Sley settles in:

- `cargo test --locked -p heddle-cli --test git_process_lint -- --nocapture`
- `cargo test --locked -p heddle-cli --test cli_integration git_replacement_matrix -- --nocapture`
- `cargo test --locked -p heddle-cli --test git_bridge_integration round_trip_preserves_annotated_tag_object_sha -- --nocapture`
- `cargo test --locked -p heddle-cli --test git_bridge_integration import_populates_mirror_with_identical_annotated_tag_object -- --nocapture`
- `cargo test --locked -p heddle-cli --test cli_integration remotes::test_cli_raw_git_clone_adopt_fetches_notes_before_import -- --nocapture`
- `cargo test --locked -p heddle-cli --test cli_integration remotes::git_overlay_remote_remove_uneditable_include_leaves_both_configs_unmutated -- --nocapture`

The default workspace test run already covers these, but naming them in CI makes
Sley-integration regressions fail in a step with the right mental model.

## Heddle Cleanup Once Sley APIs Land

When the Sley-side asks above exist, Heddle should delete local compensating code
instead of preserving a second Git-engine abstraction:

- Replace scratch-repo `open`/`discover` choices with Sley's exact-open API.
- Replace local `find_reference(...).is_some()` helpers with Sley's
  `reference_exists` / `require_reference` helpers.
- Replace local tag/ref peeling helpers with explicit Sley object-peel and
  commit-peel APIs.
- Replace direct `HEAD` reflog appends with a Sley porcelain branch update or
  ref-transaction option.
- Replace `remote_ops` config-layer reconstruction with Sley's editable-origin
  config stack.
- Replace the lazy-hydration refusal boundary with Sley's native
  read-or-fetch blob API.
- Replace Heddle-local status/index memoization scaffolding with Sley reusable
  status/index work plans where the command needs Git-overlay status.

## Ignored Suites To Keep Scheduled

The default test loop is green without these, but they are the remaining
confidence multipliers:

- CLI fault-injection tests
- release-mode command-loop perf smoke
- release-mode pack/object perf tests
- real-world Git fixture matrix
- large-blob stress with `HEDDLE_LARGE_BLOB_MB`
- lazy hydration against a large public repo fixture
