//! Heddle remote-client command implementations.
//!
//! Cli optionally links this crate when its `client` Cargo
//! feature is on.

pub mod auth_args;
pub mod auth_cmd;
pub mod credentials;
pub mod device_flow;
pub mod grpc_remote;
pub mod presence;
pub mod support;
pub mod support_args;

pub use auth_args::AuthCommands;
pub use auth_cmd::cmd_auth;
pub use grpc_remote::{RemoteAuthMode, RemoteGrpcClient, RemoteSession};
pub use presence::{
    PublisherConfig, cmd_presence_publish, resolve_publisher_config, run_publisher,
};
pub use support::run as cmd_support;
pub use support_args::{SupportCommands, SupportGrantArgs, SupportListArgs, SupportRevokeArgs};
