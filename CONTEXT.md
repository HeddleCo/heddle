# Heddle

Heddle is the everyday version control for humans and agents. It runs in your existing Git repository with no migration, and Git remains first-class for storage, hosting, CI and teammates who use plain `git`. The decisions (capture, review, ready, land) happen in Heddle Threads, and Git history is their faithful projection. Raw Git is always available as an escape hatch, but it is not a second workflow.

Heddle is local-first and useful without a hosted account; hosted products add coordination and visibility around the same core ideas.

## Language

The everyday terms are below. See [the full glossary](docs/glossary.md) for the remaining domain language.

**Heddle**:
The local-first, self-contained agent-native version control system and OSS CLI. It is the everyday VCS interface for humans and agents in existing Git repositories without migration; raw Git remains an escape hatch within first-class Git interoperability, not a second workflow.
_Avoid_: Git add-on, hosted-only Heddle, web app, hosted backend

**Local-First**:
The complete everyday Heddle workflow remains available locally and offline, including Threads, Captures, Timeline navigation, context, discussion, review, readiness, landing, and recovery. Weft adds shared synchronization, hosted policy, identity, and private storage; Tapestry adds a richer decision interface without becoming prerequisites for core work.
_Avoid_: offline-only, hosted-required workflow, local cache

**Everyday Workflow**:
The shared human-and-agent verb path built from status, capture, start, ready, land, and undo, with machine output as an additive contract. Harness integrations manage checkout, Writer Lease, and heartbeat plumbing ambiently rather than creating a second agent workflow.
_Avoid_: agent-only workflow, lease protocol, raw Git workflow

**Agent-Native Version Control**:
Version control that makes agent work, whether performed by one agent or many in parallel, cheap to attempt, legible to assess, and safe to accept, reject, or recover through isolated work, attribution, provenance, and machine-readable proof. It removes repository archaeology from supervision so the human decides whether a change is right for the product; human-authored edits use the same everyday workflow.
_Avoid_: AI-native version control

**Principal**:
The human identity accountable for delegating or directly producing work. Principal attribution does not imply that the person manually authored agent-produced code or directly approved its landing.
_Avoid_: Git author, agent identity, automatic reviewer

**Agent**:
The producer identity for an AI agent and harness session that performed work on behalf of a Principal. Agent attribution explains production provenance; it does not confer authority or establish correctness.
_Avoid_: principal, approver, verifier

**Capture**:
The sole everyday save boundary, naming a coherent unit of work with its intent, attribution, and available proof. In Git Overlay it atomically writes the required Git Checkpoint; automatic recovery may preserve finer-grained intermediate states without promoting them to peer entries in meaningful history.
_Avoid_: Git commit, autosave, arbitrary snapshot

**Capture Selection**:
The one-shot selection of paths, hunks, or semantic units included in a Capture while unrelated working edits remain in place. It is evaluated at the save boundary and does not create a persistent staging area.
_Avoid_: Git index, staged state, partial worktree

**Conflict Set**:
An immutable, content-addressed collection of independently addressable unresolved source conflicts referenced by a State. Each conflict preserves stable identity, path or semantic anchor, one base, and a deduplicated set of attributed Conflict Candidates without making conflict-marker bytes canonical source.
_Avoid_: MERGE_STATE file, conflict-marker tree, list of conflicted paths

**Conflict Candidate**:
One attributed alternative within a source conflict, identifying its content, source State, Thread, producer, and relationship to prior candidates. Candidate identity is not directional; current and incoming are temporary presentation labels rather than durable ours and theirs sides.
_Avoid_: ours side, theirs side, conflict-marker section

**Conflicted State**:
A valid source-history State whose Conflict Set is non-empty. It can be captured, synced, inspected, extended, and used as a merge parent, but cannot become ready, land, or project as an ordinary Git commit until its source conflicts are resolved through attributed successor States.
_Avoid_: failed merge, invalid State, merge-in-progress file

**Source Conflict Resolution**:
An attributed operation that closes an exact Conflict Version by selecting a Conflict Candidate or synthesizing a new one. Agents and deterministic drivers may resolve only under signed policy with the required proof; otherwise their output remains a candidate proposal.
_Avoid_: deleting markers, path-level resolved flag, implicit merge winner

**Thread**:
The canonical unit of work and human product decision, carrying an evolving source tip with its captures, timeline, and collaboration from local attempt through ready review and landing. A ready Thread carries the workflow decision; Git history is its faithful projection for storage, hosting, CI, and teammates using plain `git`.
_Avoid_: Git branch, pull request, chat thread

**Thread Checkout**:
A disposable filesystem materialization of a Thread, not the Thread's identity or durable home. Heddle manages private checkout locations automatically for agents; humans may work in the current checkout or explicitly choose a visible location.
_Avoid_: thread directory, Git worktree identity, required path

**Child Thread**:
A Thread created automatically beneath a parent for declared parallel work, with its own Writer Lease and managed Thread Checkout. Its accepted result integrates into the parent, whose review surface aggregates the child's contribution and provenance.
_Avoid_: shared writer, timeline branch, manually managed worktree

**Thread Intent**:
The versioned, attributed statement of a Thread's desired outcome and acceptance criteria, optionally linked to its originating issue, prompt, or assignment. Agent-authored amendments remain proposals until principal-approved; unapproved material changes create attention and prevent delegated landing.
_Avoid_: task assignment, issue, agent prompt, mutable description

**Git Compatibility Boundary**:
The promise to preserve source fidelity, support everyday interoperability, and keep raw Git as a reliable escape hatch without reproducing every Git porcelain command, hook, index behavior, or historical accident. Advanced compatibility belongs in explicit bridge tooling rather than the Everyday Workflow.
_Avoid_: Git feature parity, Git porcelain clone, raw Git as parallel workflow

**Git Overlay**:
A Heddle sidecar operating on an existing Git checkout. It remains a first-class Repository Source Authority indefinitely, so existing Git repositories need no migration: active Git reads and writes use the checkout's real `.git`, while Heddle stores Captures, Threads, provenance, discussions, and Git Projection Mapping under `.heddle`.
_Avoid_: copied Git mirror, imported-only Git repo, hidden Git checkout

**Git Checkpoint (internal operation, not a CLI verb)**:
The Git commit that binds a Heddle State into the Git history of a Git Overlay checkout. The `capture` flow writes it automatically through to the checkout's real `.git`; it is the Git-facing handle shown to raw Git tooling, while the Heddle-facing handle remains the `hs-...` State ID.
_Avoid_: Heddle capture, bridge mirror commit, native state id

**Git Projection Mapping**:
The durable relationship between Heddle states and the Git object ids, refs, and projection metadata needed to reproduce or synchronize Git-facing history. It survives without a Bridge Mirror and explains how Heddle state projects into Git.
_Avoid_: bridge mapping, mirror mapping, Git checkout state

**Repository Verification State**:
The machine-readable proof surface that describes repository mode, Git/Heddle agreement, worktree dirt, remote drift, active operations, workflow guidance, and machine-contract coverage. Human status text and command breadcrumbs should be rendered from this proof rather than from separate local guesses.
_Avoid_: health text, status prose, ad hoc preflight

**Verification Attestation**:
An immutable signed claim that an identified verifier ran a versioned check against an exact source State, with its result and completion time. Landing Delegation Policies choose trusted verifiers and required checks; a different source state, changed check definition, or revoked verifier makes the attestation inapplicable.
_Avoid_: tests-passed flag, agent confidence, verification log

**Discussion**:
A repository-scoped collaboration record for a human or agent conversation anchored to code, state, symbol, thread, or review context. A discussion has durable identity independent of any single immutable state.
_Avoid_: state-attached discussion, comment thread

**Context Annotation**:
Distilled durable knowledge about the repository, such as a constraint, invariant, or rationale. A context annotation is a repository collaboration record that may be authored directly or extracted from a discussion.
_Avoid_: discussion, comment, chat note

**Context Snapshot**:
The frozen relevant view of Context Annotations associated with an immutable source State or supplied at an agent work boundary. It records exactly which intent-, path-, and symbol-scoped guidance was known for provenance or replay, while stale or ambiguous guidance creates attention rather than being silently trusted.
_Avoid_: live context store

**Source History**:
The immutable version history of repository content states. Collaboration operations may reference source history, but they do not advance it.
_Avoid_: collaboration history

**Semantic Anchor**:
A discussion or annotation target that names the repository meaning it refers to, such as a repository, thread, state, file, line range, symbol, or review signal. It preserves the source state where the reference was made and the selector used to follow that meaning forward.
_Avoid_: raw line comment, path-only comment

**Anchor Status**:
The current resolution of a semantic anchor: current, moved, changed, ambiguous, or orphaned. Anchor status tells users and agents whether Heddle can still prove where the conversation belongs.
_Avoid_: stale flag

**Attention Item**:
Anything in a repository that needs a human or agent to notice and act, such as a discussion mention, resolution conflict, orphaned anchor, thread blocker, review requirement, hosted rejection, or stale context annotation. Attention items are derived views over underlying records, with explicit overlays for assignment, read state, or snooze when needed.
_Avoid_: notification, task, todo

**Agent-Native Loop**:
The core Heddle workflow: inspect status, check inbox, isolate or fan out work, discuss uncertainty, distill context, capture attributable work, review risk, integrate, and recover or verify as needed.
_Avoid_: developer loop, AI workflow
