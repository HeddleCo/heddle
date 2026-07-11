# Multi-host perf matrix (living table)

**Status:** **Open** — only single-host stamps exist. Fill rows when hosts
complete [`MULTI_HOST_PERF.md`](MULTI_HOST_PERF.md). Do not mark multi-host
certified until ≥2 hosts share one commit with checked-in artifacts.

**Not a claim:** empty or single-host rows are not multi-host cert.

Primary single-host reference (not multi-host alone):

| Host ID | OS / arch | CPU | Commit | Stamp | n | A==B | status_json median_ms | log_json median_ms | diff_json median_ms | thread_list_json median_ms | Notes |
|---------|-----------|-----|--------|-------|:-:|:----:|----------------------:|-------------------:|--------------------:|---------------------------:|-------|
| dogfood-m1pro | macOS 26.5.1 arm64 | Apple M1 Pro | `34c101ea951358120e6d2f13b22f4551c2845df2` | `20260711T210616Z` | 5 | yes | 52.9 | 11.4 | 20.6 | 26.4 | Primary `PERF_BASELINE`; single-host only |

## Open slots

| Host ID | OS / arch | CPU | Commit | Stamp | n | A==B | status_json median_ms | log_json median_ms | diff_json median_ms | thread_list_json median_ms | Notes |
|---------|-----------|-----|--------|-------|:-:|:----:|----------------------:|-------------------:|--------------------:|---------------------------:|-------|
| *(host-b)* | | | | | | | | | | | **open** |
| *(host-c optional)* | | | | | | | | | | | **optional** |

## Artifact index (when filled)

| Stamp | Host | Absolute JSON | Environment | Paired |
|-------|------|---------------|-------------|--------|
| `20260711T210616Z` | dogfood-m1pro | `artifacts/perf/20260711T210616Z-core-loop-absolute.json` | `…-environment.txt` | `…-core-loop-paired-*.json` |

---

## Closure rule

Multi-host measurement residual is **closed** only when `MULTI_HOST_PERF.md`
pass criteria are met and this table has ≥2 complete rows on the same commit.
