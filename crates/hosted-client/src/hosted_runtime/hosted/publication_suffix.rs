// SPDX-License-Identifier: Apache-2.0
//! Choose the publication suffix after an installed tip and split it into
//! per-request pages. A page is the operations one `PublishContent` admits.
#![allow(dead_code)]

use std::collections::BTreeSet;

use objects::object::ContentHash;

/// Operations in one `ReplicationOperations` batch. The server rejects more.
pub(super) const PUBLICATION_PAGE: usize = 128;
/// Encoded `ReplicationOperations` body budget. One frame over this is rejected.
pub(super) const PUBLICATION_BATCH_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SuffixNode {
    pub id: ContentHash,
    pub parents: BTreeSet<ContentHash>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum SuffixPlan {
    /// The selected tip is already the installed frontier.
    Current,
    /// Oldest-first pages. Each page is one publication request.
    Pages(Vec<Vec<ContentHash>>),
}

pub(super) fn plan_suffix(
    _selected: ContentHash,
    _nodes: &[SuffixNode],
    _frontier: &BTreeSet<ContentHash>,
    _page_operations: usize,
) -> Result<SuffixPlan, &'static str> {
    Err("not implemented")
}

pub(super) fn fitting_prefix(
    _count_limit: usize,
    _byte_limit: usize,
    _sizes: &[usize],
) -> Result<usize, &'static str> {
    Err("not implemented")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(index: usize) -> ContentHash {
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&u64::try_from(index).expect("id").to_le_bytes());
        ContentHash::from_bytes(bytes)
    }

    fn linear(len: usize) -> (ContentHash, Vec<SuffixNode>) {
        let mut nodes = Vec::with_capacity(len);
        let mut parent = None;
        for index in 0..len {
            let node_id = id(index + 1);
            let parents = parent.into_iter().collect();
            nodes.push(SuffixNode {
                id: node_id,
                parents,
            });
            parent = Some(node_id);
        }
        (parent.expect("chain"), nodes)
    }

    #[test]
    fn empty_frontier_pages_a_long_linear_suffix_without_dropping_ops() {
        let (selected, nodes) = linear(500);
        let plan = plan_suffix(selected, &nodes, &BTreeSet::new(), PUBLICATION_PAGE)
            .expect("linear suffix");
        let SuffixPlan::Pages(pages) = plan else {
            panic!("500 new operations are a publication");
        };
        assert_eq!(pages.len(), 4);
        assert_eq!(pages[0].len(), 128);
        assert_eq!(pages[1].len(), 128);
        assert_eq!(pages[2].len(), 128);
        assert_eq!(pages[3].len(), 116);
        let flat = pages.into_iter().flatten().collect::<Vec<_>>();
        assert_eq!(flat.len(), 500);
        assert_eq!(flat.iter().collect::<BTreeSet<_>>().len(), 500);
        assert_eq!(flat[0], nodes[0].id);
        assert_eq!(*flat.last().expect("tip"), selected);
        for window in flat.windows(2) {
            let child = nodes
                .iter()
                .find(|node| node.id == window[1])
                .expect("child");
            assert!(child.parents.contains(&window[0]));
        }
    }

    #[test]
    fn suffix_starts_after_the_installed_tip() {
        let (selected, nodes) = linear(10);
        let installed = nodes[3].id;
        let plan = plan_suffix(
            selected,
            &nodes[4..],
            &BTreeSet::from([installed]),
            PUBLICATION_PAGE,
        )
        .expect("suffix");
        let SuffixPlan::Pages(pages) = plan else {
            panic!("six operations remain");
        };
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].len(), 6);
        assert_eq!(pages[0][0], nodes[4].id);
        assert!(nodes[4].parents.contains(&installed));
        assert!(!pages[0].contains(&installed));
    }

    #[test]
    fn selected_tip_already_installed_is_not_a_push() {
        let (selected, nodes) = linear(3);
        let plan = plan_suffix(
            selected,
            &nodes,
            &BTreeSet::from([selected]),
            PUBLICATION_PAGE,
        )
        .expect("current");
        assert_eq!(plan, SuffixPlan::Current);
    }

    #[test]
    fn two_sibling_roots_are_rejected() {
        let root_a = SuffixNode {
            id: id(1),
            parents: BTreeSet::new(),
        };
        let root_b = SuffixNode {
            id: id(2),
            parents: BTreeSet::new(),
        };
        let error = plan_suffix(id(2), &[root_a, root_b], &BTreeSet::new(), PUBLICATION_PAGE)
            .expect_err("siblings");
        assert_eq!(error, "publication suffix must extend one installed tip");
    }

    #[test]
    fn one_merge_root_whose_parents_are_the_frontier_is_one_page() {
        let left = id(1);
        let right = id(2);
        let merge = id(3);
        let node = SuffixNode {
            id: merge,
            parents: BTreeSet::from([left, right]),
        };
        let plan = plan_suffix(
            merge,
            &[node],
            &BTreeSet::from([left, right]),
            PUBLICATION_PAGE,
        )
        .expect("merge extends both heads");
        assert_eq!(plan, SuffixPlan::Pages(vec![vec![merge]]));
    }

    #[test]
    fn a_parent_outside_the_frontier_and_the_suffix_is_a_gap() {
        let missing = id(9);
        let child = SuffixNode {
            id: id(2),
            parents: BTreeSet::from([missing]),
        };
        let error = plan_suffix(id(2), &[child], &BTreeSet::new(), PUBLICATION_PAGE)
            .expect_err("gap");
        assert_eq!(
            error,
            "invalid source download: incomplete source ancestry"
        );
    }

    #[test]
    fn branched_suffix_over_one_page_is_not_split() {
        let (selected, mut nodes) = linear(PUBLICATION_PAGE + 2);
        let extra = SuffixNode {
            id: id(250),
            parents: BTreeSet::from([nodes[0].id]),
        };
        nodes.push(extra);
        let error = plan_suffix(selected, &nodes, &BTreeSet::new(), PUBLICATION_PAGE)
            .expect_err("branched page");
        assert_eq!(
            error,
            "branched publication suffix exceeds one publication request"
        );
    }

    #[test]
    fn byte_budget_ends_a_page_before_the_count_bound() {
        let sizes = vec![100_000, 100_000, 100_000];
        let count = fitting_prefix(PUBLICATION_PAGE, PUBLICATION_BATCH_BYTES, &sizes)
            .expect("fits");
        assert_eq!(count, 2);
        let oversized = vec![PUBLICATION_BATCH_BYTES + 1];
        assert_eq!(
            fitting_prefix(PUBLICATION_PAGE, PUBLICATION_BATCH_BYTES, &oversized),
            Err("source original metadata exceeds upload budget")
        );
    }

    #[test]
    fn tip_moved_renegotiation_drops_the_prefix_already_installed() {
        let (selected, nodes) = linear(8);
        let moved = nodes[1].id;
        let plan = plan_suffix(
            selected,
            &nodes[2..],
            &BTreeSet::from([moved]),
            3,
        )
        .expect("renegotiated suffix");
        let SuffixPlan::Pages(pages) = plan else {
            panic!("remainder is published");
        };
        assert_eq!(pages, vec![vec![nodes[2].id, nodes[3].id, nodes[4].id], vec![
            nodes[5].id, nodes[6].id, nodes[7].id
        ]]);
        assert!(pages.iter().flatten().all(|id| *id != moved));
    }
}
