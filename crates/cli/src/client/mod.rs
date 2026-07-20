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
#[cfg(feature = "client")]
pub mod review_sync;
pub mod local_daemon;
pub mod local_sync;

pub use cli_shared::ClientConfig;
#[cfg(feature = "client")]
pub use heddle_client::{
    CredentialSource, HostedGrpcClient, HostedSession, ResolvedHostedCredential,
    resolve_active_bearer, resolve_hosted_credential,
};
#[cfg(feature = "client")]
pub use human_signature::cli_human_signature_callback;
#[cfg(unix)]
pub use local_daemon::{
    LocalDaemonChannel, connect_local_daemon_channel, detect_local_daemon_with_connect_probe,
};
pub use local_daemon::{UdsTarget, detect_local_daemon};
pub use local_sync::LocalSync;
