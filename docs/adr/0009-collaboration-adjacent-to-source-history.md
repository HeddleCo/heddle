# Collaboration adjacent to source history

Collaboration operations are adjacent repository metadata, not source history states. Opening, appending to, resolving, or retargeting a discussion does not create a new source state or advance `HEAD`; collaboration records have their own operation log, identities, attribution, and sync cursor while referencing source states when needed.

**Status:** proposed

**Considered Options:** Storing discussions on states matched the current implementation and made review payloads simple, but it made conversation updates mutate source history. Keeping collaboration adjacent preserves clean source history while allowing activity views to intentionally combine source and collaboration events.
