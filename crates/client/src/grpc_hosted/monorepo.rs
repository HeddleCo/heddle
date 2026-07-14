// SPDX-License-Identifier: Apache-2.0
//! Monorepo-clone PLANNER (Spool epic P9, weft#358).
//!
//! Pure, transport-free translation of a resolved `MonorepoNode` tree (the
//! reply from `HostedGrpcClient::resolve_monorepo`) into a flat, ordered set
//! of per-spool clone operations plus the set of child edges that were NOT
//! descended into (EdgeSkip).
//!
//! The headline user feature — `heddle clone <hosted-spool> --recursive` — is
//! then just: run this planner over the resolved tree, and for each
//! [`MonorepoCloneOp`] reuse the existing per-spool hosted-clone path to
//! materialize that spool's content at `content_state` into `local_path`. The
//! CLI owns the transport; this module owns the *placement* — root at the
//! destination root, each child at its `mount_name` under its parent — so the
//! placement logic is unit-provable without a running weft.
//!
//! ## Placement rules
//! - The root node is cloned at the destination root (relative path `""`).
//! - Each descended edge is cloned at `<parent_rel>/<mount_name>`, at the
//!   edge's `anchored_state_id` (moving-anchored-ff resolved server-side).
//! - A node with no `content_state` (no content head yet) still yields an op
//!   with `content_state: None` so the caller can create an empty checkout at
//!   the mount point; its own edges are still walked.
//! - An edge with a `skipped` reason is recorded in [`MonorepoClonePlan::skipped`]
//!   and NOT descended into — unreadable / cycle / depth-bounded children are
//!   reported, never fatal.

use std::path::{Path, PathBuf};

use grpc::heddle::api::v1alpha1::{EdgeSkip, MonorepoNode};
use objects::object::StateId;

/// A single per-spool clone operation the planner emits. The CLI reuses the
/// existing hosted-clone path to satisfy each of these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonorepoCloneOp {
    /// The spool id/path to clone (the `MonorepoNode.spool_id`).
    pub spool_id: String,
    /// The content-facet state to materialize this spool at. `None` when the
    /// spool has no content head yet (empty checkout).
    pub content_state: Option<StateId>,
    /// Destination path RELATIVE to the clone root. The root spool is `""`
    /// (the destination root itself); each child mounts at
    /// `<parent>/<mount_name>`.
    pub rel_path: PathBuf,
}

impl MonorepoCloneOp {
    /// Absolute destination for this op given the clone root.
    pub fn dest_path(&self, clone_root: &Path) -> PathBuf {
        if self.rel_path.as_os_str().is_empty() {
            clone_root.to_path_buf()
        } else {
            clone_root.join(&self.rel_path)
        }
    }
}

/// A child edge the planner did not descend into, with the reason. Reported to
/// the user; never fatal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedChild {
    /// The child spool id the withheld edge points at.
    pub child_spool_id: String,
    /// The mount name of the withheld edge under its parent.
    pub mount_name: String,
    /// Destination path the child WOULD have mounted at (relative to root).
    pub rel_path: PathBuf,
    /// Why the edge was not descended.
    pub reason: EdgeSkip,
}

impl SkippedChild {
    /// Human-facing one-line reason token.
    pub fn reason_label(&self) -> &'static str {
        match self.reason {
            EdgeSkip::Unspecified => "unspecified",
            EdgeSkip::Unreadable => "unreadable",
            EdgeSkip::Cycle => "cycle",
            EdgeSkip::DepthBounded => "depth-bounded",
        }
    }
}

/// The full plan derived from a resolved `MonorepoNode`: the ordered clone
/// operations (root first, then each descended descendant in pre-order) and
/// the withheld child edges.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MonorepoClonePlan {
    /// Per-spool clone ops in pre-order (root first). Placement is coherent:
    /// a parent's op always precedes its children's.
    pub ops: Vec<MonorepoCloneOp>,
    /// Child edges recorded but not descended (unreadable / cycle / depth).
    pub skipped: Vec<SkippedChild>,
}

impl MonorepoClonePlan {
    /// Build the plan from a resolved root `MonorepoNode`. Pure — no I/O.
    pub fn from_resolved(root: &MonorepoNode) -> Self {
        let mut plan = MonorepoClonePlan::default();
        plan.walk(root, PathBuf::new());
        plan
    }

    fn walk(&mut self, node: &MonorepoNode, rel_path: PathBuf) {
        // Emit this node's clone op. An absent/malformed content_state maps to
        // `None` (empty checkout) rather than being dropped — the mount point
        // must still exist for the monorepo layout to be coherent.
        let content_state = node
            .content_state
            .as_deref()
            .and_then(|bytes| StateId::try_from_slice(bytes).ok());
        self.ops.push(MonorepoCloneOp {
            spool_id: node.spool_id.clone(),
            content_state,
            rel_path: rel_path.clone(),
        });

        for edge in &node.edges {
            let child_rel = rel_path.join(&edge.mount_name);
            match (&edge.subtree, edge.skipped) {
                // Descended: recurse into the resolved subtree at its mount.
                (Some(subtree), _) => self.walk(subtree, child_rel),
                // Withheld: record the reason, do not descend. A missing
                // subtree with no explicit reason is treated as unspecified
                // rather than silently vanishing.
                (None, skipped) => {
                    let reason = skipped
                        .and_then(|s| EdgeSkip::try_from(s).ok())
                        .unwrap_or(EdgeSkip::Unspecified);
                    self.skipped.push(SkippedChild {
                        child_spool_id: edge.child_spool_id.clone(),
                        mount_name: edge.mount_name.clone(),
                        rel_path: child_rel,
                        reason,
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use grpc::heddle::api::v1alpha1::{ChildEdgeStatus, MonorepoEdge, StateId as ProtoStateId};

    use super::*;

    fn cid(seed: u8) -> StateId {
        StateId::from_bytes([seed; 32])
    }

    fn cid_bytes(seed: u8) -> Vec<u8> {
        cid(seed).as_bytes().to_vec()
    }

    fn proto_cid(seed: u8) -> Option<ProtoStateId> {
        Some(ProtoStateId {
            value: cid_bytes(seed),
        })
    }

    /// Build a leaf node (content head, no children).
    fn leaf(spool_id: &str, content: u8) -> MonorepoNode {
        MonorepoNode {
            spool_id: spool_id.to_string(),
            content_state: Some(cid_bytes(content)),
            edges: vec![],
        }
    }

    /// A descended edge onto `subtree`, anchored at `anchor`.
    fn descended_edge(
        mount: &str,
        child_id: &str,
        anchor: u8,
        subtree: MonorepoNode,
    ) -> MonorepoEdge {
        MonorepoEdge {
            mount_name: mount.to_string(),
            child_spool_id: child_id.to_string(),
            anchored_state_id: proto_cid(anchor),
            child_head: Some(cid_bytes(anchor)),
            status: ChildEdgeStatus::UpToDate as i32,
            subtree: Some(subtree),
            skipped: None,
        }
    }

    /// A withheld edge (no subtree) with an EdgeSkip reason.
    fn skipped_edge(mount: &str, child_id: &str, anchor: u8, reason: EdgeSkip) -> MonorepoEdge {
        MonorepoEdge {
            mount_name: mount.to_string(),
            child_spool_id: child_id.to_string(),
            anchored_state_id: proto_cid(anchor),
            child_head: None,
            status: ChildEdgeStatus::Unspecified as i32,
            subtree: None,
            skipped: Some(reason as i32),
        }
    }

    /// The brief's fixture: root + 2 children + a grandchild, one child skipped.
    ///
    /// root (content c1)
    ///  ├─ libs/  -> child-a (content c2)
    ///  │            └─ vendor/ -> grandchild (content c3)
    ///  └─ secret/ -> child-b  [SKIPPED: unreadable]
    fn fixture_tree() -> MonorepoNode {
        let grandchild = leaf("acme/grandchild", 3);
        let child_a = MonorepoNode {
            spool_id: "acme/child-a".to_string(),
            content_state: Some(cid_bytes(2)),
            edges: vec![descended_edge("vendor", "acme/grandchild", 3, grandchild)],
        };
        MonorepoNode {
            spool_id: "acme/root".to_string(),
            content_state: Some(cid_bytes(1)),
            edges: vec![
                descended_edge("libs", "acme/child-a", 2, child_a),
                skipped_edge("secret", "acme/child-b", 9, EdgeSkip::Unreadable),
            ],
        }
    }

    #[test]
    fn planner_places_each_spool_at_its_mount_path_and_anchored_state() {
        let plan = MonorepoClonePlan::from_resolved(&fixture_tree());

        // Three descended ops in pre-order: root, child-a, grandchild.
        assert_eq!(plan.ops.len(), 3, "root + child-a + grandchild");

        // Root: destination root, at its own content head.
        assert_eq!(plan.ops[0].spool_id, "acme/root");
        assert_eq!(plan.ops[0].rel_path, PathBuf::new());
        assert_eq!(plan.ops[0].content_state, Some(cid(1)));

        // child-a: mounted at `libs`, materialized at the edge's ANCHORED
        // state (cid 2), which here equals its content head.
        assert_eq!(plan.ops[1].spool_id, "acme/child-a");
        assert_eq!(plan.ops[1].rel_path, PathBuf::from("libs"));
        assert_eq!(plan.ops[1].content_state, Some(cid(2)));

        // grandchild: nested under child-a's mount -> `libs/vendor`.
        assert_eq!(plan.ops[2].spool_id, "acme/grandchild");
        assert_eq!(plan.ops[2].rel_path, PathBuf::from("libs").join("vendor"));
        assert_eq!(plan.ops[2].content_state, Some(cid(3)));
    }

    #[test]
    fn planner_reports_the_skipped_child_and_does_not_clone_it() {
        let plan = MonorepoClonePlan::from_resolved(&fixture_tree());

        assert_eq!(plan.skipped.len(), 1, "exactly one withheld edge");
        let sk = &plan.skipped[0];
        assert_eq!(sk.child_spool_id, "acme/child-b");
        assert_eq!(sk.mount_name, "secret");
        assert_eq!(sk.rel_path, PathBuf::from("secret"));
        assert_eq!(sk.reason, EdgeSkip::Unreadable);
        assert_eq!(sk.reason_label(), "unreadable");

        // The skipped child never appears as a clone op.
        assert!(
            plan.ops.iter().all(|op| op.spool_id != "acme/child-b"),
            "skipped child must not be cloned"
        );
    }

    #[test]
    fn dest_path_joins_root_for_children_and_returns_root_itself_for_the_root_op() {
        let plan = MonorepoClonePlan::from_resolved(&fixture_tree());
        let root = Path::new("/tmp/mono");

        assert_eq!(plan.ops[0].dest_path(root), PathBuf::from("/tmp/mono"));
        assert_eq!(plan.ops[1].dest_path(root), PathBuf::from("/tmp/mono/libs"));
        assert_eq!(
            plan.ops[2].dest_path(root),
            PathBuf::from("/tmp/mono/libs/vendor")
        );
    }

    #[test]
    fn node_without_content_head_yields_an_op_with_no_state_but_still_walks_its_edges() {
        // A parent with no content head still needs its mount point + its
        // children materialized.
        let child = leaf("acme/child", 5);
        let root = MonorepoNode {
            spool_id: "acme/root".to_string(),
            content_state: None,
            edges: vec![descended_edge("sub", "acme/child", 5, child)],
        };
        let plan = MonorepoClonePlan::from_resolved(&root);

        assert_eq!(plan.ops.len(), 2);
        assert_eq!(plan.ops[0].spool_id, "acme/root");
        assert_eq!(plan.ops[0].content_state, None);
        assert_eq!(plan.ops[1].spool_id, "acme/child");
        assert_eq!(plan.ops[1].rel_path, PathBuf::from("sub"));
        assert_eq!(plan.ops[1].content_state, Some(cid(5)));
    }

    #[test]
    fn every_edge_skip_variant_maps_to_a_stable_label() {
        for (reason, label) in [
            (EdgeSkip::Unreadable, "unreadable"),
            (EdgeSkip::Cycle, "cycle"),
            (EdgeSkip::DepthBounded, "depth-bounded"),
        ] {
            let root = MonorepoNode {
                spool_id: "root".to_string(),
                content_state: Some(cid_bytes(1)),
                edges: vec![skipped_edge("m", "child", 2, reason)],
            };
            let plan = MonorepoClonePlan::from_resolved(&root);
            assert_eq!(plan.skipped.len(), 1);
            assert_eq!(plan.skipped[0].reason, reason);
            assert_eq!(plan.skipped[0].reason_label(), label);
        }
    }
}
