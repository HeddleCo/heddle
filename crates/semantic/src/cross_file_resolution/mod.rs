// SPDX-License-Identifier: Apache-2.0
//! Deterministic repository-level binding over persisted per-file facts.

mod rust;
mod scope;
mod typescript;

use std::collections::{BTreeMap, BTreeSet};

use objects::object::{
    ContentHash, ImportBinding, ImportEntry, OccurrenceEntry, OccurrenceRole, ResolvedSemanticEdge,
    SemanticEdgeKind, SemanticFileNode, SymbolKindTag, SymbolNamespace,
};

/// Bump whenever binding policy changes so persisted edges rebuild cleanly.
pub const RESOLVER_VERSION: u32 = 1;

/// One content-addressed semantic file placed at a repository path.
#[derive(Clone, Debug)]
pub struct RepositorySemanticFile {
    pub node_hash: ContentHash,
    pub node: SemanticFileNode,
}

/// Resolution output for one source file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileResolution {
    /// Repository files named by resolvable imports from this file.
    pub dependencies: BTreeSet<String>,
    /// Successfully bound occurrences. Unresolved occurrences are absent.
    pub edges: Vec<ResolvedSemanticEdge>,
}

/// Resolve Rust and TypeScript/JavaScript occurrences across repository files.
///
/// Binding is deliberately conservative: ambiguous definitions and imports
/// remain unresolved rather than selecting a target by iteration order.
pub fn resolve_repository(
    files: &BTreeMap<String, RepositorySemanticFile>,
) -> BTreeMap<String, FileResolution> {
    files
        .iter()
        .map(|(path, file)| {
            let mut dependencies = dependencies_for(path, &file.node, files);
            let mut edges = file
                .node
                .occurrences
                .iter()
                .filter(|occurrence| occurrence.role != OccurrenceRole::Definition)
                .filter_map(|occurrence| resolve_occurrence(path, file, occurrence, files))
                .collect::<Vec<_>>();
            edges.sort();
            edges.dedup();
            dependencies.extend(
                edges
                    .iter()
                    .filter(|edge| edge.target_path != *path)
                    .map(|edge| edge.target_path.clone()),
            );
            (
                path.clone(),
                FileResolution {
                    dependencies,
                    edges,
                },
            )
        })
        .collect()
}

fn dependencies_for(
    source_path: &str,
    source: &SemanticFileNode,
    files: &BTreeMap<String, RepositorySemanticFile>,
) -> BTreeSet<String> {
    source
        .imports
        .iter()
        .filter_map(|import| resolve_module(source_path, source, &import.module_specifier, files))
        .collect()
}

fn resolve_occurrence(
    source_path: &str,
    source: &RepositorySemanticFile,
    occurrence: &OccurrenceEntry,
    files: &BTreeMap<String, RepositorySemanticFile>,
) -> Option<ResolvedSemanticEdge> {
    let target = if occurrence.qualifier.is_empty() {
        definition(files, source_path, &occurrence.name, occurrence.namespace).or_else(|| {
            if local_definition_shadows(&source.node, occurrence) {
                None
            } else {
                resolve_imported_name(source_path, &source.node, occurrence, files)
            }
        })
    } else {
        resolve_qualified_name(source_path, &source.node, occurrence, files)
    }?;
    Some(ResolvedSemanticEdge {
        source_occurrence: occurrence.local_id,
        target_path: target.path,
        target_file_node: target.file_node,
        target_definition: target.definition,
        kind: match occurrence.role {
            OccurrenceRole::Call => SemanticEdgeKind::Calls,
            OccurrenceRole::TypeReference => SemanticEdgeKind::TypeRef,
            OccurrenceRole::Reference => SemanticEdgeKind::RefersTo,
            OccurrenceRole::Definition => return None,
        },
    })
}

fn resolve_imported_name(
    source_path: &str,
    source: &SemanticFileNode,
    occurrence: &OccurrenceEntry,
    files: &BTreeMap<String, RepositorySemanticFile>,
) -> Option<Target> {
    let candidates = visible_bindings(source, occurrence, &occurrence.name)
        .filter(|(_, binding)| binding.local == occurrence.name && binding.imported != "*")
        .filter_map(|(import, binding)| {
            let path = resolve_module(source_path, source, &import.module_specifier, files)?;
            definition(files, &path, &binding.imported, occurrence.namespace)
        })
        .collect::<BTreeSet<_>>();
    exactly_one(candidates)
}

fn resolve_qualified_name(
    source_path: &str,
    source: &SemanticFileNode,
    occurrence: &OccurrenceEntry,
    files: &BTreeMap<String, RepositorySemanticFile>,
) -> Option<Target> {
    let first = occurrence.qualifier.first()?;
    let imported = visible_bindings(source, occurrence, first)
        .filter(|(_, binding)| binding.local == *first)
        .filter(|(_, binding)| binding.imported == "*" || source.language == "rust")
        .filter_map(|(import, binding)| {
            let specifier = if source.language == "rust" && binding.imported != "*" {
                format!("{}::{}", import.module_specifier, binding.imported)
            } else {
                import.module_specifier.clone()
            };
            resolve_module(source_path, source, &specifier, files)
        })
        .collect::<BTreeSet<_>>();
    let path = exactly_one(imported).or_else(|| {
        (source.language == "rust" && matches!(first.as_str(), "crate" | "self" | "super"))
            .then(|| occurrence.qualifier.join("::"))
            .and_then(|qualifier| resolve_module(source_path, source, &qualifier, files))
    })?;
    definition(files, &path, &occurrence.name, occurrence.namespace)
}

fn visible_bindings<'a>(
    source: &'a SemanticFileNode,
    occurrence: &'a OccurrenceEntry,
    local_name: &'a str,
) -> impl Iterator<Item = (&'a ImportEntry, &'a ImportBinding)> {
    let occurrence_namespace = occurrence.namespace;
    let nearest = source
        .imports
        .iter()
        .filter(|import| {
            scope::contains(source, import.scope, occurrence.scope)
                && import.bindings.iter().any(|binding| {
                    binding.local == local_name
                        && namespaces_overlap(binding.namespace, occurrence.namespace)
                })
        })
        .map(|import| scope::depth(source, import.scope))
        .max();
    source
        .imports
        .iter()
        .filter(move |import| {
            nearest.is_some_and(|depth| {
                scope::contains(source, import.scope, occurrence.scope)
                    && scope::depth(source, import.scope) == depth
            })
        })
        .flat_map(move |import| {
            import
                .bindings
                .iter()
                .filter(move |binding| namespaces_overlap(binding.namespace, occurrence_namespace))
                .map(move |binding| (import, binding))
        })
}

fn local_definition_shadows(source: &SemanticFileNode, occurrence: &OccurrenceEntry) -> bool {
    source.occurrences.iter().any(|candidate| {
        candidate.role == OccurrenceRole::Definition
            && candidate.name == occurrence.name
            && namespaces_overlap(candidate.namespace, occurrence.namespace)
            && candidate.span.start <= occurrence.span.start
            && scope::contains(source, candidate.scope, occurrence.scope)
    })
}

fn resolve_module(
    source_path: &str,
    source: &SemanticFileNode,
    specifier: &str,
    files: &BTreeMap<String, RepositorySemanticFile>,
) -> Option<String> {
    let candidates = match source.language.as_str() {
        "rust" => rust::module_candidates(source_path, specifier),
        "typescript" | "javascript" => typescript::module_candidates(source_path, specifier),
        _ => Vec::new(),
    };
    candidates
        .into_iter()
        .find(|candidate| files.contains_key(candidate))
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Target {
    path: String,
    file_node: ContentHash,
    definition: u32,
}

fn definition(
    files: &BTreeMap<String, RepositorySemanticFile>,
    path: &str,
    name: &str,
    namespace: SymbolNamespace,
) -> Option<Target> {
    let file = files.get(path)?;
    let candidates = file
        .node
        .symbols
        .iter()
        .enumerate()
        .filter(|(_, symbol)| symbol.container_path.is_empty() && symbol.name == name)
        .filter(|(_, symbol)| namespaces_overlap(symbol_namespace(symbol.kind), namespace))
        .map(|(index, _)| Target {
            path: path.to_string(),
            file_node: file.node_hash,
            definition: index as u32,
        })
        .collect::<BTreeSet<_>>();
    exactly_one(candidates)
}

fn symbol_namespace(kind: SymbolKindTag) -> SymbolNamespace {
    match kind {
        SymbolKindTag::Function | SymbolKindTag::Const | SymbolKindTag::Other => {
            SymbolNamespace::Value
        }
        SymbolKindTag::Type
        | SymbolKindTag::Enum
        | SymbolKindTag::Trait
        | SymbolKindTag::Class
        | SymbolKindTag::Interface
        | SymbolKindTag::TypeAlias => SymbolNamespace::Type,
        SymbolKindTag::Module => SymbolNamespace::Both,
    }
}

fn namespaces_overlap(a: SymbolNamespace, b: SymbolNamespace) -> bool {
    a == SymbolNamespace::Both || b == SymbolNamespace::Both || a == b
}

fn exactly_one<T: Ord>(values: BTreeSet<T>) -> Option<T> {
    if values.len() == 1 {
        values.into_iter().next()
    } else {
        None
    }
}

#[cfg(test)]
mod tests;
