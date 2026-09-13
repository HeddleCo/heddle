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
#[cfg(feature = "client")]
pub use crate::hosted_runtime::hosted::hosted_bridge::{HostedBridge, hosted_bridge_socket_path};

/// Home relay for this build flavor. Trailing slash matches the
/// signed endpoint-descriptor encoding.
///
/// Split on the `preview` cargo feature (forwarded from `heddle-cli`):
/// stock / `--release` without the feature hardcodes production only;
/// `--features preview` hardcodes preview only. The unused URL is
/// cfg'd out, so a production binary cannot embed preview and a
/// preview binary cannot embed prod.
///
/// Hosted CLI connections still take their relay list from the signed
/// descriptor (`RelayMode::custom`). This constant is only the netd
/// home-relay map when no descriptor is in hand. Never
/// [`RelayMode::Default`]: that map is n0's `*.relay.n0.iroh.link`.
#[cfg(all(feature = "client", feature = "preview"))]
const HEDDLE_HOME_RELAY_URL: &str = "https://relay.preview.heddle.sh/";

#[cfg(all(feature = "client", not(feature = "preview")))]
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

/// Bind the persistent device endpoint together with the hosted-session
/// bridge that reuses it for CLI weft calls.
#[cfg(feature = "client")]
pub async fn bind_persistent_hosted(
    relay_mode: RelayMode,
) -> anyhow::Result<(
    Endpoint,
    crate::hosted_runtime::hosted::hosted_bridge::HostedBridge,
)> {
    let persistent = crate::hosted_runtime::net_endpoint::bind_with_provider(relay_mode).await?;
    let bridge = crate::hosted_runtime::hosted::hosted_bridge::HostedBridge::new(
        persistent.endpoint.clone(),
        Some(persistent.provider_transport),
    );
    Ok((persistent.endpoint, bridge))
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

        #[cfg(feature = "preview")]
        {
            assert_eq!(HEDDLE_HOME_RELAY_URL, "https://relay.preview.heddle.sh/");
            assert!(
                !HEDDLE_HOME_RELAY_URL.contains("://relay.heddle.sh"),
                "preview feature must not embed the production relay"
            );
            assert_eq!(host, "relay.preview.heddle.sh");
        }
        #[cfg(not(feature = "preview"))]
        {
            assert_eq!(HEDDLE_HOME_RELAY_URL, "https://relay.heddle.sh/");
            assert!(
                !HEDDLE_HOME_RELAY_URL.contains("preview"),
                "default/release build must not embed the preview relay"
            );
            assert_eq!(host, "relay.heddle.sh");
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn bind_persistent_hosted_uses_the_device_endpoint() {
        let _env_guard = config::credentials::lock_test_env();
        let home = tempfile::TempDir::new().expect("temp Heddle home");
        let previous = std::env::var_os("HEDDLE_HOME");
        unsafe {
            std::env::set_var("HEDDLE_HOME", home.path());
        }
        let (endpoint, _bridge) = bind_persistent_hosted(RelayMode::Disabled)
            .await
            .expect("persistent hosted bind");
        let node_id = persisted_node_id()
            .expect("read persisted node id")
            .expect("bind must mint a device identity");
        assert_eq!(endpoint.id(), node_id);
        endpoint.close().await;
        match previous {
            Some(value) => unsafe { std::env::set_var("HEDDLE_HOME", value) },
            None => unsafe { std::env::remove_var("HEDDLE_HOME") },
        }
    }
}
