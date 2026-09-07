# Heddle

Heddle is a local-first, agent-native version control context. It is useful without a hosted account and is intended to be the everyday VCS interface for both humans and agents; hosted products add coordination and visibility around the same core ideas.

## Language

**Heddle**:
The local-first, self-contained agent-native version control system and OSS CLI. It is the everyday VCS interface for humans and agents; raw Git is an escape hatch and interoperability boundary, not a parallel workflow.
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

**Agent Identity Assurance**:
The evidence level behind Agent attribution: unknown, claimed by local input, observed through an authenticated harness integration, or attested by a signed binding from a policy-trusted provider, harness, or runner. Heddle pursues broad coverage and displays it honestly, but Heddle-owned ambient detection is capped at observed and model identity is not correctness evidence.
_Avoid_: verified agent without level, model trust score, inferred identity

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

**Conflict ID**:
The stable opaque identity of one source-conflict episode across candidate additions, anchor movement, and resolution attempts. It names the continuing conflict, not any exact candidate set or file location.
_Avoid_: hunk hash, path-derived ID, Conflict Version ID

**Conflict Version ID**:
The content address of one exact immutable version of a conflict's anchor, base, candidate set, and status. Resolutions cite the version they considered so a stale resolution cannot silently close a conflict changed by concurrent work.
_Avoid_: Conflict ID, mutable conflict record, latest version alias

**Conflicted State**:
A valid source-history State whose Conflict Set is non-empty. It can be captured, synced, inspected, extended, and used as a merge parent, but cannot become ready, land, or project as an ordinary Git commit until its source conflicts are resolved through attributed successor States.
_Avoid_: failed merge, invalid State, merge-in-progress file

**Conflict Working Candidate**:
The marker-free ordinary file content materialized for continued work while a source conflict remains open, initially using the current Thread's content for each unresolved region plus every conflict-free merge result. Capturing an edited working candidate adds an attributed Conflict Candidate without resolving the conflict; resolution is a separate operation.
_Avoid_: conflict markers, automatic resolution, canonical winner

**Source Conflict Resolution**:
An attributed operation that closes an exact Conflict Version by selecting a Conflict Candidate or synthesizing a new one. Agents and deterministic drivers may resolve only under signed policy with the required proof; otherwise their output remains a candidate proposal.
_Avoid_: deleting markers, path-level resolved flag, implicit merge winner

**Source Resolution Conflict**:
The state produced when actors make incompatible resolution claims against the same Conflict Version. Neither claim wins by ordering; both remain visible and readiness stays blocked until a later resolution explicitly considers them.
_Avoid_: last-write-wins resolution, latest resolver wins, hidden conflict

**Thread**:
The canonical unit of work and human product decision, carrying an evolving source tip with its captures, timeline, and collaboration from local attempt through ready review and landing. A ready Thread replaces the branch-plus-pull-request as workflow truth; Git representations are projections.
_Avoid_: Git branch, pull request, chat thread

**Thread Checkout**:
A disposable filesystem materialization of a Thread, not the Thread's identity or durable home. Heddle manages private checkout locations automatically for agents; humans may work in the current checkout or explicitly choose a visible location.
_Avoid_: thread directory, Git worktree identity, required path

**Writer Lease**:
Exclusive, scoped authority to advance one Thread's source lineage, managed ambiently by a harness integration during normal agent work. Declared fan-out receives separate Child Threads; unexpected writer contention fails closed with a machine-readable recovery action rather than silently forking.
_Avoid_: agent presence, filesystem lock, manual heartbeat workflow

**Child Thread**:
A Thread created automatically beneath a parent for declared parallel work, with its own Writer Lease and managed Thread Checkout. Its accepted result integrates into the parent, whose review surface aggregates the child's contribution and provenance.
_Avoid_: shared writer, timeline branch, manually managed worktree

**Thread Refresh**:
A controlled update of a Child Thread onto a moved parent: a true fast-forward when the child has no unique work, otherwise a provenance-preserving replay when conflict-free. It runs only at transactional or harness-declared idle boundaries; conflicts, changed intent or policy, and ambiguous side effects block it instead of changing files beneath an active worker.
_Avoid_: background rebase, asynchronous checkout mutation, unconditional fast-forward

**Thread Intent**:
The versioned, attributed statement of a Thread's desired outcome and acceptance criteria, optionally linked to its originating issue, prompt, or assignment. Agent-authored amendments remain proposals until principal-approved; unapproved material changes create attention and prevent delegated landing.
_Avoid_: task assignment, issue, agent prompt, mutable description

**Landing Delegation Policy**:
A signed, versioned delegation from a principal permitting a bounded class of ready Threads to land within explicit scope, impact ceilings, proof requirements, destination, and validity. Evaluation fails closed and records the exact policy version; missing evidence requires human judgment, agent confidence cannot grant authority, and policy changes cannot authorize themselves.
_Avoid_: confidence threshold, agent approval, automatic trust

**Offline Delegated Landing**:
A local landing authorized without contacting Weft by an unexpired Landing Delegation Policy and locally verifiable required evidence, unless the policy explicitly requires fresh hosted authorization. Weft independently accepts or rejects later publication without rewriting the local landing.
_Avoid_: cached hosted approval, unconditional offline authority, automatic rollback

**Gitlink**:
A source tree entry representing a Git submodule pointer to a commit in another repository. Its durable meaning is the entry path and format-aware target Git object ID, not ordinary file bytes.
_Avoid_: submodule blob, heddle-submodule blob, submodule file

**Gitlink Placeholder**:
A filesystem presentation of a Gitlink in a native Heddle worktree. It is not source history content; unchanged placeholder bytes preserve the Gitlink during capture, while edited placeholder bytes become an ordinary file replacement.
_Avoid_: submodule file, synthetic source file, magic blob

**Sley**:
Heddle's native Git-format engine. Sley owns Git object identity semantics and Git operation behavior, while Heddle owns the stable durable encoding of Heddle source history objects.
_Avoid_: external Git adapter, optional Git backend, Git subprocess wrapper

**Git Compatibility Boundary**:
The promise to preserve source fidelity, support everyday interoperability, and keep raw Git as a reliable escape hatch without reproducing every Git porcelain command, hook, index behavior, or historical accident. Advanced compatibility belongs in explicit bridge tooling rather than the Everyday Workflow.
_Avoid_: Git feature parity, Git porcelain clone, raw Git as parallel workflow

**Git Overlay**:
A Heddle sidecar operating on an existing Git checkout. It remains a first-class Repository Source Authority indefinitely: active Git reads and writes use the checkout's real `.git`, while Heddle stores Captures, Threads, provenance, discussions, and Git Projection Mapping under `.heddle`.
_Avoid_: copied Git mirror, imported-only Git repo, hidden Git checkout

**Repository Source Authority**:
The durable repository choice of Git Overlay or native Heddle as the owner of source objects, refs, and worktree behavior. It is stored in repository config; the presence of `.git` only makes Git Projection available and does not select authority.
_Avoid_: inferred Git mode, `.git`-presence authority, projection availability

**Native Heddle Repository**:
A repository whose source history is stored in Heddle's native object model under `.heddle`. Git interoperability is an explicit projection, import, or export path rather than the active source store.
_Avoid_: adopted Git Overlay, hidden Git repository, Git-backed Heddle repository

**Repository Adoption**:
The explicit transition from Git Overlay source storage into a Native Heddle Repository when native storage is desired. Adoption is not initialization, routine product maturity, or a prerequisite for continued first-class Heddle use.
_Avoid_: Git Overlay initialization, sidecar setup, ordinary Git import

**Bridge Mirror**:
The retired bare Git repository formerly stored at `.heddle/git`. Current-format repositories never create, read, repair, or migrate it; Git projection uses reconstructable Heddle state plus Raw Git Object Residuals. Use this term only for historical design context.
_Avoid_: active Git store, projection cache, current repository component

**Git Checkpoint (internal operation, not a CLI verb)**:
The Git commit that binds a Heddle State into the Git history of a Git Overlay checkout. The `capture` flow writes it automatically through to the checkout's real `.git`; it is the Git-facing handle shown to raw Git tooling, while the Heddle-facing handle remains the `hd-...` State ID.
_Avoid_: Heddle capture, bridge mirror commit, native state id

**Raw Git Object Residual**:
Verbatim Git object bytes preserved because Heddle cannot reconstruct them byte-for-byte from native state, such as lossy imports or identities that were not representable in Heddle's normalized model. Residuals are an exception path for Git fidelity, not the normal source of Git-overlay state.
_Avoid_: bridge mirror, active Git store, reconstructed object

**Git Projection Mapping**:
The durable relationship between Heddle states and the Git object ids, refs, and projection metadata needed to reproduce or synchronize Git-facing history. It survives without a Bridge Mirror and explains how Heddle state projects into Git.
_Avoid_: bridge mapping, mirror mapping, Git checkout state

**Repository Verification State**:
The machine-readable proof surface that describes repository mode, Git/Heddle agreement, worktree dirt, remote drift, active operations, workflow guidance, and machine-contract coverage. Human status text and command breadcrumbs should be rendered from this proof rather than from separate local guesses.
_Avoid_: health text, status prose, ad hoc preflight

**Verification Attestation**:
An immutable signed claim that an identified verifier ran a versioned check against an exact source State, with its result and completion time. Landing Delegation Policies choose trusted verifiers and required checks; a different source state, changed check definition, or revoked verifier makes the attestation inapplicable.
_Avoid_: tests-passed flag, agent confidence, verification log

**Machine-Contract Proof**:
The verification dimension that proves command catalog metadata, JSON envelopes, schema introspection, documentation drift checks, and op-id support agree. It should be derived from the command contract source of truth, not hand-maintained counters.
_Avoid_: hardcoded schema summary, docs-only checklist

**CRDT Collaboration Record**:
A concurrently editable collaboration artifact, such as a discussion or context annotation, that can merge independent local edits without replacing Heddle's immutable source history model.
_Avoid_: CRDT state model, CRDT source history

**Discussion**:
A repository-scoped collaboration record for a human or agent conversation anchored to code, state, symbol, thread, or review context. A discussion has durable identity independent of any single immutable state.
_Avoid_: state-attached discussion, comment thread

**Discussion ID**:
An opaque stable UUIDv7 identifier for a discussion. Human-readable meaning belongs in discussion titles, anchors, and list output, not in the identifier.
_Avoid_: discussion slug, content-addressed discussion id

**Collaboration Operation ID**:
An opaque stable content-addressed identifier for a collaboration operation. It is distinct from a source history ChangeId even if it uses familiar Heddle short-prefix UX. After a hosted push or pull, a published turn's id is the envelope a clone materializes from the hosted snapshot (author, timestamp, and `turn_op_key`); the originator's pre-push local-only id is retired so every machine agrees.
_Avoid_: change id for collaboration

**Collaboration Idempotency Key**:
A stable command-attempt key used to deduplicate retried collaboration writes that have the same intended effect. It is separate from the collaboration operation ID, which hashes the exact canonical operation envelope bytes.
_Avoid_: semantic operation id, normalized operation hash

**Discussion Title**:
A human-readable summary of a discussion. Broad anchors such as repository and thread discussions require a title; precise code anchors may derive one from the anchor and first turn.
_Avoid_: slug, subject line

**Context Annotation**:
Distilled durable knowledge about the repository, such as a constraint, invariant, or rationale. A context annotation is a repository collaboration record that may be authored directly or extracted from a discussion.
_Avoid_: discussion, comment, chat note

**Context Extraction**:
The explicit act of distilling a discussion into a context annotation. Decision turns do not automatically become context annotations.
_Avoid_: automatic context extraction

**Context Snapshot**:
The frozen relevant view of Context Annotations associated with an immutable source State or supplied at an agent work boundary. It records exactly which intent-, path-, and symbol-scoped guidance was known for provenance or replay, while stale or ambiguous guidance creates attention rather than being silently trusted.
_Avoid_: live context store

**Discussion Turn**:
An append-only contribution to a discussion. Corrections are represented by later turns rather than editing the original turn.
_Avoid_: editable message

**Discussion Turn Kind**:
The structured purpose of a discussion turn from a controlled set such as comment, question, answer, blocker, decision, handoff, or status. Turn kind gives agents and inbox views signal without replacing the turn body.
_Avoid_: message type explosion

**Discussion Turn Reference**:
A structured link from one discussion turn to an earlier collaboration operation it answers, supersedes, hands off from, or otherwise responds to. It is a workflow edge, not a quotation of the earlier turn body.
_Avoid_: text heuristic, quoted reply

**Discussion Reopen**:
An explicit operation that moves a resolved discussion back into active conversation while preserving the earlier resolution in the discussion history.
_Avoid_: unresolve, edit resolution

**Resolution Conflict**:
A discussion state where concurrent resolution operations make incompatible claims about how the discussion was closed. A resolution conflict remains attention-worthy until a later operation chooses the intended resolution.
_Avoid_: last-write-wins resolution, hidden resolution

**Resolution Conflict Resolution**:
A collaboration operation that chooses the intended outcome among incompatible resolution operations by citing the conflicting operations. It is distinct from ordinary discussion resolve or reopen operations.
_Avoid_: silent winner, ordinary resolve

**Collaboration Conflict Resolution**:
A collaboration operation that resolves an explicit collaboration conflict by citing the conflict kind, conflicting operations, chosen outcome, and authority context. Resolution conflicts and visibility conflicts use this shared pattern with kind-specific payloads.
_Avoid_: implicit conflict winner, last-write-wins

**Collaboration Attestation**:
A collaboration operation that signs or asserts a claim about earlier collaboration operations without mutating them. It can upgrade trust in old history while preserving content-addressed operation identity.
_Avoid_: patching signature, rewriting operation

**Collaboration Redaction**:
A privileged collaboration operation or hosted policy action that suppresses sensitive operation content from normal views or sync while preserving enough audit metadata to explain what was redacted. It is not general editing.
_Avoid_: edit, delete, rewrite

**Visibility Conflict**:
A collaboration state where concurrent visibility operations make incompatible policy-sensitive claims about who may see or act on a record. The effective view stays at the most restrictive safe visibility until a later operation resolves the conflict.
_Avoid_: last-write-wins visibility, silent visibility change

**Agent Resolution**:
A discussion resolution operation performed by an agent under capability policy. Agent resolutions carry agent attribution, confidence, and an explicit resolution kind.
_Avoid_: automatic closure

**Agent Coordination Discussion**:
A discussion used by agents working in parallel threads to exchange durable questions, blockers, decisions, and handoff context. Agent coordination discussions are visible to the delegating human or policy scope by default.
_Avoid_: ephemeral agent chat, thread-local note

**Agent Handoff**:
A coordination pattern where an agent transfers durable context, blockers, or next steps through discussions and attention targets. It is not a separate Heddle primitive unless future lifecycle needs justify one.
_Avoid_: handoff object

**Capability-Interrupted Agent**:
An agent whose in-flight work was interrupted because capability refresh removed authority needed for the task. It is not a completed agent, and it should be distinguishable from ordinary blockers in machine-readable state.
_Avoid_: done agent, generic blocked agent

**Agent Task Assignment**:
Operational metadata that defines an agent's delegated work and execution policy, such as whether offline continuation is allowed. Its identifier can be referenced by collaboration operations as optional provenance, but it is not repository collaboration history in v1.
_Avoid_: discussion task, collaboration assignment

**Agent Harness**:
An external runtime that selects models and drives agent prompts, tools, and processes. Heddle integrates with harnesses ambiently and owns the durable work lifecycle, but does not require agent execution to run through Heddle.
_Avoid_: Heddle runner, VCS orchestrator

**Identity Cursor**:
The current harness identity published into the workspace sidecar `.heddle/identity` by installed hooks. Fields use ACP names (`provider`, `model`, `thought_level`, `session`, `parent`). Each capture freezes that cursor onto that state. Mid-thread `/model` or `/effort` updates the cursor only; earlier states keep the pair they froze. Session segments rotate when a published provider, model, or thought_level changes. Empty → set is attach, not a rotate. Unpublished fields are omitted. Heddle does not invent a model from hoped-for env or `/proc`.
_Avoid_: model of the thread, hoped-for env, /proc hunt, Cursor-guesses-Sonnet

**Agent Timeline**:
A navigable Heddle-native history of an agent run's tool-call activity, cursor movement, branches, and linked source captures. It supports seeking to a recorded state and using that point as the origin of an alternate Heddle Thread without rewriting prior timeline or source history.
_Avoid_: raw transcript, runner log, chat history

**Timeline Retention Policy**:
An explicit user or organization policy permitting selected operational Timeline detail to be compacted while preserving identities, tombstones, fork points, decisions, and honest loss-of-materialization status. Without one, recorded states remain navigable and are never silently removed by age-based maintenance.
_Avoid_: automatic garbage collection, silent expiry, cache eviction

**Forensic Session Material**:
Raw agent traces and optional session transcripts that Heddle retains only after explicit local-retention consent. They remain local unless separately disclosed through Weft's Private State Substrate after a PII warning; normal push, readiness, and Timeline sync imply neither consent.
_Avoid_: agent timeline, default review evidence, required transcript

**Forensic Retention Consent**:
An explicit, scoped choice permitting Heddle to retain raw traces or session transcripts. It is independent of Forensic Disclosure Consent and is not implied by automatic scrubbed Timeline recording.
_Avoid_: timeline capture, repository privacy setting, upload consent

**Forensic Disclosure Consent**:
An explicit, scoped choice permitting retained Forensic Session Material to be encrypted and sent to Weft. It is independent of local retention and must identify the destination and PII risk.
_Avoid_: push consent, private-repository setting, retention consent

**FacetKind**:
The typed history-graph domain a durable fact belongs to. Source History is the only facet Git Projection, checkout, and land may select; confidential runtime, collaboration, agent timeline, and forensic material are adjacent facets with their own identities and laws.
_Avoid_: env/* thread, name-prefix convention

**Private State Substrate**:
Weft's reusable encrypted storage, recipient-policy, authorization, audit, and purge primitive for typed confidential objects. Runtime Profiles and opt-in Forensic Session Material share this security substrate without sharing schemas or graph laws.
_Avoid_: VisibilityTier::Private, private Source History, environment-version schema

**Runtime Profile**:
A confidential-runtime facet: a typed `EnvProfileRef` pointing at immutable `EnvProfileVersion` versions of named encrypted slots. It is not a Source History state or tree, cannot be checked out, landed, or selected by Git Projection, and must not be stored as an `env/*` source thread. Ciphertext bytes may reuse a byte store only if ownership, reachability, authorization, sync, purge, and projection stay facet-aware.
_Avoid_: env thread, encrypted source state, VisibilityTier::Private as encryption

**Runtime Profile Recipient**:
A versioned encryption public descriptor created or imported by a selected provider and endorsed by the principal's signing identity. The default is not an X25519 key derived from the signing seed. A software-exportable key is an explicit weaker-custody fallback.
_Avoid_: derived encryption subkey, signing-seed HKDF

**Policy Broker**:
The authorization boundary that resolves scoped decrypt requests and runs a child with the selected values without returning values or key material to its caller. It holds provider handles rather than exporting private keys. Hardware protects custody; the broker enforces authorization, revocation, and audit. Same-UID callers are cooperative, not an adversarial boundary.
_Avoid_: daemon-as-key-holder, CLI-held unwrap as agent isolation

**Timeline Operation**:
An immutable event in an agent timeline, such as creating a timeline step, moving a cursor, opening a timeline branch, or linking a tool capture. Timeline operations use Heddle-native attribution, versioned durable encoding, and explicit operation kinds rather than mutable log rows.
_Avoid_: latest timeline JSON, append-only text log

**Timeline Step**:
The durable timeline unit for one OpenCode tool call. A timeline step records the native tool call identity, scrubbed summary, result status, side-effect classification, and links to any tool capture without making raw tool payloads the default shared record.
_Avoid_: raw tool invocation, console transcript

**Timeline Cursor**:
The explicit position of a human, agent, or runner within an agent timeline. Seeking may materialize a linked source state without rewriting history; the first source mutation from a historical point automatically creates a Timeline Branch and an alternate Heddle Thread.
_Avoid_: UI scroll position, implicit last step

**Timeline Branch**:
A divergent continuation of an agent timeline from a prior timeline point, used for retries, alternate attempts, or reviewable forks of agent execution. Heddle creates one automatically, together with an alternate Heddle Thread, when source work resumes from a historical Timeline Cursor.
_Avoid_: source branch, Git branch, thread fork

**Native Tool Call ID**:
The stable identifier emitted by the OpenCode adapter or another native tool runtime for a single tool invocation. Heddle uses it to correlate timeline steps, deduplicate retries, and link tool captures; it is not a human display label or a source history change id.
_Avoid_: display name, request log line, change id

**Tool Capture**:
An automatic recovery point linked to a tool call that changed repository or worktree state, including a failed call that partially changed tracked state. It remains inspectable and recoverable without becoming a meaningful source-history boundary by itself.
_Avoid_: screenshot, raw command archive, tool transcript

**Task Provenance**:
Metadata that explains why or under which local delegation an agent produced collaboration operations. It is distinct from agent attribution, which names the actor that authored an operation.
_Avoid_: agent attribution, task authority

**Hosted Task Provenance Alias**:
A Weft-minted hosted-safe identifier that groups collaboration operations from the same local agent task within a specific Weft repository and policy scope. It is provenance for hosted views, not task assignment authority or runner lifecycle state.
_Avoid_: raw task assignment id, hosted task assignment

**Cross-Domain Provenance View**:
A derived view that correlates source attribution, collaboration operations, task assignment metadata, and sync metadata. It is not collaboration content unless a human or agent explicitly writes commentary about the relationship.
_Avoid_: provenance discussion, task record

**Durable Async Coordination**:
The local Heddle collaboration model where humans and agents exchange persistent records that can be read, queried, merged, and reconciled without live connectivity.
_Avoid_: real-time chat, presence

**Attention Target**:
A structured target for attention or readiness, such as a principal, agent, thread, role, or current checkout context. It may be entered through human-friendly mention syntax, but the durable meaning is the resolved target.
_Avoid_: raw @mention text, display-name routing

**Server-Validated Local Capability**:
A locally minted Biscuit capability whose maximum permission scope has been validated by Weft. Heddle can derive attenuated child capabilities locally, but hosted trust comes from Weft validating the root capability's scope.
_Avoid_: self-sovereign hosted token, locally trusted hosted token

**Capability Refresh**:
The automatic act of replacing the active server-validated local capability after Weft reports that hosted policy or grants have changed. Refresh creates a new capability identity linked to the prior active capability, is user-visible, affects future policy context and sync attempts, and does not mutate existing immutable Biscuit tokens or rewrite existing operation provenance.
_Avoid_: mutating a biscuit, silently rewriting capability history

**Derived Capability Narrowing**:
The automatic reduction of effective scope for locally derived capabilities when the refreshed root capability is narrower. Heddle treats derived capability effectiveness as capped by the current server-validated root rather than mutating previously minted child Biscuits.
_Avoid_: mutating child biscuits, stale derived authority

**Operation Capability Context**:
The capability context recorded in a collaboration operation when it is created. It contains capability identity and a canonical scope summary as local provenance about the actor's claimed authority and policy view at creation time, not full token material.
_Avoid_: hosted acceptance, remote grant

**Hosted Acceptance Context**:
Sync metadata describing the capability context Weft accepted for a collaboration operation on a specific remote. It stores acceptance facts such as capability identity, accepted scope summary, remote identity, accepted time, and policy or grant version when available, not full token material.
_Avoid_: operation capability context, creation authority

**Capability-Aware Local Filtering**:
Local CLI filtering that uses the active server-validated local capability to decide which collaboration records to show by default. It is policy context, not a hard security boundary over local filesystem data.
_Avoid_: local access control, local hosted enforcement

**Expired Capability Context**:
A cached capability context whose freshness window has passed without a successful Weft refresh. It may be used for degraded local reads with clear labeling, but it is not presented as current hosted authority.
_Avoid_: current permission, valid hosted scope

**Restricted Collaboration Record**:
A collaboration record that Heddle filters according to active capability policy. In the OSS local store, restriction does not imply encryption unless a future encrypted storage mode says so explicitly.
_Avoid_: encrypted discussion

**Collaboration Validity**:
The acceptance state of a collaboration operation. Local validity means Heddle can parse and structurally apply the operation; hosted validity means Weft accepts it under hosted policy.
_Avoid_: valid without scope

**Unknown Collaboration Author**:
A degraded local attribution state used when Heddle cannot resolve a principal or agent for a collaboration operation. Unknown authorship is visible and low-trust, and Weft may reject it under hosted policy.
_Avoid_: anonymous trusted author

**Import Actor**:
The principal or agent that imported external or orphaned collaboration history into Heddle. It is distinct from the original author metadata carried by the imported content.
_Avoid_: original author, anonymous importer

**Repository Collaboration Log**:
A repository-level collection of collaboration records that can reconcile independent local edits. Discussions are records in this log; their turns, resolutions, and anchor changes are part of the record's history.
_Avoid_: state discussion blob, per-turn CRDT

**Collaboration Store**:
The Heddle-native local storage for collaboration operations, indexes, and derived views. It uses repository-local content-addressed objects and rebuildable indexes rather than an embedded database as the durable source of truth.
_Avoid_: collaboration database, SQLite collaboration store

**Collaboration Store Layout**:
The local filesystem layout rooted at `.heddle/collaboration/`, with durable operations under `ops/` and rebuildable indexes, views, sync metadata, and temporary files under separate subdirectories.
_Avoid_: collaboration in source objects, collaboration in oplog

**Collaboration Retention**:
The policy for keeping or removing locally valid collaboration operations. In v1, locally valid collaboration operations are retained even when Weft rejects them under hosted policy; cleanup only removes temporary artifacts or rebuilds disposable indexes and views.
_Avoid_: collaboration garbage collection

**Collaboration Sync Lane**:
The Weft-backed repository synchronization lane that exchanges collaboration operations separately from immutable source objects while remaining part of normal Heddle push and pull behavior.
_Avoid_: separate discussion product sync, chat sync

**Collaboration Sync Metadata**:
Local metadata about whether collaboration operations are pending, accepted, rejected, blocked by hosted-invalid causal history, or cursor-synced with Weft. A hosted rejection is sync metadata about a retained local operation, not collaboration content.
_Avoid_: rejection operation

**Unresolved Collaboration Operation**:
A structurally valid local collaboration operation whose cited causal parents are not available locally. It is retained as pending/unresolved but is not applied to materialized views until parents arrive or an explicit import/orphan rule accounts for the gap.
_Avoid_: applied orphan operation, missing-parent success

**Invalid Collaboration Artifact**:
Malformed or unverifiable collaboration bytes retained for diagnostics or quarantine. It is not a locally valid collaboration operation and is not applied to materialized views.
_Avoid_: valid operation, silently deleted operation

**Collaboration Import Root**:
An explicit operation that introduces imported or orphaned collaboration history as a new causal root with recorded source, reason, and trust level.
_Avoid_: silent orphan apply, missing parent workaround

**Hosted Rejection Reason**:
Sync metadata explaining why Weft rejected a collaboration operation, with a stable machine-readable code and a human display message. It is not collaboration content and may be refreshed by later sync attempts.
_Avoid_: rejection comment, rejection turn

**Hosted-Valid Continuation**:
A new collaboration operation that continues a collaboration record without citing a hosted-rejected local operation or its hosted-invalid descendants as causal parents. It creates a Weft-acceptable causal path while preserving the rejected local history.
_Avoid_: rewriting rejection, deleting rejected parent

**Heddle-Hosted Collaboration**:
Collaboration records shared through a Heddle remote backed by Weft. Heddle discussions, context, and attention do not project through Git hosting or Git bridge surfaces.
_Avoid_: Git-hosted discussions, Git notes collaboration

**Local-Only Collaboration**:
Collaboration records that exist only in the local repository because no Heddle remote is configured or synchronized. Heddle should label this state when users might assume Git push shared the collaboration records.
_Avoid_: unsynced Git comments

**Linked Collaboration**:
Collaboration records that are causally or semantically tied to source content being synchronized, such as discussions anchored to pushed states, changes, changed paths or symbols, active blockers or questions for the pushed thread, and operations that continue those records.
_Avoid_: unrelated discussion sync

**Live Collaboration Sync**:
Iroh-backed synchronization that keeps already-synced collaboration records up to date while a Weft connection is active. Live sync updates the durable repository collaboration log; it is not a separate chat transport.
_Avoid_: chat stream, presence

**Collaboration Watch**:
An explicit foreground command mode that keeps collaboration records current through live sync and renders updates for humans or agents. It is the first live-sync lifecycle before daemon ownership exists.
_Avoid_: invisible background sync

**Collaboration Operation**:
An immutable event in the repository collaboration log, such as opening a discussion, appending a turn, resolving a discussion, retargeting an anchor, or changing visibility. Collaboration operations carry Heddle-style attribution for the acting principal, agent, capability context, and originating work context when known.
_Avoid_: document overwrite, latest discussion document

**Primary Collaboration Record**:
The single collaboration record that a collaboration operation primarily targets for indexing and materialization. Operations may reference other records or source anchors, but they do not have multiple primary records.
_Avoid_: multi-record operation target, implicit target

**Source History**:
The immutable version history of repository content states. Collaboration operations may reference source history, but they do not advance it.
_Avoid_: collaboration history

**Compensating Collaboration Operation**:
A later collaboration operation that corrects, reverses, or supersedes an earlier collaboration operation without erasing it.
_Avoid_: undo discussion, delete turn

**Causal Ordering**:
The ordering relationship between collaboration operations based on which prior operations each operation observed. Concurrent operations can both be valid without one overwriting the other.
_Avoid_: global total order

**Collaboration View**:
A derived current view of collaboration records produced from collaboration operations. Views are caches or query outputs, not the durable source of truth.
_Avoid_: source-of-truth discussion document

**Semantic Anchor**:
A discussion or annotation target that names the repository meaning it refers to, such as a repository, thread, state, file, line range, symbol, or review signal. It preserves the source state where the reference was made and the selector used to follow that meaning forward.
_Avoid_: raw line comment, path-only comment

**Anchor Status**:
The current resolution of a semantic anchor: current, moved, changed, ambiguous, or orphaned. Anchor status tells users and agents whether Heddle can still prove where the conversation belongs.
_Avoid_: stale flag

**Attention Item**:
Anything in a repository that needs a human or agent to notice and act, such as a discussion mention, resolution conflict, orphaned anchor, thread blocker, review requirement, hosted rejection, or stale context annotation. Attention items are derived views over underlying records, with explicit overlays for assignment, read state, or snooze when needed.
_Avoid_: notification, task, todo

**Attention Severity**:
The readiness impact of an attention item, such as blocker, warning, or informational. Only targeted or high-severity attention items should block readiness.
_Avoid_: priority

**Attention Target**:
The principal, agent, or thread expected to notice an attention item. Principal, agent, and thread targets are distinct because they represent accountability, actor identity, and work-unit relevance respectively.
_Avoid_: assignee string, text mention

**Agent-Native Loop**:
The core Heddle workflow: inspect status, check inbox, isolate or fan out work, discuss uncertainty, distill context, capture attributable work, review risk, integrate, and recover or verify as needed.
_Avoid_: developer loop, AI workflow

**Weft**:
The hosted store and policy-verification boundary for Heddle repositories. Weft serves the governed Iroh RPC contract to equal Heddle and Tapestry clients; it verifies client authority and coordinates shared state without owning either client's workflow.
_Avoid_: Heddle server, Heddle core

**Iroh Hosted Transport**:
The shared hosted call transport used by native Heddle clients, browser WebAssembly clients, and Weft application endpoints. It carries the governed protobuf contract over operation-scoped Iroh QUIC streams while keeping framing, call context, failures, and replay policy behind one interface.
_Avoid_: gRPC replacement contract, pack-only transport, browser WebTransport

**Hosted Client Parity**:
The requirement that Heddle and Tapestry address the same governed Weft operations over the Iroh RPC contract. A client's current interface or key capability may change how an operation is prepared or authorized, but does not make that operation exclusive to the client.
_Avoid_: Tapestry-only operation, CLI-only hosted API, web workflow authority

**Human-Signable Action**:
An exact canonical hosted operation proposed by an agent or client that lacks the required human key, with its payload digest, scope, expiry, and idempotency identity fixed before approval. It has no effect until a human reviews and signs it in a capable client and submits those same bytes for Weft verification.
_Avoid_: agent approval, unsigned mutation, mutable approval request

**Passkey-Capable Client**:
A client able to perform human-key operations with a passkey while keeping the private key inside its authenticator boundary. Tapestry has this capability natively in the browser today; that is a temporary signing-surface advantage, not ownership of the underlying Weft operation.
_Avoid_: Tapestry authority, server-held human key, browser-only operation

**Weft Relay**:
The Weft-operated Iroh relay that gives browser clients secure-WebSocket access and gives native clients a fallback path when direct UDP connectivity is unavailable. It forwards encrypted Iroh traffic and is distinct from the Weft application endpoint that terminates calls and enforces hosted policy.
_Avoid_: WebTransport endpoint, application proxy, hosted authorization service

**Tapestry**:
The browser client for Heddle collaboration, review, onboarding, and operational visibility. It is an equal client of Weft's Iroh RPC contract and currently provides native passkey ceremonies for human-key operations and Human-Signable Actions.
_Avoid_: Heddle web, Heddle core
