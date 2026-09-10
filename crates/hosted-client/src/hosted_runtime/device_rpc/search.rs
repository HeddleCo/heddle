//! Finite indexed local search; result frames never retain a SQLite transaction.
use std::{collections::BTreeSet, sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use api::heddle::api::{v1alpha1::CallFailureCode, v2alpha1::*};
use iroh::endpoint::SendStream;
use prost::Message;

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
            let mode =
                search_request::Mode::try_from(request.mode).context("unknown search mode")?;
            let mut spools = BTreeSet::new();
            for spool in &request.spools {
                ensure!(
                    spools.insert(uuid::Uuid::parse_str(&spool.id)?),
                    "duplicate search Spool"
                );
            }
            ensure!(
                !spools.is_empty() && spools.len() <= 32,
                "search requires one to thirty-two explicit Spools"
            );
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
                if mode != search_request::Mode::Semantic {
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
                        let mut hits = repo::thread_replication::collaboration_search::search(
                            &spool.heddle_dir,
                            &request.text,
                            offset,
                            remaining as u32 + 1,
                        )?;
                        let exhausted = hits.len() <= remaining;
                        hits.truncate(remaining);
                        examined = examined.saturating_add(hits.len());
                        offset = offset
                            .checked_add(hits.len() as u32)
                            .context("search offset overflow")?;
                        let repository = repo::Repository::open(&spool.root)?;
                        let facts = worker_session.facts(Some(&spool.capability_path))?;
                        for hit in hits {
                            let replica = repo::thread_replication::ThreadReplica::open(
                                &spool.heddle_dir,
                                hit.thread,
                            )?;
                            if !super::auth::thread_visible(
                                &repository,
                                &replica,
                                uuid::Uuid::parse_str(&worker_session.principal)?,
                                facts.delegation_agent_id.as_deref(),
                            )? {
                                continue;
                            }
                            let reference = RecordRef {
                                spool: Some(SpoolRef {
                                    id: spool.id.to_string(),
                                }),
                                id: hit.record,
                            };
                            let entity = if hit.kind == 2 {
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
                                    ..Default::default()
                                })),
                            });
                        }
                        if exhausted {
                            position += 1;
                            offset = 0;
                        } else {
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
                        coverage: if mode == search_request::Mode::Semantic {
                            Coverage::Unavailable
                        } else {
                            Coverage::Partial
                        } as i32,
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
