//! Finite indexed local search; result frames never retain a SQLite transaction.
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use api::heddle::api::{v1alpha1::CallFailureCode, v2alpha1::*};
use iroh::endpoint::SendStream;
use objects::store::ObjectStore;
use prost::Message;

/// An abandoned stream must stop its blocking search worker and release the
/// content-work permit. `spawn_blocking` cannot be aborted once it starts, so
/// each bounded preparation batch checks this latch before more work.
struct CancelSearchOnDrop(Arc<AtomicBool>);

impl Drop for CancelSearchOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

struct SearchSelection {
    spools: BTreeSet<uuid::Uuid>,
    threads: BTreeMap<uuid::Uuid, BTreeSet<Vec<u8>>>,
    kinds: BTreeSet<i32>,
    annotations: Option<objects::object::AnnotationQuery>,
    source_scope: LocalSourceScope,
}

#[derive(Clone)]
enum LocalSourceScope {
    Current,
    Retained,
    Exact(RevisionRef),
}

impl SearchSelection {
    fn parse(request: &SearchRequest) -> Result<Self> {
        ensure!(
            request.spools.len() <= 32 && request.threads.len() <= 64 && request.domains.len() <= 6,
            "search selector bound exceeded"
        );
        let mut spools = BTreeSet::new();
        for spool in &request.spools {
            ensure!(
                spools.insert(uuid::Uuid::parse_str(&spool.id)?),
                "duplicate search Spool"
            );
        }
        let mut threads = BTreeMap::<uuid::Uuid, BTreeSet<Vec<u8>>>::new();
        for thread in &request.threads {
            let spool = thread
                .spool
                .as_ref()
                .context("search Thread requires Spool")?;
            let spool = uuid::Uuid::parse_str(&spool.id)?;
            let id = thread
                .id
                .as_ref()
                .context("search Thread requires identity")?;
            ensure!(
                id.value.len() == 32,
                "search Thread identity must be 32 bytes"
            );
            ensure!(
                threads.entry(spool).or_default().insert(id.value.clone()),
                "duplicate search Thread"
            );
        }
        if spools.is_empty() {
            spools.extend(threads.keys().copied());
        } else if !threads.is_empty() {
            spools.retain(|spool| threads.contains_key(spool));
        }
        let annotations = request
            .annotations
            .as_ref()
            .map(thread_api::collaboration::annotation_query)
            .transpose()?;
        let mut kinds = BTreeSet::new();
        if request.domains.is_empty() {
            if annotations.is_some() {
                kinds.insert(SearchDomain::Context as i32);
            } else {
                kinds.extend([
                    SearchDomain::Thread as i32,
                    SearchDomain::Discussion as i32,
                    SearchDomain::Context as i32,
                ]);
            }
        } else {
            for domain in &request.domains {
                let kind = SearchDomain::try_from(*domain).context("invalid search domain")?;
                ensure!(
                    matches!(
                        kind,
                        SearchDomain::Thread
                            | SearchDomain::Discussion
                            | SearchDomain::Context
                            | SearchDomain::SourceContent
                            | SearchDomain::SourceSymbol
                            | SearchDomain::Revision
                    ),
                    "unsupported search domain"
                );
                ensure!(kinds.insert(kind as i32), "duplicate search domain");
            }
        }
        ensure!(
            annotations.is_none() || kinds == BTreeSet::from([SearchDomain::Context as i32]),
            "annotation filters select context revisions only"
        );
        ensure!(
            !request.text.trim().is_empty() || annotations.is_some(),
            "search requires text or annotation filters"
        );
        ensure!(request.text.len() <= 4096, "search text exceeds bound");
        let source_requested = kinds.contains(&(SearchDomain::SourceContent as i32))
            || kinds.contains(&(SearchDomain::SourceSymbol as i32));
        ensure!(
            request.source_scope.is_none() || source_requested,
            "source scope requires source content or symbol domain"
        );
        let source_scope = match request.source_scope.as_ref() {
            None | Some(search_request::SourceScope::SourceHistory(0 | 1)) => {
                LocalSourceScope::Current
            }
            Some(search_request::SourceScope::SourceHistory(2)) => LocalSourceScope::Retained,
            Some(search_request::SourceScope::SourceHistory(_)) => {
                anyhow::bail!("unknown source history scope")
            }
            Some(search_request::SourceScope::SourceRevision(revision)) => {
                let spool = revision
                    .spool
                    .as_ref()
                    .context("source revision Spool required")?;
                let id = uuid::Uuid::parse_str(&spool.id)?;
                ensure!(
                    !threads.is_empty()
                        && threads.keys().all(|selected| *selected == id)
                        && spools.len() == 1
                        && spools.contains(&id),
                    "exact source revision requires selected Threads in its Spool"
                );
                LocalSourceScope::Exact(revision.clone())
            }
        };
        Ok(Self {
            spools,
            threads,
            kinds,
            annotations,
            source_scope,
        })
    }
}

use super::{DeviceRpc, account_auth::AccountSession, failure, stream::ObservationAuthority};

impl DeviceRpc {
    pub(super) async fn search_local(
        &self,
        session: AccountSession,
        body: &[u8],
        mut send: SendStream,
    ) -> Result<()> {
        let session = Arc::new(session);
        let result = tokio::time::timeout(Duration::from_secs(30), async {
            let request = SearchRequest::decode(body)?;
            let mode = search_request::Mode::try_from(request.mode).context("unknown search mode")?;
            ensure!(matches!(mode, search_request::Mode::Unspecified | search_request::Mode::Lexical), "device supports lexical Search only");
            let selection = SearchSelection::parse(&request)?;
            let mut spools = selection.spools.clone();
            if spools.is_empty() {
                if let Some(catalog) = repo::device_catalog::store::Catalog::read(&self.home)? {
                    for registered in catalog.registrations()? {
                        if session.facts(Some(&registered.capability_path)).is_ok() {
                            ensure!(spools.len() < 64, "authorized local Search discovery exceeds 64 Spools; select explicit Spools");
                            spools.insert(registered.id);
                        }
                    }
                }
            }
            ensure!(spools.len() <= 64, "search Spool selection exceeds bound");
            let requested = request.budget.unwrap_or_default();
            let frame = if requested.max_frame_bytes == 0 {
                65536
            } else {
                requested.max_frame_bytes
            };
            let bytes = if requested.max_snapshot_bytes == 0 {
                1024 * 1024
            } else {
                requested.max_snapshot_bytes
            };
            ensure!(
                (1024..=256 * 1024).contains(&frame) && bytes <= 8 * 1024 * 1024,
                "search byte budget outside device bounds"
            );
            let page = request.page.clone().unwrap_or_default();
            let limit = if page.size == 0 { 64 } else { page.size };
            ensure!(
                limit <= 256 && (requested.max_items == 0 || limit < requested.max_items),
                "search page exceeds item budget"
            );
            let mut selected = Vec::new();
            for id in spools {
                let spool = repo::device_catalog::load(&self.home, id)?;
                session.facts(Some(&spool.capability_path))?;
                selected.push(spool);
            }
            let permit = self
                .content_work
                .clone()
                .try_acquire_owned()
                .context("device query capacity exhausted")?;
            let this = self.clone();
            let worker_session = session.clone();
            let cancellation = Arc::new(AtomicBool::new(false));
            let _cancel_on_drop = CancelSearchOnDrop(cancellation.clone());
            let worker = tokio::task::spawn_blocking(move || -> Result<Vec<SearchEvent>> {
                let _permit = permit;
                worker_session.check_current(&this.home)?;
                let mut normalized = request.clone();
                normalized.page = None;
                normalized.budget = None;
                let mut digest = blake3::Hasher::new_derive_key("heddle-device-search-v2");
                digest.update(&normalized.encode_to_vec());
                digest.update(&worker_session.binding());
                digest.update(&this.endpoint);
                let mut generations = Vec::with_capacity(selected.len());
                for spool in &selected {
                    let generation =
                        repo::thread_replication::collaboration::generation(&spool.heddle_dir)?;
                    digest.update(&generation);
                    let metadata = repo::operation_dedup::observation::generation(&spool.heddle_dir)?;
                    digest.update(&metadata);
                    generations.push((generation, metadata));
                }
                let mut source_filters = Vec::with_capacity(selected.len());
                for spool in &selected {
                    let filter = match &selection.source_scope {
                        LocalSourceScope::Current => Some(repo::thread_replication::collaboration_search::SourceSelection::Current),
                        LocalSourceScope::Retained => Some(repo::thread_replication::collaboration_search::SourceSelection::Retained),
                        LocalSourceScope::Exact(revision) => {
                            match revision.revision.as_ref().context("source revision identity required")? {
                                revision_ref::Revision::State(state) => {
                                    ensure!(state.value.len()==32,"source State identity must be 32 bytes");
                                    let mut bytes=[0;32]; bytes.copy_from_slice(&state.value);
                                    Some(repo::thread_replication::collaboration_search::SourceSelection::Exact(objects::object::StateId::from_bytes(bytes)))
                                }
                                revision_ref::Revision::GitCommitOid(oid) => {
                                    ensure!((oid.len()==40 || oid.len()==64) && oid.bytes().all(|byte|byte.is_ascii_hexdigit()),"invalid Git source revision selector");
                                    let repository=repo::Repository::open(&spool.root)?;
                                    repository.git_overlay_mapped_state_for_git_commit(&oid.to_ascii_lowercase())?
                                        .map(repo::thread_replication::collaboration_search::SourceSelection::Exact)
                                }
                            }
                        }
                    };
                    match filter {
                        Some(repo::thread_replication::collaboration_search::SourceSelection::Current) => digest.update(b"current"),
                        Some(repo::thread_replication::collaboration_search::SourceSelection::Retained) => digest.update(b"retained"),
                        Some(repo::thread_replication::collaboration_search::SourceSelection::Exact(state)) => digest.update(state.as_bytes()),
                        None => digest.update(b"unmapped"),
                    };
                    source_filters.push(filter);
                }
                let binding = digest.finalize();
                let cursor_directory = selected.first().map(|spool| &spool.heddle_dir);
                let mut cursor_scope = blake3::Hasher::new_derive_key("heddle-device-search-cursor-actor-v1");
                cursor_scope.update(&worker_session.binding());
                cursor_scope.update(&this.endpoint);
                if let Some(spool) = selected.first() {
                    let facts = worker_session.facts(Some(&spool.capability_path))?;
                    cursor_scope.update(facts.delegation_agent_id.as_deref().unwrap_or("").as_bytes());
                }
                let cursor_scope = *cursor_scope.finalize().as_bytes();
                let (mut position, mut after_operation) = if page.after_page.is_empty() {
                    (0usize, None)
                } else {
                    let directory = cursor_directory.context("search cursor has no selected Spool")?;
                    let (ordinal, operation) = repo::device_page_cursors::resume_search(
                        directory, cursor_scope, binding.as_bytes(), &page.after_page,
                    )?;
                    (ordinal, Some(objects::object::ContentHash::from_bytes(operation)))
                };
                ensure!(position < selected.len() || selected.is_empty(), "search cursor outside Spools");
                let mut events = Vec::new();
                let mut examined = 0usize;
                let mut visible = 0usize;
                // The final generation fence discards this buffered page if
                // either accepted sources or visibility changes during the
                // scan. Reuse one verified source projection per exact target
                // within that snapshot, including multiple indexed symbols
                // or content rows from the same original State.
                let mut source_projections = BTreeMap::<
                    (uuid::Uuid, objects::object::ContentHash, objects::object::StateId),
                    Option<objects::object::EntryRedactions>,
                >::new();
                let mut source_paths = BTreeMap::<
                    (uuid::Uuid, objects::object::ContentHash, objects::object::StateId, String),
                    bool,
                >::new();
                let mut admitted_sources = vec![Vec::new(); selected.len()];
                let mut admitted_target_count = 0usize;
                let mut content_ready = true;
                let mut symbols_ready = true;
                if selection.kinds.contains(&(SearchDomain::SourceContent as i32))
                    || selection.kinds.contains(&(SearchDomain::SourceSymbol as i32))
                {
                    let principal = uuid::Uuid::parse_str(&worker_session.principal)?;
                    for (index, spool) in selected.iter().enumerate() {
                        let Some(source) = source_filters[index] else { continue; };
                        let facts = worker_session.facts(Some(&spool.capability_path))?;
                        let repository = repo::Repository::open(&spool.root)?;
                        let selected_threads = selection.threads.get(&spool.id)
                            .map(|threads| threads.iter().map(|id| {
                                let bytes: [u8;32] = id.as_slice().try_into()
                                    .context("selected Search Thread identity must be 32 bytes")?;
                                Ok(objects::object::ContentHash::from_bytes(bytes))
                            }).collect::<Result<Vec<_>>>())
                            .transpose()?;
                        let search_index = repo::thread_replication::source_search::SourceSearchReader::open(
                            &spool.heddle_dir,
                        )?;
                        search_index.visit_indexed_targets(
                            source,
                            selected_threads.as_deref(),
                            |candidate| {
                                ensure!(!cancellation.load(Ordering::Acquire), "search query canceled");
                                worker_session.check_clock()?;
                                if selection.threads.get(&spool.id).is_some_and(|threads| !threads.contains(candidate.thread.as_bytes().as_slice())) {
                                    return Ok(());
                                }
                                let key = (spool.id, candidate.thread, candidate.revision);
                                if !source_projections.contains_key(&key) {
                                    let admission = repo::thread_replication::ThreadReplica::open(
                                        &spool.heddle_dir, candidate.thread,
                                    ).ok().and_then(|replica| {
                                        super::auth::source_content_admission(
                                            &repository, &replica, principal,
                                            facts.delegation_agent_id.as_deref(), candidate.revision,
                                        ).ok()
                                    });
                                    if matches!(admission, Some(super::auth::SourceContentAdmission::Visible(_)
                                        | super::auth::SourceContentAdmission::Unavailable)) {
                                        admitted_target_count += 1;
                                        ensure!(admitted_target_count <= 4096, "authorized source search scope exceeds target budget");
                                    }
                                    match admission {
                                        Some(super::auth::SourceContentAdmission::Visible(proof)) => {
                                            source_projections.insert(key, Some(proof));
                                        }
                                        Some(super::auth::SourceContentAdmission::Unavailable) => {
                                            content_ready = false;
                                            symbols_ready = false;
                                            source_projections.insert(key, None);
                                        }
                                        _ => { source_projections.insert(key, None); }
                                    }
                                }
                                let Some(redactions) = source_projections.get(&key).and_then(Option::as_ref) else { return Ok(()); };
                                let readiness = search_index.readiness_for_authorized_target(
                                    candidate.thread, candidate.revision,
                                )?;
                                content_ready &= readiness.content;
                                symbols_ready &= readiness.symbols;
                                admitted_sources[index].push(repo::thread_replication::collaboration_search::AdmittedSourceTarget {
                                    thread: candidate.thread,
                                    revision: candidate.revision,
                                    denied_leaves: redactions.leaves().iter().copied().collect(),
                                });
                                Ok(())
                            },
                        )?;
                    }
                }
                for domain in &selection.kinds {
                    let ready = match SearchDomain::try_from(*domain)? {
                        SearchDomain::SourceContent => content_ready,
                        SearchDomain::SourceSymbol => symbols_ready,
                        _ => true,
                    };
                    events.push(SearchEvent {
                        source: Some(this.endpoint()),
                        payload: Some(search_event::Payload::DomainStatus(SearchDomainStatus {
                            domain: *domain,
                            coverage: if ready { Coverage::Complete } else { Coverage::Partial } as i32,
                            supported_modes: vec![search_request::Mode::Lexical as i32],
                        })),
                    });
                }
                // Candidate-row work is bounded independently of disclosure
                // preparation. Hidden source targets cannot consume this
                // page deadline and change a visible result into an error.
                let deadline = std::time::Instant::now() + Duration::from_secs(25);
                let mut next_boundary = (position, None);
                let mut has_more = false;
                if matches!(mode, search_request::Mode::Unspecified | search_request::Mode::Lexical) {
                    'scan: while position < selected.len() {
                        ensure!(
                            std::time::Instant::now() < deadline,
                            "device query work deadline exceeded"
                        );
                        worker_session.check_clock()?;
                        let spool = &selected[position];
                        worker_session.facts(Some(&spool.capability_path))?;
                        ensure!(examined < 10_000, "device search candidate work bound exceeded");
                        let remaining = (10_000 - examined).min(256);
                        let kinds: Vec<_> = selection.kinds.iter().copied().map(|kind| kind - 1)
                            .filter(|kind|source_filters[position].is_some() || !matches!(kind,3|4))
                            .collect();
                        if kinds.is_empty() {position+=1;after_operation=None;continue;}
                        let repository = repo::Repository::open(&spool.root)?;
                        let query_text = if selection.kinds.len() == 1
                            && selection.kinds.contains(&(SearchDomain::Revision as i32))
                            && request.text.trim().starts_with("git:")
                        {
                            let oid = request.text.trim().trim_start_matches("git:");
                            ensure!((oid.len()==40 || oid.len()==64) && oid.bytes().all(|byte| byte.is_ascii_hexdigit()), "invalid Git commit selector");
                            let Some(mapped) = repository.git_overlay_mapped_state_for_git_commit(&oid.to_ascii_lowercase())? else {
                                position += 1;
                                after_operation = None;
                                continue;
                            };
                            mapped.to_string_full()
                        } else {
                            request.text.clone()
                        };
                        let batch = repo::thread_replication::collaboration_search::search_native_admitted(
                            &spool.heddle_dir,
                            &query_text,
                            after_operation,
                            remaining as u32,
                            &kinds,
                            selection.annotations.as_ref(),
                            source_filters[position].unwrap_or(repo::thread_replication::collaboration_search::SourceSelection::Current),
                            &admitted_sources[position],
                        )?;
                        let exhausted = batch.scanned < remaining;
                        let facts = worker_session.facts(Some(&spool.capability_path))?;
                        let principal = uuid::Uuid::parse_str(&worker_session.principal)?;
                        for hit in batch.hits {
                            examined += 1;
                            after_operation = Some(hit.cursor);
                            if selection.threads.get(&spool.id).is_some_and(|threads| !threads.contains(hit.thread.as_bytes().as_slice())) { continue; }
                            let replica = repo::thread_replication::ThreadReplica::open(
                                &spool.heddle_dir,
                                hit.thread,
                            )?;
                            if !super::auth::thread_visible(
                                &repository,
                                &replica,
                                principal,
                                facts.delegation_agent_id.as_deref(),
                            )? {
                                continue;
                            }
                            if hit.kind == 1 {
                                let id = hit.record.parse::<objects::object::DiscussionRecordId>()?;
                                if !super::auth::discussion_visible(&repository, &replica, principal, facts.delegation_agent_id.as_deref(), id)? { continue; }
                                let mut anchor = replica.discussion_summary(id, bytes as usize)?.discussion.anchor;
                                let scope = objects::object::CollaborationScope { spool: spool.id, thread: Some(hit.thread) };
                                let (coverage, _) = super::collaboration_targets::project_for(spool, principal, facts.delegation_agent_id.as_deref(), &replica, &scope, &mut anchor, &mut [])?;
                                if coverage == Coverage::Unavailable { continue; }
                                let (signed, _) = replica.operation(&hit.operation)?.context("indexed discussion operation absent")?;
                                let objects::object::thread_replication::ThreadOperationBody::Discussion(original) = &signed.verify()?.body else {
                                    anyhow::bail!("indexed discussion facet mismatch")
                                };
                                let original = objects::object::CollaborationOperationEnvelope::decode(original)?.operation;
                                let mut authored_anchor = match original.body {
                                    objects::object::CollaborationOperationBodyV1::Open { anchor, .. }
                                    | objects::object::CollaborationOperationBodyV1::RebindAnchor { anchor, .. }
                                    | objects::object::CollaborationOperationBodyV1::LegacyImported { anchor, .. } => Some(anchor),
                                    _ => None,
                                };
                                if let Some(anchor) = authored_anchor.as_mut() {
                                    let (coverage, _) = super::collaboration_targets::project_for(spool, principal, facts.delegation_agent_id.as_deref(), &replica, &scope, anchor, &mut [])?;
                                    if coverage == Coverage::Unavailable { continue; }
                                }
                            }
                            if hit.kind == 2 {
                                let (signed, _) = replica.operation(&hit.operation)?.context("indexed context operation absent")?;
                                let mut context = signed.verify()?.context_revision()?.context("indexed context revision absent")?;
                                if let Some(discussion) = context.extracted_from {
                                    if !super::auth::discussion_visible(&repository, &replica, principal, facts.delegation_agent_id.as_deref(), discussion)? { continue; }
                                }
                                if selection.annotations.as_ref().is_some_and(|query| !query.matches(&context.tags)) { continue; }
                                let scope = objects::object::CollaborationScope { spool: spool.id, thread: Some(hit.thread) };
                                let (coverage, _) = super::collaboration_targets::project_for(spool, principal, facts.delegation_agent_id.as_deref(), &replica, &scope, &mut context.anchor, &mut context.tags)?;
                                if coverage == Coverage::Unavailable { continue; }
                            }
                            let revision_hit = if let Some(revision) = hit.revision {
                                let key = (spool.id, hit.thread, revision);
                                if !source_projections.contains_key(&key) {
                                    let proof = super::auth::source_content_visibility(
                                        &repository, &replica, principal,
                                        facts.delegation_agent_id.as_deref(), revision,
                                    )?;
                                    source_projections.insert(key, proof);
                                }
                                let Some(redactions) = source_projections.get(&key).and_then(Option::as_ref) else { continue; };
                                if matches!(hit.kind, 3 | 4) {
                                    let path_key = (spool.id, hit.thread, revision, hit.path.clone());
                                    if !source_paths.contains_key(&path_key) {
                                        let state = repository.store().get_state(&revision)?.context("indexed source state absent")?;
                                        let mut path_work = 0;
                                        let admitted = super::content::visible_path_entry(
                                            repository.store(), state.tree, &hit.path,
                                            redactions,
                                            &mut path_work,
                                        ).is_ok();
                                        source_paths.insert(path_key.clone(), admitted);
                                    }
                                    if !source_paths[&path_key] { continue; }
                                }
                                Some(RevisionRef {
                                    spool: Some(SpoolRef { id: spool.id.to_string() }),
                                    revision: Some(revision_ref::Revision::State(
                                        api::heddle::api::v1alpha1::StateId { value: revision.as_bytes().to_vec() }
                                    )),
                                })
                            } else { None };
                            if visible == limit as usize {
                                has_more = true;
                                break 'scan;
                            }
                            let reference = RecordRef {
                                spool: Some(SpoolRef {
                                    id: spool.id.to_string(),
                                }),
                                id: hit.record,
                            };
                            let entity = if let Some(revision) = revision_hit.as_ref() {
                                entity_ref::Entity::Revision(revision.clone())
                            } else if hit.kind == 0 {
                                entity_ref::Entity::Thread(ThreadRef {
                                    spool: Some(SpoolRef { id: spool.id.to_string() }),
                                    id: Some(ThreadId { value: hit.thread.as_bytes().to_vec() }),
                                })
                            } else if hit.kind == 2 {
                                entity_ref::Entity::Context(reference)
                            } else {
                                entity_ref::Entity::Discussion(reference)
                            };
                            events.push(SearchEvent {
                                source: Some(this.endpoint()),
                                payload: Some(search_event::Payload::Hit(SearchHit {
                                    subject: Some(EntityRef {
                                        entity: Some(entity),
                                    }),
                                    summary: hit.snippet,
                                    score: -hit.score,
                                    domain: i32::from(hit.kind) + 1,
                                    thread: Some(ThreadRef {
                                        spool: Some(SpoolRef { id: spool.id.to_string() }),
                                        id: Some(ThreadId { value: hit.thread.as_bytes().to_vec() }),
                                    }),
                                    causal_id: if hit.kind == 2 { hit.operation.as_bytes().to_vec() } else { Vec::new() },
                                    location: revision_hit.map(|revision| SourceLocation {
                                        revision: Some(revision),
                                        path: hit.path,
                                        symbol_id: hit.symbol_id,
                                        start_line: hit.start_line,
                                        end_line: hit.end_line,
                                        thread: Some(ThreadRef {
                                            spool: Some(SpoolRef { id: spool.id.to_string() }),
                                            id: Some(ThreadId { value: hit.thread.as_bytes().to_vec() }),
                                        }),
                                        ..Default::default()
                                    }),
                                    match_kind: if hit.kind == 5 { SearchMatchKind::HashExact as i32 } else if request.text.trim().is_empty() { SearchMatchKind::Structured as i32 } else { SearchMatchKind::Fulltext as i32 },
                                    symbol_name: (hit.kind == 4).then_some(hit.symbol_name),
                                    ..Default::default()
                                })),
                            });
                            visible += 1;
                            next_boundary = (position, Some(hit.cursor));
                        }
                        if exhausted {
                            position += 1;
                            after_operation = None;
                        }
                    }
                }
                let next_page = if !has_more {
                    vec![]
                } else {
                    let directory = cursor_directory.context("search cursor has no selected Spool")?;
                    let operation = next_boundary.1.context("search continuation has no served operation")?;
                    repo::device_page_cursors::issue(directory, cursor_scope, binding.as_bytes(), &format!("search:{}", next_boundary.0), *operation.as_bytes())?.to_vec()
                };
                events.push(SearchEvent {
                    source: Some(this.endpoint()),
                    payload: Some(search_event::Payload::Complete(SectionStatus {
                        section: "search".into(),
                        coverage: if (selection.kinds.contains(&(SearchDomain::SourceContent as i32)) && !content_ready)
                            || (selection.kinds.contains(&(SearchDomain::SourceSymbol as i32)) && !symbols_ready) {
                            Coverage::Partial
                        } else { Coverage::Complete } as i32,
                        page: Some(PageInfo {
                            exhausted: !has_more,
                            next_page,
                            ..Default::default()
                        }),
                        ..Default::default()
                    })),
                });
                for (spool, (observed, metadata)) in selected.iter().zip(&generations) {
                    ensure!(
                        repo::thread_replication::collaboration::generation(&spool.heddle_dir)?
                            == *observed,
                        "search data changed during query; refresh the first page"
                    );
                    ensure!(repo::operation_dedup::observation::generation(&spool.heddle_dir)? == *metadata,
                        "search index changed during query; refresh the first page");
                }
                Ok(events)
            });
            let events = worker.await??;
            let mut sent = 0u64;
            for event in events {
                session.check_current(&self.home)?;
                let encoded = event.encode_to_vec();
                sent = sent
                    .checked_add(encoded.len() as u64)
                    .context("search bytes overflow")?;
                ensure!(
                    encoded.len() <= frame as usize && sent <= bytes,
                    "search result exceeds accepted byte budget"
                );
                send.write_all(&api::framing::encode_stream_message(&encoded)?)
                    .await?;
            }
            Result::<()>::Ok(())
        })
        .await
        .map_err(anyhow::Error::from)
        .and_then(|result| result);
        if let Err(error) = result {
            tokio::time::timeout(
                Duration::from_secs(5),
                send.write_all(&api::framing::encode_stream_failure(&failure(
                    CallFailureCode::FailedPrecondition,
                    error,
                ))?),
            )
            .await??;
        }
        send.finish()?;
        Ok(())
    }
}
