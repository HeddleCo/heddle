# VC-Bench Heddle-native adaptation

This experiment compares Heddle's native version-control workflow with the
ordinary Git workflow using the public six-scenario VC-Bench harness.

## Source

- Repository: <https://github.com/gitbutlerapp/version-control-bench>
- Pinned revision: `d1c071b7a07a16e2bf7ec85055ed75456f75e1ed`
- Benchmark site: <https://vcbench.dev/>
- Run date: 2026-07-20

The adaptation is recorded in `heddle-native.patch`. Apply it to the pinned
upstream revision; it adds the `heddle-native` arm without changing the Git
arm, task fixtures, prompts, or deterministic task verifiers.

## Matched contract

Both lanes use the same six fixtures, task text, Codex model (`gpt-5.5`),
timeout, and upstream verifier. The Git lane is unmodified. The native lane:

1. Creates the original Git fixture, then imports it with `heddle init` and
   `heddle adopt`.
2. Instructs the agent to use Heddle for all version-control writes, mapping
   commits to states/captures, branches to threads, and uncommitted changes to
   uncaptured changes.
3. Blocks raw Git, GitButler, and Jujutsu writes while tracing Heddle commands.
4. Exports the final Heddle thread to a temporary bare Git repository and
   overlays uncaptured worktree files so the unchanged upstream oracle grades
   both lanes identically.

The export is only a grading adapter. It is not used during the agent task and
does not introduce a production compatibility path.

## Reproduction

Build the Heddle binary from the source revision recorded in `RESULTS.md`, then
run from a patched checkout of VC-Bench:

```sh
node scripts/run-pilot-agent.mjs --self-test-metrics true \
  --agent codex --arm git --out /tmp/vcb-self-test

node scripts/run-full-matrix.mjs \
  --k 1 \
  --tasks pilot-1-selective-validation,pilot-2-multi-amend,pilot-3-split-commit,pilot-4-reorder-commits,pilot-5-squash-commits,pilot-6-update-dirty-branch \
  --agents codex \
  --arms git,heddle-native \
  --codex-model gpt-5.5 \
  --batch-name heddle-git-native-k1 \
  --out /tmp/vcb-runs/heddle-git-native-k1 \
  --but-bin /usr/bin/false \
  --jj-bin /usr/bin/false \
  --heddle-bin /absolute/path/to/heddle \
  --timeout-ms 900000 \
  --fail-on-failures true
```

`k=1` is a directional comparison, not a reliability estimate. Increase `k`
and pin the agent runtime/model again before using the timing deltas as a
release gate.
