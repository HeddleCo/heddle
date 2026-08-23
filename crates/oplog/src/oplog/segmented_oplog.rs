// SPDX-License-Identifier: Apache-2.0
//! V5 oplog layout: an atomic manifest over immutable V4 container segments.

use std::{
    fs,
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use objects::{
    error::{HeddleError, Result},
    fs_atomic::{
        create_dir_all_durable, sync_directory, write_file_atomic,
        write_file_atomic_reconstructible,
    },
};

use super::{
    oplog_types::{OpBatch, OpEntry, OpRecord},
    packed_oplog::{
        OplogRecoveryReport, PackedOpLog, PackedOpLogIndex, recover_oplog_at,
        write_recovery_report_sidecar,
    },
};

const MANIFEST_MAGIC: &[u8; 8] = b"HDOPV5\0\0";
const MANIFEST_VERSION: u32 = 2;
const ATOMIC_TEMP_SWEEP_GRACE: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq, Eq)]
struct SegmentDescriptor {
    level: u8,
    path: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Manifest {
    generation: u64,
    base: String,
    segments: Vec<SegmentDescriptor>,
    entry_count: u64,
    head_id: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct SegmentedOpLogIndex {
    canonical_path: PathBuf,
    manifest: Manifest,
    manifest_bytes: Option<Vec<u8>>,
    base: PackedOpLogIndex,
    segments: Vec<PackedOpLogIndex>,
}

impl SegmentedOpLogIndex {
    pub(crate) fn exists(path: &Path) -> bool {
        path.exists() || manifest_path(path).exists()
    }

    pub(crate) fn open(path: &Path) -> Result<Self> {
        let manifest_path = manifest_path(path);
        let (manifest, manifest_bytes) = if manifest_path.exists() {
            let bytes = fs::read(&manifest_path)?;
            (parse_manifest(&bytes)?, Some(bytes))
        } else {
            let base = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| HeddleError::InvalidObject("invalid oplog filename".to_string()))?
                .to_string();
            let base_index = PackedOpLogIndex::open(path)?;
            validate_first_entry_id(&base_index)?;
            let manifest = Manifest {
                generation: 0,
                base,
                segments: Vec::new(),
                entry_count: base_index.entry_count(),
                head_id: base_index.head_id(),
            };
            return Ok(Self {
                canonical_path: path.to_path_buf(),
                manifest,
                manifest_bytes: None,
                base: base_index,
                segments: Vec::new(),
            });
        };

        let base = PackedOpLogIndex::open(&resolve_relative(path, &manifest.base)?)?;
        let mut segments = Vec::with_capacity(manifest.segments.len());
        for descriptor in &manifest.segments {
            segments.push(PackedOpLogIndex::open(&resolve_relative(
                path,
                &descriptor.path,
            )?)?);
        }
        validate_container_sequence(&manifest, &base, &segments)?;
        let actual_count = base.entry_count()
            + segments
                .iter()
                .map(PackedOpLogIndex::entry_count)
                .sum::<u64>();
        let actual_head = segments
            .last()
            .map_or_else(|| base.head_id(), PackedOpLogIndex::head_id);
        if actual_count != manifest.entry_count || actual_head != manifest.head_id {
            return Err(HeddleError::InvalidObject(
                "oplog manifest disagrees with immutable segments".to_string(),
            ));
        }
        Ok(Self {
            canonical_path: path.to_path_buf(),
            manifest,
            manifest_bytes,
            base,
            segments,
        })
    }

    pub(crate) fn empty(path: PathBuf) -> Self {
        let base_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("oplog.bin")
            .to_string();
        Self {
            canonical_path: path.clone(),
            manifest: Manifest {
                generation: 0,
                base: base_name,
                segments: Vec::new(),
                entry_count: 0,
                head_id: 0,
            },
            manifest_bytes: None,
            base: PackedOpLogIndex::empty(path),
            segments: Vec::new(),
        }
    }

    pub(crate) fn read_head_id(path: &Path) -> Result<u64> {
        let manifest = manifest_path(path);
        if manifest.exists() {
            return Ok(parse_manifest(&fs::read(manifest)?)?.head_id);
        }
        PackedOpLog::read_head_id(path)
    }

    /// Strictly validate one manifest generation without mutating it. A writer
    /// may replace the manifest and unlink the prior generation between our
    /// pointer read and container opens; retry only when the pointer changed so
    /// that transient publication races are not reported as corruption.
    pub(crate) fn validate_strict(path: &Path) -> Result<()> {
        for _ in 0..3 {
            let before = fs::read(manifest_path(path)).ok();
            match Self::open(path) {
                Ok(_) => return Ok(()),
                Err(error) => {
                    let after = fs::read(manifest_path(path)).ok();
                    if before == after {
                        return Err(error);
                    }
                }
            }
        }
        let _ = Self::open(path)?;
        Ok(())
    }

    pub(crate) fn validate_current(path: &Path) -> Result<()> {
        let manifest_path = manifest_path(path);
        if manifest_path.exists() {
            let mut manifest = parse_manifest(&fs::read(&manifest_path)?)?;
            let mut salvaged = false;
            let mut recovery_report = None;
            for relative in std::iter::once(manifest.base.as_str()).chain(
                manifest
                    .segments
                    .iter()
                    .map(|segment| segment.path.as_str()),
            ) {
                let segment = resolve_relative(path, relative)?;
                if PackedOpLogIndex::open(&segment).is_err() {
                    PackedOpLog::ensure_current(&segment)?;
                    if recovery_report.is_none() {
                        recovery_report = OplogRecoveryReport::from_prior_sidecar(&segment);
                    }
                    salvaged = true;
                }
            }
            if salvaged {
                let discarded = invalid_suffix(&manifest, path)?;
                if discarded.segments > 0 {
                    return Err(HeddleError::InvalidObject(format!(
                        "oplog recovery would discard {} later segment(s) containing {} complete entries; run `heddle maintenance oplog recover` to authorize and report that data loss",
                        discarded.segments, discarded.entries
                    )));
                }
                manifest.generation = manifest.generation.checked_add(1).ok_or_else(|| {
                    HeddleError::InvalidObject("oplog manifest generation overflow".to_string())
                })?;
                reconcile_manifest_metadata(path, &mut manifest)?;
                let manifest_bytes = encode_manifest(&manifest)?;
                if let Some(mut report) = recovery_report {
                    report.suffix_segments_discarded = 0;
                    report.suffix_entries_discarded = 0;
                    write_recovery_report_sidecar(&manifest_path, &report)?;
                }
                write_manifest(path, &manifest_bytes)?;
                sweep_unlisted_containers(path, &manifest);
            }
            return Ok(());
        }
        PackedOpLog::ensure_current(path)
    }

    pub(crate) fn is_healthy(path: &Path) -> Result<bool> {
        let manifest_path = manifest_path(path);
        if manifest_path.exists() {
            let manifest = parse_manifest(&fs::read(manifest_path)?)?;
            for relative in std::iter::once(manifest.base.as_str()).chain(
                manifest
                    .segments
                    .iter()
                    .map(|segment| segment.path.as_str()),
            ) {
                let segment = resolve_relative(path, relative)?;
                PackedOpLog::validate_header(&segment)?;
                if !PackedOpLog::trailer_ok(&segment)? {
                    return Ok(false);
                }
            }
            return Ok(true);
        }
        PackedOpLog::validate_header(path)?;
        PackedOpLog::trailer_ok(path)
    }

    pub(crate) fn head_id(&self) -> u64 {
        self.manifest.head_id
    }

    pub(crate) fn matches_file_on_disk(&self) -> Result<bool> {
        match &self.manifest_bytes {
            Some(expected) => {
                if fs::read(manifest_path(&self.canonical_path))? != *expected
                    || !self.base.matches_file_on_disk()?
                {
                    return Ok(false);
                }
                for segment in &self.segments {
                    if !segment.matches_file_on_disk()? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            None => {
                if manifest_path(&self.canonical_path).exists() {
                    return Ok(false);
                }
                self.base.matches_file_on_disk()
            }
        }
    }

    pub(crate) fn last_entry(&self) -> Result<Option<OpEntry>> {
        let mut entries = self.recent_entries(1)?;
        Ok(entries.pop())
    }

    pub(crate) fn recent_entries(&self, count: usize) -> Result<Vec<OpEntry>> {
        let mut remaining = count;
        let mut out = Vec::with_capacity(count.min(self.manifest.entry_count as usize));
        for segment in self
            .segments
            .iter()
            .rev()
            .chain(std::iter::once(&self.base))
        {
            if remaining == 0 {
                break;
            }
            let mut entries = segment.recent_entries(remaining)?;
            remaining -= entries.len();
            out.append(&mut entries);
        }
        Ok(out)
    }

    pub(crate) fn entries_after(&self, since_head_id: u64) -> Result<Vec<OpEntry>> {
        let mut out = Vec::new();
        for segment in std::iter::once(&self.base).chain(self.segments.iter()) {
            if segment.head_id() > since_head_id {
                out.extend(segment.entries_after(since_head_id)?);
            }
        }
        Ok(out)
    }

    pub(crate) fn collect_batches_scoped(
        &self,
        count: usize,
        predicate: impl Fn(&OpBatch) -> bool,
        scope: Option<&str>,
    ) -> Result<Vec<OpBatch>> {
        self.collect_batches_after_scoped(0, count, predicate, scope)
    }

    pub(crate) fn collect_batches_after_scoped(
        &self,
        since_head_id: u64,
        count: usize,
        predicate: impl Fn(&OpBatch) -> bool,
        scope: Option<&str>,
    ) -> Result<Vec<OpBatch>> {
        let mut out = Vec::new();
        for segment in self
            .segments
            .iter()
            .rev()
            .chain(std::iter::once(&self.base))
        {
            if out.len() == count || segment.head_id() <= since_head_id {
                continue;
            }
            let batches = segment.collect_batches_after_scoped(
                since_head_id,
                count - out.len(),
                |batch| predicate(batch),
                scope,
            )?;
            out.extend(batches);
        }
        Ok(out)
    }

    pub(crate) fn transaction_commit(&self, transaction_id: &str) -> Result<Option<(u64, u64)>> {
        for segment in std::iter::once(&self.base).chain(self.segments.iter()) {
            if let Some(commit) = segment.transaction_commit(transaction_id)? {
                return Ok(Some(commit));
            }
        }
        Ok(None)
    }

    pub(crate) fn committed_batch_records(&self, transaction_id: &str) -> Result<Vec<OpRecord>> {
        let Some((_entry_id, batch_id)) = self.transaction_commit(transaction_id)? else {
            return Ok(Vec::new());
        };
        let Some(batch) = self
            .collect_batches_scoped(1, |batch| batch.id == batch_id, None)?
            .pop()
        else {
            return Ok(Vec::new());
        };
        Ok(batch
            .entries
            .into_iter()
            .filter(|entry| !super::oplog_types::is_transaction_commit(&entry.operation))
            .map(|entry| entry.operation)
            .collect())
    }

    pub(crate) fn committed_batch(&self, transaction_id: &str) -> Result<Option<OpBatch>> {
        let Some((_entry_id, batch_id)) = self.transaction_commit(transaction_id)? else {
            return Ok(None);
        };
        Ok(self
            .collect_batches_scoped(1, |batch| batch.id == batch_id, None)?
            .pop())
    }

    pub(crate) fn committed_transaction_ids<'a>(
        &self,
        candidates: impl IntoIterator<Item = &'a str>,
    ) -> Result<std::collections::HashSet<String>> {
        candidates
            .into_iter()
            .filter_map(|candidate| match self.transaction_commit(candidate) {
                Ok(Some(_)) => Some(Ok(candidate.to_string())),
                Ok(None) => None,
                Err(error) => Some(Err(error)),
            })
            .collect()
    }

    pub(crate) fn append_entries(&self, entries: &[OpEntry]) -> Result<Self> {
        self.append_entries_inner(entries, false)
    }

    pub(crate) fn append_entries_reconstructible(&self, entries: &[OpEntry]) -> Result<Self> {
        self.append_entries_inner(entries, true)
    }

    fn append_entries_inner(&self, entries: &[OpEntry], reconstructible: bool) -> Result<Self> {
        if entries.is_empty() {
            return Ok(self.clone());
        }
        let generation = self.manifest.generation.checked_add(1).ok_or_else(|| {
            HeddleError::InvalidObject("oplog manifest generation overflow".to_string())
        })?;
        let mut descriptors = self.manifest.segments.clone();
        let mut indexes = self.segments.clone();
        let mut carry_entries = entries.to_vec();
        let mut level = 0u8;
        while descriptors.last().is_some_and(|tail| tail.level == level) {
            let prior = indexes.pop().ok_or_else(|| {
                HeddleError::InvalidObject("oplog segment index count mismatch".to_string())
            })?;
            descriptors.pop();
            let mut merged = prior.entries_after(0)?;
            merged.extend(carry_entries);
            carry_entries = merged;
            level = level.checked_add(1).ok_or_else(|| {
                HeddleError::InvalidObject("oplog segment level overflow".to_string())
            })?;
        }

        let start = carry_entries
            .first()
            .map_or(self.head_id(), |entry| entry.id);
        let head = entries.last().map_or(self.head_id(), |entry| entry.id);
        let relative = format!(
            "oplog.segments/segment-l{level:02}-{start:020}-{head:020}-g{generation:020}.bin"
        );
        let path = resolve_relative(&self.canonical_path, &relative)?;
        let segment_parent = path.parent().unwrap_or_else(|| Path::new("."));
        if reconstructible {
            fs::create_dir_all(segment_parent)?;
        } else {
            create_dir_all_durable(segment_parent)?;
        }
        let mut packed = PackedOpLog::new(path.clone());
        packed.entries = carry_entries;
        packed.head_id = head;
        if reconstructible {
            packed.save_reconstructible()?;
        } else {
            packed.save()?;
        }
        let index = PackedOpLogIndex::open(&path)?;

        let mut manifest = self.manifest.clone();
        manifest.generation = generation;
        descriptors.push(SegmentDescriptor {
            level,
            path: relative,
        });
        manifest.segments = descriptors;
        manifest.entry_count = manifest
            .entry_count
            .checked_add(entries.len() as u64)
            .ok_or_else(|| HeddleError::InvalidObject("oplog entry count overflow".to_string()))?;
        manifest.head_id = head;
        let bytes = encode_manifest(&manifest)?;
        if reconstructible {
            write_manifest_reconstructible(&self.canonical_path, &bytes)?;
        } else {
            write_manifest(&self.canonical_path, &bytes)?;
        }
        sweep_unlisted_containers(&self.canonical_path, &manifest);

        indexes.push(index);
        Ok(Self {
            canonical_path: self.canonical_path.clone(),
            manifest,
            manifest_bytes: Some(bytes),
            base: self.base.clone(),
            segments: indexes,
        })
    }

    pub(crate) fn materialize_entries(&self) -> Result<Vec<OpEntry>> {
        self.entries_after(0)
    }

    pub(crate) fn replace_entries(&self, entries: Vec<OpEntry>) -> Result<Self> {
        self.replace_entries_inner(entries)
    }

    pub(crate) fn recover(path: &Path) -> Result<OplogRecoveryReport> {
        if !manifest_path(path).exists() {
            return recover_oplog_at(path);
        }
        let prior_generation_recovery =
            OplogRecoveryReport::from_prior_sidecar(&manifest_path(path));
        let mut manifest = parse_manifest(&fs::read(manifest_path(path))?)?;
        if let Some(report) = prior_generation_recovery.as_ref()
            && Self::open(path).is_ok()
        {
            return Ok(report.clone());
        }
        let mut recovered = None;
        let mut repaired_this_run = false;
        for relative in std::iter::once(manifest.base.as_str()).chain(
            manifest
                .segments
                .iter()
                .map(|segment| segment.path.as_str()),
        ) {
            let report = recover_oplog_at(&resolve_relative(path, relative)?)?;
            let repaired = !report.already_healthy;
            if repaired {
                if !repaired_this_run {
                    recovered = Some(report);
                }
                repaired_this_run = true;
            } else if report.prior_recovery && recovered.is_none() {
                recovered = Some(report);
            }
        }
        if recovered.is_none() {
            let _ = Self::open(path)?;
            return Ok(prior_generation_recovery.unwrap_or_else(OplogRecoveryReport::healthy));
        }
        let discarded = invalid_suffix(&manifest, path)?;
        if !repaired_this_run && discarded.segments == 0 && Self::open(path).is_ok() {
            return Ok(recovered.unwrap());
        }
        let discarded_segments = usize::try_from(discarded.segments).map_err(|_| {
            HeddleError::InvalidObject("oplog suffix length exceeds platform limits".to_string())
        })?;
        quarantine_pruned_suffix(path, &manifest, discarded_segments)?;
        let pruned_paths = manifest.segments[manifest.segments.len() - discarded_segments..]
            .iter()
            .map(|segment| segment.path.clone())
            .collect::<Vec<_>>();
        manifest.generation = manifest.generation.checked_add(1).ok_or_else(|| {
            HeddleError::InvalidObject("oplog manifest generation overflow".to_string())
        })?;
        manifest
            .segments
            .truncate(manifest.segments.len() - discarded.segments as usize);
        reconcile_manifest_metadata(path, &mut manifest)?;
        let manifest_bytes = encode_manifest(&manifest)?;
        let mut report = recovered.unwrap();
        if discarded.segments > 0 {
            report.already_healthy = false;
        }
        report.suffix_segments_discarded = discarded.segments;
        report.suffix_entries_discarded = discarded.entries;
        report.sidecar_path = Some(write_recovery_report_sidecar(
            &manifest_path(path),
            &report,
        )?);
        // Publish the durable loss report first. If the manifest swap fails,
        // the still-invalid old manifest forces explicit recovery to recompute
        // and overwrite these counts; the inverse order could commit suffix
        // loss and crash before preserving an honest operator report.
        write_manifest(path, &manifest_bytes)?;
        for relative in pruned_paths {
            if let Ok(pruned) = resolve_relative(path, &relative) {
                let _ = fs::remove_file(pruned);
            }
        }
        sweep_unlisted_containers(path, &manifest);
        Ok(report)
    }

    fn replace_entries_inner(&self, entries: Vec<OpEntry>) -> Result<Self> {
        let obsolete = self
            .manifest
            .segments
            .iter()
            .map(|segment| &segment.path)
            .chain(std::iter::once(&self.manifest.base))
            .filter(|relative| relative.starts_with("oplog.segments/"))
            .cloned()
            .collect::<Vec<_>>();
        let head = entries.last().map_or(0, |entry| entry.id);
        let generation = self.manifest.generation.checked_add(1).ok_or_else(|| {
            HeddleError::InvalidObject("oplog manifest generation overflow".to_string())
        })?;
        let relative = format!("oplog.segments/base-{head:020}-g{generation:020}.bin");
        let path = resolve_relative(&self.canonical_path, &relative)?;
        create_dir_all_durable(path.parent().unwrap_or_else(|| Path::new(".")))?;
        let mut packed = PackedOpLog::new(path.clone());
        packed.head_id = head;
        packed.entries = entries;
        packed.save()?;
        let base = PackedOpLogIndex::open(&path)?;
        let manifest = Manifest {
            generation,
            base: relative,
            segments: Vec::new(),
            entry_count: base.entry_count(),
            head_id: head,
        };
        let bytes = encode_manifest(&manifest)?;
        write_manifest(&self.canonical_path, &bytes)?;
        for relative in obsolete {
            if let Ok(old) = resolve_relative(&self.canonical_path, &relative)
                && old != path
            {
                // The manifest swap above is the linearization point. Orphan
                // cleanup is deliberately best-effort so a failed unlink can
                // never make the caller retry an already-committed append.
                let _ = fs::remove_file(old);
            }
        }
        sweep_unlisted_containers(&self.canonical_path, &manifest);
        Ok(Self {
            canonical_path: self.canonical_path.clone(),
            manifest,
            manifest_bytes: Some(bytes),
            base,
            segments: Vec::new(),
        })
    }

    #[cfg(test)]
    pub(crate) fn segment_count(&self) -> usize {
        self.segments.len()
    }
}

fn write_manifest(path: &Path, bytes: &[u8]) -> Result<()> {
    let manifest = manifest_path(path);
    write_file_atomic(&manifest, bytes)?;
    Ok(())
}

fn write_manifest_reconstructible(path: &Path, bytes: &[u8]) -> Result<()> {
    let manifest = manifest_path(path);
    write_file_atomic_reconstructible(&manifest, bytes)?;
    Ok(())
}

fn sweep_unlisted_containers(canonical_path: &Path, manifest: &Manifest) {
    let live = std::iter::once(manifest.base.as_str())
        .chain(
            manifest
                .segments
                .iter()
                .map(|segment| segment.path.as_str()),
        )
        .filter_map(|relative| resolve_relative(canonical_path, relative).ok())
        .collect::<std::collections::HashSet<_>>();
    let segment_dir = canonical_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("oplog.segments");
    if let Ok(entries) = fs::read_dir(segment_dir) {
        for entry in entries.filter_map(std::result::Result::ok) {
            let path = entry.path();
            let is_generation_container = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    generation_container_is_sweepable(&path, name, manifest.generation)
                });
            let is_generation_temp = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(is_stale_generation_atomic_temp);
            if (is_generation_container || is_generation_temp)
                && entry.file_type().is_ok_and(|kind| kind.is_file())
                && !live.contains(&path)
            {
                let _ = fs::remove_file(path);
            }
        }
    }
    if !live.contains(canonical_path) {
        let _ = fs::remove_file(canonical_path);
    }
    let parent = canonical_path.parent().unwrap_or_else(|| Path::new("."));
    if let Ok(entries) = fs::read_dir(parent) {
        for entry in entries.filter_map(std::result::Result::ok) {
            let is_manifest_temp = entry
                .file_name()
                .to_str()
                .is_some_and(|name| atomic_temp_is_stale(name, ".oplog.manifest"));
            if is_manifest_temp && entry.file_type().is_ok_and(|kind| kind.is_file()) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

fn generation_container_is_sweepable(path: &Path, name: &str, selected_generation: u64) -> bool {
    if !(name.starts_with("base-") || name.starts_with("segment-")) {
        return false;
    }
    let Some(generation) = name
        .strip_suffix(".bin")
        .and_then(|name| name.rsplit_once("-g"))
        .and_then(|(_, generation)| generation.parse::<u64>().ok())
    else {
        return false;
    };
    if generation > selected_generation {
        return false;
    }
    // A writer publishes the immutable container before atomically replacing
    // the manifest, and lock-free readers can still be opening the prior
    // generation while the new manifest becomes visible. Keep recent
    // containers through that publication window; a later sweep reclaims them.
    path.metadata()
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age >= ATOMIC_TEMP_SWEEP_GRACE)
}

/// Preserve every container that explicit recovery removes from the selected
/// generation. The quarantine copy is made durable before the manifest swap;
/// a crash can therefore leave an extra copy, but can never publish suffix loss
/// without a recoverable byte-for-byte container outside the sweep namespace.
fn quarantine_pruned_suffix(
    canonical_path: &Path,
    manifest: &Manifest,
    discarded_segments: usize,
) -> Result<()> {
    if discarded_segments == 0 {
        return Ok(());
    }
    let quarantine_dir = canonical_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("oplog.quarantine");
    create_dir_all_durable(&quarantine_dir)?;
    let first_discarded = manifest
        .segments
        .len()
        .checked_sub(discarded_segments)
        .ok_or_else(|| HeddleError::InvalidObject("invalid oplog suffix length".to_string()))?;
    for descriptor in &manifest.segments[first_discarded..] {
        let source = resolve_relative(canonical_path, &descriptor.path)?;
        let file_name = source.file_name().ok_or_else(|| {
            HeddleError::InvalidObject("invalid oplog segment filename".to_string())
        })?;
        let destination = next_pruned_quarantine_path(&quarantine_dir, file_name);
        match fs::hard_link(&source, &destination) {
            Ok(()) => sync_directory(&quarantine_dir)?,
            Err(_) => write_file_atomic(&destination, &fs::read(&source)?)?,
        }
    }
    Ok(())
}

fn next_pruned_quarantine_path(quarantine_dir: &Path, file_name: &std::ffi::OsStr) -> PathBuf {
    let base = quarantine_dir.join(file_name).with_extension("bin.pruned");
    if !base.exists() {
        return base;
    }
    for index in 1.. {
        let mut candidate = base.as_os_str().to_os_string();
        candidate.push(format!(".{index}"));
        let candidate = PathBuf::from(candidate);
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!("unbounded pruned-container suffix search should always return")
}

fn is_stale_generation_atomic_temp(name: &str) -> bool {
    let Some(target) = name.strip_prefix('.') else {
        return false;
    };
    ((target.starts_with("base-") || target.starts_with("segment-"))
        && target.contains(".bin.tmp-"))
        && atomic_temp_is_stale(
            name,
            &format!(".{}", target.split(".tmp-").next().unwrap_or("")),
        )
}

fn atomic_temp_is_stale(name: &str, target: &str) -> bool {
    let Some(suffix) = name
        .strip_prefix(target)
        .and_then(|rest| rest.strip_prefix(".tmp-"))
    else {
        return false;
    };
    let parts = suffix.split('-').collect::<Vec<_>>();
    let Ok([pid, timestamp, counter]) = <[&str; 3]>::try_from(parts) else {
        return false;
    };
    if pid.parse::<u32>().is_err() || counter.parse::<u64>().is_err() {
        return false;
    }
    let Ok(timestamp) = timestamp.parse::<u128>() else {
        return false;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    now.as_nanos().saturating_sub(timestamp) >= ATOMIC_TEMP_SWEEP_GRACE.as_nanos()
}

fn reconcile_manifest_metadata(path: &Path, manifest: &mut Manifest) -> Result<()> {
    let base = PackedOpLogIndex::open(&resolve_relative(path, &manifest.base)?)?;
    let mut entry_count = base.entry_count();
    let mut head_id = base.head_id();
    for descriptor in &manifest.segments {
        let segment = PackedOpLogIndex::open(&resolve_relative(path, &descriptor.path)?)?;
        entry_count = entry_count
            .checked_add(segment.entry_count())
            .ok_or_else(|| HeddleError::InvalidObject("oplog entry count overflow".to_string()))?;
        head_id = segment.head_id();
    }
    manifest.entry_count = entry_count;
    manifest.head_id = head_id;
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DiscardedSuffix {
    segments: u64,
    entries: u64,
}

fn invalid_suffix(manifest: &Manifest, path: &Path) -> Result<DiscardedSuffix> {
    let base = PackedOpLogIndex::open(&resolve_relative(path, &manifest.base)?)?;
    validate_first_entry_id(&base)?;
    let mut previous = base.entry_id_range().map(|(_, last)| last);
    let mut valid = 0usize;
    for descriptor in &manifest.segments {
        let segment = PackedOpLogIndex::open(&resolve_relative(path, &descriptor.path)?)?;
        let Some((first, last)) = segment.entry_id_range() else {
            break;
        };
        match previous {
            Some(previous) => {
                let expected = previous.checked_add(1).ok_or_else(|| {
                    HeddleError::InvalidObject("oplog entry id overflow".to_string())
                })?;
                if first != expected {
                    break;
                }
            }
            None if first != 1 => break,
            None => {}
        }
        valid += 1;
        previous = Some(last);
    }
    let discarded = &manifest.segments[valid..];
    let entries = discarded.iter().try_fold(0u64, |count, descriptor| {
        let segment = PackedOpLogIndex::open(&resolve_relative(path, &descriptor.path)?)?;
        count
            .checked_add(segment.entry_count())
            .ok_or_else(|| HeddleError::InvalidObject("oplog entry count overflow".to_string()))
    })?;
    Ok(DiscardedSuffix {
        segments: discarded.len() as u64,
        entries,
    })
}

fn validate_container_sequence(
    manifest: &Manifest,
    base: &PackedOpLogIndex,
    segments: &[PackedOpLogIndex],
) -> Result<()> {
    let mut unique = std::collections::HashSet::new();
    if !unique.insert(manifest.base.as_str())
        || manifest
            .segments
            .iter()
            .any(|segment| !unique.insert(segment.path.as_str()))
    {
        return Err(HeddleError::InvalidObject(
            "oplog manifest repeats a container path".to_string(),
        ));
    }
    if manifest
        .segments
        .windows(2)
        .any(|pair| pair[0].level <= pair[1].level)
    {
        return Err(HeddleError::InvalidObject(
            "oplog segment levels are not strictly descending".to_string(),
        ));
    }

    validate_first_entry_id(base)?;
    let mut previous = base.entry_id_range().map(|(_, last)| last);
    for segment in segments {
        let Some((first, last)) = segment.entry_id_range() else {
            return Err(HeddleError::InvalidObject(
                "oplog append segment is empty".to_string(),
            ));
        };
        if let Some(previous) = previous {
            let expected = previous
                .checked_add(1)
                .ok_or_else(|| HeddleError::InvalidObject("oplog entry id overflow".to_string()))?;
            if first != expected {
                return Err(HeddleError::InvalidObject(
                    "oplog manifest container ids are not contiguous".to_string(),
                ));
            }
        } else if first != 1 {
            return Err(HeddleError::InvalidObject(
                "oplog EntryId sequence must start at 1".to_string(),
            ));
        }
        if last < first {
            return Err(HeddleError::InvalidObject(
                "oplog manifest container id range is invalid".to_string(),
            ));
        }
        previous = Some(last);
    }
    Ok(())
}

fn validate_first_entry_id(base: &PackedOpLogIndex) -> Result<()> {
    if base
        .entry_id_range()
        .is_some_and(|(first, _last)| first != 1)
    {
        return Err(HeddleError::InvalidObject(
            "oplog EntryId sequence must start at 1".to_string(),
        ));
    }
    Ok(())
}

fn manifest_path(path: &Path) -> PathBuf {
    path.with_file_name("oplog.manifest")
}

fn resolve_relative(canonical_path: &Path, relative: &str) -> Result<PathBuf> {
    let relative_path = Path::new(relative);
    if relative_path.is_absolute()
        || relative_path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(HeddleError::InvalidObject(
            "oplog manifest contains an unsafe path".to_string(),
        ));
    }
    Ok(canonical_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(relative_path))
}

fn encode_manifest(manifest: &Manifest) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    out.extend_from_slice(MANIFEST_MAGIC);
    out.extend_from_slice(&MANIFEST_VERSION.to_le_bytes());
    out.extend_from_slice(&manifest.generation.to_le_bytes());
    out.extend_from_slice(&manifest.entry_count.to_le_bytes());
    out.extend_from_slice(&manifest.head_id.to_le_bytes());
    write_string(&mut out, &manifest.base)?;
    let segment_count = u32::try_from(manifest.segments.len())
        .map_err(|_| HeddleError::InvalidObject("too many oplog segments".to_string()))?;
    out.extend_from_slice(&segment_count.to_le_bytes());
    for segment in &manifest.segments {
        out.push(segment.level);
        write_string(&mut out, &segment.path)?;
    }
    Ok(out)
}

fn write_string(out: &mut Vec<u8>, value: &str) -> Result<()> {
    let len = u32::try_from(value.len())
        .map_err(|_| HeddleError::InvalidObject("oplog manifest path too long".to_string()))?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(value.as_bytes());
    Ok(())
}

fn parse_manifest(bytes: &[u8]) -> Result<Manifest> {
    let mut cursor = ManifestCursor { bytes, offset: 0 };
    if cursor.read(8)? != MANIFEST_MAGIC {
        return Err(HeddleError::InvalidObject(
            "invalid oplog manifest magic".to_string(),
        ));
    }
    let version = cursor.read_u32()?;
    if version < MANIFEST_VERSION {
        return Err(HeddleError::StorageFormatMigrationRequired {
            storage: "oplog manifest".to_string(),
            found: version,
            required: MANIFEST_VERSION,
        });
    }
    if version > MANIFEST_VERSION {
        return Err(HeddleError::StorageFormatTooNew {
            storage: "oplog manifest".to_string(),
            found: version,
            supported: MANIFEST_VERSION,
        });
    }
    let generation = cursor.read_u64()?;
    let entry_count = cursor.read_u64()?;
    let head_id = cursor.read_u64()?;
    let base = cursor.read_string()?;
    let segment_count = cursor.read_u32()? as usize;
    if segment_count > 64 {
        return Err(HeddleError::InvalidObject(
            "oplog manifest exceeds binary tier bound".to_string(),
        ));
    }
    let mut segments = Vec::with_capacity(segment_count);
    for _ in 0..segment_count {
        segments.push(SegmentDescriptor {
            level: cursor.read(1)?[0],
            path: cursor.read_string()?,
        });
    }
    if cursor.offset != bytes.len() {
        return Err(HeddleError::InvalidObject(
            "oplog manifest has trailing bytes".to_string(),
        ));
    }
    Ok(Manifest {
        generation,
        base,
        segments,
        entry_count,
        head_id,
    })
}

struct ManifestCursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl ManifestCursor<'_> {
    fn read(&mut self, len: usize) -> Result<&[u8]> {
        let end = self.offset.checked_add(len).ok_or_else(|| {
            HeddleError::InvalidObject("oplog manifest length overflow".to_string())
        })?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| HeddleError::InvalidObject("truncated oplog manifest".to_string()))?;
        self.offset = end;
        Ok(value)
    }

    fn read_u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.read(4)?.try_into().unwrap()))
    }

    fn read_u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.read(8)?.try_into().unwrap()))
    }

    fn read_string(&mut self) -> Result<String> {
        let len = self.read_u32()? as usize;
        String::from_utf8(self.read(len)?.to_vec())
            .map_err(|_| HeddleError::InvalidObject("non-UTF-8 oplog manifest path".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::Utc;
    use objects::object::Principal;
    use tempfile::TempDir;

    use super::*;
    use crate::oplog::packed_oplog::{packed_file_full_reads, reset_packed_file_full_reads};

    fn entry(id: u64) -> OpEntry {
        let state = crate::oplog::fresh_state_id();
        OpEntry {
            id,
            timestamp: Utc::now(),
            operation: OpRecord::Snapshot {
                new_state: state,
                prev_head: None,
                head: Some(state),
                thread: None,
            },
            undone: false,
            batch_id: id,
            batch_index: 0,
            scope: Some("lane".to_string()),
            actor: Arc::new(Principal::new("Test", "test@example.com")),
            operation_id: None,
        }
    }

    fn empty_view(temp: &TempDir) -> (PathBuf, SegmentedOpLogIndex) {
        let path = temp.path().join("oplog.bin");
        PackedOpLog::new(path.clone()).save().unwrap();
        let view = SegmentedOpLogIndex::open(&path).unwrap();
        (path, view)
    }

    #[test]
    fn append_publishes_only_one_new_immutable_segment() {
        let temp = TempDir::new().unwrap();
        let (path, view) = empty_view(&temp);
        let base_before = fs::read(&path).unwrap();

        reset_packed_file_full_reads();
        let updated = view.append_entries(&[entry(1)]).unwrap();
        assert_eq!(
            packed_file_full_reads(),
            1,
            "append validates only its new segment"
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            base_before,
            "the V4 base stays immutable"
        );
        assert_eq!(updated.segment_count(), 1);
        assert_eq!(updated.last_entry().unwrap().unwrap().id, 1);

        let reopened = SegmentedOpLogIndex::open(&path).unwrap();
        assert_eq!(reopened.head_id(), 1);
        assert_eq!(reopened.recent_entries(1).unwrap()[0].id, 1);
    }

    #[test]
    fn binary_tiers_bound_segments_without_rewriting_the_base() {
        let temp = TempDir::new().unwrap();
        let (path, mut view) = empty_view(&temp);
        let base_before = fs::read(&path).unwrap();
        reset_packed_file_full_reads();
        for id in 1..=128 {
            view = view.append_entries(&[entry(id)]).unwrap();
        }

        for entry in fs::read_dir(temp.path().join("oplog.segments"))
            .unwrap()
            .filter_map(std::result::Result::ok)
        {
            fs::File::options()
                .write(true)
                .open(entry.path())
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH)
                .unwrap();
        }
        sweep_unlisted_containers(&path, &view.manifest);

        assert_eq!(view.segment_count(), 1);
        assert_eq!(view.manifest.segments[0].level, 7);
        assert_eq!(view.materialize_entries().unwrap().len(), 128);
        assert_eq!(fs::read(&path).unwrap(), base_before);
        assert_eq!(
            packed_file_full_reads(),
            128,
            "each append validates only its newly published carry segment"
        );
        let segment_files = fs::read_dir(temp.path().join("oplog.segments"))
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
            .count();
        assert_eq!(
            segment_files, 1,
            "binary carry cleanup retains only the selected tier"
        );

        let reopened = SegmentedOpLogIndex::open(&path).unwrap();
        assert_eq!(reopened.head_id(), 128);
    }

    #[test]
    fn unlisted_crash_tail_is_ignored_but_missing_listed_segment_fails() {
        let temp = TempDir::new().unwrap();
        let (path, view) = empty_view(&temp);
        let updated = view.append_entries(&[entry(1)]).unwrap();
        let orphan = temp.path().join("oplog.segments/orphan.bin");
        fs::write(&orphan, b"partial crash tail").unwrap();
        let staged_generation = temp
            .path()
            .join("oplog.segments/segment-l00-00000000000000000002-00000000000000000002-g999.bin");
        let stale_generation = temp.path().join(
            "oplog.segments/segment-l00-00000000000000000000-00000000000000000000-g00000000000000000000.bin",
        );
        let staged_temp = temp.path().join(
            "oplog.segments/.segment-l00-00000000000000000002-00000000000000000002-g999.bin.tmp-1-2-3",
        );
        let unknown_temp = temp.path().join(
            "oplog.segments/.segment-l00-00000000000000000002-00000000000000000002-g999.bin.tmp-operator-note",
        );
        let manifest_temp = temp.path().join(".oplog.manifest.tmp-1-2-3");
        let active_segment_temp = objects::fs_atomic::temp_path(&staged_generation);
        let active_manifest_temp = objects::fs_atomic::temp_path(&manifest_path(&path));
        fs::write(&staged_generation, b"complete but unpublished").unwrap();
        fs::write(&stale_generation, b"old unpublished generation").unwrap();
        fs::File::options()
            .write(true)
            .open(&stale_generation)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH)
            .unwrap();
        fs::write(&staged_temp, b"partial atomic write").unwrap();
        fs::write(&unknown_temp, b"operator evidence").unwrap();
        fs::write(&manifest_temp, b"partial manifest").unwrap();
        fs::write(&active_segment_temp, b"active segment write").unwrap();
        fs::write(&active_manifest_temp, b"active manifest write").unwrap();
        assert_eq!(SegmentedOpLogIndex::open(&path).unwrap().head_id(), 1);
        let updated = updated.append_entries(&[entry(2)]).unwrap();
        assert!(
            staged_generation.exists(),
            "a future generation may be between its segment and manifest rename"
        );
        assert!(!stale_generation.exists());
        assert!(!staged_temp.exists());
        assert!(!manifest_temp.exists());
        assert!(active_segment_temp.exists());
        assert!(active_manifest_temp.exists());
        assert!(orphan.exists(), "unknown operator files are not GC targets");
        assert!(unknown_temp.exists(), "non-atomic temp names are preserved");

        let listed = resolve_relative(&path, &updated.manifest.segments[0].path).unwrap();
        fs::remove_file(listed).unwrap();
        assert!(SegmentedOpLogIndex::open(&path).is_err());
    }

    #[test]
    fn atomic_temp_sweep_requires_a_valid_old_writer_stamp() {
        assert!(!atomic_temp_is_stale(
            ".oplog.manifest.tmp-writer-2-3",
            ".oplog.manifest"
        ));
        assert!(!atomic_temp_is_stale(
            ".oplog.manifest.tmp-1-time-3",
            ".oplog.manifest"
        ));
        assert!(!atomic_temp_is_stale(
            ".oplog.manifest.tmp-1-2-counter",
            ".oplog.manifest"
        ));
        assert!(atomic_temp_is_stale(
            ".oplog.manifest.tmp-1-2-3",
            ".oplog.manifest"
        ));
        let temp = TempDir::new().unwrap();
        let old = temp
            .path()
            .join("segment-l00-1-1-g00000000000000000002.bin");
        fs::write(&old, b"old").unwrap();
        fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH)
            .unwrap();
        assert!(!generation_container_is_sweepable(&old, "operator.bin", 2));
        assert!(!generation_container_is_sweepable(
            &old,
            "segment-note.bin",
            2
        ));
        assert!(generation_container_is_sweepable(
            &old,
            "segment-l00-1-1-g00000000000000000002.bin",
            2
        ));
        assert!(!generation_container_is_sweepable(
            &old,
            "segment-l00-2-2-g00000000000000000003.bin",
            2
        ));
        let recent = temp
            .path()
            .join("segment-l00-2-2-g00000000000000000001.bin");
        fs::write(&recent, b"recent").unwrap();
        assert!(!generation_container_is_sweepable(
            &recent,
            "segment-l00-2-2-g00000000000000000001.bin",
            2
        ));
    }

    #[test]
    fn corrupt_manifest_fails_without_falling_back_to_stale_base() {
        let temp = TempDir::new().unwrap();
        let (path, view) = empty_view(&temp);
        view.append_entries(&[entry(1)]).unwrap();
        let manifest = manifest_path(&path);
        let mut bytes = fs::read(&manifest).unwrap();
        bytes[0] ^= 0xff;
        fs::write(manifest, bytes).unwrap();
        assert!(SegmentedOpLogIndex::open(&path).is_err());
    }

    #[test]
    fn entry_id_sequence_must_start_at_one_without_or_with_a_manifest() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("oplog.bin");
        let mut packed = PackedOpLog::new(path.clone());
        packed.entries = vec![entry(2)];
        packed.head_id = 2;
        packed.save().unwrap();

        let error = SegmentedOpLogIndex::open(&path).unwrap_err().to_string();
        assert!(error.contains("must start at 1"), "{error}");

        let manifest = Manifest {
            generation: 1,
            base: "oplog.bin".to_string(),
            segments: Vec::new(),
            entry_count: 1,
            head_id: 2,
        };
        write_manifest(&path, &encode_manifest(&manifest).unwrap()).unwrap();
        let error = SegmentedOpLogIndex::open(&path).unwrap_err().to_string();
        assert!(error.contains("must start at 1"), "{error}");

        PackedOpLog::new(path.clone()).save().unwrap();
        let segment_path = temp.path().join("segment.bin");
        let mut segment = PackedOpLog::new(segment_path);
        segment.entries = vec![entry(2)];
        segment.head_id = 2;
        segment.save().unwrap();
        let manifest = Manifest {
            generation: 2,
            base: "oplog.bin".to_string(),
            segments: vec![SegmentDescriptor {
                level: 0,
                path: "segment.bin".to_string(),
            }],
            entry_count: 1,
            head_id: 2,
        };
        write_manifest(&path, &encode_manifest(&manifest).unwrap()).unwrap();
        let error = SegmentedOpLogIndex::open(&path).unwrap_err().to_string();
        assert!(error.contains("must start at 1"), "{error}");
    }

    #[test]
    fn manifest_codec_round_trips_generation_and_paths() {
        let manifest = Manifest {
            generation: 42,
            base: "oplog.segments/base.bin".to_string(),
            segments: vec![SegmentDescriptor {
                level: 3,
                path: "oplog.segments/segment.bin".to_string(),
            }],
            entry_count: 9,
            head_id: 12,
        };
        assert_eq!(
            parse_manifest(&encode_manifest(&manifest).unwrap()).unwrap(),
            manifest
        );
    }

    #[test]
    fn automatic_repair_refuses_suffix_loss_until_explicit_recovery_reports_it() {
        let temp = TempDir::new().unwrap();
        let (path, view) = empty_view(&temp);
        let mut current = view;
        for id in 1..=7 {
            current = current.append_entries(&[entry(id)]).unwrap();
        }
        let generation_before = current.manifest.generation;
        let middle = resolve_relative(&path, &current.manifest.segments[1].path).unwrap();
        let later = resolve_relative(&path, &current.manifest.segments[2].path).unwrap();
        let later_bytes = fs::read(&later).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&middle)
            .unwrap()
            .set_len(40)
            .unwrap();

        let manifest_before = fs::read(manifest_path(&path)).unwrap();
        let error = SegmentedOpLogIndex::validate_current(&path)
            .unwrap_err()
            .to_string();
        assert!(error.contains("would discard 2 later segment"), "{error}");
        assert!(
            error.contains("heddle maintenance oplog recover"),
            "{error}"
        );
        assert_eq!(fs::read(manifest_path(&path)).unwrap(), manifest_before);
        assert!(
            later.exists(),
            "automatic repair must retain the healthy suffix"
        );
        for entry in fs::read_dir(temp.path().join("oplog.segments"))
            .unwrap()
            .filter_map(std::result::Result::ok)
        {
            fs::File::options()
                .write(true)
                .open(entry.path())
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH)
                .unwrap();
        }

        let report = SegmentedOpLogIndex::recover(&path).unwrap();
        assert!(!report.already_healthy);
        assert!(report.prior_recovery);
        assert!(report.sidecar_path.as_ref().unwrap().exists());
        assert!(middle.with_extension("bin.corrupt").exists());
        assert_eq!(report.suffix_segments_discarded, 2);
        assert_eq!(report.suffix_entries_discarded, 1);
        assert!(!later.exists(), "pruned suffix leaves the live namespace");
        let quarantined = std::fs::read_dir(temp.path().join("oplog.quarantine"))
            .unwrap()
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        assert_eq!(
            quarantined.len(),
            usize::try_from(report.suffix_segments_discarded).unwrap(),
            "every logically discarded suffix container must be quarantined"
        );
        assert!(
            quarantined
                .iter()
                .any(|candidate| fs::read(candidate).unwrap() == later_bytes),
            "the intact committed suffix must remain byte-for-byte recoverable"
        );
        let recovered = SegmentedOpLogIndex::open(&path).unwrap();
        assert_eq!(recovered.head_id(), 4);
        assert_eq!(recovered.segment_count(), 1);
        assert!(recovered.manifest.generation > generation_before);
        let generation_containers = std::fs::read_dir(temp.path().join("oplog.segments"))
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                (name.starts_with("base-") || name.starts_with("segment-"))
                    && name.ends_with(".bin")
            })
            .count();
        assert_eq!(
            generation_containers, 1,
            "repair must collect every pruned immutable container"
        );

        let repeated = SegmentedOpLogIndex::recover(&path).unwrap();
        assert!(repeated.already_healthy);
        assert!(repeated.prior_recovery);
        assert_eq!(repeated.suffix_segments_discarded, 2);
        assert_eq!(repeated.suffix_entries_discarded, 1);
        assert_eq!(repeated.sidecar_path, report.sidecar_path);
    }

    #[test]
    fn repeat_recovery_prefers_generation_report_when_damaged_base_stays_selected() {
        let temp = TempDir::new().unwrap();
        let (path, view) = empty_view(&temp);
        let mut current = view.replace_entries((1..=4).map(entry).collect()).unwrap();
        for id in 5..=7 {
            current = current.append_entries(&[entry(id)]).unwrap();
        }
        let base = resolve_relative(&path, &current.manifest.base).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&base)
            .unwrap()
            .set_len(40)
            .unwrap();

        let error = SegmentedOpLogIndex::validate_current(&path)
            .unwrap_err()
            .to_string();
        assert!(error.contains("would discard 2 later segment"), "{error}");

        let report = SegmentedOpLogIndex::recover(&path).unwrap();
        assert_eq!(report.suffix_segments_discarded, 2);
        assert_eq!(report.suffix_entries_discarded, 3);
        assert!(
            base.with_extension("bin.oplog.recovery").exists(),
            "the selected repaired base retains its container-local sidecar"
        );
        let recovered = SegmentedOpLogIndex::open(&path).unwrap();
        assert_eq!(
            resolve_relative(&path, &recovered.manifest.base).unwrap(),
            base,
            "the repaired base remains selected after suffix truncation"
        );

        let repeated = SegmentedOpLogIndex::recover(&path).unwrap();
        assert!(repeated.already_healthy);
        assert!(repeated.prior_recovery);
        assert_eq!(repeated.suffix_segments_discarded, 2);
        assert_eq!(repeated.suffix_entries_discarded, 3);
        assert_eq!(repeated.sidecar_path, report.sidecar_path);

        let advanced = recovered.append_entries(&[entry(1)]).unwrap();
        let latest_segment = resolve_relative(&path, &advanced.manifest.segments[0].path).unwrap();
        let mut torn = fs::read(&latest_segment).unwrap();
        torn.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        let automatic_damaged_end = torn.len() as u64;
        fs::write(&latest_segment, torn).unwrap();
        SegmentedOpLogIndex::validate_current(&path).unwrap();

        let latest_repeat = SegmentedOpLogIndex::recover(&path).unwrap();
        assert!(latest_repeat.already_healthy);
        assert!(latest_repeat.prior_recovery);
        assert_eq!(latest_repeat.strategy.as_deref(), Some("footer-guided"));
        assert_eq!(latest_repeat.entries_recovered, 1);
        assert_eq!(latest_repeat.damaged_byte_end, automatic_damaged_end);
        assert!(latest_repeat.damaged_byte_start > 40);
        assert_eq!(latest_repeat.suffix_segments_discarded, 0);
        assert_eq!(latest_repeat.suffix_entries_discarded, 0);
        assert_eq!(latest_repeat.sidecar_path, report.sidecar_path);

        let healthy = SegmentedOpLogIndex::open(&path).unwrap();
        let two = healthy.append_entries(&[entry(2)]).unwrap();
        let latest_segment = resolve_relative(&path, &two.manifest.segments[0].path).unwrap();
        let mut torn = fs::read(&latest_segment).unwrap();
        torn.extend_from_slice(&[0xca, 0xfe, 0xba, 0xbe]);
        let explicit_damaged_end = torn.len() as u64;
        fs::write(&latest_segment, torn).unwrap();
        let explicit = SegmentedOpLogIndex::recover(&path).unwrap();
        assert!(!explicit.already_healthy);
        assert_eq!(explicit.strategy.as_deref(), Some("footer-guided"));
        assert_eq!(explicit.entries_recovered, 2);
        assert_eq!(explicit.entries_lost, Some(0));
        assert_eq!(explicit.damaged_byte_end, explicit_damaged_end);
        assert!(explicit.damaged_byte_start > 40);
        assert_eq!(explicit.suffix_segments_discarded, 0);
        assert_eq!(explicit.suffix_entries_discarded, 0);

        let explicit_repeat = SegmentedOpLogIndex::recover(&path).unwrap();
        assert!(explicit_repeat.already_healthy);
        assert!(explicit_repeat.prior_recovery);
        assert_eq!(explicit_repeat.strategy, explicit.strategy);
        assert_eq!(explicit_repeat.entries_recovered, 2);
        assert_eq!(explicit_repeat.damaged_byte_end, explicit_damaged_end);
        assert_eq!(explicit_repeat.suffix_segments_discarded, 0);
        assert_eq!(explicit_repeat.suffix_entries_discarded, 0);
    }

    #[test]
    fn forged_duplicate_reordered_and_overlapping_manifests_are_rejected() {
        let temp = TempDir::new().unwrap();
        let (path, view) = empty_view(&temp);
        let one = view.append_entries(&[entry(1)]).unwrap();
        let two = one.append_entries(&[entry(2)]).unwrap();
        let three = two.append_entries(&[entry(3)]).unwrap();

        let mut duplicate = three.manifest.clone();
        duplicate.segments[1] = duplicate.segments[0].clone();
        write_manifest(&path, &encode_manifest(&duplicate).unwrap()).unwrap();
        let error = SegmentedOpLogIndex::open(&path).unwrap_err().to_string();
        assert!(error.contains("repeats a container path"), "{error}");
        assert!(SegmentedOpLogIndex::recover(&path).is_err());

        let mut reordered = three.manifest.clone();
        reordered.segments.swap(0, 1);
        write_manifest(&path, &encode_manifest(&reordered).unwrap()).unwrap();
        let error = SegmentedOpLogIndex::open(&path).unwrap_err().to_string();
        assert!(
            error.contains("not contiguous") || error.contains("levels"),
            "{error}"
        );

        let overlap_relative = "oplog.segments/overlap.bin";
        let overlap_path = resolve_relative(&path, overlap_relative).unwrap();
        let mut overlap_container = PackedOpLog::new(overlap_path);
        overlap_container.entries = vec![entry(1)];
        overlap_container.head_id = 1;
        overlap_container.save().unwrap();
        let mut overlap = two.manifest.clone();
        overlap.segments.push(SegmentDescriptor {
            level: 0,
            path: overlap_relative.to_string(),
        });
        overlap.entry_count = 3;
        write_manifest(&path, &encode_manifest(&overlap).unwrap()).unwrap();
        let error = SegmentedOpLogIndex::open(&path).unwrap_err().to_string();
        assert!(error.contains("not contiguous"), "{error}");
    }
}
