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

use std::hash::{Hash, Hasher};

use tree_sitter::Node;

use crate::parser::{Language, ParsedFile};

/// Categorisation of an item. Used as part of [`ItemKey`] so two items with
/// the same name but different shapes (e.g. a struct `Foo` and a function
/// `Foo`) don't collide.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum ItemKind {
    Function,
    Method,
    Impl,
    Module,
    Struct,
    Enum,
    Trait,
    TypeAlias,
    Const,
    Static,
}

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
    let mut stack: Vec<(Node<'_>, Vec<String>, usize)> = vec![(root, base_scope.to_vec(), 0)];
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
                    // Container with body (impl / trait / class / mod
                    // with `{ ... }`): the container ITSELF is not an
                    // item — its bytes become inter-item content
                    // surrounding the per-method items inside. This
                    // gives each method its own merge resolution
                    // instead of forcing the whole container through
                    // text_hunk_merge as a single unit.
                    let mut next_scope = scope.clone();
                    next_scope.push(name);
                    stack.push((body, next_scope, depth + 1));
                } else {
                    // Leaf item — top-level fn, struct, const, mod
                    // header, etc. Push as item; nothing inside is
                    // independently tracked.
                    let mut item_scope = scope.clone();
                    item_scope.extend(extra_scope);
                    let item_key = ItemKey {
                        kind,
                        name,
                        scope: item_scope,
                        signature_hash,
                    };
                    let start_byte =
                        leading_metadata_start(language, source, child);
                    out.push(Item {
                        key: item_key,
                        start_byte,
                        end_byte: child.end_byte(),
                    });
                }
            } else {
                // Unclassified at this level: walk it later so we still
                // find items in anonymous wrapper nodes (e.g.
                // `source_file` children, declaration_list wrappers).
                stack.push((child, scope.clone(), depth + 1));
            }
        }
    }
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
/// `next_start`. Per-language rules:
///
/// * Rust: outer `attribute_item` (`#[...]`) always; line/block comments
///   only when (a) they are not inner doc comments (`//!` / `/*!`) and
///   (b) no blank line separates them from the item. `inner_attribute_item`
///   (`#![...]`) is NEVER bound — it applies to the enclosing
///   module/crate, not the following item.
/// * Go: line/block comments only when no blank line separates them.
/// * Java: marker / regular annotations always; line/block comments only
///   when no blank line separates them (matches the Rust/Go rule —
///   standalone comments separated by blank lines must NOT migrate with
///   the next method/class during merges).
///
/// Python decorators are not handled here because tree-sitter wraps them
/// in `decorated_definition`, a different node kind than
/// `function_definition` (handled at classification time).
/// JavaScript / TypeScript / C / C++ have no equivalent leading-sibling
/// metadata pattern that this driver currently recognises.
fn is_leading_metadata_for(
    language: Language,
    prev: Node<'_>,
    source: &str,
    next_start: usize,
) -> bool {
    let kind = prev.kind();
    match language {
        Language::Rust => match kind {
            // Outer attributes only. `inner_attribute_item` (`#![...]`)
            // applies to the enclosing scope — absorbing it into the
            // next item drops or relocates crate-/module-level
            // attributes (`#![no_std]`, `#![allow(...)]`) when that
            // item is deleted, modified, or duplicated across sides.
            "attribute_item" => true,
            "line_comment" | "block_comment" => {
                !is_rust_inner_doc_comment(source, prev)
                    && !has_blank_line_between(source, prev.end_byte(), next_start)
            }
            _ => false,
        },
        Language::Go => matches!(kind, "comment")
            && !has_blank_line_between(source, prev.end_byte(), next_start),
        Language::Java => match kind {
            "marker_annotation" | "annotation" => true,
            "line_comment" | "block_comment" => {
                !has_blank_line_between(source, prev.end_byte(), next_start)
            }
            _ => false,
        },
        Language::Python
        | Language::JavaScript
        | Language::TypeScript
        | Language::C
        | Language::Cpp
        | Language::Unknown => false,
    }
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

/// Classifier output: what kind of item, its name, an optional body to
/// recurse into (for container items), and a parameter-signature hash for
/// overload disambiguation (zero for non-function items).
struct Classified<'a> {
    kind: ItemKind,
    name: String,
    container_body: Option<Node<'a>>,
    signature_hash: u64,
    /// Extra scope components appended to the inherited scope before
    /// constructing the ItemKey. Used for Go method receivers — without
    /// the receiver type in scope, two methods named `String` on
    /// different receiver types collide.
    extra_scope: Vec<String>,
}

/// Classify a node as an item the merger recognises, or return `None`.
fn classify_node<'a>(
    language: Language,
    source: &'a str,
    node: Node<'a>,
) -> Option<Classified<'a>> {
    let kind = node.kind();
    match language {
        Language::Rust => classify_rust_node(source, node, kind),
        Language::Python => classify_python_node(source, node, kind),
        Language::JavaScript | Language::TypeScript => classify_js_node(source, node, kind),
        Language::Go => classify_go_node(source, node, kind),
        Language::C | Language::Cpp => classify_c_node(source, node, kind),
        Language::Java => classify_java_node(source, node, kind),
        Language::Unknown => None,
    }
}

fn classify_rust_node<'a>(
    source: &'a str,
    node: Node<'a>,
    kind: &str,
) -> Option<Classified<'a>> {
    match kind {
        "function_item" => {
            let name = name_from_field(source, node, "name")?;
            let signature_hash = signature_hash_from_field(source, node, "parameters");
            Some(Classified {
                kind: ItemKind::Function,
                name,
                container_body: None,
                signature_hash,
                extra_scope: Vec::new(),
            })
        }
        "function_signature_item" => {
            // Trait method signature without body.
            let name = name_from_field(source, node, "name")?;
            let signature_hash = signature_hash_from_field(source, node, "parameters");
            Some(Classified {
                kind: ItemKind::Method,
                name,
                container_body: None,
                signature_hash,
                extra_scope: Vec::new(),
            })
        }
        "impl_item" => {
            // Name an impl by `<type>` or `<trait> for <type>` so two impls
            // for the same type but different traits get distinct keys.
            let name = rust_impl_name(source, node)?;
            let container_body = node.child_by_field_name("body");
            Some(Classified {
                kind: ItemKind::Impl,
                name,
                container_body,
                signature_hash: 0,
                extra_scope: Vec::new(),
            })
        }
        "mod_item" => {
            let name = name_from_field(source, node, "name")?;
            // mod may be a header (no body, `mod foo;`) or have a body.
            let container_body = node.child_by_field_name("body");
            Some(Classified {
                kind: ItemKind::Module,
                name,
                container_body,
                signature_hash: 0,
                extra_scope: Vec::new(),
            })
        }
        "struct_item" => simple_item(source, node, "name", ItemKind::Struct),
        "enum_item" => simple_item(source, node, "name", ItemKind::Enum),
        "trait_item" => {
            let name = name_from_field(source, node, "name")?;
            let container_body = node.child_by_field_name("body");
            Some(Classified {
                kind: ItemKind::Trait,
                name,
                container_body,
                signature_hash: 0,
                extra_scope: Vec::new(),
            })
        }
        "union_item" => simple_item(source, node, "name", ItemKind::Struct),
        "type_item" => simple_item(source, node, "name", ItemKind::TypeAlias),
        "const_item" => simple_item(source, node, "name", ItemKind::Const),
        "static_item" => simple_item(source, node, "name", ItemKind::Static),
        _ => None,
    }
}

fn classify_python_node<'a>(
    source: &'a str,
    node: Node<'a>,
    kind: &str,
) -> Option<Classified<'a>> {
    match kind {
        "function_definition" => {
            let name = name_from_field(source, node, "name")?;
            let signature_hash = signature_hash_from_field(source, node, "parameters");
            Some(Classified {
                kind: ItemKind::Function,
                name,
                container_body: None,
                signature_hash,
                extra_scope: Vec::new(),
            })
        }
        "class_definition" => {
            let name = name_from_field(source, node, "name")?;
            let container_body = node.child_by_field_name("body");
            Some(Classified {
                kind: ItemKind::Module,
                name,
                container_body,
                signature_hash: 0,
                extra_scope: Vec::new(),
            })
        }
        // tree-sitter Python wraps decorated symbols in
        // `decorated_definition` with children:
        //   * one or more `decorator` nodes (`@foo`, `@bar.baz`, ...)
        //   * a `definition` field pointing at a class_definition or
        //     function_definition
        // Treat the whole wrapper as a leaf item so the decorators are
        // part of the item's byte range. Otherwise the inner def
        // classifies first and the decorators end up as orphaned
        // inter-item content — reorder/delete merges drop or
        // misattach them. Inner classification (name + signature) is
        // copied from the inner definition; container_body is FORCED
        // to None even when the inner is a class, so the decorated
        // class merges as one atomic unit (we lose per-method
        // resolution inside decorated classes, but keep the decorator
        // bound to its class — the simpler trade-off, since reordering
        // a decorated class while editing its methods is rarer than
        // simply moving/deleting the whole decorated symbol).
        "decorated_definition" => {
            let inner = node.child_by_field_name("definition")?;
            let inner_kind = inner.kind();
            let inner_classified = classify_python_node(source, inner, inner_kind)?;
            Some(Classified {
                container_body: None,
                ..inner_classified
            })
        }
        _ => None,
    }
}

fn classify_js_node<'a>(
    source: &'a str,
    node: Node<'a>,
    kind: &str,
) -> Option<Classified<'a>> {
    match kind {
        "function_declaration" | "generator_function_declaration" => {
            let name = name_from_field(source, node, "name")?;
            let signature_hash = signature_hash_from_field(source, node, "parameters");
            Some(Classified {
                kind: ItemKind::Function,
                name,
                container_body: None,
                signature_hash,
                extra_scope: Vec::new(),
            })
        }
        "class_declaration" => {
            let name = name_from_field(source, node, "name")?;
            let container_body = node.child_by_field_name("body");
            Some(Classified {
                kind: ItemKind::Module,
                name,
                container_body,
                signature_hash: 0,
                extra_scope: Vec::new(),
            })
        }
        "method_definition" => {
            let name = name_from_field(source, node, "name")?;
            let signature_hash = signature_hash_from_field(source, node, "parameters");
            Some(Classified {
                kind: ItemKind::Method,
                name,
                container_body: None,
                signature_hash,
                extra_scope: Vec::new(),
            })
        }
        _ => None,
    }
}

fn classify_go_node<'a>(
    source: &'a str,
    node: Node<'a>,
    kind: &str,
) -> Option<Classified<'a>> {
    match kind {
        "function_declaration" => {
            let name = name_from_field(source, node, "name")?;
            let signature_hash = signature_hash_from_field(source, node, "parameters");
            Some(Classified {
                kind: ItemKind::Function,
                name,
                container_body: None,
                signature_hash,
                extra_scope: Vec::new(),
            })
        }
        "method_declaration" => {
            let name = name_from_field(source, node, "name")?;
            let signature_hash = signature_hash_from_field(source, node, "parameters");
            // Receiver type disambiguates two methods with the same name
            // on different receivers — `func (a A) String()` vs
            // `func (b B) String()`. Without it the BTreeMap collapses
            // them and one method is dropped from the merge.
            let extra_scope = go_receiver_type(source, node)
                .map(|t| vec![t])
                .unwrap_or_default();
            Some(Classified {
                kind: ItemKind::Method,
                name,
                container_body: None,
                signature_hash,
                extra_scope,
            })
        }
        _ => None,
    }
}

/// Extract the receiver type from a Go `method_declaration` as a
/// whitespace-stripped string (e.g. `"A"`, `"*A"`, `"Foo[T]"`). Returns
/// `None` for non-methods or malformed receivers.
fn go_receiver_type(source: &str, node: Node<'_>) -> Option<String> {
    let receiver = node.child_by_field_name("receiver")?;
    let mut cursor = receiver.walk();
    for child in receiver.children(&mut cursor) {
        if child.kind() == "parameter_declaration"
            && let Some(ty) = child.child_by_field_name("type")
        {
            return Some(strip_whitespace(&source[ty.byte_range()]));
        }
    }
    None
}

fn classify_c_node<'a>(
    source: &'a str,
    node: Node<'a>,
    kind: &str,
) -> Option<Classified<'a>> {
    if kind == "function_definition" {
        let declarator = node.child_by_field_name("declarator")?;
        let name = identifier_in_subtree(source, declarator)?;
        // C/C++ parameter list lives inside the declarator subtree as a
        // `parameter_list` node — find it for overload disambiguation.
        // Use the structural hash (arity + per-parameter type) so a
        // parameter-name rename doesn't split function identity.
        let signature_hash = find_descendant(declarator, &["parameter_list"])
            .map(|n| signature_hash_from_parameter_list(source, n))
            .unwrap_or(0);
        return Some(Classified {
            kind: ItemKind::Function,
            name,
            container_body: None,
            signature_hash,
            extra_scope: Vec::new(),
        });
    }
    None
}

fn classify_java_node<'a>(
    source: &'a str,
    node: Node<'a>,
    kind: &str,
) -> Option<Classified<'a>> {
    match kind {
        "method_declaration" | "constructor_declaration" => {
            let name = name_from_field(source, node, "name")?;
            let signature_hash = signature_hash_from_field(source, node, "parameters");
            Some(Classified {
                kind: ItemKind::Method,
                name,
                container_body: None,
                signature_hash,
                extra_scope: Vec::new(),
            })
        }
        "class_declaration" | "interface_declaration" => {
            let name = name_from_field(source, node, "name")?;
            let container_body = node.child_by_field_name("body");
            Some(Classified {
                kind: ItemKind::Module,
                name,
                container_body,
                signature_hash: 0,
                extra_scope: Vec::new(),
            })
        }
        _ => None,
    }
}

fn simple_item<'a>(
    source: &'a str,
    node: Node<'a>,
    name_field: &str,
    kind: ItemKind,
) -> Option<Classified<'a>> {
    let name = name_from_field(source, node, name_field)?;
    Some(Classified {
        kind,
        name,
        container_body: None,
        signature_hash: 0,
        extra_scope: Vec::new(),
    })
}

/// Hash the parameter list at field `field`, keying on arity + types
/// only. Returns 0 when the field is absent (e.g. parameterless
/// declarations).
fn signature_hash_from_field(source: &str, node: Node<'_>, field: &str) -> u64 {
    let Some(params) = node.child_by_field_name(field) else {
        return 0;
    };
    signature_hash_from_parameter_list(source, params)
}

/// Hash a parameter-list node by arity + per-parameter type spelling,
/// IGNORING parameter names. A pure parameter rename (`foo(x: u32)` →
/// `foo(y: u32)`) must NOT change the hash — otherwise the renamed
/// function gets a different `ItemKey.signature_hash` from base, the
/// merger treats it as delete+add, and a disjoint body change on the
/// other side surfaces as a modify/delete conflict instead of merging
/// cleanly (Codex r5 P1 #1).
///
/// The walk is uniform across languages: for each NAMED child of the
/// parameter-list (anonymous punctuation `(`, `)`, `,` is skipped
/// because tree-sitter anonymous nodes are excluded from named-children
/// iteration), look for a `type` field. Hash its whitespace-stripped
/// spelling when present, else a placeholder so untyped parameters
/// still contribute to arity. Arity is mixed in at the end so
/// `foo(x: u32)` and `foo(x: u32, y: u32)` don't collide.
///
/// Per-language notes on the `type` field:
/// * Rust `parameter` has `type`; `self_parameter` does not — hashed
///   as the placeholder (consistent across sides).
/// * Python `typed_parameter` / `typed_default_parameter` have `type`;
///   bare `identifier` / `default_parameter` (untyped) hash as the
///   placeholder.
/// * TypeScript `required_parameter` / `optional_parameter` have
///   `type`; plain JavaScript parameters don't (placeholder).
/// * Java `formal_parameter` and Go `parameter_declaration` always
///   have `type`.
/// * C/C++ `parameter_declaration` has `type` (the type specifier;
///   the declarator carrying the name lives in a separate field that
///   we deliberately don't read).
fn signature_hash_from_parameter_list(source: &str, params: Node<'_>) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let mut cursor = params.walk();
    let mut arity: u64 = 0;
    for child in params.named_children(&mut cursor) {
        if child.kind() == "comment" {
            continue;
        }
        arity += 1;
        let type_text = child
            .child_by_field_name("type")
            .map(|t| strip_whitespace(&source[t.byte_range()]))
            .unwrap_or_else(|| "_".to_string());
        type_text.hash(&mut hasher);
        // Separator so `foo(ab, c)` and `foo(a, bc)` don't collide on
        // concatenated type spellings.
        b"|".hash(&mut hasher);
    }
    arity.hash(&mut hasher);
    hasher.finish()
}

fn hash_normalized(s: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    // Strip whitespace rather than splitting on it: `split_whitespace`
    // leaves punctuation attached to identifiers, so `foo(x,y)` and
    // `foo(x, y)` produce different token streams and hash differently
    // — the same function ends up with distinct ItemKeys across sides.
    strip_whitespace(s).hash(&mut hasher);
    hasher.finish()
}

/// Drop all Unicode whitespace from `s`, preserving every other byte.
/// Cosmetic reformatting that only adds/removes whitespace becomes
/// invisible to the identity comparison; punctuation that semantically
/// distinguishes spellings (`*A` vs `A`, `Foo[T]` vs `Foo`) is retained.
fn strip_whitespace(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

fn find_descendant<'a>(node: Node<'a>, kinds: &[&str]) -> Option<Node<'a>> {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if kinds.contains(&current.kind()) {
            return Some(current);
        }
        for i in (0..current.child_count()).rev() {
            if let Some(child) = current.child(i as u32) {
                stack.push(child);
            }
        }
    }
    None
}

fn name_from_field(source: &str, node: Node<'_>, field: &str) -> Option<String> {
    let name_node = node.child_by_field_name(field)?;
    Some(source[name_node.byte_range()].to_string())
}

fn identifier_in_subtree(source: &str, node: Node<'_>) -> Option<String> {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if matches!(
            current.kind(),
            "identifier" | "field_identifier" | "type_identifier" | "property_identifier"
        ) {
            return Some(source[current.byte_range()].to_string());
        }
        for i in (0..current.child_count()).rev() {
            if let Some(child) = current.child(i as u32) {
                stack.push(child);
            }
        }
    }
    None
}

/// Name an impl block. Two impls of the same type with different traits must
/// produce different keys, so we include the trait when present:
///   `impl Foo` → `Foo`
///   `impl Trait for Foo` → `Trait for Foo`
fn rust_impl_name(source: &str, node: Node<'_>) -> Option<String> {
    let trait_node = node.child_by_field_name("trait");
    let type_node = node.child_by_field_name("type")?;
    let type_name = source[type_node.byte_range()].to_string();
    let key = if let Some(trait_node) = trait_node {
        format!("{} for {}", &source[trait_node.byte_range()], type_name)
    } else {
        type_name
    };
    // Strip ALL whitespace from the key so cosmetic reformatting around
    // `::`, `<>`, etc. doesn't turn into a "different impl"
    // misclassification — same shape as `hash_normalized` for signature
    // hashes (r3 fix `021ed8e`).
    Some(strip_whitespace(&key))
}
