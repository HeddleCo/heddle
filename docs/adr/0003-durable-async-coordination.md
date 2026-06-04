# Durable async coordination

Heddle discussions are durable asynchronous coordination records in the OSS CLI, while real-time delivery, presence, notification routing, unread state, and web inboxes belong to Weft and Tapestry. This keeps the local CLI useful offline and scriptable for agents, while the hosted products can add live collaboration without making Heddle's core primitives depend on a hosted account.

After the repo split, Heddle documentation owns the CLI/local model and cross-repo contracts for collaboration records. Weft server internals and Tapestry web implementation details should live in their own repositories, with Heddle ADRs naming them only where a boundary or protocol contract matters.

**Status:** proposed

**Considered Options:** Real-time chat in the OSS CLI would make discussions feel immediately collaborative, but it would pull hosted transport, presence, and notification semantics into the local core. State-only notes would preserve locality, but they would not support parallel-agent coordination across threads.
