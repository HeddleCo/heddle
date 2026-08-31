// SPDX-License-Identifier: Apache-2.0
//! Stable crate boundary for the machine's single, persistent Iroh
//! endpoint — the device-wide network foundation the box network
//! daemon binds (heddle#1533).
//!
//! The endpoint is bound on the *persisted* device node id
//! (`hosted_runtime::agent_node_identity`), so its cryptographic
//! address survives process restarts — the acceptance clause the
//! browser claim link depends on (heddle#1620).
//!
//! ## Seam for piece 3 (heddle#1620)
//!
//! [`bind_persistent_endpoint`] returns the raw [`Endpoint`] and this
//! module re-exports iroh's [`Router`], so the claim protocol can be
//! mounted on the running endpoint without reaching into this crate's
//! internals:
//!
//! ```ignore
//! use hosted_client::network::{bind_persistent_endpoint, default_relay_mode, Router};
//!
//! let endpoint = bind_persistent_endpoint(default_relay_mode()).await?;
//! // piece 3: mount the claim ALPN on the live endpoint
//! let router = Router::builder(endpoint.clone())
//!     .accept(CLAIM_ALPN_V1, claim_protocol)
//!     .spawn();
//! ```
//!
//! The surface is deliberately narrow: bind, read the node id, choose
//! a relay mode, and (via the re-exports) attach a router. Everything
//! else about the endpoint stays private to the hosted runtime.

#[cfg(feature = "client")]
pub use iroh::protocol::Router;
#[cfg(feature = "client")]
pub use iroh::{Endpoint, EndpointId, RelayMode};

/// Owner-root claim router hosted on the daemon endpoint, and the socket
/// convention for bridging its co-sign step to a foreground signer
/// (heddle#1620, piece 3). See [`crate::hosted_runtime::claim_bridge`].
#[cfg(feature = "client")]
pub use crate::hosted_runtime::claim_bridge::{
    DaemonClaimRouter, claim_bridge_socket_path, mount_claim_router,
};

/// Relay mode that keeps the endpoint reachable through the default
/// (number-0) relay servers.
///
/// The persistent endpoint must stay relay-reachable: a browser
/// holding only a claim link has no direct path to the machine, so it
/// dials the advertised node id through a relay. Binding with
/// [`RelayMode::Disabled`] would strand exactly that caller. Piece 2
/// (weft subscription) will be able to pass a signed
/// [`RelayMode::Custom`] set instead; the daemon keeps whatever relay
/// mode it was bound with online for its whole lifetime.
#[cfg(feature = "client")]
pub fn default_relay_mode() -> RelayMode {
    RelayMode::Default
}

/// Bind the machine's single persistent Iroh endpoint on the device
/// node id, staying reachable through `relay_mode`.
///
/// The returned [`Endpoint`] must be kept alive for as long as the
/// endpoint should serve; dropping it (or calling
/// [`Endpoint::close`]) tears down the relay connection. The device
/// identity is loaded-or-minted once and reused on every subsequent
/// bind, so restarting the process rebinds the same node id.
#[cfg(feature = "client")]
pub async fn bind_persistent_endpoint(relay_mode: RelayMode) -> anyhow::Result<Endpoint> {
    crate::hosted_runtime::net_endpoint::bind(relay_mode).await
}

/// The persisted device node id, or `None` when the identity has
/// never been minted. Does not mint one as a side effect, so a status
/// probe stays read-only.
#[cfg(feature = "client")]
pub fn persisted_node_id() -> anyhow::Result<Option<EndpointId>> {
    crate::hosted_runtime::net_endpoint::persisted_node_id()
}
