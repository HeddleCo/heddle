// SPDX-License-Identifier: Apache-2.0
//! Canonical manifest/object encodings and fsck rules for the immutable CAS.
//!
//! **Additive.** Nothing in the existing object or wire paths reads or writes
//! any of this yet. It is the upstream format definition the data-model
//! reshape will build on (weft epic #1052, Phase 1: *"Design canonical
//! manifest/object encodings and fsck rules upstream in Heddle because object
//! and wire formats belong there"*). Cutover is a later phase, deliberately.
//!
//! # What is here
//!
//! * [`node`] — the canonical manifest node: a 32-way HAMT keyed by
//!   `(object kind, object hash)`, addressed by `BLAKE3` of its canonical
//!   bytes. One logical node has exactly one byte string, so identical
//!   membership always yields an identical root.
//! * [`build`] — deterministic construction and expansion. Replacing one object
//!   rewrites only the old and new routes; every other subtree keeps its hash.
//! * [`binding`] — the `(spool, facet, owner) -> content root` binding, with
//!   owner identity deliberately outside the shared root.
//! * [`extent`] — the canonical pack-range claim: per-record `BLAKE3` digests
//!   in offset-canonical order, covering a range gap-free.
//! * [`fsck`] — the integrity rules, each with a name.
//!
//! # The immutable/mutable line
//!
//! A manifest node carries object kind, object hash, decoded size, trie
//! structure, and subtree summaries — nothing else. Pack id, storage key,
//! offset, encoded length, ETag, audience, and current head are **mutable**
//! control-plane facts. They live in [`extent`], resolved after authorization,
//! so a repack changes a read envelope and never a manifest hash.
//!
//! # Compatibility
//!
//! The node layout, the `WPMF` magic, the `weft-plan-manifest-key-v1` routing
//! domain, and the offset-canonical extent ordering are byte-identical to the
//! already-merged downstream consumer (weft PR #1069 and its follow-up fix
//! #1070, `weft/docs/PLAN_MANIFEST_FORMAT.md`). This module is the normative
//! upstream *definition* of bytes that already exist downstream, not a second
//! competing format — see the crate-level note in [`node`] on why the magic
//! was kept rather than renamed.
//!
//! Facet identity follows the ratified weft #358 decision: four uniform facets
//! per spool, no content-bearing discriminant.

pub mod binding;
pub mod build;
pub mod extent;
pub mod fsck;
pub mod node;

pub use binding::{
    MANIFEST_BINDING_MAGIC, MANIFEST_BINDING_VERSION, ManifestBinding, ManifestBindingDecodeError,
    ManifestFacet, ManifestFacetParseError, ManifestOwnerKind,
};
pub use build::{
    BuiltManifest, ManifestBuildError, ManifestExpandError, ManifestNodeSource, ManifestNodeStore,
    build_manifest, expand_manifest,
};
pub use extent::{
    PACK_CLAIM_MAGIC, PACK_CLAIM_VERSION, PackClaimDecodeError, PackRangeClaim, PackRecord,
};
pub use fsck::{
    FsckFinding, FsckOptions, FsckReport, FsckRule, ManifestObjectIndex, PackRangeAudit,
    fsck_manifest, fsck_manifest_store, fsck_manifest_with, fsck_pack_range,
};
pub use node::{
    MANIFEST_BRANCH_WIDTH, MANIFEST_FORMAT_VERSION, MANIFEST_LEAF_MAX_ENTRIES, MANIFEST_NODE_MAGIC,
    MANIFEST_ROUTE_BITS, MANIFEST_ROUTE_DOMAIN, MANIFEST_ROUTE_LEVELS, ManifestBranch,
    ManifestChild, ManifestDecodeError, ManifestKey, ManifestLeaf, ManifestNode, ManifestNodeError,
    ManifestObject, ManifestObjectKind, ManifestRoute,
};
