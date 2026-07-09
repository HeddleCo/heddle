// SPDX-License-Identifier: Apache-2.0
//! Client configuration.

use std::net::{IpAddr, SocketAddr};

use wire::AuthToken;

/// Client configuration.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Client identifier.
    pub client_id: String,
    /// Authentication token.
    pub token: Option<AuthToken>,
    /// Optional private key used to prove possession of a bound token.
    pub auth_proof_key_pem: Option<String>,
    /// Server key used to look up the credential in the credential store
    /// (`~/.heddle/credentials.toml`).  Matches the key used by `heddle auth login`.
    pub server_key: Option<String>,
    /// Enable TLS.
    pub tls_enabled: bool,
    /// Override the expected TLS server name.
    pub tls_domain_name: Option<String>,
    /// Optional PEM CA certificate bundle for server verification.
    pub tls_ca_certificate_pem: Option<String>,
    /// Skip TLS certificate verification (insecure).
    pub tls_skip_verify: bool,
    /// Explicitly allow cleartext (non-TLS) connections to non-loopback hosts.
    ///
    /// Loopback cleartext is always permitted for local development. Non-loopback
    /// cleartext requires this flag (CLI `--insecure`, remote `insecure = true`,
    /// user config, or `HEDDLE_REMOTE_INSECURE`).
    pub allow_insecure: bool,
    /// Connection timeout in seconds.
    pub timeout_secs: u64,
    /// Enable compression.
    pub compression: bool,
    /// Preferred chunk size.
    pub chunk_size: usize,
    /// Enable chunked transfer negotiation.
    pub chunked_transfer: bool,
    /// Enable resumable transfer negotiation.
    pub resumable_transfer: bool,
    /// Enable pack transfer negotiation.
    pub pack_transfer: bool,
    /// Enable partial fetch negotiation.
    pub partial_fetch: bool,
}

impl ClientConfig {
    /// Create a new client configuration.
    pub fn new(client_id: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            token: None,
            auth_proof_key_pem: None,
            server_key: None,
            tls_enabled: false,
            tls_domain_name: None,
            tls_ca_certificate_pem: None,
            tls_skip_verify: false,
            allow_insecure: false,
            timeout_secs: 30,
            compression: true,
            chunk_size: 64 * 1024,
            chunked_transfer: true,
            resumable_transfer: true,
            pack_transfer: true,
            partial_fetch: true,
        }
    }

    /// Set authentication token.
    pub fn with_token(mut self, token: AuthToken) -> Self {
        self.token = Some(token);
        self
    }

    /// Set a private key used for proof-of-possession metadata.
    pub fn with_auth_proof_key_pem(mut self, pem: impl Into<String>) -> Self {
        self.auth_proof_key_pem = Some(pem.into());
        self
    }

    /// Set the server key used to look up credentials in the credential store.
    pub fn with_server_key(mut self, key: impl Into<String>) -> Self {
        self.server_key = Some(key.into());
        self
    }

    /// Enable TLS.
    pub fn with_tls(mut self, skip_verify: bool) -> Self {
        self.tls_enabled = true;
        self.tls_skip_verify = skip_verify;
        self
    }

    pub fn with_tls_domain_name(mut self, domain_name: impl Into<String>) -> Self {
        self.tls_domain_name = Some(domain_name.into());
        self
    }

    pub fn with_tls_ca_certificate_pem(mut self, pem: impl Into<String>) -> Self {
        self.tls_enabled = true;
        self.tls_ca_certificate_pem = Some(pem.into());
        self
    }

    /// Set timeout.
    pub fn with_timeout(mut self, secs: u64) -> Self {
        self.timeout_secs = secs;
        self
    }

    /// Disable compression.
    pub fn without_compression(mut self) -> Self {
        self.compression = false;
        self
    }

    /// Set chunk size.
    pub fn with_chunk_size(mut self, size: usize) -> Self {
        self.chunk_size = size;
        self
    }

    /// Enable or disable chunked transfer negotiation.
    pub fn with_chunked_transfer(mut self, enabled: bool) -> Self {
        self.chunked_transfer = enabled;
        self
    }

    /// Enable or disable resumable transfer negotiation.
    pub fn with_resumable_transfer(mut self, enabled: bool) -> Self {
        self.resumable_transfer = enabled;
        self
    }

    /// Enable or disable pack transfer negotiation.
    pub fn with_pack_transfer(mut self, enabled: bool) -> Self {
        self.pack_transfer = enabled;
        self
    }

    /// Enable or disable partial fetch negotiation.
    pub fn with_partial_fetch(mut self, enabled: bool) -> Self {
        self.partial_fetch = enabled;
        self
    }

    /// Explicitly allow cleartext to non-loopback addresses.
    pub fn with_allow_insecure(mut self, allow: bool) -> Self {
        self.allow_insecure = allow;
        self
    }
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self::new("heddle-client")
    }
}

/// True for loopback IPs (`127.0.0.0/8`, `::1`).
pub fn is_loopback_ip(ip: IpAddr) -> bool {
    ip.is_loopback()
}

/// Whether a cleartext client connection to `addr` is permitted.
///
/// - TLS enabled → always allowed
/// - Cleartext to loopback → allowed (local dev)
/// - Cleartext to non-loopback → only when `allow_insecure` is set
pub fn cleartext_connect_allowed(
    addr: SocketAddr,
    tls_enabled: bool,
    allow_insecure: bool,
) -> bool {
    tls_enabled || is_loopback_ip(addr.ip()) || allow_insecure
}

/// Error message when refusing non-loopback cleartext without an explicit opt-in.
pub fn cleartext_refused_message(addr: SocketAddr) -> String {
    format!(
        "refusing cleartext connection to non-loopback address {addr}; \
enable TLS (remote.tls_enabled / HEDDLE_REMOTE_TLS) or pass --insecure \
(or set remote.insecure=true / HEDDLE_REMOTE_INSECURE=1) for intentional cleartext \
(e.g. VPN → VPS testing)"
    )
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

    use super::*;

    #[test]
    fn loopback_cleartext_allowed_without_insecure() {
        let v4 = SocketAddr::from((Ipv4Addr::LOCALHOST, 8421));
        let v6 = SocketAddr::from((Ipv6Addr::LOCALHOST, 8421));
        assert!(cleartext_connect_allowed(v4, false, false));
        assert!(cleartext_connect_allowed(v6, false, false));
    }

    #[test]
    fn non_loopback_cleartext_requires_insecure() {
        let addr = SocketAddr::from((Ipv4Addr::new(203, 0, 113, 10), 8421));
        assert!(!cleartext_connect_allowed(addr, false, false));
        assert!(cleartext_connect_allowed(addr, false, true));
        assert!(cleartext_connect_allowed(addr, true, false));
    }

    #[test]
    fn is_loopback_classifies_hosts() {
        assert!(is_loopback_ip(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
        assert!(is_loopback_ip(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(!is_loopback_ip(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
    }
}
