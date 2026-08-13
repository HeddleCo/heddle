// SPDX-License-Identifier: Apache-2.0
//! Heddle-owned local state-review operations.
//!
//! Reads and writes review-signature attachments. Verifies the client-supplied
//! signature against the deterministic [`signing_payload`] before persisting.

// `::state_review` disambiguates from this module's own name
// (`local_review::state_review`), the same way the hosted impl
// disambiguates by being in a sibling module.
use ::state_review::{
    PathSymbol, ReadingOrderPartition, SymbolKind, payload::build_review_payload_partition_owned,
};
use crypto::verify_payload_signature;
use objects::{
    lock::RepositoryLockExt,
    object::{
        Blob, DiffKind, Discussion, DiscussionsBlob, ProducerId, ReviewKind, ReviewScope,
        ReviewSignature, ReviewSignaturesBlob, RiskSignalBlob, RiskSignalKind, SignalAnchor, State,
        StateAttachment, StateAttachmentBody, StateId, signing_payload,
    },
    store::ObjectStore,
    worktree::diff_blobs,
};
use repo::{Repository, StateAttachmentKind};
use serde::{Deserialize, Serialize};

use super::{LocalReviewContext, LocalReviewError, map_repository_error, with_idempotency};

/// Maximum drift (seconds) between the client's `signed_at_unix` and the
/// local wall clock. Generous enough to absorb NTP skew, narrow enough
/// to bound the window for replay-style attacks.
const SIGN_TIMESTAMP_SKEW_SECS: i64 = 5 * 60;

/// Collision-safe marker reserved for verdict metadata persisted in the
/// review-signature justification field by the hosted review contract.
const VERDICT_ENVELOPE_TAG: &str = "\u{1}hd-verdict-v1\u{1}";

/// Idempotency namespace for the repository-local JSON response codec.
///
/// The earlier implementation reused the hosted RPC verb while caching
/// Prost bytes. Keeping the codec generation in the verb makes that old data
/// a cross-verb conflict instead of attempting to decode it as JSON. Dedup
/// entries expire after seven days, so a new operation id is the clean-cut
/// migration path.
const LOCAL_SIGN_REPLAY_VERB: &str = "local.state_review.sign/json-v1";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewSummary {
    pub headline: String,
    pub files_changed: u32,
    pub added_lines: u32,
    pub removed_lines: u32,
    pub in_budget_signal_count: u32,
    pub hidden_signal_count: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewSignalKind {
    DiffSummary,
    Risk(RiskSignalKind),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewSignalVisibility {
    Visible,
    Hidden,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewSignal {
    pub kind: ReviewSignalKind,
    pub anchor: SignalAnchor,
    pub reason: String,
    pub producer: ProducerId,
    pub computed_at: Option<i64>,
    pub visibility: ReviewSignalVisibility,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewPayload {
    pub state_id: StateId,
    pub summary: ReviewSummary,
    pub agent_narrative: Option<String>,
    pub partition: ReadingOrderPartition,
    pub in_budget_signals: Vec<ReviewSignal>,
    pub all_signals: Vec<ReviewSignal>,
    pub tick_budget: u32,
    pub discussions: Vec<Discussion>,
    pub signing_kinds: Vec<ReviewKind>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredReviewSignature {
    pub id: String,
    pub signature: ReviewSignature,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignReviewRequest {
    pub state_id: StateId,
    pub kind: ReviewKind,
    pub scope: ReviewScope,
    pub justification: Option<String>,
    pub algorithm: String,
    pub public_key: Vec<u8>,
    pub signature: Vec<u8>,
    pub signed_at: i64,
    pub client_operation_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignReviewResult {
    pub signature_id: String,
    pub state_id: StateId,
}

/// Deep local review module used in-process by native CLI commands.
#[derive(Clone)]
pub struct LocalStateReview {
    inner: LocalReviewContext,
}

impl LocalStateReview {
    pub fn new(inner: LocalReviewContext) -> Self {
        Self { inner }
    }

    pub fn get_review_payload(
        &self,
        state_id: StateId,
        include_all_signals: bool,
    ) -> Result<ReviewPayload, LocalReviewError> {
        let repo = self.inner.repo();
        let state = repo
            .store()
            .get_state(&state_id)
            .map_err(map_repository_error)?
            .ok_or_else(|| {
                LocalReviewError::not_found(format!(
                    "state {} not found",
                    state_id.to_string_full()
                ))
            })?;

        // Diff the state's tree against its first parent so the summary
        // counts reflect what actually changed in this state. The
        // signal registry / budgeter will eventually layer on top of
        // this; until then `files_changed` is the most useful single
        // number an agent can use for self-review.
        let diff_summary =
            compute_state_diff_summary(repo, &state).map_err(map_repository_error)?;

        let summary = ReviewSummary {
            headline: state.intent.clone().unwrap_or_default(),
            files_changed: diff_summary.files_changed,
            added_lines: diff_summary.added_lines,
            removed_lines: diff_summary.removed_lines,
            in_budget_signal_count: 0,
            hidden_signal_count: 0,
        };

        let agent_narrative = if state.attribution.agent.is_some() {
            state.intent.clone()
        } else {
            None
        };

        // Surface fired risk signals if requested. The signal registry will
        // Trimmed budget split: every signal is visible. The former ranked
        // `state_review::budget` helper was unused and removed (HEDDLE-DR-10);
        // reintroduce a ranked partition here if tick budgeting ships.
        let mut all_signals = Vec::new();
        if include_all_signals
            && let Some(hash) =
                attachment_hash(repo, &state.state_id, StateAttachmentKind::RiskSignals)?
            && let Some(blob) = repo.store().get_blob(&hash).map_err(map_repository_error)?
        {
            let decoded = RiskSignalBlob::decode(blob.content())
                .map_err(|err| LocalReviewError::internal(format!("decode risk signals: {err}")))?;
            all_signals = decoded
                .signals
                .into_iter()
                .map(|signal| review_signal(signal, ReviewSignalVisibility::Visible))
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
        let mut in_budget_signals = Vec::new();
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
            in_budget_signals.push(ReviewSignal {
                kind: ReviewSignalKind::DiffSummary,
                anchor: SignalAnchor {
                    file: String::new(),
                    symbol: None,
                    line_range: None,
                },
                reason: summary_reason.clone(),
                producer: ProducerId::new("review_show.diff_summary", 1),
                computed_at: None,
                visibility: ReviewSignalVisibility::Visible,
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
                in_budget_signals.push(ReviewSignal {
                    kind: ReviewSignalKind::DiffSummary,
                    anchor: SignalAnchor {
                        file: path_kind.path.clone(),
                        symbol: None,
                        line_range: None,
                    },
                    reason,
                    producer: ProducerId::new("review_show.diff_summary", 1),
                    computed_at: None,
                    visibility: ReviewSignalVisibility::Visible,
                });
            }
        }

        // Build the reading-order partition from the same domain symbols
        // used at the hosted boundary: tree-sitter when the
        // `semantic` feature is enabled, path-only fallback otherwise.
        let symbols = changed_files_as_symbols(repo, &state, &diff_summary.changed_paths)
            .map_err(map_repository_error)?;
        let partition = build_review_payload_partition_owned(symbols);

        // Decode the state's durable discussions attachment when present.
        let discussions =
            match attachment_hash(repo, &state.state_id, StateAttachmentKind::Discussions)? {
                Some(hash) => {
                    let blob = repo
                        .store()
                        .get_blob(&hash)
                        .map_err(map_repository_error)?
                        .ok_or_else(|| {
                            LocalReviewError::internal(format!(
                                "discussions blob {} referenced by state {} is missing",
                                hash,
                                state.state_id.to_string_full()
                            ))
                        })?;
                    let decoded = DiscussionsBlob::decode(blob.content()).map_err(|err| {
                        LocalReviewError::internal(format!("decode discussions: {err}"))
                    })?;
                    decoded.discussions
                }
                None => Vec::new(),
            };

        let mut summary = summary;
        summary.in_budget_signal_count = in_budget_signals.len() as u32;
        summary.hidden_signal_count =
            all_signals.len().saturating_sub(in_budget_signals.len()) as u32;

        let payload = ReviewPayload {
            state_id,
            summary,
            agent_narrative,
            partition,
            in_budget_signals,
            all_signals,
            tick_budget: 3,
            discussions,
            signing_kinds: vec![
                ReviewKind::Read,
                ReviewKind::AgentPreview,
                ReviewKind::AgentCoReview,
            ],
        };

        Ok(payload)
    }

    pub async fn sign_state(
        &self,
        req: SignReviewRequest,
    ) -> Result<SignReviewResult, LocalReviewError> {
        let req_bytes = serde_json::to_vec(&req)
            .map_err(|error| LocalReviewError::internal(format!("encode sign request: {error}")))?;
        let client_operation_id = req.client_operation_id.clone();
        let inner = self.inner.clone();

        let response = with_idempotency(
            &self.inner,
            &client_operation_id,
            LOCAL_SIGN_REPLAY_VERB,
            &req_bytes,
            move || {
                let inner = inner.clone();
                async move { execute_sign_state(&inner, req).await }
            },
        )
        .await?;

        Ok(response)
    }

    pub fn list_signatures(
        &self,
        state_id: StateId,
    ) -> Result<Vec<StoredReviewSignature>, LocalReviewError> {
        let repo = self.inner.repo();
        let state = repo
            .store()
            .get_state(&state_id)
            .map_err(map_repository_error)?
            .ok_or_else(|| {
                LocalReviewError::not_found(format!(
                    "state {} not found",
                    state_id.to_string_full()
                ))
            })?;

        let signatures =
            match attachment_hash(repo, &state.state_id, StateAttachmentKind::ReviewSignatures)? {
                Some(hash) => {
                    let blob = repo
                        .store()
                        .get_blob(&hash)
                        .map_err(map_repository_error)?
                        .ok_or_else(|| {
                            LocalReviewError::internal(format!(
                                "review signatures blob {} missing from object store",
                                hash
                            ))
                        })?;
                    let decoded = ReviewSignaturesBlob::decode(blob.content()).map_err(|err| {
                        LocalReviewError::internal(format!("decode review signatures: {err}"))
                    })?;
                    decoded
                        .signatures
                        .into_iter()
                        .enumerate()
                        .map(|(idx, sig)| StoredReviewSignature {
                            id: synthetic_signature_id(idx),
                            signature: sig,
                        })
                        .collect()
                }
                None => Vec::new(),
            };

        Ok(signatures)
    }
}

/// Body of [`LocalStateReview::sign_state`]. Lifted out of the public method
/// method so [`with_idempotency`] can re-execute it inside its closure.
async fn execute_sign_state(
    inner: &LocalReviewContext,
    req: SignReviewRequest,
) -> Result<SignReviewResult, LocalReviewError> {
    let state_id = req.state_id;
    let repo = inner.repo();

    // Build the ReviewSignature, then verify the client-supplied
    // signature is well-formed and matches the deterministic signing
    // payload. A malformed or forged signature must never reach the
    // persisted blob. Attribute the signature to the local-mode
    // caller (`Repository::get_principal` resolves env vars then
    // `[principal]` in `.heddle/config.toml`), not the state's author
    // — Bob signing Alice's state should record Bob.
    let actor = repo
        .get_principal()
        .map_err(|err| LocalReviewError::internal(format!("resolve caller principal: {err}")))?;
    if req
        .justification
        .as_deref()
        .is_some_and(|text| text.starts_with(VERDICT_ENVELOPE_TAG))
    {
        return Err(LocalReviewError::invalid_argument(
            "justification must not begin with the reserved verdict-envelope prefix",
        ));
    }
    let justification = req.justification.clone().filter(|text| !text.is_empty());

    let now = chrono::Utc::now().timestamp();
    let signed_at = req.signed_at;
    if signed_at == 0 {
        return Err(LocalReviewError::invalid_argument(
            "signed_at is required and must match the timestamp the client signed over",
        ));
    }
    if (signed_at - now).abs() > SIGN_TIMESTAMP_SKEW_SECS {
        return Err(LocalReviewError::invalid_argument(format!(
            "signed_at={signed_at} is too far from server time={now} (max skew {SIGN_TIMESTAMP_SKEW_SECS}s)"
        )));
    }

    let new_sig = ReviewSignature {
        actor,
        kind: req.kind,
        scope: req.scope.clone(),
        justification: justification.clone(),
        signed_at,
        algorithm: req.algorithm.clone(),
        public_key: hex::encode(&req.public_key),
        signature: hex::encode(&req.signature),
    };
    new_sig.validate().map_err(|err| {
        LocalReviewError::invalid_argument(format!("invalid review signature: {err}"))
    })?;

    let public_key_bytes = req.public_key.clone();
    let signature_bytes = req.signature.clone();
    let payload = signing_payload(
        state_id,
        req.kind,
        &req.scope,
        signed_at,
        justification.as_deref(),
    );
    verify_payload_signature(
        &payload,
        &req.algorithm,
        &public_key_bytes,
        &signature_bytes,
    )
    .map_err(|err| {
        LocalReviewError::invalid_argument(format!(
            "review signature failed verification ({}): {err}",
            req.algorithm
        ))
    })?;

    let new_index = append_review_signature(repo, state_id, new_sig)?;

    Ok(SignReviewResult {
        signature_id: synthetic_signature_id(new_index),
        state_id,
    })
}

/// Append one signed review record while holding the repository write lock.
fn append_review_signature(
    repo: &Repository,
    state_id: StateId,
    signature: ReviewSignature,
) -> Result<usize, LocalReviewError> {
    let _lock = repo
        .locker()
        .write()
        .map_err(|err| LocalReviewError::internal(err.to_string()))?;
    repo.store()
        .get_state(&state_id)
        .map_err(map_repository_error)?
        .ok_or_else(|| {
            LocalReviewError::not_found(format!("state {} not found", state_id.to_string_full()))
        })?;
    let prior = repo
        .latest_state_attachment(&state_id, StateAttachmentKind::ReviewSignatures)
        .map_err(map_repository_error)?;
    let mut blob = match prior.as_ref().map(|attachment| {
        let StateAttachmentBody::ReviewSignatures(hash) = &attachment.body else {
            unreachable!()
        };
        *hash
    }) {
        Some(hash) => {
            let raw = repo
                .store()
                .get_blob(&hash)
                .map_err(map_repository_error)?
                .ok_or_else(|| {
                    LocalReviewError::internal(format!(
                        "existing review signatures blob {} missing from object store",
                        hash
                    ))
                })?;
            ReviewSignaturesBlob::decode(raw.content()).map_err(|err| {
                LocalReviewError::internal(format!("decode review signatures: {err}"))
            })?
        }
        None => ReviewSignaturesBlob::new(Vec::new()),
    };
    blob.signatures.push(signature);
    let new_index = blob.signatures.len() - 1;

    let bytes = blob
        .encode()
        .map_err(|err| LocalReviewError::internal(format!("encode review signatures: {err}")))?;
    let content_hash = repo
        .store()
        .put_blob(&Blob::new(bytes))
        .map_err(map_repository_error)?;

    let attachment = StateAttachment {
        state_id,
        body: StateAttachmentBody::ReviewSignatures(content_hash),
        attribution: repo.get_attribution().map_err(map_repository_error)?,
        created_at: chrono::Utc::now(),
        supersedes: prior.map(|attachment| attachment.id()),
    };
    repo.put_state_attachment(&attachment)
        .map_err(map_repository_error)?;
    Ok(new_index)
}

/// `ReviewSignature` doesn't carry an explicit id; we synthesise one from
/// the per-state index so local output has stable signature ids within a
/// single state. (A future schema bump may add an explicit id.)
fn synthetic_signature_id(index: usize) -> String {
    format!("rs-{index}")
}

fn attachment_hash(
    repo: &Repository,
    state_id: &StateId,
    kind: StateAttachmentKind,
) -> Result<Option<objects::object::ContentHash>, LocalReviewError> {
    let Some(attachment) = repo
        .latest_state_attachment(state_id, kind)
        .map_err(map_repository_error)?
    else {
        return Ok(None);
    };
    let hash = match attachment.body {
        StateAttachmentBody::RiskSignals(hash)
        | StateAttachmentBody::ReviewSignatures(hash)
        | StateAttachmentBody::Discussions(hash) => hash,
        _ => unreachable!(),
    };
    Ok(Some(hash))
}

fn review_signal(
    signal: objects::object::RiskSignal,
    visibility: ReviewSignalVisibility,
) -> ReviewSignal {
    ReviewSignal {
        kind: ReviewSignalKind::Risk(signal.kind),
        anchor: signal.anchor,
        reason: signal.reason,
        producer: signal.producer,
        computed_at: Some(signal.computed_at),
        visibility,
    }
}

// ---------------------------------------------------------------------------
// Symbol extraction for the shared review-payload domain model.
// ---------------------------------------------------------------------------

/// Symbol projection for the reading-order partition. When the `semantic`
/// feature is enabled and the
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
    use ::semantic::symbol_resolver::{Definition, extract_definitions};
    let definitions: Vec<Definition> = match extract_definitions(source, std::path::Path::new(path))
    {
        Ok(defs) => defs,
        Err(_) => return false,
    };
    if definitions.is_empty() {
        return false;
    }
    for d in definitions {
        let symbol = match d.parent_name.as_deref() {
            Some(parent) if !parent.is_empty() => format!("{parent}::{}", d.name),
            _ => d.name,
        };
        out.push(PathSymbol {
            file: path.to_string(),
            symbol,
            kind: d.kind,
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
    use repo::{Repository, operation_dedup::OperationDedupStore};
    use tempfile::TempDir;

    use super::*;

    fn fresh_review() -> (LocalStateReview, Arc<Repository>, TempDir) {
        let temp = TempDir::new().expect("create tempdir");
        // SAFETY: these serial tests own the process-global attribution.
        unsafe {
            std::env::set_var("HEDDLE_PRINCIPAL_NAME", "Alice Tester");
            std::env::set_var("HEDDLE_PRINCIPAL_EMAIL", "alice@example.com");
        }
        let repo = Repository::init_default(temp.path()).expect("init repo");
        let dedup = OperationDedupStore::open(repo.heddle_dir()).expect("open dedup");
        let repo = Arc::new(repo);
        let review =
            LocalStateReview::new(LocalReviewContext::new(Arc::clone(&repo), Arc::new(dedup)));
        (review, repo, temp)
    }

    fn capture_state(repo: &Repository, content: &[u8]) -> StateId {
        std::fs::write(repo.root().join("hello.txt"), content).expect("write file");
        repo.snapshot(Some("seed".to_string()), None)
            .expect("snapshot")
            .state_id
    }

    fn sign_request(state_id: StateId, operation_id: impl Into<String>) -> SignReviewRequest {
        let signer = crypto::Ed25519Signer::generate().expect("generate ed25519 key");
        let scope = ReviewScope::WholeChange;
        let signed_at = chrono::Utc::now().timestamp();
        let payload = signing_payload(state_id, ReviewKind::Read, &scope, signed_at, None);
        SignReviewRequest {
            state_id,
            kind: ReviewKind::Read,
            scope,
            justification: None,
            algorithm: "ed25519".to_string(),
            public_key: signer.public_key().to_vec(),
            signature: signer.sign(&payload).expect("sign payload"),
            signed_at,
            client_operation_id: operation_id.into(),
        }
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn local_interface_signs_lists_and_replays_without_protocol_types() {
        let (review, repo, _temp) = fresh_review();
        let state_id = capture_state(&repo, b"hello\n");
        let operation_id = objects::object::OperationId::new().to_string();
        let request = sign_request(state_id, operation_id);

        let first = review.sign_state(request.clone()).await.expect("sign");
        let replay = review.sign_state(request).await.expect("replay");

        assert_eq!(first, replay);
        assert_eq!(first.state_id, state_id);
        let signatures = review.list_signatures(state_id).expect("signatures");
        assert_eq!(signatures.len(), 1, "replay must not append");
        assert_eq!(signatures[0].id, "rs-0");
        assert_eq!(signatures[0].signature.kind, ReviewKind::Read);
        assert_eq!(signatures[0].signature.scope, ReviewScope::WholeChange);
        assert_eq!(signatures[0].signature.actor.name, b"Alice Tester");
        assert_eq!(signatures[0].signature.actor.email, b"alice@example.com");
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn legacy_prost_replay_is_a_controlled_operation_id_conflict() {
        use repo::operation_dedup::hash_request_body;

        let (review, repo, _temp) = fresh_review();
        let state_id = capture_state(&repo, b"hello\n");
        let operation_id = objects::object::OperationId::new();
        let request = sign_request(state_id, operation_id.to_string());
        let request_bytes = serde_json::to_vec(&request).expect("encode request");

        // Simulate a response cached by the retired hosted/Prost-backed
        // implementation. The bytes intentionally are not valid JSON.
        review
            .inner
            .dedup
            .record(
                operation_id,
                "state_review.sign_state",
                hash_request_body(&request_bytes),
                vec![0x0a, 0x03, b'o', b'l', b'd'],
            )
            .expect("record legacy replay");

        let error = review
            .sign_state(request)
            .await
            .expect_err("legacy replay must not be decoded as local JSON");

        assert_eq!(
            error.code(),
            crate::local_review::LocalReviewCode::FailedPrecondition
        );
        assert!(
            error
                .message()
                .contains("different operation or replay encoding")
        );
        assert!(
            review
                .list_signatures(state_id)
                .expect("signatures")
                .is_empty()
        );
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn local_interface_rejects_a_forged_signature() {
        let (review, repo, _temp) = fresh_review();
        let state_id = capture_state(&repo, b"hello\n");
        let mut request = sign_request(state_id, "");
        let last = request.signature.len() - 1;
        request.signature[last] ^= 0xff;

        let error = review
            .sign_state(request)
            .await
            .expect_err("forgery must fail");

        assert_eq!(
            error.code(),
            crate::local_review::LocalReviewCode::InvalidArgument
        );
        assert!(error.message().contains("failed verification"));
        assert!(
            review
                .list_signatures(state_id)
                .expect("signatures")
                .is_empty()
        );
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn local_interface_rejects_the_reserved_verdict_envelope_prefix() {
        let (review, repo, _temp) = fresh_review();
        let state_id = capture_state(&repo, b"hello\n");
        let mut request = sign_request(state_id, "");
        request.justification = Some(format!("{VERDICT_ENVELOPE_TAG}{{\"verdict\":\"hold\"}}"));

        let error = review
            .sign_state(request)
            .await
            .expect_err("reserved verdict envelope must fail");

        assert_eq!(
            error.code(),
            crate::local_review::LocalReviewCode::InvalidArgument
        );
        assert!(error.message().contains("reserved verdict-envelope prefix"));
        assert!(
            review
                .list_signatures(state_id)
                .expect("signatures")
                .is_empty()
        );
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn local_interface_rejects_a_skewed_timestamp() {
        let (review, repo, _temp) = fresh_review();
        let state_id = capture_state(&repo, b"hello\n");
        let mut request = sign_request(state_id, "");
        request.signed_at += 60 * 60;

        let error = review
            .sign_state(request)
            .await
            .expect_err("skewed timestamp must fail");

        assert_eq!(
            error.code(),
            crate::local_review::LocalReviewCode::InvalidArgument
        );
        assert!(error.message().contains("too far from server time"));
        assert!(
            review
                .list_signatures(state_id)
                .expect("signatures")
                .is_empty()
        );
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn local_interface_attributes_the_signature_to_the_current_principal() {
        let (review, repo, _temp) = fresh_review();
        let state_id = capture_state(&repo, b"hello\n");

        // SAFETY: this serial test owns the process-global attribution.
        unsafe {
            std::env::set_var("HEDDLE_PRINCIPAL_NAME", "Bob Signer");
            std::env::set_var("HEDDLE_PRINCIPAL_EMAIL", "bob@example.com");
        }
        review
            .sign_state(sign_request(state_id, ""))
            .await
            .expect("sign as Bob");

        let signature = review
            .list_signatures(state_id)
            .expect("signatures")
            .pop()
            .expect("signature")
            .signature;
        assert_eq!(signature.actor.name, b"Bob Signer");
        assert_eq!(signature.actor.email, b"bob@example.com");
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn local_interface_serializes_concurrent_signature_appends() {
        let (review, repo, _temp) = fresh_review();
        let state_id = capture_state(&repo, b"hello\n");
        let request_a = sign_request(state_id, objects::object::OperationId::new().to_string());
        let request_b = sign_request(state_id, objects::object::OperationId::new().to_string());

        let first_review = review.clone();
        let second_review = review.clone();
        let (a, b) = tokio::join!(
            first_review.sign_state(request_a),
            second_review.sign_state(request_b)
        );
        a.expect("first sign");
        b.expect("second sign");

        assert_eq!(
            review.list_signatures(state_id).expect("signatures").len(),
            2
        );
    }

    #[test]
    #[serial_test::serial(process_global)]
    fn local_payload_exposes_domain_summary_signals_and_reading_order() {
        let (review, repo, _temp) = fresh_review();
        let state_id = capture_state(&repo, b"first\nsecond\nthird\n");

        let payload = review.get_review_payload(state_id, false).expect("payload");

        assert_eq!(payload.state_id, state_id);
        assert!(payload.summary.files_changed >= 1);
        assert!(payload.summary.added_lines >= 3);
        assert_eq!(
            payload.summary.in_budget_signal_count,
            payload.in_budget_signals.len() as u32
        );
        let signal = payload.in_budget_signals.first().expect("diff signal");
        assert_eq!(signal.kind, ReviewSignalKind::DiffSummary);
        assert_eq!(signal.producer.module, "review_show.diff_summary");
        assert_eq!(signal.visibility, ReviewSignalVisibility::Visible);
        assert_eq!(signal.anchor.file, "hello.txt");
        let surfaced = payload
            .partition
            .structural
            .iter()
            .chain(payload.partition.consequence.iter())
            .chain(payload.partition.tests_and_docs.iter())
            .any(|symbol| symbol.file == "hello.txt");
        assert!(surfaced, "changed path must appear in reading order");

        std::fs::write(
            repo.root().join("hello.txt"),
            b"first\nsecond changed\nthird\nfourth\n",
        )
        .expect("modify file");
        let modified_state = repo
            .snapshot(Some("modify".to_string()), None)
            .expect("snapshot modification")
            .state_id;
        let modified = review
            .get_review_payload(modified_state, false)
            .expect("payload");
        assert_eq!(modified.summary.files_changed, 1);
        assert!(modified.summary.added_lines >= 1);
        assert!(modified.summary.removed_lines >= 1);
        assert_eq!(
            modified.in_budget_signals[0].anchor.file, "hello.txt",
            "the aggregate signal must anchor on the changed file"
        );
        assert!(
            modified.in_budget_signals[0]
                .reason
                .contains("files changed")
        );
    }

    #[test]
    #[serial_test::serial(process_global)]
    fn local_payload_surfaces_gitlink_target_changes() {
        let (review, repo, _temp) = fresh_review();
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
        let attribution = objects::object::Attribution::human(objects::object::Principal::new(
            "Gitlink Reviewer",
            "gitlink@example.test",
        ));
        let base = State::new_snapshot(old_tree_hash, Vec::new(), attribution.clone());
        repo.store().put_state(&base).expect("put base state");
        let changed = State::new_snapshot(new_tree_hash, vec![base.state_id], attribution);
        repo.store().put_state(&changed).expect("put changed state");

        let payload = review
            .get_review_payload(changed.state_id, false)
            .expect("payload");

        assert_eq!(payload.summary.files_changed, 1);
        assert_eq!(payload.summary.added_lines, 0);
        assert_eq!(payload.summary.removed_lines, 0);
        assert_eq!(payload.in_budget_signals[0].anchor.file, "vendor");
        let surfaced = payload
            .partition
            .structural
            .iter()
            .chain(payload.partition.consequence.iter())
            .chain(payload.partition.tests_and_docs.iter())
            .any(|symbol| symbol.file == "vendor" && symbol.symbol == "vendor");
        assert!(surfaced, "gitlink change must remain path-visible");
    }

    #[test]
    #[serial_test::serial(process_global)]
    fn local_payload_tolerates_a_missing_tree() {
        let (review, repo, _temp) = fresh_review();
        let state_id = capture_state(&repo, b"hello\n");
        let mut state = repo
            .store()
            .get_state(&state_id)
            .expect("get state")
            .expect("state");
        state.tree = objects::object::ContentHash::compute(b"missing-tree");
        let missing_tree_state_id = state.id();
        repo.store().put_state(&state).expect("put mutated state");

        let payload = review
            .get_review_payload(missing_tree_state_id, false)
            .expect("missing tree must not block the review payload");

        assert_eq!(payload.summary.files_changed, 0);
        assert_eq!(payload.in_budget_signals.len(), 1);
        assert_eq!(
            payload.in_budget_signals[0].kind,
            ReviewSignalKind::DiffSummary
        );
        assert_eq!(
            payload.in_budget_signals[0].producer.module,
            "review_show.diff_summary"
        );
    }

    #[test]
    fn line_count_matches_git_semantics() {
        assert_eq!(line_count(b""), 0);
        assert_eq!(line_count(b"\n"), 1);
        assert_eq!(line_count(b"hello"), 1);
        assert_eq!(line_count(b"hello\n"), 1);
        assert_eq!(line_count(b"hello\nworld"), 2);
        assert_eq!(line_count(b"hello\nworld\n"), 2);
        assert_eq!(line_count(&[0xff, 0xfe, 0xfd]), 0);
    }
}
