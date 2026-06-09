// SPDX-License-Identifier: Apache-2.0
//! Remote URL normalization and local-path resolution via sley-transport.

use std::path::{Path, PathBuf};

use sley_core::{GitError, Result};
use sley_transport::{RemoteTransport, parse_remote_url};

/// Whether `value` names a filesystem path rather than a wire URL.
pub fn configured_remote_is_local_path(value: &str) -> bool {
    value.starts_with('/')
        || value.starts_with("./")
        || value.starts_with("../")
        || value.starts_with('~')
        || value.starts_with(std::path::MAIN_SEPARATOR)
}

/// Resolve a config `url =` value that may be a relative local path.
pub fn configured_remote_local_path(value: &str, relative_base: &Path) -> PathBuf {
    if value == "~"
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home);
    }
    if let Some(rest) = value.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }

    let path = Path::new(value);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        relative_base.join(path)
    }
}

/// Normalize a configured remote URL for transport and parsing.
pub fn normalize_configured_remote_url(value: &str, relative_base: &Path) -> Result<String> {
    if configured_remote_is_local_path(value) {
        let path = configured_remote_local_path(value, relative_base);
        Ok(path_to_file_url(&path))
    } else {
        Ok(value.to_string())
    }
}

/// Extract the on-disk repository path from a `file://` or local-path URL.
pub fn local_path_from_remote_url(url: &str, relative_base: &Path) -> Result<PathBuf> {
    let parsed = parse_remote_url(url)?;
    match parsed.transport {
        RemoteTransport::File => Ok(PathBuf::from(parsed.path)),
        RemoteTransport::Local => {
            let path = PathBuf::from(&parsed.path);
            Ok(if path.is_absolute() {
                path
            } else {
                relative_base.join(path)
            })
        }
        _ => Err(GitError::Unsupported(format!(
            "expected a local remote URL, got {url}"
        ))),
    }
}

/// Whether `url` uses the `file://` scheme or a bare local path spelling.
pub fn remote_url_is_file(url: &str) -> Result<bool> {
    Ok(matches!(
        parse_remote_url(url)?.transport,
        RemoteTransport::File | RemoteTransport::Local
    ))
}

fn path_to_file_url(path: &Path) -> String {
    let display = path.display().to_string();
    if display.starts_with('/') {
        format!("file://{display}")
    } else {
        format!("file:///{display}")
    }
}