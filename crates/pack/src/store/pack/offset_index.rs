// SPDX-License-Identifier: Apache-2.0
use std::{
    io::{BufWriter, Write},
    path::Path,
};

use bytes::Bytes;
use tempfile::NamedTempFile;

use super::{disk_sort::sort_records, pack_index::PackIndex};
use crate::store::{Result, StoreError};

pub(super) struct OffsetIndex {
    data: Bytes,
    _file: Option<NamedTempFile>,
    _scratch: Option<super::ScratchDir>,
}
impl OffsetIndex {
    pub fn new(index: &PackIndex, root: Option<&Path>) -> Result<Self> {
        // At most 1 MiB of physical-order records for ordinary small packs.
        // Larger whole-pack transfers still use bounded external sorting.
        if root.is_none() || index.len() <= 1024 * 1024 / 16 {
            let mut records = Vec::with_capacity(index.len());
            for (ordinal, entry) in index.iter().enumerate() {
                records.push((entry?.offset, ordinal as u64));
            }
            records.sort_unstable();
            let mut data = Vec::with_capacity(records.len() * 16);
            for (offset, ordinal) in records {
                data.extend_from_slice(&offset.to_be_bytes());
                data.extend_from_slice(&ordinal.to_be_bytes());
            }
            return Ok(Self {
                data: Bytes::from(data),
                _file: None,
                _scratch: None,
            });
        }
        let scratch = super::ScratchDir::new(
            root.ok_or_else(|| {
                StoreError::InvalidObject("disk offset index requires scratch".into())
            })?,
            "pack-offsets-",
        )?;
        let root = scratch.path();
        let mut input = NamedTempFile::new_in(root)?;
        {
            let mut writer = BufWriter::new(input.as_file_mut());
            for (ordinal, entry) in index.iter().enumerate() {
                writer.write_all(&entry?.offset.to_be_bytes())?;
                writer.write_all(&(ordinal as u64).to_be_bytes())?;
            }
            writer.flush()?;
        }
        let file = sort_records::<16>(input.reopen()?, root)?;
        let data = if index.is_empty() {
            Bytes::new()
        } else {
            Bytes::from_owner(unsafe { memmap2::MmapOptions::new().map(file.as_file())? })
        };
        Ok(Self {
            data,
            _file: Some(file),
            _scratch: Some(scratch),
        })
    }
    fn record(&self, index: usize) -> Result<(u64, usize)> {
        let bytes = self
            .data
            .get(index * 16..index * 16 + 16)
            .ok_or_else(|| StoreError::InvalidObject("offset index truncated".into()))?;
        let offset = u64::from_be_bytes(
            bytes[..8]
                .try_into()
                .map_err(|_| StoreError::InvalidObject("offset index field".into()))?,
        );
        let ordinal = u64::from_be_bytes(
            bytes[8..]
                .try_into()
                .map_err(|_| StoreError::InvalidObject("offset index ordinal".into()))?,
        );
        Ok((
            offset,
            usize::try_from(ordinal)
                .map_err(|_| StoreError::InvalidObject("offset ordinal exceeds platform".into()))?,
        ))
    }
    pub fn aliases(&self, offset: u64) -> Result<bool> {
        let mut low = 0;
        let mut high = self.data.len() / 16;
        while low < high {
            let mid = low + (high - low) / 2;
            if self.record(mid)?.0 < offset {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        Ok(low + 1 < self.data.len() / 16
            && self.record(low)?.0 == offset
            && self.record(low + 1)?.0 == offset)
    }
    pub fn visit(&self, mut visitor: impl FnMut(u64, usize, usize) -> Result<()>) -> Result<()> {
        let mut next = 0;
        while next < self.data.len() / 16 {
            let (offset, ordinal) = self.record(next)?;
            let first = next;
            next += 1;
            while next < self.data.len() / 16 && self.record(next)?.0 == offset {
                next += 1;
            }
            visitor(offset, ordinal, next - first)?;
        }
        Ok(())
    }
}
