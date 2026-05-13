// SPDX-License-Identifier: Apache-2.0
//! Remote target resolution.

use std::{
    net::{SocketAddr, ToSocketAddrs},
    path::PathBuf,
};

/// A remote target - either a network address or a local path.
#[derive(Debug, Clone)]
pub enum RemoteTarget {
    /// Network address (host:port).
    Network {
        addr: SocketAddr,
        repo_path: Option<String>,
    },
    /// Local filesystem path (file:// URL).
    Local(PathBuf),
}

impl RemoteTarget {
    /// Parse from a string.
    ///
    /// Accepts:
    /// - `file:///path/to/repo` or `file://path/to/repo`
    /// - `/path/to/repo` (raw path, if it exists as a directory)
    /// - `host:port` (network address)
    pub fn parse(s: &str) -> Result<Self, String> {
        // Check for file:// protocol
        if let Some(path) = s.strip_prefix("file://") {
            return Ok(RemoteTarget::Local(PathBuf::from(path)));
        }

        if let Some((addr, repo_path)) = parse_network_with_repo_path(s) {
            return Ok(RemoteTarget::Network { addr, repo_path });
        }

        // Check if it's a raw path (exists as a directory)
        let path = PathBuf::from(s);
        if path.exists() && path.is_dir() {
            return Ok(RemoteTarget::Local(path));
        }

        Err(format!(
            "invalid remote url (expected file://path or host:port): {}",
            s
        ))
    }

    /// Check if this is a local target.
    pub fn is_local(&self) -> bool {
        matches!(self, RemoteTarget::Local(_))
    }

    /// Check if this is a network target.
    pub fn is_network(&self) -> bool {
        matches!(self, RemoteTarget::Network { .. })
    }
}

impl std::fmt::Display for RemoteTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RemoteTarget::Network { addr, repo_path } => {
                if let Some(repo_path) = repo_path {
                    write!(f, "heddle://{}/{}", addr, repo_path)
                } else {
                    write!(f, "{}", addr)
                }
            }
            RemoteTarget::Local(path) => write!(f, "file://{}", path.display()),
        }
    }
}

fn parse_network_with_repo_path(s: &str) -> Option<(SocketAddr, Option<String>)> {
    if let Some(rest) = s.strip_prefix("heddle://") {
        return parse_network_with_repo_path(rest);
    }

    if let Ok(addr) = s.parse::<SocketAddr>() {
        return Some((addr, None));
    }

    if let Some(addr) = resolve_socket_addr(s) {
        return Some((addr, None));
    }

    let slash = s.find('/')?;
    let (addr_part, repo_part) = s.split_at(slash);
    let addr = resolve_socket_addr(addr_part)?;
    let repo_path = repo_part.trim_start_matches('/');
    if repo_path.is_empty() {
        return Some((addr, None));
    }
    Some((addr, Some(repo_path.to_string())))
}

fn resolve_socket_addr(addr: &str) -> Option<SocketAddr> {
    if let Ok(parsed) = addr.parse::<SocketAddr>() {
        return Some(parsed);
    }

    addr.to_socket_addrs().ok()?.next()
}

#[cfg(test)]
mod tests {
    use super::RemoteTarget;

    #[test]
    fn parses_hostname_without_repo_path() {
        let target = RemoteTarget::parse("localhost:8421").expect("parse localhost");
        match target {
            RemoteTarget::Network { addr, repo_path } => {
                assert_eq!(addr.port(), 8421);
                assert!(addr.ip().is_loopback());
                assert!(repo_path.is_none());
            }
            other => panic!("expected network target, got {other:?}"),
        }
    }

    #[test]
    fn parses_hostname_with_repo_path() {
        let target =
            RemoteTarget::parse("localhost:8421/acme/heddle").expect("parse localhost repo path");
        match target {
            RemoteTarget::Network { addr, repo_path } => {
                assert_eq!(addr.port(), 8421);
                assert!(addr.ip().is_loopback());
                assert_eq!(repo_path.as_deref(), Some("acme/heddle"));
            }
            other => panic!("expected network target, got {other:?}"),
        }
    }
}