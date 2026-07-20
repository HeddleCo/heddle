// SPDX-License-Identifier: Apache-2.0
//! Heddle protocol client.
//!
//! Connects to Heddle servers for push, pull, and other operations.

#[cfg(feature = "client")]
pub mod context_sync;
#[cfg(feature = "client")]
pub mod discussion_sync;
#[cfg(feature = "client")]
pub mod human_signature;
pub mod local_sync;
#[cfg(feature = "client")]
pub mod review_sync;

pub use cli_shared::ClientConfig;
#[cfg(feature = "client")]
pub use heddle_client::{HostedAuthMode, HostedClient, HostedSession};
#[cfg(feature = "client")]
pub use human_signature::cli_human_signature_callback;
pub use local_sync::LocalSync;
