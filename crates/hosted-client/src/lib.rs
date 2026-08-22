// SPDX-License-Identifier: Apache-2.0
//! Heddle's in-repo hosted client.
//!
//! Transport, credentials, identity, and the hosted sync glue that verbs use
//! against a weft server. `heddle-api` protos remain the only shared seam with
//! weft/tapestry; this crate owns the client side of that contract.

pub mod attribution;
pub mod attachments;
pub mod client;
#[cfg(feature = "client")]
pub mod extensions;
#[cfg(feature = "client")]
pub mod hosted_runtime;

/// Register factories needed to reopen CLI-owned lazy hosted repositories.
#[cfg(feature = "client")]
pub fn register_hosted_factory() {
    hosted_runtime::hosted::register_hosted_factory();
}
