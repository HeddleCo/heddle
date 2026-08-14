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
pub(crate) mod credential_file;
pub(crate) mod device_flow;
pub(crate) mod hosted;
pub(crate) mod websocket;
pub(crate) mod whoami;

pub(crate) use auth::cmd_auth;
pub(crate) use hosted::{
    HostedAuthMode, HostedClient, HostedSession, ServerStream, resolve_active_bearer,
    resolve_hosted_credential,
};
pub(crate) use websocket::connect_websocket;
