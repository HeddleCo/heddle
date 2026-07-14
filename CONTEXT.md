# Heddle

Heddle is a local-first, agent-native version control context. It is useful without a hosted account, while hosted products add coordination and visibility around the same core ideas.

## Language

**Heddle**:
The local-first, self-contained agent-native version control system and OSS CLI. Heddle must remain useful without a hosted account.
_Avoid_: hosted-only Heddle, web app, hosted backend

**Agent-Native Version Control**:
Version control designed around durable agent workflows as first-class behavior: isolated work, explicit attribution, retryable operations, disposable attempts, provenance, and machine-readable contracts.
_Avoid_: AI-native version control

**Gitlink**:
A source tree entry representing a Git submodule pointer to a commit in another repository. Its durable meaning is the entry path and format-aware target Git object ID, not ordinary file bytes.
_Avoid_: submodule blob, heddle-submodule blob, submodule file

**Gitlink Placeholder**:
A filesystem presentation of a Gitlink in a native Heddle worktree. It is not source history content; unchanged placeholder bytes preserve the Gitlink during capture, while edited placeholder bytes become an ordinary file replacement.
_Avoid_: submodule file, synthetic source file, magic blob

**Sley**:
Heddle's native Git-format engine. Sley owns Git object identity semantics and Git operation behavior, while Heddle owns the stable durable encoding of Heddle source history objects.
_Avoid_: external Git adapter, optional Git backend, Git subprocess wrapper

**Git Overlay**:
A Heddle sidecar operating on an existing Git checkout. Active Git-overlay reads and writes use the checkout's real `.git` repository for Git commits, refs, packs, index, and worktree state; Heddle stores native metadata such as captures, threads, provenance, discussions, and Git Projection Mapping under `.heddle`.
_Avoid_: copied Git mirror, imported-only Git repo, hidden Git checkout

**Repository Source Authority**:
The durable repository choice of Git Overlay or native Heddle as the owner of source objects, refs, and worktree behavior. It is stored in repository config; the presence of `.git` only makes Git Projection available and does not select authority.
_Avoid_: inferred Git mode, `.git`-presence authority, projection availability

**Native Heddle Repository**:
A repository whose source history is stored in Heddle's native object model under `.heddle`. Git interoperability is an explicit projection, import, or export path rather than the active source store.
_Avoid_: adopted Git Overlay, hidden Git repository, Git-backed Heddle repository

**Repository Adoption**:
The explicit transition from Git Overlay source storage into a Native Heddle Repository. Adoption is not normal Git Overlay initialization and is chosen when the repository needs Heddle-native source storage.
_Avoid_: Git Overlay initialization, sidecar setup, ordinary Git import

**Bridge Mirror**:
The bare Git repository at `.heddle/git` used by explicit Git bridge import, export, sync, reconstruction, and maintenance paths. It is not the active Git-overlay repository: normal Git-overlay reads use the checkout, while `commit` and branch movement write through to the checkout's real `.git` repository.
_Avoid_: active Git store, Git-overlay `.git`, canonical Git object store

**Git Checkpoint (internal operation, not a CLI verb)**:
The Git commit that binds a Heddle state into the Git history of a Git-overlay checkout. The `commit` flow writes it through to the checkout's real `.git` repository, and it is the Git-facing handle shown to raw Git tooling; the Heddle-facing handle remains the `hd-...` state id.
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
An opaque stable content-addressed identifier for a collaboration operation. It is distinct from a source history ChangeId even if it uses familiar Heddle short-prefix UX.
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
The frozen view of context annotations associated with an immutable source state. It records what guidance was known for provenance or replay, but it is not the live source of truth for context annotations.
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

**Agent Timeline**:
A Heddle-native record stream for an agent run's tool-call activity, cursor movement, branches, and captures. Foundation objects and local storage are in place; richer cursor views, capture automation, and hosted projection are still planned. The public `AgentGatewayService` and `AgentService` contracts are also planned and are not registered as live API services in the 0.1.0 candidate. Agent timelines are adjacent repository metadata that explain agent execution without becoming source history states.
_Avoid_: raw transcript, runner log, chat history

**Timeline Operation**:
An immutable event in an agent timeline, such as creating a timeline step, moving a cursor, opening a timeline branch, or linking a tool capture. Timeline operations use Heddle-native attribution, versioned durable encoding, and explicit operation kinds rather than mutable log rows.
_Avoid_: latest timeline JSON, append-only text log

**Timeline Step**:
The durable timeline unit for one OpenCode tool call. A timeline step records the native tool call identity, scrubbed summary, result status, side-effect classification, and links to any tool capture without making raw tool payloads the default shared record.
_Avoid_: raw tool invocation, console transcript

**Timeline Cursor**:
The explicit position of a human, agent, or runner view within an agent timeline. Cursor movement is recorded as timeline history, not inferred from the latest displayed step.
_Avoid_: UI scroll position, implicit last step

**Timeline Branch**:
A divergent continuation of an agent timeline from a prior timeline point, used for retries, alternate attempts, or reviewable forks of agent execution. Timeline branches do not create source branches by themselves.
_Avoid_: source branch, Git branch, thread fork

**Native Tool Call ID**:
The stable identifier emitted by the OpenCode adapter or another native tool runtime for a single tool invocation. Heddle uses it to correlate timeline steps, deduplicate retries, and link tool captures; it is not a human display label or a source history change id.
_Avoid_: display name, request log line, change id

**Tool Capture**:
A Heddle capture created because a tool call changed repository or worktree state. Repo-changing tool calls create tool captures, and failed tool calls still create captures when they changed tracked state before failing.
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
gRPC-backed synchronization that keeps already-synced collaboration records up to date while a Weft connection is active. Live sync updates the durable repository collaboration log; it is not a separate chat transport.
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
The hosted collaboration and coordination product for Heddle repositories. Weft owns hosted identity, policy, multi-user coordination, and remote collaboration behavior.
_Avoid_: Heddle server, Heddle core

**Tapestry**:
The hosted web product for Heddle collaboration, review, onboarding, and operational visibility.
_Avoid_: Heddle web, Heddle core
