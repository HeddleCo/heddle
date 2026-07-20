# Heddle-native VC-Bench results

Run date: 2026-07-20

## Provenance

| Component | Value |
| --- | --- |
| VC-Bench revision | `d1c071b7a07a16e2bf7ec85055ed75456f75e1ed` |
| Heddle source revision | `43b958483307d73e1928a6eaa463fd62c656cebd` |
| Heddle version | `0.10.4` |
| Heddle binary SHA-256 | `f0fff6c5a508a18354bac2b02226ca33d6e65c934956178a140ec9c92de09276` |
| Codex CLI | `0.144.6` |
| Configured/observed model | `gpt-5.5` |
| Trials | `k=1`, six tasks per lane |

## Headline

Both lanes passed all six deterministic upstream verifiers.

| Lane | Pass | Mean wall | Median wall | Mean task VC commands | Mean failed task VC commands | Mean task VC runtime | Mean tokens |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Git | 6/6 | 79.7 s | 100.3 s | 18.5 | 0.7 | 0.678 s | 25,560 |
| Heddle native | 6/6 | 256.7 s | 280.0 s | 57.7 | 3.2 | 3.984 s | 74,068 |

Relative to Git, the native lane used 222% more wall time and 211.7% more
task-level version-control commands. The paired mean wall-time delta was
177.0 seconds (task-clustered 95% CI: 89.0 to 265.0 seconds). With only one
trial per cell, treat these as directional measurements.

## Per task

| Scenario | Git | Heddle native | Native delta | Git commands | Native commands |
| --- | ---: | ---: | ---: | ---: | ---: |
| Selective commit | 105.4 s | 131.6 s | +24.9% | 21 | 24 |
| Multi-amend | 99.3 s | 300.0 s | +202.3% | 24 | 64 |
| Split commit | 100.3 s | 280.0 s | +179.2% | 28 | 62 |
| Reorder commits | 42.1 s | 279.8 s | +565.1% | 9 | 72 |
| Squash commits | 30.3 s | 183.6 s | +505.1% | 11 | 40 |
| Update dirty branch | 100.8 s | 365.0 s | +261.9% | 18 | 84 |

## Interpretation

The native lane's measured Heddle subprocess time averaged only 3.984 seconds
per task, versus 256.7 seconds of agent wall time. Most of the gap therefore
came from agent discovery and workflow composition: 45.7 native inspection
commands per task versus 11.7 for Git, plus attempts at familiar but absent or
differently-shaped operations.

The most visible missing or hard-to-discover workflows were:

- amend several earlier states while preserving their intent;
- selectively capture paths/hunks from a dirty worktree;
- split, reorder, and squash states directly;
- refresh an adopted thread after its base changes;
- discover valid JSON output modes and state-to-state path diff syntax.

The result is encouraging on expressiveness—all six final states were correct—but
it identifies agent-facing porcelain and help/prompt material as a much larger
near-term opportunity than transport tuning for this workload.

## Run notes

The first native selective-commit attempt was excluded because the configured
model returned an external capacity error. Its partially modified workspace and
result were retained separately, and that one cell was rerun with the identical
model, prompt, fixture, harness, and timeout. The replacement passed and is the
only selective native sample in the aggregate above.

The harness metrics self-test passed all 17 cases. All six upstream checker
runs confirmed the no-op fixture fails and the Git reference solution passes;
their optional GitButler reference phase could not run because the `but` binary
was not installed. The actual 12-cell matrix does not use GitButler.
