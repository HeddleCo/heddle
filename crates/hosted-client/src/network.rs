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
//! The daemon mounts native v2 endpoint and account-claim methods on this
//! endpoint and advertises its actual relay for browser links.

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

/// The persisted device node id, or `None` when the identity has
/// never been minted. Does not mint one as a side effect, so a status
/// probe stays read-only.
#[cfg(feature = "client")]
pub fn persisted_node_id() -> anyhow::Result<Option<EndpointId>> {
    crate::hosted_runtime::net_endpoint::persisted_node_id()
}

/// Locally advertised browser route, learned from the running endpoint.
#[cfg(feature = "client")]
#[derive(serde::Serialize, serde::Deserialize)]
struct DeviceReachability {
    node_id: String,
    relay_url: Option<String>,
    pid: u32,
}

#[cfg(feature = "client")]
fn reachability_path(heddle_home: &std::path::Path) -> std::path::PathBuf {
    repo::daemon::box_state_dir_in(heddle_home).join("heddle-netd.reachability.json")
}

/// Load the running local daemon identity without creating keys or dialing.
/// A stopped daemon must not delay ordinary local harness permission prompts.
#[cfg(feature = "client")]
pub fn running_device_node_id() -> anyhow::Result<Option<EndpointId>> {
    let path = reachability_path(&repo::identity::heddle_home_dir());
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata: DeviceReachability = serde_json::from_slice(&bytes)?;
    let Some(endpoint) = persisted_node_id()? else {
        return Ok(None);
    };
    Ok(
        (metadata.node_id == endpoint.to_string() && repo::daemon::pid_alive(metadata.pid))
            .then_some(endpoint),
    )
}

/// Keep claim-link relay metadata current as the device's home relay changes.
/// The daemon owns this task and aborts it before removing its discovery files.
#[cfg(feature = "client")]
pub async fn advertise_reachability(
    endpoint: Endpoint,
    heddle_home: std::path::PathBuf,
) -> anyhow::Result<()> {
    use futures::StreamExt as _;
    use n0_watcher::Watcher as _;
    let mut addresses = endpoint.watch_addr().stream();
    let path = reachability_path(&heddle_home);
    while let Some(address) = addresses.next().await {
        let metadata = DeviceReachability {
            node_id: endpoint.id().to_string(),
            relay_url: address.relay_urls().next().map(ToString::to_string),
            pid: std::process::id(),
        };
        let bytes = serde_json::to_vec(&metadata)?;
        objects::fs_atomic::write_file_atomic_secret(&path, &bytes)?;
    }
    Ok(())
}

#[cfg(feature = "client")]
pub(crate) fn claim_relay_url(node_id: &str) -> anyhow::Result<String> {
    use anyhow::Context as _;
    let path = reachability_path(&repo::identity::heddle_home_dir());
    let metadata: DeviceReachability =
        serde_json::from_slice(&std::fs::read(&path).context(
            "device reachability unavailable; start `heddle netd` and wait for its relay",
        )?)?;
    if metadata.node_id != node_id || !repo::daemon::pid_alive(metadata.pid) {
        anyhow::bail!("device reachability belongs to a different or stopped daemon");
    }
    metadata
        .relay_url
        .context("device has no reachable relay yet; wait for its relay connection")
}

#[cfg(feature = "client")]
pub fn remove_reachability(heddle_home: &std::path::Path) -> anyhow::Result<()> {
    let path = reachability_path(heddle_home);
    let data = match std::fs::read(&path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let metadata: DeviceReachability = serde_json::from_slice(&data)?;
    if metadata.pid == std::process::id() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(all(test, feature = "client"))]
mod tests {
    use super::*;

    #[test]
    fn default_relay_mode_is_heddle_custom_for_this_build_flavor() {
        let _process_env_guard = crate::test_process_env::shared_blocking();
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
}
