// SPDX-License-Identifier: Apache-2.0
//! Isolated experiment carrying Heddle sync messages and native packs over Iroh.
//!
//! The experiment is a root-workspace member and consumes the public
//! transport-neutral `heddle-api` messages over Iroh operation streams.
//! This keeps the contract constant while testing Iroh as the transport.

mod client;
mod codec;
mod server;

pub use client::{IrohClient, NativePullOutcome, WireBenchmarkOutcome};
pub use codec::ALPN;
pub use server::{ExperimentRepository, IrohServer};

/// Result type for the experimental transport.
pub type Result<T> = std::result::Result<T, TransportError>;

/// Errors produced by the experimental transport.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("Iroh transport failed: {0}")]
    Iroh(String),
    #[error("transport I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("protobuf decode failed: {0}")]
    Decode(#[from] prost::DecodeError),
    #[error("protobuf encode failed: {0}")]
    Encode(#[from] prost::EncodeError),
    #[error("Heddle protocol failed: {0}")]
    Protocol(#[from] wire::ProtocolError),
    #[error("object store failed: {0}")]
    Store(#[from] objects::store::StoreError),
    #[error("invalid transport frame: {0}")]
    InvalidFrame(String),
    #[error("remote rejected request: {0}")]
    Remote(String),
}
