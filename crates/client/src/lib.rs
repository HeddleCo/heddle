//! Heddle hosted-backend client and command implementations.
//!
//! Cli optionally links this crate when its `client` Cargo
//! feature is on. The trait surface that cli dispatches through lives
//! in `weft-client-shim`; this crate provides the real impls.

pub mod auth_args;
pub mod auth_cmd;
pub mod credentials;
pub mod device_flow;
pub mod grpc_hosted;
pub mod presence;
pub mod support;
pub mod support_args;

pub use auth_args::AuthCommands;
pub use auth_cmd::cmd_auth;
// Re-export `device_flow` under the historical `auth` module name so
// callers using `weft_client::auth::{...}` resolve symbols at the
// same path the cli used internally pre-move.
pub use device_flow as auth;
pub use grpc_hosted::HostedGrpcClient;
pub use presence::{
    PublisherConfig, cmd_presence_publish, resolve_publisher_config, run_publisher,
};
pub use support::run as cmd_support;
pub use support_args::{SupportCommands, SupportGrantArgs, SupportListArgs, SupportRevokeArgs};
