# Multi-host equal-work performance matrix (prep)

**Status:** **Prep / open** — harness and single-host stamps exist; a multi-host
matrix is **not** certified until ≥2 independent hosts complete this recipe and
artifacts are checked in under `artifacts/perf/`.

**Not a claim:** completing this doc does **not** assert multi-host cert,
Git wins, or cross-tool superiority.

Cross-check: [`PERF_BASELINE.md`](PERF_BASELINE.md), [`RELEASE_GATES.md`](RELEASE_GATES.md)
G5, [`PLATFORM_MATRIX.md`](PLATFORM_MATRIX.md), `scripts/program/core-loop-bench.sh`.

---

## Why multi-host

Single-host n=5 stamps (including A==B self-pairs) calibrate harness noise and
tip medians on one machine class. External speed claims and Wave 6 “multi-host
open” residual need **independent hosts** so host noise, OS, and load cannot
be mistaken for product regression or win.

Minimum useful matrix:

| Role | Example | Purpose |
|------|---------|---------|
| **Host A** | Primary dogfood (e.g. macOS arm64 M-class) | Continuity with existing `PERF_BASELINE` stamps |
| **Host B** | Linux arm64 or x86_64 CI-class / quiet lab machine | Second OS + CPU microarchitecture |
| **Optional Host C** | Quieter same-class as A, or Windows only if product elevates | Noise control or platform residual |

Wave 7 does **not** require Windows multi-host perf for green; Windows remains
mount-foundation unless product raises the bar.

---

## Equal-work rules (non-negotiable)

Same rules as `PERF_BASELINE.md` / G5:

1. **Equal fixture** — `core-loop-bench.sh` defaults (300 files, 24 threads, one dirty).
2. **Require success** — non-zero exit aborts; no timing failed work.
3. **Release binary** — `cargo build --release -p heddle-cli --locked --features client`.
4. **n ≥ 5 timed trials + ≥1 warmup** for cert-oriented samples.
5. **A==B self-pairs** preferred (omit `--no-paired` unless host time budget forces absolute-only).
6. **No Git comparison** in this matrix unless a separate equal-work Git protocol is explicitly designed later.
7. **Same commit SHA** across hosts for a single matrix row (rebuild on each host from the same git tip).

---

## Per-host recipe

On **each** host, from a clean checkout of the **same** commit:

```bash
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"   # or host-equivalent
cd /path/to/heddle

# Isolated target (avoid filling the worktree; parallel-safe)
export CARGO_TARGET_DIR="/tmp/heddle-mh-perf-${USER}-$(uname -s)-$(uname -m)"
cargo build --release -p heddle-cli --locked --features client

# Capture host identity before the bench
HOST_ID="$(hostname -s 2>/dev/null || hostname)"
STAMP_PREFIX="$(date -u +%Y%m%dT%H%M%SZ)"
OUT_DIR="$PWD/artifacts/perf"
mkdir -p "$OUT_DIR"

# Optional: write a human host card the template expects
cat > "$OUT_DIR/${STAMP_PREFIX}-${HOST_ID}-host-card.txt" <<EOF
host_id=${HOST_ID}
uname=$(uname -a)
commit=$(git rev-parse HEAD)
branch=$(git rev-parse --abbrev-ref HEAD)
rustc=$(rustc --version)
cargo=$(cargo --version)
cpu_note=<fill: model / cores>
mem_note=<fill: total RAM>
load_note=<fill: 1m load at start; quiet? concurrent builds?>
os_note=<fill: macOS/Linux version>
EOF

bash scripts/program/core-loop-bench.sh \
  --heddle "$CARGO_TARGET_DIR/release/heddle" \
  --trials 5 \
  --warmup 1 \
  --out-dir "$OUT_DIR"
```

Expected outputs (names from harness; stamp is harness-generated UTC):

- `artifacts/perf/<stamp>-core-loop-absolute.json`
- `artifacts/perf/<stamp>-environment.txt`
- `artifacts/perf/<stamp>-core-loop-paired-*.json` (if A==B ran)
- Plus the hand-written `*-host-card.txt` above

**Copy/commit policy:** check in JSON + environment + host card under
`artifacts/perf/`. Do not invent timings. Prefer one commit that lands all
hosts for the same git tip when possible.

---

## Matrix row template

Copy into `docs/program/MULTI_HOST_PERF_MATRIX.md` (or a dated section) when
hosts complete. Empty = not certified.

| Host ID | OS / arch | CPU | Commit | Stamp | n | A==B | status_json median_ms | status_json p95_ms | Notes |
|---------|-----------|-----|--------|-------|:-:|:----:|----------------------:|-------------------:|-------|
| *(example)* dogfood-m1 | macOS arm64 | M1 Pro | `34c101ea…` | `20260711T210616Z` | 5 | yes | 52.9 | 54.1 | Single-host primary; not multi-host alone |
| host-b | | | | | | | | | **open** |
| host-c | | | | | | | | | **optional** |

### Pass criteria for “multi-host measurement residual closed”

All of:

1. **≥2 hosts** completed the recipe on the **same commit**.
2. Artifacts present under `artifacts/perf/` for each host (absolute + env; A==B preferred).
3. Matrix table filled with medians for at least: `status_json`, `log_json`, `diff_json`, `thread_list_json`.
4. `PERF_BASELINE.md` gains a **Multi-host** section pointing at the stamps (still no Git win language).
5. `PLATFORM_MATRIX.md` Wave 6 multi-host checkbox checked with evidence links.

**Still not a win claim:** cross-host median differences are host noise unless
an equal-work paired A/B (two builds) is run *per host* for a hotspot change.

---

## Hotspot code changes (interaction)

If a Wave 6 **code** optimization lands:

1. Run this multi-host recipe **before and after** (or paired A/B binary paths
   via `paired-bench.py`) on **at least one** quiet host.
2. Prefer re-running Host A + Host B on the post-change tip.
3. Never claim a win from weaker durability, skipped validation, or unequal fixtures.

---

## Operator checklist (executable)

- [ ] Pick commit tip; freeze SHA for the matrix row.
- [ ] Host A: build release, quiet load if possible, n=5 + A==B, host card.
- [ ] Host B: same commit, same recipe, host card.
- [ ] Optional Host C.
- [ ] Land artifacts + fill matrix table.
- [ ] Update `PERF_BASELINE.md` multi-host section + `PLATFORM_MATRIX` checkbox.
- [ ] Explicitly **not** claiming Git win / external marketing numbers without quieter multi-host agreement.

---

## Related harness

| Piece | Path |
|-------|------|
| Absolute + A==B runner | `scripts/program/core-loop-bench.sh` |
| Manual paired A/B | `scripts/program/paired-bench.py` |
| Single-host baseline | `docs/program/PERF_BASELINE.md` |
| Manifest perf suite | `scripts/program/manifest.toml` `suite=perf` |
| Matrix stub file | `docs/program/MULTI_HOST_PERF_MATRIX.md` |
