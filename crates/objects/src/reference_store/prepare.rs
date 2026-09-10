//! Shared immutable reference preparation for local capture and hosted/device integration.
//! Inputs are exact authorized ancestry snapshots. Outputs must be persisted before
//! signing/admitting their descriptor; this module has no database or network policy.
use std::collections::BTreeMap;

use crate::{
    error::{HeddleError, Result},
    object::{
        Blob, CollaborationScope, ContentHash, ObjectSource, State, TreeEntryTarget,
        source_target::{
            SourceRangeProjection, SourceSelector,
            capture::{
                self, FileResolution, ReferenceClosure, ResolutionStatus, SourceTargetSnapshot,
                TargetResolution,
            },
        },
        source_target_map::{MapBudget, SourceTargetMap, SourceTargetMapStore},
        thread_replication::Capture,
    },
};
fn err(value: impl std::fmt::Display) -> HeddleError {
    HeddleError::InvalidObject(value.to_string())
}
fn budget() -> MapBudget {
    MapBudget::new(65_536, 32 * 1024 * 1024, 65_536, 32 * 1024 * 1024)
}
pub struct Seed {
    pub file: FileResolution,
    pub target: TargetResolution,
}
pub struct Prepared {
    pub capture: Capture,
    pub blobs: BTreeMap<ContentHash, Vec<u8>>,
}
struct Overlay {
    inherited: BTreeMap<ContentHash, Vec<u8>>,
    created: BTreeMap<ContentHash, Vec<u8>>,
    created_bytes: usize,
}
impl Overlay {
    fn put(&mut self, value: &impl serde::Serialize) -> Result<ContentHash> {
        let bytes = capture::encode(value)?;
        let hash = ContentHash::compute_typed("blob", &bytes);
        self.write(hash, bytes)?;
        Ok(hash)
    }
}
impl SourceTargetMapStore for Overlay {
    type Error = HeddleError;
    fn read(&mut self, hash: ContentHash, max: usize) -> Result<Option<Vec<u8>>> {
        let value = self
            .created
            .get(&hash)
            .or_else(|| self.inherited.get(&hash));
        if value.is_some_and(|bytes| bytes.len() > max) {
            return Err(err("reference map read budget"));
        }
        Ok(value.cloned())
    }
    fn write(&mut self, hash: ContentHash, bytes: Vec<u8>) -> Result<()> {
        if ContentHash::compute_typed("blob", &bytes) != hash {
            return Err(err("reference map hash mismatch"));
        }
        if !self.inherited.contains_key(&hash) && !self.created.contains_key(&hash) {
            if self.created.len() >= 65_536 || bytes.len() > 32 * 1024 * 1024 - self.created_bytes {
                return Err(err("reference prepared object budget exceeded"));
            }
            self.created_bytes += bytes.len();
            self.created.insert(hash, bytes);
        }
        Ok(())
    }
}
fn files(store: &impl ObjectSource, root: ContentHash) -> Result<BTreeMap<String, ContentHash>> {
    let mut pending = vec![(String::new(), root)];
    let mut result = BTreeMap::new();
    let mut work = 0;
    while let Some((prefix, hash)) = pending.pop() {
        let tree = store
            .get_tree(&hash)?
            .ok_or_else(|| err("capture tree missing"))?;
        for entry in tree.entries() {
            work += 1;
            if work > 65_536 {
                return Err(err("capture reference tree budget exceeded"));
            }
            let path = if prefix.is_empty() {
                entry.name().to_owned()
            } else {
                format!("{prefix}/{}", entry.name())
            };
            if path.len() > 4096 {
                return Err(err("capture reference path budget exceeded"));
            }
            match entry.target() {
                TreeEntryTarget::Tree { hash } => pending.push((path, *hash)),
                TreeEntryTarget::Blob { hash, .. } => {
                    result.insert(path, *hash);
                }
                _ => {}
            }
        }
    }
    Ok(result)
}

pub fn prepare(
    store: &impl ObjectSource,
    scope: CollaborationScope,
    state: &State,
    collaboration_frontier: ContentHash,
    roots: Vec<ReferenceClosure>,
    seeds: Vec<Seed>,
    mut symbol: impl FnMut(
        &FileResolution,
        &FileResolution,
        &Blob,
        &Blob,
        &str,
    ) -> Result<(String, ResolutionStatus)>,
) -> Result<Prepared> {
    if scope.spool.is_nil() || scope.thread.is_none() || roots.len() > 128 || seeds.len() > 65_536 {
        return Err(err("invalid reference preparation scope or budget"));
    }
    if roots.iter().all(|root| root.targets.is_empty()) && seeds.is_empty() {
        return Ok(Prepared {
            capture: state.encode_current_msgpack()?.into(),
            blobs: BTreeMap::new(),
        });
    }
    let mut overlay = Overlay {
        inherited: BTreeMap::new(),
        created: BTreeMap::new(),
        created_bytes: 0,
    };
    let mut inherited_bytes = 0usize;
    for root in &roots {
        if root.snapshot.scope.spool != scope.spool {
            return Err(err("reference ancestry crosses Spool"));
        }
        for (hash, bytes) in &root.blobs {
            if !overlay.inherited.contains_key(hash) {
                if overlay.inherited.len() >= 65_536
                    || bytes.len() > 32 * 1024 * 1024 - inherited_bytes
                {
                    return Err(err("reference ancestry budget exceeded"));
                }
                inherited_bytes += bytes.len();
                overlay.inherited.insert(*hash, bytes.clone());
            }
        }
    }
    let mut inherited_files = BTreeMap::new();
    let mut inherited_targets = BTreeMap::new();
    let (mut file_root, mut target_root) = (None, None);
    if let Some(first) = roots.first() {
        file_root = first.snapshot.files;
        target_root = first.snapshot.targets;
    }
    let current = files(store, state.tree)?;
    let mut file_candidates: BTreeMap<ContentHash, Vec<FileResolution>> = BTreeMap::new();
    let mut target_candidates: BTreeMap<ContentHash, Vec<(TargetResolution, FileResolution)>> =
        BTreeMap::new();
    for inherited in roots {
        for (id, target) in inherited.targets {
            let file = inherited
                .files
                .get(&target.core.file)
                .ok_or_else(|| err("inherited target file missing"))?;
            let candidates = target_candidates.entry(id).or_default();
            if !candidates
                .iter()
                .any(|entry| entry == &(target.clone(), file.clone()))
            {
                candidates.push((target, file.clone()));
            }
        }
        for (id, file) in inherited.files {
            let candidates = file_candidates.entry(id).or_default();
            if !candidates.contains(&file) {
                candidates.push(file);
            }
        }
    }
    for (id, candidates) in file_candidates {
        let matching: Vec<_> = candidates
            .iter()
            .filter(|file| {
                file.status == ResolutionStatus::Resolved
                    && current.get(&file.path).copied() == file.blob
            })
            .collect();
        let selected = if matching.len() == 1 {
            matching[0]
        } else {
            &candidates[0]
        };
        let mut file = selected.clone();
        if candidates.len() > 1 && matching.len() != 1 {
            file.status = ResolutionStatus::Ambiguous;
        }
        inherited_files.insert(id, file);
    }
    for (id, candidates) in target_candidates {
        let selected_file = inherited_files
            .get(&candidates[0].0.core.file)
            .ok_or_else(|| err("selected target file missing"))?;
        let matching: Vec<_> = candidates
            .iter()
            .filter(|(_, file)| file == selected_file)
            .collect();
        let selected = if matching.len() == 1 {
            &matching[0].0
        } else {
            &candidates[0].0
        };
        let mut target = selected.clone();
        if candidates.iter().any(|(other, _)| other != selected) && matching.len() != 1 {
            target.status = ResolutionStatus::Ambiguous;
        }
        if selected_file.status != ResolutionStatus::Resolved {
            target.status = selected_file.status;
        }
        inherited_targets.insert(id, target);
    }

    for seed in seeds {
        if seed.file.core.scope.spool != scope.spool
            || seed.target.core.file != seed.file.core.id().map_err(err)?
        {
            return Err(err("reference seed scope or core mismatch"));
        }
        let target = seed.target.core.id().map_err(err)?;
        inherited_files
            .entry(seed.target.core.file)
            .or_insert(seed.file);
        inherited_targets.entry(target).or_insert(seed.target);
    }
    if inherited_targets.is_empty() {
        return Ok(Prepared {
            capture: state.encode_current_msgpack()?.into(),
            blobs: BTreeMap::new(),
        });
    }
    let mut pairs = BTreeMap::new();
    let mut maps = BTreeMap::new();
    let mut changed_bytes = 0u64;
    let mut map_store = overlay;
    let mut work = budget();
    for (id, file) in &mut inherited_files {
        let previous = file.clone();
        let direct = current.get(&file.path).copied();
        let moved: Vec<_> = if direct.is_none() {
            current
                .iter()
                .filter(|(_, hash)| Some(**hash) == file.blob)
                .map(|(path, hash)| (path.clone(), *hash))
                .take(2)
                .collect()
        } else {
            Vec::new()
        };
        if let Some(blob) = direct {
            file.blob = Some(blob);
            if file.status != ResolutionStatus::Ambiguous {
                file.status = ResolutionStatus::Resolved;
            }
        } else if moved.len() == 1 {
            file.path = moved[0].0.clone();
            file.blob = Some(moved[0].1);
            if file.status != ResolutionStatus::Ambiguous {
                file.status = ResolutionStatus::Resolved;
            }
        } else {
            file.status = if moved.is_empty() {
                ResolutionStatus::Deleted
            } else {
                ResolutionStatus::Ambiguous
            };
        }
        if previous.blob != file.blob || previous.status != file.status {
            pairs.insert(*id, (previous.clone(), file.clone()));
            if let (Some(old), Some(new)) = (previous.blob, file.blob)
                && old != new
            {
                let length = store
                    .decoded_blob_len(&old)?
                    .unwrap_or(u64::MAX)
                    .saturating_add(store.decoded_blob_len(&new)?.unwrap_or(u64::MAX));
                changed_bytes = changed_bytes.saturating_add(length);
                if length > 8 * 1024 * 1024 || changed_bytes > 32 * 1024 * 1024 {
                    return Err(err("reference changed-file byte budget exceeded"));
                }
                let old_blob = store
                    .get_blob(&old)?
                    .ok_or_else(|| err("old reference file missing"))?;
                let new_blob = store
                    .get_blob(&new)?
                    .ok_or_else(|| err("new reference file missing"))?;
                maps.insert(*id, (old_blob, new_blob));
            }
        }
        let value = map_store.put(file)?;
        file_root = SourceTargetMap::update(&mut map_store, file_root, *id, Some(value), &mut work)
            .map_err(err)?;
    }
    let mut line_maps = BTreeMap::new();
    for (file, (old, new)) in &maps {
        line_maps.insert(
            *file,
            crate::worktree::source_line_edit_map(old, new, 8 * 1024 * 1024, 65_536)
                .map_err(err)?,
        );
    }
    for (id, target) in &mut inherited_targets {
        if let Some((previous, file)) = pairs.get(&target.core.file) {
            if file.status != ResolutionStatus::Resolved {
                target.status = file.status;
            } else if target.status == ResolutionStatus::Resolved {
                match &target.selector {
                    SourceSelector::Lines { range } => {
                        use crate::worktree::SourceLineMapBuild;
                        match line_maps.get(&target.core.file) {
                            Some(SourceLineMapBuild::Ready(map)) => {
                                match map.project(*range).map_err(err)? {
                                    SourceRangeProjection::Resolved { range, .. } => {
                                        target.selector = SourceSelector::Lines { range }
                                    }
                                    SourceRangeProjection::Deleted => {
                                        target.status = ResolutionStatus::Deleted
                                    }
                                    SourceRangeProjection::Ambiguous => {
                                        target.status = ResolutionStatus::Ambiguous
                                    }
                                }
                            }
                            Some(_) => target.status = ResolutionStatus::Ambiguous,
                            None => {}
                        }
                    }
                    SourceSelector::Symbol { address } => {
                        if let Some((old, new)) = maps.get(&target.core.file) {
                            let (address, status) = symbol(previous, file, old, new, address)?;
                            target.selector = SourceSelector::Symbol { address };
                            target.status = status;
                        }
                    }
                    SourceSelector::File => {}
                }
            }
        }
        let value = map_store.put(target)?;
        target_root =
            SourceTargetMap::update(&mut map_store, target_root, *id, Some(value), &mut work)
                .map_err(err)?;
    }

    let descriptor = SourceTargetSnapshot {
        version: 1,
        scope,
        state: state.id(),
        collaboration_frontier,
        files: file_root,
        targets: target_root,
    };
    let descriptor = map_store.put(&descriptor)?;
    Ok(Prepared {
        capture: Capture {
            state: state.encode_current_msgpack()?,
            source_targets: Some(descriptor),
        },
        blobs: map_store.created,
    })
}
