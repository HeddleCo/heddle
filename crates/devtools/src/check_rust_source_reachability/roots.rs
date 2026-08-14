// SPDX-License-Identifier: Apache-2.0
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

pub(super) fn crate_roots(crate_dir: &Path, src: &Path) -> Result<BTreeSet<PathBuf>> {
    let manifest_path = crate_dir.join("Cargo.toml");
    let manifest_source = fs::read_to_string(&manifest_path)
        .with_context(|| format!("read {}", manifest_path.display()))?;
    let manifest: toml::Value = toml::from_str(&manifest_source)
        .with_context(|| format!("parse {}", manifest_path.display()))?;
    let package = manifest.get("package").and_then(toml::Value::as_table);
    let mut roots = BTreeSet::new();

    if package
        .and_then(|p| p.get("autolib"))
        .and_then(toml::Value::as_bool)
        != Some(false)
    {
        add_if_file(&mut roots, src.join("lib.rs"));
    }
    if let Some(lib) = manifest.get("lib").and_then(toml::Value::as_table) {
        if lib.get("path").is_some() {
            add_manifest_path(&mut roots, crate_dir, lib.get("path"));
        } else {
            add_if_file(&mut roots, src.join("lib.rs"));
        }
    }
    if package
        .and_then(|p| p.get("autobins"))
        .and_then(toml::Value::as_bool)
        != Some(false)
    {
        add_if_file(&mut roots, src.join("main.rs"));
        add_auto_bins(&mut roots, &src.join("bin"))?;
    }
    for target_kind in ["bin", "example", "test", "bench"] {
        if let Some(targets) = manifest.get(target_kind).and_then(toml::Value::as_array) {
            for target in targets {
                add_manifest_path(
                    &mut roots,
                    crate_dir,
                    target.as_table().and_then(|table| table.get("path")),
                );
            }
        }
    }
    if let Some(build) = package.and_then(|p| p.get("build")) {
        add_manifest_path(&mut roots, crate_dir, Some(build));
    }
    Ok(roots)
}

fn add_auto_bins(roots: &mut BTreeSet<PathBuf>, bin_dir: &Path) -> Result<()> {
    if !bin_dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(bin_dir).with_context(|| format!("read {}", bin_dir.display()))? {
        let path = entry?.path();
        if path.extension().is_some_and(|ext| ext == "rs") {
            roots.insert(path);
        } else if path.is_dir() {
            add_if_file(roots, path.join("main.rs"));
        }
    }
    Ok(())
}

fn add_manifest_path(roots: &mut BTreeSet<PathBuf>, crate_dir: &Path, value: Option<&toml::Value>) {
    if let Some(path) = value.and_then(toml::Value::as_str) {
        add_if_file(roots, crate_dir.join(path));
    }
}

fn add_if_file(paths: &mut BTreeSet<PathBuf>, path: PathBuf) {
    if path.is_file() {
        paths.insert(path);
    }
}
