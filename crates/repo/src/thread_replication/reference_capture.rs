//! Capture-time shared targets. Immutable blobs precede SQL publication;
//! admission and the rebuildable root pointer share one metadata transaction.
use std::collections::BTreeMap;

use objects::{
    object::{
        AnnotationSourceReference, AnnotationTag, Blob, CollaborationAnchor,
        CollaborationOperationBodyV1, CollaborationOperationEnvelope, CollaborationRevision,
        CollaborationScope, ContentHash, State, StateId, TreeEntryTarget,
        source_target::{
            SourceAffinity, SourceFileCore, SourceLineRange, SourceRangeProjection, SourceSelector,
            SourceTargetBinding, SourceTargetCore,
            capture::{
                self, FileResolution, ReferenceClosure, ResolutionStatus, SourceTargetSnapshot,
                TargetResolution,
            },
        },
        source_target_map::{MapBudget, SourceTargetMap},
        thread_replication::{Capture, ThreadFacet, ThreadOperation, ThreadOperationBody},
    },
    reference_store::{MapStore, Source},
    store::ObjectStore,
};
use rusqlite::{OptionalExtension, Transaction, params};

use super::{Error, Result, ThreadReplica};
use crate::Repository;

fn err(value: impl std::fmt::Display) -> Error {
    Error::Invalid(value.to_string())
}
fn budget() -> MapBudget {
    MapBudget::new(65_536, 32 * 1024 * 1024, 65_536, 32 * 1024 * 1024)
}
fn put(store: &impl ObjectStore, value: &impl serde::Serialize) -> Result<ContentHash> {
    Ok(store.put_blob(&Blob::new(capture::encode(value)?))?)
}
fn selector(source: &objects::object::CollaborationSourceAnchor) -> Result<SourceSelector> {
    Ok(if !source.symbol_id.is_empty() {
        SourceSelector::Symbol {
            address: source.symbol_id.clone(),
        }
    } else if let (Some(start), Some(end)) = (source.start_line, source.end_line) {
        SourceSelector::Lines {
            range: SourceLineRange {
                start: start
                    .checked_sub(1)
                    .ok_or_else(|| err("source lines are one-based"))?,
                end,
                start_affinity: SourceAffinity::After,
                end_affinity: SourceAffinity::Before,
            },
        }
    } else {
        SourceSelector::File
    })
}
fn files(store: &impl ObjectStore, root: ContentHash) -> Result<BTreeMap<String, ContentHash>> {
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
impl ThreadReplica {
    pub(super) fn validate_reference_capture(
        &self,
        operation: &ThreadOperation,
        store: &impl ObjectStore,
    ) -> Result<()> {
        if let Some(proof) = operation.reference_proof(&self.genesis()?)? {
            capture::closure(&Source(store), proof.descriptor, &proof.scope, proof.state)?;
        }
        Ok(())
    }
    /// Materialize a shared reference at the exact viewed revision. Callers
    /// separately authorize the selected scope; references grant no access.
    pub fn resolve_source_target(
        &self,
        repo: &Repository,
        reference: &objects::object::source_target::SourceTargetReference,
        viewed_revision: StateId,
    ) -> Result<Option<capture::ResolvedSourceTarget>> {
        let viewed = self.reference_scope()?;
        let scope = reference.binding.scope(&viewed).map_err(err)?.clone();
        if scope.spool != viewed.spool {
            return Err(err("cross-Spool reference requires its owning endpoint"));
        }
        let owner = scope.thread.unwrap_or(self.thread);
        let replica = ThreadReplica::open(repo.heddle_dir(), owner)?;
        let revision = match &reference.binding {
            SourceTargetBinding::ViewedThread => viewed_revision,
            SourceTargetBinding::NamedThread { .. } => {
                let projection = replica.projection()?;
                match projection.source_heads.as_slice() {
                    [] => projection.genesis.base,
                    [state] => *state,
                    _ => {
                        return Err(err(
                            "named Thread has concurrent source heads; select a revision",
                        ));
                    }
                }
            }
            SourceTargetBinding::PinnedRevision { revision, .. } => match revision {
                CollaborationRevision::State { state_id } => *state_id,
                CollaborationRevision::GitCommit { oid } => repo
                    .git_overlay_mapped_state_for_git_commit(oid)?
                    .ok_or_else(|| err("pinned Git revision is not imported"))?,
            },
        };
        let mut result = None;
        for snapshot in replica.reference_snapshots_at(revision, repo.store())? {
            if let Some(target) = snapshot.targets.get(&reference.target) {
                let file = snapshot
                    .files
                    .get(&target.core.file)
                    .ok_or_else(|| err("target file resolution is missing"))?;
                let resolved = capture::ResolvedSourceTarget {
                    scope: scope.clone(),
                    state: revision,
                    file: file.clone(),
                    target: target.clone(),
                };
                if result
                    .as_ref()
                    .is_some_and(|previous| previous != &resolved)
                {
                    return Err(err(
                        "reference has concurrent resolutions at this source revision",
                    ));
                }
                result = Some(resolved);
            }
        }
        Ok(result)
    }
    fn reference_scope(&self) -> Result<CollaborationScope> {
        Ok(CollaborationScope {
            spool: self.genesis()?.spool.parse().map_err(err)?,
            thread: Some(self.thread),
        })
    }
    /// Called after source capture but before signing its publication. Retrying
    /// an admitted State reuses its exact signed descriptor, never today's tags.
    pub fn prepare_capture(&self, repo: &Repository, state: &State) -> Result<Capture> {
        let connection = self.connect()?;
        let existing:Option<Vec<u8>>=connection.query_row("SELECT o.canonical FROM operations o WHERE o.thread=?1 AND o.source_revision=?2 AND o.status=1 ORDER BY o.id LIMIT 1",params![self.thread.as_bytes(),state.id().as_bytes()],|r|r.get(0)).optional()?;
        if let Some(bytes) = existing {
            if let ThreadOperationBody::Capture(capture) = ThreadOperation::decode(&bytes)?.body {
                return Ok(capture);
            }
        }
        let scope = self.reference_scope()?;
        let store = repo.store();
        let mut roots = Vec::new();
        for parent in &state.parents {
            roots.extend(self.reference_snapshots_at(*parent, store)?);
        }
        let mut inherited_files = BTreeMap::new();
        let mut inherited_targets = BTreeMap::new();
        let (mut file_root, mut target_root) = (None, None);
        if let Some(first) = roots.first() {
            file_root = first.snapshot.files;
            target_root = first.snapshot.targets;
        }
        for inherited in roots {
            for (id, file) in inherited.files {
                if let Some(prior) = inherited_files.get(&id) {
                    if prior != &file {
                        let mut conflict = file;
                        conflict.status = ResolutionStatus::Ambiguous;
                        inherited_files.insert(id, conflict);
                    }
                } else {
                    inherited_files.insert(id, file);
                }
            }
            for (id, target) in inherited.targets {
                if let Some(prior) = inherited_targets.get(&id) {
                    if prior != &target {
                        let mut conflict = target;
                        conflict.status = ResolutionStatus::Ambiguous;
                        inherited_targets.insert(id, conflict);
                    }
                } else {
                    inherited_targets.insert(id, target);
                }
            }
        }
        let (seed_count, seed_bytes): (i64, i64) = connection.query_row(
            "SELECT count(*),coalesce(sum(length(source)),0) FROM reference_seeds WHERE thread=?1",
            [self.thread.as_bytes()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if seed_count > 65_536 || seed_bytes > 32 * 1024 * 1024 {
            return Err(err("source target seed budget exceeded"));
        }
        let mut statement = connection.prepare(
            "SELECT source FROM reference_seeds WHERE thread=?1 ORDER BY target LIMIT 65537",
        )?;
        let seeds = statement
            .query_map([self.thread.as_bytes()], |r| r.get::<_, Vec<u8>>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if seeds.len() > 65_536 {
            return Err(err("source target seed budget exceeded"));
        }
        for bytes in seeds {
            let reference: AnnotationSourceReference = capture::decode(&bytes)?;
            reference.validate().map_err(err)?;
            let Some(target) = &reference.source.target else {
                continue;
            };
            if inherited_targets.contains_key(&target.target) {
                continue;
            }
            let core = SourceFileCore {
                scope: reference.scope.clone(),
                revision: reference.source.revision.clone(),
                path: reference.source.path.clone(),
            };
            let file = core.id().map_err(err)?;
            let target_core = SourceTargetCore {
                file,
                revision: reference.source.revision.clone(),
                selector: selector(&reference.source)?,
            };
            if target_core.id().map_err(err)? != target.target {
                return Err(err("new target identity differs from original evidence"));
            }
            let baseline_id = match &core.revision {
                CollaborationRevision::State { state_id } => *state_id,
                CollaborationRevision::GitCommit { oid } => repo
                    .git_overlay_mapped_state_for_git_commit(oid)?
                    .ok_or_else(|| err("tracked Git target has no imported State"))?,
            };
            let baseline = store
                .get_state(&baseline_id)?
                .ok_or_else(|| err("target original State missing"))?;
            let old = files(store, baseline.tree)?.get(&core.path).copied();
            inherited_files.entry(file).or_insert(FileResolution {
                path: core.path.clone(),
                core,
                blob: old,
                status: if old.is_some() {
                    ResolutionStatus::Resolved
                } else {
                    ResolutionStatus::Deleted
                },
            });
            inherited_targets.insert(
                target.target,
                TargetResolution {
                    selector: target_core.selector.clone(),
                    core: target_core,
                    status: ResolutionStatus::Resolved,
                },
            );
        }
        if inherited_targets.is_empty() {
            return Ok(state.encode_current_msgpack()?.into());
        }
        let current = files(store, state.tree)?;
        let mut pairs = BTreeMap::new();
        let mut maps = BTreeMap::new();
        let mut changed_bytes = 0u64;
        let mut map_store = MapStore(store);
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
                file.status = ResolutionStatus::Resolved;
            } else {
                file.status = if moved.is_empty() {
                    ResolutionStatus::Deleted
                } else {
                    ResolutionStatus::Ambiguous
                };
            }
            if previous.blob != file.blob || previous.status != file.status {
                pairs.insert(*id, (previous.clone(), file.clone()));
                if let (Some(old), Some(new)) = (previous.blob, file.blob) {
                    if old != new {
                        let length = store
                            .blob_size(&old)?
                            .unwrap_or(u64::MAX)
                            .saturating_add(store.blob_size(&new)?.unwrap_or(u64::MAX));
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
            }
            file_root = SourceTargetMap::update(
                &mut map_store,
                file_root,
                *id,
                Some(put(store, file)?),
                &mut work,
            )
            .map_err(err)?;
        }
        let mut line_maps = BTreeMap::new();
        for (file, (old, new)) in &maps {
            line_maps.insert(
                *file,
                objects::worktree::source_line_edit_map(old, new, 8 * 1024 * 1024, 65_536)
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
                            use objects::worktree::SourceLineMapBuild;
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
                            #[cfg(feature = "tree-sitter-symbols")]
                            if let Some((old, new)) = maps.get(&target.core.file) {
                                let update = crate::discussion_anchor_travel::travel_symbol_anchor(
                                    &std::collections::HashMap::from([(
                                        previous.path.clone(),
                                        old.content().to_vec(),
                                    )]),
                                    &std::collections::HashMap::from([(
                                        file.path.clone(),
                                        new.content().to_vec(),
                                    )]),
                                    &objects::object::SymbolAnchor::new(&previous.path, address),
                                );
                                target.selector = SourceSelector::Symbol {
                                    address: update.new_anchor.symbol,
                                };
                                target.status = if update.ambiguous {
                                    ResolutionStatus::Ambiguous
                                } else if update.orphaned {
                                    ResolutionStatus::Deleted
                                } else {
                                    ResolutionStatus::Resolved
                                };
                            }
                            #[cfg(not(feature = "tree-sitter-symbols"))]
                            {
                                let _ = (address, previous);
                                target.status = ResolutionStatus::Ambiguous;
                            }
                        }
                        SourceSelector::File => {}
                    }
                }
            }
            target_root = SourceTargetMap::update(
                &mut map_store,
                target_root,
                *id,
                Some(put(store, target)?),
                &mut work,
            )
            .map_err(err)?;
        }
        let frontier = self.frontier_page(ThreadFacet::Discussion, None, 129)?;
        if frontier.len() > 128 {
            return Err(err("reference collaboration frontier budget exceeded"));
        }
        let descriptor = SourceTargetSnapshot {
            version: 1,
            scope,
            state: state.id(),
            collaboration_frontier: ContentHash::compute_typed(
                "heddle-source-target-frontier-v1",
                &capture::encode(&frontier)?,
            ),
            files: file_root,
            targets: target_root,
        };
        Ok(Capture {
            state: state.encode_current_msgpack()?,
            source_targets: Some(put(store, &descriptor)?),
        })
    }
    fn reference_snapshots_at(
        &self,
        state: StateId,
        store: &impl ObjectStore,
    ) -> Result<Vec<ReferenceClosure>> {
        let connection = self.connect()?;
        let genesis = self.genesis()?;
        let (owner, bytes) = descriptor_rows_at(&connection, self.thread, state)?;
        let scope = CollaborationScope {
            spool: genesis.spool.parse().map_err(err)?,
            thread: Some(owner),
        };
        bytes
            .into_iter()
            .map(|bytes| {
                Ok(capture::closure(
                    &Source(store),
                    super::hash(&bytes)?,
                    &scope,
                    state,
                )?)
            })
            .collect()
    }
    pub(super) fn index_reference_sources(
        &self,
        tx: &Transaction<'_>,
        operation: &ThreadOperation,
        id: ContentHash,
    ) -> Result<()> {
        let mut sources = Vec::new();
        if let Some(context) = operation.context_revision()? {
            if let CollaborationAnchor::Source { source } = &context.anchor {
                sources.push(AnnotationSourceReference {
                    scope: context.metadata.scope.clone(),
                    source: source.clone(),
                });
            }
            for tag in &context.tags {
                match tag {
                    AnnotationTag::Source { target }
                    | AnnotationTag::Symbol {
                        target: Some(target),
                        ..
                    } => sources.push(target.clone()),
                    _ => {}
                }
            }
            crate::reference_projection::project_properties(
                tx,
                &context.metadata.scope,
                id,
                &context.tags,
            )
            .map_err(err)?;
        }
        if let ThreadOperationBody::Discussion(bytes) = &operation.body {
            let discussion = CollaborationOperationEnvelope::decode(bytes)
                .map_err(err)?
                .operation;
            if let (
                Some(metadata),
                CollaborationOperationBodyV1::Open {
                    anchor: CollaborationAnchor::Source { source },
                    ..
                },
            ) = (discussion.metadata, discussion.body)
            {
                sources.push(AnnotationSourceReference {
                    scope: metadata.scope,
                    source,
                });
            }
        }
        for source in sources {
            if let Some(target) = &source.source.target {
                if matches!(target.binding, SourceTargetBinding::ViewedThread) {
                    let scope = self.reference_scope()?;
                    if source.scope.spool != scope.spool {
                        return Err(err("viewed target original evidence crosses Spool"));
                    }
                    let existing:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM reference_seeds WHERE thread=?1 AND target=?2)",params![self.thread.as_bytes(),target.target.as_bytes()],|r|r.get(0))?;
                    let projected =
                        match crate::reference_projection::root(tx, &scope).map_err(err)? {
                            Some(root) => crate::reference_projection::resolve_at(
                                tx,
                                root,
                                target.target,
                                &mut budget(),
                            )
                            .map_err(err)?
                            .is_some(),
                            None => false,
                        };
                    if !existing && !projected {
                        let file = SourceFileCore {
                            scope: source.scope.clone(),
                            revision: source.source.revision.clone(),
                            path: source.source.path.clone(),
                        };
                        let core = SourceTargetCore {
                            file: file.id().map_err(err)?,
                            revision: source.source.revision.clone(),
                            selector: selector(&source.source)?,
                        };
                        if core.id().map_err(err)? != target.target {
                            return Err(err(
                                "new source target differs from its signed original evidence",
                            ));
                        }
                    }
                    tx.execute("INSERT OR IGNORE INTO reference_seeds(thread,operation,target,source) VALUES(?1,?2,?3,?4)",params![self.thread.as_bytes(),id.as_bytes(),target.target.as_bytes(),capture::encode(&source)?])?;
                }
            }
        }
        Ok(())
    }
    pub(super) fn publish_capture_references(
        &self,
        tx: &Transaction<'_>,
        operation: &ThreadOperation,
        id: ContentHash,
        store: &impl ObjectStore,
    ) -> Result<()> {
        let ThreadOperationBody::Capture(payload) = &operation.body else {
            return Ok(());
        };
        let Some(descriptor) = payload.source_targets else {
            return Ok(());
        };
        let state = State::decode_current_msgpack(&payload.state)?;
        let scope = self.reference_scope()?;
        let closure = capture::closure(&Source(store), descriptor, &scope, state.id())?;
        tx.execute("INSERT OR IGNORE INTO reference_captures(thread,state,operation,descriptor,targets) VALUES(?1,?2,?3,?4,?5)",params![self.thread.as_bytes(),state.id().as_bytes(),id.as_bytes(),descriptor.as_bytes(),closure.snapshot.targets.map(|id|id.as_bytes().to_vec())])?;
        for (hash, bytes) in &closure.blobs {
            if bytes.starts_with(b"HDTM\x01") {
                tx.execute(
                    "INSERT OR IGNORE INTO reference_map_nodes(hash,body) VALUES(?1,?2)",
                    params![hash.as_bytes(), bytes],
                )?;
            }
        }
        // Only an unambiguous source head gets a default Thread projection.
        let count:i64=tx.query_row("SELECT count(*) FROM operations o WHERE o.thread=?1 AND o.facet=1 AND o.status=1 AND NOT EXISTS(SELECT 1 FROM parents p JOIN operations child ON child.id=p.child WHERE p.parent=o.id AND child.status=1)",[self.thread.as_bytes()],|r|r.get(0))?;
        if count == 1 {
            let expected = crate::reference_projection::root(tx, &scope).map_err(err)?;
            crate::reference_projection::publish_root(
                tx,
                &scope,
                expected,
                closure.snapshot.targets,
            )
            .map_err(err)?;
        } else {
            tx.execute(
                "DELETE FROM reference_roots WHERE spool=?1 AND thread=?2",
                params![scope.spool.as_bytes(), self.thread.as_bytes()],
            )?;
        }
        Ok(())
    }
}

/// A fork inherits the exact base's root, never the parent's latest checkout.
pub(super) fn inherit_root(
    tx: &Transaction<'_>,
    genesis: &objects::object::thread_replication::ThreadGenesis,
) -> Result<()> {
    let Some(parent) = genesis.parent else {
        return Ok(());
    };
    let (owner, _) = descriptor_rows_at(tx, parent, genesis.base)?;
    let roots: Vec<Option<Vec<u8>>> = tx
        .prepare(
            "SELECT DISTINCT targets FROM reference_captures WHERE thread=?1 AND state=?2 LIMIT 2",
        )?
        .query_map(params![owner.as_bytes(), genesis.base.as_bytes()], |row| {
            row.get(0)
        })?
        .collect::<std::result::Result<_, _>>()?;
    if roots.len() != 1 {
        return Ok(());
    };
    let scope = CollaborationScope {
        spool: genesis.spool.parse().map_err(err)?,
        thread: Some(genesis.id()?),
    };
    if crate::reference_projection::root(tx, &scope)
        .map_err(err)?
        .is_none()
    {
        let root = roots[0]
            .as_ref()
            .map(|bytes| super::hash(bytes))
            .transpose()?;
        crate::reference_projection::publish_root(tx, &scope, None, root).map_err(err)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;

fn descriptor_rows_at(
    connection: &rusqlite::Connection,
    mut owner: ContentHash,
    state: StateId,
) -> Result<(ContentHash, Vec<Vec<u8>>)> {
    for _ in 0..128 {
        let rows:Vec<Vec<u8>>=connection.prepare("SELECT DISTINCT descriptor FROM reference_captures WHERE thread=?1 AND state=?2 ORDER BY descriptor LIMIT 129")?.query_map(params![owner.as_bytes(),state.as_bytes()],|r|r.get(0))?.collect::<std::result::Result<_,_>>()?;
        if rows.len() > 128 {
            return Err(err("reference parent frontier budget exceeded"));
        }
        if !rows.is_empty() {
            return Ok((owner, rows));
        }
        let genesis: Option<Vec<u8>> = connection
            .query_row(
                "SELECT genesis FROM threads WHERE id=?1",
                [owner.as_bytes()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(bytes) = genesis else {
            return Ok((owner, rows));
        };
        let genesis = objects::object::thread_replication::ThreadGenesis::decode(&bytes)?;
        if genesis.base != state {
            return Ok((owner, rows));
        }
        let Some(parent) = genesis.parent else {
            return Ok((owner, rows));
        };
        owner = parent;
    }
    Err(err("reference Thread inheritance depth exceeds budget"))
}
