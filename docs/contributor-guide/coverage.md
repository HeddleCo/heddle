# Per-crate coverage policy

Heddle enforces a **per-crate line-coverage floor**. The gate is
per-crate (not workspace-global) so a low-coverage crate can't be
masked by high-coverage neighbours. This page is the canonical source
for the policy and the per-crate numbers; `codecov.yml`,
`.github/workflows/rust-tests.yml`, and the `CONTRIBUTING.md` summary
all point here.

## How the gate fails CI

The scheduled/manual `Coverage` job in
[`.github/workflows/rust-tests.yml`](../../.github/workflows/rust-tests.yml)
runs `cargo llvm-cov` over the OSS feature set
(`git-overlay,native,semantic,zstd`) to produce `lcov.info`, then runs:

```bash
cargo run -p heddle-devtools --quiet -- \
  audit-coverage lcov.info \
    --gate objects=80 \
    --gate refs=80 \
    --gate repo=85 \
    --gate cli=75 \
    --gate mount=68 \
    --gate semantic=80 \
    --gate oplog=78 \
    --gate wire=80 \
    --gate state_review=80 \
    --gate crypto=80 \
    --gate daemon=80 \
    --gate ingest=80
```

`audit-coverage` aggregates `LF:`/`LH:` lcov records by workspace crate
(matched on `crates/<name>/`) and **exits non-zero** when any gated
crate is below its threshold. The step runs *before* the Codecov
upload, so the scheduled/manual coverage run goes red whether or not
Codecov is reachable. This is the **coverage gate of record**, but it is
intentionally kept off the hot pull-request and push path.

Codecov mirrors the same floors as per-crate `coverage.status.project`
entries in [`codecov.yml`](../../codecov.yml) (`threshold: 0%`, so any
drop fails the status on runs that upload coverage). Codecov is useful
for trend visibility, but it is not the gate of record — the in-CI step
is.

## Floors vs. goals

Two numbers per crate:

- **Floor** — the enforced `--gate`/`target` value. Dropping below it
  fails CI. Pinned at or just under current `main` coverage, rounded
  down to a whole percent so normal measurement jitter doesn't flap.
- **Goal** — the coverage we want the crate to reach. Where the goal is
  above current coverage, the floor is *ratcheted* (pinned near current)
  and the goal is documented here. Raise the floor toward the goal in
  the same PR that adds the tests pushing coverage up — never ahead of
  the tests, or you turn `main` red.

| Crate | Floor (enforced) | Goal | Notes |
|---|---|---|---|
| `objects` | 80% | 80% | at goal |
| `refs` | 80% | 80% | at goal |
| `repo` | 85% | 85% | at goal |
| `cli` | 75% | 75% | at goal |
| `mount` | 68% | 70% | ratchet — platform-gated (FUSE/projfs) |
| `semantic` | 80% | 80% | at goal |
| `oplog` | 78% | 80% | ratchet — current ≈ 78.3% |
| `wire` | 80% | 80% | at goal |
| `state_review` | 80% | 80% | at goal |
| `crypto` | 80% | 80% | at goal |
| `daemon` | 80% | 80% | at goal |
| `ingest` | 80% | 80% | at goal |

Generated hosted API bindings are built and tested in `HeddleCo/api`; they are
not an in-tree crate and are therefore outside this repository's coverage
gate. Heddle's adapter behavior remains covered by the consuming crates.

## Running the gate locally

```bash
cargo llvm-cov --locked --workspace \
  --features git-overlay,native,semantic,zstd \
  --lcov --output-path lcov.info

cargo run -p heddle-devtools --quiet -- \
  audit-coverage lcov.info --gate repo=85 --gate cli=75   # …etc
```

A green local run means a green CI run, modulo lcov's normal sensitivity
to feature flags. Pass `--gate <crate>=0` for a crate to read its current
percentage without enforcing anything.

## Requesting a target bump

The floors are deliberately conservative; raising one is encouraged.

1. **Add the tests** that move the crate's coverage above the new floor.
2. In the **same PR**, raise the number in all three places — they must
   agree or the in-CI gate and the Codecov status will diverge:
   - the `--gate <crate>=<pct>` flag in
     [`.github/workflows/rust-tests.yml`](../../.github/workflows/rust-tests.yml),
   - the `coverage.status.project.<crate>.target` in
     [`codecov.yml`](../../codecov.yml),
   - the **Floor** column in the table above.
3. Run the gate locally (above) to confirm the new floor passes before
   pushing.
4. To add a *new* crate to the gate, give it an entry in all three
   places. Confirm it actually has lines under `crates/<name>/` in
   `lcov.info` first — crates whose code is generated into `OUT_DIR`
   (like `grpc`) can't be line-gated.

Lowering a floor is a regression and should be rare — it needs a reason
in the PR description (e.g. a crate's tested surface was intentionally
removed). The CI gate and Codecov status both block silent drift.
