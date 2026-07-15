# Heddle-native collaboration operations

Heddle uses a Heddle-native operation model for discussions, context annotations, attention, and collaboration convergence. External CRDT libraries may be tactical implementation aids, but they must not define the domain model; collaboration operations need Heddle-specific attribution, capability context, causal parents, source anchors, and resolution-conflict semantics.

**Status:** accepted

**Considered Options:** A general-purpose CRDT library could accelerate implementation, but it would likely impose document/list semantics that do not match Heddle's repository collaboration model. Heddle-native operations preserve control over the model and leave room to reimplement tactical dependencies later.
