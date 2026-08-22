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
            let path = PathBuf::from(path);
            if path.is_dir() {
                return Ok(RemoteTarget::Local(path));
            }
            return Err(format!(
                "invalid remote url (local path does not exist): {s}"
            ));
        }

        if let Some((addr, repo_path)) = parse_network_with_repo_path(s) {
            return Ok(RemoteTarget::Network { addr, repo_path });
        }

        // Check if it's a raw path (exists as a directory)
        let path = PathBuf::from(s);
        if path.exists() && path.is_dir() {
            return Ok(RemoteTarget::Local(path));
        }

        if s.starts_with("heddle://") {
            return Err(format!(
                "invalid remote url: {s} (`heddle://` carries no addressing that push can use; use https://<host>/<repo> or host:port/repo)"
            ));
        }

        if looks_like_unresolved_local_path(s) {
            return Err(format!(
                "invalid remote url (local path does not exist): {s}"
            ));
        }

        Err(format!(
            "invalid remote url (expected file://path or host:port): {}",
            s
        ))
    }

    /// Parse a target under native repository source authority.
    ///
    /// Native repositories may use an HTTPS repository URL after the caller
    /// has verified the server's well-known Iroh endpoint. The regular parser
    /// deliberately keeps treating HTTPS as non-native so Git-owned callers
    /// retain their existing transport classification.
    pub fn parse_native(s: &str) -> Result<Self, String> {
        if let Some(rest) = s.strip_prefix("https://") {
            let (addr, repo_path) = parse_https_network_with_repo_path(rest)
                .ok_or_else(|| format!("invalid native HTTPS remote url: {s}"))?;
            return Ok(RemoteTarget::Network { addr, repo_path });
        }
        Self::parse(s)
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

fn parse_https_network_with_repo_path(s: &str) -> Option<(SocketAddr, Option<String>)> {
    if s.is_empty() || s.contains(['?', '#', '@']) {
        return None;
    }
    let (authority, repo_path) = match s.split_once('/') {
        Some((authority, path)) => (authority, Some(path.trim_matches('/'))),
        None => (s, None),
    };
    if authority.is_empty() {
        return None;
    }
    let addr = resolve_socket_addr(authority).or_else(|| {
        let host = authority
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(authority);
        (host, 443).to_socket_addrs().ok()?.next()
    })?;
    let repo_path = repo_path
        .filter(|path| !path.is_empty())
        .map(str::to_string);
    Some((addr, repo_path))
}

fn resolve_socket_addr(addr: &str) -> Option<SocketAddr> {
    if let Ok(parsed) = addr.parse::<SocketAddr>() {
        return Some(parsed);
    }

    addr.to_socket_addrs().ok()?.next()
}

fn looks_like_unresolved_local_path(value: &str) -> bool {
    value.starts_with('/')
        || value.starts_with("./")
        || value.starts_with("../")
        || value.starts_with("~/")
        || value.contains('\\')
        || (!value.contains("://") && !value.contains(':'))
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

    #[test]
    fn native_parser_accepts_https_without_changing_generic_classification() {
        assert!(RemoteTarget::parse("https://127.0.0.1:8431/acme/heddle").is_err());

        let target = RemoteTarget::parse_native("https://127.0.0.1:8431/acme/heddle")
            .expect("parse native HTTPS URL");
        match target {
            RemoteTarget::Network { addr, repo_path } => {
                assert_eq!(addr, "127.0.0.1:8431".parse().unwrap());
                assert_eq!(repo_path.as_deref(), Some("acme/heddle"));
            }
            other => panic!("expected network target, got {other:?}"),
        }
    }

    #[test]
    fn native_https_parser_defaults_to_port_443() {
        let target =
            RemoteTarget::parse_native("https://127.0.0.1/acme/heddle").expect("parse HTTPS URL");
        match target {
            RemoteTarget::Network { addr, .. } => assert_eq!(addr.port(), 443),
            other => panic!("expected network target, got {other:?}"),
        }
    }

    #[test]
    fn heddle_scheme_without_port_is_not_a_push_url() {
        let error = RemoteTarget::parse("heddle://api.heddle.sh/luke/tiny-notes")
            .expect_err("schemeless heddle:// host has no addressing");
        assert!(
            error.contains("heddle://") && error.contains("no addressing"),
            "refusal must name the scheme gap: {error}"
        );
        assert!(RemoteTarget::parse_native("heddle://api.heddle.sh/luke/tiny-notes").is_err());
        assert!(RemoteTarget::parse("heddle://127.0.0.1:8421/luke/tiny-notes").is_ok());
    }

    #[test]
    fn nonexistent_local_paths_fail_closed() {
        let missing = "/tmp/heddle-missing-remote-path-does-not-exist";
        assert!(RemoteTarget::parse(missing).is_err());
        assert!(RemoteTarget::parse(&format!("file://{missing}")).is_err());
        assert!(RemoteTarget::parse("tiny-notes-mirror").is_err());
    }
}
