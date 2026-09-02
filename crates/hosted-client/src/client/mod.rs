// SPDX-License-Identifier: Apache-2.0
//! CLI sync orchestration.
//!
//! Local-copy helpers and command-specific hosted sync glue live here. The
//! transport/session implementation remains private in `hosted_runtime`.

#[cfg(feature = "client")]
pub mod context_sync;
#[cfg(feature = "client")]
pub mod discussion_live;
#[cfg(feature = "client")]
pub mod discussion_sync;
#[cfg(feature = "client")]
pub mod human_signature;
pub mod local_sync;
#[cfg(feature = "client")]
pub mod repo_events;
#[cfg(feature = "client")]
pub mod review_sync;

pub use config::ClientConfig;
#[cfg(feature = "client")]
pub use human_signature::cli_human_signature_callback;
pub use local_sync::LocalSync;

#[cfg(feature = "client")]
pub use crate::hosted_runtime::{
    HostedAuthMode, HostedClient, HostedSession, connect_websocket, resolve_active_bearer,
    resolve_hosted_credential,
};
