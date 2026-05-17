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
    // De-overlap: if a child item overlaps a parent (e.g. nested fn inside an
    // impl method body), keep the outer one. The DFS skips into impl bodies
    // but stops at function bodies — so this is defence-in-depth.
    let mut out: Vec<Item> = Vec::new();
    for item in items {
        if let Some(last) = out.last()
            && item.start_byte < last.end_byte
        {
            // Nested inside a previously-recorded item; drop.
            continue;
        }
        out.push(item);
    }
    out
}

/// Top-level entry: segment a parsed file into items + record the source
/// length so reconstruction can recover inter-item content.
pub(crate) fn segment_file(parsed: &ParsedFile) -> FileSegments {
    FileSegments {
        items: extract_items(parsed),
        source_len: parsed.source.len(),
    }
}

fn collect_items(
    language: Language,
    source: &str,
    node: Node<'_>,
    scope: &[String],
    out: &mut Vec<Item>,
) {
    // Walk this node's children, classifying each. We only recurse into
    // *container* nodes whose children should themselves be considered top
    // level (impl bodies, mod bodies, trait bodies). Inside a function body
    // we stop — that function is the merge unit and nested fn / closure
    // edits are resolved by its bytes-level merge.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some((kind, name, is_container, container_body)) =
            classify_node(language, source, child)
        {
            let item_key = ItemKey {
                kind,
                name: name.clone(),
                scope: scope.to_vec(),
            };
            out.push(Item {
                key: item_key,
                start_byte: child.start_byte(),
                end_byte: child.end_byte(),
            });
            // For impl / mod / trait, recurse into the body so we catch
            // methods with their scope set to the container's name. The body
            // is a separate child node (`declaration_list` for impl, `body`
            // field for mod / trait).
            if is_container
                && let Some(body) = container_body
            {
                let mut next_scope = scope.to_vec();
                next_scope.push(name);
                collect_items(language, source, body, &next_scope, out);
            }
        } else {
            // Unclassified at this level: recurse so we still find items in
            // anonymous wrapper nodes (e.g. `source_file` children).
            collect_items(language, source, child, scope, out);
        }
    }
}

/// Returns `(kind, name, is_container, container_body)` if `node` is an item
/// the merger recognises. `is_container` is true for nodes whose body should
/// be traversed for sub-items (impl, mod, trait); `container_body` is the
/// body node to recurse into.
fn classify_node<'a>(
    language: Language,
    source: &'a str,
    node: Node<'a>,
) -> Option<(ItemKind, String, bool, Option<Node<'a>>)> {
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
) -> Option<(ItemKind, String, bool, Option<Node<'a>>)> {
    match kind {
        "function_item" => {
            let name = name_from_field(source, node, "name")?;
            Some((ItemKind::Function, name, false, None))
        }
        "function_signature_item" => {
            // Trait method signature without body.
            let name = name_from_field(source, node, "name")?;
            Some((ItemKind::Method, name, false, None))
        }
        "impl_item" => {
            // Name an impl by `<type>` or `<trait> for <type>` so two impls
            // for the same type but different traits get distinct keys.
            let name = rust_impl_name(source, node)?;
            let body = node.child_by_field_name("body");
            Some((ItemKind::Impl, name, true, body))
        }
        "mod_item" => {
            let name = name_from_field(source, node, "name")?;
            // mod may be a header (no body, `mod foo;`) or have a body.
            let body = node.child_by_field_name("body");
            Some((ItemKind::Module, name, body.is_some(), body))
        }
        "struct_item" => {
            let name = name_from_field(source, node, "name")?;
            Some((ItemKind::Struct, name, false, None))
        }
        "enum_item" => {
            let name = name_from_field(source, node, "name")?;
            Some((ItemKind::Enum, name, false, None))
        }
        "trait_item" => {
            let name = name_from_field(source, node, "name")?;
            let body = node.child_by_field_name("body");
            Some((ItemKind::Trait, name, true, body))
        }
        "union_item" => {
            let name = name_from_field(source, node, "name")?;
            Some((ItemKind::Struct, name, false, None))
        }
        "type_item" => {
            let name = name_from_field(source, node, "name")?;
            Some((ItemKind::TypeAlias, name, false, None))
        }
        "const_item" => {
            let name = name_from_field(source, node, "name")?;
            Some((ItemKind::Const, name, false, None))
        }
        "static_item" => {
            let name = name_from_field(source, node, "name")?;
            Some((ItemKind::Static, name, false, None))
        }
        _ => None,
    }
}

fn classify_python_node<'a>(
    source: &'a str,
    node: Node<'a>,
    kind: &str,
) -> Option<(ItemKind, String, bool, Option<Node<'a>>)> {
    match kind {
        "function_definition" => {
            let name = name_from_field(source, node, "name")?;
            Some((ItemKind::Function, name, false, None))
        }
        "class_definition" => {
            let name = name_from_field(source, node, "name")?;
            let body = node.child_by_field_name("body");
            Some((ItemKind::Module, name, true, body))
        }
        _ => None,
    }
}

fn classify_js_node<'a>(
    source: &'a str,
    node: Node<'a>,
    kind: &str,
) -> Option<(ItemKind, String, bool, Option<Node<'a>>)> {
    match kind {
        "function_declaration" | "generator_function_declaration" => {
            let name = name_from_field(source, node, "name")?;
            Some((ItemKind::Function, name, false, None))
        }
        "class_declaration" => {
            let name = name_from_field(source, node, "name")?;
            let body = node.child_by_field_name("body");
            Some((ItemKind::Module, name, true, body))
        }
        "method_definition" => {
            let name = name_from_field(source, node, "name")?;
            Some((ItemKind::Method, name, false, None))
        }
        _ => None,
    }
}

fn classify_go_node<'a>(
    source: &'a str,
    node: Node<'a>,
    kind: &str,
) -> Option<(ItemKind, String, bool, Option<Node<'a>>)> {
    match kind {
        "function_declaration" => {
            let name = name_from_field(source, node, "name")?;
            Some((ItemKind::Function, name, false, None))
        }
        "method_declaration" => {
            let name = name_from_field(source, node, "name")?;
            Some((ItemKind::Method, name, false, None))
        }
        _ => None,
    }
}

fn classify_c_node<'a>(
    source: &'a str,
    node: Node<'a>,
    kind: &str,
) -> Option<(ItemKind, String, bool, Option<Node<'a>>)> {
    if kind == "function_definition" {
        let declarator = node.child_by_field_name("declarator")?;
        let name = identifier_in_subtree(source, declarator)?;
        return Some((ItemKind::Function, name, false, None));
    }
    None
}

fn classify_java_node<'a>(
    source: &'a str,
    node: Node<'a>,
    kind: &str,
) -> Option<(ItemKind, String, bool, Option<Node<'a>>)> {
    match kind {
        "method_declaration" | "constructor_declaration" => {
            let name = name_from_field(source, node, "name")?;
            Some((ItemKind::Method, name, false, None))
        }
        "class_declaration" | "interface_declaration" => {
            let name = name_from_field(source, node, "name")?;
            let body = node.child_by_field_name("body");
            Some((ItemKind::Module, name, true, body))
        }
        _ => None,
    }
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
    // Normalize whitespace within the key so cosmetic reformatting doesn't
    // turn into a "different impl" misclassification.
    Some(key.split_whitespace().collect::<Vec<_>>().join(" "))
}
