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
        if let Some(classified) = classify_node(language, source, child) {
            let Classified {
                kind,
                name,
                container_body,
                signature_hash,
                extra_scope,
            } = classified;
            let mut item_scope = scope.to_vec();
            item_scope.extend(extra_scope);
            let item_key = ItemKey {
                kind,
                name: name.clone(),
                scope: item_scope,
                signature_hash,
            };
            out.push(Item {
                key: item_key,
                start_byte: child.start_byte(),
                end_byte: child.end_byte(),
            });
            // For impl / mod / trait / class, recurse into the body so we
            // catch methods with their scope set to the container's name.
            if let Some(body) = container_body {
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
/// whitespace-normalized string (e.g. `"A"`, `"*A"`, `"Foo[T]"`). Returns
/// `None` for non-methods or malformed receivers.
fn go_receiver_type(source: &str, node: Node<'_>) -> Option<String> {
    let receiver = node.child_by_field_name("receiver")?;
    let mut cursor = receiver.walk();
    for child in receiver.children(&mut cursor) {
        if child.kind() == "parameter_declaration"
            && let Some(ty) = child.child_by_field_name("type")
        {
            return Some(
                source[ty.byte_range()]
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" "),
            );
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
        let signature_hash = find_descendant(declarator, &["parameter_list"])
            .map(|n| hash_normalized(&source[n.byte_range()]))
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

/// Hash the spelling of the parameter-list child named `field`, with
/// whitespace normalized so cosmetic reformatting doesn't fragment matches.
/// Returns 0 when the field is absent (e.g. parameterless declarations).
fn signature_hash_from_field(source: &str, node: Node<'_>, field: &str) -> u64 {
    let Some(params) = node.child_by_field_name(field) else {
        return 0;
    };
    hash_normalized(&source[params.byte_range()])
}

fn hash_normalized(s: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for token in s.split_whitespace() {
        token.hash(&mut hasher);
    }
    hasher.finish()
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
    // Normalize whitespace within the key so cosmetic reformatting doesn't
    // turn into a "different impl" misclassification.
    Some(key.split_whitespace().collect::<Vec<_>>().join(" "))
}
