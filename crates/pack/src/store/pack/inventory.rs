// SPDX-License-Identifier: Apache-2.0
use std::{fs::File, path::Path};

use bytes::Bytes;
use tempfile::NamedTempFile;

use super::{PackIndex, PackObjectId};
use crate::store::Result;

/// Owned disk-backed installed identities. The retained index survives removal
/// of the transfer's staging directory and is deleted when this value drops.
#[derive(Debug)]
pub struct PackInventory {
    index: PackIndex,
    _file: NamedTempFile,
}
impl PackInventory {
    pub fn copy_from_index(index_path: &Path, scratch_root: &Path) -> Result<Self> {
        let mut file = NamedTempFile::new_in(scratch_root)?;
        std::io::copy(&mut File::open(index_path)?, file.as_file_mut())?;
        let bytes = Bytes::from_owner(unsafe { memmap2::MmapOptions::new().map(file.as_file())? });
        Ok(Self {
            index: PackIndex::from_owned_bytes(bytes)?,
            _file: file,
        })
    }
    pub fn len(&self) -> usize {
        self.index.len()
    }
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }
    pub fn contains(&self, id: &PackObjectId) -> Result<bool> {
        Ok(self.index.find(id)?.is_some())
    }
    pub fn ids(&self) -> impl Iterator<Item = Result<PackObjectId>> + '_ {
        self.index.iter().map(|entry| entry.map(|entry| entry.id))
    }
}
