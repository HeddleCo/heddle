//! CLI-owned hosted runtime.
//!
//! Protocol contracts and canonical signing bytes come from `heddle-api`.
//! This module owns Heddle CLI application behavior: credentials applied to a
//! session, native Iroh transport, provider negotiation and download, and the
//! hosted command implementations. It is deliberately not a public transport
//! client surface.

mod agent_node_identity;
pub(crate) mod auth;
pub(crate) mod auth_requests;
mod claim_authorization;
#[cfg(test)]
mod claim_authorization_tests;
pub(crate) mod credential_file;
pub(crate) mod device_flow;
pub mod hosted;
pub(crate) mod identity;
mod identity_server;
mod identity_state;
#[cfg(test)]
mod identity_tests;
pub(crate) mod root_mint;
#[cfg(test)]
mod root_mint_tests;
pub mod websocket;
pub(crate) mod whoami;

pub use hosted::{
    HostedAuthMode, HostedClient, HostedSession, ServerStream, resolve_active_bearer,
    resolve_hosted_credential,
};
pub use websocket::connect_websocket;
