# Heddle Testing Strategy

This guide maps common change types to the test layer that should catch the
regression. Start with the layer closest to the contract you changed, then add
lower-level or slower coverage only when the risk crosses that boundary.

## Test Layers

| Layer | Main locations | Catches |
| --- | --- | --- |
| Command behavior | `crates/cli/tests/core_functionality/`, `crates/cli/tests/cli_integration/`, `crates/cli/tests/comprehensive/`, `crates/cli/tests/production_features/` | User-visible CLI behavior, exit codes, stdout/stderr split, JSON output, repository state after commands, failure recovery language |
| Command contract lints | `crates/cli/tests/render_lint.rs`, `tier_coverage.rs`, `op_id_coverage.rs`, `idempotency_lint.rs`, `typed_error_lint.rs`, `advice_lint.rs`, `git_process_lint.rs`, `cli_integration/error_envelope_lint.rs`, `cli_integration/stdout_stderr_split.rs`, `cli_integration/doctor_docs.rs` | Drift in repo-wide command conventions that normal behavior tests can miss |
| Object storage and formats | `crates/objects/src/*_tests.rs`, `crates/objects/tests/`, `crates/format/src/*_tests.rs`, `crates/objects/benches/` | Content addressing, object codecs, pack/delta round trips, ignore matching, worktree scans, compression behavior |
| Refs, HEAD, threads, and oplog-adjacent state | `crates/refs/src/refs/*_tests.rs`, `crates/repo/src/repository_tests.rs`, `crates/repo/tests/`, `crates/oplog/src/oplog/*_tests.rs`, CLI tests under `refs_and_history`, `undo_and_special`, and `thread_*` | CAS and expected-old behavior, packed refs, HEAD/thread convergence, undo/redo safety, timeline materialization recovery |
| Git bridge and Sley boundary | `crates/cli/tests/git_bridge_integration.rs`, `roundtrip_fidelity.rs`, `commit_conformance.rs`, `cli_integration/git_*`, `cli_integration/realworld_git.rs`, `tests/bridge_migration_test.sh`, `git_process_lint.rs` | Adopt/export fidelity, Git object SHA preservation, annotated tags, notes, real-world Git shapes, accidental production `git` subprocess use |
| Hosted sync, wire, and auth | `crates/wire/src/auth_tests.rs`, `crates/cli/tests/serve_local.rs`, `crates/cli/tests/presence_publish.rs`, Postgres-gated tests in crates such as `refs` and `oplog` | Wire protocol framing, auth/token behavior, local serve flows, hosted metadata crossing SQL or service boundaries |
| Formal specs and property tests | `specs/quint/*.qnt`, `specs/quint/verify.sh`, `crates/cli/tests/formal_specs.rs` | State-machine invariant drift in merge, refs/HEAD, locks, agents, worktrees, and repository operations |
| Performance fixtures and benches | `docs/perf/`, `crates/cli/tests/cli_integration/perf_core_loop.rs`, `crates/*/benches/` | Gross runtime regressions, allocation or I/O regressions, fixture-backed speedup claims |

## Change Type To Verification

| If you changed... | Add or update... | Run at minimum... |
| --- | --- | --- |
| A new CLI command or subcommand | A command behavior test near the closest existing module; command tier coverage; op-id/idempotency coverage if it mutates state; JSON/stdout tests if it supports machine output | `cargo test -p heddle-cli --test core_functionality <test_name>` or `cargo test -p heddle-cli --test cli_integration <test_name>`, plus any contract lint named by the failing compile/test output |
| CLI rendering, diagnostics, help, or docs snippets | `render_lint`, `stdout_stderr_split`, `error_envelope_lint`, `doctor_docs`, schema snapshots when JSON changes | Focused `cargo test -p heddle-cli --test cli_integration <test_name>` and `heddle doctor docs --all` when markdown commands changed |
| Object IDs, object serialization, tree walking, ignore rules, packfiles, deltas, or compression | Unit tests in `objects` or `format`; zstd-enabled pack/delta coverage when compression is involved | `cargo test -p heddle-objects`, `cargo test -p heddle-format`, and `cargo test -p heddle-objects --features zstd` when pack/delta compression changed |
| Refs, HEAD, packed refs, thread records, undo/redo records, or timeline materialization | `refs`/`repo` tests close to the invariant; CLI integration only for user-visible recovery text; formal specs when guards or transitions change | `cargo test -p heddle-refs`, `cargo test -p heddle-repo`, focused CLI test, and `./specs/quint/verify.sh` for modeled state machines |
| Race, crash, or fault recovery behavior | Deterministic interleaving or fault-injection tests first; slow soak tests only behind `#[ignore]` | The focused crate test plus the matching CLI fault/recovery test if users see the outcome |
| Git import/export, Git overlay remotes, Sley integration, or Git compatibility claims | Git bridge, round-trip fidelity, conformance, real-world fixture, and `git_process_lint` coverage | `cargo test -p heddle-cli --test git_bridge_integration`, focused `cli_integration::git_*` test, and `cargo test -p heddle-cli --test git_process_lint` |
| Hosted sync, auth, local serve, Postgres-backed refs/oplog, or server-shaped metadata | Wire/auth tests for pure protocol behavior; ignored Postgres integration when SQL semantics matter | `cargo test -p heddle-wire`, focused CLI hosted test, and service-backed `--features postgres -- --ignored` only when the change crosses SQL/service boundaries |
| Merge, lock, refs/HEAD, agent, worktree, or repository-operation invariants | The relevant Quint spec and Rust property mirror | `./specs/quint/verify.sh`; use `./specs/quint/verify.sh --thorough` before landing high-risk invariant work |
| Performance-sensitive code | A small default smoke test if behavior changed; an ignored release fixture or bench for the claimed workload; docs/perf notes naming fixture and metric | Focused default test, then the fixture command that measures the claim, such as `cargo test --release -p heddle-cli --test cli_integration core_loop_command_surface_perf_smoke -- --ignored --nocapture` |
| A repo-wide convention | A lint test with an explainable allowlist, not a scattered set of example tests | The lint itself and one representative behavior test proving the convention matters |

## New Command Test Checklist

Use this checklist when adding or reshaping a command.

- Put everyday command behavior in `crates/cli/tests/core_functionality/` when
  it fits an existing area. Use `crates/cli/tests/cli_integration/` for command
  contracts, matrix behavior, JSON/error envelopes, Git overlay interop, and
  fixtures shared across commands.
- Build the fixture with `TempDir` and run the binary through
  `CARGO_BIN_EXE_heddle`. Pin `HEDDLE_PRINCIPAL_NAME` and
  `HEDDLE_PRINCIPAL_EMAIL` when the command can capture source history.
- Assert the durable result, not only stdout. Re-open the repository, inspect
  refs, read object state, or run a follow-up command when that is the user
  contract.
- Cover one happy path and one meaningful failure path. The failure path should
  prove the command reports the right problem and does not leave partial state
  behind.
- If the command mutates state, thread `--op-id` and add idempotency coverage or
  update the allowlist with an explicit reason.
- If the command supports `--output json`, assert valid JSON, stdout-only
  machine output, and diagnostics on stderr.
- If the command adds help text, examples, schemas, exit codes, or docs
  snippets, update the matching lint or generated snapshot in the same change.
- Prefer small fixtures. Move large matrices, real-world repositories,
  service-backed flows, or timing budgets behind `#[ignore]`.

Minimal shape:

```rust
#[test]
fn command_does_the_user_visible_thing() {
    let temp = tempfile::TempDir::new().unwrap();
    let dir = temp.path();

    let output = heddle_output(&["command", "--output", "json"], Some(dir))
        .expect("command succeeds");

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["status"], "ok");

    let repo = repo::Repository::open(dir).expect("repo opens after command");
    assert!(repo.some_contract().expect("contract can be checked"));
}
```

The helper names vary by module. Copy the local harness instead of importing a
distant one just to save a few lines.

## Performance-Sensitive Change Checklist

Performance work is complete only when the claim names the workload and the
metric.

- Name the fixture shape: for example, many small files, deep linear history,
  wide ref set, large blobs, Git overlay import/export, hosted pack transfer, or
  semantic merge.
- Record the baseline command, metric, and environment before changing code.
  Useful metrics include wall time, peak RSS, object count, pack count, bytes
  read/written, and cold vs warm cache behavior.
- Keep the normal `cargo test` path small and deterministic. A default smoke
  test should catch obvious behavior or order-of-magnitude regressions, not
  enforce noisy wall-clock budgets.
- Put release-mode, large-fixture, public-repo, service-backed, or soak
  benchmarks behind `#[ignore]`, `--release`, or an explicit environment flag.
- Store reusable fixture definitions or notes under `docs/perf/` when future
  workers need to repeat the measurement.
- In the PR, capture, or issue comment, write: fixture, command, baseline,
  result, and which metric improved or stayed within budget.

Template for a performance note:

```text
Fixture:
Command:
Mode: default smoke | ignored release test | bench | manual profile
Metric:
Baseline:
After:
Budget or expected direction:
Why this stays default/ignored:
```

## Ignored, Nightly, And Fixture-Heavy Tests

Default tests should be deterministic, local, small, and useful during normal
development. Use ignored or scheduled jobs when a test is valuable but too
expensive or environment-sensitive for that loop.

Use `#[ignore]` for tests that require any of the following:

- A release build or wall-clock budget.
- A real external service, such as Postgres through `DATABASE_URL`.
- Network access or public repositories, such as large Git hydration fixtures.
- OS-specific kernel integrations such as FUSE, FSKit, ProjFS, or NFS smoke
  behavior.
- Large generated fixtures, multi-GB-class blobs, long soak loops, or flaky
  timing assumptions.

Every ignored test should state why it is ignored and include the command to run
it. Prefer:

```rust
#[ignore = "release-mode command-surface perf smoke; run with `cargo test --release -p heddle-cli --test cli_integration core_loop_command_surface_perf_smoke -- --ignored --nocapture`"]
```

Use `--ignored` when you want only the heavy suite. Use `--include-ignored`
when an acceptance or nightly job intentionally runs both default and ignored
tests. Do not hide missing ordinary behavior coverage behind an ignored test;
the default suite still needs a small regression check for the same contract.

## Practical Verification Loop

For most changes:

1. Run the narrowest test that proves the edited contract.
2. Run the relevant lint or spec if the change touches a repo-wide convention or
   modeled state machine.
3. Run `cargo fmt --check` and `cargo clippy -- -D warnings` before broad review
   when code changed.
4. Run `cargo test` or `cargo test --workspace` when the blast radius crosses
   crate boundaries.
5. Run ignored, release, fixture-heavy, or service-backed tests only when the
   change touches that boundary or makes a claim they measure.

For docs-only changes, verify the edited markdown and run `heddle doctor docs
--all` when command snippets changed. A full Rust build is not required for pure
testing-strategy prose.
