// SPDX-License-Identifier: Apache-2.0
//! Heddle's in-repo hosted client.
//!
//! Transport, credentials, identity, and the hosted sync glue that verbs use
//! against a weft server. `heddle-api` protos remain the only shared seam with
//! weft/tapestry; this crate owns the client side of that contract.
#![allow(
    clippy::await_holding_lock,
    clippy::clone_on_copy,
    clippy::collapsible_if,
    clippy::items_after_test_module,
    clippy::manual_clamp,
    clippy::needless_borrow,
    clippy::needless_borrows_for_generic_args,
    clippy::needless_late_init,
    clippy::needless_update,
    clippy::redundant_closure,
    clippy::too_many_arguments,
    clippy::type_complexity,
    clippy::while_let_loop
)]

pub mod attachments;
pub mod attribution;
pub mod client;
#[cfg(feature = "client")]
pub mod hosted_runtime;
pub mod network;

/// Register factories needed to reopen CLI-owned lazy hosted repositories.
#[cfg(feature = "client")]
pub fn register_hosted_factory() {
    hosted_runtime::hosted::register_hosted_factory();
}
