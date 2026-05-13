# Formal Specifications (Quint)

Heddle uses [Quint](https://quint-lang.org/) to formally specify and verify its core state machines. When modifying state machine logic, update the corresponding Quint spec and Rust property tests.

## When to Update Specs

Update or add Quint specs when changing:

- **Merge/conflict resolution** — `merge_resolution.qnt` ← `crates/repo/src/merge_state.rs`, `crates/cli/src/cli/commands/merge/`
- **Lock protocol** — `lock_protocol.qnt` ← `crates/core/src/lock.rs`
- **Refs/HEAD management** — `refs_head.qnt` ← `crates/refs/src/refs/refs_manager.rs`
- **Agent lifecycle** — `agent_lifecycle.qnt` ← `crates/core/src/store/agent_registry.rs`
- **Worktree lifecycle** — `worktree_lifecycle.qnt` ← `crates/core/src/worktree/worktree_patch.rs`, `crates/cli/src/cli/commands/worktree_cmd.rs`
- **Repository operations** — `repository_ops.qnt` ← `crates/repo/src/repository_snapshot.rs`, `crates/repo/src/repository_goto.rs`

**Rule of thumb:** If your change adds, removes, or modifies a guard condition, state transition, or invariant in any of these systems, update the spec.

## File Layout

```
specs/quint/
  common.qnt                 # Shared abstract types (ChangeId, FilePath, etc.)
  merge_resolution.qnt       # Three-way merge + conflict resolution
  lock_protocol.qnt          # Read/write lock mutual exclusion + ordering
  refs_head.qnt              # HEAD + thread refs with CAS + packed refs
  agent_lifecycle.qnt        # Agent spawn/done/merge + stale pruning
  worktree_lifecycle.qnt     # Patch worktree create/switch/delete
  repository_ops.qnt         # Composed snapshot/goto/merge guards
  verify.sh                  # Run all specs + Rust property tests
  README.md                  # Install, run, add specs

tests/formal_specs.rs        # Rust property tests mirroring the Quint specs
```

## Running Specs

```bash
# Quick verification (~5s)
./specs/quint/verify.sh

# Thorough verification (~60s, 500K traces per spec)
./specs/quint/verify.sh --thorough

# Single spec
quint run --max-samples=10000 --max-steps=20 --invariant=safety specs/quint/merge_resolution.qnt

# Rust property tests only
cargo test --test formal_specs
```

## Spec Structure

Every spec follows the same pattern:

```quint
module my_spec {
  import common from "./common"

  // 1. State variables
  var myState: bool

  // 2. Initial state
  action init = all { myState' = false }

  // 3. Actions (one per operation, with guard conditions)
  action doThing: bool = all {
    not(myState),        // guard
    myState' = true,     // effect
  }

  // 4. Step (nondeterministic choice of actions)
  action step = any { doThing }

  // 5. Safety invariants
  val safety = and { /* invariants */ }

  // 6. Regression traces (named runs for edge cases)
  run regressionTrace = { init.then(doThing) }
}
```

## Quint Syntax Pitfalls

| Pitfall | Wrong | Right |
|---------|-------|-------|
| Adding new map keys | `map.set(newKey, val)` | `map.put(newKey, val)` |
| Using primed vars in expressions | `ids' = ...; map = ids'.fold(...)` | `val x = ...; ids' = x; map = x.fold(...)` |
| Reserved names | `var head: str` | `var currentHead: str` (`head` is built-in) |
| Missing else | `if (x) a` | `if (x) a else b` (else is mandatory) |
| No `then` keyword | `if (x) then a else b` | `if (x) a else b` |

## Adding a Regression Trace

When you find or fix a bug in a state machine:

1. Add a `run` block to the relevant `.qnt` spec that reproduces the scenario
2. Comment it as `// REG-N: <description>`
3. Add a corresponding test case to `tests/formal_specs.rs`

```quint
// REG-7: Fix for issue #42 — abort during partial resolve left stale state
run abortDuringPartialResolve = {
  init
    .then(startMerge("feat"))
    .then(resolveFile("a.rs"))
    .then(abortMerge)
    // Verify: no stale resolved files after abort
}
```

## Adding a New Spec

1. Create `specs/quint/new_spec.qnt` importing `common`
2. Define `var` state, `action init`, `action step`, `val safety`
3. Run: `quint run --max-samples=10000 --max-steps=20 --invariant=safety specs/quint/new_spec.qnt`
4. Add a corresponding `mod` in `tests/formal_specs.rs` with proptest
5. Run: `cargo test --test formal_specs`
6. Add regression traces for known edge cases

## Abstraction Strategy

Quint specs use small finite sets to keep the state space tractable:

- **ChangeIds:** 5 abstract values (`"id1"`.."id5"`)
- **File paths:** 4 values (`"a.rs"`.."d.rs"`)
- **Thread names:** 3 values (`"main"`, `"feat"`, `"agent-1"`)
- **Processes:** 3 values (for lock contention modeling)

This keeps state spaces under ~2M states while preserving the essential properties. If you need more values for a specific scenario, add them to `common.qnt` but be aware of exponential state growth.
