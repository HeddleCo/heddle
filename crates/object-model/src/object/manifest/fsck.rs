// SPDX-License-Identifier: Apache-2.0
//! fsck rules for a manifest/object graph.
//!
//! Every rule has a name. A corrupt graph is not merely "invalid" — fsck says
//! *which* invariant broke and at which node, because the operator response
//! differs: a digest mismatch means the bytes are wrong, a dangling ref means
//! publication tore, and an extent gap means a grant would authorize bytes
//! nobody selected.
//!
//! The rules divide into four families:
//!
//! 1. **Well-formed** — every reachable node decodes canonically
//!    ([`FsckRule::MalformedNode`], [`FsckRule::NonCanonicalNodeEncoding`],
//!    [`FsckRule::LeafEntriesOutOfOrder`], …).
//! 2. **Digests match** — node bytes hash to the address they were fetched by,
//!    subtree summaries equal what the subtree actually holds, and every
//!    encoded pack record hashes to its declared digest.
//! 3. **No dangling refs** — every branch child resolves, every leaf object is
//!    present in the object index, and no grant names an object the manifest
//!    does not cover.
//! 4. **Gap-free coverage** — a pack range's records partition `[start, end)`
//!    exactly, in offset-canonical order, with no gap and no overlap.
//!
//! fsck is *checking*, not repair, and it is total: it collects every finding
//! rather than bailing at the first, so one run tells an operator the whole
//! story. Traversal is still bounded — visited nodes are not re-entered and
//! depth cannot exceed the fixed route — so an adversarial node set cannot
//! make it spin.

use std::collections::{BTreeMap, BTreeSet};

use super::{
    build::{ManifestNodeSource, ManifestNodeStore, build_manifest},
    extent::PackRangeClaim,
    node::{
        MANIFEST_LEAF_MAX_ENTRIES, MANIFEST_ROUTE_LEVELS, ManifestDecodeError, ManifestKey,
        ManifestNode, ManifestObject,
    },
};
use crate::object::ContentHash;

// ── Rules ───────────────────────────────────────────────────────────

/// The named integrity rules fsck enforces.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FsckRule {
    // Well-formed.
    /// Node bytes failed to decode at all (bad magic, unknown version or tag,
    /// unknown object kind, truncation).
    MalformedNode,
    /// Node bytes decoded but are not their own canonical spelling.
    NonCanonicalNodeEncoding,
    /// Node bytes carry data past the declared content.
    TrailingBytes,
    /// Leaf entries are not strictly ascending by `(kind, hash)`.
    LeafEntriesOutOfOrder,
    /// A leaf names one object key twice.
    DuplicateObjectKey,
    /// A branch declares an empty bitmap; the canonical empty set is a leaf.
    EmptyBranchBitmap,

    // Digests match.
    /// Node bytes do not hash to the address they were fetched by.
    NodeDigestMismatch,
    /// A branch's `(object_count, decoded_bytes)` summary disagrees with its
    /// actual subtree.
    SubtreeSummaryMismatch,
    /// A leaf's declared `decoded_size` disagrees with the object index.
    ObjectSizeMismatch,
    /// An encoded pack record does not hash to its declared digest.
    ExtentDigestMismatch,

    // No dangling refs.
    /// A branch names a child that is absent from the node source.
    DanglingNodeRef,
    /// A leaf names an object that is absent from the object index.
    DanglingObjectRef,
    /// A pack claim authorizes bytes for an object the manifest does not cover.
    ExtentObjectNotInManifest,
    /// A node is present in the store but unreachable from the root.
    UnreachableNode,

    // Structural canonicity.
    /// A leaf holds more than the bound while routing bits remain to split on.
    LeafOverfull,
    /// A branch's whole subtree would fit in one leaf; it must not exist.
    UnderfullBranch,
    /// A non-root leaf holds no entries.
    EmptyNonRootLeaf,
    /// A branch's declared depth disagrees with its position in the trie.
    BranchDepthMismatch,
    /// An entry sits at a position its route does not lead to.
    MisroutedEntry,
    /// Rebuilding the trie from the expanded object set yields a different
    /// root. The backstop for any structural deviation the local rules miss.
    NonCanonicalTrieShape,
    /// Traversal exceeded the fixed route depth.
    DepthExceeded,

    // Gap-free coverage.
    /// Pack records are not strictly ascending by offset.
    ExtentsOutOfOffsetOrder,
    /// Two pack records claim overlapping bytes.
    ExtentOverlap,
    /// Consecutive pack records leave an unclaimed byte gap.
    ExtentGap,
    /// The record partition does not cover `[start, end)` exactly.
    RangeCoverageMismatch,
    /// A pack record claims zero bytes.
    ZeroLengthExtent,
}

impl FsckRule {
    /// The stable rule name, for logs, metrics, and test assertions.
    pub fn name(self) -> &'static str {
        match self {
            Self::MalformedNode => "malformed-node",
            Self::NonCanonicalNodeEncoding => "non-canonical-node-encoding",
            Self::TrailingBytes => "trailing-bytes",
            Self::LeafEntriesOutOfOrder => "leaf-entries-out-of-order",
            Self::DuplicateObjectKey => "duplicate-object-key",
            Self::EmptyBranchBitmap => "empty-branch-bitmap",
            Self::NodeDigestMismatch => "node-digest-mismatch",
            Self::SubtreeSummaryMismatch => "subtree-summary-mismatch",
            Self::ObjectSizeMismatch => "object-size-mismatch",
            Self::ExtentDigestMismatch => "extent-digest-mismatch",
            Self::DanglingNodeRef => "dangling-node-ref",
            Self::DanglingObjectRef => "dangling-object-ref",
            Self::ExtentObjectNotInManifest => "extent-object-not-in-manifest",
            Self::UnreachableNode => "unreachable-node",
            Self::LeafOverfull => "leaf-overfull",
            Self::UnderfullBranch => "underfull-branch",
            Self::EmptyNonRootLeaf => "empty-non-root-leaf",
            Self::BranchDepthMismatch => "branch-depth-mismatch",
            Self::MisroutedEntry => "misrouted-entry",
            Self::NonCanonicalTrieShape => "non-canonical-trie-shape",
            Self::DepthExceeded => "depth-exceeded",
            Self::ExtentsOutOfOffsetOrder => "extents-out-of-offset-order",
            Self::ExtentOverlap => "extent-overlap",
            Self::ExtentGap => "extent-gap",
            Self::RangeCoverageMismatch => "range-coverage-mismatch",
            Self::ZeroLengthExtent => "zero-length-extent",
        }
    }
}

impl std::fmt::Display for FsckRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

impl ManifestDecodeError {
    /// Map a decode failure onto the fsck rule it violates, so a rejection is
    /// reported by name rather than as an opaque parse error.
    pub fn fsck_rule(&self) -> FsckRule {
        match self {
            Self::BadMagic
            | Self::UnsupportedVersion(_)
            | Self::UnknownNodeTag(_)
            | Self::UnknownObjectKind(_)
            | Self::Truncated => FsckRule::MalformedNode,
            Self::TrailingBytes => FsckRule::TrailingBytes,
            Self::EntriesOutOfOrder => FsckRule::LeafEntriesOutOfOrder,
            Self::DuplicateObjectKey(_) => FsckRule::DuplicateObjectKey,
            Self::EmptyBranchBitmap => FsckRule::EmptyBranchBitmap,
            Self::NonCanonicalEncoding => FsckRule::NonCanonicalNodeEncoding,
            Self::AddressMismatch { .. } => FsckRule::NodeDigestMismatch,
        }
    }
}

// ── Findings ────────────────────────────────────────────────────────

/// One violation: the rule, where it was found, and a human-readable detail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsckFinding {
    pub rule: FsckRule,
    /// The manifest node the finding attaches to, when there is one.
    pub node: Option<ContentHash>,
    pub detail: String,
}

impl FsckFinding {
    fn new(rule: FsckRule, node: Option<ContentHash>, detail: impl Into<String>) -> Self {
        Self {
            rule,
            node,
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for FsckFinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.node {
            Some(node) => write!(f, "{}: {} ({})", self.rule, self.detail, node.short()),
            None => write!(f, "{}: {}", self.rule, self.detail),
        }
    }
}

/// Every finding from one fsck run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FsckReport {
    findings: Vec<FsckFinding>,
}

impl FsckReport {
    pub fn findings(&self) -> &[FsckFinding] {
        &self.findings
    }

    /// True when nothing was found. A clean report is the *only* basis for
    /// treating a root as usable.
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }

    /// The distinct rules violated, in rule order.
    pub fn violated_rules(&self) -> BTreeSet<FsckRule> {
        self.findings.iter().map(|finding| finding.rule).collect()
    }

    pub fn has_rule(&self, rule: FsckRule) -> bool {
        self.findings.iter().any(|finding| finding.rule == rule)
    }

    fn push(&mut self, finding: FsckFinding) {
        self.findings.push(finding);
    }
}

// ── Object index ────────────────────────────────────────────────────

/// The set of content objects a manifest may legally name, with their decoded
/// sizes.
///
/// Supplying an index turns on the dangling-object and size rules. Omitting it
/// checks manifest structure alone, which is what a receiver that has the
/// manifest but not yet the bodies can do.
pub trait ManifestObjectIndex {
    /// Decoded size of `key`, or `None` if the object is absent.
    fn decoded_size(&self, key: &ManifestKey) -> Option<u64>;
}

impl ManifestObjectIndex for BTreeMap<ManifestKey, u64> {
    fn decoded_size(&self, key: &ManifestKey) -> Option<u64> {
        self.get(key).copied()
    }
}

impl ManifestObjectIndex for std::collections::HashMap<ManifestKey, u64> {
    fn decoded_size(&self, key: &ManifestKey) -> Option<u64> {
        self.get(key).copied()
    }
}

/// What fsck should check beyond node structure.
#[derive(Default)]
pub struct FsckOptions<'a> {
    /// Objects the manifest may name. Enables [`FsckRule::DanglingObjectRef`]
    /// and [`FsckRule::ObjectSizeMismatch`].
    pub objects: Option<&'a dyn ManifestObjectIndex>,
    /// Report nodes present in the store but unreachable from the root.
    /// Off by default: a shared node store legitimately holds other roots'
    /// nodes, so only a whole-store sweep should turn this on.
    pub report_unreachable: bool,
}

// ── Manifest fsck ───────────────────────────────────────────────────

/// Check a manifest graph rooted at `root` for structure and digests only.
pub fn fsck_manifest<S: ManifestNodeSource + ?Sized>(source: &S, root: &ContentHash) -> FsckReport {
    fsck_manifest_with(source, root, &FsckOptions::default())
}

/// Check a manifest graph rooted at `root`, with optional object and
/// reachability checks.
pub fn fsck_manifest_with<S: ManifestNodeSource + ?Sized>(
    source: &S,
    root: &ContentHash,
    options: &FsckOptions<'_>,
) -> FsckReport {
    let mut report = FsckReport::default();
    let mut visited = BTreeSet::new();
    let mut objects = Vec::new();
    let mut expansion_complete = true;

    visit(
        source,
        root,
        0,
        true,
        options,
        &mut visited,
        &mut objects,
        &mut report,
        &mut expansion_complete,
    );

    // Backstop: rebuild the canonical trie from what we actually expanded and
    // compare roots. Local rules catch the diagnosable cases; this catches
    // anything they do not. Skipped when expansion was incomplete, since a
    // partial object set would rebuild to a different root for a reason
    // already reported.
    if expansion_complete
        && let Ok(rebuilt) = build_manifest(objects.iter().copied())
        && rebuilt.root != *root
    {
        report.push(FsckFinding::new(
            FsckRule::NonCanonicalTrieShape,
            Some(*root),
            format!(
                "rebuilding from {} expanded objects yields root {}",
                objects.len(),
                rebuilt.root
            ),
        ));
    }

    report
}

/// Check every root in `roots` against a whole node store, additionally
/// reporting nodes no root reaches.
pub fn fsck_manifest_store<S: ManifestNodeStore + ?Sized>(
    store: &S,
    roots: &[ContentHash],
    options: &FsckOptions<'_>,
) -> FsckReport {
    let mut report = FsckReport::default();
    let mut reachable = BTreeSet::new();

    for root in roots {
        let mut visited = BTreeSet::new();
        let mut objects = Vec::new();
        let mut expansion_complete = true;
        visit(
            store,
            root,
            0,
            true,
            options,
            &mut visited,
            &mut objects,
            &mut report,
            &mut expansion_complete,
        );
        if expansion_complete
            && let Ok(rebuilt) = build_manifest(objects.iter().copied())
            && rebuilt.root != *root
        {
            report.push(FsckFinding::new(
                FsckRule::NonCanonicalTrieShape,
                Some(*root),
                format!("rebuilt root {} differs", rebuilt.root),
            ));
        }
        reachable.extend(visited);
    }

    if options.report_unreachable {
        for hash in store.node_hashes() {
            if !reachable.contains(&hash) {
                report.push(FsckFinding::new(
                    FsckRule::UnreachableNode,
                    Some(hash),
                    "node is present but no supplied root reaches it",
                ));
            }
        }
    }

    report
}

/// Visit one node, returning its `(object_count, decoded_bytes)` subtree
/// summary when the subtree was readable enough to compute one.
///
/// `expansion_complete` is cleared only when a node could not be *read* —
/// missing, mis-addressed, malformed, or past the route depth. A node that
/// reads fine but violates a shape rule still expands completely, so the
/// rebuild backstop stays meaningful for it.
#[allow(clippy::too_many_arguments)]
fn visit<S: ManifestNodeSource + ?Sized>(
    source: &S,
    hash: &ContentHash,
    depth: u8,
    is_root: bool,
    options: &FsckOptions<'_>,
    visited: &mut BTreeSet<ContentHash>,
    objects: &mut Vec<ManifestObject>,
    report: &mut FsckReport,
    expansion_complete: &mut bool,
) -> Option<(u64, u64)> {
    if depth > MANIFEST_ROUTE_LEVELS {
        *expansion_complete = false;
        report.push(FsckFinding::new(
            FsckRule::DepthExceeded,
            Some(*hash),
            format!("traversal passed the fixed {MANIFEST_ROUTE_LEVELS}-level route"),
        ));
        return None;
    }
    if !visited.insert(*hash) {
        // Shared subtree, already accounted for. Content addressing makes a
        // true cycle infeasible; this keeps a corrupt store from spinning.
        return None;
    }

    let Some(bytes) = source.node_bytes(hash) else {
        *expansion_complete = false;
        report.push(FsckFinding::new(
            FsckRule::DanglingNodeRef,
            Some(*hash),
            "node is referenced but absent from the node source",
        ));
        return None;
    };

    let actual = ContentHash::compute(bytes);
    if actual != *hash {
        *expansion_complete = false;
        report.push(FsckFinding::new(
            FsckRule::NodeDigestMismatch,
            Some(*hash),
            format!("bytes hash to {actual}"),
        ));
        return None;
    }

    let node = match ManifestNode::decode(bytes) {
        Ok(node) => node,
        Err(error) => {
            *expansion_complete = false;
            report.push(FsckFinding::new(
                error.fsck_rule(),
                Some(*hash),
                error.to_string(),
            ));
            return None;
        }
    };

    match node {
        ManifestNode::Leaf(leaf) => {
            let entries = leaf.entries();
            if entries.is_empty() && !is_root {
                report.push(FsckFinding::new(
                    FsckRule::EmptyNonRootLeaf,
                    Some(*hash),
                    "only the root may be the empty leaf",
                ));
            }
            if entries.len() > MANIFEST_LEAF_MAX_ENTRIES && depth < MANIFEST_ROUTE_LEVELS {
                report.push(FsckFinding::new(
                    FsckRule::LeafOverfull,
                    Some(*hash),
                    format!(
                        "leaf at depth {depth} holds {} entries; the bound is {MANIFEST_LEAF_MAX_ENTRIES} while routing bits remain",
                        entries.len()
                    ),
                ));
            }

            let mut decoded_bytes = 0u64;
            for entry in entries {
                decoded_bytes = decoded_bytes.saturating_add(entry.decoded_size);
                if let Some(index) = options.objects {
                    match index.decoded_size(&entry.key()) {
                        None => {
                            report.push(FsckFinding::new(
                                FsckRule::DanglingObjectRef,
                                Some(*hash),
                                format!("{} object {} is not present", entry.kind, entry.hash),
                            ));
                        }
                        Some(size) if size != entry.decoded_size => {
                            report.push(FsckFinding::new(
                                FsckRule::ObjectSizeMismatch,
                                Some(*hash),
                                format!(
                                    "{} object {} declares {} bytes but holds {size}",
                                    entry.kind, entry.hash, entry.decoded_size
                                ),
                            ));
                        }
                        Some(_) => {}
                    }
                }
                objects.push(*entry);
            }
            Some((entries.len() as u64, decoded_bytes))
        }
        ManifestNode::Branch(branch) => {
            if branch.depth() != depth {
                report.push(FsckFinding::new(
                    FsckRule::BranchDepthMismatch,
                    Some(*hash),
                    format!("declares depth {} but sits at {depth}", branch.depth()),
                ));
            }

            let mut total_count = 0u64;
            let mut total_bytes = 0u64;
            let mut all_children_summarized = true;
            for child in branch.children() {
                let before = objects.len();
                let summary = visit(
                    source,
                    &child.hash,
                    depth + 1,
                    false,
                    options,
                    visited,
                    objects,
                    report,
                    expansion_complete,
                );

                // Everything newly expanded under this child must route through
                // this branch's slot at this depth. Applied at every level of
                // the descent, this checks the whole route, not just one group.
                for entry in &objects[before..] {
                    if entry.key().route().group(depth) != child.slot {
                        report.push(FsckFinding::new(
                            FsckRule::MisroutedEntry,
                            Some(child.hash),
                            format!(
                                "{} object {} routes to slot {} at depth {depth}, not {}",
                                entry.kind,
                                entry.hash,
                                entry.key().route().group(depth),
                                child.slot
                            ),
                        ));
                    }
                }

                match summary {
                    Some((count, bytes)) => {
                        if count != child.object_count || bytes != child.decoded_bytes {
                            report.push(FsckFinding::new(
                                FsckRule::SubtreeSummaryMismatch,
                                Some(*hash),
                                format!(
                                    "slot {} summarizes ({}, {}) but holds ({count}, {bytes})",
                                    child.slot, child.object_count, child.decoded_bytes
                                ),
                            ));
                        }
                        total_count += count;
                        total_bytes = total_bytes.saturating_add(bytes);
                    }
                    None => {
                        // Either a shared subtree (already counted) or a broken
                        // one (already reported). Either way this branch's own
                        // total is no longer checkable.
                        all_children_summarized = false;
                    }
                }
            }

            if all_children_summarized {
                if total_count <= MANIFEST_LEAF_MAX_ENTRIES as u64 && depth < MANIFEST_ROUTE_LEVELS
                {
                    report.push(FsckFinding::new(
                        FsckRule::UnderfullBranch,
                        Some(*hash),
                        format!(
                            "branch subtree holds {total_count} objects; it must be a single leaf"
                        ),
                    ));
                }
                Some((total_count, total_bytes))
            } else {
                None
            }
        }
    }
}

// ── Pack-range fsck ─────────────────────────────────────────────────

/// What to check a pack claim against, beyond its own structure.
#[derive(Default)]
pub struct PackRangeAudit<'a> {
    /// The raw bytes of `[start, end)` as fetched, enabling
    /// [`FsckRule::ExtentDigestMismatch`].
    pub range_bytes: Option<&'a [u8]>,
    /// Object keys the manifest authorizes, enabling
    /// [`FsckRule::ExtentObjectNotInManifest`].
    pub authorized: Option<&'a BTreeSet<ManifestKey>>,
}

/// Check that a coalesced pack range is offset-canonical, gap-free, and — when
/// the caller supplies them — digest-correct and manifest-covered.
///
/// This is the rule that keeps one physical range read from authorizing an
/// unselected byte gap in a mixed-audience pack.
pub fn fsck_pack_range(claim: &PackRangeClaim, audit: &PackRangeAudit<'_>) -> FsckReport {
    let mut report = FsckReport::default();

    if claim.end < claim.start {
        report.push(FsckFinding::new(
            FsckRule::RangeCoverageMismatch,
            None,
            format!("range end {} precedes start {}", claim.end, claim.start),
        ));
        return report;
    }

    let records = claim.records();
    if records.is_empty() {
        if claim.end != claim.start {
            report.push(FsckFinding::new(
                FsckRule::RangeCoverageMismatch,
                None,
                format!(
                    "range covers {} bytes but claims no records",
                    claim.end - claim.start
                ),
            ));
        }
        return report;
    }

    let mut cursor = claim.start;
    for (index, record) in records.iter().enumerate() {
        if record.length == 0 {
            report.push(FsckFinding::new(
                FsckRule::ZeroLengthExtent,
                None,
                format!("record {index} ({}) claims zero bytes", record.object.hash),
            ));
        }

        if index > 0 && record.offset < records[index - 1].offset {
            report.push(FsckFinding::new(
                FsckRule::ExtentsOutOfOffsetOrder,
                None,
                format!(
                    "record {index} at offset {} follows offset {}",
                    record.offset,
                    records[index - 1].offset
                ),
            ));
        }

        if record.offset > cursor {
            report.push(FsckFinding::new(
                FsckRule::ExtentGap,
                None,
                format!(
                    "unclaimed bytes [{cursor}, {}) before record {index}",
                    record.offset
                ),
            ));
        } else if record.offset < cursor {
            report.push(FsckFinding::new(
                FsckRule::ExtentOverlap,
                None,
                format!(
                    "record {index} starts at {} inside claimed bytes ending at {cursor}",
                    record.offset
                ),
            ));
        }

        let Some(end) = record.end() else {
            report.push(FsckFinding::new(
                FsckRule::RangeCoverageMismatch,
                None,
                format!("record {index} offset + length overflows u64"),
            ));
            return report;
        };
        cursor = cursor.max(end);

        if let (Some(bytes), Some(record_end)) = (audit.range_bytes, record.end())
            && record.offset >= claim.start
        {
            let from = (record.offset - claim.start) as usize;
            let to = record_end.saturating_sub(claim.start) as usize;
            match bytes.get(from..to) {
                Some(slice) => {
                    let digest = ContentHash::compute(slice);
                    if digest != record.encoded_digest {
                        report.push(FsckFinding::new(
                            FsckRule::ExtentDigestMismatch,
                            None,
                            format!(
                                "record {index} ({}) hashes to {digest}, not {}",
                                record.object.hash, record.encoded_digest
                            ),
                        ));
                    }
                }
                None => {
                    report.push(FsckFinding::new(
                        FsckRule::RangeCoverageMismatch,
                        None,
                        format!("record {index} extends past the supplied range bytes"),
                    ));
                }
            }
        }

        if let Some(authorized) = audit.authorized
            && !authorized.contains(&record.key())
        {
            report.push(FsckFinding::new(
                FsckRule::ExtentObjectNotInManifest,
                None,
                format!(
                    "record {index} authorizes {} object {}, which the manifest does not cover",
                    record.object.kind, record.object.hash
                ),
            ));
        }
    }

    if cursor != claim.end {
        report.push(FsckFinding::new(
            FsckRule::RangeCoverageMismatch,
            None,
            format!(
                "records cover through {cursor} but the range ends at {}",
                claim.end
            ),
        ));
    }

    if let Some(bytes) = audit.range_bytes
        && let Some(expected) = claim.byte_len()
        && bytes.len() as u64 != expected
    {
        report.push(FsckFinding::new(
            FsckRule::RangeCoverageMismatch,
            None,
            format!("supplied {} bytes for a {expected}-byte range", bytes.len()),
        ));
    }

    report
}
