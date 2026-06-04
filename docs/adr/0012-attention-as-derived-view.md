# Attention as a derived view

Attention items are derived views over underlying repository records, not an independent task database. Discussions, review requirements, thread state, anchor health, and context checks remain the source of truth; assignment, read state, and snooze can be explicit per-actor overlays when needed.

Discussion blockers are discussion turns with blocker turn kind when authored as conversation content. Assignment, read state, and snooze remain attention overlays. This preserves the durable fact that a human or agent explicitly declared a blocker.

**Status:** proposed

**Considered Options:** Making every attention item a durable object would simplify inbox queries, but it would duplicate state and risk drift such as a resolved discussion still appearing as an open task. A derived view keeps `heddle inbox` honest while preserving room for actor-specific overlays.
