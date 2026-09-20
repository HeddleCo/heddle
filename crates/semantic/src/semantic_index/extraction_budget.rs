//! Structural limits checked before allocating Heddle-owned semantic facts.
//!
//! This bounds extraction expansion; it is not a tree-sitter allocator quota.
use crate::parser::ParseBudget;

#[derive(Debug, thiserror::Error)]
pub enum ExtractionBudgetError {
    #[error("semantic analysis interrupted")]
    Interrupted,
    #[error("semantic analysis extraction budget exceeded: {0}")]
    Exceeded(&'static str),
}

pub(super) const MAX_SOURCE_BYTES: usize = 1 << 20;
const MAX_NODES: usize = 32_768;
const MAX_DEPTH: usize = 128;
const MAX_SPAN_BYTES: usize = 16 << 20;
const MAX_ANCESTOR_BYTES: usize = 16 << 20;

/// Walk through the existing tree with a cursor, before any definition,
/// token-stream, syntax-index or semantic-fact vectors are built. Node counts
/// bound vector cardinality; summed spans bound repeated subtree processing;
/// ancestor names separately bound container-name duplication.
pub(super) fn admit(
    root: tree_sitter::Node<'_>,
    budget: &ParseBudget,
) -> Result<(), ExtractionBudgetError> {
    let mut cursor = root.walk();
    let mut depth = 0usize;
    let mut nodes = 0usize;
    let mut span_bytes = 0usize;
    let mut ancestor_bytes = 0usize;
    let mut parent_name_bytes = 0usize;
    loop {
        if budget.interrupted() {
            return Err(ExtractionBudgetError::Interrupted);
        }
        let node = cursor.node();
        nodes = nodes.saturating_add(1);
        span_bytes = span_bytes.saturating_add(node.byte_range().len());
        ancestor_bytes = ancestor_bytes.saturating_add(parent_name_bytes);
        if nodes > MAX_NODES {
            return Err(ExtractionBudgetError::Exceeded("AST node count"));
        }
        if depth > MAX_DEPTH {
            return Err(ExtractionBudgetError::Exceeded("AST depth"));
        }
        if span_bytes > MAX_SPAN_BYTES {
            return Err(ExtractionBudgetError::Exceeded("repeated subtree spans"));
        }
        if ancestor_bytes > MAX_ANCESTOR_BYTES {
            return Err(ExtractionBudgetError::Exceeded("ancestor names"));
        }
        if cursor.goto_first_child() {
            parent_name_bytes = parent_name_bytes.saturating_add(name_bytes(node));
            depth += 1;
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return Ok(());
            }
            parent_name_bytes = parent_name_bytes.saturating_sub(name_bytes(cursor.node()));
            depth -= 1;
        }
    }
}

fn name_bytes(node: tree_sitter::Node<'_>) -> usize {
    // A type field covers Rust impl receivers and Go receiver/type spellings;
    // charging both fields is conservative when their syntax overlaps.
    ["name", "type", "receiver"]
        .into_iter()
        .filter_map(|field| node.child_by_field_name(field))
        .fold(0usize, |total, child| {
            total.saturating_add(child.byte_range().len())
        })
}
