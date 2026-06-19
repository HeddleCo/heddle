// SPDX-License-Identifier: Apache-2.0
//! Heddle protocol client.
//!
//! Connects to Heddle servers for push, pull, and other operations.

#[cfg(feature = "local-services")]
pub mod local_daemon;
pub mod local_sync;

pub use cli_shared::ClientConfig;
#[cfg(feature = "client")]
pub use heddle_client::{RemoteAuthMode, RemoteGrpcClient, RemoteSession};
#[cfg(all(unix, feature = "local-services"))]
pub use local_daemon::{
    LocalDaemonChannel, connect_local_daemon_channel, detect_local_daemon_with_connect_probe,
};
#[cfg(feature = "local-services")]
pub use local_daemon::{UdsTarget, detect_local_daemon};
pub use local_sync::LocalSync;
