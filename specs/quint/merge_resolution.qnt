// Merge/Conflict Resolution State Machine
//
// Models the three-way merge flow in Heddle:
//   NoMerge -> (FastForward | ThreeWayMerge) -> ConflictResolution -> Committed
//
// Source: crates/repo/src/merge_state.rs
//         crates/cli/src/cli/commands/merge/merge_algo.rs

module merge_resolution {
  import common from "./common"

  // --- State variables ---

  // Whether a merge is currently in progress (MERGE_STATE file exists)
  var mergeActive: bool

  // The set of files with conflicts in the current merge
  var conflicts: Set[str]

  // The set of files that have been resolved
  var resolved: Set[str]

  // Current HEAD change ID (renamed from "head" to avoid built-in collision)
  var currentHead: str

  // Thread name -> tip change ID
  var threads: str -> str

  // Whether the working directory is clean (matches HEAD tree)
  var worktreeClean: bool

  // Ancestry relation: (ancestor, descendant) pairs
  var ancestry: Set[(str, str)]

  // The "ours" and "theirs" change IDs for the active merge
  var mergeOurs: str
  var mergeTheirs: str

  // --- Initial state ---

  action init = all {
    mergeActive' = false,
    conflicts' = Set(),
    resolved' = Set(),
    currentHead' = "hd-x7k9qm4htv2wn8",
    threads' = Map("main" -> "hd-x7k9qm4htv2wn8", "feat/auth-flow" -> "hd-a3bf6r2pyw9kmjt"),
    worktreeClean' = true,
    ancestry' = Set(("hd-x7k9qm4htv2wn8", "hd-a3bf6r2pyw9kmjt"), ("hd-x7k9qm4htv2wn8", "hd-d8nz5v4qec7xh6s")),
    mergeOurs' = "",
    mergeTheirs' = "",
  }

  // --- Helper predicates ---

  pure def isAncestor(anc: str, desc: str, rel: Set[(str, str)]): bool = {
    rel.contains((anc, desc))
  }

  def unresolved: Set[str] = conflicts.exclude(resolved)

  // --- Unchanged helpers (defined before use) ---

  action unchanged_mergeActive = mergeActive' = mergeActive
  action unchanged_conflicts = conflicts' = conflicts
  action unchanged_resolved = resolved' = resolved
  action unchanged_currentHead = currentHead' = currentHead
  action unchanged_threads = threads' = threads
  action unchanged_worktreeClean = worktreeClean' = worktreeClean
  action unchanged_ancestry = ancestry' = ancestry
  action unchanged_mergeOurs = mergeOurs' = mergeOurs
  action unchanged_mergeTheirs = mergeTheirs' = mergeTheirs

  action stutter = all {
    unchanged_mergeActive,
    unchanged_conflicts,
    unchanged_resolved,
    unchanged_currentHead,
    unchanged_threads,
    unchanged_worktreeClean,
    unchanged_ancestry,
    unchanged_mergeOurs,
    unchanged_mergeTheirs,
  }

  // --- Actions ---

  // Fast-forward: move HEAD to target, no merge state needed
  action fastForward(target: str): bool = all {
    currentHead' = target,
    mergeActive' = false,
    conflicts' = Set(),
    resolved' = Set(),
    worktreeClean' = true,
    mergeOurs' = "",
    mergeTheirs' = "",
    unchanged_ancestry,
    unchanged_threads,
  }

  // Three-way merge: may produce conflicts
  action threeWayMerge(theirTip: str): bool = {
    nondet newConflicts = common::FILE_PATHS.powerset().oneOf()
    if (newConflicts.size() == 0) {
      // Clean merge: create merge commit immediately
      nondet mergeResult = common::CHANGE_IDS.exclude(Set(currentHead, theirTip)).oneOf()
      all {
        currentHead' = mergeResult,
        mergeActive' = false,
        conflicts' = Set(),
        resolved' = Set(),
        worktreeClean' = true,
        mergeOurs' = "",
        mergeTheirs' = "",
        ancestry' = ancestry.union(Set(
          (currentHead, mergeResult),
          (theirTip, mergeResult)
        )),
        unchanged_threads,
      }
    } else {
      // Conflicts: enter conflict resolution state
      all {
        mergeActive' = true,
        conflicts' = newConflicts,
        resolved' = Set(),
        mergeOurs' = currentHead,
        mergeTheirs' = theirTip,
        worktreeClean' = false,
        unchanged_currentHead,
        unchanged_ancestry,
        unchanged_threads,
      }
    }
  }

  // Start a merge: check preconditions and determine merge type.
  action startMerge(threadName: str): bool = all {
    not(mergeActive),
    worktreeClean,
    threads.keys().contains(threadName),
    threads.get(threadName) != currentHead,
    val theirTip = threads.get(threadName)
    if (isAncestor(currentHead, theirTip, ancestry))
      fastForward(theirTip)
    else if (isAncestor(theirTip, currentHead, ancestry))
      stutter
    else
      threeWayMerge(theirTip)
  }

  // Resolve a single file.
  // Models MergeStateManager::resolve() at merge_state.rs:59-72
  action resolveFile(path: str): bool = all {
    mergeActive,
    conflicts.contains(path),
    not(resolved.contains(path)),
    resolved' = resolved.union(Set(path)),
    unchanged_mergeActive,
    unchanged_conflicts,
    unchanged_currentHead,
    unchanged_threads,
    unchanged_worktreeClean,
    unchanged_ancestry,
    unchanged_mergeOurs,
    unchanged_mergeTheirs,
  }

  // Resolve all remaining conflicts at once.
  // Models MergeStateManager::resolve_all() at merge_state.rs:75-91
  action resolveAll: bool = all {
    mergeActive,
    unresolved.size() > 0,
    resolved' = conflicts,
    unchanged_mergeActive,
    unchanged_conflicts,
    unchanged_currentHead,
    unchanged_threads,
    unchanged_worktreeClean,
    unchanged_ancestry,
    unchanged_mergeOurs,
    unchanged_mergeTheirs,
  }

  // Abort the merge: discard MERGE_STATE, restore working directory.
  // Models MergeStateManager::abort() at merge_state.rs:107-117
  action abortMerge: bool = all {
    mergeActive,
    mergeActive' = false,
    conflicts' = Set(),
    resolved' = Set(),
    worktreeClean' = true,
    mergeOurs' = "",
    mergeTheirs' = "",
    unchanged_currentHead,
    unchanged_threads,
    unchanged_ancestry,
  }

  // Finish the merge: create merge commit if all conflicts resolved.
  // Models MergeStateManager::finish() at merge_state.rs:119-143
  action finishMerge: bool = all {
    mergeActive,
    unresolved.size() == 0,
    nondet mergeResult = common::CHANGE_IDS.exclude(Set(mergeOurs, mergeTheirs)).oneOf()
    all {
      currentHead' = mergeResult,
      mergeActive' = false,
      conflicts' = Set(),
      resolved' = Set(),
      worktreeClean' = true,
      mergeOurs' = "",
      mergeTheirs' = "",
      ancestry' = ancestry.union(Set(
        (mergeOurs, mergeResult),
        (mergeTheirs, mergeResult)
      )),
      unchanged_threads,
    }
  }

  // Snapshot (commit) — during merge requires all conflicts resolved.
  action snapshot: bool = {
    if (mergeActive) {
      all {
        unresolved.size() == 0,
        finishMerge,
      }
    } else {
      all {
        not(worktreeClean),
        nondet newState = common::CHANGE_IDS.exclude(Set(currentHead)).oneOf()
        all {
          currentHead' = newState,
          worktreeClean' = true,
          ancestry' = ancestry.union(Set((currentHead, newState))),
          unchanged_mergeActive,
          unchanged_conflicts,
          unchanged_resolved,
          unchanged_threads,
          unchanged_mergeOurs,
          unchanged_mergeTheirs,
        }
      }
    }
  }

  // Modify working directory (make it dirty)
  action modifyWorktree: bool = all {
    not(mergeActive),
    worktreeClean,
    worktreeClean' = false,
    unchanged_mergeActive,
    unchanged_conflicts,
    unchanged_resolved,
    unchanged_currentHead,
    unchanged_threads,
    unchanged_ancestry,
    unchanged_mergeOurs,
    unchanged_mergeTheirs,
  }

  // --- Step: nondeterministic choice of next action ---

  action step = any {
    nondet thread = common::THREAD_NAMES.oneOf()
    startMerge(thread),

    nondet path = common::FILE_PATHS.oneOf()
    resolveFile(path),

    resolveAll,
    abortMerge,
    finishMerge,
    snapshot,
    modifyWorktree,
  }

  // =====================================================
  // SAFETY INVARIANTS
  // =====================================================

  // INV-1: resolved is always a subset of conflicts
  val resolvedSubsetOfConflicts = resolved.subseteq(conflicts)

  // INV-2: If no merge is active, conflicts and resolved are empty
  val noMergeImpliesCleanState = {
    not(mergeActive) implies (conflicts.size() == 0 and resolved.size() == 0)
  }

  // INV-3: mergeOurs and mergeTheirs are set iff merge is active
  val mergeParentsConsistent = {
    if (mergeActive)
      mergeOurs != "" and mergeTheirs != ""
    else
      mergeOurs == "" and mergeTheirs == ""
  }

  // INV-4: merge active implies worktree not clean
  val mergeImpliesDirtyWorktree = {
    mergeActive implies not(worktreeClean)
  }

  // Combined safety invariant for model checking
  val safety = and {
    resolvedSubsetOfConflicts,
    noMergeImpliesCleanState,
    mergeParentsConsistent,
    mergeImpliesDirtyWorktree,
  }

  // =====================================================
  // REGRESSION TRACES
  // Named runs that exercise specific edge cases.
  // Add a new run here whenever a bug is found.
  // =====================================================

  // REG-1: Merge with conflicts -> resolve one -> abort -> merge again
  // Verifies that abort fully cleans state so a new merge can start.
  run mergeAbortRemergeCycle = {
    init
      .then(modifyWorktree)
      .then(snapshot)
      .then(startMerge("feat/auth-flow"))
      .then(resolveFile("src/store/repository.rs"))
      .then(abortMerge)
      .then(startMerge("feat/auth-flow"))
  }

  // REG-2: Merge with all files conflicting -> resolveAll -> finish
  // Maximum conflict scenario.
  run fullConflictResolveAll = {
    init
      .then(modifyWorktree)
      .then(snapshot)
      .then(startMerge("feat/auth-flow"))
      .then(resolveAll)
      .then(finishMerge)
  }

  // REG-3: Snapshot after merge completes (via snapshot action, not finishMerge)
  // Verifies the snapshot guard allows committing when all conflicts resolved.
  run snapshotCompletesMerge = {
    init
      .then(modifyWorktree)
      .then(snapshot)
      .then(startMerge("feat/auth-flow"))
      .then(resolveAll)
      .then(snapshot)
  }

  // REG-4: Idempotent resolve — resolving the same file twice is a no-op
  // Verifies resolveFile guard prevents double-resolve.
  run idempotentResolve = {
    init
      .then(modifyWorktree)
      .then(snapshot)
      .then(startMerge("feat/auth-flow"))
      .then(resolveFile("src/store/repository.rs"))
      .then(resolveFile("src/store/repository.rs"))  // should be a no-op (guard blocks it)
  }

  // REG-5: Normal snapshot cycle (no merge)
  // Verifies basic modify -> snapshot -> modify -> snapshot works.
  run normalSnapshotCycle = {
    init
      .then(modifyWorktree)
      .then(snapshot)
      .then(modifyWorktree)
      .then(snapshot)
  }
}
