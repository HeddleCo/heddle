// SPDX-License-Identifier: Apache-2.0
//! Tree-sitter AST traversal and per-language symbol extraction.
//!
//! Contains the core logic for walking a tree-sitter parse tree and
//! matching definition nodes (functions, structs, classes, methods, etc.)
//! against a target symbol name across all supported languages.

use crate::symbol_resolver::ResolvedSymbol;

/// Walk tree-sitter AST to find function/method/struct/enum definitions
/// matching a target name. Returns all matches.
pub(crate) fn find_definitions(
    node: &tree_sitter::Node,
    source: &[u8],
    target_name: &str,
) -> Vec<ResolvedSymbol> {
    let mut results = Vec::new();
    find_definitions_recursive(node, source, target_name, None, &mut results);
    results
}

fn find_definitions_recursive(
    node: &tree_sitter::Node,
    source: &[u8],
    target_name: &str,
    current_parent: Option<&str>,
    results: &mut Vec<ResolvedSymbol>,
) {
    let kind = node.kind();

    match kind {
        // ── Rust ──────────────────────────────────────────────
        "function_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source);
                if name == target_name {
                    results.push(ResolvedSymbol {
                        name: name.to_string(),
                        start_line: node.start_position().row as u32 + 1,
                        end_line: node.end_position().row as u32 + 1,
                        parent_name: current_parent.map(String::from),
                    });
                }
            }
        }
        "impl_item" => {
            // Extract the type name from the impl block.
            let impl_type_name = extract_impl_type_name(node, source);
            let parent = impl_type_name.as_deref();

            // Walk children of impl block with the parent set.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                find_definitions_recursive(&child, source, target_name, parent, results);
            }
            return; // Already recursed into children.
        }
        "struct_item" | "enum_item" | "type_item" | "trait_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source);
                if name == target_name {
                    results.push(ResolvedSymbol {
                        name: name.to_string(),
                        start_line: node.start_position().row as u32 + 1,
                        end_line: node.end_position().row as u32 + 1,
                        parent_name: current_parent.map(String::from),
                    });
                }
            }
        }

        // ── Python ───────────────────────────────────────────
        "function_definition" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source);
                if name == target_name {
                    results.push(ResolvedSymbol {
                        name: name.to_string(),
                        start_line: node.start_position().row as u32 + 1,
                        end_line: node.end_position().row as u32 + 1,
                        parent_name: current_parent.map(String::from),
                    });
                }
            }
        }
        "class_definition" => {
            let class_name = node
                .child_by_field_name("name")
                .map(|n| node_text(&n, source).to_string());

            // The class itself is a definition.
            if let Some(ref name) = class_name
                && name == target_name
            {
                results.push(ResolvedSymbol {
                    name: name.clone(),
                    start_line: node.start_position().row as u32 + 1,
                    end_line: node.end_position().row as u32 + 1,
                    parent_name: current_parent.map(String::from),
                });
            }

            // Recurse into class body with class name as parent.
            let parent = class_name.as_deref();
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                find_definitions_recursive(&child, source, target_name, parent, results);
            }
            return;
        }

        // ── Go ───────────────────────────────────────────────
        "function_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source);
                if name == target_name {
                    results.push(ResolvedSymbol {
                        name: name.to_string(),
                        start_line: node.start_position().row as u32 + 1,
                        end_line: node.end_position().row as u32 + 1,
                        parent_name: current_parent.map(String::from),
                    });
                }
            }
        }
        "method_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source);
                if name == target_name {
                    // Try to extract the receiver type as the parent.
                    let receiver_type = extract_go_receiver_type(node, source);
                    results.push(ResolvedSymbol {
                        name: name.to_string(),
                        start_line: node.start_position().row as u32 + 1,
                        end_line: node.end_position().row as u32 + 1,
                        parent_name: receiver_type.or_else(|| current_parent.map(String::from)),
                    });
                }
            }
        }
        "type_declaration" => {
            // Go type declarations contain type_spec children.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "type_spec"
                    && let Some(name_node) = child.child_by_field_name("name")
                {
                    let name = node_text(&name_node, source);
                    if name == target_name {
                        results.push(ResolvedSymbol {
                            name: name.to_string(),
                            start_line: child.start_position().row as u32 + 1,
                            end_line: child.end_position().row as u32 + 1,
                            parent_name: current_parent.map(String::from),
                        });
                    }
                }
            }
        }

        // ── JavaScript / TypeScript ──────────────────────────
        // Note: "function_declaration" is shared with Go and handled above.
        "method_definition" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source);
                if name == target_name {
                    results.push(ResolvedSymbol {
                        name: name.to_string(),
                        start_line: node.start_position().row as u32 + 1,
                        end_line: node.end_position().row as u32 + 1,
                        parent_name: current_parent.map(String::from),
                    });
                }
            }
        }
        "class_declaration" => {
            let class_name = node
                .child_by_field_name("name")
                .map(|n| node_text(&n, source).to_string());

            if let Some(ref name) = class_name
                && name == target_name
            {
                results.push(ResolvedSymbol {
                    name: name.clone(),
                    start_line: node.start_position().row as u32 + 1,
                    end_line: node.end_position().row as u32 + 1,
                    parent_name: current_parent.map(String::from),
                });
            }

            let parent = class_name.as_deref();
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                find_definitions_recursive(&child, source, target_name, parent, results);
            }
            return;
        }
        "lexical_declaration" | "variable_declaration" => {
            // Handle `const foo = () => { ... }` or `const foo = function() { ... }`
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "variable_declarator"
                    && let Some(name_node) = child.child_by_field_name("name")
                {
                    let name = node_text(&name_node, source);
                    if name == target_name {
                        // Check if the value is a function expression or arrow function.
                        if let Some(value_node) = child.child_by_field_name("value") {
                            let vkind = value_node.kind();
                            if vkind == "arrow_function"
                                || vkind == "function"
                                || vkind == "function_expression"
                            {
                                results.push(ResolvedSymbol {
                                    name: name.to_string(),
                                    start_line: node.start_position().row as u32 + 1,
                                    end_line: node.end_position().row as u32 + 1,
                                    parent_name: current_parent.map(String::from),
                                });
                            }
                        }
                    }
                }
            }
        }
        // Object literal property whose value is a function/arrow expression,
        // e.g. `export const db = { insert: async (...) => {...} };` — a
        // common TS/JS pattern that the variable_declarator branch above
        // misses because the function lives one level deeper.
        "pair" => {
            if let Some(key_node) = node.child_by_field_name("key") {
                let key = node_text(&key_node, source);
                if key == target_name
                    && let Some(value_node) = node.child_by_field_name("value")
                {
                    let vkind = value_node.kind();
                    if vkind == "arrow_function"
                        || vkind == "function"
                        || vkind == "function_expression"
                    {
                        results.push(ResolvedSymbol {
                            name: key.to_string(),
                            start_line: node.start_position().row as u32 + 1,
                            end_line: node.end_position().row as u32 + 1,
                            parent_name: current_parent.map(String::from),
                        });
                    }
                }
            }
        }

        _ => {}
    }

    // Default recursive walk for non-scope-introducing nodes.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        find_definitions_recursive(&child, source, target_name, current_parent, results);
    }
}

/// Extract the type name from a Rust `impl` block.
/// Handles `impl Foo { ... }` and `impl Trait for Foo { ... }`.
fn extract_impl_type_name(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    // In tree-sitter-rust, the impl_item has a "type" field for the self type.
    if let Some(type_node) = node.child_by_field_name("type") {
        // The type node might be a type_identifier or a generic_type, etc.
        // For simple cases, just grab the first identifier-like text.
        return Some(extract_type_identifier(&type_node, source));
    }
    None
}

/// Extract a simple type identifier from a type node.
/// For `Foo<Bar>`, returns "Foo". For plain `Foo`, returns "Foo".
fn extract_type_identifier(node: &tree_sitter::Node, source: &[u8]) -> String {
    match node.kind() {
        "type_identifier" | "identifier" => node_text(node, source).to_string(),
        "generic_type" | "scoped_type_identifier" => {
            // Get the first child which should be the base type name.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "type_identifier" || child.kind() == "identifier" {
                    return node_text(&child, source).to_string();
                }
            }
            node_text(node, source).to_string()
        }
        _ => node_text(node, source).to_string(),
    }
}

/// Extract the receiver type from a Go method declaration.
/// For `func (r *Repo) Open() { ... }`, returns `Some("Repo")`.
fn extract_go_receiver_type(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    let params = node.child_by_field_name("receiver")?;
    // The receiver is a parameter_list containing a parameter_declaration.
    let mut cursor = params.walk();
    for child in params.children(&mut cursor) {
        if child.kind() == "parameter_declaration" {
            // The type might be a pointer_type or a type_identifier.
            if let Some(type_node) = child.child_by_field_name("type") {
                let text = node_text(&type_node, source);
                // Strip leading * for pointer receivers.
                let trimmed = text.trim_start_matches('*');
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

/// Get the UTF-8 text of a tree-sitter node.
fn node_text<'a>(node: &tree_sitter::Node, source: &'a [u8]) -> &'a str {
    let start = node.start_byte();
    let end = node.end_byte();
    std::str::from_utf8(&source[start..end]).unwrap_or("")
}