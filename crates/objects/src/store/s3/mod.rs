// SPDX-License-Identifier: Apache-2.0
//! S3-compatible object storage backend for Heddle.
//!
//! `S3Store` implements [`ObjectStore`] and works with AWS S3, MinIO, Wasabi,
//! DigitalOcean Spaces, and any other S3-compatible service.
//!
//! It is the first built-in extension of the [`ObjectStore`] trait: it is built
//! separately from the repository and injected via [`Repository::open_with_store`],
//! the same mechanism used by any custom backend.
//!
//! # Example
//!
//! ```rust,ignore
//! use objects::store::s3::{S3Store, S3StoreBuilder};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let store = S3Store::builder()
//!         .bucket("my-heddle-repo")
//!         .region("us-east-1")
//!         .prefix("my-project/")
//!         .build()
//!         .await?;
//!     Ok(())
//! }
//! ```
//!
//! [`ObjectStore`]: crate::ObjectStore
//! [`Repository::open_with_store`]: crate::store::Repository::open_with_store

mod s3_impl;
mod s3_store;

#[cfg(test)]
mod s3_tests;

pub use s3_store::{S3Store, S3StoreBuilder};
