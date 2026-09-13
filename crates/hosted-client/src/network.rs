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

/// Home relay for this build flavor. Trailing slash matches the
/// signed endpoint-descriptor encoding.
///
/// Split on `debug_assertions` — the existing cargo profile gate
/// (`cargo test` / `cargo build` vs `cargo build --release`). Dev and
/// preview-shaped debug binaries hardcode preview only; shipped
/// `--release` binaries hardcode production only. The unused URL is
/// cfg'd out, so a release binary cannot embed preview and a debug
/// binary cannot embed prod.
///
/// Hosted CLI connections still take their relay list from the signed
/// descriptor (`RelayMode::custom`). This constant is only the netd
/// home-relay map when no descriptor is in hand. Never
/// [`RelayMode::Default`]: that map is n0's `*.relay.n0.iroh.link`.
#[cfg(all(feature = "client", debug_assertions))]
const HEDDLE_HOME_RELAY_URL: &str = "https://relay.preview.heddle.sh/";

#[cfg(all(feature = "client", not(debug_assertions)))]
const HEDDLE_HOME_RELAY_URL: &str = "https://relay.heddle.sh/";

/// Relay mode that keeps the endpoint reachable through this build's
/// Heddle home relay.
///
/// The persistent endpoint must stay relay-reachable: a browser
/// holding only a claim link has no direct path to the machine, so it
/// dials the advertised node id through a relay. Binding with
/// [`RelayMode::Disabled`] would strand exactly that caller. The
/// daemon keeps this custom map online for its whole lifetime.
#[cfg(feature = "client")]
pub fn default_relay_mode() -> RelayMode {
    let url = HEDDLE_HOME_RELAY_URL.parse().unwrap_or_else(|error| {
        // Crate constant. A parse failure is a programming error, not
        // a runtime condition; falling through to Default would put
        // netd on n0's map.
        panic!("HEDDLE_HOME_RELAY_URL {HEDDLE_HOME_RELAY_URL:?} must parse as RelayUrl: {error}")
    });
    RelayMode::custom([url])
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

#[cfg(all(test, feature = "client"))]
mod tests {
    use super::*;

    #[test]
    fn default_relay_mode_is_heddle_custom_for_this_build_flavor() {
        let mode = default_relay_mode();
        assert!(
            matches!(mode, RelayMode::Custom(_)),
            "netd must bind RelayMode::Custom, got {mode:?}"
        );

        let urls: Vec<iroh::RelayUrl> = mode.relay_map().urls();
        assert_eq!(
            urls.len(),
            1,
            "this build flavor must hardcode exactly one home relay, got {urls:?}"
        );

        let host = urls[0]
            .host_str()
            .unwrap_or("")
            .trim_end_matches('.')
            .to_string();
        assert!(
            !host.ends_with("n0.iroh.link") && host != "n0.iroh.link",
            "n0 default relay leaked into netd bind: {host}"
        );

        #[cfg(debug_assertions)]
        {
            assert_eq!(HEDDLE_HOME_RELAY_URL, "https://relay.preview.heddle.sh/");
            assert!(
                !HEDDLE_HOME_RELAY_URL.contains("://relay.heddle.sh"),
                "debug/dev build must not embed the production relay"
            );
            assert_eq!(host, "relay.preview.heddle.sh");
        }
        #[cfg(not(debug_assertions))]
        {
            assert_eq!(HEDDLE_HOME_RELAY_URL, "https://relay.heddle.sh/");
            assert!(
                !HEDDLE_HOME_RELAY_URL.contains("preview"),
                "release build must not embed the preview relay"
            );
            assert_eq!(host, "relay.heddle.sh");
        }
    }
}
