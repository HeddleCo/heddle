// SPDX-License-Identifier: Apache-2.0
use std::{fs, path::Path};

use tempfile::TempDir;

use super::*;

fn fixture(files: &[(&str, &str)]) -> TempDir {
    let temp = tempfile::tempdir().expect("tempdir");
    for (path, source) in files {
        let path = temp.path().join(path);
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(path, source).expect("write fixture");
    }
    temp
}

fn manifest() -> &'static str {
    "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"
}

fn check(temp: &TempDir) -> Report {
    check_workspace(temp.path()).expect("check fixture")
}

#[test]
fn follows_conventional_nested_and_path_modules() {
    let temp = fixture(&[
        ("crates/demo/Cargo.toml", manifest()),
        (
            "crates/demo/src/lib.rs",
            "mod api;\n#[path = \"shared/extra.rs\"]\nmod custom;\n",
        ),
        ("crates/demo/src/api.rs", "mod nested;\n"),
        ("crates/demo/src/api/nested.rs", "pub fn nested() {}\n"),
        ("crates/demo/src/shared/extra.rs", "pub fn custom() {}\n"),
    ]);

    let report = check(&temp);
    assert!(report.unreachable.is_empty());
    assert_eq!(report.source_count, 4);
}

#[test]
fn path_attribute_is_relative_to_declaring_file() {
    let temp = fixture(&[
        ("crates/demo/Cargo.toml", manifest()),
        ("crates/demo/src/lib.rs", "mod repository;\n"),
        (
            "crates/demo/src/repository.rs",
            "#[path = \"sibling.rs\"]\nmod sibling;\n",
        ),
        ("crates/demo/src/sibling.rs", "pub fn sibling() {}\n"),
    ]);

    assert!(check(&temp).unreachable.is_empty());
}

#[test]
fn reports_unreachable_source() {
    let temp = fixture(&[
        ("crates/demo/Cargo.toml", manifest()),
        ("crates/demo/src/lib.rs", "pub fn live() {}\n"),
        ("crates/demo/src/orphan.rs", "pub fn invisible() {}\n"),
    ]);

    let report = check(&temp);
    assert_eq!(report.unreachable.len(), 1);
    assert!(report.unreachable[0].ends_with(Path::new("src/orphan.rs")));
}

#[test]
fn treats_auto_discovered_binaries_as_crate_roots() {
    let temp = fixture(&[
        ("crates/demo/Cargo.toml", manifest()),
        ("crates/demo/src/lib.rs", "pub fn library() {}\n"),
        ("crates/demo/src/bin/tool.rs", "fn main() {}\n"),
        (
            "crates/demo/src/bin/nested/main.rs",
            "mod helper;\nfn main() {}\n",
        ),
        (
            "crates/demo/src/bin/nested/helper.rs",
            "pub fn helper() {}\n",
        ),
    ]);

    assert!(check(&temp).unreachable.is_empty());
}

#[test]
fn reports_crate_and_module_dead_code_blankets() {
    let temp = fixture(&[
        ("crates/demo/Cargo.toml", manifest()),
        (
            "crates/demo/src/lib.rs",
            "#![allow(dead_code)]\n#[allow(dead_code)]\nmod hidden {}\n",
        ),
    ]);

    let report = check(&temp);
    assert_eq!(report.dead_code_blankets.len(), 2);
}

#[test]
fn permits_scoped_dead_code_allow() {
    let temp = fixture(&[
        ("crates/demo/Cargo.toml", manifest()),
        (
            "crates/demo/src/lib.rs",
            "// Kept for a platform-specific caller.\n#[allow(dead_code)]\nfn platform_hook() {}\n",
        ),
    ]);

    assert!(check(&temp).dead_code_blankets.is_empty());
}
