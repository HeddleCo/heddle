# Heddle Formal Specifications (Quint)

Formal models of Heddle's core state machines, verified via random simulation and (optionally) exhaustive model checking with Apalache.

## Specs

| Spec | Models | Key Invariants | Regression Traces |
|------|--------|----------------|-------------------|
| `merge_resolution.qnt` | Three-way merge + conflict resolution | resolved ⊆ conflicts, no snapshot with unresolved conflicts | 5 |
| `lock_protocol.qnt` | Read/write advisory locks | Mutual exclusion, deadlock freedom via lock ordering | 4 |
| `refs_head.qnt` | HEAD + thread refs with CAS | Loose shadows packed, no ref lost across pack | 5 |
| `agent_lifecycle.qnt` | Agent spawn/done/merge + stale pruning | No backward transitions, completion time consistency | 4 |
| `worktree_lifecycle.qnt` | Patch worktree create/switch/delete | Cannot delete current, unique names, current always exists | 4 |
| `repository_ops.qnt` | Composed snapshot/goto/merge guards | Attached HEAD valid, clean state when no merge | 5 |
| `collaboration_convergence.qnt` | Immutable collaboration op-set replication and hosted disposition | Causal closure, deterministic convergence, accepted/rejected/blocked separation | 6 |

## Install

```bash
npm install -g @informalsystems/quint
```

## Run

One command:

```bash
./specs/quint/verify.sh              # Quick: 10K traces (~5s)
./specs/quint/verify.sh --thorough   # Thorough: 500K traces (~60s)
```

Or manually:

```bash
quint run --max-samples=10000 --max-steps=20 --invariant=safety specs/quint/merge_resolution.qnt
```

## Exhaustive Verification (Apalache)

Apalache provides true exhaustive state exploration but requires Java 17+ and
can hit state space explosion on specs with large domains (refs, merge).
For most purposes the random simulator at 500K traces provides equivalent
confidence — it runs 3M traces across all specs in ~60s.

```bash
# Only practical for small specs (lock_protocol, agent_lifecycle)
quint verify --invariant=safety specs/quint/lock_protocol.qnt
```

## CI

The GitHub Actions workflow (`.github/workflows/formal-specs.yml`) runs three tiers:

1. **Quint Simulation** (every PR) — 10K random traces per spec
2. **Quint Thorough** (merge to main) — 500K traces × 50 steps per spec (3M total)
3. **Rust Property Tests** (every PR) — proptest with 10K cases

## Adding a Regression Trace

When a bug is found:

1. Add a `run` block to the relevant `.qnt` spec capturing the exact failing sequence
2. Name it `REG-N: <description>` in a comment
3. The trace serves as a permanent regression test

Example:

```quint
// REG-6: Double resolve after abort should work
run doubleResolveAfterAbort = {
  init
    .then(startMerge("feat"))
    .then(resolveFile("a.rs"))
    .then(abortMerge)
    .then(startMerge("feat"))
    .then(resolveFile("a.rs"))
    .then(resolveAll)
    .then(finishMerge)
}
```

## Adding New Specs

1. Import shared types: `import common from "./common"`
2. Define `var` state, `action init`, `action step`, and `val safety`
3. Run: `quint run --invariant=safety specs/quint/your_spec.qnt`
4. Add corresponding Rust property test in `tests/formal_specs.rs`
5. Add regression traces for known edge cases
