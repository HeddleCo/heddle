// SPDX-License-Identifier: Apache-2.0
//! The single source of truth for "which tree-sitter nodes are comments" and
//! the non-comment leaf walk built on it.
//!
//! Both change-classification (`strip_comments`) and the semantic-index token
//! stream (`hd-sem-sym-v1`) need to walk an AST in document order while
//! skipping comment subtrees. Keeping the predicate and the walk here — in the
//! parser crate — means there is exactly one definition of "comment" and one
//! traversal shape; the consumers only differ in what they do with each leaf.

use tree_sitter::Node;

/// Whether a tree-sitter node kind names a comment. Comment subtrees are
/// skipped wholesale by [`walk_non_comment_leaves`], so doc-comments and
/// ordinary comments alike are excluded from semantic fingerprints.
pub fn is_comment_node(kind: &str) -> bool {
    matches!(
        kind,
        "comment" | "line_comment" | "block_comment" | "doc_comment" | "string_comment"
    )
}

/// DFS in document order over `node`'s subtree, invoking `on_leaf` for each
/// non-comment leaf (a node with no children). Comment nodes — and their
/// entire subtrees — are skipped.
///
/// The stack pushes children in reverse so they pop in source order, giving a
/// stable pre-order document-order traversal. Iterative (not recursive) so
/// deeply-nested-but-valid input drives heap, not call depth.
pub fn walk_non_comment_leaves<'tree>(node: Node<'tree>, mut on_leaf: impl FnMut(Node<'tree>)) {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if is_comment_node(current.kind()) {
            continue;
        }
        if current.child_count() == 0 {
            on_leaf(current);
            continue;
        }
        let child_count = current.child_count();
        for index in (0..child_count).rev() {
            if let Some(child) = current.child(index as u32) {
                stack.push(child);
            }
        }
    }
}
