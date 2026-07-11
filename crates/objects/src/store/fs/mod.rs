// SPDX-License-Identifier: Apache-2.0
//! Filesystem-based object store.

mod fs_impl;
mod fs_io;
mod fs_pack;
mod fs_paths;
mod fs_store;
mod pack_install_journal;

#[cfg(test)]
mod fs_tests;

pub use fs_io::read_file_bytes_for_pack;
pub use fs_store::{FsStore, LooseObjectWriteMode};
pub use pack_install_journal::{
    DEFAULT_PACK_INSTALL_INTENT_TTL_SECS, PackInstallIntent, PackInstallPhase,
    PackInstallRecoverReport, install_pack_bytes_journaled, recover_pack_install_intents,
    recover_pack_install_intents_with_ttl,
};
