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
profile total, process startup, warm repository work, repository open,
verification, worktree status, snapshot subphases, monitor startup, rendering,
and network work. Structural counters use the same percentiles.

Wave 0 covers `--version`, `help`, clean status, one-dirty-path status, a
one-path capture, one-path diff, a 20-entry log over 1,000 ancestors, and a
1,000-thread list. Repository fixtures contain 10k and 100k paths. Version and
help enforce the cold-process band; repository commands run after two explicit
native-monitor warmups. The versioned, runner-scoped cold-process and scale
baselines live in `docs/perf/cli-core-loop-baseline.json`. CI selects the
controlled Blacksmith profile with `HEDDLE_PERF_BASELINE`; local runs use the
recorded local calibration profile.

The ignored marker keeps the harness out of debug and ordinary unit-test runs;
the dedicated release workflow runs it on a controlled runner. Every bounded
100k local command has a fail-loud absolute p95 gate independent of the runner
baseline: clean status is 50 ms, one-path status is 75 ms, and one-path durable
capture, diff, bounded log, and bounded thread-list are each 100 ms. Structural
directory, hash, object-decode, history-walk, repository-open, and zero-network
gates remain in force alongside the latency contract.

The repeatable negative controls are:

```sh
HEDDLE_PERF_NEGATIVE_CONTROL=latency HEDDLE_PERF_SAMPLES=5 <command-above>
HEDDLE_PERF_NEGATIVE_CONTROL=full-scan HEDDLE_PERF_SAMPLES=5 <command-above>
HEDDLE_PERF_NEGATIVE_CONTROL=subtree-skip HEDDLE_PERF_SAMPLES=5 <command-above>
HEDDLE_PERF_NEGATIVE_CONTROL=eager-pack-index HEDDLE_PERF_SAMPLES=5 <command-above>
HEDDLE_PERF_NEGATIVE_CONTROL=duplicate-open HEDDLE_PERF_SAMPLES=5 <command-above>
```

They respectively inject 50 ms into the timed window, disable the monitor and
remove the warm index before every sample, deliberately disable subtree and
unchanged-child skips while retaining the warm index, eagerly build the global
packed-object location map during every repository open, and execute a second
repository open inside each sample. Each invocation must fail with `PERF GATE
RED`. The eager-index control is pinned by the warm repository-open p95 gate
(2 ms), not only by the aggregate wall-time band.

## 2026-08-15 local calibration

The before column is a fresh-main five-sample calibration on the same host.
The after column is the final 20-sample release contract. Status consumes hot
directory proofs; capture applies an authoritative one-file monitor delta to a
cached, hash-validated tree chain and commits the complete snapshot closure in
one durable pack.

| Case (100k paths) | Before p95 | After p95 | Absolute gate |
|---|---:|---:|---:|
| Clean status | 12.815 ms | 11.617 ms | 50 ms |
| One-path status | 164.556 ms | 61.843 ms | 75 ms |
| One-path durable capture | 1045.373 ms | 49.674 ms | 100 ms |
| One-path diff | 354.053 ms | 62.965 ms | 100 ms |
| Bounded log (20 of 1,000) | 20.724 ms | 10.316 ms | 100 ms |
| Bounded thread-list (1,000) | 332.113 ms | 72.622 ms | 100 ms |

Repository open measured 0 ms throughout, and no measured command initialized
a network client. Clean and one-path status scanned 2 and 6 directories;
capture used the direct tree rewrite and hashed only the changed file. Disabling
subtree and unchanged-child reuse is a repeatable red control: at 100k paths its
five-sample p95 was 501.425 ms for status and 1557.785 ms for capture, exceeding
the 75/100 ms gates while scanning 2,004 directories.
