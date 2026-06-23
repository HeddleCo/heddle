//! Hosted-session open seam.
//!
//! "Open a usable hosted session for a remote" used to be hand-assembled at
//! every hosted entry point (push, pull, fetch, clone, support, approval,
//! lazy hydration): resolve the auth token, fall back to the credential
//! store, attach the proof key, build the validated client config, connect,
//! then run mandatory credential rotation. This module owns that assembly so
//! the command modules choose only intent + remote and call one seam.

use std::net::SocketAddr;

use anyhow::Result;
use cli_shared::{ClientConfig, UserConfig};
use wire::{AuthToken, ProtocolError};

use crate::{credentials, grpc_hosted::HostedGrpcClient};

/// How a hosted session resolves its auth token.
pub enum HostedAuthMode {
    /// Use only the token from env/user config (`remote_token`). Used by
    /// fetch, support, approval, clone, and lazy hydration.
    ConfigToken,
    /// Use the config token, falling back to the per-server credential store
    /// (and its proof key) when no config token is present. Used by push and
    /// pull.
    CredentialFallback,
}

/// A validated, connectable hosted-session configuration.
///
/// Building runs the fallible TLS/auth validation up front, so callers that
/// must not leave partial on-disk artifacts (clone, push) — or that build the
/// config on one thread and connect on another (lazy hydration) — can
/// prevalidate before any irreversible work, then `connect()` afterwards.
/// Callers with no such ordering constraint use
/// [`HostedGrpcClient::open_session`], which builds and connects in one call.
pub struct HostedSession {
    config: ClientConfig,
}

impl HostedSession {
    /// Resolve auth + build the validated client config for a hosted session.
    /// Owns credential-store fallback (per `mode`), server-key attachment, and
    /// proof-key attachment — the assembly the command modules used to
    /// hand-roll.
    pub fn build(
        user_config: &UserConfig,
        server_key: Option<String>,
        mode: HostedAuthMode,
    ) -> Result<Self> {
        let (token, credential_proof_key) = match mode {
            HostedAuthMode::ConfigToken => (user_config.remote_token()?, None),
            HostedAuthMode::CredentialFallback => {
                let mut token = user_config.remote_token()?;
                let mut credential_proof_key = None;
                if token.is_none()
                    && let Some(ref key) = server_key
                {
                    // Propagate a malformed credentials.toml instead of
                    // swallowing it: a parse error here (e.g. a missing
                    // `subject` field) used to fall through to an
                    // unauthenticated request, which the server rejects with
                    // the opaque "missing authorization metadata" — hiding the
                    // real cause. The `?` surfaces the underlying
                    // "parsing <path>: <toml error>" so the user can fix the
                    // file. A *missing* file still returns Ok(None) and falls
                    // back cleanly.
                    if let Some(cred) = credentials::resolve_credential_for_server(key)? {
                        token = Some(AuthToken::new(cred.token, "credential-store"));
                        credential_proof_key = cred.private_key_pem;
                    }
                }
                (token, credential_proof_key)
            }
        };

        let mut config = user_config.heddle_client_config(token)?;
        if let Some(key) = server_key {
            config = config.with_server_key(key);
        }
        if let Some(pem) = credential_proof_key
            && config.auth_proof_key_pem.is_none()
        {
            config = config.with_auth_proof_key_pem(pem);
        }
        Ok(Self { config })
    }

    /// Connect and run mandatory credential rotation.
    ///
    /// The rotation MUST run immediately after connect — every hosted entry
    /// point relies on a fresh token before its first RPC. This is the single
    /// place that pairs connect with rotation; see the source-presence guard
    /// in this module's tests.
    pub async fn connect(&self, addr: SocketAddr) -> Result<HostedGrpcClient, ProtocolError> {
        let mut client = HostedGrpcClient::connect(addr, &self.config).await?;
        client.auto_rotate_if_needed().await;
        Ok(client)
    }
}

impl HostedGrpcClient {
    /// Open a usable hosted session in one call: resolve auth, build the
    /// validated client config, connect, and run mandatory rotation. Callers
    /// that must prevalidate the config before irreversible work build a
    /// [`HostedSession`] first, then [`HostedSession::connect`].
    pub async fn open_session(
        addr: SocketAddr,
        user_config: &UserConfig,
        server_key: Option<String>,
        mode: HostedAuthMode,
    ) -> Result<Self> {
        Ok(HostedSession::build(user_config, server_key, mode)?
            .connect(addr)
            .await?)
    }
}

#[cfg(test)]
mod tests {
    //! The connect/rotate invariant now lives here, in the one seam every
    //! hosted entry point opens its session through. This source-presence
    //! guard replaces the per-call-site rotation checks: rotation MUST run
    //! immediately after `HostedGrpcClient::connect`, or a process whose
    //! cached token has slipped past expiry hits an auth failure on its first
    //! RPC even though the rotation data is on disk.

    #[test]
    fn session_connect_rotates_credentials_after_connect() {
        let source = include_str!("session.rs");
        let connect_idx = source
            .find("HostedGrpcClient::connect(addr, &self.config)")
            .expect("session.rs must connect with the resolved addr");
        let after_connect = &source[connect_idx..];
        let rotate_offset = after_connect
            .find("auto_rotate_if_needed")
            .expect("auto_rotate_if_needed must appear in session.rs");
        assert!(
            rotate_offset < 400,
            "auto_rotate_if_needed must follow HostedGrpcClient::connect within the \
             same async block (found {rotate_offset} chars later)",
        );
    }
}
