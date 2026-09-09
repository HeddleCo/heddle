//! Native hosted runtime.
//!
//! Protocol contracts and canonical signing bytes come from `heddle-api`.
//! This module owns credentials applied to a session, native Iroh transport,
//! provider negotiation, and hosted identity operations. Process inputs enter
//! as resolved values; clap interpretation and presentation stay in caller
//! Adapters. Auth exposes typed outcomes and live events so CLI and embedded
//! callers share one operation Interface without sharing process state.

mod agent_node_identity;
pub mod auth;
mod auth_pairing;
mod device_rpc;
mod auth_login;
mod auth_login_agent;
#[cfg(test)]
mod auth_login_tests;
pub mod auth_requests;
mod claim_authorization;
#[cfg(test)]
mod claim_authorization_tests;
pub(crate) mod claim_bridge;
mod claim_native;
pub mod claim_offer;
pub(crate) mod credential_file;
pub(crate) mod device_flow;
pub mod hosted;
mod identity_state;
pub(crate) mod net_endpoint;
mod owner_root;
#[cfg(test)]
mod owner_root_tests;
pub(crate) mod root_mint;
#[cfg(test)]
mod root_mint_tests;
pub mod websocket;
pub mod whoami;

pub use device_flow::AgentTemplate;
pub use hosted::{
    HostedAuthMode, HostedClient, HostedSession, ServerStream, resolve_active_bearer,
    resolve_hosted_credential,
};
pub use websocket::connect_websocket;
