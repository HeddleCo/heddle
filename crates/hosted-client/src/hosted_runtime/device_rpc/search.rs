//! Finite indexed local search; result frames never retain a SQLite transaction.
use std::{collections::{BTreeMap, BTreeSet}, sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use api::heddle::api::{v1alpha1::CallFailureCode, v2alpha1::*};
use iroh::endpoint::SendStream;
use prost::Message;

struct SearchSelection {
    spools: BTreeSet<uuid::Uuid>,
    threads: BTreeMap<uuid::Uuid, BTreeSet<Vec<u8>>>,
    kinds: BTreeSet<i32>,
    annotations: Option<objects::object::AnnotationQuery>,
}

impl SearchSelection {
    fn parse(request: &SearchRequest) -> Result<Self> {
        ensure!(request.spools.len() <= 32 && request.threads.len() <= 64 && request.domains.len() <= 3, "search selector bound exceeded");
        let mut spools = BTreeSet::new();
        for spool in &request.spools {
            ensure!(spools.insert(uuid::Uuid::parse_str(&spool.id)?), "duplicate search Spool");
        }
        let mut threads = BTreeMap::<uuid::Uuid, BTreeSet<Vec<u8>>>::new();
        for thread in &request.threads {
            let spool = thread.spool.as_ref().context("search Thread requires Spool")?;
            let spool = uuid::Uuid::parse_str(&spool.id)?;
            let id = thread.id.as_ref().context("search Thread requires identity")?;
            ensure!(id.value.len() == 32, "search Thread identity must be 32 bytes");
            ensure!(threads.entry(spool).or_default().insert(id.value.clone()), "duplicate search Thread");
        }
        if spools.is_empty() { spools.extend(threads.keys().copied()); }
        else if !threads.is_empty() { spools.retain(|spool| threads.contains_key(spool)); }
        let annotations = request.annotations.as_ref().map(thread_api::collaboration::annotation_query).transpose()?;
        let mut kinds = BTreeSet::new();
        if request.domains.is_empty() {
            if annotations.is_some() { kinds.insert(SearchDomain::Context as i32); }
            else { kinds.extend([SearchDomain::Thread as i32, SearchDomain::Discussion as i32, SearchDomain::Context as i32]); }
        } else {
            for domain in &request.domains {
                let kind = SearchDomain::try_from(*domain).context("invalid search domain")?;
                ensure!(matches!(kind, SearchDomain::Thread | SearchDomain::Discussion | SearchDomain::Context), "unsupported search domain");
                ensure!(kinds.insert(kind as i32), "duplicate search domain");
            }
        }
        ensure!(annotations.is_none() || kinds == BTreeSet::from([SearchDomain::Context as i32]), "annotation filters select context revisions only");
        ensure!(!request.text.trim().is_empty() || annotations.is_some(), "search requires text or annotation filters");
        ensure!(request.text.len() <= 4096, "search text exceeds bound");
        Ok(Self { spools, threads, kinds, annotations })
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
            let worker = tokio::task::spawn_blocking(move || -> Result<Vec<SearchEvent>> {
                let _permit = permit;
                let deadline = std::time::Instant::now() + Duration::from_secs(25);
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
                    generations.push(generation);
                }
                let binding = digest.finalize();
                let (mut position, mut offset) = if page.after_page.is_empty() {
                    (0usize, 0u32)
                } else {
                    ensure!(
                        page.after_page.len() == 40 && &page.after_page[..32] == binding.as_bytes(),
                        "search cursor differs from query, caller or committed data"
                    );
                    (
                        u32::from_be_bytes(page.after_page[32..36].try_into()?) as usize,
                        u32::from_be_bytes(page.after_page[36..40].try_into()?),
                    )
                };
                ensure!(position <= selected.len(), "search cursor outside Spools");
                let mut events = Vec::new();
                let mut examined = 0usize;
                if matches!(mode, search_request::Mode::Unspecified | search_request::Mode::Lexical) {
                    while position < selected.len()
                        && events.len() < limit as usize
                        && examined < 1024
                    {
                        ensure!(
                            std::time::Instant::now() < deadline,
                            "device query work deadline exceeded"
                        );
                        worker_session.check_clock()?;
                        let spool = &selected[position];
                        worker_session.facts(Some(&spool.capability_path))?;
                        let remaining = limit as usize - events.len();
                        let kinds: Vec<_> = selection.kinds.iter().copied().map(|kind| kind - 1).collect();
                        let batch = repo::thread_replication::collaboration_search::search_native(
                            &spool.heddle_dir,
                            &request.text,
                            offset,
                            remaining as u32,
                            &kinds,
                            selection.annotations.as_ref(),
                        )?;
                        let exhausted = batch.scanned < remaining;
                        examined = examined.saturating_add(batch.scanned);
                        offset = offset
                            .checked_add(batch.scanned as u32)
                            .context("search offset overflow")?;
                        let repository = repo::Repository::open(&spool.root)?;
                        let facts = worker_session.facts(Some(&spool.capability_path))?;
                        let principal = uuid::Uuid::parse_str(&worker_session.principal)?;
                        for hit in batch.hits {
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
                                let id = uuid::Uuid::parse_str(&hit.record)?;
                                if !super::auth::discussion_visible(&repository, &replica, principal, facts.delegation_agent_id.as_deref(), objects::object::DiscussionRecordId::from_uuid(id)?)? { continue; }
                            }
                            if hit.kind == 2 {
                                let (signed, _) = replica.operation(&hit.operation)?.context("indexed context operation absent")?;
                                let context = signed.verify()?.context_revision()?.context("indexed context revision absent")?;
                                if let Some(discussion) = context.extracted_from {
                                    if !super::auth::discussion_visible(&repository, &replica, principal, facts.delegation_agent_id.as_deref(), discussion)? { continue; }
                                }
                            }
                            let reference = RecordRef {
                                spool: Some(SpoolRef {
                                    id: spool.id.to_string(),
                                }),
                                id: hit.record,
                            };
                            let entity = if hit.kind == 0 {
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
                                    thread: Some(ThreadRef {
                                        spool: Some(SpoolRef { id: spool.id.to_string() }),
                                        id: Some(ThreadId { value: hit.thread.as_bytes().to_vec() }),
                                    }),
                                    causal_id: if hit.kind == 2 { hit.operation.as_bytes().to_vec() } else { Vec::new() },
                                    ..Default::default()
                                })),
                            });
                        }
                        if exhausted {
                            position += 1;
                            offset = 0;
                        } else if events.len() >= limit as usize {
                            break;
                        }
                    }
                } else {
                    position = selected.len();
                }
                let exhausted = position == selected.len();
                let next_page = if exhausted {
                    vec![]
                } else {
                    [
                        binding.as_bytes().as_slice(),
                        &(position as u32).to_be_bytes(),
                        &offset.to_be_bytes(),
                    ]
                    .concat()
                };
                events.push(SearchEvent {
                    source: Some(this.endpoint()),
                    payload: Some(search_event::Payload::Complete(SectionStatus {
                        section: "search".into(),
                        coverage: if exhausted { Coverage::Complete } else { Coverage::Partial } as i32,
                        page: Some(PageInfo {
                            exhausted,
                            next_page,
                            ..Default::default()
                        }),
                        ..Default::default()
                    })),
                });
                for (spool, observed) in selected.iter().zip(&generations) {
                    ensure!(
                        repo::thread_replication::collaboration::generation(&spool.heddle_dir)?
                            == *observed,
                        "search data changed during query; refresh the first page"
                    );
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
