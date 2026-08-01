// SPDX-License-Identifier: Apache-2.0
//! Strict reader index for canonical timeline-operation packs.

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
};

use objects::{
    error::{HeddleError, Result},
    object::{ContentHash, TimelineOperationId},
    store::{
        CompressionConfig, PackBuilder, install_pack_bytes_journaled,
        pack::{ObjectType, PackObjectId, PackReader},
    },
};

pub(crate) struct TimelinePackSet {
    packs_dir: PathBuf,
    packs: Vec<TimelinePack>,
    operation_locations: HashMap<TimelineOperationId, usize>,
}

struct TimelinePack {
    pack_path: PathBuf,
    index_path: PathBuf,
    reader: PackReader<'static>,
}

impl TimelinePackSet {
    pub(crate) fn open(packs_dir: PathBuf) -> Result<Self> {
        let mut set = Self {
            packs_dir,
            packs: Vec::new(),
            operation_locations: HashMap::new(),
        };
        set.reload()?;
        Ok(set)
    }

    pub(crate) fn reload_if_disk_changed(&mut self) -> Result<()> {
        let disk_paths = paired_pack_paths(&self.packs_dir)?;
        let loaded_paths = self
            .packs
            .iter()
            .map(|pack| (pack.pack_path.clone(), pack.index_path.clone()))
            .collect::<Vec<_>>();
        if disk_paths != loaded_paths {
            self.reload()?;
        }
        Ok(())
    }

    pub(crate) fn reload(&mut self) -> Result<()> {
        let mut packs = Vec::new();
        let mut operation_locations = HashMap::new();
        for (pack_path, index_path) in paired_pack_paths(&self.packs_dir)? {
            let reader = PackReader::open(&pack_path, &index_path)?;
            let pack_index = packs.len();
            for id in reader.list_ids() {
                let operation_id = timeline_id_from_pack_id(id)?;
                operation_locations
                    .entry(operation_id)
                    .or_insert(pack_index);
            }
            packs.push(TimelinePack {
                pack_path,
                index_path,
                reader,
            });
        }
        self.packs = packs;
        self.operation_locations = operation_locations;
        Ok(())
    }

    pub(crate) fn operation_ids(&self) -> Vec<TimelineOperationId> {
        self.operation_locations.keys().copied().collect()
    }

    pub(crate) fn read_operation(&self, id: &TimelineOperationId) -> Result<Option<Vec<u8>>> {
        let Some(pack_index) = self.operation_locations.get(id).copied() else {
            return Ok(None);
        };
        let pack_id = timeline_pack_id(id);
        let Some((object_type, bytes)) = self.packs[pack_index].reader.get_object(&pack_id)? else {
            return Err(HeddleError::InvalidObject(format!(
                "timeline pack index names missing operation {}",
                id.short()
            )));
        };
        if object_type != ObjectType::TimelineOperation {
            return Err(HeddleError::InvalidObject(format!(
                "timeline pack entry {} has object type {object_type:?}",
                id.short()
            )));
        }
        let computed_id = TimelineOperationId::for_bytes(&bytes);
        if computed_id != *id {
            return Err(HeddleError::InvalidObject(format!(
                "timeline packed operation id mismatch: expected {}, decoded {}",
                id.short(),
                computed_id.short()
            )));
        }
        Ok(Some(bytes))
    }

    pub(crate) fn consolidate(
        &mut self,
        loose_operations: Vec<(TimelineOperationId, Vec<u8>)>,
        aggressive: bool,
    ) -> Result<(u64, u64)> {
        self.reload()?;
        let old_pack_files = self
            .file_paths()
            .into_iter()
            .map(|(pack, index)| (pack.to_path_buf(), index.to_path_buf()))
            .collect::<Vec<_>>();
        if loose_operations.is_empty() && old_pack_files.len() <= 1 {
            return Ok((0, 0));
        }

        let mut canonical_operations = BTreeMap::new();
        for id in self.operation_ids() {
            let bytes = self.read_operation(&id)?.ok_or_else(|| {
                HeddleError::InvalidObject(format!(
                    "timeline pack lost indexed operation {}",
                    id.short()
                ))
            })?;
            canonical_operations.insert(id, bytes);
        }
        merge_loose_operations(&mut canonical_operations, loose_operations)?;
        if canonical_operations.is_empty() {
            return Ok((0, 0));
        }

        let mut compression = CompressionConfig::default();
        if !aggressive {
            compression.max_delta_size = 0;
        }
        let mut builder = PackBuilder::new(compression);
        for (id, bytes) in &canonical_operations {
            builder.add_id(
                timeline_pack_id(id),
                ObjectType::TimelineOperation,
                bytes.clone(),
            );
        }
        let (pack_data, index_data, stats) = builder.build()?;
        verify_candidate_pack(&pack_data, &index_data, &canonical_operations)?;
        let new_pack_name = install_pack_bytes_journaled(&self.packs_dir, &pack_data, &index_data)?;
        self.reload()?;
        verify_canonical_operations(self, &canonical_operations)?;
        retire_old_packs(&old_pack_files, &new_pack_name)?;
        self.reload()?;
        verify_canonical_operations(self, &canonical_operations)?;

        Ok((
            stats.object_count,
            stats
                .total_uncompressed
                .saturating_sub(stats.total_compressed),
        ))
    }

    pub(crate) fn file_paths(&self) -> Vec<(&Path, &Path)> {
        self.packs
            .iter()
            .map(|pack| (pack.pack_path.as_path(), pack.index_path.as_path()))
            .collect()
    }
}

fn verify_candidate_pack(
    pack_data: &[u8],
    index_data: &[u8],
    expected: &BTreeMap<TimelineOperationId, Vec<u8>>,
) -> Result<()> {
    let reader = PackReader::from_slice(pack_data, index_data)?;
    if reader.list_ids().len() != expected.len() {
        return Err(HeddleError::InvalidObject(
            "new timeline pack entry count differs from canonical operations".to_string(),
        ));
    }
    for (id, expected_bytes) in expected {
        match reader.get_object(&timeline_pack_id(id))? {
            Some((ObjectType::TimelineOperation, bytes))
                if &bytes == expected_bytes && TimelineOperationId::for_bytes(&bytes) == *id => {}
            _ => {
                return Err(HeddleError::InvalidObject(format!(
                    "new timeline pack failed canonical verification for operation {}",
                    id.short()
                )));
            }
        }
    }
    Ok(())
}

fn merge_loose_operations(
    canonical_operations: &mut BTreeMap<TimelineOperationId, Vec<u8>>,
    loose_operations: Vec<(TimelineOperationId, Vec<u8>)>,
) -> Result<()> {
    for (id, bytes) in loose_operations {
        if let Some(packed_bytes) = canonical_operations.get(&id)
            && packed_bytes != &bytes
        {
            return Err(HeddleError::InvalidObject(format!(
                "loose and packed timeline operation {} differ",
                id.short()
            )));
        }
        canonical_operations.insert(id, bytes);
    }
    Ok(())
}

fn verify_canonical_operations(
    packs: &TimelinePackSet,
    expected: &BTreeMap<TimelineOperationId, Vec<u8>>,
) -> Result<()> {
    for (id, expected_bytes) in expected {
        let actual_bytes = packs.read_operation(id)?.ok_or_else(|| {
            HeddleError::InvalidObject(format!(
                "new timeline pack does not resolve operation {}",
                id.short()
            ))
        })?;
        if &actual_bytes != expected_bytes {
            return Err(HeddleError::InvalidObject(format!(
                "new timeline pack changed canonical bytes for operation {}",
                id.short()
            )));
        }
    }
    Ok(())
}

fn retire_old_packs(old_pack_files: &[(PathBuf, PathBuf)], new_pack_name: &str) -> Result<()> {
    for (pack_path, index_path) in old_pack_files {
        if pack_path.file_stem().and_then(|stem| stem.to_str()) == Some(new_pack_name) {
            continue;
        }
        remove_file_ignore_missing(pack_path)?;
        remove_file_ignore_missing(index_path)?;
    }
    Ok(())
}

fn remove_file_ignore_missing(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

pub(crate) fn timeline_pack_id(id: &TimelineOperationId) -> PackObjectId {
    PackObjectId::Hash(ContentHash::from_bytes(*id.as_bytes()))
}

fn timeline_id_from_pack_id(id: PackObjectId) -> Result<TimelineOperationId> {
    match id {
        PackObjectId::Hash(hash) => Ok(TimelineOperationId::from_bytes(*hash.as_bytes())),
        PackObjectId::StateId(_) => Err(HeddleError::InvalidObject(
            "timeline pack contains a state-id entry".to_string(),
        )),
    }
}

fn paired_pack_paths(packs_dir: &Path) -> Result<Vec<(PathBuf, PathBuf)>> {
    let entries = match fs::read_dir(packs_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let pack_path = entry?.path();
        if pack_path
            .extension()
            .is_some_and(|extension| extension == "pack")
        {
            let index_path = pack_path.with_extension("idx");
            if index_path.is_file() {
                paths.push((pack_path, index_path));
            }
        }
    }
    paths.sort();
    Ok(paths)
}
