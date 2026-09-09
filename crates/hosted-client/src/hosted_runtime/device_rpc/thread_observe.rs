//! Exact private Thread composition; every requested section reports coverage.
use anyhow::{Context, Result, bail, ensure};
use api::heddle::api::v2alpha1::*;
use objects::object::{
    ContentHash,
    thread_replication::{
        ThreadFacet, ThreadOperationBody,
        metadata::{Property, ThreadControl},
    },
};
use prost::Message;
use repo::thread_replication::ThreadReplica;

use super::{
    DeviceRpc,
    auth::Session,
    checkout,
    thread::{revision, signed_record},
};

impl DeviceRpc {
    pub(super) async fn observe_thread(
        &self,
        session: &Session,
        body: &[u8],
        send: iroh::endpoint::SendStream,
    ) -> Result<()> {
        let request = ObserveThreadRequest::decode(body)?;
        let thread = checkout::thread(session, request.thread.as_ref())?;
        let replica = ThreadReplica::open(&session.spool.heddle_dir, thread)?;
        let mut query = request.clone();
        query.observe = None;
        if let Some(pages) = query.pages.as_mut() {
            for page in [
                &mut pages.captures,
                &mut pages.reviews,
                &mut pages.collaboration,
                &mut pages.timeline,
            ]
            .into_iter()
            .flatten()
            {
                page.after_page.clear();
            }
        }
        self.observe_view(
            session,
            "/heddle.api.v2alpha1.ThreadService/ObserveThread",
            &query.encode_to_vec(),
            request.observe.clone().unwrap_or_default(),
            send,
            |budget, binding| self.thread_snapshot(session, &replica, &request, budget, binding),
            || {
                Ok(repo::thread_replication::projection::version(
                    replica.thread_id(),
                    replica.generation()?,
                )
                .as_bytes()
                .to_vec())
            },
        )
        .await
    }
    fn thread_snapshot(
        &self,
        session: &Session,
        replica: &ThreadReplica,
        request: &ObserveThreadRequest,
        budget: &ReadBudget,
        binding: &[u8],
    ) -> Result<(Vec<(String, ThreadEvent)>, PageInfo, Vec<u8>)> {
        let mut overview = self.thread_overview(session, replica)?;
        let generation = overview.version.clone();
        let reference = overview.r#ref.clone().context("Thread scope")?;
        let mut events = Vec::new();
        let mut all_exhausted = true;
        let sections: std::collections::BTreeSet<_> = if request.sections.is_empty() {
            [ThreadSection::Overview as i32].into_iter().collect()
        } else {
            request.sections.iter().copied().collect()
        };
        for section in sections {
            let section = ThreadSection::try_from(section).context("unknown Thread section")?;
            let (name, coverage, page) = match section {
                ThreadSection::Overview => continue,
                ThreadSection::Captures => {
                    let page = request
                        .pages
                        .as_ref()
                        .and_then(|p| p.captures.as_ref())
                        .cloned()
                        .unwrap_or_default();
                    let after = decode_cursor(&page.after_page, binding, b"captures")?
                        .map(|bytes| ContentHash::from_bytes(bytes));
                    let size = page_size(&page, budget);
                    let mut records =
                        replica.accepted_page(ThreadFacet::Source, after, size + 1)?;
                    let exhausted = records.len() <= size;
                    records.truncate(size);
                    let next = if exhausted {
                        Vec::new()
                    } else {
                        encode_cursor(
                            records.last().context("capture page")?.0.as_bytes(),
                            binding,
                            b"captures",
                        )
                    };
                    for (id, signed) in records {
                        let operation = signed.verify()?;
                        let state = operation.source_state()?.context("source operation")?;
                        let payload = CaptureSummary {
                            revision: Some(revision(&reference, state.id())),
                            thread: Some(reference.clone()),
                            summary: state.intent.unwrap_or_default(),
                            captured_at: Some(prost_types::Timestamp {
                                seconds: state.created_at.timestamp(),
                                nanos: state.created_at.timestamp_subsec_nanos() as i32,
                            }),
                            ..Default::default()
                        };
                        events.push((
                            format!("capture:{id}"),
                            event(thread_event::Payload::Capture(payload)),
                        ));
                        if request.include_operations {
                            events.push((
                                format!("operation:{id}"),
                                event(thread_event::Payload::SignedOperation(signed_record(
                                    &signed,
                                )?)),
                            ));
                        }
                    }
                    (
                        "captures",
                        Coverage::Complete,
                        PageInfo {
                            next_page: next,
                            exhausted,
                            matching_count: overview.capture_count,
                        },
                    )
                }
                ThreadSection::Sharing => {
                    let candidates = replica.metadata_frontier(&Property::Sharing)?;
                    let frontier = overview
                        .metadata_frontiers
                        .iter()
                        .find(|p| p.property == ThreadProperty::Sharing as i32)
                        .context("sharing frontier")?;
                    if candidates.len() <= 1 {
                        let mut sharing = ThreadSharingPolicy {
                            thread: Some(reference.clone()),
                            version: frontier.version.clone(),
                            ..Default::default()
                        };
                        if let Some((_, signed)) = candidates.first() {
                            let operation = signed.verify()?;
                            let ThreadOperationBody::Metadata(bytes) = operation.body else {
                                bail!("sharing facet")
                            };
                            let control = ThreadControl::decode(&bytes)?;
                            let prepared = thread_api::thread_control::PreparedControl {
                                record: signed_record(signed)?,
                                control,
                                thread: reference.clone(),
                                property_version: frontier.version.clone(),
                                thread_version: Vec::new(),
                            };
                            sharing = prepared.set_sharing()?.policy.context("sharing policy")?;
                            sharing.version = frontier.version.clone();
                        }
                        events.push((
                            "sharing".into(),
                            event(thread_event::Payload::Sharing(sharing)),
                        ));
                    }
                    for (id, signed) in candidates {
                        if request.include_operations {
                            events.push((
                                format!("operation:{id}"),
                                event(thread_event::Payload::SignedOperation(signed_record(
                                    &signed,
                                )?)),
                            ));
                        }
                    }
                    (
                        "sharing",
                        if overview.metadata_conflicts.iter().any(|c| {
                            c.frontier
                                .as_ref()
                                .is_some_and(|f| f.property == ThreadProperty::Sharing as i32)
                        }) {
                            Coverage::Partial
                        } else {
                            Coverage::Complete
                        },
                        PageInfo {
                            exhausted: true,
                            ..Default::default()
                        },
                    )
                }
                ThreadSection::Review => {
                    let source = compared_revision(session, replica, request.source.as_ref())?;
                    let genesis = replica.genesis()?;
                    let target = if let Some(parent) = genesis.parent {
                        let parent = ThreadReplica::open(&session.spool.heddle_dir, parent)?;
                        compared_revision(session, &parent, request.base.as_ref())?
                    } else if let Some(base) = request.base.as_ref() {
                        let state = checkout::revision(session, Some(base))?;
                        ensure!(
                            state == genesis.base,
                            "comparison base is outside the root Thread lineage"
                        );
                        Some(base.clone())
                    } else {
                        Some(revision(&reference, genesis.base))
                    };
                    if let (Some(source), Some(base)) = (source, target) {
                        events.push((
                            "comparison".into(),
                            event(thread_event::Payload::Comparison(ReviewComparison {
                                source: Some(source),
                                base: Some(base),
                                policy_version: overview.review_policy_version.clone(),
                            })),
                        ));
                    } else {
                        overview.requirements.push(Requirement{kind:RequirementKind::ConflictResolution as i32,subject:Some(EntityRef{entity:Some(entity_ref::Entity::Thread(reference.clone()))}),explanation:"Select exact source and target revisions from the concurrent heads to review".into(),..Default::default()});
                    }
                    // Review UUIDs are independently causal. Decisions stay bound
                    // to their original comparison, never rewritten to fresh heads.
                    let page = request
                        .pages
                        .as_ref()
                        .and_then(|p| p.reviews.as_ref())
                        .cloned()
                        .unwrap_or_default();
                    let after =
                        decode_cursor(&page.after_page, binding, b"reviews")?.map(|bytes| {
                            let mut id = [0; 16];
                            id.copy_from_slice(&bytes[..16]);
                            format!("review:{}", uuid::Uuid::from_bytes(id))
                        });
                    let size = page_size(&page, budget);
                    let properties = replica
                        .metadata_property_page(after.as_deref().or(Some("review:")), size + 1)?;
                    let properties = properties
                        .into_iter()
                        .take_while(|property| matches!(property, Property::Review(_)))
                        .collect::<Vec<_>>();
                    let exhausted = properties.len() <= size;
                    let mut next = Vec::new();
                    for property in properties.into_iter().take(size) {
                        let Property::Review(id) = property else {
                            bail!("review index property")
                        };
                        let candidates = replica.metadata_frontier(&property)?;
                        let ids = candidates.iter().map(|(id, _)| *id).collect();
                        let version =
                            objects::object::thread_replication::metadata::property_version(
                                replica.thread_id(),
                                &property,
                                &ids,
                            )?
                            .as_bytes()
                            .to_vec();
                        let frontier = ThreadPropertyFrontier {
                            property: ThreadProperty::Review as i32,
                            record_id: id.to_string(),
                            version: version.clone(),
                            operation_ids: candidates
                                .iter()
                                .map(|(id, _)| id.as_bytes().to_vec())
                                .collect(),
                        };
                        overview.metadata_frontiers.push(frontier.clone());
                        if candidates.len() > 1 {
                            overview.metadata_conflicts.push(ThreadMetadataConflict {
                                frontier: Some(frontier),
                                candidates: candidates
                                    .iter()
                                    .map(|(_, s)| signed_record(s))
                                    .collect::<Result<_>>()?,
                            });
                        }
                        for (operation_id, signed) in candidates {
                            let operation = signed.verify()?;
                            let ThreadOperationBody::Metadata(bytes) = operation.body else {
                                bail!("review facet")
                            };
                            let control = ThreadControl::decode(&bytes)?;
                            let prepared = thread_api::thread_control::PreparedControl {
                                record: signed_record(&signed)?,
                                control,
                                thread: reference.clone(),
                                property_version: version.clone(),
                                thread_version: Vec::new(),
                            };
                            // Conflicting review values remain original candidates
                            // in overview; never emit them as one chosen decision.
                            if frontier_candidate_count(&overview, id) == 1 {
                                events.push((
                                    format!("review:{id}"),
                                    event(thread_event::Payload::Review(
                                        prepared.record_review()?.decision.context("review")?,
                                    )),
                                ));
                            }
                            if request.include_operations {
                                events.push((
                                    format!("operation:{operation_id}"),
                                    event(thread_event::Payload::SignedOperation(signed_record(
                                        &signed,
                                    )?)),
                                ));
                            }
                        }
                        if !exhausted {
                            let mut bytes = [0; 32];
                            bytes[..16].copy_from_slice(id.as_bytes());
                            next = encode_cursor(&bytes, binding, b"reviews");
                        }
                    }
                    (
                        "review",
                        Coverage::Partial,
                        PageInfo {
                            exhausted,
                            next_page: next,
                            ..Default::default()
                        },
                    )
                }
                ThreadSection::Collaboration => {
                    ("collaboration", Coverage::Unavailable, PageInfo::default())
                }
                ThreadSection::Analysis => ("analysis", Coverage::Unavailable, PageInfo::default()),
                ThreadSection::Checkouts => {
                    ("checkouts", Coverage::Unavailable, PageInfo::default())
                }
                ThreadSection::Timeline => ("timeline", Coverage::Unavailable, PageInfo::default()),
                ThreadSection::Unspecified => bail!("Thread section required"),
            };
            all_exhausted &= page.exhausted || coverage == Coverage::Unavailable;
            let status = SectionStatus {
                section: name.into(),
                coverage: coverage as i32,
                page: Some(page),
                ..Default::default()
            };
            overview.sections.push(status.clone());
            events.push((
                format!("status:{name}"),
                event(thread_event::Payload::Status(status)),
            ));
        }
        if repo::thread_replication::projection::version(replica.thread_id(), replica.generation()?)
            .as_bytes()
            .as_slice()
            != generation
        {
            return Err(super::stream::SnapshotChanged.into());
        }
        events.insert(
            0,
            (
                "overview".into(),
                event(thread_event::Payload::Overview(overview)),
            ),
        );
        Ok((
            events,
            PageInfo {
                exhausted: all_exhausted,
                ..Default::default()
            },
            generation,
        ))
    }
}
fn compared_revision(
    session: &Session,
    replica: &ThreadReplica,
    requested: Option<&RevisionRef>,
) -> Result<Option<RevisionRef>> {
    let view = replica.projection()?;
    let reference = ThreadRef {
        spool: Some(SpoolRef {
            id: session.spool.id.to_string(),
        }),
        id: Some(ThreadId {
            value: replica.thread_id().as_bytes().to_vec(),
        }),
    };
    if let Some(requested) = requested {
        let state = checkout::revision(session, Some(requested))?;
        ensure!(
            state == view.genesis.base || replica.accepted_source_revision(state)?.is_some(),
            "comparison revision is not admitted in the selected Thread"
        );
        return Ok(Some(revision(&reference, state)));
    }
    match view.source_heads.as_slice() {
        [] => Ok(Some(revision(&reference, view.genesis.base))),
        [state] => Ok(Some(revision(&reference, *state))),
        _ => Ok(None),
    }
}
fn frontier_candidate_count(view: &ThreadOverview, id: uuid::Uuid) -> usize {
    view.metadata_frontiers
        .iter()
        .find(|f| f.record_id == id.to_string())
        .map(|f| f.operation_ids.len())
        .unwrap_or_default()
}
fn event(payload: thread_event::Payload) -> ThreadEvent {
    ThreadEvent {
        frame: None,
        payload: Some(payload),
    }
}
fn page_size(page: &PageRequest, budget: &ReadBudget) -> usize {
    let requested = if page.size == 0 { 16 } else { page.size };
    requested
        .min(budget.max_items.saturating_sub(3).max(1))
        .min(128) as usize
}
fn encode_cursor(id: &[u8; 32], binding: &[u8], section: &[u8]) -> Vec<u8> {
    let digest = blake3::hash(&[binding, section].concat());
    [digest.as_bytes().as_slice(), id.as_slice()].concat()
}
fn decode_cursor(cursor: &[u8], binding: &[u8], section: &[u8]) -> Result<Option<[u8; 32]>> {
    if cursor.is_empty() {
        return Ok(None);
    }
    ensure!(
        cursor.len() == 64
            && cursor[..32] == *blake3::hash(&[binding, section].concat()).as_bytes(),
        "page cursor belongs to another query"
    );
    Ok(Some(cursor[32..].try_into()?))
}
