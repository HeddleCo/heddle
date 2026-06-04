# Explicit cross-domain transactions

Collaboration operations can participate in explicit cross-domain transactions with source mutations, but remain independent by default. Workflows such as resolving a discussion by edit, extracting context from a discussion, or creating an agent handoff may link source and collaboration effects atomically; ordinary source undo, source capture, and discussion turns do not implicitly absorb each other.

**Status:** proposed

**Considered Options:** Automatically folding collaboration operations into source transactions would make some workflows convenient, but it would couple source history and collaboration history too tightly. Keeping all operations independent would be simpler, but it would make important semantic transitions like "resolved by this edit" easier to tear.
