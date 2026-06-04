# Inbox as the attention command

<!-- doctor-docs:planned -->
Heddle uses `heddle inbox` as the canonical cross-cutting command for attention items. Discussions, reviews, thread blockers, orphaned anchors, resolution conflicts, and stale context can each keep object-specific commands, but humans and agents need one stable command for "what needs me next?" The inbox is local-first and remains useful without Weft; when hosted attention is available, it can merge policy-filtered hosted overlays into the same view.

Attention should route through structured targets such as principals, agents, threads, roles, or the current checkout context. Human text can support convenient mention syntax, but durable inbox behavior should not depend on ambiguous display-name parsing.

<!-- doctor-docs:planned -->
Every `heddle inbox` JSON response should include an explicit schema identifier and version. Inbox is an agent work queue, so consumers need to detect contract changes without inferring them from field presence.

<!-- doctor-docs:planned -->
`heddle inbox` JSON should expose task provenance grouping when available and policy-visible. Human output may offer optional grouping by task provenance, but the default view should still prioritize severity, target, and recency so urgent attention items are not hidden inside task groups.

First-slice inbox JSON should not claim unread/read state unless explicit actor read-state metadata ships. It may expose derived attention status, targets, timestamps, and reasons; read and snooze overlays can be added later as explicit actor metadata.

Unresolved collaboration operations appear in `inbox` as diagnostics or attention when they affect current work or sync. They do not appear as normal discussion content until their causal parents are available or explicitly accounted for.

Local-only and hosted-rejected collaboration should appear in `inbox` when they affect actionability, readiness, or sync, but as typed diagnostics with reason codes and lane metadata. They should not be disguised as discussion turns or generic warnings.

**Status:** proposed

**Considered Options:** Keeping attention only under `discuss`, `review`, and `thread` would avoid another top-level command, but it would force agents to poll several surfaces and reconstruct priority themselves. `status` already owns the current checkout condition and next action, not a personal or agent work queue.
