---
status: accepted
---

# Retire Persistent Bridge Mirror

Heddle does not maintain a repo-local `.heddle/git` bridge mirror. Git Overlay writes directly to the checkout's real `.git`; explicit Git import/export and Git remote operations stream from Heddle state plus Raw Git Object Residuals through durable Git Projection Mapping. Current-format runtimes never inspect or migrate the retired directory. Older repositories must be converted offline before they are opened by this clean cut.

## Consequences

- Public `bridge git` commands are retired in favor of `adopt`, `import git`, `export git`, and top-level remote verbs routed by remote capability.
- `init` in an existing Git checkout selects Git Overlay source authority. `adopt` imports the selected Git history, then atomically selects native Heddle source authority; the retained `.git` is available only through explicit Git Projection.
- Raw Git Object Residuals are required for non-byte-faithful imported objects that must round-trip byte-identically.
- Raw Git Object Residuals are the only fallback for an object that cannot be reconstructed byte-for-byte. Missing residual closure is a hard error.
- There is no runtime migration, mirror fallback, or mirror maintenance surface.
