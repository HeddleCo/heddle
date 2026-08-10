// SPDX-License-Identifier: Apache-2.0

use std::path::{Component, Path, PathBuf};

pub(super) fn module_candidates(source_path: &str, specifier: &str) -> Vec<String> {
    let source = Path::new(source_path);
    let root = crate_root(source);
    let module_dir = module_dir(source);
    let mut segments = specifier.split("::").filter(|part| !part.is_empty());
    let first = segments.next();
    let mut base = match first {
        Some("crate") => root,
        Some("self") => module_dir,
        Some("super") => {
            let mut base = module_dir;
            base.pop();
            base
        }
        Some(segment) => {
            let mut base = root;
            base.push(segment);
            base
        }
        None => return Vec::new(),
    };
    for segment in segments {
        if segment == "super" {
            base.pop();
        } else if segment != "self" {
            base.push(segment);
        }
    }
    candidates(base, &["rs"])
}

fn crate_root(path: &Path) -> PathBuf {
    let components = path.components().collect::<Vec<_>>();
    let src = components
        .iter()
        .rposition(|component| component.as_os_str() == "src");
    match src {
        Some(index) => components[..=index].iter().collect(),
        None => PathBuf::new(),
    }
}

fn module_dir(path: &Path) -> PathBuf {
    let mut dir = path.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("");
    if !matches!(stem, "lib" | "main" | "mod") {
        dir.push(stem);
    }
    dir
}

fn candidates(base: PathBuf, extensions: &[&str]) -> Vec<String> {
    let normalized = normalize(base);
    let mut out = extensions
        .iter()
        .map(|extension| normalized.with_extension(extension))
        .collect::<Vec<_>>();
    out.extend(
        extensions
            .iter()
            .map(|extension| normalized.join(format!("mod.{extension}"))),
    );
    out.into_iter()
        .filter_map(|path| path.to_str().map(str::to_string))
        .collect()
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
    fn resolves_crate_and_super_modules() {
        assert_eq!(
            module_candidates("src/client.rs", "crate::api")[0],
            "src/api.rs"
        );
        assert_eq!(
            module_candidates("src/client/worker.rs", "super::api")[0],
            "src/client/api.rs"
        );
    }
}
