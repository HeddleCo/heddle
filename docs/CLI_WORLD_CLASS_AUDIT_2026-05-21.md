# Heddle CLI World-Class Audit - 2026-05-21

Status: release-ready against the current audit evidence. This report records
current evidence against `docs/CLI_WORLD_CLASS_RUBRIC.md`; everyday commands are
scored A or better, global CLI behavior is scored A+, and no hard-gate failures
remain in the verified surface.

## Release Decision

Ready to mark release-quality for the audited CLI surface. The no-`git` overlay
hard gate, schema drift gate, docs drift gate, public-help reachability gate,
auto-output contract, TTY behavior, corrupt-repo recovery, destructive-safety
checks, exit-code taxonomy, JSON error envelopes, and runtime `git` process lint
are covered by automated gates. The full locked workspace test suite also passes
after the final materialized-file mode fix.

## Tested Repositories

| Category | Repository | Evidence | Shape |
|---|---|---|---|
| Small/typical | Local `tapestry` repo cloned to `/tmp` during audit | Manual CLI transcript on temporary clone with dirty file and `PATH=""` | Svelte/TypeScript app, normal single-repo workflow |
| Large/messy | `gix-shaped` vendored fixture from `GitoxideLabs/gitoxide` | `realworld_fixtures_clone_and_import_round_trip` | packed refs, large history, binary churn, tags |
| Large/messy | `tokio-shaped` vendored fixture from `tokio-rs/tokio` | `realworld_fixtures_clone_and_import_round_trip` | merge-heavy, multi-branch, rename-heavy |
| Large/messy | `ripgrep-shaped` vendored fixture from `BurntSushi/ripgrep` | `realworld_fixtures_clone_and_import_round_trip` | many small files, concurrent branches |
| Large/messy | `git-shaped` vendored fixture from `git/git` | `realworld_fixtures_clone_and_import_round_trip`; manual dirty long-path worktree probe | deep DAG, octopus merge, annotated tags, gitlink, dirty worktree, long path with spaces |

The four vendored fixtures are pinned in
`crates/cli/tests/realworld_git/realworld_repos.toml` and verified by tip OID at
extract time.

## Commands And Evidence Captured This Pass

| Command / surface | Evidence | Result | Rubric notes |
|---|---|---|---|
| `clone` | `cargo test --locked -p heddle-cli --test cli_integration realworld_fixtures_clone_and_import_round_trip -- --ignored --nocapture` | Pass | Real public fixtures clone with `PATH=""`. |
| `bridge import` | Same real-world fixture test | Pass | Imports all refs with `PATH=""`. |
| `fsck --git --output json` | Same real-world fixture test | Pass | JSON parses and `valid=true` with `PATH=""`; no host-`git` carve-out remains in this test. |
| `thread list --output json` | Same real-world fixture test | Pass | JSON parses and exposes imported threads with `PATH=""`. |
| `status --output json` | `git_replacement_matrix_fresh_git_read_commands_without_git_on_path`; manual `git-shaped` dirty long-path probe | Pass | Fresh Git worktree without Heddle state reports `git-overlay`, current thread, and dirty path with `PATH=""`; large fixture probe detected `untracked dirty file.txt` under a long path with spaces. |
| `diagnose --output json` | `git_replacement_matrix_fresh_git_read_commands_without_git_on_path`; manual `tapestry` clone audit; manual `git-shaped` dirty long-path probe | Pass | No longer fails when upstream drift probing cannot spawn `git`; large dirty-worktree probe parsed cleanly with `PATH=""`. |
| `status --output json` | `git_replacement_matrix_fresh_git_read_commands_without_git_on_path`; manual `tapestry` clone audit | Pass | No longer fails when upstream drift probing cannot spawn `git`. |
| `ready --output json` | `git_replacement_matrix_fresh_git_read_commands_without_git_on_path`; manual `tapestry` clone audit | Pass | Emits parseable JSON and exits 0 with `PATH=""`. |
| `help status` | Manual `tapestry` audit | Pass | Purpose, usage, flags, examples; stdout only. |
| everyday help entrypoints | `everyday_commands_have_all_required_help_entrypoints` | Pass | Rubric everyday commands support `heddle help <cmd>`, `<cmd> --help`, and `<cmd> -h`. |
| public command-path help entrypoints | `public_command_paths_have_all_required_help_entrypoints` | Pass after fix | Enumerates visible clap command paths and verifies `heddle help <path>`, `<path> --help`, and `<path> -h`; nested `heddle help thread list`-style paths now route to clap help. |
| bad command typo | Manual `statuz` typo after `heddle` audit | Pass | Exit 2; suggests `stash`, `start`, `status`; stderr only. |
| missing `--repo` path in JSON mode | `missing_repo_path_emits_actionable_json_error_envelope` | Pass after fix | Stderr JSON envelope has non-empty `kind=path_not_found` and `hint` mentioning `--repo`; stdout empty. |
| `doctor schemas --output json` | Manual command plus `doctor_schemas_has_no_drift_or_unmatched_registered_verbs` | Pass after docs fix | 26 registered verbs, 26 passing verbs, zero unmatched verbs, zero issues. |
| `doctor docs --all --output json` | Manual command plus docs-doctor unit/integration tests | Pass after docs/scanner fix | 47 Markdown files scanned, zero issues; `.codex/` worktrees ignored as generated audit scratch space. |
| `init --repo <path>` | `test_cli_init_honors_global_repo_path`; manual probe | Pass after fix | Initializes the requested path, not the process cwd; conflicting `--repo` + positional paths fail before side effects. |
| everyday save/read machine streams | `git_replacement_matrix_everyday_save_read_machine_streams_without_git_on_path` | Pass | In Git-overlay mode with `PATH=""`, `init`, `status`, `capture`, `checkpoint`, `log`, `show`, `diagnose`, and `ready` emit parseable JSON with empty stderr; `diff --output text` respects `NO_COLOR=1`. |
| `start` / `merge` / `undo` JSON workflow | `start_merge_undo_json_workflow_keeps_machine_streams_clean` | Pass after strengthening | Isolated solid checkout, feature capture, merge preview/apply, undo list/preview all emit parseable JSON with empty stderr; merge and undo preview paths do not mutate current state or worktree content; repeat merge is a successful `Already up to date` text no-op. |
| `version --repo <path> --verbose --output json` | `version_verbose_honors_explicit_repo_path` | Pass after fix | Verbose bug-context JSON reports the explicitly requested repository root, not the process cwd. |
| `resolve` outside a merge | `resolve_without_merge_emits_actionable_json_error` | Pass after fix | JSON and text failures use `kind=no_merge_in_progress` / `Error: No merge in progress`, hint at `heddle status`, keep stdout empty, and avoid the old `object not found` wrapper. |
| corrupt repository ref recovery | `fsck_on_corrupt_ref_emits_integrity_hint_in_text_and_json` | Pass after fix | Corrupt thread ref fails non-zero with stdout clean; JSON stderr has `kind=repository_integrity_error`, preserves the invalid-object error, and hints `heddle fsck --full`; text mode mirrors the recovery hint. |
| global quiet/color/narrow text behavior | `quiet_no_color_and_narrow_text_outputs_preserve_global_contract`; `narrow_no_color_text_outputs_cover_everyday_read_surfaces` | Pass after fix | `--quiet` suppresses capture/log tips; `NO_COLOR=1` wins over forced color; `COLUMNS=28/30` text succeeds across status, diagnose, doctor, diff, log, show, thread list, status, bridge status, fsck, and ready with primary labels intact. |
| default auto-output contract | `default_auto_output_is_json_when_stdout_is_piped_and_text_when_forced`; `tty_auto_mode_renders_text_and_explicit_json_stays_json` | Pass | Confirms piped stdout uses parseable JSON in `auto` mode; TTY stdout uses human text for `status` and rich guidance for `start`; `--output text` and `--output json` override auto regardless of stream. |
| exit-code and failure-stream taxonomy | `global_exit_codes_and_failure_streams_are_predictable` | Pass | Help exits 0 on stdout; clap parse errors exit 2 on stderr with suggestions; environment failures exit 1 with clean stdout and JSON error envelope when requested. |
| `ready` text/no-op output | `ready_text_names_ready_and_already_ready_noop_states` | Pass | Text mode names ready vs already-ready no-op states, includes readiness detail and next action, and keeps stderr empty. |
| state-reader missing-ID recovery | `unknown_state_id_hints_at_heddle_log_across_state_readers` | Pass after strengthening | `goto`, `show`, and `diff` fail non-zero with clean stdout, original error, and `heddle log` recovery hint. |
| destructive-safety checks | `start_merge_undo_json_workflow_keeps_machine_streams_clean`; `thread_cleanup`; `test_cli_capture_blocks_large_git_overlay_deletion_without_force` | Pass | Merge/undo previews are non-mutating; thread cleanup refuses ambiguous destructive modes and dry-runs safely; large deletion capture requires `--force`. |
| materialized checkout file modes | `cargo test --locked -p heddle-repo --lib`; `cargo test --locked --workspace` | Pass after fix | Regular materialized blobs normalize to `0o644`, executables to `0o755`; checkouts no longer inherit restrictive loose-object modes such as `0o600` under restrictive umasks. |
| full workspace regression suite | `cargo test --locked --workspace` | Pass | Confirms the CLI, repo, bridge, mount, objects, semantic, daemon, review, docs, and doctest surfaces still pass after the audit fixes. |
| CI release gates | `.github/workflows/rust-tests.yml` | Added | PR/main CI now explicitly runs docs drift, schema drift, no-`git` overlay matrix, public help reachability, auto-output, TTY output, corrupt-repo recovery, exit-code taxonomy, destructive-safety, and runtime `git` process lint gates. |

## Fixes Made During This Audit Pass

- Replaced `git symbolic-ref` usage in `Repository::git_overlay_current_branch`
  with native `.git/HEAD` / Sley-backed detection.
- Made Git-overlay worktree status fall back to Heddle's native tree comparison
  when the `git` executable is absent.
- Made upstream drift probing degrade to `remote_tracking=null` when the `git`
  executable is absent instead of failing read commands.
- Expanded `git_replacement_matrix` to cover a fresh Git worktree with no
  Heddle state under `PATH=""` for `status`, `diagnose`, `thread list`,
  `status`, and `ready`.
- Updated real-world fixture tests so `fsck --git --output json` and
  `thread list --output json` run under `PATH=""` rather than borrowing host Git.
- Classified missing path IO errors as `path_not_found` in JSON-mode error
  envelopes, with an actionable `--repo` / `heddle init` recovery hint.
- Added parseable JSON samples for every registered schema verb in
  `docs/json-schemas.md`, including thread/workspace/review/bridge/diagnose
  surfaces and the new `path_not_found` error kind.
- Tightened docs drift checking by ignoring generated `.codex/` worktrees and
  recognizing client-feature-gated `support` docs, then updated stale documented
  invocations such as `--workspace heavy`, `thread start`, and redaction
  subcommand syntax.
- Fixed `heddle --repo <path> init` so it initializes the requested repository
  path instead of silently initializing the process current directory; conflicting
  `--repo` and positional init paths now fail before creating either repo.
- Added a no-`git` everyday machine-stream regression covering Git-overlay
  `init`, `status`, `capture`, `checkpoint`, `log`, `show`, `diagnose`,
  `ready`, and `diff` with `NO_COLOR=1`.
- Fixed `heddle doctor --repo <path> --verbose --output json` so bug-context output
  reports the explicitly requested repository instead of the process cwd.
- Classified merge resolve/continue/abort-style no-operation failures as
  `no_merge_in_progress` with an actionable `heddle status` hint, and
  no-conflict resolve attempts as `no_conflicts_to_resolve`.
- Added a clean JSON workflow regression for `start`, `merge --preview`,
  `merge`, `undo --list`, and `undo --preview`.
- Added global accessibility/stream regression coverage for `--quiet`,
  `NO_COLOR` precedence, and narrow `COLUMNS` rendering. A follow-up broad
  narrow/no-color regression caught and fixed `log` tips ignoring `--quiet`;
  discoverability tips now respect the global quiet flag.
- Extended the custom help router so `heddle help <nested command path>` reaches
  clap-derived help for public subcommands, then added a public command-tree
  test covering `help`, `--help`, and `-h` for every visible path.
- Added explicit regressions for `--output auto`: piped stdout produces JSON,
  TTY stdout produces human text, TTY `start` prints actionable guidance, and
  explicit `--output text` / `--output json` override auto mode regardless of
  stream.
- Added an exit-code/stream regression covering help success, clap parse errors,
  and JSON-mode environment failures.
- Strengthened merge/undo workflow coverage so preview modes prove they do not
  mutate current state or worktree content before the real apply.
- Added ready text/no-op coverage and broadened missing-state recovery checks
  across `goto`, `show`, and `diff`.
- Classified invalid, corrupt, or missing object failures as
  `repository_integrity_error`, with an actionable `heddle fsck --full` hint in
  both JSON and text modes.
- Fixed the help-router regression test to match the current unknown-topic
  wording: unknown help names now say `no topic or command`, then point back to
  `heddle help advanced` and curated `heddle help`.
- Fixed repository materialization so regular worktree files are explicitly
  normalized to `0o644` and executables to `0o755`; relying on the loose-object
  source mode was wrong under restrictive umasks and could leave checkouts at
  `0o600`.
- Added current destructive-safety evidence for thread cleanup and large deletion
  capture, then named those checks in the explicit CI release gate.
- Added an explicit `CLI world-class release gates` step to Rust CI for docs
  drift, schema drift, no-`git` overlay workflows, public help reachability,
  auto-output, TTY output, corrupt-repo recovery, exit-code taxonomy,
  destructive safety, and runtime `git` process lint checks.
- Promoted the thread/status workspace view onto the curated core-loop help surface,
  aligned default `heddle help` around setup/orient/work/check/inspect/integrate/recover/doctor,
  and moved `review`, `discuss`, `context`, and `goto` behind advanced/topic
  help.
- Kept `heddle status` as the explicit scriptable workspace summary and
  `heddle thread list` as the grouped current/stacked/parallel thread view.
- Strengthened `status` text output so the next-action section names the command,
  why it is the right move, and the follow-up where one is useful.
- Added `heddle help` as a public text/JSON command catalog for agents,
  shell integrations, and generated docs; registered the JSON schema and
  documented the output contract.
- Added `heddle help git-dependencies` to spell out which Git-overlay workflows
  work without `git` on `PATH` and which remaining process calls are optional
  escape hatches.
- Added the `--output auto` contract to the default curated help so developers
  see the TTY-text / piped-JSON behavior before they integrate.

## Verification Commands

```bash
cargo test -p heddle-cli --test cli_integration git_replacement_matrix -- --nocapture
# 12 passed; 0 failed (latest rerun after public-help/auto-output changes)

cargo test -p heddle-cli --test cli_integration git_replacement_matrix_everyday_save_read_machine_streams_without_git_on_path -- --nocapture
# 1 passed; 0 failed (also included in full git_replacement_matrix run)

cargo test -p heddle-cli --test cli_integration test_cli_init_ -- --nocapture
# 6 passed; 0 failed

cargo test -p heddle-cli --test cli_integration start_merge_undo_json_workflow_keeps_machine_streams_clean -- --nocapture
# 1 passed; 0 failed; verifies clean JSON streams and non-mutating merge/undo previews

cargo test -p heddle-cli --test cli_integration version_verbose_honors_explicit_repo_path -- --nocapture
# 1 passed; 0 failed

cargo test -p heddle-cli --test cli_integration resolve_without_merge_emits_actionable_json_error -- --nocapture
# 1 passed; 0 failed

cargo test -p heddle-cli --test cli_integration fsck_on_corrupt_ref_emits_integrity_hint_in_text_and_json -- --nocapture
# 1 passed; 0 failed

cargo test --locked -p heddle-cli --test cli_integration refs_and_history::test_cli_help_verb_falls_through_to_clap -- --nocapture
# 1 passed; 0 failed

cargo test --locked -p heddle-repo --lib repository::repository_materialization::tests::materialized_blob_uses_normal_writable_mode -- --nocapture
# 1 passed; 0 failed

cargo test --locked -p heddle-repo --lib
# 288 passed; 0 failed

cargo test -p heddle-cli --test cli_integration quiet_no_color_and_narrow_text_outputs_preserve_global_contract -- --nocapture
# 1 passed; 0 failed

cargo test -p heddle-cli --test cli_integration narrow_no_color_text_outputs_cover_everyday_read_surfaces -- --nocapture
# 1 passed; 0 failed

cargo test -p heddle-cli --test cli_integration tty_auto_mode_renders_text_and_explicit_json_stays_json -- --nocapture
# 1 passed; 0 failed

cargo test -p heddle-cli cli::tips::tests -- --nocapture
# 6 passed; 0 failed

cargo test -p heddle-cli --test git_process_lint -- --nocapture
# 1 passed; 0 failed (latest rerun after CI gate addition)

cargo test -p heddle-cli --test cli_integration oss_cli_polish -- --nocapture
# 45 passed; 0 failed

cargo test --locked -p heddle-cli --test cli_integration realworld_fixtures_clone_and_import_round_trip -- --ignored --nocapture
# 1 passed; 0 failed; finished in 348.02s (current-code rerun)

# Manual large-fixture dirty-worktree probe
# tar xzf crates/cli/tests/realworld_git/fixtures/git-shaped.tar.gz -C /tmp/<audit>
# env PATH= target/debug/heddle clone /tmp/<audit>/git-shaped /tmp/<audit>/work
# mkdir -p /tmp/<audit>/work/dirty workspace/long path segment with spaces/another long path segment for audit coverage
# printf ... > /tmp/<audit>/work/.../untracked dirty file.txt
# env PATH= target/debug/heddle --repo /tmp/<audit>/work --output json status
# env PATH= target/debug/heddle --repo /tmp/<audit>/work --output json diagnose
# status reported repository_capability=git-overlay and observed the dirty long-path file; diagnose JSON parsed cleanly

cargo test -p heddle-cli --test cli_integration missing_repo -- --nocapture
# 3 passed; 0 failed

cargo test -p heddle-cli --test cli_integration thread_cleanup -- --nocapture
# 9 passed; 0 failed

cargo test -p heddle-cli --test cli_integration test_cli_capture_blocks_large_git_overlay_deletion_without_force -- --nocapture
# 1 passed; 0 failed

cargo test -p heddle-cli --test cli_integration everyday_commands_have_all_required_help_entrypoints -- --nocapture
# 1 passed; 0 failed

cargo test -p heddle-cli --test cli_integration public_command_paths_have_all_required_help_entrypoints -- --nocapture
# 1 passed; 0 failed

cargo test -p heddle-cli --test cli_integration default_auto_output_is_json_when_stdout_is_piped_and_text_when_forced -- --nocapture
# 1 passed; 0 failed

cargo test -p heddle-cli --test cli_integration global_exit_codes_and_failure_streams_are_predictable -- --nocapture
# 1 passed; 0 failed

cargo test -p heddle-cli --test cli_integration ready_text_names_ready_and_already_ready_noop_states -- --nocapture
# 1 passed; 0 failed

cargo test -p heddle-cli --test cli_integration unknown_state_id_hints_at_heddle_log_across_state_readers -- --nocapture
# 1 passed; 0 failed

cargo test -p heddle-cli --test cli_integration git_overlay_matrix_native_worktree_branch_switch_and_remote_drift_surface_cleanly -- --nocapture
# 1 passed; confirms normal upstream drift reporting still works when git is available

cargo test --locked --workspace
# Pass; includes CLI integration, repo/materialization, bridge, objects, mount,
# semantic, daemon, review, doctests, schema/help/render lint, and broad
# command-surface regression coverage.

cargo test -p heddle-cli --test cli_integration doctor_schemas_has_no_drift_or_unmatched_registered_verbs -- --nocapture
# 1 passed; 0 failed

cargo test -p heddle-cli --test cli_integration doctor_docs -- --nocapture
# 7 passed; 0 failed

cargo test -p heddle-cli cli::commands::doctor_docs::tests -- --nocapture
# 10 passed; 0 failed

target/debug/heddle --repo /home/heddleco/dev/HeddleCo/heddle doctor schemas --output json
# 26 registered verbs; 0 unmatched_verbs; 0 issues

target/debug/heddle --repo /home/heddleco/dev/HeddleCo/heddle doctor docs --all --output json
# 47 files_scanned; 0 issues
```

Manual temporary `tapestry` audit shape:

```bash
git clone /home/heddleco/dev/HeddleCo/tapestry /tmp/<audit>/tapestry
printf audit-dirty > /tmp/<audit>/tapestry/'audit dirty file.txt'
env PATH= heddle --repo /tmp/<audit>/tapestry status
env PATH= heddle --repo /tmp/<audit>/tapestry --output json status
env PATH= heddle --repo /tmp/<audit>/tapestry --output json diagnose
env PATH= heddle --repo /tmp/<audit>/tapestry --output json status
env PATH= heddle --repo /tmp/<audit>/tapestry --output json ready
env PATH= NO_COLOR=1 heddle --repo /tmp/<audit>/no-such --output json status
heddle help status
./target/debug/heddle statuz
```

Observed: success cases exited 0 and emitted parseable JSON where requested;
JSON-mode missing path errors emitted one JSON object on stderr and no stdout;
`statuz` typo after `heddle` exited 2 with suggestions on stderr.

## Command Matrix

Every named everyday command in `docs/CLI_WORLD_CLASS_RUBRIC.md` now has direct
current evidence and scores A or better. `workspace` has since been promoted
onto the curated core-loop help surface and defaults to the control-tower view
when run without a subcommand. Advanced public commands are held to B or better
by the full workspace suite, public command-path help coverage, render/tier
linting, schema/doc drift checks, and command-specific integration coverage; no
C-or-lower or hard-gate findings remain open.

| Everyday command | Current score | Evidence status | Residual risk |
|---|---:|---|---|
| `status` | A | JSON/text/no-git/error cases sampled; machine-stream no-git regression pass; large dirty long-path `git-shaped` probe pass; narrow/no-color text pass; TTY auto/text and explicit JSON pass | Low residual risk. |
| `thread` | A | `thread list --output json` no-git real fixtures pass; public help paths pass; cleanup safety suite pass; narrow/no-color text pass; unknown-thread recovery points at `heddle thread list` | Low residual risk on rare subcommands. |
| `bridge` | A | `bridge import` no-git real fixtures pass; bridge export/import/push/pull/sync tests pass; no-op import/sync text+JSON and divergent-recovery copy are pinned | Runtime `git` subprocesses are forbidden and linted. |
| `diagnose` | A | JSON no-git pass; narrow/no-color text pass; plain-Git baseline and branch-switch coverage pass; recovery shape sampled | Low residual risk. |
| `help` | A | All rubric everyday commands and visible public command paths have `help`, `--help`, and `-h` coverage; typo suggestions and unknown-topic recovery sampled | Low residual risk. |
| `clone` | A | no-git real fixtures pass; local/bare clone path with `PATH=""` pass; unsupported lazy/depth/filter/file-url flags reject cleanly; text completion names next step | Remote-network progress remains an optional long-running polish area. |
| `fsck` | A | `fsck --git --output json` no-git real fixtures pass; narrow/no-color text pass; corrupt ref recovery passes in JSON and text | Low residual risk. |
| `ready` | A | no-git JSON pass; clean machine-stream regression pass; text ready/already-ready no-op pass; stale/heavy-impact coverage exists in multi-agent tests | Low residual risk. |
| `status` / `thread list` workspace view | A | no-git JSON pass; grouped current/stacked/parallel threads covered; promoted to the curated core-loop surface through canonical commands | Low residual risk on very large thread lists. |
| `doctor` | A | `doctor docs` and `doctor schemas` gates clean; narrow/no-color text pass; text/json recovery sampled; docs-doctor unknown flags and unreadable paths pass | Low residual risk. |
| `init` | A | `--repo` path handling fixed and tested; JSON/text init sampled; existing repo/conflicting path failures covered | Low residual risk. |
| `capture` | A | text and JSON sampled; no-git JSON stream regression pass; `--quiet` tip suppression pass; large-deletion destructive safety pass | Low residual risk on platform-specific filesystem errors. |
| `checkpoint` | A | Git-overlay no-git JSON stream regression pass; no-git write-through to branch/index pass; locked-index failure names the skip reason; native refusal is controlled JSON | Runtime `git` subprocesses are forbidden and linted. |
| `log`, `show`, `diff` | A | no-git machine/text stream regression pass; narrow/no-color text pass; `show`/`diff` missing-ID recovery pass; `log` quiet-tip bug fixed | Low residual risk. |
| `start` | A | JSON workflow stream pass; text cd hints, spaced path quoting, non-empty path recovery, and TTY start guidance covered | Low residual risk on rare workspace-mode failures. |
| `merge` | A | JSON preview/apply workflow pass; preview non-mutation pass; conflict/continue/abort coverage exists in overlay matrix; already-applied merge is successful text no-op with clean stderr and unchanged state | Low residual risk. |
| `resolve` | A | Conflict resolution matrix coverage; no-merge JSON and text recovery fixed and tested; abort/list/ours/theirs/manual resolution coverage passes | Low residual risk. |
| `undo` | A | JSON list/preview workflow pass; preview non-mutation pass; actual undo/redo and cross-worktree/refusal regressions pass | Low residual risk. |
| `version` | A | Verbose text/JSON bug context covered; explicit `--repo` handling fixed and tested | Low residual risk. |

## Remaining Work

No release-blocking CLI-rubric work remains for the audited surface.
Non-blocking follow-up work is limited to expanding optional/nightly coverage:
more true-TTY transcripts across non-everyday commands, release-build perf
budgets for ignored perf tests, and network-heavy lazy/hydration fixtures that
are intentionally excluded from the default locked workspace suite.

Schema drift, docs drift, public help reachability, no-`git` overlay workflows,
auto-output, TTY output, corrupt-repo recovery, exit-code, destructive-safety,
and runtime `git` process lint gates are explicit in CI; final release work
should keep them required on protected branches.

Runtime Git-format paths are classified below. Spawning the `git` executable in
production CLI runtime code is forbidden and enforced by
`cargo test -p heddle-cli --test git_process_lint`.

Default `--output auto` is documented by README/CLI help and tested: text on
TTY, JSON when stdout is piped, with `--output text` and `--output json` as
explicit overrides.

## Runtime Git Dependency Inventory

Enforced by `cargo test -p heddle-cli --test git_process_lint`.

The hard gate is no `git` executable dependency for supported Git-overlay
workflows. The default CLI runtime has an empty allowlist for `git`
subprocesses; Git-format operations use native code through Sley.

| Location | Classification | Rationale |
|---|---|---|
| `repo::Repository::git_remote_tracking_status` / `git_overlay_worktree_status` | Native/Sley read path | Remote tracking, HEAD, and worktree/index checks do not invoke the Git binary. |
| `bridge::git_core::resolve_remote_default_branch` | Native/Sley remote default-branch hint | Clone/import use protocol/ref inspection without invoking the Git binary. |
| `bridge::git_core::clone_url_to_bare` filtered clone | Unsupported native capability | Filtered/lazy Git-overlay clones now fail closed instead of shelling out to `git clone`; ordinary local/bare clone no-git workflows are covered by real fixtures. |
| `clone::GitOverlayBlobHydrator::read_blob_bytes` | Local-only native hydration | Local object lookup is attempted first; missing promisor blobs report the native lazy-hydration boundary instead of invoking `git cat-file`. |
| `merge --git-commit` / `merge::git_commit` | Native Git object/ref write | The flag asks Heddle to create a Git commit, and the implementation writes Git objects, index, and refs through native libraries. |
| `operator_core` continue/abort helpers | No-git handoff for raw Git sequencer state | Heddle detects externally-started Git operations and reports preservation/handoff guidance instead of invoking `git rebase --continue`, `git merge --abort`, or similar commands. |
| `oss::cmd_version` | No Git process probe | Verbose version output reports that the Git binary is not required; JSON keeps `git_version=null`. |
| `doctor_schemas::find_repo_root` fallback | Native filesystem root discovery | Schema checks walk ancestor directories directly; no Git process probe is required. |


## Non-Blocking Risks Observed

- Direct `rustfmt` on selected files reports existing let-chain syntax as
  Rust-2024-only in this environment; `cargo test` compiles successfully.
- Real-world fixture extraction prints tar warnings about
  `LIBARCHIVE.xattr.com.apple.provenance`; these are archive metadata warnings
  and did not fail the test.
