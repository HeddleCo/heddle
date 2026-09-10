//! Exact-revision detail projections, bounded before decoding source payloads.
use std::collections::BTreeSet;

use anyhow::{Context, Result, ensure};
use api::heddle::api::{v1alpha1 as shared, v2alpha1::*};
use objects::{
    object::{ContentHash, State},
    store::ObjectStore,
};
use prost::Message;

use super::{
    auth::Session,
    checkout,
    content::{complete, normalize},
};

type Payload = content_event::Payload;
fn page(
    read: &PageRequest,
    binding: &[u8; 32],
    total: usize,
    capacity: usize,
) -> Result<(usize, usize, PageInfo)> {
    let after = if read.after_page.is_empty() {
        0
    } else {
        ensure!(
            read.after_page.len() == 40 && read.after_page[..32] == binding[..],
            "page differs from exact revision or query"
        );
        u64::from_le_bytes(read.after_page[32..].try_into()?) as usize
    };
    ensure!(after <= total, "page offset exceeds collection");
    let size = if read.size == 0 {
        100
    } else {
        read.size as usize
    };
    ensure!(capacity > 0, "read budget has no page capacity");
    let end = total.min(after.saturating_add(size.min(capacity)));
    Ok((
        after,
        end,
        PageInfo {
            exhausted: end == total,
            next_page: if end == total {
                vec![]
            } else {
                [binding.as_slice(), &(end as u64).to_le_bytes()].concat()
            },
            ..Default::default()
        },
    ))
}
fn blob(
    store: &objects::store::FsStore,
    hash: &ContentHash,
    max: u64,
) -> Result<objects::object::Blob> {
    let size = objects::store::ObjectSource::decoded_blob_len(store, hash)?
        .context("content object unavailable")?;
    ensure!(size <= max, "decoded content exceeds budget");
    let value = store
        .get_blob(hash)?
        .context("content object unavailable")?;
    ensure!(
        value.hash() == *hash && value.size() as u64 == size,
        "content object identity mismatch"
    );
    Ok(value)
}
pub(super) fn state(state: &State, revision: &RevisionRef, emit: &mut impl FnMut(Payload) -> Result<()>) -> Result<()> {
    emit(Payload::State(super::content_summary::summary(state, None)))?;
    emit(Payload::SelectionComplete(complete("state", revision, Coverage::Complete, None)))
}
pub(super) fn diff(
    repository: &repo::Repository,
    session: &Session,
    state: &State,
    revision: &RevisionRef,
    read: &DiffRead,
    limits: ReadBudget,
    emit: &mut impl FnMut(Payload) -> Result<()>,
) -> Result<()> {
    let base = checkout::revision(session, read.base.as_ref())?;
    let previous = repository
        .store()
        .get_state(&base)?
        .context("base revision unavailable")?;
    ensure!(previous.id() == base, "base revision identity mismatch");
    ensure!(read.paths.len() <= 128, "too many diff paths");
    for path in &read.paths {
        normalize(path, false)?;
    }
    // Preflight both closures before invoking the existing rename-aware diff engine.
    let mut bytes = 0u64;
    for root in [state.tree, previous.tree] {
        let mut pending = vec![root];
        let mut visited = BTreeSet::new();
        while let Some(hash) = pending.pop() {
            if !visited.insert(hash) {
                continue;
            }
            ensure!(visited.len() <= 4096, "diff tree work exceeds budget");
            let tree = repository
                .store()
                .get_tree(&hash)?
                .context("diff tree unavailable")?;
            ensure!(tree.hash() == hash, "diff tree identity mismatch");
            for entry in tree.entries() {
                if let Some(child) = entry.tree_hash() {
                    pending.push(child);
                }
                if let Some(hash) = entry.leaf_content_hash() {
                    bytes = bytes
                        .checked_add(
                            objects::store::ObjectSource::decoded_blob_len(
                                repository.store(),
                                &hash,
                            )?
                            .context("diff blob unavailable")?,
                        )
                        .context("diff size overflow")?;
                    ensure!(
                        bytes <= limits.max_snapshot_bytes,
                        "diff source bytes exceed budget"
                    );
                }
            }
        }
    }
    let result = verbs::diff::compute_state_diff(repository, &base, &state.id(), false, 3)?;
    let changes: Vec<_> = result
        .changes
        .into_iter()
        .filter(|change| {
            read.paths.is_empty()
                || read.paths.iter().any(|path| {
                    change.path == *path || change.path.starts_with(&format!("{path}/"))
                })
        })
        .collect();
    let mut query = read.clone();
    query.page = None;
    let binding = blake3::hash(&[revision.encode_to_vec(), query.encode_to_vec()].concat());
    let (start, end, page) = page(
        &read.page.clone().unwrap_or_default(),
        binding.as_bytes(),
        changes.len(),
        limits.max_items.saturating_sub(1) as usize,
    )?;
    for change in &changes[start..end] {
        let kind = match change.kind.as_str() {
            "added" => shared::FileDiffKind::Added,
            "deleted" => shared::FileDiffKind::Deleted,
            "renamed" => shared::FileDiffKind::Renamed,
            _ => shared::FileDiffKind::Modified,
        };
        ensure!(
            change.lines.is_some() || change.binary,
            "diff engine could not compute selected content"
        );
        let mut hunks: Vec<shared::DiffHunk> = Vec::new();
        for line in change.lines.iter().flatten() {
            if line.prefix == "@" {
                let parts: Vec<_> = line.content.split_whitespace().collect();
                ensure!(
                    parts.len() == 4 && parts[0] == "@" && parts[3] == "@@",
                    "invalid native diff hunk header"
                );
                let span = |value: &str, prefix: char| -> Result<(u32, u32)> {
                    let (start, count) = value
                        .strip_prefix(prefix)
                        .context("diff hunk sign")?
                        .split_once(',')
                        .context("diff hunk range")?;
                    Ok((start.parse()?, count.parse()?))
                };
                let (old_start, old_lines) = span(parts[1], '-')?;
                let (new_start, new_lines) = span(parts[2], '+')?;
                hunks.push(shared::DiffHunk {
                    old_start,
                    old_lines,
                    new_start,
                    new_lines,
                    ..Default::default()
                });
            } else {
                let hunk = hunks.last_mut().context("native diff line has no hunk")?;
                hunk.lines.push(shared::DiffLine {
                    kind: match line.prefix.as_str() {
                        "+" => shared::DiffLineKind::Added,
                        "-" => shared::DiffLineKind::Removed,
                        " " => shared::DiffLineKind::Context,
                        _ => anyhow::bail!("unknown native diff line"),
                    } as i32,
                    content: line.content.clone(),
                    ..Default::default()
                });
            }
        }
        emit(Payload::Diff(shared::FileDiff {
            path: change.path.clone(),
            kind: kind as i32,
            hunks,
            ..Default::default()
        }))?;
    }
    emit(Payload::SelectionComplete(complete(
        "diff",
        revision,
        Coverage::Complete,
        Some(page),
    )))
}
pub(super) fn provenance(
    repository: &repo::Repository,
    state: &State,
    revision: &RevisionRef,
    read: &ProvenanceRead,
    limits: ReadBudget,
    emit: &mut impl FnMut(Payload) -> Result<()>,
) -> Result<()> {
    normalize(&read.path, false)?;
    let mut work = 0;
    let hash = super::content::blob_hash(
        repository.store(),
        state,
        &BlobRead {
            source: Some(blob_read::Source::Path(read.path.clone())),
            ..Default::default()
        },
        &mut work,
    )?;
    let source = blob(repository.store(), &hash, limits.max_snapshot_bytes)?;
    let text = std::str::from_utf8(source.content()).context("provenance requires text")?;
    let mut result = ProvenanceResult {
        path: read.path.clone(),
        revision: Some(revision.clone()),
        coverage: Coverage::Unavailable as i32,
        ..Default::default()
    };
    if let Some(root) = state.provenance {
        let entry = super::content::path_entry(repository.store(), root, &read.path, &mut work)?;
        let hash = entry
            .leaf_content_hash()
            .context("provenance path must be a blob")?;
        let encoded = blob(repository.store(), &hash, limits.max_snapshot_bytes)?;
        let provenance: objects::object::FileProvenance = rmp_serde::from_slice(encoded.content())?;
        provenance.validate()?;
        ensure!(
            provenance.file_blob == source.hash(),
            "provenance differs from exact source blob"
        );
        let lines: Vec<_> = text.lines().collect();
        ensure!(
            lines.len() == provenance.line_count as usize,
            "provenance line count mismatch"
        );
        let start = read.start_line.max(1) - 1;
        ensure!(start <= provenance.line_count, "line offset exceeds file");
        let end = if read.line_count == 0 {
            provenance.line_count
        } else {
            provenance
                .line_count
                .min(start.saturating_add(read.line_count))
        };
        let mut distinct = BTreeSet::new();
        let mut multi = 0;
        let mut carried = 0;
        for span in &provenance.spans {
            let set = &provenance.origin_sets[span.origin_set_index as usize];
            let origins: Vec<_> = set
                .origin_indexes
                .iter()
                .map(|index| {
                    let origin = &provenance.origins[*index as usize];
                    shared::BlameOrigin {
                        state_id: Some(shared::StateId {
                            value: origin.state_id.as_bytes().to_vec(),
                        }),
                        author: origin.attribution.principal.name_lossy().into_owned(),
                        timestamp: origin.authored_at.unwrap_or(origin.created_at).to_rfc3339(),
                        principal_name: origin.attribution.principal.name_lossy().into_owned(),
                        principal_email: origin.attribution.principal.email_lossy().into_owned(),
                        agent_provider: origin
                            .attribution
                            .agent
                            .as_ref()
                            .map(|agent| agent.provider.clone())
                            .unwrap_or_default(),
                        agent_model: origin
                            .attribution
                            .agent
                            .as_ref()
                            .map(|agent| agent.model.clone())
                            .unwrap_or_default(),
                    }
                })
                .collect();
            for line in span.start_line.max(start)..(span.start_line + span.line_len).min(end) {
                let first = origins.first().context("empty provenance origin set")?;
                let is_carried = set
                    .origin_indexes
                    .iter()
                    .any(|index| provenance.origins[*index as usize].state_id != state.id());
                distinct.extend(set.origin_indexes.iter().copied());
                multi += u32::from(origins.len() > 1);
                carried += u32::from(is_carried);
                result.lines.push(shared::BlameLine {
                    line_number: line + 1,
                    content: lines[line as usize].into(),
                    state_id: first.state_id.clone(),
                    author: first.author.clone(),
                    timestamp: first.timestamp.clone(),
                    origins: origins.clone(),
                    carried: is_carried,
                });
                ensure!(
                    result.encoded_len() <= limits.max_frame_bytes as usize / 2,
                    "provenance range exceeds frame budget; request fewer lines"
                );
            }
        }
        result.coverage = Coverage::Complete as i32;
        result.summary = Some(shared::BlameSummary {
            total_lines: end - start,
            distinct_origin_count: distinct.len() as u32,
            multi_origin_line_count: multi,
            carried_line_count: carried,
        });
    }
    let coverage = Coverage::try_from(result.coverage)?;
    emit(Payload::Provenance(result))?;
    emit(Payload::SelectionComplete(complete(
        "provenance",
        revision,
        coverage,
        None,
    )))
}
