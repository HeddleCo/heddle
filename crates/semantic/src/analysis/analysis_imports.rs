// SPDX-License-Identifier: Apache-2.0
//! Import/dependency change detection.

use std::collections::{HashMap, HashSet};

use objects::object::SemanticChange;

use crate::parser::{Language, ParsedFile};

/// Detect import/dependency changes between two file versions.
///
/// If `manifest_content` is provided (e.g. contents of `Cargo.toml` or `package.json`),
/// dependency versions will be resolved from it instead of showing "unknown".
pub fn detect_import_changes(
    old_path: &std::path::Path,
    new_path: &std::path::Path,
    old_content: &str,
    new_content: &str,
) -> Vec<SemanticChange> {
    detect_import_changes_with_manifest(old_path, new_path, old_content, new_content, None)
}

/// Detect import changes with optional manifest for version resolution.
pub fn detect_import_changes_with_manifest(
    old_path: &std::path::Path,
    new_path: &std::path::Path,
    old_content: &str,
    new_content: &str,
    manifest_content: Option<&str>,
) -> Vec<SemanticChange> {
    let old_parsed = ParsedFile::parse(old_content, Language::from_path(old_path));
    let new_parsed = ParsedFile::parse(new_content, Language::from_path(new_path));

    detect_import_changes_with_parsed(
        old_path,
        new_path,
        old_parsed.as_ref(),
        new_parsed.as_ref(),
        manifest_content,
    )
}

pub(crate) fn detect_import_changes_with_parsed(
    _old_path: &std::path::Path,
    new_path: &std::path::Path,
    old_parsed: Option<&ParsedFile>,
    new_parsed: Option<&ParsedFile>,
    manifest_content: Option<&str>,
) -> Vec<SemanticChange> {
    let mut changes = Vec::new();

    let old_imports: HashSet<String> = old_parsed
        .map(|p| p.extract_imports().into_iter().map(|i| i.raw).collect())
        .unwrap_or_default();

    let new_imports: HashSet<String> = new_parsed
        .map(|p| p.extract_imports().into_iter().map(|i| i.raw).collect())
        .unwrap_or_default();

    let versions = manifest_content
        .map(|m| parse_manifest_versions(m, Language::from_path(new_path)))
        .unwrap_or_default();

    let old_deps = dependency_names(&old_imports);
    let new_deps = dependency_names(&new_imports);

    for dep_name in new_deps.difference(&old_deps) {
        let version = versions
            .get(dep_name)
            .cloned()
            .unwrap_or_else(|| "unknown".to_string());
        changes.push(SemanticChange::DependencyAdded {
            name: dep_name.clone(),
            version,
        });
    }

    for dep_name in old_deps.difference(&new_deps) {
        changes.push(SemanticChange::DependencyRemoved {
            name: dep_name.clone(),
        });
    }

    changes
}

fn dependency_names(imports: &HashSet<String>) -> HashSet<String> {
    imports
        .iter()
        .filter_map(|import| extract_dependency_from_import(import))
        .filter(|name| !is_stdlib_dependency(name))
        .collect()
}

/// Parse dependency versions from a manifest file (Cargo.toml or package.json).
fn parse_manifest_versions(content: &str, language: Language) -> HashMap<String, String> {
    match language {
        Language::Rust => parse_cargo_toml_versions(content),
        Language::JavaScript | Language::TypeScript => parse_package_json_versions(content),
        _ => HashMap::new(),
    }
}

/// Extract crate versions from Cargo.toml.
fn parse_cargo_toml_versions(content: &str) -> HashMap<String, String> {
    let mut versions = HashMap::new();
    // Simple line-based parser for [dependencies] sections.
    let mut in_deps = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_deps = trimmed == "[dependencies]"
                || trimmed == "[dev-dependencies]"
                || trimmed == "[build-dependencies]";
            continue;
        }
        if !in_deps {
            continue;
        }
        // Handle: crate_name = "version"
        if let Some((name, rest)) = trimmed.split_once('=') {
            let name = name.trim().trim_matches('"');
            let rest = rest.trim();
            if let Some(version) = rest.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
                versions.insert(name.to_string(), version.to_string());
            }
            // Handle: crate_name = { version = "X", ... }
            else if rest.starts_with('{')
                && let Some(start) = rest.find("version")
            {
                let after = &rest[start..];
                if let Some(eq) = after.find('=') {
                    let val = after[eq + 1..].trim().trim_start_matches('"');
                    if let Some(end) = val.find('"') {
                        versions.insert(name.to_string(), val[..end].to_string());
                    }
                }
            }
        }
    }
    versions
}

/// Extract dependency versions from package.json using simple line parsing.
/// Handles the common `"name": "version"` pattern inside dependency sections.
fn parse_package_json_versions(content: &str) -> HashMap<String, String> {
    let mut versions = HashMap::new();
    let mut in_deps = false;
    let mut brace_depth: i32 = 0;
    for line in content.lines() {
        let trimmed = line.trim();
        // Detect dependency sections.
        if (trimmed.contains("\"dependencies\"")
            || trimmed.contains("\"devDependencies\"")
            || trimmed.contains("\"peerDependencies\""))
            && trimmed.contains(':')
        {
            in_deps = true;
            if trimmed.contains('{') {
                brace_depth = 1;
            }
            continue;
        }
        if in_deps {
            brace_depth += trimmed.matches('{').count() as i32;
            brace_depth -= trimmed.matches('}').count() as i32;
            if brace_depth <= 0 {
                in_deps = false;
                continue;
            }
            // Parse "name": "version"
            if let Some((name_part, version_part)) = trimmed.split_once(':') {
                let name = name_part.trim().trim_matches(|c| c == '"' || c == ',');
                let version = version_part
                    .trim()
                    .trim_matches(|c| c == '"' || c == ',' || c == ' ');
                if !name.is_empty() && !version.is_empty() && !version.starts_with('{') {
                    versions.insert(name.to_string(), version.to_string());
                }
            }
        }
    }
    versions
}

fn extract_dependency_from_import(import: &str) -> Option<String> {
    let trimmed = import.trim();

    if let Some(stripped) = trimmed.strip_prefix("use ") {
        let path = stripped.trim_end_matches(';');
        let first = path.split("::").next()?;
        return Some(first.to_string());
    }

    if trimmed.starts_with("extern crate ") {
        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        if parts.len() >= 3 {
            return Some(parts[2].trim_end_matches(';').to_string());
        }
    }

    None
}

fn is_stdlib_dependency(name: &str) -> bool {
    matches!(name, "std" | "core" | "alloc" | "crate" | "super" | "self")
}