// SPDX-License-Identifier: Apache-2.0
//! Ignore pattern helpers for worktree operations.

use std::path::Path;

pub fn should_ignore(path: &Path, patterns: &[String]) -> bool {
    let path_str = path.to_string_lossy();

    for pattern in patterns {
        if is_root_admin_pattern(pattern) {
            if is_root_path_match(path, pattern) {
                return true;
            }
            continue;
        }

        if pattern.ends_with('/') {
            let dir_pattern = pattern.trim_end_matches('/');
            if is_dir_match(path, dir_pattern) {
                return true;
            }
        } else if let Some(suffix) = pattern.strip_prefix('*') {
            if path_str.ends_with(suffix) {
                return true;
            }
            for component in path.components() {
                if let Some(s) = component.as_os_str().to_str()
                    && s.ends_with(suffix)
                {
                    return true;
                }
            }
        } else {
            for component in path.components() {
                if let Some(s) = component.as_os_str().to_str()
                    && s == pattern
                {
                    return true;
                }
            }
        }
    }

    false
}

fn is_root_admin_pattern(pattern: &str) -> bool {
    matches!(pattern, ".heddle" | ".heddleignore" | ".git")
}

fn is_root_path_match(path: &Path, pattern: &str) -> bool {
    let mut components = path.components();
    let Some(first) = components.next() else {
        return false;
    };
    let Some(first_str) = first.as_os_str().to_str() else {
        return false;
    };
    first_str == pattern
}

fn is_dir_match(path: &Path, dir_pattern: &str) -> bool {
    let path_str = path.to_string_lossy();
    if path_str.starts_with(&format!("{}/", dir_pattern)) {
        return true;
    }
    for component in path.components() {
        if let Some(s) = component.as_os_str().to_str()
            && s == dir_pattern
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn test_glob_extension() {
        let patterns = vec!["*.log".to_string()];
        assert!(should_ignore(&PathBuf::from("test.log"), &patterns));
        assert!(should_ignore(&PathBuf::from("debug.log"), &patterns));
        assert!(!should_ignore(&PathBuf::from("test.txt"), &patterns));
    }

    #[test]
    fn test_directory_pattern() {
        let patterns = vec!["build/".to_string()];
        assert!(should_ignore(&PathBuf::from("build/output.txt"), &patterns));
        assert!(should_ignore(&PathBuf::from("build"), &patterns));
        assert!(!should_ignore(&PathBuf::from("builder.txt"), &patterns));
    }

    #[test]
    fn test_simple_pattern() {
        let patterns = vec!["node_modules".to_string()];
        assert!(should_ignore(
            &PathBuf::from("node_modules/package.json"),
            &patterns
        ));
        assert!(!should_ignore(&PathBuf::from("src/main.rs"), &patterns));
    }

    #[test]
    fn test_simple_pattern_does_not_match_prefixes() {
        let patterns = vec!["target".to_string()];
        assert!(should_ignore(
            &PathBuf::from("target/output.txt"),
            &patterns
        ));
        assert!(should_ignore(&PathBuf::from("build/target/app"), &patterns));
        assert!(!should_ignore(&PathBuf::from("target.txt"), &patterns));
        assert!(!should_ignore(
            &PathBuf::from("targeted/output.txt"),
            &patterns
        ));
    }

    #[test]
    fn test_root_admin_patterns_do_not_ignore_nested_paths() {
        let patterns = vec![".heddle".to_string(), ".heddleignore".to_string()];
        assert!(should_ignore(&PathBuf::from(".heddle/objects"), &patterns));
        assert!(should_ignore(
            &PathBuf::from(".heddle/state/index.bin"),
            &patterns
        ));
        assert!(should_ignore(&PathBuf::from(".heddleignore"), &patterns));
        assert!(!should_ignore(
            &PathBuf::from("examples/calculator/.heddle/objects"),
            &patterns
        ));
        assert!(!should_ignore(
            &PathBuf::from("examples/calculator/.heddle/state/index.bin"),
            &patterns
        ));
        assert!(!should_ignore(
            &PathBuf::from("examples/calculator/.heddleignore"),
            &patterns
        ));
    }
}