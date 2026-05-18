// SPDX-License-Identifier: Apache-2.0
//! Tree-sitter AST traversal and per-language symbol extraction.
//!
//! Contains the core logic for walking a tree-sitter parse tree and
//! matching definition nodes (functions, structs, classes, methods, etc.)
//! against a target symbol name across all supported languages.

use crate::symbol_resolver::ResolvedSymbol;

/// Walk tree-sitter AST to find function/method/struct/enum definitions
/// matching a target name. Returns all matches.
///
/// Iterative DFS over a `Vec<(Node, Option<String>)>` stack — the
/// stack-overflow shape that motivated the iterative form in
/// `merge_driver::items::collect_items` (heddle#114 422031b) lives in
/// this walker too: a recursive walker recurses for every child of
/// every non-scope node, so a deeply-parseable input drives call depth
/// proportional to AST depth. The iterative form alone is the fix; no
/// depth cap, which would silently drop deep definitions.
pub(crate) fn find_definitions(
    node: &tree_sitter::Node,
    source: &[u8],
    target_name: &str,
) -> Vec<ResolvedSymbol> {
    let mut results = Vec::new();
    let mut stack: Vec<(tree_sitter::Node, Option<String>)> = vec![(*node, None)];

    while let Some((node, parent)) = stack.pop() {
        let current_parent = parent.as_deref();
        let kind = node.kind();
        // True when the kind's arm below handles its own child traversal
        // (scope-introducing nodes that re-parent their children). For
        // these we skip the default "push all children with same parent"
        // step at the bottom of the loop.
        let mut descended_with_new_parent = false;

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
                // Extract the type name from the impl block, then schedule
                // children with that name as their parent scope. Push in
                // reverse so popping yields the original child order — the
                // public API documents matches in source order.
                let impl_type_name = extract_impl_type_name(&node, source);
                let mut cursor = node.walk();
                let children: Vec<_> = node.children(&mut cursor).collect();
                for child in children.into_iter().rev() {
                    stack.push((child, impl_type_name.clone()));
                }
                descended_with_new_parent = true;
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

                // Schedule the class body with class name as parent scope.
                let mut cursor = node.walk();
                let children: Vec<_> = node.children(&mut cursor).collect();
                for child in children.into_iter().rev() {
                    stack.push((child, class_name.clone()));
                }
                descended_with_new_parent = true;
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
                        let receiver_type = extract_go_receiver_type(&node, source);
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

                let mut cursor = node.walk();
                let children: Vec<_> = node.children(&mut cursor).collect();
                for child in children.into_iter().rev() {
                    stack.push((child, class_name.clone()));
                }
                descended_with_new_parent = true;
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

        // Default walk for non-scope-introducing nodes: schedule every
        // child with the same parent scope. Skipped for scope-introducing
        // arms above, which already pushed their children with a new
        // parent.
        if !descended_with_new_parent {
            let mut cursor = node.walk();
            let children: Vec<_> = node.children(&mut cursor).collect();
            for child in children.into_iter().rev() {
                stack.push((child, parent.clone()));
            }
        }
    }
    results
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

// =====================================================================
// Codex r2 (heddle#68 cid 3255570487, heddle#120): find_definitions
// must not stack-overflow on deeply-nested trees. Mirrors the
// `collect_items` test added in heddle#114 commit 422031b.
// =====================================================================
#[cfg(all(test, feature = "lang-rust"))]
mod tests {
    use super::find_definitions;

    #[test]
    fn deeply_nested_rust_modules_does_not_stack_overflow() {
        // Build a Rust file with `depth` nested mod blocks holding one
        // fn at the centre. Parse it, then run find_definitions inside
        // a thread with a small stack so a recursive walker overflows
        // before reaching the leaf.
        let depth = 2000usize;
        let mut s = String::new();
        for i in 0..depth {
            s.push_str(&format!("mod m{i} {{\n"));
        }
        s.push_str("fn target() {}\n");
        for _ in 0..depth {
            s.push_str("}\n");
        }

        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .expect("set rust language");
        let tree = parser.parse(&s, None).expect("parse");
        let source = s.into_bytes();

        let handle = std::thread::Builder::new()
            .stack_size(128 * 1024)
            .spawn(move || find_definitions(&tree.root_node(), &source, "target"))
            .expect("spawn");
        let results = handle
            .join()
            .expect("find_definitions must not stack-overflow on deeply-nested input");
        assert!(
            results.iter().any(|r| r.name == "target"),
            "deep target fn must be returned, not silently dropped by a depth cap; got {results:?}"
        );
    }
}
