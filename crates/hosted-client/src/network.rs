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
