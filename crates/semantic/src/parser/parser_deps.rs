// SPDX-License-Identifier: Apache-2.0
//! Dependency extraction helpers.

use std::collections::HashMap;

use super::{ParsedFile, parser_types::ImportKind};

/// Extract dependencies from a parsed file.
pub fn extract_dependencies(parsed: &ParsedFile) -> HashMap<String, Vec<String>> {
    let mut deps = HashMap::new();
    let imports = parsed.extract_imports();

    for import in imports {
        match import.kind {
            ImportKind::ExternCrate => {
                let parts: Vec<&str> = import.raw.split_whitespace().collect();
                if parts.len() >= 3 {
                    let name = parts[2].trim_end_matches(';');
                    deps.entry(name.to_string())
                        .or_insert_with(Vec::new)
                        .push("extern".to_string());
                }
            }
            _ => {
                if import.raw.starts_with("use ") {
                    let path = &import.raw[4..].trim_end_matches(';');
                    let first_segment = path.split("::").next().unwrap_or(path);
                    if first_segment != "crate" && first_segment != "std" && first_segment != "core"
                    {
                        deps.entry(first_segment.to_string())
                            .or_insert_with(Vec::new)
                            .push(path.to_string());
                    }
                }
            }
        }
    }

    deps
}