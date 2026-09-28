# Heddle compatibility promise

**Owner decision: 2026-09-28.** These commitments apply now, before 1.0.

## Stable

- **Heddle source objects:** the content-addressed object format, including States and Changes, preserves existing object identity and readability. A new encoding must not silently reinterpret or invalidate stored objects; any incompatible format needs an explicit version and migration path.
- **Identifiers:** `hs-` State IDs and `hc-` ChangeIds retain their meanings. A State ID names one immutable State; a ChangeId carries logical change identity across rewrites. Stored identities remain interpretable.
- **Git checkpoints:** `Heddle-State` and `Heddle-Change` commit trailers retain their names and meanings.
- **Git round-trip:** importing local Git history and exporting it reproduces byte-identical Git commit, tree, blob, and tag objects, with the same object IDs and a valid object graph. The [round-trip oracle](../README.md) is the compatibility gate.

Changes that would break these promises require an explicit new version and a path for existing repositories; they cannot arrive as silent changes to the current format.

## Unstable until 1.0

- **CLI verbs and flags:** command names, arguments, flags, and human output may change before 1.0. The `--output json` machine contract keeps its existing, separate version rule: the `heddle-cli` crate version is its contract version. Catalogued output shapes, including `output_kind`, discriminators, and exit codes, are stable within a pre-1.0 minor release; breaking changes bump the minor version and additive changes bump the patch version. See [the JSON contract](exit-codes.md#schemacontract-stability) and `heddle <command> --schema`.
- **Hosted wire API:** `heddle-api` remains in `0.31.0-alpha.*`; its alpha wire surfaces have no compatibility promise yet.
- **Hosted features:** behavior, availability, and policy may change before 1.0.
- **Other formats and Rust APIs:** no compatibility freeze is declared here for sidecars, refs, oplog internals, or published Rust APIs beyond the stable source-object commitment above.

Existing CI and release gates still apply. This document sets no additional coverage, performance, known-bug, soak, or deprecation-window threshold for 1.0.
