// SPDX-License-Identifier: Apache-2.0
//! Local filesystem-oriented object-store extension traits.

use std::{path::Path, path::PathBuf};

use crate::object::ContentHash;

use super::{LocalObjectStore, Result, pack};

/// Extension trait for hardlink/reflink-friendly local materialization.
pub trait LocalObjectStoreExt: Send + Sync {
    fn loose_blob_path(&self, _hash: &ContentHash) -> Option<PathBuf> {
        None
    }

    fn promote_to_loose_uncompressed(&self, _hash: &ContentHash) -> Result<bool> {
        Ok(false)
    }

    fn clear_recent_caches(&self) {}
}

/// Extension trait for local pack maintenance and filesystem-path installs.
pub trait PackMaintenanceStoreExt: LocalObjectStore {
    fn install_pack_streaming(
        &self,
        pack_path: &Path,
        index_path: &Path,
    ) -> Result<Vec<pack::PackObjectId>> {
        let pack_data = std::fs::read(pack_path)?;
        let index_data = std::fs::read(index_path)?;
        let ids = self.install_pack(&pack_data, &index_data)?;
        let _ = std::fs::remove_file(pack_path);
        let _ = std::fs::remove_file(index_path);
        Ok(ids)
    }

    fn pack_objects(&self, _aggressive: bool) -> Result<(u64, u64)> {
        Ok((0, 0))
    }

    fn prune_loose_objects(&self) -> Result<(u64, u64)> {
        Ok((0, 0))
    }

    fn begin_snapshot_write_batch(&self) -> Result<()> {
        Ok(())
    }

    fn flush_snapshot_write_batch(&self) -> Result<()> {
        Ok(())
    }

    fn abort_snapshot_write_batch(&self) {}
}
