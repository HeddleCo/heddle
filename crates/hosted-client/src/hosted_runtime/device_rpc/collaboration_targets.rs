//! Materialize shared references without changing the original signed records.
use anyhow::{Result, ensure};
use api::heddle::api::v2alpha1::Coverage;
use objects::object::{
    AnnotationTag, CollaborationAnchor, CollaborationRevision, CollaborationScope,
    CollaborationSourceAnchor,
    source_target::{SourceSelector, capture::ResolutionStatus},
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
    let repository = repo::Repository::open(&session.spool.root)?;
    let projection = replica.projection()?;
    let viewed = match projection.source_heads.as_slice() {
        [] => projection.genesis.base,
        [state] => *state,
        _ => return Ok((Coverage::Partial, scope.clone())),
    };
    let mut coverage = Coverage::Complete;
    let mut resolve =
        |scope: &mut CollaborationScope, source: &mut CollaborationSourceAnchor| -> Result<()> {
            let Some(reference) = &source.target else {
                return Ok(());
            };
            let selected = reference.binding.scope(scope)?;
            if selected.spool != session.spool.id {
                coverage = Coverage::Unavailable;
                return Ok(());
            }
            // Reference indirection cannot authorize another resource. This endpoint
            // has admitted only this exact locally owned Spool for the request.
            ensure!(
                scope.spool == session.spool.id,
                "source anchor belongs to another Spool"
            );
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
                }
                Some(_) => coverage = Coverage::Partial,
                None => coverage = Coverage::Unavailable,
            }
            Ok(())
        };
    let mut selected = scope.clone();
    if let CollaborationAnchor::Source { source } = anchor {
        resolve(&mut selected, source)?;
    }
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
    Ok((coverage, selected))
}
