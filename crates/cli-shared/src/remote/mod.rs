// SPDX-License-Identifier: Apache-2.0
//! Remote configuration management.
//!
//! Remote aliases remain repository-scoped and live in `.heddle/remotes.toml`.

mod target;

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use objects::fs_atomic::write_file_atomic;
use repo::Repository;
use serde::{Deserialize, Serialize};
pub use target::RemoteTarget;

#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("toml error: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("toml serialize error: {0}")]
    TomlSerialize(#[from] toml::ser::Error),

    #[error("remote not found: {0}")]
    NotFound(String),

    #[error("no default remote configured")]
    NoDefaultRemote,

    #[error("invalid remote url: {0}")]
    InvalidUrl(String),
}

pub type Result<T> = std::result::Result<T, RemoteError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Remote {
    pub url: String,
    /// Explicitly allow cleartext (non-TLS) connections to non-loopback hosts
    /// for this remote. Equivalent to CLI `--insecure` for push/pull/clone.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub insecure: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct RemotesFile {
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub remotes: HashMap<String, Remote>,
}

impl RemotesFile {
    pub fn load(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(contents) => Ok(toml::from_str(&contents)?),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err.into()),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let contents = toml::to_string_pretty(self)?;
        write_file_atomic(path, contents.as_bytes())?;
        Ok(())
    }
}

pub struct RemoteConfig {
    path: PathBuf,
    file: RemotesFile,
}

impl RemoteConfig {
    pub fn open(repo: &Repository) -> Result<Self> {
        let path = repo.heddle_dir().join("remotes.toml");
        let file = RemotesFile::load(&path)?;
        Ok(Self { path, file })
    }

    pub fn list(&self) -> Vec<(String, Remote)> {
        let mut items: Vec<_> = self
            .file
            .remotes
            .iter()
            .map(|(name, remote)| (name.clone(), remote.clone()))
            .collect();
        items.sort_by(|a, b| a.0.cmp(&b.0));
        items
    }

    pub fn get(&self, name: &str) -> Result<Remote> {
        self.file
            .remotes
            .get(name)
            .cloned()
            .ok_or_else(|| RemoteError::NotFound(name.to_string()))
    }

    pub fn add(&mut self, name: &str, remote: Remote) -> Result<()> {
        if self.file.default.is_none() {
            self.file.default = Some(name.to_string());
        }
        self.file.remotes.insert(name.to_string(), remote);
        self.file.save(&self.path)?;
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> Result<()> {
        if self.file.remotes.remove(name).is_none() {
            return Err(RemoteError::NotFound(name.to_string()));
        }
        if self.file.default.as_deref() == Some(name) {
            self.file.default = None;
        }
        self.file.save(&self.path)?;
        Ok(())
    }

    pub fn clear_default(&mut self) -> Result<()> {
        self.file.default = None;
        self.file.save(&self.path)?;
        Ok(())
    }

    pub fn set_default(&mut self, name: &str) -> Result<()> {
        if !self.file.remotes.contains_key(name) {
            return Err(RemoteError::NotFound(name.to_string()));
        }
        self.file.default = Some(name.to_string());
        self.file.save(&self.path)?;
        Ok(())
    }

    pub fn default_name(&self) -> Option<&str> {
        self.file.default.as_deref()
    }
}

/// Resolve a remote argument (name or URL) into a concrete target.
pub fn resolve_remote(repo: &Repository, remote_arg: Option<&str>) -> Result<RemoteTarget> {
    Ok(resolve_remote_with_key(repo, remote_arg)?.0)
}

/// Resolve a remote argument (name or URL) into a concrete target, also
/// returning the raw URL string that can be used as a credential store key.
pub fn resolve_remote_with_key(
    repo: &Repository,
    remote_arg: Option<&str>,
) -> Result<(RemoteTarget, Option<String>)> {
    let (target, key, _insecure) = resolve_remote_with_key_and_insecure(repo, remote_arg)?;
    Ok((target, key))
}

/// Resolve a remote argument into target, credential key, and the remote's
/// configured `insecure` flag (false for ad-hoc URL specs).
pub fn resolve_remote_with_key_and_insecure(
    repo: &Repository,
    remote_arg: Option<&str>,
) -> Result<(RemoteTarget, Option<String>, bool)> {
    let cfg = RemoteConfig::open(repo)?;

    let spec = match remote_arg {
        Some(spec) => spec.to_string(),
        None => cfg
            .default_name()
            .ok_or(RemoteError::NoDefaultRemote)?
            .to_string(),
    };

    // Named remote first so configured `insecure` applies even when the
    // name also happens to parse as a bare host:port.
    if let Ok(remote) = cfg.get(&spec)
        && let Ok(target) = RemoteTarget::parse(&remote.url)
    {
        let key = credential_key_from_url(&remote.url);
        return Ok((target, key, remote.insecure));
    }

    if let Ok(target) = RemoteTarget::parse(&spec) {
        let key = credential_key_from_url(&spec);
        return Ok((target, key, false));
    }

    let remote = cfg.get(&spec)?;
    if let Ok(target) = RemoteTarget::parse(&remote.url) {
        let key = credential_key_from_url(&remote.url);
        return Ok((target, key, remote.insecure));
    }

    Err(RemoteError::InvalidUrl(remote.url))
}

/// Whether a named remote (or the default) has `insecure = true` in
/// `.heddle/remotes.toml`. Returns false for URL specs / missing remotes.
pub fn remote_allows_insecure(repo: &Repository, remote_arg: Option<&str>) -> bool {
    resolve_remote_with_key_and_insecure(repo, remote_arg)
        .map(|(_, _, insecure)| insecure)
        .unwrap_or(false)
}

/// Extract the hostname (credential store key) from a remote URL string.
///
/// Returns `None` for local paths (file:// or bare paths).
pub fn credential_key_from_remote_url(url: &str) -> Option<String> {
    credential_key_from_url(url)
}

/// Internal implementation of credential key extraction.
fn credential_key_from_url(url: &str) -> Option<String> {
    // Strip known scheme prefixes.
    let rest = url
        .strip_prefix("heddle://")
        .or_else(|| url.strip_prefix("https://"))
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);

    // Skip local paths.
    if rest.starts_with('/') || url.starts_with("file://") {
        return None;
    }

    // The credential key is the host part (before the first '/').
    let host_part = rest.split('/').next().unwrap_or(rest);
    if host_part.is_empty() {
        return None;
    }
    Some(host_part.to_string())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use repo::Repository;

    use super::*;

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{unique}-{}", std::process::id()))
    }

    #[test]
    fn remote_config_save_uses_atomic_write_and_persists() {
        // Mutex serializes env-var access across this crate's tests so
        // parallel runs don't observe each other's writes.
        static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = unique_temp_dir("heddle-remote-test");
        fs::create_dir_all(&temp).expect("create temp dir");
        let repo = Repository::init_default(&temp).expect("init repo");

        {
            let mut cfg = RemoteConfig::open(&repo).expect("open config");
            cfg.add(
                "origin",
                Remote {
                    url: "http://heddle.example:8421/repo".to_string(),
                    insecure: false,
                },
            )
            .expect("add remote");
        }

        let path = repo.heddle_dir().join("remotes.toml");
        assert!(path.exists(), "expected remotes file to exist");

        let contents = fs::read_to_string(&path).expect("read remotes file");
        assert!(contents.contains("origin"));
        assert!(contents.contains("heddle.example:8421"));

        let reopened = RemoteConfig::open(&repo).expect("reopen config");
        let remote = reopened.get("origin").expect("load remote");
        assert_eq!(remote.url, "http://heddle.example:8421/repo");

        let _ = fs::remove_dir_all(temp);
    }
}
