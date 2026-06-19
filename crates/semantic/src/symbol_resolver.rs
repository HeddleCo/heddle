// SPDX-License-Identifier: Apache-2.0
//! Tree-sitter based symbol resolution for source files.
//!
//! Resolves symbol names (like `Repository::open` or `cmd_context_get`)
//! to line ranges in source files by parsing the AST with tree-sitter.
//!
//! Lives in the `semantic` crate so anchor-travel code in `objects`-adjacent
//! modules can use it without a `repo` dependency. The `repo` crate
//! re-exports the public surface for backwards compatibility.

use std::path::Path;

use crate::{parser::Language, symbol_extraction::find_definitions};

/// Result of resolving a symbol to lines in a source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSymbol {
    /// The matched symbol name.
    pub name: String,
    /// 1-indexed start line (inclusive).
    pub start_line: u32,
    /// 1-indexed end line (inclusive).
    pub end_line: u32,
    /// Parent scope name, if any (e.g., the impl block or class name).
    pub parent_name: Option<String>,
}

/// Errors that can occur during symbol resolution.
#[derive(Debug, thiserror::Error)]
pub enum SymbolResolveError {
    #[error("unsupported file extension: {0}")]
    UnsupportedLanguage(String),

    #[error("failed to parse source file")]
    ParseFailed,

    #[error("symbol not found: {0}")]
    SymbolNotFound(String),
}

/// Coarse symbol classification used by the reading-order partition.
/// Mirrors the `state_review::SymbolKind` taxonomy without taking a
/// dependency on that crate — the consumer maps these tags to the
/// state-review enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefinitionKind {
    /// Type / struct definition.
    Type,
    /// Trait declaration (Rust).
    Trait,
    /// Class declaration (Python / JS / TS / Java / C++).
    Class,
    /// Interface declaration (TS / Java / Go).
    Interface,
    /// Type alias (`type Foo = ...`).
    TypeAlias,
    /// Enum definition.
    EnumDef,
    /// Constant declaration at module scope.
    ConstDecl,
    /// Module / namespace.
    Module,
    /// Function body — the consequence tier.
    Function,
    /// Anything we could parse but couldn't classify.
    Other,
}

/// One definition found in a source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    /// Symbol name as it appears in the AST. For methods this is the
    /// bare name; the parent scope is captured separately so callers
    /// can build a qualified `Parent::method` form when they want one.
    pub name: String,
    pub kind: DefinitionKind,
    /// 1-indexed start line, inclusive.
    pub start_line: u32,
    /// 1-indexed end line, inclusive.
    pub end_line: u32,
    /// Surrounding scope name (impl block, class, namespace, ...).
    pub parent_name: Option<String>,
}

/// Walk the source file and return one [`Definition`] per top-level or
/// nested definition node. Returns `Ok(vec![])` for files we can parse
/// but contain no definitions, `Err(UnsupportedLanguage)` for files
/// without a tree-sitter parser (binaries, unknown extensions),
/// `Err(ParseFailed)` if the parser errored. Callers should treat the
/// `UnsupportedLanguage` arm as "fall back to path-only projection".
pub fn extract_definitions(
    source: &[u8],
    path: &Path,
) -> Result<Vec<Definition>, SymbolResolveError> {
    let language = Language::from_path(path).parser_handle().ok_or_else(|| {
        SymbolResolveError::UnsupportedLanguage(
            path.extension()
                .map(|e| e.to_string_lossy().into_owned())
                .unwrap_or_else(|| "<none>".to_string()),
        )
    })?;

    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&language)
        .map_err(|_| SymbolResolveError::ParseFailed)?;

    let tree = parser
        .parse(source, None)
        .ok_or(SymbolResolveError::ParseFailed)?;
    if tree.root_node().has_error() {
        return Err(SymbolResolveError::ParseFailed);
    }

    let mut out = Vec::new();
    walk_definitions(&tree.root_node(), source, None, &mut out);
    Ok(out)
}

fn node_text<'a>(node: &tree_sitter::Node, source: &'a [u8]) -> &'a str {
    std::str::from_utf8(&source[node.byte_range()]).unwrap_or("")
}

fn push_named_definition(
    node: &tree_sitter::Node,
    source: &[u8],
    dk: DefinitionKind,
    parent: Option<&str>,
    out: &mut Vec<Definition>,
) {
    if let Some(name_node) = node.child_by_field_name("name") {
        let name = node_text(&name_node, source).to_string();
        if name.is_empty() {
            return;
        }
        out.push(Definition {
            name,
            kind: dk,
            start_line: node.start_position().row as u32 + 1,
            end_line: node.end_position().row as u32 + 1,
            parent_name: parent.map(String::from),
        });
    }
}

fn walk_definitions(
    node: &tree_sitter::Node,
    source: &[u8],
    current_parent: Option<&str>,
    out: &mut Vec<Definition>,
) {
    let kind = node.kind();

    match kind {
        // ── Rust ──────────────────────────────────────────────
        "function_item" => {
            push_named_definition(node, source, DefinitionKind::Function, current_parent, out)
        }
        "struct_item" => {
            push_named_definition(node, source, DefinitionKind::Type, current_parent, out)
        }
        "enum_item" => {
            push_named_definition(node, source, DefinitionKind::EnumDef, current_parent, out)
        }
        "trait_item" => {
            push_named_definition(node, source, DefinitionKind::Trait, current_parent, out)
        }
        "type_item" => {
            push_named_definition(node, source, DefinitionKind::TypeAlias, current_parent, out)
        }
        "const_item" | "static_item" => {
            push_named_definition(node, source, DefinitionKind::ConstDecl, current_parent, out)
        }
        "mod_item" => {
            push_named_definition(node, source, DefinitionKind::Module, current_parent, out)
        }
        "impl_item" => {
            // Walk children with the impl's type as parent so methods
            // get the qualified parent name.
            let parent_name = extract_rust_impl_type_name(node, source);
            let parent = parent_name.as_deref();
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                walk_definitions(&child, source, parent, out);
            }
            return;
        }

        // ── Python ───────────────────────────────────────────
        "function_definition" => {
            push_named_definition(node, source, DefinitionKind::Function, current_parent, out)
        }
        "class_definition" => {
            let class_name = node
                .child_by_field_name("name")
                .map(|n| node_text(&n, source).to_string());
            if let Some(ref name) = class_name
                && !name.is_empty()
            {
                out.push(Definition {
                    name: name.clone(),
                    kind: DefinitionKind::Class,
                    start_line: node.start_position().row as u32 + 1,
                    end_line: node.end_position().row as u32 + 1,
                    parent_name: current_parent.map(String::from),
                });
            }
            let parent = class_name.as_deref();
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                walk_definitions(&child, source, parent, out);
            }
            return;
        }

        // ── Go ───────────────────────────────────────────────
        "function_declaration" => {
            // Note: Go and JS/TS share this kind. The kind is `Function`
            // either way so we just emit it.
            push_named_definition(node, source, DefinitionKind::Function, current_parent, out)
        }
        "method_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                if !name.is_empty() {
                    let receiver = extract_go_receiver_type(node, source);
                    out.push(Definition {
                        name,
                        kind: DefinitionKind::Function,
                        start_line: node.start_position().row as u32 + 1,
                        end_line: node.end_position().row as u32 + 1,
                        parent_name: receiver.or_else(|| current_parent.map(String::from)),
                    });
                }
            }
        }
        "type_declaration" => {
            // Go: `type Foo struct { ... }` or `type Foo interface { ... }`.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "type_spec"
                    && let Some(name_node) = child.child_by_field_name("name")
                {
                    let name = node_text(&name_node, source).to_string();
                    if name.is_empty() {
                        continue;
                    }
                    let dk = match child.child_by_field_name("type").map(|t| t.kind()) {
                        Some("interface_type") => DefinitionKind::Interface,
                        Some("struct_type") => DefinitionKind::Type,
                        _ => DefinitionKind::TypeAlias,
                    };
                    out.push(Definition {
                        name,
                        kind: dk,
                        start_line: child.start_position().row as u32 + 1,
                        end_line: child.end_position().row as u32 + 1,
                        parent_name: current_parent.map(String::from),
                    });
                }
            }
        }

        // ── JavaScript / TypeScript ──────────────────────────
        "method_definition" => {
            push_named_definition(node, source, DefinitionKind::Function, current_parent, out)
        }
        "class_declaration" => {
            let class_name = node
                .child_by_field_name("name")
                .map(|n| node_text(&n, source).to_string());
            if let Some(ref name) = class_name
                && !name.is_empty()
            {
                out.push(Definition {
                    name: name.clone(),
                    kind: DefinitionKind::Class,
                    start_line: node.start_position().row as u32 + 1,
                    end_line: node.end_position().row as u32 + 1,
                    parent_name: current_parent.map(String::from),
                });
            }
            let parent = class_name.as_deref();
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                walk_definitions(&child, source, parent, out);
            }
            return;
        }
        "interface_declaration" => {
            push_named_definition(node, source, DefinitionKind::Interface, current_parent, out)
        }
        "type_alias_declaration" => {
            push_named_definition(node, source, DefinitionKind::TypeAlias, current_parent, out)
        }
        "enum_declaration" => {
            push_named_definition(node, source, DefinitionKind::EnumDef, current_parent, out)
        }
        "lexical_declaration" | "variable_declaration" => {
            // `const foo = () => { ... }` or `const foo = function() { ... }`
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "variable_declarator"
                    && let Some(name_node) = child.child_by_field_name("name")
                {
                    let name = node_text(&name_node, source).to_string();
                    if name.is_empty() {
                        continue;
                    }
                    if let Some(value_node) = child.child_by_field_name("value") {
                        let vkind = value_node.kind();
                        let dk = if vkind == "arrow_function"
                            || vkind == "function"
                            || vkind == "function_expression"
                        {
                            DefinitionKind::Function
                        } else {
                            DefinitionKind::ConstDecl
                        };
                        out.push(Definition {
                            name,
                            kind: dk,
                            start_line: node.start_position().row as u32 + 1,
                            end_line: node.end_position().row as u32 + 1,
                            parent_name: current_parent.map(String::from),
                        });
                    }
                }
            }
        }

        // ── C / C++ / Java ───────────────────────────────────
        "struct_specifier" | "class_specifier" => {
            push_named_definition(node, source, DefinitionKind::Class, current_parent, out)
        }
        "namespace_definition" => {
            push_named_definition(node, source, DefinitionKind::Module, current_parent, out)
        }
        "enum_specifier" => {
            push_named_definition(node, source, DefinitionKind::EnumDef, current_parent, out)
        }
        "constructor_declaration" => {
            push_named_definition(node, source, DefinitionKind::Function, current_parent, out)
        }

        _ => {}
    }

    // Default recursive descent for non-scope-introducing nodes.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_definitions(&child, source, current_parent, out);
    }
}

fn extract_rust_impl_type_name(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    let type_node = node.child_by_field_name("type")?;
    Some(extract_type_identifier(&type_node, source))
}

fn extract_type_identifier(node: &tree_sitter::Node, source: &[u8]) -> String {
    match node.kind() {
        "type_identifier" | "identifier" => node_text(node, source).to_string(),
        "generic_type" | "scoped_type_identifier" => {
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

fn extract_go_receiver_type(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    let params = node.child_by_field_name("receiver")?;
    let mut cursor = params.walk();
    for child in params.children(&mut cursor) {
        if child.kind() == "parameter_declaration"
            && let Some(type_node) = child.child_by_field_name("type")
        {
            let text = node_text(&type_node, source);
            return Some(text.trim_start_matches('*').to_string());
        }
    }
    None
}

/// Resolve a symbol name to a line range in source code.
///
/// Supports qualified names like `Repository::open` (splits on `::`).
/// For qualified names, the part before `::` is matched against the parent
/// scope (impl block, class, etc.) and the part after is the definition name.
///
/// Returns `(start_line, end_line)` as 1-indexed, inclusive line numbers.
pub fn resolve_symbol_lines(
    source: &[u8],
    path: &Path,
    symbol: &str,
) -> Result<(u32, u32), SymbolResolveError> {
    let language = Language::from_path(path).parser_handle().ok_or_else(|| {
        SymbolResolveError::UnsupportedLanguage(
            path.extension()
                .map(|e| e.to_string_lossy().into_owned())
                .unwrap_or_else(|| "<none>".to_string()),
        )
    })?;

    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&language)
        .map_err(|_| SymbolResolveError::ParseFailed)?;

    let tree = parser
        .parse(source, None)
        .ok_or(SymbolResolveError::ParseFailed)?;

    // Split qualified name: "Repository::open" -> parent="Repository", target="open"
    let (parent_filter, target_name) = if let Some(pos) = symbol.rfind("::") {
        (Some(&symbol[..pos]), &symbol[pos + 2..])
    } else {
        (None, symbol)
    };

    let definitions = find_definitions(&tree.root_node(), source, target_name);

    // If a parent filter is specified, prefer matches where the parent matches.
    let matched = if let Some(parent) = parent_filter {
        definitions
            .iter()
            .find(|d| {
                d.parent_name
                    .as_deref()
                    .map(|p| p == parent)
                    .unwrap_or(false)
            })
            .or_else(|| definitions.first())
    } else {
        definitions.first()
    };

    match matched {
        Some(sym) => Ok((sym.start_line, sym.end_line)),
        None => Err(SymbolResolveError::SymbolNotFound(symbol.to_string())),
    }
}

/// Resolve all definitions of a symbol name, returning all matches.
///
/// This is useful when a symbol appears in multiple contexts (e.g.,
/// multiple impl blocks). Returns an empty vec if no matches found.
pub fn resolve_all_symbols(
    source: &[u8],
    path: &Path,
    symbol: &str,
) -> Result<Vec<ResolvedSymbol>, SymbolResolveError> {
    let language = Language::from_path(path).parser_handle().ok_or_else(|| {
        SymbolResolveError::UnsupportedLanguage(
            path.extension()
                .map(|e| e.to_string_lossy().into_owned())
                .unwrap_or_else(|| "<none>".to_string()),
        )
    })?;

    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&language)
        .map_err(|_| SymbolResolveError::ParseFailed)?;

    let tree = parser
        .parse(source, None)
        .ok_or(SymbolResolveError::ParseFailed)?;

    let (parent_filter, target_name) = if let Some(pos) = symbol.rfind("::") {
        (Some(&symbol[..pos]), &symbol[pos + 2..])
    } else {
        (None, symbol)
    };

    let definitions = find_definitions(&tree.root_node(), source, target_name);

    if let Some(parent) = parent_filter {
        let filtered: Vec<_> = definitions
            .into_iter()
            .filter(|d| {
                d.parent_name
                    .as_deref()
                    .map(|p| p == parent)
                    .unwrap_or(false)
            })
            .collect();
        Ok(filtered)
    } else {
        Ok(definitions)
    }
}

/// Extract a range of lines from source bytes.
///
/// `start` and `end` are 1-indexed, inclusive. Returns the bytes
/// for those lines (including newlines).
pub fn extract_line_range(source: &[u8], start: u32, end: u32) -> Vec<u8> {
    let mut line: u32 = 1;
    let mut byte_start = 0;

    for (i, &b) in source.iter().enumerate() {
        if line == start {
            byte_start = i;
            break;
        }
        if b == b'\n' {
            line += 1;
        }
    }

    if line < start {
        return Vec::new();
    }

    for (i, &b) in source[byte_start..].iter().enumerate() {
        if b == b'\n' {
            line += 1;
            if line > end {
                return source[byte_start..byte_start + i + 1].to_vec();
            }
        }
    }

    source[byte_start..].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_rust_fn_main() {
        let source = br#"
fn helper() -> bool {
    true
}

fn main() {
    println!("hello");
    let x = 1;
}

fn after() {}
"#;
        let path = Path::new("test.rs");
        let (start, end) = resolve_symbol_lines(source, path, "main").unwrap();
        assert_eq!(start, 6);
        assert_eq!(end, 9);
    }

    #[test]
    fn resolve_rust_qualified_impl_method() {
        let source = br#"
struct Repository {
    path: String,
}

impl Repository {
    pub fn open(path: &str) -> Self {
        Repository {
            path: path.to_string(),
        }
    }

    pub fn close(&self) {}
}

impl Default for Repository {
    fn default() -> Self {
        Repository::open(".")
    }
}
"#;
        let path = Path::new("repo.rs");
        let (start, end) = resolve_symbol_lines(source, path, "Repository::open").unwrap();
        assert_eq!(start, 7);
        assert_eq!(end, 11);
    }

    #[test]
    fn resolve_rust_struct() {
        let source = br#"
pub struct Config {
    pub name: String,
    pub value: u32,
}
"#;
        let path = Path::new("config.rs");
        let (start, end) = resolve_symbol_lines(source, path, "Config").unwrap();
        assert_eq!(start, 2);
        assert_eq!(end, 5);
    }

    #[test]
    fn resolve_python_function() {
        let source = br#"
def helper():
    pass

def process_data(items):
    result = []
    for item in items:
        result.append(item * 2)
    return result

def cleanup():
    pass
"#;
        let path = Path::new("main.py");
        let (start, end) = resolve_symbol_lines(source, path, "process_data").unwrap();
        assert_eq!(start, 5);
        assert_eq!(end, 9);
    }

    #[test]
    fn resolve_python_class_method() {
        let source = br#"
class Repository:
    def __init__(self, path):
        self.path = path

    def open(self):
        return True
"#;
        let path = Path::new("repo.py");
        let (start, end) = resolve_symbol_lines(source, path, "Repository::open").unwrap();
        assert_eq!(start, 6);
        assert_eq!(end, 7);
    }

    #[test]
    #[cfg(feature = "lang-go")]
    fn resolve_go_function() {
        let source = br#"package main

func helper() bool {
    return true
}

func processData(items []int) []int {
    result := make([]int, 0)
    for _, item := range items {
        result = append(result, item*2)
    }
    return result
}
"#;
        let path = Path::new("main.go");
        let (start, end) = resolve_symbol_lines(source, path, "processData").unwrap();
        assert_eq!(start, 7);
        assert_eq!(end, 13);
    }

    #[test]
    fn resolve_symbol_not_found() {
        let source = br#"
fn main() {}
"#;
        let path = Path::new("test.rs");
        let err = resolve_symbol_lines(source, path, "nonexistent").unwrap_err();
        assert!(matches!(err, SymbolResolveError::SymbolNotFound(_)));
    }

    #[test]
    fn resolve_unsupported_extension() {
        let source = b"some content";
        let path = Path::new("test.xyz");
        let err = resolve_symbol_lines(source, path, "main").unwrap_err();
        assert!(matches!(err, SymbolResolveError::UnsupportedLanguage(_)));
    }

    #[test]
    fn extract_line_range_basic() {
        let source = b"line 1\nline 2\nline 3\nline 4\nline 5\n";
        let result = extract_line_range(source, 2, 4);
        assert_eq!(result, b"line 2\nline 3\nline 4\n");
    }

    #[test]
    fn extract_line_range_single_line() {
        let source = b"line 1\nline 2\nline 3\n";
        let result = extract_line_range(source, 2, 2);
        assert_eq!(result, b"line 2\n");
    }

    #[test]
    fn resolve_js_function_declaration() {
        let source = br#"
function helper() {
    return true;
}

function processData(items) {
    return items.map(x => x * 2);
}
"#;
        let path = Path::new("main.js");
        let (start, end) = resolve_symbol_lines(source, path, "processData").unwrap();
        assert_eq!(start, 6);
        assert_eq!(end, 8);
    }

    #[test]
    fn resolve_js_arrow_function_const() {
        let source = br#"
const helper = () => true;

const processData = (items) => {
    return items.map(x => x * 2);
};
"#;
        let path = Path::new("utils.js");
        let (start, end) = resolve_symbol_lines(source, path, "processData").unwrap();
        assert_eq!(start, 4);
        assert_eq!(end, 6);
    }

    /// Regression: real-world TS code often defines methods as arrow-
    /// function properties of an object literal (e.g. a `db` helper).
    /// The variable_declarator branch missed these — `pair` handling
    /// catches them. Without this, `heddle context set --scope symbol:insert`
    /// against `export const db = { insert: async () => {...} }` shipped
    /// `resolved_lines: None` and the chip never rendered.
    #[test]
    fn resolve_typescript_object_literal_property_arrow_function() {
        let source = br#"
export const db = {
    query: async (sql: string) => {
        return [];
    },
    insert: async (table: string, data: Record<string, any>) => {
        const keys = Object.keys(data);
        return keys;
    },
};
"#;
        let path = Path::new("db.ts");
        let (start, end) = resolve_symbol_lines(source, path, "insert").unwrap();
        // `insert` lives at lines 6–9 in the source above (1-indexed,
        // counting the leading newline as line 1).
        assert!((5..=7).contains(&start), "got start={start}");
        assert!(end > start && end <= 10, "got end={end}");
    }

    #[test]
    fn resolve_typescript_function() {
        let source = br#"
function helper(): boolean {
    return true;
}

function processData(items: number[]): number[] {
    return items.map(x => x * 2);
}
"#;
        let path = Path::new("main.ts");
        let (start, end) = resolve_symbol_lines(source, path, "processData").unwrap();
        assert_eq!(start, 6);
        assert_eq!(end, 8);
    }

    #[test]
    fn resolve_all_returns_multiple_matches() {
        let source = br#"
impl Foo {
    fn do_thing(&self) {}
}

impl Bar {
    fn do_thing(&self) {}
}
"#;
        let path = Path::new("test.rs");
        let results = resolve_all_symbols(source, path, "do_thing").unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].parent_name.as_deref(), Some("Foo"));
        assert_eq!(results[1].parent_name.as_deref(), Some("Bar"));
    }

    #[test]
    fn extract_definitions_reports_rust_taxonomy_parent_scopes_and_ranges() {
        let source = br#"const LIMIT: usize = 10;
pub mod outer {
    pub struct Widget {
        pub id: u64,
    }

    pub enum Mode {
        Fast,
        Slow,
    }

    pub trait Runner {
        fn run(&self);
    }

    pub type WidgetResult<T> = Result<T, Error>;

    impl Widget {
        pub fn build(id: u64) -> Self {
            Self { id }
        }
    }
}
"#;

        let defs = extract_definitions(source, Path::new("lib.rs")).unwrap();

        assert_definition(&defs, "LIMIT", DefinitionKind::ConstDecl, 1, 1, None);
        assert_definition(&defs, "outer", DefinitionKind::Module, 2, 23, None);
        assert_definition(&defs, "Widget", DefinitionKind::Type, 3, 5, None);
        assert_definition(&defs, "Mode", DefinitionKind::EnumDef, 7, 10, None);
        assert_definition(&defs, "Runner", DefinitionKind::Trait, 12, 14, None);
        assert_definition(
            &defs,
            "WidgetResult",
            DefinitionKind::TypeAlias,
            16,
            16,
            None,
        );
        assert_definition(
            &defs,
            "build",
            DefinitionKind::Function,
            19,
            21,
            Some("Widget"),
        );
    }

    #[test]
    fn extract_definitions_reports_typescript_taxonomy_parent_scopes_and_ranges() {
        let source = br#"interface Service {
    run(): void;
}

type Handler = (value: string) => void;

enum Status {
    Ready,
    Done,
}

class Controller {
    start(): void {
        handle("start");
    }
}

export const handle = (value: string): void => {
    console.log(value);
};

export const settings = { retry: 2 };
"#;

        let defs = extract_definitions(source, Path::new("controller.ts")).unwrap();

        assert_definition(&defs, "Service", DefinitionKind::Interface, 1, 3, None);
        assert_definition(&defs, "Handler", DefinitionKind::TypeAlias, 5, 5, None);
        assert_definition(&defs, "Status", DefinitionKind::EnumDef, 7, 10, None);
        assert_definition(&defs, "Controller", DefinitionKind::Class, 12, 16, None);
        assert_definition(
            &defs,
            "start",
            DefinitionKind::Function,
            13,
            15,
            Some("Controller"),
        );
        assert_definition(&defs, "handle", DefinitionKind::Function, 18, 20, None);
        assert_definition(&defs, "settings", DefinitionKind::ConstDecl, 22, 22, None);
    }

    #[test]
    fn extract_definitions_rejects_parse_error_trees() {
        let err =
            extract_definitions(b"fn broken( -> usize { 1 }", Path::new("broken.rs")).unwrap_err();

        assert!(matches!(err, SymbolResolveError::ParseFailed));
    }

    fn assert_definition(
        defs: &[Definition],
        name: &str,
        kind: DefinitionKind,
        start_line: u32,
        end_line: u32,
        parent_name: Option<&str>,
    ) {
        assert!(
            defs.iter().any(|def| {
                def.name == name
                    && def.kind == kind
                    && def.start_line == start_line
                    && def.end_line == end_line
                    && def.parent_name.as_deref() == parent_name
            }),
            "expected {name:?} {kind:?} lines {start_line}-{end_line} parent {parent_name:?}, got: {defs:?}"
        );
    }
}
