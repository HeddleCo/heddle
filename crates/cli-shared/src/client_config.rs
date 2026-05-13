// SPDX-License-Identifier: Apache-2.0
//! Client configuration.

use proto::AuthToken;

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
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self::new("heddle-client")
    }
}