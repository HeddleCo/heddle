// SPDX-License-Identifier: Apache-2.0
//! Core parsing implementation.

use tree_sitter::{Node, Parser, Tree as TSTree};

use super::{
    parser_language::Language,
    parser_types::{FunctionDef, Import, ImportKind},
};

/// A parsed file with its AST.
#[derive(Debug)]
pub struct ParsedFile {
    pub language: Language,
    pub source: String,
    tree: TSTree,
}

impl ParsedFile {
    /// Parse a file's contents. Returns `None` when the parser declines OR the
    /// resulting tree contains any error node — the driver uses this strict form
    /// for the three merge inputs so an unparseable side routes to the text
    /// fallback.
    pub fn parse(source: impl Into<String>, language: Language) -> Option<Self> {
        Self::parse_inner(source, language, true)
    }

    /// Like [`ParsedFile::parse`] but tolerant of error nodes: returns the
    /// (error-recovered) tree even when `has_error()`. Used by the heddle#484
    /// output-boundary conservation floor, which must extract whatever items the
    /// reconstructed output contains — benign recovery noise (e.g. a stray `;`
    /// from a deleted single-line item) must not be mistaken for a dropped item.
    pub(crate) fn parse_allow_errors(source: impl Into<String>, language: Language) -> Option<Self> {
        Self::parse_inner(source, language, false)
    }

    fn parse_inner(
        source: impl Into<String>,
        language: Language,
        reject_errors: bool,
    ) -> Option<Self> {
        let source = source.into();
        let lang = language.parser()?;

        let mut parser = Parser::new();
        parser.set_language(&lang).ok()?;
        let tree = parser.parse(&source, None)?;

        if reject_errors && tree.root_node().has_error() {
            return None;
        }

        Some(Self {
            language,
            source,
            tree,
        })
    }

    /// Get the root node of the AST.
    pub fn root_node(&self) -> Node<'_> {
        self.tree.root_node()
    }

    /// Extract function definitions from the file.
    pub fn extract_functions(&self) -> Vec<FunctionDef> {
        let mut functions = Vec::new();
        let mut stack = vec![self.root_node()];

        while let Some(node) = stack.pop() {
            if Self::is_function_node(&node, self.language)
                && let Some(name) = self.get_function_name(&node)
            {
                functions.push(FunctionDef {
                    name: name.to_string(),
                    signature: self.get_function_signature(&node),
                    start_line: node.start_position().row,
                    end_line: node.end_position().row,
                    content: self.source[node.byte_range()].to_string(),
                });
            }

            push_children_reverse(node, &mut stack);
        }

        functions
    }

    /// Extract imports from the file.
    pub fn extract_imports(&self) -> Vec<Import> {
        match self.language {
            Language::Rust => self.extract_rust_imports(),
            Language::Python => self.extract_imports_by_kind(
                &["import_statement", "import_from_statement"],
                ImportKind::Import,
            ),
            Language::JavaScript | Language::TypeScript => {
                self.extract_imports_by_kind(&["import_statement"], ImportKind::Import)
            }
            Language::Go | Language::Java => {
                self.extract_imports_by_kind(&["import_declaration"], ImportKind::Import)
            }
            _ => Vec::new(),
        }
    }

    /// Check if a node kind string represents a function definition in the given language.
    pub fn is_function_kind(kind: &str, language: Language) -> bool {
        match language {
            Language::Rust => {
                kind == "function_item"
                    || kind == "method_declaration"
                    || kind == "closure_expression"
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
            _ => false,
        }
    }

    fn is_function_node(node: &Node<'_>, language: Language) -> bool {
        match language {
            Language::Rust => {
                node.kind() == "function_item"
                    || node.kind() == "method_declaration"
                    || node.kind() == "closure_expression"
            }
            Language::Python => node.kind() == "function_definition",
            Language::JavaScript | Language::TypeScript => {
                node.kind() == "function_declaration"
                    || node.kind() == "method_definition"
                    || node.kind() == "generator_function_declaration"
                    || (node.kind() == "variable_declarator"
                        && node
                            .child_by_field_name("value")
                            .is_some_and(|value| is_javascript_function_value(value.kind())))
            }
            Language::Go => {
                node.kind() == "function_declaration" || node.kind() == "method_declaration"
            }
            Language::C | Language::Cpp => node.kind() == "function_definition",
            Language::Java => {
                node.kind() == "method_declaration" || node.kind() == "constructor_declaration"
            }
            _ => false,
        }
    }

    fn get_function_name(&self, node: &Node<'_>) -> Option<&str> {
        if let Some(name) = node.child_by_field_name("name") {
            return Some(&self.source[name.byte_range()]);
        }
        if let Some(declarator) = node.child_by_field_name("declarator") {
            if let Some(name) = self.c_function_name(declarator) {
                return Some(name);
            }
            if let Some(name) = self.find_identifier_in_subtree(declarator) {
                return Some(name);
            }
        }

        for i in 0..node.child_count() {
            if let Some(child) = node.child(i as u32)
                && matches!(
                    child.kind(),
                    "identifier" | "field_identifier" | "type_identifier" | "property_identifier"
                )
            {
                return Some(&self.source[child.byte_range()]);
            }
        }
        None
    }

    /// Resolve the actual function name from a C/C++ declarator.
    ///
    /// Mirrors `merge_driver::items::c_function_name` (heddle#114
    /// commit `dc37af8`, Codex r5 P1 #2). A plain DFS over the
    /// declarator subtree returns the FIRST identifier-ish leaf — for
    /// a templated qualified name like `void Foo<U>::bar()` the
    /// scope's inner `type_identifier` ("Foo") wins, so every method
    /// on the same scope collapses to name="Foo". Instead, walk the
    /// declarator's `declarator` field, peel wrapper layers, and
    /// recurse into `qualified_identifier` / `template_function`'s
    /// `name` field so the scope's identifier never wins.
    ///
    /// Duplicated rather than lifted to a shared module: the function
    /// is short, and `parser_core` vs `merge_driver` are different
    /// concerns. Lift if a third caller appears.
    fn c_function_name(&self, function_declarator: Node<'_>) -> Option<&str> {
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
                    return Some(&self.source[current.byte_range()]);
                }
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

    fn find_identifier_in_subtree(&self, node: Node<'_>) -> Option<&str> {
        let mut stack = vec![node];
        while let Some(current) = stack.pop() {
            if matches!(
                current.kind(),
                "identifier" | "field_identifier" | "type_identifier" | "property_identifier"
            ) {
                return Some(&self.source[current.byte_range()]);
            }
            push_children_reverse(current, &mut stack);
        }
        None
    }

    fn get_function_signature(&self, node: &Node<'_>) -> String {
        if node.kind() == "variable_declarator" {
            return self.get_variable_function_signature(node);
        }

        let mut signature_parts = Vec::new();

        for i in 0..node.child_count() {
            if let Some(child) = node.child(i as u32) {
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
                    signature_parts.push(&self.source[child.byte_range()]);
                }
                if matches!(
                    kind,
                    "block" | "compound_statement" | "statement_block" | "suite"
                ) {
                    break;
                }
            }
        }

        signature_parts.join(" ")
    }

    fn get_variable_function_signature(&self, node: &Node<'_>) -> String {
        let Some(name) = node.child_by_field_name("name") else {
            return String::new();
        };
        let Some(value) = node.child_by_field_name("value") else {
            return self.source[name.byte_range()].to_string();
        };

        let mut signature_parts = vec![&self.source[name.byte_range()]];
        for i in 0..value.child_count() {
            if let Some(child) = value.child(i as u32) {
                if matches!(child.kind(), "formal_parameters" | "parameters") {
                    signature_parts.push(&self.source[child.byte_range()]);
                }
                if matches!(child.kind(), "statement_block" | "body") {
                    break;
                }
            }
        }
        signature_parts.join(" ")
    }

    fn extract_rust_imports(&self) -> Vec<Import> {
        let mut imports = Vec::new();
        let root = self.root_node();

        for i in 0..root.child_count() {
            if let Some(child) = root.child(i as u32) {
                if child.kind() == "use_declaration" {
                    let text = &self.source[child.byte_range()];
                    imports.push(Import {
                        raw: text.to_string(),
                        kind: ImportKind::Use,
                    });
                } else if child.kind() == "extern_crate_declaration" {
                    let text = &self.source[child.byte_range()];
                    imports.push(Import {
                        raw: text.to_string(),
                        kind: ImportKind::ExternCrate,
                    });
                }
            }
        }

        imports
    }

    fn extract_imports_by_kind(&self, kinds: &[&str], kind: ImportKind) -> Vec<Import> {
        let mut imports = Vec::new();
        let root = self.root_node();

        for i in 0..root.child_count() {
            if let Some(child) = root.child(i as u32)
                && kinds.contains(&child.kind())
            {
                let text = &self.source[child.byte_range()];
                imports.push(Import {
                    raw: text.to_string(),
                    kind: kind.clone(),
                });
            }
        }

        imports
    }
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
