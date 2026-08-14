// SPDX-License-Identifier: Apache-2.0
//! Explicit persistent cache-directory exports.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// Prefix for every cache-directory environment variable.
pub const CACHE_ENV_PREFIX: &str = "HCI_CACHE_";

/// Prepared cache slots for one check.
#[derive(Debug, Clone, Default)]
pub struct PreparedCaches {
    /// Environment exports.
    pub env: BTreeMap<String, String>,
    /// Directories successfully created.
    pub dirs: Vec<PathBuf>,
}

/// Create declared cache slots under `cache_root`.
/// A failed cache creation degrades to a cold build.
#[must_use]
pub fn prepare_caches(paths: &[String], cache_root: &Path) -> PreparedCaches {
    let mut prepared = PreparedCaches::default();
    let mut used = BTreeMap::<String, u32>::new();
    for path in paths {
        let base = slot_name(path);
        let slot = match used.get_mut(&base) {
            Some(count) => {
                *count += 1;
                format!("{base}_{count}")
            }
            None => {
                used.insert(base.clone(), 0);
                base
            }
        };
        let directory = cache_root.join(&slot);
        if std::fs::create_dir_all(&directory).is_err() {
            continue;
        }
        prepared.env.insert(
            format!("{CACHE_ENV_PREFIX}{slot}"),
            directory.display().to_string(),
        );
        prepared.dirs.push(directory);
    }
    prepared
}

fn slot_name(path: &str) -> String {
    let mut output = String::with_capacity(path.len());
    let mut separated = true;
    for character in path.chars() {
        if character.is_ascii_alphanumeric() {
            output.push(character.to_ascii_uppercase());
            separated = false;
        } else if !separated {
            output.push('_');
            separated = true;
        }
    }
    while output.ends_with('_') {
        output.pop();
    }
    if output.is_empty() {
        "CACHE".to_string()
    } else {
        output
    }
}
