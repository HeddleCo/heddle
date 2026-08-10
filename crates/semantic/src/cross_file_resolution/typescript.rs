// SPDX-License-Identifier: Apache-2.0

use std::path::{Component, Path, PathBuf};

const EXTENSIONS: [&str; 4] = ["ts", "tsx", "js", "jsx"];

pub(super) fn module_candidates(source_path: &str, specifier: &str) -> Vec<String> {
    if !specifier.starts_with('.') {
        return Vec::new();
    }
    let base = normalize(
        Path::new(source_path)
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .join(specifier),
    );
    let mut out = Vec::new();
    if base.extension().is_some() {
        push(&mut out, &base);
    } else {
        for extension in EXTENSIONS {
            push(&mut out, &base.with_extension(extension));
        }
        for extension in EXTENSIONS {
            push(&mut out, &base.join(format!("index.{extension}")));
        }
    }
    out
}

fn push(out: &mut Vec<String>, path: &Path) {
    if let Some(path) = path.to_str() {
        out.push(path.to_string());
    }
}

fn normalize(path: PathBuf) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_relative_modules_and_index_files() {
        let candidates = module_candidates("src/client/main.ts", "../api");
        assert_eq!(candidates[0], "src/api.ts");
        assert!(candidates.contains(&"src/api/index.ts".to_string()));
        assert!(module_candidates("src/main.ts", "react").is_empty());
    }
}
