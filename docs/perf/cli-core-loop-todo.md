# CLI Core Loop Performance TODO

Target: keep the full command contracts while moving warm-cache live-repo reads
toward these bands:

- `repo_open_ms`: 14-15 ms -> 5-8 ms
- `worktree_status_ms`: 29-30 ms -> 12-18 ms
- `thread_summary_ms`: 14 ms -> 4-8 ms

## Done

- Add `HEDDLE_PROFILE=1` for stderr-only command and status phase timings.
- Add ignored release smoke coverage for the core command loop.
- Cache user config per CLI process.
- Resolve `status` output mode once and carry it through rendering.
- Avoid the duplicate full thread-summary walk in `status --output json`.
- Skip text-only import-hint and materialized-thread advisory work in
  `status --short --output text`.
- Skip the text-only import hint in `thread list --output json`.
- Cache repository capability on `Repository` instead of probing `.git` on every
  `capability()` call.
- Add a status-specific worktree profile to show index load, compare, flatten,
  save, and monitor persistence inside `worktree_status_ms`.
- Collapse repository open's git/Heddle discovery into one ancestor walk and
  route legacy ref migration through the declarative migration pass only.
- Skip clean tracked directories during the untracked-file scan when the cached
  child-name digest proves there are no added paths below that subtree.
- Amortize status/verify `Repository::open`: CLI injects the opened repo into
  `ExecutionContext`; core reports `repo_open_ms = 0` when injected and does
  not re-open; verify skips the plain-Git probe when a Heddle repo is already
  injected; CLI profile folds the shell open into `repo_open_ms` so the phase
  is truthful; verify no longer opens a second time only for repo config/JSON.

## Next

- Split `collect_thread_summaries` into explicit shapes:
  current-thread summary, thread-list summary, and full workspace summary.
- Teach short/default status to request a cheaper worktree status shape when it
  only needs sorted path lists.
- Reduce repo-open work by skipping migration/hydrator probes when a repo has a
  clean schema ledger and no lazy-hydrator file.
- Add release smoke baselines for a large real-ish repo fixture, not only the
  small synthetic core-loop fixture.
