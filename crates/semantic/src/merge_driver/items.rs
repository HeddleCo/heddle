// SPDX-License-Identifier: Apache-2.0
//! AST-defined item extraction.
//!
//! A file is decomposed into a sequence of *items* — top-level constructs that
//! the semantic merger treats as atomic merge units — interleaved with
//! *inter-item segments* (everything between items, including the
//! preamble and postamble).
//!
//! Items are scoped to the top level (file root) plus the immediate bodies of
//! `impl` blocks for Rust. Nested closures and inner functions are NOT
//! independently extracted: they participate in their enclosing item's merge.
//!
//! Per-language behaviour (classification, leading-metadata bindings,
//! signature hashing, scope extraction) lives in [`super::language_rules`].
//! This module owns the language-agnostic extraction loop and queries the
//! [`super::language_rules::LanguageRules`] trait for everything that varies
//! by language. The structural split is the heddle#133 audit refactor:
//! routes class-A (identity / key-collision) and class-B (sibling-ownership)
//! findings into one obvious place per language rather than a constellation
//! of per-language `match` arms.
//!
//! See HeddleCo/heddle#133 for the audit motivation.

use std::collections::HashMap;
use std::rc::Rc;

use tree_sitter::Node;

pub(super) use super::language_rules::{ItemKind, UseIdentity};
use super::language_rules::{rules_for, Classified, MetadataBinding};
use crate::parser::{Language, ParsedFile};

/// Stable identifier for an item across the three sides. Two items match iff
/// their `ItemKey`s are equal.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct ItemKey {
    pub kind: ItemKind,
    pub name: String,
    /// Path of enclosing items, outermost first. Empty for top-level items.
    /// Used to disambiguate methods of the same name in different `impl`
    /// blocks.
    pub scope: Vec<String>,
    /// Hash of the parameter-list spelling for function-like items. Zero
    /// for items without parameters (structs, consts, type aliases, etc.).
    /// Disambiguates overloads — same name, different arity/types — that
    /// would otherwise collide on (kind, name, scope) alone.
    pub signature_hash: u64,
}

/// A single extracted item with its byte range in the source.
#[derive(Clone, Debug)]
pub(crate) struct Item {
    pub key: ItemKey,
    pub start_byte: usize,
    pub end_byte: usize,
    /// For `use` / `pub use` items only: the expanded leaf set used for
    /// cross-side matching. `None` for every other item kind. Never used for
    /// byte emission, so the original grouped declaration text is preserved.
    /// Consumed by [`canonicalize_use_keys`] (leaf-set collision keying); the
    /// add/add resolution in [`super::reconstruct`] then dedups only on exact
    /// bytes and conflicts on every other difference.
    pub use_identity: Option<UseIdentity>,
}

/// The result of segmenting a file: items in source order, exposed via the
/// `items` accessor; the underlying source byte length stashed for
/// reconstruction.
#[derive(Clone, Debug)]
pub(crate) struct FileSegments {
    pub items: Vec<Item>,
    pub source_len: usize,
}

impl FileSegments {
    /// Slices of inter-item content. Length is `items.len() + 1`. The first
    /// slice is the preamble (before the first item); the last is the
    /// postamble (after the last item); middle slices sit between consecutive
    /// items.
    pub fn inter_item_ranges(&self) -> Vec<(usize, usize)> {
        let mut out = Vec::with_capacity(self.items.len() + 1);
        let mut cursor = 0usize;
        for item in &self.items {
            out.push((cursor, item.start_byte));
            cursor = item.end_byte;
        }
        out.push((cursor, self.source_len));
        out
    }
}

/// Extract items from a parsed file. Public for the `debug_items` hatch and
/// for `segment_file`.
pub(crate) fn extract_items(parsed: &ParsedFile) -> Vec<Item> {
    let mut items = Vec::new();
    let root = parsed.root_node();
    collect_items(parsed.language, &parsed.source, root, &[], &mut items);
    // Items can be reported in any DFS order; ensure source order for
    // deterministic reconstruction.
    items.sort_by_key(|item| item.start_byte);
    items
}

/// Top-level entry: segment a parsed file into items + record the source
/// length so reconstruction can recover inter-item content.
pub(crate) fn segment_file(parsed: &ParsedFile) -> FileSegments {
    FileSegments {
        items: extract_items(parsed),
        source_len: parsed.source.len(),
    }
}

/// Cap on AST traversal depth. Beyond this, nodes are not extracted as
/// items — they remain inter-item content and merge via the text-level
/// fallback. Picked well above realistic source nesting (deep generic
/// expressions in real code rarely cross ~50 levels) so the cap only
/// trips on pathological / synthetic input.
const MAX_TRAVERSAL_DEPTH: usize = 256;

fn collect_items(
    language: Language,
    source: &str,
    root: Node<'_>,
    base_scope: &[String],
    out: &mut Vec<Item>,
) {
    // Iterative DFS over the AST. Avoids the unbounded recursion shape
    // a deeply-parseable file could otherwise trigger — collect_items
    // used to recurse for every container body AND every unclassified
    // wrapper node, so a synthetic 50k-deep tree would blow the stack
    // even though tree-sitter itself parses it iteratively. Each stack
    // entry is (node-whose-children-to-walk, scope-at-that-node,
    // depth-from-root); the depth guard bails out beyond
    // `MAX_TRAVERSAL_DEPTH` rather than running unbounded work.
    let base_rc: Rc<Vec<String>> = Rc::new(base_scope.to_vec());
    let mut stack: Vec<(Node<'_>, Rc<Vec<String>>, usize)> = vec![(root, Rc::clone(&base_rc), 0)];

    while let Some((node, scope, depth)) = stack.pop() {
        if depth > MAX_TRAVERSAL_DEPTH {
            continue;
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if let Some(classified) = classify_node(language, source, child) {
                let Classified {
                    kind,
                    name,
                    container_body,
                    signature_hash,
                    extra_scope,
                } = classified;
                if let Some(body) = container_body {
                    let mut next_scope = (*scope).clone();
                    next_scope.push(name);
                    stack.push((body, Rc::new(next_scope), depth + 1));
                } else {
                    let mut item_scope = (*scope).clone();
                    item_scope.extend(extra_scope);
                    let use_identity = if matches!(kind, ItemKind::Use) {
                        super::language_rules::use_identity(language, source, child)
                    } else {
                        None
                    };
                    let item_key = ItemKey {
                        kind,
                        name,
                        scope: item_scope,
                        signature_hash,
                    };
                    let start_byte = leading_metadata_start(language, source, child);
                    out.push(Item {
                        key: item_key,
                        start_byte,
                        end_byte: child.end_byte(),
                        use_identity,
                    });
                }
            } else {
                stack.push((child, Rc::clone(&scope), depth + 1));
            }
        }
    }
}

/// Rekey every `use` item across the three sides so two declarations
/// collide for cross-side matching iff their expanded leaf sets intersect
/// on ANY import path — not just the lexicographically-smallest leaf.
///
/// Why this is necessary: items match across sides by exact [`ItemKey`]
/// equality, but leaf-set *intersection* is not transitive, so no
/// per-declaration single key can capture it (`a::{Bar, Baz}` overlaps
/// `a::Baz` but their minimum leaves differ → distinct keys → both emitted
/// → duplicate `Baz`, a Rust "defined multiple times" error — the
/// heddle#468 bug class, Codex r1's representative-key fix only caught
/// overlap on the minimum leaf). Equality-based matching CAN model
/// intersection if every declaration in one connected component (linked by
/// shared leaves) is rekeyed to one canonical name. That is exactly a
/// union-find over leaves: union all leaves within each declaration, then
/// rekey each declaration to its component's smallest leaf.
///
/// The result for the existing add/add resolution in
/// [`super::reconstruct`]:
/// * identical leaf sets → same canonical key, byte-identical → **dedup**
///   (the original grouped text is preserved — bytes are untouched);
/// * overlapping but not identical → same canonical key, divergent bytes →
///   **conflict** (the conservative resolution; we never silently rewrite
///   or combine import statements);
/// * disjoint leaf sets → distinct canonical keys → **union** (the r0
///   additive-re-export case stays clean).
///
/// Un-normalizable forms (globs, `as` aliases, nested groups) all carry the
/// single sentinel leaf, so they share one component and conflict-on-
/// divergence only with each other — never with a real import path.
///
/// This is a no-op for any `use` whose leaf set overlaps nothing: its
/// component is itself and its canonical name equals its own minimum leaf,
/// matching the pre-canonicalization seed key.
pub(crate) fn canonicalize_use_keys(
    base: &mut FileSegments,
    ours: &mut FileSegments,
    theirs: &mut FileSegments,
) {
    let mut uf = LeafUnionFind::default();
    for seg in [&*base, &*ours, &*theirs] {
        for item in &seg.items {
            let Some(identity) = &item.use_identity else {
                continue;
            };
            let mut leaf_iter = identity.leaves.iter();
            let Some(first) = leaf_iter.next() else {
                continue;
            };
            let anchor = uf.intern(first);
            for leaf in leaf_iter {
                let node = uf.intern(leaf);
                uf.union(anchor, node);
            }
        }
    }

    let canonical = uf.component_min_label();
    for seg in [base, ours, theirs] {
        for item in &mut seg.items {
            let Some(identity) = &item.use_identity else {
                continue;
            };
            if let Some(first) = identity.leaves.first()
                && let Some(name) = canonical.get(first)
            {
                item.key.name = name.clone();
            }
        }
    }
}

/// Union-find over leaf import-path strings. Leaves are interned to dense
/// indices on first sight; `union` links the components two leaves belong
/// to; [`component_min_label`] returns, for every interned leaf, the
/// lexicographically-smallest leaf in its component (the canonical name).
#[derive(Default)]
struct LeafUnionFind {
    index: HashMap<String, usize>,
    labels: Vec<String>,
    parent: Vec<usize>,
}

impl LeafUnionFind {
    fn intern(&mut self, leaf: &str) -> usize {
        if let Some(&i) = self.index.get(leaf) {
            return i;
        }
        let i = self.labels.len();
        self.index.insert(leaf.to_string(), i);
        self.labels.push(leaf.to_string());
        self.parent.push(i);
        i
    }

    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }

    fn union(&mut self, a: usize, b: usize) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra != rb {
            self.parent[ra] = rb;
        }
    }

    fn component_min_label(&mut self) -> HashMap<String, String> {
        let mut root_min: HashMap<usize, String> = HashMap::new();
        for i in 0..self.labels.len() {
            let root = self.find(i);
            let label = self.labels[i].clone();
            root_min
                .entry(root)
                .and_modify(|m| {
                    if label < *m {
                        *m = label.clone();
                    }
                })
                .or_insert(label);
        }
        let mut out = HashMap::with_capacity(self.labels.len());
        for i in 0..self.labels.len() {
            let root = self.find(i);
            out.insert(self.labels[i].clone(), root_min[&root].clone());
        }
        out
    }
}

/// Single dispatch site for per-language node classification. Delegates to
/// the [`super::language_rules::LanguageRules`] implementation chosen by
/// [`rules_for`].
fn classify_node<'a>(
    language: Language,
    source: &'a str,
    node: Node<'a>,
) -> Option<Classified<'a>> {
    rules_for(language)?.classify_node(language, source, node)
}

/// Walk backward through `node`'s preceding siblings, extending the
/// effective start of the item to absorb any "leading metadata" — outer
/// attributes, decorators, annotations, and doc comments — that belong
/// to the next item. Without this, structural reorder/delete merges leave
/// the metadata stranded in inter-item content where it can be pulled
/// into the wrong slot or duplicated across slots (Codex r3 P1 #2).
fn leading_metadata_start(language: Language, source: &str, node: Node<'_>) -> usize {
    let mut earliest = node.start_byte();
    let mut current = node;
    while let Some(prev) = current.prev_sibling() {
        if !is_leading_metadata_for(language, prev, source, current.start_byte()) {
            break;
        }
        earliest = prev.start_byte();
        current = prev;
    }
    earliest
}

/// Whether `prev` is metadata that "belongs to" the item starting at
/// `next_start`. The rule list per language is data-driven via
/// [`super::language_rules::LanguageRules::leading_metadata_kinds`]; this
/// function applies the binding condition uniformly.
fn is_leading_metadata_for(
    language: Language,
    prev: Node<'_>,
    source: &str,
    next_start: usize,
) -> bool {
    let Some(rules) = rules_for(language) else {
        return false;
    };
    let kind = prev.kind();
    rules.leading_metadata_kinds().iter().any(|rule| {
        rule.kind == kind
            && match rule.binding {
                MetadataBinding::Always => true,
                MetadataBinding::NoBlankLine => {
                    !has_blank_line_between(source, prev.end_byte(), next_start)
                }
                MetadataBinding::RustOuterComment => {
                    !is_rust_inner_doc_comment(source, prev)
                        && !has_blank_line_between(source, prev.end_byte(), next_start)
                }
            }
    })
}

/// Whether a Rust `line_comment` / `block_comment` is an *inner* doc
/// comment (`//!` or `/*!`). Inner doc comments document the enclosing
/// module/crate, not the following item, so they must not be absorbed
/// into the next item's range — same reasoning as `inner_attribute_item`.
/// Text-based rather than grammar-based so the check survives
/// tree-sitter-rust grammar revisions that move the marker between
/// child-node names.
fn is_rust_inner_doc_comment(source: &str, node: Node<'_>) -> bool {
    let bytes = source.as_bytes();
    let start = node.start_byte();
    if start + 3 > source.len() {
        return false;
    }
    let head = &bytes[start..start + 3];
    head == b"//!" || head == b"/*!"
}

/// Whether the byte range `start..end` contains a blank line — i.e.,
/// two or more `\n` bytes. Used to distinguish a doc-comment block
/// attached to the next item (no blank line) from a free-floating
/// comment (blank line present).
fn has_blank_line_between(source: &str, start: usize, end: usize) -> bool {
    if start >= end {
        return false;
    }
    source.as_bytes()[start..end]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
        >= 2
}
