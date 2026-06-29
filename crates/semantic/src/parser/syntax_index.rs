// SPDX-License-Identifier: Apache-2.0
//! Compact syntax index derived from a tree-sitter parse tree.

use std::ops::Range;

use tree_sitter::Node;

use super::{
    parser_language::Language,
    parser_types::{FunctionDef, Import, ImportKind},
};

/// Compact Heddle-owned syntax data for one parsed source file.
#[derive(Debug)]
pub struct SyntaxIndex {
    functions: Vec<IndexedFunction>,
    imports: Vec<IndexedImport>,
    line_offsets: Vec<usize>,
}

/// Borrowed view of a function indexed in a [`SyntaxIndex`].
#[derive(Clone, Copy, Debug)]
pub struct FunctionRef<'a> {
    inner: &'a IndexedFunction,
    source: &'a str,
}

/// Borrowed view of an import indexed in a [`SyntaxIndex`].
#[derive(Clone, Copy, Debug)]
pub struct ImportRef<'a> {
    inner: &'a IndexedImport,
    source: &'a str,
}

#[derive(Debug)]
struct IndexedFunction {
    name: String,
    signature: String,
    start_line: usize,
    end_line: usize,
    content: Range<usize>,
}

#[derive(Debug)]
struct IndexedImport {
    raw: Range<usize>,
    kind: ImportKind,
}

impl SyntaxIndex {
    pub(super) fn build(language: Language, source: &str, root: Node<'_>) -> Self {
        let mut index = Self {
            functions: Vec::new(),
            imports: Vec::new(),
            line_offsets: line_offsets(source),
        };

        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            if is_function_node(&node, language)
                && let Some(name) = function_name(&node, source)
            {
                index.functions.push(IndexedFunction {
                    name: name.to_string(),
                    signature: function_signature(&node, source),
                    start_line: node.start_position().row,
                    end_line: node.end_position().row,
                    content: node.byte_range(),
                });
            }

            push_children_reverse(node, &mut stack);
        }

        let mut cursor = root.walk();
        for child in root.children(&mut cursor) {
            match language {
                Language::Rust => match child.kind() {
                    "use_declaration" => index.imports.push(IndexedImport {
                        raw: child.byte_range(),
                        kind: ImportKind::Use,
                    }),
                    "extern_crate_declaration" => index.imports.push(IndexedImport {
                        raw: child.byte_range(),
                        kind: ImportKind::ExternCrate,
                    }),
                    _ => {}
                },
                Language::Python => {
                    if matches!(child.kind(), "import_statement" | "import_from_statement") {
                        index.imports.push(IndexedImport {
                            raw: child.byte_range(),
                            kind: ImportKind::Import,
                        });
                    }
                }
                Language::JavaScript | Language::TypeScript => {
                    if child.kind() == "import_statement" {
                        index.imports.push(IndexedImport {
                            raw: child.byte_range(),
                            kind: ImportKind::Import,
                        });
                    }
                }
                Language::Go | Language::Java => {
                    if child.kind() == "import_declaration" {
                        index.imports.push(IndexedImport {
                            raw: child.byte_range(),
                            kind: ImportKind::Import,
                        });
                    }
                }
                Language::C | Language::Cpp | Language::Unknown => {}
            }
        }

        index
    }

    pub fn functions<'a>(&'a self, source: &'a str) -> impl Iterator<Item = FunctionRef<'a>> + 'a {
        self.functions
            .iter()
            .map(move |inner| FunctionRef { inner, source })
    }

    pub fn imports<'a>(&'a self, source: &'a str) -> impl Iterator<Item = ImportRef<'a>> + 'a {
        self.imports
            .iter()
            .map(move |inner| ImportRef { inner, source })
    }

    /// Byte offsets where each line starts. The first entry is always `0`.
    pub fn line_offsets(&self) -> &[usize] {
        &self.line_offsets
    }
}

impl FunctionRef<'_> {
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    pub fn signature(&self) -> &str {
        &self.inner.signature
    }

    pub fn start_line(&self) -> usize {
        self.inner.start_line
    }

    pub fn end_line(&self) -> usize {
        self.inner.end_line
    }

    pub fn content(&self) -> &str {
        &self.source[self.inner.content.clone()]
    }

    pub fn to_owned(self) -> FunctionDef {
        FunctionDef {
            name: self.name().to_string(),
            signature: self.signature().to_string(),
            start_line: self.start_line(),
            end_line: self.end_line(),
            content: self.content().to_string(),
        }
    }
}

impl ImportRef<'_> {
    pub fn raw(&self) -> &str {
        &self.source[self.inner.raw.clone()]
    }

    pub fn kind(&self) -> ImportKind {
        self.inner.kind
    }

    pub fn to_owned(self) -> Import {
        Import {
            raw: self.raw().to_string(),
            kind: self.kind(),
        }
    }
}

pub(super) fn is_function_kind(kind: &str, language: Language) -> bool {
    match language {
        Language::Rust => {
            kind == "function_item" || kind == "method_declaration" || kind == "closure_expression"
        }
        Language::Python => kind == "function_definition",
        Language::JavaScript | Language::TypeScript => {
            kind == "function_declaration"
                || kind == "method_definition"
                || kind == "generator_function_declaration"
                || kind == "variable_declarator"
        }
        Language::Go => kind == "function_declaration" || kind == "method_declaration",
        Language::C | Language::Cpp => kind == "function_definition",
        Language::Java => kind == "method_declaration" || kind == "constructor_declaration",
        Language::Unknown => false,
    }
}

fn is_function_node(node: &Node<'_>, language: Language) -> bool {
    match language {
        Language::JavaScript | Language::TypeScript => {
            matches!(
                node.kind(),
                "function_declaration" | "method_definition" | "generator_function_declaration"
            ) || (node.kind() == "variable_declarator"
                && node
                    .child_by_field_name("value")
                    .is_some_and(|value| is_javascript_function_value(value.kind())))
        }
        _ => is_function_kind(node.kind(), language),
    }
}

fn function_name<'a>(node: &Node<'_>, source: &'a str) -> Option<&'a str> {
    if let Some(name) = node.child_by_field_name("name") {
        return Some(&source[name.byte_range()]);
    }
    if let Some(declarator) = node.child_by_field_name("declarator") {
        if let Some(name) = c_function_name(declarator, source) {
            return Some(name);
        }
        if let Some(name) = first_identifier_in_subtree(declarator, source) {
            return Some(name);
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if matches!(
            child.kind(),
            "identifier" | "field_identifier" | "type_identifier" | "property_identifier"
        ) {
            return Some(&source[child.byte_range()]);
        }
    }
    None
}

fn c_function_name<'a>(function_declarator: Node<'_>, source: &'a str) -> Option<&'a str> {
    let mut current = function_declarator.child_by_field_name("declarator")?;
    for _ in 0..32 {
        match current.kind() {
            "identifier"
            | "field_identifier"
            | "type_identifier"
            | "property_identifier"
            | "operator_name"
            | "destructor_name" => return Some(&source[current.byte_range()]),
            "qualified_identifier" | "template_function" => {
                current = current.child_by_field_name("name")?;
            }
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

fn first_identifier_in_subtree<'a>(node: Node<'_>, source: &'a str) -> Option<&'a str> {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if matches!(
            current.kind(),
            "identifier" | "field_identifier" | "type_identifier" | "property_identifier"
        ) {
            return Some(&source[current.byte_range()]);
        }
        push_children_reverse(current, &mut stack);
    }
    None
}

fn function_signature(node: &Node<'_>, source: &str) -> String {
    if node.kind() == "variable_declarator" {
        return variable_function_signature(node, source);
    }

    let mut signature_parts = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let kind = child.kind();
        if matches!(
            kind,
            "identifier"
                | "field_identifier"
                | "type_identifier"
                | "property_identifier"
                | "parameters"
                | "formal_parameters"
                | "parameter_list"
                | "function_declarator"
                | "type_parameters"
                | "type_arguments"
                | "return_type"
                | "type_annotation"
                | "result"
        ) {
            signature_parts.push(&source[child.byte_range()]);
        }
        if matches!(
            kind,
            "block" | "compound_statement" | "statement_block" | "suite"
        ) {
            break;
        }
    }

    signature_parts.join(" ")
}

fn variable_function_signature(node: &Node<'_>, source: &str) -> String {
    let Some(name) = node.child_by_field_name("name") else {
        return String::new();
    };
    let Some(value) = node.child_by_field_name("value") else {
        return source[name.byte_range()].to_string();
    };

    let mut signature_parts = vec![&source[name.byte_range()]];
    let mut cursor = value.walk();
    for child in value.children(&mut cursor) {
        if matches!(child.kind(), "formal_parameters" | "parameters") {
            signature_parts.push(&source[child.byte_range()]);
        }
        if matches!(child.kind(), "statement_block" | "body") {
            break;
        }
    }
    signature_parts.join(" ")
}

fn line_offsets(source: &str) -> Vec<usize> {
    let mut offsets =
        Vec::with_capacity(source.as_bytes().iter().filter(|&&b| b == b'\n').count() + 1);
    offsets.push(0);
    for (index, byte) in source.bytes().enumerate() {
        if byte == b'\n' && index + 1 < source.len() {
            offsets.push(index + 1);
        }
    }
    offsets
}

fn is_javascript_function_value(kind: &str) -> bool {
    matches!(
        kind,
        "arrow_function" | "function_expression" | "generator_function"
    )
}

fn push_children_reverse<'tree>(node: Node<'tree>, stack: &mut Vec<Node<'tree>>) {
    let child_count = node.child_count();
    for index in (0..child_count).rev() {
        if let Some(child) = node.child(index as u32) {
            stack.push(child);
        }
    }
}
