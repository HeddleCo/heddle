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

## Release Regression Contract

Run the Wave 0 release contract with:

```sh
TMPDIR=/home/scratch \
cargo test --release -p heddle-cli --test cli_integration \
  core_loop_release_contract -- --ignored --nocapture
```

Compilation happens before the test process starts. The harness also reports
fixture construction (`SETUP`) separately, before starting any command sample.
It prints the runner fingerprint and p50/p95/p99/max for end-to-end wall time,
profile total, process startup, warm repository work, monitor startup,
rendering, and network work. Structural counters use the same percentiles.

Wave 0 covers `--version`, `help`, clean status, one-dirty-path status, and a
one-path capture. Repository fixtures contain 10k and 100k paths. Version and
help enforce the cold-process band; repository commands run after two explicit
native-monitor warmups. The versioned, runner-scoped baselines and target gaps
live in `docs/perf/cli-core-loop-baseline.json`. CI selects the controlled
Blacksmith profile with `HEDDLE_PERF_BASELINE`; local runs use the recorded
local calibration profile.

The ignored marker keeps the harness out of debug and ordinary unit-test runs;
the dedicated release workflow runs it on a controlled runner. Absolute bands
already met use the product target. Missed bands use a baseline ratchet and keep
the target gap explicit.

The three repeatable negative controls are:

```sh
HEDDLE_PERF_NEGATIVE_CONTROL=latency HEDDLE_PERF_SAMPLES=5 <command-above>
HEDDLE_PERF_NEGATIVE_CONTROL=full-scan HEDDLE_PERF_SAMPLES=5 <command-above>
HEDDLE_PERF_NEGATIVE_CONTROL=duplicate-open HEDDLE_PERF_SAMPLES=5 <command-above>
```

They respectively inject 50 ms into the timed window, disable the monitor and
remove the warm index before every sample, and execute a second repository open
inside each sample. Each invocation must fail with `PERF GATE RED`.
