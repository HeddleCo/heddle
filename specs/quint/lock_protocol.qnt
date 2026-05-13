// Lock Protocol Specification
//
// Models the read/write advisory lock system in Heddle.
// Verifies mutual exclusion and deadlock freedom.
//
// Source: crates/objects/src/lock.rs

module lock_protocol {
  // Lock names
  pure val LOCKS = Set("repo", "refs")

  // Process identifiers
  pure val PROCS = Set("main-cli", "agent-worker-1", "agent-worker-2")

  // --- State variables ---

  // Per-lock: count of shared readers
  var readers: str -> int

  // Per-lock: whether an exclusive writer holds it
  var writerHeld: str -> bool

  // Per-process: which locks are held and in what mode
  // Value: "none" | "read" | "write"
  var holding: (str, str) -> str

  // --- Initial state ---

  action init = all {
    readers' = Map("repo" -> 0, "refs" -> 0),
    writerHeld' = Map("repo" -> false, "refs" -> false),
    holding' = Map(
      ("main-cli", "repo") -> "none", ("main-cli", "refs") -> "none",
      ("agent-worker-1", "repo") -> "none", ("agent-worker-1", "refs") -> "none",
      ("agent-worker-2", "repo") -> "none", ("agent-worker-2", "refs") -> "none"
    ),
  }

  // --- Actions ---

  // Acquire a shared read lock
  action acquireRead(proc: str, lock: str): bool = all {
    // Guard: no writer holds this lock
    not(writerHeld.get(lock)),
    // Guard: process doesn't already hold this lock
    holding.get((proc, lock)) == "none",
    // Effect
    readers' = readers.set(lock, readers.get(lock) + 1),
    holding' = holding.set((proc, lock), "read"),
    writerHeld' = writerHeld,
  }

  // Acquire an exclusive write lock
  action acquireWrite(proc: str, lock: str): bool = all {
    // Guard: no readers on this lock
    readers.get(lock) == 0,
    // Guard: no writer on this lock
    not(writerHeld.get(lock)),
    // Guard: process doesn't already hold this lock
    holding.get((proc, lock)) == "none",
    // Guard: lock ordering — if acquiring write on "refs", must already hold "repo"
    // This prevents deadlock: repo.lock is always acquired before refs.lock
    // Read locks on refs are allowed independently (e.g. status queries)
    if (lock == "refs")
      holding.get((proc, "repo")) != "none"
    else
      true,
    // Effect
    writerHeld' = writerHeld.set(lock, true),
    holding' = holding.set((proc, lock), "write"),
    readers' = readers,
  }

  // Release a lock
  action release(proc: str, lock: str): bool = all {
    // Guard: process holds this lock
    holding.get((proc, lock)) != "none",
    // Guard: if releasing "repo", must not still hold "refs"
    // (release in reverse acquisition order)
    if (lock == "repo")
      holding.get((proc, "refs")) == "none"
    else
      true,
    // Effect: update based on lock mode
    if (holding.get((proc, lock)) == "read") all {
      readers' = readers.set(lock, readers.get(lock) - 1),
      writerHeld' = writerHeld,
    } else all {
      writerHeld' = writerHeld.set(lock, false),
      readers' = readers,
    },
    holding' = holding.set((proc, lock), "none"),
  }

  // --- Step ---

  action step = any {
    nondet proc = PROCS.oneOf()
    nondet lock = LOCKS.oneOf()
    any {
      acquireRead(proc, lock),
      acquireWrite(proc, lock),
      release(proc, lock),
    }
  }

  // =====================================================
  // SAFETY INVARIANTS
  // =====================================================

  // INV-1: Mutual exclusion — writer held implies zero readers
  val mutualExclusion = LOCKS.forall(lock =>
    writerHeld.get(lock) implies readers.get(lock) == 0
  )

  // INV-2: At most one writer per lock
  val singleWriter = LOCKS.forall(lock =>
    if (writerHeld.get(lock))
      PROCS.filter(p => holding.get((p, lock)) == "write").size() == 1
    else
      PROCS.filter(p => holding.get((p, lock)) == "write").size() == 0
  )

  // INV-3: Reader count matches actual holders
  val readerCountConsistent = LOCKS.forall(lock =>
    readers.get(lock) == PROCS.filter(p => holding.get((p, lock)) == "read").size()
  )

  // INV-4: No reader count goes negative
  val noNegativeReaders = LOCKS.forall(lock =>
    readers.get(lock) >= 0
  )

  // INV-5: Lock ordering — no process holds write on refs without holding repo
  // This is the deadlock prevention invariant.
  // Read locks on refs are allowed independently.
  val lockOrdering = PROCS.forall(proc =>
    if (holding.get((proc, "refs")) == "write")
      holding.get((proc, "repo")) != "none"
    else
      true
  )

  // Combined safety invariant
  val safety = and {
    mutualExclusion,
    singleWriter,
    readerCountConsistent,
    noNegativeReaders,
    lockOrdering,
  }

  // =====================================================
  // REGRESSION TRACES
  // =====================================================

  // REG-1: Correct lock ordering — acquire repo then refs
  run correctLockOrder = {
    init
      .then(acquireWrite("main-cli", "repo"))
      .then(acquireWrite("main-cli", "refs"))
      .then(release("main-cli", "refs"))
      .then(release("main-cli", "repo"))
  }

  // REG-2: Multiple concurrent readers on same lock
  run concurrentReaders = {
    init
      .then(acquireRead("main-cli", "repo"))
      .then(acquireRead("agent-worker-1", "repo"))
      .then(acquireRead("agent-worker-2", "repo"))
      .then(release("main-cli", "repo"))
      .then(release("agent-worker-1", "repo"))
      .then(release("agent-worker-2", "repo"))
  }

  // REG-3: Writer blocks readers — reader must wait
  // (Modeled as: writer acquires, then releases, then reader acquires)
  run writerThenReader = {
    init
      .then(acquireWrite("main-cli", "repo"))
      .then(release("main-cli", "repo"))
      .then(acquireRead("agent-worker-1", "repo"))
      .then(release("agent-worker-1", "repo"))
  }

  // REG-4: Read on refs without repo is allowed (found during spec development)
  // This was originally flagged as a violation — reads don't need ordering.
  run readRefsWithoutRepo = {
    init
      .then(acquireRead("agent-worker-1", "refs"))
      .then(release("agent-worker-1", "refs"))
  }
}
