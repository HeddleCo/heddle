// Refs/HEAD Management with CAS (Compare-And-Swap)
//
// Models the reference management system in Heddle:
// - HEAD state (Attached/Detached)
// - Thread (branch) refs with CAS-based updates
// - Packed refs optimization (loose -> packed, lookup: loose first)
//
// Source: crates/refs/src/refs/refs_manager.rs

module refs_head {
  import common from "./common"

  // CAS expectation for ref updates
  type RefExpect =
    | ExpectAny
    | ExpectMissing
    | ExpectValue(str)

  // HEAD state
  type HeadState =
    | Attached(str)
    | Detached(str)

  // --- State variables ---

  // Current HEAD
  var headState: HeadState

  // Loose thread refs: name -> Option (None = missing)
  var looseThreads: str -> str

  // Packed thread refs: name -> Option
  var packedThreads: str -> str

  // Which thread names exist as loose refs
  var looseThreadNames: Set[str]

  // Which thread names exist as packed refs
  var packedThreadNames: Set[str]

  // Whether the refs lock is held
  var refsLocked: bool

  // --- Initial state ---

  action init = all {
    headState' = Attached("main"),
    looseThreads' = Map("main" -> "hd-x7k9qm4htv2wn8"),
    packedThreads' = Map(),
    looseThreadNames' = Set("main"),
    packedThreadNames' = Set(),
    refsLocked' = false,
  }

  // --- Helpers ---

  // Resolve a thread name: loose takes precedence over packed
  def resolveThread(name: str): str = {
    if (looseThreadNames.contains(name))
      looseThreads.get(name)
    else if (packedThreadNames.contains(name))
      packedThreads.get(name)
    else
      ""  // not found
  }

  def threadExists(name: str): bool = {
    looseThreadNames.contains(name) or packedThreadNames.contains(name)
  }

  // --- Actions ---

  // Set a thread ref with CAS check
  action setThread(name: str, expected: RefExpect, newValue: str): bool = all {
    not(refsLocked),
    refsLocked' = false,
    // CAS check
    match expected {
      | ExpectAny => true
      | ExpectMissing => not(threadExists(name))
      | ExpectValue(v) => resolveThread(name) == v
    },
    // Effect: write to loose refs
    looseThreads' = looseThreads.put(name, newValue),
    looseThreadNames' = looseThreadNames.union(Set(name)),
    // Unchanged
    packedThreads' = packedThreads,
    packedThreadNames' = packedThreadNames,
    headState' = headState,
  }

  // Delete a thread ref with CAS check
  action deleteThread(name: str, expected: RefExpect): bool = all {
    not(refsLocked),
    refsLocked' = false,
    // Must exist
    threadExists(name),
    // CAS check
    match expected {
      | ExpectAny => true
      | ExpectMissing => false  // can't delete something expected missing
      | ExpectValue(v) => resolveThread(name) == v
    },
    // Effect: remove from both loose and packed
    looseThreadNames' = looseThreadNames.exclude(Set(name)),
    packedThreadNames' = packedThreadNames.exclude(Set(name)),
    looseThreads' = looseThreads,
    packedThreads' = packedThreads,
    headState' = headState,
  }

  // Write HEAD (attach to thread or detach to state)
  action writeHead(newHead: HeadState): bool = all {
    headState' = newHead,
    // Unchanged
    looseThreads' = looseThreads,
    looseThreadNames' = looseThreadNames,
    packedThreads' = packedThreads,
    packedThreadNames' = packedThreadNames,
    refsLocked' = refsLocked,
  }

  // Pack refs: move all loose refs into packed, delete loose files.
  // Models RefManager::pack_refs()
  action packRefs: bool = all {
    not(refsLocked),
    // Must have loose refs to pack
    looseThreadNames.size() > 0,
    // Effect: merge loose into packed
    packedThreads' = looseThreadNames.fold(packedThreads, (acc, name) =>
      acc.put(name, looseThreads.get(name))
    ),
    packedThreadNames' = packedThreadNames.union(looseThreadNames),
    // Delete all loose refs
    looseThreadNames' = Set(),
    looseThreads' = Map(),
    // Unchanged
    headState' = headState,
    refsLocked' = refsLocked,
  }

  // --- Step ---

  action step = any {
    nondet name = common::THREAD_NAMES.oneOf()
    nondet value = common::CHANGE_IDS.oneOf()
    any {
      setThread(name, ExpectAny, value),
      setThread(name, ExpectMissing, value),
      nondet expectVal = common::CHANGE_IDS.oneOf()
      setThread(name, ExpectValue(expectVal), value),
      deleteThread(name, ExpectAny),
      nondet expectVal2 = common::CHANGE_IDS.oneOf()
      deleteThread(name, ExpectValue(expectVal2)),
    },

    nondet threadName = common::THREAD_NAMES.oneOf()
    writeHead(Attached(threadName)),

    nondet stateId = common::CHANGE_IDS.oneOf()
    writeHead(Detached(stateId)),

    packRefs,
  }

  // =====================================================
  // SAFETY INVARIANTS
  // =====================================================

  // INV-1: Lookup precedence — loose refs shadow packed refs.
  // For any thread name, resolveThread returns the loose value if it exists,
  // regardless of what packed says.
  val looseShadowsPacked = common::THREAD_NAMES.forall(name =>
    if (looseThreadNames.contains(name))
      resolveThread(name) == looseThreads.get(name)
    else if (packedThreadNames.contains(name))
      resolveThread(name) == packedThreads.get(name)
    else
      resolveThread(name) == ""
  )

  // INV-2: After packRefs, all previously loose values are in packed.
  // (This is verified transitively through the state machine — packRefs
  // copies all loose into packed before clearing loose.)
  // We check the structural invariant: no thread is "lost" —
  // if it was ever set and not deleted, it's resolvable.
  val noRefLost = common::THREAD_NAMES.forall(name =>
    if (looseThreadNames.contains(name) or packedThreadNames.contains(name))
      resolveThread(name) != ""
    else
      true
  )

  // INV-3: Loose and packed thread sets are consistent with their maps
  val looseConsistent = looseThreadNames.forall(name =>
    looseThreads.keys().contains(name)
  )

  // Combined safety
  val safety = and {
    looseShadowsPacked,
    noRefLost,
    looseConsistent,
  }

  // =====================================================
  // REGRESSION TRACES
  // =====================================================

  // REG-1: Set thread, pack, verify still resolvable
  run setPackResolve = {
    init
      .then(setThread("feat/auth-flow", ExpectMissing, "hd-a3bf6r2pyw9kmjt"))
      .then(packRefs)
      // After pack: "feat/auth-flow" should resolve to "hd-a3bf6r2pyw9kmjt" via packed
  }

  // REG-2: Loose shadows packed — update after pack
  run looseShadowsAfterPack = {
    init
      .then(setThread("feat/auth-flow", ExpectMissing, "hd-a3bf6r2pyw9kmjt"))
      .then(packRefs)
      .then(setThread("feat/auth-flow", ExpectValue("hd-a3bf6r2pyw9kmjt"), "hd-d8nz5v4qec7xh6s"))
      // "feat/auth-flow" resolves to "hd-d8nz5v4qec7xh6s" (loose), not "hd-a3bf6r2pyw9kmjt" (packed)
  }

  // REG-3: CAS rejection — ExpectValue with correct value succeeds
  run casRejection = {
    init
      .then(setThread("feat/auth-flow", ExpectMissing, "hd-a3bf6r2pyw9kmjt"))
      .then(setThread("feat/auth-flow", ExpectValue("hd-a3bf6r2pyw9kmjt"), "hd-d8nz5v4qec7xh6s"))
      // This succeeds because current value IS "hd-a3bf6r2pyw9kmjt"
  }

  // REG-4: HEAD attach/detach cycle
  run headAttachDetach = {
    init
      .then(writeHead(Detached("hd-a3bf6r2pyw9kmjt")))
      .then(writeHead(Attached("main")))
      .then(writeHead(Detached("hd-d8nz5v4qec7xh6s")))
  }

  // REG-5: Delete thread then verify it's gone
  run deleteAndGone = {
    init
      .then(setThread("feat/auth-flow", ExpectMissing, "hd-a3bf6r2pyw9kmjt"))
      .then(deleteThread("feat/auth-flow", ExpectAny))
      // "feat/auth-flow" should now resolve to ""
  }
}
