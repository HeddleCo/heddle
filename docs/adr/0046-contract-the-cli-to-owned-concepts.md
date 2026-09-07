---
status: accepted
---

# Contract the CLI to Heddle-owned concepts

Heddle's public CLI exposes the workflow and durable concepts Heddle owns. Implementation primitives, broad Git-compatibility aliases, developer checks, and Weft control-plane operations do not remain public merely because their code lives in this repository.

The everyday workflow is `init` or `clone`, then `status`, `diff`, `capture`, `start`, `ready`, and `land`. `pull`, `push`, and `remote` follow repository source authority. In Git Overlay, `capture` writes the Heddle State and Git Checkpoint as one verified operation under ADR 0052. `sync`, `resolve`, `continue`, and `abort` own workflow reconciliation. Manual `merge` and `rebase` implementations remain behind those workflow modules rather than competing as public verbs.

Git Overlay is not a compatibility shim. The checkout's real `.git` owns commits, refs, packs, index, and worktree state. Heddle uses Sley, its embedded Git engine, to implement capture checkpoints, `clone`, `pull`, `push`, and `remote` directly against that store. `.heddle` owns captures, provenance, threads, discussions, and mappings. Heddle never requires the `git` executable or a second `.heddle/git` object store; explicit projection uses reconstructable state plus Raw Git Object Residuals.

The surface stays intentionally narrow. Operations outside it may be performed with another Git-compatible client, but that client is optional and Heddle never invokes it. Explicit projection operations remain under Heddle when they translate between Git and Heddle data. `adopt` is the atomic transition from Git Overlay to Native Heddle and its full feature set.

Hosted authentication needed by Heddle remains available, but Weft support grants, spool administration, external proof flows, and presence publishing are not Heddle version-control commands. Their interfaces belong to Weft tools, hosted products, or internal adapters.

## Consequences

- Keep `capture` as the sole public save boundary and make its Git Overlay checkpoint atomic with the Heddle State update.
- Keep `clone`, `pull`, `push`, and `remote` authority-dispatched and backed by Sley in Git Overlay.
- Do not expose `checkpoint`, `switch`, `stash`, `clean`, `fetch`, `git-overlay`, `merge`, or `rebase` as competing top-level verbs.
- Keep publication separate from `land`; `heddle push` publishes through the active source adapter.
- Remove `support`, `spool`, `prove`, and hosted presence publishing from the Heddle CLI; local `agent presence` remains a Heddle coordination surface.
- Prefer deleting obsolete command paths over hiding aliases or preserving compatibility shims.
- Keep low-level implementations only when a surviving deep module uses them.
- Command catalog, help, schemas, docs, recovery actions, and tests must derive from the contracted tree.
