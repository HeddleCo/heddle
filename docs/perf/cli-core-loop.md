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
wall-clock time. Some commands also emit command-specific timings.

Set `HEDDLE_PROFILE=jsonl` to write one structured JSON line to stderr:

```sh
HEDDLE_PROFILE=jsonl heddle status --output json
```

The JSONL trace uses static command and phase names with numeric metrics only.
It must not include paths, argv, object ids, remote URLs, environment variables,
or filenames. This makes it safe to collect while preserving stdout for normal
machine output.

The current named phase coverage includes:

- `status`: repository open, current state, operation, remote tracking, import
  hints, Git overlay status and health, verification, Git index, worktree
  status, thread summaries, parallel thread state, late state, materialized
  threads, advice, build total, render, and detailed worktree scanner counters.
- `thread list`: summary collection, repository verification, and command body.
- `verify`: plain-Git probe, repository open, repository checks, and command
  body.

Use this when a real repository feels slow and the next move is unclear. The
phase split should make it obvious whether to inspect startup/config overhead,
worktree scanning, ref/thread summary work, Sley-backed Git engine work, or
rendering.
Sley-backed Git engine work should show up inside the command-specific phases
rather than as a hidden subprocess floor.

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
too expensive and environment-sensitive for the normal test loop. Treat it as a
manual smoke check for obvious command-loop regressions rather than a CI gate or
a claim about expected speed.
