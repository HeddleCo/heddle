//! Materialize shared references without changing the original signed records.
use anyhow::Result;
use api::heddle::api::v2alpha1::Coverage;
use objects::{
    object::{
        AnnotationTag, CollaborationAnchor, CollaborationRevision, CollaborationScope,
        CollaborationSourceAnchor,
    },
    store::ObjectStore,
};
use repo::thread_replication::ThreadReplica;

use super::auth::Session;

pub(super) fn project(
    session: &Session,
    replica: &ThreadReplica,
    scope: &CollaborationScope,
    anchor: &mut CollaborationAnchor,
    tags: &mut [AnnotationTag],
) -> Result<(Coverage, CollaborationScope)> {
    project_for(
        &session.spool,
        uuid::Uuid::parse_str(&session.principal)?,
        session.agent_id.as_deref(),
        replica,
        scope,
        anchor,
        tags,
    )
}

pub(super) fn project_for(
    spool: &repo::device_catalog::DeviceSpool,
    principal: uuid::Uuid,
    agent: Option<&str>,
    replica: &ThreadReplica,
    scope: &CollaborationScope,
    anchor: &mut CollaborationAnchor,
    tags: &mut [AnnotationTag],
) -> Result<(Coverage, CollaborationScope)> {
    let repository = repo::Repository::open(&spool.root)?;
    let mut coverage = Coverage::Complete;
    let source_visible =
        |scope: &CollaborationScope, source: &CollaborationSourceAnchor| -> Result<bool> {
            if scope.spool != spool.id {
                return Ok(false);
            }
            let Some(thread) = scope.thread else {
                return Ok(false);
            };
            let target = if thread == replica.thread_id() {
                replica.clone()
            } else {
                ThreadReplica::open(&spool.heddle_dir, thread)?
            };
            let CollaborationRevision::State { state_id } = &source.revision else {
                return Ok(false);
            };
            let Some(redactions) = super::auth::source_content_visibility(
                &repository,
                &target,
                principal,
                agent,
                *state_id,
            )?
            else {
                return Ok(false);
            };
            if source.path.is_empty() {
                return Ok(true);
            }
            let Some(state) = repository.store().get_state(state_id)? else {
                return Ok(false);
            };
            if state.id() != *state_id {
                return Ok(false);
            }
            let mut work = 0;
            Ok(super::content::visible_path_entry(
                repository.store(),
                state.tree,
                &source.path,
                &redactions,
                &mut work,
            )
            .is_ok())
        };
    let mut resolve =
        |scope: &mut CollaborationScope, source: &mut CollaborationSourceAnchor| -> Result<()> {
            if !source_visible(scope, source)? {
                coverage = Coverage::Unavailable;
                return Ok(());
            }
            // Original signed coordinates remain immutable. A target's moving
            // location is a separate SourceTargetResolutionEvent; its absence must
            // not hide an independently visible authored anchor.
            Ok(())
        };
    for tag in tags {
        match tag {
            AnnotationTag::Source { target }
            | AnnotationTag::Symbol {
                target: Some(target),
                ..
            } => resolve(&mut target.scope, &mut target.source)?,
            _ => {}
        }
    }
    let mut selected = scope.clone();
    if let CollaborationAnchor::Source { source } = anchor {
        resolve(&mut selected, source)?;
    } else if let Some((state_id, path)) = match anchor {
        CollaborationAnchor::State { state_id } => Some((*state_id, String::new())),
        CollaborationAnchor::Path { state_id, path }
        | CollaborationAnchor::Symbol { state_id, path, .. } => Some((*state_id, path.clone())),
        _ => None,
    } {
        let source = CollaborationSourceAnchor {
            revision: CollaborationRevision::State { state_id },
            path,
            symbol_id: String::new(),
            start_line: None,
            end_line: None,
            target: None,
        };
        if !source_visible(&selected, &source)? {
            coverage = Coverage::Unavailable;
        }
    } else if matches!(anchor, CollaborationAnchor::Change { .. }) {
        coverage = Coverage::Unavailable;
    }
    Ok((coverage, selected))
}

/// Resolve one shared target for an already admitted referrer. A reference
/// chooses a scope but never grants source read access in that scope.
pub(super) fn resolution_for(
    repository: &repo::Repository,
    session: &Session,
    owner: &ThreadReplica,
    reference: &objects::object::source_target::SourceTargetReference,
    viewed_revision: Option<objects::object::StateId>,
) -> Result<api::heddle::api::v2alpha1::SourceTargetResolution> {
    use api::heddle::api::{v1alpha1::StateId as WireStateId, v2alpha1 as wire};
    use objects::object::source_target::{
        SourceSelector, SourceTargetBinding, capture::ResolutionStatus,
    };
    let owner_scope = CollaborationScope {
        spool: session.spool.id,
        thread: Some(owner.thread_id()),
    };
    let scope = reference.binding.scope(&owner_scope)?;
    let viewed_thread = matches!(reference.binding, SourceTargetBinding::ViewedThread)
        .then(|| wire_thread(session.spool.id, owner.thread_id()));
    let key = wire::SourceTargetResolutionKey {
        reference: Some(thread_api::collaboration::source_target_ref(reference)),
        viewed_thread,
    };
    let unavailable = || wire::SourceTargetResolution {
        key: Some(key.clone()),
        status: wire::source_target_resolution::Status::Unavailable as i32,
        ..Default::default()
    };
    if scope.spool != session.spool.id {
        return Ok(unavailable());
    }
    let target_id = scope.thread.unwrap_or(owner.thread_id());
    let target = if target_id == owner.thread_id() {
        owner.clone()
    } else {
        ThreadReplica::open(&session.spool.heddle_dir, target_id)?
    };
    let selected = match &reference.binding {
        SourceTargetBinding::ViewedThread => match viewed_revision {
            Some(revision) => Some(revision),
            None => unique_current_source(&target)?,
        },
        SourceTargetBinding::NamedThread { .. } => unique_current_source(&target)?,
        SourceTargetBinding::PinnedRevision { revision, .. } => match revision {
            CollaborationRevision::State { state_id } => Some(*state_id),
            CollaborationRevision::GitCommit { oid } => {
                repository.git_overlay_mapped_state_for_git_commit(oid)?
            }
        },
    };
    let Some(selected) = selected else {
        return Ok(unavailable());
    };
    let principal = uuid::Uuid::parse_str(&session.principal)?;
    let Some(redactions) = super::auth::source_content_visibility(
        repository,
        &target,
        principal,
        session.agent_id.as_deref(),
        selected,
    )?
    else {
        return Ok(unavailable());
    };
    let computed_for = Some(wire::RevisionRef {
        spool: Some(wire::SpoolRef {
            id: session.spool.id.to_string(),
        }),
        revision: Some(wire::revision_ref::Revision::State(WireStateId {
            value: selected.as_bytes().to_vec(),
        })),
    });
    let resolved = match owner.resolve_source_target(repository, reference, selected) {
        Ok(Some(value)) => value,
        Ok(None) | Err(repo::thread_replication::Error::ReferenceProjectionPending) => {
            return Ok(unavailable());
        }
        Err(repo::thread_replication::Error::ReferenceProjectionAmbiguous) => {
            return Ok(wire::SourceTargetResolution {
                key: Some(key.clone()),
                computed_for,
                status: wire::source_target_resolution::Status::Ambiguous as i32,
                ..Default::default()
            });
        }
        Err(error) => return Err(error.into()),
    };
    let status = match (&resolved.file.status, &resolved.target.status) {
        (ResolutionStatus::Ambiguous, _) | (_, ResolutionStatus::Ambiguous) => {
            wire::source_target_resolution::Status::Ambiguous
        }
        (ResolutionStatus::Deleted, _) | (_, ResolutionStatus::Deleted) => {
            wire::source_target_resolution::Status::Deleted
        }
        _ => wire::source_target_resolution::Status::Resolved,
    };
    let mut result = wire::SourceTargetResolution {
        key: Some(key.clone()),
        computed_for,
        status: status as i32,
        ..Default::default()
    };
    if status != wire::source_target_resolution::Status::Resolved {
        return Ok(result);
    }
    let Some(state) = repository.store().get_state(&selected)? else {
        return Ok(unavailable());
    };
    if state.id() != selected {
        return Ok(unavailable());
    }
    let mut work = 0;
    if super::content::visible_path_entry(
        repository.store(),
        state.tree,
        &resolved.file.path,
        &redactions,
        &mut work,
    )
    .is_err()
    {
        return Ok(unavailable());
    }
    let (symbol_id, start_line, end_line) = match resolved.target.selector {
        SourceSelector::File => (String::new(), None, None),
        SourceSelector::Symbol { address } => (address, None, None),
        SourceSelector::Lines { range } => (String::new(), Some(range.start + 1), Some(range.end)),
    };
    result.location = Some(wire::SourceLocation {
        revision: result.computed_for.clone(),
        path: resolved.file.path,
        symbol_id,
        start_line,
        end_line,
        thread: Some(wire_thread(session.spool.id, target_id)),
    });
    Ok(result)
}

fn unique_current_source(replica: &ThreadReplica) -> Result<Option<objects::object::StateId>> {
    let projection = replica.projection()?;
    Ok(match projection.source_heads.as_slice() {
        [] => Some(projection.genesis.base),
        [state] => Some(*state),
        _ => None,
    })
}
fn wire_thread(
    spool: uuid::Uuid,
    thread: objects::object::ContentHash,
) -> api::heddle::api::v2alpha1::ThreadRef {
    use api::heddle::api::v2alpha1 as wire;
    wire::ThreadRef {
        spool: Some(wire::SpoolRef {
            id: spool.to_string(),
        }),
        id: Some(wire::ThreadId {
            value: thread.as_bytes().to_vec(),
        }),
    }
}
