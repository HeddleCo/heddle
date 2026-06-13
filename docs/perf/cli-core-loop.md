# CLI Core Loop Performance

The CLI has two developer-facing performance tools for the everyday command
loop.

## Command Profile

Set `HEDDLE_PROFILE=1` to print command timings to stderr:

```sh
HEDDLE_PROFILE=1 heddle status --output json
```

The command's normal stdout is unchanged, so JSON output remains parseable.
Top-level profiles include config load, logging init, command body, and total
wall-clock time. Some commands also emit command-specific phases; `status`
reports repository open, state lookup, operation lookup, remote tracking,
import hints, and worktree scan time.

Use this when a real repository feels slow and the next move is unclear. The
phase split should make it obvious whether to inspect startup/config overhead,
worktree scanning, ref/thread summary work, Git subprocess fallbacks, or
rendering.

## Release Smoke Benchmark

Run the command-surface smoke benchmark with:

```sh
cargo test --release -p heddle-cli --test cli_integration \
  core_loop_command_surface_perf_smoke -- --ignored --nocapture
```

The fixture creates a small native repository, captures a baseline, creates
multiple threads, leaves one dirty file, then times the core loop:

- `heddle`
- `heddle help`
- `heddle help --output json`
- `heddle status`
- `heddle status --short`
- `heddle status --output json`
- `heddle status --output json`
- `heddle thread list --output json`
- `heddle log --output json`
- `heddle diff --output json`
- `heddle ready --output json`

The test is ignored by default because release builds and wall-clock budgets are
too expensive and environment-sensitive for the normal test loop. In release
mode, each command has a generous budget intended to catch obvious regressions,
not tiny variance.
