// Collaboration Op-Set Convergence
//
// Models immutable collaboration operations independently from source history.
// Replicas converge by set union, expose only causally closed operations, and
// derive conflicts, heads, and display order from operation identity.

module collaboration_convergence {
  type OpKind = Append | Resolve | Reopen

  pure val ROOT = "op-root"
  pure val OP_IDS = Set(
    ROOT,
    "op-turn-a",
    "op-turn-b",
    "op-resolve-a",
    "op-resolve-b",
    "op-conflict-resolve-a",
    "op-conflict-resolve-b",
    "op-reopen",
    "op-missing-parent",
    "op-missing-child",
    "op-rejected-parent",
    "op-rejected-child",
    "op-valid-continuation",
    "op-duplicate-content-a",
    "op-duplicate-content-b"
  )

  pure val SUBJECTS = Set("discussion", "turn-conflict", "resolution-conflict")

  pure def kindOf(id: str): OpKind = {
    if (id == "op-resolve-a" or id == "op-resolve-b"
        or id == "op-conflict-resolve-a" or id == "op-conflict-resolve-b")
      Resolve
    else if (id == "op-reopen")
      Reopen
    else
      Append
  }

  pure def parentsOf(id: str): Set[str] = {
    if (id == ROOT)
      Set()
    else if (id == "op-turn-a" or id == "op-turn-b"
        or id == "op-rejected-parent" or id == "op-valid-continuation"
        or id == "op-duplicate-content-a" or id == "op-duplicate-content-b"
        or id == "op-missing-parent")
      Set(ROOT)
    else if (id == "op-resolve-a" or id == "op-resolve-b")
      Set("op-turn-a", "op-turn-b")
    else if (id == "op-conflict-resolve-a" or id == "op-conflict-resolve-b")
      Set("op-resolve-a", "op-resolve-b")
    else if (id == "op-reopen")
      Set("op-resolve-a")
    else if (id == "op-missing-child")
      Set("op-missing-parent")
    else if (id == "op-rejected-child")
      Set("op-rejected-parent")
    else
      Set()
  }

  pure def subjectOf(id: str): str = {
    if (id == "op-resolve-a" or id == "op-resolve-b" or id == "op-reopen")
      "turn-conflict"
    else if (id == "op-conflict-resolve-a" or id == "op-conflict-resolve-b")
      "resolution-conflict"
    else
      "discussion"
  }

  pure def contentOf(id: str): str = {
    if (id == "op-duplicate-content-a" or id == "op-duplicate-content-b")
      "same-content"
    else if (id == "op-resolve-a")
      "choose-a"
    else if (id == "op-resolve-b")
      "choose-b"
    else if (id == "op-conflict-resolve-a")
      "merge-resolution-a"
    else if (id == "op-conflict-resolve-b")
      "merge-resolution-b"
    else
      id
  }

  pure def directPrecedes(ancestor: str, descendant: str): bool =
    parentsOf(descendant).contains(ancestor)

  pure def withinTwo(ancestor: str, descendant: str): bool = or {
    directPrecedes(ancestor, descendant),
    parentsOf(descendant).exists(parent => directPrecedes(ancestor, parent)),
  }

  pure def withinThree(ancestor: str, descendant: str): bool = or {
    directPrecedes(ancestor, descendant),
    parentsOf(descendant).exists(parent => withinTwo(ancestor, parent)),
  }

  pure def precedes(ancestor: str, descendant: str): bool = or {
    directPrecedes(ancestor, descendant),
    parentsOf(descendant).exists(parent => withinThree(ancestor, parent)),
  }

  pure def heads(ids: Set[str]): Set[str] = ids.filter(id =>
    not(ids.exists(other => precedes(id, other)))
  )

  pure def rank(id: str): int = {
    if (id == ROOT) 0
    else if (id == "op-turn-a") 1
    else if (id == "op-turn-b") 2
    else if (id == "op-resolve-a") 3
    else if (id == "op-resolve-b") 4
    else if (id == "op-conflict-resolve-a") 5
    else if (id == "op-conflict-resolve-b") 6
    else if (id == "op-reopen") 7
    else if (id == "op-missing-parent") 8
    else if (id == "op-missing-child") 9
    else if (id == "op-rejected-parent") 10
    else if (id == "op-rejected-child") 11
    else if (id == "op-valid-continuation") 12
    else if (id == "op-duplicate-content-a") 13
    else 14
  }

  pure def displayHead(ids: Set[str]): str = heads(ids).fold("", (selected, id) =>
    if (selected == "" or rank(id) < rank(selected)) id else selected
  )

  pure def conflictingSubjects(ids: Set[str]): Set[str] = SUBJECTS.filter(subject =>
    ids.exists(left => and {
      kindOf(left) == Resolve,
      subjectOf(left) == subject,
      ids.exists(right => and {
        kindOf(right) == Resolve,
        subjectOf(right) == subject,
        contentOf(left) != contentOf(right),
        not(precedes(left, right)),
        not(precedes(right, left)),
      }),
    })
  )

  pure def reopenedSubjects(ids: Set[str]): Set[str] = SUBJECTS.filter(subject =>
    ids.exists(reopen => and {
      kindOf(reopen) == Reopen,
      subjectOf(reopen) == subject,
      ids.exists(resolve => and {
        kindOf(resolve) == Resolve,
        subjectOf(resolve) == subject,
        precedes(resolve, reopen),
      }),
    })
  )

  var arrivedA: Set[str]
  var arrivedB: Set[str]
  var visibleA: Set[str]
  var visibleB: Set[str]
  var hostedReceived: Set[str]
  var hostedAccepted: Set[str]
  var hostedRejected: Set[str]

  action init = all {
    arrivedA' = Set(ROOT),
    arrivedB' = Set(ROOT),
    visibleA' = Set(ROOT),
    visibleB' = Set(ROOT),
    hostedReceived' = Set(ROOT),
    hostedAccepted' = Set(ROOT),
    hostedRejected' = Set(),
  }

  action unchangedHosted = all {
    hostedReceived' = hostedReceived,
    hostedAccepted' = hostedAccepted,
    hostedRejected' = hostedRejected,
  }

  action arriveA(id: str): bool = all {
    OP_IDS.contains(id),
    arrivedA' = arrivedA.union(Set(id)),
    arrivedB' = arrivedB,
    visibleA' = visibleA,
    visibleB' = visibleB,
    unchangedHosted,
  }

  action arriveB(id: str): bool = all {
    OP_IDS.contains(id),
    arrivedA' = arrivedA,
    arrivedB' = arrivedB.union(Set(id)),
    visibleA' = visibleA,
    visibleB' = visibleB,
    unchangedHosted,
  }

  action syncAFromB: bool = all {
    arrivedA' = arrivedA.union(arrivedB),
    arrivedB' = arrivedB,
    visibleA' = visibleA,
    visibleB' = visibleB,
    unchangedHosted,
  }

  action syncBFromA: bool = all {
    arrivedA' = arrivedA,
    arrivedB' = arrivedB.union(arrivedA),
    visibleA' = visibleA,
    visibleB' = visibleB,
    unchangedHosted,
  }

  action materializeA(id: str): bool = all {
    arrivedA.contains(id),
    parentsOf(id).subseteq(visibleA),
    visibleA' = visibleA.union(Set(id)),
    arrivedA' = arrivedA,
    arrivedB' = arrivedB,
    visibleB' = visibleB,
    unchangedHosted,
  }

  action materializeB(id: str): bool = all {
    arrivedB.contains(id),
    parentsOf(id).subseteq(visibleB),
    visibleB' = visibleB.union(Set(id)),
    arrivedA' = arrivedA,
    arrivedB' = arrivedB,
    visibleA' = visibleA,
    unchangedHosted,
  }

  action hostReceive(id: str): bool = all {
    OP_IDS.contains(id),
    hostedReceived' = hostedReceived.union(Set(id)),
    hostedAccepted' = hostedAccepted,
    hostedRejected' = hostedRejected,
    arrivedA' = arrivedA,
    arrivedB' = arrivedB,
    visibleA' = visibleA,
    visibleB' = visibleB,
  }

  def blockedDescendants: Set[str] = hostedReceived.filter(id => and {
    not(hostedRejected.contains(id)),
    hostedRejected.exists(rejected => precedes(rejected, id)),
  })

  def validContinuations: Set[str] = hostedReceived.filter(id => and {
    not(hostedRejected.contains(id)),
    not(blockedDescendants.contains(id)),
    parentsOf(id).size() > 0,
    parentsOf(id).subseteq(hostedAccepted),
  })

  action hostAccept(id: str): bool = all {
    validContinuations.contains(id),
    hostedAccepted' = hostedAccepted.union(Set(id)),
    hostedReceived' = hostedReceived,
    hostedRejected' = hostedRejected,
    arrivedA' = arrivedA,
    arrivedB' = arrivedB,
    visibleA' = visibleA,
    visibleB' = visibleB,
  }

  action hostReject(id: str): bool = all {
    hostedReceived.contains(id),
    id != ROOT,
    not(hostedAccepted.contains(id)),
    not(blockedDescendants.contains(id)),
    hostedRejected' = hostedRejected.union(Set(id)),
    hostedReceived' = hostedReceived,
    hostedAccepted' = hostedAccepted,
    arrivedA' = arrivedA,
    arrivedB' = arrivedB,
    visibleA' = visibleA,
    visibleB' = visibleB,
  }

  action step = any {
    nondet id = OP_IDS.oneOf()
    arriveA(id),
    nondet id = OP_IDS.oneOf()
    arriveB(id),
    syncAFromB,
    syncBFromA,
    nondet id = OP_IDS.oneOf()
    materializeA(id),
    nondet id = OP_IDS.oneOf()
    materializeB(id),
    nondet id = OP_IDS.oneOf()
    hostReceive(id),
    nondet id = OP_IDS.oneOf()
    hostAccept(id),
    nondet id = OP_IDS.oneOf()
    hostReject(id),
  }

  val replicaClosure = and {
    visibleA.subseteq(arrivedA),
    visibleB.subseteq(arrivedB),
    visibleA.forall(id => parentsOf(id).subseteq(visibleA)),
    visibleB.forall(id => parentsOf(id).subseteq(visibleB)),
  }

  val hostedSubsets = and {
    hostedAccepted.subseteq(hostedReceived),
    hostedRejected.subseteq(hostedReceived),
    blockedDescendants.subseteq(hostedReceived),
    hostedAccepted.intersect(hostedRejected).size() == 0,
    hostedAccepted.intersect(blockedDescendants).size() == 0,
    hostedRejected.intersect(blockedDescendants).size() == 0,
    validContinuations.subseteq(hostedReceived),
  }

  val hostedCausalClosure = hostedAccepted.forall(id =>
    parentsOf(id).subseteq(hostedAccepted)
  )

  val deterministicConvergence = visibleA == visibleB implies and {
    heads(visibleA) == heads(visibleB),
    displayHead(visibleA) == displayHead(visibleB),
    conflictingSubjects(visibleA) == conflictingSubjects(visibleB),
    reopenedSubjects(visibleA) == reopenedSubjects(visibleB),
  }

  val concurrentAppendsSurvive = {
    visibleA.contains("op-turn-a") and visibleA.contains("op-turn-b") implies
      heads(visibleA).contains("op-turn-a") or heads(visibleA).contains("op-turn-b")
        or visibleA.exists(id => precedes("op-turn-a", id) and precedes("op-turn-b", id))
  }

  val contentDoesNotDefineIdentity = {
    visibleA.contains("op-duplicate-content-a") and visibleA.contains("op-duplicate-content-b") implies
      visibleA.filter(id => contentOf(id) == "same-content").size() == 2
  }

  val derivedCollaborationSemantics = and {
    visibleA.contains("op-resolve-a") and visibleA.contains("op-resolve-b") implies
      conflictingSubjects(visibleA).contains("turn-conflict"),
    visibleA.contains("op-conflict-resolve-a") and visibleA.contains("op-conflict-resolve-b") implies
      conflictingSubjects(visibleA).contains("resolution-conflict"),
    visibleA.contains("op-reopen") implies
      reopenedSubjects(visibleA).contains("turn-conflict"),
    hostedRejected.contains("op-rejected-parent")
        and hostedReceived.contains("op-rejected-child")
        and not(hostedRejected.contains("op-rejected-child")) implies
      blockedDescendants.contains("op-rejected-child"),
    hostedAccepted.contains(ROOT)
        and hostedReceived.contains("op-valid-continuation")
        and not(hostedRejected.contains("op-valid-continuation")) implies
      validContinuations.contains("op-valid-continuation"),
  }

  val safety = and {
    replicaClosure,
    hostedSubsets,
    hostedCausalClosure,
    deterministicConvergence,
    concurrentAppendsSurvive,
    contentDoesNotDefineIdentity,
    derivedCollaborationSemantics,
  }

  // REG-1: A child arriving before its parent remains hidden until causal closure.
  run missingParent = {
    init
      .then(arriveA("op-missing-child"))
      .then(arriveA("op-missing-parent"))
      .then(materializeA("op-missing-parent"))
      .then(materializeA("op-missing-child"))
  }

  // REG-2: Concurrent sibling turns survive union and converge at both replicas.
  run siblingTurns = {
    init
      .then(arriveA("op-turn-a"))
      .then(materializeA("op-turn-a"))
      .then(arriveB("op-turn-b"))
      .then(materializeB("op-turn-b"))
      .then(syncAFromB)
      .then(syncBFromA)
      .then(materializeA("op-turn-b"))
      .then(materializeB("op-turn-a"))
  }

  // REG-3: Incompatible resolutions remain present and derive a conflict; replay is idempotent.
  run competingResolves = {
    init
      .then(arriveA("op-turn-a"))
      .then(materializeA("op-turn-a"))
      .then(arriveA("op-turn-b"))
      .then(materializeA("op-turn-b"))
      .then(arriveA("op-resolve-a"))
      .then(materializeA("op-resolve-a"))
      .then(arriveA("op-resolve-b"))
      .then(arriveA("op-resolve-b"))
      .then(materializeA("op-resolve-b"))
      .then(arriveA("op-reopen"))
      .then(materializeA("op-reopen"))
  }

  // REG-4: Competing resolutions of a resolution conflict form the next conflict recursively.
  run competingConflictResolutions = {
    init
      .then(arriveA("op-turn-a"))
      .then(materializeA("op-turn-a"))
      .then(arriveA("op-turn-b"))
      .then(materializeA("op-turn-b"))
      .then(arriveA("op-resolve-a"))
      .then(materializeA("op-resolve-a"))
      .then(arriveA("op-resolve-b"))
      .then(materializeA("op-resolve-b"))
      .then(arriveA("op-conflict-resolve-a"))
      .then(materializeA("op-conflict-resolve-a"))
      .then(arriveA("op-conflict-resolve-b"))
      .then(materializeA("op-conflict-resolve-b"))
  }

  // REG-5: A rejected parent's descendant is blocked while a sibling continuation remains valid.
  run rejectedParentContinuation = {
    init
      .then(hostReceive("op-rejected-parent"))
      .then(hostReject("op-rejected-parent"))
      .then(hostReceive("op-rejected-child"))
      .then(hostReceive("op-valid-continuation"))
      .then(hostAccept("op-valid-continuation"))
  }

  // REG-6: Equal content under distinct operation identities survives as two operations.
  run duplicateContentIdentity = {
    init
      .then(arriveA("op-duplicate-content-a"))
      .then(materializeA("op-duplicate-content-a"))
      .then(arriveA("op-duplicate-content-b"))
      .then(materializeA("op-duplicate-content-b"))
  }
}
