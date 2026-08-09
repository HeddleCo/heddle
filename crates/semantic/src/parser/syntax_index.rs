// SPDX-License-Identifier: Apache-2.0
//! Compact syntax index derived from a tree-sitter parse tree.

use std::ops::Range;

use objects::object::{
    ByteSpan, ImportBinding, ImportEntry, ImportKindTag, OccurrenceEntry, OccurrenceRole,
    ScopeEntry, ScopeKind, SymbolNamespace,
};
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
    semantic_scopes: Vec<ScopeEntry>,
    semantic_imports: Vec<ImportEntry>,
    occurrences: Vec<OccurrenceEntry>,
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
            semantic_scopes: Vec::new(),
            semantic_imports: Vec::new(),
            occurrences: Vec::new(),
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
                // Zig has no top-level import statement node — `@import` is a
                // builtin call bound in a `variable_declaration`, so there is
                // no dedicated import kind to collect here.
                Language::C | Language::Cpp | Language::Zig | Language::Unknown => {}
            }
        }

        let (semantic_scopes, semantic_imports, occurrences) =
            build_source_facts(language, source, root);
        index.semantic_scopes = semantic_scopes;
        index.semantic_imports = semantic_imports;
        index.occurrences = occurrences;

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

    pub(crate) fn semantic_scopes(&self) -> &[ScopeEntry] {
        &self.semantic_scopes
    }

    pub(crate) fn semantic_imports(&self) -> &[ImportEntry] {
        &self.semantic_imports
    }

    pub(crate) fn occurrences(&self) -> &[OccurrenceEntry] {
        &self.occurrences
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

fn build_source_facts(
    language: Language,
    source: &str,
    root: Node<'_>,
) -> (Vec<ScopeEntry>, Vec<ImportEntry>, Vec<OccurrenceEntry>) {
    let mut scopes = vec![ScopeEntry {
        local_id: 0,
        parent: None,
        kind: ScopeKind::Module,
        span: byte_span(root),
    }];
    let mut imports = Vec::new();
    let mut occurrences = Vec::new();
    let mut stack = Vec::new();
    push_children_with_scope_reverse(root, 0, &mut stack);

    while let Some((node, parent_scope)) = stack.pop() {
        let scope = if let Some(kind) = scope_kind(node.kind()) {
            let local_id = scopes.len() as u32;
            scopes.push(ScopeEntry {
                local_id,
                parent: Some(parent_scope),
                kind,
                span: byte_span(node),
            });
            local_id
        } else {
            parent_scope
        };

        if is_import_node(node, language) {
            imports.extend(extract_import_entries(node, language, source, scope));
            continue;
        }
        if let Some(import) = extract_dynamic_import(node, language, source, scope) {
            imports.push(import);
            continue;
        }

        if is_path_node(node.kind()) {
            if let Some(occurrence) = path_occurrence(node, source, scope) {
                occurrences.push(occurrence);
            }
            continue;
        }
        if is_identifier_node(node.kind()) {
            occurrences.push(identifier_occurrence(node, source, scope, &scopes));
            continue;
        }

        push_children_with_scope_reverse(node, scope, &mut stack);
    }

    imports.sort_by_key(|import| import.span.start);
    occurrences.sort_by_key(|occurrence| occurrence.span.start);
    for (local_id, occurrence) in occurrences.iter_mut().enumerate() {
        occurrence.local_id = local_id as u32;
    }
    (scopes, imports, occurrences)
}

fn scope_kind(kind: &str) -> Option<ScopeKind> {
    if is_function_node_kind(kind) {
        return Some(ScopeKind::Function);
    }
    if matches!(
        kind,
        "struct_item"
            | "enum_item"
            | "trait_item"
            | "impl_item"
            | "class_definition"
            | "class_declaration"
            | "interface_declaration"
            | "type_declaration"
            | "struct_declaration"
            | "enum_declaration"
    ) {
        return Some(ScopeKind::Type);
    }
    if matches!(kind, "mod_item" | "namespace_definition" | "module") {
        return Some(ScopeKind::Module);
    }
    if matches!(
        kind,
        "block" | "compound_statement" | "statement_block" | "suite"
    ) {
        return Some(ScopeKind::Block);
    }
    None
}

fn is_function_node_kind(kind: &str) -> bool {
    matches!(
        kind,
        "function_item"
            | "function_definition"
            | "function_declaration"
            | "method_definition"
            | "method_declaration"
            | "constructor_declaration"
            | "generator_function_declaration"
            | "closure_expression"
            | "arrow_function"
            | "function_expression"
            | "generator_function"
    )
}

fn is_import_node(node: Node<'_>, language: Language) -> bool {
    match language {
        Language::Rust => matches!(node.kind(), "use_declaration" | "extern_crate_declaration"),
        Language::Python => matches!(node.kind(), "import_statement" | "import_from_statement"),
        Language::JavaScript | Language::TypeScript => {
            node.kind() == "import_statement"
                || (node.kind() == "export_statement"
                    && node.child_by_field_name("source").is_some())
        }
        Language::Go => node.kind() == "import_spec",
        Language::Java => node.kind() == "import_declaration",
        Language::C | Language::Cpp => node.kind() == "preproc_include",
        Language::Zig | Language::Unknown => false,
    }
}

fn extract_import_entries(
    node: Node<'_>,
    language: Language,
    source: &str,
    scope: u32,
) -> Vec<ImportEntry> {
    match language {
        Language::Rust => vec![rust_import(node, source, scope)],
        Language::Python => python_imports(node, source, scope),
        Language::JavaScript | Language::TypeScript => {
            vec![javascript_import(node, language, source, scope)]
        }
        Language::Go => vec![go_import(node, source, scope)],
        Language::Java => vec![java_import(node, source, scope)],
        Language::C | Language::Cpp => vec![c_import(node, source, scope)],
        Language::Zig | Language::Unknown => Vec::new(),
    }
}

fn rust_import(node: Node<'_>, source: &str, scope: u32) -> ImportEntry {
    if node.kind() == "extern_crate_declaration" {
        let imported = node
            .child_by_field_name("name")
            .map(|name| node_text(name, source).to_string())
            .unwrap_or_default();
        let local = node
            .child_by_field_name("alias")
            .map(|alias| node_text(alias, source).to_string())
            .unwrap_or_else(|| imported.clone());
        return ImportEntry {
            kind: ImportKindTag::Use,
            module_specifier: imported.clone(),
            bindings: vec![ImportBinding {
                imported,
                local,
                namespace: SymbolNamespace::Both,
            }],
            scope,
            span: byte_span(node),
        };
    }

    let body = node
        .child_by_field_name("argument")
        .map(|argument| node_text(argument, source))
        .unwrap_or_default();
    let (module_specifier, binding_texts) = if let Some(open) = body.find('{') {
        let prefix = body[..open].trim().trim_end_matches("::").to_string();
        let close = body.rfind('}').unwrap_or(body.len());
        (prefix, split_top_level(&body[open + 1..close]))
    } else {
        let path = body.split(" as ").next().unwrap_or(body).trim();
        let module = path
            .rsplit_once("::")
            .map(|(module, _)| module)
            .unwrap_or(path)
            .to_string();
        (module, vec![body])
    };
    let bindings = binding_texts
        .into_iter()
        .filter_map(|binding| rust_binding(binding.trim()))
        .collect();
    ImportEntry {
        kind: if named_child_of_kind(node, "visibility_modifier").is_some() {
            ImportKindTag::Reexport
        } else {
            ImportKindTag::Use
        },
        module_specifier,
        bindings,
        scope,
        span: byte_span(node),
    }
}

fn rust_binding(value: &str) -> Option<ImportBinding> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if value == "*" || value.ends_with("::*") {
        return Some(ImportBinding {
            imported: "*".to_string(),
            local: "*".to_string(),
            namespace: SymbolNamespace::Both,
        });
    }
    let (path, alias) = value
        .split_once(" as ")
        .map_or((value, None), |(path, alias)| (path, Some(alias.trim())));
    let imported = path.rsplit("::").next().unwrap_or(path).trim();
    let local = alias.unwrap_or(imported);
    Some(ImportBinding {
        imported: imported.to_string(),
        local: local.to_string(),
        namespace: SymbolNamespace::Both,
    })
}

fn javascript_import(node: Node<'_>, language: Language, source: &str, scope: u32) -> ImportEntry {
    let raw = node_text(node, source);
    let namespace = if language == Language::TypeScript
        && (raw.trim_start().starts_with("import type")
            || raw.trim_start().starts_with("export type"))
    {
        SymbolNamespace::Type
    } else {
        SymbolNamespace::Value
    };
    let module_specifier = node
        .child_by_field_name("source")
        .map(|source_node| unquote(node_text(source_node, source)))
        .unwrap_or_default();
    let mut bindings = Vec::new();
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        match current.kind() {
            "import_clause" => {
                let mut cursor = current.walk();
                for child in current.named_children(&mut cursor) {
                    if child.kind() == "identifier" {
                        bindings.push(ImportBinding {
                            imported: "default".to_string(),
                            local: node_text(child, source).to_string(),
                            namespace,
                        });
                    }
                }
            }
            "namespace_import" | "namespace_export" => {
                if let Some(local) = first_identifier(current, source) {
                    bindings.push(ImportBinding {
                        imported: "*".to_string(),
                        local,
                        namespace,
                    });
                }
            }
            "import_specifier" | "export_specifier" => {
                let imported = current
                    .child_by_field_name("name")
                    .map(|name| unquote(node_text(name, source)))
                    .or_else(|| first_identifier(current, source))
                    .unwrap_or_default();
                let local = current
                    .child_by_field_name("alias")
                    .map(|alias| unquote(node_text(alias, source)))
                    .unwrap_or_else(|| imported.clone());
                bindings.push(ImportBinding {
                    imported,
                    local,
                    namespace,
                });
            }
            _ => {}
        }
        push_children_reverse(current, &mut stack);
    }
    ImportEntry {
        kind: if node.kind() == "export_statement" {
            ImportKindTag::Reexport
        } else {
            ImportKindTag::Import
        },
        module_specifier,
        bindings,
        scope,
        span: byte_span(node),
    }
}

fn python_imports(node: Node<'_>, source: &str, scope: u32) -> Vec<ImportEntry> {
    if node.kind() == "import_from_statement" {
        let module_specifier = node
            .child_by_field_name("module_name")
            .map(|module| node_text(module, source).to_string())
            .unwrap_or_default();
        let mut bindings = Vec::new();
        let module_id = node
            .child_by_field_name("module_name")
            .map(|module| module.id());
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if Some(child.id()) == module_id {
                continue;
            }
            if child.kind() == "aliased_import" || child.kind() == "dotted_name" {
                bindings.push(python_binding(child, source));
            } else if child.kind() == "wildcard_import" {
                bindings.push(ImportBinding {
                    imported: "*".to_string(),
                    local: "*".to_string(),
                    namespace: SymbolNamespace::Both,
                });
            }
        }
        return vec![ImportEntry {
            kind: ImportKindTag::Import,
            module_specifier,
            bindings,
            scope,
            span: byte_span(node),
        }];
    }

    let mut entries = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if matches!(child.kind(), "aliased_import" | "dotted_name") {
            let binding = python_binding(child, source);
            let module_specifier = child
                .child_by_field_name("name")
                .map(|name| node_text(name, source).to_string())
                .unwrap_or_else(|| {
                    node_text(child, source)
                        .split(" as ")
                        .next()
                        .unwrap()
                        .into()
                });
            entries.push(ImportEntry {
                kind: ImportKindTag::Import,
                module_specifier,
                bindings: vec![binding],
                scope,
                span: byte_span(child),
            });
        }
    }
    entries
}

fn python_binding(node: Node<'_>, source: &str) -> ImportBinding {
    let imported = node
        .child_by_field_name("name")
        .map(|name| node_text(name, source).to_string())
        .unwrap_or_else(|| node_text(node, source).split(" as ").next().unwrap().into());
    let local = node
        .child_by_field_name("alias")
        .map(|alias| node_text(alias, source).to_string())
        .unwrap_or_else(|| imported.rsplit('.').next().unwrap_or(&imported).to_string());
    ImportBinding {
        imported,
        local,
        namespace: SymbolNamespace::Both,
    }
}

fn go_import(node: Node<'_>, source: &str, scope: u32) -> ImportEntry {
    let module_specifier = node
        .child_by_field_name("path")
        .map(|path| unquote(node_text(path, source)))
        .unwrap_or_default();
    let imported = module_specifier
        .rsplit('/')
        .next()
        .unwrap_or(&module_specifier)
        .to_string();
    let local = node
        .child_by_field_name("name")
        .map(|name| node_text(name, source).to_string())
        .unwrap_or_else(|| imported.clone());
    ImportEntry {
        kind: ImportKindTag::Import,
        module_specifier,
        bindings: vec![ImportBinding {
            imported,
            local,
            namespace: SymbolNamespace::Both,
        }],
        scope,
        span: byte_span(node),
    }
}

fn java_import(node: Node<'_>, source: &str, scope: u32) -> ImportEntry {
    let raw = node_text(node, source);
    let module_specifier = raw
        .trim()
        .trim_start_matches("import ")
        .trim_start_matches("static ")
        .trim_end_matches(';')
        .trim()
        .to_string();
    let imported = module_specifier
        .rsplit('.')
        .next()
        .unwrap_or(&module_specifier)
        .to_string();
    ImportEntry {
        kind: ImportKindTag::Import,
        module_specifier,
        bindings: vec![ImportBinding {
            local: imported.clone(),
            imported,
            namespace: SymbolNamespace::Both,
        }],
        scope,
        span: byte_span(node),
    }
}

fn c_import(node: Node<'_>, source: &str, scope: u32) -> ImportEntry {
    let raw = node_text(node, source);
    let module_specifier = raw
        .trim()
        .trim_start_matches("#include")
        .trim()
        .trim_matches(['<', '>', '"'])
        .to_string();
    ImportEntry {
        kind: ImportKindTag::Import,
        module_specifier,
        bindings: Vec::new(),
        scope,
        span: byte_span(node),
    }
}

fn extract_dynamic_import(
    node: Node<'_>,
    language: Language,
    source: &str,
    scope: u32,
) -> Option<ImportEntry> {
    let is_dynamic_import = match language {
        Language::JavaScript | Language::TypeScript if node.kind() == "call_expression" => node
            .child_by_field_name("function")
            .is_some_and(|function| node_text(function, source) == "import"),
        Language::Zig if node.kind() == "builtin_function" => {
            named_child_of_kind(node, "builtin_identifier")
                .is_some_and(|function| node_text(function, source) == "@import")
        }
        _ => false,
    };
    if !is_dynamic_import {
        return None;
    }
    let string = first_node_of_kind(node, &["string", "string_literal"])?;
    Some(ImportEntry {
        kind: ImportKindTag::Dynamic,
        module_specifier: unquote(node_text(string, source)),
        bindings: Vec::new(),
        scope,
        span: byte_span(node),
    })
}

fn path_occurrence(node: Node<'_>, source: &str, scope: u32) -> Option<OccurrenceEntry> {
    let mut segments = Vec::new();
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if is_identifier_node(current.kind())
            || matches!(current.kind(), "self" | "super" | "crate")
        {
            segments.push((current.start_byte(), node_text(current, source).to_string()));
            continue;
        }
        push_children_reverse(current, &mut stack);
    }
    segments.sort_by_key(|(start, _)| *start);
    segments.dedup_by_key(|(start, _)| *start);
    let (_, name) = segments.pop()?;
    let qualifier = segments.into_iter().map(|(_, name)| name).collect();
    let namespace = if node.kind().contains("type") {
        SymbolNamespace::Type
    } else {
        SymbolNamespace::Value
    };
    Some(OccurrenceEntry {
        local_id: 0,
        role: occurrence_role(node, namespace),
        name,
        qualifier,
        namespace,
        scope,
        span: byte_span(node),
    })
}

fn identifier_occurrence(
    node: Node<'_>,
    source: &str,
    scope: u32,
    scopes: &[ScopeEntry],
) -> OccurrenceEntry {
    let namespace = identifier_namespace(node);
    let role = occurrence_role(node, namespace);
    let definition_in_own_scope = role == OccurrenceRole::Definition
        && node
            .parent()
            .is_some_and(|parent| scope_kind(parent.kind()).is_some());
    OccurrenceEntry {
        local_id: 0,
        role,
        name: node_text(node, source).to_string(),
        qualifier: Vec::new(),
        namespace,
        scope: if definition_in_own_scope {
            scopes[scope as usize].parent.unwrap_or(scope)
        } else {
            scope
        },
        span: byte_span(node),
    }
}

fn occurrence_role(node: Node<'_>, namespace: SymbolNamespace) -> OccurrenceRole {
    if is_definition_name(node) {
        OccurrenceRole::Definition
    } else if is_call_target(node) {
        OccurrenceRole::Call
    } else if namespace == SymbolNamespace::Type {
        OccurrenceRole::TypeReference
    } else {
        OccurrenceRole::Reference
    }
}

fn is_definition_name(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    let is_name_field = parent
        .child_by_field_name("name")
        .is_some_and(|name| name.id() == node.id());
    is_name_field
        && matches!(
            parent.kind(),
            "function_item"
                | "function_definition"
                | "function_declaration"
                | "method_definition"
                | "method_declaration"
                | "constructor_declaration"
                | "class_definition"
                | "class_declaration"
                | "interface_declaration"
                | "struct_item"
                | "struct_declaration"
                | "enum_item"
                | "enum_declaration"
                | "trait_item"
                | "type_item"
                | "type_alias_declaration"
                | "mod_item"
                | "const_item"
                | "static_item"
                | "variable_declarator"
                | "parameter"
                | "required_parameter"
                | "optional_parameter"
                | "formal_parameter"
                | "field_declaration"
        )
}

fn is_call_target(mut node: Node<'_>) -> bool {
    while let Some(parent) = node.parent() {
        if is_path_node(parent.kind()) {
            node = parent;
            continue;
        }
        if parent.kind() == "call_expression" {
            return parent
                .child_by_field_name("function")
                .is_some_and(|function| function.id() == node.id());
        }
        return false;
    }
    false
}

fn identifier_namespace(node: Node<'_>) -> SymbolNamespace {
    if node.kind().contains("type") {
        return SymbolNamespace::Type;
    }
    let mut current = node;
    for _ in 0..4 {
        let Some(parent) = current.parent() else {
            break;
        };
        if matches!(
            parent.kind(),
            "type_annotation"
                | "generic_type"
                | "type_arguments"
                | "type_parameters"
                | "return_type"
                | "trait_bounds"
        ) {
            return SymbolNamespace::Type;
        }
        current = parent;
    }
    SymbolNamespace::Value
}

fn is_identifier_node(kind: &str) -> bool {
    matches!(
        kind,
        "identifier"
            | "type_identifier"
            | "field_identifier"
            | "property_identifier"
            | "package_identifier"
            | "namespace_identifier"
    )
}

fn is_path_node(kind: &str) -> bool {
    matches!(
        kind,
        "scoped_identifier"
            | "scoped_type_identifier"
            | "qualified_identifier"
            | "field_expression"
            | "member_expression"
            | "attribute"
            | "selector_expression"
            | "field_access"
    )
}

fn split_top_level(value: &str) -> Vec<&str> {
    let mut depth = 0u32;
    let mut start = 0usize;
    let mut parts = Vec::new();
    for (index, byte) in value.bytes().enumerate() {
        match byte {
            b'{' | b'(' | b'[' => depth += 1,
            b'}' | b')' | b']' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                parts.push(&value[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&value[start..]);
    parts
}

fn first_identifier(node: Node<'_>, source: &str) -> Option<String> {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if is_identifier_node(current.kind()) {
            return Some(node_text(current, source).to_string());
        }
        push_children_reverse(current, &mut stack);
    }
    None
}

fn first_node_of_kind<'tree>(node: Node<'tree>, kinds: &[&str]) -> Option<Node<'tree>> {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if kinds.contains(&current.kind()) {
            return Some(current);
        }
        push_children_reverse(current, &mut stack);
    }
    None
}

fn named_child_of_kind<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| child.kind() == kind)
}

fn node_text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    &source[node.byte_range()]
}

fn unquote(value: &str) -> String {
    value
        .strip_prefix(['\'', '"', '`'])
        .and_then(|value| value.strip_suffix(['\'', '"', '`']))
        .unwrap_or(value)
        .to_string()
}

fn byte_span(node: Node<'_>) -> ByteSpan {
    ByteSpan::new(node.start_byte() as u32, node.end_byte() as u32)
}

fn push_children_with_scope_reverse<'tree>(
    node: Node<'tree>,
    scope: u32,
    stack: &mut Vec<(Node<'tree>, u32)>,
) {
    let child_count = node.child_count();
    for index in (0..child_count).rev() {
        if let Some(child) = node.child(index as u32) {
            stack.push((child, scope));
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
        Language::Zig => kind == "function_declaration",
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
