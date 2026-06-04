# Collaboration visibility convergence

Collaboration visibility changes do not use last-writer-wins. Visibility is policy-sensitive, so concurrent incompatible visibility changes materialize as a visibility conflict and the effective view should choose the most restrictive safe visibility until the conflict is resolved.

Visibility conflicts create attention items and can block readiness when the record is linked to current work or hosted sync. Unrelated historical visibility conflicts can remain diagnostic-only.

Visibility conflicts use the shared collaboration conflict-resolution pattern with a visibility-specific payload. The operation cites the conflicting visibility operations, the chosen effective visibility, and the authority context for making that choice.

**Status:** accepted

**Considered Options:** Last-writer-wins visibility would be easy to implement, but it could silently expose restricted collaboration content. Always requiring single-writer visibility would reduce concurrency too much for local-first collaboration.
