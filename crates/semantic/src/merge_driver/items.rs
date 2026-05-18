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
/// * JavaScript / TypeScript: `decorator` siblings always bind to the
///   following method / class. In tree-sitter-js/ts the decorator is a
///   sibling of `method_definition` / `class_declaration` inside
///   `class_body`, not a wrapper — so without explicit recognition the
///   decorator stays in inter-item content and reorder / delete / add
///   merges leak it onto the wrong symbol.
///
/// Python decorators are not handled here because tree-sitter wraps them
/// in `decorated_definition`, a different node kind than
/// `function_definition` (handled at classification time). C / C++
/// have no equivalent leading-sibling metadata pattern.
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
        Language::JavaScript | Language::TypeScript => kind == "decorator",
        Language::Python | Language::C | Language::Cpp | Language::Unknown => false,
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
        Language::JavaScript | Language::TypeScript => classify_js_node(language, source, node, kind),
        Language::Go => classify_go_node(source, node, kind),
        Language::C | Language::Cpp => classify_c_node(language, source, node, kind),
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
            let signature_hash =
                signature_hash_from_field(Language::Rust, source, node, "parameters");
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
            let signature_hash =
                signature_hash_from_field(Language::Rust, source, node, "parameters");
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
            let signature_hash =
                signature_hash_from_field(Language::Python, source, node, "parameters");
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
    language: Language,
    source: &'a str,
    node: Node<'a>,
    kind: &str,
) -> Option<Classified<'a>> {
    match kind {
        "function_declaration" | "generator_function_declaration" => {
            let name = name_from_field(source, node, "name")?;
            let signature_hash = signature_hash_from_field(language, source, node, "parameters");
            Some(Classified {
                kind: ItemKind::Function,
                name,
                container_body: None,
                signature_hash,
                extra_scope: Vec::new(),
            })
        }
        // `class_declaration` covers concrete classes;
        // `abstract_class_declaration` is the TS-only variant for
        // `abstract class`. `interface_declaration` is the TS
        // interface container. All three carry a `name` and a body
        // that holds methods we want extracted as per-method items —
        // without explicit classification their bodies extract zero
        // items and the whole container routes through whole-file
        // text-merge (Codex r8 P2, cid 3256283862).
        "class_declaration" | "abstract_class_declaration" | "interface_declaration" => {
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
        // `method_definition` is the concrete class/object method
        // (with body). `method_signature` and `abstract_method_signature`
        // are TS-only body-less declarations inside interfaces and
        // abstract classes respectively. They share the same `name`
        // and `parameters` field shape, so the same key-derivation
        // applies — abstract methods just don't carry a body but
        // remain identifiable by (name, parameter signature).
        "method_definition" | "method_signature" | "abstract_method_signature" => {
            let name = name_from_field(source, node, "name")?;
            let signature_hash = signature_hash_from_field(language, source, node, "parameters");
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
            let signature_hash =
                signature_hash_from_field(Language::Go, source, node, "parameters");
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
            let signature_hash =
                signature_hash_from_field(Language::Go, source, node, "parameters");
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
    language: Language,
    source: &'a str,
    node: Node<'a>,
    kind: &str,
) -> Option<Classified<'a>> {
    match kind {
        "function_definition" => {
            let declarator = node.child_by_field_name("declarator")?;
            let name = c_function_name(source, declarator)?;
            // Out-of-class definitions (`A::foo`, `ns::Foo::bar`) need the
            // qualified scope as part of the key — without it, methods
            // sharing a name across unrelated classes/namespaces collapse
            // to the same ItemKey and the per-side occurrence indexer
            // can pair unrelated functions across sides whenever one side
            // adds or reorders a same-named method (Codex r6 P1 #1).
            let extra_scope = c_function_scope(source, declarator);
            // C/C++ parameter list lives inside the declarator subtree as a
            // `parameter_list` node — find it for overload disambiguation.
            // Use the structural hash (arity + per-parameter type + per-
            // parameter declarator shape) so a parameter-name rename
            // doesn't split function identity AND so pointer/reference/
            // array/function-pointer modifiers in the declarator field
            // disambiguate `f(int)` vs `f(int*)` (Codex r6 P1 #2).
            //
            // Trailing cv- and ref-qualifiers (`const`, `volatile`,
            // `&`, `&&`) live as CHILDREN of the outer
            // `function_declarator`, alongside `parameters` and
            // `declarator`. Without folding them into the hash,
            // member-function overloads on cv- or ref-qualifier alone
            // (`foo()` vs `foo() const`) collapse to identical
            // signature_hashes (Codex r8 P2, cid 3256283859).
            //
            // `noexcept` is deliberately NOT folded in: C++ does not
            // allow overloading by exception specification, so a
            // noexcept addition/removal is a REDECLARATION of the
            // same function — not a new overload. Including it would
            // split identity across sides whenever noexcept changes,
            // degrading the resolution to delete + add (Codex r9 P1,
            // cid 3256397416).
            let signature_hash = c_signature_hash(language, source, declarator);
            Some(Classified {
                kind: ItemKind::Function,
                name,
                container_body: None,
                signature_hash,
                extra_scope,
            })
        }
        // C++ user-defined-type containers: classify with the type's
        // name as the scope component, walk into the body so per-method
        // items inherit `scope=[ClassName]`. Without this, inline
        // methods inside `class A { void foo() {} }` extract as
        // (Function, "foo", [], _) — identical to inline `foo` in any
        // other class — and the per-side occurrence indexer mis-pairs
        // unrelated functions across sides whenever one side adds or
        // reorders a same-named class (Codex r8 P1, cid 3256283864).
        //
        // Out-of-class definitions (`void A::foo()`) still land in the
        // top-level walker with `extra_scope=["A"]` from
        // `c_function_scope`, producing the same scope `["A"]` — so
        // both forms key identically and merge consistently across
        // refactors that move methods inside/outside class bodies.
        //
        // Anonymous classes / structs / unions (no `name` field) skip
        // classification and fall through to the unclassified walker,
        // contributing empty scope. That keeps existing behavior for
        // anonymous types — their methods are rare and any disambiguation
        // we'd invent would diverge between sides.
        "class_specifier" | "struct_specifier" | "union_specifier" => {
            let name = name_from_field(source, node, "name")
                .map(|n| strip_whitespace(&n))?;
            let container_body = node.child_by_field_name("body");
            Some(Classified {
                kind: ItemKind::Module,
                name,
                container_body,
                signature_hash: 0,
                extra_scope: Vec::new(),
            })
        }
        "namespace_definition" if language == Language::Cpp => {
            // Anonymous namespaces (`namespace { ... }`) have no `name`
            // field — fall through to the walker so their contents key
            // at file scope (consistent with C++ semantics where
            // anonymous-namespace symbols have internal linkage at
            // translation-unit scope).
            let name = name_from_field(source, node, "name")
                .map(|n| strip_whitespace(&n))?;
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

fn classify_java_node<'a>(
    source: &'a str,
    node: Node<'a>,
    kind: &str,
) -> Option<Classified<'a>> {
    match kind {
        "method_declaration" | "constructor_declaration" => {
            let name = name_from_field(source, node, "name")?;
            let signature_hash =
                signature_hash_from_field(Language::Java, source, node, "parameters");
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
fn signature_hash_from_field(
    language: Language,
    source: &str,
    node: Node<'_>,
    field: &str,
) -> u64 {
    let Some(params) = node.child_by_field_name(field) else {
        return 0;
    };
    signature_hash_from_parameter_list(language, source, params)
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
/// still contribute to arity. The parameter node KIND is also mixed in
/// so syntactically-distinct parameter classes (TypeScript
/// `required_parameter` vs `optional_parameter` vs Python
/// `default_parameter`) don't collapse on identical type field text —
/// `foo(x: number)` and `foo(x?: number)` are different overload
/// declarations. Arity is mixed in at the end so `foo(x: u32)` and
/// `foo(x: u32, y: u32)` don't collide.
///
/// For C/C++ the parameter `type` field carries only the type
/// specifier (`int`, `T`, `Foo`). Pointer / reference / array /
/// function-pointer modifiers and cv-qualifiers live in the
/// `declarator` field alongside the parameter name, so a name-stripped
/// declarator shape is mixed in too — without it, `f(int)`, `f(int*)`,
/// `f(int&)`, `f(int[])` all collapse to the same hash (Codex r6 P1
/// #2).
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
/// * C/C++ `parameter_declaration` has `type` (the type specifier);
///   modifiers come from the declarator shape, not the `type` field.
fn signature_hash_from_parameter_list(
    language: Language,
    source: &str,
    params: Node<'_>,
) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let mut cursor = params.walk();
    let mut arity: u64 = 0;
    let is_c_family = matches!(language, Language::C | Language::Cpp);
    for child in params.named_children(&mut cursor) {
        if child.kind() == "comment" {
            continue;
        }
        arity += 1;
        // Parameter NODE KIND distinguishes `required_parameter` from
        // `optional_parameter` etc. — same type field text, different
        // overload identity.
        child.kind().hash(&mut hasher);
        b":".hash(&mut hasher);
        let type_text = child
            .child_by_field_name("type")
            .map(|t| strip_whitespace(&source[t.byte_range()]))
            .unwrap_or_else(|| "_".to_string());
        type_text.hash(&mut hasher);
        if is_c_family {
            b"@".hash(&mut hasher);
            let mut shape = String::new();
            if let Some(decl) = child.child_by_field_name("declarator") {
                emit_c_declarator_shape(decl, &mut shape);
            }
            shape.hash(&mut hasher);
        }
        // Separator so `foo(ab, c)` and `foo(a, bc)` don't collide on
        // concatenated type spellings.
        b"|".hash(&mut hasher);
    }
    arity.hash(&mut hasher);
    hasher.finish()
}

/// Combined signature hash for a C/C++ outer `function_declarator`:
/// mixes the parameter-list hash with the trailing cv- and ref-
/// qualifiers (`type_qualifier`, `ref_qualifier`) that live as direct
/// children of the declarator.
///
/// Trailing-return-type, `virtual`, and `noexcept` are deliberately
/// NOT mixed in: none of them change overload identity. `noexcept`
/// in particular is metadata — C++ does NOT allow overloading by
/// exception specification, so `foo()` and `foo() noexcept` are
/// REDECLARATIONS of the same function. Including it would split
/// identity across sides whenever noexcept is added/removed and
/// degrade the resolution to delete + add (Codex r9 P1, cid
/// 3256397416). It also incidentally avoids the parameter-name
/// leakage hazard from conditional `noexcept(noexcept(x.foo()))`
/// clauses where parameter names appear in the hashed text (Codex
/// r9 P2, cid 3256397421).
///
/// Source spelling is hashed after whitespace stripping so cosmetic
/// reformatting (`foo() const` vs `foo()  const`) doesn't split keys.
fn c_signature_hash(language: Language, source: &str, declarator: Node<'_>) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let param_hash = find_descendant(declarator, &["parameter_list"])
        .map(|n| signature_hash_from_parameter_list(language, source, n))
        .unwrap_or(0);
    param_hash.hash(&mut hasher);
    let mut cursor = declarator.walk();
    for child in declarator.children(&mut cursor) {
        match child.kind() {
            "type_qualifier" | "ref_qualifier" => {
                b"@".hash(&mut hasher);
                child.kind().hash(&mut hasher);
                strip_whitespace(&source[child.byte_range()]).hash(&mut hasher);
            }
            _ => {}
        }
    }
    hasher.finish()
}

/// Drop all Unicode whitespace from `s`, preserving every other byte.
/// Cosmetic reformatting that only adds/removes whitespace becomes
/// invisible to the identity comparison; punctuation that semantically
/// distinguishes spellings (`*A` vs `A`, `Foo[T]` vs `Foo`) is retained.
fn strip_whitespace(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Emit a name-stripped canonical shape for a C/C++ parameter
/// declarator. Pointer / reference / array / function-pointer wrappers
/// contribute single-character symbols; identifier leaves (the
/// parameter name, when present) are dropped so a name-only rename
/// doesn't change the hash. Abstract and named declarator variants
/// (`int*` vs `int* p`) collapse to the same shape — they describe
/// identical parameter types.
///
/// Examples:
/// * `int x` (declarator: identifier) → ""
/// * `int* p` / `int*` → "*"
/// * `const T& r` → "&"
/// * `int** pp` → "**"
/// * `int (*fp)(int)` → "*()" (function-pointer wrapper around a
///   pointer wrapper)
/// * `T arr[]` → "[]"
///
/// Unknown declarator kinds emit a `<kind>` token verbatim so we don't
/// silently collapse distinctions in rarer shapes (operator overloads
/// with reference-qualifiers, structured-binding declarators, etc.).
fn emit_c_declarator_shape(node: Node<'_>, out: &mut String) {
    match node.kind() {
        // Name leaves — strip across both named and abstract forms.
        "identifier" | "field_identifier" | "type_identifier" => {}
        "pointer_declarator" | "abstract_pointer_declarator" => out.push('*'),
        "reference_declarator" | "abstract_reference_declarator" => out.push('&'),
        "array_declarator" | "abstract_array_declarator" => out.push_str("[]"),
        "function_declarator" | "abstract_function_declarator" => out.push_str("()"),
        // Pass-through wrappers — no symbol of their own, just recurse.
        "parenthesized_declarator" | "abstract_parenthesized_declarator" => {}
        // Unknown shape — include verbatim so we don't lose signal.
        k => {
            out.push('<');
            out.push_str(k);
            out.push('>');
        }
    }
    // Recurse into NAMED children so identifier leaves can be stripped
    // by the leaf-clause above. Anonymous punctuation (`*`, `&`, etc.)
    // is excluded from named-children iteration.
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        emit_c_declarator_shape(child, out);
    }
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

/// Resolve the actual function name from a C/C++ `function_declarator`.
/// `identifier_in_subtree` over the declarator subtree picks up the
/// FIRST matching identifier in DFS order — for a templated qualified
/// name like `Foo<U>::bar()` the scope's inner `type_identifier`
/// ("Foo") wins and all methods on the same scope collide on
/// name="Foo" (Codex r5 P1 #2).
///
/// Instead, walk the declarator's `declarator` field, stripping layers
/// (`pointer_declarator`, `reference_declarator`, nested
/// `function_declarator`) and recursing into `qualified_identifier`'s
/// `name` field until a plain identifier-ish leaf is reached. That
/// yields the actual method name regardless of how complex the scope
/// prefix is (`Foo::Bar::baz` → "baz"; `Foo<U>::bar` → "bar";
/// `ns::operator+` → "operator+"; `Foo::~Foo` → "~Foo").
fn c_function_name(source: &str, function_declarator: Node<'_>) -> Option<String> {
    let mut current = function_declarator.child_by_field_name("declarator")?;
    // Cap traversal so a pathological wrapper chain doesn't loop.
    for _ in 0..32 {
        match current.kind() {
            "identifier"
            | "field_identifier"
            | "type_identifier"
            | "property_identifier"
            | "operator_name"
            | "destructor_name" => {
                return Some(source[current.byte_range()].to_string());
            }
            // Scope-qualified names: descend to the name field so the
            // scope's identifier never wins.
            "qualified_identifier" | "template_function" => {
                current = current.child_by_field_name("name")?;
            }
            // Wrappers that don't bear the name themselves; the name
            // sits one level deeper in `declarator`.
            "pointer_declarator"
            | "reference_declarator"
            | "function_declarator"
            | "parenthesized_declarator" => {
                current = current.child_by_field_name("declarator")?;
            }
            _ => return None,
        }
    }
    None
}

/// Extract the qualified scope chain from a C/C++ `function_declarator`,
/// outermost first. Returns `[]` for unqualified definitions
/// (`void foo()` inside the file scope) and the chain of scope
/// identifiers for out-of-class definitions: `void A::foo()` → `["A"]`,
/// `void ns::A::foo()` → `["ns", "A"]`, `template <T> void Foo<T>::bar()`
/// → `["Foo"]` (whitespace stripped). Whitespace is stripped from each
/// component so cosmetic reformatting (`A :: foo` vs `A::foo`) doesn't
/// produce different keys.
///
/// Template-argument lists are stripped from each component so the
/// out-of-class form `A<T>::foo` keys at the same scope as the inline
/// form `class A { void foo() {} }` (which inherits scope from the
/// `class_specifier.name` text — `A` only, never `A<T>`). Without this
/// normalization, refactoring a method between inline and out-of-class
/// looks like delete + add to the merger (Codex r9 P2, cid 3256397418).
///
/// The walk mirrors `c_function_name`: strip pointer/reference/array/
/// parenthesized wrappers via the `declarator` field, and at each
/// `qualified_identifier` record its `scope` field text and descend
/// into its `name` field. A `template_function` doesn't bear scope —
/// stop the walk there.
fn c_function_scope(source: &str, function_declarator: Node<'_>) -> Vec<String> {
    let mut scope = Vec::new();
    let Some(mut current) = function_declarator.child_by_field_name("declarator") else {
        return scope;
    };
    for _ in 0..32 {
        match current.kind() {
            "qualified_identifier" => {
                if let Some(s) = current.child_by_field_name("scope") {
                    let raw = strip_whitespace(&source[s.byte_range()]);
                    scope.push(strip_template_args(&raw));
                }
                let Some(next) = current.child_by_field_name("name") else {
                    return scope;
                };
                current = next;
            }
            "pointer_declarator"
            | "reference_declarator"
            | "function_declarator"
            | "parenthesized_declarator" => {
                let Some(next) = current.child_by_field_name("declarator") else {
                    return scope;
                };
                current = next;
            }
            _ => return scope,
        }
    }
    scope
}

/// Strip a trailing template-argument list from a C++ scope component
/// for ItemKey normalization. `A<T>` → `A`, `Foo<int, std::map<K, V>>`
/// → `Foo`, `Bar` → `Bar`. Match on the FIRST `<` so nested arguments
/// are dropped wholesale alongside the outer pair.
///
/// Only intended for scope qualifier text (e.g. the `scope` field of a
/// `qualified_identifier`). Not safe for arbitrary identifiers — an
/// operator name like `operator<` would be truncated. Scope components
/// can't be operator names in well-formed C++, so this is fine here.
fn strip_template_args(s: &str) -> String {
    match s.find('<') {
        Some(i) => s[..i].to_string(),
        None => s.to_string(),
    }
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
    // misclassification (r3 fix `021ed8e`).
    Some(strip_whitespace(&key))
}
