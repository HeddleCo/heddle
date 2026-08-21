//! CLI-owned hosted runtime.
//!
//! Protocol contracts and canonical signing bytes come from `heddle-api`.
//! This module owns Heddle CLI application behavior: credentials applied to a
//! session, native Iroh transport, provider negotiation and download, and the
//! hosted command implementations. It is deliberately not a public transport
//! client surface.

mod agent_node_identity;
pub(crate) mod auth;
mod auth_login;
mod auth_login_agent;
#[cfg(test)]
mod auth_login_tests;
pub(crate) mod auth_requests;
mod claim_authorization;
#[cfg(test)]
mod claim_authorization_tests;
pub(crate) mod credential_file;
pub(crate) mod device_flow;
pub(crate) mod hosted;
mod identity_state;
mod root_mint;
#[cfg(test)]
mod root_mint_tests;
pub(crate) mod websocket;
pub(crate) mod whoami;

pub(crate) use auth::cmd_auth;
pub(crate) use hosted::{
    HostedAuthMode, HostedClient, HostedSession, ServerStream, resolve_active_bearer,
    resolve_hosted_credential,
};
pub(crate) use websocket::connect_websocket;
