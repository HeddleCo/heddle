//! Materialize shared references without changing the original signed records.
use anyhow::{Result, ensure};
use api::heddle::api::v2alpha1::Coverage;
use objects::{
    object::{
        AnnotationTag, CollaborationAnchor, CollaborationRevision, CollaborationScope,
        CollaborationSourceAnchor,
        source_target::{SourceSelector, capture::ResolutionStatus},
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
    let projection = replica.projection()?;
    let viewed = match projection.source_heads.as_slice() {
        [] => Some(projection.genesis.base),
        [state] => Some(*state),
        _ => None,
    };
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
    let mut resolve = |scope: &mut CollaborationScope,
                       source: &mut CollaborationSourceAnchor|
     -> Result<()> {
        if !source_visible(scope, source)? {
            coverage = Coverage::Unavailable;
            return Ok(());
        }
        let Some(reference) = &source.target else {
            return Ok(());
        };
        let Some(viewed) = viewed else {
            coverage = Coverage::Unavailable;
            return Ok(());
        };
        let selected = reference.binding.scope(scope)?;
        if selected.spool != spool.id {
            coverage = Coverage::Unavailable;
            return Ok(());
        }
        if let Some(thread) = selected.thread {
            let target_replica = ThreadReplica::open(&spool.heddle_dir, thread)?;
            if !super::auth::thread_visible(&repository, &target_replica, principal, agent)? {
                coverage = Coverage::Unavailable;
                return Ok(());
            }
        }
        // Reference indirection cannot authorize another resource. This endpoint
        // has admitted only this exact locally owned Spool for the request.
        ensure!(
            scope.spool == spool.id,
            "source anchor belongs to another Spool"
        );
        if !super::auth::source_revision_visible(&repository, replica, principal, agent, viewed)? {
            coverage = Coverage::Unavailable;
            return Ok(());
        }
        match replica.resolve_source_target(&repository, reference, viewed)? {
            Some(value)
                if value.file.status == ResolutionStatus::Resolved
                    && value.target.status == ResolutionStatus::Resolved =>
            {
                *scope = value.scope;
                source.revision = CollaborationRevision::State {
                    state_id: value.state,
                };
                source.path = value.file.path;
                source.symbol_id = String::new();
                source.start_line = None;
                source.end_line = None;
                match value.target.selector {
                    SourceSelector::File => {}
                    SourceSelector::Symbol { address } => source.symbol_id = address,
                    SourceSelector::Lines { range } => {
                        source.start_line = Some(range.start + 1);
                        source.end_line = Some(range.end);
                    }
                }
                if !source_visible(scope, source)? {
                    coverage = Coverage::Unavailable;
                }
            }
            Some(_) if coverage == Coverage::Complete => coverage = Coverage::Partial,
            Some(_) => {}
            None => coverage = Coverage::Unavailable,
        }
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
