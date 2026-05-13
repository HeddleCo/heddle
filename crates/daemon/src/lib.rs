// SPDX-License-Identifier: Apache-2.0
//! Heddle local-mode gRPC daemon.
//!
//! Hosts the W2 gRPC services on a Unix-domain socket inside a single repo,
//! reachable by the same-user CLI for the latency-sensitive agent loop. No
//! Biscuit, no TLS, no multi-tenant — local-only, single-user, same-process
//! auth via SO_PEERCRED on Linux and `getpeereid` on macOS.
//!
//! The hosted variant (with Postgres, Biscuit, push/pull) lives in the
//! `server` crate. The two sides share the gRPC service contract from the
//! `grpc` crate but implement it against different state stores.

pub mod grpc_local_impl;

#[cfg(unix)]
pub mod local_daemon;