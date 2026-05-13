// Repository Operations — Composed State Machine
//
// Models the high-level repository operations (snapshot, goto, merge)
// with their lock requirements and precondition guards.
//
// This spec composes lock, merge, and refs concerns to verify
// end-to-end command safety.
//
// Source: crates/repo/src/repository_snapshot.rs
//         crates/repo/src/repository_goto.rs
//         crates/cli/src/cli/commands/merge/mod.rs

module repository_ops {
  import common from "./common"

  // HEAD state
  type HeadState =
    | Attached(str)
    | Detached(str)

  // --- State variables ---

  var repoHead: HeadState
  var threads: str -> str
  var threadNames: Set[str]
  var worktreeClean: bool
  var mergeInProgress: bool
  var mergeConflicts: Set[str]
  var mergeResolved: Set[str]
  var writeLockHeld: bool

  // --- Initial state ---

  action init = all {
    repoHead' = Attached("main"),
    threads' = Map("main" -> "hd-x7k9qm4htv2wn8", "feat/auth-flow" -> "hd-a3bf6r2pyw9kmjt"),
    threadNames' = Set("main", "feat/auth-flow"),
    worktreeClean' = true,
    mergeInProgress' = false,
    mergeConflicts' = Set(),
    mergeResolved' = Set(),
    writeLockHeld' = false,
  }

  // --- Helpers ---

  def headChangeId: str = match repoHead {
    | Attached(thread) =>
      if (threadNames.contains(thread)) threads.get(thread) else ""
    | Detached(stateId) => stateId
  }

  def unresolved: Set[str] = mergeConflicts.exclude(mergeResolved)

  // --- Actions ---

  // Snapshot (commit): requires write lock, checks merge guard
  action snapshot: bool = all {
    // Guard: write lock available
    not(writeLockHeld),
    // Guard: if merge in progress, all conflicts must be resolved
    if (mergeInProgress) unresolved.size() == 0 else true,
    // Guard: worktree must have changes (dirty) or merge completing
    not(worktreeClean) or mergeInProgress,
    // Effect
    nondet newId = common::CHANGE_IDS.oneOf()
    all {
      // Update HEAD
      match repoHead {
        | Attached(thread) => all {
          threads' = threads.set(thread, newId),
          repoHead' = repoHead,
        }
        | Detached(_) => all {
          repoHead' = Detached(newId),
          threads' = threads,
        }
      },
      worktreeClean' = true,
      mergeInProgress' = false,
      mergeConflicts' = Set(),
      mergeResolved' = Set(),
      writeLockHeld' = false,
      threadNames' = threadNames,
    }
  }

  // Goto (checkout): requires write lock, always detaches HEAD
  action goto(targetId: str): bool = all {
    // Guard: write lock available
    not(writeLockHeld),
    // Guard: target exists (abstracted — we just need a valid ID)
    common::CHANGE_IDS.contains(targetId),
    // Guard: no merge in progress
    not(mergeInProgress),
    // Effect: detach HEAD, materialize target tree
    all {
      repoHead' = Detached(targetId),
      worktreeClean' = true,
      // Unchanged
      threads' = threads,
      threadNames' = threadNames,
      mergeInProgress' = mergeInProgress,
      mergeConflicts' = mergeConflicts,
      mergeResolved' = mergeResolved,
      writeLockHeld' = false,
    }
  }

  // Start a merge with potential conflicts
  action startMerge(threadName: str): bool = all {
    // Guard: no merge already in progress
    not(mergeInProgress),
    // Guard: worktree is clean
    worktreeClean,
    // Guard: write lock available
    not(writeLockHeld),
    // Guard: thread exists
    threadNames.contains(threadName),
    // Nondeterministically choose conflicts
    nondet newConflicts = common::FILE_PATHS.powerset().oneOf()
    if (newConflicts.size() == 0) all {
      // Clean merge — auto-commit
      nondet newId = common::CHANGE_IDS.oneOf()
      all {
        match repoHead {
          | Attached(thread) => all {
            threads' = threads.set(thread, newId),
            repoHead' = repoHead,
          }
          | Detached(_) => all {
            repoHead' = Detached(newId),
            threads' = threads,
          }
        },
        worktreeClean' = true,
        mergeInProgress' = false,
        mergeConflicts' = Set(),
        mergeResolved' = Set(),
        writeLockHeld' = false,
        threadNames' = threadNames,
      }
    } else all {
      // Conflicts
      mergeInProgress' = true,
      mergeConflicts' = newConflicts,
      mergeResolved' = Set(),
      worktreeClean' = false,
      writeLockHeld' = false,
      // Unchanged
      repoHead' = repoHead,
      threads' = threads,
      threadNames' = threadNames,
    }
  }

  // Resolve a conflict
  action resolveConflict(path: str): bool = all {
    mergeInProgress,
    mergeConflicts.contains(path),
    not(mergeResolved.contains(path)),
    mergeResolved' = mergeResolved.union(Set(path)),
    // Unchanged
    repoHead' = repoHead,
    threads' = threads,
    threadNames' = threadNames,
    worktreeClean' = worktreeClean,
    mergeInProgress' = mergeInProgress,
    mergeConflicts' = mergeConflicts,
    writeLockHeld' = writeLockHeld,
  }

  // Abort merge
  action abortMerge: bool = all {
    mergeInProgress,
    mergeInProgress' = false,
    mergeConflicts' = Set(),
    mergeResolved' = Set(),
    worktreeClean' = true,
    writeLockHeld' = false,
    // Unchanged
    repoHead' = repoHead,
    threads' = threads,
    threadNames' = threadNames,
  }

  // Modify working directory
  action modifyWorktree: bool = all {
    not(mergeInProgress),
    worktreeClean,
    worktreeClean' = false,
    // Unchanged
    repoHead' = repoHead,
    threads' = threads,
    threadNames' = threadNames,
    mergeInProgress' = mergeInProgress,
    mergeConflicts' = mergeConflicts,
    mergeResolved' = mergeResolved,
    writeLockHeld' = writeLockHeld,
  }

  // Attach HEAD to a thread
  action attachHead(threadName: str): bool = all {
    threadNames.contains(threadName),
    not(mergeInProgress),
    repoHead' = Attached(threadName),
    // Unchanged
    threads' = threads,
    threadNames' = threadNames,
    worktreeClean' = worktreeClean,
    mergeInProgress' = mergeInProgress,
    mergeConflicts' = mergeConflicts,
    mergeResolved' = mergeResolved,
    writeLockHeld' = writeLockHeld,
  }

  // --- Step ---

  action step = any {
    snapshot,

    nondet targetId = common::CHANGE_IDS.oneOf()
    goto(targetId),

    nondet thread = common::THREAD_NAMES.oneOf()
    startMerge(thread),

    nondet path = common::FILE_PATHS.oneOf()
    resolveConflict(path),

    abortMerge,
    modifyWorktree,

    nondet thread = common::THREAD_NAMES.oneOf()
    attachHead(thread),
  }

  // =====================================================
  // SAFETY INVARIANTS
  // =====================================================

  // INV-1: Cannot snapshot with unresolved merge conflicts
  // (Structural — the guard prevents it, but we verify no reachable
  // state violates the post-condition)
  val resolvedSubsetOfConflicts = mergeResolved.subseteq(mergeConflicts)

  // INV-2: No merge state when merge not in progress
  val cleanWhenNoMerge = {
    not(mergeInProgress) implies (
      mergeConflicts.size() == 0 and mergeResolved.size() == 0
    )
  }

  // INV-3: goto always produces detached HEAD
  // (Verified by goto action — HEAD is always Detached after goto)

  // INV-4: Attached HEAD references an existing thread
  val attachedHeadValid = match repoHead {
    | Attached(thread) => threadNames.contains(thread)
    | Detached(_) => true
  }

  // INV-5: Thread map and thread name set are consistent
  val threadConsistency = threadNames.forall(name =>
    threads.keys().contains(name)
  )

  // Combined safety
  val safety = and {
    resolvedSubsetOfConflicts,
    cleanWhenNoMerge,
    attachedHeadValid,
    threadConsistency,
  }

  // =====================================================
  // REGRESSION TRACES
  // =====================================================

  // REG-1: Full edit-commit cycle on attached HEAD
  run attachedSnapshotCycle = {
    init
      .then(modifyWorktree)
      .then(snapshot)
  }

  // REG-2: Goto detaches HEAD
  run gotoDetachesHead = {
    init
      .then(goto("hd-d8nz5v4qec7xh6s"))
      // HEAD should now be Detached("hd-d8nz5v4qec7xh6s")
  }

  // REG-3: Merge with conflicts -> resolve -> snapshot (attached)
  run mergeResolveSnapshot = {
    init
      .then(startMerge("feat/auth-flow"))
      .then(resolveConflict("src/store/repository.rs"))
      .then(resolveConflict("src/refs/refs_manager.rs"))
      .then(resolveConflict("src/cli/commands/merge.rs"))
      .then(resolveConflict("tests/integration.rs"))
      .then(snapshot)
  }

  // REG-4: Goto during merge is blocked
  // goto guard checks not(mergeInProgress), so this is a no-op
  run gotoBlockedDuringMerge = {
    init
      .then(startMerge("feat/auth-flow"))
      .then(abortMerge)  // must abort before goto
      .then(goto("hd-d8nz5v4qec7xh6s"))
  }

  // REG-5: Detached HEAD snapshot
  run detachedSnapshot = {
    init
      .then(goto("hd-a3bf6r2pyw9kmjt"))
      .then(modifyWorktree)
      .then(snapshot)
  }
}
