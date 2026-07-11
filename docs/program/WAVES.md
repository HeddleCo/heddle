# Multi-wave implementation plan

Root agent owns architecture, integration, review, git history, and certification.
Subagents get **disjoint path ownership**; they must not commit/push unless assigned.

## Wave 0 — Product contract + trustworthy baseline (this wave)

**Owner:** root  
**Paths:** `docs/program/**`, `scripts/program/**`, `artifacts/baseline/**`  
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

## Wave 2 — Git fidelity & verification ownership

**Paths:** `crates/git-projection/**`, `crates/ingest/**`, `crates/core/src/verify*`, `crates/core/src/status*`  
**Focus:** oracle regressions, verification single ownership, residual bridge cleanup slices from `VERIFICATION_CLEANUP_PLAN.md`  
**Not in scope:** unrelated CLI polish

## Wave 3 — Save / thread / workflow facade extraction

**Paths:** `crates/core/src/save*`, thread-shaping, CLI `workflow`/`start`/`ready`/`land` adapters only  
**Focus:** typed `*Options`/`*Report`; CLI becomes render/dispatch

## Wave 4 — Remotes / projection command extraction

**Paths:** CLI remote modules → core/repo ops; push/pull/sync capability routing  
**Keep:** wire protocol changes minimal unless contract broken

## Wave 5 — Concurrency / crash consistency

**Paths:** `objects` atomic FS, oplog, refs locks, operation dedup  
**Focus:** property tests, fault injection suites already present

## Wave 6 — Performance hotspots (correct paths only)

**Prerequisite:** Wave 2–3 correctness green for touched ops  
**Focus:** status/verify open amortization, worktree scan, pack/hash benches  
**Required evidence:** before/after paired timings, p95/p99, correctness held

## Wave 7 — Platform matrix & long-tail

Windows materialization, mount optional, large-ref packed-refs degradation docs/tests

## Wave 8 — Certification

Full curated + oracle + format + clippy + doc + perf cert (5 trials) → release gate checklist green

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
