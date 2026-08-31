// SPDX-License-Identifier: Apache-2.0
//! Binding the machine's single persistent Iroh endpoint on the
//! *persisted* device node id.
//!
//! This is the private implementation behind the crate-public
//! [`crate::network`] boundary. It deliberately reuses
//! [`super::agent_node_identity`] — the same stable secret key the
//! hosted connection binds — so the box network daemon
//! (heddle#1533) advertises a node id that survives process
//! restarts, which is the acceptance clause the browser claim link
//! relies on (heddle#1620). Minting a fresh key here would silently
//! invalidate every outstanding claim URL.

use anyhow::{Context, Result};
use iroh::{Endpoint, EndpointId, RelayMode, endpoint::presets};

use super::agent_node_identity;

/// Bind an endpoint on the persisted device node id, reachable
/// through `relay_mode`. The identity is loaded-or-minted exactly
/// once (serialized by the identity write lock); every subsequent
/// bind — including after a restart — reuses the same secret key and
/// therefore the same node id.
pub(crate) async fn bind(relay_mode: RelayMode) -> Result<Endpoint> {
    let identity = agent_node_identity::load_or_create()
        .context("loading persisted device node identity")?;
    Endpoint::builder(presets::Minimal)
        .relay_mode(relay_mode)
        .secret_key(identity.secret_key())
        .bind()
        .await
        .context("binding persistent device iroh endpoint")
}

/// The persisted device node id, or `None` when the identity has
/// never been minted. Reads the identity without minting one, so a
/// status probe does not create credential material as a side
/// effect.
pub(crate) fn persisted_node_id() -> Result<Option<EndpointId>> {
    Ok(agent_node_identity::load()
        .context("reading persisted device node identity")?
        .map(|identity| identity.node_id()))
}
