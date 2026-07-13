---
status: accepted
---

# Dispatch source operations by repository authority

Repository Source Authority determines which storage and transport adapter executes a source operation. The Heddle command surface stays consistent across authorities where the semantics are shared.

In Git Overlay, the real `.git` is authoritative for commits, refs, packs, index, and worktree state. Sley executes `clone`, `commit`, `pull`, `push`, and `remote` directly against it. Heddle metadata remains in `.heddle`. There is no normal `.heddle/git` mirror and no dependency on the `git` executable.

`capture` records the granular Heddle state, provenance, and coordination metadata. `commit` then records captured source history in `.git`. `land` projects a managed thread into the same authoritative Git store. Remote verbs use Sley configuration and streaming transport.

In Native Heddle, `capture`, `pull`, `push`, `remote`, and workflow commands use Heddle-owned storage and transport. `commit` is unnecessary because a capture is already native source history. `adopt` atomically imports the selected Git history, switches durable source authority, and exposes the full native feature set.

Behavior, recommendations, and machine action templates select typed source actions from durable authority. They do not repair invalid command strings after construction.

## Consequences

- Source authority chooses a deep adapter, not an external executable.
- Git Overlay mutations must preserve `.git` and `.heddle` consistency or fail with typed recovery.
- Credentials, progress, and remote configuration flow through Sley interfaces.
- Unsupported Git operations fail closed; users may choose another Git-compatible client without making it a Heddle dependency.
- Compatibility shims and persistent bridge mirrors are not part of the model.
