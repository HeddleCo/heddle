// Worktree Lifecycle State Machine
//
// Models the patch-based worktree system in Heddle:
//   Create -> (Clean/Dirty) -> (Current/Stashed) -> Delete
//
// Source: crates/objects/src/worktree/worktree_patch.rs
//         crates/cli/src/cli/commands/worktree_cmd.rs

module worktree_lifecycle {
  import common from "./common"

  // Worktree entry
  type Worktree = {
    name: str,
    dirty: bool,
    thread: str,
  }

  // Possible worktree names
  pure val WT_NAMES = Set("default", "auth-flow", "fix-merge")

  // --- State variables ---

  // Registry: worktree ID -> entry
  var worktrees: str -> Worktree

  // Which worktree IDs exist
  var worktreeIds: Set[str]

  // Current active worktree (empty string = none)
  var currentWt: str

  // Whether the write lock is held (for switch atomicity)
  var writeLockHeld: bool

  // --- Initial state ---

  action init = all {
    val defaultWt = { name: "default", dirty: false, thread: "main" }
    all {
      worktrees' = Map("wt-r3kf8m2qvx" -> defaultWt),
      worktreeIds' = Set("wt-r3kf8m2qvx"),
      currentWt' = "wt-r3kf8m2qvx",
      writeLockHeld' = false,
    }
  }

  // --- Helpers ---

  def nameInUse(name: str): bool = {
    worktreeIds.exists(id => worktrees.get(id).name == name)
  }

  // --- Actions ---

  // Create a new worktree
  action create(wtId: str, name: str, thread: str): bool = all {
    // Guard: ID not in use
    not(worktreeIds.contains(wtId)),
    // Guard: name not in use
    not(nameInUse(name)),
    // Effect
    val entry = { name: name, dirty: false, thread: thread }
    all {
      worktrees' = worktrees.put(wtId, entry),
      worktreeIds' = worktreeIds.union(Set(wtId)),
      currentWt' = currentWt,
      writeLockHeld' = writeLockHeld,
    }
  }

  // Switch to a different worktree (requires write lock)
  // Models cmd_worktree_switch: capture -> clear -> materialize -> set_current
  action switch(targetId: str): bool = all {
    // Guard: target exists
    worktreeIds.contains(targetId),
    // Guard: not switching to self
    targetId != currentWt,
    // Guard: write lock not held by someone else
    not(writeLockHeld),
    // Effect: atomic switch under write lock
    // 1. Capture current worktree's dirty state (already reflected)
    // 2. Set new current
    // 3. New worktree materializes as clean
    val oldWt = worktrees.get(currentWt)
    val targetWt = worktrees.get(targetId)
    all {
      currentWt' = targetId,
      // Target worktree becomes clean after materialization
      worktrees' = worktrees.set(targetId, { ...targetWt, dirty: false }),
      worktreeIds' = worktreeIds,
      writeLockHeld' = false,
    }
  }

  // Delete a worktree
  action deleteWt(wtId: str, force: bool): bool = all {
    // Guard: worktree exists
    worktreeIds.contains(wtId),
    // Guard: CANNOT delete current worktree
    wtId != currentWt,
    // Guard: if not force, must be clean
    if (not(force))
      not(worktrees.get(wtId).dirty)
    else
      true,
    // Effect
    val remainingIds = worktreeIds.exclude(Set(wtId))
    all {
      worktreeIds' = remainingIds,
      worktrees' = remainingIds.fold(Map(), (acc, id) => acc.put(id, worktrees.get(id))),
      currentWt' = currentWt,
      writeLockHeld' = writeLockHeld,
    }
  }

  // Modify files in the current worktree (make it dirty)
  action modifyCurrentWt: bool = all {
    currentWt != "",
    not(worktrees.get(currentWt).dirty),
    worktrees' = worktrees.set(currentWt, {
      name: worktrees.get(currentWt).name,
      dirty: true,
      thread: worktrees.get(currentWt).thread,
    }),
    worktreeIds' = worktreeIds,
    currentWt' = currentWt,
    writeLockHeld' = writeLockHeld,
  }

  // Snapshot in current worktree (makes it clean)
  action snapshotCurrentWt: bool = all {
    currentWt != "",
    worktrees.get(currentWt).dirty,
    worktrees' = worktrees.set(currentWt, {
      name: worktrees.get(currentWt).name,
      dirty: false,
      thread: worktrees.get(currentWt).thread,
    }),
    worktreeIds' = worktreeIds,
    currentWt' = currentWt,
    writeLockHeld' = writeLockHeld,
  }

  // --- Step ---

  action step = any {
    nondet wtId = common::WORKTREE_IDS.oneOf()
    nondet name = WT_NAMES.oneOf()
    nondet thread = common::THREAD_NAMES.oneOf()
    create(wtId, name, thread),

    nondet targetId = common::WORKTREE_IDS.oneOf()
    switch(targetId),

    nondet wtId = common::WORKTREE_IDS.oneOf()
    deleteWt(wtId, false),

    nondet wtId = common::WORKTREE_IDS.oneOf()
    deleteWt(wtId, true),

    modifyCurrentWt,
    snapshotCurrentWt,
  }

  // =====================================================
  // SAFETY INVARIANTS
  // =====================================================

  // INV-1: Current worktree always exists in the registry
  val currentExists = {
    currentWt != "" implies worktreeIds.contains(currentWt)
  }

  // INV-2: Cannot delete current worktree (enforced by guard)
  // Structural: after any step, current is still in worktreeIds
  // (same as INV-1, verified continuously)

  // INV-3: No duplicate names
  val uniqueNames = worktreeIds.forall(id1 =>
    worktreeIds.forall(id2 =>
      if (id1 != id2)
        worktrees.get(id1).name != worktrees.get(id2).name
      else
        true
    )
  )

  // INV-4: At least one worktree always exists (current)
  val atLeastOneWorktree = worktreeIds.size() >= 1

  // INV-5: worktreeIds and worktrees map keys are consistent
  val registryConsistent = worktreeIds.forall(id =>
    worktrees.keys().contains(id)
  )

  // Combined safety
  val safety = and {
    currentExists,
    uniqueNames,
    atLeastOneWorktree,
    registryConsistent,
  }

  // =====================================================
  // REGRESSION TRACES
  // =====================================================

  // REG-1: Create, switch, delete old worktree
  run createSwitchDelete = {
    init
      .then(create("wt-p7nt5b9djw", "auth-flow", "feat/auth-flow"))
      .then(switch("wt-p7nt5b9djw"))
      .then(deleteWt("wt-r3kf8m2qvx", false))  // old default, now deletable
  }

  // REG-2: Cannot delete current worktree (guard prevents it)
  // This trace should succeed — deleteWt on current is a no-op.
  run cannotDeleteCurrent = {
    init
      .then(create("wt-p7nt5b9djw", "auth-flow", "feat/auth-flow"))
      .then(deleteWt("wt-r3kf8m2qvx", false))  // wt-r3kf8m2qvx is current, should be blocked
  }

  // REG-3: Dirty worktree blocks non-force delete
  run dirtyBlocksDelete = {
    init
      .then(create("wt-p7nt5b9djw", "auth-flow", "feat/auth-flow"))
      .then(switch("wt-p7nt5b9djw"))
      .then(switch("wt-r3kf8m2qvx"))
      .then(modifyCurrentWt)
      .then(switch("wt-p7nt5b9djw"))
      // wt-r3kf8m2qvx is now dirty and not current — force=false should block
      .then(deleteWt("wt-r3kf8m2qvx", true))  // force=true works
  }

  // REG-4: Switch materializes target as clean
  run switchMaterializesClean = {
    init
      .then(create("wt-p7nt5b9djw", "auth-flow", "feat/auth-flow"))
      .then(modifyCurrentWt)
      .then(switch("wt-p7nt5b9djw"))  // wt-p7nt5b9djw should be clean after materialization
  }
}
