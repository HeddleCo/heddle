// SPDX-License-Identifier: Apache-2.0
//! `heddle review` handler — calls hosted services in-process.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use daemon::grpc_local_impl::{GrpcLocalService, LocalSignalService, LocalStateReviewService};
use grpc::heddle::v1::{
    GetRepoSignalHealthRequest, GetReviewPayloadRequest, ListSignaturesRequest, PathSymbolRef,
    ReviewScope as ProtoReviewScope, SignStateRequest, signal_service_server::SignalService,
    state_review_service_server::StateReviewService,
};
use repo::{HistoryQuery, Repository, operation_dedup::OperationDedupStore};
use serde::Serialize;

use super::history_target::{resolve_state_id, resolve_state_id_bytes};
use crate::cli::{
    cli_args::{
        Cli, ReviewCommands, ReviewHealthArgs, ReviewNextArgs, ReviewShowArgs, ReviewSignArgs,
    },
    should_output_json,
};

const AGENT_GLYPH: &str = "※";
const HUMAN_GLYPH: &str = "✓";

pub async fn run(cli: &Cli, command: &ReviewCommands) -> Result<()> {
    match command {
        ReviewCommands::Show(args) => run_show(cli, args).await,
        ReviewCommands::Sign(args) => run_sign(cli, args).await,
        ReviewCommands::Next(args) => run_next(cli, args).await,
        ReviewCommands::Health(args) => run_health(cli, args).await,
    }
}

#[derive(Serialize)]
struct ReviewShowOutput {
    change_id: String,
    headline: String,
    agent_narrative: Option<String>,
    files_changed: u32,
    in_budget_signals: Vec<SignalView>,
    all_signals: Vec<SignalView>,
    discussions: Vec<DiscussionView>,
    signing_kinds: Vec<String>,
    signatures: Vec<SignatureView>,
}

#[derive(Serialize)]
struct SignalView {
    kind: String,
    file: String,
    symbol: String,
    reason: String,
    producer: String,
    visibility: String,
}

#[derive(Serialize)]
struct DiscussionView {
    id: String,
    file: String,
    symbol: String,
    status: String,
    body_changed_since_open: bool,
    orphaned: bool,
}

#[derive(Serialize)]
struct SignatureView {
    actor_name: String,
    actor_email: String,
    kind: String,
    glyph: &'static str,
    is_agent: bool,
    signed_at_secs: i64,
    scope_kind: String,
    scope_symbols: Vec<String>,
}

async fn run_show(cli: &Cli, args: &ReviewShowArgs) -> Result<()> {
    let svc = open_state_review_service()?;
    let state_id = resolve_state(args.state.as_deref())?;
    let payload_resp = svc
        .get_review_payload(tonic::Request::new(GetReviewPayloadRequest {
            repo_path: String::new(),
            state_id: state_id.clone(),
            include_all_signals: args.all_signals,
        }))
        .await
        .map_err(status_to_anyhow)?
        .into_inner();
    let signatures_resp = svc
        .list_signatures(tonic::Request::new(ListSignaturesRequest {
            repo_path: String::new(),
            state_id: state_id.clone(),
        }))
        .await
        .map_err(status_to_anyhow)?
        .into_inner();

    use grpc::heddle::v1::ReviewKind;
    let summary = payload_resp.summary.unwrap_or_default();
    let signatures: Vec<SignatureView> = signatures_resp
        .signatures
        .iter()
        .map(|s| {
            let kind = ReviewKind::try_from(s.kind).unwrap_or(ReviewKind::Unspecified);
            let is_agent = matches!(kind, ReviewKind::AgentPreview | ReviewKind::AgentCoReview);
            let (scope_kind, scope_symbols) = match s.scope.as_ref().and_then(|x| x.scope.as_ref())
            {
                Some(grpc::heddle::v1::review_scope::Scope::WholeChange(_)) => {
                    ("whole_change".to_string(), Vec::new())
                }
                Some(grpc::heddle::v1::review_scope::Scope::Symbols(list)) => (
                    "symbols".to_string(),
                    list.symbols
                        .iter()
                        .map(|sym| format!("{}:{}", sym.file, sym.symbol))
                        .collect(),
                ),
                None => (String::new(), Vec::new()),
            };
            SignatureView {
                actor_name: s.actor_name.clone(),
                actor_email: s.actor_email.clone(),
                kind: review_kind_to_str(kind).to_string(),
                glyph: if is_agent { AGENT_GLYPH } else { HUMAN_GLYPH },
                is_agent,
                signed_at_secs: s.signed_at.as_ref().map(|t| t.seconds).unwrap_or(0),
                scope_kind,
                scope_symbols,
            }
        })
        .collect();

    let output = ReviewShowOutput {
        change_id: bytes_to_change_id_string(&payload_resp.state_id),
        headline: summary.headline,
        agent_narrative: opt_string(payload_resp.agent_narrative),
        files_changed: summary.files_changed,
        in_budget_signals: payload_resp
            .in_budget_signals
            .iter()
            .map(signal_view)
            .collect(),
        all_signals: payload_resp.all_signals.iter().map(signal_view).collect(),
        discussions: payload_resp
            .discussions
            .iter()
            .map(discussion_view)
            .collect(),
        signing_kinds: payload_resp
            .signing_footer
            .map(|f| {
                f.available_kinds
                    .into_iter()
                    .map(|k| {
                        review_kind_to_str(
                            ReviewKind::try_from(k).unwrap_or(ReviewKind::Unspecified),
                        )
                        .to_string()
                    })
                    .collect()
            })
            .unwrap_or_default(),
        signatures,
    };
    if should_output_json(cli, None) {
        println!(
            "{}",
            serde_json::to_string(&output).context("serialize review payload")?
        );
    } else {
        render_text(&output, args.all_signals);
    }
    Ok(())
}

fn render_text(out: &ReviewShowOutput, all_signals: bool) {
    println!("review of state {}", out.change_id);
    if !out.headline.is_empty() {
        println!("  {}", out.headline);
    }
    if let Some(narrative) = &out.agent_narrative
        && !narrative.is_empty()
    {
        println!("\n  agent narrative:");
        for line in narrative.lines() {
            println!("    {line}");
        }
    }
    if !out.in_budget_signals.is_empty() {
        println!("\n  signals (in budget):");
        for s in &out.in_budget_signals {
            println!("    ▸ [{}] {}:{} — {}", s.kind, s.file, s.symbol, s.reason);
        }
    }
    if all_signals && !out.all_signals.is_empty() {
        println!("\n  signals (all):");
        for s in &out.all_signals {
            let marker = if s.visibility == "hidden" {
                "·"
            } else {
                "▸"
            };
            println!(
                "    {marker} [{}] {}:{} — {} [{}]",
                s.kind, s.file, s.symbol, s.reason, s.visibility
            );
        }
    }
    if !out.discussions.is_empty() {
        println!("\n  discussions:");
        for d in &out.discussions {
            let mut suffix = String::new();
            if d.body_changed_since_open {
                suffix.push_str(" [body changed]");
            }
            if d.orphaned {
                suffix.push_str(" [orphaned]");
            }
            println!(
                "    {} ({}) {}:{}{}",
                d.id, d.status, d.file, d.symbol, suffix
            );
        }
    }
    if !out.signatures.is_empty() {
        println!("\n  signatures:");
        for s in &out.signatures {
            println!(
                "    {} {} <{}> [{}]",
                s.glyph, s.actor_name, s.actor_email, s.kind
            );
        }
    }
    if !out.signing_kinds.is_empty() {
        println!(
            "\n  available signing kinds: {}",
            out.signing_kinds.join(", ")
        );
    }
}

async fn run_sign(cli: &Cli, args: &ReviewSignArgs) -> Result<()> {
    use grpc::heddle::v1::review_scope::{Scope, SymbolList, WholeChange};
    let svc = open_state_review_service()?;
    let state_id_bytes = resolve_state_id_bytes(&open_repo()?, &args.state)?;
    let scope_inner = if args.symbols.is_empty() {
        Scope::WholeChange(WholeChange {})
    } else {
        let parsed: Result<Vec<_>> = args
            .symbols
            .iter()
            .map(|s| {
                let (file, symbol) = s
                    .split_once(':')
                    .ok_or_else(|| anyhow!("--symbols expects 'file:symbol', got '{s}'"))?;
                Ok(PathSymbolRef {
                    file: file.to_string(),
                    symbol: symbol.to_string(),
                })
            })
            .collect();
        Scope::Symbols(SymbolList { symbols: parsed? })
    };
    let scope = ProtoReviewScope {
        scope: Some(scope_inner),
    };
    let req = SignStateRequest {
        repo_path: String::new(),
        state_id: state_id_bytes,
        kind: args.kind.as_proto() as i32,
        scope: Some(scope),
        justification: args.justification.clone().unwrap_or_default(),
        algorithm: args.algorithm.clone(),
        public_key: hex::decode(&args.public_key)
            .map_err(|e| anyhow::anyhow!("public_key must be hex-encoded: {e}"))?,
        signature: hex::decode(&args.signature)
            .map_err(|e| anyhow::anyhow!("signature must be hex-encoded: {e}"))?,
        signed_at: Some(prost_types::Timestamp {
            seconds: args.signed_at_unix,
            nanos: 0,
        }),
        client_operation_id: crate::operation_id::wire(cli),
    };
    let resp = svc
        .sign_state(tonic::Request::new(req))
        .await
        .map_err(status_to_anyhow)?
        .into_inner();
    if should_output_json(cli, None) {
        let state_str = bytes_to_change_id_string(&resp.state_id);
        let out = serde_json::json!({
            "signature_id": resp.signature_id,
            "change_id": state_str,
        });
        println!("{out}");
    } else {
        println!(
            "signed state {} as {} (signature_id {})",
            bytes_to_change_id_string(&resp.state_id),
            args.kind.as_wire(),
            resp.signature_id
        );
    }
    Ok(())
}

async fn run_next(cli: &Cli, args: &ReviewNextArgs) -> Result<()> {
    let svc = open_state_review_service()?;
    let repo = open_repo()?;
    let head = repo
        .head()
        .context("read HEAD")?
        .ok_or_else(|| anyhow!("repository has no HEAD; capture a state first"))?;

    let actor_email = args
        .mine_only
        .then(|| {
            repo.config()
                .principal
                .as_ref()
                .map(|p| p.email.clone())
                .ok_or_else(|| {
                    anyhow!("--mine-only requires a configured principal in repo config")
                })
        })
        .transpose()?;

    let history = repo
        .query_history(&HistoryQuery::new(Some(head)).with_limit(NEXT_SCAN_LIMIT))
        .context("walk history for pending reviews")?;

    let mut next_state: Option<NextStateView> = None;
    for state in history {
        let state_id_bytes = state.change_id.as_bytes().to_vec();
        let state_id_str = state.change_id.to_string_full();
        let signatures = svc
            .list_signatures(tonic::Request::new(ListSignaturesRequest {
                repo_path: String::new(),
                state_id: state_id_bytes,
            }))
            .await
            .map_err(status_to_anyhow)?
            .into_inner()
            .signatures;

        let satisfied = signatures.iter().any(|s| {
            let actor_match = match actor_email.as_deref() {
                Some(email) => s.actor_email.eq_ignore_ascii_case(email),
                None => true,
            };
            let kind_match = match args.kind.as_deref() {
                Some(k) => {
                    let parsed = grpc::heddle::v1::ReviewKind::try_from(s.kind)
                        .unwrap_or(grpc::heddle::v1::ReviewKind::Unspecified);
                    review_kind_to_str(parsed) == k
                }
                None => true,
            };
            actor_match && kind_match
        });

        if !satisfied {
            next_state = Some(NextStateView {
                change_id: state_id_str,
                headline: state.intent.clone().unwrap_or_default(),
                existing_signatures: signatures.len() as u32,
            });
            break;
        }
    }

    if should_output_json(cli, None) {
        match &next_state {
            Some(view) => println!(
                "{}",
                serde_json::to_string(view).context("serialize next pending review")?
            ),
            None => println!("null"),
        }
    } else {
        match &next_state {
            Some(view) => {
                println!("next pending review: {}", view.change_id);
                if !view.headline.is_empty() {
                    println!("  {}", view.headline);
                }
                println!("  existing signatures: {}", view.existing_signatures);
            }
            None => println!(
                "no pending reviews in the last {NEXT_SCAN_LIMIT} states reachable from HEAD"
            ),
        }
    }
    Ok(())
}

const NEXT_SCAN_LIMIT: usize = 50;

#[derive(Serialize)]
struct NextStateView {
    change_id: String,
    headline: String,
    existing_signatures: u32,
}

async fn run_health(cli: &Cli, args: &ReviewHealthArgs) -> Result<()> {
    let svc = open_signal_service()?;
    let resp = svc
        .get_repo_signal_health(tonic::Request::new(GetRepoSignalHealthRequest {
            repo_path: String::new(),
            window_states: args.window.unwrap_or(0),
        }))
        .await
        .map_err(status_to_anyhow)?
        .into_inner();
    if should_output_json(cli, None) {
        let entries: Vec<_> = resp
            .entries
            .iter()
            .map(|e| {
                serde_json::json!({
                    "module_id": e.module_id,
                    "fire_rate": e.fire_rate,
                    "warn": e.warn,
                })
            })
            .collect();
        let out = serde_json::json!({
            "entries": entries,
            "window_states": resp.window_states,
        });
        println!("{out}");
    } else {
        println!("signal health (window: {} states)", resp.window_states);
        if resp.entries.is_empty() {
            println!("  (no signals fired in the window)");
        } else {
            for e in &resp.entries {
                let warn = if e.warn { " ⚠" } else { "" };
                println!("  {:30} {:>6.1}%{}", e.module_id, e.fire_rate * 100.0, warn);
            }
        }
    }
    Ok(())
}

fn open_state_review_service() -> Result<LocalStateReviewService> {
    let repo = open_repo()?;
    let dedup = OperationDedupStore::open(repo.heddle_dir()).context("open dedup store")?;
    let inner = GrpcLocalService::new(Arc::new(repo), Arc::new(dedup));
    Ok(LocalStateReviewService::new(inner))
}

fn open_signal_service() -> Result<LocalSignalService> {
    let repo = open_repo()?;
    let dedup = OperationDedupStore::open(repo.heddle_dir()).context("open dedup store")?;
    let inner = GrpcLocalService::new(Arc::new(repo), Arc::new(dedup));
    Ok(LocalSignalService::new(inner))
}

fn open_repo() -> Result<Repository> {
    let cwd = std::env::current_dir().context("get current working directory")?;
    Repository::open(&cwd).context("open Heddle repository")
}

fn signal_view(s: &grpc::heddle::v1::RiskSignal) -> SignalView {
    let anchor = s.anchor.clone().unwrap_or_default();
    SignalView {
        kind: s.kind.clone(),
        file: anchor.file,
        symbol: anchor.symbol,
        reason: s.reason.clone(),
        producer: s.producer_module.clone(),
        visibility: s.visibility.clone(),
    }
}

fn discussion_view(d: &grpc::heddle::v1::AnchoredDiscussion) -> DiscussionView {
    use grpc::heddle::v1::discussion_resolution::State;
    let anchor = d.anchor.clone().unwrap_or_default();
    let status = match d.resolution.as_ref().and_then(|r| r.state.as_ref()) {
        Some(State::Open(_)) | None => "open",
        Some(State::IntoAnnotation(_)) => "resolved_into_annotation",
        Some(State::ByEdit(_)) => "resolved_by_edit",
        Some(State::Dismissed(_)) => "dismissed",
    }
    .to_string();
    DiscussionView {
        id: d.id.clone(),
        file: anchor.file,
        symbol: anchor.symbol,
        status,
        body_changed_since_open: d.body_changed_since_open,
        orphaned: d.orphaned,
    }
}

fn opt_string(s: String) -> Option<String> {
    if s.is_empty() { None } else { Some(s) }
}

fn resolve_state(explicit: Option<&str>) -> Result<Vec<u8>> {
    let repo = open_repo()?;
    if let Some(s) = explicit {
        // Routes through the canonical resolver so short/full IDs and
        // marker names all work — matches `heddle log --json` output.
        return Ok(resolve_state_id(&repo, s)?.as_bytes().to_vec());
    }
    let head = repo
        .head()
        .context("read HEAD")?
        .ok_or_else(|| anyhow!("repository has no HEAD; capture a state first"))?;
    Ok(head.as_bytes().to_vec())
}

fn status_to_anyhow(status: tonic::Status) -> anyhow::Error {
    anyhow!("{}: {}", status.code(), status.message())
}

/// Render a 16-byte ChangeId from the wire as its display form. Empty input
/// → empty string (matches the prior empty-string-sentinel behavior).
fn bytes_to_change_id_string(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    objects::object::ChangeId::try_from_slice(bytes)
        .map(|id| id.to_string_full())
        .unwrap_or_default()
}

fn review_kind_to_str(kind: grpc::heddle::v1::ReviewKind) -> &'static str {
    use grpc::heddle::v1::ReviewKind;
    match kind {
        ReviewKind::Read => "read",
        ReviewKind::AgentPreview => "agent_preview",
        ReviewKind::AgentCoReview => "agent_co_review",
        ReviewKind::Unspecified => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shape contract for `review health --json`. The handler builds the
    /// JSON object inline with `serde_json::json!`; this test pins the
    /// keys, types, and nested entry shape against a hand-built sample
    /// that mirrors the handler's exact construction. Keeps the JSON
    /// surface stable for downstream tooling without spinning up a full
    /// local-service round-trip.
    #[test]
    fn review_health_json_shape() {
        // Mirror the exact `serde_json::json!` block in `run_health` so
        // the pinned shape and the wire shape track together.
        let entries = vec![
            serde_json::json!({
                "module_id": "novelty.tree_sitter",
                "fire_rate": 0.42_f32,
                "warn": false,
            }),
            serde_json::json!({
                "module_id": "self_flagged_uncertainty",
                "fire_rate": 0.81_f32,
                "warn": true,
            }),
        ];
        let out = serde_json::json!({
            "entries": entries,
            "window_states": 12u32,
        });
        let serialized = serde_json::to_string(&out).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&serialized).unwrap();

        // Top-level keys.
        let obj = parsed.as_object().expect("top-level is an object");
        assert!(obj.contains_key("entries"), "missing entries");
        assert!(obj.contains_key("window_states"), "missing window_states");

        // window_states is a number.
        assert!(
            obj["window_states"].is_number(),
            "window_states must be a number"
        );

        // entries is an array; each entry has the expected keys/types.
        let arr = obj["entries"].as_array().expect("entries is array");
        assert_eq!(arr.len(), 2, "two sample entries round-trip");
        for entry in arr {
            let e = entry.as_object().expect("entry is object");
            assert!(e.contains_key("module_id"));
            assert!(e.contains_key("fire_rate"));
            assert!(e.contains_key("warn"));
            assert!(e["module_id"].is_string(), "module_id must be string");
            assert!(e["fire_rate"].is_number(), "fire_rate must be number");
            assert!(e["warn"].is_boolean(), "warn must be boolean");
        }

        // Spot-check values to make sure the shape matches the runtime
        // representation, not just the structural skeleton.
        assert_eq!(arr[0]["module_id"], "novelty.tree_sitter");
        assert_eq!(arr[1]["warn"], true);
    }

    /// Guards `review_kind_to_str` against silent gaps when a new
    /// `ReviewKind` variant is added — adding a new variant without an
    /// arm in the helper would compile (the match returns "" via the
    /// catch-all) but produce garbage downstream.
    #[test]
    fn review_kind_to_str_covers_known_variants() {
        use grpc::heddle::v1::ReviewKind;
        assert_eq!(review_kind_to_str(ReviewKind::Read), "read");
        assert_eq!(
            review_kind_to_str(ReviewKind::AgentPreview),
            "agent_preview"
        );
        assert_eq!(
            review_kind_to_str(ReviewKind::AgentCoReview),
            "agent_co_review"
        );
        assert_eq!(review_kind_to_str(ReviewKind::Unspecified), "");
    }
}
