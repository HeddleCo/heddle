# Hosted clone: PR #2067 review evidence

Clone retains the setup-time discussion projection and signed originals, and the
context projection with its signed history. Empty discussion spools no longer
repeat ObserveCollaboration. Generic pulls fetch context histories with eight
requests in flight and apply the results in server order.

Native and Git-overlay worktree materialisation runs alongside collaboration
application. Ref publication follows both, then clone intent is cleared. Setup
failures stop the source branch immediately and retain the recovery intent.

Verified descriptor sets are cached in HEDDLE_HOME and their signatures and
validity windows are checked on every use. An expired, malformed, or mismatched
cache fetches the HTTPS set again. Implemented-method inventories are scoped to
the verified endpoint, descriptor document, trust policy, and the intersection
of descriptor and attestation validity. Verified netd routes use the same cache.

Local endpoint binding overlaps HTTPS discovery. Netd adapter binds overlap
identity verification. Relay configuration and authenticated resource/personal
lookups retain their data dependencies on verified routing and a connection.
Per-name cells coalesce resource lookups without holding the entries mutex over
an RPC. A revoked generation cannot be repopulated by an older request.

## Structural proof

The fresh-clone fixture records every peer RPC, including DescribeEndpoint and
ObserveCollaboration. The baseline is the saved binary from `9f0753be`.

| RPC | Before | After, fresh endpoint cache | After, warm endpoint cache |
| --- | ---: | ---: | ---: |
| ResolveResources | 2 | 1 | 1 |
| ObserveThreads | 1 | 1 | 1 |
| ObserveCollaboration | 4 | 2 | 2 |
| DescribeEndpoint | 1 | 1 | 0 |
| Fetch | 1 | 1 | 1 |
| ObserveThread | 1 | 1 | 1 |
| Total | 10 | 7 | 6 |

The warm-cache test verifies that the descriptor HTTPS request is skipped, and
that a cached inventory naming another endpoint triggers discovery again.
Trust tests exercise expired signed sets, invalid caches, root changes, TLS
policy, and region selection.

A discussion mirror lock holds sync unfinished while the worktree completes;
refs remain unpublished until that lock is released. A failing setup RPC returns
while Fetch is gated. A history barrier admits eight requests before answering
any; its failure prevents the ninth request, proving concurrency and its bound.

Linux syscall assertions use strace on isolated child processes. The 242-file
clone reads 316 directory batches and makes 13,877 filesystem syscalls, with
44,208 total calls in the recorded run. Its fixed directory cap accounts for
256 blob shards and metadata; the filesystem budget scales with checkout files.
Network scheduling calls are reported separately rather than asserted as an
exact total.

The 52,515-state installation reads 54 directory batches and makes 62,158 total
syscalls, versus the baseline's 210,110 and 692,325. The 2,048-state assertion
also verifies a constant directory-read budget and a syscall budget that permits
one loose-presence check per State, without repeated directory scans.

Run the external-tool assertions explicitly:

```sh
cargo test --locked -p heddle-cli --features client --test hosted_clone_writes clone_syscall_budget -- --ignored --nocapture
cargo test --locked -p heddle-objects streaming_install_syscall_budget -- --ignored --nocapture
```

## Timing methodology

`clone_timing_fixture` compares the same source and fixture transport with ten
samples per arm at each injected RPC delay. Each pair alternates AB/BA order.
Every clone gets a fresh destination and HEDDLE_HOME. The payload is 242 files
of 160 KiB, 39,649,280 bytes total. Every sample checks payload bytes and HEAD.
Wall time includes process startup and shutdown. p90 uses the nearest rank;
medians are computed from all ten samples in each arm.

Run with an independently built baseline binary, a scratch TMPDIR, and an
isolated Cargo target:

```sh
HEDDLE_CLONE_BASELINE_BINARY=/path/to/baseline/heddle \
HEDDLE_CLONE_BENCH_SAMPLES=10 HEDDLE_CLONE_BENCH_LATENCY_MS=200 \
cargo test --locked -p heddle-cli --features client --test hosted_clone_writes clone_timing_fixture -- --ignored --nocapture
```

| RPC delay | Arm | n | Min (s) | Median (s) | p90 (s) |
| --- | --- | ---: | ---: | ---: | ---: |
| 0 ms | before | 10 | 3.289 | 3.405 | 3.796 |
| 0 ms | after | 10 | 3.291 | 3.481 | 3.772 |
| 200 ms | before | 10 | 4.970 | 5.119 | 5.344 |
| 200 ms | after | 10 | 4.330 | 4.445 | 6.101 |

0 ms raw pairs (seconds): `3.796→3.772`, `3.362→3.679`, `3.347→3.526`, `3.408→3.386`, `3.677→4.695`, `4.140→3.652`, `3.399→3.350`, `3.289→3.295`, `3.403→3.437`, `3.412→3.291`.

200 ms raw pairs (seconds): `5.344→6.101`, `5.373→6.688`, `5.099→4.586`, `5.138→4.438`, `5.222→4.601`, `5.042→4.438`, `5.066→4.452`, `4.970→4.330`, `5.153→4.435`, `5.083→4.367`.

The distribution is mixed: the 0 ms median regresses by 2.2%, while the
200 ms median improves by 13.2%. The 200 ms p90 regresses by 14.2%. These
measurements support the structural RPC reduction; they do not establish a
uniform wall-time improvement. No raw samples were discarded.

## Validation

All runs used fresh HEDDLE_HOME, TMPDIR=/home/scratch, and the isolated target.

```text
cargo nextest run --locked -p heddle-cli --features client --test-threads 4 --no-fail-fast
Summary [1660.786s] 2171 tests run: 2170 passed (3 slow), 1 timed out, 44 skipped

cargo nextest run --locked -p heddle-cli --features client --test-threads 1 -E 'test(hosted_publication_batches_1000_states_and_later_capture)'
Summary [270.779s] 1 test run: 1 passed (1 slow), 2214 skipped

cargo nextest run --locked -p heddle-hosted-client --features client --test-threads 4 --no-fail-fast
Summary [190.572s] 507 tests run: 507 passed (2 slow), 3 skipped

cargo test --locked --workspace --exclude heddle-cli -- --test-threads=4
123 result targets: 4810 passed; 0 failed; 29 ignored; exit=0

cargo clippy --locked --workspace --all-targets --features client -- -D warnings -D dead-code
exit=0
```

The CLI's 1,000-State publication fixture reached its existing six-minute
deadline under concurrent suite load. It passed alone after all suites finished,
without deadline changes; it also passed in both earlier full runs. All 2,171
CLI test names have passing results. The stable workspace replay includes
doctests; an earlier run reached doctests while dependencies were being rebuilt
and could not resolve those artifacts.

```text
cargo run --locked -p heddle-docsgen -- --check
llms.txt / llms-full.txt are up to date
exit=0
```

Surfaces: clone's existing args and help; human and JSON error reporting; Git
materialisation through sley; existing wire messages; gitlink capture, clean
status, deletion, target changes, replacement, and recovery. Populated gitlink
replacement refuses with an actionable message and preserves the contents;
empty directory replacement succeeds. The reflink probe lives in metadata.

Native clone Fetches one Thread's advertised source heads (trees and blobs
for those states, not replay history). Concurrent heads of that Thread stay
listed for `heddle resolve`. Default is the spool's configured
`default_thread`; `--thread` selects a name instead. Unrelated Threads are
not listed or installed; older blobs stay on the remote until `heddle pull`.
If the spool has no default Thread, clone refuses and asks for `--thread`
instead of guessing `main`/`master`/first-published. Measured on this change
against the native hosted fixture:

| RPC | #2067 after, fresh cache | One-thread tip | Configured default (this change) | 64 extra advertised Threads | 8 unique historical blobs dropped from tip |
| --- | ---: | ---: | ---: | ---: | ---: |
| ResolveResources | 1 | 2 | 1 | 1 | 1 |
| ObserveSpool | 0 | 0 | 1 | 1 | 1 |
| ObserveThreads | 1 | 0 | 0 | 0 | 0 |
| ObserveCollaboration | 2 | 2 | 2 | 2 | 2 |
| DescribeEndpoint | 1 | 1 | 1 | 1 | 0 |
| Fetch | 1 | 1 | 1 | 1 | 1 |
| ObserveThread | 1 | 2 | 2 | 2 | 2 |
| Total | 7 | 8 | 8 | 8 | 7 |

The extra ObserveSpool is the configured default Thread; `--thread` still
resolves one name. ObserveThreads is gone from the clone path. Fetch stays 1
with 64 unrelated Threads and with deeper history. Truncating signed
operation ancestry would require a weft/protocol change; tip trees and blobs
are already the Fetch content.
