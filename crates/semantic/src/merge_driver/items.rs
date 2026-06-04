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
use super::language_rules::{rules_for, Classified, MetadataBinding, USE_POISON_KEY};
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
    /// `Some` when this item is a *container* — an `impl` / `mod` / `trait` /
    /// namespace / class whose body holds nested child items. Carrying the
    /// parse-tree parent→child edge here (a shallow tree, not a flat list) is
    /// the heddle#490 fix: the merger pairs and reconstructs containers
    /// structurally via [`ItemKey`] matching + recursion, so container
    /// identity and nesting are never re-derived from byte offsets (the
    /// flatten-then-re-derive class that produced the heddle#484 P0). `None`
    /// for leaf items (functions, structs, `use`, …) and for containers
    /// nested past [`CONTAINER_DEPTH_LIMIT`], which merge as one opaque byte
    /// blob.
    pub body: Option<ContainerBody>,
}

/// The body of a container item. `inner_start`/`inner_end` are the byte span
/// of the body node (the delimiters `{` / `}` fall *inside* this span for
/// brace languages); `items` are the nested children in source order.
///
/// The container's *header* is `[Item::start_byte, inner_start)` (e.g.
/// `impl Foo `), its *footer* is `[inner_end, Item::end_byte)` (usually
/// empty). The braces are woven as the body region's preamble/postamble by
/// [`super::reconstruct`], so no `{`/`}` is ever synthesized or trimmed.
#[derive(Clone, Debug)]
pub(crate) struct ContainerBody {
    pub inner_start: usize,
    pub inner_end: usize,
    pub items: Vec<Item>,
}

/// The result of segmenting a file: top-level items in source order (each
/// container item carries its nested children), plus the source byte length
/// stashed for reconstruction.
#[derive(Clone, Debug)]
pub(crate) struct FileSegments {
    pub items: Vec<Item>,
    pub source_len: usize,
}

/// Inter-item slices for a list of sibling items occupying the byte region
/// `[region_start, region_end)`. Length is `items.len() + 1`: the first slice
/// is the preamble (region_start → first item), the last is the postamble
/// (last item → region_end), middle slices sit between consecutive items.
/// Used both at file scope and recursively for each container body.
pub(crate) fn inter_ranges(
    items: &[Item],
    region_start: usize,
    region_end: usize,
) -> Vec<(usize, usize)> {
    let mut out = Vec::with_capacity(items.len() + 1);
    let mut cursor = region_start;
    for item in items {
        out.push((cursor, item.start_byte));
        cursor = item.end_byte;
    }
    out.push((cursor, region_end));
    out
}

/// Extract items from a parsed file as a shallow tree (containers carry their
/// children). Extraction itself is iterative (bounded stack regardless of
/// nesting depth — heddle#114 r1 P2); the tree is then assembled
/// non-recursively from the flat pre-order list by byte containment.
pub(crate) fn extract_items(parsed: &ParsedFile) -> Vec<Item> {
    let raws = collect_raw_items(parsed.language, &parsed.source, parsed.root_node());
    assemble_tree(raws)
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

/// Cap on *container* nesting carried into the merge tree. A container nested
/// deeper than this is emitted as an opaque leaf (`body: None`, whole byte
/// range) whose contents merge as text rather than being recursed into. This
/// bounds the recursion depth of [`super::reconstruct`]'s tree walk so it
/// cannot overflow the stack on pathological nesting (the heddle#114 r1 P2
/// 128 KiB / 2000-module guard). Real Rust/C++/Java code never nests
/// `impl`/`mod`/`class` blocks anywhere near this deep.
const CONTAINER_DEPTH_LIMIT: usize = 8;

/// A flat, pre-tree extraction record. Collected iteratively (bounded stack);
/// [`assemble_tree`] folds the list into the nested [`Item`] tree by byte
/// containment.
struct RawItem {
    key: ItemKey,
    start_byte: usize,
    end_byte: usize,
    use_identity: Option<UseIdentity>,
    /// `Some((inner_start, inner_end))` when this record is a container we
    /// recursed into; `None` for leaves and for opaque (too-deep) containers.
    container: Option<(usize, usize)>,
}

/// Iterative DFS over the AST producing a flat list of [`RawItem`]s. Avoids
/// the unbounded recursion a deeply-parseable file could otherwise trigger —
/// extraction used to recurse for every container body AND every unclassified
/// wrapper node, so a synthetic 50k-deep tree would blow the stack even
/// though tree-sitter itself parses iteratively. Each stack entry is
/// `(node-whose-children-to-walk, scope, ast_depth, container_depth)`;
/// `ast_depth` bails the whole walk past [`MAX_TRAVERSAL_DEPTH`], while
/// `container_depth` stops *recursing into* container bodies past
/// [`CONTAINER_DEPTH_LIMIT`] (such a container is recorded as an opaque leaf).
fn collect_raw_items(language: Language, source: &str, root: Node<'_>) -> Vec<RawItem> {
    let mut out: Vec<RawItem> = Vec::new();
    let empty: Rc<Vec<String>> = Rc::new(Vec::new());
    // (node, scope, ast_depth, container_depth)
    let mut stack: Vec<(Node<'_>, Rc<Vec<String>>, usize, usize)> =
        vec![(root, Rc::clone(&empty), 0, 0)];

    while let Some((node, scope, depth, cdepth)) = stack.pop() {
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
                let start_byte = leading_metadata_start(language, source, child);
                if let Some(body) = container_body {
                    let recurse = cdepth < CONTAINER_DEPTH_LIMIT;
                    let item_key = ItemKey {
                        kind,
                        name: name.clone(),
                        scope: (*scope).clone(),
                        signature_hash,
                    };
                    out.push(RawItem {
                        key: item_key,
                        start_byte,
                        end_byte: child.end_byte(),
                        use_identity: None,
                        container: recurse.then(|| (body.start_byte(), body.end_byte())),
                    });
                    if recurse {
                        let mut next_scope = (*scope).clone();
                        next_scope.push(name);
                        stack.push((body, Rc::new(next_scope), depth + 1, cdepth + 1));
                    }
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
                    out.push(RawItem {
                        key: item_key,
                        start_byte,
                        end_byte: child.end_byte(),
                        use_identity,
                        container: None,
                    });
                }
            } else {
                stack.push((child, Rc::clone(&scope), depth + 1, cdepth));
            }
        }
    }
    out
}

/// Fold a flat pre-order [`RawItem`] list into the nested [`Item`] tree by
/// byte containment. Non-recursive (an explicit stack of open containers), so
/// it is stack-safe regardless of nesting; the resulting tree depth is bounded
/// by [`CONTAINER_DEPTH_LIMIT`] (deeper containers were recorded as opaque
/// leaves). The parse tree guarantees proper nesting, so a simple
/// "close any container that ends before this item starts, then attach"
/// sweep over start-sorted records reconstructs the parent→child edges.
fn assemble_tree(mut raws: Vec<RawItem>) -> Vec<Item> {
    raws.sort_by_key(|r| r.start_byte);

    let mut top: Vec<Item> = Vec::new();
    // Open containers: (partially-built container Item, its accumulated
    // children, its inner_end). The deepest-open container is last.
    let mut open: Vec<(Item, Vec<Item>, usize)> = Vec::new();

    fn attach(item: Item, open: &mut [(Item, Vec<Item>, usize)], top: &mut Vec<Item>) {
        match open.last_mut() {
            Some((_, children, _)) => children.push(item),
            None => top.push(item),
        }
    }

    fn close_one(open: &mut Vec<(Item, Vec<Item>, usize)>, top: &mut Vec<Item>) {
        let (mut container, children, inner_end) = open.pop().unwrap();
        if let Some(body) = container.body.as_mut() {
            debug_assert_eq!(body.inner_end, inner_end);
            body.items = children;
        }
        attach(container, open, top);
    }

    for raw in raws {
        while open.last().is_some_and(|(_, _, end)| raw.start_byte >= *end) {
            close_one(&mut open, &mut top);
        }
        match raw.container {
            Some((inner_start, inner_end)) => {
                let item = Item {
                    key: raw.key,
                    start_byte: raw.start_byte,
                    end_byte: raw.end_byte,
                    use_identity: raw.use_identity,
                    body: Some(ContainerBody {
                        inner_start,
                        inner_end,
                        items: Vec::new(),
                    }),
                };
                open.push((item, Vec::new(), inner_end));
            }
            None => {
                let item = Item {
                    key: raw.key,
                    start_byte: raw.start_byte,
                    end_byte: raw.end_byte,
                    use_identity: raw.use_identity,
                    body: None,
                };
                attach(item, &mut open, &mut top);
            }
        }
    }
    while !open.is_empty() {
        close_one(&mut open, &mut top);
    }
    top
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
/// The leaf union runs ONLY when every `use` item on every side is a
/// fully-analyzable plain import ([`UseIdentity::Plain`]). A single
/// unanalyzable form ([`UseIdentity::Unanalyzable`] — `self` in a group,
/// nested group, glob, `as` alias, metavariable, malformed) anywhere
/// **poisons** the use-region: we cannot extract its leaves, so it might
/// overlap a plain import on a leaf we never saw, and the leaf partition
/// can no longer be trusted. In that case every `use` item is rekeyed to
/// the shared [`USE_POISON_KEY`] instead, collapsing the region into one
/// component that [`super::reconstruct::resolve_use_component`] resolves
/// as a single conservative whole-region 3-way merge (byte-identical →
/// dedup, anything else → conflict). Capping the clever union to
/// plain-imports-only makes the exotic-form drip class impossible
/// (heddle#468 r6 on PR #477).
///
/// This is a no-op for any `use` whose leaf set overlaps nothing in the
/// un-poisoned path: its component is itself and its canonical name equals
/// its own minimum leaf, matching the pre-canonicalization seed key.
pub(crate) fn canonicalize_use_keys(
    base: &mut FileSegments,
    ours: &mut FileSegments,
    theirs: &mut FileSegments,
) {
    // Poison gate: any unanalyzable `use` on any side disqualifies the
    // whole region from the leaf union. Collapse every `use` item onto one
    // key so the conservative whole-region merge runs instead.
    let mut poisoned = false;
    for seg in [&*base, &*ours, &*theirs] {
        visit_items(&seg.items, &mut |item| {
            if matches!(item.use_identity, Some(UseIdentity::Unanalyzable)) {
                poisoned = true;
            }
        });
    }
    if poisoned {
        for seg in [base, ours, theirs] {
            visit_items_mut(&mut seg.items, &mut |item| {
                if item.use_identity.is_some() {
                    item.key.name = USE_POISON_KEY.to_string();
                }
            });
        }
        return;
    }

    let mut uf = LeafUnionFind::default();
    for seg in [&*base, &*ours, &*theirs] {
        visit_items(&seg.items, &mut |item| {
            let Some(UseIdentity::Plain(leaves)) = &item.use_identity else {
                return;
            };
            let mut leaf_iter = leaves.iter();
            let Some(first) = leaf_iter.next() else {
                return;
            };
            let anchor = uf.intern(first);
            for leaf in leaf_iter {
                let node = uf.intern(leaf);
                uf.union(anchor, node);
            }
        });
    }

    let canonical = uf.component_min_label();
    for seg in [base, ours, theirs] {
        visit_items_mut(&mut seg.items, &mut |item| {
            let Some(UseIdentity::Plain(leaves)) = &item.use_identity else {
                return;
            };
            if let Some(first) = leaves.first()
                && let Some(name) = canonical.get(first)
            {
                item.key.name = name.clone();
            }
        });
    }
}

/// Pre-order visit of every item in `items` and, recursively, every item in
/// each container body. Recursion depth is bounded by
/// [`CONTAINER_DEPTH_LIMIT`].
pub(crate) fn visit_items<'a>(items: &'a [Item], f: &mut impl FnMut(&'a Item)) {
    for item in items {
        f(item);
        if let Some(body) = &item.body {
            visit_items(&body.items, f);
        }
    }
}

/// Mutable [`visit_items`].
fn visit_items_mut(items: &mut [Item], f: &mut impl FnMut(&mut Item)) {
    for item in items {
        f(item);
        if let Some(body) = &mut item.body {
            visit_items_mut(&mut body.items, f);
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
