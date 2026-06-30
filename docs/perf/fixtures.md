# Performance Fixture Suite

Status: scaffold. This page defines the shared fixture vocabulary for future
streaming, transfer, and command-loop performance work. Only the smoke fixture
in `crates/cli/tests/performance.rs` is executable by default today; heavier
rows are opt-in fixture contracts until a benchmark or ignored test lands.

## Fixture Shapes

| Fixture | Status | Shape | Default scale | Heavy gate |
|---|---|---|---:|---|
| `native-core-smoke` | Shipped smoke | Small native repository combining many small files, several multi-MB blobs, short linear history, several refs, and a rename/move. | 24 small files, 3 x 2 MiB blobs, 4 user snapshots, 8 extra refs | None; normal `cargo test` |
| `many-small-files` | Planned harness | Snapshot, status, pack, and transfer paths over thousands to millions of tiny source files spread across directories. | Covered only by smoke subset | `--ignored` or Criterion; `HEDDLE_PERF_FIXTURE_SCALE=large` |
| `multi-mb-blobs` | Planned harness | Few large binary blobs sized to local limits; multi-GB-class should be scaled down unless run on dedicated hardware. | Covered by three 2 MiB smoke blobs | Release/nightly; `HEDDLE_PERF_LARGE_BLOB_BYTES` |
| `deep-history` | Planned harness | Long linear history with small deltas, plus log/goto/sync traversals. | Covered by 4 user snapshots | `--ignored`; release mode for budgets |
| `many-refs` | Planned harness | Wide local thread, marker, and remote-ref sets that exercise packed refs and summaries. | Covered by 8 extra thread refs | Criterion or `--ignored`; `HEDDLE_PERF_REF_COUNT` |
| `rename-move` | Planned harness | File and directory moves with mostly unchanged content, used by diff, merge, Git bridge, and semantic rename detection. | Covered by one file move | `--ignored` for large trees |
| `git-overlay-import-export` | Foundation in place | Import a Git-shaped source, export back through the bridge, and compare object/ref/mapping behavior. | Not in smoke | `--ignored`; local Git/Sley fixture; no network by default |
| `hosted-native-sync` | Foundation in place | Transfer a native Heddle repository through hosted sync surfaces and compare native pack/object closure behavior. | Not in smoke | Explicit hosted target env; release/nightly only |
| `semantic-merge` | Foundation in place | Function-level merge and semantic diff workloads over generated source files. Existing Criterion notes live in `semantic-merge.md`. | Not in smoke | Criterion; semantic feature; optional baselines |

The default smoke fixture exists to keep the suite executable and cheap. It is
not a performance baseline and should not be used to claim a speedup.

## Metrics

Every executable fixture should emit a JSON-ish record with these fields when
available:

| Metric | Meaning |
|---|---|
| `wall_time_ms` | End-to-end elapsed time for the named phase, measured with `Instant`. Split setup from measured operation when possible. |
| `peak_rss_bytes` | Best-effort process peak RSS. It may be absent on platforms without a cheap API. |
| `object_count` | Heddle object count after setup: blobs + trees + states + actions unless the fixture states otherwise. |
| `pack_count` | Count of `.heddle/packs/*.pack` files after setup or after the measured operation. |
| `bytes_read` | Transport or filesystem bytes read by the measured phase when instrumentation exists; otherwise logical fixture payload bytes read. |
| `bytes_written` | Transport or filesystem bytes written by the measured phase when instrumentation exists; otherwise logical fixture payload bytes written. |
| `cache_mode` | `cold`, `warm`, or `both`. Cold means Heddle in-process caches were cleared or the process was restarted; OS page-cache cold runs require dedicated hardware notes. |
| `syscall_count` | Optional. Capture only when `strace`, `dtrace`, or platform tooling is cheap and stable enough for the runner. |

Suggested record shape:

```json
{
  "schema": "heddle-perf-fixture/v1",
  "fixture_id": "native-core-smoke",
  "shape": ["many-small-files", "multi-mb-blobs", "deep-history", "many-refs", "rename-move"],
  "scale": "smoke",
  "gate": "default",
  "metrics": {
    "wall_time_ms": 0,
    "peak_rss_bytes": null,
    "object_count": 0,
    "pack_count": 0,
    "bytes_read": 0,
    "bytes_written": 0,
    "cache_mode": "both"
  }
}
```

## Gating Rules

- Normal `cargo test` may only build smoke-scale fixtures.
- Heavy local fixtures must be `#[ignore]`, Criterion benches, release-only
  smoke tests, or guarded by explicit env such as
  `HEDDLE_PERF_FIXTURE_SCALE=large`.
- Networked or hosted fixtures must require an explicit target env and should
  skip clearly when credentials or endpoints are absent.
- Baseline JSON should be committed only for stable, repeatable harnesses.
- Any claimed speedup must name the fixture, metric, scale, cache mode, and
  command used to collect it.
