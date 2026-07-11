// SPDX-License-Identifier: Apache-2.0
//! Local-mode implementation of the [`StateReviewService`] gRPC contract.
//!
//! Reads / writes the [`ReviewSignaturesBlob`] persisted at
//! [`State::review_signatures`]. Verifies the client-supplied signature
//! against the deterministic [`signing_payload`] before persisting.

// `::state_review` disambiguates from this module's own name
// (`grpc_local_impl::state_review`), the same way the hosted impl
// disambiguates by being in a sibling module.
use ::state_review::{
    PathSymbol, ReadingOrderPartition, SymbolKind, build_review_payload_partition,
};
use crypto::verify_payload_signature;
use grpc::heddle::v1::{
    AnchoredDiscussion, GetReviewPayloadRequest, GetReviewProgressRequest,
    GetReviewProgressResponse, ListSignaturesRequest, ListSignaturesResponse, MergeRequirement,
    PathSymbolRef as ProtoPathSymbolRef, ReadingOrderPartition as ProtoReadingOrderPartition,
    RecordCheckAckRequest, RecordCheckAckResponse, RecordVerdictRequest, RecordVerdictResponse,
    ReviewPayload, ReviewScope as ProtoReviewScope, ReviewSignature as ProtoReviewSignature,
    ReviewSummary, RiskSignal as ProtoRiskSignal, SignStateRequest, SignStateResponse,
    SignalAnchor as ProtoSignalAnchor, SigningFooter,
    state_review_service_server::StateReviewService,
};
use objects::{
    lock::RepositoryLockExt,
    object::{
        Blob, ChangeId, DiffKind, Discussion, DiscussionResolution, DiscussionsBlob, ReviewKind,
        ReviewScope, ReviewSignature, ReviewSignaturesBlob, RiskSignalBlob, State, SymbolAnchor,
        signing_payload,
    },
    store::ObjectStore,
    worktree::diff_blobs,
};
use prost::Message;
use repo::Repository;
use tonic::{Request, Response, Status};

use super::{GrpcLocalService, to_status, with_idempotency};

/// Maximum drift (seconds) between the client's `signed_at_unix` and the
/// server's wall clock. Generous enough to absorb NTP skew, narrow enough
/// to bound the window for replay-style attacks.
const SIGN_TIMESTAMP_SKEW_SECS: i64 = 5 * 60;

/// Local-mode `StateReviewService` implementation.
#[derive(Clone)]
pub struct LocalStateReviewService {
    inner: GrpcLocalService,
}

impl LocalStateReviewService {
    pub fn new(inner: GrpcLocalService) -> Self {
        Self { inner }
    }
}

#[tonic::async_trait]
impl StateReviewService for LocalStateReviewService {
    async fn get_review_payload(
        &self,
        request: Request<GetReviewPayloadRequest>,
    ) -> Result<Response<ReviewPayload>, Status> {
        let req = request.into_inner();
        let change_id = parse_change_id(&req.state_id)?;
        let repo = self.inner.repo();
        let state = repo
            .store()
            .get_state(&change_id)
            .map_err(to_status)?
            .ok_or_else(|| {
                Status::not_found(format!("state {} not found", change_id.to_string_full()))
            })?;

        // Diff the state's tree against its first parent so the summary
        // counts reflect what actually changed in this state. The
        // signal registry / budgeter will eventually layer on top of
        // this; until then `files_changed` is the most useful single
        // number an agent can use for self-review.
        let diff_summary = compute_state_diff_summary(repo, &state).map_err(to_status)?;

        let summary = ReviewSummary {
            headline: state.intent.clone().unwrap_or_default(),
            files_changed: diff_summary.files_changed,
            added_lines: diff_summary.added_lines,
            removed_lines: diff_summary.removed_lines,
            in_budget_signal_count: 0,
            hidden_signal_count: 0,
        };

        let agent_narrative = if state.attribution.agent.is_some() {
            state.intent.clone().unwrap_or_default()
        } else {
            String::new()
        };

        // Surface fired risk signals if requested. The signal registry will
        // replace this with a proper budget split; until then everything we
        // read is "visible".
        let mut all_signals: Vec<ProtoRiskSignal> = Vec::new();
        if req.include_all_signals
            && let Some(hash) = state.risk_signals
            && let Some(blob) = repo.store().get_blob(&hash).map_err(to_status)?
        {
            let decoded = RiskSignalBlob::decode(blob.content())
                .map_err(|err| Status::internal(format!("decode risk signals: {err}")))?;
            all_signals = decoded
                .signals
                .into_iter()
                .map(|s| risk_signal_to_proto(s, "visible"))
                .collect();
        }

        // Synthesize a structured `diff_summary` signal so the
        // `in_budget_signals` array is non-empty even before the real
        // signal registry is wired up. Anchored on each modified
        // file (capped) so consumers can iterate without losing the
        // summary aggregate. This is a deliberate stable shape: agents
        // already iterating signals get a usable record per file
        // change, and the registry-driven path will simply layer real
        // signals alongside it.
        let mut in_budget_signals: Vec<ProtoRiskSignal> = Vec::new();
        let summary_kind = match (
            diff_summary.added_files,
            diff_summary.modified_files,
            diff_summary.deleted_files,
        ) {
            (a, 0, 0) if a > 0 => "diff_summary.added_only",
            (0, m, 0) if m > 0 => "diff_summary.modified_only",
            (0, 0, d) if d > 0 => "diff_summary.deleted_only",
            (0, 0, 0) => "diff_summary.empty",
            _ => "diff_summary.mixed",
        };
        let summary_reason = format!(
            "{} files changed (+{}/-{}, {} added, {} modified, {} deleted)",
            diff_summary.files_changed,
            diff_summary.added_lines,
            diff_summary.removed_lines,
            diff_summary.added_files,
            diff_summary.modified_files,
            diff_summary.deleted_files,
        );
        // Per-file anchors keep the array reasoning-friendly when
        // many files change, but cap so very large diffs don't bloat
        // the payload. The aggregate summary always rides on the
        // first entry's reason field; the rest carry per-file deltas.
        const MAX_DIFF_SIGNAL_ANCHORS: usize = 32;
        if diff_summary.changed_paths.is_empty() {
            in_budget_signals.push(ProtoRiskSignal {
                kind: summary_kind.to_string(),
                anchor: Some(ProtoSignalAnchor {
                    file: String::new(),
                    symbol: String::new(),
                    start_line: 0,
                    end_line: 0,
                }),
                reason: summary_reason.clone(),
                producer_module: "review_show.diff_summary".to_string(),
                producer_version: 1,
                computed_at: None,
                visibility: "visible".to_string(),
            });
        } else {
            for (idx, path_kind) in diff_summary
                .changed_paths
                .iter()
                .take(MAX_DIFF_SIGNAL_ANCHORS)
                .enumerate()
            {
                let reason = if idx == 0 {
                    summary_reason.clone()
                } else {
                    format!("{} ({})", path_kind.path, path_kind.kind_str())
                };
                in_budget_signals.push(ProtoRiskSignal {
                    kind: summary_kind.to_string(),
                    anchor: Some(ProtoSignalAnchor {
                        file: path_kind.path.clone(),
                        symbol: String::new(),
                        start_line: 0,
                        end_line: 0,
                    }),
                    reason,
                    producer_module: "review_show.diff_summary".to_string(),
                    producer_version: 1,
                    computed_at: None,
                    visibility: "visible".to_string(),
                });
            }
        }

        // Server-side reading-order partition. Same per-symbol
        // extraction logic as the hosted handler: tree-sitter when the
        // `semantic` feature is enabled, path-only fallback otherwise.
        let symbols = changed_files_as_symbols(repo, &state, &diff_summary.changed_paths)
            .map_err(to_status)?;
        let partition = build_review_payload_partition(&symbols);

        // Project the state's `DiscussionsBlob` (when present)
        // into the wire-shape `AnchoredDiscussion` list.
        let discussions = match state.discussions {
            Some(hash) => {
                let blob = repo
                    .store()
                    .get_blob(&hash)
                    .map_err(to_status)?
                    .ok_or_else(|| {
                        Status::internal(format!(
                            "discussions blob {} referenced by state {} is missing",
                            hash,
                            state.change_id.to_string_full()
                        ))
                    })?;
                let decoded = DiscussionsBlob::decode(blob.content())
                    .map_err(|err| Status::internal(format!("decode discussions: {err}")))?;
                decoded
                    .discussions
                    .iter()
                    .map(discussion_to_anchored_proto)
                    .collect()
            }
            None => Vec::<AnchoredDiscussion>::new(),
        };

        let mut summary = summary;
        summary.in_budget_signal_count = in_budget_signals.len() as u32;
        summary.hidden_signal_count =
            all_signals.len().saturating_sub(in_budget_signals.len()) as u32;

        let payload = ReviewPayload {
            state_id: req.state_id.clone(),
            summary: Some(summary),
            agent_narrative,
            partition: Some(partition_to_proto(partition)),
            in_budget_signals,
            all_signals,
            tick_budget: 3,
            discussions,
            // Local mode has no policy registry — merge requirements
            // are surfaced only by the hosted handler.
            merge_requirements: Vec::<MergeRequirement>::new(),
            signing_footer: Some(SigningFooter {
                available_kinds: vec![
                    grpc::heddle::v1::ReviewKind::Read as i32,
                    grpc::heddle::v1::ReviewKind::AgentPreview as i32,
                    grpc::heddle::v1::ReviewKind::AgentCoReview as i32,
                ],
            }),
        };

        Ok(Response::new(payload))
    }

    async fn sign_state(
        &self,
        request: Request<SignStateRequest>,
    ) -> Result<Response<SignStateResponse>, Status> {
        let req = request.into_inner();
        let req_bytes = req.encode_to_vec();
        let client_operation_id = req.client_operation_id.clone();
        let inner = self.inner.clone();

        let response = with_idempotency(
            &self.inner,
            &client_operation_id,
            "state_review.sign_state",
            &req_bytes,
            move || {
                let inner = inner.clone();
                async move { execute_sign_state(&inner, req).await }
            },
        )
        .await?;

        Ok(Response::new(response))
    }

    async fn list_signatures(
        &self,
        request: Request<ListSignaturesRequest>,
    ) -> Result<Response<ListSignaturesResponse>, Status> {
        let req = request.into_inner();
        let change_id = parse_change_id(&req.state_id)?;
        let repo = self.inner.repo();
        let state = repo
            .store()
            .get_state(&change_id)
            .map_err(to_status)?
            .ok_or_else(|| {
                Status::not_found(format!("state {} not found", change_id.to_string_full()))
            })?;

        let signatures = match state.review_signatures {
            Some(hash) => {
                let blob = repo
                    .store()
                    .get_blob(&hash)
                    .map_err(to_status)?
                    .ok_or_else(|| {
                        Status::internal(format!(
                            "review signatures blob {} missing from object store",
                            hash
                        ))
                    })?;
                let decoded = ReviewSignaturesBlob::decode(blob.content())
                    .map_err(|err| Status::internal(format!("decode review signatures: {err}")))?;
                decoded
                    .signatures
                    .into_iter()
                    .enumerate()
                    .map(|(idx, sig)| review_signature_to_proto(sig, synthetic_signature_id(idx)))
                    .collect()
            }
            None => Vec::new(),
        };

        Ok(Response::new(ListSignaturesResponse { signatures }))
    }

    // The verdict (weft#481) and review-progress (weft#482) RPCs are defined
    // on the wire in heddle-grpc 0.19 but implemented server-side in weft
    // (the verdict blob append + the `review_check_acks` table). Local mode
    // does not back these; the handlers land with the weft impl.
    async fn record_verdict(
        &self,
        _request: Request<RecordVerdictRequest>,
    ) -> Result<Response<RecordVerdictResponse>, Status> {
        Err(Status::unimplemented(
            "RecordVerdict is not available in local mode (weft#481)",
        ))
    }

    async fn record_check_ack(
        &self,
        _request: Request<RecordCheckAckRequest>,
    ) -> Result<Response<RecordCheckAckResponse>, Status> {
        Err(Status::unimplemented(
            "RecordCheckAck is not available in local mode (weft#482)",
        ))
    }

    async fn get_review_progress(
        &self,
        _request: Request<GetReviewProgressRequest>,
    ) -> Result<Response<GetReviewProgressResponse>, Status> {
        Err(Status::unimplemented(
            "GetReviewProgress is not available in local mode (weft#482)",
        ))
    }
}

/// Body of [`LocalStateReviewService::sign_state`]. Lifted out of the trait
/// method so [`with_idempotency`] can re-execute it inside its closure.
async fn execute_sign_state(
    inner: &GrpcLocalService,
    req: SignStateRequest,
) -> Result<SignStateResponse, Status> {
    // 1. Validate the kind.
    let kind = match grpc::heddle::v1::ReviewKind::try_from(req.kind)
        .map_err(|_| Status::invalid_argument(format!("unknown review kind tag {}", req.kind)))?
    {
        grpc::heddle::v1::ReviewKind::Read => ReviewKind::Read,
        grpc::heddle::v1::ReviewKind::AgentPreview => ReviewKind::AgentPreview,
        grpc::heddle::v1::ReviewKind::AgentCoReview => ReviewKind::AgentCoReview,
        grpc::heddle::v1::ReviewKind::Unspecified => {
            return Err(Status::invalid_argument("review kind is required"));
        }
    };

    // 2. Locate the state.
    let change_id = parse_change_id(&req.state_id)?;
    let repo = inner.repo();

    // 3. Translate the scope.
    let scope = match req.scope.as_ref() {
        Some(s) => proto_scope_to_object(s)?,
        None => ReviewScope::WholeChange,
    };

    // 4. Build the ReviewSignature, then verify the client-supplied
    // signature is well-formed and matches the deterministic signing
    // payload. A malformed or forged signature must never reach the
    // persisted blob. Attribute the signature to the local-mode
    // caller (`Repository::get_principal` resolves env vars then
    // `[principal]` in `.heddle/config.toml`), not the state's author
    // — Bob signing Alice's state should record Bob.
    let actor = repo
        .get_principal()
        .map_err(|err| Status::internal(format!("resolve caller principal: {err}")))?;
    let justification = if req.justification.is_empty() {
        None
    } else {
        Some(req.justification.clone())
    };

    let now = chrono::Utc::now().timestamp();
    let signed_at = req.signed_at.as_ref().map(|t| t.seconds).unwrap_or(0);
    if signed_at == 0 {
        return Err(Status::invalid_argument(
            "signed_at is required and must match the timestamp the client signed over",
        ));
    }
    if (signed_at - now).abs() > SIGN_TIMESTAMP_SKEW_SECS {
        return Err(Status::invalid_argument(format!(
            "signed_at={signed_at} is too far from server time={now} (max skew {SIGN_TIMESTAMP_SKEW_SECS}s)"
        )));
    }

    let new_sig = ReviewSignature {
        actor,
        kind,
        scope: scope.clone(),
        justification: justification.clone(),
        signed_at,
        algorithm: req.algorithm.clone(),
        public_key: hex::encode(&req.public_key),
        signature: hex::encode(&req.signature),
    };
    new_sig
        .validate()
        .map_err(|err| Status::invalid_argument(format!("invalid review signature: {err}")))?;

    let public_key_bytes = req.public_key.clone();
    let signature_bytes = req.signature.clone();
    let payload = signing_payload(change_id, kind, &scope, signed_at, justification.as_deref());
    verify_payload_signature(
        &payload,
        &req.algorithm,
        &public_key_bytes,
        &signature_bytes,
    )
    .map_err(|err| {
        Status::invalid_argument(format!(
            "review signature failed verification ({}): {err}",
            req.algorithm
        ))
    })?;

    // 5. Serialize the read-modify-write on `review_signatures`
    // behind the repo write-lock and re-load the state inside the
    // critical section. Two concurrent SignStates with different
    // operation ids would otherwise both read the same base blob and
    // the second `put_state` would clobber the first signature.
    let _lock = repo
        .locker()
        .write()
        .map_err(|err| Status::internal(err.to_string()))?;
    let state = repo
        .store()
        .get_state(&change_id)
        .map_err(to_status)?
        .ok_or_else(|| {
            Status::not_found(format!("state {} not found", change_id.to_string_full()))
        })?;
    let mut blob = match state.review_signatures {
        Some(hash) => {
            let raw = repo
                .store()
                .get_blob(&hash)
                .map_err(to_status)?
                .ok_or_else(|| {
                    Status::internal(format!(
                        "existing review signatures blob {} missing from object store",
                        hash
                    ))
                })?;
            ReviewSignaturesBlob::decode(raw.content())
                .map_err(|err| Status::internal(format!("decode review signatures: {err}")))?
        }
        None => ReviewSignaturesBlob::new(Vec::new()),
    };
    blob.signatures.push(new_sig);
    let new_index = blob.signatures.len() - 1;

    // 6. Persist the new blob.
    let bytes = blob
        .encode()
        .map_err(|err| Status::internal(format!("encode review signatures: {err}")))?;
    let content_hash = repo
        .store()
        .put_blob(&Blob::new(bytes))
        .map_err(to_status)?;

    // 7. Persist the updated state.
    let new_state = state.with_review_signatures(content_hash);
    repo.store().put_state(&new_state).map_err(to_status)?;

    Ok(SignStateResponse {
        signature_id: synthetic_signature_id(new_index),
        state_id: req.state_id,
    })
}

/// `ReviewSignature` doesn't carry an explicit id; we synthesise one from
/// the per-state index so the wire surface has stable signature ids within a
/// single state. (A future schema bump may add an explicit id.)
fn synthetic_signature_id(index: usize) -> String {
    format!("rs-{index}")
}

fn parse_change_id(s: &[u8]) -> Result<ChangeId, Status> {
    ChangeId::try_from_slice(s)
        .map_err(|err| Status::invalid_argument(format!("invalid state_id: {err}")))
}

fn proto_scope_to_object(scope: &ProtoReviewScope) -> Result<ReviewScope, Status> {
    use grpc::heddle::v1::review_scope::Scope;
    match scope.scope.as_ref() {
        None | Some(Scope::WholeChange(_)) => Ok(ReviewScope::WholeChange),
        Some(Scope::Symbols(list)) => {
            if list.symbols.is_empty() {
                return Err(Status::invalid_argument(
                    "symbols scope requires at least one symbol anchor",
                ));
            }
            let symbols = list
                .symbols
                .iter()
                .map(|s| SymbolAnchor::new(s.file.clone(), s.symbol.clone()))
                .collect();
            Ok(ReviewScope::Symbols(symbols))
        }
    }
}

fn object_scope_to_proto(scope: &ReviewScope) -> ProtoReviewScope {
    use grpc::heddle::v1::review_scope::{Scope, SymbolList, WholeChange};
    let inner = match scope {
        ReviewScope::WholeChange => Scope::WholeChange(WholeChange {}),
        ReviewScope::Symbols(symbols) => Scope::Symbols(SymbolList {
            symbols: symbols
                .iter()
                .map(|s| ProtoPathSymbolRef {
                    file: s.file.clone(),
                    symbol: s.symbol.clone(),
                })
                .collect(),
        }),
    };
    ProtoReviewScope { scope: Some(inner) }
}

fn review_signature_to_proto(sig: ReviewSignature, signature_id: String) -> ProtoReviewSignature {
    ProtoReviewSignature {
        signature_id,
        actor_name: sig.actor.name.clone(),
        actor_email: sig.actor.email.clone(),
        kind: review_kind_to_proto(sig.kind) as i32,
        scope: Some(object_scope_to_proto(&sig.scope)),
        justification: sig.justification.unwrap_or_default(),
        signed_at: Some(prost_types::Timestamp {
            seconds: sig.signed_at,
            nanos: 0,
        }),
        algorithm: sig.algorithm,
        public_key: hex::decode(&sig.public_key).unwrap_or_default(),
        signature: hex::decode(&sig.signature).unwrap_or_default(),
        // Legacy positive signatures predate the verdict surface (weft#481):
        // VERDICT_UNSPECIFIED (treated as SIGN) with no decline reason.
        verdict: grpc::heddle::v1::Verdict::Unspecified as i32,
        reason: String::new(),
    }
}

fn review_kind_to_proto(kind: ReviewKind) -> grpc::heddle::v1::ReviewKind {
    match kind {
        ReviewKind::Read => grpc::heddle::v1::ReviewKind::Read,
        ReviewKind::AgentPreview => grpc::heddle::v1::ReviewKind::AgentPreview,
        ReviewKind::AgentCoReview => grpc::heddle::v1::ReviewKind::AgentCoReview,
    }
}

fn risk_signal_to_proto(sig: objects::object::RiskSignal, visibility: &str) -> ProtoRiskSignal {
    let (start_line, end_line) = sig.anchor.line_range.unwrap_or((0, 0));
    ProtoRiskSignal {
        kind: sig.kind.as_str().to_string(),
        anchor: Some(ProtoSignalAnchor {
            file: sig.anchor.file,
            symbol: sig.anchor.symbol.unwrap_or_default(),
            start_line,
            end_line,
        }),
        reason: sig.reason,
        producer_module: sig.producer.module,
        producer_version: sig.producer.version,
        computed_at: Some(prost_types::Timestamp {
            seconds: sig.computed_at,
            nanos: 0,
        }),
        visibility: visibility.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Symbol extraction + discussion projection (mirrors hosted impl).
// ---------------------------------------------------------------------------

/// Symbol projection for the reading-order partition. Mirrors the hosted-handler
/// implementation: when the `semantic` feature is enabled and the
/// changed path has a tree-sitter parser and a readable new-side blob, emits
/// one [`PathSymbol`] per definition. Otherwise falls back to a single path-only
/// entry (kind = `Other`), which keeps deletes and gitlink pointer changes
/// visible even though they do not carry Heddle blob content.
fn changed_files_as_symbols(
    repo: &Repository,
    state: &State,
    changed_paths: &[ChangedPath],
) -> objects::error::Result<Vec<PathSymbol>> {
    let new_tree = match repo.store().get_tree(&state.tree)? {
        Some(t) => t,
        None => return Ok(Vec::new()),
    };
    let new_files = collect_files(repo, &new_tree, "")?;

    let mut out: Vec<PathSymbol> = Vec::new();
    for path_kind in changed_paths {
        let path = &path_kind.path;
        #[cfg_attr(not(feature = "semantic"), allow(unused_mut))]
        let mut emitted_any = false;
        if let Some(hash) = new_files.get(path) {
            #[cfg(feature = "semantic")]
            {
                if let Some(blob) = repo.store().get_blob(hash)? {
                    emitted_any = extract_file_symbols(path, blob.content(), &mut out);
                }
            }
            #[cfg(not(feature = "semantic"))]
            {
                let _ = hash;
            }
        }
        if !emitted_any {
            out.push(PathSymbol {
                file: path.clone(),
                symbol: path.clone(),
                kind: SymbolKind::Other,
            });
        }
    }
    Ok(out)
}

#[cfg(feature = "semantic")]
fn extract_file_symbols(path: &str, source: &[u8], out: &mut Vec<PathSymbol>) -> bool {
    use ::semantic::symbol_resolver::{Definition, DefinitionKind, extract_definitions};
    let definitions: Vec<Definition> = match extract_definitions(source, std::path::Path::new(path))
    {
        Ok(defs) => defs,
        Err(_) => return false,
    };
    if definitions.is_empty() {
        return false;
    }
    for d in definitions {
        let kind = match d.kind {
            DefinitionKind::Type => SymbolKind::Type,
            DefinitionKind::Trait => SymbolKind::Trait,
            DefinitionKind::Class => SymbolKind::Class,
            DefinitionKind::Interface => SymbolKind::Interface,
            DefinitionKind::TypeAlias => SymbolKind::TypeAlias,
            DefinitionKind::EnumDef => SymbolKind::EnumDef,
            DefinitionKind::ConstDecl => SymbolKind::ConstDecl,
            DefinitionKind::Module => SymbolKind::Module,
            DefinitionKind::Function => SymbolKind::Function,
            DefinitionKind::Other => SymbolKind::Other,
        };
        let symbol = match d.parent_name.as_deref() {
            Some(parent) if !parent.is_empty() => format!("{parent}::{}", d.name),
            _ => d.name,
        };
        out.push(PathSymbol {
            file: path.to_string(),
            symbol,
            kind,
        });
    }
    true
}

fn collect_files(
    repo: &Repository,
    tree: &objects::object::Tree,
    prefix: &str,
) -> objects::error::Result<std::collections::HashMap<String, objects::object::ContentHash>> {
    let mut out = std::collections::HashMap::new();
    for entry in tree.entries() {
        let path = if prefix.is_empty() {
            entry.name().to_string()
        } else {
            format!("{prefix}/{}", entry.name())
        };
        if entry.is_tree() {
            if let Some(hash) = entry.tree_hash()
                && let Some(subtree) = repo.store().get_tree(&hash)?
            {
                let sub = collect_files(repo, &subtree, &path)?;
                out.extend(sub);
            }
        } else if let Some(hash) = entry.content_hash() {
            out.insert(path, hash);
        }
    }
    Ok(out)
}

fn partition_to_proto(p: ReadingOrderPartition) -> ProtoReadingOrderPartition {
    ProtoReadingOrderPartition {
        structural: p.structural.iter().map(path_symbol_to_proto).collect(),
        consequence: p.consequence.iter().map(path_symbol_to_proto).collect(),
        tests_and_docs: p.tests_and_docs.iter().map(path_symbol_to_proto).collect(),
    }
}

fn path_symbol_to_proto(p: &PathSymbol) -> ProtoPathSymbolRef {
    ProtoPathSymbolRef {
        file: p.file.clone(),
        symbol: p.symbol.clone(),
    }
}

fn discussion_to_anchored_proto(d: &Discussion) -> AnchoredDiscussion {
    AnchoredDiscussion {
        id: d.id.clone(),
        anchor: Some(ProtoPathSymbolRef {
            file: d.anchor.file.clone(),
            symbol: d.anchor.symbol.clone(),
        }),
        opened_against_state: d.opened_against_state.as_bytes().to_vec(),
        opened_at: Some(prost_types::Timestamp {
            seconds: d.opened_at,
            nanos: 0,
        }),
        turns: d
            .turns
            .iter()
            .map(|t| grpc::heddle::v1::DiscussionTurn {
                author_name: t.author.name.clone(),
                author_email: t.author.email.clone(),
                body: t.body.clone(),
                posted_at: Some(prost_types::Timestamp {
                    seconds: t.posted_at,
                    nanos: 0,
                }),
            })
            .collect(),
        resolution: Some(discussion_resolution_to_proto(&d.resolution)),
        body_changed_since_open: d.body_changed_since_open,
        orphaned: d.orphaned,
        visibility: d.visibility.as_str().to_string(),
    }
}

fn discussion_resolution_to_proto(
    resolution: &DiscussionResolution,
) -> grpc::heddle::v1::DiscussionResolution {
    use grpc::heddle::v1::discussion_resolution::{
        Dismissed, Open, ResolvedByEdit, ResolvedIntoAnnotation, State,
    };
    let state = match resolution {
        DiscussionResolution::Open => State::Open(Open {}),
        DiscussionResolution::ResolvedIntoAnnotation { annotation_id } => {
            State::IntoAnnotation(ResolvedIntoAnnotation {
                annotation_id: annotation_id.clone(),
            })
        }
        DiscussionResolution::ResolvedByEdit { state_id } => State::ByEdit(ResolvedByEdit {
            state_id: state_id.as_bytes().to_vec(),
        }),
        DiscussionResolution::Dismissed { reason } => State::Dismissed(Dismissed {
            reason: reason.clone(),
        }),
    };
    grpc::heddle::v1::DiscussionResolution { state: Some(state) }
}

// ---------------------------------------------------------------------------
// Diff summary helpers (state.tree vs first parent's tree).
// ---------------------------------------------------------------------------

/// File-change kinds we surface in the diff summary signal anchors.
/// Mirrors `objects::object::DiffKind` minus the `Unchanged` variant
/// (we filter those out before constructing this).
#[derive(Debug, Clone)]
struct ChangedPath {
    path: String,
    kind: DiffKind,
}

impl ChangedPath {
    fn kind_str(&self) -> &'static str {
        match self.kind {
            DiffKind::Added => "added",
            DiffKind::Modified => "modified",
            DiffKind::Deleted => "deleted",
            DiffKind::Unchanged => "unchanged",
        }
    }
}

/// Aggregated counts plus a path list, computed by diffing
/// `state.tree` against the first parent's tree (or empty when the
/// state is a root). When `state.parents` is empty every file in the
/// state's tree counts as added, which makes "first capture" reviews
/// non-empty too. The `_state` prefix on `_state` is intentional: the
/// helper currently only reads `state.tree` and `state.parents`.
struct DiffSummary {
    files_changed: u32,
    added_files: u32,
    modified_files: u32,
    deleted_files: u32,
    added_lines: u32,
    removed_lines: u32,
    changed_paths: Vec<ChangedPath>,
}

/// Compute a summary diff for `state` vs its first parent. Errors
/// from the object store propagate; missing trees / blobs are skipped
/// silently (treated as zero-change for that path) so a partially
/// pruned object store never blocks the review surface. The
/// distinction matters: missing-object errors must become zero (the
/// summary is best-effort, callers want a payload they can render),
/// but genuine I/O errors must still propagate so a corrupt store
/// surfaces loudly instead of silently truncating the review.
fn compute_state_diff_summary(
    repo: &Repository,
    state: &State,
) -> objects::error::Result<DiffSummary> {
    use objects::object::Tree;
    let parent_tree_hash = if let Some(parent_id) = state.parents.first() {
        match repo.store().get_state(parent_id)? {
            Some(parent_state) => parent_state.tree,
            None => Tree::new().hash(),
        }
    } else {
        Tree::new().hash()
    };

    // Resolve both tree objects up front so the missing-tree case
    // becomes a synthesized empty changeset rather than an error from
    // the recursive diff. `get_tree` returns `Ok(None)` for missing
    // (not an error), and propagates only on genuine I/O — matching
    // the policy the doc-comment claims.
    let parent_tree_obj = repo.store().get_tree(&parent_tree_hash)?;
    let new_tree_obj = repo.store().get_tree(&state.tree)?;

    // If either tree is missing from the local store the diff is not
    // meaningful — return an empty summary instead of erroring out.
    // This mirrors the "Modified branch tolerates missing blobs" stance
    // for the *tree* level: a partially pruned store should never block
    // review payload retrieval, only render an empty summary.
    let changes = if parent_tree_obj.is_some() && new_tree_obj.is_some() {
        repo.diff_trees(&parent_tree_hash, &state.tree)?
    } else {
        objects::object::FileChangeSet::new()
    };

    // Compute per-file line deltas. We only count `Modified` here for
    // the symmetric add/remove totals; `Added` files contribute every
    // line as an add, and `Deleted` files contribute every line as a
    // remove. Files with non-utf8 contents (e.g. binaries) silently
    // contribute zero — `diff_blobs` already returns an empty vec in
    // that case, and we mirror the same behavior for raw line counts.
    let mut added_lines: u32 = 0;
    let mut removed_lines: u32 = 0;
    let mut changed_paths: Vec<ChangedPath> = Vec::with_capacity(changes.len());

    let parent_files = match parent_tree_obj.as_ref() {
        Some(t) => collect_files(repo, t, "")?,
        None => std::collections::HashMap::new(),
    };
    let new_files = match new_tree_obj.as_ref() {
        Some(t) => collect_files(repo, t, "")?,
        None => std::collections::HashMap::new(),
    };

    let mut added_files: u32 = 0;
    let mut modified_files: u32 = 0;
    let mut deleted_files: u32 = 0;

    for change in changes.iter() {
        match change.kind {
            DiffKind::Added => {
                added_files += 1;
                // Missing blob (`get_blob` returns `Ok(None)`) → file
                // counts but contributes zero lines. Genuine I/O
                // errors still propagate via `?` — same shape as the
                // Modified branch's intent, but here we keep the
                // distinction explicit so a corrupt store surfaces
                // rather than getting silently swallowed.
                if let Some(hash) = new_files.get(&change.path)
                    && let Some(blob) = repo.store().get_blob(hash)?
                {
                    added_lines = added_lines.saturating_add(line_count(blob.content()));
                }
            }
            DiffKind::Deleted => {
                deleted_files += 1;
                if let Some(hash) = parent_files.get(&change.path)
                    && let Some(blob) = repo.store().get_blob(hash)?
                {
                    removed_lines = removed_lines.saturating_add(line_count(blob.content()));
                }
            }
            DiffKind::Modified => {
                modified_files += 1;
                // `get_blob` already returns `Ok(None)` for a missing
                // blob, so `?` here only fires on genuine I/O. Match
                // the Added/Deleted branches' propagation policy
                // explicitly instead of the older `.ok().flatten()`
                // form, which silently swallowed IO errors and
                // conflated them with "missing".
                let old_blob = match parent_files.get(&change.path) {
                    Some(h) => repo.store().get_blob(h)?,
                    None => None,
                };
                let new_blob = match new_files.get(&change.path) {
                    Some(h) => repo.store().get_blob(h)?,
                    None => None,
                };
                if let (Some(old), Some(new)) = (old_blob, new_blob) {
                    for line in diff_blobs(&old, &new) {
                        match line {
                            objects::worktree::DiffLine::Added(_) => {
                                added_lines = added_lines.saturating_add(1);
                            }
                            objects::worktree::DiffLine::Removed(_) => {
                                removed_lines = removed_lines.saturating_add(1);
                            }
                            objects::worktree::DiffLine::Context(_) => {}
                        }
                    }
                }
            }
            DiffKind::Unchanged => continue,
        }
        changed_paths.push(ChangedPath {
            path: change.path.clone(),
            kind: change.kind,
        });
    }

    Ok(DiffSummary {
        files_changed: changed_paths.len() as u32,
        added_files,
        modified_files,
        deleted_files,
        added_lines,
        removed_lines,
        changed_paths,
    })
}

/// Count the number of newline-separated lines in a file blob. Binary
/// blobs (non-utf8) count as zero — we deliberately don't byte-count
/// them, since "lines" is meaningless for binary content. A trailing
/// newline does not introduce a phantom empty line.
fn line_count(content: &[u8]) -> u32 {
    let Ok(s) = std::str::from_utf8(content) else {
        return 0;
    };
    if s.is_empty() {
        return 0;
    }
    let trimmed = s.strip_suffix('\n').unwrap_or(s);
    if trimmed.is_empty() {
        return 1;
    }
    (trimmed.matches('\n').count() as u32).saturating_add(1)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crypto::Signer as _;
    use grpc::heddle::v1::ReviewScope as ProtoReviewScope;
    use repo::{Repository, operation_dedup::OperationDedupStore};
    use tempfile::TempDir;

    use super::*;

    fn fresh_service() -> (LocalStateReviewService, Arc<Repository>, TempDir) {
        let temp = TempDir::new().expect("create tempdir");
        // SAFETY: tests run with a controlled environment; setting these
        // env vars steers the default attribution into a predictable place.
        // SAFETY: setting env vars in tests; rust 2024 marks these unsafe.
        unsafe {
            std::env::set_var("HEDDLE_PRINCIPAL_NAME", "Alice Tester");
            std::env::set_var("HEDDLE_PRINCIPAL_EMAIL", "alice@example.com");
        }
        let repo = Repository::init_default(temp.path()).expect("init repo");
        let dedup = OperationDedupStore::open(repo.heddle_dir()).expect("open dedup");
        let repo = Arc::new(repo);
        let svc =
            LocalStateReviewService::new(GrpcLocalService::new(repo.clone(), Arc::new(dedup)));
        (svc, repo, temp)
    }

    fn capture_state(repo: &Repository) -> ChangeId {
        // Write a tiny file so snapshot has something to capture.
        std::fs::write(repo.root().join("hello.txt"), b"hi").expect("write file");
        let state = repo
            .snapshot(Some("seed".to_string()), None)
            .expect("snapshot");
        state.change_id
    }

    fn sign_request(state_id: &ChangeId, op_id: &str) -> SignStateRequest {
        let signer = crypto::Ed25519Signer::generate().expect("generate ed25519 key");
        let scope = ReviewScope::WholeChange;
        let signed_at = chrono::Utc::now().timestamp();
        let payload = signing_payload(*state_id, ReviewKind::Read, &scope, signed_at, None);
        let signature = signer.sign(&payload).expect("sign payload");
        use grpc::heddle::v1::review_scope::{Scope, WholeChange};
        SignStateRequest {
            repo_path: String::new(),
            state_id: state_id.as_bytes().to_vec(),
            kind: grpc::heddle::v1::ReviewKind::Read as i32,
            scope: Some(ProtoReviewScope {
                scope: Some(Scope::WholeChange(WholeChange {})),
            }),
            justification: String::new(),
            algorithm: "ed25519".to_string(),
            public_key: signer.public_key().to_vec(),
            signature: signature.clone(),
            signed_at: Some(prost_types::Timestamp {
                seconds: signed_at,
                nanos: 0,
            }),
            client_operation_id: op_id.to_string(),
        }
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn sign_state_persists_to_review_signatures_blob() {
        let (svc, repo, _tmp) = fresh_service();
        let state_id = capture_state(&repo);

        let resp = svc
            .sign_state(Request::new(sign_request(&state_id, "")))
            .await
            .expect("sign_state");
        assert!(!resp.get_ref().signature_id.is_empty());
        assert_eq!(resp.get_ref().state_id, state_id.as_bytes().to_vec());

        let listing = svc
            .list_signatures(Request::new(ListSignaturesRequest {
                repo_path: String::new(),
                state_id: state_id.as_bytes().to_vec(),
            }))
            .await
            .expect("list_signatures");
        let sigs = &listing.get_ref().signatures;
        assert_eq!(sigs.len(), 1, "expected one signature, got {sigs:?}");
        assert_eq!(sigs[0].kind, grpc::heddle::v1::ReviewKind::Read as i32);
        assert_eq!(sigs[0].algorithm, "ed25519");
        assert_eq!(sigs[0].actor_name, "Alice Tester");
        assert_eq!(sigs[0].actor_email, "alice@example.com");
        let scope_case = sigs[0].scope.as_ref().and_then(|s| s.scope.as_ref());
        assert!(matches!(
            scope_case,
            Some(grpc::heddle::v1::review_scope::Scope::WholeChange(_))
        ));
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn sign_state_idempotent() {
        let (svc, repo, _tmp) = fresh_service();
        let state_id = capture_state(&repo);
        let op_id = objects::object::OperationId::new().to_string();
        // The second call must replay the *same* request body — fresh
        // signatures hash differently, so we build once and clone.
        let req = sign_request(&state_id, &op_id);

        let first = svc
            .sign_state(Request::new(req.clone()))
            .await
            .expect("first sign_state");
        let second = svc
            .sign_state(Request::new(req))
            .await
            .expect("second sign_state");
        assert_eq!(
            first.get_ref().signature_id,
            second.get_ref().signature_id,
            "replayed call must return the same signature_id"
        );

        let listing = svc
            .list_signatures(Request::new(ListSignaturesRequest {
                repo_path: String::new(),
                state_id: state_id.as_bytes().to_vec(),
            }))
            .await
            .expect("list_signatures");
        assert_eq!(
            listing.get_ref().signatures.len(),
            1,
            "idempotent replay must not append a duplicate signature"
        );
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn sign_state_rejects_forged_signature() {
        let (svc, repo, _tmp) = fresh_service();
        let state_id = capture_state(&repo);
        let mut req = sign_request(&state_id, "");
        // Flip the last byte of the signature so verification fails.
        let last = req.signature.len() - 1;
        req.signature[last] ^= 0xff;

        let err = svc
            .sign_state(Request::new(req))
            .await
            .expect_err("forged signature must be rejected");
        assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");
        assert!(
            err.message().contains("failed verification"),
            "unexpected error message: {}",
            err.message()
        );
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn sign_state_rejects_skewed_timestamp() {
        let (svc, repo, _tmp) = fresh_service();
        let state_id = capture_state(&repo);
        let mut req = sign_request(&state_id, "");
        // Timestamp 1 hour in the future is well outside the skew window.
        if let Some(ts) = req.signed_at.as_mut() {
            ts.seconds += 60 * 60;
        }

        let err = svc
            .sign_state(Request::new(req))
            .await
            .expect_err("skewed timestamp must be rejected");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains("too far from server time"));
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn sign_state_attributes_to_caller_not_state_author() {
        // Regression for the codex-flagged bug: sign_state used to
        // attribute the signature to the state's author. Bob signing
        // Alice's state would record Alice. The signature must record
        // the *caller*. In local mode the caller is resolved via
        // `Repository::get_principal()` (env vars then config).
        let (svc, repo, _tmp) = fresh_service();
        // `fresh_service` already set the env to Alice; capture the
        // state under Alice's identity so `state.attribution.principal`
        // is Alice.
        let state_id = capture_state(&repo);

        // Now switch the local user to Bob and sign Alice's state.
        // SAFETY: tests run with a controlled environment.
        unsafe {
            std::env::set_var("HEDDLE_PRINCIPAL_NAME", "Bob Signer");
            std::env::set_var("HEDDLE_PRINCIPAL_EMAIL", "bob@example.com");
        }

        svc.sign_state(Request::new(sign_request(&state_id, "")))
            .await
            .expect("sign_state");

        let listing = svc
            .list_signatures(Request::new(ListSignaturesRequest {
                repo_path: String::new(),
                state_id: state_id.as_bytes().to_vec(),
            }))
            .await
            .expect("list_signatures");
        let sigs = &listing.get_ref().signatures;
        assert_eq!(sigs.len(), 1);
        assert_eq!(
            sigs[0].actor_name, "Bob Signer",
            "signature must attribute to the caller (Bob), not the state author (Alice)"
        );
        assert_eq!(sigs[0].actor_email, "bob@example.com");

        // Restore the env so other serial tests see the expected
        // Alice baseline.
        unsafe {
            std::env::set_var("HEDDLE_PRINCIPAL_NAME", "Alice Tester");
            std::env::set_var("HEDDLE_PRINCIPAL_EMAIL", "alice@example.com");
        }
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn sign_state_serializes_concurrent_appends() {
        // Regression for the codex-flagged race: two SignStates with
        // different operation ids could both read the same base
        // `review_signatures` blob, then the second `put_state` would
        // clobber the first signature. The fix wraps the
        // read-modify-write in `repo.locker().write()` and re-loads
        // the state inside the lock.
        let (svc, repo, _tmp) = fresh_service();
        let state_id = capture_state(&repo);

        // Two distinct ops so neither replays the other through dedup.
        let op_a = objects::object::OperationId::new().to_string();
        let op_b = objects::object::OperationId::new().to_string();
        let req_a = sign_request(&state_id, &op_a);
        let req_b = sign_request(&state_id, &op_b);

        let svc_a = svc.clone();
        let svc_b = svc.clone();
        let (a, b) = tokio::join!(
            svc_a.sign_state(Request::new(req_a)),
            svc_b.sign_state(Request::new(req_b)),
        );
        a.expect("first sign_state");
        b.expect("second sign_state");

        let listing = svc
            .list_signatures(Request::new(ListSignaturesRequest {
                repo_path: String::new(),
                state_id: state_id.as_bytes().to_vec(),
            }))
            .await
            .expect("list_signatures");
        assert_eq!(
            listing.get_ref().signatures.len(),
            2,
            "both concurrent signatures must land — neither should be lost \
             to a stale-blob clobber"
        );
    }

    /// Regression: `get_review_payload` previously returned
    /// `summary.files_changed = 0` and empty `in_budget_signals` for
    /// every state, even when the diff against the parent had real
    /// content. This test snapshots once (root state — every file is
    /// "added"), then snapshots again with a modification, and asserts
    /// both states report a non-empty diff summary plus a populated
    /// `diff_summary` signal anchored on the changed file.
    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn get_review_payload_populates_diff_summary_and_signals() {
        let (svc, repo, _tmp) = fresh_service();

        // First capture: root state, one file added. Every line of
        // `hello.txt` should count as added.
        std::fs::write(repo.root().join("hello.txt"), b"first\nsecond\nthird\n")
            .expect("write hello.txt");
        let first = repo
            .snapshot(Some("first capture".to_string()), None)
            .expect("first snapshot");

        let resp_first = svc
            .get_review_payload(Request::new(GetReviewPayloadRequest {
                repo_path: String::new(),
                state_id: first.change_id.as_bytes().to_vec(),
                include_all_signals: false,
            }))
            .await
            .expect("get_review_payload first");
        let payload_first = resp_first.into_inner();
        let summary_first = payload_first.summary.as_ref().expect("summary present");
        assert!(
            summary_first.files_changed >= 1,
            "first state should report at least one file changed (vs empty parent), got {}",
            summary_first.files_changed
        );
        assert!(
            summary_first.added_lines >= 3,
            "first state should report 3+ added lines, got {}",
            summary_first.added_lines
        );
        assert!(
            !payload_first.in_budget_signals.is_empty(),
            "in_budget_signals must include a diff_summary entry"
        );
        let first_signal = &payload_first.in_budget_signals[0];
        assert!(
            first_signal.kind.starts_with("diff_summary."),
            "expected synthetic diff_summary signal kind, got {}",
            first_signal.kind
        );
        assert_eq!(first_signal.producer_module, "review_show.diff_summary");
        assert_eq!(first_signal.visibility, "visible");

        // Second capture: modify the file. Diff vs the first state's
        // tree should report a single modified file with at least one
        // added and one removed line.
        std::fs::write(
            repo.root().join("hello.txt"),
            b"first\nsecond\nthird\nfourth\n",
        )
        .expect("modify hello.txt");
        let second = repo
            .snapshot(Some("second capture".to_string()), None)
            .expect("second snapshot");

        let resp_second = svc
            .get_review_payload(Request::new(GetReviewPayloadRequest {
                repo_path: String::new(),
                state_id: second.change_id.as_bytes().to_vec(),
                include_all_signals: false,
            }))
            .await
            .expect("get_review_payload second");
        let payload_second = resp_second.into_inner();
        let summary_second = payload_second.summary.as_ref().expect("summary present");
        assert_eq!(
            summary_second.files_changed, 1,
            "second state should report exactly one modified file"
        );
        assert!(
            summary_second.added_lines >= 1,
            "second state should report at least one added line, got {}",
            summary_second.added_lines
        );
        assert!(
            !payload_second.in_budget_signals.is_empty(),
            "in_budget_signals must include a diff_summary entry"
        );
        let signal = &payload_second.in_budget_signals[0];
        assert_eq!(
            signal
                .anchor
                .as_ref()
                .map(|a| a.file.as_str())
                .unwrap_or(""),
            "hello.txt",
            "diff_summary signal should anchor on the modified file"
        );
        assert!(
            signal.reason.contains("files changed"),
            "first signal reason should carry the aggregate summary, got {}",
            signal.reason
        );
        // The summary's signal-count fields should reflect the visible
        // budget so consumers can short-circuit on empty-vs-populated
        // without re-counting the array.
        assert_eq!(
            summary_second.in_budget_signal_count,
            payload_second.in_budget_signals.len() as u32,
            "in_budget_signal_count must match the array length"
        );
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn get_review_payload_surfaces_gitlink_target_changes() {
        let (svc, repo, _tmp) = fresh_service();

        let old_target = "0303030303030303030303030303030303030303"
            .parse()
            .expect("old git oid");
        let new_target = "0404040404040404040404040404040404040404"
            .parse()
            .expect("new git oid");
        let old_tree = objects::object::Tree::from_entries(vec![
            objects::object::TreeEntry::gitlink("vendor", old_target).expect("old gitlink"),
        ]);
        let new_tree = objects::object::Tree::from_entries(vec![
            objects::object::TreeEntry::gitlink("vendor", new_target).expect("new gitlink"),
        ]);
        let old_tree_hash = repo.store().put_tree(&old_tree).expect("put old tree");
        let new_tree_hash = repo.store().put_tree(&new_tree).expect("put new tree");
        let base = State::new_snapshot(
            old_tree_hash,
            Vec::new(),
            objects::object::Attribution::human(objects::object::Principal::new(
                "Gitlink Reviewer",
                "gitlink@example.test",
            )),
        );
        let base_id = base.change_id;
        repo.store().put_state(&base).expect("put base state");
        let changed = State::new_snapshot(
            new_tree_hash,
            vec![base_id],
            objects::object::Attribution::human(objects::object::Principal::new(
                "Gitlink Reviewer",
                "gitlink@example.test",
            )),
        );
        let changed_id = changed.change_id;
        repo.store().put_state(&changed).expect("put changed state");

        let resp = svc
            .get_review_payload(Request::new(GetReviewPayloadRequest {
                repo_path: String::new(),
                state_id: changed_id.as_bytes().to_vec(),
                include_all_signals: false,
            }))
            .await
            .expect("get_review_payload gitlink change");
        let payload = resp.into_inner();
        let summary = payload.summary.as_ref().expect("summary present");
        assert_eq!(
            summary.files_changed, 1,
            "gitlink pointer change should count as one changed path"
        );
        assert_eq!(summary.added_lines, 0);
        assert_eq!(summary.removed_lines, 0);
        assert_eq!(
            payload
                .in_budget_signals
                .first()
                .and_then(|signal| signal.anchor.as_ref())
                .map(|anchor| anchor.file.as_str()),
            Some("vendor"),
            "diff_summary signal should be anchored on the gitlink path"
        );
        let partition = payload.partition.expect("partition present");
        let surfaced = partition
            .structural
            .iter()
            .chain(partition.consequence.iter())
            .chain(partition.tests_and_docs.iter())
            .any(|symbol| symbol.file == "vendor" && symbol.symbol == "vendor");
        assert!(surfaced, "gitlink change should be path-visible in review");
    }

    /// Regression for codex feedback on PRs #52 (tree fallback) and
    /// #56 (blob fallback): `compute_state_diff_summary` must tolerate
    /// missing trees and blobs by returning an empty/partial summary
    /// rather than blocking the entire review payload. Construct a
    /// state whose `tree` points to a content hash that was never
    /// stored — `get_tree` returns `Ok(None)`, and the function must
    /// fall back to an empty changeset instead of erroring out of
    /// `diff_trees`. (Pre-fix, the Modified branch already tolerated
    /// missing blobs via `.ok().flatten()`, but the Added/Deleted
    /// branches and the top-level diff_trees call did not — surfaces
    /// of pruned object stores all errored out inconsistently.)
    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn get_review_payload_tolerates_missing_tree() {
        let (svc, repo, _tmp) = fresh_service();
        let state_id = capture_state(&repo);

        // Load the state, swap its tree pointer to a synthetic hash
        // that nothing references, and re-persist. The state object
        // itself survives, but `repo.store().get_tree(state.tree)`
        // will return `Ok(None)` — the missing-tree case.
        let state = repo
            .store()
            .get_state(&state_id)
            .expect("get state")
            .expect("state present");
        let bogus_tree = objects::object::ContentHash::compute(b"definitely-not-in-store-bytes");
        let mut mutated = state.clone();
        mutated.tree = bogus_tree;
        repo.store().put_state(&mutated).expect("put mutated state");

        // The review payload must still come back — empty summary,
        // but no Status::Internal error from the gRPC layer.
        let resp = svc
            .get_review_payload(Request::new(GetReviewPayloadRequest {
                repo_path: String::new(),
                state_id: state_id.as_bytes().to_vec(),
                include_all_signals: false,
            }))
            .await
            .expect("missing tree must not block review payload");
        let payload = resp.into_inner();
        let summary = payload.summary.as_ref().expect("summary present");
        assert_eq!(
            summary.files_changed, 0,
            "missing tree must produce a zero-change summary, got {} files",
            summary.files_changed
        );
        // Synthetic diff_summary signal should still be present (with
        // the `empty` kind) so consumers always see at least one
        // signal — keeps the wire shape stable.
        assert!(
            !payload.in_budget_signals.is_empty(),
            "in_budget_signals should always contain at least the synthetic diff_summary entry"
        );
        let kind = &payload.in_budget_signals[0].kind;
        assert!(
            kind.starts_with("diff_summary."),
            "expected synthetic diff_summary signal, got {kind}"
        );
    }

    /// `line_count` should match git-style line counts — trailing
    /// newline never produces a phantom empty line, but an unterminated
    /// final line still counts.
    #[test]
    fn line_count_matches_git_semantics() {
        assert_eq!(line_count(b""), 0);
        assert_eq!(line_count(b"\n"), 1);
        assert_eq!(line_count(b"hello"), 1);
        assert_eq!(line_count(b"hello\n"), 1);
        assert_eq!(line_count(b"hello\nworld"), 2);
        assert_eq!(line_count(b"hello\nworld\n"), 2);
        assert_eq!(line_count(b"a\nb\nc\n"), 3);
        // Non-utf8 bytes count as zero (treated as binary).
        assert_eq!(line_count(&[0xff, 0xfe, 0xfd]), 0);
    }
}
