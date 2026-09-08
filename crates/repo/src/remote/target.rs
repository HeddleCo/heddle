// SPDX-License-Identifier: Apache-2.0
//! Remote target resolution.

use std::{net::Ipv6Addr, path::PathBuf};

/// A remote target - either a network address or a local path.
#[derive(Debug, Clone)]
pub enum RemoteTarget {
    /// Network authority (host or host:port).
    Network {
        authority: String,
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
    /// - `https://host[:port]/repo` (port defaults to HTTPS 443)
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

        // Existing local directories win over scheme-less hosted paths.
        let path = PathBuf::from(s);
        if path.is_dir() {
            return Ok(RemoteTarget::Local(path));
        }

        if let Some((authority, repo_path)) = parse_network_with_repo_path(s) {
            return Ok(RemoteTarget::Network {
                authority,
                repo_path,
            });
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

    /// Parse a hosted target. Repository source authority does not change URL routing.
    pub fn parse_native(s: &str) -> Result<Self, String> {
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
            RemoteTarget::Network {
                authority,
                repo_path,
            } => {
                if let Some(repo_path) = repo_path {
                    write!(f, "https://{authority}/{repo_path}")
                } else {
                    write!(f, "{authority}")
                }
            }
            RemoteTarget::Local(path) => write!(f, "file://{}", path.display()),
        }
    }
}

fn parse_network_with_repo_path(s: &str) -> Option<(String, Option<String>)> {
    if s.ends_with(".git") || s.contains("://") && !s.starts_with("https://") {
        return None;
    }
    if let Some(rest) = s.strip_prefix("https://") {
        return parse_authority_with_repo_path(rest, true);
    }
    // A slash distinguishes a bare hosted path from a configured remote name.
    if looks_like_unresolved_local_path_prefix(s) {
        return None;
    }
    parse_authority_with_repo_path(s, s.contains('/'))
}

fn parse_authority_with_repo_path(
    s: &str,
    allow_default_https_port: bool,
) -> Option<(String, Option<String>)> {
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
    validate_authority(authority, allow_default_https_port)?;
    let repo_path = repo_path
        .filter(|path| !path.is_empty())
        .map(str::to_string);
    Some((authority.to_string(), repo_path))
}

fn validate_authority(authority: &str, allow_default_https_port: bool) -> Option<()> {
    if authority.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return None;
    }
    if let Some(bracketed) = authority.strip_prefix('[') {
        let (host, suffix) = bracketed.split_once(']')?;
        host.parse::<Ipv6Addr>().ok()?;
        return match suffix {
            "" if allow_default_https_port => Some(()),
            suffix if suffix.starts_with(':') => validate_port(&suffix[1..]),
            _ => None,
        };
    }
    if authority.contains(['[', ']']) {
        return None;
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && !host.contains(':') => validate_port(port),
        None if allow_default_https_port && !authority.is_empty() => Some(()),
        _ => None,
    }
}

fn validate_port(port: &str) -> Option<()> {
    port.parse::<u16>().ok().map(|_| ())
}

fn looks_like_unresolved_local_path_prefix(value: &str) -> bool {
    value.starts_with('/')
        || value.starts_with("./")
        || value.starts_with("../")
        || value.starts_with("~/")
        || value.contains('\\')
}

fn looks_like_unresolved_local_path(value: &str) -> bool {
    looks_like_unresolved_local_path_prefix(value)
        || (!value.contains("://") && !value.contains(':'))
}

#[cfg(test)]
mod tests {
    use super::RemoteTarget;

    #[test]
    fn hosted_url_preserves_hostname_authority() {
        let target = RemoteTarget::parse("https://api-staging.heddle.sh/org/repo")
            .expect("parse hosted hostname");
        match target {
            RemoteTarget::Network {
                authority,
                repo_path,
            } => {
                assert_eq!(authority, "api-staging.heddle.sh");
                assert_eq!(repo_path.as_deref(), Some("org/repo"));
                assert!(authority.parse::<std::net::IpAddr>().is_err());
            }
            other => panic!("expected network target, got {other:?}"),
        }
    }

    #[test]
    fn parses_hostname_without_repo_path() {
        let target = RemoteTarget::parse("localhost:8421").expect("parse localhost");
        match target {
            RemoteTarget::Network {
                authority,
                repo_path,
            } => {
                assert_eq!(authority, "localhost:8421");
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
            RemoteTarget::Network {
                authority,
                repo_path,
            } => {
                assert_eq!(authority, "localhost:8421");
                assert_eq!(repo_path.as_deref(), Some("acme/heddle"));
            }
            other => panic!("expected network target, got {other:?}"),
        }
    }

    #[test]
    fn native_and_generic_parsers_accept_hosted_https() {
        assert!(RemoteTarget::parse("https://127.0.0.1:8431/acme/heddle").is_ok());

        let target = RemoteTarget::parse_native("https://127.0.0.1:8431/acme/heddle")
            .expect("parse native HTTPS URL");
        match target {
            RemoteTarget::Network {
                authority,
                repo_path,
            } => {
                assert_eq!(authority, "127.0.0.1:8431");
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
            RemoteTarget::Network { authority, .. } => assert_eq!(authority, "127.0.0.1"),
            other => panic!("expected network target, got {other:?}"),
        }
    }

    #[test]
    fn https_without_port_uses_default_https_port() {
        assert!(RemoteTarget::parse("https://api.heddle.sh/luke/tiny-notes").is_ok());
        assert!(RemoteTarget::parse_native("https://api.heddle.sh/luke/tiny-notes").is_ok());
        assert!(RemoteTarget::parse("https://127.0.0.1:8421/luke/tiny-notes").is_ok());
    }

    #[test]
    fn nonexistent_local_paths_fail_closed() {
        let missing = "/tmp/heddle-missing-remote-path-does-not-exist";
        assert!(RemoteTarget::parse(missing).is_err());
        assert!(RemoteTarget::parse(&format!("file://{missing}")).is_err());
        assert!(RemoteTarget::parse("tiny-notes-mirror").is_err());
    }
}
