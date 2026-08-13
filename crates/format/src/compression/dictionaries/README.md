# Tree/state zstd dictionaries

`tree-state-v1.zdict` is an immutable, bundled zstd dictionary for loose tree
and state objects. Its Heddle dictionary ID is `1`; that ID and these exact
bytes must remain available in every future decoder. The asset's BLAKE3 is
`ca0ee171814fa7bf91f4c22ef7a1a4d87f7f13a4052b47774e50a8c90a85cd80` and a
unit test pins it.

The v1 dictionary is trained offline from a deterministic synthetic corpus of
512 serialized trees and 384 serialized states shaped like Heddle repository
history. A disjoint corpus of 128 trees and 128 states is used for the size
measurement, so the reported result does not measure the training inputs.

Reproduce the asset and holdout measurement from the repository root:

```text
cargo run -p heddle-objects --features zstd \
  --example train_tree_state_dictionary -- \
  crates/format/src/compression/dictionaries/tree-state-v1.zdict
```

Do not overwrite an existing dictionary in a released format. Train a new
asset, assign the next nonzero Heddle dictionary ID, retain all prior assets,
and add the new ID to the decoder registry.
