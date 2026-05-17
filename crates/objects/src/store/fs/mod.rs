// SPDX-License-Identifier: Apache-2.0
//! Filesystem-based object store.

mod fs_impl;
mod fs_io;
mod fs_pack;
mod fs_paths;
mod fs_store;

#[cfg(test)]
mod fs_tests;

pub use fs_io::read_file_bytes_for_pack;
pub use fs_store::{FsStore, LooseObjectWriteMode};