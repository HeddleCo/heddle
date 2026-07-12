---
status: accepted
---

# Git Overlay initialization and native adoption

Existing Git repositories use Git Overlay by default: Git-format state and normal Git ref, index, object, and worktree operations use the checkout's real `.git`, while Heddle coordination, provenance, captures, threads, discussions, and Git Projection Mapping live under `.heddle`. `heddle init` establishes that sidecar relationship and must make the existing Git history truthfully visible without copying it into native storage.

`heddle adopt` is the explicit transition from Git Overlay into a Native Heddle Repository. Adoption moves source history into Heddle's native object model so native-only behavior can rely on Heddle source storage; it is not an alias for sidecar initialization or ordinary Git import.

## Consequences

- Git Overlay commands must operate directly on the real `.git` repository where Git semantics are authoritative.
- Repository source authority is explicit durable repository metadata. The presence of `.git` alone must not determine whether the repository is Git Overlay or Native Heddle.
- Composite Heddle operations may update both `.git` and `.heddle`, but they do so as one verified operation with an explicit Git Projection Mapping.
- Observe-only commands must never synthesize native history to hide an uninitialized or inconsistent overlay.
- Help, status guidance, verification, and onboarding use `init` for normal Git Overlay setup and reserve `adopt` for the explicit storage transition.
