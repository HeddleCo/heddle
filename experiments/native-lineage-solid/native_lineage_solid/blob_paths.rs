// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use objects::{
    object::ContentHash,
    store::{FsStore, ObjectStore},
};

pub fn path_text(
    store: &FsStore,
    paths: &HashMap<String, ContentHash>,
) -> Result<Vec<(PathBuf, String)>> {
    let mut sorted = paths.iter().collect::<Vec<_>>();
    sorted.sort_by_key(|(path, _)| path.as_str());
    sorted
        .into_iter()
        .map(|(path, hash)| {
            let bytes = store
                .get_blob(hash)?
                .with_context(|| format!("missing blob {hash}"))?;
            Ok((
                PathBuf::from(path),
                String::from_utf8_lossy(bytes.content()).into_owned(),
            ))
        })
        .collect()
}

pub fn extension(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|name| Path::new(name).extension())
        .and_then(|value| value.to_str())
        .unwrap_or("")
}
