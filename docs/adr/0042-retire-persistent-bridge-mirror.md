---
status: accepted
---

# Retire Persistent Bridge Mirror

Heddle will retire the persistent repo-local `.heddle/git` bridge mirror as a user-facing and long-term internal staging repository. Git Overlay writes directly to the checkout's real `.git`, while explicit Git import/export and Git remote operations will stream from Heddle state plus Raw Git Object Residuals through durable Git projection mapping. The mirror existed to preserve byte fidelity and stage bridge operations, but those responsibilities are clearer and safer as reconstructable Heddle-native state plus content-addressed residual Git object bytes for the cases Heddle cannot reconstruct exactly.

## Consequences

- Public `bridge git` commands are retired in favor of `adopt`, `import git`, `export git`, and top-level remote verbs routed by remote capability.
- Raw Git Object Residuals are required for non-byte-faithful imported objects that must round-trip byte-identically.
- Existing `.heddle/git` mirrors migrate lazily into residual storage and become removable through maintenance cleanup.
