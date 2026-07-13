---
status: accepted
---

# Contract the CLI to Heddle-owned concepts

Heddle's public CLI exposes the native workflow and durable concepts Heddle owns. Implementation primitives, Git-compatibility aliases, developer checks, and Weft control-plane operations do not remain public merely because their code lives in this repository.

The everyday workflow is `init` or `adopt`, then `status`, `diff`, `capture`, `start`, `ready`, and `land`. Native repositories also own `push` and `pull`. `sync`, `resolve`, `continue`, and `abort` own workflow reconciliation. Manual `merge` and `rebase` implementations remain behind those workflow modules rather than competing as public verbs.

Git Overlay users run Git-native commands such as `git switch`, `git stash`, `git clean`, and `git fetch` directly. Heddle does not provide aliases for them. Explicit projection operations remain under Heddle only when they translate between Git and Heddle data.

Hosted authentication needed by Heddle remains available, but Weft support grants, spool administration, external proof flows, and presence publishing are not Heddle version-control commands. Their interfaces belong to Weft tools, hosted products, or internal adapters.

## Consequences

- Remove `commit`, `checkpoint`, `switch`, `stash`, `clean`, `fetch`, `git-overlay`, `merge`, and `rebase` from the public command tree.
- Keep publication separate from `land`; Git Overlay publishes with direct Git commands.
- Remove `support`, `spool`, `prove`, and `presence` from the Heddle CLI.
- Prefer deleting obsolete command paths over hiding aliases or preserving compatibility shims.
- Keep low-level implementations only when a surviving deep module uses them.
- Command catalog, help, schemas, docs, recovery actions, and tests must derive from the contracted tree.
