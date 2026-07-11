# Multi-wave implementation plan

Root agent owns architecture, integration, review, git history, and certification.
Subagents get **disjoint path ownership**; they must not commit/push unless assigned.

## Wave 0 — Product contract + trustworthy baseline (this wave)

**Owner:** root  
**Paths:** `docs/program/**`, `scripts/program/**`, `artifacts/baseline/**`  
**Status (2026-07-11):** **Done** for local program foundation — contract, audit, gap map, release gates, curated manifest, runner, and first baseline recorded under `artifacts/baseline/`. Branch work continues on program tip; full platform push remains root-owned.  
**Done when:**

- Product contract, architecture audit, gap map, release gates checked in
- Curated manifest + baseline runner produce classified JSON
- First baseline recorded (commands, commit, env, results)
- Branch pushed

## Wave 1 — Measurement foundation hardening

**Agent A (harness):** extend git-process lint to `core` + `git-projection`; classify oracle suite; fix harness bugs only  
**Agent B (perf tooling):** paired-bench runner + stats JSON; inventory Criterion targets  
**Agent C (architecture inventory):** process provenance map, OnceLock inventory, CLI→core remaining command matrix  
**Root:** integrate, run baseline, commit  
**Status (2026-07-11):** **Done** for measurement foundation — git-process lint scope, oracle classification in manifest, paired-bench + core-loop absolute harness, CLI residual inventory, and wave1/curated baseline stamps recorded. Perf **n=5 cert sample** recorded in Wave 8 partial cert (`docs/program/PERF_BASELINE.md`).

## Wave 2 — Git fidelity & verification ownership

**Paths:** `crates/git-projection/**`, `crates/ingest/**`, `crates/core/src/verify*`, `crates/core/src/status*`  
**Focus:** oracle regressions, verification single ownership, residual bridge cleanup slices from `VERIFICATION_CLEANUP_PLAN.md`  
**Not in scope:** unrelated CLI polish  
**Status (2026-07-11):** **Substantially complete / integrated on program branch** — verification ownership and status facade moves landed (e.g. status verdict/setup guidance in core). High-signal re-cert post-wave: roundtrip-fidelity, commit-conformance, git-process-lint, formal-specs all **pass** on `b748bfd4` (`artifacts/baseline/post-wave23-merged/`). Residual bridge cleanup may continue as scoped slices; not a gate blocker for the re-cert suite.

## Wave 3 — Save / thread / workflow facade extraction

**Paths:** `crates/core/src/save*`, thread-shaping, CLI `workflow`/`start`/`ready`/`land` adapters only  
**Focus:** typed `*Options`/`*Report`; CLI becomes render/dispatch  
**Status (2026-07-11):** **High-value complete on program tip** — pure plan/report/decision ownership for capture safety, ready/land accounting, land text, undo human labels, thread resolve blockers, operator verification claims, harness relay JSON, monorepo/fanout preflight, git commit index planning, resolve markers, and related helpers live in `heddle-core`. CLI retains I/O, RecoveryAdvice, hooks, and render. Full thin-CLI for every `cmd_*` is **not** claimed; long-tail residual tracked in `docs/program/cli-domain-residual.md` (schemas/doctor/watch/redact/etc.).

## Wave 4 — Remotes / projection command extraction

**Paths:** CLI remote modules → core/repo ops; push/pull/sync capability routing  
**Keep:** wire protocol changes minimal unless contract broken  
**Status (2026-07-11):** **High-value complete on program tip** — remote list/show, push/pull plans/outcomes, transport mismatch, URL/location/hosted path helpers, hosted result field parsing, and git-projection commit index / next-action pure pieces are in core. Network bodies and Sley overlay I/O remain CLI.

## Wave 5 — Concurrency / crash consistency

**Paths:** `objects` atomic FS, oplog, refs locks, operation dedup  
**Focus:** property tests, fault injection suites already present  
**Status:** **Not started / optional early slices** — not part of post-wave23 high-signal re-cert scope.

## Wave 6 — Performance hotspots (correct paths only)

**Prerequisite:** Wave 2–3 correctness green for touched ops  
**Focus:** status/verify open amortization, worktree scan, pack/hash benches  
**Required evidence:** before/after paired timings, p95/p99, correctness held  
**Status:** **Unblocked for correct-path hotspot work** — n=5 equal-work core-loop absolute + A==B self-pairs recorded (see `PERF_BASELINE.md`); still **not** a Git win claim. Wave 2–3 / Wave 8 high-signal correctness green for proceeding on correct paths with before/after paired evidence.

## Wave 7 — Platform matrix & long-tail

Windows materialization, mount optional, large-ref packed-refs degradation docs/tests  
**Status:** **Not started.**

## Wave 8 — Certification

Full curated + oracle + format + clippy + doc + perf cert (5 trials) → release gate checklist green  
**Status (2026-07-11, TODO #3 ac8c re-cert):** **Release-gate green on tip for correctness gates** — full curated **19/19 green** on tip `ac8c1aa64361f123ba5c2a542a284134d2dc2a0f` (`artifacts/baseline/wave-ac8c-cert-merged/summary.json`, `CARGO_TARGET_DIR=/tmp/heddle-nw-t3`; single `run-baseline.sh --suite curated` on detached clean worktree `/tmp/heddle-nw-cert`, `dirty=0`). All oracles + fmt green. Clippy **`-D warnings` pass**, soft clippy **pass** (0 warnings), `cargo doc -p heddle-core --no-deps --locked` **pass**. Prior TODO #3 e614 stamp on `e6145058` (`wave-e614-cert-merged`), TODO N5 stamp on `96a422a8` (`todo-n5-cert-merged`), TODO R2 stamp on `6a09ecb7` (`todo-r2-cert-merged`), TODO #4 stamp on `a5b1dc68` (`todo4-curated-merged`), and Wave 8 stamp on `d3db0143` (`wave-next-merged`) remain historical greens; **superseded** for tip authority. Prior **n=5** core-loop perf sample retained (`PERF_BASELINE.md`) — **not** a Git win claim; multi-host perf still open. Remaining Wave 8 gaps: multi-host / platform matrix only (not a tip-correctness blocker).
---

## First three bounded implementation tasks (start immediately)

### Task 1 — Curated baseline harness (root)

Add `scripts/program/manifest.toml` + `run-baseline.sh` + result classifier; record first baseline.

### Task 2 — Expand git-process lint coverage (harness)

Include `crates/core/src` and `crates/git-projection/src` in `git_process_lint` scan dirs; keep allowlist empty.

### Task 3 — Architecture residual matrix artifact (inventory)

Generate checked-in `docs/program/cli-domain-residual.md` listing `cmd_*` modules not yet delegated to `heddle_core`, ordered by LOC, for Wave 3+ extraction order.

---

## Agent return template

Each delegated task returns:

1. Root cause  
2. Why selected engine owns the behavior  
3. Changed files  
4. Public-interface changes  
5. Tests + exact commands  
6. Oracle cases gained/held/regressed  
7. Before/after timings (perf only)  
8. Remaining risks  
9. Overlapping paths  

## Reject criteria for delegated diffs

- Complexity move without ownership improvement  
- Compatibility only in wrapper  
- Borrow external `git` to fake native behavior  
- Benchmark gaming  
- Weaker validation/durability  
- Silent fallbacks  
- Regress unsupported platforms/features  
