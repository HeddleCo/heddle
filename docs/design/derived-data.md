# State-derived data

The contract lives in `heddle_object_model::object::derived`; execution and local
persistence live in `repo::derived`. There is no runtime registry or scheduler.
A Rust `Derivation` supplies a descriptor and deterministic composition of a
state's local change and its parents' values, in recorded parent order. An empty
parent sequence is the root base case.

A result key is `(StateId, derivation id, version)`. Algorithm, codec and immutable
configuration changes require a new version or id. Nodes encode the full key,
ordered parent keys and roots, and canonical value bytes. Their typed BLAKE3
hashes identify shared subgraphs. A binding is immutable: publishing a different
root for an existing key fails. This slice supports same-kind parent dependencies;
additional dependency kinds need an explicit contract extension.

`ensure(source, store, derivation, state, limits)` performs bottom-up execution
with a temporary SQLite worklist. Each input is discovered once; ancestor inputs
spill to disk. Only the current input, immediate parent values and composition
output remain in executor memory. The active-state index detects cycles without
a recursive call stack. All traversal and membership queries have work, byte,
parent-count and scratch-disk limits. Exhaustion returns a typed error and leaves
any already completed nodes reusable. It does not publish the requested result.

Eager callers stream new states in parent order. Lazy callers invoke the same
executor when a result is needed. A missing binding means coverage is unknown,
not an empty closure. A warm ensure reads one binding and its root node without
loading source history. Appending a state reads its input and immediate parent
results, without expanding their closures. This relies on the store retaining
the complete DAG of every live binding. Source/store caches and composition
scratch have their own bounds; the executor's `peak_working_bytes` is conservative
byte accounting, not process RSS. Async callers use bounded blocking workers;
there are no history-sized futures or recursively spawned tasks.

`LocalDerivedStore::open(path)` uses an existing parent directory, normally
`.heddle/state/derived.sqlite`. SQLite WAL with full synchronous commits persists
nodes before publishing bindings. Nodes and its page-cache target are each
bounded at 1 MiB; reads check blob length before copying it. Store transactions
never span source reads or host I/O. `DerivedStore` is the host adapter contract:
reads must enforce byte limits before allocating, writes must compare immutable
bytes/roots, and a binding may become visible only after its complete DAG is
persisted. Retention and job scheduling belong to the host.

The first implementation, `StateClosure`, stores a state's local change and
references its parent result nodes. `StateGraphSource` verifies a native state's
identity and projects its tree hash and parents. `closure_contains` answers exact
state membership with a disk-backed visited set. Shared ancestors are stored
once; merge parents are never summed into a cardinality. This is state-graph
membership, not tree/blob availability or authorization. Negative membership
requires exhausting the reachable graph; missing/corrupt nodes and budgets are
errors, never `false`. `load_node` exposes a validated result for inspection.

Tests exercise 100,000-state linear history on a 2 MiB thread stack, a 1,024-way
merge, eager/lazy recomputation, file-backed restart, a native-state source and
corrupt/missing/cyclic/version-conflicting dependencies. The deep executor test
uses an in-memory SQLite result database and a file-backed traversal worklist;
separate file-backed tests verify persistence. No dense transitive closure is
materialized.
