# Versioned agent timeline operations

Agent timelines should use first-class Heddle-native timeline operations stored adjacent to source history. Timeline operations explain agent execution, tool calls, cursor movement, branches, and tool captures without advancing `HEAD` or becoming source history states. They may reference source states, oplog recovery entries, collaboration operations, and captures when those relationships are part of the execution provenance.

Agent timeline sync is designed as a Weft-backed capability from its first
hosted implementation. The local model, durable identity, hosted validation,
policy filtering, and sync metadata are designed together rather than joined by
a later export bridge. The local Timeline transport that preceded this design
is not part of the shared public API.

The public contract reserves two explicitly `PLANNED` interfaces.
`AgentGatewayService` defines resumable daemon-to-Weft timeline ingest and
command delivery. `AgentService` defines Tapestry-facing run queries, watches,
policy, permissions, and intervention. Neither interface is registered by Weft
or live in Tapestry for API 0.1.0. OpenCode is the first intended
full-intervention adapter; Codex and Claude Code remain capability-negotiated
and observe-only until honest control adapters exist. Local timeline objects and
commands may ship independently of those hosted interfaces.

When hosted sync ships, Weft may reject or suppress timeline operations under
hosted policy, but accepted timeline history remains Heddle-native repository
metadata rather than Git-hosted notes or provider transcripts.

Durable timeline operation bytes use canonical MessagePack envelopes with an explicit timeline operation schema version and an explicit operation kind. JSON, protobuf, gRPC, OpenCode adapter structs, and Tapestry view models are adapters, not the durable source of truth. The timeline codec follows the collaboration operation pattern: one latest encoder, explicit version dispatch, frozen decode-only historical schema snapshots, no blind unversioned decode path, and golden canonical byte/hash fixtures for representative operation kinds.

Native tool payload handling is scrubbed by default. Synced timeline operations include structured summaries, side-effect classification, stable native tool call IDs, and hashes of native tool payloads when needed for audit or deduplication. Raw native tool args, shell commands, environment fragments, stdout, stderr, and provider transcripts are not synced by default. A future explicit policy may permit narrower raw-payload sharing, but the default Weft path carries summaries and hashes rather than sensitive command material.

Every OpenCode tool call creates a timeline step. Tool calls that change repository or worktree state create tool captures linked from the timeline step, and a failed tool call still creates a capture when it changed tracked state before failing. This keeps failed partial mutations inspectable and recoverable instead of hiding them behind a failed runner status.

Timeline cursor movement is itself durable history. Moving a cursor writes a timeline operation and an oplog recovery entry so crash recovery and diagnostics can explain both the intended cursor transition and the repository recovery state. The cursor is not inferred solely from the latest timeline step or from UI position.

Shell tool steps default to `external-side-effects-unknown` unless Heddle can prove containment. Containment may come from a known sandbox, a restricted tool adapter, or another explicit execution boundary that proves the command could not affect state outside the declared repository or worktree scope. Without that proof, timeline views and policy checks should treat the shell call as possibly having external side effects even if no tracked repository changes were captured.

**Status:** proposed

**Considered Options:** Reusing runner logs would be simpler, but it would make provider transcript shape and local privacy accidents part of Heddle's durable model. Storing raw OpenCode tool calls in collaboration operations would reuse existing sync concepts, but it would confuse human collaboration records with execution telemetry and increase accidental disclosure risk. Keeping timelines local-only until later would defer privacy and schema questions, but it would force a migration when Weft sync arrives and would weaken the first hosted review surfaces. A Heddle-native, versioned, scrubbed timeline operation model keeps execution provenance first-class while preserving the boundary between source history, collaboration history, and sensitive raw tool payloads.
