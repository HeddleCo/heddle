//! Exact-State analysis uses the existing native semantic index and committed change feed.
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context, Result, ensure};
use api::heddle::api::v2alpha1::*;
#[cfg(feature = "semantic")]
use objects::object::OperationId;
use objects::{object::ContentHash, store::ObjectStore};
use prost::Message;

use super::{DeviceRpc, auth::Session, checkout};

#[derive(Debug)]
pub(super) struct Runtime {
    #[cfg(feature = "semantic")]
    executor: std::sync::OnceLock<String>,
    #[cfg(all(test, feature = "semantic"))]
    pub gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    #[cfg(feature = "semantic")]
    pub workers: Arc<tokio::sync::Semaphore>,
    pub active: Mutex<BTreeMap<(uuid::Uuid, String, ContentHash), Arc<AtomicBool>>>,
}
impl Default for Runtime {
    fn default() -> Self {
        Self {
            #[cfg(all(test, feature = "semantic"))]
            gate: Mutex::new(None),
            #[cfg(feature = "semantic")]
            executor: std::sync::OnceLock::new(),
            #[cfg(feature = "semantic")]
            workers: Arc::new(tokio::sync::Semaphore::new(2)),
            active: Mutex::new(BTreeMap::new()),
        }
    }
}

#[cfg(feature = "semantic")]
impl Runtime {
    fn executor(&self) -> Result<&str> {
        if self.executor.get().is_none() {
            let _ = self
                .executor
                .set(repo::device_operations::executor_identity()?);
        }
        self.executor
            .get()
            .map(String::as_str)
            .context("executor identity initialization failed")
    }
}

impl DeviceRpc {
    #[cfg(feature = "semantic")]
    pub(super) async fn start_analysis(
        &self,
        session: Session,
        body: &[u8],
        mut send: iroh::endpoint::SendStream,
    ) -> Result<()> {
        let session = Arc::new(session);
        let result=(||->Result<(Vec<u8>,Option<(repo::device_operations::Started,ContentHash,Vec<objects::object::StateId>,String,tokio::sync::OwnedSemaphorePermit)>)> {
            let request=StartAnalysisRequest::decode(body)?;
            ensure!(request.execution_endpoint.as_ref()==Some(&self.endpoint()),"analysis requires this exact execution endpoint");
            ensure!(request.expected_disclosure_policy_version.is_empty(),"local semantic indexing does not execute provider disclosure policies");
            ensure!(request.kinds.len()<=6 && request.kinds.iter().all(|kind| matches!(*kind,k if k==AnalysisKind::SemanticIndex as i32||k==AnalysisKind::SemanticDiff as i32)),"this native producer executes semantic index and diff analysis");
            ensure!(!request.kinds.contains(&(AnalysisKind::SemanticDiff as i32)) || request.base.is_some(),"semantic diff requires exact base revision");
            let mut state=vec![checkout::revision(&session,request.source.as_ref())?];
            if let Some(base)=request.base.as_ref() { let base=checkout::revision(&session,Some(base))?;if !state.contains(&base){state.push(base);} }
            session.check_current(&self.home)?;
            let namespace=session.command_namespace()?;
            let id=request.client_operation_id.parse::<OperationId>()?;
            let key=repo::operation_dedup::receipt_record_key(&namespace,id);
            let reference=RecordRef{spool:Some(SpoolRef{id:session.spool.id.to_string()}),id:key.to_string()};
            let mut receipt=self.receipt(&request.client_operation_id);
            receipt.outcome=Some(mutation_receipt::Outcome::PendingOperation(reference.clone()));
            let response=MutationResponse{receipt:Some(receipt)}.encode_to_vec();
            if let Some(replayed) = repo::device_operations::replay_response(&session.spool.heddle_dir, &repo::device_operations::Command { namespace: &namespace, id, method: "/heddle.api.v2alpha1.AnalysisService/StartAnalysis", request_hash: *blake3::hash(body).as_bytes() })? { return Ok((replayed, None)); }
            let permit=self.analysis.workers.clone().try_acquire_owned().context("native analysis workers are busy")?;
            let started=repo::device_operations::start(&session.spool.heddle_dir,repo::device_operations::Command{namespace:&namespace,id,method:"/heddle.api.v2alpha1.AnalysisService/StartAnalysis",request_hash:*blake3::hash(body).as_bytes()},self.analysis.executor()?,OperationRecord{
                r#ref:Some(reference),client_operation_id:request.client_operation_id,state:operation_record::State::Queued as i32,
                total_units:Some(state.len() as u64),unit:"semantic index".into(),cancellation_supported:true,..Default::default()
            },response)?;
            let response=started.response.clone();
            Ok((response,Some((started,key,state,namespace,permit))))
        })();
        let response = match result {
            Ok((response, Some((started, key, state, namespace, permit)))) => {
                if started.fresh {
                    let cancelled = Arc::new(AtomicBool::new(false));
                    self.analysis
                        .active
                        .lock()
                        .map_err(|_| anyhow::anyhow!("analysis registry poisoned"))?
                        .insert(
                            (session.spool.id, namespace.clone(), key),
                            cancelled.clone(),
                        );
                    let this = self.clone();
                    let session = session.clone();
                    tokio::spawn(async move {
                        if let Err(error) = this
                            .run_analysis(
                                session.clone(),
                                namespace.clone(),
                                key,
                                state,
                                cancelled,
                                permit,
                            )
                            .await
                        {
                            tracing::error!(%error,"native semantic executor failed");
                            let failure = super::failure(
                                api::heddle::api::v1alpha1::CallFailureCode::FailedPrecondition,
                                error,
                            );
                            if let Err(error) = this.analysis.executor().and_then(|executor| {
                                repo::device_operations::transition(
                                    &session.spool.heddle_dir,
                                    &namespace,
                                    key,
                                    executor,
                                    operation_record::State::Failed,
                                    Some(failure),
                                )
                            }) {
                                tracing::error!(%error,"cannot persist executor failure");
                            }
                            if let Ok(mut active) = this.analysis.active.lock() {
                                active.remove(&(session.spool.id, namespace, key));
                            }
                        }
                    });
                }
                response
            }
            Ok((response, None)) => response,
            Err(error) => {
                let bytes = api::framing::encode_failure_response(&super::failure(
                    api::heddle::api::v1alpha1::CallFailureCode::FailedPrecondition,
                    error,
                ))?;
                send.write_all(&bytes).await?;
                send.finish()?;
                return Ok(());
            }
        };
        session.check_current(&self.home)?;
        let bytes = api::framing::encode_success_response(&response)?;
        tokio::time::timeout(std::time::Duration::from_secs(5), send.write_all(&bytes)).await??;
        send.finish()?;
        Ok(())
    }
    #[cfg(feature = "semantic")]
    async fn run_analysis(
        &self,
        session: Arc<Session>,
        namespace: String,
        key: ContentHash,
        state: Vec<objects::object::StateId>,
        cancelled: Arc<AtomicBool>,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> Result<()> {
        #[cfg(test)]
        {
            let gate = self
                .analysis
                .gate
                .lock()
                .map_err(|_| anyhow::anyhow!("analysis test gate"))?
                .clone();
            if let Some(gate) = gate {
                let _release = gate.acquire().await?;
            }
        }
        let feed = self.feed(&session)?;
        let mut changes = feed.changes.subscribe();
        let mut clock = self.authority_clock.subscribe()?;
        let mut check_clock = true;
        let mut watch_open = true;
        let running = repo::device_operations::transition(
            &session.spool.heddle_dir,
            &namespace,
            key,
            self.analysis.executor()?,
            operation_record::State::Running,
            None,
        )?;
        if running.cancellation_requested {
            cancelled.store(true, Ordering::Release);
        }
        let source = session.clone();
        let home = self.home.clone();
        let flag = cancelled.clone();
        let mut work = tokio::task::spawn_blocking(move || -> Result<()> {
            let _permit = permit;
            source.check_current(&home)?;
            let repository = repo::Repository::open(&source.spool.root)?;
            let budget=repo::SemanticParseBudget { cancelled:flag, deadline:std::time::Instant::now()+std::time::Duration::from_secs(30) };
            for state in state {
                repository.analyze_semantic_index_with_admission(state,budget.clone(), || source.check_current(&home).map_err(|error|repo::HeddleError::InvalidObject(error.to_string())))?;
            }

            Ok(())
        });
        let result = loop {
            tokio::select! {
                value=&mut work=>break value.map_err(anyhow::Error::from).and_then(|v|v),
                _=clock.expired(||session.check_clock()), if check_clock=>{ cancelled.store(true,Ordering::Release); check_clock=false; },
                changed=changes.changed(), if watch_open=>{
                    if changed.is_err() { cancelled.store(true,Ordering::Release); watch_open=false; continue; }
                    let record=repo::device_operations::get(&session.spool.heddle_dir,&namespace,key)?;
                    if record.is_some_and(|record|record.cancellation_requested) || session.check_current(&self.home).is_err() { cancelled.store(true,Ordering::Release); }
                }
            }
        };
        let (state, failure) = match result {
            Ok(()) => (operation_record::State::Completed, None),
            Err(_error) if cancelled.load(Ordering::Acquire) => {
                (operation_record::State::Canceled, None)
            }
            Err(error) => (
                operation_record::State::Failed,
                Some(super::failure(
                    api::heddle::api::v1alpha1::CallFailureCode::FailedPrecondition,
                    error,
                )),
            ),
        };
        let transitioned = repo::device_operations::transition(
            &session.spool.heddle_dir,
            &namespace,
            key,
            self.analysis.executor()?,
            state,
            failure,
        );
        self.analysis
            .active
            .lock()
            .map_err(|_| anyhow::anyhow!("analysis registry poisoned"))?
            .remove(&(session.spool.id, namespace, key));
        transitioned?;
        Ok(())
    }
    pub(super) fn cancel_operation(&self, session: &Session, body: &[u8]) -> Result<Vec<u8>> {
        let request = CancelOperationRequest::decode(body)?;
        let reference = request.operation.as_ref().context("operation required")?;
        checkout::same_spool(session, reference.spool.as_ref())?;
        let namespace = session.command_namespace()?;
        let key = ContentHash::from_hex(&reference.id)?;
        let response = MutationResponse {
            receipt: Some(self.receipt(&request.client_operation_id)),
        }
        .encode_to_vec();
        let accepted = repo::device_operations::cancel(
            &session.spool.heddle_dir,
            repo::device_operations::Command {
                namespace: &namespace,
                id: request.client_operation_id.parse()?,
                method: "/heddle.api.v2alpha1.OperationService/CancelOperation",
                request_hash: *blake3::hash(body).as_bytes(),
            },
            key,
            &request.expected_version,
            response,
        )?;
        if let Some(flag) = self
            .analysis
            .active
            .lock()
            .map_err(|_| anyhow::anyhow!("analysis registry poisoned"))?
            .get(&(session.spool.id, namespace, key))
        {
            flag.store(true, Ordering::Release);
        }
        Ok(accepted.response)
    }
}

impl super::stream::Event for AnalysisEvent {
    fn frame(&mut self, frame: StreamFrame) {
        self.frame = Some(frame);
    }
}
impl DeviceRpc {
    pub(super) async fn observe_analysis(
        &self,
        session: &Session,
        body: &[u8],
        send: iroh::endpoint::SendStream,
    ) -> Result<()> {
        let request = ObserveAnalysisRequest::decode(body)?;
        checkout::revision(session, request.source.as_ref())?;
        if request.base.is_some() {
            checkout::revision(session, request.base.as_ref())?;
        }
        ensure!(
            request.paths.len() <= 128 && request.symbols.len() <= 128 && request.kinds.len() <= 6,
            "analysis selector bound"
        );
        for path in &request.paths {
            super::content::normalize(path, false)?;
        }
        ensure!(
            request
                .symbols
                .iter()
                .all(|symbol| !symbol.is_empty() && symbol.len() <= 4096),
            "analysis symbol selector bound"
        );
        let mut normalized = request.clone();
        normalized.observe = None;
        if let Some(page) = normalized.page.as_mut() {
            page.after_page.clear();
        }
        self.observe_view(
            session,
            "/heddle.api.v2alpha1.AnalysisService/ObserveAnalysis",
            &normalized.encode_to_vec(),
            request.observe.clone().unwrap_or_default(),
            send,
            |budget, binding| self.analysis_snapshot(session, &request, budget, binding),
            || analysis_version(session, &request),
        )
        .await
    }
    fn analysis_snapshot(
        &self,
        session: &Session,
        request: &ObserveAnalysisRequest,
        budget: &ReadBudget,
        binding: &[u8],
    ) -> Result<(Vec<(String, AnalysisEvent)>, PageInfo, Vec<u8>)> {
        let before = analysis_version(session, request)?;
        let repository = repo::Repository::open(&session.spool.root)?;
        let state = checkout::revision(session, request.source.as_ref())?;
        let base = request
            .base
            .as_ref()
            .map(|base| checkout::revision(session, Some(base)))
            .transpose()?;
        let kinds: std::collections::BTreeSet<_> = if request.kinds.is_empty() {
            [AnalysisKind::SemanticIndex as i32].into_iter().collect()
        } else {
            request.kinds.iter().copied().collect()
        };
        let mut records = Vec::new();
        for raw_kind in kinds {
            let kind = AnalysisKind::try_from(raw_kind).context("unknown analysis kind")?;
            ensure!(kind != AnalysisKind::Unspecified, "analysis kind required");
            let root = repository.attached_semantic_index(&state)?;
            let root_base = base
                .map(|base| repository.attached_semantic_index(&base))
                .transpose()?
                .flatten();
            let supported = matches!(
                kind,
                AnalysisKind::SemanticIndex | AnalysisKind::SemanticDiff
            );
            let available = root.is_some()
                && (kind != AnalysisKind::SemanticDiff || (base.is_some() && root_base.is_some()));
            let key = ContentHash::compute_typed(
                "heddle-native-analysis-v2",
                &[
                    state.as_bytes().as_slice(),
                    &raw_kind.to_be_bytes(),
                    base.as_ref()
                        .map(|v| v.as_bytes().as_slice())
                        .unwrap_or_default(),
                ]
                .concat(),
            );
            let reference = RecordRef {
                spool: Some(SpoolRef {
                    id: session.spool.id.to_string(),
                }),
                id: key.to_string(),
            };
            let mut findings = Vec::new();
            let mut complete = true;
            if supported && available {
                let current = semantic_symbols(&repository, state, request, &mut complete)?;
                if kind == AnalysisKind::SemanticIndex {
                    for ((path, address), symbol) in current {
                        findings.push(AnalysisFinding {
                            analysis: Some(reference.clone()),
                            id: format!("{path}::{address}"),
                            path,
                            line: Some(symbol.span.0),
                            explanation: format!("{address} ({:?})", symbol.kind),
                            severity: analysis_finding::Severity::Informational as i32,
                            evidence: vec![],
                        });
                    }
                } else {
                    let old = semantic_symbols(
                        &repository,
                        base.context("semantic diff base")?,
                        request,
                        &mut complete,
                    )?;
                    let keys: std::collections::BTreeSet<_> =
                        old.keys().chain(current.keys()).cloned().collect();
                    for (path, address) in keys {
                        let prior = old.get(&(path.clone(), address.clone()));
                        let next = current.get(&(path.clone(), address.clone()));
                        if prior.map(|s| s.semantic_hash) == next.map(|s| s.semantic_hash) {
                            continue;
                        }
                        let change = match (prior, next) {
                            (None, Some(_)) => "added",
                            (Some(_), None) => "removed",
                            _ => "changed",
                        };
                        findings.push(AnalysisFinding {
                            analysis: Some(reference.clone()),
                            id: format!("{path}::{address}"),
                            path,
                            line: next.or(prior).map(|s| s.span.0),
                            explanation: format!("{address}: semantic definition {change}"),
                            severity: analysis_finding::Severity::Informational as i32,
                            evidence: vec![],
                        });
                    }
                }
            }
            let coverage = if !supported || !available {
                Coverage::Unavailable
            } else if complete {
                Coverage::Complete
            } else {
                Coverage::Partial
            };
            let analysis = AnalysisRecord {
                r#ref: Some(reference),
                source: request.source.clone(),
                base: request.base.clone(),
                kind: raw_kind,
                analyzer: "heddle-semantic-index".into(),
                analyzer_version: root
                    .as_ref()
                    .map(|r| r.extractor_version.to_string())
                    .unwrap_or_default(),
                coverage: coverage as i32,
                operation: None,
                version: before.clone(),
                finding_count: (supported && available).then_some(findings.len() as u64),
            };
            records.push((
                format!("{key}"),
                AnalysisEvent {
                    frame: None,
                    payload: Some(analysis_event::Payload::Analysis(analysis)),
                },
            ));
            for finding in findings {
                records.push((
                    format!("{key}/{}", finding.id),
                    AnalysisEvent {
                        frame: None,
                        payload: Some(analysis_event::Payload::Finding(finding)),
                    },
                ));
            }
        }
        ensure!(records.len() <= 8192, "analysis result work bound");
        let page = request.page.clone().unwrap_or_default();
        let after: usize =
            super::account_observe::decode_page(&page.after_page, binding, b"analysis")?
                .unwrap_or(0);
        ensure!(
            after <= records.len(),
            "analysis page outside current result"
        );
        let limit = super::account_observe::page_size(&page, budget).min(256);
        let end = (after + limit).min(records.len());
        let exhausted = end == records.len();
        let events = records
            .into_iter()
            .skip(after)
            .take(limit)
            .collect::<Vec<_>>();
        let bytes: usize = events.iter().map(|(_, event)| event.encoded_len()).sum();
        ensure!(
            bytes as u64 <= budget.max_snapshot_bytes
                && events
                    .iter()
                    .all(|(_, event)| event.encoded_len() <= budget.max_frame_bytes as usize),
            "analysis page byte budget exceeded"
        );
        ensure!(
            analysis_version(session, request)? == before,
            "analysis changed during projection"
        );
        let next_page = if exhausted {
            Vec::new()
        } else {
            super::account_observe::encode_page(&end, binding, b"analysis")?
        };
        Ok((
            events,
            PageInfo {
                next_page,
                exhausted,
                ..Default::default()
            },
            before,
        ))
    }
}
fn analysis_version(session: &Session, request: &ObserveAnalysisRequest) -> Result<Vec<u8>> {
    let repository = repo::Repository::open(&session.spool.root)?;
    let mut hash = blake3::Hasher::new_derive_key("heddle-native-analysis-view-v2");
    for revision in [request.source.as_ref(), request.base.as_ref()]
        .into_iter()
        .flatten()
    {
        let state = checkout::revision(session, Some(revision))?;
        hash.update(state.as_bytes());
        if let Some(attachment) =
            repository.latest_state_attachment(&state, repo::StateAttachmentKind::SemanticIndex)?
        {
            hash.update(attachment.id().as_hash().as_bytes());
        }
    }
    // Includes audience/source-possession changes; attachment-only wakes still alter hash above.
    hash.update(&repo::operation_dedup::observation::generation(
        &session.spool.heddle_dir,
    )?);
    Ok(hash.finalize().as_bytes().to_vec())
}
fn semantic_symbols(
    repository: &repo::Repository,
    state: objects::object::StateId,
    request: &ObserveAnalysisRequest,
    complete: &mut bool,
) -> Result<BTreeMap<(String, String), objects::object::SymbolEntry>> {
    use objects::object::{SemanticEntryKind, SemanticFileNode, SemanticTreeNode};
    let root = repository
        .attached_semantic_index(&state)?
        .context("semantic index absent")?;
    let source_tree = repository
        .store()
        .get_state(&state)?
        .context("analysis source unavailable")?
        .tree;
    let mut path_work = 0usize;
    let mut pending = vec![(String::new(), root.tree, 0usize)];
    let mut entries = 0usize;
    let mut bytes = 0u64;
    let mut result = BTreeMap::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while let Some((prefix, hash, depth)) = pending.pop() {
        ensure!(
            entries <= 4096 && depth <= 128 && std::time::Instant::now() < deadline,
            "semantic query work budget exceeded"
        );
        let length = objects::store::ObjectSource::decoded_blob_len(repository.store(), &hash)?
            .context("semantic directory absent")?;
        bytes = bytes
            .checked_add(length)
            .context("semantic query byte overflow")?;
        ensure!(bytes <= 8 * 1024 * 1024, "semantic query byte budget");
        let blob = repository
            .store()
            .get_blob(&hash)?
            .context("semantic directory absent")?;
        let tree = SemanticTreeNode::decode(blob.content())?;
        entries = entries.saturating_add(tree.entries.len());
        ensure!(entries <= 4096, "semantic query entry bound");
        for entry in tree.entries {
            super::content::normalize(&entry.name, false)?;
            ensure!(
                !entry.name.contains('/'),
                "semantic component is not a single path segment"
            );
            let path = if prefix.is_empty() {
                entry.name
            } else {
                format!("{prefix}/{}", entry.name)
            };
            if !request.paths.is_empty()
                && !request.paths.iter().any(|selected| {
                    path == *selected
                        || path.starts_with(&format!("{selected}/"))
                        || selected.starts_with(&format!("{path}/"))
                })
            {
                continue;
            }
            match entry.kind {
                SemanticEntryKind::Dir => pending.push((path, entry.node, depth + 1)),
                SemanticEntryKind::Opaque => *complete = false,
                SemanticEntryKind::File => {
                    let length = objects::store::ObjectSource::decoded_blob_len(
                        repository.store(),
                        &entry.node,
                    )?
                    .context("semantic file absent")?;
                    bytes = bytes
                        .checked_add(length)
                        .context("semantic query byte overflow")?;
                    ensure!(bytes <= 8 * 1024 * 1024, "semantic query byte budget");
                    let blob = repository
                        .store()
                        .get_blob(&entry.node)?
                        .context("semantic file absent")?;
                    let file = SemanticFileNode::decode(blob.content())?;
                    let source = super::content::path_entry(
                        repository.store(),
                        source_tree,
                        &path,
                        &mut path_work,
                    )?;
                    ensure!(
                        source.blob_hash() == Some(file.source_blob),
                        "semantic file does not describe this exact source path"
                    );
                    for symbol in file.symbols {
                        let address = symbol.address();
                        if request.symbols.is_empty() || request.symbols.contains(&address) {
                            result.insert((path.clone(), address), symbol);
                            ensure!(result.len() <= 4096, "semantic symbol result bound");
                        }
                    }
                }
            }
        }
    }
    Ok(result)
}
